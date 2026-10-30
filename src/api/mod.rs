//! 路由层。

pub mod access;
pub mod auth;
pub mod compress;
pub mod config;
pub mod dav_fs;
pub mod error;
pub mod files;
pub mod files_mutate;
pub mod media;
pub mod prefix;
pub mod proxy;
pub mod security;
pub mod shares;
pub mod static_files;
pub mod system;
pub mod terminal;
pub mod tokens;
pub mod webdav;

/// 宽松布尔查询参数：接受 `1/0`、`true/false`、`yes/no`、`on/off`。
///
/// 用 `Option<bool>` 直接反序列化时，`?refresh=1` 会让**整条查询串**解析失败，
/// 表现为 400 —— 调用方完全看不出是哪个参数的问题（terminal 的 WebSocket 握手
/// 曾因此只报「连接已断开」）。所以凡是查询串里的布尔值一律走这里。
pub(crate) fn boolish<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.map(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }))
}
