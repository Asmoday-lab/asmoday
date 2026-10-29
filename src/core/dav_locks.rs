//! WebDAV 写锁表（设计文档 3.10 / 4.2「LOCK 伪锁」的补强）。
//!
//! 原先只回一个随机 token，不记录任何状态：两个客户端同时编辑同一文件时，
//! 后保存的会静默覆盖前一个（Office / WPS 保存必踩）。这里做最小可用的
//! 排他锁——内存表 + 30 分钟过期，不落盘、不跨进程（单实例锁已保证只有一个进程）。
//!
//! 语义按 RFC 4918 的常用子集：
//! * `LOCK` 已被他人持有 → 423 Locked；
//! * 写操作（PUT / DELETE / MOVE / COPY / MKCOL / PROPPATCH）命中锁且 `If`
//!   头没带对应 token → 423 Locked；
//! * `UNLOCK` token 不匹配 → 409 Conflict；
//! * 过期自动失效，后台不需要定时器（读的时候顺手清理）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 锁有效期：Office 默认 30 分钟，够保存一次文档
pub const LOCK_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
struct Entry {
    token: String,
    expires: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockError {
    /// 资源已被其它客户端锁定
    Locked,
    /// UNLOCK 的 token 与当前锁不匹配
    Conflict,
}

#[derive(Debug, Default)]
pub struct DavLockTable {
    entries: Mutex<HashMap<String, Entry>>,
}

impl DavLockTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加锁；已有锁且 `If` 未携带对应 token 时返回 `Locked`。
    /// 返回最终的 lock token（刷新时复用原 token，避免客户端缓存失效）。
    pub fn lock(&self, key: &str, if_header: Option<&str>) -> Result<String, LockError> {
        let mut entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        let now = Instant::now();
        let tokens = lock_tokens(if_header);

        if let Some(entry) = entries.get(key) {
            if entry.expires > now {
                if tokens.iter().any(|token| token == &entry.token) {
                    let token = entry.token.clone();
                    entries.insert(
                        key.to_string(),
                        Entry {
                            token: token.clone(),
                            expires: now + LOCK_TTL,
                        },
                    );
                    return Ok(token);
                }
                return Err(LockError::Locked);
            }
        }

        let token = format!("opaquelocktoken:{}", crate::core::auth::random_token());
        entries.insert(
            key.to_string(),
            Entry {
                token: token.clone(),
                expires: now + LOCK_TTL,
            },
        );
        Ok(token)
    }

    /// 解锁；token 不匹配返回 `Conflict`（RFC 4918 要求 409）。
    pub fn unlock(&self, key: &str, token: &str) -> Result<(), LockError> {
        let mut entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        match entries.get(key) {
            Some(entry) if entry.expires > Instant::now() => {
                if entry.token != token {
                    return Err(LockError::Conflict);
                }
                entries.remove(key);
                Ok(())
            }
            // 锁已过期 / 不存在：当作解锁成功，客户端不会因为过期而卡住
            _ => {
                entries.remove(key);
                Ok(())
            }
        }
    }

    /// 写操作前的校验：资源被锁且 `If` 没带对应 token → `Locked`。
    pub fn check_write(&self, key: &str, if_header: Option<&str>) -> Result<(), LockError> {
        let entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        let Some(entry) = entries.get(key) else {
            return Ok(());
        };
        if entry.expires <= Instant::now() {
            return Ok(());
        }
        if lock_tokens(if_header)
            .iter()
            .any(|token| token == &entry.token)
        {
            return Ok(());
        }
        Err(LockError::Locked)
    }

    /// 供测试与运维观察：当前有效锁数量（顺手清掉过期项，避免内存无限增长）
    pub fn active(&self) -> usize {
        let mut entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        let now = Instant::now();
        entries.retain(|_, entry| entry.expires > now);
        entries.len()
    }
}

/// 从 `If: (<opaquelocktoken:xxx>)` / `If: <http://…> (<token>)` 里抠出所有 token。
///
/// 只认 `opaquelocktoken:` 前缀，其它 `<...>` 是资源 URL，不能当 token 用。
pub fn lock_tokens(if_header: Option<&str>) -> Vec<String> {
    let Some(value) = if_header else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else { break };
        let candidate = after[..end].trim();
        if candidate.starts_with("opaquelocktoken:") {
            out.push(candidate.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_client_cannot_lock() {
        let table = DavLockTable::new();
        let token = table.lock("/dav/m0/a.docx", None).unwrap();
        assert_eq!(table.active(), 1);
        assert_eq!(
            table.lock("/dav/m0/a.docx", None).unwrap_err(),
            LockError::Locked
        );
        // 带着自己的 token 再来一次 = 续期，token 不变
        let again = table
            .lock("/dav/m0/a.docx", Some(&format!("(<{token}>)")))
            .unwrap();
        assert_eq!(again, token);
    }

    #[test]
    fn write_requires_matching_if_header() {
        let table = DavLockTable::new();
        let token = table.lock("/dav/m0/a.txt", None).unwrap();
        assert_eq!(
            table.check_write("/dav/m0/a.txt", None).unwrap_err(),
            LockError::Locked
        );
        assert!(
            table
                .check_write("/dav/m0/a.txt", Some(&format!("(<{token}>)")))
                .is_ok()
        );
        // 没锁的路径不受影响
        assert!(table.check_write("/dav/m0/b.txt", None).is_ok());
    }

    #[test]
    fn unlock_rejects_foreign_token_then_releases() {
        let table = DavLockTable::new();
        let token = table.lock("/dav/m0/a.txt", None).unwrap();
        assert_eq!(
            table
                .unlock("/dav/m0/a.txt", "opaquelocktoken:nope")
                .unwrap_err(),
            LockError::Conflict
        );
        table.unlock("/dav/m0/a.txt", &token).unwrap();
        assert!(table.check_write("/dav/m0/a.txt", None).is_ok());
    }

    #[test]
    fn parses_tokens_but_ignores_resource_urls() {
        let header = "<http://host/dav/m0/a.txt> (<opaquelocktoken:abc>)";
        assert_eq!(lock_tokens(Some(header)), vec!["opaquelocktoken:abc"]);
        assert!(lock_tokens(Some("<http://host/dav/m0/a.txt>")).is_empty());
        assert!(lock_tokens(None).is_empty());
    }
}
