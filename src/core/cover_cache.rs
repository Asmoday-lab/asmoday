//! 音乐封面缓存（v4.9.21）。
//!
//! 背景：`GET /api/media/cover` 每次都要把整首 MP3/FLAC 打开一遍解析 ID3/APIC。
//! 本地盘不算什么，但在 SMB/UNC 上实测 p50 6 秒、最慢 12.7 秒（一屏十几行就是
//! 十几个并发读，把共享打满，延迟一路爬）。封面又几乎不变，没有任何理由重复读。
//!
//! 两条约束决定了实现方式：
//! - **键必须带文件指纹**（mtime + size）。封面随标签走，标签一改文件 mtime 就变，
//!   所以不需要 TTL，也不会读到旧图 —— 和 `media_tags` 同一套失效口径。
//! - **负结果也要缓存**。没有内嵌封面的曲子占多数，不缓存的话每次换页都要重新
//!   开一遍文件确认「确实没有」，白付一次网络往返。
//!
//! 预算按字节算（默认 96 MiB）：封面是几百 KB 级的 `Vec<u8>`，只按条数限制会失控。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::audio_tags::Cover;

/// 单条记录本身的固定开销（key、Entry、HashMap 槽位），用于让负缓存也计入预算。
const ENTRY_OVERHEAD: usize = 96;

/// 缓存键：`(展示路径, mtime, size)`。
///
/// 用元组而不是结构体：三个值只参与哈希/相等，没有别的读者，结构体字段会被
/// `dead_code` 判定为「从未读取」。
pub type Key = (String, u64, u64);

pub fn key(path: impl Into<String>, mtime: u64, size: u64) -> Key {
    (path.into(), mtime, size)
}

struct Entry {
    /// `None` = 已经确认「这首没有内嵌封面」（负缓存）
    cover: Option<Arc<Cover>>,
    cost: usize,
    tick: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<Key, Entry>,
    bytes: usize,
}

pub struct CoverCache {
    inner: Mutex<Inner>,
    budget: usize,
    max_entries: usize,
    /// 并发闸：网络盘上无节制并发只会让每个请求都更慢
    slots: tokio::sync::Semaphore,
    clock: AtomicU64,
}

impl CoverCache {
    pub fn new(budget_bytes: usize, max_entries: usize, concurrency: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            budget: budget_bytes.max(1),
            max_entries: max_entries.max(1),
            slots: tokio::sync::Semaphore::new(concurrency.max(1)),
            clock: AtomicU64::new(1),
        }
    }

    /// 取并发许可（读盘期间持有）。信号量只会在进程退出时关闭，`None` 当无闸处理。
    pub async fn acquire(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.slots.acquire().await.ok()
    }

    /// 查缓存：`None` = 没查过；`Some(None)` = 查过，确实没有封面。
    pub fn lookup(&self, key: &Key) -> Option<Option<Arc<Cover>>> {
        let mut inner = self.inner.lock().ok()?;
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let entry = inner.map.get_mut(key)?;
        entry.tick = tick;
        Some(entry.cover.clone())
    }

    pub fn store(&self, key: Key, cover: Option<Arc<Cover>>) {
        let cost = cover
            .as_ref()
            .map(|cover| cover.data.len())
            .unwrap_or(0)
            .saturating_add(ENTRY_OVERHEAD);
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(old) = inner.map.insert(key, Entry { cover, cost, tick }) {
            inner.bytes = inner.bytes.saturating_sub(old.cost);
        }
        inner.bytes += cost;
        // 超预算 / 超条数就从最久没被访问的开始淘汰（LRU）
        while (inner.bytes > self.budget || inner.map.len() > self.max_entries)
            && !inner.map.is_empty()
        {
            let Some(victim) = inner
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.tick)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(removed) = inner.map.remove(&victim) {
                inner.bytes = inner.bytes.saturating_sub(removed.cost);
            }
        }
    }

    /// 清空（测试与诊断用；正常流程只靠 LRU）
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.map.clear();
            inner.bytes = 0;
        }
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, usize) {
        let inner = self.inner.lock().expect("cache poisoned");
        (inner.map.len(), inner.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover(bytes: usize) -> Arc<Cover> {
        Arc::new(Cover {
            mime: "image/jpeg".to_string(),
            data: vec![0u8; bytes],
        })
    }

    #[test]
    fn caches_positive_and_negative_results() {
        let cache = CoverCache::new(1024 * 1024, 16, 2);
        let hit_key = key("/music/a.mp3", 1, 10);
        assert!(cache.lookup(&hit_key).is_none(), "没查过时必须是 None");

        cache.store(hit_key.clone(), Some(cover(100)));
        assert!(cache.lookup(&hit_key).expect("应命中").is_some());

        let missing = key("/music/b.mp3", 1, 10);
        cache.store(missing.clone(), None);
        let hit = cache.lookup(&missing).expect("负缓存也要命中");
        assert!(hit.is_none(), "负缓存的值就是 None");
    }

    #[test]
    fn mtime_change_invalidates() {
        let cache = CoverCache::new(1024 * 1024, 16, 2);
        cache.store(key("/a.mp3", 1, 10), Some(cover(10)));
        // 文件改过 → 新键，旧记录自然查不到
        assert!(cache.lookup(&key("/a.mp3", 2, 10)).is_none());
    }

    #[test]
    fn evicts_least_recently_used() {
        let cache = CoverCache::new(1024, 100, 2);
        let first = key("/a", 1, 1);
        let second = key("/b", 1, 1);
        cache.store(first.clone(), Some(cover(100)));
        cache.store(second.clone(), Some(cover(100)));
        // 碰一下 first，让 second 变成最久未使用
        assert!(cache.lookup(&first).is_some());

        cache.store(key("/c", 1, 1), Some(cover(900)));
        let (len, bytes) = cache.stats();
        assert_eq!(len, 1, "预算只够放最大的那条");
        assert!(bytes <= 1024);
        assert!(cache.lookup(&second).is_none(), "LRU 先淘汰 second");
    }
}
