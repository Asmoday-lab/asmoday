//! tus 1.0.0 子集：分块上传与断点续传（设计文档 3.2.3.1）。
//!
//! 参照 FileBrowser Quantum 的上传模型（tus.io 协议）：
//!
//! ```text
//! POST   /api/files/upload/tus          建会话（Upload-Length 声明总长）→ 201 + Location
//! HEAD   /api/files/upload/tus/{id}     查已收字节数（Upload-Offset）
//! PATCH  /api/files/upload/tus/{id}     追加分片（Upload-Offset + application/offset+octet-stream）
//! DELETE /api/files/upload/tus/{id}     取消并清理
//! ```
//!
//! 与 tus 规范一致的关键取舍：
//! - **偏移的权威是临时文件的长度**，不是内存计数器——HEAD 直接 stat 磁盘，
//!   客户端断线重连后从真实进度续传；内存会话丢了（服务重启）整单作废，
//!   客户端按 404 重新 init，与规范行为一致。
//! - PATCH 逐分片**流式追加落盘**：服务端内存占用 = 缓冲区，与分片大小无关
//!   （顺手修正了整包上传 `body: Bytes` 全量进内存的矛盾）。
//! - 收满 `Upload-Length` 即自动落位（tus 的 Sync 语义），不需要额外的 complete 请求。
//!
//! 会话状态只在内存（与 FileBrowser 的 MemUploadCache 同级取舍）：
//! 服务启动时无条件清空 `<data>/uploads_tmp/`，运行期只按 TTL 兜底清孤儿。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde::Deserialize;

use crate::AppState;
use crate::core::fs::FsError;
use crate::core::fs_ops::{self, NameBook, OnConflict, resolve_placement};
use crate::core::path_guard;

/// 会话过期时间：临时文件 mtime 超过它就在下一次 init 时被清理。
/// 断点续传的窗口期——客户端重传间隔（浏览器标签页恢复）远小于该值。
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 分块上传会话表 + 临时目录。
pub struct TusStore {
    tmp_dir: PathBuf,
    sessions: Mutex<HashMap<String, TusSession>>,
    /// 每个上传会话一把异步锁（v4.9.20）：同一 id 的 PATCH / DELETE 串行。
    ///
    /// 两个并发 PATCH 同一 offset 是典型的 TOCTOU：都读到 current=N 都通过
    /// 校验，O_APPEND 只保证单次 write 不覆盖、不保证多个 chunk 不交错——
    /// 长度恰好凑满 Upload-Length 时会静默产出损坏文件。锁住整个
    /// 「校验偏移 → 流式写 → 落位/删除」临界区后，后来的 PATCH 会看到
    /// 已推进的 offset 并按规范吃 409。
    op_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
pub(crate) struct TusSession {
    temp_path: PathBuf,
    target_dir: PathBuf,
    name: String,
    conflict: OnConflict,
    length: u64,
    created: i64,
}

impl TusStore {
    pub fn new(tmp_dir: PathBuf) -> Self {
        // 内存会话不跨重启，磁盘上的半成品必然无人认领：启动时无条件清空
        if let Err(err) = std::fs::remove_dir_all(&tmp_dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("warn: 清理分块上传临时目录失败：{err}");
        }
        Self {
            tmp_dir,
            sessions: Mutex::new(HashMap::new()),
            op_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 取某个会话的操作锁（不存在也会建：调用方随后 re-get 会话判 404）。
    pub(crate) fn op_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.op_locks
            .lock()
            .ok()
            .map(|mut map| map.entry(id.to_string()).or_default().clone())
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())))
    }

    fn sweep_expired(&self) {
        let cutoff = crate::core::auth::now_secs() - SESSION_TTL.as_secs() as i64;
        let stale: Vec<String> = self
            .sessions
            .lock()
            .ok()
            .map(|map| {
                map.iter()
                    .filter(|(_, s)| s.created < cutoff)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in stale {
            self.remove(&id);
        }
        // 会话表之外的孤儿文件（进程崩溃后表已空、文件还在）：按 mtime 兜底
        if let Ok(reader) = std::fs::read_dir(&self.tmp_dir) {
            for entry in reader.filter_map(Result::ok) {
                let Ok(meta) = entry.metadata() else { continue };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                if !meta.is_dir() && mtime.unwrap_or(i64::MAX) < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    fn insert(&self, id: String, session: TusSession) {
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(id, session);
        }
    }

    fn get(&self, id: &str) -> Option<TusSession> {
        self.sessions.lock().ok()?.get(id).cloned()
    }

    fn remove(&self, id: &str) {
        let session = self.sessions.lock().ok().and_then(|mut map| map.remove(id));
        if let Some(session) = session {
            let _ = std::fs::remove_file(&session.temp_path);
        }
        // 操作锁一并摘除：会话没了，等待中的锁条目是垃圾。
        // 还持着锁的调用方与等待者各自持有 Arc，串行语义不受影响。
        if let Ok(mut map) = self.op_locks.lock() {
            map.remove(id);
        }
    }

    fn temp_file(&self, id: &str) -> PathBuf {
        self.tmp_dir.join(format!("{id}.part"))
    }
}

#[derive(Debug, Deserialize)]
pub struct TusInitQuery {
    /// 目标目录（绝对路径，必须落在挂载点内）
    pub dir: String,
    /// 文件名（校验同整包上传：validate_name）
    pub name: String,
    /// 同名策略：fail（默认，409）/ overwrite / rename
    #[serde(default)]
    pub conflict: Option<String>,
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

fn fs_error(err: FsError) -> Response {
    crate::api::error::fs_error(&err)
}

fn parse_conflict(raw: Option<&str>) -> Result<OnConflict, Response> {
    match raw.unwrap_or("fail") {
        "fail" => Ok(OnConflict::Skip), // Skip 的 placement 语义「同名即报错」，见 finalize_upload
        "overwrite" => Ok(OnConflict::Overwrite),
        "rename" => Ok(OnConflict::Rename),
        other => Err(fail(
            StatusCode::BAD_REQUEST,
            format!("unknown conflict policy: {other}"),
        )),
    }
}

/// POST /api/files/upload/tus：建立分块上传会话。
///
/// 校验链与整包上传完全一致（路径 / 只读 / 配额 / 授权根），会话建立时一次性做完，
/// 后续 PATCH 只对内存会话里的 `target_dir` 落位，不再重新解析路径。
pub async fn tus_init(
    State(state): State<AppState>,
    Query(query): Query<TusInitQuery>,
    headers: HeaderMap,
) -> Response {
    let length: u64 = match headers
        .get("upload-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(len) if len > 0 => len,
        _ => {
            return fail(
                StatusCode::BAD_REQUEST,
                "missing or invalid Upload-Length header",
            );
        }
    };

    // 单文件上限同样约束分块上传（设计文档 3.2.3）
    let limit = {
        let config = state.config.read().await;
        config.media.upload_max_size_mb
    };
    if limit > 0 && length > limit.saturating_mul(1024 * 1024) {
        return crate::api::error::api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "E_TOO_LARGE",
            format!("File exceeds the configured {limit} MiB upload limit"),
        );
    }

    if let Err(err) = path_guard::validate_name(&query.name) {
        return fs_error(err);
    }
    let conflict = match parse_conflict(query.conflict.as_deref()) {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let dir = match crate::api::files::resolve_upload_dir(&state, &query.dir, length).await {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    state.tus.sweep_expired();

    if let Err(err) = std::fs::create_dir_all(&state.tus.tmp_dir) {
        return fs_error(FsError::Io(format!("创建分块临时目录失败：{err}")));
    }

    let id = crate::core::auth::random_token();
    let temp_path = state.tus.temp_file(&id);
    if let Err(err) = std::fs::File::create(&temp_path) {
        return fs_error(FsError::Io(format!("创建分块临时文件失败：{err}")));
    }
    state.tus.insert(
        id.clone(),
        TusSession {
            temp_path: temp_path.clone(),
            target_dir: dir,
            name: query.name,
            conflict,
            length,
            created: crate::core::auth::now_secs(),
        },
    );

    let location = format!("/api/files/upload/tus/{id}");
    (
        StatusCode::CREATED,
        [(
            header::LOCATION,
            header::HeaderValue::from_str(&location)
                .unwrap_or_else(|_| header::HeaderValue::from_static("/")),
        )],
    )
        .into_response()
}

/// HEAD /api/files/upload/tus/{id}：查当前偏移（断点续传的续传点）。
pub async fn tus_head(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let Some(session) = state.tus.get(&id) else {
        return fail(StatusCode::NOT_FOUND, "upload session not found");
    };
    // 偏移的权威是临时文件长度：客户端断线 / 换标签页后照旧能续
    let offset = std::fs::metadata(&session.temp_path)
        .map(|m| m.len())
        .unwrap_or(0);
    (
        StatusCode::OK,
        [
            ("upload-offset", offset.to_string()),
            ("upload-length", session.length.to_string()),
            ("cache-control", "no-store".to_string()),
        ],
    )
        .into_response()
}

/// PATCH /api/files/upload/tus/{id}：流式追加分片。
///
/// 逐帧写盘（`body.frame()` → `AsyncWriteExt::write_all_buf`），不整包缓冲；
/// 客户端中途断开时已写入的部分就是下次续传的起点。
pub async fn tus_patch(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/offset+octet-stream"))
    {
        return fail(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/offset+octet-stream",
        );
    }
    let claimed: u64 = match headers
        .get("upload-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(v) => v,
        None => return fail(StatusCode::BAD_REQUEST, "missing Upload-Offset header"),
    };

    // 同会话操作串行（v4.9.20）：锁住「校验偏移 → 流式写 → 落位/删除」
    // 整段临界区。抢到锁后必须重新 get —— 等待期间会话可能已被并发
    // DELETE / 并发 PATCH 落位删除。
    let lock = state.tus.op_lock(&id);
    let _serial = lock.lock().await;
    let Some(session) = state.tus.get(&id) else {
        return fail(StatusCode::NOT_FOUND, "upload session not found");
    };

    let current = std::fs::metadata(&session.temp_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if claimed != current {
        return crate::api::error::api_error(
            StatusCode::CONFLICT,
            "E_OFFSET_MISMATCH",
            format!("Upload-Offset {claimed} does not match server offset {current}"),
        );
    }

    let mut file = match tokio::fs::OpenOptions::new()
        .append(true)
        .open(&session.temp_path)
        .await
    {
        Ok(file) => file,
        Err(err) => return fs_error(FsError::Io(format!("打开分块临时文件失败：{err}"))),
    };
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let mut chunk = match frame {
            // 非数据帧（trailers 等）直接跳过
            Ok(f) => match f.into_data() {
                Ok(chunk) => chunk,
                Err(_) => continue,
            },
            Err(_) => {
                // 客户端断开：已写入部分保留为续传起点，会话不动
                return fail(StatusCode::BAD_REQUEST, "request body aborted");
            }
        };
        if let Err(err) = tokio::io::AsyncWriteExt::write_all_buf(&mut file, &mut chunk).await {
            return fs_error(FsError::Io(format!("写入分块失败：{err}")));
        }
    }
    drop(file);

    let size = std::fs::metadata(&session.temp_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if size > session.length {
        // 超过声明长度：会话作废重来（防计数器漂移导致的无限增长）
        state.tus.remove(&id);
        return fail(
            StatusCode::BAD_REQUEST,
            "upload exceeds declared Upload-Length",
        );
    }
    if size == session.length {
        // 收满即落位（tus Sync 语义），不需要单独的 complete 请求。
        // finalize 失败时**保留会话**：临时文件完好，客户端修掉冲突
        // （比如删掉同名文件）后再 PATCH 一次即可重新落位；
        // 会话由 24h TTL 兜底清理。
        return match finalize_upload(&state, &id, &session) {
            Ok(target) => {
                if let Some(target) = target {
                    crate::api::files::forget_parent_snapshot(&state, &target);
                }
                (
                    StatusCode::NO_CONTENT,
                    [("upload-offset", size.to_string())],
                )
                    .into_response()
            }
            Err(err) => fs_error(err),
        };
    }

    (
        StatusCode::NO_CONTENT,
        [("upload-offset", size.to_string())],
    )
        .into_response()
}

/// DELETE /api/files/upload/tus/{id}：取消上传并清理临时文件。
///
/// 与 PATCH 抢同一把会话锁：否则「对方正在写、这边删文件」在 POSIX 上
/// 是写进已 unlink 的 inode（数据静默丢失），Windows 上直接删不动。
pub async fn tus_delete(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let lock = state.tus.op_lock(&id);
    let _serial = lock.lock().await;
    state.tus.remove(&id);
    StatusCode::NO_CONTENT.into_response()
}

/// 把收满的临时文件落位到目标目录。
///
/// 优先原子改名（同设备瞬时完成）；数据目录与挂载点不同盘时改名会失败
/// （EXDEV / Windows ERROR_NOT_SAME_DEVICE），回退到 `move_entry` 的
/// 「临时名复制 + 字节校验 + 删源」路径。
fn finalize_upload(
    state: &AppState,
    id: &str,
    session: &TusSession,
) -> Result<Option<PathBuf>, FsError> {
    let mut book = NameBook::new(&session.target_dir);
    let (final_name, overwrite) =
        match resolve_placement(&mut book, &session.name, false, session.conflict)? {
            fs_ops::Placement::Skip => {
                // conflict=fail 且同名：resolve_placement 对 Skip 策略返回 Placement::Skip，
                // 这里转成显式 409，而不是静默丢弃
                return Err(FsError::Conflict(format!(
                    "File already exists: {}",
                    session.name
                )));
            }
            fs_ops::Placement::Use(name, overwrite) => (name, overwrite),
        };
    let target = session.target_dir.join(&final_name);

    // 落位目标纳入 per-path 锁（v4.9.20）：「上传落位」与「删除/移动同名
    // 文件」互斥；跨会话同名竞争在这里表现为 E_BUSY，而不是把整单上传丢掉。
    let _path_guard = state.path_locks.acquire(std::slice::from_ref(&target))?;

    let attempt = if overwrite {
        path_guard::rename_replace(&session.temp_path, &target)
    } else {
        path_guard::rename_no_replace(&session.temp_path, &target)
    };
    if attempt.is_ok() {
        state.tus.sessions.lock().ok().map(|mut map| map.remove(id));
        return Ok(Some(target));
    }

    // 跨设备回退：临时目录里先改成正式名，再走 move_entry（复制 + 校验 + 删源）
    let staged = session
        .temp_path
        .with_file_name(format!(".asmoday-tus-{final_name}"));
    std::fs::rename(&session.temp_path, &staged)
        .map_err(|err| FsError::Io(format!("暂存待移动文件失败：{err}")))?;
    let mut book2 = NameBook::new(&session.target_dir);
    let moved = fs_ops::move_entry(
        &staged,
        &session.target_dir,
        &mut book2,
        session.conflict,
        false,
    )?;
    if moved.is_none() {
        // Skip 冲突且复制路径没动源文件：别把暂存文件留成孤儿
        let _ = std::fs::remove_file(&staged);
    }
    state.tus.sessions.lock().ok().map(|mut map| map.remove(id));
    Ok(moved)
}
