//! 媒体库接口（设计文档 3.4）：缩略图 + 媒体索引。
//!
//! 媒体索引只做「按目录扫描 + 分类」，不做刮削、不联网。
//! 视频封面可选：配置了外部 FFmpeg 才截首帧，否则回占位图。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::ffmpeg;
use crate::core::fs::{self, FsError};
use crate::core::thumbnail::{PLACEHOLDER_SVG, ThumbOutcome};
use crate::AppState;

/// 单次扫描的条目上限，防超大目录拖垮后端
const SCAN_LIMIT: usize = 4000;
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

#[derive(Debug, Serialize)]
pub struct BrowseResult {
    pub root: String,
    pub items: Vec<MediaItem>,
    pub scanned: usize,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct AlbumsQuery {
    pub path: Option<String>,
    /// 目录树向下钻取的层数（每一层里「直接含媒体文件」的目录都算一本相册）
    pub depth: Option<usize>,
    /// image / video / audio（默认 image）——影视库与音乐库的「按文件夹」视图共用本接口
    pub kind: Option<String>,
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

async fn resolve_default(state: &AppState, requested: Option<&str>) -> Result<PathBuf, FsError> {
    let roots = roots_for(state).await;
    match requested.map(str::trim).filter(|p| !p.is_empty()) {
        Some(raw) => fs::resolve_existing(raw, &roots.allowed),
        None => {
            let Some(root) = roots.default_root() else {
                return Err(FsError::NotFound("没有可用的挂载点".to_string()));
            };
            let path = root.path.clone();
            fs::resolve_existing(&path, &roots.allowed)
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

    let read = tokio::task::spawn_blocking(move || crate::core::id3::read_cover(&path)).await;
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

    let scan_root = root.clone();
    let thumbnails = Arc::clone(&state.thumbnails);
    let auth_store = Arc::clone(&state.auth);
    let outcome = tokio::task::spawn_blocking(move || {
        scan(&scan_root, &wanted, depth, &thumbnails, &auth_store)
    })
    .await;

    match outcome {
        Ok((items, scanned, truncated)) => Json(BrowseResult {
            root: fs::normalize_display(&root),
            items,
            scanned,
            truncated,
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
    let scan_root = root.clone();
    let auth_store = Arc::clone(&state.auth);
    let outcome = tokio::task::spawn_blocking(move || {
        scan_albums(&scan_root, depth, &wanted, &auth_store)
    })
    .await;

    match outcome {
        Ok((items, truncated)) => Json(json!({
            "root": fs::normalize_display(&root),
            "items": items,
            "truncated": truncated,
        }))
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

/// 单次扫描里最多为多少张图片读取 ThumbHash（避免上万条时把请求拖慢）
const HASH_LIMIT: usize = 800;

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
    let bytes = data_encoding::BASE64.decode(hash_b64.trim().as_bytes()).ok()?;
    let (r, g, b, _) = thumbhash::thumb_hash_to_average_rgba(&bytes).ok()?;
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(r),
        channel(g),
        channel(b)
    ))
}

fn scan(
    root: &Path,
    wanted: &[&str],
    depth: usize,
    thumbnails: &crate::core::thumbnail::Thumbnailer,
    auth: &crate::core::auth::AuthStore,
) -> (Vec<MediaItem>, usize, bool) {
    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    // 参与取色的图片下标 + 源路径（走完目录后统一批量查库，HASH_LIMIT 内）
    let mut hash_candidates: Vec<(usize, PathBuf)> = Vec::new();

    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, level)) = queue.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            scanned += 1;
            if out.len() >= SCAN_LIMIT {
                truncated = true;
                return (out, scanned, truncated);
            }

            let path = entry.path();
            let Ok(file_type) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();

            // 系统垃圾（Thumbs.db / .DS_Store / ._* / ~$*）一律不进媒体库，
            // 否则相册里会混进一堆打不开的元数据文件（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }

            if file_type.is_dir() {
                if level + 1 <= depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
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
            if !wanted.contains(&kind) {
                continue;
            }

            let meta = entry.metadata().ok();
            let tags = if kind == "audio" {
                crate::core::id3::read_tags(&path)
            } else {
                Default::default()
            };
            let subtitle = if kind == "video" {
                find_subtitle(&path)
            } else {
                None
            };
            // ThumbHash 在目录走完后统一从 SQLite 批量取（v2.8.2），首次浏览回退纯色占位
            let is_image = kind == "image";
            let has_hash_slot = is_image && hash_candidates.len() < HASH_LIMIT;
            out.push(MediaItem {
                name,
                path: fs::normalize_display(&path),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                kind,
                ext,
                folder: fs::normalize_display(&dir),
                title: tags.title,
                artist: tags.artist,
                album: tags.album,
                cover: tags.has_cover,
                hash: None,
                color: None,
                subtitle,
            });
            if has_hash_slot {
                hash_candidates.push((out.len() - 1, path));
            }
        }
    }

    // 批量取 ThumbHash：一次连接逐键查 SQLite；库未命中的话顺手把存量 .th
    // 旁路文件搬进库（v2.8.2 升级路径），搬完即删，老部署零手工迁移。
    if !hash_candidates.is_empty() {
        let keys: Vec<String> = hash_candidates
            .iter()
            .map(|(_, path)| crate::core::thumbnail::Thumbnailer::hash_key(path))
            .collect();
        let mut hits = auth.thumb_hashes_get_many(&keys);
        let mut migrated: Vec<(String, String)> = Vec::new();
        for ((index, _), key) in hash_candidates.iter().zip(keys.iter()) {
            let mut value = hits.remove(key);
            if value.is_none() {
                // 键名本身就是旁路文件名（`<hash>.th`）
                let sidecar = thumbnails.cache_dir().join(key);
                if let Ok(raw) = std::fs::read_to_string(&sidecar) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        value = Some(trimmed.to_string());
                        migrated.push((key.clone(), trimmed.to_string()));
                        let _ = std::fs::remove_file(&sidecar);
                    }
                }
            }
            if let Some(hash) = value {
                if let Some(item) = out.get_mut(*index) {
                    item.hash = Some(hash.clone());
                    item.color = average_color(&hash);
                }
            }
        }
        auth.thumb_hashes_set_many(&migrated);
    }

    // 新的在前
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    (out, scanned, truncated)
}

// ---------------------------------------------------------------------------
// 照片库相册视图（设计文档 3.4.3）
// ---------------------------------------------------------------------------

/// 单个相册最多扫多少个目录项（防止一本相册把请求拖死）
const ALBUM_SCAN_LIMIT: usize = 20_000;

fn scan_albums(
    root: &Path,
    depth: usize,
    wanted: &[&str],
    auth: &crate::core::auth::AuthStore,
) -> (Vec<Album>, bool) {
    let mut albums: Vec<Album> = Vec::new();
    // 与 albums 一一对应的封面真实路径（取 ThumbHash 用；展示串已换成 `/` 分隔，
    // 拿展示串算 key 会与缩略图生成时的 key 不一致）
    let mut covers: Vec<Option<PathBuf>> = Vec::new();
    let mut truncated = false;
    let mut scanned = 0usize;

    // 与 v2.8 相册视图同一口径（设计文档 3.4.3）：遍历目录树，**每个直接含有
    // 图片的目录**都记一本相册——包括扫描起点（挂载目录）自己；计数只看该目录
    // 第一层，点进相册也只列第一层，卡片上的条数才和看到的一致。
    let mut stack: Vec<(PathBuf, usize, bool)> = vec![(root.to_path_buf(), 0, true)];

    'walk: while let Some((dir, level, is_root)) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else { continue };
        let mut count = 0usize;
        // (mtime, path)：相册封面取最新的一张图
        let mut newest: Option<(u64, PathBuf)> = None;

        for entry in reader.flatten() {
            scanned += 1;
            if scanned > ALBUM_SCAN_LIMIT {
                truncated = true;
                break 'walk;
            }
            let Ok(file_type) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            // 系统垃圾（Thumbs.db / .DS_Store / ._*）一律不进媒体库（设计文档 3.3.7）
            if crate::core::junk::is_system_junk(&name) {
                continue;
            }

            if file_type.is_dir() {
                if level < depth
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.to_lowercase().as_str())
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
            if newest.as_ref().map(|(best, _)| mtime > *best).unwrap_or(true) {
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
        let (modified, cover_source) = match newest {
            Some((mtime, path)) => (Some(mtime), Some(path)),
            None => (None, None),
        };
        albums.push(Album {
            name,
            path: fs::normalize_display(&dir),
            count,
            is_root,
            cover: cover_source.as_ref().map(|path| fs::normalize_display(path)),
            cover_hash: None,
            cover_color: None,
            modified,
        });
        covers.push(cover_source);
    }

    // 封面 ThumbHash 一次性批量取（与媒体列表同一条惰性迁移路径）
    let lookup: Vec<(usize, PathBuf)> = covers
        .iter()
        .enumerate()
        .filter_map(|(index, path)| path.as_ref().map(|path| (index, path.clone())))
        .collect();
    if !lookup.is_empty() {
        let keys: Vec<String> = lookup
            .iter()
            .map(|(_, path)| crate::core::thumbnail::Thumbnailer::hash_key(path))
            .collect();
        let hits = auth.thumb_hashes_get_many(&keys);
        for ((index, _), key) in lookup.iter().zip(keys.iter()) {
            if let Some(hash) = hits.get(key) {
                if let Some(album) = albums.get_mut(*index) {
                    album.cover_hash = Some(hash.clone());
                    album.cover_color = average_color(hash);
                }
            }
        }
    }

    // 新的在前；没有 mtime 的沉底
    albums.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    (albums, truncated)
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
    let target = match fs::resolve_existing(&body.path, &roots.allowed) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    let display = fs::normalize_display(&target);

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let result = conn.execute(
        "INSERT INTO play_history (file_path, last_played_at, progress) VALUES (?1, ?2, ?3)
         ON CONFLICT(file_path) DO UPDATE SET last_played_at = ?2, progress = ?3",
        rusqlite::params![display, crate::core::auth::now_secs(), body.progress.max(0)],
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
        "SELECT file_path, last_played_at, progress FROM play_history
         ORDER BY last_played_at DESC LIMIT 200",
    ) {
        Ok(statement) => statement,
        Err(err) => return fs_error(FsError::Io(format!("查询播放历史失败：{err}"))),
    };
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "path": row.get::<_, String>(0)?,
            "last_played_at": row.get::<_, Option<i64>>(1)?,
            "progress": row.get::<_, Option<i64>>(2)?,
        }))
    });
    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() })).into_response(),
        Err(err) => fs_error(FsError::Io(format!("查询播放历史失败：{err}"))),
    }
}

/// POST /api/media/favorite：收藏 / 取消收藏
pub async fn favorite(State(state): State<AppState>, Json(body): Json<FavoriteBody>) -> Response {
    let roots = roots_for(&state).await;
    let target = match fs::resolve_existing(&body.path, &roots.allowed) {
        Ok(path) => path,
        Err(err) => return fs_error(err),
    };
    let display = fs::normalize_display(&target);

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return fs_error(FsError::Io(err)),
    };
    let result = if body.favorite {
        conn.execute(
            "INSERT OR IGNORE INTO favorites (file_path, created_at) VALUES (?1, ?2)",
            rusqlite::params![display, crate::core::auth::now_secs()],
        )
    } else {
        conn.execute("DELETE FROM favorites WHERE file_path = ?1", [&display])
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
        .prepare("SELECT file_path, created_at FROM favorites ORDER BY created_at DESC")
    {
        Ok(statement) => statement,
        Err(err) => return fs_error(FsError::Io(format!("查询收藏失败：{err}"))),
    };
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "path": row.get::<_, String>(0)?,
            "created_at": row.get::<_, Option<i64>>(1)?,
        }))
    });
    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() })).into_response(),
        Err(err) => fs_error(FsError::Io(format!("查询收藏失败：{err}"))),
    }
}

// ---------------------------------------------------------------------------
// 缩略图缓存管理（设计文档 3.4：设置页「清理媒体缓存」+ 容量可视化）
// ---------------------------------------------------------------------------

/// GET /api/media/cache：缓存占用与当前策略
pub async fn cache_stats(State(state): State<AppState>) -> Response {
    let dir = state.thumbnails.cache_dir().to_path_buf();
    let (files, bytes) = tokio::task::spawn_blocking(move || {
        let mut files = 0usize;
        let mut bytes = 0u64;
        if let Ok(reader) = std::fs::read_dir(&dir) {
            for entry in reader.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        files += 1;
                        bytes += meta.len();
                    }
                }
            }
        }
        (files, bytes)
    })
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
