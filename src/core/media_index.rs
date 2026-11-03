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
/// v4.8.0 起新鲜度不再由请求来维护：`spawn_media_indexer` 每
/// `[media].index_scan_interval_secs`（默认 300）在后台走一遍树，只重写目录戳
/// 变了的目录。所以这个窗口要**明显大于**后台周期 —— 10 分钟 = 两个周期，
/// 后台偶发跳一拍也不会掉回「用户请求自己扫」那条慢路径。
///
/// 外部新增文件的最大可见延迟就是后台周期（默认 5 分钟），不是这个窗口；
/// 窗口只管「请求要不要自己动 IO」。应用自己的写操作会主动作废索引
/// （`forget_parent_snapshot`），点「重新扫描」也会立刻穿透。
pub const FRESH: u64 = 600;
/// 一个写事务里最多写多少个目录。
///
/// 事务**只包住写**（readdir 在事务外，见 `walk_into_index`），所以这个数字决定的是
/// 「写锁被持有多久」而不是「一次读盘有多久」：16 个目录的 `DELETE`+`INSERT` 是毫秒级，
/// 其他写操作（登录、上传、缩略图缓存）排队等它一下就过去了。以前事务横跨 readdir，
/// 首次建库能把 `auth/setup` 直接卡成 `database is locked`。
pub const CHUNK: usize = 16;

/// 排序维度（v4.9.0）。
///
/// 只有这四个维度是**能走索引**的：时间、名称、体积、条目数。拍摄时间不在此列 ——
/// 索引里存的是文件 mtime，不是 EXIF 拍摄时间，两者在「照片从相机导出」的场景里
/// 经常差好几年，混用会让排序结果看起来随机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Mtime,
    /// 名称：媒体按 [`natural_key`] 归一化后的列，相册按目录路径小写
    Name,
    Size,
    /// 只有相册墙有「条目数」这个概念
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// 一次排序：维度 + 方向
#[derive(Debug, Clone, Copy)]
pub struct Sort {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Sort {
    pub const fn new(key: SortKey, dir: SortDir) -> Self {
        Self { key, dir }
    }

    /// 解析 `?sort=` 参数。
    ///
    /// 白名单而不是自由拼接：这个值会进 SQL 的 `ORDER BY`，绝不能透传。
    /// 旧值 `mtime` / `path` 必须继续认（老书签、老前端缓存的 URL 还在用）。
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let raw = raw.unwrap_or("").trim().to_ascii_lowercase();
        let parsed = match raw.as_str() {
            "" | "mtime" | "mtime_desc" => Self::new(SortKey::Mtime, SortDir::Desc),
            "mtime_asc" | "oldest" => Self::new(SortKey::Mtime, SortDir::Asc),
            "path" | "name" | "name_asc" => Self::new(SortKey::Name, SortDir::Asc),
            "name_desc" => Self::new(SortKey::Name, SortDir::Desc),
            "size_desc" | "size" => Self::new(SortKey::Size, SortDir::Desc),
            "size_asc" => Self::new(SortKey::Size, SortDir::Asc),
            "count_desc" => Self::new(SortKey::Count, SortDir::Desc),
            "count_asc" => Self::new(SortKey::Count, SortDir::Asc),
            other => return Err(format!("不支持的排序：{other}")),
        };
        Ok(parsed)
    }

    pub fn is_desc(&self) -> bool {
        matches!(self.dir, SortDir::Desc)
    }

    /// `ORDER BY` 里的方向关键字
    pub fn sql_dir(&self) -> &'static str {
        match self.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        }
    }

    /// keyset 游标的主键比较符：降序时「在游标之后」= 更小
    pub fn sql_op(&self) -> &'static str {
        match self.dir {
            SortDir::Asc => ">",
            SortDir::Desc => "<",
        }
    }
}

/// 名称排序键：小写 + 数字段左侧补零，让 SQL 的字符串比较退化成「自然序」。
///
/// SQLite 没有自然排序，`第10集` 按字节比会排在 `第2集` 前面。把数字段补到 10 位
/// （`0000000002` / `0000000010`）之后，二进制比较的结果就和人类预期一致，
/// 于是名称排序仍能走索引 + keyset 分页，不必把整页拉回内存重排。
pub fn natural_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let mut digits = String::new();
    for ch in name.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        flush_digits(&mut out, &mut digits);
        for low in ch.to_lowercase() {
            out.push(low);
        }
    }
    flush_digits(&mut out, &mut digits);
    out
}

fn flush_digits(out: &mut String, digits: &mut String) {
    if digits.is_empty() {
        return;
    }
    let trimmed = digits.trim_start_matches('0');
    for _ in 0..10usize.saturating_sub(trimmed.len()) {
        out.push('0');
    }
    out.push_str(trimmed);
    digits.clear();
}

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
    /// 册内媒体总字节数（按体积排序 + 卡片副标题）
    pub bytes: u64,
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
             (path, real, dir, depth, name, kind, ext, size, mtime, sort_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                natural_key(&asset.name),
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

/// `path` 是否落在 `root` 下面（含根本身）。
///
/// 段边界必须严格：`/mnt/a` 不能吃掉 `/mnt/ab`。Windows 盘符与 UNC 都不区分大小写，
/// 而展示串统一用 `/`，所以这里只做「忽略大小写的前缀 + 段边界」判定。
fn under(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return false;
    }
    // 用 `get` 而不是直接切字节：中文目录名在字节中途截断会 panic
    let Some(head) = path.get(..root.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(root) {
        return false;
    }
    matches!(path.as_bytes().get(root.len()), None | Some(b'/'))
}

/// 丢掉不属于任何当前挂载点的索引行（v4.8.1）。
///
/// 换挂载点之后旧根的 `media_assets` / `media_dirs` / `media_roots` 没人清：查询按
/// root 前缀过滤，旧行不会串进新视图，但它们会永久占库；更糟的是配置回落到
/// 「挂载点全部离线 → cwd 隐式根」时，上一次挂载留下的相册会原样冒出来
/// （2026-11-03「换了挂载点图片库还显示旧目录」就是这个 + 前端快照）。
pub fn retain_roots(conn: &Connection, keep: &[String]) -> rusqlite::Result<usize> {
    let keep: Vec<String> = keep
        .iter()
        .map(|root| root.trim_end_matches('/').to_string())
        .filter(|root| !root.is_empty())
        .collect();

    let mut stmt = conn.prepare("SELECT dir FROM media_dirs")?;
    let dirs: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut stmt = conn.prepare("SELECT root FROM media_roots")?;
    let roots: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let tx = conn.unchecked_transaction()?;
    let mut removed = 0usize;
    for dir in dirs {
        if keep.iter().any(|root| under(root, &dir)) {
            continue;
        }
        forget_dir(&tx, &dir)?;
        removed += 1;
    }
    for root in roots {
        let stale = !keep
            .iter()
            .any(|keep| keep.eq_ignore_ascii_case(root.trim_end_matches('/')));
        if stale {
            tx.execute("DELETE FROM media_roots WHERE root = ?1", params![root])?;
        }
    }
    // 目录行没了、条目还在的孤儿（旧版本 forget_dir 只删一头的残留）
    tx.execute(
        "DELETE FROM media_assets WHERE dir NOT IN (SELECT dir FROM media_dirs)",
        [],
    )?;
    tx.commit()?;
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

/// 一页媒体（默认 mtime 降序，同 mtime 用展示路径升序兜底）
///
/// 游标是 `(主键, 展示路径)`：主键的形态由 `sort.key` 决定 —— 时间是毫秒戳、
/// 名称是 [`natural_key`] 串、体积是字节数。**主键与次级键的顺序必须和
/// `ORDER BY` 完全一致**，否则翻页会重复或漏项。降序时比较符翻成 `<`。
///
/// 返回 `(本页, 是否还有下一页)`。
pub fn page(
    conn: &Connection,
    scope: &Scope,
    kinds: &[&str],
    sort: Sort,
    cursor: Option<&(String, String)>,
    limit: usize,
) -> rusqlite::Result<(Vec<Asset>, bool)> {
    let key = key_column(sort.key);
    let mut sql = String::from(
        "SELECT a.name, a.path, a.real, a.dir, a.kind, a.ext, a.size, a.mtime
         FROM media_assets a WHERE ",
    );
    sql.push_str(Scope::clause());
    sql.push_str(" AND ");
    sql.push_str(&kinds_clause(kinds));
    if cursor.is_some() {
        let op = sort.sql_op();
        sql.push_str(&format!(
            " AND ({key} {op} ? OR ({key} = ? AND a.path > ?))"
        ));
    }
    sql.push_str(&format!(
        " ORDER BY {key} {}, a.path ASC LIMIT ?",
        sort.sql_dir()
    ));

    let mut values = scope.params();
    values.extend(kinds_params(kinds));
    if let Some((cursor_key, path)) = cursor {
        let key_value = cursor_value(sort.key, cursor_key);
        values.push(key_value.clone());
        values.push(key_value);
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

/// 媒体表的排序列表达式。`Count` 在媒体列表里没有意义，退化成时间（调用方应先拦掉）
fn key_column(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "a.sort_name",
        SortKey::Size => "a.size",
        SortKey::Mtime | SortKey::Count => "a.mtime",
    }
}

/// 游标主键 → SQL 参数值（名称是文本，其余是整数）
fn cursor_value(key: SortKey, raw: &str) -> Value {
    match key {
        SortKey::Name => Value::Text(raw.to_string()),
        _ => Value::Integer(raw.parse::<i64>().unwrap_or(0)),
    }
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
/// 排序维度：`Name` 走目录路径小写（同父目录的册子挨在一起）、`Mtime` 走册内
/// 最新一张、`Count` 走条目数、`Size` 走占用空间。四种都支持升降序，游标口径与
/// `ORDER BY` 严格同源。
pub fn album_page(
    conn: &Connection,
    scope: &Scope,
    kinds: &[&str],
    sort: Sort,
    cursor: Option<&(String, String)>,
    limit: usize,
) -> rusqlite::Result<(Vec<AlbumRow>, bool)> {
    let key = album_key_expr(sort.key);
    let mut sql = String::from(
        "SELECT a.dir, COUNT(*) AS c, MAX(a.mtime) AS m,
                MIN(CASE WHEN a.mtime > 0 THEN a.mtime END) AS f,
                SUM(a.size) AS b
         FROM media_assets a WHERE ",
    );
    sql.push_str(Scope::clause());
    sql.push_str(" AND ");
    sql.push_str(&kinds_clause(kinds));
    sql.push_str(" GROUP BY a.dir");

    let mut values = scope.params();
    values.extend(kinds_params(kinds));

    if let Some((cursor_key, dir)) = cursor {
        let op = sort.sql_op();
        sql.push_str(&format!(
            " HAVING {key} {op} ? OR ({key} = ? AND a.dir > ?)"
        ));
        let value = match sort.key {
            SortKey::Name => Value::Text(cursor_key.to_lowercase()),
            _ => Value::Integer(cursor_key.parse::<i64>().unwrap_or(0)),
        };
        values.push(value.clone());
        values.push(value);
        values.push(Value::Text(dir.clone()));
    }

    sql.push_str(&format!(
        " ORDER BY {key} {}, a.dir ASC LIMIT ?",
        sort.sql_dir()
    ));
    values.push(Value::Integer(limit as i64 + 1));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(AlbumRow {
            dir: row.get(0)?,
            count: row.get::<_, i64>(1)?.max(0) as usize,
            mtime: row.get::<_, Option<i64>>(2)?.unwrap_or(0).max(0) as u64,
            first_mtime: row.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
            bytes: row.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
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

/// 相册聚合后的排序表达式（与 `album_page` 的 `ORDER BY` / `HAVING` 必须一致）
fn album_key_expr(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "lower(a.dir)",
        SortKey::Count => "COUNT(*)",
        SortKey::Size => "SUM(a.size)",
        SortKey::Mtime => "MAX(a.mtime)",
    }
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

    /// 测试里最常用的排序：新的在前
    const NEWEST: Sort = Sort::new(SortKey::Mtime, SortDir::Desc);

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
                let mut cursor: Option<(String, String)> = None;
                let mut rounds = 0;
                loop {
                    rounds += 1;
                    assert!(rounds < 20, "分页没有收敛");
                    let (items, more) = page(conn, &scope, &["image"], NEWEST, cursor.as_ref(), 3)?;
                    for item in &items {
                        seen.push(item.path.clone());
                    }
                    if !more {
                        break;
                    }
                    let last = items
                        .last()
                        .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                    cursor = Some((last.mtime.to_string(), last.path.clone()));
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
                let (items, more) = page(conn, &scope, &["image"], NEWEST, None, 120)?;
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
                let (items, _) = page(conn, &scope, &["image"], NEWEST, None, 10)?;
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
                let (items, _) = page(conn, &scope, &["image"], NEWEST, None, 10)?;
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

                let (rows, more) = album_page(conn, &scope, &["image"], NEWEST, None, 10)?;
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
                let (by_path, _) = album_page(
                    conn,
                    &scope,
                    &["image"],
                    Sort::new(SortKey::Name, SortDir::Asc),
                    None,
                    10,
                )?;
                assert_eq!(by_path[0].dir, "/lib/2020");
                assert_eq!(by_path[1].dir, "/lib/2021");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn natural_key_orders_numbers_like_a_human() {
        assert!(natural_key("第2集") < natural_key("第10集"));
        assert!(natural_key("IMG_2.jpg") < natural_key("IMG_10.jpg"));
        assert!(
            natural_key("a1") < natural_key("a01b"),
            "前导零不能改变大小关系"
        );
        assert_eq!(natural_key("AbC.TXT"), "abc.txt", "大小写必须折叠");
        assert_eq!(natural_key("v1.2.10"), "v0000000001.0000000002.0000000010");
    }

    /// v4.9.0：四个维度 × 两个方向，每一页都靠游标接上，翻了不能重也不能漏
    #[test]
    fn every_sort_dimension_pages_without_gaps() {
        let index = index("sorts");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/lib", 1);
                let mut assets = Vec::new();
                for i in 0..23u64 {
                    let mut one = asset("/lib", &format!("ep{i}.jpg"), 100 + (i % 5));
                    // 体积与时间反着来，两个维度不会「碰巧一样」掩盖 bug
                    one.size = 1000 - i;
                    assets.push(one);
                }
                replace_dir(conn, "/lib", scope.min_depth, 1, &assets)?;

                let sorts = [
                    Sort::new(SortKey::Mtime, SortDir::Desc),
                    Sort::new(SortKey::Mtime, SortDir::Asc),
                    Sort::new(SortKey::Name, SortDir::Desc),
                    Sort::new(SortKey::Name, SortDir::Asc),
                    Sort::new(SortKey::Size, SortDir::Desc),
                    Sort::new(SortKey::Size, SortDir::Asc),
                ];
                for sort in sorts {
                    let mut seen = Vec::new();
                    let mut cursor: Option<(String, String)> = None;
                    let mut rounds = 0;
                    loop {
                        rounds += 1;
                        assert!(rounds < 40, "{sort:?} 分页没有收敛");
                        let (items, more) =
                            page(conn, &scope, &["image"], sort, cursor.as_ref(), 5)?;
                        for item in &items {
                            seen.push(item.path.clone());
                        }
                        if items.is_empty() {
                            assert!(!more, "{sort:?} 空页却还有 next");
                            break;
                        }
                        if !more {
                            break;
                        }
                        let last = items
                            .last()
                            .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                        let key = match sort.key {
                            SortKey::Name => natural_key(&last.name),
                            SortKey::Size => last.size.to_string(),
                            _ => last.mtime.to_string(),
                        };
                        cursor = Some((key, last.path.clone()));
                    }
                    assert_eq!(seen.len(), 23, "{sort:?} 漏项或重复");
                    let mut unique = seen.clone();
                    unique.sort();
                    unique.dedup();
                    assert_eq!(unique.len(), 23, "{sort:?} 翻页出现重复");
                }
                Ok(())
            })
            .unwrap();
    }

    /// 相册墙的条目数 / 体积排序也要走同一套游标
    #[test]
    fn album_sorts_by_count_and_size() {
        let index = index("album-sorts");
        index
            .with_conn(|conn| {
                let scope = Scope::new("/lib", 1);
                // bytes = count × size：a=300、b=150、c=400
                for (dir, count, size) in [("a", 1u64, 300u64), ("b", 3, 50), ("c", 2, 200)] {
                    let assets: Vec<Asset> = (0..count)
                        .map(|i| {
                            let mut one = asset(&format!("/lib/{dir}"), &format!("p{i}.jpg"), 10);
                            one.size = size;
                            one
                        })
                        .collect();
                    replace_dir(
                        conn,
                        &format!("/lib/{dir}"),
                        scope.min_depth + 1,
                        1,
                        &assets,
                    )?;
                }

                let (rows, _) = album_page(
                    conn,
                    &scope,
                    &["image"],
                    Sort::new(SortKey::Count, SortDir::Desc),
                    None,
                    10,
                )?;
                let order: Vec<&str> = rows.iter().map(|row| row.dir.as_str()).collect();
                assert_eq!(order, vec!["/lib/b", "/lib/c", "/lib/a"], "条目多的在前");

                let (rows, _) = album_page(
                    conn,
                    &scope,
                    &["image"],
                    Sort::new(SortKey::Size, SortDir::Asc),
                    None,
                    10,
                )?;
                let order: Vec<&str> = rows.iter().map(|row| row.dir.as_str()).collect();
                assert_eq!(
                    order,
                    vec!["/lib/b", "/lib/a", "/lib/c"],
                    "按总体积从小到大"
                );
                assert_eq!(rows[2].bytes, 400, "体积是册内累加，不是单张");

                // 游标：count 排序每页 1 本，三页走完不重不漏
                let sort = Sort::new(SortKey::Count, SortDir::Desc);
                let mut seen = Vec::new();
                let mut cursor: Option<(String, String)> = None;
                loop {
                    let (items, more) =
                        album_page(conn, &scope, &["image"], sort, cursor.as_ref(), 1)?;
                    if items.is_empty() {
                        break;
                    }
                    seen.extend(items.iter().map(|row| row.dir.clone()));
                    if !more {
                        break;
                    }
                    let last = items
                        .last()
                        .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
                    cursor = Some((last.count.to_string(), last.dir.clone()));
                }
                assert_eq!(seen.len(), 3);
                let mut unique = seen.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), 3, "相册翻页出现重复");
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
    fn retain_roots_drops_stale_mounts() {
        let index = index("retain");
        index
            .with_conn(|conn| {
                replace_dir(
                    conn,
                    "//srv/share/Photos",
                    3,
                    1,
                    &[asset("//srv/share/Photos", "a.jpg", 10)],
                )?;
                mark_synced(conn, "//srv/share/", 3)?;
                // 换掉的旧挂载点：换完之后这棵树再也不会有请求走到
                replace_dir(
                    conn,
                    "C:/old/examples",
                    4,
                    1,
                    &[asset("C:/old/examples", "b.jpg", 20)],
                )?;
                mark_synced(conn, "C:/old", 3)?;

                assert_eq!(
                    retain_roots(conn, &["//srv/share/".to_string()])?,
                    1,
                    "旧根整棵清掉，当前根一行不动"
                );
                let (image, _, _) = totals(conn, &Scope::new("//srv/share", 2))?;
                assert_eq!(image, 1);
                let (stale, _, _) = totals(conn, &Scope::new("C:/old", 3))?;
                assert_eq!(stale, 0, "旧挂载点的条目不能留着");
                assert_eq!(synced(conn, "C:/old")?, None, "废弃根的新鲜窗口也要删");
                assert!(synced(conn, "//srv/share/")?.is_some());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn retain_roots_respects_segment_boundaries() {
        assert!(under("/mnt/a", "/mnt/a"));
        assert!(under("/mnt/a/", "/mnt/a/b.jpg"));
        assert!(!under("/mnt/a", "/mnt/ab"), "段边界不能靠字符串前缀");
        // 盘符根与 UNC 都按忽略大小写比：Windows 上 `Z:/` 与 `z:/` 是同一个盘
        assert!(under("C:/", "C:/Users"));
        assert!(under("c:/", "C:/Users"));
        assert!(!under("", "/data"));
        // 中文目录名：按字节切会在多字节字符中间断开，必须走 `get`
        assert!(under("/媒体/照片", "/媒体/照片/a.jpg"));
        assert!(!under("/媒体", "/媒"), "短路径不能被当成前缀命中");
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
