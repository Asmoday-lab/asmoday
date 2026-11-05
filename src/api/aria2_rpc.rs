//! aria2 兼容 JSON-RPC 端点（v4.9.26，设计文档 3.10）。
//!
//! `POST /jsonrpc`（JSON-RPC 2.0，支持批量数组）、`GET /jsonrpc?method=…&params=…`
//! 与 `WS /jsonrpc`（双向：请求 + `aria2.on*` 通知推送）。
//!
//! 鉴权：每个请求的 `params` 第一项必须是 `token:<rpc_secret>`——密钥来自
//! `[download].rpc_secret`，未配置时用 `<data>/download_rpc_secret` 里持久化的
//! 自动生成值（main 启动时落盘并打印）。校验失败统一返回 aria2 语义的
//! `{"code":1,"message":"Unauthorized"}`。
//!
//! 兼容面刻意收窄并**诚实声明**：`getVersion.enabledFeatures` 不含
//! BitTorrent / Metalink / FTP，客户端会自动隐藏对应 UI，不留假入口。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::AppState;
use crate::core::download::{RpcError, WindowList};

/// 方法清单（system.listMethods；通知另列）
const METHODS: &[&str] = &[
    "aria2.addUri",
    "aria2.remove",
    "aria2.forceRemove",
    "aria2.pause",
    "aria2.forcePause",
    "aria2.unpause",
    "aria2.tellStatus",
    "aria2.tellActive",
    "aria2.tellWaiting",
    "aria2.tellStopped",
    "aria2.getOption",
    "aria2.changeOption",
    "aria2.getGlobalOption",
    "aria2.changeGlobalOption",
    "aria2.getGlobalStat",
    "aria2.purgeDownloadResult",
    "aria2.removeDownloadResult",
    "aria2.getVersion",
    "aria2.getSessionInfo",
    "aria2.shutdown",
    "aria2.forceShutdown",
    "system.multicall",
    "system.listMethods",
    "system.listNotifications",
];

const NOTIFICATIONS: &[&str] = &[
    "aria2.onDownloadStart",
    "aria2.onDownloadPause",
    "aria2.onDownloadStop",
    "aria2.onDownloadComplete",
    "aria2.onDownloadError",
    "aria2.onBtDownloadComplete",
];

/// 统一入口（`any("/jsonrpc")`）：WS 升级 / GET 参数式 / POST JSON-RPC。
/// `[download].enable=false` 时整个端点 404（无引擎暴露面）。
pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    if !state.downloads.enabled() {
        return plain_error(StatusCode::NOT_FOUND, "download engine is disabled");
    }
    let is_ws = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_ws {
        return websocket(state, request).await;
    }
    match *request.method() {
        axum::http::Method::GET => get_call(&state, request).await,
        axum::http::Method::POST => post_call(&state, request).await,
        _ => plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

// ---------------------------------------------------------------------------
// HTTP 入口
// ---------------------------------------------------------------------------

async fn post_call(state: &AppState, request: Request) -> Response {
    let body = match axum::body::to_bytes(
        request.into_body(),
        4 * 1024 * 1024, // RPC 请求都是 KB 级；4 MiB 覆盖极端批量
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(_) => return plain_error(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => return rpc_fault_json_response(Value::Null, 2, format!("parse error: {err}")),
    };
    let response = dispatch_value(state, parsed).await;
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&response).unwrap_or_else(|_| "{}".into()),
    )
        .into_response()
}

async fn get_call(state: &AppState, request: Request) -> Response {
    // aria2 的 GET 形态：?method=aria2.tellActive&params=(percent-encoded JSON array)
    let query = request.uri().query().unwrap_or_default();
    let mut method = None;
    let mut params = Vec::<Value>::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "method" => method = Some(percent_decode(value)),
            "params" => {
                params = serde_json::from_str(&percent_decode(value)).unwrap_or_default();
            }
            _ => {}
        }
    }
    let Some(method) = method else {
        return plain_error(StatusCode::BAD_REQUEST, "missing method");
    };
    let id = Value::String("get".into());
    let response = call(state, &method, params, &id).await;
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&response).unwrap_or_else(|_| "{}".into()),
    )
        .into_response()
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

// ---------------------------------------------------------------------------
// WebSocket：请求 + 通知推送
// ---------------------------------------------------------------------------

async fn websocket(state: AppState, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        Err(_) => return plain_error(StatusCode::BAD_REQUEST, "expected websocket upgrade"),
    };
    upgrade.on_upgrade(move |socket| ws_loop(state, socket))
}

async fn ws_loop(state: AppState, mut socket: WebSocket) {
    let mut events = state.downloads.subscribe();
    loop {
        tokio::select! {
            // 任务事件 → aria2.on* 通知
            notification = events.recv() => {
                match notification {
                    Ok((gid, event)) => {
                        let payload = json!({
                            "jsonrpc": "2.0",
                            "method": event.method(),
                            "params": [gid],
                        });
                        if socket
                            .send(Message::Text(payload.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // 客户端请求（与 POST 同一套分派）
            frame = socket.recv() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(value) => value,
                            Err(err) => {
                                let fault = rpc_fault_json(Value::Null, 2, format!("parse error: {err}"));
                                if socket.send(Message::Text(fault.to_string().into())).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };
                        let response = dispatch_value(&state, parsed).await;
                        if socket
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // 二进制帧不是本协议的一部分
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC 分派
// ---------------------------------------------------------------------------

/// 顶层分派：单个请求 / 批量数组
async fn dispatch_value(state: &AppState, value: Value) -> Value {
    match value {
        Value::Array(batch) => {
            let results = futures_util::future::join_all(
                batch.into_iter().map(|item| dispatch_single(state, item)),
            )
            .await;
            Value::Array(results)
        }
        single => dispatch_single(state, single).await,
    }
}

async fn dispatch_single(state: &AppState, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = request
        .get("params")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    call(state, &method, params, &id).await
}

/// 单方法调用 → JSON-RPC 响应
async fn call(state: &AppState, method: &str, params: Vec<Value>, id: &Value) -> Value {
    // 鉴权：params[0] 必须是 token:<secret>（aria2 语义）
    match params.first().and_then(Value::as_str) {
        Some(token) if state.downloads.secret_matches(token) => {}
        _ => return rpc_fault_json(id.clone(), 1, "Unauthorized".to_string()),
    }

    let args = &params[1..];
    match run_method(state, method, args).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(err) => rpc_fault_json(id.clone(), err.code, err.message),
    }
}

fn rpc_fault_json(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// 请求体都没解析出来时的 fault 响应（id = null）
fn rpc_fault_json_response(id: Value, code: i64, message: String) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&rpc_fault_json(id, code, message)).unwrap_or_else(|_| "{}".into()),
    )
        .into_response()
}

async fn run_method(state: &AppState, method: &str, args: &[Value]) -> Result<Value, RpcError> {
    let manager = &state.downloads;
    match method {
        // -- 任务创建 -------------------------------------------------------
        "aria2.addUri" => {
            let uris = string_list(
                args.first()
                    .and_then(Value::as_array)
                    .ok_or_else(|| RpcError::new(2, "uris is required"))?,
            );
            let options = args.get(1).cloned().unwrap_or(Value::Null);
            let dir = option_str(&options, "dir");
            let out = option_str(&options, "out");
            let user_agent = option_str(&options, "user-agent");
            let segments = option_str(&options, "max-connection-per-server")
                .and_then(|v| v.parse::<usize>().ok());
            let headers = option_header_list(&options);
            let gid = manager
                .add_uri(uris, dir, out, user_agent, headers, segments)
                .await?;
            Ok(Value::String(gid))
        }

        // -- 任务控制 -------------------------------------------------------
        "aria2.remove" | "aria2.forceRemove" => {
            let gid = arg_str(args, 0, "gid")?;
            manager.remove(&gid).await.map(Value::String)
        }
        "aria2.pause" => {
            let gid = arg_str(args, 0, "gid")?;
            manager.pause(&gid).await.map(Value::String)
        }
        "aria2.forcePause" => {
            let gid = arg_str(args, 0, "gid")?;
            // force 语义：不可暂停的状态不报错
            match manager.pause(&gid).await {
                Ok(gid) => Ok(Value::String(gid)),
                Err(_) => Ok(Value::String(gid)),
            }
        }
        "aria2.unpause" => {
            let gid = arg_str(args, 0, "gid")?;
            manager.unpause(&gid).await.map(Value::String)
        }

        // -- 查询 -----------------------------------------------------------
        "aria2.tellStatus" => {
            let gid = arg_str(args, 0, "gid")?;
            let keys = args.get(1).and_then(Value::as_array).map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
            manager
                .status_with_keys(&gid, keys.as_deref())
                .await
                .ok_or_else(|| unknown_gid(&gid))
        }
        "aria2.tellActive" => {
            let keys = args_key_filter(args);
            let list = manager.tell_active().await;
            Ok(filter_keys(list, keys))
        }
        "aria2.tellWaiting" => {
            let offset = arg_usize(args, 0)?;
            let num = arg_usize(args, 1)?;
            let keys = args_key_filter(args.get(2..).unwrap_or(&[]));
            let list = manager.window(WindowList::Waiting, offset, num).await;
            Ok(filter_keys(list, keys))
        }
        "aria2.tellStopped" => {
            let offset = arg_usize(args, 0)?;
            let num = arg_usize(args, 1)?;
            let keys = args_key_filter(args.get(2..).unwrap_or(&[]));
            let list = manager.window(WindowList::Stopped, offset, num).await;
            Ok(filter_keys(list, keys))
        }
        "aria2.getGlobalStat" => Ok(manager.global_stat().await),

        // -- 选项 -----------------------------------------------------------
        "aria2.getOption" => {
            let gid = arg_str(args, 0, "gid")?;
            manager
                .option_of(&gid)
                .await
                .ok_or_else(|| unknown_gid(&gid))
        }
        "aria2.changeOption" => {
            let gid = arg_str(args, 0, "gid")?;
            let options = args.get(1).cloned().unwrap_or(Value::Null);
            manager
                .change_option(&gid, &options)
                .await
                .ok_or_else(|| unknown_gid(&gid))?;
            Ok(Value::String("OK".into()))
        }
        "aria2.getGlobalOption" => Ok(manager.global_options()),
        "aria2.changeGlobalOption" => Ok(Value::String("OK".into())),

        // -- 结果维护 -------------------------------------------------------
        "aria2.purgeDownloadResult" => {
            manager.purge().await;
            Ok(Value::String("OK".into()))
        }
        "aria2.removeDownloadResult" => {
            let gid = arg_str(args, 0, "gid")?;
            manager.remove_result(&gid).await?;
            Ok(Value::String("OK".into()))
        }

        // -- 元信息 ---------------------------------------------------------
        "aria2.getVersion" => Ok(json!({
            // 对外报告 aria2 稳定版语义版本；能力差异由 enabledFeatures 声明
            "version": "1.37.0",
            // 不含 BitTorrent / Metalink / FTP / SFTP：客户端据此隐藏对应入口
            "enabledFeatures": [
                "Asmoday",
                "Async DNS",
                "GZip",
                "HTTPS",
                "Message Digest",
                "Multi-Connection",
                "Resumable",
            ],
        })),
        "aria2.getSessionInfo" => Ok(json!({"sessionId": session_id()})),
        "aria2.shutdown" | "aria2.forceShutdown" => {
            // 引擎不随 RPC 关停进程——下载任务无损，进程管理走系统服务
            Ok(Value::String("OK".into()))
        }

        "system.multicall" => {
            let calls = args
                .first()
                .and_then(Value::as_array)
                .ok_or_else(|| RpcError::new(2, "multicall expects an array"))?;
            let mut results = Vec::with_capacity(calls.len());
            for call_item in calls {
                let name = call_item
                    .get("methodName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let call_params = call_item
                    .get("params")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // aria2 语义：内层调用的 params **不含** token——鉴权在 multicall
                // 层做一次即可，内层直接走 run_method（不重复鉴权）
                let outcome = match Box::pin(run_method(state, &name, &call_params)).await {
                    Ok(result) => json!([result]),
                    Err(err) => json!({
                        "faultCode": err.code,
                        "faultString": err.message,
                    }),
                };
                results.push(outcome);
            }
            Ok(Value::Array(results))
        }
        "system.listMethods" => Ok(Value::Array(METHODS.iter().map(|m| json!(m)).collect())),
        "system.listNotifications" => Ok(Value::Array(
            NOTIFICATIONS.iter().map(|m| json!(m)).collect(),
        )),

        other => Err(RpcError::new(2, format!("method {other} is not available"))),
    }
}

fn unknown_gid(gid: &str) -> RpcError {
    RpcError::new(2, format!("Cannot get download information: GID {gid}"))
}

// ---------------------------------------------------------------------------
// 参数解析小工具
// ---------------------------------------------------------------------------

fn arg_str(args: &[Value], index: usize, name: &str) -> Result<String, RpcError> {
    args.get(index)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcError::new(2, format!("{name} is required")))
}

fn arg_usize(args: &[Value], index: usize) -> Result<usize, RpcError> {
    args.get(index)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .ok_or_else(|| RpcError::new(2, "expected a non-negative integer"))
}

/// tellActive(secret, keys?) 的 keys 在第 1 位
fn args_key_filter(args: &[Value]) -> Option<Vec<String>> {
    args.first().and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn filter_keys(list: Vec<Value>, keys: Option<Vec<String>>) -> Value {
    let Some(keys) = keys.filter(|k| !k.is_empty()) else {
        return Value::Array(list);
    };
    Value::Array(
        list.into_iter()
            .map(|item| {
                let mut filtered = serde_json::Map::new();
                if let Some(gid) = item.get("gid") {
                    filtered.insert("gid".into(), gid.clone());
                }
                for key in &keys {
                    if let Some(field) = item.get(key) {
                        filtered.insert(key.clone(), field.clone());
                    }
                }
                Value::Object(filtered)
            })
            .collect(),
    )
}

fn string_list(list: &[Value]) -> Vec<String> {
    list.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn option_str(options: &Value, key: &str) -> Option<String> {
    options.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `header` 选项：["Name: value", ...] → (name, value) 列表
fn option_header_list(options: &Value) -> Vec<(String, String)> {
    options
        .get("header")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    Some((name.trim().to_string(), value.trim().to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // '+' 在 query 串里是空格（aria2 客户端按 URL 编码发送）
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 会话标识（aria2.getSessionInfo 的 sessionId）：进程启动即固定
fn session_id() -> String {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("{:016x}", rand::random::<u64>()))
        .clone()
}
