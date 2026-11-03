//! 系统状态接口（设计文档 6.2）。
//!
//! - `GET  /api/system/ffmpeg-status`：按三层优先级探测当前生效的 FFmpeg
//! - `POST /api/system/test-ffmpeg`：验证设置页里填的某个路径是否可用
//! - `GET  /api/system/health`：版本与运行时长
//! - `POST /api/system/cleanup/{thumbnails,parts,shares}`：三类清理入口
//! - `POST /api/system/thumbnails/rebuild` + `GET .../rebuild/status`：
//!   批量重建缩略图（单线程队列，防低功耗设备 CPU 打爆，设计文档 3.4.2.1）
//!
//! 探测类接口都只跑 `ffmpeg -version` / `-encoders`，不碰任何转码。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::core::{ffmpeg, fs, fs_ops, media_index, path_guard, thumbnail};

/// 批量重建缩略图的进度（进程内单份，重启即清零）
#[derive(Debug, Clone, Default, Serialize)]
pub struct RebuildStatus {
    pub is_running: bool,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
}

/// 进度状态容器
pub type RebuildState = Mutex<RebuildStatus>;

/// 单次重建的候选上限：低功耗设备上一口气扫几十万张没有意义
const REBUILD_SCAN_LIMIT: usize = 5000;

/// 预生成用哪一档宽度：与前端网格的 `THUMB_WIDTH.tile` 保持一致，
/// 否则生成的图与浏览时请求的键不同，等于白跑。
const PREHEAT_WIDTH: u32 = 400;

/// `GET /api/system/ffmpeg-status`
pub async fn ffmpeg_status(State(state): State<AppState>) -> Response {
    let configured = {
        let config = state.config.read().await;
        config.media.ffmpeg_path.clone()
    };

    let resolved = ffmpeg::resolve(&configured);
    let body = probe(resolved.path, resolved.source).await;
    (StatusCode::OK, Json(body)).into_response()
}

#[derive(Deserialize)]
pub struct TestPayload {
    /// 待测试的路径；留空表示「按当前优先级探测」
    #[serde(default)]
    pub path: String,
}

/// `POST /api/system/test-ffmpeg`
///
/// 只测用户填的那一条（不回落），这样「该路径不可用」才是准确结论。
pub async fn test_ffmpeg(
    State(_state): State<AppState>,
    body: Option<Json<TestPayload>>,
) -> Response {
    let path = body.map(|Json(payload)| payload.path).unwrap_or_default();

    if path.trim().is_empty() {
        // 空路径等价于「测当前生效的那个」，语义最不容易被误解
        return ffmpeg_status(State(_state)).await;
    }

    // 允许直接填目录（安装包解压出来常是 `...\ffmpeg\bin`）
    let candidate = ffmpeg::executable(&path);
    let body = probe(Some(candidate), ffmpeg::FfmpegSource::Config).await;
    (StatusCode::OK, Json(body)).into_response()
}

/// 在阻塞线程里跑 `-version` / `-encoders`，把结果拼成前端约定的结构。
///
/// `webp_support` 决定缩略图与视频封面能否输出 WebP（设计文档 3.4 / 3.5）。
async fn probe(
    path: Option<std::path::PathBuf>,
    source: ffmpeg::FfmpegSource,
) -> serde_json::Value {
    let Some(bin) = path else {
        return json!({
            "status": "missing",
            "version": null,
            "path": null,
            "source": source.as_str(),
            "webp_support": false,
        });
    };

    let probe_path = bin.clone();
    let (version, caps) = tokio::task::spawn_blocking(move || {
        let version = ffmpeg::probe_version(&probe_path);
        // 探测不到版本就不必再跑一次 -encoders
        let caps = match version {
            Some(_) => ffmpeg::probe_capabilities(&probe_path),
            None => ffmpeg::Capabilities::default(),
        };
        (version, caps)
    })
    .await
    .unwrap_or((None, ffmpeg::Capabilities::default()));

    match version {
        Some(version) => json!({
            "status": "ok",
            "version": version,
            "path": bin.display().to_string(),
            "source": source.as_str(),
            "webp_support": caps.webp,
        }),
        None => json!({
            "status": "missing",
            "version": null,
            "path": bin.display().to_string(),
            "source": source.as_str(),
            "webp_support": false,
        }),
    }
}

// ---------------------------------------------------------------------------
// 健康检查 / 清理入口（设计文档 6.3）
// ---------------------------------------------------------------------------

/// GET /api/system/health
pub async fn health(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "build": option_env!("ASMODAY_BUILD_ID").unwrap_or("dev"),
        "uptime": state.started.elapsed().as_secs(),
    }))
    .into_response()
}

/// POST /api/system/cleanup/thumbnails
pub async fn cleanup_thumbnails(State(state): State<AppState>) -> Response {
    let thumbnails = state.thumbnails.clone();
    // 先按当前配置刷新一次上限，避免用旧配置淘汰
    {
        let config = state.config.read().await;
        thumbnails.apply_limits(config.media.limit_bytes(), config.media.max_age_secs());
    }
    match tokio::task::spawn_blocking(move || thumbnails.run_cleanup()).await {
        Ok(Ok(bytes)) => Json(json!({ "status": "ok", "removed_bytes": bytes })).into_response(),
        Ok(Err(err)) => crate::api::error::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("thumbnail cleanup failed: {err}"),
        ),
        Err(err) => crate::api::error::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("thumbnail cleanup failed: {err}"),
        ),
    }
}

/// POST /api/system/cleanup/parts：只清 24 小时前的 `.part`
pub async fn cleanup_parts(State(state): State<AppState>) -> Response {
    let roots = mount_roots(&state).await;
    let summary = tokio::task::spawn_blocking(move || {
        let mut removed = 0usize;
        let mut freed = 0u64;
        for root in roots {
            let (count, bytes) =
                fs_ops::cleanup_parts_older_than(&root, std::time::Duration::from_secs(24 * 3600));
            removed += count;
            freed += bytes;
        }
        (removed, freed)
    })
    .await;

    match summary {
        Ok((removed, freed)) => Json(json!({
            "status": "ok",
            "removed": removed,
            "removed_bytes": freed,
        }))
        .into_response(),
        Err(err) => crate::api::error::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("part cleanup failed: {err}"),
        ),
    }
}

/// POST /api/system/backup：下载一份数据库热备份（设计文档 4.1）
///
/// 备份里是密码哈希、TOTP 密钥与会话，属于高敏操作，所以：
/// - **只接受 POST**：GET 会被一次顶级导航 CSRF 拉走（SameSite=Lax 允许
///   顶级导航携带 Cookie），改 POST 后有写方法同源校验兜底；
/// - 要求 **15 分钟内验证过密码**（`POST /api/auth/verify-password`），
///   没验证过回 403 `REVERIFY_REQUIRED`，前端弹密码框。这是单次操作的
///   确认，不是全局闸门——终端与日常操作不受影响；
/// - 响应 `no-store` + `attachment`，绝不能被缓存或内联渲染。
pub async fn backup(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    let verified = crate::api::auth::cookie_token(&headers)
        .map(|token| state.auth.session_verified_recently(&token))
        .unwrap_or(false);
    if !verified {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "code": "REVERIFY_REQUIRED",
                    "message": "Password verification required before downloading a backup",
                }
            })),
        )
            .into_response();
    }

    let stamp = crate::core::auth::now_secs();
    let temp = state
        .data_dir
        .join("backups")
        .join(format!(".backup-{stamp}-{}.db", std::process::id()));
    let auth = std::sync::Arc::clone(&state.auth);
    let target = temp.clone();

    let produced = tokio::task::spawn_blocking(move || auth.backup_to(&target)).await;
    match produced {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            return crate::api::error::detail(StatusCode::INTERNAL_SERVER_ERROR, err);
        }
        Err(err) => {
            return crate::api::error::detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("backup task failed: {err}"),
            );
        }
    }

    // 读进内存后立刻删掉临时文件：备份是一次性的下载物，留在数据目录里
    // 只会变成「谁也不知道该不该删」的垃圾（也可能被误当作线上库）。
    let bytes = match tokio::fs::read(&temp).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp).await;
            return crate::api::error::detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read backup failed: {err}"),
            );
        }
    };
    let _ = tokio::fs::remove_file(&temp).await;

    let filename = format!("asmoday-{}.db", utc_stamp(stamp));
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        axum::body::Body::from(bytes),
    )
        .into_response()
}

/// UNIX 秒 → `YYYYMMDD-HHMMSS`（UTC），只为给下载文件起个可读名字。
///
/// 不引 chrono：需要的就是十几行（Howard Hinnant 的 civil_from_days）。
fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// POST /api/system/cleanup/shares：清理 30 天前的分享 tombstone
pub async fn cleanup_shares(State(state): State<AppState>) -> Response {
    let auth = state.auth.clone();
    let now = crate::core::auth::now_secs();
    match tokio::task::spawn_blocking(move || auth.purge_share_tombstones(now)).await {
        Ok(Ok(removed)) => Json(json!({ "status": "ok", "removed": removed })).into_response(),
        Ok(Err(err)) => crate::api::error::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("share cleanup failed: {err}"),
        ),
        Err(err) => crate::api::error::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("share cleanup failed: {err}"),
        ),
    }
}

async fn mount_roots(state: &AppState) -> Vec<PathBuf> {
    let config = state.config.read().await;
    path_guard::load(&config, &state.cwd)
        .mounts
        .into_iter()
        .filter_map(|mount| mount.root)
        .collect()
}

// ---------------------------------------------------------------------------
// 批量重建缩略图（设计文档 3.4.2.1）
// ---------------------------------------------------------------------------

/// POST /api/system/thumbnails/rebuild：立即返回，进度靠轮询
pub async fn rebuild_thumbnails(State(state): State<AppState>) -> Response {
    {
        let mut status = match state.rebuild.lock() {
            Ok(status) => status,
            Err(_) => {
                return crate::api::error::detail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "rebuild state poisoned",
                );
            }
        };
        if status.is_running {
            return Json(json!({ "status": "already_running" })).into_response();
        }
        *status = RebuildStatus {
            is_running: true,
            ..Default::default()
        };
    }

    let state_clone = state.clone();
    tokio::spawn(async move {
        run_rebuild(state_clone).await;
    });

    Json(json!({ "status": "started" })).into_response()
}

/// POST /api/system/preheat-thumbnails
///
/// 预生成缩略图：把「最近 N 天的图片」提前渲好，之后浏览就是直接命中本地缓存，
/// 不再每张都从网盘读原图（那是图片库卡顿的唯一原因）。
///
/// 候选直接从**索引**取，不再走一遍文件系统；已生成的在 `get_prepared` 里
/// 先查缓存就返回，所以重复调用是安全的（幂等）。
pub async fn preheat_thumbnails(State(state): State<AppState>) -> Response {
    {
        let mut status = match state.preheat.lock() {
            Ok(status) => status,
            Err(_) => {
                return crate::api::error::detail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "preheat state poisoned",
                );
            }
        };
        if status.is_running {
            return Json(json!({ "status": "already_running" })).into_response();
        }
        *status = RebuildStatus {
            is_running: true,
            ..Default::default()
        };
    }

    let worker = state.clone();
    tokio::spawn(async move {
        run_preheat(worker).await;
    });

    Json(json!({ "status": "started" })).into_response()
}

/// GET /api/system/preheat-thumbnails/status
pub async fn preheat_status(State(state): State<AppState>) -> Response {
    let status = state
        .preheat
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    Json(status).into_response()
}

/// POST /api/system/preheat-thumbnails/stop
///
/// 只置一个标志，worker 每张之间检查一次 —— 正在生成的那张会跑完。
pub async fn preheat_stop(State(state): State<AppState>) -> Response {
    if let Ok(mut status) = state.preheat.lock() {
        status.is_running = false;
    }
    Json(json!({ "status": "stopping" })).into_response()
}

/// 索引扫描后顺带补一小批（v4.9.11）。
///
/// 与手动「全量预生成」共用同一份状态：正在手动跑就直接跳过，不抢带宽。
pub async fn preheat_batch(state: &AppState, limit: usize) {
    if limit == 0 {
        return;
    }
    {
        let Ok(mut status) = state.preheat.lock() else {
            return;
        };
        if status.is_running {
            return;
        }
        *status = RebuildStatus {
            is_running: true,
            ..Default::default()
        };
    }
    run_preheat_inner(state, Some(limit)).await;
}

/// 预生成的实际执行体。
///
/// 并发刻意压到 3 路：缩略图全局信号量是「核数 - 1」，预生成只借其中三份，
/// 剩下的留给正在浏览的请求，不会把页面卡成一片灰块。
async fn run_preheat(state: AppState) {
    run_preheat_inner(&state, None).await;
}

/// `limit` 为 `None` 表示按时间窗把候选全部补完；`Some(n)` 只补 n 张（索引后的小批）。
async fn run_preheat_inner(state: &AppState, limit: Option<usize>) {
    const WORKERS: usize = 3;
    const MAX_CANDIDATES: usize = 200_000;
    let batch_limit = limit.unwrap_or(MAX_CANDIDATES).min(MAX_CANDIDATES);

    let (days, ffmpeg_path) = {
        let config = state.config.read().await;
        (
            config.media.thumb_preheat_days,
            config.media.ffmpeg_path.clone(),
        )
    };
    let since = if days == 0 {
        0
    } else {
        let now = crate::core::auth::now_secs().max(0) as u64;
        now.saturating_sub(days.saturating_mul(24 * 3600))
    };

    let index = Arc::clone(&state.media_index);
    let rows = match tokio::task::spawn_blocking(move || {
        index.with_conn(|conn| Ok(media_index::images_for_preheat(conn, since, batch_limit)))
    })
    .await
    {
        Ok(Some(rows)) => rows,
        _ => Vec::new(),
    };

    if rows.is_empty() {
        if let Ok(mut status) = state.preheat.lock() {
            *status = RebuildStatus::default();
        }
        crate::log_info!("thumbnail preheat: nothing to do (index empty or window too narrow)");
        return;
    }

    let total = rows.len();
    if let Ok(mut status) = state.preheat.lock() {
        status.total = total;
        status.completed = 0;
        status.failed = 0;
    }
    crate::log_info!(
        "thumbnail preheat started: {} image(s), window {} day(s)",
        total,
        if days == 0 { 0 } else { days }
    );

    let ffmpeg_bin = crate::core::ffmpeg::resolve(&ffmpeg_path)
        .path
        .filter(|bin| bin.is_file());

    // 共享任务队列：三个 worker 各自取一张，互不等待
    let queue = Arc::new(std::sync::Mutex::new(rows.into_iter()));
    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let queue = Arc::clone(&queue);
        let worker = state.clone();
        let ffmpeg_bin = ffmpeg_bin.clone();
        workers.push(tokio::spawn(async move {
            loop {
                // 停止信号：每张之间检查，正在跑的那张会跑完
                if !worker
                    .preheat
                    .lock()
                    .map(|status| status.is_running)
                    .unwrap_or(false)
                {
                    break;
                }
                let job = match queue.lock() {
                    Ok(mut guard) => guard.next(),
                    Err(_) => break,
                };
                let Some((real, mtime, size)) = job else {
                    break;
                };

                let path = PathBuf::from(&real);
                // 键直接由索引里的 size/mtime 算出，不用为「判断有没有缓存」再 stat 一次
                let key = crate::core::thumbnail::ThumbKey {
                    width: PREHEAT_WIDTH,
                    fingerprint: crate::core::thumbnail::Thumbnailer::fingerprint_from(size, mtime),
                    fresh_write: false,
                };
                let (outcome, _) = worker
                    .thumbnails
                    .get_prepared(&path, key, ffmpeg_bin.as_deref())
                    .await;
                let ok = matches!(outcome, crate::core::thumbnail::ThumbOutcome::Ready(_));

                if let Ok(mut status) = worker.preheat.lock() {
                    status.completed += 1;
                    if !ok {
                        status.failed += 1;
                    }
                }
                // 让出一点点：预生成是背景活，不该把浏览的带宽吃干
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }));
    }
    for handle in workers {
        let _ = handle.await;
    }

    let (completed, failed) = match state.preheat.lock() {
        Ok(mut status) => {
            status.is_running = false;
            (status.completed, status.failed)
        }
        Err(_) => (0, 0),
    };
    crate::log_info!(
        "thumbnail preheat finished: {}/{} generated, {} failed",
        completed.saturating_sub(failed),
        total,
        failed
    );
}

/// GET /api/system/thumbnails/rebuild/status
pub async fn rebuild_status(State(state): State<AppState>) -> Response {
    let status = state
        .rebuild
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    Json(status).into_response()
}

async fn run_rebuild(state: AppState) {
    let roots = mount_roots(&state).await;
    let desktop_only = false;
    let _ = desktop_only;

    let candidates = tokio::task::spawn_blocking(move || collect_candidates(&roots))
        .await
        .unwrap_or_default();

    {
        if let Ok(mut status) = state.rebuild.lock() {
            status.total = candidates.len();
        }
    }

    let configured = {
        let config = state.config.read().await;
        config.media.ffmpeg_path.clone()
    };
    let ffmpeg_bin = ffmpeg::resolve(&configured)
        .path
        .filter(|bin| bin.is_file());

    for (path, is_video) in candidates {
        let (outcome, fresh) = if is_video {
            match ffmpeg_bin.as_deref() {
                Some(bin) => (state.thumbnails.video_poster(&path, bin, 320).await, None),
                None => (thumbnail::ThumbOutcome::Placeholder, None),
            }
        } else {
            state
                .thumbnails
                .get(&path, 320, ffmpeg_bin.as_deref())
                .await
        };

        // 新鲜 ThumbHash 入库（v2.8.2：SQLite，不再落 .th 旁路文件）
        if let Some(fresh) = fresh {
            state.auth.thumb_hash_set(&fresh.key, &fresh.value);
        }

        if let Ok(mut status) = state.rebuild.lock() {
            match outcome {
                thumbnail::ThumbOutcome::Ready(_) => status.completed += 1,
                // 拥塞（Busy）与真失败一样算这一轮没出图：重跑一次就补上
                thumbnail::ThumbOutcome::Busy | thumbnail::ThumbOutcome::Placeholder => {
                    status.failed += 1
                }
            }
        }
        // 单线程 + 主动让出：重建期间普通请求仍然能得到响应
        tokio::task::yield_now().await;
    }

    if let Ok(mut status) = state.rebuild.lock() {
        status.is_running = false;
    }
    crate::log_info!("thumbnail rebuild finished");
}

/// 收集需要重建的图片/视频（跳过已有有效缓存的项由 `get` 内部处理）
fn collect_candidates(roots: &[PathBuf]) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    for root in roots {
        walk_candidates(root, &mut out);
    }
    out.truncate(REBUILD_SCAN_LIMIT);
    out
}

fn walk_candidates(dir: &Path, out: &mut Vec<(PathBuf, bool)>) {
    if out.len() >= REBUILD_SCAN_LIMIT {
        return;
    }
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        if out.len() >= REBUILD_SCAN_LIMIT {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || path_guard::is_part_file(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_candidates(&path, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let ext = fs::extension_of_name(&name);
        let (kind, _) = fs::classify_name(&name, &ext, false);
        match kind {
            "image" => out.push((path, false)),
            "video" => out.push((path, true)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;

    #[test]
    fn utc_stamp_matches_known_instants() {
        // 1970-01-01T00:00:00Z
        assert_eq!(utc_stamp(0), "19700101-000000");
        // 2023-11-14T22:13:20Z
        assert_eq!(utc_stamp(1_700_000_000), "20231114-221320");
        // 闰日：2024-02-29T12:00:00Z
        assert_eq!(utc_stamp(1_709_208_000), "20240229-120000");
    }
}
