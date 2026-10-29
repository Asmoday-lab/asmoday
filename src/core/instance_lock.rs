//! 单实例锁：同一个数据目录只允许一个 Asmoday 进程。
//!
//! 为什么必须有：内存里的 per-path 锁、`.asmoday-part` 启动清理、SQLite 写入
//! 都建立在「只有一个进程」的假设上。systemd 与 docker 同时拉起（或用户
//! 手滑开第二个）时，后启动的实例会把前一个**正在写**的临时文件当垃圾清掉，
//! 两个进程还会互相覆盖配置。
//!
//! **为什么用标准库的文件锁而不是自己拿 SQLite 排他锁**：这条路上踩过一次真事故。
//! 早先用「独立锁库 + `locking_mode=EXCLUSIVE` + `BEGIN EXCLUSIVE`」，结果在
//! 回滚日志模式下 `BEGIN EXCLUSIVE` 只先拿 RESERVED，要真写盘才升级为
//! EXCLUSIVE，而 RESERVED 不挡别人 —— 第二个实例照样启动成功（实测两个
//! 进程同时在跑）。改成「真的写一次并提交」才勉强对，但那本质上是把数据库
//! 的锁语义当文件锁用，任何一次 `PRAGMA` 调整或 SQLite 版本变化都可能再破。
//!
//! 文件锁本来就是操作系统原语，**`std::fs::File::try_lock` 从 Rust 1.89 起
//! 已稳定**（Unix 落 `flock`、Windows 落 `LockFileEx`），连第三方依赖都不用。
//! 进程崩溃由内核自动释放，不会留下要手工删的「僵尸锁」。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// 持锁的文件句柄必须活到进程退出：句柄一关，锁就没了。
///
/// 用 `Vec` 而不是单个槽位：同一进程里可能出现多个数据目录（测试尤其如此），
/// 只留第一个的话，后拿到的句柄会在函数返回时被 drop —— 锁当场失效，
/// 「第二个实例被拒」的语义也就不成立了。
static HOLDERS: OnceLock<Mutex<Vec<File>>> = OnceLock::new();

/// 获取数据目录的独占锁。失败时返回可直接打印给用户的原因。
pub fn acquire(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join(".instance.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open instance lock {}: {e}", path.display()))?;

    // 非阻塞：拿不到就说明另一个实例活着，立刻报错比排队等 5 秒更有用
    if let Err(err) = file.try_lock() {
        return match err {
            // 明确被告知「现在拿不到」：这才是「已经有一个实例在跑」
            TryLockError::WouldBlock => Err(contended_message(&path)),
            // 平台把 WouldBlock 包进 io::Error 的情况（老版本/特殊文件系统）
            TryLockError::Error(inner)
                if inner.kind() == std::io::ErrorKind::WouldBlock
                    || inner.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Err(contended_message(&path))
            }
            // 其余是真错误（只读盘、网络盘不支持锁…），原样透出便于排障
            TryLockError::Error(inner) => Err(format!(
                "cannot take instance lock {}: {inner}",
                path.display()
            )),
        };
    }

    // 锁已经拿到，顺手把 pid 写进去：崩溃后只剩这个文件时，能看出上一个实例是谁。
    // 内容不参与加锁语义，写失败也不影响正确性。
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid={}", std::process::id());
    let _ = file.flush();

    // 句柄交给全局保存，活到进程退出；push 失败（毒化）也不影响已持有的锁
    if let Ok(mut holders) = HOLDERS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        holders.push(file);
    }
    Ok(())
}

fn contended_message(path: &Path) -> String {
    format!(
        "another Asmoday instance is already using this data directory ({}). \
         Stop it first, or start with a different --data-dir",
        path.display()
    )
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
        let dir = scratch("busy");
        acquire(&dir).expect("第一个实例必须拿到锁");

        let err = acquire(&dir).expect_err("第二个实例必须被拒绝");
        assert!(
            err.contains("another Asmoday instance"),
            "错误文案要能直接给用户看：{err}"
        );

        // 锁文件里应当留下持锁 pid（排障用）。
        // 注意不能用 read_to_string 校验：Windows 的 LockFileEx 会挡住**其它句柄**的读，
        // 同一个进程里新开句柄读也会报 ERROR_LOCK_VIOLATION —— 那正好反证锁是真的，
        // 所以这里只看文件长度，内容留给排障时人看。
        let len = std::fs::metadata(dir.join(".instance.lock"))
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert!(len > 0, "锁文件应写入持锁 pid（长度 > 0）");
    }

    #[test]
    fn stale_lock_file_is_reused_in_place() {
        // 锁文件是持久文件而非临时文件：进程退出后残留一个旧文件完全正常，
        // 下一次启动必须原地复用，绝不能因为「文件已存在」就报错。
        let dir = scratch("reuse");
        std::fs::write(dir.join(".instance.lock"), b"pid=99999\n").unwrap();

        acquire(&dir).expect("残留的锁文件必须能原地复用");
    }
}
