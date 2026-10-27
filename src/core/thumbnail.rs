//! 缩略图生成（设计文档 3.4）。
//!
//! - 并发上限 2（信号量），拿不到就回占位图，绝不让数千张图一起解码把 CPU 打满；
//! - 结果落磁盘缓存，键 = 路径 + 修改时间 + 大小 + 宽度（源文件 mtime 变了自动失效）；
//! - 容量上限与过期天数来自 `config.toml` 的 `[media]`，按 LRU + 过期天数淘汰；
//! - 生成时顺带算一枚 ThumbHash（约 25 字节），供前端做「主色调模糊占位」防抖动。
//!
//! 红线：图片走内置解码器；视频只允许经**外部可选** FFmpeg 截取首帧当封面，
//! 自身绝不链接 libav*，也绝不参与任何转码（设计文档 3.4 / 3.5）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use data_encoding::BASE64;
use tokio::sync::Semaphore;

use crate::core::ffmpeg::{self, Capabilities, PosterFormat};

/// 同时生成的缩略图数量上限
pub const MAX_CONCURRENT: usize = 2;
/// 等待信号量的最长时间，超时回占位图
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1500);
/// 每隔多少次生成跑一次淘汰
const CLEANUP_EVERY: usize = 25;

/// 占位图（内联 SVG，1 个请求 0 依赖）
pub const PLACEHOLDER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="320" height="240" viewBox="0 0 320 240">
<rect width="320" height="240" fill="#1e2128"/>
<g fill="none" stroke="#3f3f52" stroke-width="3" stroke-linecap="round" stroke-linejoin="round">
<rect x="104" y="82" width="112" height="84" rx="8"/>
<path d="M104 142l28-28 22 22 18-18 32 32"/>
<circle cx="136" cy="108" r="7" fill="#3f3f52" stroke="none"/>
</g>
</svg>"##;

/// 生成好的图片字节 + 正确的 Content-Type
/// （视频封面可能是 JPEG 也可能是 PNG，取决于外部 FFmpeg 的构建）
pub struct Rendered {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

pub enum ThumbOutcome {
    /// 生成/命中缓存成功
    Ready(Rendered),
    /// 队列拥塞或解码失败 → 占位图
    Placeholder,
}

pub struct Thumbnailer {
    semaphore: Arc<Semaphore>,
    cache_dir: PathBuf,
    generated: AtomicUsize,
    /// 缓存总容量上限（字节），由 `[media].thumb_cache_max_size_mb` 决定
    limit_bytes: AtomicU64,
    /// 缩略图最长保留秒数，由 `[media].thumb_cache_max_age_days` 决定
    max_age_secs: AtomicU64,
    /// FFmpeg 编码能力缓存（键 = 可执行文件路径），避免每张缩略图都跑一次 `-encoders`
    caps: Arc<Mutex<Option<(PathBuf, Capabilities)>>>,
}

impl Thumbnailer {
    pub fn new(cache_dir: PathBuf) -> Self {
        let defaults = crate::core::config::MediaConfig::default();
        Self {
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            cache_dir,
            generated: AtomicUsize::new(0),
            limit_bytes: AtomicU64::new(defaults.limit_bytes()),
            max_age_secs: AtomicU64::new(defaults.max_age_secs()),
            caps: Arc::new(Mutex::new(None)),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 应用 `[media]` 配置（启动时与配置热重载后调用）。
    pub fn apply_limits(&self, limit_bytes: u64, max_age_secs: u64) {
        self.limit_bytes.store(limit_bytes.max(1), Ordering::Relaxed);
        self.max_age_secs.store(max_age_secs, Ordering::Relaxed);
    }

    /// 图片缩略图的缓存基名（不带扩展名：实际后缀由编码结果决定，WebP / JPEG 都要能命中）
    fn cache_base(path: &Path, width: u32) -> String {
        let (len, mtime) = Self::source_stamp(path);

        let mut hasher = DefaultHasher::new();
        path.to_string_lossy().hash(&mut hasher);
        len.hash(&mut hasher);
        mtime.hash(&mut hasher);
        width.hash(&mut hasher);
        format!("{:016x}-{width}", hasher.finish())
    }

    /// 视频首帧封面的缓存键（不带扩展名，实际扩展名由输出格式决定）
    fn poster_key(path: &Path, width: u32) -> String {
        let (len, mtime) = Self::source_stamp(path);

        let mut hasher = DefaultHasher::new();
        path.to_string_lossy().hash(&mut hasher);
        len.hash(&mut hasher);
        mtime.hash(&mut hasher);
        width.hash(&mut hasher);
        format!("{:016x}-{width}p", hasher.finish())
    }

    /// ThumbHash 旁路文件键：与宽度无关，任何尺寸生成一次即可复用
    fn hash_key(path: &Path) -> String {
        let (len, mtime) = Self::source_stamp(path);
        let mut hasher = DefaultHasher::new();
        path.to_string_lossy().hash(&mut hasher);
        len.hash(&mut hasher);
        mtime.hash(&mut hasher);
        format!("{:016x}.th", hasher.finish())
    }

    fn source_stamp(path: &Path) -> (u64, u64) {
        let meta = std::fs::metadata(path).ok();
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (len, mtime)
    }

    /// 读取已算好的 ThumbHash（base64 字符串）。没有就返回 None，由前端回退纯色。
    pub fn thumb_hash(&self, path: &Path) -> Option<String> {
        let file = self.cache_dir.join(Self::hash_key(path));
        let raw = std::fs::read_to_string(file).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// 取缩略图：缓存 → 限流生成 → 占位。
    ///
    /// `webp_bin` 为可用的 FFmpeg；给了且该构建支持 libwebp 时输出 WebP（设计文档 3.4），
    /// 否则退回内置 JPEG 编码器。缓存里同时兼容两种后缀，换构建也不会重复生成。
    pub async fn get(&self, path: &Path, width: u32, webp_bin: Option<&Path>) -> ThumbOutcome {
        let width = width.clamp(64, 1024);
        let base = self.cache_dir.join(Self::cache_base(path, width));

        for (ext, mime) in [("webp", "image/webp"), ("jpg", "image/jpeg")] {
            let file = base.with_extension(ext);
            if let Ok(bytes) = tokio::fs::read(&file).await {
                // 触碰 mtime，参与 LRU 排序
                let _ = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&file)
                    .await;
                return ThumbOutcome::Ready(Rendered { bytes, mime });
            }
        }

        let permit = match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.acquire()).await {
            Ok(Ok(permit)) => permit,
            // 队列被打满：立刻回占位图，由前端稍后重试
            _ => return ThumbOutcome::Placeholder,
        };

        let source = path.to_path_buf();
        let sidecar = self.cache_dir.join(Self::hash_key(path));
        let cache_dir = self.cache_dir.clone();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        let caps = Arc::clone(&self.caps);
        let webp_bin = webp_bin.map(Path::to_path_buf);

        let result = tokio::task::spawn_blocking(move || {
            let (thumb, hash) = render_thumbnail(&source, width)?;
            let (bytes, mime) = encode_thumbnail(&thumb, webp_bin.as_deref(), &caps, &base)?;
            let _ = std::fs::write(&sidecar, BASE64.encode(&hash));
            if cache_dir.exists() {
                let _ = cleanup_cache(&cache_dir, limit, max_age);
            }
            Some((bytes, mime))
        })
        .await;

        drop(permit);

        match result {
            Ok(Some((bytes, mime))) => {
                self.count_generated();
                ThumbOutcome::Ready(Rendered { bytes, mime })
            }
            _ => ThumbOutcome::Placeholder,
        }
    }

    /// 生成计数 + 每 N 次跑一次后台淘汰（避免每次生成都全目录扫一遍）
    fn count_generated(&self) {
        let count = self.generated.fetch_add(1, Ordering::Relaxed) + 1;
        if count % CLEANUP_EVERY != 0 {
            return;
        }
        let dir = self.cache_dir.clone();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _ = cleanup_cache(&dir, limit, max_age);
        });
    }

    /// 视频首帧封面（设计文档 3.4：影视库不联网刮削，靠外部 FFmpeg 截首帧）。
    ///
    /// 与图片缩略图共用同一个缓存目录、信号量和 LRU 策略；没有 FFmpeg 时直接回占位图。
    /// 输出格式优先 WebP → JPEG → PNG，按该构建实际拥有的编码器逐级降级；
    /// 缓存查找三种后缀都试一遍，换过构建也能复用旧图。
    pub async fn video_poster(&self, path: &Path, bin: &Path, width: u32) -> ThumbOutcome {
        let width = width.clamp(64, 1024);
        let base = self.cache_dir.join(Self::poster_key(path, width));

        for format in ffmpeg::POSTER_FORMATS {
            let file = base.with_extension(format.extension());
            if let Ok(bytes) = tokio::fs::read(&file).await {
                let _ = tokio::fs::OpenOptions::new().write(true).open(&file).await;
                return ThumbOutcome::Ready(Rendered {
                    bytes,
                    mime: format.mime(),
                });
            }
        }

        // 共享同一个信号量：截帧比解 JPEG 更吃 CPU，绝不允许并发打满
        let permit = match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.acquire()).await {
            Ok(Ok(permit)) => permit,
            _ => return ThumbOutcome::Placeholder,
        };

        let source = path.to_path_buf();
        let stem = base.clone();
        let ffmpeg_bin = bin.to_path_buf();
        let cache_dir = self.cache_dir.clone();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        let caps = Arc::clone(&self.caps);

        let result = tokio::task::spawn_blocking(move || {
            let supports = capabilities(&caps, &ffmpeg_bin);
            for format in ffmpeg::POSTER_FORMATS {
                // 没有 libwebp 就别浪费一次进程启动
                if format == PosterFormat::Webp && !supports.webp {
                    continue;
                }
                let destination = stem.with_extension(format.extension());
                // 先试 1 秒（避开片头黑帧），极短视频截不到就退回第 0 帧
                let ok = ffmpeg::extract_poster(
                    &ffmpeg_bin,
                    &source,
                    &destination,
                    1,
                    width,
                    format,
                ) || ffmpeg::extract_poster(
                    &ffmpeg_bin,
                    &source,
                    &destination,
                    0,
                    width,
                    format,
                );
                if !ok {
                    continue;
                }
                if cache_dir.exists() {
                    let _ = cleanup_cache(&cache_dir, limit, max_age);
                }
                if let Ok(bytes) = std::fs::read(&destination) {
                    return Some((bytes, format.mime()));
                }
            }
            None
        })
        .await;

        drop(permit);

        match result {
            Ok(Some((bytes, mime))) => {
                self.count_generated();
                ThumbOutcome::Ready(Rendered { bytes, mime })
            }
            _ => ThumbOutcome::Placeholder,
        }
    }

    /// 后台/手动清理：按「过期天数 + LRU 容量」淘汰，返回释放的字节数。
    pub fn run_cleanup(&self) -> std::io::Result<u64> {
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        cleanup_cache(&self.cache_dir, limit, max_age)
    }

    /// 清空整个缓存目录（设置页「清理媒体缓存」按钮）。
    pub fn clear(&self) -> std::io::Result<(usize, u64)> {
        clear_cache(&self.cache_dir)
    }
}

/// 解码 → 等比缩放 → RGBA 位图，同时产出 32×32 的 ThumbHash。
///
/// 统一转 RGBA：WebP 保留透明通道，JPEG 分支再降到 RGB（JPEG 不支持 alpha）。
fn render_thumbnail(path: &Path, width: u32) -> Option<(image::RgbaImage, Vec<u8>)> {
    let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let image = reader.decode().ok()?;

    let small = image.thumbnail(32, 32).to_rgba8();
    let hash = thumbhash::rgba_to_thumb_hash(
        small.width() as usize,
        small.height() as usize,
        small.as_raw(),
    );

    Some((image.thumbnail(width, width).to_rgba8(), hash))
}

/// 编码缩略图：WebP 优先（外部 FFmpeg 的 libwebp），不可用则内置 JPEG。
///
/// 成功时字节已落盘（`<base>.webp` 或 `<base>.jpg`），返回 (字节, MIME)；
/// 全部失败返回 None，由调用方回占位图。
fn encode_thumbnail(
    thumb: &image::RgbaImage,
    webp_bin: Option<&Path>,
    caps: &Arc<Mutex<Option<(PathBuf, Capabilities)>>>,
    base: &Path,
) -> Option<(Vec<u8>, &'static str)> {
    if let Some(bin) = webp_bin {
        if capabilities(caps, bin).webp {
            let target = base.with_extension("webp");
            if ffmpeg::encode_webp_rgba(
                bin,
                thumb.as_raw(),
                thumb.width(),
                thumb.height(),
                82,
                &target,
            ) {
                match std::fs::read(&target) {
                    Ok(bytes) => return Some((bytes, "image/webp")),
                    Err(_) => {
                        let _ = std::fs::remove_file(&target);
                    }
                }
            }
        }
    }

    let rgb = image::DynamicImage::ImageRgba8(thumb.clone()).to_rgb8();
    let mut buffer = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, 82);
    encoder.encode_image(&rgb).ok()?;
    if let Some(parent) = base.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(base.with_extension("jpg"), &buffer);
    Some((buffer, "image/jpeg"))
}

/// FFmpeg 能力探测结果缓存：同一路径只探测一次，换路径（改配置）才重探。
fn capabilities(cache: &Arc<Mutex<Option<(PathBuf, Capabilities)>>>, bin: &Path) -> Capabilities {
    let mut guard = cache.lock().unwrap_or_else(|err| err.into_inner());
    if let Some((cached, caps)) = guard.as_ref() {
        if cached == bin {
            return *caps;
        }
    }
    let caps = ffmpeg::probe_capabilities(bin);
    *guard = Some((bin.to_path_buf(), caps));
    caps
}

/// 先按过期天数删，再按最久未访问淘汰，直到低于容量上限。
///
/// `max_age_secs == 0` 表示不做过期淘汰（仅测试用）。
pub fn cleanup_cache(dir: &Path, limit: u64, max_age_secs: u64) -> std::io::Result<u64> {
    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    let mut removed: u64 = 0;
    let now = SystemTime::now();

    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);

        // 过期淘汰：长期没人看过的缩略图直接删（删失败则留待 LRU 再试一次）
        if max_age_secs > 0 {
            if let Ok(age) = now.duration_since(modified) {
                if age >= Duration::from_secs(max_age_secs) && std::fs::remove_file(&path).is_ok() {
                    removed += meta.len();
                    continue;
                }
            }
        }

        total += meta.len();
        let accessed = meta.accessed().unwrap_or(modified);
        files.push((path, meta.len(), accessed));
    }

    if total <= limit {
        return Ok(removed);
    }

    // 最久未访问的排前面
    files.sort_by_key(|(_, _, accessed)| *accessed);

    for (path, size, _) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            removed += size;
        }
    }

    Ok(removed)
}

/// 清空缓存目录，返回 (删除文件数, 释放字节数)。
pub fn clear_cache(dir: &Path) -> std::io::Result<(usize, u64)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut freed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if std::fs::remove_file(&path).is_ok() {
                count += 1;
                freed += meta.len();
            }
        }
    }
    Ok((count, freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 测试用临时目录：放在 target/ 下，避免系统临时目录的权限差异
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_base_changes_with_width_and_path() {
        let dir = scratch_dir("thumb-key");
        let file = dir.join("a.jpg");
        fs::write(&file, b"x").unwrap();

        let a = Thumbnailer::cache_base(&file, 320);
        let b = Thumbnailer::cache_base(&file, 640);
        assert_ne!(a, b);
        assert_eq!(a, Thumbnailer::cache_base(&file, 320));
        // 基名不带扩展名：WebP / JPEG 两种缓存靠 with_extension 派生
        assert!(!a.contains('.'), "基名不应含扩展名：{a}");
        assert!(Path::new(&a).with_extension("webp").ends_with(format!("{a}.webp")));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 带透明通道的 PNG 也要能落到 JPEG（JPEG 无 alpha，必须先降为 RGB）
    #[test]
    fn renders_transparent_png_to_jpeg() {
        let dir = scratch_dir("thumb-alpha");
        let file = dir.join("a.png");
        let mut source = image::RgbaImage::new(64, 32);
        for pixel in source.pixels_mut() {
            *pixel = image::Rgba([200, 40, 40, 128]);
        }
        source.save(&file).unwrap();

        let (thumb, hash) = render_thumbnail(&file, 32).unwrap();
        assert!(thumb.width() <= 32 && thumb.height() <= 32);
        assert!(!hash.is_empty(), "ThumbHash 不能为空");

        let caps = Arc::new(Mutex::new(None));
        let base = dir.join("out");
        let (bytes, mime) = encode_thumbnail(&thumb, None, &caps, &base).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "应输出 JPEG 魔数");
        assert!(base.with_extension("jpg").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_drops_oldest_until_under_limit() {        let dir = scratch_dir("thumb-cleanup");
        for i in 0..10 {
            fs::write(dir.join(format!("f{i}.jpg")), vec![0u8; 1024]).unwrap();
        }
        // 10 KiB 总量，限制到 5 KiB → 至少删一半（不启用过期淘汰）
        let removed = cleanup_cache(&dir, 5 * 1024, 0).unwrap();
        assert!(removed >= 4 * 1024, "应淘汰足够文件，实际 {removed}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_drops_expired_entries() {
        let dir = scratch_dir("thumb-expire");
        fs::write(dir.join("old.jpg"), vec![0u8; 2048]).unwrap();
        // 过期阈值 1 秒 + 等 1.1 秒，把刚写的文件判为过期
        std::thread::sleep(Duration::from_millis(1100));
        let removed = cleanup_cache(&dir, 10 * 1024 * 1024, 1).unwrap();
        assert_eq!(removed, 2048);
        assert!(!dir.join("old.jpg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_cache_removes_everything() {
        let dir = scratch_dir("thumb-clear");
        for i in 0..3 {
            fs::write(dir.join(format!("c{i}.jpg")), vec![0u8; 100]).unwrap();
        }
        let (count, freed) = clear_cache(&dir).unwrap();
        assert_eq!(count, 3);
        assert_eq!(freed, 300);
        let _ = fs::remove_dir_all(&dir);
    }
}
