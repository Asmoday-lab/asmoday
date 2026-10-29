//! 子路径前缀中间件（设计文档 3.1 / 4.2）。
//!
//! 职责只有两件：
//!
//! 1. 解析本次请求的生效前缀（配置 `base_url` 优先，其次可信反代的
//!    `X-Forwarded-Prefix`，逻辑见 [`crate::core::prefix`]）；
//! 2. 把入站路径上的前缀**剥掉**，让内层路由永远看到 `/api/...`、`/assets/...`。
//!
//! 第 2 步是「反代带前缀但用户没配 `base_url`」能跑起来的关键：
//! 此时 main 不会 `nest`，请求打到的是 `/asmoday/api/status`，不剥就全 404。
//!
//! 前缀通过请求扩展下发，静态资源回落（`static_files::serve`）据此注入
//! `window.__ASMODAY_BASE__` —— 前端 `lib/base.ts` 靠它给所有绝对路径加前缀。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::uri::PathAndQuery;
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;

/// 「配置与反代头不一致」的 WARN 只打一次：热重载后可能再触发一轮
static MISMATCH_LOGGED: AtomicBool = AtomicBool::new(false);

pub async fn apply(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let forwarded = request
        .headers()
        .get(crate::core::prefix::HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let (configured, trusted) = {
        let config = state.config.read().await;
        (
            config.server.proxy.base_url.clone(),
            config.server.proxy.trusted_ips.clone(),
        )
    };

    // 只有可信代理发来的头才算数（默认 127.0.0.1 / ::1，已覆盖本机反代）
    let from_trusted_proxy = match (peer, forwarded.as_deref()) {
        (Some(addr), Some(_)) => {
            let ip = crate::api::auth::peer_ip(addr);
            trusted
                .iter()
                .any(|rule| crate::api::auth::ip_matches(&ip, rule))
        }
        _ => false,
    };

    let (prefix, mismatch) = crate::core::prefix::resolve(
        &configured,
        forwarded.as_deref().filter(|_| from_trusted_proxy),
    );
    if mismatch && !MISMATCH_LOGGED.swap(true, Ordering::Relaxed) {
        crate::log_warn!(
            "X-Forwarded-Prefix disagrees with [server.proxy].base_url ({} vs {}); \
             using base_url. Fix the reverse proxy or the config to silence this.",
            configured.trim(),
            forwarded.as_deref().unwrap_or(""),
        );
    }

    if !prefix.is_empty() {
        let stripped = prefix.strip(request.uri().path()).to_string();
        // `strip` 不匹配时原样返回；只有真的变了才重建 URI（避免无谓分配）
        if stripped != request.uri().path() {
            if let Some(pq) = path_and_query(&stripped, request.uri().query()) {
                let mut parts = request.uri().clone().into_parts();
                parts.path_and_query = Some(pq);
                if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                    *request.uri_mut() = uri;
                }
            }
        }
    }

    request.extensions_mut().insert(prefix);
    next.run(request).await
}

/// 重建 `path[?query]`：剥前缀只动路径，查询串必须原样带回去
/// （终端 WS 的 `?id=`、文件列表的游标都靠它）。
fn path_and_query(path: &str, query: Option<&str>) -> Option<PathAndQuery> {
    let text = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    text.parse::<PathAndQuery>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_query_keeps_query_string() {
        let pq = path_and_query("/api/status", Some("a=1&b=2")).unwrap();
        assert_eq!(pq.as_str(), "/api/status?a=1&b=2");
        let pq = path_and_query("/api/status", None).unwrap();
        assert_eq!(pq.as_str(), "/api/status");
    }
}
