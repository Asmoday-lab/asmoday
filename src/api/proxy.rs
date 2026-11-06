//! 内置反向代理网关（设计文档 3.6）。
//!
//! `/proxy/{service_id}/…` → `[[services]].target_url/…`
//!
//! - 自动剥离 `X-Frame-Options`；上游 CSP 只剥 `frame-ancestors` 指令后透传
//!   （整条丢弃会让第三方页面在本站 origin 上裸奔，2026-11-04 审查），
//!   上游没有 CSP 时补 `frame-ancestors 'self'` 兜底防点击劫持；
//! - 转发前重建 `X-Forwarded-For` / `X-Real-IP` 为真实客户端来源
//!   （客户端伪造的 XFF 不能原样交给上游做审计/限流）；
//! - 支持 WebSocket Upgrade 透传（双向桥接）；
//! - 支持 `inject_headers` 注入（实现服务间免登录）；
//! - `rewrite_html` 打开时，把目标站点的绝对地址改写为代理路径（兼容老旧后台）；
//! - `strip_prefix` 语义由路由天然实现：`/proxy/<id>` 一定被剥掉。

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::AppState;
use crate::core::config::ServiceConfig;

use crate::api::auth::SESSION_COOKIE;

/// 需要从请求里剔除的逐跳头
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    // 让上游返回明文，避免压缩后无法改写 HTML
    "accept-encoding",
];

/// 需要从响应里剔除的头（含允许 iframe 内嵌的项）。
///
/// CSP **不再整条丢弃**：只剥掉 `frame-ancestors` 指令（见
/// `strip_frame_ancestors`），其余指令原样透传——上游的 CSP 是它页面
/// XSS 防线的一部分，剥光了等于把第三方应用在本站 origin 上解除武装。
const DROP_RESPONSE: &[&str] = &[
    "x-frame-options",
    "transfer-encoding",
    "connection",
    "keep-alive",
];

/// 从 CSP 值里剥掉 `frame-ancestors` 指令，允许本站 iframe 内嵌。
/// 剥完为空（上游 CSP 只声明了 frame-ancestors）时返回 None。
fn strip_frame_ancestors(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(';')
        .filter(|directive| {
            let directive = directive.trim();
            let name = directive
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            name != "frame-ancestors"
        })
        .collect();
    let joined = kept.join("; ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// 细节型错误（4xx 参数类）
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2）
fn error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// 可选的 WebSocket 升级提取器：只有真的带了 Upgrade 头才尝试升级，
/// 避免普通 HTTP 请求因为缺少握手头而直接 400。
pub struct MaybeWs(pub Option<WebSocketUpgrade>);

impl<S> FromRequestParts<S> for MaybeWs
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if is_websocket(&parts.headers) {
            match WebSocketUpgrade::from_request_parts(parts, state).await {
                Ok(upgrade) => Ok(MaybeWs(Some(upgrade))),
                Err(err) => {
                    eprintln!("[asmoday] websocket upgrade rejected: {err:?}");
                    Ok(MaybeWs(None))
                }
            }
        } else {
            Ok(MaybeWs(None))
        }
    }
}

async fn lookup(state: &AppState, id: &str) -> Result<ServiceConfig, Response> {
    let config = state.config.read().await;
    config
        .services
        .iter()
        .find(|service| service.id == id)
        .cloned()
        .ok_or_else(|| {
            error_code(
                StatusCode::NOT_FOUND,
                "SERVICE_NOT_FOUND",
                format!("Unknown service: {id}"),
            )
        })
}

fn build_target(base: &str, rest: &str, query: Option<&str>) -> String {
    let base = base.trim_end_matches('/');
    let rest = rest.trim_start_matches('/');
    let mut url = if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rest}")
    };
    if let Some(query) = query
        && !query.is_empty()
    {
        url.push('?');
        url.push_str(query);
    }
    url
}

/// http(s):// → ws(s)://，WebSocket 上游连接要求 ws 方案
fn to_ws_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        url.to_string()
    }
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

fn is_html(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/html"))
        .unwrap_or(false)
}

/// 从请求的 `Cookie` 头里摘掉本站会话，其余照原样保留。
///
/// 返回 `None` 表示摘完一条不剩——那这个头就整个不发，别给上游一个空串。
fn strip_session_cookie(value: &HeaderValue) -> Option<HeaderValue> {
    let raw = value.to_str().ok()?;
    let prefix = format!("{SESSION_COOKIE}=");
    let kept: Vec<&str> = raw
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.starts_with(&prefix))
        .collect();
    if kept.is_empty() {
        return None;
    }
    HeaderValue::from_str(&kept.join("; ")).ok()
}

/// 这条 `Set-Cookie` 是不是本站会话（同名即丢弃）。
fn is_session_cookie(value: &HeaderValue) -> bool {
    let Ok(raw) = value.to_str() else {
        return false;
    };
    let prefix = format!("{SESSION_COOKIE}=");
    raw.trim_start()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
}

/// `/proxy/{service_id}`
#[allow(clippy::too_many_arguments)]
pub async fn handle_root(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(service_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: MaybeWs,
    body: Bytes,
) -> Response {
    dispatch(
        state,
        peer,
        service_id,
        String::new(),
        method,
        uri,
        headers,
        ws,
        body,
    )
    .await
}

/// `/proxy/{service_id}/{*rest}`
#[allow(clippy::too_many_arguments)]
pub async fn handle_rest(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((service_id, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: MaybeWs,
    body: Bytes,
) -> Response {
    dispatch(
        state, peer, service_id, rest, method, uri, headers, ws, body,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: AppState,
    peer: SocketAddr,
    service_id: String,
    rest: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: MaybeWs,
    body: Bytes,
) -> Response {
    let service = match lookup(&state, &service_id).await {
        Ok(service) => service,
        Err(response) => return response,
    };

    let target = build_target(&service.target_url, &rest, uri.query());

    // ---- WebSocket 透传 ----
    if is_websocket(&headers) {
        // CSWSH 防线（与 terminal 同口径）：握手是 GET，鉴权中间件的同源校验
        // 对 GET 不生效；配置了 inject_headers 免登录的服务，跨站页面一句
        // `new WebSocket(...)` 就能无凭证直连。
        if !crate::api::auth::ws_origin_allowed(&headers) {
            return error_response(
                StatusCode::FORBIDDEN,
                "Cross-site WebSocket handshake rejected",
            );
        }
        let Some(upgrade) = ws.0 else {
            return error_response(StatusCode::BAD_REQUEST, "WebSocket 升级请求缺少必要头部");
        };
        let inject: Vec<(String, String)> = service
            .inject_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let ws_target = to_ws_url(&target);
        return upgrade.on_upgrade(move |socket| bridge_ws(socket, ws_target, inject));
    }

    // ---- 普通 HTTP 透传 ----
    // 客户端真实来源（2026-11-04 审查）：转发前重建 XFF / X-Real-IP。
    // 浏览器发出的 XFF 是客户端自己写的、可任意伪造；照原样转发的话，
    // 上游应用按 XFF 做的审计/限流一样中招。取值口径与登录限流一致
    // （可信反代场景从右往左取第一个不可信地址，否则用 socket 地址）。
    let real_ip = {
        let (trust, trusted) = {
            let config = state.config.read().await;
            (
                config.server.proxy.trust_proxy_headers,
                config.server.proxy.trusted_ips.clone(),
            )
        };
        crate::api::auth::pick_client_ip(
            trust,
            &trusted,
            &crate::api::auth::peer_ip(peer),
            &headers,
        )
    };

    let client = if service.insecure_skip_verify {
        match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("创建代理客户端失败：{err}"),
                );
            }
        }
    } else {
        state.http.clone()
    };

    let mut request = client.request(method.clone(), &target);

    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        // 来源头不透传，转发前用 real_ip 重建（见上方 real_ip 注释）
        if lower == "x-forwarded-for" || lower == "x-real-ip" {
            continue;
        }
        // 本站会话 Cookie 绝不下发给上游：Cookie 是浏览器自动带上的，
        // 上游哪怕只记一条访问日志，也等于把 asmoday 的 session token 交了出去。
        // 上游自己的 Cookie 照常转发（只摘掉 `asmoday_session` 这一条）。
        if lower == header::COOKIE.as_str() {
            if let Some(filtered) = strip_session_cookie(value) {
                request = request.header(name, filtered);
            }
            continue;
        }
        request = request.header(name, value);
    }
    for (name, value) in &service.inject_headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request = request
        .header("x-forwarded-for", real_ip.as_str())
        .header("x-real-ip", real_ip.as_str());

    if !body.is_empty() {
        request = request.body(body);
    }

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Upstream unreachable: {} ({err})", service.target_url),
            );
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    let mut builder = Response::builder().status(status);
    let mut saw_csp = false;
    for (name, value) in upstream_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if DROP_RESPONSE.contains(&lower.as_str()) {
            continue;
        }
        // CSP 只剥 frame-ancestors（允许 iframe），其余指令透传
        if lower == "content-security-policy" || lower == "content-security-policy-report-only" {
            saw_csp = saw_csp || lower == "content-security-policy";
            if let Some(stripped) = value.to_str().ok().and_then(strip_frame_ancestors)
                && let Ok(stripped) = HeaderValue::from_str(&stripped)
            {
                builder = builder.header(name, stripped);
            }
            continue;
        }
        // 上游同名 Set-Cookie 会把浏览器的 asmoday 会话顶掉（等于被上游登出），
        // 也不能让它反向覆盖本站 Cookie。
        if lower == header::SET_COOKIE.as_str() && is_session_cookie(value) {
            continue;
        }
        if service.rewrite_html && lower == "content-length" {
            continue;
        }
        builder = builder.header(name, value);
    }
    // 上游没有任何 CSP 时补一条只管 frame-ancestors 的兜底：第三方页面与本站
    // 同源，不能既没有上游 CSP 又没有防点击劫持声明。
    if !saw_csp {
        builder = builder.header(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'self'");
    }

    // 老旧后台的绝对路径改写
    if service.rewrite_html && is_html(&upstream_headers) {
        let base = service.target_url.trim_end_matches('/');
        return match upstream.text().await {
            Ok(text) => {
                let rewritten = text.replace(base, &format!("/proxy/{service_id}"));
                let body = Body::from(rewritten.clone());
                match builder
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header(header::CONTENT_LENGTH, rewritten.len())
                    .body(body)
                {
                    Ok(response) => response,
                    Err(err) => error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("构造代理响应失败：{err}"),
                    ),
                }
            }
            Err(err) => error_response(StatusCode::BAD_GATEWAY, format!("读取上游响应失败：{err}")),
        };
    }

    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| std::io::Error::other(err.to_string())));

    match builder.body(Body::from_stream(stream)) {
        Ok(response) => response,
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("构造代理响应失败：{err}"),
        ),
    }
}

/// 双向桥接浏览器 ↔ 上游 WebSocket。
async fn bridge_ws(socket: WebSocket, target: String, inject: Vec<(String, String)>) {
    let mut request = match target.clone().into_client_request() {
        Ok(request) => request,
        Err(_) => return,
    };
    for (name, value) in inject {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            request.headers_mut().insert(name, value);
        }
    }

    let upstream = match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _)) => stream,
        Err(err) => {
            eprintln!("[asmoday] proxied websocket failed to reach upstream ({target}): {err}");
            return;
        }
    };

    let (mut client_tx, mut client_rx) = socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    loop {
        tokio::select! {
            message = client_rx.next() => {
                let Some(Ok(message)) = message else { break };
                let outbound = match message {
                    Message::Text(text) => WsMessage::Text(text.to_string().into()),
                    Message::Binary(data) => WsMessage::Binary(data),
                    Message::Ping(data) => WsMessage::Ping(data),
                    Message::Pong(data) => WsMessage::Pong(data),
                    Message::Close(_) => break,
                };
                if upstream_tx.send(outbound).await.is_err() {
                    break;
                }
            }
            message = upstream_rx.next() => {
                let Some(Ok(message)) = message else { break };
                let inbound = match message {
                    WsMessage::Text(text) => Message::Text(text.to_string().into()),
                    WsMessage::Binary(data) => Message::Binary(data),
                    WsMessage::Ping(data) => Message::Ping(data),
                    WsMessage::Pong(data) => Message::Pong(data),
                    WsMessage::Close(_) => break,
                    WsMessage::Frame(_) => continue,
                };
                if client_tx.send(inbound).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_never_reaches_upstream() {
        let raw =
            HeaderValue::from_static("asmoday_session=secret123; grafana_user=admin; theme=dark");
        let filtered = strip_session_cookie(&raw).expect("其余 cookie 应保留");
        let text = filtered.to_str().unwrap();
        assert!(!text.contains("secret123"), "会话 token 被下发了：{text}");
        assert!(text.contains("grafana_user=admin"), "{text}");
        assert!(text.contains("theme=dark"), "{text}");
    }

    #[test]
    fn cookie_header_is_dropped_when_only_session_left() {
        let raw = HeaderValue::from_static("asmoday_session=secret123");
        assert!(
            strip_session_cookie(&raw).is_none(),
            "只剩本站会话时整个头都不该发"
        );
    }

    #[test]
    fn upstream_set_cookie_cannot_hijack_session() {
        assert!(is_session_cookie(&HeaderValue::from_static(
            "asmoday_session=attacker; Path=/; HttpOnly"
        )));
        assert!(is_session_cookie(&HeaderValue::from_static(
            "ASMODAY_SESSION=attacker"
        )));
        assert!(!is_session_cookie(&HeaderValue::from_static(
            "grafana_session=ok; Path=/"
        )));
    }

    #[test]
    fn csp_keeps_xss_defenses_without_frame_ancestors() {
        // 只剥 frame-ancestors，其余指令（XSS 防线）必须保留
        let csp = "default-src 'self'; frame-ancestors 'none'; script-src 'self'";
        let stripped = strip_frame_ancestors(csp).expect("非空 CSP 应保留");
        assert!(!stripped.contains("frame-ancestors"), "{stripped}");
        assert!(stripped.contains("default-src 'self'"), "{stripped}");
        assert!(stripped.contains("script-src 'self'"), "{stripped}");

        // 大小写不敏感 + 无空格写法
        let stripped = strip_frame_ancestors("default-src *;FRAME-ANCESTORS *").unwrap();
        assert!(!stripped.contains("ANCESTORS"), "{stripped}");

        // 整条 CSP 只有 frame-ancestors：剥完为空，交给兜底逻辑
        assert!(strip_frame_ancestors("frame-ancestors 'self'").is_none());
    }
}
