//! WebDAV 挂载（设计文档 3.1 / 10）。
//!
//! - 认证：HTTP Basic，用户名随意，密码填 `asm_` 前缀的 App Token；
//! - 只读令牌只能 GET / PROPFIND；
//! - 支持 OPTIONS / PROPFIND / GET / HEAD / PUT / MKCOL / DELETE / MOVE。

use std::path::{Component, Path as StdPath, PathBuf};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use data_encoding::BASE64;

use crate::core::fs;
use crate::AppState;

const PREFIX: &str = "/dav";

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Asmoday WebDAV\"")],
        "需要 App Token",
    )
        .into_response()
}

fn basic_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = BASE64.decode(encoded.trim().as_bytes()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (_, password) = text.split_once(':')?;
    Some(password.to_string())
}

fn percent_decode(input: &str) -> Option<String> {
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

fn percent_encode_path(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 把 `/dav/a/b` 映射成挂载点下的真实路径。
fn resolve(uri_path: &str, root: &StdPath) -> Result<PathBuf, Response> {
    let rest = uri_path.strip_prefix(PREFIX).unwrap_or("");
    let rest = rest.trim_start_matches('/');
    let decoded = percent_decode(rest).ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "路径编码非法").into_response()
    })?;

    let candidate = StdPath::new(&decoded);
    for component in candidate.components() {
        if matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err((StatusCode::FORBIDDEN, "禁止路径穿越").into_response());
        }
    }

    Ok(root.join(candidate))
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn entry_xml(href: &str, name: &str, is_dir: bool, size: u64) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>\
<D:displayname>{}</D:displayname>\
<D:resourcetype>{}</D:resourcetype>\
<D:getcontentlength>{}</D:getcontentlength>\
<D:getcontenttype>{}</D:getcontenttype>\
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
        }
    )
}

fn dav_error(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

/// WebDAV 统一入口：`/dav` 与 `/dav/*`
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = basic_token(&headers) else {
        return unauthorized();
    };
    let Some(readonly) = crate::api::tokens::verify_token(&state, &token) else {
        return unauthorized();
    };

    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };
    let Some(root_entry) = roots.default_root() else {
        return dav_error(StatusCode::NOT_FOUND, "没有可用的挂载点");
    };
    let root = PathBuf::from(root_entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));

    let target = match resolve(uri.path(), &root) {
        Ok(path) => path,
        Err(response) => return response,
    };

    let display_path = format!(
        "{}/{}",
        PREFIX,
        fs::normalize_display(target.strip_prefix(&root).unwrap_or(&target))
            .trim_start_matches('/')
    );

    match method.as_str() {
        "OPTIONS" => (
            StatusCode::OK,
            [
                (HeaderName::from_static("dav"), "1, 2".to_string()),
                (header::ALLOW, "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, MOVE".to_string()),
            ],
        )
            .into_response(),

        "PROPFIND" => {
            let depth = headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("1")
                .to_string();

            let meta = match std::fs::metadata(&target) {
                Ok(meta) => meta,
                Err(_) => return dav_error(StatusCode::NOT_FOUND, "路径不存在"),
            };

            let mut body_xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">");
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "dav".to_string());
            body_xml.push_str(&entry_xml(&display_path, &name, meta.is_dir(), meta.len()));

            if meta.is_dir() && depth != "0" {
                if let Ok(reader) = std::fs::read_dir(&target) {
                    for entry in reader.flatten() {
                        let Ok(meta) = entry.metadata() else { continue };
                        let child_name = entry.file_name().to_string_lossy().to_string();
                        let child_display = format!(
                            "{}/{}",
                            display_path.trim_end_matches('/'),
                            percent_encode_path(&child_name)
                        );
                        body_xml.push_str(&entry_xml(
                            &child_display,
                            &child_name,
                            meta.is_dir(),
                            meta.len(),
                        ));
                    }
                }
            }

            body_xml.push_str("</D:multistatus>");

            (
                StatusCode::MULTI_STATUS,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                body_xml,
            )
                .into_response()
        }

        "GET" | "HEAD" => {
            if target.is_dir() {
                return dav_error(StatusCode::METHOD_NOT_ALLOWED, "目录请用 PROPFIND");
            }
            match tokio::fs::read(&target).await {
                Ok(bytes) => {
                    let mime = mime_guess::from_path(&target)
                        .first_or_octet_stream()
                        .to_string();
                    (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, mime),
                            (header::CONTENT_LENGTH, bytes.len().to_string()),
                        ],
                        if method.as_str() == "HEAD" { Vec::new() } else { bytes },
                    )
                        .into_response()
                }
                Err(_) => dav_error(StatusCode::NOT_FOUND, "文件不存在"),
            }
        }

        "PUT" => {
            if readonly {
                return dav_error(StatusCode::FORBIDDEN, "该令牌为只读");
            }
            if let Some(parent) = target.parent() {
                if !parent.is_dir() {
                    return dav_error(StatusCode::CONFLICT, "父目录不存在");
                }
            }
            match tokio::fs::write(&target, &body).await {
                Ok(_) => StatusCode::CREATED.into_response(),
                Err(err) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("写入失败：{err}")),
            }
        }

        "MKCOL" => {
            if readonly {
                return dav_error(StatusCode::FORBIDDEN, "该令牌为只读");
            }
            match tokio::fs::create_dir(&target).await {
                Ok(_) => StatusCode::CREATED.into_response(),
                Err(err) => dav_error(StatusCode::CONFLICT, &format!("创建目录失败：{err}")),
            }
        }

        "DELETE" => {
            if readonly {
                return dav_error(StatusCode::FORBIDDEN, "该令牌为只读");
            }
            let result = if target.is_dir() {
                tokio::fs::remove_dir_all(&target).await
            } else {
                tokio::fs::remove_file(&target).await
            };
            match result {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(err) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("删除失败：{err}")),
            }
        }

        "MOVE" => {
            if readonly {
                return dav_error(StatusCode::FORBIDDEN, "该令牌为只读");
            }
            let Some(destination) = headers
                .get("destination")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
            else {
                return dav_error(StatusCode::BAD_REQUEST, "缺少 Destination 头");
            };

            let dest_path = match destination.split_once("://") {
                Some((_, rest)) => rest.find('/').map(|idx| &rest[idx..]).unwrap_or("/"),
                None => destination.as_str(),
            };

            let dest = match resolve(dest_path, &root) {
                Ok(path) => path,
                Err(response) => return response,
            };

            match tokio::fs::rename(&target, &dest).await {
                Ok(_) => StatusCode::CREATED.into_response(),
                Err(err) => dav_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("移动失败：{err}")),
            }
        }

        _ => dav_error(StatusCode::METHOD_NOT_ALLOWED, "不支持的方法"),
    }
}
