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
//! 搜索只缓存**名字与类型**，不缓存大小 / mtime：那两项才是会原地变化的东西，
//! 交给命中时的一次 `symlink_metadata`（每页最多 `limit` 次）现取。
//!
//! 媒体库（v4.3.1，设计文档 6.6）需要按 mtime 排序、按大小展示，光有名字不够，
//! 所以另有一张 `media_snapshots` 表，条目里带上 `size` / `mtime`。失效判定与
//! 搜索同一套（目录 mtime），另有两条补充：应用自己**原地覆盖**写文件后主动
//! `forget`（覆盖不动目录 mtime），以及前端的「重新扫描」走 `refresh` 强制穿透。

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
/// 媒体快照的格式版本（与上面刻意不同：两种 blob 互相喂给对方都会解码失败）
const FORMAT_MEDIA: u8 = 2;
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

/// 媒体快照里的一条：除了名字与类型，还带**大小**与**修改时间（秒）**。
///
/// 媒体库要按 mtime 降序排、要显示体积，而这两项 `readdir` 不给（只有 `stat` 给）。
/// 缓存它们是为了省掉「每条一次 `stat`」，代价是**原地覆盖写**（改名 / 增删以外
/// 的改动）不会推进目录 mtime，只能靠调用方主动 `forget` 或前端强制重扫兜底。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaEntry {
    pub name: String,
    pub kind: u8,
    pub size: u64,
    pub mtime: u64,
}

impl MediaEntry {
    pub fn new(name: impl Into<String>, kind: u8, size: u64, mtime: u64) -> Self {
        Self {
            name: name.into(),
            kind,
            size,
            mtime,
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
    media_stores: AtomicU64,
}

impl DirCache {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            conn: Mutex::new(None),
            stores: AtomicU64::new(0),
            media_stores: AtomicU64::new(0),
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

    /// 丢掉单个目录（改名 / 删除 / 原地覆盖写之后由调用方补一刀）。
    ///
    /// 结构变化本身会被目录 mtime 抓到，这里主要是给「原地覆盖」用：内容变了但
    /// 目录 mtime 不动，缓存里的 `size` / `mtime` 就是旧的，排序会停在老位置。
    pub fn forget(&self, path: &str) {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM dir_snapshots WHERE path = ?1", params![path])?;
            conn.execute("DELETE FROM media_snapshots WHERE path = ?1", params![path])
        });
    }

    /// 读一次媒体快照（带 size / mtime 的版本）。`stamp` 不一致就是 miss。
    pub fn load_media(&self, path: &str, stamp: i64) -> Option<Vec<MediaEntry>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT entries, used_at FROM media_snapshots WHERE path = ?1 AND stamp = ?2",
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
                    "UPDATE media_snapshots SET used_at = ?2 WHERE path = ?1",
                    params![path, now],
                );
            }
            Ok(decode_media(&blob))
        })
        .flatten()
    }

    /// 写一次媒体快照。`stamp` 不合规（太新 / 取不到）时直接跳过。
    ///
    /// 顺序由调用方决定：调用方按名字排好再存，命中时按原顺序还原 —— 遍历顺序
    /// 稳定，分页游标才稳定（`readdir` 的顺序是不可依赖的）。
    pub fn store_media(&self, path: &str, stamp: i64, entries: &[MediaEntry]) {
        if decode_guard(stamp).is_none() {
            return;
        }
        let blob = encode_media(entries);
        let now = now_secs();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO media_snapshots (path, stamp, entries, entry_count, used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![path, stamp, blob, entries.len() as i64, now],
            )
        });
        let stores = self.media_stores.fetch_add(1, Ordering::Relaxed) + 1;
        if stores.is_multiple_of(GC_EVERY) {
            self.gc();
        }
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

    /// 当前缓存了多少个目录的**媒体**快照（测试 / 观测用）
    #[cfg(test)]
    pub fn count_media(&self) -> i64 {
        self.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM media_snapshots", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .unwrap_or(0)
    }

    /// LRU 裁剪：超出 `MAX_DIRS` 的旧快照直接删（两张表各裁各的）
    fn gc(&self) {
        self.with_conn(|conn| {
            // rowid 兜底：一次搜索里几千个目录的 `used_at` 往往是同一秒，
            // 只按它排序等于随机淘汰；REPLACE 会换新 rowid，正好反映「最近写入」
            for table in ["dir_snapshots", "media_snapshots"] {
                conn.execute(
                    &format!(
                        "DELETE FROM {table} WHERE path NOT IN \
                         (SELECT path FROM {table} ORDER BY used_at DESC, rowid DESC LIMIT ?1)"
                    ),
                    params![MAX_DIRS],
                )?;
            }
            Ok(())
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

/// `[版本 u8][条数 u32]{kind u8, size u64, mtime u64, 名长 u16, 名字字节}…`
///
/// 固定宽度而不是 varint：媒体库一次几十万条，多占几 MB 换「一眼能看懂、
/// 不会因为移位写错而悄悄读歪」是划算的（`size` / `mtime` 都是 8 字节小端）。
fn encode_media(entries: &[MediaEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + entries.len() * 32);
    out.push(FORMAT_MEDIA);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        let bytes = entry.name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        out.push(entry.kind);
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.mtime.to_le_bytes());
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

/// 解码媒体快照；版本不认 / 数据被截断就返回 `None`（当作 miss）。
fn decode_media(blob: &[u8]) -> Option<Vec<MediaEntry>> {
    if blob.first() != Some(&FORMAT_MEDIA) {
        return None;
    }
    let mut rest = blob.get(1..)?;
    let count = read_u32(&mut rest)? as usize;
    // 每条至少 19 字节（kind 1 + size 8 + mtime 8 + 名长 2）
    if count > rest.len() / 19 {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = *rest.first()?;
        rest = rest.get(1..)?;
        let size = read_u64(&mut rest)?;
        let mtime = read_u64(&mut rest)?;
        let len = read_u16(&mut rest)? as usize;
        let name = std::str::from_utf8(rest.get(..len)?).ok()?.to_string();
        rest = rest.get(len..)?;
        entries.push(MediaEntry {
            name,
            kind,
            size,
            mtime,
        });
    }
    Some(entries)
}

fn read_u64(rest: &mut &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = rest.get(..8)?.try_into().ok()?;
    *rest = rest.get(8..)?;
    Some(u64::from_le_bytes(bytes))
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

    fn media_sample() -> Vec<MediaEntry> {
        vec![
            MediaEntry::new("img_0001.jpg", KIND_FILE, 4_312_777, 1_730_000_001),
            MediaEntry::new("2024 旅行", KIND_DIR, 0, 0),
            MediaEntry::new("超大.iso", KIND_FILE, 5_368_709_120, 1_700_000_000),
            MediaEntry::new("链接", KIND_LINK, 0, 0),
        ]
    }

    #[test]
    fn media_roundtrip_keeps_size_mtime_and_unicode() {
        let entries = media_sample();
        assert_eq!(decode_media(&encode_media(&entries)), Some(entries));
    }

    #[test]
    fn media_and_search_blobs_never_mix() {
        // 两张表各存各的格式，喂错了必须解码失败（而不是读出半截乱码）
        assert!(decode_media(&encode_entries(&sample())).is_none());
        assert!(decode_entries(&encode_media(&media_sample())).is_none());
        assert!(decode_media(&[]).is_none());
    }

    #[test]
    fn store_media_hits_only_with_same_stamp_and_forget_clears_both() {
        let cache = cache("media-hit");
        let stamp = old_stamp();
        let key = "C:/data/photos";
        assert!(cache.load_media(key, stamp).is_none(), "空库必须 miss");

        cache.store_media(key, stamp, &media_sample());
        assert_eq!(cache.load_media(key, stamp), Some(media_sample()));
        assert!(
            cache.load_media(key, stamp + 1).is_none(),
            "戳变了必须 miss"
        );
        // 两张表互不干扰：媒体快照不影响搜索快照
        assert!(cache.load(key, stamp).is_none());
        assert_eq!(cache.count_media(), 1);

        // 原地覆盖写之后调用方会 forget —— 两张表一起清
        cache.store(key, stamp, &sample());
        cache.forget(key);
        assert!(cache.load_media(key, stamp).is_none());
        assert!(cache.load(key, stamp).is_none());
        assert_eq!(cache.count_media(), 0);
    }
}
