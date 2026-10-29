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
use std::sync::Mutex;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::core::{ffmpeg, fs, fs_ops, path_guard, thumbnail};

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

/// GET /api/system/backup：下载一份数据库热备份（设计文档 4.1）
///
/// 备份里是密码哈希、TOTP 密钥与会话，属于高敏操作，所以：
/// - 中间件要求「5 分钟内验证过密码」（`E_REAUTH_REQUIRED`，见 `require_auth`）；
/// - 响应 `no-store` + `attachment`，绝不能被缓存或内联渲染。
pub async fn backup(State(state): State<AppState>) -> Response {
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
                thumbnail::ThumbOutcome::Placeholder => status.failed += 1,
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
