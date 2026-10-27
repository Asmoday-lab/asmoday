//! 终端 WebSocket 接口（设计文档 6.3）。
//!
//! 协议约定：
//! - 服务端 → 客户端：`Text` 为 JSON 控制帧（`{"type":"ready",...}`），`Binary` 为 PTY 原始字节流；
//! - 客户端 → 服务端：`Binary` 为键盘原始字节，`Text` 为 JSON 控制帧（目前仅 `{"type":"resize",...}`），
//!   Text 无法解析为 JSON 时按普通输入写入。
//!
//! 用 Binary/Text 分流而不是给键盘数据加前缀，避免用户输入 `{` 被误判成控制帧。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::core::fs::{self, FsError};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    pub path: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub session_id: Option<String>,
}

/// WS /api/terminal/ws?path=&cols=&rows=&session_id=
///
/// `path` 可选。**未指定目录时前端必须传空字符串或直接省略**，
/// 由后端用 `dirs::home_dir()` 解析真实主目录（Windows `C:\Users\<name>`、
/// Linux `/home/<name>`），不要传 `~` / `%USERPROFILE%` / `$HOME`。
pub async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    Query(query): Query<TerminalQuery>,
) -> Response {
    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };

    // 主目录：dirs 是跨平台唯一可靠来源（Windows 走 Known Folder API，
    // Linux/macOS 依次看 $HOME、passwd、NSHomeDirectory，自己读环境变量会漏）
    let home: Option<PathBuf> = dirs::home_dir().filter(|path| path.is_dir());

    // 终端的工作目录白名单 = 挂载点 + 用户主目录。
    // 设计文档 3.3 要求默认落在主目录，但主目录通常不在挂载点内；若不放行，
    // 前端把返回的 cwd 作为 path 重连时就会被 403 拒绝（表现为"终端连不上"）。
    // 注意：这只影响终端，文件管理仍严格限定在挂载点内。
    let mut allowed = roots.allowed.clone();
    if let Some(home) = &home {
        if let Ok(canon) = std::fs::canonicalize(home) {
            if !allowed.iter().any(|root| root == &canon) {
                allowed.push(canon);
            }
        }
    }

    // 目录被删掉是常态（文件管理里刚删过），这时退回默认目录继续开终端，
    // 而不是把 WS 握手直接拒掉——浏览器读不到握手失败的响应体，用户只会看到"连不上"。
    let mut warning: Option<String> = None;

    // 占位符兜底：万一前端（或旧缓存里的 JS）传了 `~`、`$HOME`、`%USERPROFILE%`，
    // 这里展开成真实主目录，避免 Shell 因为字面量路径不存在而拉不起来。
    let requested = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|raw| match expand_home(raw, home.as_deref()) {
            Some(expanded) => {
                crate::log_warn!(
                    "terminal path received a home placeholder '{raw}'; resolved to the system home directory"
                );
                warning =
                    Some("path 不要传 ~ 或 %USERPROFILE%，已按系统主目录解析".to_string());
                expanded.to_string_lossy().into_owned()
            }
            None => raw.to_string(),
        });

    // 目标目录：优先用 path（可以是目录，也可以是文件所在目录），否则用默认根
    let mut cwd: PathBuf = match requested.as_deref() {
        Some(raw) => match fs::resolve_existing(raw, &allowed) {
            Ok(path) => {
                if path.is_dir() {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or(path)
                }
            }
            Err(FsError::NotFound(_)) => {
                warning = Some(format!("目录已不存在（{raw}），已切换到默认目录"));
                match default_cwd(&roots, home.as_deref()) {
                    Some(path) => path,
                    None => return reject(FsError::NotFound("没有可用的挂载点".to_string())),
                }
            }
            // 越权一律拒绝，不能因为"路径不存在"以外的原因退回默认目录
            Err(err) => return reject(err),
        },
        None => match default_cwd(&roots, home.as_deref()) {
            Some(path) => path,
            None => return reject(FsError::NotFound("没有可用的挂载点".to_string())),
        },
    };

    // 兜底：目录已不存在时退回工作目录
    if !cwd.is_dir() {
        cwd = state.cwd.clone();
    }

    let cols = query.cols.unwrap_or(120).clamp(20, 500);
    let rows = query.rows.unwrap_or(30).clamp(5, 200);
    let session_id = query.session_id.filter(|s| !s.trim().is_empty());

    ws.on_upgrade(move |socket| handle_socket(socket, state, cwd, cols, rows, session_id, warning))
}

/// 默认终端目录（设计文档 3.3）：优先用户主目录，其次默认挂载点。
fn default_cwd(roots: &fs::RootSet, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = home {
        return Some(home.to_path_buf());
    }
    let root = roots.default_root()?;
    fs::resolve_existing(&root.path.clone(), &roots.allowed).ok()
}

/// 把 `~`、`~/x`、`$HOME/x`、`%USERPROFILE%\x` 展开成真实路径。
///
/// 只认这些明确的占位符前缀；其余路径原样交给 `PathBuf`（POSIX 风格的
/// `C:/Users/x` 在 Windows 上会被系统 API 正确归一化）。
fn expand_home(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    let text = raw.trim();
    let upper = text.to_ascii_uppercase();

    let rest = if let Some(rest) = text.strip_prefix('~') {
        rest
    } else if let Some(rest) = text.strip_prefix("${HOME}") {
        rest
    } else if upper.starts_with("${HOME}") {
        &text["${HOME}".len()..]
    } else if let Some(rest) = text.strip_prefix("$HOME").or_else(|| text.strip_prefix("$home")) {
        rest
    } else if upper.starts_with("%USERPROFILE%") {
        &text["%USERPROFILE%".len()..]
    } else {
        return None;
    };

    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        Some(home.to_path_buf())
    } else {
        Some(home.join(rest))
    }
}

fn reject(err: FsError) -> Response {
    crate::api::error::fs_error(&err)
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    session_id: Option<String>,
    warning: Option<String>,
) {
    let manager = state.terminals.clone();

    // 重连优先复用；否则新建
    let existing = session_id.as_deref().and_then(|id| manager.get(id));
    let reconnected = existing.is_some();
    let session = match existing {
        Some(session) => session,
        None => match manager.create(&cwd, cols, rows) {
            Ok(session) => session,
            Err(message) => {
                let mut socket = socket;
                let payload = json!({ "type": "error", "message": message });
                let _ = socket.send(Message::Text(payload.to_string().into())).await;
                return;
            }
        },
    };

    session.clients.fetch_add(1, Ordering::SeqCst);
    let _ = session.resize(cols, rows);

    let (mut sink, mut stream) = socket.split();

    let ready = json!({
        "type": "ready",
        "session_id": session.id,
        "cwd": session.cwd,
        "shell": session.shell,
        "reconnected": reconnected,
        "buffered": session.replay().len(),
        // 目录被删等非致命情况：连接照建，但要让用户知道落点变了
        "warning": warning,
    });
    if sink.send(Message::Text(ready.to_string().into())).await.is_err() {
        session.clients.fetch_sub(1, Ordering::SeqCst);
        return;
    }

    // 断线重连：先回放环形缓冲，再追增量
    let replay = session.replay();
    if !replay.is_empty() {
        let _ = sink.send(Message::Binary(replay.into())).await;
    }

    let mut output = session.output.subscribe();

    loop {
        tokio::select! {
            produced = output.recv() => {
                match produced {
                    Ok(data) => {
                        if sink.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        if let Err(message) = session.write_all(&data) {
                            let _ = sink.send(Message::Text(json!({"type":"error","message":message}).to_string().into())).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let control = serde_json::from_str::<serde_json::Value>(&text).ok();
                        match control.and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string)) {
                            Some(kind) if kind == "resize" => {
                                let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                                let next_cols = value.get("cols").and_then(|v| v.as_u64()).unwrap_or(cols as u64);
                                let next_rows = value.get("rows").and_then(|v| v.as_u64()).unwrap_or(rows as u64);
                                let _ = session.resize(next_cols as u16, next_rows as u16);
                            }
                            _ => {
                                // 非控制帧：当作用户输入
                                if let Err(message) = session.write_all(text.as_bytes()) {
                                    let _ = sink.send(Message::Text(json!({"type":"error","message":message}).to_string().into())).await;
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    // 断开：连接数归零则 3 分钟后回收（期间重连可续命）
    session.clients.fetch_sub(1, Ordering::SeqCst);
    manager.schedule_reap(session.id.clone());
}

/// GET /api/terminal/sessions：调试/前端恢复标签页时用
pub async fn list_sessions(State(state): State<AppState>) -> Response {
    let manager = state.terminals.clone();
    axum::Json(json!({ "count": manager.len() })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_placeholders() {
        let home = Path::new(r"C:\Users\tester");
        // 设计文档 6.3：前端不该传占位符，但后端必须兜住
        assert_eq!(expand_home("~", Some(home)), Some(home.to_path_buf()));
        assert_eq!(expand_home("~/docs", Some(home)), Some(home.join("docs")));
        assert_eq!(
            expand_home("%USERPROFILE%\\docs", Some(home)),
            Some(home.join("docs"))
        );
        assert_eq!(
            expand_home("%userprofile%/docs", Some(home)),
            Some(home.join("docs"))
        );
        assert_eq!(expand_home("$HOME/docs", Some(home)), Some(home.join("docs")));
        assert_eq!(
            expand_home("${HOME}/docs", Some(home)),
            Some(home.join("docs"))
        );
    }

    #[test]
    fn leaves_real_paths_alone() {
        let home = Path::new(r"C:\Users\tester");
        // 真实路径原样返回，交给 PathBuf / canonicalize 处理
        assert_eq!(expand_home("C:/Users/tester", Some(home)), None);
        assert_eq!(expand_home(r"C:\Users\tester", Some(home)), None);
        assert_eq!(expand_home("/home/tester", Some(home)), None);
        assert_eq!(expand_home("", Some(home)), None);
        // 拿不到主目录时不做任何猜测
        assert_eq!(expand_home("~", None), None);
    }
}
