//! 访问令牌（WebDAV / 图床直链用），设计文档 3.1。
//!
//! - WebDAV Basic Auth 首选访问令牌：形如 `asm_<32 hex>`，数据库只存 SHA-1 摘要；
//!   可设置只读/读写、有效天数与挂载点范围——**范围收口到令牌**是它存在的意义；
//! - v4.4.0 起管理员账号（用户名 + 登录密码）也可作为 Basic 凭证直连 WebDAV，
//!   解决「首次部署还没有任何令牌」的冷启动问题；这是**显式放宽**的产物，
//!   只对 WebDAV 入口生效，Bearer Token / Cookie 会话的边界不动；
//! - 密码直连 = 全量管理员权限（读写、全部挂载点），并遵守两条硬性拒绝：
//!   `must_change_password`（一次性密码还没换）与 TOTP 已开启（Basic Auth
//!   表达不了第二因子，这类账号请用令牌）；
//! - 密码直连走 Argon2 逐次校验太贵（WebDAV 客户端一次同步能打几十个请求），
//!   用 [`PasswordCache`] 做进程内缓存：键是「用户名+密码」的 SHA-1（内存不落明文），
//!   值里存验证通过那一刻的 Argon2 哈希快照——改密后哈希对不上立刻失效，
//!   不需要额外的失效通知机制。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha1::{Digest, Sha1};

use crate::AppState;
use crate::core::auth::now_secs;

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
        Ok(iter) => Json(json!({ "items": iter.filter_map(Result::ok).collect::<Vec<_>>() }))
            .into_response(),
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取失败：{err}"),
        ),
    }
}

/// POST /api/tokens
pub async fn create(State(state): State<AppState>, Json(body): Json<CreateToken>) -> Response {
    let name = body.name.trim();
    if name.is_empty() {
        return error_code(
            StatusCode::BAD_REQUEST,
            "TOKEN_NAME_EMPTY",
            "Token name is required",
        );
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
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建失败：{err}"),
        ),
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
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("删除失败：{err}"),
        ),
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
    if let Some(expires) = expires_at
        && expires <= now_secs()
    {
        return None;
    }
    Some(TokenAccess {
        readonly: readonly != 0,
        mount_scope,
    })
}

// ---------------------------------------------------------------------------
// 管理员账号 + 登录密码直连 WebDAV（v4.4.0）
// ---------------------------------------------------------------------------

/// 缓存有效期：只用来免掉 Argon2 的逐请求开销，库里用户行每次都重查，
/// 所以短一点没有副作用——改密 / 关 2FA 都即时生效，不依赖缓存过期。
const PASSWORD_CACHE_TTL: i64 = 300;
/// 条目上限：正常只有一两个管理员在挂载，64 绰绰有余；满了整体重扫清过期项
const PASSWORD_CACHE_MAX: usize = 64;

#[derive(Clone)]
struct PasswordEntry {
    /// 验证通过那一刻的 Argon2 哈希快照：改密后与库里的新哈希对不上，缓存即失效
    db_hash: String,
    verified_at: i64,
}

/// 管理员密码直连的进程内缓存（见模块文档）。
#[derive(Default)]
pub struct PasswordCache {
    inner: Mutex<HashMap<String, PasswordEntry>>,
}

impl PasswordCache {
    /// 命中条件：键存在 + 未过期 + 库里当前哈希与验证时一致。
    fn lookup(&self, key: &str, current_hash: &str, now: i64) -> bool {
        let Ok(mut map) = self.inner.lock() else {
            return false;
        };
        let fresh = map
            .get(key)
            .is_some_and(|entry| now - entry.verified_at < PASSWORD_CACHE_TTL);
        if !fresh {
            map.remove(key);
            return false;
        }
        map.get(key)
            .is_some_and(|entry| entry.db_hash == current_hash)
    }

    fn insert(&self, key: String, db_hash: String, now: i64) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        if map.len() >= PASSWORD_CACHE_MAX {
            let cutoff = now - PASSWORD_CACHE_TTL;
            map.retain(|_, entry| entry.verified_at > cutoff);
            // 还超就按时间淘汰最旧的：管理员数量是常数量级，走不到这里
            while map.len() >= PASSWORD_CACHE_MAX {
                let oldest = map
                    .iter()
                    .min_by_key(|(_, entry)| entry.verified_at)
                    .map(|(key, _)| key.clone());
                match oldest {
                    Some(key) => {
                        map.remove(&key);
                    }
                    None => break,
                }
            }
        }
        map.insert(
            key,
            PasswordEntry {
                db_hash,
                verified_at: now,
            },
        );
    }
}

/// 缓存键：SHA-1(小写用户名 + \x00 + 密码)。
///
/// 内存里不落明文密码；SHA-1 在这里只做「同一进程内的查找索引」，
/// 不承担防碰撞安全职责（放行与否还要对上库里的 Argon2 哈希）。
fn password_cache_key(username: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(username.trim().to_lowercase().as_bytes());
    hasher.update([0u8]);
    hasher.update(password.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 校验 WebDAV Basic 凭证的管理员密码回退（用户名 + 登录密码）。
///
/// 放行 = 全量管理员权限（读写、全部挂载点）；两条硬性拒绝见模块文档。
/// 正确密码但被拒（2FA / 待改密）同样计一次防爆破失败：客户端反复重试
/// 说明凭证口径有问题，应该去换令牌而不是继续撞。
pub fn verify_admin_password(
    state: &AppState,
    username: &str,
    password: &str,
) -> Option<TokenAccess> {
    verify_admin_with(&state.auth, &state.dav_passwords, username, password)
}

/// `verify_admin_password` 的可测内核：只依赖 `AuthStore` 与缓存，不依赖完整 AppState。
pub(crate) fn verify_admin_with(
    store: &crate::core::auth::AuthStore,
    cache: &PasswordCache,
    username: &str,
    password: &str,
) -> Option<TokenAccess> {
    if password.is_empty() {
        return None;
    }
    let (user, hash, _secret) = store.find_user(username).ok().flatten()?;
    if user.must_change_password || user.totp_enabled {
        return None;
    }
    let key = password_cache_key(username, password);
    let now = crate::core::auth::now_secs();
    if cache.lookup(&key, &hash, now) {
        return Some(TokenAccess {
            readonly: false,
            mount_scope: None,
        });
    }
    if !crate::core::auth::verify_password(&hash, password) {
        return None;
    }
    cache.insert(key, hash, now);
    Some(TokenAccess {
        readonly: false,
        mount_scope: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::auth::AuthStore;

    fn temp_store(name: &str) -> AuthStore {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!("tokens-{name}.db"));
        let _ = std::fs::remove_file(&path);
        let store = AuthStore::new(path);
        store.init().unwrap();
        store
    }

    const PASS: &str = "Asmoday#2026";

    #[test]
    fn admin_password_grants_full_access_and_caches() {
        let store = temp_store("full");
        store.create_user("vfy_admin", PASS).unwrap();
        let cache = PasswordCache::default();

        let access = verify_admin_with(&store, &cache, "vfy_admin", PASS).unwrap();
        assert!(!access.readonly, "密码直连 = 管理员全量权限");
        assert!(access.mount_scope.is_none(), "不限挂载点");

        // 第二次走缓存：改回错误的 Argon2 哈希无法伪造（这里只验证仍放行，
        // 缓存命中路径与首验路径的返回完全一致）
        let again = verify_admin_with(&store, &cache, "vfy_admin", PASS).unwrap();
        assert!(!again.readonly);
        // 错误密码永远拒绝
        assert!(verify_admin_with(&store, &cache, "vfy_admin", "wrong-password").is_none());
    }

    #[test]
    fn password_change_invalidates_cached_credentials() {
        let store = temp_store("rotate");
        let user = store.create_user("vfy_rotate", PASS).unwrap();
        let cache = PasswordCache::default();

        assert!(verify_admin_with(&store, &cache, "vfy_rotate", PASS).is_some());
        store.update_password(user.id, "NewPass#2026").unwrap();
        // 旧密码立刻失效（缓存里的 Argon2 快照对不上新哈希），新密码可用
        assert!(verify_admin_with(&store, &cache, "vfy_rotate", PASS).is_none());
        assert!(verify_admin_with(&store, &cache, "vfy_rotate", "NewPass#2026").is_some());
    }

    #[test]
    fn onetime_password_accounts_are_rejected() {
        let store = temp_store("onetime");
        let user = store.create_user("vfy_onetime", PASS).unwrap();
        let cache = PasswordCache::default();

        // --init / --reset-password 发的是一次性密码，改密前不许直连
        store.set_must_change_password(user.id, true).unwrap();
        assert!(verify_admin_with(&store, &cache, "vfy_onetime", PASS).is_none());

        store.set_must_change_password(user.id, false).unwrap();
        assert!(verify_admin_with(&store, &cache, "vfy_onetime", PASS).is_some());
    }

    #[test]
    fn totp_enabled_accounts_are_rejected() {
        let store = temp_store("totp");
        let user = store.create_user("vfy_totp", PASS).unwrap();
        let cache = PasswordCache::default();

        store.set_totp(user.id, "JBSWY3DPEHPK3PXP", true).unwrap();
        // Basic Auth 表达不了第二因子：密码对也不放行，请用令牌
        assert!(verify_admin_with(&store, &cache, "vfy_totp", PASS).is_none());

        store.set_totp(user.id, "", false).unwrap();
        assert!(verify_admin_with(&store, &cache, "vfy_totp", PASS).is_some());
    }

    #[test]
    fn unknown_users_and_empty_passwords_fail_fast() {
        let store = temp_store("unknown");
        store.create_user("vfy_unknown", PASS).unwrap();
        let cache = PasswordCache::default();

        assert!(verify_admin_with(&store, &cache, "nobody", PASS).is_none());
        assert!(verify_admin_with(&store, &cache, "vfy_unknown", "").is_none());
    }
}
