//! 内置反向代理网关（设计文档 3.6）。
//!
//! `/proxy/{service_id}/…` → `[[services]].target_url/…`
//!
//! - 自动剥离 `X-Frame-Options` / `CSP`，允许 iframe 内嵌；
//! - 支持 WebSocket Upgrade 透传（双向桥接）；
//! - 支持 `inject_headers` 注入（实现服务间免登录）；
//! - `rewrite_html` 打开时，把目标站点的绝对地址改写为代理路径（兼容老旧后台）；
//! - `strip_prefix` 语义由路由天然实现：`/proxy/<id>` 一定被剥掉。

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::core::config::ServiceConfig;
use crate::AppState;

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

/// 需要从响应里剔除的头（含允许 iframe 内嵌的两项）
const DROP_RESPONSE: &[&str] = &[
    "x-frame-options",
    "content-security-policy",
    "content-security-policy-report-only",
    "transfer-encoding",
    "connection",
    "keep-alive",
];

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
                    eprintln!("[asmoday] WebSocket 升级被拒绝：{err:?}");
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
        .ok_or_else(|| error_code(StatusCode::NOT_FOUND, "SERVICE_NOT_FOUND", format!("Unknown service: {id}")))
}

fn build_target(base: &str, rest: &str, query: Option<&str>) -> String {
    let base = base.trim_end_matches('/');
    let rest = rest.trim_start_matches('/');
    let mut url = if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rest}")
    };
    if let Some(query) = query {
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
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

fn is_websocket(headers: &HeaderMap) -> bool {    headers
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

/// `/proxy/{service_id}`
pub async fn handle_root(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: MaybeWs,
    body: Bytes,
) -> Response {
    dispatch(state, service_id, String::new(), method, uri, headers, ws, body).await
}

/// `/proxy/{service_id}/{*rest}`
pub async fn handle_rest(
    State(state): State<AppState>,
    Path((service_id, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: MaybeWs,
    body: Bytes,
) -> Response {
    dispatch(state, service_id, rest, method, uri, headers, ws, body).await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: AppState,
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
        request = request.header(name, value);
    }
    for (name, value) in &service.inject_headers {
        request = request.header(name.as_str(), value.as_str());
    }

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
    for (name, value) in upstream_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if DROP_RESPONSE.contains(&lower.as_str()) {
            continue;
        }
        if service.rewrite_html && lower == "content-length" {
            continue;
        }
        builder = builder.header(name, value);
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
            Err(err) => error_response(
                StatusCode::BAD_GATEWAY,
                format!("读取上游响应失败：{err}"),
            ),
        };
    }

    let stream = upstream.bytes_stream().map(|chunk| {
        chunk.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))
    });

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
            eprintln!("[asmoday] 反代 WebSocket 连接上游失败（{target}）：{err}");
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
