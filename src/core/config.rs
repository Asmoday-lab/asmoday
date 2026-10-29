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
# IPv6：写 "::" 即双栈监听（同一端口同时收 IPv4 + IPv6 连接），
# 只收 v6 写 "::1"（本机）或具体地址；IPv6 字面量不需要方括号。
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

# 终端 PTY 拉起覆盖（设计文档 3.3.5）。两项都留空/缺省时走自动探测：
# Windows pwsh → powershell；类 Unix $SHELL → zsh → bash → sh。
# [terminal]
# 自定义 shell（绝对路径或 PATH 内可执行名，如 "/usr/bin/zsh"、"pwsh"）
# shell = ""
# 自定义启动参数。**设置后完全替换**内置参数（-l / -NoExit 等都不再加），
# 例如接入 tmux：args = ["-c", "tmux new -A -s asmoday"]。
# 注意：历史记录隔离依赖 shell 自身的 HISTFILE / PSReadLine 配置，
# 走自定义参数时这部分由你自己的配置负责。
# args = []

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
    /// 终端 PTY 拉起覆盖（设计文档 3.3.5）
    pub terminal: TerminalConfig,
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
            terminal: TerminalConfig::default(),
        }
    }
}

/// `[terminal]`：PTY 拉起覆盖（设计文档 3.3.5）。
///
/// 两项都缺省时走自动探测；`shell` 非空则优先于 `$SHELL` 与平台探测；
/// `args` 非空则**完全替换**内置启动参数（`-l` / `-NoExit` 等一律不再追加），
/// 供 `["-c", "tmux new -A -s asmoday"]` 这类自定义接入。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// 覆盖自动探测的 shell（绝对路径或 PATH 内可执行名；空 = 自动探测）
    pub shell: String,
    /// 覆盖默认启动参数（空 = 用内置参数）
    pub args: Vec<String>,
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

// ---------------------------------------------------------------------------
// 保留注释的序列化（红线 10：配置文件是给人看的）
//
// 思路：先把新配置序列化成一份「纯净文档」，再按字段粒度合并进现有文档的
// `toml_edit::DocumentMut` —— 只替换 `Item` 的值，`Key`/`Table` 上挂着的
// 注释（decor）、空行、键序全部原样保留；新增的段追加在末尾。
//
// 中间层为什么也用 toml_edit 而不是 toml::Value：`toml::map::Map` 是
// BTreeMap，过一道就会把 `[[services]]` 里的字段按字母序重排，
// 用户看到的顺序就和自己写的不一样了。
// ---------------------------------------------------------------------------

/// 允许在文档里「整体消失」的路径。
///
/// 只有表数组与纯表能缺席（解析时 `#[serde(default)]` 会补成空），
/// **标量数组不能**：`trusted_ips = []` 与「没有这个键」语义完全不同
/// （后者回到默认白名单，前者是真空白名单）。
const OMITTABLE_PATHS: &[&str] = &["system", "system.mounts", "services", "services.inject_headers"];

fn is_omittable(path: &str) -> bool {
    OMITTABLE_PATHS.contains(&path)
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// 值为空，且该路径允许缺席 → 文档里不该留下 `x = []` / `[x]` 这种噪声。
fn is_omittable_item(path: &str, item: &toml_edit::Item) -> bool {
    if !is_omittable(path) {
        return false;
    }
    is_empty_item(item)
}

/// 这个 Item 在「写不写都一样」的意义上是不是空的。
fn is_empty_item(item: &toml_edit::Item) -> bool {
    match item {
        toml_edit::Item::Value(toml_edit::Value::Array(array)) => array.is_empty(),
        toml_edit::Item::ArrayOfTables(tables) => tables.is_empty(),
        toml_edit::Item::Table(table) => table.is_empty(),
        _ => false,
    }
}

/// 表里每一项都「可省略且为空」→ 整张表不必出现在文档里。
fn table_fully_omittable(path: &str, table: &toml_edit::Table) -> bool {
    !table.is_empty()
        && table
            .iter()
            .all(|(key, item)| is_omittable_item(&join_path(path, key), item))
}

/// 把数组摊成单行（`["a", "b"]`）。
///
/// 序列化产物里数组可能是 `[\n    "a",\n]` 的多行形式，直接 clone 会把用户
/// 原本单行的 `trusted_ips = [...]` 撑成好几行——值没变，格式却被改动了。
fn inline_array(array: &mut toml_edit::Array) {
    array.set_trailing_comma(false);
    array.set_trailing("");
    for (index, value) in array.iter_mut().enumerate() {
        value.decor_mut().set_prefix(if index == 0 { "" } else { " " });
        value.decor_mut().set_suffix("");
    }
}

/// 递归摊平整份文档里的数组（含表内、表数组内）。
fn inline_all(table: &mut toml_edit::Table) {
    use toml_edit::{Item, Value};
    for (_, item) in table.iter_mut() {
        match item {
            Item::Value(Value::Array(array)) => inline_array(array),
            Item::Table(sub) => inline_all(sub),
            Item::ArrayOfTables(tables) => {
                for entry in tables.iter_mut() {
                    inline_all(entry);
                }
            }
            _ => {}
        }
    }
}

/// 把 `source` 合并进 `target`，返回「target 是否已变空」。
fn merge_table(target: &mut toml_edit::Table, source: &toml_edit::Table, prefix: &str) -> bool {
    use toml_edit::Item;

    // 1) 清掉「新配置里没有的键」与「应缺席的空键」。
    //    Config 是 deny_unknown_fields，所以前者只可能是用户从源码模式删掉的段。
    let drop_keys: Vec<String> = target
        .iter()
        .filter(|(key, _)| {
            let path = join_path(prefix, key);
            match source.get(*key) {
                None => true,
                Some(item) => is_omittable_item(&path, item),
            }
        })
        .map(|(key, _)| key.to_string())
        .collect();
    for key in drop_keys {
        target.remove(&key);
    }

    // 2) 逐字段写入：已有表递归下去（保住表内的注释），缺的键补上。
    for (key, source_item) in source.iter() {
        let path = join_path(prefix, key);
        if is_omittable_item(&path, source_item) {
            continue;
        }

        if !target.contains_key(key) {
            // 整张表都是「可省略的空值」（如 `[system]` 下只有一个空 mounts）时
            // 不要写进文档：否则第一次保存留下 `[system]/mounts = []`，
            // 第二次保存又把它当空段删掉，两次结果不一致（不幂等）。
            if let Some(table) = source_item.as_table() {
                if table_fully_omittable(&path, table) {
                    continue;
                }
            }
            target.insert(key, source_item.clone());
            continue;
        }

        // 注意：`get_mut` 的借用必须在下一次借用 `target` 前结束，
        // 所以「写值」与「合并后删空表」分成两步，不要塞进同一个 match 元组。
        match source_item {
            Item::Table(source_sub) => {
                let became_empty = match target.get_mut(key) {
                    Some(Item::Table(sub)) => merge_table(sub, source_sub, &path),
                    Some(slot) => {
                        *slot = source_item.clone();
                        false
                    }
                    None => false,
                };
                if became_empty {
                    target.remove(key);
                }
            }
            Item::ArrayOfTables(source_tables) => {
                if let Some(item) = target.get_mut(key) {
                    merge_table_array(item, source_tables, &path);
                }
            }
            _ => {
                if let Some(slot) = target.get_mut(key) {
                    *slot = source_item.clone();
                }
            }
        }
    }

    // 3) 递归合并后父表空了（例如只剩 `[system]` 而没挂载点），交给调用方删掉整段
    target.is_empty()
}

/// 表数组（`[[system.mounts]]` / `[[services]]`）的就地合并。
///
/// 按索引对齐：已有条目原地改字段（条目上的注释、条目内注释都保留），
/// 多出来的追加，少了的截断 —— 只有「条目数量变少」时才会丢掉被删条目的注释，
/// 那是用户真的删了它，可以接受。
fn merge_table_array(
    item: &mut toml_edit::Item,
    source: &toml_edit::ArrayOfTables,
    path: &str,
) {
    use toml_edit::Item;

    if !matches!(item, Item::ArrayOfTables(_)) {
        // 目标不是表数组（用户手写成了 `services = []`）→ 直接按新值重建
        *item = Item::ArrayOfTables(source.clone());
        return;
    }
    let Some(tables) = item.as_array_of_tables_mut() else {
        return;
    };
    for (index, entry) in source.iter().enumerate() {
        if index < tables.len() {
            if let Some(table) = tables.get_mut(index) {
                merge_table(table, entry, path);
            }
        } else {
            tables.push(entry.clone());
        }
    }
    while tables.len() > source.len() {
        let last = tables.len() - 1;
        tables.remove(last);
    }
}

/// 把配置渲染成文本，**保留 `existing` 里的注释**。
///
/// `existing` 为 `None`（首次保存）或解析失败时，退到带注释的默认模板 /
/// `to_string_pretty`：保存这个动作本身不该失败。
fn render_preserving_comments(existing: Option<&str>, config: &Config) -> Result<String, String> {
    let plain = || toml::to_string_pretty(config).map_err(|e| format!("序列化配置失败: {e}"));

    let fresh = match plain() {
        Ok(text) => text,
        // 序列化失败是真异常（Save 必须失败），不要静默写空文件
        Err(err) => return Err(err),
    };
    let Ok(mut fresh_doc) = fresh.parse::<toml_edit::DocumentMut>() else {
        return plain();
    };
    inline_all(fresh_doc.as_table_mut());
    let Ok(mut doc) = existing.unwrap_or(DEFAULT_CONFIG_TOML).parse::<toml_edit::DocumentMut>()
    else {
        // 现有文件被手改坏了（少括号之类）：退回全量重写，至少配置能保存成功
        return plain();
    };

    merge_table(doc.as_table_mut(), fresh_doc.as_table(), "");
    Ok(doc.to_string())
}

impl Config {
    /// 原子写入配置文件（先写临时文件再 rename），**保留原有注释与键顺序**。
    ///
    /// 实现要点：不是 `toml::to_string_pretty` 全量重写（那会丢掉用户写的每一行注释，
    /// 破坏红线 10「配置文件可手改、注释可读」），而是把现有文件解析成
    /// `toml_edit::DocumentMut`，只在字段粒度上替换“值”，注释/空行/键序原地不动。
    ///
    /// 内容与现有文件一致时**不写盘**（设计文档 3.5）：既省一次 IO，
    /// 也避免自己触发一次无意义的热重载。
    pub async fn save_to(&self, path: &Path) -> Result<(), String> {
        self.validate()?;

        // 现有文件读不到（首次保存 / 权限问题）就用默认模板当骨架，
        // 这样生成出来的文件同样是带注释的版本，而不是一堆裸键值。
        let existing = tokio::fs::read_to_string(path).await.ok();
        let body = render_preserving_comments(existing.as_deref(), self)?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("创建配置目录 {} 失败: {e}", parent.display()))?;
            }
        }

        write_if_changed(path, &body)
            .await
            .map_err(|e| format!("写入配置 {} 失败: {e}", path.display()))?;

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
        // 全接口监听（0.0.0.0 / ::，解析失败已被上面的 validate 拦截）
        let host_unspecified = self
            .server
            .host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_unspecified())
            .unwrap_or(false);
        if host_unspecified && !self.server.ssl.enabled {
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
                    "listening on all interfaces (0.0.0.0/::) without SSL. If you plan to expose this to the internet, please use a reverse proxy like Caddy/Nginx with HTTPS."
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

/// 防抖窗口（设计文档 3.5：500ms）。编辑器一次保存常连发多个事件，
/// 等它彻底写完再读，否则会读到半个文件。
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
/// 相同热重载日志的最小间隔（设计文档 3.5：防刷屏卡死）
const RELOAD_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 自写盘抑制计数：>0 时监听器忽略一切事件。
///
/// 防的是「写盘 → 触发监听 → 重载 → 再写盘」的死循环。用计数而不是 bool，
/// 因为它可能被并发持有（可视化表单与源码模式同时保存）。
static SUSPEND_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 持有期间挂起配置监听器；`Drop` 自动恢复——异常返回也不会把监听器永久挂死。
pub struct WatchSuspend;

impl Drop for WatchSuspend {
    fn drop(&mut self) {
        SUSPEND_COUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 挂起配置监听器，防止自己的写盘动作被当成外部修改。
pub fn suspend_watch() -> WatchSuspend {
    SUSPEND_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    WatchSuspend
}

fn watch_suspended() -> bool {
    SUSPEND_COUNT.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// 最近一次由本进程写入的配置原文。
///
/// 挂起计数器只能盖住「写盘那一刻」，文件事件往往在 rename 之后几十毫秒才送达，
/// 那时挂起已经解除；这份内容比对就是第二道闸：内容与自己刚写的一致 → 纯回声，直接丢弃。
static SELF_WRITE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 只在内容真的变化时写盘（设计文档 3.5「内容没变坚决不写盘」）。
///
/// 返回 `true` 表示确实写了。调用方不用再自己挂起监听器——这里已经处理。
pub async fn write_if_changed(path: &Path, body: &str) -> std::io::Result<bool> {
    if let Ok(existing) = tokio::fs::read_to_string(path).await {
        if existing == body {
            return Ok(false);
        }
    }
    let _guard = suspend_watch();
    let tmp = path.with_extension("toml.tmp");
    tokio::fs::write(&tmp, body.as_bytes()).await?;
    tokio::fs::rename(&tmp, path).await?;
    if let Ok(mut slot) = SELF_WRITE.lock() {
        *slot = Some(body.to_string());
    }
    Ok(true)
}

/// 事件是否代表「配置内容真的变了」。
///
/// 只认 `Create` 与 `Modify`（`Metadata` / `Other` 除外）：
/// - `Access`（文件被读取）在文件管理器里浏览一眼就会触发，放进来会让热重载被无意义地反复触发；
/// - `ModifyKind::Metadata`（改权限、改时间戳）同样不算内容变更；
/// - **`ModifyKind::Any` 必须放行**：Windows 的 notify 后端只报 `Modify(Any)`，
///   按文档字面只收 `Modify(Data)` 会让 Windows 上的热重载彻底失效。
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::event::{EventKind, ModifyKind};
    match kind {
        EventKind::Create(_) => true,
        EventKind::Modify(inner) => !matches!(inner, ModifyKind::Metadata(_) | ModifyKind::Other),
        // 后端无法归类时的兜底：宁可多重载一次，也不要让 Windows 用户改了配置没反应
        EventKind::Any => true,
        _ => false,
    }
}

/// 后台线程监听 config.toml 变更并热重载内存配置。
///
/// - 监听父目录（编辑器常用「临时文件 + rename」整体替换，监听文件本身会丢事件）；
/// - 500ms 防抖；
/// - 事件种类过滤 + 内容比对 + 自写盘挂起，三重防死循环；
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

            let mut last_applied: Option<String> = None;
            let mut last_log: Option<std::time::Instant> = None;

            loop {
                let event = match rx.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                };
                let Ok(event) = event else { continue };
                // 自己写盘引发的回声：直接丢掉，绝不重载
                if watch_suspended() {
                    continue;
                }
                if !event.paths.iter().any(|p| p.file_name() == Some(file_name.as_os_str())) {
                    continue;
                }
                // 排障用：不同平台的 notify 后端给出的事件种类并不一致
                //（Windows 常给 `Modify(Any)`，Linux 给 `Modify(Data(Any))`），
                // 打一条 DEBUG 就能看出为什么没触发重载
                crate::log_debug!("config watcher event: {:?}", event.kind);
                if !is_content_change(&event.kind) {
                    continue;
                }
                // 防抖：吞掉防抖窗口内连发的后续事件
                while let Ok(more) = rx.recv_timeout(RELOAD_DEBOUNCE) {
                    if more.is_err() {
                        return;
                    }
                }
                let raw = match std::fs::read_to_string(&target) {
                    Ok(raw) => raw,
                    Err(err) => {
                        crate::log_warn!(
                            "config hot reload skipped (read failed: {err}), keeping previous config"
                        );
                        continue;
                    }
                };
                // 内容没变（自己刚写过、或编辑器只 touch 了 mtime）就不重载
                if last_applied.as_deref() == Some(raw.as_str()) {
                    continue;
                }
                // 自己刚写的内容：事件迟到了而已，记下来别重载
                let echo = SELF_WRITE
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .map(|written| written == raw)
                    .unwrap_or(false);
                if echo {
                    last_applied = Some(raw);
                    continue;
                }
                if apply_text(&state, &raw) {
                    last_applied = Some(raw);
                    // 日志节流：1 秒内最多一条，避免事件风暴刷屏
                    let due = last_log
                        .map(|t| t.elapsed() >= RELOAD_LOG_INTERVAL)
                        .unwrap_or(true);
                    if due {
                        crate::log_info!("config hot-reloaded from {}", target.display());
                        last_log = Some(std::time::Instant::now());
                    }
                }
            }
        });

    if spawned.is_err() {
        crate::log_warn!("failed to spawn config watcher thread, hot reload disabled");
    }
}

/// 解析并应用一份配置文本。返回 `true` 表示已生效。
fn apply_text(state: &crate::AppState, raw: &str) -> bool {
    let config = match toml::from_str::<Config>(raw) {
        Ok(config) => config,
        Err(err) => {
            crate::log_warn!("config hot reload skipped (parse failed: {err}), keeping previous config");
            return false;
        }
    };
    if let Err(err) = config.validate() {
        crate::log_warn!("config validation failed, keeping previous config: {err}");
        return false;
    }

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
    true
}

// ---------------------------------------------------------------------------
// 测试：注释保留（红线 10）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn comment_lines(text: &str) -> usize {
        text.lines().filter(|line| line.trim_start().starts_with('#')).count()
    }

    /// 文档里是否真有这一行表头（注释掉的示例不算）
    fn has_headers(text: &str, header: &str) -> bool {
        text.lines().any(|line| line.trim() == header)
    }

    fn parsed(text: &str) -> Config {
        toml::from_str(text).expect("渲染结果必须能被自己解析")
    }

    fn rendered(cfg: &Config) -> String {
        render_preserving_comments(Some(DEFAULT_CONFIG_TOML), cfg).unwrap()
    }

    #[test]
    fn rendering_keeps_every_template_comment() {
        let mut config = parsed(DEFAULT_CONFIG_TOML);
        config.server.port = 5000;
        config.server.host = "127.0.0.1".to_string();

        let text = rendered(&config);
        assert!(
            comment_lines(&text) >= comment_lines(DEFAULT_CONFIG_TOML),
            "注释行数不能减少：{} < {}",
            comment_lines(&text),
            comment_lines(DEFAULT_CONFIG_TOML)
        );
        // 关键注释文案必须还在（不能是“只剩几个 #”）
        for needle in [
            "# Asmoday 配置文件",
            "# 监听地址",
            "# 媒体缓存策略",
            "# HTTP 访问日志独立级别",
            "# 以下为可选配置，默认注释",
        ] {
            assert!(text.contains(needle), "注释丢失：{needle}\n---\n{text}");
        }
        assert!(text.contains("port = 5000"), "值没写进去：\n{text}");
        // 数组要摊成单行：序列化产物是多行格式，直接搬会把用户原有的一行撑开
        assert!(
            text.contains(r#"trusted_ips = ["127.0.0.1", "172.16.0.0/12"]"#),
            "数组被拆成多行了：\n{text}"
        );
        assert_eq!(parsed(&text), config);
    }

    #[test]
    fn user_added_comments_and_keys_survive() {
        let source = r#"# 我自己的备注：这台机器给客厅用
[server]
# 端口别用 4242，和 NAS 冲突
port = 4242

[media]
thumb_cache_max_size_mb = 512
"#;
        let mut config = parsed(source);
        config.server.port = 8080;
        config.media.thumb_cache_max_size_mb = 2048;

        let text = render_preserving_comments(Some(source), &config).unwrap();
        assert!(text.contains("# 我自己的备注：这台机器给客厅用"), "{text}");
        assert!(text.contains("# 端口别用 4242，和 NAS 冲突"), "{text}");
        assert!(text.contains("port = 8080"), "{text}");
        assert!(text.contains("thumb_cache_max_size_mb = 2048"), "{text}");
        assert_eq!(parsed(&text), config);
    }

    #[test]
    fn mounts_and_services_are_added_then_removed_cleanly() {
        let mut config = parsed(DEFAULT_CONFIG_TOML);
        config.system.mounts.push(MountConfig {
            name: "Local Storage".to_string(),
            path: "/mnt/nas".to_string(),
            readonly: true,
        });
        config.services.push(ServiceConfig {
            id: "grafana".to_string(),
            name: "监控面板".to_string(),
            icon: Some("chart".to_string()),
            kind: "external".to_string(),
            target_url: "http://192.168.1.50:3000".to_string(),
            strip_prefix: true,
            rewrite_html: false,
            insecure_skip_verify: false,
            inject_headers: BTreeMap::from([("X-WEBAUTH-USER".to_string(), "admin".to_string())]),
        });

        let text = rendered(&config);
        // 只认真正的表头行：模板里的 `# [[system.mounts]]` 是注释示例，contains 会误判
        assert!(has_headers(&text, "[[system.mounts]]"), "{text}");
        assert!(has_headers(&text, "[[services]]"), "{text}");
        assert!(has_headers(&text, "[services.inject_headers]"), "{text}");
        assert!(text.contains("# 以下为可选配置，默认注释"), "模板注释不能丢：\n{text}");
        assert_eq!(parsed(&text), config);

        // 删干净之后，文档里不该留下空段或空数组
        let mut emptied = config.clone();
        emptied.system.mounts.clear();
        emptied.services.clear();
        let text = render_preserving_comments(Some(&text), &emptied).unwrap();
        assert!(!has_headers(&text, "[[system.mounts]]"), "{text}");
        assert!(!has_headers(&text, "[[services]]"), "{text}");
        assert!(!has_headers(&text, "[services.inject_headers]"), "{text}");
        assert!(!has_headers(&text, "[system]"), "空的 [system] 段应当移除：\n{text}");
        assert_eq!(parsed(&text), emptied);
    }

    #[test]
    fn empty_scalar_array_is_not_dropped() {
        // trust_proxy_headers 为 true 但白名单清空 = 所有人都被当成代理，
        // 这个「空」是用户的真实意图，绝不能因为“空就省略”而被默认值顶掉。
        let mut config = parsed(DEFAULT_CONFIG_TOML);
        config.server.proxy.trusted_ips.clear();
        config.auth.proxy_trust.trusted_ips.clear();

        let text = rendered(&config);
        assert!(text.contains("trusted_ips = []"), "{text}");
        assert_eq!(parsed(&text), config);
    }

    #[test]
    fn rendering_is_idempotent() {
        let mut config = parsed(DEFAULT_CONFIG_TOML);
        config.log.format = "json".to_string();
        config.shares.referer_allowlist = vec!["example.com".to_string()];

        let first = rendered(&config);
        let second = render_preserving_comments(Some(&first), &config).unwrap();
        assert_eq!(first, second, "同样的配置渲染两次必须一字不差");
    }

    #[test]
    fn broken_template_falls_back_to_plain_serialization() {
        // 现有文件被手改坏了（例如少个方括号）：保存动作本身不能失败
        let mut config = parsed(DEFAULT_CONFIG_TOML);
        config.server.port = 9000;
        let text = render_preserving_comments(Some("[server\nport = 1"), &config).unwrap();
        assert_eq!(parsed(&text), config);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asmoday-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn save_to_keeps_comments_on_disk() {
        let dir = scratch("comments");
        let path = dir.join(CONFIG_FILENAME);
        std::fs::write(
            &path,
            "# 主机上的手写备注：别删这一行\n[server]\n# 端口备注\nport = 4242\n",
        )
        .unwrap();

        let (mut config, created) = load_or_create(&path).await.unwrap();
        assert!(!created);
        config.server.port = 4321;
        config.media.thumb_cache_max_size_mb = 256;
        config.save_to(&path).await.unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 主机上的手写备注：别删这一行"), "{text}");
        assert!(text.contains("# 端口备注"), "{text}");
        assert!(text.contains("port = 4321"), "{text}");
        assert_eq!(parsed(&text), config);

        // 内容没变就不写盘（避免自触发一次热重载）
        let stable = std::fs::read_to_string(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        config.save_to(&path).await.unwrap();
        let again = std::fs::read_to_string(&path).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(stable, again, "第二次保存不应该改变内容");
        assert_eq!(before, after, "内容未变化的保存不应落盘");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_to_creates_annotated_config_from_scratch() {
        let dir = scratch("fresh");
        let path = dir.join("nested").join(CONFIG_FILENAME);
        let mut config = Config::default();
        config.server.port = 4243;
        config.save_to(&path).await.unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Asmoday 配置文件 — TOML 格式"), "{text}");
        assert!(text.contains("port = 4243"), "{text}");
        assert_eq!(parsed(&text), config);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
