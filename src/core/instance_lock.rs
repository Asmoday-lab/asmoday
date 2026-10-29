//! 单实例锁：同一个数据目录只允许一个 Asmoday 进程。
//!
//! 为什么必须有：内存里的 per-path 锁、`.asmoday-part` 启动清理、SQLite 写入
//! 都建立在「只有一个进程」的假设上。systemd 与 docker 同时拉起（或用户
//! 手滑开第二个）时，后启动的实例会把前一个**正在写**的临时文件当垃圾清掉，
//! 两个进程还会互相覆盖配置。
//!
//! 为什么用 SQLite 而不是 flock：flock / LockFileEx 是两套 API，还要额外依赖。
//! 项目本来就有 rusqlite，用一个独立的锁库文件 `locking_mode=EXCLUSIVE` 就能
//! 拿到跨平台的进程级排他锁；进程崩溃时由操作系统回收，不会留下需要手工删的
//! 「僵尸锁」。
//!
//! **踩过的坑**：只发 `BEGIN EXCLUSIVE` 是**不够**的。回滚日志（DELETE）模式下
//! `BEGIN EXCLUSIVE` 只先拿 RESERVED，EXCLUSIVE 要到真正写盘时才升级，
//! 而 RESERVED 不挡别人 —— 于是第二个实例照样能启动（实测过）。
//! 正确姿势是「写一次并提交」：`locking_mode=EXCLUSIVE` 会让连接在事务结束后
//! **继续持有**那次写入拿到的 EXCLUSIVE 锁，直到进程退出或 handle 关闭。

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rusqlite::Connection;

/// 持有锁的连接必须活到进程退出：`Connection` drop 时锁才释放。
static HOLDER: OnceLock<Mutex<Connection>> = OnceLock::new();

/// 获取数据目录的独占锁。失败时返回可直接打印给用户的原因。
pub fn acquire(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join(".instance.lock");
    let conn = Connection::open(&path)
        .map_err(|e| format!("cannot open instance lock {}: {e}", path.display()))?;

    // 绝不排队：拿不到锁说明另一个实例活着，立刻报错比等 5 秒更有用
    conn.busy_timeout(Duration::from_millis(0))
        .map_err(|e| lock_error(&path, e))?;
    conn.pragma_update(None, "journal_mode", "DELETE")
        .map_err(|e| lock_error(&path, e))?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
        .map_err(|e| lock_error(&path, e))?;

    // 真的写一次（不是 BEGIN EXCLUSIVE）：只有这样才会升级到文件级 EXCLUSIVE 锁，
    // 并由 locking_mode=EXCLUSIVE 在提交后继续持有。pid 写进去只为排障时能看出
    // 上一个实例是谁（下次启动会被覆盖）。
    let pid = std::process::id() as i64;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_lock (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             pid INTEGER NOT NULL,
             started_at INTEGER NOT NULL
         );",
    )
    .map_err(|e| lock_error(&path, e))?;
    conn.execute(
        "INSERT INTO instance_lock (id, pid, started_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET pid = excluded.pid, started_at = excluded.started_at",
        rusqlite::params![pid, crate::core::auth::now_secs()],
    )
    .map_err(|e| lock_error(&path, e))?;

    // 只有一个线程会走到这里，set 失败（重复调用）也不影响锁本身
    let _ = HOLDER.set(Mutex::new(conn));
    Ok(())
}

fn lock_error(path: &Path, err: rusqlite::Error) -> String {
    let busy = matches!(
        &err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::DatabaseBusy
                || inner.code == rusqlite::ErrorCode::DatabaseLocked
    );
    if busy {
        return format!(
            "another Asmoday instance is already using this data directory ({}). \
             Stop it first, or start with a different --data-dir",
            path.display()
        );
    }
    format!("cannot take instance lock {}: {err}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("asmoday-instance-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_instance_is_rejected() {
        // 同进程开第二个连接模拟第二个实例。SQLite 在同一进程内也按 inode 记账
        // （unixInodeInfo / Windows 按 handle），所以这里能真实复现跨进程的抢锁。
        let dir = scratch("busy");
        acquire(&dir).expect("第一个实例必须拿到锁");

        let err = acquire(&dir).expect_err("第二个实例必须被拒绝");
        assert!(
            err.contains("another Asmoday instance"),
            "错误文案要能直接给用户看：{err}"
        );

        // 锁库里写下持锁 pid（排障用）。此时库被排他锁住，只能从文件层面确认：
        // 空文件 = 没写进去，说明我们退回了「只 BEGIN 不写」的老路。
        let lock_file = dir.join(".instance.lock");
        assert!(
            std::fs::metadata(&lock_file)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "锁库必须真的写入了内容（否则拿不到 EXCLUSIVE）"
        );
    }
}
