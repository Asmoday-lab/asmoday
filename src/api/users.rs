//! 用户管理接口（v4.9.24 多用户，设计文档 3.1.6）。
//!
//! 全部端点只放行 `role = admin`（`AdminUser` 提取器）。数据面（文件/媒体/分享）
//! 是共享的，这里管的只是「谁能登录、登录后是什么角色」——对齐 Navidrome /
//! Immich 的用户管理语义：
//! - 创建：管理员代设初始密码，`must_change_password=1`，首登强制改密；
//! - 改名：UNIQUE 兜重名，改名后该用户全部会话失效；
//! - 停用：记录保留、拒绝登录、踢掉会话，启用即恢复；
//! - 删除：级联清掉该用户的会话/收藏/播放历史/令牌，分享转 tombstone。
//!
//! 守护规则：系统里永远至少留一个可用的 admin（不能停用/降级/删除最后一个）。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::auth::{AdminUser, json_error, json_error_code};
use crate::AppState;

fn user_json(u: &crate::core::auth::User) -> serde_json::Value {
    json!({
        "id": u.id,
        "username": u.username,
        "role": u.role,
        "is_active": u.is_active,
        "totp_enabled": u.totp_enabled,
        "must_change_password": u.must_change_password,
        "permissions": u.permissions,
        "mount_scope": u.mount_scope,
    })
}

/// GET /api/users（管理员）
pub async fn list(State(state): State<AppState>, _admin: AdminUser) -> Response {
    match state.auth.list_users() {
        Ok(users) => Json(json!({ "items": users.iter().map(user_json).collect::<Vec<_>>() }))
            .into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
}

use axum::response::{IntoResponse, Response};

/// POST /api/users（管理员）
pub async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    let role = body.role.as_deref().unwrap_or("user");
    match state.auth.create_user(&body.username, &body.password, role) {
        Ok(user) => Json(json!({ "status": "ok", "user": user_json(&user) })).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    /// 权限位掩码（v4.9.25）：整数；admin 账号传了也只落库（判定时恒全放行）
    #[serde(default)]
    pub permissions: Option<i64>,
    /// 挂载点收权（v4.9.25）：`"*"` 或挂载点展示路径的 JSON 数组
    #[serde(default)]
    pub mount_scope: Option<String>,
}

/// PATCH /api/users/{id}（管理员）：改名 / 代设密码 / 调角色 / 启停，可一次多项。
///
/// 自我保护：管理员不能停用或降级自己（删自己走 DELETE，单独拦）——
/// 防止「手滑把自己踢出管理页」。
pub async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> Response {
    let Some(target) = state.auth.user_by_id(id).ok().flatten() else {
        return json_error_code(StatusCode::NOT_FOUND, "USER_NOT_FOUND", "User not found");
    };

    if let Some(username) = &body.username {
        if let Err(err) = state.auth.rename_user(target.id, username) {
            return json_error(StatusCode::BAD_REQUEST, err);
        }
        state
            .auth
            .invalidate_sessions(target.id)
            .map_err(|e| json_error(StatusCode::INTERNAL_SERVER_ERROR, e))
            .ok();
    }
    if let Some(password) = &body.password
        && let Err(err) = state.auth.admin_set_password(target.id, password)
    {
        return json_error(StatusCode::BAD_REQUEST, err);
    }
    if let Some(role) = &body.role {
        if target.id == admin.0.id && role != "admin" {
            return json_error_code(
                StatusCode::BAD_REQUEST,
                "E_LAST_ADMIN",
                "Cannot demote your own admin account",
            );
        }
        if let Err(err) = state.auth.set_user_role(target.id, role) {
            return json_error(StatusCode::BAD_REQUEST, err);
        }
    }
    if let Some(active) = body.is_active {
        if target.id == admin.0.id && !active {
            return json_error_code(
                StatusCode::BAD_REQUEST,
                "E_LAST_ADMIN",
                "Cannot disable your own account",
            );
        }
        if let Err(err) = state.auth.set_user_active(target.id, active) {
            return json_error(StatusCode::BAD_REQUEST, err);
        }
    }
    // 权限位 + 挂载点收权（v4.9.25）：两个一起传一次落库，避免中间态；
    // 只传其一则另一个保持原值。权限收紧即时生效（AuthUser 每请求重读用户行）。
    if body.permissions.is_some() || body.mount_scope.is_some() {
        let Some(current) = state.auth.user_by_id(target.id).ok().flatten() else {
            return json_error_code(StatusCode::NOT_FOUND, "USER_NOT_FOUND", "User not found");
        };
        let permissions = body.permissions.unwrap_or(current.permissions);
        let mount_scope = body
            .mount_scope
            .clone()
            .unwrap_or_else(|| current.mount_scope.clone());
        if let Err(err) = state
            .auth
            .set_user_perms(target.id, permissions, &mount_scope)
        {
            return json_error(StatusCode::BAD_REQUEST, err);
        }
    }

    match state.auth.user_by_id(target.id) {
        Ok(Some(user)) => Json(json!({ "status": "ok", "user": user_json(&user) })).into_response(),
        _ => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "读取用户失败".to_string(),
        ),
    }
}

/// DELETE /api/users/{id}（管理员）：删除账号并级联清理归属数据。
pub async fn remove(
    State(state): State<AppState>,
    admin: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response {
    match state.auth.delete_user(id, admin.0.id) {
        Ok(()) => Json(json!({ "status": "ok" })).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err),
    }
}
