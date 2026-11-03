//! 媒体索引（v4.7.0，设计文档 6.6）。
//!
//! 媒体库原来每次请求都把整棵树走一遍：`readdir` + 逐条分类 + 全量排序，只为了
//! 返回 120 条。两万张图时首屏还好，**每翻一页都要重排两万条**，NAS 上就是
//! 「越往下越木」。这里把扫描结果落成一张扁平的 `media_assets` 表：
//!
//! - 分页 = `ORDER BY mtime DESC, path ASC LIMIT n`（keyset 游标，不重不漏）；
//! - 计数 = `GROUP BY kind`；
//! - 相册墙 = `GROUP BY dir`（每个直接含媒体的目录一本册子）。
//!
//! 三张表各管一件事：
//!
//! - `media_assets`：一个媒体文件一行，`path` 是展示路径（`//server/share` 锚点原样
//!   保留，前端所有回传都用它），`real` 是扫描时的真实路径（算缩略图键 / 打开文件用，
//!   两者在 UNC 上不是同一个串）；
//! - `media_dirs`：目录行，只记「这个目录是哪一版 mtime 的扫描结果」。
//!   与 `dir_cache` 的媒体快照同一套失效口径（目录 mtime 纳秒），命中就跳过写库；
//! - `media_roots`：每个浏览根的「上次完整同步时间 + 深度」。
//!   窗口期（[`FRESH`] 秒）内的请求直接走 SQL，不再 stat 一遍整棵树。
//!
//! **为什么可以有窗口期**：目录 mtime 只反映「这一层名字集合变了」，兄弟目录的内容
//! 变化它不知道，所以想做到零延迟就只能每次 stat 全树（那是旧实现的代价）。这里换成
//! 「[`FRESH`] 秒内认为索引是新鲜的，且每服务一页就续一次」：连续浏览期间不重走树，
//! 期间新拷进来的文件最多晚一分钟出现，用户点「重新扫描」可以立刻穿透。
//! 应用自己的写操作会主动 [`invalidate`]，不靠窗口。
//!
//! 索引是**派生数据**：库被删、表结构变了、路径换了挂载点，最坏结果只是回到
//! 「每次请求重扫」，媒体库本身不受影响（调用方全都留了内存扫描兜底）。

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

/// 索引「还算新鲜」的窗口（秒）：期间请求直接查库，不 stat 整棵树
///
/// 60 秒是「翻页手感」与「外部改动可见性」的折中：每成功服务一页都会续一次
/// （`touch_synced`），所以连续浏览期间永远不会重走树；停手一分钟后下一次
/// 请求才核对目录，或者用户点「重新扫描」立刻穿透。
pub const FRESH: u64 = 60;
/// 一次事务里最多写多少个目录（首次建库两万行，别拿一个长事务卡住别的写入）
pub const CHUNK: usize = 64;

/// 索引实例。一个进程一个，内部复用一条连接（懒打开）。
pub struct MediaIndex {
    db_path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl MediaIndex {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            conn: Mutex::new(None),
        }
    }

    /// 在索引连接上执行；打开失败返回 `None`（调用方回退内存扫描）。
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        let mut guard = self.conn.lock().ok()?;
        if guard.is_none() {
            *guard = open(&self.db_path);
        }
        f(guard.as_ref()?)
            .map_err(|err| crate::log_debug!("媒体索引不可用：{err}"))
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

/// 一个媒体文件在索引里的样子
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    /// 展示路径（`//` 锚点，前端回传的就是它）
    pub path: String,
    /// 真实路径串（扫描时的原样；UNC 上是 `\\?\UNC\…`）
    pub real: String,
    /// 所在目录的展示路径
    pub dir: String,
    pub kind: String,
    pub ext: String,
    pub size: u64,
    pub mtime: u64,
}

/// 相册墙的一行：`GROUP BY dir` 的聚合结果
#[derive(Debug, Clone)]
pub struct AlbumRow {
    pub dir: String,
    pub count: usize,
    /// 册内最新一条的 mtime（封面 / 排序键）
    pub mtime: u64,
    /// 册内最早一条的 mtime（0 = 未知）
    pub first_mtime: u64,
}

/// 一次查询的作用域：某个根目录下、`depth` 层以内
///
/// `depth` 是**绝对段数**（`//srv/share/a/b` 记 4），所以在任何根下算出来的范围
/// 都一致，索引不必按根重复存。
#[derive(Debug, Clone)]
pub struct Scope {
    pub root: String,
    /// 根自己的绝对段数
    pub min_depth: i64,
    /// 最深允许的绝对段数 = `min_depth + depth`
    pub max_depth: i64,
}

impl Scope {
    pub fn new(root: &str, depth: usize) -> Self {
        let min_depth = segments(root);
        Self {
            root: root.to_string(),
            min_depth,
            max_depth: min_depth + depth as i64,
        }
    }

    /// 第 `level` 层（根为 0）的绝对段数
    pub fn depth_of(&self, level: usize) -> i64 {
        self.min_depth + level as i64
    }

    /// 前缀：`root` 本身以 `/` 结尾时不能补第二个（盘符根 `C:/`）
    pub fn prefix(&self) -> String {
        if self.root.ends_with('/') {
            self.root.clone()
        } else {
            format!("{}/", self.root)
        }
    }

    /// 作用域条件：根本身，或者以 `root/` 开头的子孙目录
    fn clause() -> &'static str {
        "(a.dir = ? OR instr(a.dir, ?) = 1) AND a.depth BETWEEN ? AND ?"
    }

    fn params(&self) -> Vec<Value> {
        vec![
            Value::Text(self.root.clone()),
            Value::Text(self.prefix()),
            Value::Integer(self.min_depth),
            Value::Integer(self.max_depth),
        ]
    }
}

/// 路径的绝对段数：`//srv/share/a` → 3、`C:/Users` → 2
pub fn segments(path: &str) -> i64 {
    path.split('/').filter(|part| !part.is_empty()).count() as i64
}

fn kinds_clause(kinds: &[&str]) -> String {
    let marks = vec!["?"; kinds.len()].join(", ");
    format!("a.kind IN ({marks})")
}

fn kinds_params(kinds: &[&str]) -> Vec<Value> {
    kinds
        .iter()
        .map(|kind| Value::Text((*kind).to_string()))
        .collect()
}

/// 目录行里记的「这一版扫描」的 mtime 戳；`None` = 这个目录没入库
pub fn dir_stamp(conn: &Connection, dir: &str) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT stamp FROM media_dirs WHERE dir = ?1",
        params![dir],
        |row| row.get::<_, i64>(0),
    )
    .optional()
}

/// 整目录替换：先删该目录旧行，再写新行
///
/// 用「先删后插」而不是逐条 diff：目录内容变了基本是整批变（拷一册照片进来），
/// diff 的复杂度换不来收益，而且 `DELETE` 走的是 `dir` 索引，代价可控。
pub fn replace_dir(
    conn: &Connection,
    dir: &str,
    depth: i64,
    stamp: i64,
    assets: &[Asset],
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM media_assets WHERE dir = ?1", params![dir])?;
    if !assets.is_empty() {
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO media_assets
             (path, real, dir, depth, name, kind, ext, size, mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for asset in assets {
            stmt.execute(params![
                &asset.path,
                &asset.real,
                dir,
                depth,
                &asset.name,
                &asset.kind,
                &asset.ext,
                asset.size as i64,
                asset.mtime as i64,
            ])?;
        }
    }
    conn.execute(
        "INSERT INTO media_dirs (dir, depth, stamp, indexed_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(dir) DO UPDATE SET depth = ?2, stamp = ?3, indexed_at = ?4",
        params![dir, depth, stamp, now_secs()],
    )?;
    Ok(())
}

/// 丢一个目录的索引行（应用自己原地覆盖写之后调用）
pub fn forget_dir(conn: &Connection, dir: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM media_assets WHERE dir = ?1", params![dir])?;
    conn.execute("DELETE FROM media_dirs WHERE dir = ?1", params![dir])?;
    Ok(())
}

/// 作废所有「新鲜窗口」标记（任何写操作后调用，下一次请求重新同步）
pub fn invalidate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM media_roots", [])?;
    Ok(())
}

/// 删掉作用域内**这次没走到**的目录行（外部删目录后索引不能留死行）
pub fn prune(
    conn: &Connection,
    scope: &Scope,
    visited: &std::collections::HashSet<String>,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT dir FROM media_dirs
         WHERE (dir = ?1 OR instr(dir, ?2) = 1) AND depth BETWEEN ?3 AND ?4",
    )?;
    let known: Vec<String> = stmt
        .query_map(
            params![
                &scope.root,
                scope.prefix(),
                scope.min_depth,
                scope.max_depth
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut removed = 0usize;
    for dir in known {
        if visited.contains(&dir) {
            continue;
        }
        forget_dir(conn, &dir)?;
        removed += 1;
    }
    Ok(removed)
}

/// 作用域内各类型总数 `(image, video, audio)`：不受 `kind` 过滤影响
pub fn totals(conn: &Connection, scope: &Scope) -> rusqlite::Result<(usize, usize, usize)> {
    let sql = format!(
        "SELECT a.kind, COUNT(*) FROM media_assets a WHERE {} GROUP BY a.kind",
        Scope::clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut totals = (0usize, 0usize, 0usize);
    let rows = stmt.query_map(params_from_iter(scope.params()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (kind, count) = row?;
        let count = count.max(0) as usize;
        match kind.as_str() {
            "image" => totals.0 = count,
            "video" => totals.1 = count,
            "audio" => totals.2 = count,
            _ => {}
        }
    }
    Ok(totals)
}

/// 一页媒体（mtime 降序，同 mtime 用展示路径升序兜底）
///
/// 返回 `(本页, 是否还有下一页)`；游标与排序键严格一致，翻页不重不漏。
pub fn page(
    conn: &Connection,
    scope: &Scope,
    kinds: &[&str],
    cursor: Option<&(u64, String)>,
    limit: usize,
) -> rusqlite::Result<(Vec<Asset>, bool)> {
    let mut sql = String::from(
        "SELECT a.name, a.path, a.real, a.dir, a.kind, a.ext, a.size, a.mtime
         FROM media_assets a WHERE ",
    );
    sql.push_str(Scope::clause());
    sql.push_str(" AND ");
    sql.push_str(&kinds_clause(kinds));
    if cursor.is_some() {
        sql.push_str(" AND (a.mtime < ? OR (a.mtime = ? AND a.path > ?))");
    }
    sql.push_str(" ORDER BY a.mtime DESC, a.path ASC LIMIT ?");

    let mut values = scope.params();
    values.extend(kinds_params(kinds));
    if let Some((mtime, path)) = cursor {
        values.push(Value::Integer(*mtime as i64));
        values.push(Value::Integer(*mtime as i64));
        values.push(Value::Text(path.clone()));
    }
    // 多取一条用来判断 has_more，省掉一次 COUNT
    values.push(Value::Integer(limit as i64 + 1));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), map_asset)?;
    let mut items = Vec::with_capacity(limit.min(256));
    for row in rows {
        items.push(row?);
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok((items, has_more))
}

fn map_asset(row: &rusqlite::Row<'_>) -> rusqlite::Result<Asset> {
    Ok(Asset {
        name: row.get(0)?,
        path: row.get(1)?,
        real: row.get(2)?,
        dir: row.get(3)?,
        kind: row.get(4)?,
        ext: row.get(5)?,
        size: row.get::<_, i64>(6)?.max(0) as u64,
        mtime: row.get::<_, i64>(7)?.max(0) as u64,
    })
}

/// 相册墙一页：按目录聚合
///
/// `by_path = true` 走路径序（同父目录的册子挨在一起），否则按册内最新一张的时间倒序。
pub fn album_page(
    conn: &Connection,
    scope: &Scope,
    kinds: &[&str],
    by_path: bool,
    cursor: Option<&(u64, String)>,
    limit: usize,
) -> rusqlite::Result<(Vec<AlbumRow>, bool)> {
    let mut sql = String::from(
        "SELECT a.dir, COUNT(*) AS c, MAX(a.mtime) AS m,
                MIN(CASE WHEN a.mtime > 0 THEN a.mtime END) AS f
         FROM media_assets a WHERE ",
    );
    sql.push_str(Scope::clause());
    sql.push_str(" AND ");
    sql.push_str(&kinds_clause(kinds));
    sql.push_str(" GROUP BY a.dir");

    let mut values = scope.params();
    values.extend(kinds_params(kinds));

    if let Some((mtime, path)) = cursor {
        if by_path {
            sql.push_str(
                " HAVING lower(a.dir) > lower(?) OR (lower(a.dir) = lower(?) AND a.dir > ?)",
            );
            values.push(Value::Text(path.clone()));
            values.push(Value::Text(path.clone()));
            values.push(Value::Text(path.clone()));
        } else {
            sql.push_str(" HAVING MAX(a.mtime) < ? OR (MAX(a.mtime) = ? AND a.dir > ?)");
            values.push(Value::Integer(*mtime as i64));
            values.push(Value::Integer(*mtime as i64));
            values.push(Value::Text(path.clone()));
        }
    }

    if by_path {
        sql.push_str(" ORDER BY lower(a.dir) ASC, a.dir ASC LIMIT ?");
    } else {
        sql.push_str(" ORDER BY MAX(a.mtime) DESC, a.dir ASC LIMIT ?");
    }
    values.push(Value::Integer(limit as i64 + 1));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(AlbumRow {
            dir: row.get(0)?,
            count: row.get::<_, i64>(1)?.max(0) as usize,
            mtime: row.get::<_, Option<i64>>(2)?.unwrap_or(0).max(0) as u64,
            first_mtime: row.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
        })
    })?;
    let mut items = Vec::with_capacity(limit.min(256));
    for row in rows {
        items.push(row?);
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok((items, has_more))
}

/// 相册总数（`GROUP BY dir` 的行数）
pub fn album_total(conn: &Connection, scope: &Scope, kinds: &[&str]) -> rusqlite::Result<usize> {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM media_assets a WHERE {} AND {} GROUP BY a.dir)",
        Scope::clause(),
        kinds_clause(kinds)
    );
    let mut values = scope.params();
    values.extend(kinds_params(kinds));
    let count: i64 = conn.query_row(&sql, params_from_iter(values), |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// 某本册子的封面：册内最新的一条
pub fn album_cover(
    conn: &Connection,
    dir: &str,
    kinds: &[&str],
) -> rusqlite::Result<Option<Asset>> {
    use rusqlite::OptionalExtension;
    let sql = format!(
        "SELECT a.name, a.path, a.real, a.dir, a.kind, a.ext, a.size, a.mtime
         FROM media_assets a WHERE a.dir = ? AND {}
         ORDER BY a.mtime DESC, a.path ASC LIMIT 1",
        kinds_clause(kinds)
    );
    let mut values = vec![Value::Text(dir.to_string())];
    values.extend(kinds_params(kinds));
    conn.query_row(&sql, params_from_iter(values), map_asset)
        .optional()
}

/// 上次完整同步：`(深度, 时刻)`
pub fn synced(conn: &Connection, root: &str) -> rusqlite::Result<Option<(i64, u64)>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT depth, synced_at FROM media_roots WHERE root = ?1",
        params![root],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64)),
    )
    .optional()
}

/// 记录一次完整同步（深度取最大值：先用 depth=2 看过、再看 depth=3 时窗口自然扩大）
pub fn mark_synced(conn: &Connection, root: &str, depth: usize) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO media_roots (root, depth, synced_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(root) DO UPDATE SET depth = MAX(depth, ?2), synced_at = ?3",
        params![root, depth as i64, now_secs()],
    )?;
    Ok(())
}

/// 只把「新鲜窗口」往后推，不改深度
///
/// 作用：一页要 `build_items`（读图头 / 识字幕）花上几秒，窗口若从「走完树」算起，
/// 用户开始往下翻时早就过期了，于是又走一遍树。所以**每成功服务一页就续一次** ——
/// 连续浏览期间索引一直热，停手 [`FRESH`] 秒后下一次请求才重新核对目录。
pub fn touch_synced(conn: &Connection, root: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE media_roots SET synced_at = ?2 WHERE root = ?1",
        params![root, now_secs()],
    )?;
    Ok(())
}

/// 批量读图片像素宽高（键 = 真实路径串）
///
/// 只认「路径 + mtime + 大小」三项都对的缓存行：图被换掉（哪怕同名）就必须重读。
pub fn dims_get_many(
    conn: &Connection,
    keys: &[(&str, u64, u64)],
) -> std::collections::HashMap<String, (u32, u32)> {
    use std::collections::HashMap;
    let mut hits = HashMap::new();
    if keys.is_empty() {
        return hits;
    }
    let Ok(mut stmt) = conn
        .prepare_cached("SELECT w, h FROM media_dims WHERE path = ?1 AND mtime = ?2 AND size = ?3")
    else {
        return hits;
    };
    for (path, mtime, size) in keys {
        if let Ok((w, h)) = stmt.query_row(params![path, *mtime as i64, *size as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        }) {
            hits.insert((*path).to_string(), (w as u32, h as u32));
        }
    }
    hits
}

/// 写回新读到的像素宽高（只写 miss 的那些，一页最多 120 行）
pub fn dims_put_many(conn: &Connection, rows: &[(&str, u64, u64, u32, u32)]) {
    if rows.is_empty() {
        return;
    }
    let Ok(mut stmt) = conn.prepare_cached(
        "INSERT OR REPLACE INTO media_dims (path, mtime, size, w, h)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    ) else {
        return;
    };
    for (path, mtime, size, w, h) in rows {
        let _ = stmt.execute(params![
            path,
            *mtime as i64,
            *size as i64,
            *w as i64,
            *h as i64
        ]);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("asmoday-media-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 走正式迁移建库，顺带验证 v5 的建表语句
    fn index(tag: &str) -> MediaIndex {
        let dir = scratch(tag);
        let db = dir.join("asmoday.db");
        let store = crate::core::auth::AuthStore::new(db.clone());
        store.init().unwrap();
        MediaIndex::new(db)
    }

    fn asset(dir: &str, name: &str, mtime: u64) -> Asset {
        Asset {
            name: name.to_string(),
            path: format!("{dir}/{name}"),
            real: format!("{dir}/{name}"),
            dir: dir.to_string(),
            kind: "image".to_string(),
            ext: "jpg".to_string(),
            size: 1,
            mtime,
        }
    }

    #[test]
    fn keyset_paging_covers_every_item_once() {
        let index = index("paging");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/data/photos", 2);
                replace_dir(
                    conn,
                    "/data/photos",
                    scope.min_depth,
                    1,
                    &(0..7)
                        .map(|i| asset("/data/photos", &format!("p{i}.jpg"), 100 + i))
                        .collect::<Vec<_>>(),
                )?;
                replace_dir(
                    conn,
                    "/data/photos/sub",
                    scope.min_depth + 1,
                    1,
                    &[asset("/data/photos/sub", "s.jpg", 200)],
                )?;
                replace_dir(
                    conn,
                    "/data/other",
                    segments("/data/other"),
                    1,
                    &[asset("/data/other", "x.jpg", 300)],
                )?;

                let mut seen = Vec::new();
                let mut cursor: Option<(u64, String)> = None;
                let mut rounds = 0;
                loop {
                    rounds += 1;
                    assert!(rounds < 20, "分页没有收敛");
                    let (items, more) = page(conn, &scope, &["image"], cursor.as_ref(), 3)?;
                    for item in &items {
                        seen.push(item.path.clone());
                    }
                    if !more {
                        break;
                    }
                    let last = items
                        .last()
                        .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                    cursor = Some((last.mtime, last.path.clone()));
                }

                assert_eq!(seen.len(), 8, "根 + 子目录共 8 张，作用域外的那张不能进来");
                let mut unique = seen.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), seen.len(), "翻页出现重复条目");
                assert!(seen[0].ends_with("s.jpg"), "新的在前");
                // 深度：sub 的深度是 2，depth=2 能看到；再深一层就超范围
                assert!(seen.iter().all(|path| !path.contains("/data/other")));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn totals_ignore_kind_filter_and_respect_depth() {
        let index = index("totals");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/data", 1);
                let mut mixed = vec![asset("/data", "a.jpg", 10), asset("/data", "b.jpg", 11)];
                mixed.push(Asset {
                    kind: "video".to_string(),
                    ext: "mp4".to_string(),
                    ..asset("/data", "c.mp4", 12)
                });
                replace_dir(conn, "/data", scope.min_depth, 1, &mixed)?;
                replace_dir(
                    conn,
                    "/data/deep/deeper",
                    scope.min_depth + 2,
                    1,
                    &[asset("/data/deep/deeper", "z.jpg", 99)],
                )?;

                assert_eq!(totals(conn, &scope)?, (2, 1, 0), "深度外的不能算进来");
                let (items, more) = page(conn, &scope, &["image"], None, 120)?;
                assert_eq!(items.len(), 2, "只取 image");
                assert!(!more);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn replace_dir_drops_rows_that_vanished() {
        let index = index("replace");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/data", 0);
                replace_dir(
                    conn,
                    "/data",
                    scope.min_depth,
                    1,
                    &[
                        asset("/data", "old.jpg", 10),
                        asset("/data", "keep.jpg", 11),
                    ],
                )?;
                replace_dir(
                    conn,
                    "/data",
                    scope.min_depth,
                    2,
                    &[asset("/data", "keep.jpg", 12)],
                )?;
                let (items, _) = page(conn, &scope, &["image"], None, 10)?;
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].path, "/data/keep.jpg");
                assert_eq!(dir_stamp(conn, "/data")?, Some(2));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn prune_removes_dirs_that_were_not_visited() {
        let index = index("prune");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/data", 1);
                replace_dir(
                    conn,
                    "/data",
                    scope.min_depth,
                    1,
                    &[asset("/data", "a.jpg", 1)],
                )?;
                replace_dir(
                    conn,
                    "/data/gone",
                    scope.min_depth + 1,
                    1,
                    &[asset("/data/gone", "b.jpg", 2)],
                )?;

                let removed = prune(conn, &scope, &HashSet::from(["/data".to_string()]))?;
                assert_eq!(removed, 1);
                let (items, _) = page(conn, &scope, &["image"], None, 10)?;
                assert_eq!(items.len(), 1, "死目录的条目必须一起走");
                assert_eq!(items[0].path, "/data/a.jpg");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn albums_group_by_dir_and_cover_is_newest() {
        let index = index("albums");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/lib", 2);
                replace_dir(
                    conn,
                    "/lib/2020",
                    scope.min_depth + 1,
                    1,
                    &[
                        asset("/lib/2020", "a.jpg", 100),
                        asset("/lib/2020", "b.jpg", 300),
                    ],
                )?;
                replace_dir(
                    conn,
                    "/lib/2021",
                    scope.min_depth + 1,
                    1,
                    &[asset("/lib/2021", "c.jpg", 200)],
                )?;
                replace_dir(
                    conn,
                    "/lib/empty",
                    scope.min_depth + 1,
                    1,
                    &[Asset {
                        kind: "text".to_string(),
                        ext: "txt".to_string(),
                        ..asset("/lib/empty", "note.txt", 5)
                    }],
                )?;

                let (rows, more) = album_page(conn, &scope, &["image"], false, None, 10)?;
                assert!(!more);
                assert_eq!(rows.len(), 2, "没有图片的目录不上架");
                assert_eq!(rows[0].dir, "/lib/2020", "按册内最新一张倒序");
                assert_eq!(rows[0].count, 2);
                assert_eq!(rows[0].first_mtime, 100);
                assert_eq!(album_total(conn, &scope, &["image"])?, 2);

                let cover = album_cover(conn, "/lib/2020", &["image"])?
                    .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                assert!(cover.path.ends_with("b.jpg"));

                // 路径序：同一父目录的册子挨在一起
                let (by_path, _) = album_page(conn, &scope, &["image"], true, None, 10)?;
                assert_eq!(by_path[0].dir, "/lib/2020");
                assert_eq!(by_path[1].dir, "/lib/2021");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn synced_window_records_the_deepest_depth() {
        let index = index("synced");
        index
            .with_conn(|conn| {
                assert_eq!(synced(conn, "/lib")?, None);
                mark_synced(conn, "/lib", 2)?;
                mark_synced(conn, "/lib", 3)?;
                mark_synced(conn, "/lib", 1)?;
                let (depth, at) =
                    synced(conn, "/lib")?.ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                assert_eq!(depth, 3, "深度只增不减，否则每次看相册墙都要重扫");
                assert!(at > 0);
                invalidate(conn)?;
                assert_eq!(synced(conn, "/lib")?, None, "写操作后窗口失效");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn segments_count_unc_and_drive_roots_alike() {
        assert_eq!(segments("//192.168.1.1/anas/Photos"), 3);
        assert_eq!(segments("C:/Users/Sun"), 3);
        assert_eq!(segments("/data"), 1);
        // 盘符根不能让前缀多出一个斜杠
        assert_eq!(Scope::new("C:/", 1).prefix(), "C:/");
        assert_eq!(Scope::new("//srv/share", 1).prefix(), "//srv/share/");
    }
}
