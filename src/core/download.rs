//! aria2 兼容下载引擎（v4.9.26，设计文档 3.10；BT v4.9.27，设计文档 3.11）。
//!
//! 目标：asmoday 自身成为下载服务端，`/jsonrpc` 暴露 aria2 兼容协议，
//! AriaNg / Aria2 Explorer / 任意 aria2 客户端填地址 + 密钥即用。
//!
//! 本模块管**引擎**（任务队列 + 调度 + 状态机），协议层在 `api::aria2_rpc`，
//! 两种传输后端：
//!
//! - HTTP(S) 多连接分段下载（`Range` 并发 + `.aria2` 控制文件续传）
//! - BitTorrent（magnet / .torrent / DHT / PEX，走 `core::bt` 的 librqbit 会话）
//!
//! 队列语义对齐 aria2：active / waiting(含 paused) / stopped 族。
//! 仍然不做 Metalink / FTP / SFTP —— `getVersion.enabledFeatures` 里不声明，
//! 客户端会自动隐藏对应入口，不会出现「假支持」。
//!
//! 落盘安全：所有任务目录必须落在 `download.roots` 白名单内（前缀校验，
//! Windows 大小写不敏感），RPC 传进来的 `dir` 不合法直接拒绝。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

use super::bt::{BtEngine, TorrentHandle, TorrentSource};

// ---------------------------------------------------------------------------
// 状态与数据结构
// ---------------------------------------------------------------------------

/// 传输类型。BT 的进度语义与 HTTP 完全不同（校验字节 vs 写入字节），
/// 所有进度/速度/文件清单的分支都看这个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Http,
    Torrent,
}

/// 任务状态（aria2 语义：waiting 含排队；paused 是显式暂停，出现在 waiting 清单里）

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Waiting,
    Paused,
    Error,
    Complete,
    Removed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Waiting => "waiting",
            Status::Paused => "paused",
            Status::Error => "error",
            Status::Complete => "complete",
            Status::Removed => "removed",
        }
    }
}

/// 任务创建时的选项（aria2 options 的子集，未知键在协议层吞掉）
#[derive(Debug, Clone, Default)]
pub struct TaskOptions {
    pub dir: PathBuf,
    pub out: Option<String>,
    pub user_agent: Option<String>,
    /// 请求头（"Name: value"），逐段请求都带
    pub headers: Vec<(String, String)>,
    /// changeOption 进来的其他键（getOption 原样回显，引擎不消费）
    pub extra: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentState {
    pub start: u64,
    /// 段尾（闭区间）；`u64::MAX` = 无长度/不支持 Range 的单流下载
    pub end: u64,
    /// 本段已写入字节数
    pub done: u64,
}

/// 单个任务的可变状态。协议层 `tellStatus` 的字段来源。
pub struct TaskState {
    pub gid: String,
    pub kind: TaskKind,
    pub uris: Vec<String>,
    pub status: Status,
    pub dir: PathBuf,
    pub out_name: String,
    pub total: u64,
    /// 各段进度；completed = sum(done)。BT 任务恒为空。
    pub segments: Vec<SegmentState>,
    pub connections: usize,
    pub error_code: u32,
    pub error_message: String,
    pub options: TaskOptions,
    /// 速度采样：上次快照（时刻 + 字节），查询时算增量
    speed_sample: (Instant, u64),
    pub speed: u64,
    pub etag: Option<String>,
    /// BT：种子句柄（换任务/暂停/删除都要用）。`None` = HTTP 任务。
    pub bt: Option<TorrentHandle>,
    /// BT：已校验的字节数（与 segments 互斥，只能从 stats 读）
    pub bt_completed: u64,
    pub upload_length: u64,
    pub upload_speed: u64,
    pub info_hash: String,
    pub piece_length: u64,
    pub num_pieces: u64,
    pub seeder: bool,
    /// BT 文件清单（相对路径 + 长度 + 已完成），`tellStatus.files` 的来源
    pub bt_files: Vec<BtFileEntry>,
    /// `tellStatus.bittorrent` 子对象（info.name / announceList / comment）
    pub bittorrent: Option<Value>,
}

/// BT 种子里单个文件的进度
#[derive(Debug, Clone)]
pub struct BtFileEntry {
    /// 相对种子根的文件路径
    pub relative: PathBuf,
    pub length: u64,
    pub completed: u64,
    pub selected: bool,
}

impl TaskState {
    pub fn completed(&self) -> u64 {
        match self.kind {
            TaskKind::Torrent => self.bt_completed,
            TaskKind::Http => self.segments.iter().map(|s| s.done).sum(),
        }
    }

    pub fn target_path(&self) -> PathBuf {
        self.dir.join(&self.out_name)
    }

    /// 控制文件路径（aria2 惯例：目标文件名 + ".aria2"）
    pub fn control_path(&self) -> PathBuf {
        let mut name = self.out_name.clone();
        name.push_str(".aria2");
        self.dir.join(name)
    }

    /// 速度刷新：距上次采样 ≥ 500ms 才重算，避免客户端高频轮询时数字抖成锯齿。
    /// BT 任务的数字全部来自 librqbit（它自己维护滑动窗口），这里只做搬运。
    fn refresh_speed(&mut self) {
        if let Some(handle) = self.bt.clone() {
            self.apply_bt_stats(&handle);
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.speed_sample.0);
        if dt < Duration::from_millis(500) {
            return;
        }
        let completed = self.completed();
        self.speed = if self.status == Status::Active {
            ((completed.saturating_sub(self.speed_sample.1)) as f64 / dt.as_secs_f64()) as u64
        } else {
            0
        };
        self.speed_sample = (now, completed);
    }

    /// 把 librqbit 的统计搬进任务状态。
    ///
    /// 注意 `stats().state` 不等于我们的任务状态：用户刚点暂停时 stats 可能还是
    /// `Live`，所以 `Paused` / `Removed` 由上层控制，这里只在 live/error/完成之间推断。
    fn apply_bt_stats(&mut self, handle: &TorrentHandle) {
        use librqbit::TorrentStatsState;

        let stats = handle.stats();
        self.total = stats.total_bytes.max(self.total);
        self.bt_completed = stats.progress_bytes;
        self.upload_length = stats.uploaded_bytes;
        self.seeder = stats.finished;
        if let Some(live) = &stats.live {
            self.speed = (live.download_speed.mbps * 1024.0 * 1024.0) as u64;
            self.upload_speed = (live.upload_speed.mbps * 1024.0 * 1024.0) as u64;
            self.connections = live.snapshot.peer_stats.live;
        }
        // 文件清单（metadata 解析完成前是空的，客户端会先显示 0/0）
        if let Some(metadata) = handle.metadata.load_full() {
            let selected = handle.only_files();
            if self.bt_files.len() != metadata.file_infos.len() {
                self.num_pieces = metadata.info.pieces.as_ref().len() as u64 / 20;
                self.piece_length = metadata.info.piece_length as u64;
                self.bt_files = metadata
                    .file_infos
                    .iter()
                    .enumerate()
                    .map(|(index, info)| BtFileEntry {
                        relative: info.relative_filename.clone(),
                        length: info.len,
                        completed: stats.file_progress.get(index).copied().unwrap_or(0),
                        selected: selected
                            .as_ref()
                            .map(|list| list.contains(&index))
                            .unwrap_or(true),
                    })
                    .collect();
                if let Some(name) = &metadata.name {
                    self.out_name = name.clone();
                }
            } else {
                for (entry, done) in self.bt_files.iter_mut().zip(&stats.file_progress) {
                    entry.completed = *done;
                }
            }
            let trackers: Vec<Value> = handle
                .shared()
                .trackers
                .iter()
                .map(|url| json!([url.as_str()]))
                .collect();
            self.bittorrent = Some(json!({
                "announceList": trackers,
                "comment": "",
                "creationDate": 0,
                "mode": if metadata.file_infos.len() > 1 { "multi" } else { "single" },
                "info": {"name": metadata.name.clone().unwrap_or_else(|| self.out_name.clone())},
            }));
        }
        match stats.state {
            TorrentStatsState::Error => {
                if self.status == Status::Active {
                    self.status = Status::Error;
                    self.error_code = 1;
                    self.error_message = stats
                        .error
                        .clone()
                        .unwrap_or_else(|| "bittorrent engine error".to_string());
                }
            }
            _ if stats.finished && self.status == Status::Active => {
                self.status = Status::Complete;
            }
            _ => {}
        }
    }
}

/// 任务句柄：状态 + 取消令牌（unpause 时整体换新）
pub struct TaskHandle {
    pub state: Mutex<TaskState>,
    cancel: std::sync::Mutex<CancellationToken>,
}

impl TaskHandle {
    fn current_token(&self) -> CancellationToken {
        self.cancel.lock().unwrap().clone()
    }
    fn cancel_current(&self) {
        self.cancel.lock().unwrap().cancel();
    }
    fn reset_token(&self) {
        *self.cancel.lock().unwrap() = CancellationToken::new();
    }
}

/// 任务事件（WS 通知的来源）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Pause,
    Stop,
    Complete,
    Error,
    /// BT 种子完成（aria2.onBtDownloadComplete）：与 Complete 一起发
    BtComplete,
}

impl Event {
    pub fn method(self) -> &'static str {
        match self {
            Event::Start => "aria2.onDownloadStart",
            Event::Pause => "aria2.onDownloadPause",
            Event::Stop => "aria2.onDownloadStop",
            Event::Complete => "aria2.onDownloadComplete",
            Event::Error => "aria2.onDownloadError",
            Event::BtComplete => "aria2.onBtDownloadComplete",
        }
    }
}

#[derive(Default)]
struct TaskMap {
    /// 插入顺序（tellWaiting / tellStopped 的 offset 语义依赖它）
    order: Vec<String>,
    tasks: HashMap<String, Arc<TaskHandle>>,
    /// 进入 stopped 族（error/complete/removed）的 GID，最新在前
    stopped: Vec<String>,
}

/// 实例设置里下载 RPC 密钥的键名（`app_settings.key`，v4.9.36）。
pub const RPC_SECRET_SETTING: &str = "download_rpc_secret";

/// 下载引擎。AppState 以 `Arc<DownloadManager>` 持有。
pub struct DownloadManager {
    http: reqwest::Client,
    /// RPC 密钥（`token:<secret>` 鉴权）。可在运行中替换：前端「刷新密钥」写库
    /// 之后直接换内存值，旧密钥下一个请求即失效，不需要重启（v4.9.36）。
    secret: Arc<RwLock<String>>,
    /// 默认落盘根（roots 未配置时）
    default_root: PathBuf,
    /// 允许的落盘根白名单（空 = 只允许 default_root）
    roots: Vec<PathBuf>,
    /// 每任务最大连接数
    pub segments: usize,
    /// `[download].enable=false` 时为 false：RPC handler 直接 404，无协议暴露面
    enabled: bool,
    /// 全局并发任务上限（aria2 max-concurrent-downloads）
    max_concurrent: Arc<Semaphore>,
    max_concurrent_count: usize,
    tasks: Mutex<TaskMap>,
    events: broadcast::Sender<(String, Event)>,
    /// BitTorrent 后端（`[download].enable_bt=false` 时为 None）
    bt: Option<Arc<BtEngine>>,
}

/// 协议层错误（JSON-RPC fault）
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn unknown_gid(gid: &str) -> RpcError {
    RpcError::new(2, format!("Cannot get download information: GID {gid}"))
}

/// 常量时间字节比较（`secret_matches` 用；长度不等直接驳回）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}

impl DownloadManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: reqwest::Client,
        secret: String,
        roots: Vec<PathBuf>,
        segments: usize,
        max_concurrent: usize,
        enabled: bool,
        bt: Option<Arc<BtEngine>>,
    ) -> Self {
        let default_root = roots.first().cloned().unwrap_or_default();
        let (events, _) = broadcast::channel(256);
        let max_concurrent_count = max_concurrent.clamp(1, 32);
        Self {
            http,
            secret: Arc::new(RwLock::new(secret)),
            default_root,
            roots,
            segments: segments.clamp(1, 32),
            enabled,
            max_concurrent: Arc::new(Semaphore::new(max_concurrent_count)),
            max_concurrent_count,
            tasks: Mutex::new(TaskMap::default()),
            events,
            bt,
        }
    }

    /// BT 后端是否可用（配置开关 + 会话是否起来）
    pub fn bt_enabled(&self) -> bool {
        self.enabled && self.bt.is_some()
    }

    /// 入站 peer 监听端口（未监听时为 None），启动日志用
    pub fn bt_listen_port(&self) -> Option<u16> {
        self.bt.as_ref().and_then(|bt| bt.listen_port())
    }

    /// 订阅任务事件（WS 通知通道）
    pub fn subscribe(&self) -> broadcast::Receiver<(String, Event)> {
        self.events.subscribe()
    }

    /// RPC 密钥校验（`token:<secret>` 全串比较）
    ///
    /// 常量时间比较：密钥是长期有效的共享凭据，RPC 每秒能打几千次，
    /// `==` 的逐字节提前返回理论上可被统计出前缀。长度不等直接驳回即可
    /// —— 固定 20 位随机串，长度本身不是秘密。
    pub fn secret_matches(&self, token: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(given) = token.strip_prefix("token:") else {
            return false;
        };
        let Ok(secret) = self.secret.read() else {
            return false;
        };
        constant_time_eq(given.as_bytes(), secret.as_bytes())
    }

    /// 引擎是否启用（disabled 时协议层直接 404）
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// RPC 密钥（`/api/downloads/engine` 回显 + 页面复制用）
    pub fn secret(&self) -> String {
        self.secret
            .read()
            .map(|secret| secret.clone())
            .unwrap_or_default()
    }

    /// 运行中替换 RPC 密钥（前端「刷新密钥」；调用方负责先写库）。
    ///
    /// 旧密钥立即失效：`/jsonrpc` 下一次请求就是 401，不需要重启进程。
    pub fn replace_secret(&self, next: String) {
        if let Ok(mut guard) = self.secret.write() {
            *guard = next;
        }
    }

    /// 允许落盘的根白名单（`dir` 只能落在这些目录内）
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 全局并发上限（创建时快照，改配置需重启）
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent_count
    }

    // -- 目录白名单 ---------------------------------------------------------

    /// 请求目录是否在白名单内；返回（尽量 canonical 的）路径。
    ///
    /// Windows 大小写不敏感（与挂载点校验同口径）；网络路径 canonicalize
    /// 可能失败（离线 SMB），失败时按字面量对比。
    fn resolve_dir(&self, requested: Option<&str>) -> Result<PathBuf, RpcError> {
        let raw = match requested {
            Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir.trim()),
            _ => self.default_root.clone(),
        };
        let canon = std::fs::canonicalize(&raw).unwrap_or_else(|_| normalize(&raw));
        let mut effective: &[PathBuf] = &self.roots;
        if effective.is_empty() {
            // 无配置：退回默认根（构造时保证非空；空 PathBuf 时一律拒绝）
            effective = std::slice::from_ref(&self.default_root);
        }
        for root in effective {
            if root.as_os_str().is_empty() {
                continue;
            }
            let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| normalize(root));
            if path_contains(&root_canon, &canon) {
                return Ok(canon);
            }
        }
        Err(RpcError::new(
            2,
            format!(
                "dir is not within allowed download roots: {}",
                canon.display()
            ),
        ))
    }

    // -- 任务生命周期 -------------------------------------------------------

    /// addUri：创建任务并排队。返回 GID。
    ///
    /// `magnet:` 开头（或 40 位 info hash 裸串）走 BitTorrent 分支，
    /// 与 aria2 的 addUri 行为一致 —— 客户端不需要区分调用哪个方法。
    pub async fn add_uri(
        self: &Arc<Self>,
        uris: Vec<String>,
        dir: Option<String>,
        out: Option<String>,
        user_agent: Option<String>,
        headers: Vec<(String, String)>,
        segments_override: Option<usize>,
    ) -> Result<String, RpcError> {
        let uri = uris
            .first()
            .cloned()
            .ok_or_else(|| RpcError::new(2, "uris is required"))?;
        if is_torrent_uri(&uri) {
            return self
                .add_torrent(TorrentSource::Magnet(uri), dir, out, None, false)
                .await;
        }
        if !(uri.starts_with("http://") || uri.starts_with("https://")) {
            return Err(RpcError::new(2, "uris must contain an http(s) URL"));
        }
        let dir = self.resolve_dir(dir.as_deref())?;

        let filename = match out {
            Some(name) if is_safe_filename(&name) => name,
            Some(name) => {
                return Err(RpcError::new(2, format!("invalid out filename: {name}")));
            }
            None => filename_from_url(&uri),
        };

        let options = TaskOptions {
            dir,
            out: Some(filename.clone()),
            user_agent,
            headers,
            extra: Vec::new(),
        };
        let segments = segments_override
            .unwrap_or(self.segments)
            .clamp(1, self.segments.max(1));

        let gid = new_gid();
        let handle = Arc::new(TaskHandle {
            state: Mutex::new(TaskState {
                gid: gid.clone(),
                kind: TaskKind::Http,
                uris: uris.clone(),
                status: Status::Waiting,
                dir: options.dir.clone(),
                out_name: filename,
                total: 0,
                segments: Vec::new(),
                connections: 0,
                error_code: 0,
                error_message: String::new(),
                options,
                speed_sample: (Instant::now(), 0),
                speed: 0,
                etag: None,
                bt: None,
                bt_completed: 0,
                upload_length: 0,
                upload_speed: 0,
                info_hash: String::new(),
                piece_length: 0,
                num_pieces: 0,
                seeder: false,
                bt_files: Vec::new(),
                bittorrent: None,
            }),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
        });

        {
            let mut map = self.tasks.lock().await;
            map.order.push(gid.clone());
            map.tasks.insert(gid.clone(), handle.clone());
        }

        let engine = EngineHandle {
            http: self.http.clone(),
            max_concurrent: self.max_concurrent.clone(),
            events: self.events.clone(),
            segments,
        };
        let manager = self.clone();
        tokio::spawn(run_task(engine, manager, handle));
        Ok(gid)
    }

    /// addTorrent / addUri(magnet)：创建 BT 任务并排队。返回 GID。
    ///
    /// `only_files` 是 aria2 的 `select-file` 语义（1 基索引），这里已经换成 0 基。
    pub async fn add_torrent(
        self: &Arc<Self>,
        source: TorrentSource,
        dir: Option<String>,
        out: Option<String>,
        only_files: Option<Vec<usize>>,
        paused: bool,
    ) -> Result<String, RpcError> {
        let bt = self
            .bt
            .clone()
            .ok_or_else(|| RpcError::new(2, "bittorrent is disabled on this server"))?;
        let dir = self.resolve_dir(dir.as_deref())?;
        // aria2 会替用户建目录；librqbit 的 output_folder 要求已存在
        std::fs::create_dir_all(&dir).map_err(|err| {
            RpcError::new(2, format!("cannot create dir {}: {err}", dir.display()))
        })?;
        if let Some(name) = &out
            && !is_safe_filename(name)
        {
            return Err(RpcError::new(2, format!("invalid out filename: {name}")));
        }
        // 加会话前先拿到 info hash / 种子名：非法字节在这里就拒绝，
        // magnet 也能立刻有 infoHash（不必等元信息解析）
        let preview = source.preview().map_err(|err| RpcError::new(2, err))?;

        let uris = match &source {
            TorrentSource::Magnet(uri) => vec![uri.clone()],
            TorrentSource::Bytes(_) => Vec::new(),
        };
        let out_name = out
            .clone()
            .or(preview.name)
            .unwrap_or_else(|| format!("torrent-{}", new_gid()));
        let options = TaskOptions {
            dir: dir.clone(),
            out: out.clone(),
            user_agent: None,
            headers: Vec::new(),
            extra: Vec::new(),
        };

        let gid = new_gid();
        let handle = Arc::new(TaskHandle {
            state: Mutex::new(TaskState {
                gid: gid.clone(),
                kind: TaskKind::Torrent,
                uris,
                // 元信息还没到也算 active：aria2 对 magnet 同样立刻进 active
                status: if paused {
                    Status::Paused
                } else {
                    Status::Active
                },
                dir,
                out_name,
                total: 0,
                segments: Vec::new(),
                connections: 0,
                error_code: 0,
                error_message: String::new(),
                options,
                speed_sample: (Instant::now(), 0),
                speed: 0,
                etag: None,
                bt: None,
                bt_completed: 0,
                upload_length: 0,
                upload_speed: 0,
                info_hash: preview.info_hash,
                piece_length: 0,
                num_pieces: 0,
                seeder: false,
                bt_files: Vec::new(),
                bittorrent: None,
            }),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
        });

        {
            let mut map = self.tasks.lock().await;
            map.order.push(gid.clone());
            map.tasks.insert(gid.clone(), handle.clone());
        }
        if paused {
            self.mark_stopped(&gid).await;
        }

        let manager = self.clone();
        let events = self.events.clone();
        let permit = self.max_concurrent.clone();
        tokio::spawn(run_bt_task(
            manager, bt, events, permit, handle, source, only_files, paused,
        ));
        Ok(gid)
    }

    async fn get(&self, gid: &str) -> Option<Arc<TaskHandle>> {
        self.tasks.lock().await.tasks.get(gid).cloned()
    }

    /// 暂停：active → 落控制文件；waiting → 直接翻状态
    pub async fn pause(&self, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        let (is_bt, bt_torrent) = {
            let mut state = handle.state.lock().await;
            if !matches!(state.status, Status::Active | Status::Waiting) {
                return Err(RpcError::new(
                    2,
                    format!(
                        "cannot pause a download in status {}",
                        state.status.as_str()
                    ),
                ));
            }
            state.status = Status::Paused;
            state.connections = 0;
            state.refresh_speed();
            state.speed = 0;
            state.upload_speed = 0;
            (state.kind == TaskKind::Torrent, state.bt.clone())
        };
        match (is_bt, bt_torrent) {
            // BT：交给 librqbit 暂停（它会停掉 peer 连接但保留校验状态），
            // 监控协程不退出 —— 用户随时可能 unpause。
            (true, Some(torrent)) => {
                if let Some(bt) = &self.bt {
                    let _ = bt.pause(&torrent).await;
                }
            }
            // 还在解析元信息：状态已落成 paused，协程拿到句柄后会补一刀
            (true, None) => {}
            (false, _) => handle.cancel_current(),
        }
        self.events.send((gid.to_string(), Event::Pause)).ok();
        // 不进 stopped 清单（v4.9.31）：aria2 里 paused 属于 waiting 族，
        // `tellStopped` 只收 complete / error / removed。旧实现顺手 mark_stopped，
        // 结果同一个任务既在 tellWaiting 又在 tellStopped —— AriaNg 的
        // 「已停止」页会冒出一条还在队列里的任务。
        Ok(gid.to_string())
    }

    /// 恢复：paused → 重新排队执行（换新取消令牌）
    pub async fn unpause(self: &Arc<Self>, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        let (segments, is_bt, bt_torrent) = {
            let mut state = handle.state.lock().await;
            if state.status != Status::Paused {
                return Err(RpcError::new(
                    2,
                    format!(
                        "cannot unpause a download in status {}",
                        state.status.as_str()
                    ),
                ));
            }
            state.status = Status::Waiting;
            state.error_code = 0;
            state.error_message.clear();
            (
                self.segments,
                state.kind == TaskKind::Torrent,
                state.bt.clone(),
            )
        };
        if is_bt {
            if let Some(torrent) = bt_torrent
                && let Some(bt) = &self.bt
            {
                bt.unpause(&torrent)
                    .await
                    .map_err(|err| RpcError::new(2, format!("cannot resume torrent: {err}")))?;
            }
            // 句柄还没就绪（元信息解析中）也直接回 active：协程看到非 paused
            // 就不会补暂停，元信息到了自然开跑。
            handle.state.lock().await.status = Status::Active;
            return Ok(gid.to_string());
        }
        handle.reset_token();
        let engine = EngineHandle {
            http: self.http.clone(),
            max_concurrent: self.max_concurrent.clone(),
            events: self.events.clone(),
            segments,
        };
        let manager = self.clone();
        tokio::spawn(run_task(engine, manager, handle));
        Ok(gid.to_string())
    }

    /// forceRemove：取消执行并移除（不删已下载文件，对齐 aria2）
    pub async fn remove(&self, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        let (is_bt, was_running, bt_torrent) = {
            let mut state = handle.state.lock().await;
            let running = matches!(state.status, Status::Active | Status::Waiting);
            state.status = Status::Removed;
            state.speed = 0;
            state.upload_speed = 0;
            state.connections = 0;
            (state.kind == TaskKind::Torrent, running, state.bt.clone())
        };
        if is_bt {
            // 句柄可能还没就绪（元信息解析中）：协程返回时会看到 Removed 自行清理
            if let Some(torrent) = bt_torrent
                && let Some(bt) = &self.bt
            {
                bt.remove(&torrent).await;
            }
        } else if was_running {
            handle.cancel_current();
        }
        self.events.send((gid.to_string(), Event::Stop)).ok();
        self.mark_stopped(gid).await;
        Ok(gid.to_string())
    }

    /// 清空全部已停止任务。返回删除的 GID 列表。
    pub async fn purge(&self) -> Vec<String> {
        let (purged, handles) = {
            let mut map = self.tasks.lock().await;
            let purged = map.stopped.clone();
            let mut handles = Vec::new();
            for gid in &purged {
                if let Some(handle) = map.tasks.remove(gid) {
                    handles.push(handle);
                }
            }
            let live: Vec<String> = map.tasks.keys().cloned().collect();
            map.order.retain(|g| live.contains(g));
            map.stopped.clear();
            (purged, handles)
        };
        // 锁外释放 BT 会话：已完成的任务仍在做种，不清掉会一直传数据
        for handle in handles {
            let bt_handle = handle.state.lock().await.bt.clone();
            if let (Some(torrent), Some(engine)) = (bt_handle, &self.bt) {
                engine.remove(&torrent).await;
            }
        }
        purged
    }

    /// removeDownloadResult：只许删已停止的任务
    pub async fn remove_result(&self, gid: &str) -> Result<String, RpcError> {
        let handle = {
            let mut map = self.tasks.lock().await;
            if let Some(handle) = map.tasks.get(gid) {
                let status = handle.state.lock().await.status;
                if matches!(status, Status::Active | Status::Waiting | Status::Paused) {
                    return Err(RpcError::new(
                        2,
                        format!(
                            "GID {gid} cannot be removed now (status {})",
                            status.as_str()
                        ),
                    ));
                }
            }
            let handle = map.tasks.remove(gid).ok_or_else(|| unknown_gid(gid))?;
            map.order.retain(|g| g != gid);
            map.stopped.retain(|g| g != gid);
            handle
        };
        let bt_handle = handle.state.lock().await.bt.clone();
        if let (Some(torrent), Some(engine)) = (bt_handle, &self.bt) {
            engine.remove(&torrent).await;
        }
        Ok(gid.to_string())
    }

    async fn mark_stopped(&self, gid: &str) {
        let mut map = self.tasks.lock().await;
        map.stopped.retain(|g| g != gid);
        map.stopped.insert(0, gid.to_string());
        // 防止失控增长：截掉 1000 名开外的历史
        map.stopped.truncate(1000);
    }

    // -- 查询 ---------------------------------------------------------------

    /// 任务快照（可按 keys 过滤字段，aria2 tellStatus 语义）
    pub async fn status_with_keys(
        &self,
        gid: &str,
        keys: Option<&[String]>,
    ) -> Option<serde_json::Value> {
        let handle = self.get(gid).await?;
        let mut state = handle.state.lock().await;
        state.refresh_speed();
        Some(task_json(&state, keys))
    }

    pub async fn tell_active(&self) -> Vec<serde_json::Value> {
        self.list_by(|status| status == Status::Active).await
    }

    pub async fn tell_waiting(&self) -> Vec<serde_json::Value> {
        self.list_by(|status| matches!(status, Status::Waiting | Status::Paused))
            .await
    }

    pub async fn tell_stopped(&self) -> Vec<serde_json::Value> {
        let map = self.tasks.lock().await;
        let mut out = Vec::new();
        for gid in &map.stopped {
            if let Some(handle) = map.tasks.get(gid) {
                let mut state = handle.state.lock().await;
                state.refresh_speed();
                out.push(task_json(&state, None));
            }
        }
        out
    }

    /// tellWaiting / tellStopped 的 offset+num 窗口
    pub async fn window(
        &self,
        list: WindowList,
        offset: usize,
        num: usize,
    ) -> Vec<serde_json::Value> {
        let mut full = match list {
            WindowList::Waiting => self.tell_waiting().await,
            WindowList::Stopped => self.tell_stopped().await,
        };
        if offset >= full.len() {
            return Vec::new();
        }
        full.drain(..offset);
        if num == 0 {
            return full; // aria2 语义：num=0 → 全部
        }
        full.truncate(num);
        full
    }

    async fn list_by(&self, pred: impl Fn(Status) -> bool) -> Vec<serde_json::Value> {
        let map = self.tasks.lock().await;
        let mut out = Vec::new();
        for gid in &map.order {
            let Some(handle) = map.tasks.get(gid) else {
                continue;
            };
            let mut state = handle.state.lock().await;
            if pred(state.status) {
                state.refresh_speed();
                out.push(task_json(&state, None));
            }
        }
        out
    }

    /// 全局统计（getGlobalStat）。
    ///
    /// 单次遍历：旧实现为了拿三个计数，把 active / waiting / stopped 三份清单
    /// 各序列化一遍，而 AriaNg 这类客户端每秒轮询一次。上传速度按**所有**任务
    /// 求和——BT 下载完成后仍在做种，那时状态是 stopped，只统计 active 会让
    /// 「做种上传」永远显示 0（v4.9.31）。
    pub async fn global_stat(&self) -> serde_json::Value {
        let map = self.tasks.lock().await;
        let (mut active, mut waiting, mut stopped) = (0usize, 0usize, 0usize);
        let (mut down, mut up) = (0u64, 0u64);
        for gid in &map.order {
            let Some(handle) = map.tasks.get(gid) else {
                continue;
            };
            let mut state = handle.state.lock().await;
            match state.status {
                Status::Active => {
                    state.refresh_speed();
                    active += 1;
                    down += state.speed;
                    up += state.upload_speed;
                }
                Status::Waiting | Status::Paused => waiting += 1,
                Status::Complete | Status::Error | Status::Removed => {
                    // 做种的 BT 任务在这里：速度只有 stats 会给
                    state.refresh_speed();
                    stopped += 1;
                    up += state.upload_speed;
                }
            }
        }
        serde_json::json!({
            "downloadSpeed": down,
            "uploadSpeed": up,
            "numActive": active,
            "numWaiting": waiting,
            "numStopped": stopped,
            "numStoppedTotal": stopped,
        })
    }

    /// 任务选项回显（aria2.getOption）
    pub async fn option_of(&self, gid: &str) -> Option<serde_json::Value> {
        let handle = self.get(gid).await?;
        let state = handle.state.lock().await;
        let mut options = serde_json::Map::new();
        options.insert("dir".into(), display_path(&state.dir).into());
        if let Some(out) = &state.options.out {
            options.insert("out".into(), out.clone().into());
        }
        if let Some(ua) = &state.options.user_agent {
            options.insert("user-agent".into(), ua.clone().into());
        }
        for (key, value) in &state.options.extra {
            options.insert(key.clone(), value.clone().into());
        }
        Some(Value::Object(options))
    }

    /// 修改选项（aria2.changeOption）。
    ///
    /// `dir` 必须仍过白名单；`dir` / `out` 只对未开跑（waiting/paused）的任务生效。
    /// 其余键原样存进 extra（getOption 回显），引擎不消费。
    pub async fn change_option(&self, gid: &str, options: &serde_json::Value) -> Option<()> {
        let handle = self.get(gid).await?;
        let mut state = handle.state.lock().await;
        if let Some(dir) = options.get("dir").and_then(Value::as_str) {
            let resolved = self.resolve_dir(Some(dir)).ok()?;
            if state.status != Status::Active {
                state.dir = resolved;
            }
        }
        if let Some(out) = options.get("out").and_then(Value::as_str)
            && is_safe_filename(out)
            && state.status != Status::Active
        {
            state.out_name = out.to_string();
        }
        if let Some(ua) = options.get("user-agent").and_then(Value::as_str) {
            state.options.user_agent = Some(ua.to_string());
        }
        if let Some(map) = options.as_object() {
            for (key, value) in map {
                if matches!(key.as_str(), "dir" | "out" | "user-agent" | "header") {
                    continue;
                }
                if let Some(text) = value.as_str() {
                    state.options.extra.retain(|(existing, _)| existing != key);
                    state.options.extra.push((key.clone(), text.to_string()));
                }
            }
        }
        Some(())
    }

    /// 全局选项回显（aria2.getGlobalOption）：只声明真实生效的键
    pub fn global_options(&self) -> serde_json::Value {
        let mut options = json!({
            "dir": self.default_root.to_string_lossy(),
            "max-concurrent-downloads": format!("{}", self.max_concurrent_count),
            "split": format!("{}", self.segments),
            "user-agent": "aria2/1.37.0",
        });
        if self.bt.is_some()
            && let Some(map) = options.as_object_mut()
        {
            map.insert("enable-dht".into(), json!("true"));
            map.insert("bt-max-peers".into(), json!("55"));
        }
        options
    }
}

/// tellWaiting / tellStopped 的选择
#[derive(Debug, Clone, Copy)]
pub enum WindowList {
    Waiting,
    Stopped,
}

/// 驱动协程的窄依赖（避免 DownloadManager 自环）
#[derive(Clone)]
struct EngineHandle {
    http: reqwest::Client,
    max_concurrent: Arc<Semaphore>,
    events: broadcast::Sender<(String, Event)>,
    segments: usize,
}

enum FailKind {
    Cancelled,
    Failed(u32, String),
}

type TaskOutcome = Result<(), FailKind>;

async fn run_task(engine: EngineHandle, manager: Arc<DownloadManager>, handle: Arc<TaskHandle>) {
    // 排队：信号量限制全局并发
    let _permit = match engine.max_concurrent.acquire().await {
        Ok(permit) => permit,
        Err(_) => return,
    };
    // 排队期间被暂停/删除：直接退出（状态已由 pause()/remove() 改好）
    if handle.current_token().is_cancelled() {
        return;
    }

    let gid = handle.state.lock().await.gid.clone();
    let outcome = download_task(&engine, &handle).await;

    match outcome {
        Ok(()) => {
            let mut state = handle.state.lock().await;
            if state.status == Status::Active {
                state.status = Status::Complete;
                state.speed = 0;
                state.connections = 0;
                let control = state.control_path();
                drop(state);
                // 成功即清控制文件
                let _ = tokio::fs::remove_file(control).await;
                manager.mark_stopped(&gid).await;
                engine.events.send((gid, Event::Complete)).ok();
            }
        }
        Err(FailKind::Cancelled) => {
            // 暂停：状态已被 pause() 置好；落控制文件供下次续传
            let state = handle.state.lock().await;
            let control = serde_json::json!({
                "gid": state.gid,
                "url": state.uris.first().cloned().unwrap_or_default(),
                "total": state.total,
                "etag": state.etag,
                "segments": state.segments,
            });
            let path = state.control_path();
            drop(state);
            let _ = tokio::fs::write(path, control.to_string()).await;
        }
        Err(FailKind::Failed(code, msg)) => {
            let mut state = handle.state.lock().await;
            // Waiting：探测阶段（转 Active 之前）就失败，同样要落定成 error，
            // 否则任务永远悬在 waiting。Paused（用户刚暂停）不覆盖。
            if matches!(state.status, Status::Active | Status::Waiting) {
                state.status = Status::Error;
                state.error_code = code;
                state.error_message = msg;
                state.speed = 0;
                state.connections = 0;
                drop(state);
                manager.mark_stopped(&gid).await;
                engine.events.send((gid, Event::Error)).ok();
            }
        }
    }
}

/// BT 任务协程：加入会话 → 周期性同步 → 落定完成/失败。
///
/// 与 HTTP 任务的区别：没有「分段 worker 全部返回」这种天然终点，完成与否
/// 只有 librqbit 知道。所以这里是一个轻量监控循环（800ms），把最终状态
/// 写回任务表并发通知；暂停不退出（用户随时可能恢复）。
#[allow(clippy::too_many_arguments)]
async fn run_bt_task(
    manager: Arc<DownloadManager>,
    bt: Arc<BtEngine>,
    events: broadcast::Sender<(String, Event)>,
    permit: Arc<Semaphore>,
    handle: Arc<TaskHandle>,
    source: TorrentSource,
    only_files: Option<Vec<usize>>,
    paused: bool,
) {
    // 与 HTTP 任务共用 max-concurrent-downloads 配额
    let _permit = match permit.acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return,
    };

    let gid = handle.state.lock().await.gid.clone();
    let dir = handle.state.lock().await.dir.clone();

    let torrent = match bt.add(source, &dir, only_files, paused).await {
        Ok(torrent) => torrent,
        Err(err) => {
            let mut state = handle.state.lock().await;
            // 用户可能在 add 期间就删了任务，别把 Removed 覆盖成 Error
            if matches!(
                state.status,
                Status::Waiting | Status::Active | Status::Paused
            ) {
                state.status = Status::Error;
                state.error_code = 1;
                state.error_message = err;
                state.speed = 0;
                state.connections = 0;
                drop(state);
                manager.mark_stopped(&gid).await;
                events.send((gid, Event::Error)).ok();
            }
            return;
        }
    };

    let info_hash = torrent.info_hash().as_string();
    {
        let mut state = handle.state.lock().await;
        if state.status == Status::Removed {
            drop(state);
            bt.remove(&torrent).await;
            return;
        }
        // 加会话期间用户可能已点暂停：先挂上句柄，再按当前状态补一刀
        let should_pause = state.status == Status::Paused;
        state.bt = Some(torrent.clone());
        state.info_hash = info_hash;
        // 第一次同步：把种子名/文件清单/总长填上，客户端立刻能看到结构
        state.refresh_speed();
        drop(state);
        if should_pause {
            let _ = bt.pause(&torrent).await;
        }
    }
    events.send((gid.clone(), Event::Start)).ok();

    loop {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let (status, _) = {
            let mut state = handle.state.lock().await;
            state.refresh_speed();
            (state.status, state.error_code)
        };
        match status {
            Status::Removed => return,
            Status::Complete => {
                manager.mark_stopped(&gid).await;
                // aria2 在 BT 完成时两条通知都发：onBtDownloadComplete 给种子，
                // onDownloadComplete 给通用列表（AriaNg 靠后者刷新行状态）。
                events.send((gid.clone(), Event::BtComplete)).ok();
                events.send((gid, Event::Complete)).ok();
                return;
            }
            Status::Error => {
                manager.mark_stopped(&gid).await;
                events.send((gid, Event::Error)).ok();
                return;
            }
            _ => {}
        }
    }
}

async fn download_task(engine: &EngineHandle, handle: &Arc<TaskHandle>) -> TaskOutcome {
    let cancel = handle.current_token();
    let (uri, ua, headers) = {
        let state = handle.state.lock().await;
        (
            state.uris.first().cloned().unwrap_or_default(),
            state.options.user_agent.clone(),
            state.options.headers.clone(),
        )
    };

    // 1. 探测：长度 / 续传能力 / ETag
    let mut request = engine.http.head(&uri);
    if let Some(ua) = &ua {
        request = request.header(reqwest::header::USER_AGENT, ua);
    }
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let (total, accept_ranges, etag) = match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            let total = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let accept = resp
                .headers()
                .get(reqwest::header::ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("bytes"));
            let etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            (total, accept, etag)
        }
        Ok(resp) => {
            return Err(FailKind::Failed(
                resp.status().as_u16() as u32,
                format!("HTTP {}", resp.status()),
            ));
        }
        Err(err) => return Err(FailKind::Failed(1, err.to_string())),
    };

    {
        let mut state = handle.state.lock().await;
        state.total = total.unwrap_or(0);
        state.etag = etag.clone();
    }

    // 2. 分段方案：控制文件续传（总长 + ETag 都对得上且部分文件还在）→ 沿用；
    //    否则按 Range 支持与否决定分段或单流。
    let target = handle.state.lock().await.target_path();
    let file_ready = tokio::fs::metadata(&target)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    let segments: Vec<SegmentState> = {
        let saved = if file_ready {
            load_control(&handle.state.lock().await.control_path()).await
        } else {
            None
        };
        match (total, accept_ranges, saved) {
            (Some(t), true, Some(saved))
                if saved.total == t
                    && saved.etag == etag
                    && !saved.segments.is_empty()
                    && segments_fit(&saved.segments, t) =>
            {
                saved.segments
            }
            (Some(t), true, _) => split_segments(t, engine.segments),
            // 无长度或不支持 Range：单流（不发 Range 头），长度靠收尾校验
            _ => vec![SegmentState {
                start: 0,
                end: u64::MAX,
                done: 0,
            }],
        }
    };

    {
        let mut state = handle.state.lock().await;
        state.segments = segments.clone();
        state.status = Status::Active;
        state.connections = segments.len();
    }
    engine
        .events
        .send((handle.state.lock().await.gid.clone(), Event::Start))
        .ok();

    // 3. 分段并发
    let mut workers = Vec::with_capacity(segments.len());
    for seg in segments {
        workers.push(tokio::spawn(download_segment(
            engine.clone(),
            handle.clone(),
            cancel.clone(),
            seg,
        )));
    }
    let mut first_error: Option<FailKind> = None;
    for worker in workers {
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(kind)) => {
                // 一个段失败：取消其余段，只保留第一个错误
                if first_error.is_none() {
                    first_error = Some(kind);
                }
                cancel.cancel();
            }
            Err(join_err) => {
                if first_error.is_none() {
                    first_error = Some(FailKind::Failed(1, join_err.to_string()));
                }
                cancel.cancel();
            }
        }
    }
    if let Some(kind) = first_error {
        return Err(kind);
    }

    // 4. 收尾校验
    let (completed, total) = {
        let state = handle.state.lock().await;
        (state.completed(), state.total)
    };
    if total > 0 && completed != total {
        return Err(FailKind::Failed(
            13,
            format!("length mismatch: got {completed}, want {total}"),
        ));
    }
    Ok(())
}

/// 控制文件里的段是否仍与总长吻合（防「换文件后续传」的脏数据）
fn segments_fit(segments: &[SegmentState], total: u64) -> bool {
    segments
        .iter()
        .all(|s| s.start < total && (s.end == u64::MAX || s.end < total))
}

/// 单段下载：Range 拉流写文件，带有限重试
async fn download_segment(
    engine: EngineHandle,
    handle: Arc<TaskHandle>,
    cancel: CancellationToken,
    mut seg: SegmentState,
) -> Result<(), FailKind> {
    let (uri, ua, headers, path) = {
        let state = handle.state.lock().await;
        (
            state.uris.first().cloned().unwrap_or_default(),
            state.options.user_agent.clone(),
            state.options.headers.clone(),
            state.target_path(),
        )
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .await
        .map_err(|err| FailKind::Failed(1, format!("open {}: {err}", path.display())))?;

    let mut retries = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(FailKind::Cancelled);
        }
        let ranged = seg.end != u64::MAX;
        let mut request = engine.http.get(&uri);
        if let Some(ua) = &ua {
            request = request.header(reqwest::header::USER_AGENT, ua);
        }
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        if ranged {
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", seg.start + seg.done, seg.end),
            );
        }

        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                // 服务器无视 Range 返回 200：多段任务会被写花，直接失败
                if ranged && seg.start + seg.done > 0 && resp.status() == reqwest::StatusCode::OK {
                    return Err(FailKind::Failed(
                        1,
                        "server ignored Range request".to_string(),
                    ));
                }
                let mut stream = resp.bytes_stream();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(FailKind::Cancelled),
                        chunk = stream.next() => {
                            let Some(chunk) = chunk else { break };
                            let chunk = match chunk {
                                Ok(bytes) => bytes,
                                Err(err) => {
                                    retries += 1;
                                    if retries > 3 {
                                        return Err(FailKind::Failed(1, err.to_string()));
                                    }
                                    tokio::time::sleep(Duration::from_secs(retries as u64)).await;
                                    break; // 重建请求，从 done 处续传
                                }
                            };
                            let offset = seg.start + seg.done;
                            file.seek(std::io::SeekFrom::Start(offset))
                                .await
                                .map_err(|err| FailKind::Failed(1, err.to_string()))?;
                            file.write_all(&chunk)
                                .await
                                .map_err(|err| FailKind::Failed(1, err.to_string()))?;
                            seg.done += chunk.len() as u64;
                            let mut state = handle.state.lock().await;
                            if let Some(recorded) =
                                state.segments.iter_mut().find(|s| s.start == seg.start)
                            {
                                recorded.done = seg.done;
                            }
                        }
                    }
                }
                // 流结束：本段写满即完成
                if !ranged || seg.start + seg.done > seg.end {
                    return Ok(());
                }
                // 服务器提前断流：可重试，从断点继续
                retries += 1;
                if retries > 3 {
                    return Err(FailKind::Failed(
                        1,
                        format!("segment ended early at {}", seg.start + seg.done),
                    ));
                }
            }
            Ok(resp) => {
                // 416：续传场景下段已完整，视为成功
                if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                    return Ok(());
                }
                retries += 1;
                if retries > 3 {
                    return Err(FailKind::Failed(
                        resp.status().as_u16() as u32,
                        format!("HTTP {}", resp.status()),
                    ));
                }
                tokio::time::sleep(Duration::from_secs(retries as u64)).await;
            }
            Err(err) => {
                retries += 1;
                if retries > 3 {
                    return Err(FailKind::Failed(1, err.to_string()));
                }
                tokio::time::sleep(Duration::from_secs(retries as u64)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 控制文件（断点续传）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedControl {
    #[allow(dead_code)]
    gid: String,
    #[allow(dead_code)]
    url: String,
    total: u64,
    etag: Option<String>,
    segments: Vec<SegmentState>,
}

async fn load_control(path: &Path) -> Option<SavedControl> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn new_gid() -> String {
    // aria2 的 GID 是 16 个十六进制字符
    format!("{:016x}", rand::random::<u64>())
}

fn normalize(path: &Path) -> PathBuf {
    PathBuf::from(crate::core::fs::normalize_display(path))
}

/// 展示用的路径文本：剥掉 Windows canonicalize 的 `\\?\` / `\\?\UNC\` 前缀
/// （verbatim 路径禁用 `/`，混着用会让客户端 open 失败），保留大小写。
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\")
        .map(str::to_string)
        .unwrap_or_else(|| text.into_owned())
}

/// 可比较的路径文本：剥掉 Windows canonicalize 的 `\\?\` / `\\?\UNC\` 前缀，
/// 反斜杠归一成正斜杠，Windows 上再统一小写。
fn display_comparable(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        text = format!("/{rest}");
    } else if let Some(rest) = text.strip_prefix("//?/") {
        text = rest.to_string();
    }
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// root 是否包含 path（应传 canonical 路径；Windows 大小写不敏感）
fn path_contains(root: &Path, path: &Path) -> bool {
    let root_text = display_comparable(root);
    let path_text = display_comparable(path);
    let root_text = root_text.trim_end_matches('/');
    if root_text.is_empty() {
        return false;
    }
    path_text == root_text || path_text.starts_with(&format!("{root_text}/"))
}

/// out 文件名白名单：禁路径分隔与 `..`
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.len() < 255
}

/// magnet: 链接，或 40 位十六进制 info hash（aria2 也接受裸 hash）
fn is_torrent_uri(uri: &str) -> bool {
    uri.starts_with("magnet:")
        || (uri.len() == 40 && uri.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// URL → 文件名（去 query/fragment，简单 percent-decode）
fn filename_from_url(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let name = path.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(name);
    if decoded.is_empty() || decoded == "/" {
        "index.html".to_string()
    } else {
        decoded
    }
}

#[allow(dead_code)]
fn filename_from_disposition(value: &str) -> Option<String> {
    // attachment; filename="x.zip" / filename*=UTF-8''x.zip
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            let after = rest.split("''").nth(1).unwrap_or(rest);
            let name = percent_decode(after.trim_matches('"'));
            if !name.is_empty() {
                return Some(name);
            }
        } else if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
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

/// 均分 N 段（每段 [start, end] 闭区间，末段吸收余数）
fn split_segments(total: u64, n: usize) -> Vec<SegmentState> {
    let n = n.max(1) as u64;
    let chunk = total / n;
    let mut segments = Vec::new();
    let mut start = 0;
    for i in 0..n {
        if start >= total {
            break;
        }
        let end = if i == n - 1 {
            total - 1
        } else {
            (start + chunk).min(total - 1)
        };
        if end < start {
            break;
        }
        segments.push(SegmentState {
            start,
            end,
            done: 0,
        });
        start = end + 1;
    }
    segments
}

/// tellStatus 的 JSON 快照。`keys` 提供时只保留声明的字段（gid 恒在）。
fn task_json(state: &TaskState, keys: Option<&[String]>) -> serde_json::Value {
    let completed = state.completed();
    let is_bt = state.kind == TaskKind::Torrent;
    // BT 是多文件结构：files 必须逐条列出（AriaNg 的「文件选择」直接绑这个数组）；
    // 元信息还没解析出来时退回单文件占位，别让客户端拿到空数组去画空气。
    let files = if is_bt && !state.bt_files.is_empty() {
        Value::Array(
            state
                .bt_files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    json!({
                        "index": index + 1,
                        "path": display_path(&state.dir.join(&file.relative)),
                        "length": file.length,
                        "completedLength": file.completed,
                        "selected": file.selected,
                        "indexed": true,
                        "uris": [],
                    })
                })
                .collect(),
        )
    } else {
        json!([{
            "index": 1,
            "path": display_path(&state.target_path()),
            "length": state.total,
            "completedLength": completed,
            "selected": true,
            "indexed": true,
            "uris": state
                .uris
                .iter()
                .map(|u| json!({"uri": u, "status": "used"}))
                .collect::<Vec<_>>(),
        }])
    };

    let mut value = json!({
        "gid": state.gid,
        "status": state.status.as_str(),
        "totalLength": state.total.max(completed),
        "completedLength": completed,
        "uploadLength": state.upload_length,
        "bitfield": "",
        "downloadSpeed": state.speed,
        "uploadSpeed": state.upload_speed,
        "infoHash": state.info_hash,
        "numSeeders": 0,
        "seeder": state.seeder,
        "pieceLength": state.piece_length,
        "numPieces": state.num_pieces,
        "connections": state.connections,
        "errorCode": state.error_code,
        "errorMessage": state.error_message,
        "followedBy": [],
        "following": [],
        "belongsTo": "",
        "verifiedLength": if is_bt { state.bt_completed } else { 0 },
        "dir": display_path(&state.dir),
        "files": files,
    });
    // aria2 只给 BT 任务带 bittorrent 键；凭空给 HTTP 任务加会让部分客户端误判类型
    if let Some(bt) = &state.bittorrent
        && let Some(map) = value.as_object_mut()
    {
        map.insert("bittorrent".into(), bt.clone());
    }
    match keys {
        Some(keys) if !keys.is_empty() => {
            let mut filtered = serde_json::Map::new();
            filtered.insert("gid".into(), state.gid.clone().into());
            for key in keys {
                if let Some(field) = value.get(key) {
                    filtered.insert(key.clone(), field.clone());
                }
            }
            serde_json::Value::Object(filtered)
        }
        _ => value,
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_segments_even_and_remainder() {
        let segs = split_segments(100, 8);
        assert_eq!(segs.len(), 8);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[7].end, 99);
        for pair in segs.windows(2) {
            assert_eq!(pair[1].start, pair[0].end + 1);
        }
        // 小于段数
        let tiny = split_segments(3, 8);
        assert_eq!(tiny.len(), 3);
        assert_eq!(tiny[2].end, 2);
        // 零长度
        assert!(split_segments(0, 8).is_empty());
        // 单段
        let one = split_segments(10, 1);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].start, one[0].end), (0, 9));
    }

    #[test]
    fn filenames_are_derived_safely() {
        assert_eq!(filename_from_url("http://x/a/b/big.zip?t=1"), "big.zip");
        assert_eq!(filename_from_url("http://x/"), "index.html");
        assert_eq!(
            filename_from_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf"),
            Some("报告.pdf".to_string())
        );
        assert!(is_safe_filename("ok.bin"));
        assert!(!is_safe_filename("../escape"));
        assert!(!is_safe_filename("a/b"));
        assert!(!is_safe_filename("a\\b"));
        assert!(!is_safe_filename("..hidden.."));
    }

    #[test]
    fn percent_decode_handles_utf8() {
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%2"), "bad%2");
    }

    #[test]
    fn path_prefix_guard_rejects_escape() {
        let root = Path::new("/data/downloads");
        assert!(path_contains(root, Path::new("/data/downloads/a/b.bin")));
        assert!(path_contains(root, Path::new("/data/downloads")));
        // 前缀撞名不算包含
        assert!(!path_contains(root, Path::new("/data/downloads-extra/x")));
        assert!(!path_contains(root, Path::new("/data")));
        assert!(!path_contains(Path::new(""), Path::new("/data")));
    }

    #[test]
    fn segments_fit_rejects_stale_control() {
        let good = vec![SegmentState {
            start: 0,
            end: 9,
            done: 3,
        }];
        assert!(segments_fit(&good, 10));
        let stale = vec![SegmentState {
            start: 0,
            end: 99,
            done: 3,
        }];
        assert!(!segments_fit(&stale, 10));
    }

    #[tokio::test]
    async fn task_lifecycle_remove_and_purge() {
        let base = std::env::temp_dir().join("asmoday-dl-lifecycle");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            4,
            2,
            true,
            None,
        ));
        // 不可达端口：任务创建成功，随后自己走向 error
        let gid = manager
            .add_uri(
                vec!["http://127.0.0.1:9/nothing.bin".into()],
                Some(base.to_str().unwrap().into()),
                Some("nothing.bin".into()),
                None,
                Vec::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(gid.len(), 16);

        // 等 error 落定（连接拒绝几乎立刻）
        for _ in 0..50 {
            if manager
                .tell_stopped()
                .await
                .iter()
                .any(|t| t["gid"] == gid.as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stopped = manager.tell_stopped().await;
        let entry = stopped.iter().find(|t| t["gid"] == gid.as_str()).unwrap();
        assert_eq!(entry["status"], "error");
        assert!(entry["errorCode"].as_u64().unwrap_or(0) != 0);
        // tellStatus 关键字段齐全（AriaNg 直接绑定这些键）
        assert!(entry["totalLength"].is_u64());
        assert!(entry["completedLength"].is_u64());
        assert!(entry["downloadSpeed"].is_u64());
        assert!(entry["connections"].is_u64());
        assert!(entry["errorMessage"].is_string());

        // removeDownloadResult 删得掉已停止任务；purge 清空
        assert!(manager.remove_result(&gid).await.is_ok());
        assert!(manager.remove_result(&gid).await.is_err());
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 暂停的任务只属于 waiting 族（aria2 语义），不能同时出现在 tellStopped
    /// —— 否则客户端的「已停止」页会多出一条还能继续的条目（v4.9.31）。
    #[tokio::test]
    async fn paused_task_stays_out_of_stopped_list() {
        let base = std::env::temp_dir().join("asmoday-dl-paused");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            2,
            1,
            true,
            None,
        ));
        // 只接受连接、不回响应：任务停在「等响应头」，暂停一定来得及
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let gid = manager
            .add_uri(
                vec![format!("http://{addr}/slow.bin")],
                Some(base.to_str().unwrap().into()),
                Some("slow.bin".into()),
                None,
                Vec::new(),
                None,
            )
            .await
            .unwrap();
        manager.pause(&gid).await.unwrap();

        let waiting = manager.tell_waiting().await;
        let entry = waiting.iter().find(|t| t["gid"] == gid.as_str()).unwrap();
        assert_eq!(entry["status"], "paused");
        assert!(
            !manager
                .tell_stopped()
                .await
                .iter()
                .any(|t| t["gid"] == gid.as_str()),
            "paused 不该出现在 tellStopped"
        );

        // 继续后回到 active/waiting，再移除收尾
        manager.unpause(&gid).await.unwrap();
        let status = manager.status_with_keys(&gid, None).await.unwrap()["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(matches!(status.as_str(), "active" | "waiting"), "{status}");
        manager.remove(&gid).await.unwrap();
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 刷新密钥（v4.9.36）：旧 `token:` 立即失效、新值生效，不需要重建管理器。
    #[tokio::test]
    async fn rotated_secret_invalidates_old_token() {
        let base = std::env::temp_dir().join("asmoday-dl-secret");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            2,
            1,
            true,
            None,
        );
        assert!(manager.secret_matches("token:secret1"));
        assert!(!manager.secret_matches("token:secret2"));
        // aria2 语义：必须带 `token:` 前缀，裸密钥不算
        assert!(!manager.secret_matches("secret1"));

        manager.replace_secret("secret2".into());
        assert!(!manager.secret_matches("token:secret1"));
        assert!(manager.secret_matches("token:secret2"));
        assert_eq!(manager.secret(), "secret2");

        // 引擎关闭时任何 key 都不放行
        let disabled = DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            2,
            1,
            false,
            None,
        );
        assert!(!disabled.secret_matches("token:secret1"));
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn dir_whitelist_rejects_escape() {
        let base = std::env::temp_dir().join("asmoday-dl-roots");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            4,
            2,
            true,
            None,
        );
        assert!(manager.resolve_dir(Some(base.to_str().unwrap())).is_ok());
        let outside = std::env::temp_dir().join("asmoday-dl-roots-outside");
        assert!(
            manager
                .resolve_dir(Some(outside.to_str().unwrap()))
                .is_err()
        );
        // 未指定 → 默认根
        let resolved = manager.resolve_dir(None).unwrap();
        assert!(path_contains(&base, &resolved));
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[test]
    fn torrent_uri_detection() {
        assert!(is_torrent_uri("magnet:?xt=urn:btih:abc"));
        assert!(is_torrent_uri("cab507494d02ebb1178b38f2e9d7be299c86b862"));
        assert!(!is_torrent_uri("http://example.com/a.torrent"));
        assert!(!is_torrent_uri("cab507494d02ebb1178b38f2e9d7be299c86b86")); // 39 位
        assert!(!is_torrent_uri("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
    }

    #[tokio::test]
    async fn magnet_rejected_when_bt_disabled() {
        let base = std::env::temp_dir().join("asmoday-dl-bt-off");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![base.clone()],
            4,
            2,
            true,
            None,
        ));
        let err = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862".into()],
                None,
                None,
                None,
                Vec::new(),
                None,
            )
            .await
            .unwrap_err();
        // 能力关闭时是「明确的拒绝」，而不是默默创建一个永远 waiting 的任务
        assert!(err.message.contains("bittorrent"));
        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
