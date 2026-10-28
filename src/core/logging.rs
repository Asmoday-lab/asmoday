//! 运行日志（设计文档 3.7：分级默认、多输出与防撑爆）。
//!
//! 策略一句话：**默认生产级克制，排障时分级开启，结构化可选**。
//!
//! - 三个输出通道，各自的级别独立控制（`[log]` 配置，支持热更新）：
//!   * 控制台：默认 `INFO` 及以上——终端里只出现关键信息；
//!   * 主文件 `data/logs/asmoday.log`：默认 `DEBUG` 及以上——事后排障有完整线索；
//!   * 错误文件 `data/logs/errors.log`：只收 `ERROR`，快速隔离与告警。
//! - HTTP 访问日志走独立通道（`api_level`，模块名 `api::access`）：
//!   只记方法 / 路径 / 状态码 / 耗时 / 来源 IP，**绝不记录请求体与查询串**。
//! - 只有 `DEBUG` 级别的日志才附带源代码 `文件名:行号`（`log_debug!` 宏展开），
//!   INFO 及以上不掺开发期信息。
//! - `format = "text"`（默认，人读）| `"json"`（机器解析，每行一个 JSON 对象）。
//! - 文件日志强制 UTC（RFC 3339）且无颜色代码；控制台用本地时间、TTY 下带颜色。
//! - 轮转：单文件 10 MiB × 5 备份，两个文件 sink 各自独立轮转。
//! - **消息一律英文**：跨平台无乱码、便于检索；用户数据（路径/文件名）原样保留。
//!
//! 不引入日志框架：本项目日志量很小，自带按大小轮转比按天轮转更符合
//! 「单文件最大 10MB」这条硬指标（`tracing-appender` 只支持按时间轮转）。
//!
//! 调用一律用 [`log_info!`] / [`log_warn!`] / [`log_error!`] / [`log_debug!`]
//! 四个宏：只有宏才能在调用点展开 `module_path!()` 与 `file!()/line!()`。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::config::LogConfig;

/// 单文件上限
const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 备份数量上限
const MAX_BACKUPS: usize = 5;

struct Sink {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

/// 日志级别。`Off` 只能作为阈值（关闭该通道），不作为记录级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    #[default]
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl Level {
    pub fn parse(raw: &str) -> Option<Level> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            "off" | "none" => Some(Level::Off),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
            Level::Off => "OFF",
        }
    }
}

/// 记录级别是否越过阈值。`Off` 阈值关闭整个通道。
fn passes(record: Level, threshold: Level) -> bool {
    threshold != Level::Off && record >= threshold
}

/// 运行期设置（`[log]` 配置的解析结果），支持热更新。
#[derive(Debug, Clone, Copy)]
struct Settings {
    console: Level,
    file: Level,
    api: Level,
    json: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // 默认值即设计文档 3.7 的「生产级克制」：控制台 INFO、文件 DEBUG、
        // API 通道 INFO、人读文本格式。
        Settings {
            console: Level::Info,
            file: Level::Debug,
            api: Level::Info,
            json: false,
        }
    }
}

impl Settings {
    fn from_config(config: &LogConfig) -> Settings {
        Settings {
            console: Level::parse(&config.console_level).unwrap_or_default(),
            file: Level::parse(&config.file_level).unwrap_or_default(),
            api: Level::parse(&config.api_level).unwrap_or_default(),
            json: config.format.eq_ignore_ascii_case("json"),
        }
    }
}

static SETTINGS: OnceLock<Mutex<Settings>> = OnceLock::new();
static MAIN_SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
static ERROR_SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
/// 控制台颜色开关：TTY 且非 JSON 输出时才着色。初始化时探测一次。
static COLOR: AtomicBool = AtomicBool::new(false);
/// 服务器本地时区相对 UTC 的偏移（秒），探测失败按 0 处理（即按 UTC 输出）
static LOCAL_OFFSET: OnceLock<i64> = OnceLock::new();

fn settings() -> Settings {
    SETTINGS
        .get_or_init(|| Mutex::new(Settings::default()))
        .lock()
        .map(|guard| *guard)
        .unwrap_or_default()
}

/// 本地时区偏移（秒，惰性探测一次）。
///
/// 惰性而不是只在 `init` 里算：`--check-config` 这类不碰数据目录的命令也要打日志，
/// 否则控制台时间会莫名其妙变成 UTC。
fn local_offset() -> i64 {
    *LOCAL_OFFSET.get_or_init(|| tz::local_offset_secs().unwrap_or(0))
}

/// 应用 `[log]` 配置（启动与热更新共用）。解析失败的字段保持默认值
/// ——`Config::validate` 已在上游拦截非法值，这里是兜底。
pub fn apply(config: &LogConfig) {
    let next = Settings::from_config(config);
    let lock = SETTINGS.get_or_init(|| Mutex::new(Settings::default()));
    if let Ok(mut guard) = lock.lock() {
        *guard = next;
    }
}

/// 初始化日志（幂等）。目录不可写时静默退回「仅控制台」。
pub fn init(data_dir: &Path) {
    let _ = local_offset();

    let dir = data_dir.join("logs");
    let _ = fs::create_dir_all(&dir);

    let main_path = dir.join("asmoday.log");
    let written = fs::metadata(&main_path).map(|m| m.len()).unwrap_or(0);
    let _ = MAIN_SINK.set(Mutex::new(Some(Sink {
        path: main_path,
        file: open_append(&dir.join("asmoday.log")).ok(),
        written,
    })));

    let error_path = dir.join("errors.log");
    let written = fs::metadata(&error_path).map(|m| m.len()).unwrap_or(0);
    let _ = ERROR_SINK.set(Mutex::new(Some(Sink {
        path: error_path,
        file: open_append(&dir.join("errors.log")).ok(),
        written,
    })));

    // 控制台颜色：仅 TTY；Windows 经典 conhost 需要先开 VT 处理
    #[cfg(windows)]
    enable_windows_vt();
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    COLOR.store(tty, Ordering::Relaxed);
}

/// Windows 经典 conhost 默认关闭 ANSI 转义，需显式打开
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`；Windows Terminal / 重定向场景不受影响。
#[cfg(windows)]
fn enable_windows_vt() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return;
        }
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// 轮转：asmoday.log → asmoday.log.1 → … → asmoday.log.5（最老的丢弃）
fn rotate(sink: &mut Sink) {
    sink.file = None;
    let base = &sink.path;
    let oldest = base.with_extension(format!("log.{MAX_BACKUPS}"));
    let _ = fs::remove_file(oldest);
    for index in (1..MAX_BACKUPS).rev() {
        let from = base.with_extension(format!("log.{index}"));
        let to = base.with_extension(format!("log.{}", index + 1));
        if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let first = base.with_extension("log.1");
    let _ = fs::rename(base, first);
    sink.file = open_append(base).ok();
    sink.written = 0;
}

/// 向单个文件 sink 写一行（含按大小轮转）。sink 未初始化或文件不可写时静默丢弃。
fn write_to(sink_lock: &OnceLock<Mutex<Option<Sink>>>, line: &str) {
    let Some(lock) = sink_lock.get() else { return };
    let Ok(mut guard) = lock.lock() else { return };
    let Some(sink) = guard.as_mut() else { return };
    if sink.written + line.len() as u64 > MAX_BYTES {
        rotate(sink);
    }
    if let Some(file) = sink.file.as_mut() {
        if file.write_all(line.as_bytes()).is_ok() {
            sink.written += line.len() as u64;
        }
    }
}

/// 模块名去掉 crate 前缀，日志里只留 `core::fs_ops` 这一层
fn short_module(module: &str) -> &str {
    module
        .strip_prefix("asmoday::")
        .or_else(|| module.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::")))
        .unwrap_or(module)
}

/// 控制台文本行（本地时间，TTY 下级别着色）
fn render_console_text(
    level: Level,
    module: &str,
    message: &str,
    source: Option<&str>,
    ts: &str,
    pid: u32,
    color: bool,
) -> String {
    let tag = if color {
        let code = match level {
            Level::Error => "\x1b[31m", // red
            Level::Warn => "\x1b[33m",  // yellow
            Level::Debug => "\x1b[90m", // bright black
            _ => "\x1b[32m",            // green
        };
        format!("{code}{}\x1b[0m", level.as_str())
    } else {
        level.as_str().to_string()
    };
    let suffix = source.map(|s| format!(" ({s})")).unwrap_or_default();
    format!("{ts} [{tag}] [{pid}] {module}: {message}{suffix}\n")
}

/// 文件文本行（UTC RFC 3339，无颜色）
fn render_file_text(
    level: Level,
    module: &str,
    message: &str,
    source: Option<&str>,
    ts: &str,
    pid: u32,
) -> String {
    let suffix = source.map(|s| format!(" ({s})")).unwrap_or_default();
    format!("{ts} [{}] [{pid}] {module}: {message}{suffix}\n", level.as_str())
}

/// JSON 行（结构化输出；ts 由调用方决定本地 / UTC 口径）
fn render_json(
    level: Level,
    module: &str,
    message: &str,
    source: Option<&str>,
    ts: &str,
    pid: u32,
) -> String {
    let mut object = serde_json::json!({
        "ts": ts,
        "level": level.as_str(),
        "pid": pid,
        "module": module,
        "message": message,
    });
    if let (Some(source), Some(map)) = (source, object.as_object_mut()) {
        map.insert("source".to_string(), serde_json::Value::from(source));
    }
    format!("{object}\n")
}

/// 核心发射：按通道级别过滤 → 控制台 / 主文件 / 错误文件三路输出。
fn emit(level: Level, module: &str, source: Option<&str>, message: &str) {
    let module = short_module(module);
    let pid = std::process::id();
    let now = SystemTime::now();
    let settings = settings();
    let color = COLOR.load(Ordering::Relaxed) && !settings.json;

    // 控制台：本地时间；WARN/ERROR 走 stderr
    if passes(level, settings.console) {
        let ts = format_local(now, local_offset());
        let line = if settings.json {
            render_json(level, module, message, source, &ts, pid)
        } else {
            render_console_text(level, module, message, source, &ts, pid, color)
        };
        if level >= Level::Warn {
            eprint!("{line}");
        } else {
            print!("{line}");
        }
    }

    // 主文件：UTC、无颜色
    if passes(level, settings.file) {
        let ts = iso8601_millis(now);
        let line = if settings.json {
            render_json(level, module, message, source, &ts, pid)
        } else {
            render_file_text(level, module, message, source, &ts, pid)
        };
        write_to(&MAIN_SINK, &line);
    }

    // 错误文件：只收 ERROR
    if level == Level::Error {
        let ts = iso8601_millis(now);
        let line = if settings.json {
            render_json(level, module, message, source, &ts, pid)
        } else {
            render_file_text(level, module, message, source, &ts, pid)
        };
        write_to(&ERROR_SINK, &line);
    }
}

/// HTTP 访问日志独立通道（设计文档 3.7：参考 FileBrowser 的 apiLevels）。
///
/// 记录级别固定 INFO；`api_level` 是通道闸门——`debug`/`info` 放行，
/// `warn`/`error`/`off` 整条关闭。放行后仍受控制台 / 文件各自的级别过滤。
pub fn access(module: &str, message: &str) {
    let settings = settings();
    if !passes(Level::Info, settings.api) {
        return;
    }
    emit(Level::Info, module, None, message);
}

/// 停机前强制 flush（优雅停机时调用，避免最后几行留在缓冲区）
pub fn flush() {
    for sink_lock in [&MAIN_SINK, &ERROR_SINK] {
        let Some(lock) = sink_lock.get() else { continue };
        if let Ok(mut guard) = lock.lock() {
            if let Some(sink) = guard.as_mut() {
                if let Some(file) = sink.file.as_mut() {
                    let _ = file.flush();
                }
            }
        }
    }
}

pub fn info(module: &str, message: &str) {
    emit(Level::Info, module, None, message);
}

/// DEBUG 记录：`source` 是调用点的 `文件名:行号`（由 [`log_debug!`] 宏展开）。
/// 这是唯一会携带开发期位置信息的级别（设计文档 3.7）。
pub fn debug(module: &str, source: &str, message: &str) {
    emit(Level::Debug, module, Some(source), message);
}

pub fn warn(module: &str, message: &str) {
    emit(Level::Warn, module, None, message);
}

pub fn error(module: &str, message: &str) {
    emit(Level::Error, module, None, message);
}

// ---------------------------------------------------------------------------
// 时间格式化（不引时间库：只需要到毫秒的 UTC / 本地时间两种形态）
// ---------------------------------------------------------------------------

/// 当前时刻的拆分：UTC 秒 + 毫秒
fn split_now(now: SystemTime) -> (i64, u32) {
    match now.duration_since(UNIX_EPOCH) {
        Ok(delta) => (delta.as_secs() as i64, delta.subsec_millis()),
        Err(_) => (0, 0),
    }
}

/// RFC 3339 UTC，带毫秒：`2026-10-27T15:04:05.123Z`
fn iso8601_millis(now: SystemTime) -> String {
    let (secs, millis) = split_now(now);
    let (year, month, day, hour, minute, second) = civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// 服务器本地时间：`2026-10-27 23:04:05.123`
fn format_local(now: SystemTime, offset_secs: i64) -> String {
    let (secs, millis) = split_now(now);
    let (year, month, day, hour, minute, second) = civil(secs + offset_secs);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03}")
}

/// Unix 秒 → (年, 月, 日, 时, 分, 秒)，UTC 口径
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Howard Hinnant 的 civil_from_days（1970-01-01 起的天数 → 年月日）
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 本地时区偏移探测。探不到就按 UTC 输出——只影响控制台可读性，不影响正确性。
#[cfg(unix)]
mod tz {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        // glibc 与 musl 在 64 位目标上都是「long（8 字节）+ 指针」
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }

    unsafe extern "C" {
        fn tzset();
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }

    pub fn local_offset_secs() -> Option<i64> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        unsafe {
            tzset();
            let mut out: Tm = std::mem::zeroed();
            if localtime_r(&secs, &mut out).is_null() {
                return None;
            }
            Some(out.tm_gmtoff)
        }
    }
}

#[cfg(windows)]
mod tz {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    unsafe extern "system" {
        fn GetLocalTime(out: *mut SystemTime);
        fn GetSystemTime(out: *mut SystemTime);
    }

    fn to_secs(t: &SystemTime) -> i64 {
        // 只用于求「本地 - UTC」的差值，历法换算用同一套 civil 变换即可，
        // 这里直接借道 crate 内的 UTC 反算：先用 UTC 秒推出年月日再做差太绕，
        // 改用「同一天内秒数 + 儒略日差」两段计算。
        jdn(t.year as i64, t.month as i64, t.day as i64) * 86_400
            + t.hour as i64 * 3600
            + t.minute as i64 * 60
            + t.second as i64
    }

    /// 公历 → 儒略日号（忽略时区，仅用于做差）
    fn jdn(y: i64, m: i64, d: i64) -> i64 {
        let a = (14 - m) / 12;
        let y2 = y + 4800 - a;
        let m2 = m + 12 * a - 3;
        d + (153 * m2 + 2) / 5 + 365 * y2 + y2 / 4 - y2 / 100 + y2 / 400 - 32_045
    }

    pub fn local_offset_secs() -> Option<i64> {
        unsafe {
            let mut utc = SystemTime::default();
            let mut local = SystemTime::default();
            GetSystemTime(&mut utc);
            GetLocalTime(&mut local);
            let diff = to_secs(&local) - to_secs(&utc);
            // 合法时区偏移不会超过 ±14 小时；超出说明两次调用跨了秒/分边界，放弃
            if diff.abs() <= 14 * 3600 {
                Some(diff)
            } else {
                None
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod tz {
    pub fn local_offset_secs() -> Option<i64> {
        None
    }
}

/// INFO 日志。必须用宏：只有宏能在调用点展开真实的 `module_path!()`。
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::core::logging::info(module_path!(), &format!($($arg)*))
    };
}

/// DEBUG 日志：唯一附带源代码 `文件名:行号` 的级别（设计文档 3.7）。
/// 仅在把 `[log] file_level` / `console_level` 调到 `debug` 时可见。
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::core::logging::debug(
            module_path!(),
            concat!(file!(), ":", line!()),
            &format!($($arg)*),
        )
    };
}

/// HTTP 访问日志：走 `[log] api_level` 独立通道闸门（设计文档 3.7），
/// 不受 console_level / file_level 直接控制，放行后再按各通道级别过滤。
#[macro_export]
macro_rules! log_access {
    ($($arg:tt)*) => {
        $crate::core::logging::access(module_path!(), &format!($($arg)*))
    };
}

/// WARN 日志
#[macro_export]
macro_rules! log_warn {    ($($arg:tt)*) => {
        $crate::core::logging::warn(module_path!(), &format!($($arg)*))
    };
}

/// ERROR 日志
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::core::logging::error(module_path!(), &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch() {
        assert_eq!(iso8601_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_leap_day() {
        // 2024-02-29T12:34:56Z
        assert_eq!(
            iso8601_millis(UNIX_EPOCH + std::time::Duration::from_millis(1_709_210_096_789)),
            "2024-02-29T12:34:56.789Z"
        );
    }

    #[test]
    fn local_format_applies_offset() {
        // UTC 00:00 + 8h = 本地 08:00
        let text = format_local(UNIX_EPOCH, 8 * 3600);
        assert_eq!(text, "1970-01-01 08:00:00.000");
    }

    #[test]
    fn module_prefix_is_trimmed() {
        assert_eq!(short_module("asmoday::core::fs_ops"), "core::fs_ops");
        assert_eq!(short_module("core::fs_ops"), "core::fs_ops");
    }

    #[test]
    fn level_parse_and_order() {
        assert_eq!(Level::parse("Info"), Some(Level::Info));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("none"), Some(Level::Off));
        assert_eq!(Level::parse("verbose"), None);
        assert!(Level::Debug < Level::Info);
        assert!(Level::Warn < Level::Error);
    }

    #[test]
    fn gate_off_closes_channel() {
        assert!(!passes(Level::Error, Level::Off));
        assert!(passes(Level::Debug, Level::Debug));
        assert!(!passes(Level::Debug, Level::Info));
        assert!(passes(Level::Warn, Level::Info));
    }

    #[test]
    fn console_text_colors_and_source() {
        let plain = render_console_text(
            Level::Info,
            "core::fs_ops",
            "copy done",
            None,
            "2026-10-28 23:00:00.000",
            42,
            false,
        );
        assert_eq!(plain, "2026-10-28 23:00:00.000 [INFO] [42] core::fs_ops: copy done\n");

        let colored =
            render_console_text(Level::Error, "a::b", "boom", None, "t", 1, true);
        assert!(colored.contains("\x1b[31mERROR\x1b[0m"), "ERROR 应为红色：{colored:?}");

        let with_source = render_console_text(
            Level::Debug,
            "a::b",
            "detail",
            Some("src/core/x.rs:12"),
            "t",
            1,
            false,
        );
        assert!(with_source.contains("detail (src/core/x.rs:12)"));
    }

    #[test]
    fn file_text_is_color_free_and_utc() {
        let line = render_file_text(
            Level::Warn,
            "core::config",
            "watching config file",
            None,
            "2026-10-28T15:00:00.000Z",
            7,
        );
        assert_eq!(
            line,
            "2026-10-28T15:00:00.000Z [WARN] [7] core::config: watching config file\n"
        );
        assert!(!line.contains('\x1b'), "文件日志不得包含颜色代码");
    }

    #[test]
    fn json_line_shape() {
        let plain = render_json(
            Level::Info,
            "core::config",
            "listening on 0.0.0.0:4242",
            None,
            "2026-10-28T15:00:00.000Z",
            9,
        );
        let value: serde_json::Value = serde_json::from_str(plain.trim()).unwrap();
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["module"], "core::config");
        assert_eq!(value["pid"], 9);
        assert!(value.get("source").is_none(), "非 DEBUG 不得携带 source");

        let debug_line = render_json(
            Level::Debug,
            "core::fs_ops",
            "open",
            Some("src/core/fs_ops.rs:99"),
            "t",
            1,
        );
        let value: serde_json::Value = serde_json::from_str(debug_line.trim()).unwrap();
        assert_eq!(value["source"], "src/core/fs_ops.rs:99");
    }

    #[test]
    fn settings_from_config_defaults() {
        let config = LogConfig::default();
        let settings = Settings::from_config(&config);
        assert_eq!(settings.console, Level::Info);
        assert_eq!(settings.file, Level::Debug);
        assert_eq!(settings.api, Level::Info);
        assert!(!settings.json);
    }
}
