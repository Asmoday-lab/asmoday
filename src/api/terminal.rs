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

use crate::AppState;
use crate::core::fs::{self, FsError};

#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    pub path: Option<String>,
    /// 挂载点 id（设计文档 3.2.2）。传了则 CWD = 挂载点根 + `path` 相对部分，
    /// 经 `path_guard` 归属校验后才交给 PTY；未传时 `path` 按绝对路径解析。
    pub mount_id: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub session_id: Option<String>,
    /// 纯净模式（设计文档 3.3.7）：跳过 profile/rc，用于配置文件把终端卡死的救急场景
    #[serde(default, deserialize_with = "crate::api::boolish")]
    pub no_profile: Option<bool>,
}

/// WS /api/terminal/ws?path=&cols=&rows=&session_id=
///
/// `path` 可选。**未指定目录时前端必须传空字符串或直接省略**，
/// 由后端用 `dirs::home_dir()` 解析真实主目录（Windows `C:\Users\<name>`、
/// Linux `/home/<name>`），不要传 `~` / `%USERPROFILE%` / `$HOME`。
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
    Query(query): Query<TerminalQuery>,
) -> Response {
    // CSWSH 防线：握手是 GET，鉴权中间件的同源校验对 GET 不生效，
    // 跨站页面一句 `new WebSocket("ws://host/api/terminal/ws")` 就能开一个 shell。
    if !crate::api::auth::ws_origin_allowed(&headers) {
        return crate::api::error::api_error(
            axum::http::StatusCode::FORBIDDEN,
            "E_FORBIDDEN",
            "Cross-site WebSocket handshake rejected",
        );
    }

    let roots = {
        let config = state.config.read().await;
        fs::load_roots(&config, &state.cwd)
    };

    // 挂载点集合与终端拉起覆盖（设计文档 3.3.5）：同一次读锁拿全
    let (mounts, shell_override, args_override) = {
        let config = state.config.read().await;
        (
            crate::core::path_guard::load(&config, &state.cwd),
            config.terminal.shell.trim().to_string(),
            config.terminal.args.clone(),
        )
    };

    // 主目录：dirs 是跨平台唯一可靠来源（Windows 走 Known Folder API，
    // Linux/macOS 依次看 $HOME、passwd、NSHomeDirectory，自己读环境变量会漏）
    let home: Option<PathBuf> = dirs::home_dir().filter(|path| path.is_dir());

    // 终端的工作目录白名单 = 挂载点 + 用户主目录。
    // 设计文档 3.3 要求默认落在主目录，但主目录通常不在挂载点内；若不放行，
    // 前端把返回的 cwd 作为 path 重连时就会被 403 拒绝（表现为"终端连不上"）。
    // 注意：这只影响终端，文件管理仍严格限定在挂载点内。
    let mut allowed = roots.allowed.clone();
    if let Some(home) = &home
        && let Ok(canon) = std::fs::canonicalize(home)
        && !allowed.iter().any(|root| root == &canon)
    {
        allowed.push(canon);
    }

    // 目录被删掉是常态（文件管理里刚删过），这时退回默认目录继续开终端，
    // 而不是把 WS 握手直接拒掉——浏览器读不到握手失败的响应体，用户只会看到"连不上"。
    let mut warning: Option<String> = None;

    let fallback_cwd = || default_cwd(&roots, home.as_deref());

    // CWD 解析（设计文档 3.2.2 / 6.3）：mount_id 优先（挂载根 + 相对 path），
    // 其次绝对 path，最后默认目录。归属校验统一走 path_guard，越权一律拒绝。
    let mut cwd: PathBuf = if let Some(id) = query
        .mount_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match mounts.find(id) {
            Some(mount) if mount.root.is_some() => {
                let raw = query.path.as_deref().unwrap_or("");
                match crate::core::path_guard::resolve_existing(mount, raw) {
                    Ok(path) => {
                        if path.is_dir() {
                            path
                        } else {
                            // 文件也可以作为起点：取其所在目录（与文件管理「在此打开终端」一致）
                            path.parent().map(Path::to_path_buf).unwrap_or(path)
                        }
                    }
                    Err(FsError::NotFound(_)) => {
                        warning = Some(format!("目录已不存在（挂载点 {id}），已切换到默认目录"));
                        match fallback_cwd() {
                            Some(path) => path,
                            None => {
                                return reject(FsError::NotFound("没有可用的挂载点".to_string()));
                            }
                        }
                    }
                    Err(err) => return reject(err),
                }
            }
            _ => {
                warning = Some("挂载点不存在或已离线，已切换到默认目录".to_string());
                match fallback_cwd() {
                    Some(path) => path,
                    None => return reject(FsError::NotFound("没有可用的挂载点".to_string())),
                }
            }
        }
    } else {
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
        match requested.as_deref() {
            Some(raw) => match fs::resolve_existing(raw, &allowed, roots.follow_symlinks) {
                Ok(path) => {
                    if path.is_dir() {
                        path
                    } else {
                        path.parent().map(Path::to_path_buf).unwrap_or(path)
                    }
                }
                Err(FsError::NotFound(_)) => {
                    warning = Some(format!("目录已不存在（{raw}），已切换到默认目录"));
                    match fallback_cwd() {
                        Some(path) => path,
                        None => return reject(FsError::NotFound("没有可用的挂载点".to_string())),
                    }
                }
                // 越权一律拒绝，不能因为"路径不存在"以外的原因退回默认目录
                Err(err) => return reject(err),
            },
            None => match fallback_cwd() {
                Some(path) => path,
                None => return reject(FsError::NotFound("没有可用的挂载点".to_string())),
            },
        }
    };

    // 兜底：目录已不存在时退回工作目录
    if !cwd.is_dir() {
        cwd = state.cwd.clone();
    }

    let (cols, rows) = crate::core::terminal::clamp_size(
        query.cols.unwrap_or(120) as u64,
        query.rows.unwrap_or(30) as u64,
    );
    let session_id = query.session_id.filter(|s| !s.trim().is_empty());
    let options = crate::core::terminal::ShellOptions {
        no_profile: query.no_profile.unwrap_or(false),
        // 配置覆盖（设计文档 3.3.5）：[terminal].shell > 自动探测；args 非空完全替换
        shell: shell_override,
        args: args_override,
    };

    // 历史记录强制隔离（设计文档 3.3.5）：不做探测、不做条件分支，
    // 一律把 shell 历史钉在 <data>/terminal_history/，绝不碰 %APPDATA% / ~。
    // 权限受限与重定向本身不提示（前端必须静默），warning 只保留目录落点变化。
    let env = crate::core::terminal::prepare_env(&state.data_dir);

    ws.on_upgrade(move |socket| {
        handle_socket(
            socket, state, cwd, cols, rows, session_id, options, env, warning,
        )
    })
}

/// 默认终端目录（设计文档 3.3）：优先用户主目录，其次默认挂载点。
fn default_cwd(roots: &fs::RootSet, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = home {
        return Some(home.to_path_buf());
    }
    let root = roots.default_root()?;
    fs::resolve_existing(&root.path.clone(), &roots.allowed, roots.follow_symlinks).ok()
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
    } else if let Some(rest) = text
        .strip_prefix("$HOME")
        .or_else(|| text.strip_prefix("$home"))
    {
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

// 一个 WS 处理函数要拿全「连接 + 状态 + 会话参数」，拆成结构体只是把参数
// 挪个地方，反而看不出哪些是连接期的、哪些是创建期就固定下来的。
#[allow(clippy::too_many_arguments)]
async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    session_id: Option<String>,
    options: crate::core::terminal::ShellOptions,
    env: crate::core::terminal::TerminalEnv,
    warning: Option<String>,
) {
    let manager = state.terminals.clone();

    // 重连优先复用；但模式变了（普通 ↔ 纯净）必须重建：profile 是进程启动时
    // 加载的，已经跑起来的 shell 变不回去
    let existing = session_id
        .as_deref()
        .and_then(|id| manager.get(id))
        .filter(|session| session.no_profile == options.no_profile);
    let reconnected = existing.is_some();
    let session = match existing {
        Some(session) => session,
        None => match manager.create(&cwd, cols, rows, options, &env) {
            Ok(session) => session,
            Err(message) => {
                let mut socket = socket;
                // 带上错误码：并发打满时前端要能精准打开「活动会话」面板，
                // 只给一句中文提示，用户还是不知道该关哪个
                let payload = json!({
                    "type": "error",
                    "code": "E_TOO_MANY",
                    "message": message,
                });
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
        // 纯净模式与平台：前端据此显示开关状态与 Windows 的「修复乱码」按钮
        "no_profile": session.no_profile,
        "platform": crate::core::terminal::platform(),
        // 目录被删 / 受限环境等非致命情况：连接照建，但要让用户知道
        "warning": warning,
    });
    if sink
        .send(Message::Text(ready.to_string().into()))
        .await
        .is_err()
    {
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
                                // 尺寸强制兜底（设计文档 3.3.2）：resize 帧与首连同一套钳制，
                                // 0 / 超大值绝不透传给 ConPTY（否则整块终端崩溃）
                                let (next_cols, next_rows) =
                                    crate::core::terminal::clamp_size(next_cols, next_rows);
                                let _ = session.resize(next_cols, next_rows);
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

/// GET /api/terminal/sessions：活动会话列表（设计文档 3.3.6）
///
/// 返回明细而不只是数量：并发打满时前端要能列出「谁在占着」并让用户踢掉，
/// 否则用户只会看到「已达上限」却无从下手。
pub async fn list_sessions(State(state): State<AppState>) -> Response {
    let manager = state.terminals.clone();
    let sessions = manager.snapshot();
    axum::Json(json!({
        "count": sessions.len(),
        "limit": manager.max_sessions(),
        "sessions": sessions,
    }))
    .into_response()
}

/// DELETE /api/terminal/sessions/{id}：强制关闭一条遗留 PTY（设计文档 3.3.6）
pub async fn kill_session(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if state.terminals.force_close(&id) {
        axum::Json(json!({ "status": "ok" })).into_response()
    } else {
        crate::api::error::api_error(
            axum::http::StatusCode::NOT_FOUND,
            "E_NOT_FOUND",
            "会话不存在或已结束",
        )
    }
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
        assert_eq!(
            expand_home("$HOME/docs", Some(home)),
            Some(home.join("docs"))
        );
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

    #[test]
    fn clamps_pty_size_from_any_source() {
        use crate::core::terminal::clamp_size;
        // 0 / 负数（u64 下负数早就被 serde 拒了，这里覆盖 0）绝不能直通 PTY
        assert_eq!(clamp_size(0, 0), (20, 5));
        // 超大值（前端误报窗口宽度、外接屏切换瞬间）夹到上限，
        // 而不是 `as u16` 截断成 4464 这种垃圾值
        assert_eq!(clamp_size(70_000, 9_000), (500, 200));
        // 合法值原样保留：高分屏 200 列 × 60 行必须不被改写
        assert_eq!(clamp_size(200, 60), (200, 60));
        // u64::MAX 也安全（serde 的 as_u64 可能给出任意大数）
        assert_eq!(clamp_size(u64::MAX, u64::MAX), (500, 200));
    }
}
