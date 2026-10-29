//! 文件变更操作内核（设计文档 3.2.6）。
//!
//! 这里只做「纯文件系统」的事情，不碰 HTTP、不碰数据库；handler 负责鉴权、
//! 归属校验、加锁与元数据联动。所有函数都是**同步阻塞**的，调用方用
//! `spawn_blocking` 包一层 —— 目录遍历与流式复制不该占着 async 执行器。
//!
//! 铁律：
//! 1. 防覆盖只能靠原子 no-replace（`path_guard::rename_no_replace`），
//!    预检查只用于「提前统计与友好提示」；
//! 2. 跨挂载点 / 大文件复制一律「先写 `.asmoday-part.<8位>`，校验后再改正式名」，
//!    任何一步失败都保留 `.part` 并返回错误，绝不留下看似正常的半成品；
//! 3. 目录冲突永不合并：`overwrite` 对目录直接 `E_DIR_CONFLICT`。

use std::collections::HashSet;
use std::time::Duration;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::fs::{normalize_display, FsError};
use crate::core::path_guard::{
    is_part_file, numbered_name, part_name, rename_no_replace, rename_replace, split_name,
    validate_name,
};

/// 单次 copy / move / delete 的文件数上限（设计文档 1.3 第 7 条）
pub const MAX_ITEMS: usize = 100;
/// 单次 copy / move / delete 的总字节上限：2 GiB
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 自动命名最大序号
pub const MAX_NUMBER: usize = 999;
/// `.part` → 正式名的占位重试次数
pub const PLACE_RETRY: usize = 10;
/// 流式复制的缓冲区（256 KiB，低功耗设备上也不会把内存顶上去）
const COPY_BUFFER: usize = 256 * 1024;

/// 覆盖策略（`ask` 由前端解析成具体值，后端不接受）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    /// 覆盖同名文件（目录仍然拒绝）
    Overwrite,
    /// 自动改名 `名称 (n).扩展名`
    #[default]
    Rename,
    /// 跳过
    Skip,
}

/// 逐项结果（设计文档 6.2 的 `results[]`）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ItemResult {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

impl ItemResult {
    pub fn ok(source: &Path, new_path: &Path) -> Self {
        Self {
            path: normalize_display(source),
            source: None,
            ok: true,
            new_path: Some(normalize_display(new_path)),
            error_code: None,
            message: None,
            skipped: false,
        }
    }

    pub fn skipped(source: &Path, message: &str) -> Self {
        Self {
            path: normalize_display(source),
            source: None,
            ok: false,
            new_path: None,
            error_code: Some("E_CONFLICT"),
            message: Some(message.to_string()),
            skipped: true,
        }
    }

    pub fn failed(source: &Path, err: &FsError) -> Self {
        Self {
            path: normalize_display(source),
            source: None,
            ok: false,
            new_path: None,
            error_code: Some(crate::api::error::code_of(err)),
            message: Some(err.message()),
            skipped: false,
        }
    }
}

/// 汇总（设计文档 6.2 的 `summary`）
#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub fn summarize(results: &[ItemResult]) -> Summary {
    Summary {
        ok: results.iter().filter(|r| r.ok).count(),
        failed: results
            .iter()
            .filter(|r| !r.ok && !r.skipped)
            .count(),
        skipped: results.iter().filter(|r| r.skipped).count(),
    }
}

// ---------------------------------------------------------------------------
// 上限统计
// ---------------------------------------------------------------------------

/// 统计一批路径的「项数 + 总字节」。
///
/// 遍历中一旦超限**立即中止**并返回对应错误 —— 不能先把 50 万文件数完再告诉用户
/// 「太多了」，那台低功耗设备已经卡死了。符号链接按链接本身计（不跟随）。
pub fn measure(paths: &[PathBuf]) -> Result<(usize, u64), FsError> {
    let mut items = 0usize;
    let mut bytes = 0u64;

    for path in paths {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|_| FsError::NotFound(format!("路径不存在：{}", normalize_display(path))))?;
        if meta.is_dir() {
            measure_dir(path, &mut items, &mut bytes)?;
        } else {
            items += 1;
            bytes = bytes.saturating_add(meta.len());
            check_limits(items, bytes)?;
        }
    }
    Ok((items, bytes))
}

fn measure_dir(dir: &Path, items: &mut usize, bytes: &mut u64) -> Result<(), FsError> {
    let reader = std::fs::read_dir(dir)
        .map_err(|e| FsError::Io(format!("读取目录失败 {}：{e}", normalize_display(dir))))?;
    for entry in reader {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.file_type() else { continue };
        *items += 1;
        check_limits(*items, *bytes)?;

        if meta.is_dir() {
            measure_dir(&entry.path(), items, bytes)?;
        } else if meta.is_file() {
            if let Ok(meta) = entry.metadata() {
                *bytes = bytes.saturating_add(meta.len());
                check_limits(*items, *bytes)?;
            }
        }
    }
    Ok(())
}

/// 「删除确认」用的**不失败**统计（设计文档 3.2.6.5）。
///
/// 与 [`measure`] 的区别：这里的目的只是把「将删除多少项、共多大」提前告诉用户，
/// 因此超限不报错，而是数到 `cap` 就停并把 `truncated` 置位 —— 目录再大也不许
/// 把确认弹窗卡住。读不到的条目（权限 / 断链）直接跳过，不影响其余统计。
pub fn survey(paths: &[PathBuf], cap: usize) -> (usize, u64, bool) {
    let mut acc = (0usize, 0u64);
    let mut truncated = false;
    'outer: for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        // 顶层路径本身也计入（删目录时目录自己也占一项），否则空目录会显示「0 项」
        acc.0 += 1;
        if meta.is_dir() {
            if !survey_dir(path, &mut acc, cap) {
                truncated = true;
                break 'outer;
            }
        } else {
            acc.1 = acc.1.saturating_add(meta.len());
        }
        if acc.0 >= cap {
            truncated = true;
            break 'outer;
        }
    }
    (acc.0, acc.1, truncated)
}

/// 返回 `false` 表示已经数到上限，调用方应当停止
fn survey_dir(dir: &Path, acc: &mut (usize, u64), cap: usize) -> bool {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in reader {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        acc.0 += 1;
        if file_type.is_dir() {
            if !survey_dir(&entry.path(), acc, cap) {
                return false;
            }
        } else if file_type.is_file()
            && let Ok(meta) = entry.metadata()
        {
            acc.1 = acc.1.saturating_add(meta.len());
        }
        if acc.0 >= cap {
            return false;
        }
    }
    true
}

fn check_limits(items: usize, bytes: u64) -> Result<(), FsError> {
    if items > MAX_ITEMS {
        return Err(FsError::TooMany(format!(
            "单次操作最多 {MAX_ITEMS} 项，请改用终端 cp / mv"
        )));
    }
    if bytes > MAX_BYTES {
        return Err(FsError::TooLarge(format!(
            "单次操作最多 {} GiB，请改用终端 cp / mv",
            MAX_BYTES / 1024 / 1024 / 1024
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 新建 / 重命名
// ---------------------------------------------------------------------------

/// 新建目录
pub fn mkdir(parent: &Path, name: &str) -> Result<PathBuf, FsError> {
    validate_name(name)?;
    let target = parent.join(name);
    match std::fs::create_dir(&target) {
        Ok(()) => Ok(target),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(FsError::Conflict(
            format!("同名项已存在：{name}"),
        )),
        Err(err) => Err(FsError::Io(format!("创建目录失败：{err}"))),
    }
}

/// 新建空文件
pub fn touch(parent: &Path, name: &str) -> Result<PathBuf, FsError> {
    validate_name(name)?;
    let target = parent.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(_) => Ok(target),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(FsError::Conflict(
            format!("同名项已存在：{name}"),
        )),
        Err(err) => Err(FsError::Io(format!("创建文件失败：{err}"))),
    }
}

/// 重命名（同目录改名，不跨目录）。
///
/// 大小写不敏感文件系统上 `a.txt → A.txt` 是合法重命名：目标「存在」但其实是
/// 同一个文件，此时先改成临时名再改目标名，绕开 `rename` 的「目标已存在」判定。
pub fn rename_entry(path: &Path, new_name: &str) -> Result<PathBuf, FsError> {
    validate_name(new_name)?;
    let parent = path
        .parent()
        .ok_or_else(|| FsError::BadRequest("不能重命名挂载点根目录".to_string()))?;
    let old_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| FsError::BadRequest("路径没有文件名".to_string()))?;

    if old_name == new_name {
        return Ok(path.to_path_buf());
    }
    let target = parent.join(new_name);

    if crate::core::path_guard::symlink_exists(&target) {
        let same_file = old_name.to_lowercase() == new_name.to_lowercase();
        if !same_file {
            return Err(FsError::Conflict(format!("同名项已存在：{new_name}")));
        }
        // 仅大小写变化：走临时名两步改名
        let temp = parent.join(temp_name());
        std::fs::rename(path, &temp).map_err(|e| FsError::Io(format!("重命名失败：{e}")))?;
        match rename_no_replace(&temp, &target) {
            Ok(()) => {}
            Err(err) => {
                // 尽力恢复原名，避免把用户文件留在临时名上
                let _ = std::fs::rename(&temp, path);
                return Err(err);
            }
        }
        return Ok(target);
    }

    rename_no_replace(path, &target)?;
    Ok(target)
}

fn temp_name() -> String {
    use rand::Rng as _;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..8)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!(".asmoday-rename-{suffix}")
}

// ---------------------------------------------------------------------------
// 复制 / 移动
// ---------------------------------------------------------------------------

/// 批量操作期间共享的「已占用名字」集合（设计文档 3.2.6.3）。
///
/// 键做小写归一化，同时包含磁盘上已存在的名字与本批次已分配的名字，
/// 避免 `a.txt` 与 `A.txt` 在同一批次里撞车。
pub struct NameBook {
    dir: PathBuf,
    taken: HashSet<String>,
}

impl NameBook {
    pub fn new(dir: &Path) -> Self {
        let taken = std::fs::read_dir(dir)
            .map(|reader| {
                reader
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            dir: dir.to_path_buf(),
            taken,
        }
    }

    fn exists(&self, name: &str) -> bool {
        self.taken.contains(&name.to_lowercase())
            || crate::core::path_guard::symlink_exists(&self.dir.join(name))
    }

    fn mark(&mut self, name: &str) {
        self.taken.insert(name.to_lowercase());
    }
}

/// 冲突决策结果
enum Placement {
    /// 用这个名字落盘；`overwrite=true` 时才允许替换已存在的同名文件
    Use(String, bool),
    Skip,
}

fn resolve_placement(
    book: &mut NameBook,
    name: &str,
    is_dir_src: bool,
    conflict: OnConflict,
) -> Result<Placement, FsError> {
    if !book.exists(name) {
        book.mark(name);
        return Ok(Placement::Use(name.to_string(), false));
    }

    match conflict {
        OnConflict::Skip => Ok(Placement::Skip),
        OnConflict::Overwrite => {
            // 目录不接受覆盖：既不合并内容，也不递归删除目标（设计文档 3.2.6.2）
            if is_dir_src || book.dir.join(name).is_dir() {
                return Err(FsError::DirConflict(format!(
                    "目标目录已存在，拒绝覆盖：{name}"
                )));
            }
            book.mark(name);
            Ok(Placement::Use(name.to_string(), true))
        }
        OnConflict::Rename => {
            let (base, ext) = split_name(name);
            for n in 1..=MAX_NUMBER {
                let candidate = numbered_name(&base, &ext, n);
                if !book.exists(&candidate) {
                    book.mark(&candidate);
                    return Ok(Placement::Use(candidate, false));
                }
            }
            Err(FsError::RetryExhausted(format!(
                "自动命名已用尽（{name} 的 1..{MAX_NUMBER} 全部被占用）"
            )))
        }
    }
}

/// 复制单条（文件或整个目录）。返回 `None` 表示按策略跳过。
pub fn copy_entry(
    src: &Path,
    dest_dir: &Path,
    book: &mut NameBook,
    conflict: OnConflict,
) -> Result<Option<PathBuf>, FsError> {
    let meta = std::fs::symlink_metadata(src)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{}", normalize_display(src))))?;
    let is_dir = meta.is_dir();
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| FsError::BadRequest("源路径没有文件名".to_string()))?;

    let Placement::Use(first_name, overwrite) = resolve_placement(book, &name, is_dir, conflict)?
    else {
        return Ok(None);
    };

    if is_dir {
        let target = dest_dir.join(&first_name);
        std::fs::create_dir(&target)
            .map_err(|e| FsError::Io(format!("创建目标目录失败：{e}")))?;
        copy_dir_contents(src, &target)?;
        return Ok(Some(target));
    }

    // 软链接：复制链接本身（不跟随），目标是否逃逸由上层校验
    if meta.file_type().is_symlink() {
        let target = dest_dir.join(&first_name);
        copy_symlink(src, &target)?;
        return Ok(Some(target));
    }

    copy_file_with_retry(src, dest_dir, &name, &first_name, overwrite, conflict, book)
}

/// 文件复制：**一次**流式复制到 `.part`，再反复尝试占位改名。
///
/// 占位时目标被别的操作抢先创建（`E_CONFLICT`）→ 按 `rename` 策略换下一个序号重试，
/// 最多 `PLACE_RETRY` 次（设计文档 3.2.6.3）。失败时 `.part` 保留，便于排查。
fn copy_file_with_retry(
    src: &Path,
    dest_dir: &Path,
    original_name: &str,
    first_name: &str,
    overwrite: bool,
    conflict: OnConflict,
    book: &mut NameBook,
) -> Result<Option<PathBuf>, FsError> {
    let before = std::fs::metadata(src)
        .map_err(|e| FsError::Io(format!("读取源文件信息失败：{e}")))?;

    let part = dest_dir.join(part_name(original_name));
    // DEBUG 通道：复制落点与临时文件全路径，排障「创建临时文件失败」类问题时的关键线索
    crate::log_debug!(
        "copying {src_display} -> {part_display} ({size} bytes)",
        src_display = normalize_display(src),
        part_display = normalize_display(&part),
        size = before.len()
    );
    let copied = stream_copy(src, &part)?;

    if copied != before.len() {
        let _ = std::fs::remove_file(&part);
        return Err(FsError::Io(format!(
            "复制长度不一致（期望 {} 字节，实际 {copied} 字节）",
            before.len()
        )));
    }
    // 源文件在复制过程中被改写 → 结果不可信，保留 .part 供排查
    if let (Ok(after), Ok(before_mtime)) = (std::fs::metadata(src), before.modified()) {
        if after.len() != before.len() || after.modified().ok() != Some(before_mtime) {
            return Err(FsError::Io(
                "复制过程中源文件发生变化，已保留 .asmoday-part 临时文件".to_string(),
            ));
        }
    }

    let mut candidate = first_name.to_string();
    for attempt in 0..=PLACE_RETRY {
        let target = dest_dir.join(&candidate);
        if overwrite && std::fs::symlink_metadata(&target).is_ok() {
            rename_replace(&part, &target)?;
            return Ok(Some(target));
        }
        match rename_no_replace(&part, &target) {
            Ok(()) => return Ok(Some(target)),
            Err(FsError::Conflict(_)) if conflict == OnConflict::Rename && attempt < PLACE_RETRY => {
                let Placement::Use(next, _) =
                    resolve_placement(book, original_name, false, conflict)?
                else {
                    return Ok(None);
                };
                candidate = next;
            }
            Err(err) => return Err(err),
        }
    }
    Err(FsError::RetryExhausted(
        "自动命名占位重试次数用尽".to_string(),
    ))
}

/// 流式复制（禁止 `read()` 整个文件，设计文档 8.5 第 4 条）
fn stream_copy(src: &Path, part: &Path) -> Result<u64, FsError> {
    let input = std::fs::File::open(src)
        .map_err(|e| map_io_error(e, "打开源文件失败", src))?;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, input);
    let output = std::fs::File::create(part)
        .map_err(|e| map_io_error(e, "创建临时文件失败", part))?;
    let mut writer = BufWriter::with_capacity(COPY_BUFFER, output);

    let mut total = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| map_io_error(e, "读取源文件失败", src))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|e| map_write_error(e, part))?;
        total += read as u64;
    }
    writer
        .flush()
        .map_err(|e| map_write_error(e, part))?;
    Ok(total)
}

/// 磁盘满要单独成码（E_NO_SPACE），前端能给出「换个位置」的明确建议
fn map_write_error(err: std::io::Error, path: &Path) -> FsError {
    let kind = err.kind();
    if kind == std::io::ErrorKind::StorageFull
        || matches!(err.raw_os_error(), Some(28) /* ENOSPC */ | Some(112) /* ERROR_DISK_FULL */)
    {
        return FsError::NoSpace(format!(
            "目标磁盘空间不足（写入失败：{}）",
            normalize_display(path)
        ));
    }
    map_io_error(err, "写入失败", path)
}

/// 平台 IO 错误归码：共享冲突（32/33）→ `E_BUSY`，拒绝访问（5）→ `E_FORBIDDEN`。
///
/// 这两类都是**用户可自救**的状态（关掉占用程序 / 改文件夹权限或以管理员运行），
/// 报成 500「服务器内部错误」既吓人又没信息。典型场景：下载器还在写
/// Downloads 里的文件，用户马上就复制它；或目标文件夹带着失效 SID。
fn map_io_error(err: std::io::Error, action: &str, path: &Path) -> FsError {
    if matches!(err.raw_os_error(), Some(32) /* ERROR_SHARING_VIOLATION */ | Some(33) /* ERROR_LOCK_VIOLATION */) {
        return FsError::Busy(format!(
            "{action}：文件正被其他程序占用（{}）",
            normalize_display(path)
        ));
    }
    // os error 5（ERROR_ACCESS_DENIED）几乎总是「目录 ACL 拒绝当前用户写入」：
    // 典型成因是文件夹带着旧系统 / 其他机器拷贝来的失效 SID，或由管理员权限的
    // 下载器创建，普通权限的 asmoday 只能读不能写；也可能是杀毒 / 受控文件夹
    // 访问拦截了未签名的新构建 exe。这是用户可自救的状态，给足排查路径。
    if err.raw_os_error() == Some(5) /* ERROR_ACCESS_DENIED */ {
        return FsError::Forbidden(format!(
            "{action}：目标位置拒绝写入（拒绝访问，os error 5）—— {path_display}。\
             常见原因：① 文件夹「安全」权限未授予当前用户写入（右键 → 属性 → 安全，\
             旧系统 / 其他电脑拷来的文件夹常带失效账户）；② 杀毒软件或 Windows \
             「受控文件夹访问」拦截了 asmoday.exe；③ 该文件夹由管理员权限的程序创建，\
             需以管理员身份运行 asmoday。",
            path_display = normalize_display(path)
        ));
    }
    FsError::Io(format!("{action}：{err}"))
}

fn copy_dir_contents(src: &Path, dest: &Path) -> Result<(), FsError> {
    let reader = std::fs::read_dir(src)
        .map_err(|e| FsError::Io(format!("读取源目录失败：{e}")))?;
    for entry in reader {
        let entry = entry.map_err(|e| FsError::Io(format!("遍历源目录失败：{e}")))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let Ok(meta) = std::fs::symlink_metadata(&from) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            copy_symlink(&from, &to)?;
        } else if meta.is_dir() {
            std::fs::create_dir(&to)
                .map_err(|e| FsError::Io(format!("创建目标目录失败：{e}")))?;
            copy_dir_contents(&from, &to)?;
        } else {
            copy_file_plain(&from, &to)?;
        }
    }
    Ok(())
}

/// 目录内部复制（目标目录是本次新建的，正常不会冲突）
fn copy_file_plain(src: &Path, target: &Path) -> Result<(), FsError> {
    let dir = target
        .parent()
        .ok_or_else(|| FsError::BadRequest("目标父目录缺失".to_string()))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let part = dir.join(part_name(&name));
    let copied = stream_copy(src, &part)?;
    let expected = std::fs::metadata(src).map(|m| m.len()).unwrap_or(copied);
    if copied != expected {
        let _ = std::fs::remove_file(&part);
        return Err(FsError::Io(format!(
            "复制长度不一致（期望 {expected} 字节，实际 {copied} 字节）"
        )));
    }
    rename_no_replace(&part, target)
}

/// 复制软链接本身（读 target 字符串重建链接）
fn copy_symlink(src: &Path, target: &Path) -> Result<(), FsError> {
    let link = std::fs::read_link(src)
        .map_err(|e| FsError::Io(format!("读取软链接失败：{e}")))?;

    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(&link, target);
    #[cfg(windows)]
    let result = {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        // 目标本身可能是目录；探测失败时按文件链接处理（Windows 允许悬空链接）
        if link.is_dir() {
            symlink_dir(&link, target)
        } else {
            symlink_file(&link, target)
        }
    };
    result.map_err(|e| FsError::Io(format!("创建软链接失败：{e}")))
}

/// 移动单条。同挂载点走原子 rename，跨挂载点降级为「复制 → 校验 → 删源」。
///
/// 跨挂载点删除源必须是**最后一步**（设计文档 3.2.6.4）：复制失败时源文件必须还在。
pub fn move_entry(
    src: &Path,
    dest_dir: &Path,
    book: &mut NameBook,
    conflict: OnConflict,
    same_mount: bool,
) -> Result<Option<PathBuf>, FsError> {
    let meta = std::fs::symlink_metadata(src)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{}", normalize_display(src))))?;
    let is_dir = meta.is_dir();
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| FsError::BadRequest("源路径没有文件名".to_string()))?;

    if same_mount {
        let Placement::Use(final_name, overwrite) = resolve_placement(book, &name, is_dir, conflict)?
        else {
            return Ok(None);
        };
        let target = dest_dir.join(&final_name);
        if overwrite {
            rename_replace(src, &target)?;
        } else {
            rename_no_replace(src, &target)?;
        }
        return Ok(Some(target));
    }

    let Some(target) = copy_entry(src, dest_dir, book, conflict)? else {
        return Ok(None);
    };
    // 删源是最后一步，且失败必须独立上报（设计文档 3.2.6）：
    // 此刻数据**已经**落到目标位置，如果这里抛通用错误，前端会提示「移动失败」，
    // 用户要么以为文件没动、要么以为复制也失败了，事后还得自己找哪个版本是真的。
    remove_path(src).map_err(|err| {
        FsError::SourceDeleteFailed(format!(
            "文件已成功写入目标位置，但原文件清理失败，请手动检查并删除原文件：{}（{}）",
            normalize_display(src),
            err.message()
        ))
    })?;
    Ok(Some(target))
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

/// 物理硬删：目录递归、软链接只删链接本身（不跟随）
pub fn remove_path(path: &Path) -> Result<(), FsError> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| FsError::NotFound(format!("路径不存在：{}", normalize_display(path))))?;

    let result = if meta.file_type().is_symlink() {
        // 只删链接本身，绝不跟随（调用方必须传 nofollow 解析出来的链接路径）。
        // Windows 的目录链接要 remove_dir 才能摘掉 reparse point，remove_file 会 ACCESS_DENIED
        std::fs::remove_file(path).or_else(|err| {
            if cfg!(windows) {
                std::fs::remove_dir(path).map_err(|_| err)
            } else {
                Err(err)
            }
        })
    } else if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };

    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Err(FsError::Io(
            format!(
                "无权限或文件被占用，删除失败：{}",
                normalize_display(path)
            ),
        )),
        Err(err) => Err(FsError::Io(format!(
            "删除失败 {}：{err}",
            normalize_display(path)
        ))),
    }
}

// ---------------------------------------------------------------------------
// `.asmoday-part` 启动清理（设计文档 3.2.6.4）
// ---------------------------------------------------------------------------

/// 启动时无差别清理 `.asmoday-part` 残留。
///
/// 同步操作不跨重启，启动瞬间存在的 `.part` 一定是异常终止留下的垃圾。
/// 运行期的「清理临时文件」按钮才按 24 小时阈值过滤，避免误删进行中的复制。
pub fn cleanup_parts(root: &Path) -> (usize, u64) {
    cleanup_parts_older_than(root, Duration::ZERO)
}

/// 运行期「清理临时文件」：只删最后修改时间早于 `min_age` 的 `.part`，
/// 避免误删正在进行中的复制（设计文档 3.2.6.4）。
pub fn cleanup_parts_older_than(root: &Path, min_age: Duration) -> (usize, u64) {
    let mut removed = 0usize;
    let mut freed = 0u64;
    let cutoff = std::time::SystemTime::now()
        .checked_sub(min_age)
        .unwrap_or(std::time::UNIX_EPOCH);
    walk_parts(root, cutoff, &mut removed, &mut freed);
    (removed, freed)
}

fn walk_parts(dir: &Path, cutoff: std::time::SystemTime, removed: &mut usize, freed: &mut u64) {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_parts(&path, cutoff, removed, freed);
            continue;
        }
        if is_part_file(&name) {
            // 太新的 .part 可能正在写入，跳过
            if let Ok(meta) = entry.metadata() {
                if meta.modified().map(|t| t > cutoff).unwrap_or(false) {
                    continue;
                }
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                *removed += 1;
                *freed += size;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 磁盘空间
// ---------------------------------------------------------------------------

/// 目标目录剩余可用字节数；探测失败返回 `None`（不能因为探测不到就拒绝操作）。
///
/// - Windows：`GetDiskFreeSpaceExW`；
/// - Unix：解析 `df -Pk`（POSIX 标准命令，避免手写 `struct statvfs` 的
///   平台差异 —— glibc 与 musl 的字段布局并不一致，写错了会直接崩）。
pub fn available_space(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                dir: *const u16,
                free_available: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }

        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free_available = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_available,
                &mut total,
                &mut total_free,
            )
        };
        if ok == 0 {
            return None;
        }
        return Some(free_available);
    }

    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("df")
            .arg("-Pk")
            .arg(dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().nth(1)?;
        let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail_kb.saturating_mul(1024))
    }
}

/// 复制/移动前的空间检查：不足直接拒绝并给出缺口（设计文档 3.2.6.2）
pub fn ensure_space(dir: &Path, needed: u64) -> Result<(), FsError> {
    let Some(available) = available_space(dir) else {
        return Ok(());
    };
    // 留 32 MiB 余量，避免刚好写满把系统盘搞挂
    let margin = 32 * 1024 * 1024;
    if available < needed.saturating_add(margin) {
        return Err(FsError::NoSpace(format!(
            "目标磁盘空间不足：需要 {:.1} MiB，可用 {:.1} MiB",
            needed as f64 / 1_048_576.0,
            available as f64 / 1_048_576.0
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_sharing_violation_maps_to_busy_not_internal() {
        // os error 32/33（文件被占用）必须映射成 E_BUSY，而不是 500 服务器内部错误
        let err = map_io_error(
            std::io::Error::from_raw_os_error(32),
            "打开源文件失败",
            Path::new("C:/Downloads/a.jpg"),
        );
        assert!(matches!(err, FsError::Busy(_)), "实际：{err:?}");
        assert_eq!(
            crate::api::error::code_of(&err),
            "E_BUSY",
            "用户可自救的占用错误不能报成 500"
        );
        assert_eq!(
            crate::api::error::code_of(&map_io_error(
                std::io::Error::from_raw_os_error(33),
                "写入失败",
                Path::new("C:/Downloads/a.jpg"),
            )),
            "E_BUSY"
        );
        // 其他 IO 错误维持 E_INTERNAL 不变
        let plain = map_io_error(
            std::io::Error::other("boom"),
            "打开源文件失败",
            Path::new("C:/a.txt"),
        );
        assert_eq!(crate::api::error::code_of(&plain), "E_INTERNAL");
    }

    #[test]
    fn access_denied_maps_to_forbidden_with_guidance() {
        // os error 5（拒绝访问，典型：目标目录 ACL 拒绝写入）必须映射成
        // E_FORBIDDEN(403) 并给出可自救的排查指引，而不是 500 服务器内部错误
        let err = map_io_error(
            std::io::Error::from_raw_os_error(5),
            "创建临时文件失败",
            Path::new("C:/mnt/lora/luwanq.txt.asmoday-part.ab12cd34"),
        );
        assert!(
            matches!(err, FsError::Forbidden(_)),
            "实际：{err:?}"
        );
        assert_eq!(
            crate::api::error::code_of(&err),
            "E_FORBIDDEN",
            "用户可自救的权限错误不能报成 500"
        );
        let text = err.message();
        assert!(text.contains("拒绝写入"), "文案缺「拒绝写入」：{text}");
        assert!(text.contains("管理员"), "文案缺「管理员」指引：{text}");
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "asmoday-fsops-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mkdir_and_touch_reject_duplicates() {
        let dir = scratch("mkdir");
        mkdir(&dir, "docs").unwrap();
        assert!(matches!(
            mkdir(&dir, "docs").unwrap_err(),
            FsError::Conflict(_)
        ));
        touch(&dir, "a.txt").unwrap();
        assert!(matches!(
            touch(&dir, "a.txt").unwrap_err(),
            FsError::Conflict(_)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_rejects_existing_and_handles_same_name() {
        let dir = scratch("rename");
        touch(&dir, "a.txt").unwrap();
        touch(&dir, "b.txt").unwrap();
        let a = dir.join("a.txt");
        // 同名 → 幂等成功
        assert_eq!(rename_entry(&a, "a.txt").unwrap(), a);
        // 撞名 → 409
        assert!(matches!(
            rename_entry(&a, "b.txt").unwrap_err(),
            FsError::Conflict(_)
        ));
        // 正常改名
        let renamed = rename_entry(&a, "c.txt").unwrap();
        assert!(renamed.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_uses_part_then_final_name() {
        let dir = scratch("copy");
        std::fs::create_dir(&dir.join("dest")).unwrap();
        std::fs::write(dir.join("src.bin"), b"hello").unwrap();

        let mut book = NameBook::new(&dir.join("dest"));
        let target = copy_entry(&dir.join("src.bin"), &dir.join("dest"), &mut book, OnConflict::Rename)
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        // 没有残留 .part
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("dest"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(!leftovers.iter().any(|n| is_part_file(n)), "{leftovers:?}");

        // 再复制一次 → 自动改名
        let second = copy_entry(&dir.join("src.bin"), &dir.join("dest"), &mut book, OnConflict::Rename)
            .unwrap()
            .unwrap();
        assert_eq!(second.file_name().unwrap(), "src (1).bin");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_refuses_directory_overwrite() {
        let dir = scratch("dircopy");
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::create_dir_all(dir.join("dest/src")).unwrap();

        let mut book = NameBook::new(&dir.join("dest"));
        let err = copy_entry(&dir.join("src"), &dir.join("dest"), &mut book, OnConflict::Overwrite)
            .unwrap_err();
        assert!(matches!(err, FsError::DirConflict(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn limits_abort_early() {
        // 101 个小文件即可触发数量上限
        let dir = scratch("limits");
        for i in 0..(MAX_ITEMS + 1) {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let err = measure(&[dir.clone()]).unwrap_err();
        assert!(matches!(err, FsError::TooMany(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survey_counts_without_failing() {
        // survey 是删除确认用的统计：超过 cap 只截断，绝不报错
        let dir = scratch("survey");
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{i}.txt")), b"12345").unwrap();
        }
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/deep.txt"), b"12").unwrap();

        // 顶层目录自己 + 5 个文件 + sub 目录 + sub/deep.txt = 8 项
        let (items, bytes, truncated) = survey(std::slice::from_ref(&dir), 100);
        assert_eq!(items, 8);
        assert_eq!(bytes, 27);
        assert!(!truncated);

        let (items, _, truncated) = survey(std::slice::from_ref(&dir), 2);
        assert_eq!(items, 2);
        assert!(truncated);

        // 不存在的路径直接跳过，不影响其余统计
        let missing = dir.join("nope");
        let (items, _, truncated) = survey(&[missing, dir.clone()], 100);
        assert_eq!(items, 8);
        assert!(!truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cleanup_parts_removes_orphans_only() {
        let dir = scratch("parts");
        std::fs::write(dir.join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.join(format!("keep.txt{}aaaaaaaa", crate::core::path_guard::PART_MARK)), b"y")
            .unwrap();
        let (removed, _) = cleanup_parts(&dir);
        assert_eq!(removed, 1);
        assert!(dir.join("keep.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
