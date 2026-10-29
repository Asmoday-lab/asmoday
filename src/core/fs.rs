//! 文件系统核心逻辑：根目录解析、路径穿越防护、目录列表、Range 解析。
//!
//! 安全模型：
//! 1. 所有访问都必须落在「允许的根」之内（配置里的挂载点；未配置时退化为进程工作目录）；
//! 2. 先显式拒绝任何 `..` 组件（第一道闸），再 `canonicalize` 解析软链接后做包含性校验（第二道闸）；
//! 3. 对外一律输出 POSIX 风格路径字符串。

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::core::config::Config;

/// 单页最大条目数：目录条目超过该值时强制分页（设计文档 3.2）。
pub const PAGE_SIZE: usize = 1000;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum FsError {
    /// 路径越界（不在挂载点内 / 无权限）
    Forbidden(String),
    /// 显式检测到 `../` 或软链接逃逸（设计文档 6.6 的 E_PATH_TRAVERSAL / E_SYMLINK_ESCAPE）
    PathTraversal(String),
    /// 软链接目标跑出了挂载点
    SymlinkEscape(String),
    /// 不存在
    NotFound(String),
    /// 请求参数不合法
    BadRequest(String),
    /// 只读挂载点
    Readonly(String),
    /// 目标已存在（原子 no-replace 失败）
    Conflict(String),
    /// 目录 overwrite 被拒（设计文档 3.2.6.2）
    DirConflict(String),
    /// 自动命名重试次数用尽
    RetryExhausted(String),
    /// 数量 / 体积超限（设计文档 1.3 第 7 条）
    TooMany(String),
    TooLarge(String),
    /// 目标磁盘空间不足
    NoSpace(String),
    /// 路径被并发操作占用（per-path 锁）
    Busy(String),
    /// 删除确认名不匹配
    NameMismatch(String),
    /// 删除令牌过期 / 与 paths 不匹配
    TokenExpired(String),
    TokenMismatch(String),
    /// 服务端 IO 失败
    Io(String),
    /// 跨挂载点移动：复制已成功写入目标，但删除源文件失败（设计文档 3.2.6）。
    ///
    /// 必须与普通失败区分开：数据**已经在目标位置**，用户看到的不能是
    /// 「移动失败」（否则会去目标目录再确认一遍、甚至重复复制）。
    SourceDeleteFailed(String),
}

impl FsError {
    /// 细节文本（错误码与状态码的映射在 `api::error`，避免两处各写一套）
    #[allow(dead_code)]
    pub fn message(&self) -> String {
        match self {
            FsError::Forbidden(m)
            | FsError::PathTraversal(m)
            | FsError::SymlinkEscape(m)
            | FsError::NotFound(m)
            | FsError::BadRequest(m)
            | FsError::Readonly(m)
            | FsError::Conflict(m)
            | FsError::DirConflict(m)
            | FsError::RetryExhausted(m)
            | FsError::TooMany(m)
            | FsError::TooLarge(m)
            | FsError::NoSpace(m)
            | FsError::Busy(m)
            | FsError::NameMismatch(m)
            | FsError::TokenExpired(m)
            | FsError::TokenMismatch(m)
            | FsError::SourceDeleteFailed(m)
            | FsError::Io(m) => m.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 根目录
// ---------------------------------------------------------------------------

/// 对外暴露的根（挂载点）。
#[derive(Debug, Clone, Serialize)]
pub struct RootEntry {
    pub name: String,
    /// 展示用路径（未 canonicalize，便于用户识别）
    pub path: String,
    /// 是否真实存在
    pub exists: bool,
    /// 是否为「未配置挂载点时」的兜底根
    pub implicit: bool,
}

/// 一次请求可用的根集合。
#[derive(Debug, Clone)]
pub struct RootSet {
    pub roots: Vec<RootEntry>,
    /// 已 canonicalize 的允许根，用于包含性校验
    pub allowed: Vec<PathBuf>,
    pub implicit: bool,
    /// `[system].follow_symlinks`：是否允许经由挂载点内软链接访问挂载点外的目标
    pub follow_symlinks: bool,
}

impl RootSet {
    /// 默认入口：第一个存在的根。
    pub fn default_root(&self) -> Option<&RootEntry> {
        self.roots.iter().find(|r| r.exists)
    }
}

/// 依据配置构建根集合。
///
/// 真正的挂载点扫描在 `core::path_guard::load`（唯一真源，带 `readonly` 与 `online`），
/// 这里只是把它投影成旧接口用的形状，避免两处各写一套扫描逻辑。
pub fn load_roots(config: &Config, cwd: &Path) -> RootSet {
    let set = crate::core::path_guard::load(config, cwd);

    let roots: Vec<RootEntry> = set
        .mounts
        .iter()
        .map(|mount| RootEntry {
            name: mount.info.name.clone(),
            path: mount.info.path.clone(),
            exists: mount.info.online,
            implicit: mount.info.implicit,
        })
        .collect();
    let allowed: Vec<PathBuf> = set.mounts.iter().filter_map(|m| m.root.clone()).collect();

    RootSet {
        roots,
        allowed,
        implicit: set.implicit,
        follow_symlinks: config.system.follow_symlinks,
    }
}

/// 把原生路径转成展示用字符串（Windows 反斜杠统一成 `/`，去掉 `\\?\` 前缀）。
pub fn normalize_display(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    text.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// 路径解析与安全校验
// ---------------------------------------------------------------------------

/// 校验并解析用户传入的路径，返回可用的真实路径。
///
/// `follow_symlinks = true` 时，经由**挂载点内软链接**逃出允许根的目标不再拒绝，
/// 但保留链接前缀返回（理由见 `path_guard::resolve_existing` 的注释）；
/// 直接传挂载点外的绝对路径依旧 403。
pub fn resolve_existing(
    raw: &str,
    allowed: &[PathBuf],
    follow_symlinks: bool,
) -> Result<PathBuf, FsError> {
    let candidate = parse_candidate(raw)?;
    let canon = std::fs::canonicalize(&candidate)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{raw}")))?;
    if ensure_within(&canon, allowed).is_ok() {
        return Ok(canon);
    }
    if follow_symlinks && crate::core::path_guard::escaped_via_symlink(&candidate, allowed) {
        return Ok(candidate);
    }
    Err(FsError::Forbidden("该路径不在已授权的挂载点内".to_string()))
}

/// 只做字符串层面的合法性检查 + 拼接，不要求存在（写入前再校验父目录）。
fn parse_candidate(raw: &str) -> Result<PathBuf, FsError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(FsError::BadRequest("path 不能为空".to_string()));
    }
    if trimmed.contains('\0') || trimmed.chars().any(|c| c.is_control()) {
        return Err(FsError::BadRequest("path 含非法字符".to_string()));
    }

    let path = Path::new(trimmed);

    // 第一道闸：显式拒绝任何 ../ 上跳
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(FsError::Forbidden("禁止使用 ../ 进行路径穿越".to_string()));
        }
    }

    Ok(path.to_path_buf())
}

/// 第二道闸：真实路径必须落在允许根之内。
fn ensure_within(canon: &Path, allowed: &[PathBuf]) -> Result<(), FsError> {
    if allowed.iter().any(|root| canon.starts_with(root)) {
        Ok(())
    } else {
        Err(FsError::Forbidden("该路径不在已授权的挂载点内".to_string()))
    }
}

// ---------------------------------------------------------------------------
// 目录列表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EntryInfo {
    pub name: String,
    /// POSIX 风格绝对路径
    pub path: String,
    pub is_dir: bool,
    /// 软链接按 lstat 处理，单独标记（设计文档 3.2.7）
    pub is_symlink: bool,
    pub size: u64,
    /// Unix 时间戳（秒）
    pub modified: Option<u64>,
    /// dir / image / video / audio / text / archive / other
    pub kind: &'static str,
    pub ext: String,
    /// 是否可用内置编辑器打开
    pub editable: bool,
    /// 单项 stat 失败：不让整页 500，标记后由前端显示占位（设计文档 3.2.4）
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stat_error: bool,
    /// 软链接目标的原始写法（`read_link` 原样返回，相对 / 绝对都可能是）。
    /// 仅软链接条目携带；前端悬停展示。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
    /// 跟随链接后目标是否为目录。
    /// `None` = 断链、无法解析或目标逃出挂载点（前端标记为断链并置灰）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_is_dir: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub has_more: bool,
    /// 下一页游标（设计文档 3.2.4：固定排序 + 游标分页，不用 offset）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// 扫描超过 `SCAN_LIMIT` 被强制截断（设计文档 3.2.4）
    pub truncated: bool,
    /// 所在挂载点是否只读：前端据此置灰全部变更入口
    pub readonly: bool,
    pub entries: Vec<EntryInfo>,
}

/// 单目录扫描上限：超出截断并标记 `truncated`（设计文档 3.2.4）
pub const SCAN_LIMIT: usize = 10_000;

/// 列表排序键（设计文档 v2.7.0 3.2.1：名称 / 修改时间 / 大小）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListSort {
    #[default]
    Name,
    Mtime,
    Size,
}

impl ListSort {
    /// `?sort=` 参数解析；非法值回落默认（不报错，前端永远拿得到列表）
    pub fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or("").to_ascii_lowercase().as_str() {
            "mtime" => Self::Mtime,
            "size" => Self::Size,
            _ => Self::Name,
        }
    }
}

/// 升 / 降序
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListOrder {
    #[default]
    Asc,
    Desc,
}

impl ListOrder {
    /// `?order=` 参数解析
    pub fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or("").to_ascii_lowercase().as_str() {
            "desc" => Self::Desc,
            _ => Self::Asc,
        }
    }
}

// ---------------------------------------------------------------------------
// 游标分页（设计文档 3.2.4：强制固定排序 + 游标分页，禁止 offset 分页）
// ---------------------------------------------------------------------------

/// 游标内容：上一页最后一条的 `(是否目录, 排序键, 小写名)`，外加排序口径。
///
/// 带上 `sort/order` 是为了「换了排序却拿旧游标」时直接退回第一页，
/// 而不是按旧键在新排序里乱切片。游标不透明（base64url），前端只负责原样带回。
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    sort: String,
    order: String,
    is_dir: bool,
    /// name 排序固定 0；size 用字节数；mtime 用 Unix 秒；拿不到元数据用 -1
    key: i64,
    name: String,
}

fn sort_name(sort: ListSort) -> &'static str {
    match sort {
        ListSort::Name => "name",
        ListSort::Mtime => "mtime",
        ListSort::Size => "size",
    }
}

fn order_name(order: ListOrder) -> &'static str {
    match order {
        ListOrder::Asc => "asc",
        ListOrder::Desc => "desc",
    }
}

/// 取排序键的数值形式（与 `list_dir` 里的比较逻辑一一对应）
fn sort_key(item: &(String, bool, Option<std::fs::Metadata>), sort: ListSort) -> i64 {
    match sort {
        ListSort::Name => 0,
        ListSort::Size => item.2.as_ref().map(|meta| meta.len() as i64).unwrap_or(-1),
        ListSort::Mtime => item
            .2
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(-1),
    }
}

fn encode_cursor(
    sort: ListSort,
    order: ListOrder,
    item: &(String, bool, Option<std::fs::Metadata>),
) -> String {
    let cursor = Cursor {
        sort: sort_name(sort).to_string(),
        order: order_name(order).to_string(),
        is_dir: item.1,
        key: sort_key(item, sort),
        name: item.0.to_lowercase(),
    };
    let json = serde_json::to_vec(&cursor).unwrap_or_default();
    data_encoding::BASE64URL_NOPAD.encode(&json)
}

fn decode_cursor(raw: &str, sort: ListSort, order: ListOrder) -> Option<Cursor> {
    let bytes = data_encoding::BASE64URL_NOPAD.decode(raw.as_bytes()).ok()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    // 排序口径变了就当游标作废（退回第一页），绝不拿旧键去比新序
    if cursor.sort != sort_name(sort) || cursor.order != order_name(order) {
        return None;
    }
    Some(cursor)
}

/// 条目相对游标的位置：`Greater` 表示它在游标之后（下一页从这里开始）
fn cursor_cmp(
    item: &(String, bool, Option<std::fs::Metadata>),
    sort: ListSort,
    order: ListOrder,
    cursor: &Cursor,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (item.1, cursor.is_dir) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let key_cmp = sort_key(item, sort).cmp(&cursor.key);
    let key_cmp = if order == ListOrder::Desc {
        key_cmp.reverse()
    } else {
        key_cmp
    };
    if key_cmp != Ordering::Equal {
        return key_cmp;
    }
    item.0.to_lowercase().cmp(&cursor.name)
}

/// 读取目录第一层并分页。
///
/// 排序固定「目录优先」，其余按指定键升降序；分页结果必须可重现，
/// 所以不能依赖 `readdir` 的返回顺序，同键并列时以名称（大小写不敏感）兜底。
///
/// 分页方式（设计文档 3.2.4）：**优先用游标**，`cursor` 为空时才回落到 `page`
/// （仅为老客户端保留；新前端一律传游标，目录内容变化也不会重复/漏项）。
#[allow(clippy::too_many_arguments)]
pub async fn list_dir(
    dir: &Path,
    page: usize,
    show_hidden: bool,
    readonly: bool,
    sort: ListSort,
    order: ListOrder,
    cursor: Option<&str>,
    // 当前挂载点的 canonical_root：软链接目标越出它即标记为断链（E_SYMLINK_ESCAPE 语义）。
    // 分享页等无挂载点上下文的调用传 `None`（只解析目标，不做越界判定）。
    mount_root: Option<&Path>,
    // `[system].follow_symlinks`：为 true 时越界链接不再标断链，照常给出目标类型
    follow_symlinks: bool,
) -> Result<Listing, FsError> {
    let page = page.max(1);

    let mut reader = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| FsError::Io(format!("读取目录失败：{e}")))?;

    let mut items: Vec<(String, bool, Option<std::fs::Metadata>)> = Vec::new();
    let mut truncated = false;
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| FsError::Io(format!("遍历目录失败：{e}")))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        // `.asmoday-part` 临时文件任何情况下都不显示（设计文档 3.2.6.4）
        if crate::core::path_guard::is_part_file(&name) {
            continue;
        }
        // 跨平台系统垃圾（desktop.ini / Thumbs.db / .DS_Store / ._* / ~$*）
        // 优先级高于 show_hidden：开关打开也不返回（设计文档 3.3.7）
        if crate::core::junk::is_system_junk(&name) {
            continue;
        }
        // 设计文档 3.2：默认隐藏以 `.` 开头的系统项（.git / .env …），
        // 开关打开时才一并返回；点数仍在分页之前过滤，total/has_more 才是准的
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if items.len() >= SCAN_LIMIT {
            truncated = true;
            break;
        }
        // file_type() 不跟随软链接，避免指向目录的链接被当作目录递归进去
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        items.push((name, is_dir, None));
    }

    // 按 mtime / size 排序必须先拿到全部元数据（Name 排序免 stat）
    if sort != ListSort::Name {
        for (name, _, meta) in items.iter_mut() {
            *meta = tokio::fs::symlink_metadata(dir.join(&name)).await.ok();
        }
    }

    // 目录在前；同组内按指定键排序，并列以名称（大小写不敏感）兜底
    items.sort_by(|a, b| {
        let dir_cmp = b.1.cmp(&a.1);
        if dir_cmp != std::cmp::Ordering::Equal {
            return dir_cmp;
        }
        let key_cmp = match sort {
            ListSort::Name => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
            ListSort::Size => {
                a.2.as_ref()
                    .map(|m| m.len())
                    .cmp(&b.2.as_ref().map(|m| m.len()))
            }
            ListSort::Mtime => {
                a.2.as_ref()
                    .and_then(|m| m.modified().ok())
                    .cmp(&b.2.as_ref().and_then(|m| m.modified().ok()))
            }
        };
        if key_cmp != std::cmp::Ordering::Equal {
            return if order == ListOrder::Desc {
                key_cmp.reverse()
            } else {
                key_cmp
            };
        }
        a.0.to_lowercase().cmp(&b.0.to_lowercase())
    });

    let total = items.len();
    // 游标优先；游标无效（排序变了 / 手工改坏）就退回第一页，不报错
    let start = match cursor.and_then(|raw| decode_cursor(raw, sort, order)) {
        Some(cur) => items
            .iter()
            .position(|item| cursor_cmp(item, sort, order, &cur) == std::cmp::Ordering::Greater)
            .unwrap_or(total),
        None => (page - 1).saturating_mul(PAGE_SIZE),
    };
    let end = (start + PAGE_SIZE).min(total);
    let has_more = end < total;
    let next_cursor = if has_more {
        end.checked_sub(1)
            .and_then(|last| items.get(last))
            .map(|item| encode_cursor(sort, order, item))
    } else {
        None
    };

    let mut entries = Vec::new();
    if start < total {
        for (name, is_dir, cached) in &items[start..end] {
            let full = dir.join(name);
            // lstat：软链接只报告链接本身，不 stat 目标（排序阶段已 stat 的直接复用）
            let link_meta = match cached {
                Some(meta) => Ok(meta.clone()),
                None => tokio::fs::symlink_metadata(&full).await,
            };
            let stat_error = link_meta.is_err();
            let is_symlink = link_meta
                .as_ref()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            let size = link_meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = link_meta
                .as_ref()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            let ext = extension_of_name(name);
            let (kind, editable) = classify_name(name, ext.as_str(), *is_dir);

            // 软链接：跟随解析目标（设计文档 3.2.7）。
            // - 目标信息只对当前页的软链接条目 stat，数量少，不拖累整页性能；
            // - canonicalize 成功且未越出挂载点 → target_is_dir 给真实判定；
            // - 越出挂载点时：`follow_symlinks = false` 记 None（前端标断链置灰），
            //   为 true 则照常给目标类型，与后端放行口径保持一致；
            // - 断链（ENOENT / 循环链接）恒为 None。
            let (symlink_target, target_is_dir) = if is_symlink {
                let raw = tokio::fs::read_link(&full)
                    .await
                    .ok()
                    .map(|target| target.to_string_lossy().into_owned());
                let resolved = std::fs::canonicalize(&full)
                    .map(|canon| {
                        let inside = mount_root.is_none_or(|root| {
                            canon.starts_with(root)
                                || (follow_symlinks
                                    && crate::core::path_guard::escaped_via_symlink_in(&full, root))
                        });
                        inside.then(|| canon.is_dir())
                    })
                    .unwrap_or(None);
                (raw, resolved)
            } else {
                (None, None)
            };

            entries.push(EntryInfo {
                name: name.clone(),
                path: normalize_display(&full),
                is_dir: *is_dir,
                is_symlink,
                size,
                modified,
                kind,
                ext,
                editable,
                stat_error,
                symlink_target,
                target_is_dir,
            });
        }
    }

    let parent = dir
        .parent()
        .map(normalize_display)
        .filter(|p| !p.is_empty() && p != &normalize_display(dir));

    Ok(Listing {
        path: normalize_display(dir),
        parent,
        page: start / PAGE_SIZE + 1,
        page_size: PAGE_SIZE,
        total,
        has_more,
        next_cursor,
        truncated,
        readonly,
        entries,
    })
}

/// 取小写扩展名（无扩展名返回空串）。
pub fn extension_of_name(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

const TEXT_EXTS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "json",
    "json5",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "vue",
    "svelte",
    "rs",
    "py",
    "rb",
    "go",
    "java",
    "kt",
    "c",
    "h",
    "cpp",
    "hpp",
    "cs",
    "php",
    "swift",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "html",
    "htm",
    "css",
    "scss",
    "less",
    "xml",
    "svg",
    "yaml",
    "yml",
    "sql",
    "lua",
    "pl",
    "log",
    "lock",
    "gitignore",
    "dockerfile",
    "makefile",
    "editorconfig",
];

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "avif", "heic", "heif", "ico", "tif", "tiff",
];

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "wmv", "mpg", "mpeg", "m2ts", "m3u8",
];

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "wav", "aac", "ogg", "m4a", "opus", "wma", "ape",
];

const ARCHIVE_EXTS: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "7z", "rar", "tar", "zst", "iso",
];

/// 判定文件类别与是否可编辑。
pub fn classify_name(name: &str, ext: &str, is_dir: bool) -> (&'static str, bool) {
    if is_dir {
        return ("dir", false);
    }
    // 无扩展名的常见文本文件
    let lower = name.to_lowercase();
    if TEXT_EXTS.contains(&lower.as_str()) {
        return ("text", true);
    }
    if IMAGE_EXTS.contains(&ext) {
        return ("image", false);
    }
    if VIDEO_EXTS.contains(&ext) {
        return ("video", false);
    }
    if AUDIO_EXTS.contains(&ext) {
        return ("audio", false);
    }
    if ARCHIVE_EXTS.contains(&ext) {
        return ("archive", false);
    }
    if TEXT_EXTS.contains(&ext) {
        return ("text", true);
    }
    ("other", false)
}

// ---------------------------------------------------------------------------
// HTTP Range
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// 起始偏移（含）
    pub start: u64,
    /// 结束偏移（含）
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// 解析单区间 `Range: bytes=...`。
///
/// 返回：
/// - `Ok(Some(range))` 正常区间
/// - `Ok(None)` 头不可解析 / 多区间 / 单位不是 bytes → 按整体返回
/// - `Err(())` 区间越界 → 应回 416
pub fn parse_range(header: &str, file_len: u64) -> Result<Option<ByteRange>, ()> {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        // 多区间不支持，退回整体响应
        return Ok(None);
    }
    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return Ok(None);
    };
    let start_raw = start_raw.trim();
    let end_raw = end_raw.trim();

    if file_len == 0 {
        return Err(());
    }

    if start_raw.is_empty() {
        // 后缀区间：bytes=-500
        let Ok(suffix) = end_raw.parse::<u64>() else {
            return Ok(None);
        };
        if suffix == 0 {
            return Err(());
        }
        let start = file_len.saturating_sub(suffix);
        return Ok(Some(ByteRange {
            start,
            end: file_len - 1,
        }));
    }

    let Ok(start) = start_raw.parse::<u64>() else {
        return Ok(None);
    };
    let end = if end_raw.is_empty() {
        file_len - 1
    } else {
        match end_raw.parse::<u64>() {
            Ok(v) => v.min(file_len - 1),
            Err(_) => return Ok(None),
        }
    };

    if start > end || start >= file_len {
        return Err(());
    }

    Ok(Some(ByteRange { start, end }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sort_and_order_parse() {
        assert_eq!(ListSort::parse(None), ListSort::Name);
        assert_eq!(ListSort::parse(Some("mtime")), ListSort::Mtime);
        assert_eq!(ListSort::parse(Some("SIZE")), ListSort::Size);
        assert_eq!(ListSort::parse(Some("bogus")), ListSort::Name);
        assert_eq!(ListOrder::parse(Some("desc")), ListOrder::Desc);
        assert_eq!(ListOrder::parse(None), ListOrder::Asc);
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        for raw in ["/tmp/../etc/passwd", "..\\windows\\system32", "../secret"] {
            let err = parse_candidate(raw).unwrap_err();
            assert!(matches!(err, FsError::Forbidden(_)), "应拒绝 {raw}");
        }
    }

    #[test]
    fn rejects_empty_and_control_chars() {
        assert!(parse_candidate("   ").is_err());
        assert!(parse_candidate("a\0b").is_err());
        // 存在性交给 canonicalize，这里只要能构造出候选路径即可
        let _ = parse_candidate("C:/Windows/notepad.exe").unwrap();
        let _ = allowed_unused();
    }

    fn allowed_unused() -> Result<(), FsError> {
        Ok(())
    }

    #[test]
    fn containment_check_blocks_outside_paths() {
        let root = std::env::temp_dir();
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let inside = root.join("asmoday-inside-check.txt");
        assert!(ensure_within(&inside, std::slice::from_ref(&root)).is_ok());
        let outside = if cfg!(windows) {
            PathBuf::from(r"C:\Windows\System32")
        } else {
            PathBuf::from("/etc")
        };
        assert!(ensure_within(&outside, &[root]).is_err());
    }

    #[test]
    fn parses_byte_ranges() {
        assert_eq!(
            parse_range("bytes=0-99", 1000),
            Ok(Some(ByteRange { start: 0, end: 99 }))
        );
        assert_eq!(
            parse_range("bytes=500-", 1000),
            Ok(Some(ByteRange {
                start: 500,
                end: 999
            }))
        );
        assert_eq!(
            parse_range("bytes=-100", 1000),
            Ok(Some(ByteRange {
                start: 900,
                end: 999
            }))
        );
        // 越界
        assert_eq!(parse_range("bytes=1000-", 1000), Err(()));
        // 多区间 / 非 bytes → 整体返回
        assert_eq!(parse_range("bytes=0-10,20-30", 1000), Ok(None));
        assert_eq!(parse_range("items=0-10", 1000), Ok(None));
        // 结束位置越界被裁剪
        assert_eq!(
            parse_range("bytes=900-5000", 1000),
            Ok(Some(ByteRange {
                start: 900,
                end: 999
            }))
        );
    }

    #[test]
    fn classifies_files() {
        assert_eq!(classify_name("a.rs", "rs", false), ("text", true));
        assert_eq!(classify_name("a.toml", "toml", false), ("text", true));
        assert_eq!(classify_name("a.png", "png", false), ("image", false));
        assert_eq!(classify_name("a.mp4", "mp4", false), ("video", false));
        assert_eq!(classify_name("a.bin", "bin", false), ("other", false));
        assert_eq!(classify_name("dir", "", true), ("dir", false));
        // .ts 是 TypeScript，不是 MPEG-TS 视频
        assert_eq!(classify_name("router.ts", "ts", false), ("text", true));
    }

    fn plain(name: &str, is_dir: bool) -> (String, bool, Option<std::fs::Metadata>) {
        (name.to_string(), is_dir, None)
    }

    #[test]
    fn cursor_roundtrip_keeps_sort_scope() {
        let item = plain("Photo.PNG", false);
        let raw = encode_cursor(ListSort::Name, ListOrder::Asc, &item);
        let decoded = decode_cursor(&raw, ListSort::Name, ListOrder::Asc).unwrap();
        assert_eq!(decoded.name, "photo.png");
        assert!(!decoded.is_dir);
        // 换了排序口径 → 游标作废，退回第一页而不是乱切
        assert!(decode_cursor(&raw, ListSort::Name, ListOrder::Desc).is_none());
        assert!(decode_cursor(&raw, ListSort::Size, ListOrder::Asc).is_none());
        // 乱码输入也不 panic
        assert!(decode_cursor("not-a-cursor", ListSort::Name, ListOrder::Asc).is_none());
    }

    #[test]
    fn cursor_orders_after_last_item() {
        use std::cmp::Ordering;
        let cur = Cursor {
            sort: "name".into(),
            order: "asc".into(),
            is_dir: true,
            key: 0,
            name: "b".into(),
        };
        // 同为目录：按名称排在 `b` 之后的才是下一页
        assert_eq!(
            cursor_cmp(&plain("c", true), ListSort::Name, ListOrder::Asc, &cur),
            Ordering::Greater
        );
        assert_eq!(
            cursor_cmp(&plain("a", true), ListSort::Name, ListOrder::Asc, &cur),
            Ordering::Less
        );
        // 文件全部排在目录之后
        assert_eq!(
            cursor_cmp(&plain("a", false), ListSort::Name, ListOrder::Asc, &cur),
            Ordering::Greater
        );
        // 反向：游标是文件时，目录都在它前面
        let file_cur = Cursor {
            is_dir: false,
            name: "b".into(),
            ..cur
        };
        assert_eq!(
            cursor_cmp(&plain("a", true), ListSort::Name, ListOrder::Asc, &file_cur),
            Ordering::Less
        );
    }

    #[test]
    fn cursor_respects_desc_order() {
        use std::cmp::Ordering;
        let cur = Cursor {
            sort: "size".into(),
            order: "desc".into(),
            is_dir: false,
            key: 100,
            name: "a".into(),
        };
        // 降序时更小的文件在游标之后
        let item = ("b".to_string(), false, None);
        assert_eq!(
            cursor_cmp(&item, ListSort::Size, ListOrder::Desc, &cur),
            Ordering::Greater
        );
    }
}
