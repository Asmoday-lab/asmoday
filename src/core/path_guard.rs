//! 挂载点与路径守卫（设计文档 3.2.6 / 3.2.7）。
//!
//! 这里是**所有**文件变更接口的唯一入口校验点，规则集中在此处，避免各 handler
//! 各写一套导致某个接口漏校验：
//!
//! 1. 每个挂载点在启动时（以及配置热重载后）`canonicalize` 一次，存为 `root`；
//!    之后所有归属校验都以它为准。
//! 2. 已存在的路径：`canonicalize` 后再判断是否落在 `root` 之下（跟软链接）。
//! 3. 不存在的目标（新建、复制目标）：**只 `canonicalize` 父目录**，名字单独校验。
//!    严禁对不存在的完整路径直接 `canonicalize`（会 ENOENT）。
//! 4. 防覆盖一律靠平台原子 no-replace 原语，不用 `exists()` 再 `rename()`。

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::core::config::Config;
use crate::core::fs::FsError;

/// `.asmoday-part` 临时文件保留标记（设计文档 3.2.6.4）
pub const PART_MARK: &str = ".asmoday-part.";
/// 单段文件名字节上限（多数文件系统的硬上限）
pub const MAX_NAME_BYTES: usize = 255;

// ---------------------------------------------------------------------------
// 挂载点
// ---------------------------------------------------------------------------

/// 对外暴露的挂载点（`GET /api/files/mounts` 的形状，设计文档 6.1）。
#[derive(Debug, Clone, Serialize)]
pub struct MountPoint {
    /// 稳定 id：`m0`、`m1`…，按配置顺序；热重载后同一位置仍是同一 id
    pub id: String,
    pub name: String,
    /// 展示路径（未 canonicalize，便于用户识别原配置）
    pub path: String,
    pub readonly: bool,
    /// 未配置挂载点时退化出来的兜底根（进程工作目录）
    pub implicit: bool,
    /// 目录当前是否存在（关机拔盘 / 路径写错时为 false，前端置灰）
    pub online: bool,
}

/// 挂载点 + 已 canonicalize 的根
#[derive(Debug, Clone)]
pub struct MountEntry {
    pub info: MountPoint,
    pub root: Option<PathBuf>,
}

impl MountEntry {
    pub fn canon_root(&self) -> Result<&PathBuf, FsError> {
        self.root.as_ref().ok_or_else(|| {
            FsError::NotFound(format!("挂载点 {} 当前不可用", self.info.name))
        })
    }

    /// 只读挂载点直接拒绝一切写操作（设计文档 3.2.6 通用前提）
    pub fn ensure_writable(&self) -> Result<(), FsError> {
        if self.info.readonly {
            return Err(FsError::Readonly(format!(
                "挂载点 {} 为只读",
                self.info.name
            )));
        }
        Ok(())
    }
}

/// 一次请求可用的挂载点集合。
#[derive(Debug, Clone)]
pub struct MountSet {
    pub mounts: Vec<MountEntry>,
    pub implicit: bool,
}

impl MountSet {
    /// 默认入口：第一个在线的挂载点
    pub fn default_mount(&self) -> Option<&MountEntry> {
        self.mounts.iter().find(|m| m.root.is_some())
    }

    /// 按 `mount_id` 或展示路径查找
    pub fn find(&self, id_or_path: &str) -> Option<&MountEntry> {
        let needle = id_or_path.trim();
        self.mounts
            .iter()
            .find(|m| m.info.id == needle)
            .or_else(|| self.mounts.iter().find(|m| paths_equal(&m.info.path, needle)))
            .or_else(|| self.find_by_path_text(needle))
    }

    /// 用「绝对路径前缀」反查挂载点（前端只传路径、不传 mount_id 时使用）。
    ///
    /// 注意：这是**纯字符串**匹配，只用于定位挂载点；真正的归属校验仍必须走
    /// `resolve_existing` 的 canonicalize 结果。
    pub fn find_by_path_text(&self, raw: &str) -> Option<&MountEntry> {
        let needle = raw.replace('\\', "/").to_lowercase();
        self.mounts
            .iter()
            .filter(|m| {
                let prefix = m.info.path.trim_end_matches('/').to_lowercase();
                !prefix.is_empty()
                    && (needle == prefix
                        || needle.starts_with(&format!("{prefix}/"))
                        // 兜底：canonical 形式与展示形式不一致（如 Windows 盘符大小写）
                        || m.root.as_ref().is_some_and(|root| {
                            let canon = crate::core::fs::normalize_display(root).to_lowercase();
                            needle == canon || needle.starts_with(&format!("{canon}/"))
                        }))
            })
            .max_by_key(|m| m.info.path.len())
    }

    /// 路径归属哪个挂载点（用于把绝对路径反查成 mount_id）
    pub fn mount_of(&self, canon_path: &Path) -> Option<&MountEntry> {
        self.mounts
            .iter()
            .filter(|m| m.root.as_ref().is_some_and(|root| canon_path.starts_with(root)))
            .max_by_key(|m| m.root.as_ref().map(|r| r.as_os_str().len()).unwrap_or(0))
    }

    pub fn is_readonly(&self, canon_path: &Path) -> bool {
        self.mount_of(canon_path)
            .map(|m| m.info.readonly)
            .unwrap_or(false)
    }
}

/// 依据配置构建挂载点集合。
///
/// - 配置了 `[[system.mounts]]`：只用这些目录；
/// - 未配置（或全部不存在）：退化到进程工作目录，`implicit = true`。
pub fn load(config: &Config, cwd: &Path) -> MountSet {
    let mut mounts = Vec::new();
    let mut online_any = false;

    for (index, mount) in config.system.mounts.iter().enumerate() {
        let raw = PathBuf::from(&mount.path);
        // 启动时对每个挂载点根 canonicalize 一次，之后所有校验都以后者为准
        // （设计文档 3.2.6「挂载点根」）；对外也回绝对路径，前端才能直接拿来当 path 用。
        let canonical = std::fs::canonicalize(&raw).ok().filter(|p| p.is_dir());
        if canonical.is_some() {
            online_any = true;
        }
        let display = match &canonical {
            Some(path) => crate::core::fs::normalize_display(path),
            None => crate::core::fs::normalize_display(&raw),
        };
        mounts.push(MountEntry {
            info: MountPoint {
                id: format!("m{index}"),
                name: mount.name.clone(),
                path: display,
                readonly: mount.readonly,
                implicit: false,
                online: canonical.is_some(),
            },
            root: canonical,
        });
    }

    if !online_any {
        let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        mounts.push(MountEntry {
            info: MountPoint {
                id: "m0".to_string(),
                name: "Working Directory".to_string(),
                path: crate::core::fs::normalize_display(&canon),
                readonly: false,
                implicit: true,
                online: true,
            },
            root: Some(canon),
        });
        return MountSet {
            mounts,
            implicit: true,
        };
    }

    MountSet {
        mounts,
        implicit: false,
    }
}

fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------
// 路径解析
// ---------------------------------------------------------------------------

/// 解析一个**已存在**的路径（绝对路径或相对挂载点根的相对路径），
/// 返回 canonicalize 后的真实路径。
pub fn resolve_existing(mount: &MountEntry, raw: &str) -> Result<PathBuf, FsError> {
    let root = mount.canon_root()?;
    let candidate = absolute(mount, raw)?;
    let canon = std::fs::canonicalize(&candidate)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{raw}")))?;
    if !canon.starts_with(root) {
        // 区分两种逃逸：本身是软链接（目标在挂载点外）单独报 E_SYMLINK_ESCAPE，
        // 其余（含中间路径段是软链接）统一按 ../ 穿越处理（设计文档 3.2.7 / 6.6）。
        let via_symlink = std::fs::symlink_metadata(&candidate)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        return Err(if via_symlink {
            FsError::SymlinkEscape("软链接指向挂载点外".to_string())
        } else {
            FsError::PathTraversal("该路径不在挂载点内（可能经软链接逃逸）".to_string())
        });
    }
    Ok(canon)
}

/// 解析一个存在的路径，但**不跟随最后一段**（属性页 / 列表用 lstat，设计文档 3.2.7）。
///
/// 只 canonicalize 父目录，最后一段原样保留 —— 这样软链接本身能被 lstat 看到，
/// 断链也能查询属性（普通 `canonicalize` 会直接 ENOENT）。
pub fn resolve_existing_nofollow(mount: &MountEntry, raw: &str) -> Result<PathBuf, FsError> {
    let root = mount.canon_root()?;
    let candidate = absolute(mount, raw)?;
    let (Some(parent), Some(name)) = (candidate.parent(), candidate.file_name()) else {
        return Err(FsError::BadRequest("path 缺少文件名".to_string()));
    };
    let parent_canon = std::fs::canonicalize(parent)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{raw}")))?;
    if !parent_canon.starts_with(root) {
        return Err(FsError::PathTraversal("该路径不在挂载点内".to_string()));
    }
    let path = parent_canon.join(name);
    std::fs::symlink_metadata(&path)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{raw}")))?;
    Ok(path)
}

/// 解析「父目录已存在 + 子名待创建」的目标（新建 / 复制目标）。
///
/// 只 canonicalize 父目录；名字做纯字符串校验，不碰磁盘。
pub fn resolve_child(mount: &MountEntry, parent: &str, name: &str) -> Result<PathBuf, FsError> {
    let root = mount.canon_root()?;
    validate_name(name)?;

    let parent_raw = absolute(mount, parent)?;
    let parent_canon = std::fs::canonicalize(&parent_raw)
        .map_err(|_| FsError::NotFound(format!("目录不存在：{parent}")))?;
    if !parent_canon.starts_with(root) {
        return Err(FsError::PathTraversal(
            "目标目录不在挂载点内".to_string(),
        ));
    }
    if !parent_canon.is_dir() {
        return Err(FsError::BadRequest("目标是文件，不能作为父目录".to_string()));
    }
    Ok(parent_canon.join(name))
}

/// 把用户传入的路径变成绝对路径：绝对路径原样，相对路径按挂载点根拼接。
fn absolute(mount: &MountEntry, raw: &str) -> Result<PathBuf, FsError> {
    let trimmed = raw.trim();
    if trimmed.contains('\0') || trimmed.chars().any(|c| c.is_control()) {
        return Err(FsError::BadRequest("path 含非法字符".to_string()));
    }

    // 第一道闸：显式拒绝任何 ../ 上跳
    for component in Path::new(trimmed).components() {
        if matches!(component, Component::ParentDir) {
            return Err(FsError::PathTraversal(
                "禁止使用 ../ 进行路径穿越".to_string(),
            ));
        }
    }

    if trimmed.is_empty() {
        return mount.canon_root().cloned();
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        return Ok(path);
    }
    // 相对路径：既可能是「挂载点内的相对段」，也可能是「展示路径的前缀形式」，
    // 统一按挂载点根拼接（前端一律传绝对路径，这里只是兜底）
    Ok(mount.canon_root()?.join(path))
}

// ---------------------------------------------------------------------------
// 名称校验（设计文档 3.2.6.1）
// ---------------------------------------------------------------------------

/// 单段文件名校验。任何一条不满足都直接拒绝，不做「自动纠正」。
pub fn validate_name(name: &str) -> Result<(), FsError> {
    let name = name;
    if name.is_empty() {
        return Err(FsError::BadRequest("名称不能为空".to_string()));
    }
    if name == "." || name == ".." {
        return Err(FsError::BadRequest("名称不能是 . 或 ..".to_string()));
    }
    if name.contains('\0') || name.chars().any(|c| c.is_control()) {
        return Err(FsError::BadRequest("名称含控制字符".to_string()));
    }
    if name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(FsError::BadRequest(
            "名称不能包含 / \\ : * ? \" < > |".to_string(),
        ));
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(FsError::BadRequest(
            "名称不能以空格或点结尾（Windows 会静默截断）".to_string(),
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(FsError::BadRequest(format!(
            "名称超过 {MAX_NAME_BYTES} 字节"
        )));
    }
    if is_reserved_device_name(name) {
        return Err(FsError::BadRequest(
            "该名称是 Windows 保留设备名".to_string(),
        ));
    }
    if name.contains(PART_MARK) {
        return Err(FsError::BadRequest(
            "包含保留标记 .asmoday-part".to_string(),
        ));
    }
    Ok(())
}

/// Windows 保留设备名（带扩展名同样保留：`CON.txt` 也不可创建）
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// 是否为运行期生成的 `.asmoday-part` 临时文件（列表过滤 + 启动清理共用）
pub fn is_part_file(name: &str) -> bool {
    let Some(index) = name.rfind(PART_MARK) else {
        return false;
    };
    let suffix = &name[index + PART_MARK.len()..];
    suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// 整条路径的最后一段是否为临时文件。
///
/// 设计文档 3.2.6.4（v3.4.0）：临时文件除列表过滤外，**不可预览/下载**——
/// raw / text / thumb 三个字节出口都要在解析完路径后拦一道。
pub fn is_part_path(path: &Path) -> bool {
    path.file_name()
        .map(|name| is_part_file(&name.to_string_lossy()))
        .unwrap_or(false)
}

/// 生成 `.asmoday-part` 临时文件名（8 位 CSPRNG 随机码）
pub fn part_name(original: &str) -> String {
    use rand::Rng as _;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let mut suffix = String::with_capacity(8);
    for _ in 0..8 {
        suffix.push(ALPHABET[rng.random_range(0..ALPHABET.len())] as char);
    }
    format!("{original}{PART_MARK}{suffix}")
}

// ---------------------------------------------------------------------------
// 原子落盘原语（设计文档 1.4 第 4 条）
// ---------------------------------------------------------------------------

/// 移动并**保证不覆盖**：目标已存在时返回 `FsError::Conflict`。
///
/// - Windows：`MoveFileExW` 不带 `MOVEFILE_REPLACE_EXISTING`；
/// - Linux：`renameat2(RENAME_NOREPLACE)`，内核不支持时退化为硬链接占位；
/// - 其它平台：先 `symlink_metadata` 检查再 `rename`（非原子，已在文档标注）。
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<(), FsError> {
    if symlink_exists(to) {
        return Err(FsError::Conflict(format!(
            "目标已存在：{}",
            crate::core::fs::normalize_display(to)
        )));
    }

    match platform_rename_no_replace(from, to) {
        Ok(()) => Ok(()),
        Err(err) if is_already_exists(&err) => Err(FsError::Conflict(format!(
            "目标已存在：{}",
            crate::core::fs::normalize_display(to)
        ))),
        Err(err) => Err(FsError::Io(format!("重命名失败：{err}"))),
    }
}

/// 覆盖式移动（仅在 `on_conflict=overwrite` 且目标确为文件时使用）
pub fn rename_replace(from: &Path, to: &Path) -> Result<(), FsError> {
    std::fs::rename(from, to).map_err(|e| FsError::Io(format!("替换失败：{e}")))
}

/// 用 `lstat` 判断是否存在（不跟随软链接）
pub fn symlink_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn is_already_exists(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        return true;
    }
    // Windows 目标存在时返回 ERROR_ALREADY_EXISTS(183) / ERROR_FILE_EXISTS(80)
    matches!(err.raw_os_error(), Some(80) | Some(183) | Some(17))
}

#[cfg(windows)]
fn platform_rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // MOVEFILE_WRITE_THROUGH = 0x8；刻意不带 MOVEFILE_REPLACE_EXISTING
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x8) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;

    unsafe extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const i8,
            newdirfd: i32,
            newpath: *const i8,
            flags: u32,
        ) -> i32;
    }

    let from_c = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to_c = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;

    let rc = unsafe {
        renameat2(
            AT_FDCWD,
            from_c.as_ptr(),
            AT_FDCWD,
            to_c.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    // 老内核 / 不支持的 FS：退回「硬链接占位 + 删除源」，链接本身是原子的
    if matches!(err.raw_os_error(), Some(38) /* ENOSYS */ | Some(22) /* EINVAL */) {
        std::fs::hard_link(from, to)?;
        std::fs::remove_file(from)?;
        return Ok(());
    }
    Err(err)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    // macOS / 其它：RENAME_EXCL 未逐个平台声明，先做存在性检查再改名。
    // 单管理员场景下这个竞态窗口可以接受（已写入文档「已知限制」）。
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(from, to)
}

// ---------------------------------------------------------------------------
// 自动命名（`on_conflict=rename`，设计文档 3.2.6.3）
// ---------------------------------------------------------------------------

/// 把名字拆成「基础名 + 扩展名」。
///
/// - `file.tar.gz` → (`file.tar`, `.gz`)
/// - `.env` → (`.env`, "")（隐藏文件视为无扩展名，不产生尾部点）
/// - `README` → (`README`, "")
pub fn split_name(name: &str) -> (String, String) {
    if name.starts_with('.') && !name[1..].contains('.') {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(index) if index > 0 => (name[..index].to_string(), name[index..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// 生成带序号的名字：`基础名 (n).扩展名`，n 从 1 开始。
///
/// 结果按 UTF-8 字符边界截断基础名，保证单段不超过 `MAX_NAME_BYTES`。
pub fn numbered_name(base: &str, ext: &str, n: usize) -> String {
    let suffix = format!(" ({n}){ext}");
    let budget = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut stem = base.to_string();
    if stem.len() > budget {
        stem = truncate_utf8(&stem, budget);
    }
    format!("{stem}{suffix}")
}

/// 按字符边界截断到最多 `bytes` 字节
fn truncate_utf8(input: &str, bytes: usize) -> String {
    if input.len() <= bytes {
        return input.to_string();
    }
    let mut end = bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dangerous_names() {
        for name in [
            "", ".", "..", "a/b", "a\\b", "C:x", "a*b", "a?b", "a|b", "a\"b", "a<b", "a>b",
            "trail ", "trail.", "CON", "con.txt", "LPT1", "a\0b", "x.asmoday-part.abc12345",
        ] {
            assert!(validate_name(name).is_err(), "{name} 应被拒绝");
        }
    }

    #[test]
    fn accepts_normal_names() {
        for name in ["a.txt", "照片 2024", ".env", "归档.tar.gz", "a..b", "COM10", "NULx"] {
            assert!(validate_name(name).is_ok(), "{name} 应被接受");
        }
    }

    #[test]
    fn splits_names_without_trailing_dot() {
        assert_eq!(split_name("file.tar.gz"), ("file.tar".into(), ".gz".into()));
        assert_eq!(split_name(".env"), (".env".into(), "".into()));
        assert_eq!(split_name("README"), ("README".into(), "".into()));
        assert_eq!(split_name("a.txt"), ("a".into(), ".txt".into()));
    }

    #[test]
    fn numbered_names_keep_extension() {
        assert_eq!(numbered_name("file.tar", ".gz", 1), "file.tar (1).gz");
        assert_eq!(numbered_name(".env", "", 2), ".env (2)");
        let long = "汉".repeat(200);
        let result = numbered_name(&long, ".mp4", 999);
        assert!(result.len() <= MAX_NAME_BYTES, "超长名必须被截断");
        assert!(result.ends_with(" (999).mp4"));
        // 不能截出半个多字节字符
        assert!(result.chars().all(|c| c == '汉' || c.is_ascii()));
    }

    #[test]
    fn detects_part_files() {
        assert!(is_part_file("movie.mp4.asmoday-part.a1b2c3d4"));
        assert!(!is_part_file("movie.mp4.asmoday-part.A1B2C3D4"));
        assert!(!is_part_file("movie.mp4.asmoday-part.a1b2c3d"));
        assert!(!is_part_file("movie.mp4"));
    }

    #[test]
    fn part_path_checks_last_segment() {
        // 整条路径只看最后一段：临时文件不可预览/下载（设计文档 3.2.6.4，v3.4.0）
        assert!(is_part_path(Path::new("/mnt/a/movie.mp4.asmoday-part.a1b2c3d4")));
        assert!(is_part_path(Path::new("movie.mp4.asmoday-part.a1b2c3d4")));
        assert!(!is_part_path(Path::new("/mnt/a/movie.mp4")));
        // 临时文件所在目录本身不受影响
        assert!(!is_part_path(Path::new("/mnt/x.asmoday-part.a1b2c3d4/movie.mp4")));
    }

    #[test]
    fn part_name_is_random_and_marked() {
        let a = part_name("x.bin");
        let b = part_name("x.bin");
        assert!(is_part_file(&a) && is_part_file(&b));
        assert_ne!(a, b);
    }
}
