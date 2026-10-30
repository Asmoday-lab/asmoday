//! 认证接口与鉴权中间件（设计文档 3.1 / 6）。
//!
//! - 会话走 HttpOnly Cookie（`asmoday_session`），服务端只存随机 token；
//! - `/api/*` 与 `/proxy/*` 全部经中间件拦截，只有登录接口与健康检查放行；
//! - 连续失败按 IP 封禁 15 分钟；忘记密码只能走 CLI。

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::core::auth::{now_secs, verify_password};
use crate::core::totp;

/// 会话 Cookie 名
pub const SESSION_COOKIE: &str = "asmoday_session";

/// 注入到请求扩展里的当前用户
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    /// 是否还是一次性密码（`--init` / `--reset-password`）会话：改密前只放行改密自身
    pub must_change_password: bool,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<AuthUser>().cloned().ok_or_else(|| {
            json_error_code(StatusCode::UNAUTHORIZED, "AUTH_REQUIRED", "Unauthorized")
        })
    }
}

// ---------------------------------------------------------------------------
// Cookie 工具
// ---------------------------------------------------------------------------

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 组装会话 Cookie（设计文档 3.1.3）：`Path` 跟 `base_url`、`Secure` 只在 HTTPS 下加。
fn session_cookie(token: &str, max_age: i64, path: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; Path={path}; HttpOnly; SameSite=Lax{secure}; Max-Age={max_age}"
    )
}

/// 当前请求下的 Cookie 作用域：`(Path, Secure)`。
///
/// - `Path` 必须与 `base_url` 一致（子路径部署时 Cookie 不越界到同域其他应用）；
/// - HTTPS 判定看「自己开了 SSL」或「来自可信反代的 `X-Forwarded-Proto: https`」，
///   未可信的转发头一律忽略，避免被人用假头把 Cookie 标成 Secure 导致登录态失效。
async fn cookie_scope(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> (String, bool) {
    let (base_url, ssl_enabled, trusted) = {
        let config = state.config.read().await;
        (
            config.server.proxy.base_url.clone(),
            config.server.ssl.enabled,
            config.server.proxy.trusted_ips.clone(),
        )
    };

    let base = base_url.trim().trim_end_matches('/');
    let path = if base.is_empty() {
        "/".to_string()
    } else if base.starts_with('/') {
        base.to_string()
    } else {
        format!("/{base}")
    };

    let via_trusted_proxy = peer
        .map(|addr| trusted.iter().any(|rule| ip_matches(&peer_ip(addr), rule)))
        .unwrap_or(false);
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);

    (path, ssl_enabled || (via_trusted_proxy && forwarded_https))
}

/// CSRF 第一道防线（设计文档 3.1.3）：Cookie 认证下的写操作必须与请求 Host 同源。
///
/// 判定口径：
/// - 只检 `POST / PUT / PATCH / DELETE`（GET/HEAD 不改状态）；
/// - `Origin` 优先，其次 `Referer`；两者都缺失时放行（curl / 老浏览器 / 非浏览器客户端）；
/// - `Origin: null` 视为不可信（沙箱 iframe、file://），直接拒绝；
/// - 比较时剥掉 scheme 与默认端口（80/443），子路径部署的前缀不影响主机比较。
fn origin_matches_host(headers: &HeaderMap, method: &Method) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return true;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        // 没有 Host 头（HTTP/1.0 或测试客户端）：无从判定，交给 SameSite=Lax 兜底
        return true;
    };
    let candidate = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|value| value.to_str().ok());
    let Some(candidate) = candidate else {
        return true;
    };
    if candidate.eq_ignore_ascii_case("null") {
        return false;
    }
    normalize_authority(authority_of(candidate)) == normalize_authority(host)
}

/// 从 `Origin` / `Referer` 里取出 authority（去掉 scheme、路径与 userinfo）。
fn authority_of(candidate: &str) -> &str {
    candidate
        .split("://")
        .nth(1)
        .unwrap_or(candidate)
        .split('/')
        .next()
        .unwrap_or("")
        // 去掉 userinfo（`user:pass@host`）
        .rsplit('@')
        .next()
        .unwrap_or("")
}

/// `/api/auth/verify` 已随二次验证闸门移除，这里只剩改密自身的最小闭环。
fn is_password_change_path(path: &str) -> bool {
    matches!(
        path,
        "/api/auth/password" | "/api/auth/me" | "/api/auth/logout"
    )
}

/// 统一的「客户端 IP 字符串」入口：`normalize_ip` 折叠 v4-mapped。
/// 所有需要拿 peer IP 做匹配/键值/日志的地方都走它，保证写法一致。
pub(crate) fn peer_ip(peer: SocketAddr) -> String {
    normalize_ip(peer.ip()).to_string()
}

/// XFF / X-Real-IP 单个条目的归一：剥 bracket、折叠 v4-mapped。
/// 有些反代把 IPv6 写成 `[2001:db8::1]` 甚至 `[2001:db8::1]:443`，
/// 不归一的话限流/封禁键会分裂。
fn forwarded_ip_candidate(raw: &str) -> String {
    let trimmed = raw.trim();
    let value = match trimmed.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(host, _)| host).unwrap_or(rest),
        None => trimmed,
    };
    normalize_ip_str(value)
}

/// WebSocket 握手的同源校验。
///
/// 握手是 GET，`origin_matches_host` 对 GET 一律放行（导航请求不能带自定义头），
/// 所以跨站页面 `new WebSocket("ws://victim/api/terminal/ws")` 能直接连上终端——
/// 这就是 CSWSH。这里按 Origin 严格比对；浏览器一定会带 Origin，
/// 不带的是 curl / 脚本这类非浏览器客户端，本身没有 CSRF 面。
pub(crate) fn ws_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    normalize_authority(authority_of(origin)) == normalize_authority(host)
}

/// authority 拆成 `(host, port)`。IPv6 字面量按 bracket 解析，
/// 避免 `rsplit_once(':')` 把 `[::1]:8080` 的地址本体拆碎。
fn split_authority(value: &str) -> (&str, Option<u16>) {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let host = &rest[..end];
        let port = rest[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok());
        return (host, port);
    }
    match value.rsplit_once(':') {
        // 冒号后全是数字才算端口；`example.com:` 之类残缺串按无端口处理
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, port.parse().ok())
        }
        _ => (value, None),
    }
}

/// 主机名归一：小写 + 去掉默认端口 + IPv6 规范化。
///
/// IPv6 在 Host/Origin 里必然带 bracket，剥掉后交给 `Ipv6Addr::parse`：
/// - 规范写法归一（`[0:0:0:0:0:0:0:1]` 与 `[::1]` 相等）；
/// - v4-mapped 折叠成 IPv4（`[::ffff:127.0.0.1]` 与 `127.0.0.1` 相等，
///   与下方 `normalize_ip` 对 peer 的口径一致）；
/// - 解析失败按普通主机名处理，行为与旧实现兼容。
fn normalize_authority(value: &str) -> String {
    let (host, port) = split_authority(value);
    let host = match host.parse::<std::net::Ipv6Addr>() {
        Ok(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(), // Display 即小写规范形
        },
        Err(_) => host.to_ascii_lowercase(),
    };
    match port {
        Some(80) | Some(443) => host,
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

/// socket 层的 v4-mapped IPv6 还原成 IPv4 字面量。
///
/// 双栈监听（`host = "::"` + `IPV6_V6ONLY=false`）下，IPv4 连接的 peer 是
/// `::ffff:a.b.c.d`。不还原的话 `trusted_ips` 里写的 `127.0.0.1` 匹配不上、
/// 限流/口令防爆破的键与日志分裂成两种写法（v4 直连一条、经 v6 栈又一条）。
pub(crate) fn normalize_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// `normalize_ip` 的字符串入口：解析失败原样返回（调用方多在日志/键场景）。
pub(crate) fn normalize_ip_str(ip: &str) -> String {
    ip.parse::<std::net::IpAddr>()
        .map(normalize_ip)
        .map(|parsed| parsed.to_string())
        .unwrap_or_else(|_| ip.to_string())
}

fn cleared_cookie(path: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; Path={path}; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// 细节型错误（4xx 参数类，消息本身带具体说明）
fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2：前端按 code 查 i18n）
fn json_error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// 可信来源判定 + 真实客户端 IP（设计文档 3.1.5 / 3.1.7）。
///
/// **只有** `trust_proxy_headers=true` **且** socket 来源命中 `trusted_ips` 时才采信
/// `X-Forwarded-For` / `X-Real-IP`；白名单为空等于谁都不信（否则任何人都能伪造
/// 一个 IP 把防爆破池绕过去）。未可信时一律用 socket 地址。
async fn client_ip(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let (trust, trusted) = {
        let config = state.config.read().await;
        (
            config.server.proxy.trust_proxy_headers,
            config.server.proxy.trusted_ips.clone(),
        )
    };

    let peer_ip = match peer {
        Some(addr) => crate::api::auth::peer_ip(addr),
        None => return "unknown".to_string(),
    };
    pick_client_ip(trust, &trusted, &peer_ip, headers)
}

/// `client_ip` 的纯函数内核（方便单测，不依赖 AppState）。
pub(crate) fn pick_client_ip(
    trust_proxy_headers: bool,
    trusted: &[String],
    peer_ip: &str,
    headers: &HeaderMap,
) -> String {
    if !trust_proxy_headers || !trusted.iter().any(|rule| ip_matches(peer_ip, rule)) {
        return peer_ip.to_string();
    }

    if let Some(forwarded) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
    {
        let forwarded = forwarded_ip_candidate(forwarded);
        if !forwarded.is_empty() {
            return forwarded;
        }
    }
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let real = forwarded_ip_candidate(real);
        if !real.is_empty() {
            return real;
        }
    }

    peer_ip.to_string()
}

// ---------------------------------------------------------------------------
// 中间件
// ---------------------------------------------------------------------------

/// 判断 IP 是否命中 trusted_ips（支持精确匹配与 IPv4/IPv6 CIDR）。
///
/// 输入先过 `normalize_ip_str`：双栈下 IPv4 连接的 peer 是 `::ffff:a.b.c.d`，
/// 归一后才能命中用户按 IPv4 写的规则。两个家族各自比较，不跨族匹配
/// （v4-mapped 已折叠，剩下的纯 v6 与 v4 没有可比性）。
pub(crate) fn ip_matches(ip: &str, rule: &str) -> bool {
    let ip = normalize_ip_str(ip);
    let rule = rule.trim();
    if ip.eq_ignore_ascii_case(rule) {
        return true;
    }
    let Some((network, bits)) = rule.split_once('/') else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };

    fn parse_v4(value: &str) -> Option<u128> {
        let parts: Vec<u8> = value
            .split('.')
            .map(|part| part.parse::<u8>().ok())
            .collect::<Option<Vec<u8>>>()?;
        if parts.len() != 4 {
            return None;
        }
        Some(
            ((parts[0] as u128) << 24)
                | ((parts[1] as u128) << 16)
                | ((parts[2] as u128) << 8)
                | parts[3] as u128,
        )
    }
    let (addr, network, max_bits): (u128, u128, u32) = if ip.contains(':') {
        // IPv6：`ip` 已归一（v4-mapped 折叠），这里仍按解析结果走，双保险
        match (
            ip.parse::<std::net::Ipv6Addr>(),
            network.parse::<std::net::Ipv6Addr>(),
        ) {
            (Ok(addr), Ok(net)) => (addr.to_bits(), net.to_bits(), 128),
            _ => return false,
        }
    } else {
        match (parse_v4(&ip), parse_v4(network)) {
            (Some(addr), Some(net)) => (addr, net, 32),
            _ => return false,
        }
    };
    if bits > max_bits {
        return false;
    }
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (max_bits - bits)
    };
    (addr & mask) == (network & mask)
}

/// “座上宾”模式：信任反向代理传入的用户名 Header（设计文档 3.1.8）。
///
/// 三道闸，缺一不可：
/// 1. `auth.proxy_trust.enabled` 打开，且 `user_header` 非空；
/// 2. 请求源 IP 命中 **`auth.proxy_trust.trusted_ips`**（白名单为空一律不采信）；
/// 3. 用户**必须已存在** —— 文档明确禁止自动建号，否则任何能伪造 Header 的人都能开号。
///
/// 2FA 默认不跳过：用户开了 TOTP 且没有显式 `skip_2fa=true` 时返回 None，
/// 让请求回落到本地登录把 OTP 走完（Header 直通不能成为绕过第二因子的后门）。
async fn sso_user(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<AuthUser> {
    let (enabled, header_name, trusted, skip_2fa) = {
        let config = state.config.read().await;
        (
            config.auth.proxy_trust.enabled,
            config.auth.proxy_trust.user_header.trim().to_lowercase(),
            config.auth.proxy_trust.trusted_ips.clone(),
            config.auth.proxy_trust.skip_2fa,
        )
    };
    if !enabled || header_name.is_empty() {
        return None;
    }

    let peer_ip = peer.map(crate::api::auth::peer_ip)?;
    if trusted.iter().all(|rule| rule.trim().is_empty())
        || !trusted.iter().any(|rule| ip_matches(&peer_ip, rule))
    {
        return None;
    }

    let username = headers
        .get(header_name.as_str())?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    if username.is_empty() {
        return None;
    }

    // 只认已有账号：不存在的用户直接拒绝，绝不自动建档
    let Ok(Some((user, _, _))) = state.auth.find_user(&username) else {
        return None;
    };
    if user.totp_enabled && !skip_2fa {
        return None;
    }

    Some(AuthUser {
        id: user.id,
        username: user.username,
        must_change_password: user.must_change_password,
    })
}

/// 拦截未登录请求：`/api/*` 与 `/proxy/*` 均需有效会话（或可信代理身份）。
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    // 初始化完成前，除初始化/登录接口外一律 503（设计文档 3.1.1）：
    // 否则未初始化的实例会以「匿名可读」的姿态把挂载点暴露出去。
    if state.auth.user_count().unwrap_or(0) == 0 {
        return json_error_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_INIT_REQUIRED",
            "Instance is not initialized yet",
        );
    }

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);

    let mut auth_user: Option<AuthUser> = None;
    // 本次请求是否靠会话 Cookie 认证：只有 Cookie 才需要 CSRF 同源校验
    // （Token / SSO Header 不会被浏览器自动携带，不构成 CSRF 面）
    let mut via_cookie = false;

    if let Some(token) = cookie_token(request.headers()) {
        match state.auth.session_state(&token) {
            Ok(Some(session)) => match state.auth.user_by_id(session.user_id) {
                Ok(Some(user)) => {
                    auth_user = Some(AuthUser {
                        id: user.id,
                        username: user.username,
                        must_change_password: user.must_change_password,
                    });
                    via_cookie = true;
                }
                _ => {
                    return json_error_code(
                        StatusCode::UNAUTHORIZED,
                        "AUTH_USER_MISSING",
                        "User not found",
                    );
                }
            },
            Ok(None) => {
                // 会话过期：继续尝试代理信任，失败再报错
            }
            Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
        }
    }

    if auth_user.is_none() {
        auth_user = sso_user(&state, request.headers(), peer).await;
    }

    let Some(user) = auth_user else {
        return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_REQUIRED", "Unauthorized");
    };

    // CSRF（设计文档 3.1.3）：Cookie 认证的写操作必须同源，SameSite=Lax 只是第二道防线
    if via_cookie && !origin_matches_host(request.headers(), request.method()) {
        return json_error_code(
            StatusCode::FORBIDDEN,
            "E_FORBIDDEN",
            "Cross-site request rejected",
        );
    }

    // 滑动续期（设计文档 3.1.3）：Cookie 的 Max-Age 跟着每次请求往后刷，
    // 服务端 expires_at 由 `session_user` 在临近过期时才写库。
    let refreshed_cookie = if via_cookie {
        match cookie_token(request.headers()) {
            Some(token) => {
                let (path, secure) = cookie_scope(&state, request.headers(), peer).await;
                Some(session_cookie(
                    &token,
                    crate::core::auth::SESSION_TTL,
                    &path,
                    secure,
                ))
            }
            None => None,
        }
    } else {
        None
    };

    // 一次性密码（`--init` / `--reset-password`）会话必须先改密。
    // 前端有遮罩，但遮罩只是 UI——直接 curl 带着 Cookie 照样能开终端、
    // 能读全盘。这里只放行改密自身与状态查询，其余一律 403。
    if user.must_change_password && !is_password_change_path(request.uri().path()) {
        return json_error_code(
            StatusCode::FORBIDDEN,
            "E_PASSWORD_CHANGE_REQUIRED",
            "Initial password must be changed before using the API",
        );
    }

    request.extensions_mut().insert(user);
    let mut response = next.run(request).await;
    // 处理器自己发过 Set-Cookie（登录/改密/登出）就不要再叠一条，否则两条 Cookie 打架
    if let Some(cookie) = refreshed_cookie
        && !response.headers().contains_key(header::SET_COOKIE)
        && let Ok(value) = header::HeaderValue::from_str(&cookie)
    {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

// ---------------------------------------------------------------------------
// 请求类型
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub totp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PasswordRequest {
    pub current: String,
    pub next: String,
}

#[derive(Debug, Deserialize)]
pub struct TotpEnableRequest {
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct TotpDisableRequest {
    pub password: String,
    /// 当前六位 TOTP 码，或一个一次性恢复码（设计文档 3.1.4）
    #[serde(default)]
    pub code: String,
}

// ---------------------------------------------------------------------------
// 处理函数
// ---------------------------------------------------------------------------

/// GET /api/auth/status（公开）
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let initialized = state.auth.user_count().unwrap_or(0) > 0;
    let mut username: Option<String> = None;
    let mut totp_enabled = false;
    let mut must_change_password = false;

    if let Some(token) = cookie_token(&headers)
        && let Ok(Some(user_id)) = state.auth.session_user(&token)
        && let Ok(Some(user)) = state.auth.user_by_id(user_id)
    {
        totp_enabled = user.totp_enabled;
        must_change_password = user.must_change_password;
        username = Some(user.username);
    }

    Json(json!({
        "initialized": initialized,
        "authenticated": username.is_some(),
        "username": username,
        "totp_enabled": totp_enabled,
        "must_change_password": must_change_password,
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// POST /api/auth/setup（公开，仅首次）
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SetupRequest>,
) -> Response {
    match state.auth.user_count() {
        Ok(0) => {}
        Ok(_) => {
            return json_error_code(
                StatusCode::CONFLICT,
                "AUTH_ALREADY_INITIALIZED",
                "Already initialized",
            );
        }
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }

    let user = match state.auth.create_user(&body.username, &body.password) {
        Ok(user) => user,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err),
    };

    let token = match state.auth.create_session(user.id) {
        Ok(token) => token,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let mut response = Json(json!({
        "status": "ok",
        "username": user.username,
        "message": "初始化完成"
    }))
    .into_response();

    let (path, secure) = cookie_scope(&state, &headers, Some(peer)).await;
    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(
        &token,
        crate::core::auth::SESSION_TTL,
        &path,
        secure,
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

fn record_login_failure(guard: &crate::core::auth::LoginGuard, keys: &[String; 2]) {
    for key in keys {
        guard.record_failure(key);
    }
}

fn clear_login_failures(guard: &crate::core::auth::LoginGuard, keys: &[String; 2]) {
    for key in keys {
        guard.record_success(key);
    }
}

/// POST /api/auth/login（公开）
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    // 计数维度（设计文档 3.1.5）：纯 IP（防用户名枚举）+ IP|用户名（挡单账号爆破）。
    // 两者都落在主登录池里，与分享口令、WebDAV 的独立池互不影响。
    let ip = client_ip(&state, &headers, Some(peer)).await;
    let keys = [
        ip.clone(),
        format!("{ip}|{}", body.username.trim().to_lowercase()),
    ];

    for key in &keys {
        if let Err(remaining) = state.login_guard.check(key) {
            return json_error_code(
                StatusCode::TOO_MANY_REQUESTS,
                "AUTH_RATE_LIMITED",
                format!("Too many attempts, retry in {remaining}s"),
            );
        }
    }

    let found = match state.auth.find_user(body.username.trim()) {
        Ok(found) => found,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Some((user, hash, secret)) = found else {
        record_login_failure(&state.login_guard, &keys);
        return json_error_code(
            StatusCode::UNAUTHORIZED,
            "AUTH_INVALID",
            "Invalid username or password",
        );
    };

    if !verify_password(&hash, &body.password) {
        record_login_failure(&state.login_guard, &keys);
        return json_error_code(
            StatusCode::UNAUTHORIZED,
            "AUTH_INVALID",
            "Invalid username or password",
        );
    }

    // 可选双因素（设计文档 3.1.4）：6 位数字走 TOTP，其余当一次性恢复码
    if user.totp_enabled {
        let Some(secret) = secret else {
            return json_error_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TOTP_CONFIG_MISSING",
                "TOTP secret missing",
            );
        };
        let code = body.totp.as_deref().unwrap_or("").trim();
        if code.is_empty() {
            return json_error_code(
                StatusCode::UNAUTHORIZED,
                "TOTP_REQUIRED",
                "TOTP code required",
            );
        }
        let normalized = crate::core::auth::normalize_recovery_code(code);
        let is_totp = normalized.len() == 6 && normalized.chars().all(|c| c.is_ascii_digit());
        let valid = if is_totp {
            normalized
                .parse::<u32>()
                .ok()
                .is_some_and(|number| totp::verify(&secret, number, now_secs() as u64))
        } else {
            state
                .auth
                .consume_recovery_code(user.id, code)
                .unwrap_or(false)
        };
        if !valid {
            record_login_failure(&state.login_guard, &keys);
            return json_error_code(
                StatusCode::UNAUTHORIZED,
                "TOTP_INVALID",
                "Invalid TOTP code",
            );
        }
    }

    clear_login_failures(&state.login_guard, &keys);
    let _ = state.auth.purge_expired();

    let token = match state.auth.create_session(user.id) {
        Ok(token) => token,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let mut response = Json(json!({
        "status": "ok",
        "username": user.username,
        "must_change_password": user.must_change_password,
    }))
    .into_response();
    let (path, secure) = cookie_scope(&state, &headers, Some(peer)).await;
    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(
        &token,
        crate::core::auth::SESSION_TTL,
        &path,
        secure,
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// POST /api/auth/logout（公开：清 Cookie 是幂等操作）
pub async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = cookie_token(&headers) {
        let _ = state.auth.delete_session(&token);
    }
    let mut response = Json(json!({ "status": "ok" })).into_response();
    let (path, secure) = cookie_scope(&state, &headers, Some(peer)).await;
    if let Ok(value) = header::HeaderValue::from_str(&cleared_cookie(&path, secure)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// GET /api/auth/me（受保护）
pub async fn me(auth_user: AuthUser) -> Response {
    Json(json!({
        "id": auth_user.id,
        "username": auth_user.username,
        "must_change_password": auth_user.must_change_password,
    }))
    .into_response()
}

/// POST /api/auth/password（受保护）
pub async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    auth_user: AuthUser,
    Json(body): Json<PasswordRequest>,
) -> Response {
    let found = match state.auth.find_user(&auth_user.username) {
        Ok(found) => found,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Some((user, hash, _)) = found else {
        return json_error_code(
            StatusCode::UNAUTHORIZED,
            "AUTH_USER_MISSING",
            "User not found",
        );
    };

    if !verify_password(&hash, &body.current) {
        return json_error_code(
            StatusCode::BAD_REQUEST,
            "PASSWORD_WRONG",
            "Current password is incorrect",
        );
    }

    if let Err(err) = state.auth.update_password(user.id, &body.next) {
        return json_error(StatusCode::BAD_REQUEST, err);
    }

    // 改密会清空所有会话，这里给当前浏览器补一个新的
    let token = match state.auth.create_session(user.id) {
        Ok(token) => token,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let mut response = Json(json!({ "status": "ok" })).into_response();
    let (path, secure) = cookie_scope(&state, &headers, Some(peer)).await;
    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(
        &token,
        crate::core::auth::SESSION_TTL,
        &path,
        secure,
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// GET /api/auth/totp/setup（受保护）：生成密钥、otpauth 链接与二维码
pub async fn totp_setup(State(state): State<AppState>, auth_user: AuthUser) -> Response {
    let secret = totp::generate_secret();
    if let Err(err) = state.auth.set_totp(auth_user.id, &secret, false) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    let uri = totp::otpauth_url("Asmoday", &auth_user.username, &secret);
    // 二维码跟着 setup 一起下发：前端零依赖、离线可扫，不需要再发一次请求
    Json(json!({
        "secret": secret,
        "uri": uri,
        "qr_svg": totp::qr_svg(&uri),
    }))
    .into_response()
}

/// POST /api/auth/totp/enable（受保护）
pub async fn totp_enable(
    State(state): State<AppState>,
    auth_user: AuthUser,
    Json(body): Json<TotpEnableRequest>,
) -> Response {
    let secret = match state.auth.totp_secret(auth_user.id) {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            return json_error_code(
                StatusCode::BAD_REQUEST,
                "TOTP_NOT_SETUP",
                "Generate a TOTP secret first",
            );
        }
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Ok(code) = body.code.trim().parse::<u32>() else {
        return json_error_code(
            StatusCode::BAD_REQUEST,
            "TOTP_FORMAT",
            "TOTP code must be 6 digits",
        );
    };

    if !totp::verify(&secret, code, now_secs() as u64) {
        return json_error_code(StatusCode::BAD_REQUEST, "TOTP_INVALID", "Invalid TOTP code");
    }

    if let Err(err) = state.auth.set_totp(auth_user.id, &secret, true) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    // 开启成功即发 10 个一次性恢复码：明文只在这一个响应里出现（设计文档 3.1.4）
    let codes = crate::core::auth::generate_recovery_codes(10);
    if let Err(err) = state.auth.set_recovery_codes(auth_user.id, &codes) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    Json(json!({
        "status": "ok",
        "totp_enabled": true,
        "recovery_codes": codes,
    }))
    .into_response()
}

/// POST /api/auth/totp/disable（受保护，需当前密码）
pub async fn totp_disable(
    State(state): State<AppState>,
    auth_user: AuthUser,
    Json(body): Json<TotpDisableRequest>,
) -> Response {
    let found = match state.auth.find_user(&auth_user.username) {
        Ok(found) => found,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Some((user, hash, _)) = found else {
        return json_error_code(
            StatusCode::UNAUTHORIZED,
            "AUTH_USER_MISSING",
            "User not found",
        );
    };

    if !verify_password(&hash, &body.password) {
        return json_error_code(
            StatusCode::BAD_REQUEST,
            "PASSWORD_WRONG",
            "Password is incorrect",
        );
    }

    // 关闭 2FA 必须给出「有效 TOTP 或恢复码」，只凭密码不给关（设计文档 3.1.4）
    let code = body.code.trim();
    if code.is_empty() {
        return json_error_code(
            StatusCode::BAD_REQUEST,
            "TOTP_REQUIRED",
            "TOTP code or recovery code required",
        );
    }
    let normalized = crate::core::auth::normalize_recovery_code(code);
    let is_totp = normalized.len() == 6 && normalized.chars().all(|c| c.is_ascii_digit());
    let valid = if is_totp {
        match state.auth.totp_secret(user.id) {
            Ok(Some(secret)) => normalized
                .parse::<u32>()
                .ok()
                .is_some_and(|number| totp::verify(&secret, number, now_secs() as u64)),
            _ => false,
        }
    } else {
        state
            .auth
            .consume_recovery_code(user.id, code)
            .unwrap_or(false)
    };
    if !valid {
        return json_error_code(StatusCode::BAD_REQUEST, "TOTP_INVALID", "Invalid TOTP code");
    }

    if let Err(err) = state.auth.set_totp(user.id, "", false) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }
    // 关闭后残留的恢复码必须作废，否则等于留了一把万能钥匙
    if let Err(err) = state.auth.clear_recovery_codes(user.id) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    Json(json!({ "status": "ok", "totp_enabled": false })).into_response()
}

#[cfg(test)]
mod tests {
    use super::{normalize_authority, origin_matches_host};
    use axum::http::{HeaderMap, Method, header};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            match *name {
                "host" => {
                    map.insert(header::HOST, value.parse().unwrap());
                }
                "origin" => {
                    map.insert(header::ORIGIN, value.parse().unwrap());
                }
                "referer" => {
                    map.insert(header::REFERER, value.parse().unwrap());
                }
                _ => {}
            }
        }
        map
    }

    #[test]
    fn csrf_allows_safe_methods() {
        let map = headers(&[
            ("host", "nas.local:4242"),
            ("origin", "http://evil.example"),
        ]);
        assert!(origin_matches_host(&map, &Method::GET));
        assert!(origin_matches_host(&map, &Method::HEAD));
    }

    #[test]
    fn csrf_accepts_same_origin() {
        let map = headers(&[
            ("host", "nas.local:4242"),
            ("origin", "http://nas.local:4242"),
        ]);
        assert!(origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn csrf_rejects_cross_site_origin() {
        let map = headers(&[
            ("host", "nas.local:4242"),
            ("origin", "https://evil.example"),
        ]);
        assert!(!origin_matches_host(&map, &Method::DELETE));
    }

    #[test]
    fn csrf_ignores_default_port_difference() {
        let map = headers(&[("host", "nas.local"), ("origin", "https://nas.local:443")]);
        assert!(origin_matches_host(&map, &Method::PUT));
    }

    #[test]
    fn csrf_uses_referer_when_origin_missing() {
        let map = headers(&[
            ("host", "nas.local:4242"),
            ("referer", "http://nas.local:4242/files/m0/a"),
        ]);
        assert!(origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn csrf_rejects_null_origin() {
        let map = headers(&[("host", "nas.local:4242"), ("origin", "null")]);
        assert!(!origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn csrf_allows_requests_without_origin() {
        // curl / 老浏览器不会带 Origin：交给 SameSite=Lax，不误伤 API 调用
        let map = headers(&[("host", "nas.local:4242")]);
        assert!(origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn authority_normalization_strips_default_ports() {
        assert_eq!(normalize_authority("NAS.Local:443"), "nas.local");
        assert_eq!(normalize_authority("nas.local:80"), "nas.local");
        assert_eq!(normalize_authority("nas.local:4242"), "nas.local:4242");
    }

    fn proxy_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let value: axum::http::HeaderValue = value.parse().unwrap();
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value,
            );
        }
        map
    }

    #[test]
    fn forwarded_ip_ignored_when_peer_not_trusted() {
        // 直连 + 伪造 XFF：必须只认 socket 地址（否则防爆破池一句话被绕过）
        let map = proxy_headers(&[("x-forwarded-for", "10.9.9.9")]);
        let ip = super::pick_client_ip(false, &["127.0.0.1".to_string()], "203.0.113.5", &map);
        assert_eq!(ip, "203.0.113.5");
    }

    #[test]
    fn forwarded_ip_ignored_when_whitelist_empty() {
        let map = proxy_headers(&[("x-real-ip", "10.9.9.9")]);
        let ip = super::pick_client_ip(true, &[], "203.0.113.5", &map);
        assert_eq!(ip, "203.0.113.5");
    }

    #[test]
    fn forwarded_ip_used_from_trusted_cidr() {
        let map = proxy_headers(&[("x-forwarded-for", "198.51.100.7, 172.16.0.2")]);
        let ip = super::pick_client_ip(true, &["172.16.0.0/12".to_string()], "172.16.5.9", &map);
        assert_eq!(ip, "198.51.100.7");
    }

    #[test]
    fn session_cookie_paths_follow_base_url() {
        let root = super::session_cookie("t", 60, "/", false);
        assert!(root.contains("Path=/;"));
        assert!(!root.contains("Secure"));

        let sub = super::session_cookie("t", 60, "/nas", true);
        assert!(sub.contains("Path=/nas;"));
        assert!(sub.contains("Secure"));

        let cleared = super::cleared_cookie("/nas", true);
        assert!(cleared.contains("Path=/nas;"));
        assert!(cleared.contains("Max-Age=0"));
    }

    // ---- IPv6 支持（双栈监听 / trusted_ips / 同源判定）----

    use super::{forwarded_ip_candidate, ip_matches, normalize_ip_str, peer_ip};
    use std::net::{IpAddr, SocketAddr};

    #[test]
    fn trusted_ips_match_ipv6_cidr_and_exact() {
        // 精确匹配（含大写/压缩写法归一）
        assert!(ip_matches("2001:db8::1", "2001:db8::1"));
        assert!(ip_matches("2001:DB8:0:0:0:0:0:1", "2001:db8::1"));
        assert!(!ip_matches("2001:db8::2", "2001:db8::1"));
        // CIDR
        assert!(ip_matches("2001:db8:abcd::1", "2001:db8::/32"));
        assert!(ip_matches("fe80::a", "fe80::/10"));
        assert!(!ip_matches("2001:db9::1", "2001:db8::/32"));
        assert!(ip_matches("2001:db8::1", "2001:db8::1/128"));
        // 位宽越界一律不命中
        assert!(!ip_matches("2001:db8::1", "2001:db8::/129"));
        // 家族隔离：v4 规则吃不下 v6，反之亦然
        assert!(!ip_matches("2001:db8::1", "127.0.0.1"));
        assert!(!ip_matches("2001:db8::1", "0.0.0.0/0"));
        assert!(!ip_matches("127.0.0.1", "::/0"));
        // v4 原有语义保持
        assert!(ip_matches("172.16.5.9", "172.16.0.0/12"));
        assert!(!ip_matches("172.32.0.1", "172.16.0.0/12"));
        // 非法输入不命中、不 panic
        assert!(!ip_matches("not-an-ip", "127.0.0.1"));
        assert!(!ip_matches("127.0.0.1", "300.1.1.1/8"));
    }

    #[test]
    fn v4_mapped_peer_folds_back_to_ipv4() {
        // 双栈监听下 IPv4 连接的 peer 是 ::ffff:a.b.c.d，必须还原
        let mapped = "::ffff:203.0.113.5".parse::<IpAddr>().unwrap();
        assert_eq!(normalize_ip_str("::ffff:203.0.113.5"), "203.0.113.5");
        assert_eq!(peer_ip(SocketAddr::new(mapped, 8443)), "203.0.113.5");
        // 归一后能命中 IPv4 规则（旧实现对 ::ffff: 写法全部失配）
        assert!(ip_matches("::ffff:172.16.5.9", "172.16.0.0/12"));
        // 纯 v6 保持 v6
        assert_eq!(normalize_ip_str("2001:DB8::1"), "2001:db8::1");
        // 非法字符串原样返回（日志/键场景不能丢信息）
        assert_eq!(normalize_ip_str("proxy.example"), "proxy.example");
    }

    #[test]
    fn authority_normalization_understands_ipv6() {
        // bracket + 端口
        assert_eq!(normalize_authority("[::1]:8080"), "::1:8080");
        assert_eq!(normalize_authority("[::1]"), "::1");
        // 默认端口剥掉
        assert_eq!(normalize_authority("[::1]:80"), "::1");
        assert_eq!(normalize_authority("[::1]:443"), "::1");
        // 全零组展开写法归一到压缩形
        assert_eq!(normalize_authority("[0:0:0:0:0:0:0:1]:8080"), "::1:8080");
        // v4-mapped 折叠，与 v4 直连写法等价
        assert_eq!(
            normalize_authority("[::ffff:127.0.0.1]:8080"),
            "127.0.0.1:8080"
        );
        assert_eq!(normalize_authority("[::ffff:7f00:1]"), "127.0.0.1");
        // bracket 内以 :80 结尾的地址不会被误剥端口
        assert_eq!(normalize_authority("[::80]"), "::80");
        // 主机名行为保持
        assert_eq!(normalize_authority("NAS.Local:8080"), "nas.local:8080");
        assert_eq!(normalize_authority("nas.local"), "nas.local");
    }

    #[test]
    fn csrf_and_ws_origin_accept_ipv6_hosts() {
        // Host/Origin 均为 IPv6 字面量，压缩写法不同也要相等
        let map = headers(&[
            ("host", "[2001:db8::1]:4242"),
            ("origin", "http://[2001:db8:0::1]:4242"),
        ]);
        assert!(origin_matches_host(&map, &Method::POST));
        // v4-mapped Origin 与 v4 Host 等价
        let map = headers(&[
            ("host", "127.0.0.1:4242"),
            ("origin", "http://[::ffff:127.0.0.1]:4242"),
        ]);
        assert!(origin_matches_host(&map, &Method::POST));
        // 不同地址必须拒绝
        let map = headers(&[("host", "[::1]:4242"), ("origin", "http://[::2]:4242")]);
        assert!(!origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn forwarded_value_strips_ipv6_brackets() {
        assert_eq!(forwarded_ip_candidate("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(forwarded_ip_candidate("[2001:db8::1]:443"), "2001:db8::1");
        assert_eq!(forwarded_ip_candidate(" 198.51.100.7 "), "198.51.100.7");
        // 无法解析的值原样保留
        assert_eq!(forwarded_ip_candidate("proxy.local"), "proxy.local");
    }
}
