//! 分享与图床直链（设计文档 3.1）。
//!
//! - 分享令牌随机生成，数据库只存令牌与可选密码的 Argon2 哈希；
//! - 支持有效期、是否允许下载、目录分享的子路径访问；
//! - 公开访问走 `/api/public/share/*`，不经过登录中间件，但会校验
//!   令牌、密码、有效期与 `[shares].referer_allowlist` 防盗链。

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::core::auth::{hash_password, now_secs, random_token, verify_password};
use crate::core::fs;
use crate::AppState;

/// 细节型错误（4xx 参数类）
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2）
fn error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

#[derive(Debug, Deserialize)]
pub struct CreateShare {
    pub path: String,
    #[serde(default)]
    pub password: Option<String>,
    /// 有效天数，0 或省略 = 永久
    #[serde(default)]
    pub days: u32,
    #[serde(default = "default_true")]
    pub allow_download: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ShareAuth {
    #[serde(default)]
    pub password: Option<String>,
    /// 目录分享的相对子路径（限定在分享根内）
    #[serde(default)]
    pub sub: Option<String>,
}

/// POST /api/shares
pub async fn create(State(state): State<AppState>, Json(body): Json<CreateShare>) -> Response {
    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };

    let target = match fs::resolve_existing(&body.path, &roots.allowed) {
        Ok(path) => path,
        Err(err) => return crate::api::error::fs_error(&err),
    };

    let is_folder = target.is_dir();
    let password_hash = match body.password.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(password) => match hash_password(password) {
            Ok(hash) => Some(hash),
            Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
        },
        None => None,
    };

    let token = random_token()[..16].to_string();
    let expires_at = if body.days == 0 {
        None
    } else {
        Some(now_secs() + i64::from(body.days) * 86_400)
    };

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let result = conn.execute(
        "INSERT INTO shares (token, file_path, is_folder, password_hash, allow_download, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            token,
            fs::normalize_display(&target),
            if is_folder { 1 } else { 0 },
            password_hash,
            if body.allow_download { 1 } else { 0 },
            expires_at,
            now_secs()
        ],
    );

    match result {
        Ok(_) => Json(json!({
            "status": "ok",
            "token": token,
            "url": format!("/s/{token}"),
            "is_folder": is_folder,
            "has_password": password_hash.is_some(),
            "expires_at": expires_at,
        }))
        .into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("创建分享失败：{err}")),
    }
}

/// GET /api/shares
pub async fn list(State(state): State<AppState>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let mut statement = match conn.prepare(
        "SELECT token, file_path, is_folder, password_hash IS NOT NULL, allow_download, expires_at, created_at
         FROM shares ORDER BY created_at DESC",
    ) {
        Ok(statement) => statement,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("查询失败：{err}")),
    };

    let rows = statement.query_map([], |row| {
        Ok(json!({
            "token": row.get::<_, String>(0)?,
            "path": row.get::<_, String>(1)?,
            "is_folder": row.get::<_, i64>(2)? != 0,
            "has_password": row.get::<_, i64>(3)? != 0,
            "allow_download": row.get::<_, i64>(4)? != 0,
            "expires_at": row.get::<_, Option<i64>>(5)?,
            "created_at": row.get::<_, Option<i64>>(6)?,
        }))
    });

    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() })).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("读取失败：{err}")),
    }
}

/// DELETE /api/shares/{token}
pub async fn delete(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    match conn.execute("DELETE FROM shares WHERE token = ?1", [&token]) {
        Ok(0) => error_code(StatusCode::NOT_FOUND, "SHARE_NOT_FOUND", "Share not found"),
        Ok(_) => Json(json!({ "status": "ok" })).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败：{err}")),
    }
}

/// 分享记录（file_path, is_folder, password_hash, allow_download, expires_at）
type ShareRow = (String, bool, Option<String>, bool, Option<i64>);

fn load_share(state: &AppState, token: &str) -> Result<ShareRow, Response> {
    let conn = state
        .auth_connection()
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?;

    conn.query_row(
        "SELECT file_path, is_folder, password_hash, allow_download, expires_at FROM shares WHERE token = ?1",
        [token],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, Option<i64>>(4)?,
            ))
        },
    )
    .map_err(|_| error_code(StatusCode::NOT_FOUND, "SHARE_NOT_FOUND", "Share not found"))
}

fn check_expiry(expires_at: Option<i64>) -> Result<(), Response> {
    if let Some(expires) = expires_at {
        if expires <= now_secs() {
            return Err(error_code(StatusCode::GONE, "SHARE_EXPIRED", "Share link expired"));
        }
    }
    Ok(())
}

/// 防盗链：`[shares].referer_allowlist` 非空时，带 Referer 的请求其主机
/// 必须命中白名单（精确或子域后缀）；无 Referer 的直接打开不受影响。
async fn hotlink_denied(state: &AppState, headers: &HeaderMap) -> bool {
    let allowlist = {
        let config = state.config.read().await;
        config.shares.referer_allowlist.clone()
    };
    if allowlist.is_empty() {
        return false;
    }

    let Some(referer) = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    let host = referer
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .and_then(|authority| authority.rsplit('@').next())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let host = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量 [::1]:8080
        rest.split(']').next().unwrap_or("").to_string()
    } else if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or("").to_string()
    } else {
        host
    };

    if host.is_empty() {
        return false;
    }

    !allowlist.iter().any(|rule| {
        let rule = rule.trim().to_ascii_lowercase();
        !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}")))
    })
}

/// 校验分享口令（缺省/错误都返回 401，但错误码不同，前端据此区分文案）
fn check_password(password_hash: &Option<String>, query: &ShareAuth) -> Result<(), Response> {
    if let Some(hash) = password_hash {
        let provided = query.password.as_deref().unwrap_or("");
        if !verify_password(hash, provided) {
            // 没填 = 需要口令；填了但不匹配 = 口令错误
            let code = if provided.is_empty() {
                "SHARE_PASSWORD_REQUIRED"
            } else {
                "SHARE_PASSWORD_WRONG"
            };
            return Err(error_code(StatusCode::UNAUTHORIZED, code, "Share password required"));
        }
    }
    Ok(())
}

/// 解析目录分享的相对子路径：拒绝绝对路径、`..` 与反斜杠，
/// canonicalize 后强制仍在分享根内（路径穿越第二道闸）。
fn resolve_sub(root: &std::path::Path, sub: Option<&str>) -> Result<PathBuf, Response> {
    let Some(sub) = sub.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(root.to_path_buf());
    };
    if sub.starts_with('/') || sub.starts_with('\\') || sub.contains('\\') || sub.contains('\0') {
        return Err(error(StatusCode::BAD_REQUEST, "子路径不合法"));
    }
    let mut current = root.to_path_buf();
    for part in sub.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(error(StatusCode::FORBIDDEN, "子路径不允许上跳"));
        }
        current.push(part);
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| error(StatusCode::NOT_FOUND, "分享目标已不存在"))?;
    let canonical = std::fs::canonicalize(&current)
        .map_err(|_| error(StatusCode::NOT_FOUND, "子路径不存在"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(error(StatusCode::FORBIDDEN, "子路径越出分享范围"));
    }
    Ok(canonical)
}

/// 公开接口的公共前置：有效期 + 防盗链 + 口令
async fn guard(
    state: &AppState,
    headers: &HeaderMap,
    expires_at: Option<i64>,
    password_hash: &Option<String>,
    query: &ShareAuth,
) -> Result<(), Response> {
    check_expiry(expires_at)?;
    if hotlink_denied(state, headers).await {
        return Err(error(StatusCode::FORBIDDEN, "直链来源不在防盗链白名单内"));
    }
    check_password(password_hash, query)
}

/// GET /api/public/share/{token}：分享元信息（公开）
pub async fn info(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareAuth>,
    headers: HeaderMap,
) -> Response {
    let (path, is_folder, password_hash, allow_download, expires_at) = match load_share(&state, &token) {
        Ok(share) => share,
        Err(response) => return response,
    };
    if let Err(response) = guard(&state, &headers, expires_at, &password_hash, &query).await {
        return response;
    }

    let name = path.split('/').filter(|s| !s.is_empty()).next_back().unwrap_or(&path).to_string();

    Json(json!({
        "name": name,
        "path": path,
        "is_folder": is_folder,
        "has_password": password_hash.is_some(),
        "allow_download": allow_download,
        "expires_at": expires_at,
    }))
    .into_response()
}

/// GET /api/public/share/{token}/raw：内联预览 / 图床直链（公开，支持 Range）。
/// 不受 allow_download 约束——直链的本职就是被外部引用；下载走 download 端点。
pub async fn raw(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareAuth>,
    headers: HeaderMap,
) -> Response {
    let (path, is_folder, password_hash, _, expires_at) = match load_share(&state, &token) {
        Ok(share) => share,
        Err(response) => return response,
    };
    if let Err(response) = guard(&state, &headers, expires_at, &password_hash, &query).await {
        return response;
    }
    if is_folder {
        return error_code(StatusCode::BAD_REQUEST, "SHARE_NO_RAW", "Directory share has no direct link");
    }

    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };
    let target = match fs::resolve_existing(&path, &roots.allowed) {
        Ok(target) => target,
        Err(err) => return crate::api::error::fs_error(&err),
    };

    stream_file(target, &headers, false).await
}

/// GET /api/public/share/{token}/download：下载（公开，受 allow_download 约束，目录支持 ?sub=）
pub async fn download(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareAuth>,
    headers: HeaderMap,
) -> Response {
    let (path, is_folder, password_hash, allow_download, expires_at) = match load_share(&state, &token) {
        Ok(share) => share,
        Err(response) => return response,
    };
    if let Err(response) = guard(&state, &headers, expires_at, &password_hash, &query).await {
        return response;
    }
    if !allow_download {
        return error_code(StatusCode::FORBIDDEN, "SHARE_DOWNLOAD_DISABLED", "Download disabled for this share");
    }

    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };
    let root = match fs::resolve_existing(&path, &roots.allowed) {
        Ok(target) => target,
        Err(err) => return crate::api::error::fs_error(&err),
    };

    // 目录分享允许 ?sub= 相对路径下载其中的文件
    let target = if is_folder {
        match resolve_sub(&root, query.sub.as_deref()) {
            Ok(target) if target.is_file() => target,
            Ok(_) => return error(StatusCode::BAD_REQUEST, "子路径不是文件"),
            Err(response) => return response,
        }
    } else {
        root
    };

    stream_file(target, &headers, true).await
}

/// GET /api/public/share/{token}/list：目录分享条目（公开，支持 ?sub= 浏览子目录）
pub async fn list_entries(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareAuth>,
    headers: HeaderMap,
) -> Response {
    let (path, is_folder, password_hash, _, expires_at) = match load_share(&state, &token) {
        Ok(share) => share,
        Err(response) => return response,
    };
    if let Err(response) = guard(&state, &headers, expires_at, &password_hash, &query).await {
        return response;
    }
    if !is_folder {
        return error_code(StatusCode::BAD_REQUEST, "SHARE_NOT_DIR", "Share is not a directory");
    }

    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };
    let root = match fs::resolve_existing(&path, &roots.allowed) {
        Ok(target) => target,
        Err(err) => return crate::api::error::fs_error(&err),
    };
    let target = match resolve_sub(&root, query.sub.as_deref()) {
        Ok(target) if target.is_dir() => target,
        Ok(_) => return error(StatusCode::BAD_REQUEST, "子路径不是目录"),
        Err(response) => return response,
    };

    // 相对子路径（POSIX 风格），前端据此拼 raw/download 链接
    let sub = query
        .sub
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();

    match fs::list_dir(&target, 1).await {
        Ok(listing) => {
            let entries: Vec<_> = listing
                .entries
                .iter()
                .map(|entry| {
                    let entry_sub = if sub.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{}/{}", sub.trim_matches('/'), entry.name)
                    };
                    json!({
                        "name": entry.name,
                        "is_dir": entry.is_dir,
                        "size": entry.size,
                        "modified": entry.modified,
                        "kind": entry.kind,
                        "ext": entry.ext,
                        "sub": entry_sub,
                    })
                })
                .collect();
            Json(json!({
                "path": listing.path,
                "sub": sub,
                "total": listing.total,
                "has_more": listing.has_more,
                "entries": entries,
            }))
            .into_response()
        }
        Err(err) => crate::api::error::fs_error(&err),
    }
}

/// 流式返回文件（Range 支持；download 模式附带附件头）
async fn stream_file(target: PathBuf, headers: &HeaderMap, as_attachment: bool) -> Response {
    let meta = match tokio::fs::metadata(&target).await {
        Ok(meta) => meta,
        Err(_) => return error(StatusCode::NOT_FOUND, "文件已不存在"),
    };
    if meta.is_dir() {
        return error(StatusCode::BAD_REQUEST, "目标是目录");
    }

    let total = meta.len();
    let mime = mime_guess::from_path(&target)
        .first_or_octet_stream()
        .to_string();
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let disposition = if as_attachment { "attachment" } else { "inline" };

    let mut headers_out: Vec<(&'static str, String)> = vec![
        ("content-type", mime),
        ("accept-ranges", "bytes".to_string()),
        ("cache-control", "no-store".to_string()),
        (
            "content-disposition",
            format!("{disposition}; filename*=UTF-8''{}", percent_encode(&name)),
        ),
    ];

    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());

    let parsed = match range_header {
        Some(spec) => match fs::parse_range(spec, total) {
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

    let mut response = match parsed {
        Some(range) => {
            let mut file = match tokio::fs::File::open(&target).await {
                Ok(file) => file,
                Err(_) => return error(StatusCode::NOT_FOUND, "文件已不存在"),
            };
            if file.seek(std::io::SeekFrom::Start(range.start)).await.is_err() {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "定位文件失败");
            }
            let limited = file.take(range.len());
            headers_out.push(("content-range", format!("bytes {}-{}/{}", range.start, range.end, total)));
            headers_out.push(("content-length", range.len().to_string()));
            (StatusCode::PARTIAL_CONTENT, Body::from_stream(ReaderStream::new(limited))).into_response()
        }
        None => {
            let file = match tokio::fs::File::open(&target).await {
                Ok(file) => file,
                Err(_) => return error(StatusCode::NOT_FOUND, "文件已不存在"),
            };
            headers_out.push(("content-length", total.to_string()));
            (StatusCode::OK, Body::from_stream(ReaderStream::new(file))).into_response()
        }
    };

    for (key, value) in headers_out {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(key, value);
        }
    }
    response
}

fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
