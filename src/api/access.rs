//! HTTP 访问日志（设计文档 3.7 / 6.6：参考 FileBrowser 的 apiLevels 独立通道）。
//!
//! 记录**方法 / 路径 / 状态码 / 耗时 / 来源 IP**，外加**脱敏后的查询串**：
//! 排查「哪个文件触发了异常」必须有 `path` / `action` / `download` 这类参数，
//! 但 `password` / `token` / `share_secret` 这类敏感参数的值必须打码成 `[REDACTED]`。
//! 请求体一律不记（登录接口的明文密码只在请求体里）。
//!
//! 级别固定 INFO，走 `[log] api_level` 独立闸门（info 放行，off 关闭），
//! 放行后再受控制台与文件各自的级别过滤。静态资源不记，避免健康检查刷屏。

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;
use std::time::Instant;

use crate::{AppState, log_access};

/// 敏感参数名（比较前统一小写）：值一律替换成 `[REDACTED]`。
///
/// `code` 指 2FA / 一次性验证码；`sig` / `signature` 是分享链接的签名。
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pass",
    "pwd",
    "token",
    "session",
    "share_secret",
    "secret",
    "access_token",
    "api_key",
    "apikey",
    "key",
    "sig",
    "signature",
    "code",
];

/// 查询串在日志里的最大长度：超长 query（批量 paths 列表）会把日志撑爆，
/// 截断后仍保留「是哪个接口、哪个文件」的关键信息。
const MAX_QUERY_LOG: usize = 512;

/// 是否为需要记访问日志的路径（API / WebDAV / 反代网关；静态资源不记）
fn trackable(path: &str) -> bool {
    path.starts_with("/api/")
        || path == "/dav"
        || path.starts_with("/dav/")
        || path == "/proxy"
        || path.starts_with("/proxy/")
}

/// 把 `%XX` 解码成字节（只用于**参数名**比较，参数值一律原样保留）。
fn decode_key(key: &str) -> String {
    let bytes = key.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 查询串脱敏：保留非敏感参数（`path` / `action` / `download`），
/// 敏感参数只留键名、值替换为 `[REDACTED]`。
///
/// 参数之间的 `&` 与原顺序保持不变，便于和前端代码逐字对照。
pub fn redact_query(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some((key, _value)) = pair.split_once('=') else {
            // 没带 `=` 的裸参数原样保留（它本身没有值可泄漏）
            parts.push(pair.to_string());
            continue;
        };
        let lowered = decode_key(key).trim().to_ascii_lowercase();
        if SENSITIVE_KEYS.contains(&lowered.as_str()) {
            parts.push(format!("{key}=[REDACTED]"));
        } else {
            parts.push(pair.to_string());
        }
    }
    let joined = parts.join("&");
    if joined.chars().count() <= MAX_QUERY_LOG {
        return joined;
    }
    let mut cut = String::new();
    for (idx, ch) in joined.char_indices() {
        if idx >= MAX_QUERY_LOG {
            break;
        }
        cut.push(ch);
    }
    cut.push('…');
    cut
}

pub async fn track(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let interesting = trackable(&path);
    // 只在需要记日志时克隆请求头与查询串；请求体全程不读（设计文档 3.7）
    let (headers, query) = if interesting {
        (
            req.headers().clone(),
            req.uri().query().map(redact_query).unwrap_or_default(),
        )
    } else {
        (HeaderMap::new(), String::new())
    };
    let start = Instant::now();

    let response = next.run(req).await;

    if interesting {
        let (trust_proxy, trusted) = {
            let config = state.config.read().await;
            (
                config.server.proxy.trust_proxy_headers,
                config.server.proxy.trusted_ips.clone(),
            )
        };
        let client_ip = crate::api::auth::pick_client_ip(
            trust_proxy,
            &trusted,
            &crate::api::auth::peer_ip(peer),
            &headers,
        );
        // 有查询串时拼成 `path?query`（已脱敏），否则保持纯路径
        let target = if query.is_empty() {
            path
        } else {
            format!("{path}?{query}")
        };
        log_access!(
            "{method} {target} -> {} ({} ms) from {client_ip}",
            response.status().as_u16(),
            start.elapsed().as_millis()
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::redact_query;

    #[test]
    fn keeps_diagnostic_params_and_masks_secrets() {
        // 设计文档 6.6：path / action / download 必须留着，password / token / share_secret 必须打码
        assert_eq!(
            redact_query("path=%2Fmnt%2Fa%2Fb.mkv&action=rename&download=1"),
            "path=%2Fmnt%2Fa%2Fb.mkv&action=rename&download=1"
        );
        assert_eq!(
            redact_query("path=a&password=hunter2&token=abc123&share_secret=s3cr3t"),
            "path=a&password=[REDACTED]&token=[REDACTED]&share_secret=[REDACTED]"
        );
    }

    #[test]
    fn key_matching_is_case_insensitive_and_percent_aware() {
        // 大小写混写、以及把键名做 URL 编码的写法，都不能漏掉
        assert_eq!(redact_query("PASSWORD=x"), "PASSWORD=[REDACTED]");
        assert_eq!(
            redact_query("Share_Secret=x&Access_Token=y"),
            "Share_Secret=[REDACTED]&Access_Token=[REDACTED]"
        );
        assert_eq!(redact_query("pass%77ord=x"), "pass%77ord=[REDACTED]");
        // 名字里带 password 的其它字段不属于敏感参数
        assert_eq!(redact_query("passworded=1"), "passworded=1");
    }

    #[test]
    fn bare_params_and_empty_values_survive() {
        assert_eq!(redact_query("verbose&token="), "verbose&token=[REDACTED]");
        assert_eq!(redact_query(""), "");
        // 连续的 && 不产生空段
        assert_eq!(redact_query("a=1&&b=2"), "a=1&b=2");
    }

    #[test]
    fn long_query_is_truncated_on_char_boundary() {
        let raw = format!("path={}", "a".repeat(2_000));
        let masked = redact_query(&raw);
        // 512 个字符 + 省略号
        assert_eq!(masked.chars().count(), 513);
        assert!(masked.ends_with('…'));
    }
}
