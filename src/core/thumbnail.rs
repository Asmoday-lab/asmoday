//! 缩略图生成（设计文档 3.4）。
//!
//! - 并发上限 2（信号量），拿不到就回占位图，绝不让数千张图一起解码把 CPU 打满；
//! - 结果落磁盘缓存，键 = 路径 + 修改时间 + 大小 + 有效宽度（源文件 mtime 变了自动失效；
//!   有效宽度 = min(请求宽度, 源图长边)，小图的各档位请求因此归并成同一份字节）；
//! - 容量上限与闲置天数来自 `config.toml` 的 `[media]`：命中即把 mtime 顶到当前时间，
//!   淘汰按「最久未访问」先删（闲置超期或超容量，谁先到算谁）；
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

/// `.tmp-` 残片超过这个年龄才算「死了」（进程被强杀留下的那半张图）。
///
/// 并发生成中的临时文件绝不能删：删了之后 rename 找不到源文件，那次落盘白做。
/// 一次缩略图生成是毫秒到秒级，10 分钟足够宽松。
const TEMP_STALE_AFTER: Duration = Duration::from_secs(600);

/// 命中缓存后刷新「最后访问时间」的最小间隔。
///
/// 每次命中都 `set_times` 等于给 inode 记一笔写，热图一页 120 张毫无意义；
/// 但间隔必须远小于 `thumb_cache_max_age_days`（默认 90 天），否则天天在看的图
/// 也会因为「一天只续一次」被算成闲置。1 小时是两者的平衡点。
const TOUCH_INTERVAL: Duration = Duration::from_secs(3600);

/// 单张图解码的边长上限（2026-11-04 审查）：挡住尺寸炸弹。
///
/// 一张「压缩后几百 KB、解开后 100000×100000」的 PNG 会吃掉数十 GB 内存；
/// image crate 默认不限制宽高，只有单次分配 512MB 的兜底。边长 16384 对
/// 正经照片/截图绰绰有余，超限交给 ffmpeg 分支在解码阶段直接降采样。
const MAX_IMAGE_SIDE: u32 = 16384;
/// 一次「解码 + 编码」的总超时：坏文件不能永远占着信号量（8 个卡死 = 缩略图全线 503）。
const GENERATE_TIMEOUT: Duration = Duration::from_secs(15);
/// 负缓存 TTL：同一指纹解码/截帧失败后，这么长时间内直接回占位图不再重试。
/// 没有它，目录里一个坏文件会让每次翻页都白烧一轮解码、每次都拉起 6 个 ffmpeg。
const FAIL_TTL_SECS: u64 = 30 * 60;

/// 负缓存：这个内容指纹最近是否失败过（单元目录下的 `fail-<fp>` 标记文件）。
///
/// 指纹含 mtime + size，内容一变就是新指纹，标记天然失效，不会错杀改过的图。
fn recently_failed(dir: &Path, fp: &str) -> bool {
    let Ok(meta) = std::fs::metadata(dir.join(format!("fail-{fp}"))) else {
        return false;
    };
    // 时钟回拨（mtime 在未来）也按「刚失败过」处理，与 touch_access 同口径
    meta.modified()
        .map(|t| {
            t.elapsed()
                .map(|d| d.as_secs() < FAIL_TTL_SECS)
                .unwrap_or(true)
        })
        .unwrap_or(false)
}

/// 记一次失败（存在即有效，mtime 即时间戳）。
fn mark_failed(dir: &Path, fp: &str) {
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(dir.join(format!("fail-{fp}")), b"");
    }
}

/// 生成成功后清掉失败标记（偶发失败下次不再被负缓存挡住）。
fn clear_failed(dir: &Path, fp: &str) {
    let _ = std::fs::remove_file(dir.join(format!("fail-{fp}")));
}

/// 文件是否可能仍在写盘（mtime 极新且体积可观）。
///
/// 生产路径统一走 [`is_fresh_write_parts`]（复用调用方已拿到的那份 metadata），
/// 这个包装只给测试用 —— 留着它容易让新代码多 stat 一次。
#[cfg(test)]
fn is_fresh_write(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let mtime = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    is_fresh_write_parts(meta.len(), mtime)
}

/// 已拿到 `len` / `mtime` 时判断「大文件是否刚写完」。
///
/// 时钟回拨（mtime 在未来）时不当作新鲜：宁可生成一次，也不要永远回占位图。
pub(crate) fn is_fresh_write_parts(len: u64, mtime_secs: u64) -> bool {
    if len < FRESH_SIZE_BYTES {
        return false;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.checked_sub(mtime_secs)
        .is_some_and(|age| age < FRESH_WINDOW.as_secs())
}

/// 一页缩略图在进入 `get_prepared` 之前就把键算好：一次 `metadata` 出全部三个值。
///
/// 拆出来的唯一原因是 IO 次数：`get()` 内部原先各自 stat 一遍，再加上
/// `effective_width` 的一次 `image_dimensions`（要 open + 读图头），
/// 每个缩略图请求要碰 6 次源文件。SMB 上那是 1~2 秒/张。
#[derive(Debug, Clone)]
pub struct ThumbKey {
    /// 有效宽度（小图不放大，不同档位落到同一个键）
    pub width: u32,
    /// 缓存文件名前缀（源文件长度 + mtime）
    pub fingerprint: String,
    /// 源文件是「刚写完的大文件」，先不生成
    pub fresh_write: bool,
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
    /// **可重试**：解码队列打满 / 源文件还在写入 → HTTP 503 + Retry-After。
    ///
    /// 不能回 200 占位图：`<img>` 认为加载成功，那块灰就永远不会再请求
    /// （首屏几十张图一起排队时，表现为「随机几块一直是空的」）。
    Busy,
    /// 不可重试：解码失败 / 类型不支持 → 占位图（200）
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

/// 源路径的稳定摘要（sha1 十六进制 40 字符）。
///
/// 缓存文件名一律只用它，绝不带源文件名或扩展名 —— FileBrowser 的
/// diskcache 也是 `sha1(key)` 的无扩展名文件。这样即使缓存目录被误暴露成
/// 普通目录，也不会被识别成图片、更不会「缓存给自己再缓存一遍」。
///
/// 摘要用的是 `normalize_display` 之后的写法：同一个文件可能以
/// `C:/x/y.jpg`（接口入参）或 `\\?\C://x//y.jpg`（canonicalize 结果）出现，
/// 不归一化就会算出两个键、缓存和 ThumbHash 全都对不上。
fn digest_of(path: &Path) -> String {
    let display = crate::core::fs::normalize_display(path);
    // Windows 文件系统大小写不敏感：同一个文件可能以 `C:/Photos/A.JPG`（接口入参）
    // 和 `c:/photos/a.jpg`（系统回读）两种形态出现，不归一就会算出两个单元目录。
    // 口径与 `path_guard::find_by_path_text` 的 `to_lowercase` 保持一致。
    #[cfg(windows)]
    let display = display.to_lowercase();
    let mut hasher = Sha1::new();
    hasher.update(display.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 单个源文件的缓存单元目录：`<cache>/<digest[..2]>/<digest>`。
///
/// 一级两字符扇出（同 FileBrowser）：几十万张图也不会让单目录堆到上百万项；
/// 删除一个源文件的缓存也只需要删掉这一个目录。
///
/// 单独抽成自由函数是为了让扫描侧（`api::media` 的孤儿回收）能复用同一套
/// 摘要口径 —— 索引里存的是展示串，两边各算一次摘要必须完全一致。
pub fn unit_dir(cache_dir: &Path, path: &Path) -> PathBuf {
    let digest = digest_of(path);
    cache_dir.join(&digest[..2]).join(digest)
}

/// 批量回收缓存单元（扫描确认源文件/目录已消失后调用）。
///
/// 只删整个单元目录：同一张图的各档位同生共死，留下单个文件就是永远命不中的孤儿。
/// 单元不存在（从没生成过缩略图）时 `rmdir` 失败，直接跳过 —— 这个接口必须幂等。
pub fn purge_paths(cache_dir: &Path, sources: &[String]) -> usize {
    let mut removed = 0usize;
    for source in sources {
        if std::fs::remove_dir_all(unit_dir(cache_dir, Path::new(source))).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub struct Thumbnailer {
    semaphore: Arc<Semaphore>,
    cache_dir: PathBuf,
    generated: AtomicUsize,
    /// 缓存总容量上限（字节），由 `[media].thumb_cache_max_size_mb` 决定
    limit_bytes: AtomicU64,
    /// 缓存单元最长**闲置**秒数（mtime 即最后访问时间），由 `[media].thumb_cache_max_age_days` 决定
    max_age_secs: AtomicU64,
    /// FFmpeg 编码能力缓存（键 = 可执行文件路径），避免每张缩略图都跑一次 `-encoders`
    caps: Arc<Mutex<Option<(PathBuf, Capabilities)>>>,
    /// 每个缓存单元一把异步锁：同一张图被 160 / 400 同时请求时只解码一次，
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

    /// ThumbHash 的库键：**只跟路径有关，不含 mtime**。
    ///
    /// 旧实现把 mtime 混进键里，改一次图就多一行，库随编辑次数膨胀；
    /// 现在键稳定，值由每次成功生成覆盖（缩略图本身靠 `stamp` 判过期）。
    pub fn hash_key(path: &Path) -> String {
        digest_of(path)
    }

    /// 源文件的**内容指纹**：`v4-<长度>-<mtime秒>`。既进缓存文件名，也做 HTTP ETag。
    ///
    /// 键里带源文件状态（长度 + mtime）与格式版本，所以不存在「戳与图不一致」的窗口——
    /// 命中与否只看这个文件在不在。v4.4.2 之前 `render_thumbnail` 会把小于请求宽度的图
    /// 放大（300×200 生成成 960 宽），灯箱用 960 当打底时画面先按放大版铺开、原图一到
    /// 又缩回去，就是「打开预览先大后小」；把版本号放进文件名后，旧版文件不再命中，
    /// 新图换个名字落盘，LRU 顺手把旧版收走（不再有 stamp 文件互相踩的窗口）。
    ///
    /// v4.6.3：文件名改用**有效宽度**（见 `effective_width`）。小图在 160/320/400 各存了
    /// 一份字节完全相同的副本，升版让这些重复副本一次性失效，由 LRU 收走。
    /// 缓存格式版本：**任何改变渲染结果的改动都必须 +1**，否则老图永远命中旧缓存。
    ///
    /// v5（v4.9.11）：缩放从 `imageops::thumbnail`（标量 box filter）换成
    /// fast_image_resize 的 Lanczos3。文件名不变的话，用户之前看过的图会一直
    /// 停在旧算法的模糊版本，等于这次优化白做。
    const CACHE_VERSION: u8 = 5;

    /// 视频首帧的独立版本号：它不跟随图片的取值范围（截帧比解图贵一个数量级，
    /// 跟着图片升版会让几千个视频白跑一遍 FFmpeg）。
    const POSTER_VERSION: u8 = 1;

    pub fn fingerprint(path: &Path) -> String {
        let (len, mtime) = Self::source_stamp(path);
        Self::fingerprint_from(len, mtime)
    }

    /// 已拿到 `len` / `mtime`（一次 `metadata`）时的指纹。
    ///
    /// `/api/files/thumb` 每页要处理 120 张图，而 `metadata` 在 SMB 上是 100~300ms
    /// 一次；一次请求里重复 stat 四五遍，就是「图片库比文件管理慢几十倍」的来源。
    pub fn fingerprint_from(len: u64, mtime: u64) -> String {
        format!("v{}-{len:x}-{mtime:x}", Self::CACHE_VERSION)
    }

    /// 源文件 mtime（秒）。前端把它当作 URL 上的 `v`，服务端据此决定能不能长缓存
    /// （`v` 与当前 mtime 一致 = 这条 URL 代表的就是这一版字节）。
    pub fn mtime_secs(path: &Path) -> u64 {
        Self::source_stamp(path).1
    }

    /// 有效宽度：请求宽度与源图长边取小 —— 小图不放大，不同档位因此撞到同一个值。
    ///
    /// `render_thumbnail` 从 v4.4.2 起就不放大了，但文件名一直按**请求**宽度取：
    /// 一张 200×150 的图在 160 / 320 / 400 三档各存一份**字节完全相同**的副本。
    /// 用有效宽度入键后小图只留一份（编码是确定性的，同样的位图编出同样的字节）。
    /// 只读图片文件头，不整图解码。
    ///
    /// 读不到尺寸（视频、损坏文件、无权限）时原样返回请求宽度 —— 视频首帧不走这里，
    /// 它的宽度由 FFmpeg 的缩放决定，不做归并。
    pub fn effective_width(path: &Path, width: u32) -> u32 {
        let bound = width.clamp(64, 1024);
        image::image_dimensions(path)
            .map(|(w, h)| bound.min(w.max(h)))
            .unwrap_or(bound)
    }

    /// 测试用包装：生产路径拿的是已读好的 `len` / `mtime`
    #[cfg(test)]
    fn poster_fingerprint(path: &Path) -> String {
        let (len, mtime) = Self::source_stamp(path);
        Self::poster_fingerprint_from(len, mtime)
    }

    /// 已拿到 `len` / `mtime` 时的海报指纹（省掉一次 SMB `metadata`）
    pub fn poster_fingerprint_from(len: u64, mtime: u64) -> String {
        format!("p{}-{len:x}-{mtime:x}", Self::POSTER_VERSION)
    }

    /// 命中即续期：把缓存文件的 mtime 顶到当前时间。
    ///
    /// LRU 淘汰只认 mtime（`cleanup_cache` / `unit_stats` 都是取单元内最新 mtime），
    /// 所以「有没有人看」必须落在这一个字段上。旧实现是
    /// `OpenOptions::new().write(true).open(&file)` —— **只打开不写入不会改 mtime**，
    /// 等于什么也没做，`thumb_cache_max_age_days` 实际退化成「生成后无条件过期」：
    /// 天天在看的图也会在第 90 天被删掉重生成。真正续期必须走 `File::set_times`。
    ///
    /// 距上次触碰不足 [`TOUCH_INTERVAL`] 的直接跳过：只花一次 `metadata`，
    /// 不写 inode（本地盘微秒级，SMB 上缓存目录也不在共享里）。
    fn touch_access(path: &Path) {
        let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
            return;
        };
        let now = SystemTime::now();
        // 时钟回拨（mtime 在未来）也按「该续期」处理，否则这个文件永远不再刷
        let idle = now.duration_since(modified).unwrap_or(TOUCH_INTERVAL);
        if idle < TOUCH_INTERVAL {
            return;
        }
        // Windows 上 SetFileTime 需要 FILE_WRITE_ATTRIBUTES，只读句柄会 ACCESS_DENIED
        let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) else {
            return;
        };
        let _ = file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(now)
                .set_modified(now),
        );
    }

    /// 取某个缓存单元里已生成好的图（文件名前缀就是当前内容的指纹）。
    async fn cached(dir: &Path, fp: &str, width: u32) -> Option<ThumbOutcome> {
        for (codec, mime) in [("webp", "image/webp"), ("jpg", "image/jpeg")] {
            let file = dir.join(format!("{fp}-{width}-{codec}"));
            if let Ok(bytes) = tokio::fs::read(&file).await {
                Self::touch_access(&file);
                return Some(ThumbOutcome::Ready(Rendered { bytes, mime }));
            }
        }
        None
    }

    /// 取缓存单元里的视频首帧。
    async fn cached_poster(dir: &Path, fp: &str, width: u32) -> Option<ThumbOutcome> {
        for format in ffmpeg::POSTER_FORMATS {
            let file = dir.join(format!("{fp}-{width}p-{}", format.extension()));
            if let Ok(bytes) = tokio::fs::read(&file).await {
                Self::touch_access(&file);
                return Some(ThumbOutcome::Ready(Rendered {
                    bytes,
                    mime: format.mime(),
                }));
            }
        }
        None
    }

    /// 删掉同一缓存单元里属于**旧内容**的文件（文件名前缀不是当前指纹）。
    ///
    /// 键里带源文件状态后，改一次图会多一份文件；不清理的话目录会随编辑次数线性增长。
    /// 只动我们自己命名的文件（`v…` / `p…` 且含连字符），外加超龄的 `.tmp-` 残片
    /// （进程被强杀时留下的那半张图）。当前正在写的那份临时文件不到 10 分钟，不会误删。
    fn prune_stale(dir: &Path, keep_prefix: &str) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".tmp-") {
                if temp_is_stale(&entry, now) {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            }
            if name.starts_with(keep_prefix) || !name.contains('-') {
                continue;
            }
            if !(name.starts_with('v') || name.starts_with('p')) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
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
    /// 缓存布局：`<digest[..2]>/<digest>/<指纹>-<有效宽度>-<编码>`，指纹含源文件长度与 mtime，
    /// 所以**命中只看文件在不在**（没有 stamp 文件，也就没有「戳写了图没写」的窗口）。
    /// 源文件一变指纹就变，新图换个名字落盘、旧名由 `prune_stale` 收走。
    pub async fn get(
        &self,
        path: &Path,
        width: u32,
        webp_bin: Option<&Path>,
    ) -> (ThumbOutcome, Option<FreshHash>) {
        // 按**有效宽度**入键：比请求宽度小的图不再按 160/320/400 各存一份
        // 字节完全相同的副本（渲染本身早就不放大，浪费的只是这些重复文件）。
        let width = Self::effective_width(path, width);
        let (len, mtime) = Self::source_stamp(path);
        let key = ThumbKey {
            width,
            fingerprint: Self::fingerprint_from(len, mtime),
            fresh_write: is_fresh_write_parts(len, mtime),
        };
        self.get_prepared(path, key, webp_bin).await
    }

    /// 已算好「有效宽度 / 指纹 / 是否正在写盘」时的取图路径。
    ///
    /// `/api/files/thumb` 走这条：它只在 handler 里做一次 `metadata`，把三个值一次算完
    /// 传进来，避免 `get()` 内部再 stat 一遍。SMB 上一次 `metadata` 是 100~300ms，
    /// 一页 120 张图重复几次就是几十秒——这是图片库「卡」的主要来源（v4.9.7）。
    pub async fn get_prepared(
        &self,
        path: &Path,
        key: ThumbKey,
        webp_bin: Option<&Path>,
    ) -> (ThumbOutcome, Option<FreshHash>) {
        let ThumbKey {
            width,
            fingerprint: fp,
            fresh_write,
        } = key;
        let dir = unit_dir(&self.cache_dir, path);

        if let Some(outcome) = Self::cached(&dir, &fp, width).await {
            return (outcome, None);
        }

        // 下载中的大文件先不生成（设计文档 3.4.1）：写盘还没结束，
        // 这一秒生成的图下一秒就因 mtime 变化失效，纯属白烧 CPU。
        if fresh_write {
            // 还在写盘：等下一轮再取（前端按 Retry-After 退避重试）
            return (ThumbOutcome::Busy, None);
        }

        // 同一源文件串行：320 / 960 同时请求只解码一次
        let lock = self.key_lock(&dir);
        let _serial = lock.lock().await;
        if let Some(outcome) = Self::cached(&dir, &fp, width).await {
            return (outcome, None);
        }
        // 负缓存（2026-11-04 审查）：这个指纹最近失败过，30 分钟内直接占位。
        // 必须在抢信号量之前——坏文件的每次重试都在浪费一个并发槽。
        if recently_failed(&dir, &fp) {
            return (ThumbOutcome::Placeholder, None);
        }

        let permit =
            match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                // 队列被打满：回 503，由前端退避后重新排队
                _ => return (ThumbOutcome::Busy, None),
            };

        let source = path.to_path_buf();
        let hash_key = Self::hash_key(&source);
        let work_dir = dir.clone();
        let caps = Arc::clone(&self.caps);
        let webp_bin = webp_bin.map(Path::to_path_buf);

        // 渲染 + 编码放阻塞线程池；**落盘不在这条链路上**（见下）。
        //
        // 并发许可必须活到解码**真正结束**（v4.9.22）：`spawn_blocking` 不可取消，
        // 超时只是不再等它。以前超时点一到就 drop(permit)，慢盘上被放弃的解码会越积
        // 越多，并发闸等于不存在（每个新请求都能领到空出来的槽，继续堆线程）。
        // 现在 permit 移进外层任务，任务自然结束时才归还。
        let work = tokio::spawn(async move {
            let ffmpeg_for_decode = webp_bin.clone();
            let produced = tokio::task::spawn_blocking(move || {
                let (thumb, hash) = render_thumbnail(&source, width, ffmpeg_for_decode.as_deref())?;
                let (bytes, mime, codec) =
                    encode_thumbnail(&thumb, webp_bin.as_deref(), &caps, &work_dir)?;
                Some((bytes, mime, codec, hash))
            })
            .await
            .ok()
            .flatten();
            drop(permit);
            produced
        });

        // 整链路限时：ffmpeg 卡死 / 慢盘不能无限等（超时后线程自己跑完就结束）。
        let generated = match tokio::time::timeout(GENERATE_TIMEOUT, work).await {
            Ok(Ok(value)) => value,
            // 任务 panic 与渲染失败同途：都记负缓存
            Ok(Err(_)) => None,
            Err(_) => {
                // 超时：解码线程还活着，结果未知。**不能**写负缓存，否则这张图
                // 半小时内恒为占位图；回 503 让前端退避重试才是对的。
                return (ThumbOutcome::Busy, None);
            }
        };

        let Some((bytes, mime, codec, hash)) = generated else {
            mark_failed(&dir, &fp);
            return (ThumbOutcome::Placeholder, None);
        };
        clear_failed(&dir, &fp);
        self.count_generated();

        // 字节已经在手上了，客户端不必等磁盘：FileBrowser 的 `createPreview` 也是先回
        // 响应、再 `go func()` 落缓存。写失败最多下次重做，没有要回滚的状态。
        //
        // 这里**只**清自己这一格（`prune_stale`），全树淘汰交给 `count_generated`
        // 的「每 25 次一次」（v4.9.22）。以前每生成一张都无条件 `cleanup_cache`，
        // 一次翻页 120 张 = 120 个并发全树扫描（3 万文件时是几百万次 stat），
        // 把 blocking 池占满后上传、浏览、DB 查询全部停摆。
        let target_dir = dir.clone();
        let target = target_dir.join(format!("{fp}-{width}-{codec}"));
        let keep = format!("{fp}-");
        let stored = bytes.clone();
        tokio::task::spawn_blocking(move || {
            let _ = write_atomic(&target, &stored);
            Self::prune_stale(&target_dir, &keep);
        });

        let fresh = FreshHash {
            key: hash_key,
            value: BASE64.encode(&hash),
        };
        (ThumbOutcome::Ready(Rendered { bytes, mime }), Some(fresh))
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
        std::fs::remove_dir_all(unit_dir(&self.cache_dir, path)).is_ok()
    }

    /// 视频首帧封面（设计文档 3.4：影视库不联网刮削，靠外部 FFmpeg 截首帧）。
    ///
    /// 与图片缩略图共用缓存目录、`stamp` 判据、per-key 锁和信号量；
    /// 输出格式优先 WebP → JPEG → PNG，按该构建实际拥有的编码器逐级降级。
    pub async fn video_poster(&self, path: &Path, bin: &Path, width: u32) -> ThumbOutcome {
        let (len, mtime) = Self::source_stamp(path);
        self.video_poster_prepared(
            path,
            bin,
            width,
            Self::poster_fingerprint_from(len, mtime),
            is_fresh_write_parts(len, mtime),
        )
        .await
    }

    /// 已经算好指纹与「是否正在写盘」时的截帧路径（`/api/files/thumb` 走这条）。
    pub async fn video_poster_prepared(
        &self,
        path: &Path,
        bin: &Path,
        width: u32,
        fp: String,
        fresh_write: bool,
    ) -> ThumbOutcome {
        let width = width.clamp(64, 1024);
        let dir = unit_dir(&self.cache_dir, path);

        if let Some(outcome) = Self::cached_poster(&dir, &fp, width).await {
            return outcome;
        }

        // 正在下载的影视文件：截帧要拉起 FFmpeg 进程，比解图更贵，
        // 更要等写盘结束（设计文档 3.4.1）。缓存命中已在上面返回。
        if fresh_write {
            return ThumbOutcome::Busy;
        }

        let lock = self.key_lock(&dir);
        let _serial = lock.lock().await;
        if let Some(outcome) = Self::cached_poster(&dir, &fp, width).await {
            return outcome;
        }
        // 负缓存：ffprobe 都不认的文件每次翻页都会白拉 6 个 ffmpeg，必须挡住
        if recently_failed(&dir, &fp) {
            return ThumbOutcome::Placeholder;
        }

        // 共享同一个信号量：截帧比解 JPEG 更吃 CPU，绝不允许并发打满。
        // 同 `get_prepared`：许可交给持有任务，截帧真正结束才归还（v4.9.22）。
        let permit =
            match tokio::time::timeout(ACQUIRE_TIMEOUT, self.semaphore.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                _ => return ThumbOutcome::Busy,
            };

        let source = path.to_path_buf();
        let target_dir = dir.clone();
        let fp_for_task = fp.clone();
        let ffmpeg_bin = bin.to_path_buf();
        let caps = Arc::clone(&self.caps);

        // 截帧链路限时：ffmpeg 卡在坏文件/慢盘上不能永远等
        let work = tokio::spawn(async move {
            let produced = tokio::task::spawn_blocking(move || {
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
                    let final_path =
                        target_dir.join(format!("{fp_for_task}-{width}p-{}", format.extension()));
                    if std::fs::rename(&tmp, &final_path).is_err() {
                        let _ = std::fs::remove_file(&tmp);
                        continue;
                    }
                    let keep = format!("{fp_for_task}-");
                    Self::prune_stale(&target_dir, &keep);
                    if let Ok(bytes) = std::fs::read(&final_path) {
                        return Some((bytes, format.mime()));
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            drop(permit);
            produced
        });

        let generated = match tokio::time::timeout(GENERATE_TIMEOUT, work).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => None,
            // 超时：截帧进程可能还在跑，结果未知 —— 不写负缓存，让前端重试
            Err(_) => return ThumbOutcome::Busy,
        };

        match generated {
            Some((bytes, mime)) => {
                clear_failed(&dir, &fp);
                self.count_generated();
                ThumbOutcome::Ready(Rendered { bytes, mime })
            }
            None => {
                mark_failed(&dir, &fp);
                ThumbOutcome::Placeholder
            }
        }
    }

    /// 后台/手动清理：按「闲置天数 + LRU 容量」淘汰，返回释放的字节数。
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
///
/// 解码分两路（v4.9.11）：
/// 1. `image` crate —— jpeg / png / gif / webp / bmp / tiff / ico，以及通过
///    [`register_heif_hook`] 挂上去的 heic / heif / hif；
/// 2. 上面失败时交给 ffmpeg：`-vf scale=` 让它**解码时直接降采样**，
///    既覆盖 avif 这类 `image` 不认的格式，又把内存从「整张大图」压到目标尺寸。
fn render_thumbnail(
    path: &Path,
    width: u32,
    ffmpeg_bin: Option<&Path>,
) -> Option<(image::RgbaImage, Vec<u8>)> {
    let (source_w, source_h, mut rgba) = decode_rgba(path, width, ffmpeg_bin)?;

    let tiny = fir_scale(&mut rgba, source_w, source_h, 32)?;
    let hash = thumbhash::rgba_to_thumb_hash(tiny.0 as usize, tiny.1 as usize, &tiny.2);

    // 不放大：请求尺寸比原图大时按原图长边封顶（否则灯箱占位会被拉大一圈）
    let bound = width.min(source_w.max(source_h));
    let (tw, th, buf) = fir_scale(&mut rgba, source_w, source_h, bound)?;
    Some((image::RgbaImage::from_raw(tw, th, buf)?, hash))
}

/// 把 HEIF/HEIC 解码器挂到 `image` crate 上（只做一次）。
///
/// `heifer` 是纯 Rust 实现（自带 HEVC 解码），注册之后 `ImageReader::decode()`
/// 就能直接解 `.heic` / `.heif` / `.hif`，调用点不必自己分派。
fn register_heif_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !heifer::image_crate::register_image_decoder_hooks() {
            crate::log_warn!("HEIC decoder hook not registered: the extension is already taken by another decoder");
        }
    });
}

/// 解码成 RGBA 原始像素。返回 `(宽, 高, RGBA 缓冲)`。
fn decode_rgba(path: &Path, bound: u32, ffmpeg_bin: Option<&Path>) -> Option<(u32, u32, Vec<u8>)> {
    register_heif_hook();
    // 尺寸上限（2026-11-04 审查）：image crate 默认不限宽高，一张压缩后
    // 几百 KB 的 PNG 可以展开成数十 GB 内存。超限走下面的 ffmpeg 分支，
    // 让它在解码阶段就降采样，不经历「先解全图」。
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    let decoded = image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .map(|mut reader| {
            reader.limits(limits);
            reader
        })
        .and_then(|reader| reader.decode().ok());
    if let Some(decoded) = decoded {
        let rgba = decoded.to_rgba8();
        let (w, h) = rgba.dimensions();
        return Some((w, h, rgba.into_raw()));
    }
    // `image` 不认（avif / 没有解码器的 heic / 损坏文件）：交给 ffmpeg。
    // `-vf scale=` 在解码阶段就降采样，不做「先解全图再缩」。
    let bin = ffmpeg_bin?;
    ffmpeg_decode_scaled(path, bound, bin)
}

/// ffmpeg 解码并直接缩到 `bound` 以内，输出 PNG 再读回内存。
fn ffmpeg_decode_scaled(path: &Path, bound: u32, bin: &Path) -> Option<(u32, u32, Vec<u8>)> {
    use std::process::{Command, Stdio};

    // `min(bound, iw)`：不放大；`-2` 让高度自动取偶数（多数编码器要求）
    let filter = format!("scale='min({},iw)':-2", bound.max(1));
    let out = ffmpeg::run_with_timeout(
        Command::new(bin)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args([
                "-vf",
                &filter,
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null()),
        ffmpeg::POSTER_TIMEOUT,
    )?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let decoded = image::load_from_memory(&out.stdout).ok()?;
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

/// 用 fast_image_resize 等比缩到 `bound` 以内（Lanczos3，SIMD）。
///
/// 比 `imageops::thumbnail`（标量 box filter）快 3~5 倍，且不会把细节糊成一片。
/// 返回 `(宽, 高, RGBA 缓冲)`。
///
/// 入参是 `&mut [u8]` 而不是所有者：FIR 的 `from_slice_u8` 直接借用这块内存，
/// 一次 4000×3000 的图少拷 48MB（v4.9.12 之前这里是 `src.to_vec()`，
/// 而 `render_thumbnail` 要缩两次，峰值多出两幅整图）。
fn fir_scale(src: &mut [u8], w: u32, h: u32, bound: u32) -> Option<(u32, u32, Vec<u8>)> {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let bound = bound.max(1);
    let long = w.max(h).max(1);
    let scale = (bound as f32 / long as f32).min(1.0);
    let dw = ((w as f32 * scale).round() as u32).max(1);
    let dh = ((h as f32 * scale).round() as u32).max(1);
    if dw == w && dh == h {
        return Some((w, h, src.to_vec()));
    }

    let src_image = FirImage::from_slice_u8(w, h, src, PixelType::U8x4).ok()?;
    let mut dst = FirImage::new(dw, dh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    resizer
        .resize(
            &src_image,
            &mut dst,
            Some(&ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))),
        )
        .ok()?;
    Some((dw, dh, dst.buffer().to_vec()))
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

/// `.tmp-` 文件是否已经没人管（超过 `TEMP_STALE_AFTER` 没被改写）。
fn temp_is_stale(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    entry
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= TEMP_STALE_AFTER)
}

/// 从缓存文件名里剥出指纹：`v4-<len>-<mtime>` / `p1-<len>-<mtime>`。
///
/// 缓存名固定是 `{指纹}-{宽度}-{编码}`（海报是 `{指纹}-{宽度}p-{扩展名}`），
/// 去掉末尾两段就是指纹。认不出来的一律返回 `None`（旧扁平布局、外来文件），
/// 调用方对这类文件保持不动。
fn fingerprint_of(name: &str) -> Option<String> {
    let mut tail = name.rsplitn(3, '-');
    let codec = tail.next()?;
    if !matches!(codec, "webp" | "jpg" | "png") {
        return None;
    }
    let width = tail.next()?;
    let digits = width.strip_suffix('p').unwrap_or(width);
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let fingerprint = tail.next()?;
    let mut seg = fingerprint.split('-');
    let version = seg.next()?;
    let len = seg.next()?;
    let mtime = seg.next()?;
    if seg.next().is_some() {
        return None;
    }
    let version_ok = matches!(version.as_bytes().first(), Some(b'v' | b'p'))
        && version[1..].chars().all(|ch| ch.is_ascii_digit());
    let is_hex = |text: &str| !text.is_empty() && text.chars().all(|ch| ch.is_ascii_hexdigit());
    if !version_ok || !is_hex(len) || !is_hex(mtime) {
        return None;
    }
    Some(fingerprint.to_string())
}

/// 扫一遍缓存单元：收走超龄 `.tmp-` 残片，只留下**最新指纹组**的文件，返回释放字节。
///
/// 为什么需要这一步：命中缓存会 touch 文件 mtime 参与 LRU，所以活跃单元里的旧副本
/// **永远不会**被容量淘汰 —— 于是「升级换了键格式（v3→v4）」或「改图后没赶上
/// `prune_stale`」留下的旧组会长期赖在磁盘上，用户看到的就是「thumbnails 里一堆
/// 重复图」。这里按指纹分组、只保留最新那组，存量一次性收干净。
///
/// 认不出指纹的文件（旧扁平布局、外来文件）不参与分组，也不删除。
fn sweep_unit(dir: &Path, now: SystemTime) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0;
    let mut newest: HashMap<String, SystemTime> = HashMap::new();
    let mut files: Vec<(PathBuf, u64, Option<String>)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".tmp-") {
            // 正在写的那份（新鲜的）绝不能动：删了 rename 找不到源文件，那次落盘白做
            if temp_is_stale(&entry, now) && std::fs::remove_file(entry.path()).is_ok() {
                freed += meta.len();
            }
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let group = fingerprint_of(&name);
        if let Some(group) = &group {
            let slot = newest.entry(group.clone()).or_insert(modified);
            if modified > *slot {
                *slot = modified;
            }
        }
        files.push((entry.path(), meta.len(), group));
    }
    // 只有一组（或认不出来）：没有可归并的旧副本
    if newest.len() < 2 {
        return freed;
    }
    let keep = newest
        .into_iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(group, _)| group);
    for (path, len, group) in files {
        if group.is_none() || group == keep {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            freed += len;
        }
    }
    freed
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

/// 统计一个缓存单元目录里的文件数、总字节、最近一次访问时间（mtime，命中即续期）。
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
            keep_latest(&mut newest, sub_newest);
            continue;
        }
        files += 1;
        bytes += meta.len();
        keep_latest(&mut newest, meta.modified().unwrap_or(UNIX_EPOCH));
    }
    (files, bytes, newest)
}

/// 取两者中较新的时间（函数名原来叫 `oldest`，与实现正好相反）
fn keep_latest(current: &mut SystemTime, candidate: SystemTime) {
    if candidate > *current {
        *current = candidate;
    }
}

/// 先按**闲置天数**删，再按最久未访问淘汰，直到低于容量上限。
///
/// 淘汰单位是「一个源文件的缓存单元目录」（`<digest>`），不是单个文件 ——
/// 同一张图的多个宽度必须同生共死，否则会留下永远命不中的孤儿。
/// 旧版扁平布局（`<hash>-320.webp` / `<sha>.th`）一律直接删：新版不会在
/// 顶层放文件，留着只会占空间。
///
/// 「闲置」= 单元内**最新** mtime 距今多久；命中缓存会 `set_times` 续期，
/// 所以常看的图不会被天数阈值扫掉，只有真正没人碰的单元才会过期。
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
            // 单元目录里的 `.tmp-` 残片（强杀留下的半张图）与**旧指纹组**：
            // 以前只在整单元被淘汰时顺带删，可命中缓存会把 mtime 顶新、
            // 活跃单元的旧副本永远等不到淘汰 —— 清理任务路过时先收掉。
            removed += sweep_unit(&unit_path, now);
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
    /// v4.9.11：HEIC 必须真的能解 —— iPhone 直出的照片全靠它。
    ///
    /// 用 `heifer` 自己编一张 HEIC，再走生产路径（`render_thumbnail`）解码，
    /// 等于验证「注册 image hook → ImageReader 认得 .heic → FIR 缩放」整条链。
    #[test]
    fn heic_round_trips_through_image_hook() {
        let (w, h) = (320u32, 240u32);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            rgb.extend_from_slice(&[(i % 251) as u8, 96, 160]);
        }
        let heic = heifer::encode_rgb8(w, h, &rgb, &heifer::EncodeOptions::default())
            .expect("heifer 编码 HEIC 失败");
        assert_eq!(&heic[4..8], b"ftyp", "编出来的应该是 ISOBMFF 容器");

        let dir = scratch_dir("heic");
        let file = dir.join("sample.heic");
        fs::write(&file, &heic).unwrap();

        let (thumb, hash) =
            render_thumbnail(&file, 160, None).expect("HEIC 应能被 image hook 解码");
        assert_eq!(thumb.width().max(thumb.height()), 160, "长边应缩到 160");
        assert_eq!(thumb.width() * 240, thumb.height() * 320, "宽高比要保住");
        assert!(!hash.is_empty(), "ThumbHash 不能为空");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_dir_is_stable_per_path_and_fanned_out() {
        let dir = scratch_dir("thumb-entry");
        let file = dir.join("a.jpg");
        fs::write(&file, b"x").unwrap();

        let cache = scratch_dir("thumb-entry-cache");
        let first = unit_dir(&cache, &file);
        // 改了内容（长度 / mtime 变化）之后仍是同一个目录：覆盖写，不新增
        fs::write(&file, b"xxxx").unwrap();
        assert_eq!(first, unit_dir(&cache, &file));

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

    /// Windows 文件系统大小写不敏感：同一个文件以不同大小写形态进来必须落到同一个单元。
    ///
    /// `path_guard::find_by_path_text` 早就按 `to_lowercase` 比对了，这里口径要一致，
    /// 否则同一张图会各存一份缓存、ThumbHash 也记两行。
    #[cfg(windows)]
    #[test]
    fn entry_dir_ignores_path_case_on_windows() {
        let cache = scratch_dir("thumb-case");
        assert_eq!(
            unit_dir(&cache, Path::new("C:/Photos/A.JPG")),
            unit_dir(&cache, Path::new("c:/photos/a.jpg")),
            "大小写不同也是同一个文件，必须落到同一个单元目录"
        );
        let _ = fs::remove_dir_all(&cache);
    }

    /// 缓存键带**格式版本 + 源文件状态**：v4.4.2 之前生成的「放大版」缩略图只靠
    /// 路径永远命中，灯箱拿 960 缩略图打底就会「先大后小」。把版本写进文件名是让
    /// 老缓存失效的唯一开关，删掉它这条回归会立刻变红。
    #[test]
    fn thumbnail_key_carries_format_version() {
        let dir = scratch_dir("thumb-key");
        let file = dir.join("a.png");
        image::RgbaImage::from_pixel(12, 9, image::Rgba([1, 2, 3, 255]))
            .save(&file)
            .unwrap();

        let key = Thumbnailer::fingerprint(&file);
        // 断言「带当前版本号」而不是钉死某个数字：渲染结果一变就得升版本，
        // 硬编码的话每次升版都要改测试，改着改着就会有人顺手把断言删掉。
        assert!(
            key.starts_with(&format!("v{}-", Thumbnailer::CACHE_VERSION)),
            "缩略图键必须带格式版本：{key}"
        );
        // 海报（视频首帧）用自己的版本号：跟着图片一起升版只是白跑一遍 FFmpeg
        assert!(
            Thumbnailer::poster_fingerprint(&file).starts_with("p1-"),
            "海报键必须用独立版本号"
        );
        // 内容一变（长度 / mtime 变）键必须跟着变，否则就是「改了不生效」
        assert_ne!(key, Thumbnailer::fingerprint(&dir.join("missing.png")));

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
        let (thumb, _) = render_thumbnail(&small, 960, None).unwrap();
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
        let (scaled, _) = render_thumbnail(&big, 240, None).unwrap();
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

        let (thumb, hash) = render_thumbnail(&file, 32, None).unwrap();
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
        // 落盘在后台线程，先轮询等它稳定。
        wait_for_files(&cache, 1);
        let files = collect_files(&cache);
        assert!(!files.is_empty(), "缩略图本体应已落盘");
        for file in &files {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            assert!(!name.contains('.'), "缓存文件名不能带扩展名：{name}");
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

    fn set_mtime(path: &Path, when: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// 等后台落盘稳定：缩略图的写盘不在响应链路上，断言前必须轮询。
    fn wait_for_files(cache: &Path, want: usize) -> usize {
        for _ in 0..150 {
            let count = collect_files(cache).len();
            if count >= want {
                return count;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        collect_files(cache).len()
    }

    /// 等后台落盘彻底停下（连续两次采样数量一致），再返回最终文件列表。
    ///
    /// `wait_for_files` 只保证「至少 N 个」，多档位请求可能还在后面补文件，
    /// 所以断言「不该多出文件」时必须再确认一次计数不再变化。
    fn settle_files(cache: &Path) -> Vec<PathBuf> {
        let mut last = collect_files(cache).len();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let now = collect_files(cache).len();
            if now == last {
                break;
            }
            last = now;
        }
        collect_files(cache)
    }

    #[test]
    fn cleanup_evicts_whole_units_oldest_first() {
        let cache = scratch_dir("thumb-cleanup");
        let a = put_unit(
            &cache,
            "aa1",
            &[("v3-1-1-320-jpg", 512), ("v3-1-1-960-webp", 8)],
        );
        let b = put_unit(&cache, "bb2", &[("v3-1-1-320-jpg", 512)]);
        let c = put_unit(&cache, "cc3", &[("v3-1-1-320-jpg", 512)]);
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

    /// 文件名 → 指纹：只认 `{指纹}-{宽度}[p]-{编码}`，别把旧布局或外来文件误判成一组。
    #[test]
    fn fingerprint_parses_cache_names() {
        assert_eq!(
            fingerprint_of("v4-1f-5a-400-webp").as_deref(),
            Some("v4-1f-5a")
        );
        assert_eq!(fingerprint_of("p1-2-3-400p-jpg").as_deref(), Some("p1-2-3"));
        for bad in [
            "0123456789abcdef-320.webp",
            "v4-1f-5a-400-webp.bak",
            "v4-1f-400-webp",
            "320-jpg",
            ".tmp-1-2-3.webp",
            "v4-zz-5a-400-webp",
        ] {
            assert_eq!(fingerprint_of(bad), None, "不该被识别成缓存文件：{bad}");
        }
    }

    /// 升级换了键格式后，活跃单元里的旧组不会随 LRU 淘汰（命中缓存会把 mtime 顶新），
    /// 清理任务必须按「最新指纹组」把存量重复一次性收走。
    #[test]
    fn cleanup_drops_stale_fingerprint_groups() {
        let cache = scratch_dir("thumb-groups");
        let unit = put_unit(&cache, "bb8", &[("v4-1-1-400-jpg", 120)]);
        let old = unit.join("v3-9-9-400-jpg");
        fs::write(&old, vec![0u8; 100]).unwrap();
        set_mtime(&old, SystemTime::now() - Duration::from_secs(3600));

        let freed = cleanup_cache(&cache, 10 * 1024 * 1024, 0).unwrap();
        assert_eq!(freed, 100, "只该释放旧组");
        assert!(!old.exists(), "旧指纹组必须收走");
        assert!(
            unit.join("v4-1-1-400-jpg").exists(),
            "最新组要留着（同一张图的多个宽度同生共死）"
        );
        let _ = fs::remove_dir_all(&cache);
    }

    /// 命中必须**真的**续期（v4.9.17 回归）。
    ///
    /// 旧实现命中时只 `OpenOptions::open()` 不写入 —— 打开文件不会改 mtime，
    /// 于是 `thumb_cache_max_age_days` 退化成「生成后无条件过期」：天天在看的图
    /// 也会在第 90 天整单元被删、下次打开重新解码。这里直接断言 mtime 被顶新，
    /// 并验证续期后的单元不会再被闲置阈值扫掉。
    #[test]
    fn cache_hit_renews_last_access_time() {
        let cache = scratch_dir("thumb-touch");
        let unit = put_unit(&cache, "ac1", &[("v5-1-1-400-jpg", 64)]);
        let file = unit.join("v5-1-1-400-jpg");
        set_mtime(&file, SystemTime::now() - 3 * TOUCH_INTERVAL);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hit = rt.block_on(Thumbnailer::cached(&unit, "v5-1-1", 400));
        assert!(hit.is_some(), "文件名与指纹对得上就该命中");

        let idle = SystemTime::now()
            .duration_since(fs::metadata(&file).unwrap().modified().unwrap())
            .unwrap();
        assert!(
            idle < Duration::from_secs(60),
            "命中后 mtime 必须刷新，实际闲置 {idle:?}"
        );

        // 60 秒闲置阈值：刚看过就不该被淘汰（旧实现这里必然删掉）
        let removed = cleanup_cache(&cache, 10 * 1024 * 1024, 60).unwrap();
        assert_eq!(removed, 0, "刚访问过的单元不能判过期");
        assert!(unit.exists());
        let _ = fs::remove_dir_all(&cache);
    }

    /// 闲置单元照旧被清：续期只对「真的有人看」的单元生效。
    #[test]
    fn cleanup_still_drops_idle_unit_after_others_were_touched() {
        let cache = scratch_dir("thumb-touch-idle");
        let hot = put_unit(&cache, "ad1", &[("v5-1-1-400-jpg", 100)]);
        let cold = put_unit(&cache, "ae1", &[("v5-1-1-400-jpg", 100)]);
        backdate(&cold, Duration::from_secs(3 * 24 * 3600));
        // 热单元刚被写过（put_unit），闲置阈值设为 1 天
        let removed = cleanup_cache(&cache, 10 * 1024 * 1024, 24 * 3600).unwrap();
        assert_eq!(removed, 100, "只有闲置的那个单元该被删");
        assert!(!cold.exists());
        assert!(hot.exists());
        let _ = fs::remove_dir_all(&cache);
    }

    ///
    /// 旧实现只扫顶层与 bucket 层，单元里的残片要等「整个单元被 LRU 淘汰」才顺带删，
    /// 期间还被 `unit_stats` 算进用量。但正在写的那份（新鲜的）绝不能动 ——
    /// 删了 rename 就找不到源文件，那次落盘白做。
    #[test]
    fn cleanup_sweeps_stale_temp_files_in_unit() {
        let cache = scratch_dir("thumb-tmp");
        let unit = put_unit(&cache, "aa7", &[("v4-1-1-320-jpg", 100)]);
        let stale = unit.join(".tmp-1-2-3.webp");
        fs::write(&stale, vec![0u8; 64]).unwrap();
        let handle = fs::File::options().write(true).open(&stale).unwrap();
        handle
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(3600)),
            )
            .unwrap();
        drop(handle);
        let live = unit.join(".tmp-4-5-6.webp");
        fs::write(&live, vec![0u8; 32]).unwrap();

        let removed = cleanup_cache(&cache, 10 * 1024 * 1024, 0).unwrap();
        assert_eq!(removed, 64, "只该释放超龄残片的字节");
        assert!(!stale.exists(), "超龄残片必须收走");
        assert!(live.exists(), "正在写的临时文件不能删");
        assert!(unit.exists(), "单元还在用，不能整目录删");
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
    /// 缓存文件数必须只随「有效宽度种类」增长，绝不随请求次数增长。
    #[test]
    fn repeated_requests_do_not_grow_cache() {
        let dir = scratch_dir("thumb-repeat");
        let source = dir.join("a.png");
        // 源图必须比两档请求都大，否则「有效宽度归并」会把它们合成一份（见上一条测试）
        let mut image = image::RgbaImage::new(400, 300);
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

        // 第一次请求落一个文件；落盘在后台线程（响应不等磁盘），轮询等它稳定
        let (first, _) = rt.block_on(thumbnails.get(&source, 64, None));
        assert!(matches!(first, ThumbOutcome::Ready(_)));
        let after_first = wait_for_files(&cache, 1);
        assert_eq!(
            after_first, 1,
            "一个宽度 = 一个文件（不再有 stamp）：{after_first}"
        );

        // 再取两次：命中缓存，一个文件都不新增
        for _ in 0..2 {
            let _ = rt.block_on(thumbnails.get(&source, 64, None));
        }
        assert_eq!(collect_files(&cache).len(), 1, "命中缓存不能再落文件");

        // 换宽度：只多一个文件（同一单元目录内）
        let (wide, _) = rt.block_on(thumbnails.get(&source, 96, None));
        assert!(matches!(wide, ThumbOutcome::Ready(_)));
        assert_eq!(wait_for_files(&cache, 2), 2);

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

    /// 小图的各档位请求必须归并成**一份**文件。
    ///
    /// 40×30 的图在 64 / 96 / 160 / 320 / 400 各档下有效宽度都是 40，编码又是确定性的
    /// ——升级前会落五份字节完全相同的副本，这是「thumbnails 里一堆重复图」的主因。
    #[test]
    fn small_source_collapses_width_tiers() {
        let dir = scratch_dir("thumb-collapse");
        let source = dir.join("small.png");
        image::RgbaImage::from_pixel(40, 30, image::Rgba([9, 9, 9, 255]))
            .save(&source)
            .unwrap();
        for width in [64, 96, 160, 320, 400] {
            assert_eq!(
                Thumbnailer::effective_width(&source, width),
                40,
                "有效宽度 = min(请求宽度, 源图长边)：w={width}"
            );
        }

        let cache = scratch_dir("thumb-collapse-cache");
        let thumbnails = Thumbnailer::new(cache.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for width in [64u32, 96, 160, 320, 400] {
            let (outcome, _) = rt.block_on(thumbnails.get(&source, width, None));
            assert!(matches!(outcome, ThumbOutcome::Ready(_)), "w={width}");
        }
        wait_for_files(&cache, 1);
        let files = settle_files(&cache);
        assert_eq!(files.len(), 1, "小图的所有档位必须归并成一份：{files:?}");

        // 大图仍按请求宽度分档，别把有用的小尺寸也归并掉
        let big = dir.join("big.png");
        image::RgbaImage::from_pixel(1200, 800, image::Rgba([1, 2, 3, 255]))
            .save(&big)
            .unwrap();
        assert_eq!(Thumbnailer::effective_width(&big, 320), 320);
        assert_eq!(Thumbnailer::effective_width(&big, 400), 400);
        assert_eq!(
            Thumbnailer::effective_width(&big, 4096),
            1024,
            "上限仍是 1024"
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
        let keep_unit = unit_dir(&cache, &keep);
        let gone_unit = unit_dir(&cache, &gone);
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
    fn fail_marker_blocks_until_cleared() {
        // 负缓存（2026-11-04 审查）：失败后 30 分钟内不再重试同一指纹，
        // 生成成功要清掉标记，否则偶发失败会把好图挡半个钟头
        let dir = scratch_dir("thumb-fail-marker");
        let fp = "abc123";
        assert!(!recently_failed(&dir, fp), "没失败过不能拦截");
        mark_failed(&dir, fp);
        assert!(recently_failed(&dir, fp), "失败标记必须生效");
        // 指纹含 mtime+size：内容一变就是新指纹，旧标记不波及
        assert!(!recently_failed(&dir, "other-fp"));
        clear_failed(&dir, fp);
        assert!(!recently_failed(&dir, fp), "清除后必须放行");
        let _ = fs::remove_dir_all(&dir);
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
