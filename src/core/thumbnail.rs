//! 缩略图生成（设计文档 3.4）。
//!
//! - 并发上限 2（信号量），拿不到就回占位图，绝不让数千张图一起解码把 CPU 打满；
//! - 结果落磁盘缓存，键 = 路径 + 修改时间 + 大小 + 宽度（源文件 mtime 变了自动失效）；
//! - 容量上限与过期天数来自 `config.toml` 的 `[media]`，按 LRU + 过期天数淘汰；
//! - 生成时顺带算一枚 ThumbHash（约 25 字节），供前端做「主色调模糊占位」防抖动；
//!   **v2.8.2 起存 SQLite `thumb_hashes` 表**，不再落盘旁路文件 —— 几十万张图的
//!   场景下碎文件会撑爆低功耗设备的 inode，Base64 文本进库零 inode 成本。
//!
//! 红线：图片走内置解码器；视频只允许经**外部可选** FFmpeg 截取首帧当封面，
//! 自身绝不链接 libav*，也绝不参与任何转码（设计文档 3.4 / 3.5）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use data_encoding::BASE64;
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;

use crate::core::ffmpeg::{self, Capabilities, PosterFormat};

/// 缩略图并发生成的硬上限。
///
/// 解码 + 缩放是纯 CPU 密集、全程占满一个核，并发开太大只会互相抢核并把
/// 峰值内存（一张 4K 图解码后约 30MB）乘上去，所以在小机器上也不能压到 2 以下。
const MAX_CONCURRENT_CAP: usize = 8;
/// 探测不到 CPU 数时的兜底并发（与旧版固定值一致）
const MAX_CONCURRENT_FALLBACK: usize = 2;

/// 按机器能力决定同时生成的缩略图数量：留一个核给 HTTP / PTY，至少 2、最多 8。
///
/// 旧版固定 2 —— 在 16 核 NAS 上滚动图库要排队到回占位图，在 2 核小机器上又刚好。
/// 现在按 `available_parallelism` 自适应：4 核 → 3，8 核及以上 → 8。
pub fn max_concurrent() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MAX_CONCURRENT_FALLBACK + 1);
    cores
        .saturating_sub(1)
        .clamp(MAX_CONCURRENT_FALLBACK, MAX_CONCURRENT_CAP)
}

/// 等待信号量的最长时间，超时回占位图
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1500);
/// 每隔多少次生成跑一次淘汰
const CLEANUP_EVERY: usize = 25;
/// 「下载中」判定窗口（设计文档 3.4.1 的下载目录特调）
const FRESH_WINDOW: Duration = Duration::from_secs(60);
/// 只有体积够大的新鲜文件才算「正在下载」
///
/// 刚上传的照片写盘是毫秒级，如果按 mtime 一刀切，用户上传完会先看到一堆占位图，
/// 以为缩略图坏了。真正会反复重绘的是几百兆的影视文件，所以用体积当第二道判据。
const FRESH_SIZE_BYTES: u64 = 4 * 1024 * 1024;

/// 版本戳文件名：内容为 `长度:mtime`，源文件一变缓存即失效。
const STAMP_FILE: &str = "stamp";

/// 文件是否可能仍在写盘（mtime 极新且体积可观）。
fn is_fresh_write(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() < FRESH_SIZE_BYTES {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    // 时钟回拨（mtime 在未来）时不当作新鲜：宁可生成一次，也不要永远回占位图
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age < FRESH_WINDOW)
        .unwrap_or(false)
}

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

/// 生成成功时随 `get()` 带出的新鲜 ThumbHash：由调用方写进 SQLite
/// （`AuthStore::thumb_hash_set`），缓存命中时为 None（库里已有）。
#[derive(Debug, Clone)]
pub struct FreshHash {
    /// 库主键 = `hash_key`（路径 + mtime + 大小的摘要，与宽度无关）
    pub key: String,
    /// ThumbHash 的 Base64 文本
    pub value: String,
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
    /// 每个缓存单元一把异步锁：同一张图被 320 / 960 同时请求时只解码一次，
    /// 也避免两个线程同时写同一份 stamp（FileBrowser 的 scoped lock 同思路）。
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl Thumbnailer {
    pub fn new(cache_dir: PathBuf) -> Self {
        let defaults = crate::core::config::MediaConfig::default();
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent())),
            cache_dir,
            generated: AtomicUsize::new(0),
            limit_bytes: AtomicU64::new(defaults.limit_bytes()),
            max_age_secs: AtomicU64::new(defaults.max_age_secs()),
            caps: Arc::new(Mutex::new(None)),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 应用 `[media]` 配置（启动时与配置热重载后调用）。
    pub fn apply_limits(&self, limit_bytes: u64, max_age_secs: u64) {
        self.limit_bytes
            .store(limit_bytes.max(1), Ordering::Relaxed);
        self.max_age_secs.store(max_age_secs, Ordering::Relaxed);
    }

    /// 源路径的稳定摘要（sha1 十六进制 40 字符）。
    ///
    /// 缓存文件名一律只用它，绝不带源文件名或扩展名 —— FileBrowser 的
    /// diskcache 也是 `sha1(key)` 的无扩展名文件。这样即使缓存目录被误暴露成
    /// 普通目录，也不会被识别成图片、更不会「缓存给自己再缓存一遍」。
    ///
    /// 摘要用的是 `normalize_display` 之后的写法：同一个文件可能以
    /// `C:/x/y.jpg`（接口入参）或 `\\?\C://x//y.jpg`（canonicalize 结果）出现，
    /// 不归一化就会算出两个键、缓存和 ThumbHash 全都对不上。
    fn digest(path: &Path) -> String {
        let mut hasher = Sha1::new();
        hasher.update(crate::core::fs::normalize_display(path).as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// 单个源文件的缓存单元目录：`<cache>/<digest[..2]>/<digest>`。
    ///
    /// 一级两字符扇出（同 FileBrowser）：几十万张图也不会让单目录堆到上百万项；
    /// 删除一个源文件的缓存也只需要删掉这一个目录。
    fn entry_dir(&self, path: &Path) -> PathBuf {
        let digest = Self::digest(path);
        self.cache_dir.join(&digest[..2]).join(digest)
    }

    /// ThumbHash 的库键：**只跟路径有关，不含 mtime**。
    ///
    /// 旧实现把 mtime 混进键里，改一次图就多一行，库随编辑次数膨胀；
    /// 现在键稳定，值由每次成功生成覆盖（缩略图本身靠 `stamp` 判过期）。
    pub fn hash_key(path: &Path) -> String {
        Self::digest(path)
    }

    /// 源文件的版本戳：缓存格式版本 + 长度 + mtime 秒。变了就说明源文件被改过、
    /// 或生成规则换过，缓存作废。
    ///
    /// `v2` 是一次**格式升版**：v4.4.2 之前 `render_thumbnail` 会把小于请求宽度的
    /// 图放大（300×200 的照片被生成成 960 宽的缩略图），灯箱用 960 缩略图打底时
    /// 画面先按放大版铺开、原图一到又缩回去，看起来就是「打开预览先大后小」。
    /// 只改生成代码没用——旧文件 stamp 与源文件仍然吻合，永远命中旧图；加上版本
    /// 前缀后旧 stamp 对不上，会在同一个文件名上就地重生（不产生孤儿文件）。
    const CACHE_VERSION: &str = "v2";

    fn stamp_of(path: &Path) -> String {
        let (len, mtime) = Self::source_stamp(path);
        format!("{}:{len}:{mtime}", Self::CACHE_VERSION)
    }

    /// 视频首帧的戳：只有源文件的长度 + mtime，不含缓存格式版本（理由见 `video_poster`）。
    fn poster_stamp_of(path: &Path) -> String {
        let (len, mtime) = Self::source_stamp(path);
        format!("{len}:{mtime}")
    }

    fn read_stamp(dir: &Path) -> Option<String> {
        std::fs::read_to_string(dir.join(STAMP_FILE))
            .ok()
            .map(|text| text.trim().to_string())
    }

    /// 取某个缓存单元里已生成好的图（stamp 必须与当前源文件一致）。
    async fn cached(dir: &Path, stamp: &str, width: u32) -> Option<ThumbOutcome> {
        if Self::read_stamp(dir).as_deref() != Some(stamp) {
            return None;
        }
        for (codec, mime) in [("webp", "image/webp"), ("jpg", "image/jpeg")] {
            let file = dir.join(format!("{width}-{codec}"));
            if let Ok(bytes) = tokio::fs::read(&file).await {
                // 触碰 mtime，参与 LRU 排序
                let _ = tokio::fs::OpenOptions::new().write(true).open(&file).await;
                return Some(ThumbOutcome::Ready(Rendered { bytes, mime }));
            }
        }
        None
    }

    /// 取缓存单元里的视频首帧（同样先验 stamp）。
    async fn cached_poster(dir: &Path, stamp: &str, width: u32) -> Option<ThumbOutcome> {
        if Self::read_stamp(dir).as_deref() != Some(stamp) {
            return None;
        }
        for format in ffmpeg::POSTER_FORMATS {
            let file = dir.join(format!("{width}p-{}", format.extension()));
            if let Ok(bytes) = tokio::fs::read(&file).await {
                let _ = tokio::fs::OpenOptions::new().write(true).open(&file).await;
                return Some(ThumbOutcome::Ready(Rendered {
                    bytes,
                    mime: format.mime(),
                }));
            }
        }
        None
    }

    /// 取该缓存单元的异步锁（同一源文件串行生成）。
    fn key_lock(&self, key: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().unwrap_or_else(|err| err.into_inner());
        map.entry(key.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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

    /// 取缩略图：缓存 → 限流生成 → 占位。
    ///
    /// 返回 `(结果, 新鲜 ThumbHash)`：缓存命中时第二个值为 None；
    /// 生成成功时带上 hash 由调用方入库，本模块**不再**写任何 `.th` 文件。
    ///
    /// 缓存布局对齐 FileBrowser 的 diskcache：`<digest[..2]>/<digest>/<width>-<codec>`，
    /// 同目录 `stamp` 记源文件的 len+mtime。**同一路径永远落在同一个目录里**，
    /// 源文件改了就原地覆盖，不会因为 mtime 变化不断堆出新文件。
    pub async fn get(
        &self,
        path: &Path,
        width: u32,
        webp_bin: Option<&Path>,
    ) -> (ThumbOutcome, Option<FreshHash>) {
        let width = width.clamp(64, 1024);
        let dir = self.entry_dir(path);
        let stamp = Self::stamp_of(path);

        if let Some(outcome) = Self::cached(&dir, &stamp, width).await {
            return (outcome, None);
        }

        // 下载中的大文件先不生成（设计文档 3.4.1）：写盘还没结束，
        // 这一秒生成的图下一秒就因 mtime 变化失效，纯属白烧 CPU。
        if is_fresh_write(path) {
            return (ThumbOutcome::Placeholder, None);
        }

        // 同一源文件串行：320 / 960 同时请求只解码一次，也不会两份 stamp 互相踩
        let lock = self.key_lock(&dir);
        let _serial = lock.lock().await;
        if let Some(outcome) = Self::cached(&dir, &stamp, width).await {
            return (outcome, None);
        }

        let permit = match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.acquire()).await {
            Ok(Ok(permit)) => permit,
            // 队列被打满：立刻回占位图，由前端稍后重试
            _ => return (ThumbOutcome::Placeholder, None),
        };

        let source = path.to_path_buf();
        let hash_key = Self::hash_key(&source);
        let target_dir = dir.clone();
        let cache_dir = self.cache_dir.clone();
        let stamp_for_task = stamp.clone();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        let caps = Arc::clone(&self.caps);
        let webp_bin = webp_bin.map(Path::to_path_buf);

        let result = tokio::task::spawn_blocking(move || {
            let (thumb, hash) = render_thumbnail(&source, width)?;
            let (bytes, mime, codec) =
                encode_thumbnail(&thumb, webp_bin.as_deref(), &caps, &target_dir)?;
            if write_atomic(&target_dir.join(format!("{width}-{codec}")), &bytes).is_err() {
                return None;
            }
            // stamp 最后写：中途失败最多下次重做，不会留下「有戳没图」的死缓存
            let _ = write_atomic(&target_dir.join(STAMP_FILE), stamp_for_task.as_bytes());
            let fresh = FreshHash {
                key: hash_key,
                value: BASE64.encode(&hash),
            };
            let _ = cleanup_cache(&cache_dir, limit, max_age);
            Some((bytes, mime, fresh))
        })
        .await;

        drop(permit);

        match result {
            Ok(Some((bytes, mime, fresh))) => {
                self.count_generated();
                (ThumbOutcome::Ready(Rendered { bytes, mime }), Some(fresh))
            }
            _ => (ThumbOutcome::Placeholder, None),
        }
    }

    /// 生成计数 + 每 N 次跑一次后台淘汰（避免每次生成都全目录扫一遍）
    fn count_generated(&self) {
        let count = self.generated.fetch_add(1, Ordering::Relaxed) + 1;
        if !count.is_multiple_of(CLEANUP_EVERY) {
            return;
        }
        self.prune_locks();
        let dir = self.cache_dir.clone();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _ = cleanup_cache(&dir, limit, max_age);
        });
    }

    /// 丢掉没有请求在用的 per-key 锁，避免长期运行后 map 无限长。
    fn prune_locks(&self) {
        if let Ok(mut map) = self.locks.lock()
            && map.len() > 256
        {
            map.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
    }

    /// 删除某个源文件的全部缓存（缩略图 + 版本戳）。
    ///
    /// 删除 / 改名 / 移动后调用：FileBrowser 在 `delThumbs` 里做同样的事，
    /// 否则被删掉的文件会永远留一份缓存，只能等 LRU 慢慢淘汰。
    pub fn purge(&self, path: &Path) -> bool {
        std::fs::remove_dir_all(self.entry_dir(path)).is_ok()
    }

    /// 视频首帧封面（设计文档 3.4：影视库不联网刮削，靠外部 FFmpeg 截首帧）。
    ///
    /// 与图片缩略图共用缓存目录、`stamp` 判据、per-key 锁和信号量；
    /// 输出格式优先 WebP → JPEG → PNG，按该构建实际拥有的编码器逐级降级。
    pub async fn video_poster(&self, path: &Path, bin: &Path, width: u32) -> ThumbOutcome {
        let width = width.clamp(64, 1024);
        let dir = self.entry_dir(path);
        // 海报用不带格式版本的戳：视频首帧本来就是「按容器尺寸截」，没有放大失真问题，
        // 跟着图片一起升版只会让几千个视频白白重跑一遍 FFmpeg
        let stamp = Self::poster_stamp_of(path);

        if let Some(outcome) = Self::cached_poster(&dir, &stamp, width).await {
            return outcome;
        }

        // 正在下载的影视文件：截帧要拉起 FFmpeg 进程，比解图更贵，
        // 更要等写盘结束（设计文档 3.4.1）。缓存命中已在上面返回。
        if is_fresh_write(path) {
            return ThumbOutcome::Placeholder;
        }

        let lock = self.key_lock(&dir);
        let _serial = lock.lock().await;
        if let Some(outcome) = Self::cached_poster(&dir, &stamp, width).await {
            return outcome;
        }

        // 共享同一个信号量：截帧比解 JPEG 更吃 CPU，绝不允许并发打满
        let permit = match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.acquire()).await {
            Ok(Ok(permit)) => permit,
            _ => return ThumbOutcome::Placeholder,
        };

        let source = path.to_path_buf();
        let target_dir = dir.clone();
        let cache_dir = self.cache_dir.clone();
        let stamp_for_task = stamp.clone();
        let ffmpeg_bin = bin.to_path_buf();
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        let max_age = self.max_age_secs.load(Ordering::Relaxed);
        let caps = Arc::clone(&self.caps);

        let result = tokio::task::spawn_blocking(move || {
            let supports = capabilities(&caps, &ffmpeg_bin);
            std::fs::create_dir_all(&target_dir).ok()?;
            for format in ffmpeg::POSTER_FORMATS {
                // 没有 libwebp 就别浪费一次进程启动
                if format == PosterFormat::Webp && !supports.webp {
                    continue;
                }
                // FFmpeg 靠扩展名判封装格式：只能先写带扩展名的临时文件，成功后再改名成
                // 无扩展名的缓存名（`960p-webp`），避免缓存文件被当成图片再次缩略。
                let tmp = temp_path(&target_dir, format.extension());
                // 先试 1 秒（避开片头黑帧），极短视频截不到就退回第 0 帧
                let ok = ffmpeg::extract_poster(&ffmpeg_bin, &source, &tmp, 1, width, format)
                    || ffmpeg::extract_poster(&ffmpeg_bin, &source, &tmp, 0, width, format);
                if !ok {
                    let _ = std::fs::remove_file(&tmp);
                    continue;
                }
                let final_path = target_dir.join(format!("{width}p-{}", format.extension()));
                if std::fs::rename(&tmp, &final_path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    continue;
                }
                let _ = write_atomic(&target_dir.join(STAMP_FILE), stamp_for_task.as_bytes());
                let _ = cleanup_cache(&cache_dir, limit, max_age);
                if let Ok(bytes) = std::fs::read(&final_path) {
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
    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let image = reader.decode().ok()?;

    let small = image.thumbnail(32, 32).to_rgba8();
    let hash = thumbhash::rgba_to_thumb_hash(
        small.width() as usize,
        small.height() as usize,
        small.as_raw(),
    );

    // 不放大：`thumbnail` 是等比缩放，请求尺寸比原图大时会真的把小图拉大
    // （340×475 的图请求 960 会得到 687×960）。灯箱用缩略图做加载占位，
    // 放大后的占位比原图大一圈，撤下时会露出边——按原图长边封顶即可。
    let bound = width.min(image.width().max(image.height()));
    Some((image.thumbnail(bound, bound).to_rgba8(), hash))
}

/// 编码缩略图：WebP 优先（外部 FFmpeg 的 libwebp），不可用则内置 JPEG。
///
/// 返回 `(字节, MIME, 缓存名后缀)`。**只编码、不写最终文件**：落盘由调用方
/// 原子写 `<width>-<codec>`，既不会有半张图被读到，缓存文件名也不带图片扩展名。
fn encode_thumbnail(
    thumb: &image::RgbaImage,
    webp_bin: Option<&Path>,
    caps: &Arc<Mutex<Option<(PathBuf, Capabilities)>>>,
    work_dir: &Path,
) -> Option<(Vec<u8>, &'static str, &'static str)> {
    if let Some(bin) = webp_bin
        && capabilities(caps, bin).webp
    {
        // FFmpeg 靠扩展名判输出格式，只能先落到同目录临时文件再读回来
        let _ = std::fs::create_dir_all(work_dir);
        let tmp = temp_path(work_dir, "webp");
        if ffmpeg::encode_webp_rgba(bin, thumb.as_raw(), thumb.width(), thumb.height(), 82, &tmp) {
            let bytes = std::fs::read(&tmp);
            let _ = std::fs::remove_file(&tmp);
            if let Ok(bytes) = bytes {
                return Some((bytes, "image/webp", "webp"));
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    let rgb = image::DynamicImage::ImageRgba8(thumb.clone()).to_rgb8();
    let mut buffer = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, 82);
    encoder.encode_image(&rgb).ok()?;
    Some((buffer, "image/jpeg", "jpg"))
}

/// 同目录唯一临时文件名。
///
/// `.` 开头 + `.tmp-` 前缀：列表默认隐藏，清理时也认得这个前缀，
/// 进程被强杀留下的残片会在下次清理时被收走。
fn temp_path(dir: &Path, ext: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".tmp-{}-{nanos}-{seq}.{ext}", std::process::id()))
}

/// 原子写：先写同目录临时文件，再 rename 覆盖目标。
///
/// 直接 `write` 目标文件的话，并发读可能读到写了一半的图（客户端拿到破图）；
/// rename 在同一文件系统上是原子的，读侧只会看到旧版或新版。
fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = temp_path(parent, "bin");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// FFmpeg 能力探测结果缓存：同一路径只探测一次，换路径（改配置）才重探。
fn capabilities(cache: &Arc<Mutex<Option<(PathBuf, Capabilities)>>>, bin: &Path) -> Capabilities {
    let mut guard = cache.lock().unwrap_or_else(|err| err.into_inner());
    if let Some((cached, caps)) = guard.as_ref()
        && cached == bin
    {
        return *caps;
    }
    let caps = ffmpeg::probe_capabilities(bin);
    *guard = Some((bin.to_path_buf(), caps));
    caps
}

/// 统计一个缓存单元目录里的文件数、总字节、最近一次写入时间。
fn unit_stats(dir: &Path) -> (u64, u64, SystemTime) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut newest = UNIX_EPOCH;
    let Ok(reader) = std::fs::read_dir(dir) else {
        return (0, 0, newest);
    };
    for entry in reader.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            let (sub_files, sub_bytes, sub_newest) = unit_stats(&entry.path());
            files += sub_files;
            bytes += sub_bytes;
            oldest(&mut newest, sub_newest);
            continue;
        }
        files += 1;
        bytes += meta.len();
        oldest(&mut newest, meta.modified().unwrap_or(UNIX_EPOCH));
    }
    (files, bytes, newest)
}

fn oldest(current: &mut SystemTime, candidate: SystemTime) {
    if candidate > *current {
        *current = candidate;
    }
}

/// 先按过期天数删，再按最久未访问淘汰，直到低于容量上限。
///
/// 淘汰单位是「一个源文件的缓存单元目录」（`<digest>`），不是单个文件 ——
/// 同一张图的多个宽度必须同生共死，否则会留下永远命不中的孤儿。
/// 旧版扁平布局（`<hash>-320.webp` / `<sha>.th`）一律直接删：新版不会在
/// 顶层放文件，留着只会占空间。
///
/// `max_age_secs == 0` 表示不做过期淘汰（仅测试用）。
pub fn cleanup_cache(dir: &Path, limit: u64, max_age_secs: u64) -> std::io::Result<u64> {
    // 缓存目录还没建（刚部署、还没生成过缩略图）时视为「已干净」，
    // 不能把 404 透成 500 —— 设置页的「立即清理」必须是幂等的。
    if !dir.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut units: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    let mut removed: u64 = 0;

    for bucket in std::fs::read_dir(dir)? {
        let Ok(bucket) = bucket else {
            continue;
        };
        let bucket_path = bucket.path();
        let Ok(meta) = bucket.metadata() else {
            continue;
        };
        if meta.is_file() {
            // 旧版扁平布局 / 残留临时文件：升级即清
            if std::fs::remove_file(&bucket_path).is_ok() {
                removed += meta.len();
            }
            continue;
        }
        let Ok(children) = std::fs::read_dir(&bucket_path) else {
            continue;
        };
        for child in children.flatten() {
            let unit_path = child.path();
            let Ok(unit_meta) = child.metadata() else {
                continue;
            };
            if unit_meta.is_file() {
                let _ = std::fs::remove_file(&unit_path);
                continue;
            }
            let (_, bytes, newest) = unit_stats(&unit_path);
            let expired = max_age_secs > 0
                && now
                    .duration_since(newest)
                    .map(|age| age >= Duration::from_secs(max_age_secs))
                    .unwrap_or(false);
            if expired && std::fs::remove_dir_all(&unit_path).is_ok() {
                removed += bytes;
                continue;
            }
            total += bytes;
            units.push((unit_path, bytes, newest));
        }
    }

    if total <= limit {
        return Ok(removed);
    }

    // 最久没被访问的排前面
    units.sort_by_key(|(_, _, newest)| *newest);
    for (path, bytes, _) in units {
        if total <= limit {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(bytes);
            removed += bytes;
        }
    }

    Ok(removed)
}

/// 缓存目录的实际占用（文件数, 字节）：设置页展示用，必须递归统计
/// （新版是两级扇出目录，只数顶层会永远显示 0）。
pub fn cache_usage(dir: &Path) -> (usize, u64) {
    let (files, bytes, _) = unit_stats(dir);
    (files as usize, bytes)
}

/// 清空缓存目录，返回 (删除文件数, 释放字节数)。
pub fn clear_cache(dir: &Path) -> std::io::Result<(usize, u64)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let (files, bytes, _) = unit_stats(dir);
    std::fs::remove_dir_all(dir)?;
    std::fs::create_dir_all(dir)?;
    Ok((files as usize, bytes))
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
    fn concurrency_scales_with_cores_within_bounds() {
        let n = max_concurrent();
        assert!(
            (MAX_CONCURRENT_FALLBACK..=MAX_CONCURRENT_CAP).contains(&n),
            "并发必须落在 [2, 8]，实际 {n}"
        );
    }

    /// 收集目录下的所有文件（递归）。
    fn collect_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(reader) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for entry in reader.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    /// 缓存键只由源路径决定：同一文件永远落在同一个单元目录里。
    /// 旧实现把 mtime 混进文件名，改一次图就多一个文件 —— 「越用越多」的根源。
    #[test]
    fn entry_dir_is_stable_per_path_and_fanned_out() {
        let dir = scratch_dir("thumb-entry");
        let file = dir.join("a.jpg");
        fs::write(&file, b"x").unwrap();

        let cache = scratch_dir("thumb-entry-cache");
        let thumbnails = Thumbnailer::new(cache.clone());
        let first = thumbnails.entry_dir(&file);
        // 改了内容（长度 / mtime 变化）之后仍是同一个目录：覆盖写，不新增
        fs::write(&file, b"xxxx").unwrap();
        assert_eq!(first, thumbnails.entry_dir(&file));

        let unit = first.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(unit.len(), 40, "sha1 十六进制应为 40 字符：{unit}");
        assert!(unit.chars().all(|ch| ch.is_ascii_hexdigit()));

        // 一级两字符扇出，避免单目录堆到上百万项
        let bucket = first.parent().unwrap();
        assert_eq!(bucket.parent().unwrap(), cache);
        assert_eq!(bucket.file_name().unwrap().to_string_lossy(), &unit[..2]);

        // 缓存名不带图片扩展名：媒体扫描 / 列表都不会把它当图片
        let name = first.join("320-webp");
        let text = name.to_string_lossy().to_string();
        assert_eq!(
            crate::core::fs::extension_of_name(&text),
            "",
            "缓存文件名不能带扩展名：{text}"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&cache);
    }

    /// 缓存戳带**格式版本**：v4.4.2 之前生成的「放大版」缩略图只靠 mtime 永远命中，
    /// 灯箱拿 960 缩略图打底就会「先大后小」。版本前缀是让老缓存就地重生的唯一开关，
    /// 删掉它这条回归会立刻变红。
    #[test]
    fn thumbnail_stamp_carries_format_version() {
        let dir = scratch_dir("thumb-stamp");
        let file = dir.join("a.png");
        image::RgbaImage::from_pixel(12, 9, image::Rgba([1, 2, 3, 255]))
            .save(&file)
            .unwrap();

        let stamp = Thumbnailer::stamp_of(&file);
        assert!(stamp.starts_with("v2:"), "缩略图戳必须带格式版本：{stamp}");
        // 海报（视频首帧）不跟版本：它没有放大失真问题，跟着升版只是白跑一遍 FFmpeg
        assert!(
            !Thumbnailer::poster_stamp_of(&file).starts_with("v2:"),
            "海报戳不该带缩略图格式版本"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 请求尺寸大于原图时不得放大：灯箱拿缩略图做加载占位，
    /// 放大版会比原图大一圈，原图淡入后撤下会露出边框。
    #[test]
    fn thumbnail_never_upscales() {
        let dir = scratch_dir("thumb-noup");
        let small = dir.join("small.png");
        image::RgbaImage::from_pixel(34, 47, image::Rgba([10, 20, 30, 255]))
            .save(&small)
            .unwrap();
        let (thumb, _) = render_thumbnail(&small, 960).unwrap();
        assert_eq!(
            (thumb.width(), thumb.height()),
            (34, 47),
            "比请求尺寸小的图必须原样返回"
        );

        // 大图照旧等比缩到请求宽度
        let big = dir.join("big.png");
        image::RgbaImage::from_pixel(1200, 800, image::Rgba([1, 2, 3, 255]))
            .save(&big)
            .unwrap();
        let (scaled, _) = render_thumbnail(&big, 240).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (240, 160));
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
        let work = dir.join("work");
        let (bytes, mime, codec) = encode_thumbnail(&thumb, None, &caps, &work).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(codec, "jpg");
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "应输出 JPEG 魔数");
        // 只编码不落盘：最终文件由调用方原子写，临时文件也必须清干净
        assert!(
            collect_files(&work).is_empty(),
            "编码阶段不应留下任何文件：{:?}",
            collect_files(&work)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_returns_fresh_hash_without_sidecar_file() {
        // v2.8.2 回归：ThumbHash 只随 get() 返回并由调用方入库，缓存目录里绝不能出现 .th 碎文件
        let dir = scratch_dir("thumb-no-sidecar");
        let source = dir.join("a.png");
        let mut image = image::RgbaImage::new(48, 24);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba([40, 160, 90, 255]);
        }
        image.save(&source).unwrap();

        let cache = scratch_dir("thumb-no-sidecar-cache");
        let thumbnails = Thumbnailer::new(cache.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (outcome, fresh) = rt.block_on(thumbnails.get(&source, 64, None));
        assert!(matches!(outcome, ThumbOutcome::Ready(_)));

        let fresh = fresh.expect("首次生成必须带出新鲜 ThumbHash");
        assert_eq!(fresh.key, Thumbnailer::hash_key(&source));
        assert!(!fresh.value.is_empty());
        assert!(
            BASE64.decode(fresh.value.as_bytes()).is_ok(),
            "hash 必须是合法 Base64"
        );

        // 缓存里只允许「扇出目录 / 单元目录 / 无扩展名文件」。
        // 任何带 `.jpg` / `.webp` 的文件都会让它自己被再缩略图一遍 —— 指数增殖的根源。
        let files = collect_files(&cache);
        assert!(!files.is_empty(), "缩略图本体应已落盘");
        for file in &files {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                name == STAMP_FILE || !name.contains('.'),
                "缓存文件名不能带扩展名：{name}"
            );
            assert!(!name.ends_with(".th"), "不应出现 .th 旁路文件：{name}");
        }
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&cache);
    }

    /// 在缓存里放一个「单元目录」：`<cache>/<xx>/<40 位 digest>/<文件…>`
    fn put_unit(cache: &Path, tag: &str, files: &[(&str, usize)]) -> PathBuf {
        let digest = format!("{tag:0>40}");
        let unit = cache.join(&digest[..2]).join(&digest);
        fs::create_dir_all(&unit).unwrap();
        for (name, len) in files {
            fs::write(unit.join(name), vec![0u8; *len]).unwrap();
        }
        unit
    }

    fn backdate(dir: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let handle = fs::File::options().write(true).open(entry.path()).unwrap();
            handle
                .set_times(fs::FileTimes::new().set_modified(when))
                .unwrap();
        }
    }

    #[test]
    fn cleanup_evicts_whole_units_oldest_first() {
        let cache = scratch_dir("thumb-cleanup");
        let a = put_unit(&cache, "aa1", &[("320-jpg", 512), (STAMP_FILE, 8)]);
        let b = put_unit(&cache, "bb2", &[("320-jpg", 512)]);
        let c = put_unit(&cache, "cc3", &[("320-jpg", 512)]);
        backdate(&a, Duration::from_secs(3 * 3600));
        backdate(&b, Duration::from_secs(2 * 3600));

        // 3 个单元约 1.5 KiB，限制 1 KiB → 先淘汰 a（剩 1 KiB 刚好达标）
        let removed = cleanup_cache(&cache, 1024, 0).unwrap();
        assert!(removed >= 520, "应按单元淘汰，实际 {removed}");
        assert!(!a.exists(), "最旧的单元要先删");
        assert!(b.exists() && c.exists(), "没超限的单元不能动");

        // 再收紧到 512 字节：连 b 一起淘汰，只留最新的 c（整单元删，不拆单文件）
        let removed_more = cleanup_cache(&cache, 512, 0).unwrap();
        assert!(
            removed_more >= 512,
            "继续收紧要再淘汰一个单元，实际 {removed_more}"
        );
        assert!(!b.exists(), "次旧的单元跟着删");
        assert!(c.exists(), "最新的单元必须留着");
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn cleanup_drops_expired_unit() {
        let cache = scratch_dir("thumb-expire");
        let unit = put_unit(&cache, "dd4", &[("320-jpg", 2048)]);
        backdate(&unit, Duration::from_secs(3600));
        // 过期阈值 1 秒：一小时前的单元整目录删除
        let removed = cleanup_cache(&cache, 10 * 1024 * 1024, 1).unwrap();
        assert_eq!(removed, 2048);
        assert!(!unit.exists());
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn cleanup_purges_legacy_flat_layout() {
        // 旧版把 `<hash>-320.webp` 直接摊在顶层：升级后必须一次清掉，
        // 否则这些带图片扩展名的文件会永远占地方，也还有被当图片的风险
        let cache = scratch_dir("thumb-legacy");
        fs::write(cache.join("0123456789abcdef-320.webp"), vec![0u8; 256]).unwrap();
        let removed = cleanup_cache(&cache, 10 * 1024, 0).unwrap();
        assert_eq!(removed, 256);
        assert!(collect_files(&cache).is_empty());
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn clear_cache_removes_everything() {
        let cache = scratch_dir("thumb-clear");
        put_unit(&cache, "ee5", &[("320-jpg", 100)]);
        put_unit(&cache, "ff6", &[("960-webp", 100)]);
        let (count, freed) = clear_cache(&cache).unwrap();
        assert_eq!(count, 2);
        assert_eq!(freed, 200);
        assert!(collect_files(&cache).is_empty());
        assert!(cache.exists(), "清空后目录要留着，下次直接写");
        let _ = fs::remove_dir_all(&cache);
    }

    /// 回归（指数增殖）：同一张图反复请求（含多个宽度）后，
    /// 缓存文件数必须只随「宽度种类」增长，绝不随请求次数增长。
    #[test]
    fn repeated_requests_do_not_grow_cache() {
        let dir = scratch_dir("thumb-repeat");
        let source = dir.join("a.png");
        let mut image = image::RgbaImage::new(40, 30);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba([10, 20, 200, 255]);
        }
        image.save(&source).unwrap();

        let cache = scratch_dir("thumb-repeat-cache");
        let thumbnails = Thumbnailer::new(cache.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        for _ in 0..3 {
            let (outcome, _) = rt.block_on(thumbnails.get(&source, 64, None));
            assert!(matches!(outcome, ThumbOutcome::Ready(_)));
        }
        let after_first = collect_files(&cache).len();
        assert_eq!(after_first, 2, "一个宽度 = 图 + stamp：{after_first}");

        // 换宽度：只多一个文件（同一单元目录内）
        let (wide, _) = rt.block_on(thumbnails.get(&source, 96, None));
        assert!(matches!(wide, ThumbOutcome::Ready(_)));
        assert_eq!(collect_files(&cache).len(), after_first + 1);

        // 再取一遍：全部命中，一个文件都不新增
        for _ in 0..3 {
            let _ = rt.block_on(thumbnails.get(&source, 64, None));
            let _ = rt.block_on(thumbnails.get(&source, 96, None));
        }
        assert_eq!(
            collect_files(&cache).len(),
            after_first + 1,
            "命中缓存不能再落任何文件"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&cache);
    }

    /// 删除源文件后，它的缓存单元必须能整目录清掉（只影响它自己）。
    #[test]
    fn purge_removes_only_that_source() {
        let cache = scratch_dir("thumb-purge");
        let thumbnails = Thumbnailer::new(cache.clone());
        let keep = PathBuf::from("C:/photos/keep.jpg");
        let gone = PathBuf::from("C:/photos/gone.jpg");
        let keep_unit = thumbnails.entry_dir(&keep);
        let gone_unit = thumbnails.entry_dir(&gone);
        fs::create_dir_all(&keep_unit).unwrap();
        fs::create_dir_all(&gone_unit).unwrap();
        fs::write(gone_unit.join("320-jpg"), b"x").unwrap();

        assert!(thumbnails.purge(&gone));
        assert!(!gone_unit.exists());
        assert!(keep_unit.exists(), "不能误删其它图片的缓存");
        assert_eq!(Thumbnailer::hash_key(&keep), Thumbnailer::hash_key(&keep));

        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn cleanup_is_idempotent_on_missing_dir() {
        // 目录不存在时必须返回 0 而不是 Io 错误（设置页清理按钮要幂等）
        let missing =
            std::env::temp_dir().join(format!("asmoday-thumb-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        assert_eq!(cleanup_cache(&missing, 1024, 3600).unwrap(), 0);
        assert_eq!(clear_cache(&missing).unwrap(), (0, 0));
    }

    #[test]
    fn fresh_big_file_is_treated_as_downloading() {
        let dir = scratch_dir("thumb-fresh");
        let big = dir.join("movie.mp4");
        fs::write(&big, vec![0u8; (FRESH_SIZE_BYTES + 1) as usize]).unwrap();
        // 刚写完的大文件：正是「下载中」的形态，必须先回占位图
        assert!(is_fresh_write(&big));

        // 小文件不受影响：刚上传的照片要能立刻出缩略图
        let small = dir.join("photo.jpg");
        fs::write(&small, vec![0u8; 1024]).unwrap();
        assert!(!is_fresh_write(&small));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_file_is_never_deferred() {
        let dir = scratch_dir("thumb-old");
        let file = dir.join("archive.mkv");
        fs::write(&file, vec![0u8; (FRESH_SIZE_BYTES + 1) as usize]).unwrap();
        // 把 mtime 往前拨 10 分钟：写盘早已结束，必须正常生成
        let old = SystemTime::now() - Duration::from_secs(600);
        let handle = fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        assert!(!is_fresh_write(&file));

        // 不存在 / 没权限时必须返回 false（不能让缩略图整体 500）
        assert!(!is_fresh_write(&dir.join("nope.mp4")));

        let _ = fs::remove_dir_all(&dir);
    }
}
