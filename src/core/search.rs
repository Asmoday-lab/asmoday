//! 文件名搜索：确定序 DFS + 栈游标分页（设计文档 4.6）。
//!
//! 原来的搜索只看当前目录第一层、凑满 200 条就把 `truncated` 置真 ——
//! 和本地管理器「搜整个文件夹」差得远，用户只能一层层手动翻。这里换成
//! **递归 + 游标续页**，并且不设结果上限：
//!
//! - 遍历顺序是确定的（每层目录内 `目录优先 + 名称升序`），所以「上次停在
//!   哪」可以用一个游标表达；目录中间有增删最多导致少量重复/遗漏，前端按
//!   路径去重即可，不会像 offset 分页那样整体错位。
//! - 游标记的是**遍历栈本身**：`[(目录相对路径, 下一个待访问条目名), …]`。
//!   续页只需重开栈里那几个目录（深度通常 < 10），不用从头再走一遍树，
//!   代价与页数近似无关。编码成 base64url 的 JSON 交给前端原样带回。
//! - **扫描预算**（`budget`）不是结果上限：一页最多访问 N 个条目，用完了
//!   就把游标交出去让前端接着请求。几十万条目、关键词又极窄的目录也能
//!   一路翻到底，只是多几次往返 —— 而不是「只搜到这里」。
//! - 链接目录默认不递归（与列表「指向目录的链接不当作目录」同口径）；
//!   `follow_symlinks = true` 时才进，并用 canonicalize 结果去重防环，
//!   配合 `MAX_DEPTH` 兜底。
//! - 无权限目录直接跳过，不中断整次搜索：搜索的语义是「能找到的都给你」。
//! - **只为命中的条目付一次 `stat`**：`readdir` 已经给出名字与类型，大小只有
//!   真正要展示的那几条才需要；十万条的目录不该为了一个 size 打十万次 `stat`。
//! - **目录快照缓存**（`core/dir_cache.rs`，v4.3.0）：同一个目录如果 mtime 没变，
//!   名字集合必然没变（创建/删除/改名都会推进 mtime），于是下次只 `stat` 目录本身，
//!   不必再 `readdir`。重复搜索一棵没动过的树从「N 次 readdir + N 次 stat」降到
//!   「N 次 stat」。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::dir_cache::{self, CachedEntry, DirCache, KIND_DIR, KIND_LINK};
use super::fs::{self, FsError};

/// 单页默认条数（与原「当前目录 200 条」一致，避免前端行为突变）
pub const DEFAULT_LIMIT: usize = 200;
/// 单页条数上限：再大就是前端在替用户拉一整棵树
pub const MAX_LIMIT: usize = 1000;
/// 单页最多访问的条目数。用完就交游标续页，不是截断。
pub const DEFAULT_BUDGET: usize = 50_000;
/// 递归深度上限（防链接成环、防 `node_modules` 那种极深的树把交互拖死）
pub const MAX_DEPTH: usize = 32;

/// 一次搜索的入参（与 HTTP 层解耦，方便单测直接调）
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// 关键词（调用方传原样，这里统一转小写匹配）
    pub keyword: String,
    pub limit: usize,
    /// 是否递归子目录
    pub recursive: bool,
    /// `[system].follow_symlinks`：为 true 时跟进链接目录（带防环）
    pub follow_symlinks: bool,
    /// 单页扫描预算，见 `DEFAULT_BUDGET`
    pub budget: usize,
}

/// 便捷构造（仅供单测摆场景；HTTP 层直接构造结构体，字段都要显式给）
#[cfg(test)]
impl SearchOptions {
    pub fn new(keyword: impl Into<String>) -> Self {
        Self {
            keyword: keyword.into(),
            limit: DEFAULT_LIMIT,
            recursive: false,
            follow_symlinks: true,
            budget: DEFAULT_BUDGET,
        }
    }

    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// 一条命中。`rel` 是相对搜索根的 POSIX 路径，前端用它告诉用户「在哪一层」。
#[derive(Debug, Clone)]
pub struct Hit {
    pub name: String,
    pub abs: PathBuf,
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 一页结果
#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub hits: Vec<Hit>,
    /// 本页实际访问了多少条目（结果是它的子集；前端可用来显示「已扫描」）
    pub scanned: usize,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// 遍历栈中的一层：某目录下已经按确定序排好的条目 + 下一个待访问下标
struct Frame {
    rel: String,
    abs: PathBuf,
    entries: Vec<CachedEntry>,
    pos: usize,
}

/// 游标里的一层（只有「目录」和「下一个待访问条目名」两样，
/// 因为名字列表可以重读目录重现，不需要一起塞进 URL）
#[derive(Debug, Serialize, Deserialize)]
struct CursorFrame {
    dir: String,
    next: String,
}

/// 递归搜索一页。
///
/// `cursor = None` 从根开始；否则从上次停下的位置继续。
/// `cache` 为 `None` 时退化成「每次都 readdir」（单测与无库场景）。
pub async fn run(
    root: &Path,
    opts: &SearchOptions,
    cursor: Option<&str>,
    cache: Option<&DirCache>,
) -> Result<SearchPage, FsError> {
    let limit = opts.limit.clamp(1, MAX_LIMIT);
    let budget = opts.budget.max(1);
    let keyword = opts.keyword.trim().to_lowercase();
    if keyword.is_empty() {
        return Ok(SearchPage::default());
    }

    let resume = cursor
        .filter(|raw| !raw.is_empty())
        .map(decode_cursor)
        .unwrap_or_default();

    // 栈只在「本次调用」内维护；链接去重集合同样是一次性的，
    // 跨页最坏情况是重复进一次链接目录（结果被前端按路径去重），
    // 换来的是游标不必携带 visited 集合（否则 URL 会随目录规模膨胀）。
    let mut seen_links: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<Frame> = Vec::new();

    if !resume.is_empty() {
        for frame in &resume {
            // 游标里的目录可能已经被删掉/改名：这一层直接放弃，
            // 让上层帧接着往后走（搜索必须能在变化的目录树上继续）。
            if let Ok(opened) = open_frame(root, &frame.dir, Some(&frame.next), cache).await {
                stack.push(opened);
            }
        }
    }
    if stack.is_empty() {
        stack.push(open_frame(root, "", None, cache).await?);
    }

    let mut page = SearchPage::default();
    let mut stopped = false;

    loop {
        // 结果够了 / 预算用完：都算「还有后续」，交给游标续
        if page.hits.len() >= limit || page.scanned >= budget {
            stopped = true;
            break;
        }

        let Some(top) = stack.last() else { break };
        if top.pos >= top.entries.len() {
            // 这一层走完了，回到父层继续
            stack.pop();
            continue;
        }

        let (entry, abs, rel, depth) = {
            let top = stack.last_mut().expect("上一层刚判过非空");
            let entry = top.entries[top.pos].clone();
            top.pos += 1;
            let abs = top.abs.join(&entry.name);
            let rel = if top.rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", top.rel, entry.name)
            };
            (entry, abs, rel, stack.len())
        };
        page.scanned += 1;

        let name = entry.name;
        let matched = name.to_lowercase().contains(&keyword);
        // 类型由 readdir 给定，不必为每条都 stat；大小只有「要展示」时才有意义
        let mut is_dir = entry.kind == KIND_DIR;
        let mut size = 0u64;
        let mut descend = entry.kind == KIND_DIR;

        if entry.kind == KIND_LINK {
            descend = false;
            // 链接目录：默认当作「可以点开但不能穿过去」，
            // 只有显式 follow 时才递归，并且必须没在本次遍历里见过同一真实目录
            let Ok(lmeta) = tokio::fs::symlink_metadata(&abs).await else {
                // 链接本体已经没了：不给点不开的结果
                continue;
            };
            size = lmeta.len();
            if let Ok(target) = tokio::fs::metadata(&abs).await {
                if target.is_dir() {
                    if opts.follow_symlinks {
                        is_dir = true;
                        descend = match tokio::fs::canonicalize(&abs).await {
                            Ok(real) => seen_links.insert(real),
                            Err(_) => false,
                        };
                    } else {
                        // 不跟随也要把大小报对（目标可读时）
                        size = target.len();
                    }
                } else {
                    size = target.len();
                }
            }
        } else if matched {
            // 命中才付一次 lstat：与列表同一套展示口径（目录也给自身大小）
            match tokio::fs::symlink_metadata(&abs).await {
                Ok(lmeta) => size = lmeta.len(),
                Err(_) => continue,
            }
        }

        if matched {
            page.hits.push(Hit {
                name,
                abs: abs.clone(),
                rel: rel.clone(),
                is_dir,
                size,
            });
        }

        if opts.recursive && descend && depth < MAX_DEPTH {
            // 打不开（权限 / 竞态删除）就跳过这一支，不让整次搜索失败
            if let Ok(child) = open_frame(root, &rel, None, cache).await {
                stack.push(child);
            }
        }
    }

    if stopped {
        // 栈顶若刚好已走完，先弹掉再快照 —— 否则游标会指着一个空层，
        // 续页时白跑一趟（结果没错，但多一次 readdir）。
        while stack
            .last()
            .map(|frame| frame.pos >= frame.entries.len())
            .unwrap_or(false)
        {
            stack.pop();
        }
        if let Some(encoded) = snapshot(&stack) {
            page.has_more = true;
            page.next_cursor = Some(encoded);
        }
    }

    Ok(page)
}

/// 打开一层目录：拿到条目、按「目录优先 + 名称升序」排序（缓存命中时顺序已是如此），
/// 并把位置挪到 `resume_at` 之后。
///
/// 缓存命中只需一次 `stat`（目录 mtime 对得上），不必 `readdir`。
async fn open_frame(
    root: &Path,
    rel: &str,
    resume_at: Option<&str>,
    cache: Option<&DirCache>,
) -> Result<Frame, FsError> {
    let abs = if rel.is_empty() {
        root.to_path_buf()
    } else {
        let mut path = root.to_path_buf();
        for segment in rel.split('/') {
            path.push(segment);
        }
        path
    };

    let entries = match (cache, stamp_of_dir(&abs).await) {
        (Some(cache), Some((key, stamp))) => match cache.load(&key, stamp) {
            Some(hit) => hit,
            None => {
                let fresh = read_entries(&abs).await?;
                cache.store(&key, stamp, &fresh);
                fresh
            }
        },
        // 目录刚被动过（戳不可信）或调用方没给缓存：照常读
        _ => read_entries(&abs).await?,
    };

    let pos = match resume_at.filter(|name| !name.is_empty()) {
        // 游标记的是「下一个待访问条目」本身，所以位置就是它 —— 不能再 +1。
        // 名字在目录里唯一，精确匹配比按排序键二分更稳（大小写折叠后可能并列）；
        // 名字找不到（目录内容变了）就当这一层从头来：宁可重复一条，也不漏。
        Some(target) => entries
            .iter()
            .position(|entry| entry.name == target)
            .unwrap_or(0),
        None => 0,
    };

    Ok(Frame {
        rel: rel.to_string(),
        abs,
        entries,
        pos,
    })
}

/// 目录的缓存键与戳；戳不可信（刚动过 / 取不到 mtime）时返回 `None`
async fn stamp_of_dir(abs: &Path) -> Option<(String, i64)> {
    let meta = tokio::fs::metadata(abs).await.ok()?;
    let stamp = dir_cache::stamp_of(meta.modified().ok()?)?;
    Some((fs::normalize_display(abs), stamp))
}

/// 真正读目录：一次 `readdir` 拿名字与类型，按确定序排好。
///
/// 确定序（目录在前、同组按小写名）是游标能复现位置的唯一依据 ——
/// `readdir` 的顺序不可依赖，缓存里存的也必须是这个顺序。
async fn read_entries(abs: &Path) -> Result<Vec<CachedEntry>, FsError> {
    let mut reader = tokio::fs::read_dir(abs)
        .await
        .map_err(|e| FsError::Io(format!("读取目录失败：{e}")))?;

    let mut entries: Vec<CachedEntry> = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| FsError::Io(format!("遍历目录失败：{e}")))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        // 搜索一律显示隐藏项：用户敲了关键词就是明确想看，别被 show_hidden 挡住
        if fs::skip_listing_name(abs, &name, true) {
            continue;
        }
        // file_type 不跟随链接（Windows 上 readdir 自带属性，这里不产生额外系统调用）
        let kind = match entry.file_type().await {
            Ok(ft) if ft.is_symlink() => KIND_LINK,
            Ok(ft) if ft.is_dir() => KIND_DIR,
            _ => dir_cache::KIND_FILE,
        };
        entries.push(CachedEntry { name, kind });
    }
    entries.sort_by_cached_key(|entry| (!entry.is_dir(), entry.name.to_lowercase()));
    Ok(entries)
}

/// 把当前遍历栈编码成游标；栈空表示「已经走完整棵树」，不返回游标。
fn snapshot(stack: &[Frame]) -> Option<String> {
    if stack.is_empty() {
        return None;
    }
    let frames: Vec<CursorFrame> = stack
        .iter()
        .filter_map(|frame| {
            frame.entries.get(frame.pos).map(|next| CursorFrame {
                dir: frame.rel.clone(),
                next: next.name.clone(),
            })
        })
        .collect();
    if frames.is_empty() {
        return None;
    }
    let json = serde_json::to_vec(&frames).ok()?;
    Some(data_encoding::BASE64URL_NOPAD.encode(&json))
}

/// 解游标。坏游标（手工改坏 / 老版本格式）按「从头开始」处理，
/// 与列表分页的口径一致：宁可多给一页，也不要因为一个参数让搜索直接报错。
fn decode_cursor(raw: &str) -> Vec<CursorFrame> {
    let Ok(bytes) = data_encoding::BASE64URL_NOPAD.decode(raw.as_bytes()) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asmoday-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    /// 一棵用来验证「递归 / 非递归 / 分页」的小树
    fn tree(tag: &str) -> PathBuf {
        let root = scratch(tag);
        touch(&root.join("report-2024.pdf"));
        touch(&root.join("notes.txt"));
        touch(&root.join("sub/report-draft.md"));
        touch(&root.join("sub/deep/report-final.md"));
        touch(&root.join("other/.DS_Store"));
        touch(&root.join("other/report-hidden.txt"));
        root
    }

    #[tokio::test]
    async fn non_recursive_only_searches_the_first_level() {
        let root = tree("flat");
        let page = run(&root, &SearchOptions::new("report"), None, None)
            .await
            .unwrap();
        let names: Vec<&str> = page.hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["report-2024.pdf"]);
        assert!(!page.has_more);

        let deep = run(
            &root,
            &SearchOptions::new("report").recursive(true),
            None,
            None,
        )
        .await
        .unwrap();
        let mut rels: Vec<&str> = deep.hits.iter().map(|hit| hit.rel.as_str()).collect();
        rels.sort();
        assert_eq!(
            rels,
            vec![
                "other/report-hidden.txt",
                "report-2024.pdf",
                "sub/deep/report-final.md",
                "sub/report-draft.md",
            ]
        );
    }

    #[tokio::test]
    async fn recursive_search_skips_junk_but_keeps_hidden() {
        let root = tree("junk");
        let page = run(
            &root,
            &SearchOptions::new(".ds_store").recursive(true),
            None,
            None,
        )
        .await
        .unwrap();
        // 系统垃圾永远搜不出来
        assert!(page.hits.is_empty());
        // 普通隐藏文件不在 show_hidden 的遮挡范围内（用户是明确搜的）
        let hidden = run(
            &root,
            &SearchOptions::new("report-hidden").recursive(true),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(hidden.hits.len(), 1);
        assert_eq!(hidden.hits[0].rel, "other/report-hidden.txt");
    }

    #[tokio::test]
    async fn cursor_pages_cover_the_whole_tree_without_duplicates() {
        let root = scratch("paging");
        // 120 个命中散在 4 层目录里，每页 7 条
        for index in 0..30 {
            touch(&root.join(format!("top-{index:02}-hit.txt")));
            touch(&root.join(format!("a/mid-{index:02}-hit.txt")));
            touch(&root.join(format!("a/b/deep-{index:02}-hit.txt")));
            touch(&root.join(format!("a/b/c/deeper-{index:02}-hit.txt")));
        }

        let opts = SearchOptions::new("hit").recursive(true).limit(7);
        let mut cursor: Option<String> = None;
        let mut collected: Vec<String> = Vec::new();
        let mut pages = 0;
        loop {
            let page = run(&root, &opts, cursor.as_deref(), None).await.unwrap();
            pages += 1;
            assert!(page.hits.len() <= 7);
            for hit in &page.hits {
                collected.push(hit.rel.clone());
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(pages < 100, "游标没有收敛");
        }

        assert!(pages > 10, "应当分成多页，实际 {pages} 页");
        assert_eq!(collected.len(), 120);
        let mut unique = collected.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 120, "翻页出现了重复条目");
    }

    #[tokio::test]
    async fn scan_budget_splits_a_page_instead_of_truncating() {
        let root = scratch("budget");
        // 前 50 个条目全是噪声，命中藏在最后
        for index in 0..50 {
            touch(&root.join(format!("noise-{index:02}.bin")));
        }
        touch(&root.join("zzz-needle.txt"));

        let mut opts = SearchOptions::new("needle");
        opts.budget = 10;

        let first = run(&root, &opts, None, None).await.unwrap();
        assert!(first.hits.is_empty());
        assert!(first.has_more, "预算用完必须给游标，而不是当作搜完");
        assert_eq!(first.scanned, 10);

        let mut cursor = first.next_cursor;
        let mut found = false;
        let mut hops = 0;
        while let Some(current) = cursor {
            hops += 1;
            let page = run(&root, &opts, Some(&current), None).await.unwrap();
            if page.hits.iter().any(|hit| hit.name == "zzz-needle.txt") {
                found = true;
                break;
            }
            cursor = page.next_cursor;
            assert!(hops < 20, "预算续页没有收敛");
        }
        assert!(found, "跨预算续页应当能搜到最后的文件");
    }

    #[tokio::test]
    async fn broken_cursor_restarts_from_the_top() {
        let root = tree("badcursor");
        let page = run(
            &root,
            &SearchOptions::new("report"),
            Some("!!!not-a-cursor"),
            None,
        )
        .await
        .unwrap();
        assert!(!page.hits.is_empty());
    }

    /// 目录快照缓存只对「已经落定」的目录生效；一旦目录真的变了，必须重扫。
    ///
    /// 这里等 2.1 秒让 mtime 越过 `SETTLE`（缓存只收 mtime 至少 2 秒前的目录），
    /// 剩下的全是真实路径：首次不缓存 → 落定后写入 → 命中结果一致 → 新增文件立刻可见。
    #[tokio::test]
    async fn cache_is_used_only_for_settled_directories() {
        let root = tree("cacheuse");
        let db_dir =
            std::env::temp_dir().join(format!("asmoday-search-cache-{}", std::process::id()));
        let cache = dir_cache::test_cache(&db_dir);
        let opts = SearchOptions::new("report").recursive(true).limit(100);

        let fresh = run(&root, &opts, None, Some(&cache)).await.unwrap();
        assert_eq!(cache.count(), 0, "刚写过的目录不该进缓存");

        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

        let cached = run(&root, &opts, None, Some(&cache)).await.unwrap();
        assert!(
            cache.count() >= 3,
            "落定后应当写入目录快照，实际只有 {}",
            cache.count()
        );
        let mut fresh_rels: Vec<String> = fresh.hits.iter().map(|hit| hit.rel.clone()).collect();
        fresh_rels.sort();
        let mut cached_rels: Vec<String> = cached.hits.iter().map(|hit| hit.rel.clone()).collect();
        cached_rels.sort();
        assert_eq!(cached_rels, fresh_rels, "缓存命中的结果必须与现扫一致");

        // 再走一次：读的是缓存，结果仍然一样
        let again = run(&root, &opts, None, Some(&cache)).await.unwrap();
        let mut again_rels: Vec<String> = again.hits.iter().map(|hit| hit.rel.clone()).collect();
        again_rels.sort();
        assert_eq!(again_rels, fresh_rels);

        // 新增文件：`sub` 的 mtime 变了 → 只有这一层重扫，新条目必须能搜到
        touch(&root.join("sub/report-new.md"));
        let after = run(&root, &opts, None, Some(&cache)).await.unwrap();
        assert!(
            after.hits.iter().any(|hit| hit.rel == "sub/report-new.md"),
            "目录变了却只吃了缓存：{:?}",
            after.hits.iter().map(|hit| &hit.rel).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn cursor_roundtrip_keeps_unicode_paths() {
        let frames = vec![
            CursorFrame {
                dir: "图片/2024 年".to_string(),
                next: "截图 01.png".to_string(),
            },
            CursorFrame {
                dir: "图片/2024 年/旅行".to_string(),
                next: "a&b=c.png".to_string(),
            },
        ];
        let json = serde_json::to_vec(&frames).unwrap();
        let encoded = data_encoding::BASE64URL_NOPAD.encode(&json);
        // URL 安全字符集：不该出现需要再次转义的字符
        assert!(!encoded.contains(['+', '/', '=']));
        let decoded = decode_cursor(&encoded);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1].next, "a&b=c.png");
    }
}
