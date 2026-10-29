//! 统一的 API 错误响应（设计文档 3.8 / 6.6）。
//!
//! 契约：`{"code": "E_CONFLICT", "message": "Target already exists"}`
//! - `code` 是前端查 i18n `errors.<CODE>` 的唯一依据，稳定且不随文案变化；
//! - `message` 只作诊断/兜底（**强制英文**），允许带细节（如具体是哪个字段校验失败）。
//!
//! 说明：这里刻意保留一个「无 code」的入口 `detail`，用于 4xx 参数类错误 ——
//! 这类消息本身就携带前端需要展示的具体信息（哪个文件、哪个字段），
//! 强行套一个泛化 code 反而会把它盖掉。5xx 一律补 `E_INTERNAL`：服务端细节
//! 既不该泄漏，也不需要前端逐条翻译。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::core::fs::FsError;

/// 带明确错误码的响应。
pub fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into(), "params": {} })),
    )
        .into_response()
}

/// 不带错误码的细节响应（4xx 参数类）。
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    if status.is_server_error() {
        return api_error(status, "E_INTERNAL", message);
    }
    (status, Json(json!({ "message": message }))).into_response()
}

/// `FsError` → HTTP 响应：文件系统类错误有固定映射，全站共用（设计文档 6.6 总表）。
pub fn fs_error(err: &FsError) -> Response {
    api_error(status_of(err), code_of(err), err.message())
}

/// `FsError` 对应的错误码。
pub fn code_of(err: &FsError) -> &'static str {
    match err {
        FsError::Forbidden(_) => "E_FORBIDDEN",
        FsError::PathTraversal(_) => "E_PATH_TRAVERSAL",
        FsError::SymlinkEscape(_) => "E_SYMLINK_ESCAPE",
        FsError::NotFound(_) => "E_NOT_FOUND",
        FsError::BadRequest(_) => "E_VALIDATION",
        FsError::Readonly(_) => "E_READONLY",
        FsError::Conflict(_) => "E_CONFLICT",
        FsError::DirConflict(_) => "E_DIR_CONFLICT",
        FsError::RetryExhausted(_) => "E_CONFLICT_RETRY_EXHAUSTED",
        FsError::TooMany(_) => "E_TOO_MANY",
        FsError::TooLarge(_) => "E_TOO_LARGE",
        FsError::NoSpace(_) => "E_NO_SPACE",
        FsError::Busy(_) => "E_BUSY",
        FsError::NameMismatch(_) => "E_NAME_MISMATCH",
        FsError::TokenExpired(_) => "E_TOKEN_EXPIRED",
        FsError::TokenMismatch(_) => "E_TOKEN_MISMATCH",
        // 复制已成功、只有删源失败：必须给独立 code，前端才能提示「去目标目录确认」
        FsError::SourceDeleteFailed(_) => "E_SOURCE_DELETE_FAILED",
        FsError::Io(_) => "E_INTERNAL",
    }
}

/// `FsError` 对应的状态码。
pub fn status_of(err: &FsError) -> StatusCode {
    match err {
        FsError::Forbidden(_) | FsError::SymlinkEscape(_) | FsError::Readonly(_) => {
            StatusCode::FORBIDDEN
        }
        FsError::PathTraversal(_) => StatusCode::BAD_REQUEST,
        FsError::NotFound(_) => StatusCode::NOT_FOUND,
        FsError::BadRequest(_) | FsError::NameMismatch(_) => StatusCode::BAD_REQUEST,
        FsError::Conflict(_)
        | FsError::DirConflict(_)
        | FsError::RetryExhausted(_)
        | FsError::Busy(_) => StatusCode::CONFLICT,
        FsError::TooMany(_) | FsError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        FsError::NoSpace(_) => StatusCode::INSUFFICIENT_STORAGE,
        FsError::TokenExpired(_) | FsError::TokenMismatch(_) => StatusCode::FORBIDDEN,
        // 500：数据已写入目标，属于「部分成功」的服务端收尾失败
        FsError::SourceDeleteFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        FsError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_maps_to_e_forbidden() {
        let err = FsError::Forbidden("outside".to_string());
        assert_eq!(status_of(&err), StatusCode::FORBIDDEN);
        assert_eq!(code_of(&err), "E_FORBIDDEN");
    }

    #[test]
    fn io_maps_to_internal() {
        let err = FsError::Io("disk".to_string());
        assert_eq!(status_of(&err), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code_of(&err), "E_INTERNAL");
    }

    #[test]
    fn not_found_maps_to_404() {
        let err = FsError::NotFound("missing".to_string());
        assert_eq!(status_of(&err), StatusCode::NOT_FOUND);
        assert_eq!(code_of(&err), "E_NOT_FOUND");
    }

    #[test]
    fn conflict_family_uses_409() {
        for (err, code) in [
            (FsError::Conflict("x".into()), "E_CONFLICT"),
            (FsError::DirConflict("x".into()), "E_DIR_CONFLICT"),
            (
                FsError::RetryExhausted("x".into()),
                "E_CONFLICT_RETRY_EXHAUSTED",
            ),
            (FsError::Busy("x".into()), "E_BUSY"),
        ] {
            assert_eq!(status_of(&err), StatusCode::CONFLICT, "{code}");
            assert_eq!(code_of(&err), code);
        }
    }

    #[test]
    fn source_delete_failure_has_its_own_code() {
        // 设计文档 6.5：复制成功但删源失败必须独立成码，
        // 不能再混进 E_INTERNAL —— 前端据此提示「去目标目录确认」
        let err = FsError::SourceDeleteFailed("已写入目标，但源文件清理失败".to_string());
        assert_eq!(code_of(&err), "E_SOURCE_DELETE_FAILED");
        assert_eq!(status_of(&err), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message(), "已写入目标，但源文件清理失败");
    }

    #[test]
    fn limits_map_to_413_and_507() {
        assert_eq!(
            status_of(&FsError::TooMany("x".into())),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status_of(&FsError::TooLarge("x".into())),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status_of(&FsError::NoSpace("x".into())),
            StatusCode::INSUFFICIENT_STORAGE
        );
    }
}
