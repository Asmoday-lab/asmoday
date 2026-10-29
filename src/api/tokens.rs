//! 访问令牌（WebDAV / 图床直链用），设计文档 3.1。
//!
//! - 严禁用主密码挂载 WebDAV；
//! - 令牌形如 `asm_<32 hex>`，数据库只存 SHA-1 摘要；
//! - 可设置只读/读写与有效天数。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sha1::{Digest, Sha1};

use crate::core::auth::now_secs;
use crate::AppState;

fn hash_token(token: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct CreateToken {
    pub name: String,
    #[serde(default = "default_readonly")]
    pub readonly: bool,
    /// 有效天数，0 或省略表示永久
    #[serde(default)]
    pub days: u32,
    /// 可选：把令牌限制在某个挂载点内（WebDAV 挂载通常只需要一个目录）。
    /// 省略 = 全部挂载点；填了不存在的 id 直接拒绝，避免造出「看起来能用」的令牌。
    #[serde(default)]
    pub mount_id: Option<String>,
}

/// 令牌校验结果：只读标记 + 挂载点范围
#[derive(Debug, Clone)]
pub struct TokenAccess {
    pub readonly: bool,
    /// `None` = 全部挂载点
    pub mount_scope: Option<String>,
}

fn default_readonly() -> bool {
    true
}

/// 细节型错误（4xx 参数类）
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2）
fn error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// GET /api/tokens
pub async fn list(State(state): State<AppState>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let mut statement = match conn.prepare(
        "SELECT id, token_name, is_readonly, expires_at, created_at, mount_scope FROM access_tokens ORDER BY id DESC",
    ) {
        Ok(statement) => statement,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("查询失败：{err}")),
    };

    let rows = statement.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "name": row.get::<_, String>(1)?,
            "readonly": row.get::<_, i64>(2)? != 0,
            "expires_at": row.get::<_, Option<i64>>(3)?,
            "created_at": row.get::<_, Option<i64>>(4)?,
            "mount_id": row.get::<_, Option<String>>(5)?,
        }))
    });

    match rows {
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() })).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("读取失败：{err}")),
    }
}

/// POST /api/tokens
pub async fn create(State(state): State<AppState>, Json(body): Json<CreateToken>) -> Response {
    let name = body.name.trim();
    if name.is_empty() {
        return error_code(StatusCode::BAD_REQUEST, "TOKEN_NAME_EMPTY", "Token name is required");
    }

    let token = format!("asm_{}", crate::core::auth::random_token());
    let hashed = hash_token(&token);
    let expires_at = if body.days == 0 {
        None
    } else {
        Some(now_secs() + i64::from(body.days) * 86_400)
    };

    // 挂载点范围：填了就必须真实存在，否则令牌会「永远 403」且用户不知道原因
    let scope = body
        .mount_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    if let Some(id) = &scope {
        let set = {
            let config = state.config.read().await;
            crate::core::path_guard::load(&config, &state.cwd)
        };
        if set.find(id).is_none() {
            return error_code(
                StatusCode::BAD_REQUEST,
                "E_NOT_FOUND",
                "Mount point does not exist",
            );
        }
    }

    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let result = conn.execute(
        "INSERT INTO access_tokens (user_id, token_name, token_hash, is_readonly, expires_at, created_at, mount_scope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            1,
            name,
            hashed,
            if body.readonly { 1 } else { 0 },
            expires_at,
            now_secs(),
            scope
        ],
    );

    match result {
        Ok(_) => Json(json!({
            "status": "ok",
            // 明文只在创建时返回一次
            "token": token,
            "name": name,
            "readonly": body.readonly,
            "expires_at": expires_at,
            "mount_id": scope,
        }))
        .into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("创建失败：{err}")),
    }
}

/// DELETE /api/tokens/{id}
pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = match state.auth_connection() {
        Ok(conn) => conn,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    match conn.execute("DELETE FROM access_tokens WHERE id = ?1", [id]) {
        Ok(0) => error(StatusCode::NOT_FOUND, "令牌不存在"),
        Ok(_) => Json(json!({ "status": "ok" })).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败：{err}")),
    }
}

/// 校验 WebDAV Basic 凭证（设计文档 v2.5.0 3.10）：
/// 用户名必须是 Token 名，密码是 `asm_` 明文；哈希匹配且未过期才放行。
/// 返回只读标记与挂载点范围。
pub fn verify_token(state: &AppState, username: &str, token: &str) -> Option<TokenAccess> {
    let conn = state.auth_connection().ok()?;
    let hashed = hash_token(token);
    let row: Option<(String, i64, Option<i64>, Option<String>)> = conn
        .query_row(
            "SELECT token_name, is_readonly, expires_at, mount_scope FROM access_tokens WHERE token_hash = ?1",
            [hashed],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .ok();

    let (token_name, readonly, expires_at, mount_scope) = row?;
    if token_name != username {
        return None;
    }
    if let Some(expires) = expires_at {
        if expires <= now_secs() {
            return None;
        }
    }
    Some(TokenAccess {
        readonly: readonly != 0,
        mount_scope,
    })
}
