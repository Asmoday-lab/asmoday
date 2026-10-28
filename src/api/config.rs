//! 配置管理接口（设计文档 6.2）。
//!
//! - `GET  /api/config`：返回解析后的 JSON 配置
//! - `POST /api/config`：接收 JSON，校验后序列化为 TOML 落盘，并热更新内存中的配置

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::core::config::Config;
use crate::AppState;

/// GET /api/config
pub async fn get_config(State(state): State<AppState>) -> Json<Config> {
    let config = state.config.read().await;
    Json(config.clone())
}

/// GET /api/config/safety：危险配置自检结果（设置页警告条，设计文档 3.5 / 9.3）
pub async fn get_safety(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.read().await;
    Json(serde_json::json!({ "issues": config.safety_issues() }))
}

/// POST /api/config
pub async fn update_config(
    State(state): State<AppState>,
    payload: Result<Json<Config>, JsonRejection>,
) -> Response {
    let Json(incoming) = match payload {
        Ok(value) => value,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("请求体不是合法的配置 JSON：{}", rejection.body_text()),
            );
        }
    };

    if let Err(message) = incoming.validate() {
        return error(StatusCode::BAD_REQUEST, message);
    }

    if let Err(message) = incoming.save_to(&state.config_path).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, message);
    }

    // 落盘成功后再切换内存态，避免保存失败却已生效
    {
        let mut guard = state.config.write().await;
        *guard = incoming;
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "config_path": state.config_path.display().to_string(),
        })),
    )
        .into_response()
}

/// 细节型错误（4xx 参数类）
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2）
fn error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// GET /api/config/raw：返回 config.toml 原文（源码模式）
pub async fn get_config_raw(State(state): State<AppState>) -> Response {
    match tokio::fs::read_to_string(&state.config_path).await {
        Ok(text) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            text,
        )
            .into_response(),
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取配置文件失败：{err}"),
        ),
    }
}

/// POST /api/config/raw：接收 TOML 原文，校验后原子写入并热更新
pub async fn update_config_raw(State(state): State<AppState>, body: String) -> Response {
    let parsed: Config = match toml::from_str(&body) {
        Ok(config) => config,
        Err(err) => return error_code(StatusCode::BAD_REQUEST, "CONFIG_PARSE", format!("TOML syntax error: {err}")),
    };

    if let Err(message) = parsed.validate() {
        return error(StatusCode::BAD_REQUEST, message);
    }

    // 原样落盘（保留用户注释），而不是重新序列化
    let tmp = state.config_path.with_extension("toml.tmp");
    if let Err(err) = tokio::fs::write(&tmp, body.as_bytes()).await {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("写入临时配置失败：{err}"),
        );
    }
    if let Err(err) = tokio::fs::rename(&tmp, &state.config_path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("替换配置文件失败：{err}"),
        );
    }

    {
        let mut guard = state.config.write().await;
        *guard = parsed;
    }

    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "config_path": state.config_path.display().to_string() })),
    )
        .into_response()
}
