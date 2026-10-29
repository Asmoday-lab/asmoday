//! 默认安全响应头（设计文档 3.1.9）。
//!
//! 统一在路由最外层加一次，覆盖 API、分享页与内嵌前端：
//! - `X-Content-Type-Options: nosniff`：禁止浏览器猜 MIME（缩略图/下载全靠它）；
//! - `Referrer-Policy: no-referrer`：分享直链不把来源地址泄给第三方；
//! - `Permissions-Policy`：摄像头/麦克风/定位等一律收紧（终端与媒体都不需要）；
//! - `Strict-Transport-Security`：仅在确认走 HTTPS 时下发（HTTP 下发了也没用，还会误导）；
//! - `Content-Security-Policy`：禁 `unsafe-eval`，允许打包产物需要的 `unsafe-inline`
//!   （`index.html` 的防白屏内联脚本 + Vue 的内联 style 属性），并放行 blob:
//!   供 Video.js 与 ThumbHash 画布使用；`frame-ancestors 'self'` 防点击劫持，
//!   但**必须**保留 iframe 能力（PDF 预览、Markdown 内嵌）。
//!
//! 例外：`/proxy/<id>/…` 是第三方应用的透传，响应头由代理层自行裁剪（设计文档 3.6），
//! 在这里加本站 CSP 会把对方页面直接打死，所以整段跳过。

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;

use crate::AppState;

/// CSP：够用且不误伤（说明见模块注释）。
const CSP: &str = "default-src 'self'; \
script-src 'self' 'unsafe-inline'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data: blob: https:; \
media-src 'self' data: blob:; \
font-src 'self' data:; \
connect-src 'self' ws: wss:; \
frame-src 'self' blob:; \
object-src 'none'; \
base-uri 'self'; \
form-action 'self'; \
frame-ancestors 'self'";

const PERMISSIONS: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=(), \
display-capture=(), gamepad=(), serial=(), midi=()";

/// 补默认安全头。响应里已有的同名头不覆盖（处理器更清楚自己的场景）。
pub async fn security_headers(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let forwarded_proto = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase());

    let (base_url, ssl_enabled, trusted) = {
        let config = state.config.read().await;
        (
            config.server.proxy.base_url.clone(),
            config.server.ssl.enabled,
            config.server.proxy.trusted_ips.clone(),
        )
    };

    // 剥掉子路径前缀后再判断是否代理透传
    let path = request.uri().path();
    let base = base_url.trim_end_matches('/');
    let inner = if !base.is_empty() {
        path.strip_prefix(base).unwrap_or(path)
    } else {
        path
    };
    if inner.starts_with("/proxy/") {
        return next.run(request).await;
    }

    // HTTPS 判定：自己终结 TLS，或来自可信反代的 X-Forwarded-Proto
    let via_trusted_proxy = peer
        .map(|addr| trusted.iter().any(|rule| crate::api::auth::ip_matches(&crate::api::auth::peer_ip(addr), rule)))
        .unwrap_or(false);
    let https = ssl_enabled
        || (via_trusted_proxy && forwarded_proto.as_deref() == Some("https"));

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let mut put = |name: header::HeaderName, value: &str| {
        if !headers.contains_key(&name) {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        }
    };

    put(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    put(header::REFERRER_POLICY, "no-referrer");
    put(header::CONTENT_SECURITY_POLICY, CSP);
    put(
        header::HeaderName::from_static("permissions-policy"),
        PERMISSIONS,
    );
    if https {
        put(
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000; includeSubDomains",
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::{CSP, PERMISSIONS};

    #[test]
    fn csp_never_allows_eval() {
        assert!(!CSP.contains("unsafe-eval"));
        // iframe（PDF / Markdown 内嵌）必须还活着
        assert!(CSP.contains("frame-src 'self' blob:"));
        assert!(CSP.contains("frame-ancestors 'self'"));
    }

    #[test]
    fn permissions_policy_locks_sensors() {
        for feature in ["camera=()", "microphone=()", "geolocation=()"] {
            assert!(PERMISSIONS.contains(feature));
        }
    }
}
