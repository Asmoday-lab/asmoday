//! per-path 内存锁（设计文档 3.2.8）。
//!
//! 规则：
//! - 一次操作涉及的源路径、目标路径、目标父目录全部加锁；
//! - 多把锁按规范化路径**字典序**获取，天然避免死锁（不需要等待图）；
//! - **立即失败**，不排队：拿不到就返回 `E_BUSY`，前端提示「该文件正在被其他操作占用」，
//!   绝不自动重试（排队会在低功耗设备上把请求堆成雪崩）。
//!
//! 锁只保护进程内的并发，不跨进程；v2 是单进程单管理员模型，够用。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::core::fs::FsError;

/// 已占用路径集合
#[derive(Default)]
pub struct PathLocks {
    held: Mutex<HashSet<String>>,
}

impl PathLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一次性占用一组路径；任一已被占用则整体失败（不留半把锁）。
    pub fn acquire(&self, paths: &[PathBuf]) -> Result<PathGuard<'_>, FsError> {
        let mut keys: Vec<String> = paths.iter().filter_map(|p| key_of(p)).collect();
        keys.sort();
        keys.dedup();

        let mut held = self
            .held
            .lock()
            .map_err(|_| FsError::Io("lock poisoned".to_string()))?;

        if let Some(busy) = keys.iter().find(|key| held.contains(*key)) {
            return Err(FsError::Busy(format!("路径正在被占用：{busy}")));
        }
        for key in &keys {
            held.insert(key.clone());
        }
        Ok(PathGuard { locks: self, keys })
    }

    fn release(&self, keys: &[String]) {
        if let Ok(mut held) = self.held.lock() {
            for key in keys {
                held.remove(key);
            }
        }
    }
}

/// 持有期间的锁；`Drop` 时自动释放（panic 也不会漏锁）
pub struct PathGuard<'a> {
    locks: &'a PathLocks,
    keys: Vec<String>,
}

impl Drop for PathGuard<'_> {
    fn drop(&mut self) {
        self.locks.release(&self.keys);
    }
}

/// 锁键：POSIX 风格 + 小写（大小写不敏感文件系统上 `A.txt` 与 `a.txt` 是同一个文件）
fn key_of(path: &Path) -> Option<String> {
    let text = crate::core::fs::normalize_display(path);
    let text = text.trim_end_matches('/').to_lowercase();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_fast() {
        let locks = PathLocks::new();
        let path = PathBuf::from("/tmp/asmoday-lock-test/a.txt");
        let guard = locks
            .acquire(std::slice::from_ref(&path))
            .expect("首次加锁应成功");
        let err = match locks.acquire(std::slice::from_ref(&path)) {
            Ok(_) => panic!("second acquire should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, FsError::Busy(_)));
        drop(guard);
        assert!(locks.acquire(&[path]).is_ok(), "释放后应可再次加锁");
    }

    #[test]
    fn acquisition_is_all_or_nothing() {
        let locks = PathLocks::new();
        let a = PathBuf::from("/tmp/asmoday-lock-test/a");
        let b = PathBuf::from("/tmp/asmoday-lock-test/b");
        let _guard = locks.acquire(std::slice::from_ref(&a)).unwrap();
        assert!(locks.acquire(&[a, b.clone()]).is_err());
        // 失败的批量加锁不能留下 b 的残留占用
        assert!(locks.acquire(&[b]).is_ok());
    }

    #[test]
    fn keys_are_case_insensitive() {
        let locks = PathLocks::new();
        let _guard = locks
            .acquire(&[PathBuf::from("/tmp/AsModay/Lock")])
            .unwrap();
        assert!(
            locks
                .acquire(&[PathBuf::from("/tmp/asmoday/lock")])
                .is_err()
        );
    }
}
