//! 核心域逻辑（配置、文件系统、PTY 会话、缩略图、认证、TOTP、代理、WebDAV 等）。

pub mod archive;
pub mod audio_tags;
pub mod auth;
pub mod bt;
pub mod config;
pub mod cover_cache;
pub mod dir_cache;
pub mod download;
pub mod ffmpeg;
pub mod fs;
pub mod fs_ops;
pub mod instance_lock;
pub mod junk;
pub mod locks;
pub mod logging;
pub mod media_index;
pub mod path_guard;
pub mod prefix;
pub mod reserved;
pub mod search;
pub mod terminal;
pub mod text_codec;
pub mod thumbnail;
pub mod totp;

/// 单测临时目录：`target/<profile>/test-scratch/`（已建好，调用方自己往下拼名字）。
///
/// 两条约束一起满足：
/// - **不落系统临时目录**：这个环境里 `%TEMP%` 与仓库盘的权限/重定向策略不一致，SQLite
///   落库、缩略图改名会在那里偶发权限错误，所以测试数据统一留在仓库盘上；
/// - **不往 `target` 根目录再起一个目录**：`target` 下只留 `debug` / `release` 两个
///   profile 目录，测试残留放进对应的 profile 里面，`cargo clean` 照旧一并带走。
///
/// profile 用 `cfg!(debug_assertions)` 判断：`cargo test --release` 落 `target/release`。
#[cfg(test)]
pub fn test_scratch_dir() -> std::path::PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(profile)
        .join("test-scratch");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
