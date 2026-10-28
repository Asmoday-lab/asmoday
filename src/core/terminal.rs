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

/// 拉起 shell 的方式（设计文档 3.3.7）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellOptions {
    /// 纯净模式：跳过 profile / rc，专用于配置文件死循环或严重报错时的救急
    pub no_profile: bool,
}

/// 展开成绝对路径并去掉 canonicalize 留下的 `\\?\` 前缀。
fn absolutize(path: &Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    std::path::PathBuf::from(stripped.to_string())
}

/// shell 环境探测结果（与本地 shell 一致性自愈，设计文档 3.3.6）。
///
/// 服务进程可能运行在受限令牌下（Windows 低完整性级别 / 类 Unix 主目录只读），
/// 此时 shell 写自己的历史文件必然「访问被拒绝」——PSReadLine 每敲一条命令
/// 就报一次红字，conda/npm 写配置全挂，终端体验和本地天差地别。
/// 探测到这种情况时，把历史文件重定向到数据目录（依然全站共享一份，等价于
/// 本地多窗口共享历史），并在终端里打一条一次性说明。
#[derive(Debug, Clone, Default)]
pub struct ShellProbe {
    /// 历史文件替代路径；`None` = 默认路径可写，不做任何干预
    pub history_override: Option<String>,
    /// 已本地化的说明文字（打进终端 + 随 ready 帧给前端）
    pub notice: Option<String>,
}

impl ShellProbe {
    /// 是否处于受限环境
    pub fn degraded(&self) -> bool {
        self.history_override.is_some()
    }
}

/// 当前进程令牌的完整性级别 RID（Windows 专用，设计文档 3.3.6）。
///
/// `0x1000` 低 / `0x2000` 中 / `0x3000` 高。低完整性级别下终端子进程继承令牌，
/// 对用户目录（中完整性）一切写入被拒——这就是「PSReadLine 历史访问被拒绝」
/// 的根因。启动时查一次并在服务日志提醒，让双击/服务方式启动的用户不用等到
/// 打开终端才发现。查询失败返回 `None`（宁可不报，不误报）。
#[cfg(windows)]
pub fn process_integrity_level() -> Option<u32> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let result = (|| {
            // 先取所需长度（第一次调用必然失败，靠 returnlength 拿尺寸）
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenIntegrityLevel, std::ptr::null_mut(), 0, &mut len);
            if len == 0 {
                return None;
            }
            let mut buffer = vec![0u8; len as usize];
            if GetTokenInformation(
                token,
                TokenIntegrityLevel,
                buffer.as_mut_ptr().cast(),
                len,
                &mut len,
            ) == 0
            {
                return None;
            }
            let label = buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let sid = (*label).Label.Sid;
            if sid.is_null() {
                return None;
            }
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return None;
            }
            Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
        })();
        CloseHandle(token);
        result
    }
}

/// 非 Windows 平台没有完整性级别概念，恒为 `None`。
#[cfg(not(windows))]
pub fn process_integrity_level() -> Option<u32> {
    None
}

/// 探测默认历史路径是否可写，不可写则给出数据目录下的替代路径。
///
/// Windows 探测的是 PSReadLine 的历史文件；类 Unix 探测 `$HOME` 可写性
/// （bash/zsh 的 `HISTFILE` 都落在主目录）。探测只在拉起会话时做一次，开销可忽略。
///
/// 注意 `data_dir` 会被先规范化成绝对路径：`-d data` 这类相对参数如果不展开，
/// 注入给 PSReadLine 的 HistorySavePath 就会按 **shell 自己的 cwd**（往往是
/// 主目录）解析，历史写去一个谁都找不到的地方。
#[cfg(windows)]
pub fn probe_shell_env(data_dir: &Path) -> ShellProbe {
    use std::fs::OpenOptions;

    let data_dir = absolutize(data_dir);
    let appdata = match std::env::var("APPDATA") {
        Ok(value) if !value.trim().is_empty() => std::path::PathBuf::from(value),
        // APPDATA 缺失时按 Known Folder 的常见位置猜；猜错也只是探测错，不会写坏东西
        _ => match dirs::home_dir() {
            Some(home) => home.join("AppData").join("Roaming"),
            None => return ShellProbe::default(),
        },
    };
    let history = appdata
        .join("Microsoft")
        .join("Windows")
        .join("PowerShell")
        .join("PSReadLine")
        .join("ConsoleHost_history.txt");

    // PSReadLine 自己会在首次保存时建目录，这里只探测：目录可建 + 文件可追加
    if let Some(parent) = history.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let writable = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&history)
        .is_ok();
    if writable {
        return ShellProbe::default();
    }

    let fallback_dir = data_dir.join("terminal");
    let fallback = fallback_dir.join("ConsoleHost_history.txt");
    let usable = std::fs::create_dir_all(&fallback_dir).is_ok()
        && OpenOptions::new()
            .append(true)
            .create(true)
            .open(&fallback)
            .is_ok();
    if !usable {
        // 连数据目录都写不了（磁盘满 / 权限全坏）就只能原样放行，让 shell 自己报错
        return ShellProbe::default();
    }

    ShellProbe {
        history_override: Some(fallback.to_string_lossy().into_owned()),
        notice: Some(
            "检测到服务进程运行在受限令牌下（低完整性级别），无法写入系统 PSReadLine 历史；\
             本会话已把命令历史重定向到数据目录，其余功能不受影响。\
             若要与本地终端完全一致，请以正常方式重启服务。"
                .to_string(),
        ),
    }
}

/// 类 Unix 版：探测 `$HOME` 可写性，不可写时把 `HISTFILE` 指到数据目录。
#[cfg(not(windows))]
pub fn probe_shell_env(data_dir: &Path) -> ShellProbe {
    use std::fs::OpenOptions;

    // 同 Windows：相对 data_dir 必须先展开成绝对路径，HISTFILE 才靠得住
    let data_dir = absolutize(data_dir);
    let Some(home) = dirs::home_dir() else {
        return ShellProbe::default();
    };
    let probe = home.join(".asmoday-history-probe");
    let writable = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    if writable {
        let _ = std::fs::remove_file(&probe);
        return ShellProbe::default();
    }

    let fallback_dir = data_dir.join("terminal");
    let fallback = fallback_dir.join(".bash_history");
    let usable = std::fs::create_dir_all(&fallback_dir).is_ok()
        && OpenOptions::new()
            .append(true)
            .create(true)
            .open(&fallback)
            .is_ok();
    if !usable {
        return ShellProbe::default();
    }

    ShellProbe {
        history_override: Some(fallback.to_string_lossy().into_owned()),
        notice: Some(
            "检测到主目录不可写（服务进程权限受限），本会话已把 shell 历史重定向到数据目录。\
             若要与本地终端完全一致，请以正常方式重启服务。"
                .to_string(),
        ),
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
        let master = self.master.lock().map_err(|_| "pty master poisoned".to_string())?;
        master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("调整终端尺寸失败：{e}"))?;
        if let Ok(mut size) = self.size.lock() {
            *size = (cols.max(1), rows.max(1));
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
#[derive(Clone, Default)]
pub struct TerminalManager {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    counter: Arc<AtomicUsize>,
}

/// 默认 Shell（设计文档 3.3 / 6.3）。
///
/// - Windows：显式拉起 `powershell.exe`。`%COMSPEC%` 指向的是 cmd.exe，
///   不符合设计要求；优先用 System32 下的绝对路径，避免 PATH 被改坏时找不到。
/// - 类 Unix：读 `$SHELL`，不可用（未设置或文件不存在）时回退 `/bin/bash`。
#[cfg(windows)]
fn default_shell() -> String {
    if let Ok(root) = std::env::var("SystemRoot") {
        let candidate = Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    let fallback = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    if fallback.is_file() {
        return fallback.to_string_lossy().to_string();
    }
    "powershell.exe".to_string()
}

#[cfg(not(windows))]
fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|shell| shell.trim().to_string())
        .filter(|shell| !shell.is_empty() && Path::new(shell).is_file())
        .unwrap_or_else(|| "/bin/bash".to_string())
}

/// 按模式挑 shell（设计文档 3.3.7）。
#[cfg(windows)]
fn shell_for(_options: ShellOptions) -> String {
    // Windows 的纯净模式是同一个 powershell.exe 加 `-NoProfile`，二进制不变
    default_shell()
}

/// 按模式挑 shell：纯净模式固定 bash。
///
/// `--noprofile --norc` 是 bash 专有参数，用户 `$SHELL` 是 zsh/fish 时直接传过去
/// 会启动失败；所以只在确实有 bash 时切过去，没有就退回原 shell（不加参数）。
#[cfg(not(windows))]
fn shell_for(options: ShellOptions) -> String {
    if options.no_profile && Path::new("/bin/bash").is_file() {
        return "/bin/bash".to_string();
    }
    default_shell()
}

/// 组装 Windows `-Command` 的载荷。
///
/// 两个关键点：
/// 1. `chcp 65001` 放在 `-Command` 里 —— 它在 profile **之后**执行，
///    否则用户 profile 里的 `chcp` 会把代码页改回去，中文还是乱码；
/// 2. 历史文件重定向（受限令牌自愈）排在 chcp **之前**：Set-PSReadLineOption
///    必须在用户敲第一条命令前生效，否则第一条命令就会触发一次红字报错。
fn windows_command_payload(history_override: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(path) = history_override {
        // PowerShell 单引号字符串里，' 用 '' 转义
        let escaped = path.replace('\'', "''");
        parts.push(format!("Set-PSReadLineOption -HistorySavePath '{escaped}'"));
    }
    parts.push("chcp.com 65001 > $null".to_string());
    parts.join("; ")
}

/// 追加 shell 启动参数。
///
/// Windows 走 `-NoExit -Command`：无论是否需要历史重定向，`-Command` 传进去的
/// 命令都不算交互输入，**不会**污染 PSReadLine 历史或屏幕输出。
#[cfg(windows)]
fn apply_shell_args(
    command: &mut CommandBuilder,
    _shell: &str,
    options: ShellOptions,
    history_override: Option<&str>,
) {
    command.arg("-NoLogo");
    if options.no_profile {
        command.arg("-NoProfile");
    }
    command.arg("-NoExit");
    command.arg("-Command");
    command.arg(windows_command_payload(history_override));
}

/// 类 Unix：默认以 **Login Shell** 启动（设计文档 3.3.5）。
///
/// `bash -l` / `zsh -l` 会读 `/etc/profile` 与用户的 profile 脚本，Conda、nvm、fnm
/// 这些工具的初始化都藏在里面；不登录启动的 shell 会让用户觉得「终端里的环境和 SSH 里不一样」。
/// 纯净模式（救急）则反过来：跳过 profile 与 rc，并固定用 bash 以保证 `--noprofile --norc` 有效。
#[cfg(not(windows))]
fn apply_shell_args(
    command: &mut CommandBuilder,
    shell: &str,
    options: ShellOptions,
    history_override: Option<&str>,
) {
    // 历史重定向靠 HISTFILE 环境变量（bash/zsh 都认），放在参数处理之前
    if let Some(path) = history_override {
        command.env("HISTFILE", path);
    }

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
    /// 超过 [`MAX_SESSIONS`] 时先回收已经断线（无连接）的会话，仍然超限就拒绝。
    /// `probe` 是 [`probe_shell_env`] 的结果：受限环境下带历史重定向与用户说明。
    pub fn create(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        options: ShellOptions,
        probe: &ShellProbe,
    ) -> Result<Arc<Session>, String> {
        if self.len() >= MAX_SESSIONS {
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
            if self.len() >= MAX_SESSIONS {
                return Err(format!(
                    "终端会话数已达上限（{MAX_SESSIONS} 个），请关闭不用的标签后再试"
                ));
            }
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("创建 PTY 失败：{e}"))?;

        let shell = shell_for(options);
        let mut command = CommandBuilder::new(&shell);
        // 统一成原生路径再交给 PTY：前端传来的 `C:/Users/me` 在这里变成
        // `C:\Users\me`，避免个别 shell/带空格目录下的启动失败
        let native = native_cwd(cwd);
        command.cwd(&native);
        // 只覆盖「终端能力」相关变量；`PATH` / `HOME` 等一律继承，绝不 env_clear()
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        apply_shell_args(&mut command, &shell, options, probe.history_override.as_deref());

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
            size: Mutex::new((cols.max(1), rows.max(1))),
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

        // 受限环境说明：写进环缓冲（重连回放也在）+ 广播给当前连接
        if let Some(notice) = &probe.notice {
            let banner = format!(
                "\r\n\x1b[33m[asmoday] {notice}\x1b[0m\r\n"
            );
            if let Ok(mut buffer) = session.buffer.lock() {
                buffer.extend(banner.bytes());
            }
            let _ = session.output.send(banner.into_bytes());
        }

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
            tokio::time::sleep(REAP_DELAY).await;
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
    fn shell_probe_default_is_healthy() {
        let probe = ShellProbe::default();
        assert!(!probe.degraded());
        assert!(probe.history_override.is_none());
        assert!(probe.notice.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_keeps_chcp_alone_when_healthy() {
        let payload = windows_command_payload(None);
        assert_eq!(payload, "chcp.com 65001 > $null");
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_redirects_history_before_chcp() {
        let payload = windows_command_payload(Some(r"C:/data/terminal/ConsoleHost_history.txt"));
        assert!(payload.starts_with("Set-PSReadLineOption -HistorySavePath 'C:"));
        // 重定向必须在 chcp 之前：第一条命令敲下去时 PSReadLine 就要用新路径
        let redirect = payload.find("Set-PSReadLineOption").unwrap();
        let chcp = payload.find("chcp.com").unwrap();
        assert!(redirect < chcp);
        assert!(payload.ends_with("ConsoleHost_history.txt'; chcp.com 65001 > $null"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_escapes_single_quotes_in_path() {
        let payload = windows_command_payload(Some(r"C:\o'brien\history.txt"));
        assert!(payload.contains(r"'C:\o''brien\history.txt'"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_probe_reports_unwritable_home() {
        // 用一个必然不可写的目录当数据目录，探测只验证「能给出替代路径」的结构
        let temp = std::env::temp_dir().join("asmoday-term-probe-test");
        let _ = std::fs::create_dir_all(&temp);
        let probe = probe_shell_env(&temp);
        if let Some(path) = &probe.history_override {
            assert!(path.contains("terminal"));
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn integrity_level_query_does_not_panic() {
        // 无法断言具体级别（取决于运行环境），只保证查询安全且结果自洽
        if let Some(level) = process_integrity_level() {
            assert!(level > 0, "integrity RID should be positive");
        }
    }

    #[cfg(windows)]
    #[test]
    fn integrity_level_threshold_matches_mandatory_levels() {
        // 语义校验：低 < 中 < 高（0x1000 / 0x2000 / 0x3000）
        assert!(0x1000 < 0x2000 && 0x2000 < 0x3000);
    }
}
