//! 媒体库接口（设计文档 3.4）：缩略图 + 媒体索引。
//!
//! 媒体索引只做「按目录扫描 + 分类」，不做刮削、不联网。
//! 视频封面可选：配置了外部 FFmpeg 才截首帧，否则回占位图。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::core::ffmpeg;
use crate::core::fs::{self, FsError};
use crate::core::media_index::{self, MediaIndex, Scope};
use crate::core::thumbnail::{PLACEHOLDER_SVG, ThumbOutcome, Thumbnailer};

/// 单页条目数（游标分页，设计文档 3.4）：只影响「一次返回多少条」，
/// 不再有「扫到上限就截断、后面的永远看不到」这回事。
// v4.6.23：首屏 120 条（前端也传 120）。300 条要等一次完整的 scan + 300 个格子
// 的宽高计算，NAS 上首屏会明显发木；120 条足够铺满一屏，其余触底续接。
const DEFAULT_PAGE: usize = 120;
const MAX_PAGE: usize = 1000;
/// 默认扫描深度
const DEFAULT_DEPTH: usize = 2;

#[derive(Debug, Deserialize)]
pub struct ThumbQuery {
    pub path: String,
    pub w: Option<u32>,
    /// 前端带回来的**内容指纹**（= 文件 `modified`）。对上才允许长缓存，见 `thumb`。
    pub v: Option<String>,
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
    /// 强制穿透目录快照重扫（前端「重新扫描」按钮；默认走缓存）
    #[serde(default, deserialize_with = "crate::api::boolish")]
    pub refresh: Option<bool>,
    /// 带上图片真实像素宽高（v4.6.19）：等高行布局要按真实宽高比排，
    /// 只有时间线这种「按比例铺」的视图才需要，其它调用省下这批文件头读取。
    #[serde(default, deserialize_with = "crate::api::boolish")]
    pub dims: Option<bool>,
    /// 排序：mtime_desc（默认）/ mtime_asc / name_asc / name_desc / size_desc / size_asc
    ///
    /// `count_*` 只有相册墙认（媒体列表里「一本册子有多少条」没有意义）。
    pub sort: Option<String>,
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
    /// 图片真实像素宽高（v4.6.19，仅 `dims=1` 时返回）：时间线的等高行要按真实
    /// 宽高比算格子宽度。ThumbHash 里也存了比例，但只量化到 3/5/7 几档，铺出来会失真。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
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

/// 游标：只记上一页最后一条的排序主键 + 展示路径（keyset，不用 offset）。
///
/// v4.9.0 起主键是**字符串**：时间/体积是十进制数字串，名称是自然序键。
/// 老游标（v4.8.x）只有一个数字 `m`，继续认 —— 否则用户翻页途中刷新页面
/// 会莫名其妙回到第一页。
#[derive(Debug, Serialize, Deserialize)]
struct MediaCursor {
    /// 排序主键
    #[serde(default)]
    k: String,
    /// 兼容字段：v4.8.x 游标里的 mtime
    #[serde(default, skip_serializing)]
    m: Option<u64>,
    /// 次级键：展示路径（唯一，兜底保证顺序稳定）
    p: String,
}

impl MediaCursor {
    /// 主键原文；老游标回退成 mtime 的十进制串
    fn key(&self) -> Option<String> {
        if !self.k.is_empty() {
            return Some(self.k.clone());
        }
        self.m.map(|mtime| mtime.to_string())
    }
}

fn encode_cursor(key: &str, display: &str) -> String {
    let cursor = MediaCursor {
        k: key.to_string(),
        m: None,
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

/// 把游标规整成 `(主键, 路径)`；主键形态与排序维度不匹配就当没传（回第一页）。
///
/// 宁可重来一页，也不要把一个解析不出的键塞进 SQL —— 那会悄悄返回空页，
/// 用户看到的是「翻页翻丢了」，比回到第一页难排查得多。
fn cursor_key(sort: media_index::Sort, cursor: Option<&MediaCursor>) -> Option<(String, String)> {
    let cur = cursor?;
    let key = cur.key()?;
    match sort.key {
        media_index::SortKey::Name => {
            if key.is_empty() {
                return None;
            }
        }
        _ => {
            key.parse::<i64>().ok()?;
        }
    }
    Some((key, cur.p.clone()))
}

/// 游标之后的起始下标（`raw` 必须已按同一 `sort` 排好）
///
/// `partition_point` 要的是「**落在游标之前（含游标自身）**」这个单调前缀，
/// 不是「在游标之后」—— 降序时主键大的在前，判断方向也跟着反过来。
fn page_start(raw: &[RawMedia], sort: media_index::Sort, key: Option<&(String, String)>) -> usize {
    let Some((cursor_key, cursor_path)) = key else {
        return 0;
    };
    raw.partition_point(|item| {
        let order = compare_media_key(item, sort.key, cursor_key);
        match order {
            Ordering::Greater if sort.is_desc() => true,
            Ordering::Less if !sort.is_desc() => true,
            Ordering::Equal => item.display.as_str() <= cursor_path.as_str(),
            _ => false,
        }
    })
}

fn compare_media_key(item: &RawMedia, key: media_index::SortKey, cursor_key: &str) -> Ordering {
    match key {
        media_index::SortKey::Name => media_index::natural_key(&item.name)
            .as_str()
            .cmp(cursor_key),
        media_index::SortKey::Size => item.size.cmp(&cursor_key.parse::<u64>().unwrap_or(0)),
        _ => item.mtime.cmp(&cursor_key.parse::<u64>().unwrap_or(0)),
    }
}

/// 一条记录在某个维度上的游标主键（索引路径与内存路径必须生成同一种形态）
fn sort_cursor_key(item: &RawMedia, key: media_index::SortKey) -> String {
    match key {
        media_index::SortKey::Name => media_index::natural_key(&item.name),
        media_index::SortKey::Size => item.size.to_string(),
        _ => item.mtime.to_string(),
    }
}

/// 内存回退路径的整表排序（索引不可用时才会用到）
fn sort_media(raw: &mut [RawMedia], sort: media_index::Sort) {
    raw.sort_by(|a, b| {
        let by_key = match sort.key {
            media_index::SortKey::Name => {
                media_index::natural_key(&a.name).cmp(&media_index::natural_key(&b.name))
            }
            media_index::SortKey::Size => a.size.cmp(&b.size),
            _ => a.mtime.cmp(&b.mtime),
        };
        let by_key = if sort.is_desc() {
            by_key.reverse()
        } else {
            by_key
        };
        by_key.then_with(|| a.display.cmp(&b.display))
    });
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
    /// 排序维度：mtime（默认，最近有媒体的在前）/ path（目录路径升序，同父目录的相邻）
    ///
    /// 文件夹墙按路径排是用户要的「相近的文件夹排在一起」：mtime 会把不同父目录
    /// 的册子按「最后一张照片的日期」打散，翻找时毫无方位感（v4.6.18）。
    pub sort: Option<String>,
    /// 强制穿透目录快照重扫（与媒体列表同一口径）
    #[serde(default, deserialize_with = "crate::api::boolish")]
    pub refresh: Option<bool>,
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
    /// 相册内最早一张图片的修改时间（v4.6.23）：卡片副标题展示「2002/1 – 2012/6」
    /// 这样的时间范围，和 Immich 的相册卡一致。缺失（时间戳为 0）时不出这一行
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_modified: Option<u64>,
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

/// GET /api/files/thumb?path=…&w=320&v=<指纹>
///
/// `v` 是前端带回来的内容指纹（媒体的 `modified`）。**对上了才给长缓存**：URL 一变
/// 内容必变，浏览器可以 `immutable` 一年不再回头问；对不上（老书签、内容刚改）
/// 退回 `no-cache` + ETag 304。这条是「改了缩略图规则但浏览器还抱着旧图」的解药——
/// 光改服务端缓存键没用，URL 不变浏览器永远不会重新请求。
pub async fn thumb(
    State(state): State<AppState>,
    Query(query): Query<ThumbQuery>,
    headers: HeaderMap,
) -> Response {
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
    // 图片的实体宽度 = min(请求宽度, 源图长边)：小图不放大，所以不同档位请求
    // 拿到的是同一份字节，ETag 也必须跟着落在同一个宽度上（否则同一实体两个 ETag）。
    let effective = if kind == "image" {
        Thumbnailer::effective_width(&path, width)
    } else {
        width
    };

    // 内容指纹 → ETag。前端 url 里的 `v` 是源文件的 mtime，**与当前 mtime 一致**说明
    // 这条链接代表的就是这一版字节，可以放心长缓存；不一致（老链接、文件刚改）就每次
    // 回源校验，命中只花一次 304。
    let fingerprint = crate::core::thumbnail::Thumbnailer::fingerprint(&path);
    let etag = format!("\"{fingerprint}-{effective}\"");
    let mtime = crate::core::thumbnail::Thumbnailer::mtime_secs(&path);
    let versioned = query
        .v
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|value| value == mtime);
    let cache_control = if versioned {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache_control.to_string()),
            ],
            Body::empty(),
        )
            .into_response();
    }

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
                (header::CONTENT_TYPE, rendered.mime.to_string()),
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, etag),
            ],
            Body::from(rendered.bytes),
        )
            .into_response(),
        // 拥塞 / 文件仍在写入：503 让 `<img>` 报错，前端调度器按退避重试；
        // 回 200 占位图会让浏览器认为「已加载」，那块灰再也不动。
        ThumbOutcome::Busy => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::RETRY_AFTER, "1".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            Body::empty(),
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
    let sort = match media_index::Sort::parse(query.sort.as_deref()) {
        Ok(sort) => sort,
        Err(err) => return fs_error(FsError::BadRequest(err)),
    };
    // 「条目数」是相册墙的概念，媒体列表里排它等于排时间，直接拒绝比默默改成时间好
    if matches!(sort.key, media_index::SortKey::Count) {
        return fs_error(FsError::BadRequest(
            "媒体列表不支持按条目数排序".to_string(),
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(decode_cursor);

    let scan_root = root.clone();
    let auth_store = Arc::clone(&state.auth);
    let dir_cache = Arc::clone(&state.dir_cache);
    let index = Arc::clone(&state.media_index);
    let refresh = query.refresh.unwrap_or(false);
    let want_dims = query.dims.unwrap_or(false);
    let outcome = tokio::task::spawn_blocking(move || -> Result<BrowsePage, String> {
        // 首选 SQLite 索引（v4.7.0）：一次扫描入库，之后分页 / 计数全走 SQL，
        // 窗口期内的请求连树都不用 stat
        if let Some(page) = browse_indexed(
            &index,
            &scan_root,
            depth,
            limit,
            sort,
            cursor.as_ref(),
            &wanted,
            want_dims,
            refresh,
            Some(dir_cache.as_ref()),
            &auth_store,
        ) {
            return Ok(page);
        }
        // 索引打不开（库坏了 / 被删了）才退回整树内存扫描：功能不降级，只是慢
        let (raw, scanned, counts) = scan_raw(
            &scan_root,
            &wanted,
            depth,
            Some(dir_cache.as_ref()),
            refresh,
        );
        // 稳定排序：主键 + 展示路径兜底，保证翻页不重不漏（方向由用户选的来）
        let mut raw = raw;
        sort_media(&mut raw, sort);

        let total = raw.len();
        let key = cursor_key(sort, cursor.as_ref());
        let start = page_start(&raw, sort, key.as_ref());
        let end = start.saturating_add(limit).min(total);
        let has_more = end < total;
        // 只对当前页做重活（ID3 解析、字幕探测、ThumbHash 查询、像素宽高）
        // 回退路径没有「已经握着的连接」，单独借一次给宽高缓存用
        let slice = &raw[start.min(total)..end];
        let items = match index
            .with_conn(|conn| Ok(build_items(slice, &auth_store, want_dims, Some(conn))))
        {
            Some(items) => items,
            None => build_items(slice, &auth_store, want_dims, None),
        };
        let next_cursor = if has_more && end > start {
            let last = &raw[end - 1];
            Some(encode_cursor(
                &sort_cursor_key(last, sort.key),
                &last.display,
            ))
        } else {
            None
        };
        Ok((items, scanned, total, counts, has_more, next_cursor))
    })
    .await;

    match outcome {
        Ok(Ok((items, scanned, total, counts, has_more, next_cursor))) => Json(BrowseResult {
            root: fs::normalize_display(&root),
            items,
            scanned,
            total,
            counts,
            has_more,
            next_cursor,
        })
        .into_response(),
        Ok(Err(err)) => fs_error(FsError::Io(format!("扫描媒体目录失败：{err}"))),
        Err(err) => fs_error(FsError::Io(format!("扫描媒体目录失败：{err}"))),
    }
}

/// `browse` 的返回元组：`(items, scanned, total, counts, has_more, next_cursor)`
type BrowsePage = (
    Vec<MediaItem>,
    usize,
    usize,
    KindCounts,
    bool,
    Option<String>,
);

/// 索引版 browse：同步（窗口期内跳过）→ SQL 分页 + 计数 → 只对当前页做重活
#[allow(clippy::too_many_arguments)]
fn browse_indexed(
    index: &MediaIndex,
    root: &Path,
    depth: usize,
    limit: usize,
    sort: media_index::Sort,
    cursor: Option<&MediaCursor>,
    wanted: &[&str],
    want_dims: bool,
    refresh: bool,
    cache: Option<&crate::core::dir_cache::DirCache>,
    auth: &crate::core::auth::AuthStore,
) -> Option<BrowsePage> {
    let mode = if refresh {
        SyncMode::Force
    } else {
        SyncMode::Request
    };
    let scanned = sync_index(index, root, depth, mode, cache)?;
    let scope = Scope::new(&fs::normalize_display(root), depth);
    let key = cursor_key(sort, cursor);
    // 查询本身是毫秒级，先一次性取完（数据库锁只在这一小段里握着）
    let (assets, has_more, counts, total) = index.with_conn(|conn| {
        let (image, video, audio) = media_index::totals(conn, &scope)?;
        let counts = KindCounts {
            image,
            video,
            audio,
        };
        let total = wanted
            .iter()
            .map(|kind| match *kind {
                "image" => image,
                "video" => video,
                "audio" => audio,
                _ => 0,
            })
            .sum();
        let (assets, has_more) =
            media_index::page(conn, &scope, wanted, sort, key.as_ref(), limit)?;
        Ok((assets, has_more, counts, total))
    })?;

    let next_cursor = if has_more {
        assets.last().map(|asset| {
            let key = match sort.key {
                media_index::SortKey::Name => media_index::natural_key(&asset.name),
                media_index::SortKey::Size => asset.size.to_string(),
                _ => asset.mtime.to_string(),
            };
            encode_cursor(&key, &asset.path)
        })
    } else {
        None
    };
    let raw: Vec<RawMedia> = assets
        .into_iter()
        .map(|asset| RawMedia {
            name: asset.name,
            path: PathBuf::from(&asset.real),
            display: asset.path,
            folder: asset.dir,
            size: asset.size,
            mtime: asset.mtime,
            kind: match asset.kind.as_str() {
                "video" => "video",
                "audio" => "audio",
                _ => "image",
            },
            ext: asset.ext,
        })
        .collect();
    // 读图头 / 识字幕是慢 I/O，必须在锁外做：不能让一页把别的请求一起堵住
    let items = match index.with_conn(|conn| Ok(build_items(&raw, auth, want_dims, Some(conn)))) {
        Some(items) => items,
        None => build_items(&raw, auth, want_dims, None),
    };
    // 这一页花了几秒也没关系：窗口从「服务完一页」算，连续翻页就不会再走树
    index.with_conn(|conn| media_index::touch_synced(conn, &scope.root));
    Some((items, scanned, total, counts, has_more, next_cursor))
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
    let sort = match media_index::Sort::parse(query.sort.as_deref()) {
        Ok(sort) => sort,
        Err(err) => return fs_error(FsError::BadRequest(err)),
    };
    let cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(decode_cursor);

    let scan_root = root.clone();
    let auth_store = Arc::clone(&state.auth);
    let dir_cache = Arc::clone(&state.dir_cache);
    let index = Arc::clone(&state.media_index);
    let refresh = query.refresh.unwrap_or(false);
    let outcome = tokio::task::spawn_blocking(move || {
        // 相册墙也吃索引（v4.7.0）：`GROUP BY dir` 出卡片，封面取册内最新一条。
        // 与浏览共用同一份 media_roots 窗口，所以「时间线 → 文件夹墙」不会各扫一遍
        if let Some(page) = albums_indexed(
            &index,
            &scan_root,
            depth,
            limit,
            sort,
            cursor.as_ref(),
            &wanted,
            refresh,
            Some(dir_cache.as_ref()),
            &auth_store,
        ) {
            return page;
        }
        let mut raw = scan_albums(
            &scan_root,
            depth,
            &wanted,
            Some(dir_cache.as_ref()),
            refresh,
        );
        let total = raw.len();
        sort_albums(&mut raw, sort);
        let key = album_cursor_key(sort, cursor.as_ref());
        let start = album_page_start(&raw, sort, key.as_ref());
        let end = start.saturating_add(limit).min(total);
        let has_more = end < total;
        let items = build_albums(&raw[start.min(total)..end], &auth_store);
        let next_cursor = if has_more && end > start {
            let last = &raw[end - 1];
            Some(encode_cursor(&album_sort_key(last, sort.key), &last.path))
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

/// `albums` 的返回元组：`(items, total, has_more, next_cursor)`
type AlbumPage = (Vec<Album>, usize, bool, Option<String>);

/// 索引版相册墙：`GROUP BY dir` 出卡片行，本页再各取一张封面
#[allow(clippy::too_many_arguments)]
fn albums_indexed(
    index: &MediaIndex,
    root: &Path,
    depth: usize,
    limit: usize,
    sort: media_index::Sort,
    cursor: Option<&MediaCursor>,
    wanted: &[&str],
    refresh: bool,
    cache: Option<&crate::core::dir_cache::DirCache>,
    auth: &crate::core::auth::AuthStore,
) -> Option<AlbumPage> {
    let mode = if refresh {
        SyncMode::Force
    } else {
        SyncMode::Request
    };
    sync_index(index, root, depth, mode, cache)?;
    let scope = Scope::new(&fs::normalize_display(root), depth);
    let key = album_cursor_key(sort, cursor);
    // 聚合 + 本页封面一次性取完（都是小查询），后面拼卡片时不占着数据库锁
    let (rows, covers, has_more, total) = index.with_conn(|conn| {
        let (rows, has_more) =
            media_index::album_page(conn, &scope, wanted, sort, key.as_ref(), limit)?;
        let total = media_index::album_total(conn, &scope, wanted)?;
        let mut covers = Vec::with_capacity(rows.len());
        for row in &rows {
            covers.push(media_index::album_cover(conn, &row.dir, wanted)?);
        }
        Ok((rows, covers, has_more, total))
    })?;

    let raw: Vec<RawAlbum> = rows
        .iter()
        .zip(covers.iter())
        .map(|(row, cover)| RawAlbum {
            name: Path::new(&row.dir)
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| row.dir.clone()),
            path: row.dir.clone(),
            count: row.count,
            is_root: row.dir == scope.root,
            // 展示串与真实串在 UNC 上不是一个东西，缩略图键必须用真实串
            cover_path: cover.as_ref().map(|asset| PathBuf::from(&asset.real)),
            mtime: row.mtime,
            first_mtime: row.first_mtime,
            bytes: row.bytes,
        })
        .collect();
    let items = build_albums(&raw, auth);
    let next_cursor = if has_more {
        rows.last()
            .map(|row| encode_cursor(&album_sort_key_of_row(row, sort.key), &row.dir))
    } else {
        None
    };
    index.with_conn(|conn| media_index::touch_synced(conn, &scope.root));
    Some((items, total, has_more, next_cursor))
}

/// 相册游标主键（内存路径用）
fn album_sort_key(item: &RawAlbum, key: media_index::SortKey) -> String {
    match key {
        media_index::SortKey::Name => item.path.to_lowercase(),
        media_index::SortKey::Count => item.count.to_string(),
        media_index::SortKey::Size => item.bytes.to_string(),
        media_index::SortKey::Mtime => item.mtime.to_string(),
    }
}

/// 相册游标主键（索引路径用，与 `album_sort_key` 必须同源）
fn album_sort_key_of_row(row: &media_index::AlbumRow, key: media_index::SortKey) -> String {
    match key {
        media_index::SortKey::Name => row.dir.to_lowercase(),
        media_index::SortKey::Count => row.count.to_string(),
        media_index::SortKey::Size => row.bytes.to_string(),
        media_index::SortKey::Mtime => row.mtime.to_string(),
    }
}

/// 相册游标规整：形态不匹配就当没传
fn album_cursor_key(
    sort: media_index::Sort,
    cursor: Option<&MediaCursor>,
) -> Option<(String, String)> {
    let cur = cursor?;
    let key = cur.key()?;
    if matches!(sort.key, media_index::SortKey::Name) {
        return if key.is_empty() {
            None
        } else {
            Some((key, cur.p.clone()))
        };
    }
    key.parse::<i64>().ok()?;
    Some((key, cur.p.clone()))
}

/// 相册内存排序（索引不可用时的回退路径）
fn sort_albums(raw: &mut [RawAlbum], sort: media_index::Sort) {
    raw.sort_by(|a, b| {
        let by_key = match sort.key {
            media_index::SortKey::Name => a.path.to_lowercase().cmp(&b.path.to_lowercase()),
            media_index::SortKey::Count => a.count.cmp(&b.count),
            media_index::SortKey::Size => a.bytes.cmp(&b.bytes),
            media_index::SortKey::Mtime => a.mtime.cmp(&b.mtime),
        };
        let by_key = if sort.is_desc() {
            by_key.reverse()
        } else {
            by_key
        };
        by_key.then_with(|| a.path.cmp(&b.path))
    });
}

/// 相册游标之后的起始下标
fn album_page_start(
    raw: &[RawAlbum],
    sort: media_index::Sort,
    key: Option<&(String, String)>,
) -> usize {
    let Some((cursor_key, cursor_path)) = key else {
        return 0;
    };
    raw.partition_point(|item| {
        let order = match sort.key {
            media_index::SortKey::Name => item.path.to_lowercase().cmp(cursor_key),
            media_index::SortKey::Count => {
                item.count.cmp(&cursor_key.parse::<usize>().unwrap_or(0))
            }
            media_index::SortKey::Size => item.bytes.cmp(&cursor_key.parse::<u64>().unwrap_or(0)),
            media_index::SortKey::Mtime => item.mtime.cmp(&cursor_key.parse::<u64>().unwrap_or(0)),
        };
        match order {
            Ordering::Greater if sort.is_desc() => true,
            Ordering::Less if !sort.is_desc() => true,
            Ordering::Equal => item.path.as_str() <= cursor_path.as_str(),
            _ => false,
        }
    })
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

/// 字幕探测的会话缓存（v4.7.0）：`is_file()` 在网盘上是一次真实 stat，
/// 一页七个视频就是十几次往返（实测单次 200ms+）。同一路径 + mtime 只探一次。
/// `路径 -> (mtime, 字幕展示路径)`
type SubtitleCache = HashMap<PathBuf, (u64, Option<String>)>;

static SUBTITLES: LazyLock<Mutex<SubtitleCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn find_subtitle_cached(video: &Path, mtime: u64) -> Option<String> {
    if let Ok(cache) = SUBTITLES.lock()
        && let Some((stamp, hit)) = cache.get(video)
        && *stamp == mtime
    {
        return hit.clone();
    }
    let found = find_subtitle(video);
    if let Ok(mut cache) = SUBTITLES.lock() {
        if cache.len() >= 20_000 {
            cache.clear();
        }
        cache.insert(video.to_path_buf(), (mtime, found.clone()));
    }
    found
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

/// 读一个目录的条目（带 `size` / `mtime`）：命中快照就既不 `readdir` 也不逐条 `stat`。
///
/// 这是媒体库从「每次整树重扫」变成「一次读库」的唯一入口，也是**唯一**读目录的地方。
/// `refresh = true` 时穿透缓存（前端的「重新扫描」按钮），但照样把结果写回。
///
/// 目录自己还是要 `stat` 一次的 —— 快照的失效判定全靠它（`dir_cache::stamp_of`）。
fn read_dir_entries(
    dir: &Path,
    cache: Option<&crate::core::dir_cache::DirCache>,
    refresh: bool,
) -> Vec<crate::core::dir_cache::MediaEntry> {
    use crate::core::dir_cache::{self, KIND_DIR, KIND_FILE, KIND_LINK, MediaEntry};

    let display = fs::normalize_display(dir);
    let stamp = std::fs::metadata(dir)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(dir_cache::stamp_of);

    if !refresh
        && let (Some(cache), Some(stamp)) = (cache, stamp)
        && let Some(entries) = cache.load_media(&display, stamp)
    {
        return entries;
    }

    let mut entries: Vec<MediaEntry> = Vec::new();
    if let Ok(reader) = std::fs::read_dir(dir) {
        for entry in reader.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let (kind, size, mtime) = if file_type.is_dir() {
                (KIND_DIR, 0, 0)
            } else if file_type.is_file() {
                // 只有文件需要 size / mtime；目录与链接省掉这次 metadata
                match entry.metadata() {
                    Ok(meta) => (
                        KIND_FILE,
                        meta.len(),
                        meta.modified()
                            .ok()
                            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                            .map(|delta| delta.as_secs())
                            .unwrap_or(0),
                    ),
                    // stat 失败（权限 / 竞态删除）仍保留条目：下一层过滤会丢掉它，
                    // 但如果这里直接跳过，快照就会「少一条」，比多一条更难查
                    Err(_) => (KIND_FILE, 0, 0),
                }
            } else {
                (KIND_LINK, 0, 0)
            };
            entries.push(MediaEntry::new(name, kind, size, mtime));
        }
    }
    // 顺序必须自己定死：`readdir` 的顺序不可依赖，快照命中时要复现同一顺序，
    // 相册封面这类「并列取第一个」的逻辑才不会因为缓存而换个结果
    entries.sort_by_cached_key(|entry| entry.name.to_lowercase());

    if let Some(cache) = cache
        && let Some(stamp) = stamp
    {
        cache.store_media(&display, stamp, &entries);
    }
    entries
}

/// 走一遍目录树，只做 readdir + 分类 + 一次 metadata。
///
/// **没有扫描上限**：媒体库和文件管理一样，必须能看到最后一条。`counts` 统计的是
/// 全部媒体文件（不受 `wanted` 限制），前端据此显示「库里到底有多少」。
fn scan_raw(
    root: &Path,
    wanted: &[&str],
    depth: usize,
    cache: Option<&crate::core::dir_cache::DirCache>,
    refresh: bool,
) -> (Vec<RawMedia>, usize, KindCounts) {
    use crate::core::dir_cache::{KIND_DIR, KIND_FILE};

    let mut out = Vec::new();
    let mut counts = KindCounts::default();
    let mut scanned = 0usize;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, level)) = queue.pop() {
        let entries = read_dir_entries(&dir, cache, refresh);
        scanned += entries.len();
        // 一次 normalize 给这一层的所有条目复用（原来每条都算一次）
        let folder = fs::normalize_display(&dir);

        for entry in entries {
            let name = entry.name;
            // 系统垃圾（Thumbs.db / .DS_Store / ._* / ~$*）一律不进媒体库，
            // 否则相册里会混进一堆打不开的元数据文件（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }
            let path = dir.join(&name);

            if entry.kind == KIND_DIR {
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
            if entry.kind != KIND_FILE {
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

            out.push(RawMedia {
                name,
                display: fs::normalize_display(&path),
                folder: folder.clone(),
                path,
                size: entry.size,
                mtime: entry.mtime,
                kind,
                ext,
            });
        }
    }

    (out, scanned, counts)
}

/// 把整棵树同步进索引（v4.7.0），返回本次真正 readdir 到的目录项数（诊断用）。
///
/// 窗口期（[`media_index::FRESH`] 秒）内直接返回 0 —— 一个目录都不碰，分页与计数
/// 全走 SQL，这就是「翻页不再越翻越木」的来源。
///
/// 窗口期外仍要把树走一遍，但只做「stat 目录 + 读目录快照」：目录 mtime 与索引里
/// 记的一致就不写库，也不重建两万条 `RawMedia`、不排序，比旧实现便宜一个量级。
/// （目录 mtime 不会因子目录内容变化而变化，所以想零延迟只能 stat 全树。）
/// 索引同步的触发方式（v4.8.0）。
///
/// 三者的差别只在「跳过什么」：
/// - 请求落在新鲜窗口内 → 一步 IO 都不做（这是常规浏览路径）；
/// - 用户点「重新扫描」→ 连目录戳都不信，整棵树重写；
/// - 后台轮询 → **不查窗口**（它自己就是来续窗口的），但目录戳一致仍然跳过写库。
///   后台要是也走窗口判断，每次都会在第一行就返回，永远不干活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncMode {
    Request,
    Force,
    Background,
}

impl SyncMode {
    /// 是否强制重写所有目录行（无视目录戳）
    fn refresh(self) -> bool {
        matches!(self, Self::Force)
    }

    /// 是否服从「新鲜窗口」：窗口内直接返回 0
    fn honors_window(self) -> bool {
        matches!(self, Self::Request)
    }
}

pub(crate) fn sync_index(
    index: &MediaIndex,
    root: &Path,
    depth: usize,
    mode: SyncMode,
    cache: Option<&crate::core::dir_cache::DirCache>,
) -> Option<usize> {
    let refresh = mode.refresh();
    let scope = Scope::new(&fs::normalize_display(root), depth);
    index.with_conn(|conn| {
        if mode.honors_window()
            && let Some((synced_depth, at)) = media_index::synced(conn, &scope.root)?
            && synced_depth >= depth as i64
            && (crate::core::auth::now_secs().max(0) as u64).saturating_sub(at)
                <= media_index::FRESH
        {
            crate::log_debug!(
                "媒体索引命中窗口：{}（{} 层，{}s 前同步）",
                scope.root,
                synced_depth,
                (crate::core::auth::now_secs().max(0) as u64).saturating_sub(at)
            );
            return Ok(0);
        }
        let started = std::time::Instant::now();
        let scanned = walk_into_index(conn, root, &scope, refresh, cache)?;
        media_index::mark_synced(conn, &scope.root, depth)?;
        crate::log_debug!(
            "媒体索引同步完成：{} {} 条，{}ms",
            scope.root,
            scanned,
            started.elapsed().as_millis()
        );
        Ok(scanned)
    })
}

/// 走一遍树并把每个目录的结果写进索引
///
/// 与 `scan_raw` 的口径必须完全一致（junk / svg / SKIP_DIRS / 深度 / 分类），
/// 否则「索引版」和「回退版」会给出两套结果。
/// 走树时攒下来的「待写目录」：readdir 已经做完，只等一个短事务写进去
struct PendingDir {
    dir: String,
    depth: i64,
    stamp: i64,
    assets: Vec<media_index::Asset>,
}

/// 一批目录一个事务。只做写，不夹任何文件系统调用 —— 事务时长直接等于写锁时长。
fn flush_dirs(conn: &rusqlite::Connection, batch: &mut Vec<PendingDir>) -> rusqlite::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for item in batch.iter() {
        media_index::replace_dir(&tx, &item.dir, item.depth, item.stamp, &item.assets)?;
    }
    tx.commit()?;
    batch.clear();
    Ok(())
}

fn walk_into_index(
    conn: &rusqlite::Connection,
    root: &Path,
    scope: &Scope,
    refresh: bool,
    cache: Option<&crate::core::dir_cache::DirCache>,
) -> rusqlite::Result<usize> {
    use crate::core::dir_cache::{KIND_DIR, KIND_FILE};

    let depth = (scope.max_depth - scope.min_depth).max(0) as usize;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut scanned = 0usize;
    let mut dirs = 0usize;
    let mut reused = 0usize;
    let mut io_time = std::time::Duration::ZERO;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    // 首次建库要写两万行。**读盘与写库必须分开**：NAS 上 readdir 一个目录 250ms，
    // 要是把 readdir 包在事务里，写锁会被持有几十秒，登录 / 上传 / 缩略图缓存
    // 全部撞 `database is locked`（v4.8.0 修的就是这个）。现在循环只做读，
    // 攒够一批才开一个几毫秒的写事务。
    let mut batch: Vec<PendingDir> = Vec::new();

    while let Some((dir, level)) = stack.pop() {
        let display = if level == 0 {
            scope.root.clone()
        } else {
            fs::normalize_display(&dir)
        };
        visited.insert(display.clone());
        // 目录自己的 mtime：这一层名字集合的版本号
        let stamp = std::fs::metadata(&dir)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(crate::core::dir_cache::stamp_of);
        let read_started = std::time::Instant::now();
        let entries = read_dir_entries(&dir, cache, refresh);
        io_time += read_started.elapsed();
        dirs += 1;
        scanned += entries.len();
        // 快照命中（戳一致）就认为库里这一目录的行还是对的；`stamp` 取不到时一律重写
        let unchanged =
            !refresh && stamp.is_some() && media_index::dir_stamp(conn, &display)? == stamp;
        if unchanged {
            reused += 1;
        }

        let mut assets: Vec<media_index::Asset> = Vec::new();
        for entry in entries {
            let name = entry.name;
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }
            let path = dir.join(&name);

            if entry.kind == KIND_DIR {
                if level < depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
                    && !crate::core::reserved::is_reserved_lexical(&path)
                {
                    stack.push((path, level + 1));
                }
                continue;
            }
            if entry.kind != KIND_FILE {
                continue;
            }
            let ext = fs::extension_of_name(&name);
            if ext == "svg" {
                continue;
            }
            let (kind, _) = fs::classify_name(&name, &ext, false);
            if !matches!(kind, "image" | "video" | "audio") || unchanged {
                continue;
            }
            assets.push(media_index::Asset {
                path: fs::normalize_display(&path),
                // 展示串是给前端用的（`//server/share`），真实串是给缩略图键和 open 用的
                real: path.to_string_lossy().to_string(),
                name,
                dir: display.clone(),
                kind: kind.to_string(),
                ext,
                size: entry.size,
                mtime: entry.mtime,
            });
        }

        if !unchanged {
            batch.push(PendingDir {
                dir: display,
                depth: scope.depth_of(level),
                stamp: stamp.unwrap_or(0),
                assets,
            });
            if batch.len() >= media_index::CHUNK {
                flush_dirs(conn, &mut batch)?;
            }
        }
    }
    flush_dirs(conn, &mut batch)?;

    // 这次没走到的目录（外面删掉了）连同条目一起清掉，索引不能留死行
    let tx = conn.unchecked_transaction()?;
    media_index::prune(&tx, scope, &visited)?;
    tx.commit()?;
    crate::log_debug!(
        "媒体索引走树：{} 个目录（{} 个命中旧行，跳过写库），读目录耗时 {}ms",
        dirs,
        reused,
        io_time.as_millis()
    );
    Ok(scanned)
}

/// 图片像素宽高的进程内缓存（v4.6.19）。
///
/// 时间线的等高行要按真实宽高比排，`image::image_dimensions` 只读文件头，但一页
/// 最多 300 张、网盘挂载点上的 open 并不便宜，所以按「路径 + mtime + 大小」记住结果：
/// 同一张图反复翻页只碰一次磁盘，文件被替换（mtime/大小变了）会自动重读。
struct DimEntry {
    mtime: u64,
    size: u64,
    dims: Option<(u32, u32)>,
}

static IMAGE_DIMS: LazyLock<Mutex<HashMap<PathBuf, DimEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 缓存的条目上限：够放十万张图，超了就整体清空重来（画廊场景极少走到）
const IMAGE_DIMS_CAP: usize = 100_000;

fn image_dims(path: &Path, mtime: u64, size: u64) -> Option<(u32, u32)> {
    if let Ok(cache) = IMAGE_DIMS.lock()
        && let Some(hit) = cache.get(path)
        && hit.mtime == mtime
        && hit.size == size
    {
        return hit.dims;
    }
    // 解不开的格式（HEIC/AVIF 等）返回 None，前端回落到默认比例，不影响列表
    let dims = image::image_dimensions(path).ok();
    if let Ok(mut cache) = IMAGE_DIMS.lock() {
        if cache.len() >= IMAGE_DIMS_CAP {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), DimEntry { mtime, size, dims });
    }
    dims
}

/// 一页里并发读图头的线程数（v4.9.3）。
///
/// 网盘上单次 `open + read` 是 200~400ms：120 张串行就是 40 秒以上（用户实测
/// 45000ms，就是「图片库比文件夹视图慢几十倍」的唯一原因 —— 文件夹墙根本不读文件头）。
/// 并发在这里是纯收益：请求重叠的是网络往返，不是磁盘寻道。
const DIMS_THREADS: usize = 6;

/// `(位置, 缓存键, 真实路径, mtime, size)`：一个待读的图头
type DimJob = (usize, String, PathBuf, u64, u64);
/// `(位置, 缓存键, mtime, size, 宽高)`
type DimResult = (usize, String, u64, u64, Option<(u32, u32)>);

/// 把一批「每条都要碰一次文件」的活并行跑掉（v4.9.3）。
///
/// 顺序不保证 —— 每条任务的返回值里都带着自己的下标，调用方按它归位。
/// 只有一两条时不值得开线程，直接串行（省掉建线程的固定开销）。
fn run_parallel<T, R>(jobs: Vec<T>, worker: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }
    if total == 1 {
        let job = jobs.into_iter().next().unwrap();
        return vec![worker(job)];
    }
    let queue = std::sync::Mutex::new(jobs.into_iter());
    let done = std::sync::Mutex::new(Vec::with_capacity(total));
    std::thread::scope(|scope| {
        for _ in 0..DIMS_THREADS.min(total) {
            scope.spawn(|| {
                loop {
                    // 取任务只锁一瞬间，真正的读盘在锁外
                    let job = match queue.lock() {
                        Ok(mut guard) => guard.next(),
                        Err(_) => None,
                    };
                    let Some(job) = job else {
                        break;
                    };
                    let out = worker(job);
                    if let Ok(mut guard) = done.lock() {
                        guard.push(out);
                    }
                }
            });
        }
    });
    done.into_inner().unwrap_or_default()
}

/// 并发读一批图头，返回 `(位置, 缓存键, mtime, size, 宽高)`
fn read_dims_parallel(jobs: Vec<DimJob>) -> Vec<DimResult> {
    run_parallel(jobs, |(position, key, path, mtime, size)| {
        (position, key, mtime, size, image_dims(&path, mtime, size))
    })
}

/// 给当前页的条目补上「贵」的字段：音频 ID3、视频外挂字幕、图片 ThumbHash 与像素宽高。
/// `conn` 是索引连接（可能没有）：宽高缓存读写都走它，**不能**在这里再锁一次
/// `MediaIndex` —— 调用方（`browse_indexed`）已经握着那把锁，重入就是死锁。
fn build_items(
    raw: &[RawMedia],
    auth: &crate::core::auth::AuthStore,
    want_dims: bool,
    conn: Option<&rusqlite::Connection>,
) -> Vec<MediaItem> {
    let mut items: Vec<MediaItem> = Vec::with_capacity(raw.len());
    let mut hash_slots: Vec<(usize, &Path)> = Vec::new();

    // 像素宽高先批量问索引（v4.7.0）：跨会话命中就不必再开图读文件头 ——
    // 网盘上每张一次 open 是几十毫秒，一页 120 张就是「打开图片库很慢」的大头
    let mut dim_candidates: Vec<usize> = Vec::new();
    if want_dims {
        for (position, entry) in raw.iter().enumerate() {
            if entry.kind == "image" {
                dim_candidates.push(position);
            }
        }
    }
    let mut dims: HashMap<usize, Option<(u32, u32)>> = HashMap::new();
    if !dim_candidates.is_empty() {
        let keys: Vec<(String, u64, u64)> = dim_candidates
            .iter()
            .map(|&position| {
                let entry = &raw[position];
                (
                    entry.path.to_string_lossy().to_string(),
                    entry.mtime,
                    entry.size,
                )
            })
            .collect();
        let cached = conn
            .map(|conn| {
                let borrowed: Vec<(&str, u64, u64)> = keys
                    .iter()
                    .map(|(path, mtime, size)| (path.as_str(), *mtime, *size))
                    .collect();
                media_index::dims_get_many(conn, &borrowed)
            })
            .unwrap_or_default();
        let mut jobs: Vec<DimJob> = Vec::new();
        for (slot, &position) in dim_candidates.iter().enumerate() {
            let (key, mtime, size) = &keys[slot];
            if let Some((w, h)) = cached.get(key) {
                // (0,0) 是「读过、但解不出宽高」的负缓存，别再读一遍
                dims.insert(position, (*w > 0 && *h > 0).then_some((*w, *h)));
                continue;
            }
            jobs.push((
                position,
                key.clone(),
                raw[position].path.clone(),
                *mtime,
                *size,
            ));
        }
        if !jobs.is_empty() {
            let started = std::time::Instant::now();
            let cached_hits = dim_candidates.len() - jobs.len();
            let disk_reads = jobs.len();
            let mut fresh: Vec<(String, u64, u64, u32, u32)> = Vec::new();
            for (position, key, mtime, size, read) in read_dims_parallel(jobs) {
                // 读不出也写回 (0,0)：坏图 / HEIC 这类每开一次页面重读一次纯属浪费
                let (w, h) = read.unwrap_or((0, 0));
                fresh.push((key, mtime, size, w, h));
                dims.insert(position, read);
            }
            if let Some(conn) = conn
                && !fresh.is_empty()
            {
                let borrowed: Vec<(&str, u64, u64, u32, u32)> = fresh
                    .iter()
                    .map(|(path, mtime, size, w, h)| (path.as_str(), *mtime, *size, *w, *h))
                    .collect();
                media_index::dims_put_many(conn, &borrowed);
            }
            crate::log_debug!(
                "像素宽高：{} 张图（缓存命中 {}，读盘 {}）耗时 {}ms",
                dim_candidates.len(),
                cached_hits,
                disk_reads,
                started.elapsed().as_millis()
            );
        }
    }

    // 音频 ID3 与视频外挂字幕同样是网盘 IO：一个读几十 KB 标签块，一个对每条
    // 视频做两次 `is_file`（.vtt / .srt）。串行时一页 120 条又是几十秒的量级。
    let probe_started = std::time::Instant::now();
    let audio_jobs: Vec<(usize, PathBuf)> = raw
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == "audio")
        .map(|(position, entry)| (position, entry.path.clone()))
        .collect();
    let tags_by_pos: HashMap<usize, crate::core::audio_tags::TrackTags> =
        run_parallel(audio_jobs, |(position, path)| {
            (position, crate::core::audio_tags::read_tags(&path))
        })
        .into_iter()
        .collect();

    let video_jobs: Vec<(usize, PathBuf, u64)> = raw
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == "video")
        .map(|(position, entry)| (position, entry.path.clone(), entry.mtime))
        .collect();
    let subtitle_by_pos: HashMap<usize, Option<String>> =
        run_parallel(video_jobs, |(position, path, mtime)| {
            (position, find_subtitle_cached(&path, mtime))
        })
        .into_iter()
        .collect();

    if !tags_by_pos.is_empty() || !subtitle_by_pos.is_empty() {
        crate::log_debug!(
            "音频标签 / 字幕探测：{} 条音频、{} 条视频，耗时 {}ms",
            tags_by_pos.len(),
            subtitle_by_pos.len(),
            probe_started.elapsed().as_millis()
        );
    }

    for (position, entry) in raw.iter().enumerate() {
        let tags = tags_by_pos.get(&position).cloned().unwrap_or_default();
        let subtitle = subtitle_by_pos.get(&position).cloned().flatten();
        if entry.kind == "image" {
            hash_slots.push((items.len(), entry.path.as_path()));
        }
        let dims_entry = dims.get(&position).copied().flatten();
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
            width: dims_entry.map(|(w, _)| w),
            height: dims_entry.map(|(_, h)| h),
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
    /// 最早一张图的时间戳（0 = 未知）
    first_mtime: u64,
    /// 册内媒体总字节数（v4.9.0：按体积排序）
    bytes: u64,
}

fn scan_albums(
    root: &Path,
    depth: usize,
    wanted: &[&str],
    cache: Option<&crate::core::dir_cache::DirCache>,
    refresh: bool,
) -> Vec<RawAlbum> {
    use crate::core::dir_cache::{KIND_DIR, KIND_FILE};

    let mut albums: Vec<RawAlbum> = Vec::new();

    // 与 v2.8 相册视图同一口径（设计文档 3.4.3）：遍历目录树，**每个直接含有
    // 图片的目录**都记一本相册——包括扫描起点（挂载目录）自己；计数只看该目录
    // 第一层，点进相册也只列第一层，卡片上的条数才和看到的一致。
    //
    // 没有扫描上限：相册多到几万本也必须能翻到最后一张卡片。
    let mut stack: Vec<(PathBuf, usize, bool)> = vec![(root.to_path_buf(), 0, true)];

    while let Some((dir, level, is_root)) = stack.pop() {
        let entries = read_dir_entries(&dir, cache, refresh);
        let mut count = 0usize;
        let mut bytes = 0u64;
        // (mtime, path)：相册封面取最新的一张图
        let mut newest: Option<(u64, PathBuf)> = None;
        // 最早一张图的 mtime：卡片副标题的时间范围（v4.6.23）
        let mut oldest: u64 = 0;

        for entry in entries {
            let name = entry.name;
            // 系统垃圾（Thumbs.db / .DS_Store / ._*）一律不进媒体库（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }
            let path = dir.join(&name);

            if entry.kind == KIND_DIR {
                // 同上：数据目录不是相册
                if level < depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
                    && !crate::core::reserved::is_reserved_lexical(&path)
                {
                    stack.push((path, level + 1, false));
                }
                continue;
            }
            if entry.kind != KIND_FILE {
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
            bytes += entry.size;
            if entry.mtime > 0 && (oldest == 0 || entry.mtime < oldest) {
                oldest = entry.mtime;
            }
            if newest
                .as_ref()
                .map(|(best, _)| entry.mtime > *best)
                .unwrap_or(true)
            {
                newest = Some((entry.mtime, path));
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
            first_mtime: oldest,
            bytes,
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
            first_modified: (entry.first_mtime > 0).then_some(entry.first_mtime),
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
        // 不回显缓存目录的物理路径（安全审查 2026-11-02）：UI 用不到，
        // 泄露服务器目录结构没有收益
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
    /// 测试里的默认排序：新的在前
    const NEWEST: media_index::Sort =
        media_index::Sort::new(media_index::SortKey::Mtime, media_index::SortDir::Desc);

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

        let (raw, _scanned, counts) = scan_raw(&root, &["image"], 0, None, false);
        assert_eq!(raw.len(), 5000, "旧实现到 4000 就截断，现在必须全都在");
        // counts 统计全部媒体文件，与 wanted 无关
        assert_eq!(counts.image, 5000);
        assert_eq!(counts.video, 1);
        assert_eq!(counts.audio, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 图片像素宽高（v4.6.19）：时间线的等高行要真实宽高比，解不开的格式退回 None
    #[test]
    fn image_dims_reads_header_and_survives_broken_files() {
        let root = scratch("dims");
        let wide = root.join("wide.png");
        image::RgbImage::new(40, 20).save(&wide).unwrap();
        let tall = root.join("tall.png");
        image::RgbImage::new(20, 40).save(&tall).unwrap();

        assert_eq!(image_dims(&wide, 1, 1), Some((40, 20)));
        assert_eq!(image_dims(&tall, 1, 1), Some((20, 40)));
        // mtime/大小变了当成新文件重读（缓存不能把换过的图按旧尺寸铺）
        assert_eq!(image_dims(&wide, 2, 1), Some((40, 20)));

        // 坏文件（或 HEIC 这类没编解码器的格式）只返回 None，不 panic
        let broken = root.join("broken.jpg");
        std::fs::write(&broken, b"not an image").unwrap();
        assert_eq!(image_dims(&broken, 1, 11), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// v4.9.3：一页 120 张图头串行要几十秒，必须并行跑
    #[test]
    fn run_parallel_actually_overlaps_jobs() {
        // 30 个任务、每个 20ms：串行要 600ms，6 路并发应在 100~200ms 完成
        let jobs: Vec<usize> = (0..30).collect();
        let started = std::time::Instant::now();
        let mut out = run_parallel(jobs, |n| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            n * 2
        });
        out.sort_unstable();
        assert_eq!(
            out,
            (0..30).map(|n| n * 2).collect::<Vec<_>>(),
            "任务不能漏也不能重"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(450),
            "并发没生效，耗时 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cursor_paging_covers_every_item_exactly_once() {
        let root = scratch("paging");
        for index in 0..17u64 {
            touch(&root, &format!("p{index:02}.jpg"), 1_700_000_000 + index);
        }

        let (mut raw, _, _) = scan_raw(&root, &["image"], 0, None, false);
        sort_media(&mut raw, NEWEST);
        let expected: Vec<String> = raw.iter().map(|item| item.display.clone()).collect();
        // 新的在前
        assert!(expected[0].ends_with("p16.jpg"));

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        let mut rounds = 0;
        loop {
            rounds += 1;
            assert!(rounds < 20, "分页没有收敛");
            let key = cursor_key(NEWEST, cursor.as_ref());
            let start = page_start(&raw, NEWEST, key.as_ref());
            let end = (start + 5).min(raw.len());
            for item in &raw[start..end] {
                seen.push(item.display.clone());
            }
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                k: raw[end - 1].mtime.to_string(),
                m: None,
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
        let (mut raw, _, _) = scan_raw(&root, &["image"], 0, None, false);
        sort_media(&mut raw, NEWEST);

        let mut seen = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        loop {
            let key = cursor_key(NEWEST, cursor.as_ref());
            let start = page_start(&raw, NEWEST, key.as_ref());
            let end = (start + 4).min(raw.len());
            seen.extend(raw[start..end].iter().map(|item| item.display.clone()));
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                k: raw[end - 1].mtime.to_string(),
                m: None,
                p: raw[end - 1].display.clone(),
            });
        }
        assert_eq!(seen.len(), 9, "mtime 相同也不能漏项或死循环");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_decodes_what_encode_produced() {
        let raw = encode_cursor("1700000123", "D:/media/a.jpg");
        let cursor = decode_cursor(&raw).expect("游标必须能解回来");
        assert_eq!(cursor.key().as_deref(), Some("1700000123"));
        assert_eq!(cursor.p, "D:/media/a.jpg");
        // 手工改坏的游标当第一页处理，不报错
        assert!(decode_cursor("not-a-cursor").is_none());
    }

    #[test]
    fn legacy_cursor_without_key_still_decodes() {
        // v4.8.x 的游标只有数字 m：升级后翻页途中刷新不能回到第一页
        let legacy = serde_json::json!({ "m": 1_700_000_123u64, "p": "D:/media/a.jpg" });
        let encoded = data_encoding::BASE64URL_NOPAD.encode(&serde_json::to_vec(&legacy).unwrap());
        let cursor = decode_cursor(&encoded).expect("老游标必须还能解");
        assert_eq!(cursor.key().as_deref(), Some("1700000123"));
        assert_eq!(cursor.p, "D:/media/a.jpg");
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

        let mut raw = scan_albums(&root, 3, &["image"], None, false);
        sort_albums(&mut raw, NEWEST);
        assert_eq!(raw.len(), 3, "只该有 a/b/c 三本，空目录与数据目录都不算");

        let mut seen = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        loop {
            let key = album_cursor_key(NEWEST, cursor.as_ref());
            let start = album_page_start(&raw, NEWEST, key.as_ref());
            let end = (start + 2).min(raw.len());
            seen.extend(raw[start..end].iter().map(|item| item.path.clone()));
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                k: album_sort_key(&raw[end - 1], NEWEST.key),
                m: None,
                p: raw[end - 1].path.clone(),
            });
        }
        assert_eq!(seen.len(), 3);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 文件夹墙排序（v4.6.18）：路径升序让同父目录的册子相邻，翻页不重不漏。
    #[test]
    fn albums_by_path_keep_siblings_together() {
        let root = scratch("albums-path");
        for rel in ["zeta/a", "alpha/b", "alpha/c", "beta"] {
            let dir = root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            touch(&dir, "1.jpg", 1_700_000_000);
        }

        let mut raw = scan_albums(&root, 3, &["image"], None, false);
        let by_name = media_index::Sort::new(media_index::SortKey::Name, media_index::SortDir::Asc);
        sort_albums(&mut raw, by_name);
        let names: Vec<&str> = raw.iter().map(|item| item.name.as_str()).collect();
        // alpha 自己不放图，不算一本册；alpha/b、alpha/c 必须相邻
        assert_eq!(
            names,
            vec!["b", "c", "beta", "a"],
            "alpha 下的 b/c 必须紧挨着，跨父目录不会被按时间打散"
        );

        // 分页键与排序键同源：每页 2 本走到底，结果与一次排好的完全一致
        let expected: Vec<String> = raw.iter().map(|item| item.path.clone()).collect();
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<MediaCursor> = None;
        loop {
            let key = album_cursor_key(by_name, cursor.as_ref());
            let start = album_page_start(&raw, by_name, key.as_ref());
            let end = (start + 2).min(raw.len());
            seen.extend(raw[start..end].iter().map(|item| item.path.clone()));
            if end >= raw.len() {
                break;
            }
            cursor = Some(MediaCursor {
                k: album_sort_key(&raw[end - 1], by_name.key),
                m: None,
                p: raw[end - 1].path.clone(),
            });
        }
        assert_eq!(seen, expected);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 快照缓存：落定的目录第二次只读库；目录变了自动重扫；`refresh` 强制穿透。
    #[test]
    fn scan_reuses_snapshot_and_refresh_bypasses_it() {
        use crate::core::dir_cache;

        let root = scratch("snapshot");
        touch(&root, "a.jpg", 1_700_000_001);
        touch(&root, "b.jpg", 1_700_000_002);
        let db_dir =
            std::env::temp_dir().join(format!("asmoday-media-cache-{}", std::process::id()));
        let cache = dir_cache::test_cache(&db_dir);

        let snapshot = |refresh: bool| {
            let (mut raw, _, _) = scan_raw(&root, &["image"], 0, Some(&cache), refresh);
            raw.sort_by(|a, b| a.name.cmp(&b.name));
            raw.iter()
                .map(|item| (item.name.clone(), item.size, item.mtime))
                .collect::<Vec<_>>()
        };

        let first = snapshot(false);
        assert_eq!(first.len(), 2);
        assert_eq!(cache.count_media(), 0, "刚写过的目录不该进缓存");

        // 目录 mtime 要「落定」2 秒才可信，等过去再读一次，这时才会写库
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert_eq!(snapshot(false), first, "缓存命中的结果必须与现扫一致");
        assert_eq!(cache.count_media(), 1, "落定后应当写入媒体快照");

        // 结构变化（新增文件）会推进目录 mtime —— 不 refresh 也能看到
        touch(&root, "c.jpg", 1_700_000_003);
        let after_add = snapshot(false);
        assert_eq!(after_add.len(), 3, "目录变了却只吃了缓存：{after_add:?}");

        // 刚动过的目录又回到「不可缓存」状态，等落定后重扫一次把新快照写进去，
        // 下面才谈得上「读到旧值」
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert_eq!(snapshot(false).len(), 3);

        // 原地覆盖：文件内容与大小变了，但目录 mtime 不动。
        // 不 refresh 会读到缓存里的旧 size —— 这正是「重新扫描」按钮存在的理由。
        std::fs::write(root.join("a.jpg"), vec![b'x'; 4096]).unwrap();
        let cached_size = snapshot(false)
            .into_iter()
            .find(|(name, _, _)| name == "a.jpg")
            .map(|(_, size, _)| size)
            .unwrap_or(0);
        assert_eq!(
            cached_size, 1,
            "缓存本就应该给出旧的大小（否则这个测试没有意义）"
        );

        let refreshed = snapshot(true)
            .into_iter()
            .find(|(name, _, _)| name == "a.jpg")
            .map(|(_, size, _)| size)
            .unwrap_or(0);
        assert_eq!(refreshed, 4096, "refresh 必须真的穿透缓存重扫");
        // 穿透之后顺手把新结果写回，下次不 refresh 也是新的
        assert_eq!(
            snapshot(false)
                .into_iter()
                .find(|(name, _, _)| name == "a.jpg")
                .map(|(_, size, _)| size)
                .unwrap_or(0),
            4096
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    /// 建一个走正式迁移的临时库（媒体索引要用真表）
    fn index_db(tag: &str) -> (std::path::PathBuf, crate::core::media_index::MediaIndex) {
        // 名字前缀必须与 `scratch(tag)` 错开，否则会把待扫的目录一起删掉
        let dir = scratch(&format!("db-{tag}"));
        let db = dir.join("asmoday.db");
        let store = crate::core::auth::AuthStore::new(db.clone());
        store.init().unwrap();
        let index = crate::core::media_index::MediaIndex::new(db);
        (dir, index)
    }

    /// 索引版与内存扫描版必须是同一套口径、同一个顺序（v4.7.0）
    #[test]
    fn index_pages_the_same_tree_as_memory_scan() {
        let root = scratch("index-parity");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for index in 0..5u64 {
            touch(&root, &format!("a{index}.jpg"), 1_700_000_000 + index);
        }
        touch(&root, "clip.mp4", 1_700_000_100);
        touch(&root, "Thumbs.db", 1_700_000_000);
        touch(&root, "icon.svg", 1_700_000_000);
        for index in 0..3u64 {
            touch(&sub, &format!("s{index}.jpg"), 1_700_000_500 + index);
        }

        let (db_dir, index) = index_db("parity");
        // 深度 1：根 + sub，两层的媒体都进索引；系统垃圾与 svg 不进
        let scanned = sync_index(&index, &root, 1, SyncMode::Request, None).expect("索引不可用");
        assert!(scanned >= 11, "至少要读到根与 sub 两层的条目");

        let scope = Scope::new(&fs::normalize_display(&root), 1);
        let (assets, more) = index
            .with_conn(|conn| {
                media_index::page(conn, &scope, &["image", "video"], NEWEST, None, 100)
            })
            .expect("查询失败");
        assert!(!more);

        let indexed: Vec<String> = assets.iter().map(|asset| asset.path.clone()).collect();
        let (mut raw, _, _) = scan_raw(&root, &["image", "video"], 1, None, true);
        sort_media(&mut raw, NEWEST);
        let expected: Vec<String> = raw.iter().map(|item| item.display.clone()).collect();
        assert_eq!(indexed, expected, "顺序与内存扫描必须完全一致");

        let counts = index
            .with_conn(|conn| media_index::totals(conn, &scope))
            .expect("计数失败");
        assert_eq!(counts, (8, 1, 0), "五张根图 + 三张 sub 图 + 一个视频");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    /// 窗口期内不再碰树；refresh 必须穿透（「重新扫描」按钮）
    #[test]
    fn index_window_skips_the_walk_until_refreshed() {
        let root = scratch("index-window");
        touch(&root, "a.jpg", 1_700_000_000);
        let (db_dir, index) = index_db("window");

        assert!(
            sync_index(&index, &root, 1, SyncMode::Request, None).unwrap() > 0,
            "第一次要真扫"
        );
        assert_eq!(
            sync_index(&index, &root, 1, SyncMode::Request, None),
            Some(0),
            "窗口期内必须一个目录都不碰"
        );
        assert!(
            sync_index(&index, &root, 1, SyncMode::Force, None).unwrap() > 0,
            "refresh 必须穿透窗口"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    /// 后台轮询（v4.8.0）：不服从新鲜窗口，但目录戳一致时不重写行
    ///
    /// 两件事都要成立 —— 只绕过窗口不跳过写库，等于每 5 分钟把整库重写一遍；
    /// 只跳过写库不绕过窗口，后台任务第一行就返回，永远不干活。
    #[test]
    fn background_sync_ignores_window_but_reuses_rows() {
        let root = scratch("bg-sync");
        touch(&root, "a.jpg", 1_700_000_000);
        touch(&root, "b.mp4", 1_700_000_100);
        let (db_dir, index) = index_db("bg-sync");

        assert!(sync_index(&index, &root, 1, SyncMode::Request, None).unwrap() > 0);
        // 记下隐式 rowid：replace_dir 是「先删后插」，重写就会换一批新 rowid
        let ids_before: Vec<i64> = index
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT rowid FROM media_assets ORDER BY path")?;
                let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
                Ok(rows.filter_map(Result::ok).collect::<Vec<i64>>())
            })
            .expect("查询失败");

        // 请求模式：窗口内一步不动
        assert_eq!(
            sync_index(&index, &root, 1, SyncMode::Request, None),
            Some(0)
        );
        // 后台模式：照样走树（返回条目数 > 0），但戳没变 → 一行都不重写
        assert!(
            sync_index(&index, &root, 1, SyncMode::Background, None).unwrap() > 0,
            "后台轮询必须绕过窗口"
        );
        let ids_after: Vec<i64> = index
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT rowid FROM media_assets ORDER BY path")?;
                let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
                Ok(rows.filter_map(Result::ok).collect::<Vec<i64>>())
            })
            .expect("查询失败");
        assert_eq!(ids_before, ids_after, "目录戳没变就不该重写行");

        // 目录里多一个文件 → 下一轮后台必须把它收进来
        touch(&root, "c.jpg", 1_700_000_200);
        assert!(sync_index(&index, &root, 1, SyncMode::Background, None).unwrap() > 0);
        let total = index
            .with_conn(|conn| {
                media_index::totals(conn, &Scope::new(&fs::normalize_display(&root), 1))
            })
            .expect("计数失败");
        assert_eq!(total.0, 2, "新增的那张图要被后台轮询发现");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&db_dir);
    }
}
