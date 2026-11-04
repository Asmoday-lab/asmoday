//! 文件管理接口（设计文档 6.1）。
//!
//! - `GET  /api/files/mounts`：挂载点清单（id / name / path / readonly / online）
//! - `GET  /api/files/list`：目录列表，固定排序 + 游标分页（超 1000 条翻页并回 next_cursor）
//! - `GET  /api/files/raw?path=`：流式读取，支持 HTTP Range（206 / 416）、ETag / Last-Modified（304）
//! - `PUT  /api/files/raw?path=`：编辑器保存写回（仅文本类、原子替换）

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::AppState;
use crate::core::config::Config;
use crate::core::fs::{self, FsError, RootSet};
use crate::core::fs_ops;
use crate::core::path_guard;

/// 编辑器单次保存上限（8 MiB）
const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;

/// 上传 / 分块上传的**空闲**超时：连续这么久没有新数据就当客户端已放弃。
///
/// 不是总时长限制——大文件慢速上传只要还在出数据就不会被打断。
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
    pub page: Option<usize>,
    /// 游标分页（设计文档 3.2.4）：有它就忽略 page
    #[serde(default)]
    pub cursor: Option<String>,
    /// 是否返回以 `.` 开头的隐藏项（设计文档 3.2，默认关闭）
    #[serde(default)]
    pub show_hidden: Option<bool>,
    /// 排序键：name（默认）/ mtime / size（设计文档 v2.7.0 3.2.1）
    #[serde(default)]
    pub sort: Option<String>,
    /// 升降序：asc（默认）/ desc
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    pub path: String,
    /// `?download=1`：响 application/octet-stream + Content-Disposition，强制存盘。
    /// 其它接口共用这个结构，缺省即「就地展示」。
    #[serde(default)]
    pub download: Option<String>,
}

/// `PUT /api/files/raw` 的保存选项（设计文档 3.2.2.1）。
///
/// 三个参数都**可选**：前端读文件时拿到了原文件的编码 / 换行 / BOM，保存时原样带回；
/// 老客户端不带参数时按「磁盘上现有文件」的元信息推断，绝不默认成 LF 覆盖。
#[derive(Debug, Deserialize)]
pub struct SaveQuery {
    pub path: String,
    /// `lf` / `crlf`
    #[serde(default)]
    pub eol: Option<String>,
    /// `1` / `0`（也接受 true/false）
    #[serde(default)]
    pub bom: Option<String>,
    /// `utf-8` / `gbk` / `utf-16le` / `utf-16be`
    #[serde(default)]
    pub encoding: Option<String>,
}

/// 允许根集合：复用 `mounts_for` 的 10 秒缓存（v4.9.22）。
///
/// `path_guard::load` 对每个挂载点做 `canonicalize` + `is_dir`，离线 SMB 上单次
/// 可达十几秒；而列表、raw、上传、缩略图**每个请求**都要这份根集合。
/// 挂载点配置一改 `mounts_revision` 就 +1，缓存立即失效，不会读到旧视图。
async fn roots_for(state: &AppState) -> RootSet {
    let set = mounts_for(state).await;
    let follow_symlinks = state.config.read().await.system.follow_symlinks;
    fs::root_set_from_mounts(&set, follow_symlinks)
}

/// 挂载点集合的进程内缓存（v4.9.21）；唯一真源仍是 `core::path_guard`。
pub struct MountsCache {
    pub revision: u64,
    pub at: std::time::Instant,
    pub set: path_guard::MountSet,
}

/// 保鲜期与前端轮询周期一致（`GalleryView` 10s）：在线/掉线状态最多滞后一拍，
/// 但不必为每个请求都付一次 SMB 往返。
const MOUNTS_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// 挂载点集合（带 readonly / online），带 10 秒进程内缓存。
///
/// `pub(crate)`：媒体库的路径解析（缩略图 / 封面 / 播放进度）也走这里 ——
/// 它自己每请求重扫一遍挂载点，一个离线 SMB 就能把 worker 拖死（v4.9.22）。
pub(crate) async fn mounts_for(state: &AppState) -> path_guard::MountSet {
    let revision = state
        .mounts_revision
        .load(std::sync::atomic::Ordering::Relaxed);
    // 缓存命中判断单独一小段：**不要**握着这把锁去读 config，
    // 否则「配置写锁 → 挂载点缓存」和这里的方向相反就成了死锁隐患。
    {
        let cached = state.mounts_cache.lock().await;
        if let Some(cached) = cached.as_ref()
            && cached.revision == revision
            && cached.at.elapsed() < MOUNTS_TTL
        {
            return cached.set.clone();
        }
    }

    let set = {
        let config = state.config.read().await;
        path_guard::load(&config, &state.cwd)
    };
    *state.mounts_cache.lock().await = Some(MountsCache {
        revision,
        at: std::time::Instant::now(),
        set: set.clone(),
    });
    set
}

fn fs_error(err: FsError) -> Response {
    crate::api::error::fs_error(&err)
}

/// 细节型错误（4xx 参数类）
fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2）
fn fail_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// GET /api/files/roots
/// 文件内容被原地覆盖后，丢掉它所在目录的媒体快照。
///
/// 目录快照以「目录 mtime」为失效依据，而**覆盖写既有文件不会推进目录 mtime**
/// （名字集合没变），只有创建 / 删除 / 改名才会。所以这一类写必须自己补一刀，
/// 否则媒体库里那张图的体积与时间会一直是旧的、排序也停在老位置。
pub(crate) fn forget_parent_snapshot(state: &AppState, path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let display = fs::normalize_display(parent);
        state.dir_cache.forget(&display);
        // 媒体索引是同一套失效口径（目录 mtime 看不出来原地覆盖），这里补一刀，
        // 顺手作废「新鲜窗口」：应用自己的写操作必须立刻可见，不等窗口到期
        state.media_index.with_conn(|conn| {
            crate::core::media_index::forget_dir(conn, &display)?;
            crate::core::media_index::invalidate(conn)
        });
    }
}

/// 解析并校验一个「可写入的上传目标目录」，整包 PUT 与 tus init 共用（安全审查
/// 2026-11-02 时两处口径已对齐；只读检查为 v4.6.14 补齐——此前整包上传漏了它）。
///
/// 校验链：路径解析 → 是目录 → 只读 → 剩余空间 → 授权根包含性。
pub(crate) async fn resolve_upload_dir(
    state: &AppState,
    dir: &str,
    needed_bytes: u64,
) -> Result<PathBuf, Response> {
    let roots = roots_for(state).await;
    let resolved = match fs::resolve_existing(dir, &roots.allowed, roots.follow_symlinks) {
        Ok(path) => path,
        Err(err) => return Err(fs_error(err)),
    };
    if !resolved.is_dir() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Upload target is not a directory",
        ));
    }

    // 只读挂载点拒绝写入（与 files_mutate 的 mount.ensure_writable 同口径）
    let mounts = mounts_for(state).await;
    if mounts.is_readonly(&resolved) {
        return Err(fs_error(FsError::Readonly(fs::normalize_display(
            &resolved,
        ))));
    }

    // 上传前检查目标挂载点剩余空间（设计文档 3.2.3）
    if let Err(err) = fs_ops::ensure_space(&resolved, needed_bytes) {
        return Err(fs_error(err));
    }

    // 二次确认：拼接后的真实路径仍在授权根内。
    // 允许「经由挂载点内软链接」逃逸时（`[system].follow_symlinks`）不算越权——
    // 与 `resolve_existing` 口径一致，否则链接目录里上传会被这一道单独拦掉。
    if let Ok(canonical_parent) = std::fs::canonicalize(&resolved)
        && !roots
            .allowed
            .iter()
            .any(|root| path_guard::within(&canonical_parent, root))
        && !(roots.follow_symlinks && path_guard::escaped_via_symlink(&resolved, &roots.allowed))
    {
        return Err(fail_code(
            StatusCode::FORBIDDEN,
            "E_FORBIDDEN",
            "Upload target is outside the allowed roots",
        ));
    }
    Ok(resolved)
}

pub async fn roots(State(state): State<AppState>) -> Response {
    let set = roots_for(&state).await;
    let default_root = set.default_root().map(|r| r.path.clone());
    Json(json!({
        "roots": set.roots,
        "implicit": set.implicit,
        "default": default_root,
    }))
    .into_response()
}

/// GET /api/files/mounts（设计文档 3.2.1 / 6.1）
///
/// 前端每次进入文件管理都必须重新拉取，禁止缓存 —— 单挂载点直接跳转的判定
/// 依赖这份实时清单。
pub async fn mounts(State(state): State<AppState>) -> Response {
    let set = mounts_for(&state).await;
    let items: Vec<_> = set.mounts.iter().map(|m| m.info.clone()).collect();
    // 分片大小随挂载点清单下发：文件页据此决定整包 PUT 还是 tus 分块（3.2.3.1）
    let chunk_size_mb = state.config.read().await.media.upload_chunk_size_mb;
    // 挂载点版本号（v4.9.2）：配置热重载后前端靠它发现「后端那套挂载点换了」，
    // 不用等用户手动刷新页面。前端只需轻量轮询这个接口比对数字。
    let revision = state
        .mounts_revision
        .load(std::sync::atomic::Ordering::Relaxed);
    Json(json!({
        "mounts": items,
        "implicit": set.implicit,
        "upload_chunk_size_mb": chunk_size_mb,
        "revision": revision,
    }))
    .into_response()
}

/// GET /api/files/list
pub async fn list(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    let set = mounts_for(&state).await;

    let requested = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let mount = match requested {
        Some(raw) => match set.find_by_path_text(raw) {
            Some(mount) => mount,
            None => match set.default_mount() {
                Some(mount) => mount,
                None => return fs_error(FsError::NotFound("没有可用的挂载点".to_string())),
            },
        },
        None => match set.default_mount() {
            Some(mount) => mount,
            None => return fs_error(FsError::NotFound("没有可用的挂载点".to_string())),
        },
    };

    let target = match path_guard::resolve_existing(mount, requested.unwrap_or("")) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    if !target.is_dir() {
        return fs_error(FsError::BadRequest("目标不是目录".to_string()));
    }

    let readonly = mount.info.readonly;
    match fs::list_dir(
        &target,
        query.page.unwrap_or(1),
        query.show_hidden.unwrap_or(false),
        readonly,
        fs::ListSort::parse(query.sort.as_deref()),
        fs::ListOrder::parse(query.order.as_deref()),
        query.cursor.as_deref().filter(|value| !value.is_empty()),
        // 软链接目标的越界判定基准：挂载点 canonical_root（设计文档 3.2.7）
        mount.canon_root().ok().map(|root| root.as_path()),
        mount.info.follow_symlinks,
    )
    .await
    {
        Ok(listing) => Json(listing).into_response(),
        Err(err) => fs_error(err),
    }
}

/// GET /api/files/raw
pub async fn raw(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> Response {
    let set = roots_for(&state).await;
    let path = match fs::resolve_existing(&query.path, &set.allowed, set.follow_symlinks) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    // .asmoday-part 临时文件不可预览/下载（设计文档 3.2.6.4，v3.4.0）
    if path_guard::is_part_path(&path) {
        return fs_error(FsError::NotFound("文件不存在".to_string()));
    }

    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取文件信息失败：{e}"))),
    };
    if meta.is_dir() {
        return fs_error(FsError::BadRequest("目标是目录，无法直接读取".to_string()));
    }

    let total = meta.len();
    // 条件请求（设计文档 3.2.3）：ETag 用「大小 + mtime」指纹，Last-Modified 走 RFC 1123
    let modified = meta.modified().ok();
    let mtime_nanos = modified
        .as_ref()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{total:x}-{mtime_nanos:x}\"");
    let last_modified = modified.as_ref().map(|time| httpdate::fmt_http_date(*time));

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag))
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(header::ETAG, etag.parse().expect("valid etag"));
        if let Some(value) = &last_modified
            && let Ok(parsed) = value.parse()
        {
            response.headers_mut().insert(header::LAST_MODIFIED, parsed);
        }
        return response;
    }

    // mime_guess 对 .toml / .md / .log 一律给 `text/plain`，**不带 charset** ——
    // 浏览器于是按系统本地编码（中文 Windows 是 GBK）解码，中文全成乱码。
    // 本项目只产生 UTF-8，文本类统一补上；下载时再整体覆盖成 octet-stream。
    let mut mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    if mime.starts_with("text/") && !mime.contains("charset") {
        mime.push_str("; charset=utf-8");
    }

    // `?download=1`：强制存盘。没有这个头，浏览器会就地渲染（截图里那个乱码 TOML
    // 页面就是 `window.open(raw)` 的产物），而 `<a download>` 在跨域/重定向下会被忽略。
    let download = query
        .download
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    let mut extra: Vec<(HeaderName, String)> = Vec::new();
    // 内联出口防 XSS（安全审查 2026-11-02）：nosniff 一律带上；HTML 能以本站
    // origin 执行脚本，强制走 attachment。SVG 相册灯箱要当原图内联（<img> 里
    // 不执行脚本），直开文档时靠响应级 CSP `sandbox` 关掉脚本与表单。
    let download = download || mime.starts_with("text/html");
    extra.push((header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()));
    if !download && mime == "image/svg+xml" {
        extra.push((header::CONTENT_SECURITY_POLICY, "sandbox".to_string()));
    }
    if download {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        // ASCII 兜底给老客户端，filename* 才是中文正确写法（RFC 5987 / 6266）
        let fallback: String = name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        extra.push((
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
                encode_rfc5987(&name)
            ),
        ));
        extra.push((header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()));
        mime = "application/octet-stream".to_string();
    }

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());

    let parsed = match range_header {
        Some(raw_header) => match fs::parse_range(raw_header, total) {
            Ok(value) => value,
            Err(()) => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                )
                    .into_response();
            }
        },
        None => None,
    };

    match parsed {
        Some(range) => {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(e) => return fs_error(FsError::Io(format!("打开文件失败：{e}"))),
            };
            if let Err(e) = file.seek(std::io::SeekFrom::Start(range.start)).await {
                return fs_error(FsError::Io(format!("定位文件失败：{e}")));
            }
            let limited = file.take(range.len());
            let stream = ReaderStream::new(limited);
            let mut response =
                (StatusCode::PARTIAL_CONTENT, Body::from_stream(stream)).into_response();
            let mut headers_out = vec![
                (header::CONTENT_TYPE, mime),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", range.start, range.end, total),
                ),
                (header::CONTENT_LENGTH, range.len().to_string()),
                // `no-store` 会让原图每次打开都重传一遍（灯箱里就是「闪一下才好」）。
                // 文件带 ETag，`private, no-cache` 足够安全：每次回源校验，命中走 304。
                (header::CACHE_CONTROL, "private, no-cache".to_string()),
                (header::ETAG, etag),
            ];
            if let Some(value) = last_modified {
                headers_out.push((header::LAST_MODIFIED, value));
            }
            headers_out.extend(extra.clone());
            for (name, value) in headers_out {
                if let Ok(parsed) = value.parse() {
                    response.headers_mut().insert(name, parsed);
                }
            }
            response
        }
        None => {
            let file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(e) => return fs_error(FsError::Io(format!("打开文件失败：{e}"))),
            };
            let stream = ReaderStream::new(file);
            let mut response = (StatusCode::OK, Body::from_stream(stream)).into_response();
            let mut headers_out = vec![
                (header::CONTENT_TYPE, mime),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, total.to_string()),
                // `no-store` 会让原图每次打开都重传一遍（灯箱里就是「闪一下才好」）。
                // 文件带 ETag，`private, no-cache` 足够安全：每次回源校验，命中走 304。
                (header::CACHE_CONTROL, "private, no-cache".to_string()),
                (header::ETAG, etag),
            ];
            if let Some(value) = last_modified {
                headers_out.push((header::LAST_MODIFIED, value));
            }
            headers_out.extend(extra);
            for (name, value) in headers_out {
                if let Ok(parsed) = value.parse() {
                    response.headers_mut().insert(name, parsed);
                }
            }
            response
        }
    }
}

/// `GET /api/files/archive` 的查询串（设计文档 3.2.4）。
///
/// 单目标走 `path=`（保持历史行为）；多选走 `paths=`，值是 JSON 字符串数组——
/// 重复的 `path=` 在 `serde_urlencoded` 里拿不到全部值，而逗号分隔在路径里不安全
/// （文件名本身可能带逗号），JSON 是唯一能无损传「一组路径」的朴素写法。
#[derive(Debug, Deserialize)]
pub struct ArchiveQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub paths: Option<String>,
}

impl ArchiveQuery {
    /// 解析出要打包的路径列表（去空、保序）。
    fn targets(&self) -> Result<Vec<String>, String> {
        if let Some(raw) = self
            .paths
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            let list: Vec<String> =
                serde_json::from_str(raw).map_err(|_| "paths 参数必须是字符串数组".to_string())?;
            return Ok(list
                .into_iter()
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect());
        }
        Ok(self
            .path
            .as_deref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .into_iter()
            .collect())
    }
}

/// GET /api/files/archive?path=…（或 `?paths=[…]`）：打成 ZIP 流式下发（设计文档 3.2.4）。
///
/// store 模式 + 数据描述符：不压缩、不落临时文件、内存恒定，首字节就是第一个
/// 文件的读延迟。条目在阻塞线程里摊平（walk 是同步 IO），然后交给一条 duplex
/// 管道边写边发；客户端断开时写端报错退出，任务自然结束。
///
/// 多选为什么合并成一个 zip：同一次用户手势里连开好几个下载，浏览器会当成
/// 「多文件下载」拦掉（Chrome 只放行第一个），多选下载在手机上因此完全无效。
pub async fn archive(State(state): State<AppState>, Query(query): Query<ArchiveQuery>) -> Response {
    let set = roots_for(&state).await;
    let requested = match query.targets() {
        Ok(list) if list.is_empty() => {
            return fs_error(FsError::BadRequest("没有指定要打包的路径".to_string()));
        }
        Ok(list) => list,
        Err(message) => return fs_error(FsError::BadRequest(message)),
    };
    if requested.len() > crate::core::archive::MAX_TARGETS {
        return fs_error(FsError::BadRequest(format!(
            "一次最多打包 {} 项",
            crate::core::archive::MAX_TARGETS
        )));
    }

    let mut paths = Vec::with_capacity(requested.len());
    for raw in &requested {
        let path = match fs::resolve_existing(raw, &set.allowed, set.follow_symlinks) {
            Ok(path) => path,
            Err(err) => return fs_error(err),
        };
        // .asmoday-part 上传中间文件不进包（设计文档 3.2.6.4）
        if path_guard::is_part_path(&path) {
            return fs_error(FsError::NotFound("文件不存在".to_string()));
        }
        paths.push(path);
    }

    // 单目录保持老语义：目录名做顶层、下载名也叫它；单文件仍建议直接下载
    if paths.len() == 1 && !paths[0].is_dir() {
        return fs_error(FsError::BadRequest("目标是文件，直接下载即可".to_string()));
    }

    let filename = archive_filename(&paths);
    let entries =
        match tokio::task::spawn_blocking(move || crate::core::archive::plan(&paths)).await {
            Ok(Ok(entries)) => entries,
            Ok(Err(err)) => return fs_error(FsError::Io(err.to_string())),
            Err(err) => return fs_error(FsError::Io(format!("打包失败：{err}"))),
        };

    // 256 KiB 缓冲：够吞下一条 TCP 窗口，又不至于让慢客户端在服务端囤满内存
    let (mut writer, reader) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let _ = crate::core::archive::stream_zip(entries, &mut writer).await;
    });

    let mut response =
        (StatusCode::OK, Body::from_stream(ReaderStream::new(reader))).into_response();
    let headers_out = [
        (header::CONTENT_TYPE, "application/zip".to_string()),
        (header::CACHE_CONTROL, "no-store".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"archive.zip\"; filename*=UTF-8''{}",
                encode_rfc5987(&filename)
            ),
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
    ];
    for (key, value) in headers_out {
        if let Ok(parsed) = value.parse() {
            response.headers_mut().insert(key, parsed);
        }
    }
    response
}

/// 打包下载的文件名（`Content-Disposition` 的 `filename*`）。
///
/// 单目录沿用 `<目录名>.zip`；多选时如果目标都在同一层目录里，用那个目录名
/// （多选绝大多数发生在「当前目录」），跨目录的搜索结果才退回 `download.zip`。
fn archive_filename(paths: &[PathBuf]) -> String {
    let base = if paths.len() == 1 {
        paths[0]
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
    } else {
        let parent = paths[0].parent();
        if paths.iter().all(|path| path.parent() == parent) {
            parent
                .and_then(|dir| dir.file_name())
                .map(|name| name.to_string_lossy().to_string())
        } else {
            None
        }
    }
    .unwrap_or_else(|| "download".to_string());
    format!("{base}.zip")
}

/// RFC 5987 的 `filename*` 百分号编码：只有 attr-char 原样保留，其余按字节 %XX。
///
/// 中文文件名走 `filename*=UTF-8''…` 才不会变成一串问号；`filename="…"` 那份
/// 是给不认 RFC 5987 的老客户端的 ASCII 兜底（调用处已把非 ASCII 换成 `_`）。
fn encode_rfc5987(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 3);
    for &byte in name.as_bytes() {
        let ch = byte as char;
        let safe = ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
            );
        if safe {
            out.push(ch);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// GET /api/files/text：编辑器专用的 JSON 读取（设计文档 3.2.2.1）。
///
/// 与 `/api/files/raw` 的区别是**带元信息**：字符编码、换行符风格、是否有 BOM。
/// 内容里的换行符统一成 `\n` 传输，保存时由服务端按 `eol_type` 还原。
pub async fn read_text(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    let set = mounts_for(&state).await;
    let Some(mount) = set.find_by_path_text(&query.path) else {
        return fs_error(FsError::NotFound(format!(
            "路径不在任何挂载点内：{}",
            query.path
        )));
    };
    let path = match path_guard::resolve_existing(mount, &query.path) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    // .asmoday-part 临时文件不可预览（设计文档 3.2.6.4，v3.4.0）
    if path_guard::is_part_path(&path) {
        return fs_error(FsError::NotFound("文件不存在".to_string()));
    }

    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取文件信息失败：{e}"))),
    };
    if meta.is_dir() {
        return fail(
            StatusCode::BAD_REQUEST,
            "The target is a directory, it cannot be opened in the editor",
        );
    }
    if meta.len() as usize > MAX_EDIT_BYTES {
        return fail_code(
            StatusCode::PAYLOAD_TOO_LARGE,
            "E_TOO_LARGE",
            format!(
                "File exceeds the editor limit ({} MiB)",
                MAX_EDIT_BYTES / 1024 / 1024
            ),
        );
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = fs::extension_of_name(&name);
    let (kind, editable) = fs::classify_name(&name, &ext, false);
    if kind != "text" || !editable {
        return fail_code(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "E_NOT_TEXT",
            "The built-in editor only supports text files",
        );
    }

    let raw = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => return fs_error(FsError::Io(format!("读取文件失败：{e}"))),
    };

    // 解不出干净文本就报错，绝不把替换字符（�）喂给编辑器让用户「保存」回磁盘
    let decoded = match crate::core::text_codec::decode(&raw) {
        Ok(decoded) => decoded,
        Err(err) => {
            return fail_code(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "E_ENCODING",
                err.message(),
            );
        }
    };

    Json(json!({
        "path": fs::normalize_display(&path),
        "name": name,
        "content": decoded.text,
        "encoding": decoded.encoding.as_str(),
        "encoding_label": decoded.encoding.label(),
        "eol_type": decoded.eol.as_str(),
        "eol_label": decoded.eol.label(),
        "has_bom": decoded.has_bom,
        "mixed_eol": decoded.mixed_eol,
        "size": raw.len(),
        "readonly": mount.info.readonly,
    }))
    .into_response()
}

/// PUT /api/files/raw：文本文件写回。
pub async fn write_text(
    State(state): State<AppState>,
    Query(query): Query<SaveQuery>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_EDIT_BYTES {
        return fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "File exceeds the editor limit ({} MiB)",
                MAX_EDIT_BYTES / 1024 / 1024
            ),
        );
    }

    let set = mounts_for(&state).await;
    let Some(mount) = set.find_by_path_text(&query.path) else {
        return fs_error(FsError::NotFound(format!(
            "路径不在任何挂载点内：{}",
            query.path
        )));
    };
    if let Err(err) = mount.ensure_writable() {
        return fs_error(err);
    }
    let path = match path_guard::resolve_existing(mount, &query.path) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取文件信息失败：{e}"))),
    };
    if meta.is_dir() {
        return fail(
            StatusCode::BAD_REQUEST,
            "The target is a directory, it cannot be written",
        );
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = fs::extension_of_name(&name);
    let (kind, editable) = fs::classify_name(&name, &ext, false);
    if kind != "text" || !editable {
        // 与读接口同一个码：前端只需一条 i18n 文案
        return fail_code(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "E_NOT_TEXT",
            "The built-in editor only writes text files",
        );
    }

    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => {
            return fail_code(
                StatusCode::BAD_REQUEST,
                "NOT_TEXT",
                "Content is not valid UTF-8 text",
            );
        }
    };

    // 目标风格：显式参数优先，其次沿用磁盘上现有文件的编码 / 换行 / BOM
    let existing = read_style_snapshot(&path).await;
    let encoding = match query.encoding.as_deref() {
        Some(raw) => match crate::core::text_codec::TextEncoding::parse(raw) {
            Some(encoding) => encoding,
            None => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("Unsupported encoding: {raw}"),
                );
            }
        },
        None => existing.0,
    };
    let eol = match query.eol.as_deref() {
        Some(raw) => match crate::core::text_codec::Eol::parse(raw) {
            Some(eol) => eol,
            None => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("Unsupported line ending: {raw}"),
                );
            }
        },
        None => existing.1,
    };
    let bom = match query.bom.as_deref() {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("Invalid bom parameter: {other}"),
                );
            }
        },
        None => existing.2,
    };

    // 编码不可逆（例如 GBK 文件里出现 emoji）：拒绝写盘，宁可报错也不落一坨乱码
    let encoded = match crate::core::text_codec::encode(text, encoding, eol, bom) {
        Ok(bytes) => bytes,
        Err(err) => {
            return fail_code(
                StatusCode::UNPROCESSABLE_ENTITY,
                "E_ENCODING",
                err.message(),
            );
        }
    };

    // 原子替换：同目录临时文件 + rename，避免写一半断电导致文件损坏。
    //
    // 临时名统一走 `part_name`（`<name>.asmoday-part.<8位随机>`，v4.9.22）：
    // 自带随机后缀所以并发唯一（两个标签页同时保存不会互相覆盖），而且
    // 列表过滤 / 启动清理 / 打包排除 / raw 拦截全都认这个标记 —— 以前用
    // 自定义的 `.xxx.asmoday-save.tmp`，进程被强杀后那个半截文件会永久
    // 留在用户目录里，还能被下载成 zip 的一部分。
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(crate::core::path_guard::part_name(&file_name));

    // 先落盘再改名，并且**同步刷盘**（v4.9.22）：rename 只保证「名字指向的文件是
    // 刚写的那个」，不保证数据已离开页缓存。断电/宿主强杀时会出现最终名指向
    // 空文件的经典场景——比没有文件更糟：用户看到过「保存成功」，旧内容也没了。
    // 文本都是 KB 级、一次 fsync 几毫秒，这个代价换的是「保存过就是保存过」。
    let written = tokio::fs::File::create(&tmp).await;
    let written = match written {
        Ok(file) => {
            let mut file = file;
            match tokio::io::AsyncWriteExt::write_all(&mut file, &encoded).await {
                Ok(()) => file.sync_all().await,
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    };
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(FsError::Io(format!("写入临时文件失败：{e}")));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(FsError::Io(format!("替换文件失败：{e}")));
    }

    // 原地覆盖：文件内容变了但**父目录 mtime 没变**，媒体库的目录快照会把
    // 旧的 size / mtime 一直当真的用。改完主动丢掉这一层（见 dir_cache 模块注释）
    forget_parent_snapshot(&state, &path);

    Json(json!({
        "status": "ok",
        "path": fs::normalize_display(&path),
        "size": encoded.len(),
        // 回传实际落盘风格，前端据此刷新状态栏（避免客户端与服务端推断不一致）
        "encoding": encoding.as_str(),
        "encoding_label": encoding.label(),
        "eol_type": eol.as_str(),
        "has_bom": bom && encoding != crate::core::text_codec::TextEncoding::Gbk,
    }))
    .into_response()
}

/// 现有文件的（编码, 换行, BOM）；读不出来或解不出来时退回 UTF-8 / LF / 无 BOM。
async fn read_style_snapshot(
    path: &std::path::Path,
) -> (
    crate::core::text_codec::TextEncoding,
    crate::core::text_codec::Eol,
    bool,
) {
    use crate::core::text_codec::{Eol, TextEncoding};
    match tokio::fs::read(path).await {
        Ok(bytes) => match crate::core::text_codec::decode(&bytes) {
            Ok(decoded) => (decoded.encoding, decoded.eol, decoded.has_bom),
            Err(_) => (TextEncoding::Utf8, Eol::Lf, false),
        },
        Err(_) => (TextEncoding::Utf8, Eol::Lf, false),
    }
}

/// 配置快照（属性页探测视频分辨率时需要 `media.ffmpeg_path`）。
pub async fn config_snapshot(state: &AppState) -> Config {
    state.config.read().await.clone()
}

// ---------------------------------------------------------------------------
// 上传 / 搜索
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// 目标目录（绝对路径，必须落在挂载点内）
    pub dir: String,
    /// 文件名（只允许单层名字，不含分隔符）
    pub name: String,
    /// 同名时是否覆盖
    #[serde(default)]
    pub overwrite: bool,
}

/// PUT /api/files/upload：单文件上传（前端对每个文件发一次请求，便于独立进度）。
///
/// v4.9.20 起逐帧流式落盘：旧实现的 `body: Bytes` 会把整个请求体（上限
/// `upload_max_size_mb`，0 = 4 GiB）先整包缓冲进内存再写盘，并发几个大
/// 上传就是成 GiB 的堆占用。`Content-Length` 可用时先做空间预检；客户端
/// 没带就边写边累计，超限即停（临时文件一并清掉）。
pub async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    // 单文件上限来自 `[media].upload_max_size_mb`，0 = 不限制（设计文档 3.2.3）
    let limit = {
        let config = state.config.read().await;
        config.media.upload_max_size_mb
    };
    let max_bytes = limit.saturating_mul(1024 * 1024);
    let declared: Option<u64> = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    if limit > 0 && declared.is_some_and(|size| size > max_bytes) {
        return fail_code(
            StatusCode::PAYLOAD_TOO_LARGE,
            "E_TOO_LARGE",
            format!("File exceeds the configured {limit} MiB upload limit"),
        );
    }

    // 文件名统一走 path_guard::validate_name：单层校验之外还拒 Windows
    // 保留设备名（CON/NUL/COM1…）、尾随点/空格与控制字符（安全审查 2026-11-02）
    let name = query.name.trim();
    if let Err(err) = crate::core::path_guard::validate_name(name) {
        return fail(StatusCode::BAD_REQUEST, err.message());
    }

    // 路径 / 只读 / 剩余空间 / 授权根：与 tus 分块上传共用同一套校验
    let dir = match resolve_upload_dir(&state, &query.dir, declared.unwrap_or(0)).await {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let target = dir.join(name);
    if target.exists() && !query.overwrite {
        return fail_code(
            StatusCode::CONFLICT,
            "E_CONFLICT",
            format!("File already exists: {name}"),
        );
    }

    // 目标路径纳入 per-path 锁：覆盖「存在性检查 → rename」整段，
    // 上传与删除/移动同名文件不再互踩（v4.9.20）。fail-fast，不排队。
    let _path_guard = match state.path_locks.acquire(std::slice::from_ref(&target)) {
        Ok(guard) => guard,
        Err(err) => return fs_error(err),
    };

    // 唯一临时名：`part_name` 自带 8 位随机后缀，同名并发上传不会互相覆盖；
    // 且带 `.asmoday-part.` 标记 → 列表过滤、启动清理、打包排除、raw 拦截全都认它
    // （v4.9.22 起统一命名，不再自造 `.xxx.asmoday-upload.tmp`）
    let tmp = dir.join(crate::core::path_guard::part_name(name));

    // 流式写盘：内存占用 = 单帧 chunk，与文件大小无关
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(file) => file,
        Err(e) => return fs_error(FsError::Io(format!("写入上传临时文件失败：{e}"))),
    };
    let mut written: u64 = 0;
    let mut body = body;
    loop {
        // 空闲超时（v4.9.22）：客户端断网 / 半开时 `frame()` 会一直等下去，
        // 而这段全程持着 per-path 锁 —— 用户重试同名上传只会拿到 E_BUSY，
        // 临时文件也永远留着。120 秒没有新数据就当作放弃（不是总时长限制）。
        let next = match tokio::time::timeout(UPLOAD_IDLE_TIMEOUT, body.frame()).await {
            Ok(next) => next,
            Err(_) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return fail_code(
                    StatusCode::REQUEST_TIMEOUT,
                    "E_UPLOAD_STALLED",
                    "upload stalled: no data received for 120s",
                );
            }
        };
        match next {
            Some(Ok(frame)) => {
                let mut chunk = match frame.into_data() {
                    Ok(chunk) => chunk,
                    // 非数据帧（trailers 等）直接跳过
                    Err(_) => continue,
                };
                written += chunk.len() as u64;
                if limit > 0 && written > max_bytes {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return fail_code(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "E_TOO_LARGE",
                        format!("File exceeds the configured {limit} MiB upload limit"),
                    );
                }
                if let Err(e) = file.write_all_buf(&mut chunk).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return fs_error(FsError::Io(format!("写入上传临时文件失败：{e}")));
                }
            }
            Some(Err(_)) => {
                // 客户端断开：半成品没有意义，清掉
                let _ = tokio::fs::remove_file(&tmp).await;
                return fail(StatusCode::BAD_REQUEST, "request body aborted");
            }
            None => break,
        }
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(FsError::Io(format!("写入上传临时文件失败：{e}")));
    }
    drop(file);

    // 落盘走 path_guard 的原子改名：no_replace 消除「检查后被人抢先创建」
    // 的 TOCTOU，replace 兼顾 Windows（std::fs::rename 在目标存在时失败）
    let overwrite = query.overwrite;
    let tmp_for_rename = tmp.clone();
    let target_for_rename = target.clone();
    let attempt = tokio::task::spawn_blocking(move || {
        if overwrite {
            path_guard::rename_replace(&tmp_for_rename, &target_for_rename)
        } else {
            path_guard::rename_no_replace(&tmp_for_rename, &target_for_rename)
        }
    })
    .await
    .unwrap_or_else(|_| Err(FsError::Io("rename task failed".to_string())));
    if let Err(e) = attempt {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(e);
    }
    // 覆盖上传（同名替换）同样不推进目录 mtime，必须主动失效；
    // 新建文件的场景目录 mtime 本来就会变，这一刀只是顺手，没有副作用
    forget_parent_snapshot(&state, &target);

    Json(json!({
        "status": "ok",
        "name": name,
        "path": fs::normalize_display(&target),
        "size": written,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub path: Option<String>,
    pub q: String,
    /// 是否递归子目录（默认 false，保持老客户端「只搜当前目录」的行为）
    pub recursive: Option<bool>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// GET /api/files/search：文件名模糊匹配，递归可选，游标分页（设计文档 4.6）。
///
/// 返回 `items` / `scanned` / `has_more` / `next_cursor`。没有「结果上限」：
/// 只要 `has_more` 为真，把 `next_cursor` 原样带回来就能接着往下搜。
pub async fn search(State(state): State<AppState>, Query(query): Query<SearchQuery>) -> Response {
    let keyword = query.q.trim().to_lowercase();
    if keyword.is_empty() {
        return Json(json!({
            "items": [],
            "scanned": 0,
            "has_more": false,
            "next_cursor": null,
            "recursive": query.recursive.unwrap_or(false),
        }))
        .into_response();
    }

    let roots = roots_for(&state).await;
    let requested = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let target = match requested {
        Some(raw) => match fs::resolve_existing(raw, &roots.allowed, roots.follow_symlinks) {
            Ok(path) => path,
            Err(err) => return fs_error(err),
        },
        None => {
            let Some(root) = roots.default_root() else {
                return fs_error(FsError::NotFound("没有可用的挂载点".to_string()));
            };
            let root_path = root.path.clone();
            match fs::resolve_existing(&root_path, &roots.allowed, roots.follow_symlinks) {
                Ok(path) => path,
                Err(err) => return fs_error(err),
            }
        }
    };

    if !target.is_dir() {
        return fail(StatusCode::BAD_REQUEST, "Search target is not a directory");
    }

    let recursive = query.recursive.unwrap_or(false);
    let options = crate::core::search::SearchOptions {
        keyword,
        limit: query.limit.unwrap_or(crate::core::search::DEFAULT_LIMIT),
        recursive,
        follow_symlinks: roots.follow_symlinks,
        budget: crate::core::search::DEFAULT_BUDGET,
    };

    let page = match crate::core::search::run(
        &target,
        &options,
        query.cursor.as_deref(),
        Some(state.dir_cache.as_ref()),
    )
    .await
    {
        Ok(page) => page,
        Err(err) => return fs_error(err),
    };

    let items: Vec<_> = page
        .hits
        .iter()
        .map(|hit| {
            let ext = fs::extension_of_name(&hit.name);
            let (kind, editable) = fs::classify_name(&hit.name, &ext, hit.is_dir);
            json!({
                "name": hit.name,
                "path": fs::normalize_display(&hit.abs),
                // 相对搜索根的 POSIX 路径：结果列表要告诉用户「这条在哪个子目录」
                "rel": hit.rel,
                "is_dir": hit.is_dir,
                "size": hit.size,
                "kind": kind,
                "ext": ext,
                "editable": editable,
            })
        })
        .collect();

    Json(json!({
        "items": items,
        "scanned": page.scanned,
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
        "recursive": recursive,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// 删除（设计文档 3.2 右键菜单「删除」）
// ---------------------------------------------------------------------------

/// DELETE /api/files/raw?path=…：删除文件或目录（目录递归）
pub async fn delete(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    let set = roots_for(&state).await;
    // 只读拦截与 `POST /api/files/delete` 同口径（安全审查 2026-11-04）：
    // 这条老口子此前绕过了 readonly —— 只读挂载不能删，与不能写是同一个承诺。
    let mounts = mounts_for(&state).await;
    if let Some(mount) = mounts.find_by_path_text(query.path.trim())
        && let Err(err) = mount.ensure_writable()
    {
        return fs_error(err);
    }
    // 最后一段**不跟随**：软链接本身才是删除目标。若对整条路径 canonicalize，
    // 指向目录的链接会被解析成目标实体，下面 remove_dir_all 就把目标整个删掉了
    // （设计文档 3.2.7 红线：删链接严禁递归删目标）。
    let raw = Path::new(query.path.trim());
    let target = match raw.parent().zip(raw.file_name()) {
        Some((parent, name)) if !parent.as_os_str().is_empty() => {
            let parent_text = parent.to_string_lossy();
            match fs::resolve_existing(&parent_text, &set.allowed, set.follow_symlinks) {
                Ok(parent_canon) => parent_canon.join(name),
                Err(err) => return fs_error(err),
            }
        }
        // 没有父段（删挂载点根这类）维持原语义，下面的根目录检查会拒绝
        _ => match fs::resolve_existing(&query.path, &set.allowed, set.follow_symlinks) {
            Ok(path) => path,
            Err(err) => return fs_error(err),
        },
    };

    // 挂载点根目录本身不允许删（避免一次误操作把整个库清掉）
    let canonical = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    if set.allowed.iter().any(|root| root == &canonical) {
        return fs_error(FsError::Forbidden(
            "不能删除挂载点根目录，请先删除里面的内容".to_string(),
        ));
    }

    let meta = match tokio::fs::symlink_metadata(&target).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取目标信息失败：{e}"))),
    };

    // per-path 锁与 mutate 版同口径：删同名目标时不能和复制/移动交叉
    let _guard = match state.path_locks.acquire(std::slice::from_ref(&target)) {
        Ok(guard) => guard,
        Err(err) => return fs_error(err),
    };

    let result = if meta.file_type().is_symlink() {
        // 只删链接本身：Windows 的目录链接要用 remove_dir 才能摘掉 reparse point
        std::fs::remove_file(&target).or_else(|err| {
            if cfg!(windows) {
                std::fs::remove_dir(&target).map_err(|_| err)
            } else {
                Err(err)
            }
        })
    } else if meta.is_dir() {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };

    match result {
        Ok(()) => {
            // 与 /api/files/delete 同口径：缩略图缓存、色值、收藏/历史/分享一起清。
            // 只清缓存不联动数据库会留下「DB 说有、磁盘没有」的死记录：
            // 分享不会转 tombstone（访客拿到 404 而不是 410），收藏点开即 404，
            // 30 天 tombstone 清理也永远不会触发（v4.9.22）。
            let display = fs::normalize_display(&target);
            crate::api::files_mutate::purge_thumb_cache(&state, &display);
            if let Err(err) = state
                .auth
                .forget_meta_paths(&display, crate::core::auth::now_secs())
            {
                crate::log_error!("failed to purge metadata after delete: {err}");
            }
            Json(json!({ "status": "ok" })).into_response()
        }
        Err(e) => fs_error(FsError::Io(format!("删除失败：{e}"))),
    }
}

// ---------------------------------------------------------------------------
// 外挂字幕（设计文档 3.4：影视库支持 .srt / .vtt）
// ---------------------------------------------------------------------------

/// GET /api/files/subtitle?path=…：统一输出 WebVTT（浏览器只认 vtt，srt 现场转）
pub async fn subtitle(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    let set = roots_for(&state).await;
    let path = match fs::resolve_existing(&query.path, &set.allowed, set.follow_symlinks) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    let ext = path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext != "srt" && ext != "vtt" {
        return fs_error(FsError::BadRequest("只支持 .srt / .vtt 字幕".to_string()));
    }

    let raw = match tokio::fs::read(&path).await {
        Ok(bytes) => decode_subtitle_text(&bytes),
        Err(e) => return fs_error(FsError::Io(format!("读取字幕失败：{e}"))),
    };
    let body = if ext == "srt" {
        srt_to_vtt(&raw)
    } else {
        ensure_vtt_header(&raw)
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response()
}

/// 字幕文本解码：**UTF-8 优先，失败按 GBK/GB18030 重解**（设计文档 3.4.7 第 3 条）。
///
/// `.srt` 在国内资源里极大概率是 GBK；直接 `from_utf8_lossy` 会得到满屏替换字符，
/// 用户看到的是「字幕乱码」而不是「字幕编码不支持」，所以必须先探一次。
fn decode_subtitle_text(bytes: &[u8]) -> String {
    // UTF-8 BOM：剥掉再解，否则首行会带上 \u{feff}
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).to_string();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    // encoding_rs 的 GBK 解码器同时覆盖 GB18030，够用于简体中文字幕
    let (text, _, _) = encoding_rs::GBK.decode(bytes);
    text.into_owned()
}

/// SRT → WebVTT：加文件头，时间行的 `,` 改成 `.`
fn srt_to_vtt(input: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for line in input.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// 已经是 vtt 但没有文件头时补一行
fn ensure_vtt_header(input: &str) -> String {
    let trimmed = input.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("WEBVTT") {
        trimmed.to_string()
    } else {
        format!("WEBVTT\n\n{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_decoding_handles_utf8_gbk_and_bom() {
        // 纯 UTF-8：原样返回
        assert_eq!(decode_subtitle_text("你好".as_bytes()), "你好");
        // GBK：「你好」= C4 E3 BA C3，非法 UTF-8 → 走 GBK 兜底
        assert_eq!(decode_subtitle_text(&[0xC4, 0xE3, 0xBA, 0xC3]), "你好");
        // UTF-8 BOM 必须剥掉，否则首行字幕会带一个不可见字符
        assert_eq!(decode_subtitle_text(&[0xEF, 0xBB, 0xBF, b'a']), "a");
    }

    #[test]
    fn srt_becomes_valid_vtt() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n你好\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000"));
    }
}
