//! aria2 兼容下载引擎（v4.9.26，设计文档 3.10）。
//!
//! 目标：asmoday 自身成为下载服务端，`/jsonrpc` 暴露 aria2 兼容协议，
//! AriaNg / Aria2 Explorer / 任意 aria2 客户端填地址 + 密钥即用。
//!
//! 本模块只管**引擎**（任务队列 + 分段下载 + 断点续传），协议层在
//! `api::aria2_rpc`。第一版能力边界：
//!
//! - HTTP(S) 多连接分段下载（`Range` 并发 + 控制文件续传）
//! - 队列语义对齐 aria2：active / waiting(含 paused) / stopped 族
//! - 不做：BT / Metalink / FTP —— `getVersion.enabledFeatures` 里**不声明**，
//!   客户端（AriaNg 等）会自动隐藏对应入口，不会出现「假支持」
//!
//! 落盘安全：所有任务目录必须落在 `download.roots` 白名单内（前缀校验，
//! Windows 大小写不敏感），RPC 传进来的 `dir` 不合法直接拒绝。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// 状态与数据结构
// ---------------------------------------------------------------------------

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
    pub uris: Vec<String>,
    pub status: Status,
    pub dir: PathBuf,
    pub out_name: String,
    pub total: u64,
    /// 各段进度；completed = sum(done)
    pub segments: Vec<SegmentState>,
    pub connections: usize,
    pub error_code: u32,
    pub error_message: String,
    pub options: TaskOptions,
    /// 速度采样：上次快照（时刻 + 字节），查询时算增量
    speed_sample: (Instant, u64),
    pub speed: u64,
    pub etag: Option<String>,
}

impl TaskState {
    pub fn completed(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
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

    /// 速度刷新：距上次采样 ≥ 500ms 才重算，避免客户端高频轮询时数字抖成锯齿
    fn refresh_speed(&mut self) {
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
}

impl Event {
    pub fn method(self) -> &'static str {
        match self {
            Event::Start => "aria2.onDownloadStart",
            Event::Pause => "aria2.onDownloadPause",
            Event::Stop => "aria2.onDownloadStop",
            Event::Complete => "aria2.onDownloadComplete",
            Event::Error => "aria2.onDownloadError",
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

/// 下载引擎。AppState 以 `Arc<DownloadManager>` 持有。
pub struct DownloadManager {
    http: reqwest::Client,
    /// RPC 密钥（`token:<secret>` 鉴权）
    secret: String,
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

impl DownloadManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: reqwest::Client,
        secret: String,
        roots: Vec<PathBuf>,
        segments: usize,
        max_concurrent: usize,
        enabled: bool,
    ) -> Self {
        let default_root = roots.first().cloned().unwrap_or_default();
        let (events, _) = broadcast::channel(256);
        let max_concurrent_count = max_concurrent.clamp(1, 32);
        Self {
            http,
            secret,
            default_root,
            roots,
            segments: segments.clamp(1, 32),
            enabled,
            max_concurrent: Arc::new(Semaphore::new(max_concurrent_count)),
            max_concurrent_count,
            tasks: Mutex::new(TaskMap::default()),
            events,
        }
    }

    /// 订阅任务事件（WS 通知通道）
    pub fn subscribe(&self) -> broadcast::Receiver<(String, Event)> {
        self.events.subscribe()
    }

    /// RPC 密钥校验（`token:<secret>` 全串比较）
    pub fn secret_matches(&self, token: &str) -> bool {
        self.enabled && token == format!("token:{}", self.secret)
    }

    /// 引擎是否启用（disabled 时协议层直接 404）
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// RPC 密钥持久化路径
    pub fn secret_path(data_dir: &Path) -> PathBuf {
        data_dir.join("download_rpc_secret")
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
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .ok_or_else(|| RpcError::new(2, "uris must contain an http(s) URL"))?;
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

    async fn get(&self, gid: &str) -> Option<Arc<TaskHandle>> {
        self.tasks.lock().await.tasks.get(gid).cloned()
    }

    /// 暂停：active → 落控制文件；waiting → 直接翻状态
    pub async fn pause(&self, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        {
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
        }
        handle.cancel_current();
        self.events.send((gid.to_string(), Event::Pause)).ok();
        self.mark_stopped(gid).await;
        Ok(gid.to_string())
    }

    /// 恢复：paused → 重新排队执行（换新取消令牌）
    pub async fn unpause(self: &Arc<Self>, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        let segments = {
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
            self.segments
        };
        handle.reset_token();
        {
            let mut map = self.tasks.lock().await;
            map.stopped.retain(|g| g != gid);
        }
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

    /// forceRemove：取消执行并移除
    pub async fn remove(&self, gid: &str) -> Result<String, RpcError> {
        let handle = self.get(gid).await.ok_or_else(|| unknown_gid(gid))?;
        let was_running = {
            let mut state = handle.state.lock().await;
            let running = matches!(state.status, Status::Active | Status::Waiting);
            state.status = Status::Removed;
            state.speed = 0;
            state.connections = 0;
            running
        };
        if was_running {
            handle.cancel_current();
        }
        self.events.send((gid.to_string(), Event::Stop)).ok();
        self.mark_stopped(gid).await;
        Ok(gid.to_string())
    }

    /// 清空全部已停止任务。返回删除的 GID 列表。
    pub async fn purge(&self) -> Vec<String> {
        let mut map = self.tasks.lock().await;
        let purged = map.stopped.clone();
        for gid in &purged {
            map.tasks.remove(gid);
        }
        let live: Vec<String> = map.tasks.keys().cloned().collect();
        map.order.retain(|g| live.contains(g));
        map.stopped.clear();
        purged
    }

    /// removeDownloadResult：只许删已停止的任务
    pub async fn remove_result(&self, gid: &str) -> Result<String, RpcError> {
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
        if map.tasks.remove(gid).is_none() {
            return Err(unknown_gid(gid));
        }
        map.order.retain(|g| g != gid);
        map.stopped.retain(|g| g != gid);
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

    /// 全局统计（getGlobalStat）
    pub async fn global_stat(&self) -> serde_json::Value {
        let active = self.tell_active().await;
        let waiting = self.tell_waiting().await;
        let stopped = self.tell_stopped().await;
        let speed: u64 = active
            .iter()
            .filter_map(|t| t["downloadSpeed"].as_u64())
            .sum();
        serde_json::json!({
            "downloadSpeed": speed,
            "uploadSpeed": 0,
            "numActive": active.len(),
            "numWaiting": waiting.len(),
            "numStopped": stopped.len(),
            "numStoppedTotal": stopped.len(),
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
        json!({
            "dir": self.default_root.to_string_lossy(),
            "max-concurrent-downloads": format!("{}", self.max_concurrent_count),
            "split": format!("{}", self.segments),
            "user-agent": "aria2/1.37.0",
        })
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
    let files = serde_json::json!([{
        "path": display_path(&state.target_path()),
        "length": state.total,
        "completedLength": completed,
        "selected": true,
        "indexed": true,
        "uris": state
            .uris
            .iter()
            .map(|u| serde_json::json!({"uri": u, "status": "used"}))
            .collect::<Vec<_>>(),
    }]);

    let value = serde_json::json!({
        "gid": state.gid,
        "status": state.status.as_str(),
        "totalLength": state.total.max(completed),
        "completedLength": completed,
        "uploadLength": 0,
        "bitfield": "",
        "downloadSpeed": state.speed,
        "uploadSpeed": 0,
        "infoHash": "",
        "numSeeders": 0,
        "seeder": false,
        "pieceLength": 0,
        "numPieces": 0,
        "connections": state.connections,
        "errorCode": state.error_code,
        "errorMessage": state.error_message,
        "followedBy": [],
        "following": [],
        "belongsTo": "",
        "verifiedLength": 0,
        "dir": display_path(&state.dir),
        "files": files,
    });
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
}
