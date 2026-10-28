//! 配置系统：TOML 强类型解析、默认配置生成、校验与落盘。
//!
//! 红线约束：
//! - 格式固定为 TOML，不提供 YAML 入口；
//! - 不存在“关闭认证”的配置开关，且禁止把本地登录与代理信任同时关掉。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// 默认配置文件名（相对进程工作目录）。
pub const CONFIG_FILENAME: &str = "config.toml";
/// 默认监听端口。
pub const DEFAULT_PORT: u16 = 4242;
/// 环境变量覆盖配置路径（设计文档 5.3）。
pub const ENV_CONFIG_PATH: &str = "ASMODAY_CONFIG_PATH";

/// 首次运行时落盘的默认配置：全部带注释，端口 4242，无挂载点、无外部服务。
pub const DEFAULT_CONFIG_TOML: &str = r#"# Asmoday 配置文件 — TOML 格式
# 本文件由程序首次启动时自动生成；也可在“系统设置”页面通过可视化表单或源码模式修改。

[server]
# 监听地址。默认为 "0.0.0.0"（方便局域网访问及 Docker 映射）。
# 若仅本机使用或已前置 Nginx/Caddy 反代，建议改为 "127.0.0.1"。
# 命令行 -H / 环境变量 ASMODAY_HOST 优先级更高。
host = "0.0.0.0"
# 监听端口（1-65535）。命令行 -p / 环境变量 ASMODAY_PORT 优先级更高。
port = 4242

[server.ssl]
enabled = false
cert_path = ""
key_path = ""

# 反向代理兼容性
[server.proxy]
# 是否信任 X-Forwarded-For / X-Real-IP / X-Forwarded-Proto / X-Forwarded-Host
trust_proxy_headers = true
# 仅信任来自这些地址的转发头（支持 CIDR 写法，按字符串匹配）
trusted_ips = ["127.0.0.1", "172.16.0.0/12"]
# 子路径部署前缀，如挂载在 /asmoday 则填 "/asmoday"，根路径留空
base_url = ""

[auth]
# 本地账号密码登录开关。
# 安全约束：local_enabled 与 proxy_trust.enabled 不允许同时为 false，
# 配置里不存在“关闭认证/免密”开关，忘记密码只能走 CLI 重置。
local_enabled = true

# “座上宾”模式：信任反向代理传入的用户名 Header，实现外部 IdP 免密跳转
[auth.proxy_trust]
enabled = false
user_header = "Remote-User"

# 分享 / 图床防盗链：referer_allowlist 非空时，公开直链请求若携带
# Referer 且其主机不在白名单内，一律 403（无 Referer 的直接打开不受影响）
# [shares]
# referer_allowlist = ["example.com", "cdn.example.org"]

# 媒体缓存策略（设计文档 3.4：缩略图 LRU 淘汰，防磁盘被撑爆）
[media]
# 缩略图缓存总容量上限（MiB）
thumb_cache_max_size_mb = 1024
# 缩略图最长保留天数（源文件 mtime 更新会立即失效重建）
thumb_cache_max_age_days = 90
# 外部 FFmpeg 路径（可选）。仅用于给影视库截取首帧封面，绝不参与转码。
# 可填可执行文件，也可直接填安装目录（如 C:/ffmpeg/bin）。
# 留空时依次读取环境变量 ASMODAY_FFMPEG_PATH，再自动探测系统 PATH。
ffmpeg_path = ""
# 单文件上传上限（MiB）。0 = 不限制，仅受磁盘空间约束。
upload_max_size_mb = 0

# 日志策略（设计文档 3.7）：默认生产级克制，排障时分级开启，结构化可选。
[log]
# 控制台最低级别：debug | info | warn | error | off。默认 info——终端里只出现关键信息。
console_level = "info"
# 主文件最低级别（data/logs/asmoday.log，10MiB × 5 轮转）。默认 debug，事后排障有完整线索。
file_level = "debug"
# HTTP 访问日志独立级别（方法/路径/状态码/耗时，不含请求体；off 关闭）。默认 info。
api_level = "info"
# 输出格式：text（人读，默认）| json（每行一个 JSON 对象，便于机器解析）。
format = "text"

# ==========================================
# 以下为可选配置，默认注释。需要时取消注释即可。
# ==========================================

# 挂载点（可多个）
# [[system.mounts]]
# name = "Local Storage"
# path = "D:/Media"
# readonly = false

# 外部聚合服务（可多个），由内置反向代理网关透传
# [[services]]
# id = "grafana"
# name = "监控面板"
# icon = "chart"
# type = "external"
# target_url = "http://192.168.1.50:3000"
# strip_prefix = true
# rewrite_html = false
"#;

// ---------------------------------------------------------------------------
// 类型定义
// ---------------------------------------------------------------------------

/// 根配置结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub system: SystemConfig,
    pub services: Vec<ServiceConfig>,
    /// 分享 / 图床防盗链（设计文档 3.1）
    pub shares: SharesConfig,
    /// 媒体缓存策略（设计文档 3.4 / 4.2）
    pub media: MediaConfig,
    /// 日志分级与多输出（设计文档 3.7）
    pub log: LogConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            system: SystemConfig::default(),
            services: Vec::new(),
            shares: SharesConfig::default(),
            media: MediaConfig::default(),
            log: LogConfig::default(),
        }
    }
}

/// 危险配置自检结果（设置页警告条 / 启动日志共用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SafetyIssue {
    /// `warn` 或 `error`
    pub level: &'static str,
    /// 前端 i18n key：`settings.safety.<code>`
    pub code: &'static str,
}

/// `[media]`：缩略图缓存的容量与过期策略 + 外部 FFmpeg 位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// 缓存总容量上限（MiB）
    pub thumb_cache_max_size_mb: u64,
    /// 缩略图最长保留天数
    pub thumb_cache_max_age_days: u64,
    /// 外部 FFmpeg 可执行文件路径（设计文档 3.5 优先级 1）。
    /// 留空则依次读取 `ASMODAY_FFMPEG_PATH` 与系统 `PATH`。
    pub ffmpeg_path: String,
    /// 单文件上传上限（MiB）。0 = 不限制，仅受磁盘空间约束（设计文档 3.2.3）
    pub upload_max_size_mb: u64,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            thumb_cache_max_size_mb: 1024,
            thumb_cache_max_age_days: 90,
            ffmpeg_path: String::new(),
            upload_max_size_mb: 0,
        }
    }
}

impl MediaConfig {
    /// 换算成字节，供淘汰逻辑使用
    pub fn limit_bytes(&self) -> u64 {
        self.thumb_cache_max_size_mb.saturating_mul(1024 * 1024)
    }

    /// 换算成秒，供过期逻辑使用
    pub fn max_age_secs(&self) -> u64 {
        self.thumb_cache_max_age_days.saturating_mul(24 * 60 * 60)
    }
}

/// `[log]`：日志分级与多输出策略（设计文档 3.7）。
///
/// 策略一句话：**默认生产级克制，排障时分级开启，结构化可选**——
/// 控制台默认只出 INFO+，文件默认记 DEBUG，API 访问日志走独立级别闸门，
/// 最详尽的调试信息（源码位置）只有 DEBUG 级别才附带。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogConfig {
    /// 控制台最低级别：debug | info | warn | error | off（默认 info）
    pub console_level: String,
    /// 主文件最低级别（data/logs/asmoday.log，默认 debug）
    pub file_level: String,
    /// HTTP 访问日志独立级别（方法/路径/状态码/耗时，不含请求体；off 关闭；默认 info）
    pub api_level: String,
    /// 输出格式：text（人读，默认）| json（机器解析，每行一个对象）
    pub format: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            console_level: "info".to_string(),
            file_level: "debug".to_string(),
            api_level: "info".to_string(),
            format: "text".to_string(),
        }
    }
}

/// `[shares]`：公开分享直链的防盗链白名单（主机名后缀匹配）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SharesConfig {
    /// 空 = 不校验；非空时带 Referer 的请求其主机必须命中白名单
    pub referer_allowlist: Vec<String>,
}

/// `[server]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// 监听地址（IPv4/IPv6 字面量）。默认 "0.0.0.0"（设计文档 v2.7.0 3.1.1）
    pub host: String,
    pub port: u16,
    pub ssl: SslConfig,
    pub proxy: ProxyConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: DEFAULT_PORT,
            ssl: SslConfig::default(),
            proxy: ProxyConfig::default(),
        }
    }
}

/// `[server.ssl]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SslConfig {
    pub enabled: bool,
    pub cert_path: String,
    pub key_path: String,
}

/// `[server.proxy]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub trust_proxy_headers: bool,
    pub trusted_ips: Vec<String>,
    pub base_url: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            trust_proxy_headers: true,
            trusted_ips: vec!["127.0.0.1".to_string(), "172.16.0.0/12".to_string()],
            base_url: String::new(),
        }
    }
}

/// `[auth]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub local_enabled: bool,
    pub proxy_trust: ProxyTrustConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            local_enabled: true,
            proxy_trust: ProxyTrustConfig::default(),
        }
    }
}

/// `[auth.proxy_trust]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyTrustConfig {
    pub enabled: bool,
    pub user_header: String,
    /// 只有来自这些地址的请求才采信 `user_header`（设计文档 3.1.8）
    /// 留空 = 拒绝启用 proxy_trust（启动时打 ERROR 并强制关闭）
    pub trusted_ips: Vec<String>,
    /// 是否跳过 2FA：默认仍要求 2FA，开启后启动日志必须打 WARN
    pub skip_2fa: bool,
}

impl Default for ProxyTrustConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            user_header: "Remote-User".to_string(),
            trusted_ips: vec!["127.0.0.1".to_string()],
            skip_2fa: false,
        }
    }
}

/// `[system]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemConfig {
    pub mounts: Vec<MountConfig>,
}

/// `[[system.mounts]]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    pub name: String,
    pub path: String,
    /// 只读挂载点：后端拦截一切写操作，前端置灰全部变更入口（设计文档 3.2.6）
    #[serde(default)]
    pub readonly: bool,
}

/// `[[services]]`：外部聚合服务卡片 + 内置反代目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub id: String,
    pub name: String,
    /// 前端内联 SVG 图标名（不联网取图）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// 服务类型，目前仅 `external`（反代透传）。
    #[serde(rename = "type")]
    pub kind: String,
    pub target_url: String,
    #[serde(default = "default_true")]
    pub strip_prefix: bool,
    #[serde(default)]
    pub rewrite_html: bool,
    /// 是否忽略目标站点证书错误（自签名内网服务）。
    #[serde(default)]
    pub insecure_skip_verify: bool,
    /// 反代时注入的请求头，如 { "X-WEBAUTH-USER" = "admin" }
    #[serde(default)]
    pub inject_headers: BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// 加载 / 保存 / 校验
// ---------------------------------------------------------------------------

/// 配置路径解析：环境变量优先，否则落到工作目录下的 config.toml。
pub fn resolve_path() -> PathBuf {
    match std::env::var(ENV_CONFIG_PATH) {
        Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw),
        _ => PathBuf::from(CONFIG_FILENAME),
    }
}

/// 读取配置；文件不存在时先写入带注释的默认配置，再解析。
///
/// 返回 `(config, created)`，`created` 表示本次是否新建了默认配置文件。
pub async fn load_or_create(path: &Path) -> Result<(Config, bool), String> {
    let mut created = false;

    if !path.exists() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("创建配置目录 {} 失败: {e}", parent.display()))?;
            }
        }
        tokio::fs::write(path, DEFAULT_CONFIG_TOML)
            .await
            .map_err(|e| format!("写入默认配置 {} 失败: {e}", path.display()))?;
        created = true;
    }

    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("读取配置 {} 失败: {e}", path.display()))?;

    let config: Config = toml::from_str(&raw)
        .map_err(|e| format!("解析配置 {} 失败: {e}", path.display()))?;

    config.validate()?;
    Ok((config, created))
}

impl Config {
    /// 序列化为 TOML 并原子写入（先写临时文件再 rename）。
    pub async fn save_to(&self, path: &Path) -> Result<(), String> {
        self.validate()?;

        let body = toml::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("创建配置目录 {} 失败: {e}", parent.display()))?;
            }
        }

        let tmp = path.with_extension("toml.tmp");
        tokio::fs::write(&tmp, body.as_bytes())
            .await
            .map_err(|e| format!("写入临时配置 {} 失败: {e}", tmp.display()))?;
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| format!("替换配置 {} 失败: {e}", path.display()))?;

        Ok(())
    }

    /// 结构与安全校验。任何一条不满足都拒绝落盘。
    pub fn validate(&self) -> Result<(), String> {
        if self.server.port == 0 {
            return Err("server.port 必须在 1-65535 之间".to_string());
        }
        if self.server.host.parse::<std::net::IpAddr>().is_err() {
            return Err(format!(
                "server.host 必须是合法 IP 地址（如 0.0.0.0 / 127.0.0.1 / ::），当前为 {:?}",
                self.server.host
            ));
        }

        // 红线：不允许把认证整体关掉（配置里没有免密开关，这里再兜一层）
        if !self.auth.local_enabled && !self.auth.proxy_trust.enabled {
            return Err(
                "禁止关闭全部认证：auth.local_enabled 与 auth.proxy_trust.enabled 不能同时为 false"
                    .to_string(),
            );
        }

        if self.auth.proxy_trust.enabled && self.auth.proxy_trust.user_header.trim().is_empty() {
            return Err("auth.proxy_trust.user_header 不能为空".to_string());
        }

        let base_url = self.server.proxy.base_url.trim();
        if !base_url.is_empty() && (!base_url.starts_with('/') || base_url.ends_with('/')) {
            return Err("server.proxy.base_url 必须以 / 开头且不以 / 结尾，例如 \"/asmoday\"".to_string());
        }
        if !base_url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'))
        {
            return Err("server.proxy.base_url 只能包含字母、数字、- 和 _（避免注入到 HTML 时出错）".to_string());
        }

        // 媒体缓存：0 会让淘汰逻辑「永远删到空」或「缓存永不过期」，都不合理
        if self.media.thumb_cache_max_size_mb < 16 {
            return Err("media.thumb_cache_max_size_mb 不能小于 16（MiB）".to_string());
        }
        if self.media.thumb_cache_max_age_days == 0 {
            return Err("media.thumb_cache_max_age_days 必须大于 0".to_string());
        }

        // 日志：级别与格式必须是可识别的枚举值（设计文档 3.7）
        for (field, raw) in [
            ("log.console_level", &self.log.console_level),
            ("log.file_level", &self.log.file_level),
            ("log.api_level", &self.log.api_level),
        ] {
            if crate::core::logging::Level::parse(raw).is_none() {
                return Err(format!(
                    "{field} 只能是 debug / info / warn / error / off，当前为 {raw:?}"
                ));
            }
        }
        if !matches!(self.log.format.as_str(), "text" | "json") {
            return Err(format!(
                "log.format 只能是 text 或 json，当前为 {:?}",
                self.log.format
            ));
        }

        // FFmpeg 路径允许留空（走环境变量 / PATH）；填目录也可以——
        // 解压版安装包给出的就是 `...\ffmpeg\bin`，运行时会自动补上可执行文件名。
        // 这里只做「不是空串」之外的宽松校验，真正的可用性由探测接口判断。

        let mut names = HashSet::new();
        for mount in &self.system.mounts {
            if mount.name.trim().is_empty() {
                return Err("system.mounts[].name 不能为空".to_string());
            }
            if mount.path.trim().is_empty() {
                return Err(format!("system.mounts[{}].path 不能为空", mount.name));
            }
            if !names.insert(mount.name.clone()) {
                return Err(format!("system.mounts[].name 重复: {}", mount.name));
            }
        }

        let mut ids = HashSet::new();
        for service in &self.services {
            let id = service.id.trim();
            if id.is_empty() {
                return Err("services[].id 不能为空".to_string());
            }
            if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(format!("services[{id}].id 只能包含字母、数字、- 和 _"));
            }
            if !ids.insert(id.to_string()) {
                return Err(format!("services[].id 重复: {id}"));
            }
            if service.name.trim().is_empty() {
                return Err(format!("services[{id}].name 不能为空"));
            }
            let target = service.target_url.trim();
            if !(target.starts_with("http://") || target.starts_with("https://")) {
                return Err(format!("services[{id}].target_url 必须以 http:// 或 https:// 开头"));
            }
            if service.kind != "external" {
                return Err(format!("services[{id}].type 目前仅支持 \"external\""));
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // 危险配置自检（设计文档 3.5 / 3.9 / 4.2）
    //
    // 只打日志 + 在设置页展示警告条，**不阻止启动**；唯一例外是
    // `proxy_trust.enabled=true` 但白名单为空 —— 那等于任何人都能伪造 Header
    // 冒充管理员，必须强制关掉（见 `apply_safety`）。
    // -----------------------------------------------------------------------

    /// 自检结果（`code` 供前端 i18n 翻译，`level` 决定颜色）
    pub fn safety_issues(&self) -> Vec<SafetyIssue> {
        let mut issues = Vec::new();
        let proxy = &self.server.proxy;
        let trust = &self.auth.proxy_trust;

        if trust.enabled && trust.trusted_ips.iter().all(|ip| ip.trim().is_empty()) {
            issues.push(SafetyIssue {
                level: "error",
                code: "PROXY_TRUST_NO_TRUSTED_IPS",
            });
        }
        if proxy.trust_proxy_headers && proxy.trusted_ips.iter().all(|ip| ip.trim().is_empty()) {
            issues.push(SafetyIssue {
                level: "warn",
                code: "PROXY_HEADERS_NO_TRUSTED_IPS",
            });
        }
        if trust.skip_2fa {
            issues.push(SafetyIssue {
                level: "warn",
                code: "PROXY_TRUST_SKIP_2FA",
            });
        }
        if self.server.host == "0.0.0.0" && !self.server.ssl.enabled {
            issues.push(SafetyIssue {
                level: "warn",
                code: "HOST_ALL_INTERFACES_NO_SSL",
            });
        }
        let base = proxy.base_url.trim();
        if !self.server.ssl.enabled && base.starts_with("https://") {
            issues.push(SafetyIssue {
                level: "warn",
                code: "SSL_DISABLED_WITH_HTTPS_BASE",
            });
        }
        if std::env::var("ASMODAY_RESET_PASSWORD").is_ok_and(|value| !value.trim().is_empty()) {
            issues.push(SafetyIssue {
                level: "warn",
                code: "RESET_PASSWORD_ENV_PRESENT",
            });
        }
        issues
    }

    /// 把自检里「必须强制修正」的项落到配置上，并逐条打日志。
    ///
    /// 返回自检结果，供设置页展示。
    pub fn apply_safety(&mut self) -> Vec<SafetyIssue> {
        let issues = self.safety_issues();

        if self.auth.proxy_trust.enabled
            && self
                .auth
                .proxy_trust
                .trusted_ips
                .iter()
                .all(|ip| ip.trim().is_empty())
        {
            self.auth.proxy_trust.enabled = false;
        }

        for issue in &issues {
            let message = match issue.code {
                "PROXY_TRUST_NO_TRUSTED_IPS" => {
                    "auth.proxy_trust.enabled ignored: trusted_ips is empty (config refused)"
                }
                "PROXY_HEADERS_NO_TRUSTED_IPS" => {
                    "server.proxy.trust_proxy_headers is on but trusted_ips is empty: all clients share the proxy IP"
                }
                "PROXY_TRUST_SKIP_2FA" => {
                    "auth.proxy_trust.skip_2fa is enabled: 2FA is bypassed for SSO logins"
                }
                "SSL_DISABLED_WITH_HTTPS_BASE" => {
                    "server.ssl.enabled=false but base_url looks like an https URL"
                }
                "HOST_ALL_INTERFACES_NO_SSL" => {
                    "listening on 0.0.0.0 without SSL. If you plan to expose this to the internet, please use a reverse proxy like Caddy/Nginx with HTTPS."
                }
                "RESET_PASSWORD_ENV_PRESENT" => {
                    "ASMODAY_RESET_PASSWORD is set: it takes effect on every start, remove it after resetting"
                }
                other => other,
            };
            if issue.level == "error" {
                crate::log_error!("config safety check: {message}");
            } else {
                crate::log_warn!("config safety check: {message}");
            }
        }
        issues
    }
}

// ---------------------------------------------------------------------------
// 热重载（设计文档 3.5：保存配置后无需重启进程）
// ---------------------------------------------------------------------------

/// 后台线程监听 config.toml 变更并热重载内存配置。
///
/// - 监听父目录（编辑器常用「临时文件 + rename」整体替换，监听文件本身会丢事件）；
/// - 300ms 防抖，等编辑器写完再读；
/// - 解析/校验失败时保留旧配置并在日志里说明；
/// - 端口变更仍需重启进程（listener 不会 rebind）。
pub fn spawn_watcher(state: crate::AppState) {
    let target = state.config_path.clone();
    let Some(dir) = target.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let Some(file_name) = target.file_name().map(|n| n.to_os_string()) else {
        return;
    };

    let spawned = std::thread::Builder::new()
        .name("config-watcher".to_string())
        .spawn(move || {
            use notify::Watcher as _;

            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(watcher) => watcher,
                Err(err) => {
                    crate::log_warn!("config watcher unavailable, hot reload disabled: {err}");
                    return;
                }
            };
            if let Err(err) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
                crate::log_warn!("failed to watch config file, hot reload disabled: {err}");
                return;
            }
            crate::log_info!("watching config file for changes: {}", target.display());

            loop {
                let event = match rx.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                };
                let Ok(event) = event else { continue };
                if !event.paths.iter().any(|p| p.file_name() == Some(file_name.as_os_str())) {
                    continue;
                }
                // 防抖：吞掉防抖窗口内连发的后续事件
                while let Ok(more) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
                    if more.is_err() {
                        return;
                    }
                }
                reload_into(&state, &target);
            }
        });

    if spawned.is_err() {
        crate::log_warn!("failed to spawn config watcher thread, hot reload disabled");
    }
}

fn reload_into(state: &crate::AppState, path: &Path) {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("读取失败：{e}"))
        .and_then(|raw| toml::from_str::<Config>(&raw).map_err(|e| format!("解析失败：{e}")));

    match parsed {
        Ok(config) => match config.validate() {
            Ok(()) => {
                // 缩略图策略随配置一起生效
                state
                    .thumbnails
                    .apply_limits(config.media.limit_bytes(), config.media.max_age_secs());
                // 日志分级同样随配置热更新（设计文档 3.7：排障时改配置即生效，无需重启）
                crate::core::logging::apply(&config.log);
                let mut config = config;
                // 热重载同样要过一遍危险配置自检，不许绕过启动时的拦截
                let _ = config.apply_safety();
                *state.config.blocking_write() = config;
                crate::log_info!("config hot-reloaded from {}", path.display());
            }
            Err(err) => {
                crate::log_warn!("config validation failed, keeping previous config: {err}")
            }
        },
        Err(err) => {
            crate::log_warn!("config hot reload skipped ({err}), keeping previous config")
        }
    }
}
