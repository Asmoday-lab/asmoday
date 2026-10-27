//! 文件系统核心逻辑：根目录解析、路径穿越防护、目录列表、Range 解析。
//!
//! 安全模型：
//! 1. 所有访问都必须落在「允许的根」之内（配置里的挂载点；未配置时退化为进程工作目录）；
//! 2. 先显式拒绝任何 `..` 组件（第一道闸），再 `canonicalize` 解析软链接后做包含性校验（第二道闸）；
//! 3. 对外一律输出 POSIX 风格路径字符串。

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

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

/// 校验并解析用户传入的路径，返回 canonicalize 后的真实路径。
pub fn resolve_existing(raw: &str, allowed: &[PathBuf]) -> Result<PathBuf, FsError> {
    let candidate = parse_candidate(raw)?;
    let canon = std::fs::canonicalize(&candidate)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{raw}")))?;
    ensure_within(&canon, allowed)?;
    Ok(canon)
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
            return Err(FsError::Forbidden(
                "禁止使用 ../ 进行路径穿越".to_string(),
            ));
        }
    }

    Ok(path.to_path_buf())
}

/// 第二道闸：真实路径必须落在允许根之内。
fn ensure_within(canon: &Path, allowed: &[PathBuf]) -> Result<(), FsError> {
    if allowed.iter().any(|root| canon.starts_with(root)) {
        Ok(())
    } else {
        Err(FsError::Forbidden(
            "该路径不在已授权的挂载点内".to_string(),
        ))
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub has_more: bool,
    /// 扫描超过 `SCAN_LIMIT` 被强制截断（设计文档 3.2.4）
    pub truncated: bool,
    /// 所在挂载点是否只读：前端据此置灰全部变更入口
    pub readonly: bool,
    pub entries: Vec<EntryInfo>,
}

/// 单目录扫描上限：超出截断并标记 `truncated`（设计文档 3.2.4）
pub const SCAN_LIMIT: usize = 10_000;

/// 读取目录第一层并分页。
///
/// 排序固定为「目录优先 + 名称自然序（大小写不敏感）」——分页结果必须可重现，
/// 所以不能依赖 `readdir` 的返回顺序。
pub async fn list_dir(
    dir: &Path,
    page: usize,
    show_hidden: bool,
    readonly: bool,
) -> Result<Listing, FsError> {
    let page = page.max(1);

    let mut reader = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| FsError::Io(format!("读取目录失败：{e}")))?;

    let mut items: Vec<(String, bool)> = Vec::new();
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
        let is_dir = entry
            .file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false);
        items.push((name, is_dir));
    }

    // 目录在前，其余按名称不区分大小写排序
    items.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    let total = items.len();
    let start = (page - 1).saturating_mul(PAGE_SIZE);
    let end = (start + PAGE_SIZE).min(total);

    let mut entries = Vec::new();
    if start < total {
        for (name, is_dir) in &items[start..end] {
            let full = dir.join(name);
            // lstat：软链接只报告链接本身，不 stat 目标
            let link_meta = tokio::fs::symlink_metadata(&full).await;
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
            });
        }
    }

    let parent = dir
        .parent()
        .map(|p| normalize_display(p))
        .filter(|p| !p.is_empty() && p != &normalize_display(dir));

    Ok(Listing {
        path: normalize_display(dir),
        parent,
        page,
        page_size: PAGE_SIZE,
        total,
        has_more: end < total,
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
    "txt", "md", "markdown", "json", "json5", "js", "mjs", "cjs", "jsx", "ts", "tsx", "vue",
    "svelte", "rs", "py", "rb", "go", "java", "kt", "c", "h", "cpp", "hpp", "cs", "php", "swift",
    "toml", "ini", "cfg", "conf", "env", "properties", "sh", "bash", "zsh", "fish", "ps1", "bat",
    "cmd", "html", "htm", "css", "scss", "less", "xml", "svg", "yaml", "yml", "sql", "lua", "pl",
    "log", "lock", "gitignore", "dockerfile", "makefile", "editorconfig",
];

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "avif", "heic", "heif", "ico", "tif",
    "tiff",
];

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "wmv", "mpg", "mpeg", "m2ts", "m3u8",
];

const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "aac", "ogg", "m4a", "opus", "wma", "ape"];

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
        assert!(ensure_within(&inside, &[root.clone()]).is_ok());
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
}
