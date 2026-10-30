//! 目录快照缓存（v4.3.0，设计文档 4.7）。
//!
//! 递归搜索每走一个目录都要 `readdir`，十万级文件再翻几页就是几百毫秒起步。
//! 这里把「某个目录下有哪些名字」记进 SQLite：下次搜索同一棵树时只 `stat`
//! 目录本身（一次系统调用），名字直接从库里取。
//!
//! **为什么能缓存**：搜索只按**名字**匹配，而目录内容的名字集合只在
//! 创建 / 删除 / 改名时变化 —— 这三种操作都会推进父目录的 mtime。
//!
//! **为什么不会读到旧的**：只收留「mtime 至少 2 秒前」的目录。时间戳的分辨率
//! 是系统时钟粒度（Windows 上约 15.6ms），留 2 秒余量就能保证「入缓存之后
//! 发生的任何变化」都得到一个**严格更大**的 mtime，于是失配重扫。刚被动过的
//! 目录（正在上传、正在解压）一律不入库，它们本来也一直在变。
//!
//! 缓存是**尽力而为**：任何 SQLite 错误都当 miss，绝不让搜索失败；库被删掉、
//! 表结构变了、路径换了挂载点，最坏结果只是回到「每次都 readdir」。
//!
//! 只缓存**名字与类型**，不缓存大小 / mtime：那两项才是会原地变化的东西，
//! 交给命中时的一次 `symlink_metadata`（每页最多 `limit` 次）现取。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// 普通文件
pub const KIND_FILE: u8 = 0;
/// 目录
pub const KIND_DIR: u8 = 1;
/// 符号链接 / junction（是否指向目录由调用方另行判断）
pub const KIND_LINK: u8 = 2;

/// 目录 mtime 距「现在」至少这么久才允许入缓存（见模块注释）
const SETTLE: Duration = Duration::from_secs(2);
/// blob 头部的格式版本：改了编码就让老行自然失效
const FORMAT: u8 = 1;
/// 最多留多少个目录的快照（按 `used_at` 做 LRU 裁剪）
const MAX_DIRS: i64 = 4096;
/// 每写这么多次顺手裁一次，而不是每次都扫表
const GC_EVERY: u64 = 128;
/// `used_at` 刷新阈值：一次搜索会打几千次读，不能每次都写
const TOUCH_AFTER: i64 = 3600;

/// 快照里的一条：只有名字与类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedEntry {
    pub name: String,
    pub kind: u8,
}

impl CachedEntry {
    pub fn new(name: impl Into<String>, kind: u8) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }
}

/// 目录快照缓存。一个进程一个实例，内部复用一条连接。
pub struct DirCache {
    db_path: PathBuf,
    conn: Mutex<Option<Connection>>,
    stores: AtomicU64,
}

impl DirCache {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            conn: Mutex::new(None),
            stores: AtomicU64::new(0),
        }
    }

    /// 读一次缓存。`stamp` 与入库时不一致就是 miss。
    ///
    /// 命中时顺手（每小时最多一次）刷新 `used_at`，LRU 才能反映真实使用。
    pub fn load(&self, path: &str, stamp: i64) -> Option<Vec<CachedEntry>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT entries, used_at FROM dir_snapshots WHERE path = ?1 AND stamp = ?2",
                    params![path, stamp],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((blob, used_at)) = row else {
                return Ok(None);
            };
            let now = now_secs();
            if now - used_at > TOUCH_AFTER {
                let _ = conn.execute(
                    "UPDATE dir_snapshots SET used_at = ?2 WHERE path = ?1",
                    params![path, now],
                );
            }
            Ok(decode_entries(&blob))
        })
        .flatten()
    }

    /// 写一次缓存。`stamp` 不合规（太新 / 取不到）时直接跳过。
    ///
    /// `entries` 必须是**已经排好序、已经过滤过**的最终顺序：缓存命中时
    /// 直接沿用，不再重排。
    pub fn store(&self, path: &str, stamp: i64, entries: &[CachedEntry]) {
        if decode_guard(stamp).is_none() {
            return;
        }
        let blob = encode_entries(entries);
        let now = now_secs();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO dir_snapshots (path, stamp, entries, entry_count, used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![path, stamp, blob, entries.len() as i64, now],
            )
        });
        let stores = self.stores.fetch_add(1, Ordering::Relaxed) + 1;
        if stores.is_multiple_of(GC_EVERY) {
            self.gc();
        }
    }

    /// 丢掉单个目录（该项改名 / 删除后由调用方补一刀；不依赖 mtime 也正确）
    #[cfg(test)]
    pub fn forget(&self, path: &str) {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM dir_snapshots WHERE path = ?1", params![path])
        });
    }

    /// 当前缓存了多少个目录（测试 / 观测用）
    #[cfg(test)]
    pub fn count(&self) -> i64 {
        self.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM dir_snapshots", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .unwrap_or(0)
    }

    /// LRU 裁剪：超出 `MAX_DIRS` 的旧快照直接删
    fn gc(&self) {
        self.with_conn(|conn| {
            // rowid 兜底：一次搜索里几千个目录的 `used_at` 往往是同一秒，
            // 只按它排序等于随机淘汰；REPLACE 会换新 rowid，正好反映「最近写入」
            conn.execute(
                "DELETE FROM dir_snapshots WHERE path NOT IN \
                 (SELECT path FROM dir_snapshots ORDER BY used_at DESC, rowid DESC LIMIT ?1)",
                params![MAX_DIRS],
            )
        });
    }

    /// 在一条（懒打开的）连接上执行；任何失败都返回 `None`。
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        let mut guard = self.conn.lock().ok()?;
        if guard.is_none() {
            *guard = open(&self.db_path);
        }
        f(guard.as_ref()?)
            .map_err(|err| {
                // 缓存坏了不影响搜索，只在 debug 级别留一行
                crate::log_debug!("目录快照缓存不可用：{err}");
            })
            .ok()
    }
}

fn open(path: &PathBuf) -> Option<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path).ok()?;
    conn.pragma_update(None, "journal_mode", "WAL").ok()?;
    conn.pragma_update(None, "busy_timeout", 5000).ok()?;
    Some(conn)
}

/// 测试用：在 `dir` 下建一个走正式迁移的临时库（表结构与线上一致）
#[cfg(test)]
pub(crate) fn test_cache(dir: &std::path::Path) -> DirCache {
    let db = dir.join("asmoday.db");
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::create_dir_all(dir);
    let store = crate::core::auth::AuthStore::new(db.clone());
    let _ = store.init();
    DirCache::new(db)
}

/// 目录的缓存戳：mtime 的 Unix 纳秒。
///
/// 返回 `None` 表示「这个目录不该进缓存」：取不到 mtime，或者它刚被动过
/// （距现在不到 [`SETTLE`]，此时 mtime 与后续变化可能落在同一个时钟粒度里）。
pub fn stamp_of(modified: SystemTime) -> Option<i64> {
    let stamp = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64;
    decode_guard(stamp)
}

/// 戳的合规性检查（写入与读取共用一套口径）
fn decode_guard(stamp: i64) -> Option<i64> {
    if stamp <= 0 {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    // mtime 在未来（时钟回拨 / 跨时区同步）也当成不可信
    if stamp > now || now - stamp < SETTLE.as_nanos() as i64 {
        return None;
    }
    Some(stamp)
}

/// `[版本 u8][条数 u32][kind u8][名字长度 u16][名字字节]…`
fn encode_entries(entries: &[CachedEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + entries.len() * 24);
    out.push(FORMAT);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        // 名字长度超过 u16 的（> 64KB 的单个文件名）不可能出现在正常文件系统上，
        // 真出现就按空名截断 —— 与其 panic 不如少一条
        let bytes = entry.name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        out.push(entry.kind);
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

/// 解码；格式不认（版本不同 / 数据被截断）就返回 `None`（当作 miss）。
fn decode_entries(blob: &[u8]) -> Option<Vec<CachedEntry>> {
    if blob.first() != Some(&FORMAT) {
        return None;
    }
    let mut rest = blob.get(1..)?;
    let count = read_u32(&mut rest)? as usize;
    // 每条至少 3 字节：先粗略判一下，别为一条坏数据预分配几 GB
    if count > rest.len() / 3 {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = *rest.first()?;
        rest = rest.get(1..)?;
        let len = read_u16(&mut rest)? as usize;
        let name = std::str::from_utf8(rest.get(..len)?).ok()?.to_string();
        rest = rest.get(len..)?;
        entries.push(CachedEntry { name, kind });
    }
    Some(entries)
}

fn read_u32(rest: &mut &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
    *rest = rest.get(4..)?;
    Some(u32::from_le_bytes(bytes))
}

fn read_u16(rest: &mut &[u8]) -> Option<u16> {
    let bytes: [u8; 2] = rest.get(..2)?.try_into().ok()?;
    *rest = rest.get(2..)?;
    Some(u16::from_le_bytes(bytes))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn cache(tag: &str) -> DirCache {
        let dir =
            std::env::temp_dir().join(format!("asmoday-dircache-{tag}-{}", std::process::id()));
        test_cache(&dir)
    }

    fn old_stamp() -> i64 {
        stamp_of(SystemTime::now() - Duration::from_secs(60)).unwrap()
    }

    fn sample() -> Vec<CachedEntry> {
        vec![
            CachedEntry::new("a", KIND_DIR),
            CachedEntry::new("readme.txt", KIND_FILE),
            CachedEntry::new("图片 2024", KIND_DIR),
            CachedEntry::new("链接", KIND_LINK),
        ]
    }

    #[test]
    fn encode_decode_roundtrip_keeps_order_and_unicode() {
        let entries = sample();
        let blob = encode_entries(&entries);
        assert_eq!(decode_entries(&blob), Some(entries));
    }

    #[test]
    fn decode_rejects_broken_blobs() {
        assert!(decode_entries(&[]).is_none());
        assert!(decode_entries(&[9, 0, 0, 0, 0]).is_none(), "版本不认识");
        // 声称有 1000 条但只有 3 字节
        let mut blob = vec![FORMAT];
        blob.extend_from_slice(&1000u32.to_le_bytes());
        blob.extend_from_slice(&[KIND_FILE, 1, 0]);
        assert!(decode_entries(&blob).is_none());
    }

    #[test]
    fn store_then_load_hits_only_with_same_stamp() {
        let cache = cache("hit");
        let stamp = old_stamp();
        let key = "C:/data/photos";
        assert!(cache.load(key, stamp).is_none(), "空库必须 miss");

        cache.store(key, stamp, &sample());
        assert_eq!(cache.load(key, stamp), Some(sample()));
        assert!(cache.load(key, stamp + 1).is_none(), "戳变了必须 miss");
        assert!(cache.load("C:/data/other", stamp).is_none());
        cache.forget(key);
        assert!(cache.load(key, stamp).is_none());
    }

    #[test]
    fn fresh_directories_are_never_cached() {
        let now = SystemTime::now();
        assert!(stamp_of(now).is_none(), "刚改过的目录不入缓存");
        assert!(
            stamp_of(now - Duration::from_millis(500)).is_none(),
            "不足 2 秒的同样不可信"
        );
        assert!(
            stamp_of(now + Duration::from_secs(60)).is_none(),
            "未来时间戳"
        );
        assert!(stamp_of(now - Duration::from_secs(3)).is_some());
    }

    #[test]
    fn lru_keeps_the_most_recently_used_directories() {
        let cache = cache("gc");
        let stamp = old_stamp();
        // 直接塞超过上限的行，验证 gc 的裁剪口径
        for index in 0..(MAX_DIRS + 10) {
            cache.store(&format!("C:/many/{index}"), stamp, &sample());
        }
        cache.gc();
        assert!(cache.count() <= MAX_DIRS, "裁剪后仍有 {} 行", cache.count());
        // 最后写入的那批一定还在
        assert!(
            cache
                .load(&format!("C:/many/{}", MAX_DIRS + 9), stamp)
                .is_some()
        );
    }

    #[test]
    fn cache_survives_a_missing_directory_on_disk() {
        // 缓存本身就是「一份快照」，路径不存在（挂载点拔了）时 load 也不该炸
        let cache = cache("missing");
        let stamp = old_stamp();
        cache.store("Z:/gone", stamp, &[]);
        assert_eq!(cache.load("Z:/gone", stamp), Some(Vec::new()));
        let _ = Path::new("Z:/gone");
    }
}
