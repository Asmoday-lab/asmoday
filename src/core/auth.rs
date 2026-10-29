//! 认证与会话（设计文档 3.1）。
//!
//! 安全约束（红线）：
//! - 配置里没有「关闭认证」开关，本模块也不提供任何跳过校验的分支；
//! - 密码只存 Argon2 哈希；会话只存随机 token，Cookie 走 HttpOnly；
//! - 连续 5 次失败按 IP 封禁 15 分钟；
//! - 忘记密码只能走 CLI（`--reset-password`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};
use serde::Serialize;

/// 会话有效期：7 天（滑动续期的步长）
pub const SESSION_TTL: i64 = 7 * 24 * 3600;
/// 会话绝对上限：30 天（无论怎么续期都不能超过，设计文档 3.1.3）
pub const SESSION_ABSOLUTE_TTL: i64 = 30 * 86_400;
/// 敏感操作的时间闸门：最近一次输密码必须在 5 分钟内
pub const REAUTH_WINDOW: i64 = 5 * 60;

/// 会话的服务端状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    pub user_id: i64,
    /// 最近一次「重新输入密码」的时间戳（登录时等于 created_at）
    pub verified_at: i64,
}
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
    totp_recovery_codes TEXT,
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
    created_at DATETIME,
    -- 令牌限定挂载点（NULL = 全部）：WebDAV 挂载通常只需要一个目录，
    -- 泄漏时影响面从「全库」缩到一个挂载点
    mount_scope TEXT
);

CREATE TABLE IF NOT EXISTS shares (
    token TEXT PRIMARY KEY,
    file_path TEXT NOT NULL,
    is_folder BOOLEAN NOT NULL,
    password_hash TEXT,
    allow_download BOOLEAN DEFAULT 1,
    expires_at INTEGER,
    created_at INTEGER,
    deleted_at INTEGER
);

-- 收藏与播放历史（设计文档 4.1）：主键是 (mount_id, file_path)。
-- file_path 一直是绝对路径，单主键其实不会撞；带 mount_id 是为了「按挂载点
-- 批量清理 / 分组展示」，也让将来改成挂载点内相对路径时不用再迁一次表。
CREATE TABLE IF NOT EXISTS favorites (
    mount_id TEXT NOT NULL DEFAULT '',
    file_path TEXT NOT NULL,
    created_at INTEGER,
    PRIMARY KEY (mount_id, file_path)
);

CREATE TABLE IF NOT EXISTS play_history (
    mount_id TEXT NOT NULL DEFAULT '',
    file_path TEXT NOT NULL,
    last_played_at INTEGER,
    progress INTEGER,
    PRIMARY KEY (mount_id, file_path)
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    -- 最近一次「重新输入密码」的时刻：敏感操作（终端/令牌/配置）要求它足够新，
    -- 防止一个 30 天的会话被 XSS 或借用浏览器后直接拿到系统级权限
    verified_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS thumb_hashes (
    key TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    updated_at INTEGER NOT NULL
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
    /// ThumbHash 写入计数：每 256 次顺带做一次 LRU 裁剪，防止表被频繁改动的源文件撑大
    hash_saves: AtomicU64,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 表里是否已有某一列（用于判定老库要不要迁移）。
fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut statement) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    rows.filter_map(Result::ok).any(|name| name == column)
}

/// `favorites` / `play_history` 主键升级：`file_path` → `(mount_id, file_path)`。
///
/// SQLite 不支持改主键，只能「建新表 → 抄数据 → 删旧表 → 改名」。
/// 只在老库（缺 `mount_id` 列）上真的动；新库的 `CREATE TABLE` 已经是新结构。
///
/// **不要**在这里写 `BEGIN`/`COMMIT`：迁移钩子本身已经跑在事务里，
/// 嵌套事务会直接 `cannot start a transaction within a transaction`。
fn rebuild_media_tables(conn: &Connection) -> rusqlite::Result<()> {
    if !table_has_column(conn, "favorites", "mount_id") {
        conn.execute_batch(
            "CREATE TABLE favorites_v2 (
                 mount_id TEXT NOT NULL DEFAULT '',
                 file_path TEXT NOT NULL,
                 created_at INTEGER,
                 PRIMARY KEY (mount_id, file_path)
             );
             INSERT OR IGNORE INTO favorites_v2 (mount_id, file_path, created_at)
                 SELECT '', file_path, created_at FROM favorites;
             DROP TABLE favorites;
             ALTER TABLE favorites_v2 RENAME TO favorites;",
        )?;
    }
    if !table_has_column(conn, "play_history", "mount_id") {
        conn.execute_batch(
            "CREATE TABLE play_history_v2 (
                 mount_id TEXT NOT NULL DEFAULT '',
                 file_path TEXT NOT NULL,
                 last_played_at INTEGER,
                 progress INTEGER,
                 PRIMARY KEY (mount_id, file_path)
             );
             INSERT OR IGNORE INTO play_history_v2 (mount_id, file_path, last_played_at, progress)
                 SELECT '', file_path, last_played_at, progress FROM play_history;
             DROP TABLE play_history;
             ALTER TABLE play_history_v2 RENAME TO play_history;",
        )?;
    }
    Ok(())
}

/// 存量库补列（设计文档 4.1）。
///
/// `CREATE TABLE IF NOT EXISTS` 对**已存在**的表不生效，所以老库缺的列只能 ALTER 补。
/// 放在迁移的 v1 钩子里做，是因为 v1 对存量库同样会跑（user_version 从 0 开始），
/// 而「列是否已存在」必须自己判断——ALTER 不幂等。
const BACKFILL_COLUMNS: &[(&str, &str, &str)] = &[
    (
        "users",
        "must_change_password",
        "ALTER TABLE users ADD COLUMN must_change_password BOOLEAN NOT NULL DEFAULT 0",
    ),
    // 2FA 一次性恢复码：只存 SHA-1 指纹的 JSON 数组
    (
        "users",
        "totp_recovery_codes",
        "ALTER TABLE users ADD COLUMN totp_recovery_codes TEXT",
    ),
    // 分享 tombstone：文件删掉后分享不连带消失，访问返回 410，30 天后物理清除
    (
        "shares",
        "deleted_at",
        "ALTER TABLE shares ADD COLUMN deleted_at INTEGER",
    ),
    // 令牌限定挂载点
    (
        "access_tokens",
        "mount_scope",
        "ALTER TABLE access_tokens ADD COLUMN mount_scope TEXT",
    ),
    // 敏感操作二次验证时间戳（默认 0 = 从未验证）
    (
        "sessions",
        "verified_at",
        "ALTER TABLE sessions ADD COLUMN verified_at INTEGER NOT NULL DEFAULT 0",
    ),
];

/// schema 迁移序列（`PRAGMA user_version` 记录进度）。
///
/// - **v1** 基线：全部 `CREATE TABLE IF NOT EXISTS` + 缺列才补的钩子。新库和存量库
///   （user_version 都是 0）走同一段，幂等由钩子里的列判断保证。
/// - **v2** 收藏/播放历史主键升级成 `(mount_id, file_path)`。
///
/// 以后加列加表**只往后追加** `M::up(...)`，不要回头改 v1：已经升过级的库不会重跑它。
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up_with_hook(SCHEMA, |tx: &rusqlite::Transaction| {
            for (table, column, ddl) in BACKFILL_COLUMNS {
                if !table_has_column(tx, table, column) {
                    tx.execute_batch(ddl)?;
                }
            }
            Ok(())
        }),
        M::up_with_hook("", |tx: &rusqlite::Transaction| {
            rebuild_media_tables(tx)?;
            Ok(())
        }),
    ])
}

impl AuthStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            hash_saves: AtomicU64::new(0),
        }
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    // -----------------------------------------------------------------------
    // ThumbHash 存储（v2.8.2：不再落 .th 旁路文件，改存 SQLite）
    // -----------------------------------------------------------------------

    /// 批量读取 ThumbHash（媒体扫描用）。
    ///
    /// 一次连接内复用 prepared statement 逐键查，几万行的表上也只是微秒级/键；
    /// 任何失败都返回空表，调用方回退纯色占位。
    pub fn thumb_hashes_get_many(&self, keys: &[String]) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if keys.is_empty() {
            return map;
        }
        let Ok(conn) = self.connect() else {
            return map;
        };
        let Ok(mut stmt) = conn.prepare("SELECT hash FROM thumb_hashes WHERE key = ?1") else {
            return map;
        };
        for key in keys {
            if let Ok(Some(hash)) = stmt
                .query_row(params![key], |row| row.get::<_, String>(0))
                .optional()
            {
                map.insert(key.clone(), hash);
            }
        }
        map
    }

    /// 写入 ThumbHash（缩略图生成成功时调用）
    pub fn thumb_hash_set(&self, key: &str, hash: &str) {
        self.thumb_hashes_set_many(std::slice::from_ref(&(key.to_string(), hash.to_string())));
    }

    /// 批量写入；每 256 次顺带按 updated_at 做 LRU 裁剪（保留最新 4 万行）
    pub fn thumb_hashes_set_many(&self, entries: &[(String, String)]) {
        if entries.is_empty() {
            return;
        }
        let Ok(conn) = self.connect() else {
            return;
        };
        let Ok(mut stmt) = conn.prepare(
            "INSERT OR REPLACE INTO thumb_hashes (key, hash, updated_at) VALUES (?1, ?2, ?3)",
        ) else {
            return;
        };
        let now = now_secs();
        for (key, hash) in entries {
            let _ = stmt.execute(params![key, hash, now]);
        }
        let saves = self
            .hash_saves
            .fetch_add(entries.len() as u64, Ordering::Relaxed);
        // 取模判断而非等值判断：批量写入跨过 256 边界时也要触发一次
        if (saves % 256) + entries.len() as u64 >= 256 {
            let _ = conn.execute(
                "DELETE FROM thumb_hashes WHERE key NOT IN \
                 (SELECT key FROM thumb_hashes ORDER BY updated_at DESC LIMIT 40000)",
                [],
            );
        }
    }

    /// 清空 ThumbHash 表（设置页「清理媒体缓存」联动：缓存图都删了，色值也没意义了）
    pub fn thumb_hashes_clear(&self) {
        let Ok(conn) = self.connect() else {
            return;
        };
        let _ = conn.execute("DELETE FROM thumb_hashes", []);
    }

    fn connect(&self) -> Result<Connection, String> {
        if let Some(parent) = self.db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&self.db_path)
            .map_err(|e| format!("打开数据库 {} 失败：{e}", self.db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("设置 WAL 失败：{e}"))?;
        // 设计文档 4.1：【强制】busy_timeout=5000 —— 缩略图/媒体扫描与 Web 请求并发写库时
        // 不要立刻抛 SQLITE_BUSY，等 5 秒让锁自然释放
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(|e| format!("设置 busy_timeout 失败：{e}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("设置外键失败：{e}"))?;
        Ok(conn)
    }

    pub fn init(&self) -> Result<(), String> {
        let mut conn = self.connect()?;
        // 版本化的 schema 升级：每步只跑一次，失败能定位到具体版本（见 `migrations`）
        migrations()
            .to_latest(&mut conn)
            .map_err(|e| format!("迁移数据库结构失败：{e}"))?;
        // 数据库文件权限 0600（设计文档 4.1）：里面是密码哈希、TOTP 密钥与会话，
        // 绝不能给同机其他用户可读
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&self.db_path) {
                if meta.permissions().mode() & 0o777 != 0o600 {
                    let _ = std::fs::set_permissions(
                        &self.db_path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
        }
        Ok(())
    }

    /// 读 KV 设置（不存在返回 None）
    pub fn setting(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.connect()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(|e| format!("读取设置失败：{e}"))
    }

    /// 写 KV 设置（覆盖）
    pub fn put_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| format!("写入设置失败：{e}"))?;
        Ok(())
    }

    /// 作废该用户的全部会话（改密 / 重置 / 关闭 2FA 后必须调用）
    pub fn invalidate_sessions(&self, user_id: i64) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])
            .map_err(|e| format!("作废会话失败：{e}"))?;
        Ok(())
    }

    /// 清除 TOTP（`--reset-2fa`）
    pub fn clear_totp(&self, user_id: i64) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET totp_secret = NULL, is_totp_enabled = 0 WHERE id = ?1",
            [user_id],
        )
        .map_err(|e| format!("清除 2FA 失败：{e}"))?;
        Ok(())
    }

    pub fn user_count(&self) -> Result<i64, String> {
        let conn = self.connect()?;
        conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .map_err(|e| format!("查询用户数失败：{e}"))
    }

    // -----------------------------------------------------------------------
    // 元数据联动（设计文档 3.2.6.7）
    //
    // 一致性原则：**文件系统是文件真相，数据库是元数据真相**。文件系统成功、
    // 数据库更新失败时不回滚文件系统，记 ERROR 日志，靠惰性修复兜底。
    // 因此这里的方法返回 Result，由调用方决定「记日志继续」而不是中断操作。
    // -----------------------------------------------------------------------

    /// 目标路径上是否已经有别的分享记录（移动前冲突检查）
    pub fn share_exists_at(&self, path: &str) -> Result<bool, String> {
        let conn = self.connect()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM shares WHERE deleted_at IS NULL AND (file_path = ?1 OR file_path LIKE ?1 || '/%')",
                [path],
                |row| row.get(0),
            )
            .map_err(|e| format!("查询分享失败：{e}"))?;
        Ok(count > 0)
    }

    /// 重命名 / 移动后，把 `favorites / play_history / shares` 里以旧路径为前缀的记录一并改写
    pub fn rewrite_meta_paths(&self, old: &str, new: &str) -> Result<(), String> {
        if old == new {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("开启事务失败：{e}"))?;
        // OR REPLACE：目标路径可能已有旧记录（用户以前收藏过），直接覆盖掉
        for table in ["favorites", "play_history"] {
            let sql = format!(
                "UPDATE OR REPLACE {table} SET file_path = ?2 || substr(file_path, length(?1) + 1)
                 WHERE file_path = ?1 OR file_path LIKE ?1 || '/%'"
            );
            tx.execute(&sql, params![old, new])
                .map_err(|e| format!("更新 {table} 失败：{e}"))?;
        }
        tx.execute(
            "UPDATE shares SET file_path = ?2 || substr(file_path, length(?1) + 1)
             WHERE (file_path = ?1 OR file_path LIKE ?1 || '/%') AND deleted_at IS NULL",
            params![old, new],
        )
        .map_err(|e| format!("更新 shares 失败：{e}"))?;
        tx.commit().map_err(|e| format!("提交失败：{e}"))?;
        Ok(())
    }

    /// 删除后清理元数据：收藏/历史直接删，分享置 tombstone（访问时 410）
    pub fn forget_meta_paths(&self, path: &str, now: i64) -> Result<(), String> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("开启事务失败：{e}"))?;
        for table in ["favorites", "play_history"] {
            let sql =
                format!("DELETE FROM {table} WHERE file_path = ?1 OR file_path LIKE ?1 || '/%'");
            tx.execute(&sql, [path])
                .map_err(|e| format!("清理 {table} 失败：{e}"))?;
        }
        tx.execute(
            "UPDATE shares SET deleted_at = ?2
             WHERE (file_path = ?1 OR file_path LIKE ?1 || '/%') AND deleted_at IS NULL",
            params![path, now],
        )
        .map_err(|e| format!("标记 shares tombstone 失败：{e}"))?;
        tx.commit().map_err(|e| format!("提交失败：{e}"))?;
        Ok(())
    }

    /// 启动时清理超过 30 天的分享 tombstone（设计文档 4.1：不引入常驻定时任务）
    pub fn purge_share_tombstones(&self, now: i64) -> Result<usize, String> {
        let conn = self.connect()?;
        let cutoff = now - 30 * 86_400;
        conn.execute(
            "DELETE FROM shares WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            [cutoff],
        )
        .map_err(|e| format!("清理分享 tombstone 失败：{e}"))
    }

    /// 热备份数据库到指定文件，返回备份大小（字节）。
    ///
    /// 用 `VACUUM INTO`（SQLite 官方的单文件热备手段）而不是复制 `.db`：
    /// WAL 模式下最近的事务还在 `-wal` 里，只拷主文件会得到一个「少了最近写入」
    /// 的库，而且不会有任何报错 —— 这种备份比没有备份更危险。
    ///
    /// 服务运行中也能执行：它走独立连接，只需读锁，不会阻塞线上读写。
    pub fn backup_to(&self, target: &Path) -> Result<u64, String> {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建备份目录失败：{e}"))?;
        }
        // VACUUM INTO 遇到已存在的文件会直接报错，先给出更好懂的提示
        if target.exists() {
            return Err(format!("备份目标已存在，拒绝覆盖：{}", target.display()));
        }
        let conn = self.connect()?;
        // `VACUUM INTO` 的目标只能是字面量（不接受绑定参数），单引号按 SQL 规则双写
        let literal = target.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("VACUUM INTO '{literal}'"))
            .map_err(|e| format!("备份数据库失败：{e}"))?;
        std::fs::metadata(target)
            .map(|meta| meta.len())
            .map_err(|e| format!("备份文件不可读：{e}"))
    }

    /// 数据库状态摘要（启动日志用）：WAL 是否开启 + 表数量
    pub fn describe(&self) -> String {
        match self.connect() {
            Ok(conn) => {
                let journal: String = conn
                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .unwrap_or_else(|_| "unknown".to_string());
                let tables: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(-1);
                format!("journal_mode={journal} tables={tables}")
            }
            Err(err) => format!("unavailable ({err})"),
        }
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

    pub fn find_user(
        &self,
        username: &str,
    ) -> Result<Option<(User, String, Option<String>)>, String> {
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

    /// 写入一组一次性恢复码（设计文档 3.1.4：库里只存哈希）
    pub fn set_recovery_codes(&self, user_id: i64, codes: &[String]) -> Result<(), String> {
        let hashes: Vec<String> = codes.iter().map(|code| recovery_code_hash(code)).collect();
        let blob = serde_json::to_string(&hashes).map_err(|e| format!("序列化恢复码失败：{e}"))?;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET totp_recovery_codes = ?1 WHERE id = ?2",
            params![blob, user_id],
        )
        .map_err(|e| format!("写入恢复码失败：{e}"))?;
        Ok(())
    }

    /// 校验并**用掉**一个恢复码（一次性）。
    ///
    /// 用事务做「读 → 删 → 写」，避免同一码被并发请求同时消费。
    pub fn consume_recovery_code(&self, user_id: i64, code: &str) -> Result<bool, String> {
        let normalized = normalize_recovery_code(code);
        // 恢复码固定 10 位字母数字，长度不对直接当失败，别去碰数据库
        if normalized.len() != 10 {
            return Ok(false);
        }
        let target = recovery_code_hash(code);

        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("开启事务失败：{e}"))?;
        let blob: Option<String> = tx
            .query_row(
                "SELECT totp_recovery_codes FROM users WHERE id = ?1",
                params![user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("读取恢复码失败：{e}"))?
            .flatten();
        let Some(blob) = blob else {
            return Ok(false);
        };
        let mut hashes: Vec<String> = serde_json::from_str(&blob).unwrap_or_default();
        let Some(index) = hashes.iter().position(|hash| *hash == target) else {
            return Ok(false);
        };
        hashes.remove(index);
        let remaining =
            serde_json::to_string(&hashes).map_err(|e| format!("序列化恢复码失败：{e}"))?;
        tx.execute(
            "UPDATE users SET totp_recovery_codes = ?1 WHERE id = ?2",
            params![remaining, user_id],
        )
        .map_err(|e| format!("更新恢复码失败：{e}"))?;
        tx.commit().map_err(|e| format!("提交失败：{e}"))?;
        Ok(true)
    }

    /// 关闭 2FA 时把剩余恢复码一并作废
    pub fn clear_recovery_codes(&self, user_id: i64) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET totp_recovery_codes = NULL WHERE id = ?1",
            params![user_id],
        )
        .map_err(|e| format!("清空恢复码失败：{e}"))?;
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
            "INSERT INTO sessions (token, user_id, created_at, expires_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token, user_id, now, now + SESSION_TTL, now],
        )
        .map_err(|e| format!("创建会话失败：{e}"))?;
        Ok(token)
    }

    /// 记下「刚刚重新验证过密码」：敏感操作的时间闸门依据。
    pub fn mark_verified(&self, token: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE sessions SET verified_at = ?1 WHERE token = ?2",
            params![now_secs(), token],
        )
        .map_err(|e| format!("更新验证时间失败：{e}"))?;
        Ok(())
    }

    /// 会话的完整状态（user_id + 最近验证时间）。
    ///
    /// 滑动续期（设计文档 3.1.3）：剩余不足半个 TTL 时把过期时间往后推，
    /// 但**不超过** `created_at + SESSION_ABSOLUTE_TTL`（绝对上限 30 天）。
    /// 只在临近过期时写库，避免每个请求都产生一次 UPDATE。
    pub fn session_state(&self, token: &str) -> Result<Option<SessionState>, String> {
        let conn = self.connect()?;
        let row: Option<(i64, i64, i64, i64)> = conn
            .query_row(
                "SELECT user_id, expires_at, created_at, verified_at FROM sessions WHERE token = ?1",
                params![token],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| format!("查询会话失败：{e}"))?;

        let now = now_secs();
        match row {
            Some((user_id, expires_at, created_at, verified_at)) if expires_at > now => {
                if expires_at - now <= SESSION_TTL / 2 {
                    let ceiling = created_at.saturating_add(SESSION_ABSOLUTE_TTL);
                    let renewed = (now + SESSION_TTL).min(ceiling);
                    if renewed > expires_at {
                        let _ = conn.execute(
                            "UPDATE sessions SET expires_at = ?1 WHERE token = ?2",
                            params![renewed, token],
                        );
                    }
                }
                Ok(Some(SessionState {
                    user_id,
                    verified_at,
                }))
            }
            Some(_) => {
                let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// 返回 user_id —— 过期会话视为无效并顺手清理（`session_state` 的薄包装）
    pub fn session_user(&self, token: &str) -> Result<Option<i64>, String> {
        Ok(self.session_state(token)?.map(|state| state.user_id))
    }

    pub fn delete_session(&self, token: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])
            .map_err(|e| format!("删除会话失败：{e}"))?;
        Ok(())
    }

    pub fn purge_expired(&self) -> Result<usize, String> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM sessions WHERE expires_at <= ?1",
            params![now_secs()],
        )
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
/// 敏感值的指纹（用于「同一个 ASMODAY_RESET_PASSWORD 只生效一次」，不落明文）
pub fn fingerprint(secret: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(secret.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn random_password() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    Alphanumeric.sample_string(&mut rand::rng(), 20)
}

/// 生成一次性恢复码（设计文档 3.1.4）：默认 10 个，`XXXXX-XXXXX` 的 Crockford Base32。
///
/// 10 位 × 32 字符表 ≈ 50 bit 熵，配合「用一次即失效」，SHA-1 指纹足够；
/// 库里只留指纹（`recovery_code_hash`），明文只在开启 2FA 的响应里出现一次。
pub fn generate_recovery_codes(count: usize) -> Vec<String> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut rng = rand::rng();
    (0..count)
        .map(|_| {
            let mut code = String::with_capacity(11);
            for index in 0..10 {
                if index == 5 {
                    code.push('-');
                }
                let pick = rand::Rng::random_range(&mut rng, 0..ALPHABET.len());
                code.push(ALPHABET[pick] as char);
            }
            code
        })
        .collect()
}

/// 归一化用户输入（大小写、空格、连字符都不计较；I/L→1、O→0 是 Crockford 的约定）
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect()
}

fn recovery_code_hash(code: &str) -> String {
    fingerprint(&format!(
        "asmoday-recovery:{}",
        normalize_recovery_code(code)
    ))
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
        if let Some(attempt) = map.get(key)
            && let Some(until) = attempt.blocked_until
        {
            let now = Instant::now();
            if until > now {
                return Err((until - now).as_secs().max(1));
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
    fn media_tables_upgrade_to_composite_key() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE favorites (file_path TEXT PRIMARY KEY, created_at INTEGER);
             CREATE TABLE play_history (
                 file_path TEXT PRIMARY KEY,
                 last_played_at INTEGER,
                 progress INTEGER
             );
             INSERT INTO favorites VALUES ('/m/a.mp4', 1);
             INSERT INTO play_history VALUES ('/m/b.mp4', 2, 3);",
        )
        .unwrap();

        rebuild_media_tables(&conn).unwrap();
        assert!(table_has_column(&conn, "favorites", "mount_id"));
        assert!(table_has_column(&conn, "play_history", "mount_id"));

        // 存量记录迁到空串挂载点（= 隐式根），路径与原值一字不差
        let (mount, path, created): (String, String, i64) = conn
            .query_row(
                "SELECT mount_id, file_path, created_at FROM favorites",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (mount.as_str(), path.as_str(), created),
            ("", "/m/a.mp4", 1)
        );
        let progress: i64 = conn
            .query_row("SELECT progress FROM play_history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(progress, 3);

        // 复合主键：同一个绝对路径可以同时挂在两个挂载点下
        conn.execute(
            "INSERT INTO favorites (mount_id, file_path, created_at) VALUES ('nas', '/m/a.mp4', 2)",
            [],
        )
        .unwrap();

        // 幂等：再跑一次不能丢数据
        rebuild_media_tables(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM favorites", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn backup_writes_a_usable_copy_and_never_overwrites() {
        let store = temp_store("backup");
        store.create_user("admin", "Asmoday#2026").unwrap();

        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join("auth-backup-copy.db");
        let _ = std::fs::remove_file(&out);

        let bytes = store.backup_to(&out).unwrap();
        assert!(bytes > 0, "备份必须真有内容");

        // 备份是完整可用的库，不是空壳
        let copy = Connection::open(&out).unwrap();
        let count: i64 = copy
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // 已存在的目标拒绝覆盖：静默冲掉上一份备份等于丢数据
        assert!(store.backup_to(&out).is_err());
        let _ = std::fs::remove_file(&out);
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
    fn thumb_hashes_roundtrip_and_prune() {
        let store = temp_store("thumbhash");

        // 写入 + 批量读回
        store.thumb_hash_set("key-a", "AAEC");
        store.thumb_hash_set("key-b", "BBED");
        store.thumb_hash_set("key-c", "CAFE");
        let got = store.thumb_hashes_get_many(&[
            "key-a".to_string(),
            "key-c".to_string(),
            "key-missing".to_string(),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("key-a").map(String::as_str), Some("AAEC"));
        assert_eq!(got.get("key-c").map(String::as_str), Some("CAFE"));
        assert!(!got.contains_key("key-missing"));

        // 覆盖写：同 key 只保留最新值
        store.thumb_hash_set("key-a", "ZZZZ");
        let got = store.thumb_hashes_get_many(&["key-a".to_string()]);
        assert_eq!(got.get("key-a").map(String::as_str), Some("ZZZZ"));

        // 裁剪：把 LRU 上限临时压到 2 行（直接用 SQL 模拟同一策略），确认删除逻辑生效
        let conn = store.connect().unwrap();
        conn.execute(
            "DELETE FROM thumb_hashes WHERE key NOT IN \
             (SELECT key FROM thumb_hashes ORDER BY updated_at DESC LIMIT 2)",
            [],
        )
        .unwrap();
        drop(conn);
        let got = store.thumb_hashes_get_many(&[
            "key-a".to_string(),
            "key-b".to_string(),
            "key-c".to_string(),
        ]);
        assert_eq!(got.len(), 2, "裁剪后只保留最新 2 行");

        // 清空（设置页「清理媒体缓存」联动）
        store.thumb_hashes_clear();
        let got = store.thumb_hashes_get_many(&["key-a".to_string()]);
        assert!(got.is_empty());
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

    #[test]
    fn recovery_codes_are_hashed_and_single_use() {
        let store = temp_store("recovery");
        let user = store.create_user("vfy_recovery", "Abcd1234").unwrap();
        let codes = generate_recovery_codes(10);
        assert_eq!(codes.len(), 10);
        for code in &codes {
            assert_eq!(code.len(), 11, "XXXXX-XXXXX 共 11 字符");
            assert_eq!(code.matches('-').count(), 1);
        }

        store.set_recovery_codes(user.id, &codes).unwrap();
        // 库里必须是哈希，不能出现任何一个明文
        let stored: String = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT totp_recovery_codes FROM users WHERE id = ?1",
                params![user.id],
                |row| row.get(0),
            )
            .unwrap();
        for code in &codes {
            assert!(!stored.contains(code), "恢复码明文不得落库");
        }

        // 大小写 / 连字符都不计较，且只能用一次
        let sloppy = codes[0].replace('-', "").to_lowercase();
        assert!(store.consume_recovery_code(user.id, &sloppy).unwrap());
        assert!(!store.consume_recovery_code(user.id, &codes[0]).unwrap());
        assert!(store.consume_recovery_code(user.id, &codes[1]).unwrap());

        // 关闭 2FA 时全部作废
        store.clear_recovery_codes(user.id).unwrap();
        assert!(!store.consume_recovery_code(user.id, &codes[2]).unwrap());
    }

    #[test]
    fn session_slides_but_never_past_absolute_cap() {
        let store = temp_store("session-slide");
        let user = store.create_user("vfy_slide", "Abcd1234").unwrap();
        let token = store.create_session(user.id).unwrap();
        let now = now_secs();

        // 29 天前创建、60 秒后过期：应被续期，但顶上绝对上限（created_at + 30 天）
        store
            .connect()
            .unwrap()
            .execute(
                "UPDATE sessions SET created_at = ?1, expires_at = ?2 WHERE token = ?3",
                params![now - 29 * 86_400, now + 60, token],
            )
            .unwrap();
        assert!(store.session_user(&token).unwrap().is_some());
        let (created, expires): (i64, i64) = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT created_at, expires_at FROM sessions WHERE token = ?1",
                params![token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(expires > now + 3600, "临近过期必须滑动续期");
        assert!(
            expires <= created + SESSION_ABSOLUTE_TTL,
            "不得越过 30 天绝对上限"
        );
    }
}
