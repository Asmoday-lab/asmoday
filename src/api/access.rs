//! HTTP 访问日志（设计文档 3.7：参考 FileBrowser 的 apiLevels 独立通道）。
//!
//! 只记录 **方法 / 路径 / 状态码 / 耗时 / 来源 IP**，绝不记录请求体与查询串
//! ——查询串里可能带分享密码，请求体里可能有账号密码。
//! 级别固定 INFO，走 `[log] api_level` 独立闸门（debug/info 放行，warn/error/off 关闭），
//! 放行后再受控制台与文件各自的级别过滤。静态资源不记，避免健康检查刷屏。

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;
use std::time::Instant;

use crate::{log_access, AppState};

/// 是否为需要记访问日志的路径（API / WebDAV / 反代网关；静态资源不记）
fn trackable(path: &str) -> bool {
    path.starts_with("/api/")
        || path == "/dav"
        || path.starts_with("/dav/")
        || path == "/proxy"
        || path.starts_with("/proxy/")
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
    // 只在需要记日志时克隆请求头（供来源 IP 解析），绝不落查询串与请求体
    let headers = if interesting { req.headers().clone() } else { HeaderMap::new() };
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
            &peer.ip().to_string(),
            &headers,
        );
        log_access!(
            "{method} {path} -> {} ({} ms) from {client_ip}",
            response.status().as_u16(),
            start.elapsed().as_millis()
        );
    }

    response
}
