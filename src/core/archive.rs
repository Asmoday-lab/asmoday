//! 目录打包下载（设计文档 3.2.4）：**store 模式流式 ZIP**。
//!
//! 为什么不用 `zip` / `async_zip` 的 deflate：媒体库里 99% 的体积是已经压过的
//! JPEG / WebP / MP4，deflate 一遍只换来 CPU 和等待。store 模式（method 0）配
//! **数据描述符**（general purpose bit 3）可以边读边发：内存恒定、不落临时文件、
//! 首字节延迟等于第一个文件的读延迟。
//!
//! 代价有两个，都被显式挡住：
//! - 单个文件 ≥ 4 GiB 会溢出 32 位 size 字段 → 需要 Zip64，本模块不实现，
//!   由 `plan` 返回错误让调用方提示「请单独下载」；
//! - 条目数超过 65535 会溢出 16 位计数 → 同样在 `plan` 阶段拒绝。
//!
//! 不跟随软链接：打包一个指向 `/` 的链接会把整台机器塞进 zip。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单个文件的大小上限（store + 32 位字段）。留一格给描述符。
pub const MAX_FILE_BYTES: u64 = u32::MAX as u64 - 1;
/// 条目总数上限（16 位中央目录计数）。
pub const MAX_ENTRIES: usize = u16::MAX as usize;

/// zip 内的一个条目。
pub struct Entry {
    pub abs: PathBuf,
    /// zip 内的路径，`/` 分隔，含顶层目录名
    pub name: String,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub is_dir: bool,
}

/// 把目录摊平成条目清单（顶层是 `root` 自己的名字）。
///
/// 顺序：目录先于其内容、同级按名字排序——解压工具按顺序建目录，先来的目录
/// 还能顺手继承权限。软链接、临时文件、打不开的项直接跳过（打包失败比漏几个文件更糟）。
pub fn plan(root: &Path) -> std::io::Result<Vec<Entry>> {
    let base = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let mut entries = Vec::new();
    push_dir(root, &base, &mut entries)?;
    if entries.len() > MAX_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("条目太多（{} 项），超过压缩包 65535 项上限", entries.len()),
        ));
    }
    Ok(entries)
}

fn push_dir(dir: &Path, name: &str, out: &mut Vec<Entry>) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    // 目录条目**必须以 `/` 结尾**：少了它，解压工具会当成一个 0 字节文件，
    // 后面同名的 `Docs/xxx` 就无处可放（Expand-Archive 会直接报「项已存在」）。
    out.push(Entry {
        abs: dir.to_path_buf(),
        name: format!("{name}/"),
        size: 0,
        mtime: meta.modified().ok(),
        is_dir: true,
    });

    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    children.sort();

    for child in children {
        let Ok(meta) = std::fs::symlink_metadata(&child) else {
            continue;
        };
        // 软链接一律不跟：目标是 `/` 的链接会把整机塞进 zip，目标在挂载点外的更糟
        if meta.file_type().is_symlink() {
            continue;
        }
        let Some(file_name) = child.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if crate::core::path_guard::is_part_path(&child) {
            continue;
        }
        let child_name = format!("{name}/{file_name}");
        if meta.is_dir() {
            push_dir(&child, &child_name, out)?;
        } else {
            if meta.len() >= MAX_FILE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{file_name} 超过 4 GiB，请单独下载"),
                ));
            }
            out.push(Entry {
                abs: child,
                name: child_name,
                size: meta.len(),
                mtime: meta.modified().ok(),
                is_dir: false,
            });
        }
    }
    Ok(())
}

/// 把条目写成一个 store 模式的 zip 流。
///
/// 数据描述符里的 CRC 与长度是**边读边算**的，所以整条链路只读一遍文件。
pub async fn stream_zip<W>(entries: Vec<Entry>, sink: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    struct Central {
        name: Vec<u8>,
        crc: u32,
        size: u64,
        offset: u64,
        dos_time: u16,
        dos_date: u16,
        is_dir: bool,
    }

    let mut centrals: Vec<Central> = Vec::with_capacity(entries.len());
    let mut offset: u64 = 0;
    let mut buffer = vec![0u8; 64 * 1024];

    for entry in entries {
        let name = entry.name.as_bytes().to_vec();
        let (dos_time, dos_date) = dos_stamp(entry.mtime);
        let local_offset = offset;

        if entry.is_dir {
            let header = local_header(&name, 0x0800, 0, 0, dos_time, dos_date);
            sink.write_all(&header).await?;
            offset += header.len() as u64;
            centrals.push(Central {
                name,
                crc: 0,
                size: 0,
                offset: local_offset,
                dos_time,
                dos_date,
                is_dir: true,
            });
            continue;
        }

        // 带 bit 3（数据描述符）：长度与 CRC 先写 0，真正的值跟在数据后面
        let header = local_header(&name, 0x0808, 0, entry.size, dos_time, dos_date);
        sink.write_all(&header).await?;
        offset += header.len() as u64;

        let mut file = tokio::fs::File::open(&entry.abs).await?;
        let mut hasher = crc32fast::Hasher::new();
        let mut written: u64 = 0;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            sink.write_all(&buffer[..read]).await?;
            written += read as u64;
        }
        let crc = hasher.finalize();

        let mut descriptor = Vec::with_capacity(16);
        descriptor.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        descriptor.extend_from_slice(&crc.to_le_bytes());
        descriptor.extend_from_slice(&(written as u32).to_le_bytes());
        descriptor.extend_from_slice(&(written as u32).to_le_bytes());
        sink.write_all(&descriptor).await?;
        offset += written + descriptor.len() as u64;

        centrals.push(Central {
            name,
            crc,
            size: written,
            offset: local_offset,
            dos_time,
            dos_date,
            is_dir: false,
        });
    }

    let central_start = offset;
    for central in &centrals {
        let mut record = Vec::with_capacity(46 + central.name.len());
        record.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        record.extend_from_slice(&20u16.to_le_bytes()); // version made by
        record.extend_from_slice(&20u16.to_le_bytes()); // version needed
        record
            .extend_from_slice(&(if central.is_dir { 0x0800u16 } else { 0x0808u16 }).to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes()); // method: store
        record.extend_from_slice(&central.dos_time.to_le_bytes());
        record.extend_from_slice(&central.dos_date.to_le_bytes());
        record.extend_from_slice(&central.crc.to_le_bytes());
        record.extend_from_slice(&(central.size as u32).to_le_bytes());
        record.extend_from_slice(&(central.size as u32).to_le_bytes());
        record.extend_from_slice(&(central.name.len() as u16).to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes()); // extra
        record.extend_from_slice(&0u16.to_le_bytes()); // comment
        record.extend_from_slice(&0u16.to_le_bytes()); // disk start
        record.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        // 目录要带 0x10 属性位，否则解压工具会当普通空文件建
        let mode: u32 = if central.is_dir { 0o040755 } else { 0o100644 };
        let external = if central.is_dir {
            (mode << 16) | 0x10
        } else {
            mode << 16
        };
        record.extend_from_slice(&external.to_le_bytes());
        record.extend_from_slice(&(central.offset as u32).to_le_bytes());
        record.extend_from_slice(&central.name);
        sink.write_all(&record).await?;
        offset += record.len() as u64;
    }

    let central_size = offset - central_start;
    let mut end = Vec::with_capacity(22);
    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&(centrals.len() as u16).to_le_bytes());
    end.extend_from_slice(&(centrals.len() as u16).to_le_bytes());
    end.extend_from_slice(&(central_size as u32).to_le_bytes());
    end.extend_from_slice(&(central_start as u32).to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    sink.write_all(&end).await?;
    sink.flush().await?;
    Ok(())
}

/// 本地文件头：30 字节固定 + 文件名。
fn local_header(
    name: &[u8],
    flags: u16,
    crc: u32,
    size: u64,
    dos_time: u16,
    dos_date: u16,
) -> Vec<u8> {
    let mut header = Vec::with_capacity(30 + name.len());
    header.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    header.extend_from_slice(&20u16.to_le_bytes()); // version needed
    header.extend_from_slice(&flags.to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes()); // method: store
    header.extend_from_slice(&dos_time.to_le_bytes());
    header.extend_from_slice(&dos_date.to_le_bytes());
    header.extend_from_slice(&crc.to_le_bytes());
    // 带数据描述符时这两个字段必须是 0，真实值在数据后面
    let declared = if flags & 0x0008 != 0 { 0 } else { size };
    header.extend_from_slice(&(declared as u32).to_le_bytes());
    header.extend_from_slice(&(declared as u32).to_le_bytes());
    header.extend_from_slice(&(name.len() as u16).to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes()); // extra
    header.extend_from_slice(name);
    header
}

/// SystemTime → DOS 时间/日期（zip 用 1980 起的 16 位字段，秒只有 2 秒精度）。
///
/// 统一按 UTC 写：DOS 时间没有时区概念，按本地时间写会在跨时区解压时错位。
fn dos_stamp(when: Option<SystemTime>) -> (u16, u16) {
    let secs = when
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);
    let date =
        (((year as u16).saturating_sub(1980).min(127)) << 9) | ((month as u16) << 5) | day as u16;
    let time = ((hour as u16) << 11) | ((minute as u16) << 5) | ((second / 2) as u16);
    (time, date)
}

/// 1970 起的秒数 → (年, 月, 日, 时, 分, 秒)，Howard Hinnant 的 civil_from_days。
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (
        year,
        month,
        day,
        (rest / 3600) as u32,
        ((rest % 3600) / 60) as u32,
        (rest % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asmoday-zip-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn civil_time_round_trips_known_instants() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2024-02-29T12:34:56Z（闰日）
        assert_eq!(civil_from_unix(1_709_210_096), (2024, 2, 29, 12, 34, 56));
    }

    #[tokio::test]
    async fn zip_contains_every_file_with_central_directory() {
        let root = scratch("tree");
        let folder = root.join("Album");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("a.txt"), b"hello").unwrap();
        std::fs::write(folder.join("sub/b.bin"), vec![7u8; 1024]).unwrap();

        let entries = plan(&folder).unwrap();
        assert_eq!(entries.len(), 4, "Album + a.txt + sub + sub/b.bin");
        assert_eq!(entries[0].name, "Album/", "目录条目必须以斜杠结尾");

        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let writer_task = tokio::spawn(async move {
            stream_zip(entries, &mut writer).await.unwrap();
        });
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(&bytes[..4], &[0x50, 0x4b, 0x03, 0x04], "必须是本地文件头");
        // EOCD 一定在最后 22 字节（无注释时）
        let eocd = &bytes[bytes.len() - 22..];
        assert_eq!(&eocd[..4], &[0x50, 0x4b, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([eocd[10], eocd[11]]), 4, "条目数");
        let needle = b"Album/a.txt";
        assert!(
            bytes.windows(needle.len()).any(|window| window == needle),
            "文件名必须出现在包里"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_skips_symlinks_and_rejects_oversized_files() {
        let root = scratch("plan");
        let folder = root.join("Dir");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("ok.txt"), b"x").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink("/", folder.join("escape")).unwrap();

        let entries = plan(&folder).unwrap();
        assert!(
            entries.iter().all(|entry| !entry.name.ends_with("escape")),
            "软链接不能进 zip"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
