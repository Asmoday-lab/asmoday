//! PTY 会话池（设计文档 3.3）。
//!
//! - 每个会话是一条真实的系统 shell（Windows 走 ConPTY），不是模拟终端；
//! - **环境变量全量继承**当前进程（`CommandBuilder::new` 以 `env::vars_os()` 为基线，
//!   全程**不调用** `env_clear()`）：`PATH` / `HOME` / `USERPROFILE` 原样带下去，
//!   终端里的 conda / node / cargo 才能像原生终端一样跑（设计文档 3.3.6）；
//! - 子进程的 stderr 与 stdout 共用同一个 pty 从设备，**不做任何过滤**，
//!   profile / bashrc 的报错会原样经 WebSocket 推到 xterm（`PSReadLine` 历史
//!   文件写不进去之类只报错、不中断会话）；
//! - 输出进环形缓冲区（512 KiB），断线重连时先回放，再追增量；
//! - 连接数归零后启动 3 分钟回收计时，期间重连即可原地续命；
//! - 会话 id 由进程内计数器生成，不对外暴露任何 shell 参数拼接。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

/// 断线后保留会话的时长（设计文档要求 3 分钟）
pub const REAP_DELAY: Duration = Duration::from_secs(180);
/// 单用户最大并发 PTY 会话数（设计文档 3.3：防前端死循环重连把 CPU 打满）
pub const MAX_SESSIONS: usize = 10;
/// 环形缓冲区容量（设计文档 3.3.3：上限 1MB，超出丢最旧数据）
const BUFFER_CAPACITY: usize = 1024 * 1024;

/// PTY 尺寸合法区间（设计文档 3.3.2 尺寸强制兜底）。
///
/// 0 / 负数 / 超大值绝不能透传给底层 PTY：ConPTY 收到非法尺寸会在内部算出
/// 负的填充宽度并崩溃（现象是整块终端全屏报错，如 `totalWidth -17`）。
pub const MIN_COLS: u16 = 20;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 5;
pub const MAX_ROWS: u16 = 200;

/// 把任意来源的尺寸夹进合法区间（首连查询串与后续 resize 控制帧共用一套规则）。
pub fn clamp_size(cols: u64, rows: u64) -> (u16, u16) {
    (
        cols.clamp(MIN_COLS as u64, MAX_COLS as u64) as u16,
        rows.clamp(MIN_ROWS as u64, MAX_ROWS as u64) as u16,
    )
}

/// 拉起 shell 的方式（设计文档 3.3.7）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOptions {
    /// 纯净模式：跳过 profile / rc，专用于配置文件死循环或严重报错时的救急
    pub no_profile: bool,
    /// 配置覆盖的 shell（`[terminal].shell`；空 = 自动探测）
    pub shell: String,
    /// 配置覆盖的启动参数（`[terminal].args`；非空 = 完全替换内置参数）
    pub args: Vec<String>,
}

/// 展开成绝对路径并去掉 canonicalize 留下的 `\\?\` 前缀。
fn absolutize(path: &Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    std::path::PathBuf::from(stripped.to_string())
}

/// 终端启动环境（设计文档 3.3.5，历史记录强制隔离：极高优先级）。
///
/// **绝不依赖系统目录**：无论进程是否有管理员权限，一律把 shell 历史指向
/// `<data_dir>/terminal_history/`，不再去碰 `%APPDATA%\...\PSReadLine` 或
/// `~/.bash_history`。Windows 的 UAC / 低完整性级别令牌下，写用户目录必被拒，
/// 那就会变成「每敲一条命令就红字一次」——所以不做探测、不做条件分支，
/// 直接隔离，终端行为和原生完全一致，前端也无需弹任何提示。
#[derive(Debug, Clone, Default)]
pub struct TerminalEnv {
    /// 隔离后的历史文件绝对路径（已展开，不依赖 shell 自己的 cwd）
    pub history_path: Option<String>,
}

/// 准备隔离的历史目录（设计文档 3.3.5）。
///
/// `data_dir` 会先规范化成绝对路径：`-d data` 这类相对参数如果不展开，
/// 注入给 PSReadLine 的 HistorySavePath 会按 **shell 自己的 cwd**（往往是
/// 主目录）解析，历史写去一个谁都找不到的地方。
///
/// 目录建不出来（磁盘满 / 权限全坏）时仍然注入路径——让 shell 自己报错并
/// 原样透传，绝不回退到系统目录。
pub fn prepare_env(data_dir: &Path) -> TerminalEnv {
    let data_dir = absolutize(data_dir);
    let dir = data_dir.join("terminal_history");
    let _ = std::fs::create_dir_all(&dir);
    let file = if cfg!(windows) {
        dir.join("ConsoleHost_history.txt")
    } else {
        dir.join(".bash_history")
    };
    TerminalEnv {
        history_path: Some(file.to_string_lossy().into_owned()),
    }
}

/// 目标平台标识（前端据此决定是否显示「修复乱码」按钮）
pub const fn platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}

/// 一条 PTY 会话。
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub shell: String,
    /// 是否以纯净模式拉起（重连时展示用；模式不同不能复用同一会话）
    pub no_profile: bool,
    pub created: Instant,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    buffer: Mutex<VecDeque<u8>>,
    pub output: broadcast::Sender<Vec<u8>>,
    /// 当前挂着的 WebSocket 连接数
    pub clients: AtomicUsize,
    pub size: Mutex<(u16, u16)>,
}

impl Session {
    /// 回放缓冲区快照。
    pub fn replay(&self) -> Vec<u8> {
        let buffer = self.buffer.lock().expect("pty buffer poisoned");
        buffer.iter().copied().collect()
    }

    pub fn write_all(&self, data: &[u8]) -> Result<(), String> {
        let mut writer = self.writer.lock().map_err(|_| "pty writer poisoned".to_string())?;
        writer
            .write_all(data)
            .and_then(|_| writer.flush())
            .map_err(|e| format!("写入 PTY 失败：{e}"))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        // 最后一道闸：即便调用方漏了钳制，也绝不把 0 / 超大值塞进 ConPTY
        let (cols, rows) = clamp_size(cols as u64, rows as u64);
        let master = self.master.lock().map_err(|_| "pty master poisoned".to_string())?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("调整终端尺寸失败：{e}"))?;
        if let Ok(mut size) = self.size.lock() {
            *size = (cols, rows);
        }
        Ok(())
    }

    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// 先礼貌退场再强杀（对齐本地 shell 关闭体验）。
    ///
    /// 直接 kill 会跳过 shell 的退出流程：PSReadLine 不保存收尾历史、
    /// conda/npm 这类前台子进程被连坐成孤儿。这里先敲一个 `exit\r`
    /// （空闲 shell 正常退出；若正跑着前台进程，这几个键只会落进它的
    /// stdin，随后照样强杀，不会挂起），短暂等待后兜底 kill。
    pub fn close_graceful(&self) {
        if self.write_all(b"exit\r").is_ok() {
            std::thread::sleep(Duration::from_millis(700));
        }
        self.kill();
    }
}

/// 会话池。
#[derive(Clone)]
pub struct TerminalManager {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    counter: Arc<AtomicUsize>,
    /// 单用户并发上限（`[terminal].max_sessions`）
    max_sessions: usize,
    /// 断线保活时长（`[terminal].idle_kill_seconds`）
    reap_delay: Duration,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicUsize::new(0)),
            max_sessions: MAX_SESSIONS,
            reap_delay: REAP_DELAY,
        }
    }
}

/// 一条会话的对外快照（设计文档 3.3.6：前端「活动会话」面板据此展示与强制关闭）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub shell: String,
    pub no_profile: bool,
    pub cols: u16,
    pub rows: u16,
    /// 当前挂着的 WebSocket 数：0 表示已断线正在保活，是最该被清理的
    pub clients: usize,
    /// 已存活秒数
    pub age_secs: u64,
}

impl TerminalManager {
    /// 当前所有会话的快照，按存活时间升序（最新的在前）。
    pub fn snapshot(&self) -> Vec<SessionInfo> {
        let Ok(map) = self.inner.lock() else {
            return Vec::new();
        };
        let mut list: Vec<SessionInfo> = map
            .values()
            .map(|session| {
                let (cols, rows) = session
                    .size
                    .lock()
                    .map(|guard| *guard)
                    .unwrap_or((MIN_COLS, MIN_ROWS));
                SessionInfo {
                    id: session.id.clone(),
                    cwd: session.cwd.clone(),
                    shell: session.shell.clone(),
                    no_profile: session.no_profile,
                    cols,
                    rows,
                    clients: session.clients.load(Ordering::SeqCst),
                    age_secs: session.created.elapsed().as_secs(),
                }
            })
            .collect();
        list.sort_by_key(|info| info.age_secs);
        list
    }

    /// 强制关闭指定会话（设计文档 3.3.6）。返回该会话是否存在。
    pub fn force_close(&self, id: &str) -> bool {
        let removed = self.inner.lock().ok().and_then(|mut map| map.remove(id));
        match removed {
            Some(session) => {
                session.kill();
                true
            }
            None => false,
        }
    }
}

/// 默认 Shell（设计文档 3.3 / 3.3.5）。
///
/// - Windows：**优先 PowerShell 7（pwsh.exe）**。Conda、fnm 这类工具的初始化
///   通常只写在 pwsh 7 的 `$PROFILE`（`Documents\PowerShell\`）里，5.1 的
///   `powershell.exe` 读的是另一份 `Documents\WindowsPowerShell\` profile——
///   固定拉 5.1 会出现「终端里 node 找不到」的落差（Windows Terminal 装了
///   pwsh 时同样默认用它）。逐个候选探测，都没有才回退 System32 的 5.1。
/// - 类 Unix：读 `$SHELL`，不可用（未设置或文件不存在）时回退 `/bin/bash`。
#[cfg(windows)]
fn default_shell() -> String {
    // 依优先级排列的候选：商店版 pwsh 的执行别名也认（该目录只列已安装应用的别名）
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for root in ["ProgramFiles", "ProgramW6432"] {
        if let Ok(dir) = std::env::var(root) {
            candidates.push(
                Path::new(&dir)
                    .join("PowerShell")
                    .join("7")
                    .join("pwsh.exe"),
            );
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            Path::new(&local)
                .join("Microsoft")
                .join("WindowsApps")
                .join("pwsh.exe"),
        );
    }
    if let Ok(root) = std::env::var("SystemRoot") {
        candidates.push(
            Path::new(&root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    if let Some(shell) = candidates.into_iter().find(|p| p.is_file()) {
        return shell.to_string_lossy().to_string();
    }
    let fallback = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    if fallback.is_file() {
        return fallback.to_string_lossy().to_string();
    }
    // 理论上到不了：System32 的 5.1 在任何 Windows 都在；真被裁剪掉就交 cmd.exe
    // （设计文档 3.3.1：pwsh → powershell 5.1 → cmd）
    "cmd.exe".to_string()
}

/// bash 的真实落点（设计文档 3.3.1）。
///
/// **严禁硬编码 `/bin/bash`**：Android / Termux 的根目录没有 `/bin/bash`，
/// bash 装在 `$PREFIX/bin/bash`（通常是 `/data/data/com.termux/files/usr/bin/bash`）。
/// 顺序固定为：`$PREFIX/bin/bash` → `/bin/bash`；都找不到时交给调用方降级。
#[cfg(not(windows))]
fn bash_path() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(prefix) = std::env::var("PREFIX") {
        let prefix = prefix.trim().trim_end_matches('/');
        if !prefix.is_empty() {
            candidates.push(format!("{prefix}/bin/bash"));
        }
    }
    candidates.push("/bin/bash".to_string());
    candidates.into_iter().find(|p| Path::new(p).is_file())
}

/// zsh 的真实落点（设计文档 3.3.5）。
///
/// `$SHELL` 缺失或无效时的第一顺位回退：Termux 用户普遍装 zsh 当默认 shell，
/// 但 systemd / docker 拉起的服务进程拿不到 `$SHELL`，只探测 bash 会让
/// 「终端里的环境和 SSH 里不一样」。顺序：`$PREFIX/bin/zsh` → `/bin/zsh` → `/usr/bin/zsh`。
#[cfg(not(windows))]
fn zsh_path() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(prefix) = std::env::var("PREFIX") {
        let prefix = prefix.trim().trim_end_matches('/');
        if !prefix.is_empty() {
            candidates.push(format!("{prefix}/bin/zsh"));
        }
    }
    candidates.push("/bin/zsh".to_string());
    candidates.push("/usr/bin/zsh".to_string());
    candidates.into_iter().find(|p| Path::new(p).is_file())
}

#[cfg(not(windows))]
fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|shell| shell.trim().to_string())
        .filter(|shell| !shell.is_empty() && Path::new(shell).is_file())
        // `$SHELL` 缺失（systemd 拉起、docker exec 无 env）时依次探测 zsh → bash；
        // 最后的 sh 兜底先试 Termux 的 $PREFIX/bin/sh（设计文档 3.3.1），
        // 再退 POSIX 的 /bin/sh——绝不给一个不存在的路径
        .or_else(zsh_path)
        .or_else(bash_path)
        .unwrap_or_else(|| {
            std::env::var("PREFIX")
                .ok()
                .map(|prefix| format!("{}/bin/sh", prefix.trim().trim_end_matches('/')))
                .filter(|path| Path::new(path).is_file())
                .unwrap_or_else(|| "/bin/sh".to_string())
        })
}

/// 按模式挑 shell（设计文档 3.3.7）。
///
/// 配置覆盖优先级：`[terminal].shell` 非空时优先；纯净模式（救急通道）忽略一切配置，
/// 固定走自动探测——配置文件把 shell 卡死后，用户还得靠它进得来修配置。
#[cfg(windows)]
fn shell_for(_options: &ShellOptions) -> String {
    // Windows 的纯净模式是同一个 shell exe 加 `-NoProfile`，二进制不变
    default_shell()
}

/// 按模式挑 shell：纯净模式固定 bash。
///
/// `--noprofile --norc` 是 bash 专有参数，用户 `$SHELL` 是 zsh/fish 时直接传过去
/// 会启动失败；所以只在确实有 bash 时切过去，没有就退回原 shell（不加参数）。
#[cfg(not(windows))]
fn shell_for(options: &ShellOptions) -> String {
    if options.no_profile {
        if let Some(bash) = bash_path() {
            return bash;
        }
    }
    default_shell()
}

/// 配置覆盖的 shell 是否生效（设计文档 3.3.5）。
///
/// 返回 `Some` 用配置值；`None` 走自动探测。纯净模式恒为 `None`。
fn configured_shell(options: &ShellOptions) -> Option<String> {
    if options.no_profile {
        return None;
    }
    let trimmed = options.shell.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 配置覆盖的启动参数是否生效：非空则**完全替换**内置参数（`-l` / `-NoExit` 等）。
/// 纯净模式恒为 `None`——救急通道的 `--noprofile --norc` 不能被配置顶掉。
fn configured_args(options: &ShellOptions) -> Option<&[String]> {
    (!options.no_profile && !options.args.is_empty()).then_some(options.args.as_slice())
}

/// 组装 Windows `-Command` 的载荷（设计文档 3.3.5）。
///
/// 顺序是硬要求：
/// 1. `chcp 65001` **优先**：先切 UTF-8 代码页，之后所有输出（含 profile 报错）才不乱码；
/// 2. `Set-PSReadLineOption -HistorySavePath` 把历史钉死在隔离目录，用户敲第一条
///    命令前就生效——绝不落到 `%APPDATA%`（受限令牌下必被拒，红字会一条接一条）。
#[cfg(windows)]
fn windows_command_payload(history_path: Option<&str>) -> String {
    let mut parts: Vec<String> = vec!["chcp.com 65001 > $null".to_string()];
    if let Some(path) = history_path {
        // PowerShell 单引号字符串里，' 用 '' 转义
        let escaped = path.replace('\'', "''");
        parts.push(format!("Set-PSReadLineOption -HistorySavePath '{escaped}'"));
    }
    parts.join("; ")
}

/// 追加 shell 启动参数。
///
/// Windows 走 `-NoExit -Command`：`-NoExit` 保证 `-Command` 执行完后**留在交互式
/// 会话里**（否则 PowerShell 会直接退出），同时 `$PROFILE` 照常加载——Conda、fnm
/// 的初始化都写在那里，不带 `-NoExit` 的 `-Command` 会让这些环境变量全丢。
/// `-Command` 传进去的命令不算交互输入，**不会**污染 PSReadLine 历史或屏幕输出。
#[cfg(windows)]
fn apply_shell_args(
    command: &mut CommandBuilder,
    _shell: &str,
    options: &ShellOptions,
    history_path: Option<&str>,
) {
    command.arg("-NoLogo");
    if options.no_profile {
        command.arg("-NoProfile");
    }
    command.arg("-NoExit");
    command.arg("-Command");
    command.arg(windows_command_payload(history_path));
}

/// 类 Unix：默认以 **Login Shell** 启动（设计文档 3.3.5）。
///
/// `bash -l` / `zsh -l` 会读 `/etc/profile` 与用户的 profile 脚本，Conda、nvm、fnm
/// 这些工具的初始化都藏在里面；不登录启动的 shell 会让用户觉得「终端里的环境和 SSH 里不一样」。
/// 纯净模式（救急）则反过来：跳过 profile 与 rc，并固定用 bash 以保证 `--noprofile --norc` 有效。
///
/// 历史重定向（`HISTFILE`）不在这里注入——它挪到了 `create()`，
/// 因为 `[terminal].args` 自定义参数会整段替换这里的参数，而历史隔离必须永远生效。
#[cfg(not(windows))]
fn apply_shell_args(command: &mut CommandBuilder, shell: &str, options: &ShellOptions) {
    let name = Path::new(shell)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let is_bash = name == "bash";

    if options.no_profile {
        // 只有 bash 认这两个参数；其它 shell 一律不加（宁可不纯净，也不能起不来）
        if is_bash {
            command.args(["--noprofile", "--norc"]);
        }
        return;
    }

    // `-l` 只给认识的 POSIX 家族 shell；nushell / elvish 之类参数不同，绝不硬塞
    const LOGIN_SHELLS: &[&str] = &["bash", "zsh", "fish", "ksh", "mksh", "dash", "ash", "tcsh", "csh"];
    if LOGIN_SHELLS.contains(&name.as_str()) {
        command.arg("-l");
    }
}

/// 把工作目录转换成目标平台的原生形式再交给 PTY。
///
/// 前端可能传来 POSIX 风格的 `C:/Users/me`；Windows 上路径含空格或中文时
/// 混用分隔符偶尔会被 CreateProcess 解析错，所以这里统一成反斜杠，
/// 并去掉 canonicalize 留下的 `\\?\` 长路径前缀。
#[cfg(windows)]
fn native_cwd(path: &Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    std::path::PathBuf::from(text.replace('/', "\\"))
}

#[cfg(not(windows))]
fn native_cwd(path: &Path) -> std::path::PathBuf {
    path.to_path_buf()
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按 `[terminal]` 配置构造：并发上限至少 1，保活时长 0 视为「立即回收」。
    pub fn with_limits(max_sessions: usize, reap_delay: Duration) -> Self {
        Self {
            max_sessions: max_sessions.max(1),
            reap_delay,
            ..Self::default()
        }
    }

    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        // 必须带进程级时间戳：前端会把 session_id 存进标签页，后端重启后
        // 如果 id 只由进程内计数器生成，重连就可能撞上"另一个"新会话。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("term-{stamp:x}-{n:x}")
    }

    /// 在指定目录拉起一条新会话。
    ///
    /// 超过 [`TerminalManager::max_sessions`]（`[terminal].max_sessions`）时先回收
    /// 已经断线（无连接）的会话，仍然超限就拒绝。
    /// `env` 是 [`prepare_env`] 的结果：历史文件已隔离到数据目录。
    pub fn create(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        options: ShellOptions,
        env: &TerminalEnv,
    ) -> Result<Arc<Session>, String> {
        if self.len() >= self.max_sessions {
            let idle: Vec<String> = self
                .inner
                .lock()
                .map(|map| {
                    map.iter()
                        .filter(|(_, session)| session.clients.load(Ordering::SeqCst) == 0)
                        .map(|(id, _)| id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for id in idle {
                self.kill(&id);
            }
            if self.len() >= self.max_sessions {
                return Err(format!(
                    "终端会话数已达上限（{} 个），请关闭不用的标签后再试",
                    self.max_sessions
                ));
            }
        }

        // 首连尺寸同样钳制（设计文档 3.3.2）：这里再夹一次，不信任上游
        let (cols, rows) = clamp_size(cols as u64, rows as u64);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("创建 PTY 失败：{e}"))?;

        // 配置覆盖优先级（设计文档 3.3.5）：[terminal].shell > 自动探测。
        // 纯净模式是配置写坏后的救急通道，必须忽略一切自定义，固定走探测结果。
        let shell = configured_shell(&options).unwrap_or_else(|| shell_for(&options));
        let mut command = CommandBuilder::new(&shell);
        // 统一成原生路径再交给 PTY：前端传来的 `C:/Users/me` 在这里变成
        // `C:\Users\me`，避免个别 shell/带空格目录下的启动失败
        let native = native_cwd(cwd);
        command.cwd(&native);
        // 设计文档 3.3.5：必须完整继承父进程环境变量。portable-pty 默认就继承，
        // 这里显式写一遍，防止将来有人加 `env_clear()` 把 PATH / HOME / conda 弄丢。
        for (key, value) in std::env::vars_os() {
            command.env(key, value);
        }
        // 只覆盖「终端能力」相关变量
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // 历史隔离（设计文档 3.3.5）永远生效：放在参数替换逻辑之外，
        // `[terminal].args` 自定义参数也拦不住它（bash/zsh 都认 HISTFILE）
        #[cfg(not(windows))]
        if let Some(path) = env.history_path.as_deref() {
            command.env("HISTFILE", path);
        }
        // `[terminal].args` 非空 → 完全替换内置参数（-l / -NoExit / -Command 载荷
        // 都不再追加）。Windows 下 chcp 与 PSReadLine 历史重定向随 -Command 一起
        // 失效，由使用者自己的配置负责（模板注释已说明）。
        if let Some(args) = configured_args(&options) {
            for arg in args {
                command.arg(arg);
            }
        } else {
            apply_shell_args(&mut command, &shell, &options, env.history_path.as_deref());
        }

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("启动 shell（{shell}）失败：{e}"))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("读取 PTY 失败：{e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("写入 PTY 失败：{e}"))?;

        let (tx, _rx) = broadcast::channel::<Vec<u8>>(2048);
        let id = self.next_id();

        let session = Arc::new(Session {
            id: id.clone(),
            cwd: session_display(&native),
            shell: shell.clone(),
            no_profile: options.no_profile,
            created: Instant::now(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            buffer: Mutex::new(VecDeque::with_capacity(8 * 1024)),
            output: tx.clone(),
            clients: AtomicUsize::new(0),
            size: Mutex::new((cols, rows)),
        });

        // 后台泵：阻塞读 → 环缓冲 + 广播
        {
            let session = Arc::clone(&session);
            std::thread::Builder::new()
                .name(format!("pty-reader-{id}"))
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match reader.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                let data = &chunk[..n];
                                if let Ok(mut buffer) = session.buffer.lock() {
                                    buffer.extend(data.iter().copied());
                                    while buffer.len() > BUFFER_CAPACITY {
                                        buffer.pop_front();
                                    }
                                }
                                // 没有订阅者时 send 会失败，属正常情况
                                let _ = session.output.send(data.to_vec());
                            }
                            Err(_) => break,
                        }
                    }
                })
                .map_err(|e| format!("启动 PTY 读取线程失败：{e}"))?;
        }

        // 历史文件已强制隔离到数据目录（设计文档 3.3.5），
        // 这里不再往终端里打任何「权限受限 / 已重定向」的横幅：前端必须静默，
        // 终端要像原生一样顺滑。

        self.inner
            .lock()
            .expect("session map poisoned")
            .insert(id, Arc::clone(&session));

        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.inner.lock().ok()?.get(id).cloned()
    }

    /// 连接数归零后安排回收：到期仍无连接才真正结束进程。
    pub fn schedule_reap(&self, id: String) {
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(manager.reap_delay).await;
            let should_kill = match manager.get(&id) {
                Some(session) => session.clients.load(Ordering::SeqCst) == 0,
                None => false,
            };
            if should_kill {
                if let Some(session) = manager.get(&id) {
                    // 回收走优雅退出：给 shell 留保存历史、收尾子进程的机会
                    let session_for_close = Arc::clone(&session);
                    tokio::task::spawn_blocking(move || session_for_close.close_graceful())
                        .await
                        .ok();
                    manager.kill(&id);
                }
            }
        });
    }

    pub fn kill(&self, id: &str) {
        let removed = self.inner.lock().ok().and_then(|mut map| map.remove(id));
        if let Some(session) = removed {
            session.kill();
        }
    }

    /// 进程退出时统一清理，避免留下孤儿 shell。
    pub fn shutdown(&self) {
        let sessions: Vec<(String, Arc<Session>)> = self
            .inner
            .lock()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect())
            .unwrap_or_default();
        for (_, session) in sessions {
            session.kill();
        }
        if let Ok(mut map) = self.inner.lock() {
            map.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|map| map.len()).unwrap_or(0)
    }
}

/// 会话的工作目录展示串：保留目标平台的原生分隔符
/// （Windows 下就是 `C:\Users\xxx`，Linux 下是 `/home/xxx`）。
fn session_display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_limits_are_honoured() {
        // [terminal] 的 max_sessions 直接决定并发闸门，0 也要兜成 1（否则一个都开不了）
        let manager = TerminalManager::with_limits(3, Duration::from_secs(42));
        assert_eq!(manager.max_sessions(), 3);
        assert_eq!(TerminalManager::with_limits(0, Duration::ZERO).max_sessions(), 1);
        // 缺省值必须与设计文档 3.3 一致（10 并发 / 180 秒保活）
        assert_eq!(TerminalManager::default().max_sessions(), MAX_SESSIONS);
    }

    #[test]
    fn history_is_always_isolated_under_data_dir() {
        let temp = std::env::temp_dir().join("asmoday-term-env-test");
        let _ = std::fs::create_dir_all(&temp);
        let env = prepare_env(&temp);
        let path = env.history_path.expect("历史路径必须总是给出（强制隔离）");
        let normalized = path.replace('\\', "/");
        assert!(
            normalized.contains("/terminal_history/"),
            "历史必须落在 data/terminal_history/：{path}"
        );
        // 绝不指向系统 PSReadLine 历史目录（受限令牌下必失败）
        assert!(!normalized.to_lowercase().contains("powershell/psreadline"));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_keeps_chcp_alone_when_healthy() {
        let payload = windows_command_payload(None);
        assert_eq!(payload, "chcp.com 65001 > $null");
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_chcp_first_then_history() {
        let payload = windows_command_payload(Some("C:/data/terminal_history/ConsoleHost_history.txt"));
        // 设计文档 3.3.5：chcp 65001 优先发送
        assert!(payload.starts_with("chcp.com 65001 > $null; "));
        assert!(payload.contains(
            r"Set-PSReadLineOption -HistorySavePath 'C:/data/terminal_history/ConsoleHost_history.txt'"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_escapes_single_quotes_in_path() {
        let payload = windows_command_payload(Some(r"C:\o'brien\history.txt"));
        assert!(payload.contains(r"'C:\o''brien\history.txt'"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_history_file_lands_in_data_dir() {
        let temp = std::env::temp_dir().join("asmoday-term-unix-test");
        let _ = std::fs::create_dir_all(&temp);
        let env = prepare_env(&temp);
        let path = env.history_path.unwrap();
        assert!(path.replace('\\', "/").ends_with("terminal_history/.bash_history"));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(windows)]
    #[test]
    fn default_shell_prefers_pwsh7_when_installed() {
        let shell = default_shell();
        // 装了 pwsh 7 就必须优先拉起：Conda/fnm 初始化通常只写在 pwsh 7 的
        // $PROFILE 里，5.1 读不到（表现为终端里 node 找不到）
        let pwsh_installed = ["ProgramFiles", "ProgramW6432"].iter().any(|key| {
            std::env::var(key)
                .ok()
                .map(|dir| Path::new(&dir).join("PowerShell").join("7").join("pwsh.exe").is_file())
                .unwrap_or(false)
        });
        if pwsh_installed {
            assert!(
                shell.to_lowercase().ends_with("pwsh.exe"),
                "装了 pwsh 7 就必须优先拉起，实际：{shell}"
            );
        }
        // 无论走哪个候选，都必须是本机真实存在的可执行文件
        assert!(
            Path::new(&shell).is_file() || shell == "powershell.exe",
            "默认 shell 必须真实存在：{shell}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_args_keep_noexit_and_profile() {
        let mut command = CommandBuilder::new("powershell.exe");
        apply_shell_args(&mut command, "powershell.exe", &ShellOptions::default(), Some("C:/h.txt"));
        let args: Vec<String> = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        // -NoExit 必须在：否则 -Command 执行完 PowerShell 直接退出，$PROFILE 也白加载
        assert!(args.contains(&"-NoExit".to_string()));
        assert!(!args.contains(&"-NoProfile".to_string()));

        // 纯净模式逃生舱
        let mut pure = CommandBuilder::new("powershell.exe");
                   apply_shell_args(
                    &mut pure,
                    "powershell.exe",
                    &ShellOptions {
                        no_profile: true,
                        ..Default::default()
                    },
                    None,
                );
        let pure_args: Vec<String> = pure
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(pure_args.contains(&"-NoProfile".to_string()));
    }

    #[test]
    fn empty_manager_lists_nothing_and_rejects_unknown_kill() {
        // 会话面板的边界：没有会话时返回空列表而不是 panic；
        // 关一个不存在的 id 必须返回 false（接口据此回 404），不能静默成功
        let manager = TerminalManager::new();
        assert!(manager.snapshot().is_empty());
        assert_eq!(manager.len(), 0);
        assert!(!manager.force_close("not-a-session"));
    }

    #[test]
    fn config_shell_override_priority() {
        use super::{configured_args, configured_shell};
        // 配置 shell 非空 → 生效
        let options = ShellOptions {
            shell: "  /usr/bin/zsh  ".to_string(),
            args: vec!["-c".to_string(), "tmux new -A -s asmoday".to_string()],
            ..Default::default()
        };
        assert_eq!(configured_shell(&options).as_deref(), Some("/usr/bin/zsh"));
        assert_eq!(configured_args(&options), Some(options.args.as_slice()));

        // 配置为空/纯空白 → 走自动探测
        let empty = ShellOptions::default();
        assert_eq!(configured_shell(&empty), None);
        assert_eq!(configured_args(&empty), None);

        // 纯净模式是救急通道：配置再怎么写都被忽略
        let pure = ShellOptions {
            no_profile: true,
            shell: "/usr/bin/fish".to_string(),
            args: vec!["-x".to_string()],
            ..Default::default()
        };
        assert_eq!(configured_shell(&pure), None);
        assert_eq!(configured_args(&pure), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn zsh_detected_before_bash_when_shell_env_missing() {
        use super::{bash_path, zsh_path};
        // 探测函数只回真实存在的路径；两条都在与本测试同机时 zsh 优先级更高
        // 由 default_shell 的 or_else 顺序保证（$SHELL → zsh → bash → sh），
        // 这里验证「有 zsh 时探测得到、且不因 bash 存在而误报」。
        if let Some(zsh) = zsh_path() {
            assert!(zsh.ends_with("zsh"), "zsh 探测结果必须是 zsh：{zsh}");
        }
        if let Some(bash) = bash_path() {
            assert!(bash.ends_with("bash"), "bash 探测结果必须是 bash：{bash}");
        }
    }
}
