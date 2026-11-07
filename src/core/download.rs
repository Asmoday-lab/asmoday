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
    /// `referer` 选项（Aria2 Explorer 单独发这个键，而不是塞进 `header`）
    pub referer: Option<String>,
    /// 请求头（"Name: value"），逐段请求都带
    pub headers: Vec<(String, String)>,
    /// changeOption 进来的其他键（getOption 原样回显，引擎不消费）
    pub extra: Vec<(String, String)>,
}

/// `aria2.addUri` 里引擎真正消费的参数（其余键由协议层吞掉）。
///
/// 收成一个结构体而不是七个位置参数：`dir` / `out` / `user-agent` / `referer`
/// 全是 `Option<String>`，位置传参只要错一格就是「把 UA 当成保存名」这类事故。
#[derive(Debug, Clone, Default)]
pub struct UriOptions {
    pub dir: Option<String>,
    pub out: Option<String>,
    pub user_agent: Option<String>,
    /// aria2 的 `--referer`；与 `header` 里的 `Referer:` 等价，后者优先
    pub referer: Option<String>,
    pub headers: Vec<(String, String)>,
    /// `max-connection-per-server` / `split` 的覆盖值（最终仍按全局上限收敛）
    pub segments: Option<usize>,
}

/// `header` 选项解析：**字符串数组**（aria2 RPC 原生形态，Aria2 Explorer 就发这个）
/// 与**换行分隔的单串**（AriaNg 设置里那格的写法）都收，返回 `None` 表示键不存在。
///
/// 按第一个 `:` 切分，值里的冒号原样保留 —— `Cookie: a=b; c=d`、`Referer: https://x/y:8080`
/// 都是这种形状。没有冒号的行直接丢掉（aria2 也只是忽略）。
pub(crate) fn parse_header_option(value: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let lines: Vec<&str> = match value {
        serde_json::Value::String(text) => text.split('\n').collect(),
        serde_json::Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    Some(
        lines
            .into_iter()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                Some((name.to_string(), value.trim().to_string()))
            })
            .collect(),
    )
}

/// 回显用：`header` 选项在 getOption / tellStatus 里仍是 `"Name: value"` 数组
pub(crate) fn header_lines(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect()
}

/// 把任务的 UA / referer / 自定义头装配成 `HeaderMap`。
///
/// 为什么不让调用方直接 `request.header(name, value)`：`RequestBuilder::header`
/// 遇到非法名字（`"Bad Header"`、中文、控制字符）会 **panic**，而那发生在任务协程里，
/// 表现为「任务凭空消失」——插件传什么头不由我们决定，必须逐条校验后丢掉坏的。
/// 同名头后写的覆盖先写的（UA 与 `header` 里的 `User-Agent` 撞车时以 `header` 为准，
/// 与 aria2 一致：显式 `--header` 优先于 `--user-agent`）。
pub(crate) fn task_header_map(
    user_agent: Option<&str>,
    referer: Option<&str>,
    headers: &[(String, String)],
) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let mut map = HeaderMap::new();
    let put = |map: &mut HeaderMap, name: HeaderName, value: &str| {
        match HeaderValue::from_str(value) {
            Ok(parsed) => {
                map.insert(name, parsed);
            }
            Err(_) => crate::log_warn!("dropped unusable header value for {}", name.as_str()),
        };
    };
    if let Some(ua) = user_agent {
        put(&mut map, reqwest::header::USER_AGENT, ua);
    }
    if let Some(referer) = referer {
        put(&mut map, reqwest::header::REFERER, referer);
    }
    for (name, value) in headers {
        match HeaderName::from_bytes(name.as_bytes()) {
            Ok(parsed) => put(&mut map, parsed, value),
            Err(_) => crate::log_warn!("dropped invalid header name: {name:?}"),
        }
    }
    map
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

/// 实例设置里下载 RPC 密钥的键名（`settings.key`，v4.9.36）。
pub const RPC_SECRET_SETTING: &str = "download_rpc_secret";

/// 实例设置里「允许的保存目录」的键名（JSON 数组，v4.9.37）。
pub const ROOTS_SETTING: &str = "download_roots";

/// 实例设置里 BT Tracker 列表的键名（JSON 字符串数组，v4.9.39）。
///
/// 语义对齐 aria2 的 `bt-tracker`：**追加**到每个新任务的 announce 列表里，
/// 不替换种子自带的那份。运行中的任务不会被追溯修改 —— librqbit 8.1.1 只在
/// `add_torrent` 时构造 tracker 轮询流，没有运行时增删接口（aria2 的全局选项
/// 同样是给「之后添加的种子」用的）。
pub const TRACKERS_SETTING: &str = "download_trackers";

/// Tracker 列表的上限：防止一次粘贴几万行把 UI 和种子添加拖死。
/// qBittorrent 默认列表约 30 条，公开列表（ngosang）也就 100 条上下。
pub const MAX_TRACKERS: usize = 200;

/// Tracker 归一化结果：留下合法的，报出丢弃的（附原因）。
pub struct TrackersUpdate {
    pub trackers: Vec<String>,
    pub rejected: Vec<String>,
}

/// 归一化 Tracker 列表：去空白、去重、按 scheme 白名单过滤。
///
/// 只放行 BT tracker 真正会用的协议（http / https / udp / ws / wss）。
/// 不做「聪明纠正」——`udp://` 写错成 `udp//` 的用户需要看见错误，而不是被
/// 悄悄改成另一个地址去 announce。
pub fn normalize_trackers(raw: &[String]) -> TrackersUpdate {
    let mut trackers: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for entry in raw {
        // 允许一行粘贴多个（空格 / 逗号分隔，与 aria2 的 bt-tracker 同形）
        for candidate in entry.split(['\n', '\r', ',', ' ', '\t']) {
            let url = candidate.trim();
            if url.is_empty() {
                continue;
            }
            let lower = url.to_ascii_lowercase();
            let scheme_ok = ["http://", "https://", "udp://", "ws://", "wss://"]
                .iter()
                .any(|scheme| lower.starts_with(scheme));
            // announce 地址必须比 scheme 长，「udp://」这种空主机名没有意义
            if !scheme_ok || lower.len() < 10 {
                rejected.push(url.to_string());
                continue;
            }
            if trackers.iter().any(|kept| kept == url) {
                continue;
            }
            if trackers.len() >= MAX_TRACKERS {
                rejected.push(format!("{url}（超过 {MAX_TRACKERS} 条上限）"));
                continue;
            }
            trackers.push(url.to_string());
        }
    }
    TrackersUpdate { trackers, rejected }
}

/// 把 Tracker 追加到 magnet 链接上（缺什么补什么）。
///
/// 为什么必须改 URI：librqbit 8.1.1 的 `AddTorrentOptions::trackers` **只对
/// `.torrent` 字节生效**，magnet 分支只读链接里的 `tr=`（见其 `add_torrent`）。
/// 40 位裸 info hash 先补成完整 magnet 再追加。
pub fn magnet_with_trackers(uri: &str, trackers: &[String]) -> String {
    if trackers.is_empty() {
        return uri.to_string();
    }
    let mut base = uri.trim().to_string();
    if base.len() == 40 && !base.contains(':') {
        base = format!("magnet:?xt=urn:btih:{base}");
    }
    // 已经有的一律不加：同一个 tracker 重复 announce 会被服务端当成滥用
    let existing = librqbit::Magnet::parse(&base)
        .map(|magnet| magnet.trackers)
        .unwrap_or_default();
    let mut out = base;
    for tracker in trackers {
        if existing.iter().any(|have| have == tracker) {
            continue;
        }
        out.push_str(if out.contains('?') { "&tr=" } else { "?tr=" });
        out.push_str(&percent_encode(tracker));
    }
    out
}

/// 只转义会破坏查询串的字符（RFC 3986 unreserved 之外）。
///
/// 不引 `percent-encoding`：这里只有一个用途，多一个依赖不值得。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 落盘目录配置的持久化形态：挂载点稳定 id + 相对子路径。
///
/// 为什么存 id 而不是绝对路径：挂载点路径是可改的（换盘、改配置），`m0`
/// 这类 id 在热重载后仍指向同一个位置；相对路径让「同一个配置」在换盘后继续
/// 有效。绝对路径只存在于运行期（`DownloadRoot::path`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootSpec {
    /// 挂载点 id（`m0` / `m1`…，见 `path_guard::MountPoint::id`）
    pub mount_id: String,
    /// 相对挂载点根的路径，空 = 挂载点根；分隔符统一为 `/`
    #[serde(default)]
    pub rel: String,
}

/// 运行期的落盘目录：配置 + 解析结果（供页面回显、RPC 白名单校验）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownloadRoot {
    pub mount_id: String,
    /// 挂载点展示名（页面显示 `名字 / 子路径`，不显示绝对路径）
    pub mount_name: String,
    pub rel: String,
    /// 解析出来的绝对路径（只用于落盘与 RPC `dir` 默认值）
    pub path: PathBuf,
    /// 挂载点当前是否在线（掉盘 / 网络共享断开时为 false，白名单不认它）
    pub online: bool,
}

impl DownloadRoot {
    /// 持久化形态（丢掉运行期解析出来的绝对路径）
    pub fn spec(&self) -> RootSpec {
        RootSpec {
            mount_id: self.mount_id.clone(),
            rel: self.rel.clone(),
        }
    }

    /// 页面展示文本：`挂载点名 / 子路径`，根目录只显示名字
    pub fn label(&self) -> String {
        if self.rel.is_empty() {
            self.mount_name.clone()
        } else {
            format!("{} / {}", self.mount_name, self.rel)
        }
    }
}

/// 规范化用户填的子路径：允许 `a/b`、`/a/b`（前导斜杠视作挂载点根），
/// 拒绝盘符、`..`、`~` 与空段。返回统一用 `/` 的相对路径。
pub fn normalize_rel(raw: &str) -> Result<String, String> {
    let text = raw.trim().replace('\\', "/");
    if text.is_empty() {
        return Ok(String::new());
    }
    // 盘符（`C:`）与 UNC（`//server/share`）都是绝对路径，不属于「挂载点内」
    if text.starts_with("//") || text.contains(':') {
        return Err("子路径必须是相对路径，不能带盘符或网络前缀".into());
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err("子路径不能包含 `..`".into()),
            "~" => return Err("子路径不能使用 `~`".into()),
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

/// 把「挂载点 id + 子路径」解析成可落盘的目录。
///
/// 返回（解析成功的根，逐条问题说明）。找不到的挂载点、只读挂载点、非法子路径
/// 都只跳过并给出原因 —— 一个坏条目不应当让整份配置失效，用户还能在页面上改。
pub fn resolve_roots(
    mounts: &crate::core::path_guard::MountSet,
    specs: &[RootSpec],
) -> (Vec<DownloadRoot>, Vec<String>) {
    let mut roots = Vec::new();
    let mut warnings = Vec::new();
    for spec in specs {
        let Some(entry) = mounts
            .mounts
            .iter()
            .find(|mount| mount.info.id == spec.mount_id)
        else {
            warnings.push(format!("挂载点 {} 不存在（可能已被移除）", spec.mount_id));
            continue;
        };
        if entry.info.readonly {
            warnings.push(format!("挂载点 {} 是只读的", entry.info.name));
            continue;
        }
        let rel = match normalize_rel(&spec.rel) {
            Ok(rel) => rel,
            Err(err) => {
                warnings.push(format!("{}：{err}", entry.info.name));
                continue;
            }
        };
        let Some(root) = entry.root.as_ref() else {
            // 掉线挂载点保留条目：用户能在设置里看到它，插回硬盘即恢复，
            // 不必重新填一遍
            roots.push(DownloadRoot {
                mount_id: entry.info.id.clone(),
                mount_name: entry.info.name.clone(),
                rel,
                path: PathBuf::from(&entry.info.path),
                online: false,
            });
            continue;
        };
        let path = if rel.is_empty() {
            root.clone()
        } else {
            root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        // 剥掉 Windows canonicalize 的 `\\?\` 前缀：这个路径会作为 RPC `dir`
        // 回给客户端、也会在页面上当默认值，verbatim 形式对第三方客户端不友好
        let path = PathBuf::from(crate::core::fs::normalize_display(&path));
        roots.push(DownloadRoot {
            mount_id: entry.info.id.clone(),
            mount_name: entry.info.name.clone(),
            rel,
            path,
            online: true,
        });
    }
    (roots, warnings)
}

/// 没有配置时给一个可用默认：第一个在线、可写的挂载点下的 `downloads`。
pub fn default_root_spec(mounts: &crate::core::path_guard::MountSet) -> Option<RootSpec> {
    mounts
        .mounts
        .iter()
        .find(|mount| mount.root.is_some() && !mount.info.readonly)
        .map(|mount| RootSpec {
            mount_id: mount.info.id.clone(),
            rel: "downloads".to_string(),
        })
}

/// `path` 相对 `base` 的剩余部分（纯文本比较，Windows 大小写不敏感）。
///
/// 只用于把旧配置里的绝对路径映射成挂载点子路径；真正的归属校验仍走
/// `path_guard` 的 canonicalize 口径。
fn rel_under(base: &str, path: &str) -> Option<String> {
    let base = base.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    if path.eq_ignore_ascii_case(base) {
        return Some(String::new());
    }
    if path.get(..base.len())?.eq_ignore_ascii_case(base)
        && path.as_bytes().get(base.len()) == Some(&b'/')
    {
        return path.get(base.len() + 1..).map(str::to_string);
    }
    None
}

/// 旧配置 `[download].roots`（绝对路径）→ 挂载点子路径（一次性种子）。
///
/// 映射不上的条目直接丢弃并给原因：v4.9.37 起下载目录必须在挂载点内，
/// 否则下完的文件在文件管理里根本看不见。
pub fn seed_root_specs(
    configured: &[String],
    mounts: &crate::core::path_guard::MountSet,
) -> (Vec<RootSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut warnings = Vec::new();
    for raw in configured {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Some(entry) = mounts.find_by_path_text(raw) else {
            warnings.push(format!("{raw} 不属于任何挂载点，已忽略"));
            continue;
        };
        let rel = match rel_under(&entry.info.path, raw) {
            Some(rel) => match normalize_rel(&rel) {
                Ok(rel) => rel,
                Err(err) => {
                    warnings.push(format!("{raw}：{err}"));
                    continue;
                }
            },
            None => {
                warnings.push(format!("{raw} 无法转换为挂载点内路径，已忽略"));
                continue;
            }
        };
        specs.push(RootSpec {
            mount_id: entry.info.id.clone(),
            rel,
        });
    }
    (specs, warnings)
}

/// 下载引擎。AppState 以 `Arc<DownloadManager>` 持有。
pub struct DownloadManager {
    http: reqwest::Client,
    /// RPC 密钥（`token:<secret>` 鉴权）。可在运行中替换：前端「刷新密钥」写库
    /// 之后直接换内存值，旧密钥下一个请求即失效，不需要重启（v4.9.36）。
    secret: Arc<RwLock<String>>,
    /// 允许落盘的根白名单（挂载点子路径，v4.9.37）。运行中可整体替换：
    /// 页面保存目录后写库 + 换内存，下一个任务即生效，不需要重启。
    roots: Arc<RwLock<Vec<DownloadRoot>>>,
    /// BT Tracker 列表（v4.9.39）。语义同 aria2 的 `bt-tracker`：追加到
    /// 每个**新**任务的 announce 列表；运行中可整体替换，下一个任务即生效。
    trackers: Arc<RwLock<Vec<String>>>,
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
        roots: Vec<DownloadRoot>,
        segments: usize,
        max_concurrent: usize,
        enabled: bool,
        bt: Option<Arc<BtEngine>>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        let max_concurrent_count = max_concurrent.clamp(1, 32);
        Self {
            http,
            secret: Arc::new(RwLock::new(secret)),
            roots: Arc::new(RwLock::new(roots)),
            trackers: Arc::new(RwLock::new(Vec::new())),
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

    /// 允许落盘的根白名单（挂载点子路径）。返回快照，调用方不要长期持有。
    pub fn roots(&self) -> Vec<DownloadRoot> {
        self.roots
            .read()
            .map(|roots| roots.clone())
            .unwrap_or_default()
    }

    /// 运行中替换落盘目录（页面保存后调用；调用方负责先写库）。
    ///
    /// 已存在的任务不受影响（它们已经记了自己的 `dir`），新任务按新白名单校验。
    pub fn replace_roots(&self, next: Vec<DownloadRoot>) {
        if let Ok(mut guard) = self.roots.write() {
            *guard = next;
        }
    }

    /// 当前 BT Tracker 列表（追加到新任务 announce 列表的那份）。
    pub fn trackers(&self) -> Vec<String> {
        self.trackers
            .read()
            .map(|trackers| trackers.clone())
            .unwrap_or_default()
    }

    /// 运行中替换 Tracker 列表（页面 / RPC 保存后调用；调用方负责先写库）。
    ///
    /// 只影响之后添加的任务：librqbit 在 `add_torrent` 时就把 tracker 轮询流
    /// 建好了，运行中的种子没有增删入口（aria2 的 `bt-tracker` 也只作用于
    /// 新种子）。
    pub fn replace_trackers(&self, next: Vec<String>) {
        if let Ok(mut guard) = self.trackers.write() {
            *guard = next;
        }
    }

    /// 默认落盘目录：白名单里第一个在线条目
    fn default_root_path(&self) -> Option<PathBuf> {
        self.roots()
            .into_iter()
            .find(|root| root.online)
            .map(|root| root.path)
    }

    /// 全局并发上限（创建时快照，改配置需重启）
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent_count
    }

    // -- 目录白名单 ---------------------------------------------------------

    /// 请求目录是否在白名单内；返回（尽量 canonical 的）路径。
    ///
    /// 白名单来源是「挂载点 + 子路径」（v4.9.37），已经过 `resolve_roots` 的
    /// 只读 / 越界校验；这里只做前缀匹配。Windows 大小写不敏感（与挂载点校验
    /// 同口径）；网络路径 canonicalize 可能失败（离线 SMB），失败时按字面量对比。
    fn resolve_dir(&self, requested: Option<&str>) -> Result<PathBuf, RpcError> {
        // 掉线挂载点不参与校验：文件根本写不进去，早拒绝比写到一半失败好
        let roots: Vec<DownloadRoot> = self
            .roots()
            .into_iter()
            .filter(|root| root.online)
            .collect();
        if roots.is_empty() {
            return Err(RpcError::new(
                2,
                "no download directory configured; pick a mount point in the downloads page",
            ));
        }
        let raw = match requested {
            Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir.trim()),
            _ => roots[0].path.clone(),
        };
        let canon = std::fs::canonicalize(&raw).unwrap_or_else(|_| normalize(&raw));
        for root in &roots {
            let root_canon =
                std::fs::canonicalize(&root.path).unwrap_or_else(|_| normalize(&root.path));
            if path_contains(&root_canon, &canon) {
                return Ok(canon);
            }
        }
        // 报错时把允许的根一并列出：插件里配的「下载位置」不在挂载点内是最常见的
        // 一类失败，只说「不在允许范围内」用户无从下手
        let allowed: Vec<String> = roots.iter().map(|root| display_path(&root.path)).collect();
        Err(RpcError::new(
            2,
            format!(
                "dir is not within allowed download roots: {} (allowed: {})",
                canon.display(),
                allowed.join(", ")
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
        options: UriOptions,
    ) -> Result<String, RpcError> {
        let UriOptions {
            dir,
            out,
            user_agent,
            referer,
            headers,
            segments: segments_override,
        } = options;
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
            referer,
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
            referer: None,
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
        offset: i64,
        num: usize,
    ) -> Vec<serde_json::Value> {
        let full = match list {
            WindowList::Waiting => self.tell_waiting().await,
            WindowList::Stopped => self.tell_stopped().await,
        };
        window_slice(full, list, offset, num)
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
    ///
    /// `header` 按 aria2 的形态回成字符串数组 —— AriaNg / Aria2 Explorer 的任务详情
    /// 直接把这些值填回表单，回成单个字符串它们就显示成一整块。
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
        if let Some(referer) = &state.options.referer {
            options.insert("referer".into(), referer.clone().into());
        }
        if !state.options.headers.is_empty() {
            options.insert("header".into(), header_lines(&state.options.headers).into());
        }
        for (key, value) in &state.options.extra {
            options.insert(key.clone(), value.clone().into());
        }
        Some(Value::Object(options))
    }

    /// 修改选项（aria2.changeOption）。
    ///
    /// `dir` 必须仍过白名单；`dir` / `out` 只对未开跑（waiting/paused）的任务生效。
    /// `header` / `referer` / `user-agent` **立刻替换**，运行中的任务在下一次重试或
    /// 分段续传时用上新值（每段请求都从任务状态现取，不缓存）。
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
        if let Some(referer) = options.get("referer").and_then(Value::as_str) {
            state.options.referer = Some(referer.to_string());
        }
        // 空数组视为「清空所有自定义头」，与 aria2 一致
        if let Some(value) = options.get("header") {
            state.options.headers = parse_header_option(value).unwrap_or_default();
        }
        if let Some(map) = options.as_object() {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "dir" | "out" | "user-agent" | "referer" | "header"
                ) {
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
        let dir = self
            .default_root_path()
            .map(|path| display_path(&path))
            .unwrap_or_default();
        let mut options = json!({
            "dir": dir,
            "max-concurrent-downloads": format!("{}", self.max_concurrent_count),
            "split": format!("{}", self.segments),
            "user-agent": "aria2/1.37.0",
        });
        if self.bt.is_some()
            && let Some(map) = options.as_object_mut()
        {
            map.insert("enable-dht".into(), json!("true"));
            map.insert("bt-max-peers".into(), json!("55"));
            // 与 aria2 同形：逗号分隔的 announce 列表；空列表时给空串
            map.insert("bt-tracker".into(), json!(self.trackers().join(",")));
        }
        options
    }
}

/// aria2 的 offset / num 窗口语义（纯函数，便于断言）。
///
/// 正 offset：`[offset, offset + num)`，**正序**返回。
/// 负 offset：`-1` 指队尾最后一个，`-2` 指倒数第二个，依此类推，并且
/// **倒序**返回（aria2 手册的例子：A,B,C 时 `tellWaiting(-1, 2)` → `["C","B"]`）。
/// AriaNg 拉「已完成 / 已停止」页用的正是 `-1, 1000` —— 只收无符号 offset 时
/// 这个请求会直接报错，页面就空了（v4.9.58）。
///
/// `tellWaiting()` 的顺序就是队列顺序；`tellStopped()` 内部是**最新在前**，
/// 而 aria2 的 offset 以「最早停止的」为 0，所以先反转成最早在前。
fn window_slice(
    mut full: Vec<serde_json::Value>,
    list: WindowList,
    offset: i64,
    num: usize,
) -> Vec<serde_json::Value> {
    if matches!(list, WindowList::Stopped) {
        full.reverse();
    }
    if offset < 0 {
        let back = offset.unsigned_abs() as usize;
        if back == 0 || back > full.len() {
            return Vec::new();
        }
        let start = full.len() - back;
        let take = if num == 0 { usize::MAX } else { num };
        return full[..=start].iter().rev().take(take).cloned().collect();
    }
    let offset = offset as usize;
    if offset >= full.len() {
        return Vec::new();
    }
    full.drain(..offset);
    if num == 0 {
        return full;
    }
    full.truncate(num);
    full
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

    // Tracker 列表在这一刻取快照：页面上改完只影响之后添加的任务
    let trackers = manager.trackers();
    let torrent = match bt.add(source, &dir, only_files, paused, &trackers).await {
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

/// 探测拿到的那几项：总长 / 是否支持 Range / ETag
struct RemoteInfo {
    total: Option<u64>,
    accept_ranges: bool,
    etag: Option<String>,
}

/// 重定向跳数上限（aria2 默认 5，浏览器 20，这里取中间偏宽的值）
const MAX_REDIRECTS: usize = 10;

/// 只在本次任务链里生效的 Cookie 容器。
///
/// 站点在 302 跳转页上种 Cookie、到 CDN 那一跳才校验，是防盗链的常见形态
/// （短链 → 对象存储尤其多）。原版 aria2 有 CookieStorage 会照做，而 reqwest
/// 的自动跟随只会删掉敏感头、不会吸收 `Set-Cookie`。这里按任务维度收一遍，
/// 重定向的下一跳与后续分段请求都带上，不落盘、不跨任务共享。
#[derive(Clone, Default)]
struct TaskCookies(Vec<(String, String)>);

impl TaskCookies {
    fn absorb(&mut self, headers: &reqwest::header::HeaderMap) {
        for raw in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(text) = raw.to_str() else { continue };
            let Some(pair) = text.split(';').next() else {
                continue;
            };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim().to_owned(), value.trim().to_owned());
            if name.is_empty() {
                continue;
            }
            self.0.retain(|(existing, _)| existing != &name);
            self.0.push((name, value));
        }
    }

    /// 插件显式传来的 Cookie 优先，站点后种的只补齐缺的那几项。
    fn apply(&self, headers: &mut reqwest::header::HeaderMap) {
        if self.0.is_empty() {
            return;
        }
        let mut cookie = headers
            .get(reqwest::header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        for (name, value) in &self.0 {
            let prefix = format!("{name}=");
            if cookie
                .split(';')
                .any(|part| part.trim_start().starts_with(&prefix))
            {
                continue;
            }
            if !cookie.is_empty() {
                cookie.push_str("; ");
            }
            cookie.push_str(&format!("{name}={value}"));
        }
        if let Ok(parsed) = reqwest::header::HeaderValue::from_str(&cookie) {
            headers.insert(reqwest::header::COOKIE, parsed);
        }
    }
}

/// 失败日志里标出进程看到的代理：cf_clearance 绑定出口 IP，浏览器与下载器
/// 走不同出口时 Cookie 必然失效。只打印 host:port，绝不打印代理账号密码。
fn proxy_summary() -> String {
    let mut parts = Vec::new();
    for (label, keys) in [
        ("http", ["http_proxy", "HTTP_PROXY"]),
        ("https", ["https_proxy", "HTTPS_PROXY"]),
        ("all", ["all_proxy", "ALL_PROXY"]),
    ] {
        if let Some(value) = keys.iter().find_map(|key| std::env::var(key).ok()) {
            let host = value.rsplit('@').next().unwrap_or(&value);
            parts.push(format!(
                "{label}={}",
                host.chars().take(64).collect::<String>()
            ));
        }
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" ")
    }
}

/// 日志只记「发了哪些头 / 各多长」，绝不打印 Cookie、Authorization 的值。
fn header_summary(headers: &reqwest::header::HeaderMap) -> String {
    let summary = headers
        .iter()
        .map(|(name, value)| format!("{}={}B", name.as_str(), value.len()))
        .collect::<Vec<_>>()
        .join(",");
    if summary.is_empty() {
        "(none; client defaults only)".to_string()
    } else {
        summary
    }
}

/// 发一次请求并**自己**跟随 3xx，每一跳重新装上同一份请求头。
///
/// 为什么不用 reqwest 的自动跟随：它在跨源跳转时按规范删掉 `Cookie` /
/// `Authorization` / `Proxy-Authorization`（`remove_sensitive_headers`），而且
/// 这个行为不受 redirect policy 控制。浏览器插件抓下来的链接恰好最常跨源跳
/// （短链 → CDN、图片站 → 图床、`/download?id=` → 对象存储），头一丢就是 403/401；
/// 原版 aria2 的 `--header` 是**每一跳原样重发**的，这里对齐它。
///
/// Range 也逐跳重发：跳转后拿到的是同一个文件，续传语义不能断。
async fn send_following(
    engine: &EngineHandle,
    method: reqwest::Method,
    uri: &str,
    headers: &reqwest::header::HeaderMap,
    range: Option<&str>,
    cookies: &mut TaskCookies,
) -> Result<reqwest::Response, FailKind> {
    let mut url = reqwest::Url::parse(uri)
        .map_err(|err| FailKind::Failed(1, format!("bad url {uri}: {err}")))?;
    for _ in 0..=MAX_REDIRECTS {
        let mut hop_headers = headers.clone();
        cookies.apply(&mut hop_headers);
        let mut request = engine
            .http
            .request(method.clone(), url.clone())
            .headers(hop_headers.clone());
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range);
        }
        let response = request
            .send()
            .await
            .map_err(|err| FailKind::Failed(1, err.to_string()))?;
        cookies.absorb(response.headers());
        let status = response.status();
        // 只有失败才打日志：403 到底出在哪一跳、当时带了哪些头，一眼可查
        if !status.is_success() && !status.is_redirection() {
            crate::log_warn!(
                "download {} {} range={} -> {}; sent headers: {}; proxy: {}",
                method,
                url,
                range.unwrap_or("-"),
                status,
                header_summary(&hop_headers),
                proxy_summary()
            );
        }
        if !status.is_redirection() {
            return Ok(response);
        }
        let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        else {
            // 3xx 却没有 Location：交给调用方按状态码判定失败
            return Ok(response);
        };
        let next = url
            .join(location)
            .map_err(|err| FailKind::Failed(1, format!("bad redirect target {location}: {err}")))?;
        // http(s) 之外（file://、ftp://）不跟：那不是我们要下的东西
        if !matches!(next.scheme(), "http" | "https") {
            return Err(FailKind::Failed(
                1,
                format!("redirected to unsupported scheme: {next}"),
            ));
        }
        url = next;
    }
    Err(FailKind::Failed(
        1,
        format!("too many redirects (> {MAX_REDIRECTS})"),
    ))
}

/// `bytes 0-0/12345` → `Some(12345)`；`bytes 0-0/*` → `None`
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse::<u64>().ok()
}

/// 从响应头读出探测结果（首请求就是普通 GET，没有独立探针）
fn remote_info_from(response: &reqwest::Response) -> RemoteInfo {
    let headers = response.headers();
    let partial = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let accept_ranges = partial
        || headers
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("bytes"));
    let total = if partial {
        headers
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total)
    } else {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    };
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    RemoteInfo {
        total,
        accept_ranges,
        etag,
    }
}

/// 拒绝响应归成「能自救 / 不能自救」两类，别把一整页 HTML 甩给用户。
struct RejectDetail {
    /// 命中已知反爬形态时，用诊断结论替换 401/403 那句泛泛的猜测
    reason: Option<String>,
    /// 其它站点：剥掉标签后的原文摘录
    excerpt: Option<String>,
}

fn plain_excerpt(raw: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

/// Cloudflare 的 `Just a moment...` 是**质询页**而不是权限拒绝：它要求客户端
/// 跑 JS / Turnstile 换 `cf_clearance`，而 clearance 绑定 IP + UA + TLS 指纹，
/// 换一个下载器就失效。这时候「补 Cookie/Referer」的建议是错的，得说清楚。
fn classify_reject(raw: &str) -> RejectDetail {
    let lower = raw.to_ascii_lowercase();
    let challenge = lower.contains("just a moment")
        || lower.contains("cf-chl")
        || lower.contains("challenge-platform")
        || lower.contains("cf-turnstile")
        || lower.contains("checking your browser")
        || lower.contains("cf-browser-verification")
        || lower.contains("enable javascript and cookies to continue");
    if challenge {
        return RejectDetail {
            reason: Some(
                "Cloudflare browser challenge: this site requires a JavaScript / Turnstile check that no download client (aria2 included) can pass by itself. Open the link in your browser first, then pass the cf_clearance Cookie together with the EXACT same User-Agent through the SAME exit IP (proxy); cf_clearance is bound to IP + UA + TLS fingerprint and expires quickly".to_string(),
            ),
            excerpt: None,
        };
    }
    // 只有四位错误码、没有挑战脚本 = 防火墙硬判决，换头没用
    let cf_code = ["1020", "1010", "1015", "1006", "1007", "1008", "1009"]
        .into_iter()
        .find(|code| {
            let short = format!("error {code}");
            let long = format!("error code: {code}");
            lower.contains(short.as_str()) || lower.contains(long.as_str())
        });
    if let Some(code) = cf_code {
        return RejectDetail {
            reason: Some(format!(
                "Cloudflare firewall verdict {code}: IP / browser-signature / rate-limit rejection, not a missing header. Retrying or adding cookies will not help; the exit IP or client fingerprint must change"
            )),
            excerpt: None,
        };
    }
    let excerpt = plain_excerpt(raw);
    RejectDetail {
        reason: None,
        excerpt: (!excerpt.is_empty()).then_some(excerpt),
    }
}

/// 读一小段响应体并分类。站点可能只回了 403 头却磨蹭着不发 body，给短超时。
async fn reject_detail(response: &mut reqwest::Response) -> Option<RejectDetail> {
    let mut text = String::new();
    let read = async {
        while text.len() < 8192 {
            match response.chunk().await.ok().flatten() {
                Some(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
                None => break,
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), read).await;
    (!text.is_empty()).then(|| classify_reject(&text))
}

/// 目标站明确拒绝：错误信息带上可执行的诊断结论 + 站点原文摘录。
/// 这条消息会原样进 tellStatus 的 errorMessage（插件 / WebUI 直接展示给用户）。
fn http_reject(status: reqwest::StatusCode, detail: Option<&RejectDetail>) -> FailKind {
    let code = status.as_u16() as u32;
    let fallback = "the link may need Cookie/Referer that were not forwarded, or a WAF is blocking non-browser clients";
    let msg = if let Some(reason) = detail.and_then(|item| item.reason.as_deref()) {
        format!("HTTP {status} ({reason})")
    } else {
        match code {
            401 | 403 => format!("HTTP {status} (target site refused: {fallback})"),
            _ => format!("HTTP {status}"),
        }
    };
    let excerpt = detail
        .and_then(|item| item.excerpt.as_deref())
        .map(|text| format!("; server said: {text}"))
        .unwrap_or_default();
    FailKind::Failed(code, format!("{msg}{excerpt}"))
}

/// 起手式：**只发一次普通 GET，这个响应体直接拿去下载**（对齐原版 aria2）。
///
/// v4.9.52/54 的独立探测链会先连发 HEAD、GET+Range、GET 三次，再另起一次下载
/// 请求。一次性签名链接 / 按请求次数做风控的 WAF 恰好被这几次「白请求」耗掉：
/// 探测全过、真正下载却 403，而 aria2 从头到尾只发一次所以能下。现在没有
/// 独立探测阶段——这一个响应既给长度 / ETag / Range 能力，也直接是数据流。
async fn open_remote(
    engine: &EngineHandle,
    uri: &str,
    headers: &reqwest::header::HeaderMap,
    cookies: &mut TaskCookies,
) -> Result<(reqwest::Response, RemoteInfo), FailKind> {
    let mut response =
        send_following(engine, reqwest::Method::GET, uri, headers, None, cookies).await?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = reject_detail(&mut response).await;
        return Err(http_reject(status, detail.as_ref()));
    }
    let info = remote_info_from(&response);
    Ok((response, info))
}

/// 小于这个尺寸就别分段了：多连接握手比单流更贵，还多挨几次 WAF 检查。
const MIN_SPLIT_BYTES: u64 = 1024 * 1024;

async fn download_task(engine: &EngineHandle, handle: &Arc<TaskHandle>) -> TaskOutcome {
    let cancel = handle.current_token();
    let (uri, headers) = {
        let state = handle.state.lock().await;
        (
            state.uris.first().cloned().unwrap_or_default(),
            task_header_map(
                state.options.user_agent.as_deref(),
                state.options.referer.as_deref(),
                &state.options.headers,
            ),
        )
    };

    // 1. 首请求：一次普通 GET，既是探测也是数据流（原版 aria2 的形态）。
    //    绝不能在这里另发 HEAD / Range 探针——一次性签名链接会被白白耗掉。
    let mut cookies = TaskCookies::default();
    let (first, probe) = open_remote(engine, &uri, &headers, &mut cookies).await?;
    let total = probe.total;
    let etag = probe.etag;

    {
        let mut state = handle.state.lock().await;
        state.total = total.unwrap_or(0);
        state.etag = etag.clone();
    }

    // 2. 分段方案：只有站点明确说支持 Range、文件够大、且引擎开了多段才分段。
    //    其余情况（绝大多数插件链接）直接消费首响应，全程只有一个请求。
    let want_split =
        probe.accept_ranges && total.is_some_and(|t| t >= MIN_SPLIT_BYTES) && engine.segments > 1;
    let target = handle.state.lock().await.target_path();
    let file_ready = tokio::fs::metadata(&target)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    let mut initial = Some(first);
    let segments: Vec<SegmentState> = {
        let saved = if file_ready {
            load_control(&handle.state.lock().await.control_path()).await
        } else {
            None
        };
        match (total, want_split, saved) {
            (Some(t), true, Some(saved))
                if saved.total == t
                    && saved.etag == etag
                    && !saved.segments.is_empty()
                    && segments_fit(&saved.segments, t) =>
            {
                initial = None; // 续传：另发带 Range 的请求
                saved.segments
            }
            (Some(t), true, _) => {
                initial = None; // 分段：另发带 Range 的请求
                split_segments(t, engine.segments)
            }
            // 单流：不发 Range 头，首响应直接是数据流；长度靠收尾校验
            _ => vec![SegmentState {
                start: 0,
                end: u64::MAX,
                done: 0,
            }],
        }
    };
    let segment_count = segments.len();

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

    // 3. 分段并发（首段可能直接复用首响应）
    let mut workers = Vec::with_capacity(segment_count);
    for (index, seg) in segments.into_iter().enumerate() {
        let bootstrap = if index == 0 { initial.take() } else { None };
        workers.push(tokio::spawn(download_segment(
            engine.clone(),
            handle.clone(),
            cancel.clone(),
            seg,
            bootstrap,
            cookies.clone(),
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
        // 谎报 Accept-Ranges 的 WAF：分段请求被 403/405/501 打回 → 清掉半成品，
        // 退回一个普通 GET 单流再试（对齐 aria2 的兜底形态）。
        if segment_count > 1
            && matches!(&kind, FailKind::Failed(code, _) if matches!(*code, 403 | 405 | 501))
        {
            crate::log_warn!(
                "{uri}: server refused ranged requests; falling back to a single plain GET"
            );
            // 分段失败时各 worker 已经 cancel 过旧令牌，回退这一轮必须换新，
            // 否则单流刚起跑就被自己取消，任务永远悬在 active。
            handle.reset_token();
            let cancel = handle.current_token();
            tokio::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&target)
                .await
                .map_err(|err| FailKind::Failed(1, err.to_string()))?;
            download_segment(
                engine.clone(),
                handle.clone(),
                cancel.clone(),
                SegmentState {
                    start: 0,
                    end: u64::MAX,
                    done: 0,
                },
                None,
                cookies.clone(),
            )
            .await?;
            let mut state = handle.state.lock().await;
            state.segments = vec![SegmentState {
                start: 0,
                end: u64::MAX,
                done: state.total,
            }];
            state.connections = 1;
        } else {
            return Err(kind);
        }
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
    mut initial: Option<reqwest::Response>,
    mut cookies: TaskCookies,
) -> Result<(), FailKind> {
    let (uri, path) = {
        let state = handle.state.lock().await;
        (
            state.uris.first().cloned().unwrap_or_default(),
            state.target_path(),
        )
    };

    // 从头开始的单流：先清空，免得上次失败留下的长文件在尾部残留旧字节。
    let fresh_single = seg.end == u64::MAX && seg.start == 0 && seg.done == 0;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(fresh_single)
        .open(&path)
        .await
        .map_err(|err| FailKind::Failed(1, format!("open {}: {err}", path.display())))?;

    let mut retries = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(FailKind::Cancelled);
        }
        let ranged = seg.end != u64::MAX;
        // 每轮重试都从任务状态现取请求头：changeOption 补的 Cookie 在运行中的任务上也生效
        let headers = {
            let state = handle.state.lock().await;
            task_header_map(
                state.options.user_agent.as_deref(),
                state.options.referer.as_deref(),
                &state.options.headers,
            )
        };
        let range = ranged.then(|| format!("bytes={}-{}", seg.start + seg.done, seg.end));

        // 首段直接复用 open_remote 打开的响应：整条任务只发一次请求。
        // 只有断流重试 / 真正的分段任务才会再发新请求。
        let outcome = match initial.take() {
            Some(response) => Ok(response),
            None => {
                send_following(
                    &engine,
                    reqwest::Method::GET,
                    &uri,
                    &headers,
                    range.as_deref(),
                    &mut cookies,
                )
                .await
            }
        };

        match outcome {
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
            Ok(mut resp) => {
                // 416：续传场景下段已完整，视为成功
                if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                    return Ok(());
                }
                let status = resp.status();
                // 401/403/404/405/501 是确定性拒绝：重试四次只会多挨四次风控，
                // 还会把一次性链接彻底耗死。立刻失败，并把站点原文带回去。
                if (status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS)
                    || status == reqwest::StatusCode::NOT_IMPLEMENTED
                {
                    let detail = reject_detail(&mut resp).await;
                    return Err(http_reject(status, detail.as_ref()));
                }
                retries += 1;
                if retries > 3 {
                    return Err(http_reject(status, None));
                }
                tokio::time::sleep(Duration::from_secs(retries as u64)).await;
            }
            Err(err) => {
                retries += 1;
                if retries > 3 {
                    return Err(err);
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

    /// 测试用落盘根（等价于「挂载点 m0 的根目录」）
    fn test_root(path: &Path) -> DownloadRoot {
        DownloadRoot {
            mount_id: "m0".into(),
            mount_name: "test".into(),
            rel: String::new(),
            path: path.to_path_buf(),
            online: true,
        }
    }

    /// aria2 手册的 offset 例子：A,B,C 时 `tellWaiting(-1, 2)` → C,B；
    /// 而 tellStopped 内部是「最新在前」，需先转成「最早停止的在前」再取窗口。
    #[test]
    fn window_slice_matches_aria2_offset_semantics() {
        fn gids(items: &[serde_json::Value]) -> Vec<String> {
            items
                .iter()
                .map(|item| item["gid"].as_str().unwrap_or("").to_string())
                .collect()
        }
        let queue = |names: &[&str]| -> Vec<serde_json::Value> {
            names
                .iter()
                .map(|name| serde_json::json!({ "gid": name }))
                .collect()
        };
        let waiting = queue(&["A", "B", "C"]);
        assert_eq!(
            gids(&window_slice(waiting.clone(), WindowList::Waiting, 0, 1)),
            vec!["A"]
        );
        assert_eq!(
            gids(&window_slice(waiting.clone(), WindowList::Waiting, 1, 2)),
            vec!["B", "C"]
        );
        assert_eq!(
            gids(&window_slice(waiting.clone(), WindowList::Waiting, -1, 2)),
            vec!["C", "B"]
        );
        assert_eq!(
            gids(&window_slice(waiting.clone(), WindowList::Waiting, -2, 1)),
            vec!["B"]
        );

        let stopped = queue(&["C", "B", "A"]); // 内部顺序：最新在前
        assert_eq!(
            gids(&window_slice(
                stopped.clone(),
                WindowList::Stopped,
                -1,
                1000
            )),
            vec!["C", "B", "A"]
        );
        assert_eq!(
            gids(&window_slice(stopped.clone(), WindowList::Stopped, 0, 1)),
            vec!["A"]
        );
        assert!(window_slice(stopped, WindowList::Stopped, -9, 1000).is_empty());
    }

    #[test]
    fn trackers_are_normalized_and_deduped() {
        // 换行 / 逗号 / 空格混写都要能吃下（前端一行一个，aria2 是逗号）
        let raw = vec![
            "udp://tracker.example:6969/announce\nhttps://t2.example/announce".to_string(),
            "  https://t2.example/announce  ".to_string(),
            "ftp://nope.example/announce".to_string(),
            "udp://".to_string(),
        ];
        let update = normalize_trackers(&raw);
        assert_eq!(
            update.trackers,
            vec![
                "udp://tracker.example:6969/announce",
                "https://t2.example/announce"
            ]
        );
        // 非法 scheme 与空主机名都要报出来，而不是静默吞掉
        assert_eq!(update.rejected.len(), 2);
        // 上限：第 201 条开始丢弃
        let many: Vec<String> = (0..MAX_TRACKERS + 5)
            .map(|i| format!("udp://t{i}.example:6969/announce"))
            .collect();
        let capped = normalize_trackers(&many);
        assert_eq!(capped.trackers.len(), MAX_TRACKERS);
        assert_eq!(capped.rejected.len(), 5);
        // 空输入 = 清空列表（合法请求）
        assert!(normalize_trackers(&[]).trackers.is_empty());
    }

    #[test]
    fn magnet_gets_trackers_appended_once() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=ubuntu";
        let once = magnet_with_trackers(magnet, &["udp://tracker.example:6969/announce".into()]);
        assert!(once.starts_with(magnet));
        assert!(once.ends_with("&tr=udp%3A%2F%2Ftracker.example%3A6969%2Fannounce"));

        // 重复保存同一份列表不能把同一条 tracker 加两遍
        let twice = magnet_with_trackers(&once, &["udp://tracker.example:6969/announce".into()]);
        assert_eq!(once.matches("tr=").count(), 1);
        assert_eq!(twice.matches("tr=").count(), 1);

        // 40 位裸 info hash 先补成 magnet 再追加
        let bare = "0123456789abcdef0123456789abcdef01234567";
        let from_bare = magnet_with_trackers(bare, &["https://t.example/announce".into()]);
        assert!(from_bare.starts_with("magnet:?xt=urn:btih:"));
        assert!(from_bare.ends_with("&tr=https%3A%2F%2Ft.example%2Fannounce"));

        // 编码后的形态必须能被 librqbit 解回原值（否则 tracker 会被静默丢掉）
        let parsed = librqbit::Magnet::parse(&once).expect("rewritten magnet parses");
        assert_eq!(
            parsed.trackers,
            vec!["udp://tracker.example:6969/announce".to_string()]
        );

        // 没有配置时不改原串（别给客户端造出一个「被我们改过」的 URI）
        assert_eq!(magnet_with_trackers(magnet, &[]), magnet);
    }

    #[test]
    fn tracker_list_is_runtime_replaceable() {
        let dir = std::env::temp_dir().join(format!("asmoday-trackers-{}", new_gid()));
        std::fs::create_dir_all(&dir).unwrap();
        let manager = DownloadManager::new(
            reqwest::Client::new(),
            "secret".into(),
            vec![test_root(&dir)],
            4,
            2,
            true,
            None,
        );
        assert!(manager.trackers().is_empty());
        manager.replace_trackers(vec!["udp://t.example:6969/announce".into()]);
        assert_eq!(
            manager.trackers(),
            vec!["udp://t.example:6969/announce".to_string()]
        );
        // 全局选项按 aria2 形状暴露（逗号分隔），客户端据此回显
        assert_eq!(
            manager
                .global_options()
                .get("bt-tracker")
                .and_then(Value::as_str),
            None,
            "BT 关掉时不声明 bt-tracker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        // 不可达端口：任务创建成功，随后自己走向 error
        let gid = manager
            .add_uri(
                vec!["http://127.0.0.1:9/nothing.bin".into()],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("nothing.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(gid.len(), 16);

        // 等 error 落定（三级探测各要过一趟代理，本机环境单跳可到 2s）
        for _ in 0..150 {
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
            vec![test_root(&base)],
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
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("slow.bin".into()),
                    ..Default::default()
                },
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
            vec![test_root(&base)],
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
            vec![test_root(&base)],
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
            vec![test_root(&base)],
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

    /// 子路径规范化：只接受挂载点内的相对路径（v4.9.37）
    #[test]
    fn rel_rejects_absolute_and_parent() {
        assert_eq!(normalize_rel("").unwrap(), "");
        assert_eq!(normalize_rel(" /media//movies/ ").unwrap(), "media/movies");
        assert_eq!(normalize_rel(r"media\movies").unwrap(), "media/movies");
        assert_eq!(normalize_rel(".").unwrap(), "");
        assert!(normalize_rel("../escape").is_err());
        assert!(normalize_rel("a/../../b").is_err());
        assert!(normalize_rel("C:/Windows").is_err());
        assert!(normalize_rel("//server/share").is_err());
        assert!(normalize_rel("~/home").is_err());
    }

    fn test_mount(
        id: &str,
        name: &str,
        display: &str,
        root: Option<&str>,
        readonly: bool,
    ) -> crate::core::path_guard::MountEntry {
        crate::core::path_guard::MountEntry {
            info: crate::core::path_guard::MountPoint {
                id: id.into(),
                name: name.into(),
                path: display.into(),
                readonly,
                implicit: false,
                online: root.is_some(),
                follow_symlinks: true,
            },
            root: root.map(PathBuf::from),
        }
    }

    /// 只读 / 未知 / 越界条目丢弃并给出原因；掉线条目保留但标 offline
    #[test]
    fn resolve_roots_filters_readonly_and_unknown() {
        let set = crate::core::path_guard::MountSet {
            mounts: vec![
                test_mount("m0", "Movies", "/srv/movies", Some("/srv/movies"), false),
                test_mount("m1", "Backup", "/mnt/backup", Some("/mnt/backup"), true),
                test_mount("m2", "Offline", "/mnt/offline", None, false),
            ],
            implicit: false,
        };
        let specs = vec![
            RootSpec {
                mount_id: "m0".into(),
                rel: "downloads".into(),
            },
            RootSpec {
                mount_id: "m1".into(),
                rel: String::new(),
            },
            RootSpec {
                mount_id: "m9".into(),
                rel: String::new(),
            },
            RootSpec {
                mount_id: "m2".into(),
                rel: "dl".into(),
            },
            RootSpec {
                mount_id: "m0".into(),
                rel: "../etc".into(),
            },
        ];
        let (roots, warnings) = resolve_roots(&set, &specs);
        assert_eq!(roots.len(), 2, "{warnings:?}");
        assert!(roots[0].path.ends_with("downloads"));
        assert!(roots[0].online);
        assert_eq!(roots[0].label(), "Movies / downloads");
        assert_eq!(roots[1].label(), "Offline / dl");
        assert!(!roots[1].online);
        // 只读、未知、越界各一条
        assert_eq!(warnings.len(), 3, "{warnings:?}");
    }

    /// 保存目录改完立即生效：旧根立刻不再放行，新根立刻可用（无需重启）
    #[tokio::test]
    async fn replacing_roots_takes_effect_immediately() {
        let first = std::env::temp_dir().join("asmoday-dl-roots-first");
        let second = std::env::temp_dir().join("asmoday-dl-roots-second");
        tokio::fs::create_dir_all(&first).await.unwrap();
        tokio::fs::create_dir_all(&second).await.unwrap();
        let manager = DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![test_root(&first)],
            4,
            2,
            true,
            None,
        );
        assert!(manager.resolve_dir(Some(second.to_str().unwrap())).is_err());
        manager.replace_roots(vec![test_root(&second)]);
        assert!(manager.resolve_dir(Some(second.to_str().unwrap())).is_ok());
        assert!(manager.resolve_dir(Some(first.to_str().unwrap())).is_err());
        let _ = tokio::fs::remove_dir_all(&first).await;
        let _ = tokio::fs::remove_dir_all(&second).await;
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
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        let err = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862".into()],
                UriOptions::default(),
            )
            .await
            .unwrap_err();
        // 能力关闭时是「明确的拒绝」，而不是默默创建一个永远 waiting 的任务
        assert!(err.message.contains("bittorrent"));
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    // -- Aria2 Explorer 兼容（v4.9.52）-------------------------------------

    #[test]
    fn header_option_accepts_array_and_newline_string() {
        // Aria2 Explorer 发数组（`["Cookie: ...", "User-Agent: ..."]`）
        let array = serde_json::json!(["Cookie: sid=abc; theme=dark", "User-Agent: Chrome/140"]);
        let parsed = parse_header_option(&array).unwrap();
        assert_eq!(
            parsed[0],
            ("Cookie".to_string(), "sid=abc; theme=dark".to_string())
        );
        assert_eq!(parsed[1].0, "User-Agent");

        // AriaNg 设置里那格是换行分隔的单串，同一份解析
        let text = serde_json::json!("Referer: https://a.example/p\nX-Token: a:b:c\n\nbogus");
        let parsed = parse_header_option(&text).unwrap();
        assert_eq!(parsed.len(), 2);
        // 值里的冒号必须保留（token 常见 `a:b:c`）
        assert_eq!(parsed[1], ("X-Token".to_string(), "a:b:c".to_string()));

        // 键不存在 -> None（changeOption 靠这个区分「不动」与「清空」）
        assert!(parse_header_option(&serde_json::Value::Null).is_none());
        assert_eq!(
            parse_header_option(&serde_json::json!([])).unwrap().len(),
            0
        );
    }

    #[test]
    fn task_headers_drop_bad_names_and_let_header_win() {
        let headers = vec![
            ("Cookie".to_string(), "sid=abc".to_string()),
            ("Bad Name".to_string(), "x".to_string()),
            ("User-Agent".to_string(), "Mozilla/5.0".to_string()),
        ];
        let map = task_header_map(
            Some("aria2/1.37.0"),
            Some("https://ref.example/p"),
            &headers,
        );
        assert_eq!(map.get("cookie").unwrap(), "sid=abc");
        assert_eq!(map.get("referer").unwrap(), "https://ref.example/p");
        // 显式 header 覆盖 user-agent 选项（aria2 同口径）
        assert_eq!(map.get("user-agent").unwrap(), "Mozilla/5.0");
        // 非法名字整条丢掉，且不能 panic（否则任务协程会静默消失）
        assert!(map.get("bad name").is_none());
        assert_eq!(map.len(), 3);
    }

    #[tokio::test]
    async fn change_option_roundtrips_header_and_referer() {
        let base = std::env::temp_dir().join("asmoday-dl-changeoption");
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::new(),
            "secret1".into(),
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec!["http://127.0.0.1:9/nothing.bin".into()],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        manager
            .change_option(
                &gid,
                &serde_json::json!({
                    "referer": "https://site.example/page",
                    "header": ["Cookie: sid=abc"],
                }),
            )
            .await
            .unwrap();
        let options = manager.option_of(&gid).await.unwrap();
        assert_eq!(options["referer"], "https://site.example/page");
        assert_eq!(options["header"], serde_json::json!(["Cookie: sid=abc"]));

        // 空数组 = 清空（aria2 语义），别把「清空」实现成「不动」
        manager
            .change_option(&gid, &serde_json::json!({ "header": [] }))
            .await
            .unwrap();
        let options = manager.option_of(&gid).await.unwrap();
        assert!(options.get("header").is_none());
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 极简 HTTP 桩：只解析请求行 + 头，把响应字节写回去。端口由内核分配。
    fn spawn_stub(
        handler: impl Fn(&str, &str, &std::collections::HashMap<String, String>) -> Vec<u8>
        + Send
        + Sync
        + 'static,
    ) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::Arc as StdArc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = StdArc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let Ok(clone) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(clone);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        return;
                    }
                    let mut headers = std::collections::HashMap::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() {
                            return;
                        }
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let _ = stream.write_all(&handler(&method, &path, &headers));
                    let _ = stream.flush();
                });
            }
        });
        port
    }

    /// 浏览器插件那条链路的重头戏，一次覆盖四件事：
    /// 1. HEAD 被 405 拒 → 退回 `GET + Range: bytes=0-0` 探测；
    /// 2. 跨端口 302（= 跨源）→ 每一跳都重带 `Cookie` / `Referer`（reqwest 自动跟随会丢）；
    /// 3. `referer` 选项与 `header` 数组真的到达服务端；
    /// 4. 分段 Range 请求同样带头。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explorer_headers_survive_head_405_and_cross_host_redirect() {
        const BODY: &[u8] = b"asmoday-";
        let content = spawn_stub(|method, _path, headers| {
            let authorized = headers.get("referer").map(String::as_str)
                == Some("https://site.example/page")
                && headers.get("cookie").map(String::as_str) == Some("sid=abc; theme=dark");
            if !authorized {
                return b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            // 防盗链站点的典型表现：HEAD 直接 405
            if method == "HEAD" {
                return b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let (start, end) = match headers.get("range") {
                Some(raw) => {
                    let spec = raw.trim_start_matches("bytes=");
                    let (a, b) = spec.split_once('-').unwrap();
                    let start: usize = a.parse().unwrap();
                    let end: usize = b.parse().unwrap_or(BODY.len() - 1);
                    (start.min(BODY.len()), end.min(BODY.len() - 1))
                }
                None => (0, BODY.len() - 1),
            };
            let slice = &BODY[start..=end];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
                BODY.len(),
                slice.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(slice);
            response
        });
        // 短链 / 图片站常见的形态：302 跳到另一个主机（这里用另一个端口模拟跨源）
        let jump = spawn_stub(move |_method, _path, _headers| {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{content}/file.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        });

        let base = std::env::temp_dir().join(format!(
            "asmoday-dl-explorer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            1,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{jump}/download?id=7")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    referer: Some("https://site.example/page".into()),
                    headers: vec![("Cookie".into(), "sid=abc; theme=dark".into())],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut status = String::new();
        for _ in 0..100 {
            let entry = manager.option_of(&gid).await.unwrap();
            assert_eq!(entry["referer"], "https://site.example/page");
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                status = item["status"].as_str().unwrap_or_default().to_string();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(status, "complete", "task did not finish: {status}");
        let written = tokio::fs::read(base.join("file.bin")).await.unwrap();
        assert_eq!(written, BODY);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// WAF / 防盗链的另一种形态：HEAD 与**一切**带 Range 的请求都 403，
    /// 只有普通 GET 放行 —— 原版 aria2 能下（它不探测、直接普通 GET）。
    /// 三级退路的最后一级必须兜住：普通 GET 探测成功 → 强制单流，任务照常完成。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waf_rejecting_head_and_range_still_downloads_via_plain_get() {
        const BODY: &[u8] = b"waf-allow-plain-get-only";
        let content = spawn_stub(|method, _path, headers| {
            if method == "HEAD" || headers.contains_key("range") {
                return b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"w1\"\r\nConnection: close\r\n\r\n",
                BODY.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(BODY);
            response
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-waf-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/file.bin")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut status = String::new();
        for _ in 0..100 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                status = item["status"].as_str().unwrap_or_default().to_string();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(status, "complete", "task did not finish: {status}");
        let written = tokio::fs::read(base.join("file.bin")).await.unwrap();
        assert_eq!(written, BODY);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 普通 GET 也被拒 = 真实拒绝（缺 Cookie / Referer 或风控）：任务失败，
    /// 且 errorMessage 给出可自救的排查方向，而不是一个让人对着猜的裸 403。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hard_403_reports_hint_in_error_message() {
        let content = spawn_stub(|_method, _path, _headers| {
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-hard403-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder().build().unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            2,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/file.bin")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut entry = None;
        for _ in 0..200 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                entry = Some(item);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let entry = entry.expect("task never reached a terminal state");
        assert_eq!(entry["status"], "error");
        assert_eq!(entry["errorCode"], 403);
        let message = entry["errorMessage"].as_str().unwrap_or_default();
        assert!(
            message.contains("403") && message.contains("Cookie"),
            "error message lacks diagnostic hint: {message}"
        );
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 一次性签名链接：只允许一次请求，第二次起一律 403。
    /// 旧版「HEAD → GET+Range → GET 探测 → GET 下载」会把额度耗光，
    /// 原版 aria2 只发一次所以能下 —— 这正是「连接测试成功、下载却 403」的真身。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_shot_link_is_fetched_with_exactly_one_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const BODY: &[u8] = b"one-shot-token-link";
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let content = spawn_stub(move |_method, _path, _headers| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n > 0 {
                return b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BODY.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(BODY);
            response
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-oneshot-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/file.bin?token=once")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut status = String::new();
        for _ in 0..100 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                status = item["status"].as_str().unwrap_or_default().to_string();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(status, "complete", "task did not finish: {status}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a single download must not spend more than one HTTP request"
        );
        assert_eq!(tokio::fs::read(base.join("file.bin")).await.unwrap(), BODY);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 短链在 302 上种 Cookie、CDN 那一跳才校验：原版 aria2 的 CookieStorage
    /// 会带上，我们以前不会（reqwest 自动跟随只会删敏感头）。插件自带的
    /// Cookie 与站点新种的必须同时存在，且不能互相覆盖。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_set_cookie_reaches_the_next_hop() {
        const BODY: &[u8] = b"cookie-from-redirect";
        let content = spawn_stub(|_method, _path, headers| {
            let cookie = headers.get("cookie").map(String::as_str).unwrap_or("");
            if !(cookie.contains("sid=abc") && cookie.contains("ticket=xyz")) {
                return b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BODY.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(BODY);
            response
        });
        let jump = spawn_stub(move |_method, _path, _headers| {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{content}/file.bin\r\nSet-Cookie: ticket=xyz; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-cookie-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            2,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{jump}/download?id=7")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    headers: vec![("Cookie".into(), "sid=abc".into())],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut status = String::new();
        for _ in 0..100 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                status = item["status"].as_str().unwrap_or_default().to_string();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(status, "complete", "task did not finish: {status}");
        assert_eq!(tokio::fs::read(base.join("file.bin")).await.unwrap(), BODY);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 403 页面上写着的真实原因要进 errorMessage；确定性拒绝也不许重试四次
    /// （多打几次只会更招风控，还把一次性链接彻底耗死）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hard_403_forwards_server_reason_without_retrying() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let content = spawn_stub(move |_method, _path, _headers| {
            counter.fetch_add(1, Ordering::SeqCst);
            let body = b"login required: cookie missing";
            let head = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(body);
            response
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-reason-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            2,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/file.bin")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("file.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut entry = None;
        for _ in 0..100 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                entry = Some(item);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let entry = entry.expect("task never reached a terminal state");
        assert_eq!(entry["status"], "error");
        assert_eq!(entry["errorCode"], 403);
        let message = entry["errorMessage"].as_str().unwrap_or_default();
        assert!(
            message.contains("login required: cookie missing"),
            "server-side reason lost: {message}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a deterministic 403 must fail fast instead of retrying"
        );
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 站点谎报 `Accept-Ranges: bytes`、却把所有带 Range 的请求都 403：
    /// 分段方案必须能整体退回一个普通 GET 单流，而不是把任务判死。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lying_accept_ranges_falls_back_to_single_stream() {
        let body = Arc::new(vec![b'z'; 1024 * 1024 + 7]);
        let expected_len = body.len();
        let shared = body.clone();
        let content = spawn_stub(move |_method, _path, headers| {
            if headers.contains_key("range") {
                return b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                shared.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(&shared);
            response
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-lierange-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            4,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/big.bin")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("big.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut status = String::new();
        for _ in 0..150 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                status = item["status"].as_str().unwrap_or_default().to_string();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(status, "complete", "task did not finish: {status}");
        assert_eq!(
            tokio::fs::read(base.join("big.bin")).await.unwrap().len(),
            expected_len
        );
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    /// 反爬拒绝要分类：Cloudflare 质询页给可执行结论，防火墙硬判决别让人白补
    /// Cookie，普通站点则保留剥掉标签后的原文。
    #[test]
    fn reject_details_classify_cloudflare_and_plain_text() {
        let cf = classify_reject(
            r#"<!DOCTYPE html><html lang="en-US"><head><title>Just a moment...</title>
<meta http-equiv="Content-Type" content="text/html; charset=UTF-8"></head>
<body><script src="/cdn-cgi/challenge-platform/h/b/scripts/jsd/main.js"></script></body></html>"#,
        );
        let reason = cf.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("Cloudflare browser challenge"), "{reason}");
        assert!(reason.contains("cf_clearance"), "{reason}");
        assert!(
            cf.excerpt.is_none(),
            "challenge HTML must not be dumped into errorMessage"
        );

        let block = classify_reject("<html><body>error code: 1010</body></html>");
        assert!(
            block.reason.as_deref().unwrap_or_default().contains("1010"),
            "{:?}",
            block.reason
        );
        assert!(block.excerpt.is_none());

        let plain = classify_reject("<html><body>login required: cookie missing</body></html>");
        assert!(plain.reason.is_none());
        assert_eq!(
            plain.excerpt.as_deref(),
            Some("login required: cookie missing")
        );
    }

    /// 端到端复刻用户实际收到的那张 Cloudflare 403 页：错误信息里不能再出现
    /// `<!DOCTYPE html>` 这类 boilerplate，而要告诉人 cf_clearance / UA / IP 三件事。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloudflare_challenge_page_reports_actionable_hint() {
        let content = spawn_stub(|_method, _path, _headers| {
            let body = b"<!DOCTYPE html><html lang=\"en-US\"><head><title>Just a moment...</title><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"></head><body><script src=\"/cdn-cgi/challenge-platform/h/b/scripts/jsd/main.js\"></script></body></html>";
            let head = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut response = head.into_bytes();
            response.extend_from_slice(body);
            response
        });

        let base = crate::core::test_scratch_dir().join(format!("dl-cf-{}", new_gid()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let manager = Arc::new(DownloadManager::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            "secret1".into(),
            vec![test_root(&base)],
            2,
            2,
            true,
            None,
        ));
        let gid = manager
            .add_uri(
                vec![format!("http://127.0.0.1:{content}/protected.bin")],
                UriOptions {
                    dir: Some(base.to_str().unwrap().into()),
                    out: Some("protected.bin".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let mut entry = None;
        for _ in 0..100 {
            let found = manager
                .tell_stopped()
                .await
                .into_iter()
                .find(|item| item["gid"] == gid.as_str());
            if let Some(item) = found {
                entry = Some(item);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let entry = entry.expect("task never reached a terminal state");
        assert_eq!(entry["status"], "error");
        assert_eq!(entry["errorCode"], 403);
        let message = entry["errorMessage"].as_str().unwrap_or_default();
        assert!(
            message.contains("Cloudflare browser challenge"),
            "{message}"
        );
        assert!(message.contains("cf_clearance"), "{message}");
        assert!(message.contains("User-Agent"), "{message}");
        assert!(!message.contains("<!DOCTYPE"), "{message}");
        assert!(!message.contains("<html"), "{message}");
        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
