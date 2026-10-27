//! PTY 会话池（设计文档 3.3）。
//!
//! - 每个会话是一条真实的系统 shell（Windows 走 ConPTY），不是模拟终端；
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
/// 环形缓冲区容量
const BUFFER_CAPACITY: usize = 512 * 1024;

/// 一条 PTY 会话。
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub shell: String,
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
    pub fn create(&self, cwd: &Path, cols: u16, rows: u16) -> Result<Arc<Session>, String> {
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

        let shell = default_shell();
        let mut command = CommandBuilder::new(&shell);
        // 统一成原生路径再交给 PTY：前端传来的 `C:/Users/me` 在这里变成
        // `C:\Users\me`，避免个别 shell/带空格目录下的启动失败
        let native = native_cwd(cwd);
        command.cwd(&native);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

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
                manager.kill(&id);
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
