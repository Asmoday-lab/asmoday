//! 运行日志（设计文档 3.7：日志防撑爆）。
//!
//! - 落盘位置：`data/logs/asmoday.log`；
//! - 单个文件写满 10 MiB 立刻轮转，历史文件依次改名，最多保留 5 个备份；
//! - 同时输出到控制台，容器里 `docker logs` 依然能看到；
//! - 不引入额外日志框架：本项目日志量很小，自带的按大小轮转比按天轮转更符合
//!   「单文件最大 10MB」这条硬指标（`tracing-appender` 只支持按时间轮转）。

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

/// 初始化日志（幂等）。目录不可写时静默退回「仅控制台」。
pub fn init(data_dir: &Path) {
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

fn emit(level: &str, message: &str) {
    let line = format!("{} [{level}] {message}\n", iso8601(SystemTime::now()));

    // 控制台
    if level == "ERROR" || level == "WARN" {
        eprint!("{line}");
    } else {
        print!("{line}");
    }

    let Some(lock) = SINK.get() else { return };
    let Ok(mut sink) = lock.lock() else { return };

    if sink.written + line.len() as u64 > MAX_BYTES {
        rotate(&mut sink);
    }
    if let Some(file) = sink.file.as_mut() {
        if file.write_all(line.as_bytes()).is_ok() {
            sink.written += line.len() as u64;
        }
    }
}

pub fn info(message: &str) {
    emit("INFO", message);
}

pub fn warn(message: &str) {
    emit("WARN", message);
}

pub fn error(message: &str) {
    emit("ERROR", message);
}

/// UTC 时间戳（不引 chrono：只需 ISO-8601 到秒，自己算历法更省依赖）
fn iso8601(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_leap_day() {
        // 2024-02-29T12:34:56Z
        assert_eq!(iso8601(UNIX_EPOCH + std::time::Duration::from_secs(1_709_210_096)), "2024-02-29T12:34:56Z");
    }
}
