//! WebDAV 服务端（设计文档 v2.5.0 3.10 / 6.6）。
//!
//! - 认证：HTTP Basic，用户名 = Token 名，密码 = `asm_` 前缀的 App Token 明文；
//!   校验哈希 + 过期，失败计入**独立**防爆破池（与登录 / 分享互相隔离）；
//! - 路由：`/dav/<mount_id>/<path>`，路径经 `path_guard` canonicalize，禁止逃逸与跨挂载点；
//! - 方法：OPTIONS / PROPFIND / GET / HEAD / PUT / DELETE / MKCOL / MOVE / COPY / LOCK / UNLOCK；
//! - 写方法被只读 Token 与只读挂载点双重拦截（403）；
//! - PUT 流式落盘（`.asmoday-part.<8位随机>` 临时文件 + 原子重命名，绝不 `to_bytes`）；
//! - GET/HEAD 支持 Range 分段（视频拖拽），流式回传；
//! - LOCK/UNLOCK 为伪锁：只回随机 Lock-Token，串行化依托 per-path 锁。

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use data_encoding::BASE64;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt, SeekFrom};
use tokio_util::io::ReaderStream;

use crate::core::fs;
use crate::core::{fs_ops, path_guard};
use crate::AppState;

const PREFIX: &str = "/dav";
/// PROPFIND 单层条目上限（防客户端一次拉爆 CPU/IO，设计文档 3.10）
const PROPFIND_LIMIT: usize = 1000;

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Asmoday WebDAV\"")],
        "Unauthorized",
    )
        .into_response()
}

fn dav_error(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

struct Credentials {
    username: String,
    token: String,
}

fn basic_credentials(headers: &HeaderMap) -> Option<Credentials> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = BASE64.decode(encoded.trim().as_bytes()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, token) = text.split_once(':')?;
    // 设计文档 3.10：用户名 = token_name（不再是「随意填」）
    Some(Credentials {
        username: username.trim().to_string(),
        token: token.to_string(),
    })
}

fn percent_decode_segment(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode_segment(input: &str) -> String {
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

/// 把 `/dav/<mount_id>/<rest>` 拆成（mount_id, 剩余路径）。
/// **先按 `/` 分段再逐段解码**：`%2F` 解码出的分隔符不会改变路由结构。
fn split_route(uri_path: &str) -> Result<(Option<String>, String), Response> {
    let rest = uri_path.strip_prefix(PREFIX).unwrap_or("");
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Ok((None, String::new()));
    }
    let mut segments: Vec<String> = Vec::new();
    for raw_seg in rest.split('/') {
        let decoded = percent_decode_segment(raw_seg)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "路径编码非法").into_response())?;
        // 显式拒绝 `.` / `..`（%2e%2e 也逃不掉），穿越防护交给 canonicalize
        if decoded == "." || decoded == ".." {
            return Err((StatusCode::FORBIDDEN, "禁止路径穿越").into_response());
        }
        segments.push(decoded);
    }
    let tail = segments.split_off(1).join("/");
    Ok((Some(segments.into_iter().next().unwrap_or_default()), tail))
}

/// 解析 Destination 头：绝对 URL 或绝对路径 → 同 `split_route`
fn parse_destination(value: &str) -> Result<(Option<String>, String), Response> {
    let path = match value.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|idx| &rest[idx..]).unwrap_or("/"),
        None => value,
    };
    split_route(path)
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn entry_xml(href: &str, name: &str, is_dir: bool, size: u64, mtime: i64) -> String {
    let rfc1123 = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.max(0) as u64),
    );
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>\
<D:displayname>{}</D:displayname>\
<D:resourcetype>{}</D:resourcetype>\
<D:getcontentlength>{}</D:getcontentlength>\
<D:getcontenttype>{}</D:getcontenttype>\
<D:getlastmodified>{}</D:getlastmodified>\
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        escape_xml(href),
        escape_xml(name),
        if is_dir { "<D:collection/>" } else { "" },
        size,
        if is_dir {
            "httpd/unix-directory".to_string()
        } else {
            mime_guess::from_path(name)
                .first_or_octet_stream()
                .to_string()
        },
        rfc1123,
    )
}

/// 解析单段 `Range: bytes=a-b`（含 `bytes=-n` 后缀式）。成功返回闭区间 (start, end)。
fn parse_range(header_value: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, Response> {
    let Some(value) = header_value else {
        return Ok(None);
    };
    if len == 0 {
        return Err(dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "空文件无法分段"));
    }
    let spec = value
        .trim()
        .strip_prefix("bytes=")
        .ok_or_else(|| dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "仅支持 bytes 单段范围"))?;
    // 多段范围直接按不支持处理（客户端会回退整文件）
    if spec.contains(',') {
        return Err(dav_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "不支持多段 Range",
        ));
    }
    let (start_s, end_s) = spec
        .split_once('-')
        .ok_or_else(|| dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "Range 格式非法"))?;
    let (start, end) = if start_s.is_empty() {
        // bytes=-n：最后 n 字节
        let n: u64 = end_s
            .parse()
            .map_err(|_| dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "Range 格式非法"))?;
        if n == 0 || len == 0 {
            return Err(dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "范围无法满足"));
        }
        let start = len.saturating_sub(n);
        (start, len - 1)
    } else {
        let start: u64 = start_s
            .parse()
            .map_err(|_| dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "Range 格式非法"))?;
        let end = if end_s.is_empty() {
            len - 1
        } else {
            end_s
                .parse::<u64>()
                .map_err(|_| dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "Range 格式非法"))?
                .min(len - 1)
        };
        if len == 0 || start > end || start >= len {
            return Err(dav_error(StatusCode::RANGE_NOT_SATISFIABLE, "范围无法满足"));
        }
        (start, end)
    };
    Ok(Some((start, end)))
}

async fn mount_load(state: &AppState) -> path_guard::MountSet {
    let config = state.config.read().await;
    path_guard::load(&config, &state.cwd)
}

/// 把请求里挂载点内的相对路径解析成存在的绝对路径（canonicalize，防逃逸）。
fn resolve(mount: &path_guard::MountEntry, rest: &str) -> Result<PathBuf, Response> {
    path_guard::resolve_existing(mount, rest).map_err(|err| match err {
        crate::core::fs::FsError::NotFound(_) => {
            dav_error(StatusCode::NOT_FOUND, "路径不存在")
        }
        crate::core::fs::FsError::SymlinkEscape(_) => {
            dav_error(StatusCode::FORBIDDEN, "禁止经软链接逃逸")
        }
        crate::core::fs::FsError::PathTraversal(_) => {
            dav_error(StatusCode::FORBIDDEN, "禁止路径穿越")
        }
        other => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &other.message()),
    })
}

/// 写方法统一拦截：只读 Token 与只读挂载点双重 403
fn ensure_writable(
    readonly: bool,
    mount: &path_guard::MountEntry,
) -> Result<(), Response> {
    if readonly {
        return Err(dav_error(StatusCode::FORBIDDEN, "该令牌为只读"));
    }
    mount
        .ensure_writable()
        .map_err(|_| dav_error(StatusCode::FORBIDDEN, "挂载点为只读"))
}

fn reject_mount_root(target: &Path, root: &Path) -> Result<(), Response> {
    if target == root {
        return Err(dav_error(
            StatusCode::FORBIDDEN,
            "挂载点根目录不可执行该操作",
        ));
    }
    Ok(())
}

/// 递归复制（同步实现，调用方包 spawn_blocking）。软链接按目标内容复制。
fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::create_dir(dst).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if path_guard::is_part_file(&name) {
                continue;
            }
            // 复制目录时把系统垃圾一起搬过去毫无意义，还会让目标端再被同步污染
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }
            copy_recursive(&entry.path(), &dst.join(&name))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// WebDAV 统一入口：`/dav` 与 `/dav/*`（设计文档 v2.5.0 6.6）
pub async fn handle(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // 未初始化严防死守：初始化完成前 WebDAV 一律 503，与 /api/* 同口径
    // （设计文档 v2.7.0 3.1.1，防止未鉴权实例被扫描器探测）
    if state.auth.user_count().unwrap_or(0) == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Instance is not initialized yet",
        )
            .into_response();
    }

    // ---- 认证（独立防爆破池，设计文档 3.1.5 / 3.10）----
    let Some(creds) = basic_credentials(&headers) else {
        return unauthorized();
    };
    let guard_key = format!("{}|{}", crate::api::auth::peer_ip(peer), creds.username);
    if let Err(remaining) = state.dav_guard.check(&guard_key) {
        return dav_error(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("Too many failed attempts, retry in {remaining}s"),
        );
    }
    let Some(readonly) = crate::api::tokens::verify_token(&state, &creds.username, &creds.token)
    else {
        state.dav_guard.record_failure(&guard_key);
        return unauthorized();
    };
    state.dav_guard.record_success(&guard_key);

    // ---- 路由与挂载点 ----
    let (mount_id, rest) = match split_route(uri.path()) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Some(mount_id) = mount_id else {
        return dav_error(
            StatusCode::NOT_FOUND,
            "Use /dav/<mount_id>/<path>; see GET /api/files/mounts",
        );
    };
    let set = mount_load(&state).await;
    let Some(mount) = set.find(&mount_id) else {
        return dav_error(StatusCode::NOT_FOUND, "挂载点不存在");
    };
    let Ok(root) = mount.canon_root() else {
        return dav_error(StatusCode::NOT_FOUND, "挂载点当前不可用");
    };
    let root = root.clone();

    let display_path = format!(
        "{}/{}/{}",
        PREFIX,
        percent_encode_segment(&mount_id),
        fs::normalize_display(Path::new(&rest)).trim_start_matches('/')
    );

    match method.as_str() {
        "OPTIONS" => (
            StatusCode::OK,
            [
                (HeaderName::from_static("dav"), "1, 2".to_string()),
                (
                    header::ALLOW,
                    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, MOVE, COPY, LOCK, UNLOCK"
                        .to_string(),
                ),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
        )
            .into_response(),

        "PROPFIND" => {
            let depth = headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("1")
                .to_string();
            let target = match resolve(mount, &rest) {
                Ok(path) => path,
                Err(response) => return response,
            };
            let Ok(meta) = std::fs::symlink_metadata(&target) else {
                return dav_error(StatusCode::NOT_FOUND, "路径不存在");
            };

            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| mount.info.name.clone());
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut body_xml = String::from(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">",
            );
            body_xml.push_str(&entry_xml(&display_path, &name, meta.is_dir(), meta.len(), mtime));

            let mut truncated = false;
            if meta.is_dir() && depth != "0" {
                let mut count = 0usize;
                if let Ok(reader) = std::fs::read_dir(&target) {
                    for entry in reader.flatten() {
                        let child_name = entry.file_name().to_string_lossy().to_string();
                        // 运行期 .part 临时文件对外不可见
                        if path_guard::is_part_file(&child_name) {
                            continue;
                        }
                        // 系统垃圾不进 PROPFIND：macOS 客户端看到 .DS_Store / ._*
                        // 就会当作普通文件往回写，形成同步死循环（设计文档 3.3.7）
                        if crate::core::junk::is_system_junk(&child_name) {
                            continue;
                        }
                        count += 1;
                        if count > PROPFIND_LIMIT {
                            truncated = true;
                            break;
                        }
                        let Ok(child_meta) = entry.metadata() else {
                            continue;
                        };
                        let child_display = format!(
                            "{}/{}",
                            display_path.trim_end_matches('/'),
                            percent_encode_segment(&child_name)
                        );
                        let child_mtime = child_meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        body_xml.push_str(&entry_xml(
                            &child_display,
                            &child_name,
                            child_meta.is_dir(),
                            child_meta.len(),
                            child_mtime,
                        ));
                    }
                }
            }
            body_xml.push_str("</D:multistatus>");

            let mut response = (
                StatusCode::MULTI_STATUS,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                body_xml,
            )
                .into_response();
            if truncated {
                response.headers_mut().insert(
                    HeaderName::from_static("x-truncated"),
                    header::HeaderValue::from_static("true"),
                );
            }
            response
        }

        "GET" | "HEAD" => {
            let target = match resolve(mount, &rest) {
                Ok(path) => path,
                Err(response) => return response,
            };
            let Ok(meta) = std::fs::metadata(&target) else {
                return dav_error(StatusCode::NOT_FOUND, "文件不存在");
            };
            if meta.is_dir() {
                return dav_error(StatusCode::METHOD_NOT_ALLOWED, "目录请用 PROPFIND");
            }
            let len = meta.len();
            let range = match parse_range(headers.get(header::RANGE).and_then(|v| v.to_str().ok()), len)
            {
                Ok(range) => range,
                Err(response) => return response,
            };

            let mime = mime_guess::from_path(&target)
                .first_or_octet_stream()
                .to_string();
            let mut builder = axum::response::Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes");
            let body = match range {
                None => {
                    builder = builder.header(header::CONTENT_LENGTH, len.to_string());
                    if method.as_str() == "HEAD" {
                        Body::empty()
                    } else {
                        match tokio::fs::File::open(&target).await {
                            Ok(file) => Body::from_stream(ReaderStream::new(file)),
                            Err(_) => {
                                return dav_error(StatusCode::NOT_FOUND, "文件不存在");
                            }
                        }
                    }
                }
                Some((start, end)) => {
                    let slice_len = end - start + 1;
                    builder = builder
                        .header(header::CONTENT_LENGTH, slice_len.to_string())
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{len}"),
                        )
                        .status(StatusCode::PARTIAL_CONTENT);
                    if method.as_str() == "HEAD" {
                        Body::empty()
                    } else {
                        match tokio::fs::File::open(&target).await {
                            Ok(mut file) => {
                                use tokio::io::AsyncSeekExt;
                                if let Err(err) = file.seek(SeekFrom::Start(start)).await {
                                    return dav_error(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        &format!("seek failed: {err}"),
                                    );
                                }
                                Body::from_stream(ReaderStream::new(file.take(slice_len)))
                            }
                            Err(_) => {
                                return dav_error(StatusCode::NOT_FOUND, "文件不存在");
                            }
                        }
                    }
                }
            };
            builder.body(body).unwrap_or_else(|_| {
                dav_error(StatusCode::INTERNAL_SERVER_ERROR, "response build failed")
            })
        }

        "PUT" => {
            if let Err(response) = ensure_writable(readonly, mount) {
                return response;
            }
            // 目标 = 父目录 canonicalize + 校验过的名字（设计文档 3.10「复用 path_guard」）
            let (parent_text, file_name) = match rest.rsplit_once('/') {
                Some((parent, name)) if !name.is_empty() => (parent.to_string(), name.to_string()),
                _ => (String::new(), rest.clone()),
            };
            if file_name.is_empty() {
                return dav_error(StatusCode::FORBIDDEN, "挂载点根目录不可覆盖");
            }
            // 拦截系统垃圾写入：`.DS_Store` / `._*` / `Thumbs.db` 这类文件
            // 一旦允许落盘，macOS + Windows 双端挂载会互相触发无限同步（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&file_name) {
                crate::log_info!(
                    "webdav rejected junk upload '{}' on mount {}",
                    file_name,
                    mount.info.id
                );
                return dav_error(StatusCode::FORBIDDEN, "该文件名属于系统垃圾文件，已被服务器拒绝");
            }
            let target = match path_guard::resolve_child(mount, &parent_text, &file_name) {
                Ok(path) => path,
                Err(err) => {
                    return match err {
                        crate::core::fs::FsError::NotFound(_) => {
                            dav_error(StatusCode::CONFLICT, "父目录不存在")
                        }
                        crate::core::fs::FsError::PathTraversal(_)
                        | crate::core::fs::FsError::SymlinkEscape(_) => {
                            dav_error(StatusCode::FORBIDDEN, "禁止路径穿越")
                        }
                        other => dav_error(StatusCode::BAD_REQUEST, &other.message()),
                    };
                }
            };
            let existed = path_guard::symlink_exists(&target);
            let part_path = target.with_file_name(path_guard::part_name(&file_name));

            // 流式写入临时文件（绝不 to_bytes，设计文档 3.10「防 OOM」）
            let mut stream = body.into_data_stream();
            let write_result: Result<(), Response> = async {
                let created = tokio::fs::File::create(&part_path).await;
                let mut file = match created {
                    Ok(file) => file,
                    Err(e) => {
                        return Err(dav_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("create failed: {e}"),
                        ));
                    }
                };
                while let Some(chunk) = stream.next().await {
                    let bytes = match chunk {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            return Err(dav_error(
                                StatusCode::BAD_REQUEST,
                                "request body aborted",
                            ));
                        }
                    };
                    if let Err(e) = file.write_all(&bytes).await {
                        return Err(dav_error(
                            StatusCode::INSUFFICIENT_STORAGE,
                            &format!("write failed: {e}"),
                        ));
                    }
                }
                if let Err(e) = file.flush().await {
                    return Err(dav_error(
                        StatusCode::INSUFFICIENT_STORAGE,
                        &format!("flush failed: {e}"),
                    ));
                }
                drop(file);
                Ok(())
            }
            .await;

            if let Err(response) = write_result {
                let _ = tokio::fs::remove_file(&part_path).await;
                return response;
            }

            // per-path 锁串行化最终重命名（设计文档 3.10「依托底层 per-path 互斥」）
            let lock_target = target.clone();
            let rename_result = {
                let state = state.clone();
                let part = part_path.clone();
                let dest = target.clone();
                tokio::task::spawn_blocking(move || {
                    let _guard = state.path_locks.acquire(&[lock_target]);
                    path_guard::rename_replace(&part, &dest)
                })
                .await
            };
            match rename_result {
                Ok(Ok(())) => {
                    if existed {
                        StatusCode::NO_CONTENT.into_response()
                    } else {
                        StatusCode::CREATED.into_response()
                    }
                }
                Ok(Err(err)) => {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("rename failed: {}", err.message()))
                }
                Err(err) => {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("task failed: {err}"))
                }
            }
        }

        "MKCOL" => {
            if let Err(response) = ensure_writable(readonly, mount) {
                return response;
            }
            if rest.is_empty() {
                return dav_error(StatusCode::FORBIDDEN, "挂载点根目录已存在");
            }
            let (parent_text, name) = match rest.rsplit_once('/') {
                Some((parent, name)) if !name.is_empty() => (parent.to_string(), name.to_string()),
                _ => (String::new(), rest.clone()),
            };
            // 与 PUT 同口径：系统垃圾目录（.Trashes / $RECYCLE.BIN 等）不允许新建
            if crate::core::junk::is_system_junk(&name) {
                return dav_error(StatusCode::FORBIDDEN, "该名称属于系统目录，已被服务器拒绝");
            }
            let target = match path_guard::resolve_child(mount, &parent_text, &name) {
                Ok(path) => path,
                Err(err) => {
                    return match err {
                        crate::core::fs::FsError::NotFound(_) => {
                            dav_error(StatusCode::CONFLICT, "父目录不存在")
                        }
                        crate::core::fs::FsError::Conflict(_) => {
                            dav_error(StatusCode::METHOD_NOT_ALLOWED, "名称非法")
                        }
                        other => dav_error(StatusCode::BAD_REQUEST, &other.message()),
                    };
                }
            };
            if path_guard::symlink_exists(&target) {
                return dav_error(StatusCode::METHOD_NOT_ALLOWED, "目标已存在");
            }
            match tokio::fs::create_dir(&target).await {
                Ok(_) => StatusCode::CREATED.into_response(),
                Err(err) => dav_error(StatusCode::CONFLICT, &format!("创建目录失败：{err}")),
            }
        }

        "DELETE" => {
            if let Err(response) = ensure_writable(readonly, mount) {
                return response;
            }
            let target = match resolve(mount, &rest) {
                Ok(path) => path,
                Err(response) => return response,
            };
            if let Err(response) = reject_mount_root(&target, &root) {
                return response;
            }
            let state = state.clone();
            let delete_target = target.clone();
            match tokio::task::spawn_blocking(move || {
                let _guard = state.path_locks.acquire(&[delete_target.clone()]);
                fs_ops::remove_path(&delete_target)
            })
            .await
            {
                Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
                Ok(Err(err)) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("删除失败：{}", err.message())),
                Err(err) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("task failed: {err}")),
            }
        }

        "MOVE" | "COPY" => {
            let is_move = method.as_str() == "MOVE";
            if let Err(response) = ensure_writable(readonly, mount) {
                return response;
            }
            let Some(destination) = headers
                .get("destination")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
            else {
                return dav_error(StatusCode::BAD_REQUEST, "缺少 Destination 头");
            };
            let (dest_mount_id, dest_rest) = match parse_destination(&destination) {
                Ok(pair) => pair,
                Err(response) => return response,
            };
            // 禁止跨挂载点操作（设计文档 3.10）
            if dest_mount_id.as_deref() != Some(mount_id.as_str()) {
                return dav_error(StatusCode::FORBIDDEN, "禁止跨挂载点操作");
            }
            let overwrite = headers
                .get("overwrite")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("t"))
                .unwrap_or(true);

            let source = match resolve(mount, &rest) {
                Ok(path) => path,
                Err(response) => return response,
            };
            if is_move && let Err(response) = reject_mount_root(&source, &root) {
                return response;
            }
            if std::fs::symlink_metadata(&source).is_err() {
                return dav_error(StatusCode::NOT_FOUND, "源路径不存在");
            }

            // 目标语义：dest 存在且为目录 → 拷/移进该目录；dest 不存在 → 父目录必须存在；
            // dest 是文件 → 仅 overwrite=T 时替换
            let dest = match resolve(mount, &dest_rest) {
                Ok(path) => path,
                Err(_) if dest_rest.is_empty() => return dav_error(StatusCode::NOT_FOUND, "目标不存在"),
                // 目标尚未创建是常态：退一步解析「最近一段已存在的父目录」
                Err(_) => {
                    let (parent_text, name) = match dest_rest.rsplit_once('/') {
                        Some((parent, name)) if !name.is_empty() => {
                            (parent.to_string(), name.to_string())
                        }
                        _ => (String::new(), dest_rest.clone()),
                    };
                    let parent = match resolve(mount, &parent_text) {
                        Ok(path) => path,
                        Err(response) => return response,
                    };
                    if !parent.is_dir() {
                        return dav_error(StatusCode::CONFLICT, "目标父目录不存在");
                    }
                    parent.join(&name)
                }
            };
            let final_target = match std::fs::symlink_metadata(&dest).ok().as_ref() {
                Some(meta) if meta.is_dir() => {
                    let Some(name) = source.file_name() else {
                        return dav_error(StatusCode::FORBIDDEN, "挂载点根不可作为源");
                    };
                    dest.join(name)
                }
                Some(_) if !overwrite => {
                    return dav_error(StatusCode::PRECONDITION_FAILED, "目标已存在");
                }
                _ => dest,
            };
            if final_target == source {
                return dav_error(StatusCode::FORBIDDEN, "源与目标相同");
            }
            if let Err(response) = reject_mount_root(&final_target, &root) {
                return response;
            }

            let state = state.clone();
            let src = source.clone();
            let dst = final_target.clone();
            // 覆盖判定看最终目标（dest 可能是「拷进这个目录」的目录本身）
            let remove_existing = path_guard::symlink_exists(&final_target);
            let result = tokio::task::spawn_blocking(move || {
                let _guard = state.path_locks.acquire(&[src.clone(), dst.clone()]);
                if is_move {
                    if remove_existing {
                        // 覆盖：先删目标再挪（目标可能是目录树）
                        fs_ops::remove_path(&dst).map_err(|e| e.message())?;
                        std::fs::rename(&src, &dst).map_err(|e| e.to_string())
                    } else {
                        path_guard::rename_no_replace(&src, &dst)
                            .map_err(|e| match e {
                                crate::core::fs::FsError::Conflict(_) => {
                                    "409:目标已存在".to_string()
                                }
                                other => other.message(),
                            })
                    }
                } else {
                    if remove_existing {
                        fs_ops::remove_path(&dst).map_err(|e| e.message())?;
                    }
                    copy_recursive(&src, &dst)
                }
            })
            .await;

            match result {
                Ok(Ok(())) => StatusCode::CREATED.into_response(),
                Ok(Err(message)) => {
                    if let Some(status) = message
                        .strip_prefix("409:")
                        .map(|_| StatusCode::PRECONDITION_FAILED)
                    {
                        return dav_error(status, "目标已存在");
                    }
                    dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("操作失败：{message}"))
                }
                Err(err) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("task failed: {err}")),
            }
        }

        "LOCK" => {
            // 伪锁（设计文档 3.10 / 9.4）：只发随机 token，不维护超时表
            let token = format!("opaquelocktoken:{}", crate::core::auth::random_token());
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock>\
<D:locktype><D:write/></D:locktype>\
<D:lockscope><D:exclusive/></D:lockscope>\
<D:depth>0</D:depth>\
<D:timeout>Second-3600</D:timeout>\
<D:locktoken><D:href>{token}</D:href></D:locktoken>\
</D:activelock></D:lockdiscovery></D:prop>"
            );
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/xml; charset=utf-8".to_string()),
                    (
                        HeaderName::from_static("lock-token"),
                        format!("<{token}>"),
                    ),
                ],
                xml,
            )
                .into_response()
        }

        "UNLOCK" => StatusCode::NO_CONTENT.into_response(),

        _ => dav_error(StatusCode::METHOD_NOT_ALLOWED, "不支持的方法"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_route_decodes_per_segment() {
        // 普通中文路径
        let (mid, rest) = split_route("/dav/m0/%E7%9B%B8%E5%86%8C/a%20b.txt").unwrap();
        assert_eq!(mid.as_deref(), Some("m0"));
        assert_eq!(rest, "相册/a b.txt");
        // %2F 解码成分隔符属于路径内容，不会再切路由 —— 但这里它落在剩余段里，
        // 会作为普通字符传给 path_guard，最终被名称校验或 canonicalize 拦住
        let (mid2, rest2) = split_route("/dav/m0/a%2Fb.txt").unwrap();
        assert_eq!(mid2.as_deref(), Some("m0"));
        assert_eq!(rest2, "a/b.txt");
    }

    #[test]
    fn split_route_rejects_dot_segments() {
        let err = split_route("/dav/m0/%2e%2e/secret").unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn parse_range_variants() {
        assert_eq!(parse_range(None, 100).unwrap(), None);
        assert_eq!(parse_range(Some("bytes=0-"), 100).unwrap(), Some((0, 99)));
        assert_eq!(parse_range(Some("bytes=10-19"), 100).unwrap(), Some((10, 19)));
        // 越界 end 截断
        assert_eq!(parse_range(Some("bytes=90-999"), 100).unwrap(), Some((90, 99)));
        // 后缀式
        assert_eq!(parse_range(Some("bytes=-5"), 100).unwrap(), Some((95, 99)));
        // 无法满足
        assert!(parse_range(Some("bytes=100-"), 100).is_err());
        assert!(parse_range(Some("bytes=-0"), 100).is_err());
        // 多段不支持
        assert!(parse_range(Some("bytes=0-1,5-9"), 100).is_err());
    }

    #[test]
    fn parse_destination_accepts_url_and_path() {
        let (mid, rest) = parse_destination("http://host:5100/dav/m0/sub/x.txt").unwrap();
        assert_eq!(mid.as_deref(), Some("m0"));
        assert_eq!(rest, "sub/x.txt");
        let (mid2, rest2) = parse_destination("/dav/m1").unwrap();
        assert_eq!(mid2.as_deref(), Some("m1"));
        assert_eq!(rest2, "");
    }
}
