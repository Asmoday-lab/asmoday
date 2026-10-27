//! 运行日志（设计文档 3.7.1：日志防撑爆 + 输出规范）。
//!
//! - 落盘位置：`data/logs/asmoday.log`；
//! - 单个文件写满 10 MiB 立刻轮转，历史文件依次改名，最多保留 5 个备份；
//! - 同时输出到控制台，容器里 `docker logs` 依然能看到；
//! - **消息一律英文**：跨平台无乱码、便于检索；用户数据（路径/文件名）原样保留；
//! - 控制台：服务器本地时区 `YYYY-MM-DD HH:mm:ss.SSS [LEVEL] [PID] 模块: 消息`；
//! - 文件：RFC 3339 UTC `YYYY-MM-DDTHH:MM:SS.SSSZ [LEVEL] [PID] 模块: 消息`。
//!
//! 不引入额外日志框架：本项目日志量很小，自带的按大小轮转比按天轮转更符合
//! 「单文件最大 10MB」这条硬指标（`tracing-appender` 只支持按时间轮转）。
//!
//! 调用一律用 [`log_info!`] / [`log_warn!`] / [`log_error!`] 三个宏：只有宏才能在
//! 调用点展开 `module_path!()`，拿到真实的模块名（函数做不到）。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单文件上限
const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 备份数量上限
const MAX_BACKUPS: usize = 5;

struct Sink {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
/// 服务器本地时区相对 UTC 的偏移（秒），探测失败按 0 处理（即按 UTC 输出）
static LOCAL_OFFSET: OnceLock<i64> = OnceLock::new();

/// 初始化日志（幂等）。目录不可写时静默退回「仅控制台」。
pub fn init(data_dir: &Path) {
    let _ = local_offset();

    let dir = data_dir.join("logs");
    let path = dir.join("asmoday.log");
    let _ = fs::create_dir_all(&dir);
    let written = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let file = open_append(&path).ok();
    let _ = SINK.set(Mutex::new(Sink {
        path,
        file,
        written,
    }));
}

/// 本地时区偏移（秒，惰性探测一次）。
///
/// 惰性而不是只在 `init` 里算：`--check-config` 这类不碰数据目录的命令也要打日志，
/// 否则控制台时间会莫名其妙变成 UTC。
fn local_offset() -> i64 {
    *LOCAL_OFFSET.get_or_init(|| tz::local_offset_secs().unwrap_or(0))
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

/// 模块名去掉 crate 前缀，日志里只留 `core::fs_ops` 这一层
fn short_module(module: &str) -> &str {
    module
        .strip_prefix("asmoday::")
        .or_else(|| module.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::")))
        .unwrap_or(module)
}

fn emit(level: &str, module: &str, message: &str) {
    let now = SystemTime::now();
    let module = short_module(module);
    let pid = std::process::id();

    let local = format_local(now, local_offset());
    let console_line = format!("{local} [{level}] [{pid}] {module}: {message}\n");
    if level == "ERROR" || level == "WARN" {
        eprint!("{console_line}");
    } else {
        print!("{console_line}");
    }

    let file_line = format!("{} [{level}] [{pid}] {module}: {message}\n", iso8601_millis(now));

    let Some(lock) = SINK.get() else { return };
    let Ok(mut sink) = lock.lock() else { return };

    if sink.written + file_line.len() as u64 > MAX_BYTES {
        rotate(&mut sink);
    }
    if let Some(file) = sink.file.as_mut() {
        if file.write_all(file_line.as_bytes()).is_ok() {
            sink.written += file_line.len() as u64;
        }
    }
}

/// 停机前强制 flush（优雅停机时调用，避免最后几行留在缓冲区）
pub fn flush() {
    let Some(lock) = SINK.get() else { return };
    if let Ok(mut sink) = lock.lock() {
        if let Some(file) = sink.file.as_mut() {
            let _ = file.flush();
        }
    }
}

pub fn info(module: &str, message: &str) {
    emit("INFO", module, message);
}

pub fn warn(module: &str, message: &str) {
    emit("WARN", module, message);
}

pub fn error(module: &str, message: &str) {
    emit("ERROR", module, message);
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

/// WARN 日志
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
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
}
