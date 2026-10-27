//! 认证与会话（设计文档 3.1）。
//!
//! 安全约束（红线）：
//! - 配置里没有「关闭认证」开关，本模块也不提供任何跳过校验的分支；
//! - 密码只存 Argon2 哈希；会话只存随机 token，Cookie 走 HttpOnly；
//! - 连续 5 次失败按 IP 封禁 15 分钟；
//! - 忘记密码只能走 CLI（`--reset-password`）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// 会话有效期：7 天
pub const SESSION_TTL: i64 = 7 * 24 * 3600;
/// 连续失败次数上限
pub const MAX_ATTEMPTS: u32 = 5;
/// 封禁时长
pub const BAN_DURATION: Duration = Duration::from_secs(15 * 60);

/// 建表语句，字段与设计文档 4.1 对齐（sessions 为会话所必需，文档未列，此处补上）
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT UNIQUE,
    password_hash TEXT,
    totp_secret TEXT,
    is_totp_enabled BOOLEAN DEFAULT 0,
    must_change_password BOOLEAN NOT NULL DEFAULT 0,
    created_at DATETIME
);

CREATE TABLE IF NOT EXISTS access_tokens (
    id INTEGER PRIMARY KEY,
    user_id INTEGER,
    token_name TEXT,
    token_hash TEXT UNIQUE,
    is_readonly BOOLEAN DEFAULT 1,
    expires_at DATETIME,
    created_at DATETIME
);

CREATE TABLE IF NOT EXISTS shares (
    token TEXT PRIMARY KEY,
    file_path TEXT NOT NULL,
    is_folder BOOLEAN NOT NULL,
    password_hash TEXT,
    allow_download BOOLEAN DEFAULT 1,
    expires_at INTEGER,
    created_at INTEGER
);

CREATE TABLE IF NOT EXISTS favorites (
    file_path TEXT PRIMARY KEY,
    created_at INTEGER
);

CREATE TABLE IF NOT EXISTS play_history (
    file_path TEXT PRIMARY KEY,
    last_played_at INTEGER,
    progress INTEGER
);

CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub totp_enabled: bool,
    /// 一次性密码（--init / --reset-password）登录后必须改密
    pub must_change_password: bool,
}

pub struct AuthStore {
    db_path: PathBuf,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl AuthStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    fn connect(&self) -> Result<Connection, String> {
        if let Some(parent) = self.db_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let conn = Connection::open(&self.db_path)
            .map_err(|e| format!("打开数据库 {} 失败：{e}", self.db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("设置 WAL 失败：{e}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("设置外键失败：{e}"))?;
        Ok(conn)
    }

    pub fn init(&self) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("初始化数据库结构失败：{e}"))?;
        // 存量库补列：CREATE TABLE IF NOT EXISTS 对老库不生效
        let _ = conn.execute(
            "ALTER TABLE users ADD COLUMN must_change_password BOOLEAN NOT NULL DEFAULT 0",
            [],
        );
        Ok(())
    }

    pub fn user_count(&self) -> Result<i64, String> {
        let conn = self.connect()?;
        conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .map_err(|e| format!("查询用户数失败：{e}"))
    }

    pub fn create_user(&self, username: &str, password: &str) -> Result<User, String> {
        let username = username.trim();
        if username.len() < 2 {
            return Err("用户名至少 2 个字符".to_string());
        }
        check_password_strength(password)?;

        let hash = hash_password(password)?;
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
            params![username, hash, now_secs()],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                "用户名已存在".to_string()
            } else {
                format!("创建用户失败：{e}")
            }
        })?;

        Ok(User {
            id: conn.last_insert_rowid(),
            username: username.to_string(),
            totp_enabled: false,
            must_change_password: false,
        })
    }

    pub fn find_user(&self, username: &str) -> Result<Option<(User, String, Option<String>)>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, username, password_hash, totp_secret, is_totp_enabled, must_change_password FROM users WHERE username = ?1",
            params![username],
            |row| {
                Ok((
                    User {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        totp_enabled: row.get::<_, i64>(4).unwrap_or(0) != 0,
                        must_change_password: row.get::<_, i64>(5).unwrap_or(0) != 0,
                    },
                    row.get::<_, String>(2).unwrap_or_default(),
                    row.get::<_, Option<String>>(3).unwrap_or(None),
                ))
            },
        )
        .optional()
        .map_err(|e| format!("查询用户失败：{e}"))
    }

    pub fn first_user(&self) -> Result<Option<User>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, username, is_totp_enabled, must_change_password FROM users ORDER BY id LIMIT 1",
            [],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    totp_enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
                    must_change_password: row.get::<_, i64>(3).unwrap_or(0) != 0,
                })
            },
        )
        .optional()
        .map_err(|e| format!("查询用户失败：{e}"))
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, username, is_totp_enabled, must_change_password FROM users WHERE id = ?1",
            params![id],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    totp_enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
                    must_change_password: row.get::<_, i64>(3).unwrap_or(0) != 0,
                })
            },
        )
        .optional()
        .map_err(|e| format!("查询用户失败：{e}"))
    }

    pub fn update_password(&self, user_id: i64, password: &str) -> Result<(), String> {
        check_password_strength(password)?;
        let hash = hash_password(password)?;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 0 WHERE id = ?2",
            params![hash, user_id],
        )
        .map_err(|e| format!("更新密码失败：{e}"))?;
        // 改密后强制所有会话失效
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
            .map_err(|e| format!("清理会话失败：{e}"))?;
        Ok(())
    }

    pub fn set_totp(&self, user_id: i64, secret: &str, enabled: bool) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET totp_secret = ?1, is_totp_enabled = ?2 WHERE id = ?3",
            params![secret, if enabled { 1 } else { 0 }, user_id],
        )
        .map_err(|e| format!("更新 TOTP 失败：{e}"))?;
        Ok(())
    }

    /// 标记「登录后必须改密」（--init / --reset-password 发放一次性密码时置位）
    pub fn set_must_change_password(&self, user_id: i64, required: bool) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET must_change_password = ?1 WHERE id = ?2",
            params![if required { 1 } else { 0 }, user_id],
        )
        .map_err(|e| format!("更新改密标记失败：{e}"))?;
        Ok(())
    }

    pub fn totp_secret(&self, user_id: i64) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT totp_secret FROM users WHERE id = ?1",
            params![user_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(|e| format!("查询 TOTP 失败：{e}"))
    }

    pub fn create_session(&self, user_id: i64) -> Result<String, String> {
        let token = crate::core::auth::random_token();
        let conn = self.connect()?;
        let now = now_secs();
        conn.execute(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![token, user_id, now, now + SESSION_TTL],
        )
        .map_err(|e| format!("创建会话失败：{e}"))?;
        Ok(token)
    }

    /// 返回 (user_id) —— 过期会话视为无效并顺手清理。
    pub fn session_user(&self, token: &str) -> Result<Option<i64>, String> {
        let conn = self.connect()?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT user_id, expires_at FROM sessions WHERE token = ?1",
                params![token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("查询会话失败：{e}"))?;

        match row {
            Some((user_id, expires_at)) if expires_at > now_secs() => Ok(Some(user_id)),
            Some(_) => {
                let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    pub fn delete_session(&self, token: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])
            .map_err(|e| format!("删除会话失败：{e}"))?;
        Ok(())
    }

    pub fn purge_expired(&self) -> Result<usize, String> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now_secs()])
            .map_err(|e| format!("清理过期会话失败：{e}"))
    }
}

// ---------------------------------------------------------------------------
// 密码 / 随机数
// ---------------------------------------------------------------------------

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("生成密码哈希失败：{e}"))
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn check_password_strength(password: &str) -> Result<(), String> {
    if password.chars().count() < 8 {
        return Err("密码至少 8 个字符".to_string());
    }
    if password.chars().all(|c| c.is_ascii_digit()) {
        return Err("密码不能全是数字".to_string());
    }
    Ok(())
}

/// 32 字节随机 token（16 进制）
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 一次性高强度密码（CLI --init / --reset-password 用）
pub fn random_password() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    Alphanumeric.sample_string(&mut rand::rng(), 20)
}

// ---------------------------------------------------------------------------
// 登录失败限流
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Attempt {
    failures: u32,
    blocked_until: Option<Instant>,
}

#[derive(Default)]
pub struct LoginGuard {
    inner: Mutex<HashMap<String, Attempt>>,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否允许继续尝试；被封锁时返回剩余秒数。
    pub fn check(&self, key: &str) -> Result<(), u64> {
        let mut map = match self.inner.lock() {
            Ok(map) => map,
            Err(_) => return Ok(()),
        };
        if let Some(attempt) = map.get(key) {
            if let Some(until) = attempt.blocked_until {
                let now = Instant::now();
                if until > now {
                    return Err((until - now).as_secs().max(1));
                }
            }
        }
        map.retain(|_, attempt| {
            attempt
                .blocked_until
                .map(|until| until > Instant::now())
                .unwrap_or(attempt.failures > 0)
        });
        Ok(())
    }

    pub fn record_failure(&self, key: &str) {
        let mut map = match self.inner.lock() {
            Ok(map) => map,
            Err(_) => return,
        };
        let entry = map.entry(key.to_string()).or_default();
        entry.failures += 1;
        if entry.failures >= MAX_ATTEMPTS {
            entry.blocked_until = Some(Instant::now() + BAN_DURATION);
            entry.failures = 0;
        }
    }

    pub fn record_success(&self, key: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> AuthStore {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!("auth-{name}.db"));
        let _ = std::fs::remove_file(&path);
        let store = AuthStore::new(path);
        store.init().unwrap();
        store
    }

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("Asmoday#2026").unwrap();
        assert!(verify_password(&hash, "Asmoday#2026"));
        assert!(!verify_password(&hash, "wrong-password"));
    }

    #[test]
    fn user_and_session_lifecycle() {
        let store = temp_store("session");
        assert_eq!(store.user_count().unwrap(), 0);

        let user = store.create_user("admin", "Asmoday#2026").unwrap();
        assert_eq!(store.user_count().unwrap(), 1);

        let (found, hash, _) = store.find_user("admin").unwrap().unwrap();
        assert_eq!(found.id, user.id);
        assert!(verify_password(&hash, "Asmoday#2026"));

        let token = store.create_session(user.id).unwrap();
        assert_eq!(store.session_user(&token).unwrap(), Some(user.id));

        store.delete_session(&token).unwrap();
        assert_eq!(store.session_user(&token).unwrap(), None);
    }

    #[test]
    fn weak_passwords_are_rejected() {
        let store = temp_store("weak");
        assert!(store.create_user("a", "Asmoday#2026").is_err()); // 用户名太短
        assert!(store.create_user("admin", "short").is_err());
        assert!(store.create_user("admin", "12345678").is_err());
    }

    #[test]
    fn login_guard_blocks_after_failures() {
        let guard = LoginGuard::new();
        for _ in 0..MAX_ATTEMPTS {
            assert!(guard.check("1.2.3.4").is_ok());
            guard.record_failure("1.2.3.4");
        }
        let blocked = guard.check("1.2.3.4");
        assert!(blocked.is_err(), "第 5 次失败后应被封锁");
        assert!(guard.check("5.6.7.8").is_ok(), "其它 IP 不受影响");
    }
}
