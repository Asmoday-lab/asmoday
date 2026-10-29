//! 多平台系统垃圾文件黑名单（设计文档 3.3.7）。
//!
//! 跨平台挂载一定会产生系统元数据：Windows 的 `desktop.ini` / `Thumbs.db`、
//! macOS 的 `.DS_Store` / `._*`（AppleDouble）、Office 的 `~$*.docx` 锁文件、
//! 回收站与 Spotlight 索引目录……这些东西混进文件列表会毁掉 UI，
//! 被 WebDAV 反复同步还会在两个系统之间来回生成、最终写满 inode。
//!
//! 因此本模块是**后端硬编码的强制过滤**，优先于前端 `show_hidden`：
//! 无论开关怎么传，这些名字一律不出现在列表 / 媒体库 / PROPFIND 结果里。
//!
//! **只过滤这里列出的名字**：`.env`、`.git`、`.github` 等普通隐藏项仍由
//! `show_hidden` 控制，用户想看就能看到。

/// 精确匹配（不区分大小写）：Windows / macOS / Linux 各自的系统文件与系统目录
const EXACT_NAMES: &[&str] = &[
    // Windows
    "desktop.ini",
    "thumbs.db",
    "ehthumbs.db",
    "$recycle.bin",
    "system volume information",
    // macOS
    ".ds_store",
    ".trashes",
    ".spotlight-v100",
    ".fseventsd",
    // Linux（ext 文件系统丢失文件的恢复目录）
    "lost+found",
];

/// 前缀匹配：临时 / 元数据文件
///
/// - `~$`：Office 打开文档时生成的锁文件（`~$报告.docx`）
/// - `._`：macOS AppleDouble，给每个文件配一份资源叉（FAT/exFAT 挂载必现）
const PREFIXES: &[&str] = &["~$", "._"];

/// 后缀匹配：下载器 / 归档工具的半成品（设计文档 3.2.1）。
///
/// 这些文件写盘还没结束，出现在列表里会让人误以为「电影已经下好了」，
/// 点开播放必然失败；进媒体库还会生成一堆半截图缩略图。
///
/// - `.crdownload`：Chrome / Edge
/// - `.part`：Firefox 与多数下载工具
/// - `.!ut`：uTorrent 未完成分片
/// - `.tmp`：通用临时文件
const SUFFIXES: &[&str] = &[".crdownload", ".part", ".!ut", ".tmp"];

/// 是否是必须无条件过滤的系统垃圾项（文件或目录都适用）。
pub fn is_system_junk(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    if EXACT_NAMES.contains(&lower.as_str()) {
        return true;
    }
    if SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) {
        return true;
    }
    PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::is_system_junk;

    #[test]
    fn windows_and_macos_metadata_are_filtered() {
        for name in [
            "desktop.ini",
            "Desktop.ini",
            "Thumbs.db",
            "thumbs.DB",
            "ehthumbs.db",
            ".DS_Store",
            ".ds_store",
            "._IMG_0001.JPG",
            "~$report.docx",
            "lost+found",
            ".Trashes",
            ".Spotlight-V100",
            ".fseventsd",
            "$RECYCLE.BIN",
            "System Volume Information",
        ] {
            assert!(is_system_junk(name), "{name} 应被过滤");
        }
    }

    #[test]
    fn ordinary_hidden_files_are_not_junk() {
        // 这些必须由 show_hidden 开关控制，不能被黑名单误杀
        for name in [
            ".env",
            ".git",
            ".gitignore",
            ".github",
            ".config",
            "readme.md",
        ] {
            assert!(!is_system_junk(name), "{name} 不应被过滤");
        }
    }

    #[test]
    fn prefix_matching_is_intentional_not_accidental() {
        // 只有 `~$` 与 `._` 两个前缀；同义但不同的写法不匹配
        assert!(is_system_junk("._"));
        assert!(is_system_junk("~$x"));
        assert!(!is_system_junk("~backup.txt"));
        assert!(!is_system_junk("a._b.txt"));
        assert!(!is_system_junk(""));
    }

    #[test]
    fn half_downloaded_files_are_filtered() {
        // 设计文档 3.2.1 第 3 条：下载中/临时文件不得进列表与媒体库
        for name in [
            "电影.mkv.crdownload",
            "movie.mp4.CRDOWNLOAD",
            "ubuntu.iso.part",
            "file.bin.!ut",
            "notes.tmp",
            "Photo.JPG.TMP",
        ] {
            assert!(is_system_junk(name), "{name} 应被过滤");
        }
    }

    #[test]
    fn suffix_matching_does_not_eat_normal_names() {
        // 只是「包含」这些字样的正常文件必须留着
        for name in [
            "depart.txt",
            "partial-notes.md",
            "tmp-backup.zip",
            "part1.mp4",
            "atut.txt",
        ] {
            assert!(!is_system_junk(name), "{name} 不应被过滤");
        }
    }
}
