//! 子路径部署的前缀解析（设计文档 3.1 / 4.2）。
//!
//! 前缀有两个来源：
//!
//! 1. `[server.proxy].base_url` —— 部署者自己写的，明确意图；
//! 2. 反代下发的 `X-Forwarded-Prefix`（Nginx `proxy_set_header X-Forwarded-Prefix /asmoday;`、
//!    Traefik `StripPrefix` 的配套头、Caddy `handle_path` 之外的写法都会带）。
//!
//! 为什么两个都要：只认配置的话，用户把应用挂到 `/asmoday` 而忘了改配置，
//! 页面能打开但 `assets` 全部 404（index.html 里的绝对路径没加前缀）；
//! 只认头的话，头是反代才有的，直连部署（无代理）就永远拿不到前缀。
//!
//! 优先级：**配置优先**。配置是显式意图，头只是反代的旁证；两者不一致基本意味着
//! 配置写错了，所以打一次 WARN（不是每次请求都打），否则会静默选一个、让人查半天。
//!
//! 采信条件：头只在**可信代理**发来时才算数（`[server.proxy].trusted_ips`，
//! 默认 `127.0.0.1` / `::1` 已覆盖本机反代）。虽然一个假的 `/xx` 前缀并不能越权
//! （直连请求本来就能打到任意路径），但让任意客户端左右站内链接生成没有任何好处。

/// 请求头名（小写，与 axum 的查找一致）
pub const HEADER: &str = "x-forwarded-prefix";

/// 前缀长度上限：再长基本是攻击载荷或误配（真的有人把整条 URL 塞进来）
const MAX_LEN: usize = 128;

/// 解析后的部署前缀。空串 = 根路径部署。
///
/// 通过请求扩展（`Extension<RuntimePrefix>`）下发到处理器：
/// 静态资源注入 `window.__ASMODAY_BASE__`、剥前缀判断、Cookie `Path` 都读它。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimePrefix(String);

impl RuntimePrefix {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self(prefix.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 剥掉前缀；不匹配（或本身就是根路径）时原样返回。
    ///
    /// 只按**段边界**匹配：前缀 `/app` 不能把 `/application` 剥成 `lication`。
    pub fn strip<'a>(&self, path: &'a str) -> &'a str {
        if self.0.is_empty() {
            return path;
        }
        match path.strip_prefix(self.0.as_str()) {
            // 正好等于前缀（`/app`）→ 视作站点根
            Some("") => "/",
            Some(rest) if rest.starts_with('/') => rest,
            _ => path,
        }
    }
}

/// 规范化一个前缀：`/` 开头、无尾斜杠、只允许安全字符；根路径返回 `None`。
///
/// 严格到「只放行 `A-Za-z0-9` 与 `- _ . ~ /`」是刻意的：前缀会被拼进 HTML
/// （`window.__ASMODAY_BASE__` 与 `<link href>`），放行引号/尖括号就是 XSS 面。
pub fn normalize(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return None;
    }
    if trimmed.len() > MAX_LEN || !trimmed.starts_with('/') {
        return None;
    }

    let body = trimmed.trim_end_matches('/');
    if body.is_empty() {
        return None;
    }

    let ok = body[1..].split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
    });
    ok.then(|| body.to_string())
}

/// 选出生效前缀。
///
/// 返回 `(前缀, 配置与头是否冲突)`：冲突只用于打日志，行为上仍然是配置优先。
pub fn resolve(configured: &str, forwarded: Option<&str>) -> (RuntimePrefix, bool) {
    let config_prefix = normalize(configured);
    let header_prefix = forwarded.and_then(normalize);

    match (config_prefix, header_prefix) {
        (Some(config), Some(header)) => {
            let mismatch = config != header;
            (RuntimePrefix::new(config), mismatch)
        }
        (Some(config), None) => (RuntimePrefix::new(config), false),
        (None, Some(header)) => (RuntimePrefix::new(header), false),
        (None, None) => (RuntimePrefix::default(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_clean_prefixes() {
        assert_eq!(normalize("/asmoday").as_deref(), Some("/asmoday"));
        assert_eq!(
            normalize("/apps/asmoday/").as_deref(),
            Some("/apps/asmoday")
        );
        assert_eq!(normalize("  /a_1.b~c  ").as_deref(), Some("/a_1.b~c"));
    }

    #[test]
    fn normalize_rejects_unsafe_or_root() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("/"), None);
        assert_eq!(normalize("   "), None);
        // 没有前导斜杠：反代配错时常见（`X-Forwarded-Prefix: asmoday`）
        assert_eq!(normalize("asmoday"), None);
        // 空段 / 上级目录
        assert_eq!(normalize("//asmoday"), None);
        assert_eq!(normalize("/a//b"), None);
        assert_eq!(normalize("/../etc"), None);
        // 会拼进 HTML 的字符一律拒绝
        assert_eq!(normalize("/a\"><script>"), None);
        assert_eq!(normalize("/a b"), None);
        // 超长
        assert_eq!(normalize(&format!("/{}", "a".repeat(MAX_LEN))), None);
    }

    #[test]
    fn config_wins_over_header_and_reports_mismatch() {
        let (prefix, mismatch) = resolve("/cfg", Some("/hdr"));
        assert_eq!(prefix.as_str(), "/cfg");
        assert!(mismatch, "配置与头不一致必须打日志");

        let (prefix, mismatch) = resolve("/cfg", Some("/cfg"));
        assert_eq!(prefix.as_str(), "/cfg");
        assert!(!mismatch);

        // 头缺省 / 头非法时都不算冲突
        assert!(!resolve("/cfg", None).1);
        assert!(!resolve("/cfg", Some("hdr")).1);
    }

    #[test]
    fn header_fills_in_when_config_is_empty() {
        let (prefix, mismatch) = resolve("", Some("/asmoday"));
        assert_eq!(prefix.as_str(), "/asmoday");
        assert!(!mismatch);
        assert!(resolve("", None).0.is_empty());
    }

    #[test]
    fn strip_respects_segment_boundaries() {
        let prefix = RuntimePrefix::new("/app");
        assert_eq!(prefix.strip("/app"), "/");
        assert_eq!(prefix.strip("/app/api/status"), "/api/status");
        // 关键：不能被当成字符串前缀
        assert_eq!(prefix.strip("/application"), "/application");
        assert_eq!(prefix.strip("/api/status"), "/api/status");
        // 根路径部署时什么都不改
        assert_eq!(RuntimePrefix::default().strip("/api/status"), "/api/status");
    }
}
