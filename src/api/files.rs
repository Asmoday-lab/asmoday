//! 文件管理接口（设计文档 6.1）。
//!
//! - `GET  /api/files/mounts`：挂载点清单（id / name / path / readonly / online）
//! - `GET  /api/files/roots`：同上，兼容字段（roots / implicit / default）
//! - `GET  /api/files/list?path=&page=`：目录列表，超 1000 条强制分页并返回 has_more
//! - `GET  /api/files/raw?path=`：流式读取，支持 HTTP Range（206 / 416）
//! - `PUT  /api/files/raw?path=`：编辑器保存写回（仅文本类、原子替换）

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::core::config::Config;
use crate::core::fs::{self, FsError, RootSet};
use crate::core::path_guard;
use crate::AppState;

/// 编辑器单次保存上限（8 MiB）
const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
    pub page: Option<usize>,
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

async fn roots_for(state: &AppState) -> RootSet {
    let config = state.config.read().await;
    fs::load_roots(&config, &state.cwd)
}

/// 挂载点集合（带 readonly / online，唯一真源在 core::path_guard）
async fn mounts_for(state: &AppState) -> path_guard::MountSet {
    let config = state.config.read().await;
    path_guard::load(&config, &state.cwd)
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
    Json(json!({ "mounts": items, "implicit": set.implicit })).into_response()
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
    let path = match fs::resolve_existing(&query.path, &set.allowed) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取文件信息失败：{e}"))),
    };
    if meta.is_dir() {
        return fs_error(FsError::BadRequest("目标是目录，无法直接读取".to_string()));
    }

    let total = meta.len();
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();

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
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {}-{}/{}", range.start, range.end, total),
                    ),
                    (header::CONTENT_LENGTH, range.len().to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
        None => {
            let file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(e) => return fs_error(FsError::Io(format!("打开文件失败：{e}"))),
            };
            let stream = ReaderStream::new(file);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, total.to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}

/// GET /api/files/text：编辑器专用的 JSON 读取（设计文档 3.2.2.1）。
///
/// 与 `/api/files/raw` 的区别是**带元信息**：字符编码、换行符风格、是否有 BOM。
/// 内容里的换行符统一成 `\n` 传输，保存时由服务端按 `eol_type` 还原。
pub async fn read_text(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    let set = mounts_for(&state).await;
    let Some(mount) = set.find_by_path_text(&query.path) else {
        return fs_error(FsError::NotFound(format!("路径不在任何挂载点内：{}", query.path)));
    };
    let path = match path_guard::resolve_existing(mount, &query.path) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta,
        Err(e) => return fs_error(FsError::Io(format!("读取文件信息失败：{e}"))),
    };
    if meta.is_dir() {
        return fail(StatusCode::BAD_REQUEST, "目标是目录，无法用编辑器打开");
    }
    if meta.len() as usize > MAX_EDIT_BYTES {
        return fail_code(
            StatusCode::PAYLOAD_TOO_LARGE,
            "E_TOO_LARGE",
            format!("文件超过编辑器上限（{} MiB）", MAX_EDIT_BYTES / 1024 / 1024),
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
            "内置编辑器只支持文本类文件",
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
            format!("文件超过编辑器上限（{} MiB）", MAX_EDIT_BYTES / 1024 / 1024),
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
        return fail(StatusCode::BAD_REQUEST, "目标是目录，无法写入");
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = fs::extension_of_name(&name);
    let (kind, editable) = fs::classify_name(&name, &ext, false);
    if kind != "text" || !editable {
        return fail(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "内置编辑器只允许写回文本类文件",
        );
    }

    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => {
            return fail_code(StatusCode::BAD_REQUEST, "NOT_TEXT", "Content is not valid UTF-8 text");
        }
    };

    // 目标风格：显式参数优先，其次沿用磁盘上现有文件的编码 / 换行 / BOM
    let existing = read_style_snapshot(&path).await;
    let encoding = match query.encoding.as_deref() {
        Some(raw) => match crate::core::text_codec::TextEncoding::parse(raw) {
            Some(encoding) => encoding,
            None => return fail(StatusCode::BAD_REQUEST, format!("不支持的编码：{raw}")),
        },
        None => existing.0,
    };
    let eol = match query.eol.as_deref() {
        Some(raw) => match crate::core::text_codec::Eol::parse(raw) {
            Some(eol) => eol,
            None => return fail(StatusCode::BAD_REQUEST, format!("不支持的换行符：{raw}")),
        },
        None => existing.1,
    };
    let bom = match query.bom.as_deref() {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => return fail(StatusCode::BAD_REQUEST, format!("bom 参数不合法：{other}")),
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

    // 原子替换：同目录临时文件 + rename，避免写一半断电导致文件损坏
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.asmoday-save.tmp"));

    if let Err(e) = tokio::fs::write(&tmp, &encoded).await {
        return fs_error(FsError::Io(format!("写入临时文件失败：{e}")));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(FsError::Io(format!("替换文件失败：{e}")));
    }

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
) -> (crate::core::text_codec::TextEncoding, crate::core::text_codec::Eol, bool) {
    use crate::core::text_codec::{Eol, TextEncoding};
    match tokio::fs::read(path).await {
        Ok(bytes) => match crate::core::text_codec::decode(&bytes) {
            Ok(decoded) => (decoded.encoding, decoded.eol, decoded.has_bom),
            Err(_) => (TextEncoding::Utf8, Eol::Lf, false),
        },
        Err(_) => (TextEncoding::Utf8, Eol::Lf, false),
    }
}

/// 供其它模块复用的配置快照读取（阶段三终端联动会用到挂载点校验）。
#[allow(dead_code)]
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

/// PUT /api/files/upload：单文件上传（前端对每个文件发一次请求，便于独立进度）
pub async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    // 单文件上限来自 `[media].upload_max_size_mb`，0 = 不限制（设计文档 3.2.3）
    let limit = {
        let config = state.config.read().await;
        config.media.upload_max_size_mb
    };
    if limit > 0 {
        let max_bytes = limit.saturating_mul(1024 * 1024);
        if body.len() as u64 > max_bytes {
            return fail_code(
                StatusCode::PAYLOAD_TOO_LARGE,
                "E_TOO_LARGE",
                format!("File exceeds the configured {limit} MiB upload limit"),
            );
        }
    }

    // 文件名只允许单层：拒绝分隔符与上跳
    let name = query.name.trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
    {
        return fail(StatusCode::BAD_REQUEST, "文件名不合法");
    }

    let roots = roots_for(&state).await;
    let dir = match fs::resolve_existing(&query.dir, &roots.allowed) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    if !dir.is_dir() {
        return fail(StatusCode::BAD_REQUEST, "上传目标不是目录");
    }

    let target = dir.join(name);
    if target.exists() && !query.overwrite {
        return fail_code(StatusCode::CONFLICT, "E_CONFLICT", format!("File already exists: {name}"));
    }

    // 上传前检查目标挂载点剩余空间（设计文档 3.2.3）
    if let Err(err) = crate::core::fs_ops::ensure_space(&dir, body.len() as u64) {
        return fs_error(err);
    }

    // 二次确认：拼接后的真实路径仍在授权根内
    if let Ok(canonical_parent) = std::fs::canonicalize(&dir) {
        if !roots.allowed.iter().any(|root| canonical_parent.starts_with(root)) {
            return fail_code(StatusCode::FORBIDDEN, "E_FORBIDDEN", "Upload target is outside the allowed roots");
        }
    }

    let tmp = dir.join(format!(".{name}.asmoday-upload.tmp"));
    if let Err(e) = tokio::fs::write(&tmp, &body).await {
        return fs_error(FsError::Io(format!("写入上传临时文件失败：{e}")));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return fs_error(FsError::Io(format!("落盘失败：{e}")));
    }

    Json(json!({
        "status": "ok",
        "name": name,
        "path": fs::normalize_display(&target),
        "size": body.len(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub path: Option<String>,
    pub q: String,
}

/// GET /api/files/search：仅当前目录的文件名模糊匹配（设计文档 3.2 的「轻量搜索」）
pub async fn search(State(state): State<AppState>, Query(query): Query<SearchQuery>) -> Response {
    let keyword = query.q.trim().to_lowercase();
    if keyword.is_empty() {
        return Json(json!({ "items": [], "truncated": false })).into_response();
    }

    let roots = roots_for(&state).await;
    let requested = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let target = match requested {
        Some(raw) => match fs::resolve_existing(raw, &roots.allowed) {
            Ok(path) => path,
            Err(err) => return fs_error(err),
        },
        None => {
            let Some(root) = roots.default_root() else {
                return fs_error(FsError::NotFound("没有可用的挂载点".to_string()));
            };
            let root_path = root.path.clone();
            match fs::resolve_existing(&root_path, &roots.allowed) {
                Ok(path) => path,
                Err(err) => return fs_error(err),
            }
        }
    };

    if !target.is_dir() {
        return fail(StatusCode::BAD_REQUEST, "搜索目标不是目录");
    }

    const LIMIT: usize = 200;
    let mut items = Vec::new();
    let mut truncated = false;

    let mut reader = match tokio::fs::read_dir(&target).await {
        Ok(reader) => reader,
        Err(e) => return fs_error(FsError::Io(format!("读取目录失败：{e}"))),
    };

    while let Ok(Some(entry)) = reader.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_lowercase().contains(&keyword) {
            continue;
        }
        if items.len() >= LIMIT {
            truncated = true;
            break;
        }

        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        let path = entry.path();
        let meta = entry.metadata().await.ok();
        let ext = fs::extension_of_name(&name);
        let (kind, editable) = fs::classify_name(&name, &ext, is_dir);

        items.push(json!({
            "name": name,
            "path": fs::normalize_display(&path),
            "is_dir": is_dir,
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            "kind": kind,
            "ext": ext,
            "editable": editable,
        }));
    }

    Json(json!({ "items": items, "truncated": truncated })).into_response()
}

// ---------------------------------------------------------------------------
// 删除（设计文档 3.2 右键菜单「删除」）
// ---------------------------------------------------------------------------

/// DELETE /api/files/raw?path=…：删除文件或目录（目录递归）
pub async fn delete(State(state): State<AppState>, Query(query): Query<PathQuery>) -> Response {
    let set = roots_for(&state).await;
    let target = match fs::resolve_existing(&query.path, &set.allowed) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
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

    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };

    match result {
        Ok(()) => Json(json!({ "status": "ok" })).into_response(),
        Err(e) => fs_error(FsError::Io(format!("删除失败：{e}"))),
    }
}

// ---------------------------------------------------------------------------
// 外挂字幕（设计文档 3.4：影视库支持 .srt / .vtt）
// ---------------------------------------------------------------------------

/// GET /api/files/subtitle?path=…：统一输出 WebVTT（浏览器只认 vtt，srt 现场转）
pub async fn subtitle(
    State(state): State<AppState>,
    Query(query): Query<PathQuery>,
) -> Response {
    let set = roots_for(&state).await;
    let path = match fs::resolve_existing(&query.path, &set.allowed) {
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
    let body = if ext == "srt" { srt_to_vtt(&raw) } else { ensure_vtt_header(&raw) };

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
