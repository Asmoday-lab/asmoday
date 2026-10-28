//! 外部 FFmpeg 探测（设计文档 3.5 / 6.2 / 3.4）。
//!
//! 优先级固定为三层，与设计文档一致：
//! 1. 设置页 UI 手动指定的绝对路径（`[media].ffmpeg_path`）
//! 2. 环境变量 `ASMODAY_FFMPEG_PATH`
//! 3. 系统 `PATH` 自动探测
//!
//! 红线：本模块**只**用于给影视库截取首帧封面（`-frames:v 1`），
//! 绝不参与任何视频转码；Asmoday 自身不链接 libav*。

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// 环境变量名（优先级 2）
pub const ENV_FFMPEG_PATH: &str = "ASMODAY_FFMPEG_PATH";

/// 可执行文件名：Windows 需要补 `.exe`，其余平台同名
pub fn binary_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

/// 路径来源，用于设置页「来源：…」文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegSource {
    /// 设置页指定
    Config,
    /// 环境变量
    Env,
    /// 系统 PATH
    Path,
    /// 没找到
    None,
}

impl FfmpegSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FfmpegSource::Config => "config",
            FfmpegSource::Env => "env",
            FfmpegSource::Path => "path",
            FfmpegSource::None => "none",
        }
    }
}

/// 探测结果。
#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: Option<PathBuf>,
    pub source: FfmpegSource,
}

/// 把用户输入规整成可执行文件路径。
///
/// 设置页里手填的常常是**安装目录**（如 `C:\ffmpeg\bin`、`/usr/local/bin`），
/// 直接当文件用会一律判「不可用」。目录就自动补上平台可执行文件名，
/// 这样「粘贴 bin 目录」和「粘贴 exe 全路径」两种习惯都能用。
pub fn executable(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw.trim());
    if path.is_dir() {
        path.join(binary_name())
    } else {
        path
    }
}

/// 按三层优先级解析 FFmpeg 位置。**不做**存在性校验之外的任何执行。
pub fn resolve(configured: &str) -> Resolved {
    let configured = configured.trim();
    if !configured.is_empty() {
        // 用户填了但文件不存在时也要如实报告来源，前端才能提示「该路径不可用」
        return Resolved {
            path: Some(executable(configured)),
            source: FfmpegSource::Config,
        };
    }

    if let Ok(raw) = std::env::var(ENV_FFMPEG_PATH) {
        let raw = raw.trim();
        if !raw.is_empty() {
            return Resolved {
                path: Some(executable(raw)),
                source: FfmpegSource::Env,
            };
        }
    }

    match search_path() {
        Some(path) => Resolved {
            path: Some(path),
            source: FfmpegSource::Path,
        },
        None => Resolved {
            path: None,
            source: FfmpegSource::None,
        },
    }
}

/// 遍历 `PATH` 找可执行文件（不引入 `which` 依赖）。
fn search_path() -> Option<PathBuf> {
    let raw = std::env::var_os("PATH")?;
    let name = binary_name();
    for dir in std::env::split_paths(&raw) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 组装子进程：Windows 下必须隐藏控制台窗口，否则每次截封面都会闪黑框。
pub fn command(bin: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// CREATE_NO_WINDOW
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 跑 `ffmpeg -version` 取版本号（如 `6.1.1`）。失败返回 `None`。
///
/// 同步阻塞（最多几百毫秒），调用方负责放到 `spawn_blocking` 里。
pub fn probe_version(bin: &Path) -> Option<String> {
    if !bin.is_file() {
        return None;
    }
    let output = command(bin).arg("-version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_version(&text)
}

/// 从 `ffmpeg version 6.1.1-full_build ...` 提取版本号。
fn parse_version(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let mut parts = first.split_whitespace();
    while let Some(token) = parts.next() {
        if token.eq_ignore_ascii_case("version") {
            let raw = parts.next()?;
            // git 构建会写成 `n7.0-...`：先跳过前缀字母，再取数字与点
            let cleaned: String = raw
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let cleaned = cleaned.trim_end_matches('.').to_string();
            return (!cleaned.is_empty()).then_some(cleaned);
        }
    }
    None
}

/// 从 `ffmpeg -i` 的输出里抠出视频分辨率 `(宽, 高)`（属性页用，v2.9）。
///
/// `ffmpeg -i` 不带输出文件必然以错误码退出，但元数据已经打到了 stderr，
/// 所以这里按「退出码不看、只认 stderr」来解析。
///
/// 同步阻塞（文件大也只是读头部元数据，通常 < 1s），调用方负责放到 `spawn_blocking`。
pub fn probe_video_size(bin: &Path, input: &Path) -> Option<(u32, u32)> {
    if !bin.is_file() {
        return None;
    }
    let output = command(bin)
        .arg("-hide_banner")
        .arg("-i")
        .arg(input)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stderr);
    parse_video_size(&text)
}

/// 从 ffmpeg 的流信息文本里解析 `宽x高`。
///
/// 只认 Video 流所在行的第一个合法尺寸；`0x31637661` 这类十六进制副作用
/// 靠「两边都必须是纯数字 + 合理上下限」排除。
fn parse_video_size(text: &str) -> Option<(u32, u32)> {
    for line in text.lines() {
        if !line.contains("Video:") {
            continue;
        }
        for token in line.split([',', ' ', '[', ']', '(', ')']) {
            let token = token.trim();
            let Some((w, h)) = token.split_once('x') else {
                continue;
            };
            if w.is_empty() || h.is_empty() {
                continue;
            }
            if !w.bytes().all(|c| c.is_ascii_digit()) || !h.bytes().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) else {
                continue;
            };
            // 正经视频分辨率下限 16x16，上限挡住 `0x31637661` 这类十六进制
            if (16..=100_000).contains(&w) && (16..=100_000).contains(&h) {
                return Some((w, h));
            }
        }
    }
    None
}

/// 封面输出格式。
///
/// 优先 WebP（设计文档 3.4：体积比 JPEG 小 30% 左右），该 FFmpeg 构建没有
/// libwebp 编码器时降级 JPEG；连 mjpeg 都没有的精简构建再退 PNG，保证功能不因发行版差异整体失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosterFormat {
    Webp,
    Jpeg,
    Png,
}

impl PosterFormat {
    pub fn extension(self) -> &'static str {
        match self {
            PosterFormat::Webp => "webp",
            PosterFormat::Jpeg => "jpg",
            PosterFormat::Png => "png",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            PosterFormat::Webp => "image/webp",
            PosterFormat::Jpeg => "image/jpeg",
            PosterFormat::Png => "image/png",
        }
    }
}

/// 缓存查找顺序：WebP 优先，其次 JPEG，最后 PNG（历史缓存 / 换过构建的情况都兼容）
pub const POSTER_FORMATS: [PosterFormat; 3] =
    [PosterFormat::Webp, PosterFormat::Jpeg, PosterFormat::Png];

/// 探测结果里与功能相关的编码能力。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// 是否支持 `libwebp` 编码（缩略图与首帧能否输出 WebP）
    pub webp: bool,
}

/// 跑 `ffmpeg -encoders` 看有没有 libwebp（设置页「libwebp 支持探测」，设计文档 3.5）。
///
/// 同步阻塞（几十毫秒），调用方负责放到 `spawn_blocking` 里；失败一律当不支持。
pub fn probe_capabilities(bin: &Path) -> Capabilities {
    if !bin.is_file() {
        return Capabilities::default();
    }
    let output = match command(bin).arg("-hide_banner").arg("-encoders").output() {
        Ok(output) => output,
        Err(_) => return Capabilities::default(),
    };
    if !output.status.success() {
        return Capabilities::default();
    }
    // 部分构建把编码器表打到 stderr，两边都读一遍最稳
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));

    let webp = text.lines().any(|line| {
        line.split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("libwebp"))
    });
    Capabilities { webp }
}

/// 把内存里的 RGBA 位图编码为 WebP（图片缩略图用，避免再落一份临时 PNG）。
///
/// 走 stdin 的 rawvideo 管道：宽高都很小（≤1024），拷一次内存远比写临时文件便宜。
pub fn encode_webp_rgba(
    bin: &Path,
    rgba: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    output: &Path,
) -> bool {
    use std::io::Write;

    if let Some(parent) = output.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }

    let child = command(bin)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("-i")
        .arg("pipe:0")
        .arg("-frames:v")
        .arg("1")
        .arg("-c:v")
        .arg("libwebp")
        .arg("-quality")
        .arg(quality.to_string())
        .arg("-f")
        .arg("webp")
        .arg(output)
        .stdin(Stdio::piped())
        .spawn();

    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(rgba).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(output);
            return false;
        }
    }
    let ok = matches!(child.wait(), Ok(status) if status.success());
    let written = ok && std::fs::metadata(output).map(|m| m.len() > 0).unwrap_or(false);
    if !written {
        let _ = std::fs::remove_file(output);
    }
    written
}

/// 截取视频首帧（设计文档 3.4：影视库封面）。
///
/// `at_secs` 建议 1 秒，避开部分片子开头的纯黑帧；极短视频会失败，调用方应退回 0 秒重试。
pub fn extract_poster(
    bin: &Path,
    input: &Path,
    output: &Path,
    at_secs: u32,
    width: u32,
    format: PosterFormat,
) -> bool {
    if let Some(parent) = output.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }

    let scale = format!("scale={width}:-2");
    let mut cmd = command(bin);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        // -ss 放在 -i 之前是关键帧快速定位，比解码到指定时间快一个数量级
        .arg("-ss")
        .arg(at_secs.to_string())
        .arg("-i")
        .arg(input)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(&scale);

    match format {
        PosterFormat::Webp => {
            cmd.arg("-c:v").arg("libwebp").arg("-quality").arg("80");
            cmd.arg("-f").arg("webp");
        }
        PosterFormat::Jpeg => {
            cmd.arg("-q:v").arg("5");
            cmd.arg("-f").arg("image2");
        }
        PosterFormat::Png => {
            cmd.arg("-c:v").arg("png");
            cmd.arg("-f").arg("image2");
        }
    }

    let status = cmd.arg(output).status().ok();

    match status {
        Some(status) if status.success() => {
            std::fs::metadata(output).map(|m| m.len() > 0).unwrap_or(false)
        }
        _ => {
            let _ = std::fs::remove_file(output);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_line() {
        let text = "ffmpeg version 6.1.1-full_build-www.gyan.dev Copyright (c) 2000-2023";
        assert_eq!(parse_version(text).as_deref(), Some("6.1.1"));
    }

    #[test]
    fn parses_plain_version() {
        assert_eq!(
            parse_version("ffmpeg version n7.0 Copyright").as_deref(),
            Some("7.0")
        );
    }

    #[test]
    fn missing_version_returns_none() {
        assert_eq!(parse_version("totally unrelated output"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn configured_path_wins_over_env() {
        // 只验证优先级与来源标记，不依赖机器上是否真的装了 ffmpeg
        let resolved = resolve("D:/tools/ffmpeg.exe");
        assert_eq!(resolved.source, FfmpegSource::Config);
        assert_eq!(resolved.path.as_deref(), Some(Path::new("D:/tools/ffmpeg.exe")));
    }

    #[test]
    fn directory_input_gains_binary_name() {
        let dir = std::env::temp_dir().join("asmoday-ffmpeg-dir-test");
        let _ = std::fs::create_dir_all(&dir);
        let resolved = executable(dir.to_string_lossy().as_ref());
        assert!(resolved.ends_with(binary_name()), "got {}", resolved.display());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn file_input_is_kept_as_is() {
        let resolved = executable("  D:/tools/ffmpeg.exe  ");
        assert_eq!(resolved, PathBuf::from("D:/tools/ffmpeg.exe"));
    }

    #[test]
    fn blank_config_falls_through() {
        let resolved = resolve("   ");
        // 环境变量在测试进程里可能被设置，两种结果都合法
        assert!(matches!(
            resolved.source,
            FfmpegSource::Env | FfmpegSource::Path | FfmpegSource::None
        ));
    }

    #[test]
    fn parses_video_size_from_stream_line() {
        let text = "Input #0, mov,mp4 from 'x.mp4':\n  Duration: 00:01:00\n  Stream #0:0: Video: h264 (High) (AVC1 / 0x31637661), yuv420p, 1920x1080 [SAR 1:1 DAR 16:9]\n";
        assert_eq!(parse_video_size(text), Some((1920, 1080)));
    }

    #[test]
    fn video_size_rejects_hex_and_garbage() {
        // 十六进制副作用 0x31637661 不能被当成 0x31637661 的尺寸
        let text = "Stream #0:0: Video: h264 (AVC1 / 0x31637661), none, 1k";
        assert_eq!(parse_video_size(text), None);
        assert_eq!(parse_video_size("no video stream here"), None);
        assert_eq!(parse_video_size(""), None);
    }

    #[test]
    fn parses_vertical_video_size() {
        let text = "Stream #0:0[0x1](und): Video: hevc (Main), yuv420p(tv), 1080x1920, 30 fps";
        assert_eq!(parse_video_size(text), Some((1080, 1920)));
    }

    #[test]
    fn poster_formats_carry_matching_extension_and_mime() {
        assert_eq!(PosterFormat::Webp.extension(), "webp");
        assert_eq!(PosterFormat::Webp.mime(), "image/webp");
        assert_eq!(PosterFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(PosterFormat::Png.mime(), "image/png");
        // 缓存查找顺序固定为 WebP → JPEG → PNG（设计文档 3.4 的降级链）
        assert_eq!(POSTER_FORMATS[0], PosterFormat::Webp);
        assert_eq!(POSTER_FORMATS[2], PosterFormat::Png);
    }

    #[test]
    fn missing_binary_has_no_capabilities() {
        let caps = probe_capabilities(Path::new("D:/definitely/not/ffmpeg.exe"));
        assert_eq!(caps, Capabilities::default());
        assert!(!caps.webp);
    }

    #[test]
    fn webp_encode_rejects_missing_binary() {
        let dir = std::env::temp_dir().join("asmoday-webp-encode-test");
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("x.webp");
        let ok = encode_webp_rgba(
            Path::new("D:/definitely/not/ffmpeg.exe"),
            &[0, 0, 0, 255],
            1,
            1,
            80,
            &out,
        );
        assert!(!ok);
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
