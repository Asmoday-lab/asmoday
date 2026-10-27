//! 系统状态接口（设计文档 6.2）。
//!
//! - `GET  /api/system/ffmpeg-status`：按三层优先级探测当前生效的 FFmpeg
//! - `POST /api/system/test-ffmpeg`：验证设置页里填的某个路径是否可用
//!
//! 两者都只跑 `ffmpeg -version`，不碰任何转码。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::core::ffmpeg;
use crate::AppState;

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
pub async fn test_ffmpeg(State(_state): State<AppState>, body: Option<Json<TestPayload>>) -> Response {
    let path = body
        .map(|Json(payload)| payload.path)
        .unwrap_or_default();

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
async fn probe(path: Option<std::path::PathBuf>, source: ffmpeg::FfmpegSource) -> serde_json::Value {
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
