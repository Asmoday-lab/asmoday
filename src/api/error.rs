//! 统一的 API 错误响应（设计文档 3.8 第 2 条）。
//!
//! 契约：`{"code": "AUTH_FAILED", "message": "Invalid password"}`
//! - `code` 是前端查 i18n `errors.<CODE>` 的唯一依据，稳定且不随文案变化；
//! - `message` 只作诊断/兜底，允许带细节（如具体是哪个字段校验失败）。
//!
//! 说明：这里刻意保留一个「无 code」的入口 `detail`，用于 4xx 参数类错误 ——
//! 这类消息本身就携带前端需要展示的具体信息（哪个文件、哪个字段），
//! 强行套一个泛化 code 反而会把它盖掉。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::core::fs::FsError;

/// 带明确错误码的响应。
pub fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// 不带错误码的细节响应（4xx 参数类）。
///
/// 5xx 一律补 `INTERNAL`：服务端细节既不该泄漏，也不需要前端逐条翻译。
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    if status.is_server_error() {
        return api_error(status, "INTERNAL", message);
    }
    (status, Json(json!({ "message": message }))).into_response()
}

/// `FsError` → HTTP 响应：文件系统类错误有固定映射，全站共用。
pub fn fs_error(err: &FsError) -> Response {
    let status = status_of(err);
    match err {
        FsError::Forbidden(message) => api_error(status, "PATH_FORBIDDEN", message.clone()),
        FsError::NotFound(message) => api_error(status, "PATH_NOT_FOUND", message.clone()),
        FsError::BadRequest(message) => api_error(status, "BAD_REQUEST", message.clone()),
        FsError::Io(message) => api_error(status, "INTERNAL", message.clone()),
    }
}

/// `FsError` 对应的状态码。
fn status_of(err: &FsError) -> StatusCode {
    match err {
        FsError::Forbidden(_) => StatusCode::FORBIDDEN,
        FsError::NotFound(_) => StatusCode::NOT_FOUND,
        FsError::BadRequest(_) => StatusCode::BAD_REQUEST,
        FsError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_maps_to_path_forbidden() {
        let err = FsError::Forbidden("越界".to_string());
        assert_eq!(status_of(&err), StatusCode::FORBIDDEN);
    }

    #[test]
    fn io_maps_to_internal() {
        let err = FsError::Io("disk".to_string());
        assert_eq!(status_of(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn not_found_maps_to_404() {
        let err = FsError::NotFound("missing".to_string());
        assert_eq!(status_of(&err), StatusCode::NOT_FOUND);
    }
}
