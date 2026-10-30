//! 媒体库接口（设计文档 3.4）：缩略图 + 媒体索引。
//!
//! 媒体索引只做「按目录扫描 + 分类」，不做刮削、不联网。
//! 视频封面可选：配置了外部 FFmpeg 才截首帧，否则回占位图。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::core::ffmpeg;
use crate::core::fs::{self, FsError};
use crate::core::thumbnail::{PLACEHOLDER_SVG, ThumbOutcome};

/// 单页条目数（游标分页，设计文档 3.4）：只影响「一次返回多少条」，
/// 不再有「扫到上限就截断、后面的永远看不到」这回事。
const DEFAULT_PAGE: usize = 300;
const MAX_PAGE: usize = 1000;
/// 默认扫描深度
const DEFAULT_DEPTH: usize = 2;

#[derive(Debug, Deserialize)]
pub struct ThumbQuery {
    pub path: String,
    pub w: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    pub path: Option<String>,
    /// image / video / audio / media（默认全部）
    pub kind: Option<String>,
    pub depth: Option<usize>,
    /// 单页条数（默认 300，上限 1000）
    pub limit: Option<usize>,
    /// 上一页返回的 `next_cursor`；为空即第一页
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaItem {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: Option<u64>,
    pub kind: &'static str,
    pub ext: String,
    /// 所在目录（相册聚合用）
    pub folder: String,
    /// 音频内嵌 ID3 标题/艺术家/专辑（其它类型为 null）
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// 是否有内嵌封面（`APIC`/`PIC`），为 true 时前端才去拉 `/api/media/cover`
    pub cover: bool,
    /// 主色调占位（ThumbHash，base64）；缩略图还没生成过时为 null
    pub hash: Option<String>,
    /// 由 ThumbHash 解出的平均色（#rrggbb），前端做模糊/纯色占位
    pub color: Option<String>,
    /// 同目录同名字幕（.vtt / .srt）的路径，没有则为 null
    pub subtitle: Option<String>,
}

/// 各类型总数：一键切换分区时不必等全部翻完就知道「库里到底有多少」
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct KindCounts {
    pub image: usize,
    pub video: usize,
    pub audio: usize,
}

#[derive(Debug, Serialize)]
pub struct BrowseResult {
    pub root: String,
    pub items: Vec<MediaItem>,
    /// 本次真正 readdir 到的目录项数（诊断用，前端不展示）
    pub scanned: usize,
    /// 命中 `kind` 过滤的总条数（不是本页条数）
    pub total: usize,
    pub counts: KindCounts,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 游标：只记上一页最后一条的排序键（mtime 降序 + 展示路径升序）。
///
/// 刻意不用 offset —— 扫描期间新增/删除文件不会让下一页重复或漏项。
/// 和文件列表（`core::fs`）同一套口径：稳定排序 + keyset。
#[derive(Debug, Serialize, Deserialize)]
struct MediaCursor {
    m: u64,
    p: String,
}

fn encode_cursor(mtime: u64, display: &str) -> String {
    let cursor = MediaCursor {
        m: mtime,
        p: display.to_string(),
    };
    let json = serde_json::to_vec(&cursor).unwrap_or_default();
    data_encoding::BASE64URL_NOPAD.encode(&json)
}

fn decode_cursor(raw: &str) -> Option<MediaCursor> {
    let bytes = data_encoding::BASE64URL_NOPAD
        .decode(raw.trim().as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 游标之后的起始下标（`raw` 必须已按 mtime 降序 + 路径升序排好）
fn page_start(raw: &[RawMedia], cursor: Option<&MediaCursor>) -> usize {
    match cursor {
        Some(cur) => raw.partition_point(|item| {
            item.mtime > cur.m || (item.mtime == cur.m && item.display.as_str() <= cur.p.as_str())
        }),
        None => 0,
    }
}

#[derive(Debug, Deserialize)]
pub struct AlbumsQuery {
    pub path: Option<String>,
    /// 目录树向下钻取的层数（每一层里「直接含媒体文件」的目录都算一本相册）
    pub depth: Option<usize>,
    /// image / video / audio（默认 image）——影视库与音乐库的「按文件夹」视图共用本接口
    pub kind: Option<String>,
    /// 单页相册数（默认 300，上限 1000）
    pub limit: Option<usize>,
    /// 上一页返回的 `next_cursor`
    pub cursor: Option<String>,
}

/// 相册清单分页结果（与媒体列表同一套游标口径）
#[derive(Debug, Serialize)]
pub struct AlbumsResult {
    pub root: String,
    pub items: Vec<Album>,
    /// 命中过滤的相册总数（不是本页条数）
    pub total: usize,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// 相册（媒体库「按文件夹」视图的卡片，设计文档 3.4.3）。
///
/// 与 v2.8 的相册视图同一口径：**有媒体文件的目录就是一本相册**，
/// 挂载目录本身也算（根目录里直接放的照片不该凭空消失）。
/// `count == 0` 的目录绝不出现在结果里 —— 空文件夹就是空壳卡片。
/// 图片库/影视库/音乐库共用本结构（`/api/media/albums?kind=`）。
#[derive(Debug, Clone, Serialize)]
pub struct Album {
    pub name: String,
    pub path: String,
    /// 该目录**第一层**的图片数量（与点进去看到的条数一致）
    pub count: usize,
    /// 是否就是扫描起点（挂载目录）——前端据此显示「根目录」之类的标签
    pub is_root: bool,
    /// 封面 = 该相册里**最新**的一张图片
    pub cover: Option<String>,
    /// 封面的 ThumbHash 与平均色（前端模糊打底，避免整块跳色）
    pub cover_hash: Option<String>,
    pub cover_color: Option<String>,
    /// 相册内最新一张图片的修改时间（排序用）
    pub modified: Option<u64>,
}

fn placeholder() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PLACEHOLDER_SVG,
    )
        .into_response()
}

fn fs_error(err: FsError) -> Response {
    crate::api::error::fs_error(&err)
}

async fn roots_for(state: &AppState) -> crate::core::fs::RootSet {
    let config = state.config.read().await;
    fs::load_roots(&config, &state.cwd)
}

/// 路径所属挂载点的 id（收藏 / 播放历史的分区键，设计文档 4.1）。
///
/// 找不到归属时回空串 —— 那代表「进程 cwd 兜底」的隐式根，也是老库迁移过来的
/// 记录的取值，两者必须一致，否则同一个路径会分裂成两条记录。
async fn mount_of(state: &AppState, display: &str) -> String {
    let config = state.config.read().await;
    crate::core::path_guard::load(&config, &state.cwd)
        .find_by_path_text(display)
        .map(|mount| mount.info.id.clone())
        .unwrap_or_default()
}

async fn resolve_default(state: &AppState, requested: Option<&str>) -> Result<PathBuf, FsError> {
    let roots = roots_for(state).await;
    match requested.map(str::trim).filter(|p| !p.is_empty()) {
        Some(raw) => fs::resolve_existing(raw, &roots.allowed, roots.follow_symlinks),
        None => {
            let Some(root) = roots.default_root() else {
                return Err(FsError::NotFound("没有可用的挂载点".to_string()));
            };
            let path = root.path.clone();
            fs::resolve_existing(&path, &roots.allowed, roots.follow_symlinks)
        }
    }
}

/// GET /api/files/thumb?path=…&w=320
pub async fn thumb(State(state): State<AppState>, Query(query): Query<ThumbQuery>) -> Response {
    let path = match resolve_default(&state, Some(&query.path)).await {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    // .asmoday-part 临时文件不可预览（设计文档 3.2.6.4，v3.4.0）：直接回占位图
    if crate::core::path_guard::is_part_path(&path) {
        return placeholder();
    }

    if path.is_dir() {
        return placeholder();
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = fs::extension_of_name(&name);
    let (kind, _) = fs::classify_name(&name, &ext, false);

    let width = query.w.unwrap_or(if kind == "video" { 480 } else { 320 });

    // 一次解析复用：图片缩略图用它编码 WebP（3.4），视频封面用它截首帧
    let ffmpeg_bin = {
        let configured = {
            let config = state.config.read().await;
            config.media.ffmpeg_path.clone()
        };
        ffmpeg::resolve(&configured)
            .path
            .filter(|bin| bin.is_file())
    };

    let (outcome, fresh) = match kind {
        "image" => {
            state
                .thumbnails
                .get(&path, width, ffmpeg_bin.as_deref())
                .await
        }
        "video" => {
            // 影视库封面：靠外部可选 FFmpeg 截首帧（设计文档 3.4）；
            // 未配置或截帧失败一律回占位图，绝不阻塞列表渲染
            let outcome = match ffmpeg_bin {
                Some(bin) => state.thumbnails.video_poster(&path, &bin, width).await,
                None => ThumbOutcome::Placeholder,
            };
            (outcome, None)
        }
        // 音频/其它：没有可截的帧，统一占位（设计文档 8.3）
        _ => (ThumbOutcome::Placeholder, None),
    };

    // 新鲜 ThumbHash 入库（v2.8.2：SQLite，不再落 .th 旁路文件）
    if let Some(fresh) = fresh {
        state.auth.thumb_hash_set(&fresh.key, &fresh.value);
    }

    match outcome {
        ThumbOutcome::Ready(rendered) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, rendered.mime),
                (header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            Body::from(rendered.bytes),
        )
            .into_response(),
        ThumbOutcome::Placeholder => placeholder(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CoverQuery {
    pub path: String,
}

/// GET /api/media/cover?path=…：音频内嵌封面（设计文档 3.4 音乐库）。
///
/// 直接吐原图字节，不做二次编码；没有封面回 404，前端按 `cover` 标记本来就不会请求。
pub async fn cover(State(state): State<AppState>, Query(query): Query<CoverQuery>) -> Response {
    let path = match resolve_default(&state, Some(&query.path)).await {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };

    let read =
        tokio::task::spawn_blocking(move || crate::core::audio_tags::read_cover(&path)).await;
    match read {
        Ok(Some(cover)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, cover.mime.as_str()),
                (header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            Body::from(cover.data),
        )
            .into_response(),
        _ => crate::api::error::api_error(
            StatusCode::NOT_FOUND,
            "COVER_NOT_FOUND",
            "this track has no embedded cover",
        ),
    }
}

/// GET /api/media/browse?path=…&kind=image&depth=2
pub async fn browse(State(state): State<AppState>, Query(query): Query<BrowseQuery>) -> Response {
    let root = match resolve_default(&state, query.path.as_deref()).await {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    if !root.is_dir() {
        return fs_error(FsError::BadRequest("目标不是目录".to_string()));
    }

    let wanted: Vec<&'static str> = match query.kind.as_deref() {
        Some("image") => vec!["image"],
        Some("video") => vec!["video"],
        Some("audio") => vec!["audio"],
        Some("media") | None => vec!["image", "video", "audio"],
        Some(other) => {
            return fs_error(FsError::BadRequest(format!("不支持的 kind：{other}")));
        }
    };
    // depth=0 合法：只扫起点目录第一层（相册详情用，卡片条数必须与看到的一致）
    let depth = query.depth.unwrap_or(DEFAULT_DEPTH).clamp(0, 4);
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(decode_cursor);

    let scan_root = root.clone();
    let auth_store = Arc::clone(&state.auth);
    let outcome = tokio::task::spawn_blocking(move || {
        let (raw, scanned, counts) = scan_raw(&scan_root, &wanted, depth);
        // 稳定排序：mtime 降序（新的在前），同 mtime 用展示路径兜底，保证翻页不重不漏
        let mut raw = raw;
        raw.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| a.display.cmp(&b.display))
        });

        let total = raw.len();
        let start = page_start(&raw, cursor.as_ref());
        let end = start.saturating_add(limit).min(total);
        let has_more = end < total;
        // 只对当前页做重活（ID3 解析、字幕探测、ThumbHash 查询）
        let items = build_items(&raw[start.min(total)..end], &auth_store);
        let next_cursor = if has_more && end > start {
            Some(encode_cursor(raw[end - 1].mtime, &raw[end - 1].display))
        } else {
            None
        };
        (items, scanned, total, counts, has_more, next_cursor)
    })
    .await;

    match outcome {
        Ok((items, scanned, total, counts, has_more, next_cursor)) => Json(BrowseResult {
            root: fs::normalize_display(&root),
            items,
            scanned,
            total,
            counts,
            has_more,
            next_cursor,
        })
        .into_response(),
        Err(err) => fs_error(FsError::Io(format!("扫描媒体目录失败：{err}"))),
    }
}

/// GET /api/media/albums?path=…&depth=3
///
/// 照片库「相册视图」：回「直接含有图片的目录」——**含挂载目录本身**（v2.8 口径），
/// 剔除一张图片都没有的目录；每条记录的数量只数该目录第一层，与点进去看到的条数一致。
/// 封面必须取相册里最新的一张图（设计文档 3.4.3），所以遍历时记下最大 mtime。
pub async fn albums(State(state): State<AppState>, Query(query): Query<AlbumsQuery>) -> Response {
    let root = match resolve_default(&state, query.path.as_deref()).await {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    if !root.is_dir() {
        return fs_error(FsError::BadRequest("目标不是目录".to_string()));
    }

    let depth = query.depth.unwrap_or(3).clamp(1, 4);
    let wanted: Vec<&'static str> = match query.kind.as_deref() {
        Some("image") => vec!["image"],
        Some("video") => vec!["video"],
        Some("audio") => vec!["audio"],
        Some(other) => return fs_error(FsError::BadRequest(format!("不支持的 kind：{other}"))),
        None => vec!["image"],
    };
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(decode_cursor);

    let scan_root = root.clone();
    let auth_store = Arc::clone(&state.auth);
    let outcome = tokio::task::spawn_blocking(move || {
        let mut raw = scan_albums(&scan_root, depth, &wanted);
        // 新的在前；同 mtime 用路径兜底，保证翻页稳定
        raw.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        let total = raw.len();
        let start = match &cursor {
            Some(cur) => raw.partition_point(|item| {
                item.mtime > cur.m || (item.mtime == cur.m && item.path <= cur.p)
            }),
            None => 0,
        };
        let end = start.saturating_add(limit).min(total);
        let has_more = end < total;
        let items = build_albums(&raw[start.min(total)..end], &auth_store);
        let next_cursor = if has_more && end > start {
            Some(encode_cursor(raw[end - 1].mtime, &raw[end - 1].path))
        } else {
            None
        };
        (items, total, has_more, next_cursor)
    })
    .await;

    match outcome {
        Ok((items, total, has_more, next_cursor)) => Json(AlbumsResult {
            root: fs::normalize_display(&root),
            items,
            total,
            has_more,
            next_cursor,
        })
        .into_response(),
        Err(err) => fs_error(FsError::Io(format!("扫描相册失败：{err}"))),
    }
}

/// 扫描时要跳过的目录名：应用自身的数据/构建产物不算媒体库
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "data",
    "dist",
    "target",
    ".shots",
    "thumbnails",
];

/// 找同目录同名外挂字幕：优先 .vtt，其次 .srt
fn find_subtitle(video: &Path) -> Option<String> {
    for ext in ["vtt", "srt"] {
        let candidate = video.with_extension(ext);
        if candidate.is_file() {
            return Some(fs::normalize_display(&candidate));
        }
    }
    None
}

/// ThumbHash → 平均色（#rrggbb）。解码失败就当没有，前端回退纯色。
fn average_color(hash_b64: &str) -> Option<String> {
    let bytes = data_encoding::BASE64
        .decode(hash_b64.trim().as_bytes())
        .ok()?;
    let (r, g, b, _) = thumbhash::thumb_hash_to_average_rgba(&bytes).ok()?;
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(r),
        channel(g),
        channel(b)
    ))
}

/// 扫描阶段只保留「排序 + 分类」必需的最小字段。
///
/// ID3 标签、外挂字幕、ThumbHash 都推迟到分页切片之后再算：这几件事原来对目录里
/// 每一条都执行，几千首歌的库点开一次要开几千个文件句柄，首屏全卡在这里。
struct RawMedia {
    name: String,
    path: PathBuf,
    display: String,
    folder: String,
    size: u64,
    /// 归一化后的 mtime（缺失记 0，降序时自然沉底）
    mtime: u64,
    kind: &'static str,
    ext: String,
}

/// 走一遍目录树，只做 readdir + 分类 + 一次 metadata。
///
/// **没有扫描上限**：媒体库和文件管理一样，必须能看到最后一条。`counts` 统计的是
/// 全部媒体文件（不受 `wanted` 限制），前端据此显示「库里到底有多少」。
fn scan_raw(root: &Path, wanted: &[&str], depth: usize) -> (Vec<RawMedia>, usize, KindCounts) {
    let mut out = Vec::new();
    let mut counts = KindCounts::default();
    let mut scanned = 0usize;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, level)) = queue.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            scanned += 1;
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();

            // 系统垃圾（Thumbs.db / .DS_Store / ._* / ~$*）一律不进媒体库，
            // 否则相册里会混进一堆打不开的元数据文件（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }

            if file_type.is_dir() {
                // 应用数据目录（缩略图缓存就在里面）绝不进媒体库：
                // 缓存里的图会被再缩略图一遍，产物写回同一目录，越扫越多。
                if level < depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
                    && !crate::core::reserved::is_reserved_lexical(&path)
                {
                    queue.push((path, level + 1));
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let ext = fs::extension_of_name(&name);
            // 图标类 SVG 不算「照片」
            if ext == "svg" {
                continue;
            }
            let (kind, _) = fs::classify_name(&name, &ext, false);
            match kind {
                "image" => counts.image += 1,
                "video" => counts.video += 1,
                "audio" => counts.audio += 1,
                _ => {}
            }
            if !wanted.contains(&kind) {
                continue;
            }

            let meta = entry.metadata().ok();
            out.push(RawMedia {
                name,
                display: fs::normalize_display(&path),
                folder: fs::normalize_display(&dir),
                path,
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                mtime: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                kind,
                ext,
            });
        }
    }

    (out, scanned, counts)
}

/// 给当前页的条目补上「贵」的字段：音频 ID3、视频外挂字幕、图片 ThumbHash。
fn build_items(raw: &[RawMedia], auth: &crate::core::auth::AuthStore) -> Vec<MediaItem> {
    let mut items: Vec<MediaItem> = Vec::with_capacity(raw.len());
    let mut hash_slots: Vec<(usize, &Path)> = Vec::new();

    for entry in raw {
        let tags = if entry.kind == "audio" {
            crate::core::audio_tags::read_tags(&entry.path)
        } else {
            Default::default()
        };
        let subtitle = if entry.kind == "video" {
            find_subtitle(&entry.path)
        } else {
            None
        };
        if entry.kind == "image" {
            hash_slots.push((items.len(), entry.path.as_path()));
        }
        items.push(MediaItem {
            name: entry.name.clone(),
            path: entry.display.clone(),
            size: entry.size,
            modified: (entry.mtime > 0).then_some(entry.mtime),
            kind: entry.kind,
            ext: entry.ext.clone(),
            folder: entry.folder.clone(),
            title: tags.title,
            artist: tags.artist,
            album: tags.album,
            cover: tags.has_cover,
            hash: None,
            color: None,
            subtitle,
        });
    }

    // 批量取 ThumbHash：一次连接逐键查 SQLite（v2.8.2 起只存库，不落旁路文件）
    if !hash_slots.is_empty() {
        let keys: Vec<String> = hash_slots
            .iter()
            .map(|(_, path)| crate::core::thumbnail::Thumbnailer::hash_key(path))
            .collect();
        let hits = auth.thumb_hashes_get_many(&keys);
        for ((index, _), key) in hash_slots.iter().zip(keys.iter()) {
            if let Some(hash) = hits.get(key)
                && let Some(item) = items.get_mut(*index)
            {
                item.hash = Some(hash.clone());
                item.color = average_color(hash);
            }
        }
    }

    items
}

// ---------------------------------------------------------------------------
// 照片库相册视图（设计文档 3.4.3）
// ---------------------------------------------------------------------------

/// 相册扫描结果：只留卡片必需的字段，封面 ThumbHash 推迟到分页切片之后。
struct RawAlbum {
    name: String,
    /// 展示路径（同时是游标里的次级排序键）
    path: String,
    count: usize,
    is_root: bool,
    /// 封面真实路径（算 ThumbHash 用；展示串已换成 `/` 分隔，拿它算 key 会与缩略图不一致）
    cover_path: Option<PathBuf>,
    /// 归一化后的 mtime（缺失记 0，降序时沉底）
    mtime: u64,
}

fn scan_albums(root: &Path, depth: usize, wanted: &[&str]) -> Vec<RawAlbum> {
    let mut albums: Vec<RawAlbum> = Vec::new();

    // 与 v2.8 相册视图同一口径（设计文档 3.4.3）：遍历目录树，**每个直接含有
    // 图片的目录**都记一本相册——包括扫描起点（挂载目录）自己；计数只看该目录
    // 第一层，点进相册也只列第一层，卡片上的条数才和看到的一致。
    //
    // 没有扫描上限：相册多到几万本也必须能翻到最后一张卡片。
    let mut stack: Vec<(PathBuf, usize, bool)> = vec![(root.to_path_buf(), 0, true)];

    while let Some((dir, level, is_root)) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut count = 0usize;
        // (mtime, path)：相册封面取最新的一张图
        let mut newest: Option<(u64, PathBuf)> = None;

        for entry in reader.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            // 系统垃圾（Thumbs.db / .DS_Store / ._*）一律不进媒体库（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }

            if file_type.is_dir() {
                // 同上：数据目录不是相册
                if level < depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
                    && !crate::core::reserved::is_reserved_lexical(&entry.path())
                {
                    stack.push((entry.path(), level + 1, false));
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let ext = fs::extension_of_name(&name);
            // 图标类 SVG 不算照片
            if ext == "svg" {
                continue;
            }
            let (kind, _) = fs::classify_name(&name, &ext, false);
            if !wanted.contains(&kind) {
                continue;
            }
            count += 1;
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|delta| delta.as_secs())
                .unwrap_or(0);
            if newest
                .as_ref()
                .map(|(best, _)| mtime > *best)
                .unwrap_or(true)
            {
                newest = Some((mtime, entry.path()));
            }
        }

        // 一个媒体文件都没有的目录绝不上架（设计文档 3.4.3）
        if count == 0 {
            continue;
        }

        let name = dir
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| fs::normalize_display(&dir));
        let (mtime, cover_path) = match newest {
            Some((mtime, path)) => (Some(mtime), Some(path)),
            None => (None, None),
        };
        albums.push(RawAlbum {
            name,
            path: fs::normalize_display(&dir),
            count,
            is_root,
            cover_path,
            mtime: mtime.unwrap_or(0),
        });
    }

    albums
}

/// 给当前页的相册补封面 ThumbHash（一次批量查询，不逐本开连接）。
fn build_albums(raw: &[RawAlbum], auth: &crate::core::auth::AuthStore) -> Vec<Album> {
    let mut albums: Vec<Album> = raw
        .iter()
        .map(|entry| Album {
            name: entry.name.clone(),
            path: entry.path.clone(),
            count: entry.count,
            is_root: entry.is_root,
            cover: entry
                .cover_path
                .as_ref()
                .map(|path| fs::normalize_display(path)),
            cover_hash: None,
            cover_color: None,
            modified: (entry.mtime > 0).then_some(entry.mtime),
        })
        .collect();

    let lookup: Vec<(usize, &PathBuf)> = raw
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.cover_path.as_ref().map(|path| (index, path)))
        .collect();
    if lookup.is_empty() {
        return albums;
    }

    let keys: Vec<String> = lookup
        .iter()
        .map(|(_, path)| crate::core::thumbnail::Thumbnailer::hash_key(path))
        .collect();
    let hits = auth.thumb_hashes_get_many(&keys);
    for ((index, _), key) in lookup.iter().zip(keys.iter()) {
        if let Some(hash) = hits.get(key)
            && let Some(album) = albums.get_mut(*index)
        {
            album.cover_hash = Some(hash.clone());
            album.cover_color = average_color(hash);
        }
    }
    albums
}

// ---------------------------------------------------------------------------
// 播放进度 / 收藏（favorites 与 play_history 表，设计文档 4.1）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProgressBody {
    pub path: String,
    /// 播放进度（秒）
    pub progress: i64,
}

#[derive(Debug, Deserialize)]
pub struct FavoriteBody {
    pub path: String,
    #[serde(default)]
    pub favorite: bool,
}

/// POST /api/media/progress：记录/刷新播放进度
pub async fn progress(State(state): State<AppState>, Json(body): Json<ProgressBody>) -> Response {
    let roots = roots_for(&state).await;
    let target = match fs::resolve_existing(&body.path, &roots.allowed, roots.follow_symlinks) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    let display = fs::normalize_display(&target);
    let mount_id = mount_of(&state, &display).await;

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let result = conn.execute(
        "INSERT INTO play_history (mount_id, file_path, last_played_at, progress)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(mount_id, file_path) DO UPDATE SET last_played_at = ?3, progress = ?4",
        rusqlite::params![
            mount_id,
            display,
            crate::core::auth::now_secs(),
            body.progress.max(0)
        ],
    );
    match result {
        Ok(_) => Json(json!({ "status": "ok" })).into_response(),
        Err(err) => fs_error(FsError::Io(format!("写入播放进度失败：{err}"))),
    }
}

/// GET /api/media/history：播放历史（最近 200 条）
pub async fn history(State(state): State<AppState>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let mut statement = match conn.prepare(
        "SELECT mount_id, file_path, last_played_at, progress FROM play_history
         ORDER BY last_played_at DESC LIMIT 200",
    ) {
        Ok(statement) => statement,
        Err(err) => return fs_error(FsError::Io(format!("查询播放历史失败：{err}"))),
    };
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "mount_id": row.get::<_, String>(0)?,
            "path": row.get::<_, String>(1)?,
            "last_played_at": row.get::<_, Option<i64>>(2)?,
            "progress": row.get::<_, Option<i64>>(3)?,
        }))
    });
    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() }))
            .into_response(),
        Err(err) => fs_error(FsError::Io(format!("查询播放历史失败：{err}"))),
    }
}

/// POST /api/media/favorite：收藏 / 取消收藏
pub async fn favorite(State(state): State<AppState>, Json(body): Json<FavoriteBody>) -> Response {
    let roots = roots_for(&state).await;
    let target = match fs::resolve_existing(&body.path, &roots.allowed, roots.follow_symlinks) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    let display = fs::normalize_display(&target);
    let mount_id = mount_of(&state, &display).await;

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let result = if body.favorite {
        conn.execute(
            "INSERT OR IGNORE INTO favorites (mount_id, file_path, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![mount_id, display, crate::core::auth::now_secs()],
        )
    } else {
        conn.execute(
            "DELETE FROM favorites WHERE mount_id = ?1 AND file_path = ?2",
            rusqlite::params![mount_id, display],
        )
    };
    match result {
        Ok(_) => Json(json!({ "status": "ok", "favorite": body.favorite })).into_response(),
        Err(err) => fs_error(FsError::Io(format!("更新收藏失败：{err}"))),
    }
}

/// GET /api/media/favorites：收藏清单
pub async fn favorites(State(state): State<AppState>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let mut statement = match conn
        .prepare("SELECT mount_id, file_path, created_at FROM favorites ORDER BY created_at DESC")
    {
        Ok(statement) => statement,
        Err(err) => return fs_error(FsError::Io(format!("查询收藏失败：{err}"))),
    };
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "mount_id": row.get::<_, String>(0)?,
            "path": row.get::<_, String>(1)?,
            "created_at": row.get::<_, Option<i64>>(2)?,
        }))
    });
    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() }))
            .into_response(),
        Err(err) => fs_error(FsError::Io(format!("查询收藏失败：{err}"))),
    }
}

// ---------------------------------------------------------------------------
// 缩略图缓存管理（设计文档 3.4：设置页「清理媒体缓存」+ 容量可视化）
// ---------------------------------------------------------------------------

/// GET /api/media/cache：缓存占用与当前策略
pub async fn cache_stats(State(state): State<AppState>) -> Response {
    let dir = state.thumbnails.cache_dir().to_path_buf();
    // 新版缓存是 `<sha1[..2]>/<sha1>/<width>-<codec>`，必须递归统计
    let (files, bytes) =
        tokio::task::spawn_blocking(move || crate::core::thumbnail::cache_usage(&dir))
            .await
            .unwrap_or((0, 0));

    let config = state.config.read().await;
    Json(json!({
        "files": files,
        "bytes": bytes,
        "limit_mb": config.media.thumb_cache_max_size_mb,
        "max_age_days": config.media.thumb_cache_max_age_days,
        "dir": fs::normalize_display(state.thumbnails.cache_dir()),
    }))
    .into_response()
}

/// POST /api/media/cache/clear：清空缩略图缓存（源文件不受影响）
pub async fn clear_cache(State(state): State<AppState>) -> Response {
    let thumbnails = Arc::clone(&state.thumbnails);
    let result = tokio::task::spawn_blocking(move || thumbnails.clear()).await;
    // 缓存图都删了，ThumbHash 色值也没意义了，一并清表（下次生成会重建）
    state.auth.thumb_hashes_clear();
    match result {
        Ok(Ok((files, bytes))) => {
            Json(json!({ "status": "ok", "files": files, "bytes": bytes })).into_response()
        }
        Ok(Err(err)) => fs_error(FsError::Io(format!("清理缓存失败：{err}"))),
        Err(err) => fs_error(FsError::Io(format!("清理任务失败：{err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asmoday-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造文件并给一个可预测的 mtime（秒），方便断言排序
    fn touch(dir: &Path, name: &str, mtime: u64) {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
    }

    #[test]
    fn scan_has_no_upper_limit_and_counts_every_kind() {
        let root = scratch("count");
        // 5000 张图，远超旧实现的 4000 上限
        for index in 0..5000u64 {
            touch(&root, &format!("p{index:05}.jpg"), 1_700_000_000 + index);
        }
        touch(&root, "clip.mp4", 1_700_000_000);
        touch(&root, "song.mp3", 1_700_000_000);

        let (raw, _scanned, counts) = scan_raw(&root, &["image"], 0);
        assert_eq!(raw.len(), 5000, "旧实现到 4000 就截断，现在必须全都在");
        // counts 统计全部媒体文件，与 wanted 无关
        assert_eq!(counts.image, 5000);
        assert_eq!(counts.video, 1);
        assert_eq!(counts.audio, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_paging_covers_every_item_exactly_once() {
        let root = scratch("paging");
        for index in 0..17u64 {
            touch(&root, &format!("p{index:02}.jpg"), 1_700_000_000 + index);
        }

        let (mut raw, _, _) = scan_raw(&root, &["image"], 0);
        raw.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| a.display.cmp(&b.display))
        });
        let expected: Vec<String> = raw.iter().map(|item| item.display.clone()).collect();
        // 新的在前
        assert!(expected[0].ends_with("p16.jpg"));

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        let mut rounds = 0;
        loop {
            rounds += 1;
            assert!(rounds < 20, "分页没有收敛");
            let start = page_start(&raw, cursor.as_ref());
            let end = (start + 5).min(raw.len());
            for item in &raw[start..end] {
                seen.push(item.display.clone());
            }
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                m: raw[end - 1].mtime,
                p: raw[end - 1].display.clone(),
            });
        }

        assert_eq!(seen, expected);
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "翻页出现重复条目");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_with_same_mtime_still_converges() {
        let root = scratch("same-mtime");
        // 同一秒内写入的一批文件（真实目录里非常常见），靠路径兜底也必须有确定顺序
        for index in 0..9u64 {
            touch(&root, &format!("same{index}.jpg"), 1_700_000_000);
        }
        let (mut raw, _, _) = scan_raw(&root, &["image"], 0);
        raw.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| a.display.cmp(&b.display))
        });

        let mut seen = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        loop {
            let start = page_start(&raw, cursor.as_ref());
            let end = (start + 4).min(raw.len());
            seen.extend(raw[start..end].iter().map(|item| item.display.clone()));
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                m: raw[end - 1].mtime,
                p: raw[end - 1].display.clone(),
            });
        }
        assert_eq!(seen.len(), 9, "mtime 相同也不能漏项或死循环");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_decodes_what_encode_produced() {
        let raw = encode_cursor(1_700_000_123, "D:/media/a.jpg");
        let cursor = decode_cursor(&raw).expect("游标必须能解回来");
        assert_eq!(cursor.m, 1_700_000_123);
        assert_eq!(cursor.p, "D:/media/a.jpg");
        // 手工改坏的游标当第一页处理，不报错
        assert!(decode_cursor("not-a-cursor").is_none());
    }

    #[test]
    fn albums_are_paged_and_skip_empty_dirs() {
        let root = scratch("albums");
        for (index, name) in ["a", "b", "c"].iter().enumerate() {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            touch(&dir, "1.jpg", 1_700_000_000 + index as u64);
        }
        // 空目录不是相册
        std::fs::create_dir_all(root.join("empty")).unwrap();
        // 数据目录（SKIP_DIRS）里的图不算
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        touch(&data, "cache.jpg", 1_700_000_009);

        let mut raw = scan_albums(&root, 3, &["image"]);
        raw.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        assert_eq!(raw.len(), 3, "只该有 a/b/c 三本，空目录与数据目录都不算");

        let mut seen = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        loop {
            let start = match &cursor {
                Some(cur) => raw.partition_point(|item| {
                    item.mtime > cur.m || (item.mtime == cur.m && item.path <= cur.p)
                }),
                None => 0,
            };
            let end = (start + 2).min(raw.len());
            seen.extend(raw[start..end].iter().map(|item| item.path.clone()));
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                m: raw[end - 1].mtime,
                p: raw[end - 1].path.clone(),
            });
        }
        assert_eq!(seen.len(), 3);

        let _ = std::fs::remove_dir_all(&root);
    }
}
