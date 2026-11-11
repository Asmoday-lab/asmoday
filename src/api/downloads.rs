//! 下载页的 REST 接口（v4.9.31，设计文档 3.10）。
//!
//! 为什么在 `/jsonrpc` 之外再开一层：RPC 用 `token:<secret>` 鉴权、不走 Cookie
//! 会话，密钥不能塞进浏览器；而下载引擎是**一级功能**（与文件管理、媒体库同级），
//! 页面必须能用登录态直接读写任务。于是：
//!
//! - `/jsonrpc` 保持 aria2 契约不变，供 AriaNg 等外部客户端；
//! - `/api/downloads*` 是同一份 `DownloadManager` 的会话内视图，字段沿用
//!   aria2 的 `task_json`（少一套序列化），额外补 `name` / `kind` 给列表用。
//!
//! 引擎级配置（开关 / 分段数 / 端口）**不在**这里改：它们由 `[download]` 决定，
//! 且是启动时快照，改了必须重启 —— 页面只读回显，改动走 `/api/config` +
//! 「重启生效」提示（见 `engine` 的 `restart_required`）。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use crate::AppState;
use crate::core::bt::TorrentSource;
use crate::core::download::RpcError;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// JSON-RPC fault → HTTP。未找到 GID 归 404，其余归 400（都是参数/状态类问题）。
fn rpc_error(err: RpcError) -> Response {
    let status = if err.message.contains("Cannot get download information") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    error(status, err.message)
}

fn disabled() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "下载引擎未启用（config.toml 的 [download].enable = false）",
    )
}

/// 给 aria2 形状的任务补两个列表用的字段。
///
/// `name`：BT 取 `bittorrent.info.name`（种子名），HTTP 取落盘文件名的最后一段。
/// 元信息还没解析出来的 magnet 会是空串 —— 前端按 GID 兜底显示，不要编名字。
fn decorate(mut task: Value) -> Value {
    let name = task
        .pointer("/bittorrent/info/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            task.pointer("/files/0/path")
                .and_then(Value::as_str)
                .and_then(|path| path.rsplit(['/', '\\']).next())
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let kind = if task.get("bittorrent").is_some() {
        "bt"
    } else {
        "http"
    };
    if let Some(map) = task.as_object_mut() {
        map.insert("name".into(), name.into());
        map.insert("kind".into(), kind.into());
    }
    task
}

/// GET /api/downloads：任务快照 + 全局统计（页面每秒轮询这一个接口）
pub async fn snapshot(State(state): State<AppState>, _admin: AdminUser) -> Response {
    let manager = &state.downloads;
    if !manager.enabled() {
        return Json(json!({ "enabled": false, "bt": false, "tasks": null })).into_response();
    }
    let decorate_all =
        |list: Vec<Value>| -> Vec<Value> { list.into_iter().map(decorate).collect() };
    Json(json!({
        "enabled": true,
        "bt": manager.bt_enabled(),
        "global": manager.global_stat().await,
        "tasks": {
            "active": decorate_all(manager.tell_active().await),
            "waiting": decorate_all(manager.tell_waiting().await),
            "stopped": decorate_all(manager.tell_stopped().await),
        },
    }))
    .into_response()
}

/// GET /api/downloads/engine：引擎参数 + RPC 接入信息
///
/// `roots` 是「挂载点 + 子路径」的解析结果（v4.9.37）：`label` 给页面显示
/// （`电影 / downloads`），`path` 只留给新建下载对话框当 `dir` 的默认值，
/// 界面上不渲染完整绝对路径。`rpc_secret` 只在这条管理端点返回：页面要把它
/// 显示给用户去填 AriaNg（可显隐、可复制）。密钥存放在 `settings`（v4.9.36），
/// **唯一写入点**是下面的 `rotate_secret` —— 前端刷新，没有手填入口。
pub async fn engine(State(state): State<AppState>, _admin: AdminUser) -> Response {
    let manager = &state.downloads;
    let roots: Vec<Value> = manager
        .roots()
        .iter()
        .map(|root| {
            json!({
                "mount_id": root.mount_id,
                "mount_name": root.mount_name,
                "rel": root.rel,
                "label": root.label(),
                "path": root.path.display().to_string(),
                "online": root.online,
            })
        })
        .collect();
    Json(json!({
        "enabled": manager.enabled(),
        "bt": manager.bt_enabled(),
        "bt_port": manager.bt_listen_port(),
        "segments": manager.segments,
        "max_concurrent": manager.max_concurrent(),
        "roots": roots,
        // 追加到新任务 announce 列表的 Tracker（aria2 bt-tracker 语义，v4.9.39）
        "trackers": manager.trackers(),
        "rpc_path": "/jsonrpc",
        // 以下各项是启动时快照，改配置必须重启（页面据此打「重启生效」标记）。
        // `roots` 不在其中：它走 POST /api/downloads/roots 热替换（v4.9.37）。
        "restart_required": ["enable", "segments", "max_concurrent", "enable_bt",
                             "bt_listen_port_min", "bt_listen_port_max", "bt_disable_dht"],
        "rpc_secret": if manager.enabled() {
            manager.secret()
        } else {
            String::new()
        },
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RootsRequest {
    pub roots: Vec<crate::core::download::RootSpec>,
}

/// POST /api/downloads/roots：改「允许的保存目录」（v4.9.37）
///
/// 只接受「挂载点 id + 相对子路径」：下载目录必须落在文件管理的挂载点内，
/// 否则下完的文件在 asmoday 里看不见。写库 + 换内存，**立即生效**，不重启。
/// 目录不存在时自动创建（`downloads` 这类子路径不必先手动建）。
pub async fn set_roots(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<RootsRequest>,
) -> Response {
    if !state.downloads.enabled() {
        return disabled();
    }
    if body.roots.is_empty() {
        return error(StatusCode::BAD_REQUEST, "至少保留一个保存目录");
    }
    let mounts = super::files::mounts_for(&state).await;
    let (roots, warnings) = crate::core::download::resolve_roots(&mounts, &body.roots);
    if roots.is_empty() {
        let detail = if warnings.is_empty() {
            "没有可用的挂载点目录".to_string()
        } else {
            warnings.join("；")
        };
        return error(StatusCode::BAD_REQUEST, detail);
    }
    // 只给在线条目建目录：掉线挂载点保持原样，插回设备后自动可用
    for root in roots.iter().filter(|root| root.online) {
        if let Err(err) = tokio::fs::create_dir_all(&root.path).await {
            return error(
                StatusCode::BAD_REQUEST,
                format!("创建目录 {} 失败：{err}", root.path.display()),
            );
        }
    }
    let specs: Vec<_> = roots.iter().map(|root| root.spec()).collect();
    let encoded = match serde_json::to_string(&specs) {
        Ok(encoded) => encoded,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    // 先落库再换内存：写失败就整体失败，避免「这次生效、重启回旧值」
    if let Err(err) = state
        .auth
        .put_setting(crate::core::download::ROOTS_SETTING, &encoded)
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }
    state.downloads.replace_roots(roots.clone());
    crate::log_info!(
        "download roots updated ({} entries, effective immediately)",
        roots.len()
    );
    Json(json!({ "status": "ok", "roots": roots, "warnings": warnings })).into_response()
}

/// POST /api/downloads/secret：刷新 RPC 密钥（v4.9.36）
///
/// 立即生效、无需重启：先写 `settings`（重启后仍是新值），再换内存里的
/// 当前值。旧 `token:<secret>` 下一次请求就是 401，已配置的 AriaNg 需要重填。
/// 只影响 `/jsonrpc` 的外部客户端——页面的 `/api/downloads*` 走 Cookie 会话，
/// 不依赖这个密钥。旧 `<data>/download_rpc_secret` 文件不再读取也不再写入。
pub async fn rotate_secret(State(state): State<AppState>, _admin: AdminUser) -> Response {
    if !state.downloads.enabled() {
        return disabled();
    }
    let secret = crate::core::auth::random_password();
    // 先落库：写失败就整个失败，避免「内存换了但重启回到旧值」这种半状态
    if let Err(err) = state
        .auth
        .put_setting(crate::core::download::RPC_SECRET_SETTING, &secret)
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }
    state.downloads.replace_secret(secret.clone());
    crate::log_info!("download RPC secret rotated (old token invalidated)");
    Json(json!({ "status": "ok", "rpc_secret": secret })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TrackersRequest {
    /// 允许混写分隔符（换行 / 逗号 / 空格），前端是一行一个
    pub trackers: Vec<String>,
}

/// POST /api/downloads/trackers：改 BT Tracker 列表（v4.9.39）
///
/// 语义对齐 aria2 的 `bt-tracker`：**追加**到每个新任务的 announce 列表，
/// 不替换种子自带的那些。保存后立即对之后添加的种子生效（不需要重启）；
/// 已经在跑的任务不追溯——librqbit 在加种子时就把 tracker 轮询流建好了，
/// 没有运行时增删接口。这一块不写 `config.toml`：数据库才是唯一真相。
pub async fn set_trackers(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<TrackersRequest>,
) -> Response {
    if !state.downloads.enabled() {
        return disabled();
    }
    if !state.downloads.bt_enabled() {
        return error(
            StatusCode::BAD_REQUEST,
            "BitTorrent 未启用（config.toml 的 [download].enable_bt = false）",
        );
    }
    let update = crate::core::download::normalize_trackers(&body.trackers);
    let encoded = match serde_json::to_string(&update.trackers) {
        Ok(encoded) => encoded,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    // 先落库再换内存：写失败就整体失败，避免「这次生效、重启回旧值」
    if let Err(err) = state
        .auth
        .put_setting(crate::core::download::TRACKERS_SETTING, &encoded)
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }
    state.downloads.replace_trackers(update.trackers.clone());
    crate::log_info!(
        "download trackers updated ({} entries, applies to new torrents)",
        update.trackers.len()
    );
    Json(json!({
        "status": "ok",
        "trackers": update.trackers,
        "rejected": update.rejected,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct AddUri {
    pub uri: String,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub out: Option<String>,
}

/// POST /api/downloads：按 URI 添加（magnet / 40 位 info hash 自动走 BT）
pub async fn add(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<AddUri>,
) -> Response {
    if !state.downloads.enabled() {
        return disabled();
    }
    let uri = body.uri.trim().to_string();
    if uri.is_empty() {
        return error(StatusCode::BAD_REQUEST, "uri 不能为空");
    }
    // 其余字段保持 None：请求头 / referer / 分段覆盖只在 RPC 层开放，
    // 页面没有对应输入，凭空给默认值会把客户端的语义悄悄改掉。
    match state
        .downloads
        .add_uri(
            vec![uri],
            crate::core::download::UriOptions {
                dir: body.dir,
                out: body.out,
                ..Default::default()
            },
        )
        .await
    {
        Ok(gid) => Json(json!({ "gid": gid })).into_response(),
        Err(err) => rpc_error(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddTorrent {
    /// `.torrent` 文件的 base64（前端 FileReader 转，避免 multipart 解析）
    pub torrent: String,
    #[serde(default)]
    pub dir: Option<String>,
}

/// POST /api/downloads/torrents：按种子文件添加
pub async fn add_torrent(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<AddTorrent>,
) -> Response {
    if !state.downloads.enabled() {
        return disabled();
    }
    if !state.downloads.bt_enabled() {
        return error(
            StatusCode::BAD_REQUEST,
            "BitTorrent 未启用（config.toml 的 [download].enable_bt = false）",
        );
    }
    let encoded = body.torrent.trim();
    if encoded.is_empty() {
        return error(StatusCode::BAD_REQUEST, "torrent 不能为空");
    }
    let bytes = match data_encoding::BASE64.decode(encoded.as_bytes()) {
        Ok(bytes) => bytes,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("种子不是合法 base64：{err}"),
            );
        }
    };
    match state
        .downloads
        .add_torrent(TorrentSource::Bytes(bytes), body.dir, None, None, false)
        .await
    {
        Ok(gid) => Json(json!({ "gid": gid })).into_response(),
        Err(err) => rpc_error(err),
    }
}

/// POST /api/downloads/{gid}/pause
pub async fn pause(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(gid): Path<String>,
) -> Response {
    match state.downloads.pause(&gid).await {
        Ok(gid) => Json(json!({ "gid": gid })).into_response(),
        Err(err) => rpc_error(err),
    }
}

/// POST /api/downloads/{gid}/unpause
pub async fn unpause(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(gid): Path<String>,
) -> Response {
    match state.downloads.unpause(&gid).await {
        Ok(gid) => Json(json!({ "gid": gid })).into_response(),
        Err(err) => rpc_error(err),
    }
}

/// DELETE /api/downloads/{gid}?delete_files=1
///
/// 先按状态停任务/清记录（口径见 `DownloadManager::remove_task`），
/// `delete_files=1` 时再删任务写出来的文件；只删仍在保存目录白名单内的普通文件。
#[derive(Debug, Default, Deserialize)]
pub struct RemoveQuery {
    #[serde(default, deserialize_with = "crate::api::boolish")]
    pub delete_files: Option<bool>,
}

pub async fn remove(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(gid): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> Response {
    match state
        .downloads
        .remove_task(&gid, query.delete_files.unwrap_or(false))
        .await
    {
        Ok(outcome) => Json(json!({
            "gid": outcome.gid,
            "deletedFiles": outcome.deleted_files,
            "failedFiles": outcome.failed_files,
        }))
        .into_response(),
        Err(err) => rpc_error(err),
    }
}

/// DELETE /api/downloads/stopped：清空已停止（aria2 的 purgeDownloadResult）
pub async fn purge(State(state): State<AppState>, _admin: AdminUser) -> Response {
    let removed = state.downloads.purge().await;
    Json(json!({ "removed": removed.len() })).into_response()
}

#[cfg(test)]
mod tests {
    use super::decorate;

    /// BT：名字取种子名，类型标成 bt（列表据此画图标与标记）
    #[test]
    fn decorate_prefers_torrent_name() {
        let task = serde_json::json!({
            "gid": "a1",
            "status": "active",
            "bittorrent": { "info": { "name": "ubuntu-24.04.iso" } },
            "files": [{ "index": 1, "path": "D://dl//ubuntu-24.04.iso" }],
        });
        let out = decorate(task);
        assert_eq!(out["name"], "ubuntu-24.04.iso");
        assert_eq!(out["kind"], "bt");
    }

    /// HTTP：没有 bittorrent 键，名字取落盘路径的最后一段；
    /// Windows 路径可能带反斜杠，两种分隔符都要认
    #[test]
    fn decorate_uses_basename_for_http() {
        for path in ["/data/downloads/movie.mkv", "D://downloads//movie.mkv"] {
            let task = serde_json::json!({
                "gid": "h1",
                "status": "active",
                "files": [{ "index": 1, "path": path }],
            });
            let out = decorate(task);
            assert_eq!(out["name"], "movie.mkv", "path={path}");
            assert_eq!(out["kind"], "http");
        }
    }

    /// magnet 还没解析出元信息：名字留空（前端按 GID 兜底），不要编一个假名字
    #[test]
    fn decorate_leaves_name_empty_when_unknown() {
        let task = serde_json::json!({
            "gid": "b2",
            "status": "active",
            "bittorrent": { "info": { "name": "" } },
            "files": [{ "index": 1, "path": "" }],
        });
        let out = decorate(task);
        assert_eq!(out["name"], "");
        assert_eq!(out["kind"], "bt");
    }
}
