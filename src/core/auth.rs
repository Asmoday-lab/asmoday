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

/// 会话的服务端状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    pub user_id: i64,
}
/// 连续失败次数上限
pub const MAX_ATTEMPTS: u32 = 5;
/// 封禁时长
pub const BAN_DURATION: Duration = Duration::from_secs(15 * 60);

/// 建表语句，字段与设计文档 4.1 对齐（sessions 为会话所必需，文档未列，此处补上）
///
/// v4.9.24 起为**最终结构**：多用户角色（role/is_active）与按用户隔离
/// （favorites/play_history/shares 的 user_id）一步到位。存量库由 v16 迁移补齐
/// （按缺列判断幂等），新库在 v1 就拿到最终结构、v16 钩子空跑。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT UNIQUE,
    password_hash TEXT,
    totp_secret TEXT,
    is_totp_enabled BOOLEAN DEFAULT 0,
    totp_recovery_codes TEXT,
    must_change_password BOOLEAN NOT NULL DEFAULT 0,
    created_at DATETIME,
    -- 多用户（v4.9.24）：'admin' | 'user'。管理类端点（用户/系统配置/备份）只放行 admin
    role TEXT NOT NULL DEFAULT 'user',
    -- 停用的用户拒绝登录，已有会话一并失效；记录保留，启用即恢复
    is_active BOOLEAN NOT NULL DEFAULT 1,
    -- 权限位（v4.9.25，对齐 Alist 位掩码）：admin 不看位全放行
    permissions INTEGER NOT NULL DEFAULT 511,
    -- 挂载点收权（v4.9.25，对齐 FileBrowser scope）：'*' = 全部；否则 JSON 数组
    mount_scope TEXT NOT NULL DEFAULT '*'
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
    deleted_at INTEGER,
    -- 归属（v4.9.24）：创建者；列表/删除按归属过滤，公开访问仍按 token 匿名
    user_id INTEGER NOT NULL DEFAULT 1
);

-- 收藏与播放历史（设计文档 4.1）：主键是 (user_id, mount_id, file_path)。
-- file_path 一直是绝对路径；mount_id 用于「按挂载点批量清理 / 分组展示」；
-- user_id 让同一份共享文件库里每个人有独立的收藏与播放进度（v4.9.24）。
CREATE TABLE IF NOT EXISTS favorites (
    user_id INTEGER NOT NULL DEFAULT 1,
    mount_id TEXT NOT NULL DEFAULT '',
    file_path TEXT NOT NULL,
    created_at INTEGER,
    PRIMARY KEY (user_id, mount_id, file_path)
);

CREATE TABLE IF NOT EXISTS play_history (
    user_id INTEGER NOT NULL DEFAULT 1,
    mount_id TEXT NOT NULL DEFAULT '',
    file_path TEXT NOT NULL,
    last_played_at INTEGER,
    progress INTEGER,
    PRIMARY KEY (user_id, mount_id, file_path)
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
    -- v4.4.1 前的「5 分钟二次验证闸门」依据，闸门已移除；
    -- 保留列只为存量库平滑升级，代码不再读写
    verified_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS thumb_hashes (
    key TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;

/// 权限位（v4.9.25，对齐 Alist 的 Permission 位掩码）。
///
/// 「读」不设位——挂载点在 scope 内即可读（社区同口径：FileBrowser/Alist 的
/// 普通用户默认可见可下）；写类动作逐位收权。
pub mod perms {
    /// 看隐藏文件（list / search / WebDAV PROPFIND 的 show_hidden）
    pub const SEE_HIDDEN: i64 = 1 << 0;
    /// 新建：mkdir / 上传 / touch
    pub const WRITE: i64 = 1 << 1;
    /// 重命名
    pub const RENAME: i64 = 1 << 2;
    /// 移动 / 复制
    pub const MOVE_COPY: i64 = 1 << 3;
    /// 删除（含回收确认）
    pub const DELETE: i64 = 1 << 4;
    /// 创建分享
    pub const SHARE: i64 = 1 << 5;
    /// WebDAV 读取（PROPFIND / GET / HEAD）
    pub const WEBDAV_READ: i64 = 1 << 6;
    /// WebDAV 管理（PUT / DELETE / MKCOL / MOVE / COPY 等）
    pub const WEBDAV_MANAGE: i64 = 1 << 7;
    /// 终端（社区通行：普通用户不给 shell，这里保留开关能力）
    pub const TERMINAL: i64 = 1 << 8;
    /// 全量位掩码
    pub const ALL: i64 = (1 << 9) - 1;
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub totp_enabled: bool,
    /// 一次性密码（--init / --reset-password / 管理员代设）登录后必须改密
    pub must_change_password: bool,
    /// 'admin' | 'user'（v4.9.24 多用户）
    pub role: String,
    /// 停用的账号拒绝登录（记录保留，启用即恢复）
    pub is_active: bool,
    /// 权限位掩码（v4.9.25）；admin 不看位全放行
    pub permissions: i64,
    /// 挂载点收权：'*' = 全部；否则 JSON 数组（v4.9.25）
    pub mount_scope: String,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    /// 权限位判定。admin 恒真；普通用户按位。
    pub fn can(&self, perm: i64) -> bool {
        self.is_admin() || (self.permissions & perm) != 0
    }

    /// 挂载点是否全部可见（'*' 或 admin）。
    pub fn mounts_all(&self) -> bool {
        self.is_admin() || self.mount_scope.trim() == "*"
    }

    /// scope 内挂载点清单（`mounts_all` 为真时返回 None）。
    pub fn allowed_mounts(&self) -> Option<Vec<String>> {
        if self.mounts_all() {
            return None;
        }
        Some(
            serde_json::from_str::<Vec<String>>(&self.mount_scope)
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.replace('\\', "/").trim_end_matches('/').to_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
        )
    }

    /// 挂载点展示路径（或其下的子路径）是否在 scope 内。大小写不敏感、前缀匹配，
    /// 与 `MountSet::find_by_path_text` 同口径。
    pub fn mount_allowed(&self, display_path: &str) -> bool {
        if self.mounts_all() {
            return true;
        }
        let needle = display_path
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        self.allowed_mounts()
            .unwrap_or_default()
            .iter()
            .any(|p| needle == *p || needle.starts_with(&format!("{p}/")))
    }
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
    // v4.4.1 前的二次验证时间戳；闸门已移除，仅为存量库兼容保留
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
/// - **v3** 目录快照缓存（`dir_snapshots`，见 `core/dir_cache.rs`）。
/// - **v4** 媒体库快照（`media_snapshots`，带 size / mtime，见 `core/dir_cache.rs`）。
/// - **v5** 媒体索引（`media_assets` / `media_dirs` / `media_roots`，见 `core/media_index.rs`）。
/// - **v6** 名称排序键 `media_assets.sort_name`（自然序，v4.9.0）+ 触发一次索引重建。
/// - **v7** 音频时长缓存；**v8** 音频标签缓存（按 `path+mtime+size` 失效）；
///   **v9** 外挂字幕探测缓存（按 `path+mtime` 失效，NULL 表示「确认没有」）。
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
        // v3：目录快照缓存。只是加速用的旁路数据，清空也不影响任何功能，
        // 所以不设外键、不随业务表迁移；`entries` 是自定义二进制（见 dir_cache.rs）
        M::up(
            "CREATE TABLE IF NOT EXISTS dir_snapshots (
                path TEXT PRIMARY KEY,
                stamp INTEGER NOT NULL,
                entries BLOB NOT NULL,
                entry_count INTEGER NOT NULL,
                used_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_dir_snapshots_used_at ON dir_snapshots(used_at);",
        ),
        // v4：媒体库快照（带 size / mtime）。与 v3 同一套思路、不同的条目格式，
        // 所以单独一张表 —— 两张表的 blob 互相喂给对方都会解码失败，混存只会更难排查
        M::up(
            "CREATE TABLE IF NOT EXISTS media_snapshots (
                path TEXT PRIMARY KEY,
                stamp INTEGER NOT NULL,
                entries BLOB NOT NULL,
                entry_count INTEGER NOT NULL,
                used_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_media_snapshots_used_at ON media_snapshots(used_at);",
        ),
        // v5：媒体索引（v4.7.0，见 core/media_index.rs）。与 v3/v4 的区别是它不是
        // 「按目录缓存原始条目」，而是把每个媒体文件摊平成一行 —— 分页、计数、相册
        // 聚合全走 SQL，翻页不再重扫整棵树
        M::up(
            "CREATE TABLE IF NOT EXISTS media_assets (
                path   TEXT PRIMARY KEY,
                real   TEXT NOT NULL,
                dir    TEXT NOT NULL,
                depth  INTEGER NOT NULL,
                name   TEXT NOT NULL,
                kind   TEXT NOT NULL,
                ext    TEXT NOT NULL,
                size   INTEGER NOT NULL,
                mtime  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_media_assets_dir ON media_assets(dir, mtime DESC);
            CREATE INDEX IF NOT EXISTS idx_media_assets_time ON media_assets(mtime DESC, path);
            CREATE TABLE IF NOT EXISTS media_dirs (
                dir        TEXT PRIMARY KEY,
                depth      INTEGER NOT NULL,
                stamp      INTEGER NOT NULL,
                indexed_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS media_roots (
                root      TEXT PRIMARY KEY,
                depth     INTEGER NOT NULL,
                synced_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS media_dims (
                path  TEXT PRIMARY KEY,
                mtime INTEGER NOT NULL,
                size  INTEGER NOT NULL,
                w     INTEGER NOT NULL,
                h     INTEGER NOT NULL
            );",
        ),
        // v6：名称排序键（v4.9.0）。`sort_name` = 小写 + 数字段补零的自然序键，
        // 让「第2集」排在「第10集」前面，同时仍能走 SQL 索引分页（见 core/media_index.rs）。
        //
        // `stamp = -1` 是刻意的：存量行的 sort_name 是空串，只有重新走一遍目录才能填上。
        // 置 -1 之后后台索引任务会在下一轮逐目录重写（**不清数据**，重建期间浏览照常，
        // 只是按名称排序的旧行会短暂排在前面）。清表也能达到目的，但会让媒体库空一段时间。
        M::up(
            "ALTER TABLE media_assets ADD COLUMN sort_name TEXT NOT NULL DEFAULT '';
            CREATE INDEX IF NOT EXISTS idx_media_assets_name ON media_assets(sort_name, path);
            CREATE INDEX IF NOT EXISTS idx_media_assets_size ON media_assets(size);
            UPDATE media_dirs SET stamp = -1;",
        ),
        // v7：音频时长（v4.9.5 音乐库）。单独一张表而不是往 `media_assets` 加列 ——
        // `replace_dir` 是「先删后插」，加列会在每次重扫目录时把算好的时长清零。
        // 键与 `media_dims` 同构：`path + mtime + size`，文件一改就自动失效。
        M::up(
            "CREATE TABLE IF NOT EXISTS media_durations (
                path  TEXT PRIMARY KEY,
                mtime INTEGER NOT NULL,
                size  INTEGER NOT NULL,
                ms    INTEGER NOT NULL
            );",
        ),
        // v8：音频标签缓存（v4.9.8）。音乐库列表原先每页要读 120 次 ID3，
        // 在 SMB/UNC 上单次 0.5~1 秒 —— 这就是「进音乐库等二三十秒」的来源。
        // 键与 `media_durations` 同构，文件一改（mtime/size 变）自动失效。
        M::up(
            "CREATE TABLE IF NOT EXISTS media_tags (
                path      TEXT PRIMARY KEY,
                mtime     INTEGER NOT NULL,
                size      INTEGER NOT NULL,
                title     TEXT,
                artist    TEXT,
                album     TEXT,
                has_cover INTEGER NOT NULL
            );",
        ),
        // v9：外挂字幕探测缓存（v4.9.9）。影视库每页要为每条视频做两次 `is_file`
        // （.vtt / .srt），4002 个视频翻起来就是上万次 UNC stat。
        // `sub_path` 为 NULL 表示「查过了，没有字幕」—— 负缓存同样重要，
        // 否则每次都要重新确认一遍「确实没有」。`checked_at` 给负缓存兜一个
        // 保鲜期：用户后来补了 .srt，视频本身的 mtime 并不会变。
        M::up(
            "CREATE TABLE IF NOT EXISTS media_subtitles (
                path       TEXT PRIMARY KEY,
                mtime      INTEGER NOT NULL,
                sub_path   TEXT,
                checked_at INTEGER NOT NULL DEFAULT 0
            );",
        ),
        // v10：缩略图预生成台账（v4.9.16）。此前候选只按 `mtime DESC LIMIT N` 取，
        // 不管有没有生成过 —— 库没变时每轮扫描都把最新的同一批白跑一遍（日志里
        // 21 次 `preheat started: 6/6 generated` 全是缓存命中），且窗口内图片超过
        // N 张时旧图永远排不进来。记录「这条文件已经预生成过」，键与
        // `media_dims` 同构：文件一改（mtime/size 变）自动重新入列。
        // `at` 给台账保鲜期（7 天）：janitor 按 max_age 淘汰缩略图时不清台账，
        // 没有 TTL 的话被淘汰的那批永远不再补（v4.9.16 审查修复）。
        M::up(
            "CREATE TABLE IF NOT EXISTS preheated (
                path  TEXT PRIMARY KEY,
                mtime INTEGER NOT NULL,
                size  INTEGER NOT NULL,
                at    INTEGER NOT NULL
            );",
        ),
        // v11：标签解码口径变了（v4.9.21 起对「GBK 内容 + ISO-8859-1 声明」做回退）。
        // 缓存键是 `path + mtime + size`，解码逻辑变了文件本身却没变，不清一次的话
        // 修好的解码器永远读不到老行 —— 用户重启后界面照旧乱码。
        M::up("DELETE FROM media_tags;"),
        // v12：清掉历史遗留的 `\\?\UNC\…` 键。早期版本用 `real`（verbatim 路径）当
        // 标签/时长缓存的键，后来统一成展示路径，这批行再也读不到，白占库还干扰排查。
        // 展示路径一律是 `/` 分隔，含反斜杠的一定是旧键。
        M::up("DELETE FROM media_durations WHERE instr(path, '\\') > 0;"),
        // v13：歌词探测缓存（v4.9.21）。音乐库每首按名字猜 `<文件名>.lrc`，
        // 没有就是一次 SMB 404（实测 0.3~3.8 秒）。这里记「查过了、有没有」：
        // `lrc_path` 为 NULL = 确认没有，`checked_at` 给负缓存兜保鲜期 ——
        // 用户后来补了 .lrc，音频自身的 mtime 不会变。
        M::up(
            "CREATE TABLE IF NOT EXISTS media_lyrics (
                path       TEXT PRIMARY KEY,
                mtime      INTEGER NOT NULL,
                lrc_path   TEXT,
                checked_at INTEGER NOT NULL DEFAULT 0
            );",
        ),
        // v14：字幕缓存里同样是「verbatim 路径」当键（v4.9.21 之前 `build_items`
        // 用 `entry.path` 而不是展示路径）。现在两条探测路径统一成展示路径，
        // 旧键读不到也永远不会被覆盖，清掉重探一遍最省事。
        M::up("DELETE FROM media_subtitles WHERE instr(path, '\\') > 0;"),
        M::up("DELETE FROM media_lyrics WHERE instr(path, '\\') > 0;"),
        // v15：同理清掉标签缓存里的 verbatim 键。v11 是整表清空，但已经跑过 v11 的库
        // 不会重跑，只有这一条能保证「开发机中途升级」也不会留双份。
        M::up("DELETE FROM media_tags WHERE instr(path, '\\') > 0;"),
        // v16：多用户（v4.9.24）。三件事，全部按缺列判断幂等（新库在 v1 已是最终结构）：
        // ① users 补 role / is_active，id 最小的存量用户升为 admin；
        // ② favorites / play_history 重建主键为 (user_id, mount_id, file_path)，
        //    存量行归属到第一个用户（单用户时代本来也只有他一个人在用）；
        // ③ shares 补 user_id（owner），access_tokens 的 NULL user_id 回填。
        M::up_with_hook("", |tx: &rusqlite::Transaction| {
            migrate_multi_user(tx)?;
            Ok(())
        }),
        // v17：权限位 + 挂载点收权（v4.9.25）。存量用户回填全量位 + '*' scope——
        // 升级即保持升级前「能看到全部挂载点、能做全部动作」的现状。
        M::up_with_hook("", |tx: &rusqlite::Transaction| {
            migrate_user_perms(tx)?;
            Ok(())
        }),
    ])
}

/// v17 钩子：users 补 permissions / mount_scope。**不要**写 BEGIN/COMMIT（已在事务里）。
fn migrate_user_perms(conn: &Connection) -> rusqlite::Result<()> {
    if !table_has_column(conn, "users", "permissions") {
        conn.execute_batch(
            "ALTER TABLE users ADD COLUMN permissions INTEGER NOT NULL DEFAULT 511;
             ALTER TABLE users ADD COLUMN mount_scope TEXT NOT NULL DEFAULT '*';",
        )?;
    }
    Ok(())
}

/// v16 钩子：见上面迁移注释。**不要**在这里写 `BEGIN`/`COMMIT`——迁移本身跑在事务里。
fn migrate_multi_user(conn: &Connection) -> rusqlite::Result<()> {
    // ① users 角色 + 启停
    if !table_has_column(conn, "users", "role") {
        conn.execute_batch(
            "ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'user';
             ALTER TABLE users ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT 1;
             UPDATE users SET role = 'admin' WHERE id = (SELECT MIN(id) FROM users);",
        )?;
    }
    // ② 收藏 / 播放历史：主键加 user_id，需重建表
    let owner = "(SELECT COALESCE(MIN(id), 1) FROM users)";
    if !table_has_column(conn, "favorites", "user_id") {
        conn.execute_batch(&format!(
            "CREATE TABLE favorites_v17 (
                 user_id INTEGER NOT NULL DEFAULT 1,
                 mount_id TEXT NOT NULL DEFAULT '',
                 file_path TEXT NOT NULL,
                 created_at INTEGER,
                 PRIMARY KEY (user_id, mount_id, file_path)
             );
             INSERT OR IGNORE INTO favorites_v17 (user_id, mount_id, file_path, created_at)
                 SELECT {owner}, mount_id, file_path, created_at FROM favorites;
             DROP TABLE favorites;
             ALTER TABLE favorites_v17 RENAME TO favorites;"
        ))?;
    }
    if !table_has_column(conn, "play_history", "user_id") {
        conn.execute_batch(&format!(
            "CREATE TABLE play_history_v17 (
                 user_id INTEGER NOT NULL DEFAULT 1,
                 mount_id TEXT NOT NULL DEFAULT '',
                 file_path TEXT NOT NULL,
                 last_played_at INTEGER,
                 progress INTEGER,
                 PRIMARY KEY (user_id, mount_id, file_path)
             );
             INSERT OR IGNORE INTO play_history_v17 (user_id, mount_id, file_path, last_played_at, progress)
                 SELECT {owner}, mount_id, file_path, last_played_at, progress FROM play_history;
             DROP TABLE play_history;
             ALTER TABLE play_history_v17 RENAME TO play_history;"
        ))?;
    }
    // ③ 分享归属 + 令牌归属
    if !table_has_column(conn, "shares", "user_id") {
        conn.execute_batch(&format!(
            "ALTER TABLE shares ADD COLUMN user_id INTEGER NOT NULL DEFAULT 1;
             UPDATE shares SET user_id = {owner} WHERE user_id IS NULL OR user_id = 0;"
        ))?;
    }
    if table_has_column(conn, "access_tokens", "user_id") {
        conn.execute(
            "UPDATE access_tokens SET user_id = (SELECT COALESCE(MIN(id), 1) FROM users)
             WHERE user_id IS NULL",
            [],
        )?;
    }
    Ok(())
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

    /// 删除指定键的 ThumbHash（文件删除 / 改名后调用，避免库里留死行）
    pub fn thumb_hashes_forget(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let Ok(conn) = self.connect() else {
            return;
        };
        let Ok(mut stmt) = conn.prepare("DELETE FROM thumb_hashes WHERE key = ?1") else {
            return;
        };
        for key in keys {
            let _ = stmt.execute(params![key]);
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
        let before: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);
        migrations()
            .to_latest(&mut conn)
            .map_err(|e| format!("迁移数据库结构失败：{e}"))?;
        // v6 会把所有目录戳置 -1，逼后台索引重写一遍 `sort_name`。索引是派生数据，
        // 重建期间浏览照常；但用户若在这时按名称排序会发现顺序「有点乱」，
        // 启动日志里得有一条能对上号的原因（NAS 上两万条要跑几分钟）。
        if before < 6 {
            crate::log_info!(
                "数据库已升级到 v6：媒体索引将在后台重建（补全名称排序键），期间按时间排序不受影响"
            );
        }
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

    /// 是否已进入多用户模式（v4.9.27）。
    ///
    /// 产品语义：默认安装就是**单用户**——只有一个管理员，所有能力全开，
    /// 界面上不出现权限位与挂载点收权；管理员第一次新建账号后才启用多用户。
    /// 判据是「账号总数 > 1」而不是「存在非 admin 账号」：把最后一个普通
    /// 用户停用或降级时，他名下的权限配置还在，不该让管理界面在那一刻消失。
    pub fn multi_user(&self) -> Result<bool, String> {
        Ok(self.user_count()? > 1)
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
                // 前缀匹配用 substr 而不是 `LIKE ?1 || '/%'`（v4.9.22）：
                // LIKE 里 `_` 匹配任意单字符、`%` 匹配任意串，用户路径里的
                // `IMG_1234` / `2024%` 会连带命中 `IMG-1234` 之类的无关路径。
                "SELECT COUNT(*) FROM shares WHERE deleted_at IS NULL \
                 AND (file_path = ?1 OR substr(file_path, 1, length(?1) + 1) = ?1 || '/')",
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
        //
        // 前缀判断统一用 substr（v4.9.22）：`LIKE ?1 || '/%'` 会把路径里的
        // `_`（任意单字符）和 `%`（任意串）当通配符，删一个 `IMG_1234.jpg`
        // 会顺手清掉 `IMG-1234.jpg` 的收藏与分享，且不报任何错。
        for table in ["favorites", "play_history"] {
            let sql = format!(
                "UPDATE OR REPLACE {table} SET file_path = ?2 || substr(file_path, length(?1) + 1)
                 WHERE file_path = ?1 OR substr(file_path, 1, length(?1) + 1) = ?1 || '/'"
            );
            tx.execute(&sql, params![old, new])
                .map_err(|e| format!("更新 {table} 失败：{e}"))?;
        }
        tx.execute(
            "UPDATE shares SET file_path = ?2 || substr(file_path, length(?1) + 1)
             WHERE (file_path = ?1 OR substr(file_path, 1, length(?1) + 1) = ?1 || '/')
               AND deleted_at IS NULL",
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
            let sql = format!(
                "DELETE FROM {table} WHERE file_path = ?1 \
                 OR substr(file_path, 1, length(?1) + 1) = ?1 || '/'"
            );
            tx.execute(&sql, [path])
                .map_err(|e| format!("清理 {table} 失败：{e}"))?;
        }
        tx.execute(
            "UPDATE shares SET deleted_at = ?2
             WHERE (file_path = ?1 OR substr(file_path, 1, length(?1) + 1) = ?1 || '/')
               AND deleted_at IS NULL",
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

    /// 创建用户（管理员建号 / 首启自动引导共用）。
    ///
    /// `role` 为 `"admin"` 或 `"user"`。管理员代建与一次性引导都置
    /// `must_change_password = 1`——初始密码经由控制台/管理员传递，
    /// 登录后强制换掉（对齐 FileBrowser Quantum 的 requirePasswordChange）。
    pub fn create_user(&self, username: &str, password: &str, role: &str) -> Result<User, String> {
        let username = username.trim();
        if !valid_username(username) {
            return Err("用户名需 2~32 个字符，可含中文、字母、数字与 _ - . @".to_string());
        }
        if role != "admin" && role != "user" {
            return Err(format!("未知角色：{role}"));
        }
        check_password_strength(password)?;

        let hash = hash_password(password)?;
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO users (username, password_hash, created_at, role, must_change_password,
                                permissions, mount_scope)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, '*')",
            params![username, hash, now_secs(), role, perms::ALL],
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
            must_change_password: true,
            role: role.to_string(),
            is_active: true,
            permissions: perms::ALL,
            mount_scope: "*".to_string(),
        })
    }

    pub fn find_user(
        &self,
        username: &str,
    ) -> Result<Option<(User, String, Option<String>)>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, username, password_hash, totp_secret, is_totp_enabled, must_change_password,
                    role, is_active, permissions, mount_scope
             FROM users WHERE username = ?1",
            params![username],
            |row| {
                Ok((
                    User {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        totp_enabled: row.get::<_, i64>(4).unwrap_or(0) != 0,
                        must_change_password: row.get::<_, i64>(5).unwrap_or(0) != 0,
                        role: row.get::<_, String>(6).unwrap_or_else(|_| "user".into()),
                        is_active: row.get::<_, i64>(7).unwrap_or(1) != 0,
                        permissions: row.get::<_, i64>(8).unwrap_or(perms::ALL),
                        mount_scope: row
                            .get::<_, String>(9)
                            .unwrap_or_else(|_| "*".into()),
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
            "SELECT id, username, is_totp_enabled, must_change_password, role, is_active,
                    permissions, mount_scope
             FROM users ORDER BY id LIMIT 1",
            [],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    totp_enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
                    must_change_password: row.get::<_, i64>(3).unwrap_or(0) != 0,
                    role: row.get::<_, String>(4).unwrap_or_else(|_| "user".into()),
                    is_active: row.get::<_, i64>(5).unwrap_or(1) != 0,
                    permissions: row.get::<_, i64>(6).unwrap_or(perms::ALL),
                    mount_scope: row.get::<_, String>(7).unwrap_or_else(|_| "*".into()),
                })
            },
        )
        .optional()
        .map_err(|e| format!("查询用户失败：{e}"))
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>, String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, username, is_totp_enabled, must_change_password, role, is_active,
                    permissions, mount_scope
             FROM users WHERE id = ?1",
            params![id],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    totp_enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
                    must_change_password: row.get::<_, i64>(3).unwrap_or(0) != 0,
                    role: row.get::<_, String>(4).unwrap_or_else(|_| "user".into()),
                    is_active: row.get::<_, i64>(5).unwrap_or(1) != 0,
                    permissions: row.get::<_, i64>(6).unwrap_or(perms::ALL),
                    mount_scope: row.get::<_, String>(7).unwrap_or_else(|_| "*".into()),
                })
            },
        )
        .optional()
        .map_err(|e| format!("查询用户失败：{e}"))
    }

    /// 用户清单（管理页）：按 id 排序，附账号状态。不含密码哈希等敏感列。
    pub fn list_users(&self) -> Result<Vec<User>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, username, is_totp_enabled, must_change_password, role, is_active,
                        permissions, mount_scope
                 FROM users ORDER BY id",
            )
            .map_err(|e| format!("查询用户失败：{e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(User {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    totp_enabled: row.get::<_, i64>(2).unwrap_or(0) != 0,
                    must_change_password: row.get::<_, i64>(3).unwrap_or(0) != 0,
                    role: row.get::<_, String>(4).unwrap_or_else(|_| "user".into()),
                    is_active: row.get::<_, i64>(5).unwrap_or(1) != 0,
                    permissions: row.get::<_, i64>(6).unwrap_or(perms::ALL),
                    mount_scope: row.get::<_, String>(7).unwrap_or_else(|_| "*".into()),
                })
            })
            .map_err(|e| format!("查询用户失败：{e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取用户失败：{e}"))
    }

    /// 改名。UNIQUE 冲突返回「用户名已存在」；改名后由调用方负责让该用户全部会话失效
    /// （旧会话里缓存的用户名/身份要重新登录确认）。
    pub fn rename_user(&self, user_id: i64, new_username: &str) -> Result<(), String> {
        let username = new_username.trim();
        if !valid_username(username) {
            return Err("用户名需 2~32 个字符，可含中文、字母、数字与 _ - . @".to_string());
        }
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET username = ?1 WHERE id = ?2",
            params![username, user_id],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                "用户名已存在".to_string()
            } else {
                format!("改名失败：{e}")
            }
        })?;
        Ok(())
    }

    /// 启用 / 停用账号。停用即踢掉该用户全部会话。
    /// **最后一个 admin 不能停用**（否则没有人能再进管理页）。
    pub fn set_user_active(&self, user_id: i64, active: bool) -> Result<(), String> {
        let conn = self.connect()?;
        if !active {
            ensure_not_last_admin(&conn, user_id, "不能停用最后一个管理员")?;
        }
        conn.execute(
            "UPDATE users SET is_active = ?1 WHERE id = ?2",
            params![if active { 1 } else { 0 }, user_id],
        )
        .map_err(|e| format!("更新账号状态失败：{e}"))?;
        if !active {
            conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
                .map_err(|e| format!("清理会话失败：{e}"))?;
        }
        Ok(())
    }

    /// 调整角色。**最后一个 admin 不能降级**。
    pub fn set_user_role(&self, user_id: i64, role: &str) -> Result<(), String> {
        if role != "admin" && role != "user" {
            return Err(format!("未知角色：{role}"));
        }
        let conn = self.connect()?;
        if role != "admin" {
            ensure_not_last_admin(&conn, user_id, "不能降级最后一个管理员")?;
        }
        conn.execute(
            "UPDATE users SET role = ?1 WHERE id = ?2",
            params![role, user_id],
        )
        .map_err(|e| format!("更新角色失败：{e}"))?;
        Ok(())
    }

    /// 更新权限位与挂载点收权（v4.9.25）。
    ///
    /// `mount_scope` 为 `"*"` 或挂载点展示路径的 JSON 数组（空数组 = 什么都看不到，
    /// 允许——管理员可以先收光再慢慢放）。改完由调用方决定是否清会话：权限收紧
    /// 立即生效（AuthUser 每请求重读用户行），无需踢登录。
    pub fn set_user_perms(
        &self,
        user_id: i64,
        permissions: i64,
        mount_scope: &str,
    ) -> Result<(), String> {
        let permissions = if permissions < 0 {
            0
        } else {
            permissions & perms::ALL
        };
        let scope = mount_scope.trim();
        if scope != "*" {
            let parsed: Vec<String> = serde_json::from_str(scope)
                .map_err(|_| "mount_scope 需为 \"*\" 或路径字符串的 JSON 数组".to_string())?;
            for p in &parsed {
                if p.trim().is_empty() {
                    return Err("mount_scope 数组里出现了空路径".to_string());
                }
            }
        }
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET permissions = ?1, mount_scope = ?2 WHERE id = ?3",
            params![permissions, scope, user_id],
        )
        .map_err(|e| format!("更新权限失败：{e}"))?;
        Ok(())
    }

    /// 管理员代设密码：与自助改密不同，这里保留 must_change_password=1，
    /// 让用户下次登录自己换掉（管理员不该知道用户的长期密码）。
    pub fn admin_set_password(&self, user_id: i64, password: &str) -> Result<(), String> {
        check_password_strength(password)?;
        let hash = hash_password(password)?;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 1 WHERE id = ?2",
            params![hash, user_id],
        )
        .map_err(|e| format!("设置密码失败：{e}"))?;
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])
            .map_err(|e| format!("清理会话失败：{e}"))?;
        Ok(())
    }

    /// 删除用户及其全部归属数据（会话/收藏/播放历史/分享/令牌）。
    /// 不能删自己；**不能删最后一个 admin**。
    pub fn delete_user(&self, user_id: i64, caller_id: i64) -> Result<(), String> {
        if user_id == caller_id {
            return Err("不能删除当前登录的账号".to_string());
        }
        let conn = self.connect()?;
        ensure_not_last_admin(&conn, user_id, "不能删除最后一个管理员")?;
        for sql in [
            "DELETE FROM sessions WHERE user_id = ?1",
            "DELETE FROM favorites WHERE user_id = ?1",
            "DELETE FROM play_history WHERE user_id = ?1",
            "DELETE FROM access_tokens WHERE user_id = ?1",
        ] {
            conn.execute(sql, params![user_id])
                .map_err(|e| format!("清理用户数据失败：{e}"))?;
        }
        // 分享不物理删：转 tombstone（30 天后 janitor 回收），公开链接立即 410
        conn.execute(
            "UPDATE shares SET deleted_at = ?2 WHERE user_id = ?1 AND deleted_at IS NULL",
            params![user_id, now_secs()],
        )
        .map_err(|e| format!("清理用户分享失败：{e}"))?;
        conn.execute("DELETE FROM users WHERE id = ?1", params![user_id])
            .map_err(|e| format!("删除用户失败：{e}"))?;
        Ok(())
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
            "INSERT INTO sessions (token, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_digest(&token), user_id, now, now + SESSION_TTL],
        )
        .map_err(|e| format!("创建会话失败：{e}"))?;
        Ok(token)
    }

    /// 会话状态（user_id）。
    ///
    /// 滑动续期（设计文档 3.1.3）：剩余不足半个 TTL 时把过期时间往后推，
    /// 但**不超过** `created_at + SESSION_ABSOLUTE_TTL`（绝对上限 30 天）。
    /// 只在临近过期时写库，避免每个请求都产生一次 UPDATE。
    ///
    /// 入库的 token 是 SHA-256 摘要（见 `session_digest`）：查库 / 删库 /
    /// 续期全部按摘要走，明文只存在于浏览器 Cookie 里。
    pub fn session_state(&self, token: &str) -> Result<Option<SessionState>, String> {
        let conn = self.connect()?;
        let digest = session_digest(token);
        let row: Option<(i64, i64, i64)> = conn
            .query_row(
                "SELECT user_id, expires_at, created_at FROM sessions WHERE token = ?1",
                params![digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| format!("查询会话失败：{e}"))?;

        let now = now_secs();
        match row {
            Some((user_id, expires_at, created_at)) if expires_at > now => {
                if expires_at - now <= SESSION_TTL / 2 {
                    let ceiling = created_at.saturating_add(SESSION_ABSOLUTE_TTL);
                    let renewed = (now + SESSION_TTL).min(ceiling);
                    if renewed > expires_at {
                        let _ = conn.execute(
                            "UPDATE sessions SET expires_at = ?1 WHERE token = ?2",
                            params![renewed, digest],
                        );
                    }
                }
                Ok(Some(SessionState { user_id }))
            }
            Some(_) => {
                let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![digest]);
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
        conn.execute(
            "DELETE FROM sessions WHERE token = ?1",
            params![session_digest(token)],
        )
        .map_err(|e| format!("删除会话失败：{e}"))?;
        Ok(())
    }

    /// 高敏操作的「近期密码验证」窗口（当前仅备份下载使用）。
    ///
    /// 有意**不做** v4.4.1 那种全局闸门——那版 5 分钟弹一次密码，太扰人，已删。
    /// 这里只在用户主动请求高敏操作时要求确认一次，窗口内免重复输入。
    pub const SENSITIVE_VERIFY_TTL: i64 = 15 * 60;

    /// 记录一次密码验证（复用 v4.4.1 遗留的 `sessions.verified_at` 列）。
    pub fn mark_session_verified(&self, token: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE sessions SET verified_at = ?1 WHERE token = ?2",
            params![now_secs(), session_digest(token)],
        )
        .map_err(|e| format!("更新验证时间失败：{e}"))?;
        Ok(())
    }

    /// 当前会话是否在 `SENSITIVE_VERIFY_TTL` 窗口内验证过密码。
    pub fn session_verified_recently(&self, token: &str) -> bool {
        let Ok(conn) = self.connect() else {
            return false;
        };
        conn.query_row(
            "SELECT verified_at FROM sessions WHERE token = ?1",
            params![session_digest(token)],
            |row| row.get::<_, i64>(0),
        )
        .map(|verified_at| {
            verified_at > 0 && now_secs() - verified_at <= Self::SENSITIVE_VERIFY_TTL
        })
        .unwrap_or(false)
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

/// 用户名规则：2~32 个字符；中文/字母/数字与 `_ - . @`；不能全是空格或控制字符。
/// 中文按字符计宽（一个汉字算 1 个，不按字节），与前端展示一致。
pub fn valid_username(username: &str) -> bool {
    let trimmed = username.trim();
    let len = trimmed.chars().count();
    if !(2..=32).contains(&len) {
        return false;
    }
    trimmed.chars().all(|c| {
        !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    })
}

/// 守护「系统里至少留一个可登录的 admin」：停用/降级/删除目标前调用。
/// 单行检查与写操作之间没有事务包裹——管理操作是人工低频动作，
/// 且最坏结果（两个 admin 同时被降）需要两个管理员并发操作自己，可接受。
fn ensure_not_last_admin(conn: &Connection, user_id: i64, message: &str) -> Result<(), String> {
    let is_admin: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM users WHERE id = ?1 AND role = 'admin'",
            params![user_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("查询角色失败：{e}"))?;
    if is_admin == 0 {
        return Ok(());
    }
    let other_admins: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND id != ?1 AND is_active != 0",
            params![user_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("查询管理员数失败：{e}"))?;
    if other_admins == 0 {
        return Err(message.to_string());
    }
    Ok(())
}

/// 32 字节随机 token（16 进制）
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 会话 token 的入库摘要（SHA-256 hex，2026-11-04 审查）。
///
/// `sessions.token` 列**只存摘要**，明文只存在于浏览器 Cookie 里——
/// 数据库文件被拷走（备份外泄 / 挂载点被读）也拿不到可直接使用的会话。
/// token 本身是 256 位随机值，等值查询走索引与明文同速。
pub(crate) fn session_digest(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
    fn user_crud_roles_and_last_admin_guard() {
        let store = temp_store("multi-user");
        let admin = store.create_user("boss", "Asmoday#2026", "admin").unwrap();
        assert!(admin.is_admin());
        assert!(admin.must_change_password, "建号即待改密");

        let member = store.create_user("member", "Asmoday#2026", "user").unwrap();
        assert!(!member.is_admin());

        // 清掉待改密标志（模拟用户已完成首登改密）
        store.update_password(admin.id, "Asmoday#2026").unwrap();

        // 最后一个 admin 不能被降级 / 停用 / 删除
        assert!(store.set_user_role(admin.id, "user").is_err());
        assert!(store.set_user_active(admin.id, false).is_err());
        assert!(store.delete_user(admin.id, admin.id).is_err());

        // 普通 admin 存在时，另一名 admin 可以被降级
        let second = store.create_user("boss2", "Asmoday#2026", "admin").unwrap();
        store.update_password(second.id, "Asmoday#2026").unwrap();
        store.set_user_role(second.id, "user").unwrap();
        assert!(!store.user_by_id(second.id).unwrap().unwrap().is_admin());

        // 删除用户级联清理归属数据
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO favorites (user_id, mount_id, file_path, created_at)
             VALUES (?1, 'm', '/m/x.mp4', 1)",
            params![member.id],
        )
        .unwrap();
        drop(conn);
        store.delete_user(member.id, admin.id).unwrap();
        assert!(store.user_by_id(member.id).unwrap().is_none());
        let conn = store.connect().unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM favorites WHERE user_id = ?1",
                params![member.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "删除用户应级联清掉收藏");
    }

    #[test]
    fn rename_rejects_duplicates_and_bad_names() {
        let store = temp_store("rename");
        let admin = store.create_user("boss", "Asmoday#2026", "admin").unwrap();
        store.create_user("member", "Asmoday#2026", "user").unwrap();

        // 重名拒绝
        assert!(store.rename_user(admin.id, "member").is_err());
        // 非法字符（路径分隔符）拒绝
        assert!(store.rename_user(admin.id, "a/b").is_err());
        assert!(store.rename_user(admin.id, "x").is_err());
        // 正常改名生效
        store.rename_user(admin.id, "chief").unwrap();
        assert_eq!(
            store.user_by_id(admin.id).unwrap().unwrap().username,
            "chief"
        );
    }

    #[test]
    fn disabled_users_are_excluded_from_login_lookup_guards() {
        let store = temp_store("disabled");
        let admin = store.create_user("boss", "Asmoday#2026", "admin").unwrap();
        let member = store.create_user("member", "Asmoday#2026", "user").unwrap();
        store.set_user_active(member.id, false).unwrap();

        let found = store.find_user("member").unwrap().unwrap();
        assert!(
            !found.0.is_active,
            "find_user 仍能找到行（审计需要），但 is_active=false"
        );
        assert!(
            store
                .user_by_id(member.id)
                .unwrap()
                .unwrap()
                .must_change_password
        );

        // 重新启用后恢复
        store.set_user_active(member.id, true).unwrap();
        assert!(store.user_by_id(member.id).unwrap().unwrap().is_active);
        // admin 未被误伤
        assert!(store.user_by_id(admin.id).unwrap().unwrap().is_active);
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
    fn meta_path_prefix_does_not_treat_underscore_as_wildcard() {
        // `_` 在 SQL LIKE 里是「任意单字符」：删 `IMG_1234.jpg` 不能顺手把
        // 同目录 `IMG-1234.jpg` 的收藏/分享一起清掉（v4.9.22 回归）
        let store = temp_store("meta-prefix");
        let conn = store.connect().unwrap();
        for path in [
            "/m/IMG_1234.jpg",
            "/m/IMG-1234.jpg",
            "/m/IMG_1234.jpg/inside.mp4",
        ] {
            conn.execute(
                "INSERT INTO favorites (mount_id, file_path, created_at) VALUES ('', ?1, 1)",
                [path],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO shares (token, file_path, is_folder, created_at) VALUES ('t1', '/m/IMG-1234.jpg', 0, 1)",
            [],
        )
        .unwrap();
        drop(conn);

        store.forget_meta_paths("/m/IMG_1234.jpg", 100).unwrap();

        let conn = store.connect().unwrap();
        let left: Vec<String> = conn
            .prepare("SELECT file_path FROM favorites ORDER BY file_path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(left, vec!["/m/IMG-1234.jpg".to_string()], "只该删掉自己");
        let alive: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM shares WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alive, 1, "无关分享不能被置 tombstone");
    }

    #[test]
    fn backup_writes_a_usable_copy_and_never_overwrites() {
        let store = temp_store("backup");
        store.create_user("admin", "Asmoday#2026", "admin").unwrap();

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

        let user = store.create_user("admin", "Asmoday#2026", "admin").unwrap();
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
        assert!(store.create_user("a", "Asmoday#2026", "user").is_err()); // 用户名太短
        assert!(store.create_user("admin", "short", "user").is_err());
        assert!(store.create_user("admin", "12345678", "user").is_err());
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
        let user = store
            .create_user("vfy_recovery", "Abcd1234", "user")
            .unwrap();
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
        let user = store.create_user("vfy_slide", "Abcd1234", "user").unwrap();
        let token = store.create_session(user.id).unwrap();
        let now = now_secs();

        // 29 天前创建、60 秒后过期：应被续期，但顶上绝对上限（created_at + 30 天）
        store
            .connect()
            .unwrap()
            .execute(
                "UPDATE sessions SET created_at = ?1, expires_at = ?2 WHERE token = ?3",
                params![now - 29 * 86_400, now + 60, session_digest(&token)],
            )
            .unwrap();
        assert!(store.session_user(&token).unwrap().is_some());
        let (created, expires): (i64, i64) = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT created_at, expires_at FROM sessions WHERE token = ?1",
                params![session_digest(&token)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(expires > now + 3600, "临近过期必须滑动续期");
        assert!(
            expires <= created + SESSION_ABSOLUTE_TTL,
            "不得越过 30 天绝对上限"
        );
    }

    #[test]
    fn user_perms_defaults_scope_and_update() {
        let store = temp_store("user-perms");

        // 新建用户：全量位 + '*' scope
        let user = store.create_user("perm_u1", "Abcd1234", "user").unwrap();
        assert_eq!(user.permissions, perms::ALL);
        assert!(user.mounts_all());
        assert!(user.can(perms::WRITE) && user.can(perms::TERMINAL));

        // scope 解析：JSON 数组 → 归一化小写列表；'*' → None
        store
            .set_user_perms(
                user.id,
                perms::SEE_HIDDEN | perms::WRITE,
                r#"["D:/Media","/docs"]"#,
            )
            .unwrap();
        let user = store.user_by_id(user.id).unwrap().unwrap();
        assert_eq!(user.allowed_mounts().unwrap(), vec!["d:/media", "/docs"]);
        assert!(user.mount_allowed("d:/MEDIA"));
        assert!(user.mount_allowed("D:/Media/sub/dir"));
        assert!(!user.mount_allowed("E:/other"));
        assert!(user.can(perms::WRITE) && !user.can(perms::DELETE));
        assert!(!user.mounts_all());

        // admin 不看位，恒全放行
        let admin = store
            .create_user("perm_admin", "Abcd1234", "admin")
            .unwrap();
        store.set_user_perms(admin.id, 0, "[]").unwrap();
        let admin = store.user_by_id(admin.id).unwrap().unwrap();
        assert!(admin.can(perms::DELETE) && admin.mount_allowed("anything"));

        // 非法 scope 拒绝；负数位掩码钳到 0
        assert!(store.set_user_perms(user.id, 1, "not-json").is_err());
        assert!(store.set_user_perms(user.id, 1, r#"[""]"#).is_err());
        store.set_user_perms(user.id, -1, "*").unwrap();
        assert_eq!(store.user_by_id(user.id).unwrap().unwrap().permissions, 0);
    }

    #[test]
    fn multi_user_turns_on_with_second_account() {
        let store = temp_store("multi-user-mode");
        // 空库不算多用户；只有一个管理员也不算
        assert!(!store.multi_user().unwrap());
        let admin = store.create_user("solo", "Abcd1234", "admin").unwrap();
        assert!(!store.multi_user().unwrap());

        // 建第二个账号才进多用户（哪怕它只是个普通用户）
        let second = store.create_user("second", "Abcd1234", "user").unwrap();
        assert!(store.multi_user().unwrap());

        // 停用不等于删除：权限配置还在，界面不该退回单用户
        store.set_user_active(second.id, false).unwrap();
        assert!(store.multi_user().unwrap());

        // 删掉后回到单用户
        store.delete_user(second.id, admin.id).unwrap();
        assert!(!store.multi_user().unwrap());
    }
}
