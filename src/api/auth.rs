//! 认证接口与鉴权中间件（设计文档 3.1 / 6）。
//!
//! - 会话走 HttpOnly Cookie（`asmoday_session`），服务端只存随机 token；
//! - `/api/*` 与 `/proxy/*` 全部经中间件拦截，只有登录接口与健康检查放行；
//! - 连续失败按 IP 封禁 15 分钟；忘记密码只能走 CLI。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::core::auth::{now_secs, verify_password};
use crate::core::totp;
use crate::AppState;

/// 会话 Cookie 名
pub const SESSION_COOKIE: &str = "asmoday_session";

/// 注入到请求扩展里的当前用户
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or_else(|| json_error_code(StatusCode::UNAUTHORIZED, "AUTH_REQUIRED", "Unauthorized"))
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

fn session_cookie(token: &str, max_age: i64) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}")
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
    let authority = candidate
        .split("://")
        .nth(1)
        .unwrap_or(candidate)
        .split('/')
        .next()
        .unwrap_or("")
        // 去掉 userinfo（`user:pass@host`）
        .rsplit('@')
        .next()
        .unwrap_or("");
    normalize_authority(authority) == normalize_authority(host)
}

/// 主机名归一：小写 + 去掉默认端口
fn normalize_authority(value: &str) -> String {
    let lowered = value.trim().to_ascii_lowercase();
    for suffix in [":80", ":443"] {
        if let Some(stripped) = lowered.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    lowered
}

fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// 细节型错误（4xx 参数类，消息本身带具体说明）
fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::api::error::detail(status, message)
}

/// 带错误码的错误（设计文档 3.8.2：前端按 code 查 i18n）
fn json_error_code(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    crate::api::error::api_error(status, code, message)
}

/// 限流用 IP：信任代理头时取 X-Forwarded-For 首个地址
async fn client_key(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let trust = {
        let config = state.config.read().await;
        config.server.proxy.trust_proxy_headers
    };

    if trust {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
        {
            let forwarded = forwarded.trim();
            if !forwarded.is_empty() {
                return forwarded.to_string();
            }
        }
        if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            return real.trim().to_string();
        }
    }

    peer.map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// 中间件
// ---------------------------------------------------------------------------

/// 判断 IP 是否命中 trusted_ips（支持精确匹配与 IPv4 CIDR）
fn ip_matches(ip: &str, rule: &str) -> bool {
    if ip == rule {
        return true;
    }
    let Some((network, bits)) = rule.split_once('/') else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };
    if bits > 32 {
        return false;
    }
    let parse = |value: &str| -> Option<u32> {
        let parts: Vec<u8> = value
            .split('.')
            .map(|part| part.parse::<u8>().ok())
            .collect::<Option<Vec<u8>>>()?;
        if parts.len() != 4 {
            return None;
        }
        Some(((parts[0] as u32) << 24) | ((parts[1] as u32) << 16) | ((parts[2] as u32) << 8) | parts[3] as u32)
    };
    let (Some(a), Some(b)) = (parse(ip), parse(network)) else {
        return false;
    };
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    (a & mask) == (b & mask)
}

/// “座上宾”模式：信任反向代理传入的用户名 Header（需来源 IP 在白名单内）。
async fn sso_user(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<AuthUser> {
    let (enabled, header_name, trusted) = {
        let config = state.config.read().await;
        (
            config.auth.proxy_trust.enabled,
            config.auth.proxy_trust.user_header.to_lowercase(),
            config.server.proxy.trusted_ips.clone(),
        )
    };
    if !enabled {
        return None;
    }

    let peer_ip = peer.map(|addr| addr.ip().to_string())?;
    if !trusted.iter().any(|rule| ip_matches(&peer_ip, rule)) {
        return None;
    }

    let username = headers.get(header_name.as_str())?.to_str().ok()?.trim().to_string();
    if username.is_empty() {
        return None;
    }

    if let Ok(Some((user, _, _))) = state.auth.find_user(&username) {
        return Some(AuthUser { id: user.id, username: user.username });
    }

    // 首次出现的代理用户自动建档（随机密码，不能用于本地登录）
    let password = crate::core::auth::random_password();
    match state.auth.create_user(&username, &password) {
        Ok(user) => Some(AuthUser { id: user.id, username: user.username }),
        Err(_) => None,
    }
}

/// 拦截未登录请求：`/api/*` 与 `/proxy/*` 均需有效会话（或可信代理身份）。
pub async fn require_auth(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
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
        match state.auth.session_user(&token) {
            Ok(Some(user_id)) => match state.auth.user_by_id(user_id) {
                Ok(Some(user)) => {
                    auth_user = Some(AuthUser { id: user.id, username: user.username });
                    via_cookie = true;
                }
                _ => return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_USER_MISSING", "User not found"),
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

    request.extensions_mut().insert(user);
    next.run(request).await
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

    if let Some(token) = cookie_token(&headers) {
        if let Ok(Some(user_id)) = state.auth.session_user(&token) {
            if let Ok(Some(user)) = state.auth.user_by_id(user_id) {
                totp_enabled = user.totp_enabled;
                must_change_password = user.must_change_password;
                username = Some(user.username);
            }
        }
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
pub async fn setup(State(state): State<AppState>, Json(body): Json<SetupRequest>) -> Response {
    match state.auth.user_count() {
        Ok(0) => {}
        Ok(_) => return json_error_code(StatusCode::CONFLICT, "AUTH_ALREADY_INITIALIZED", "Already initialized"),
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

    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(&token, crate::core::auth::SESSION_TTL)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// POST /api/auth/login（公开）
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let key = client_key(&state, &headers, Some(peer)).await;

    if let Err(remaining) = state.login_guard.check(&key) {
        return json_error_code(
            StatusCode::TOO_MANY_REQUESTS,
            "AUTH_RATE_LIMITED",
            format!("Too many attempts, retry in {remaining}s"),
        );
    }

    let found = match state.auth.find_user(body.username.trim()) {
        Ok(found) => found,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Some((user, hash, secret)) = found else {
        state.login_guard.record_failure(&key);
        return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_INVALID", "Invalid username or password");
    };

    if !verify_password(&hash, &body.password) {
        state.login_guard.record_failure(&key);
        return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_INVALID", "Invalid username or password");
    }

    // 可选双因素
    if user.totp_enabled {
        let Some(secret) = secret else {
            return json_error_code(StatusCode::INTERNAL_SERVER_ERROR, "TOTP_CONFIG_MISSING", "TOTP secret missing");
        };
        let code = body.totp.as_deref().unwrap_or("").trim();
        let Some(code) = code.parse::<u32>().ok() else {
            return json_error_code(StatusCode::UNAUTHORIZED, "TOTP_REQUIRED", "TOTP code required");
        };
        if !totp::verify(&secret, code, now_secs() as u64) {
            state.login_guard.record_failure(&key);
            return json_error_code(StatusCode::UNAUTHORIZED, "TOTP_INVALID", "Invalid TOTP code");
        }
    }

    state.login_guard.record_success(&key);
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
    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(&token, crate::core::auth::SESSION_TTL)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// POST /api/auth/logout（公开：清 Cookie 是幂等操作）
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_token(&headers) {
        let _ = state.auth.delete_session(&token);
    }
    let mut response = Json(json!({ "status": "ok" })).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&cleared_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// GET /api/auth/me（受保护）
pub async fn me(State(state): State<AppState>, auth_user: AuthUser) -> Response {
    let must_change_password = state
        .auth
        .user_by_id(auth_user.id)
        .ok()
        .flatten()
        .map(|user| user.must_change_password)
        .unwrap_or(false);
    Json(json!({
        "id": auth_user.id,
        "username": auth_user.username,
        "must_change_password": must_change_password,
    }))
    .into_response()
}

/// POST /api/auth/password（受保护）
pub async fn change_password(
    State(state): State<AppState>,
    auth_user: AuthUser,
    Json(body): Json<PasswordRequest>,
) -> Response {
    let found = match state.auth.find_user(&auth_user.username) {
        Ok(found) => found,
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Some((user, hash, _)) = found else {
        return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_USER_MISSING", "User not found");
    };

    if !verify_password(&hash, &body.current) {
        return json_error_code(StatusCode::BAD_REQUEST, "PASSWORD_WRONG", "Current password is incorrect");
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
    if let Ok(value) = header::HeaderValue::from_str(&session_cookie(&token, crate::core::auth::SESSION_TTL)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// GET /api/auth/totp/setup（受保护）：生成密钥与 otpauth 链接
pub async fn totp_setup(State(state): State<AppState>, auth_user: AuthUser) -> Response {
    let secret = totp::generate_secret();
    if let Err(err) = state.auth.set_totp(auth_user.id, &secret, false) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    Json(json!({
        "secret": secret,
        "uri": totp::otpauth_url("Asmoday", &auth_user.username, &secret),
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
        Ok(None) => return json_error_code(StatusCode::BAD_REQUEST, "TOTP_NOT_SETUP", "Generate a TOTP secret first"),
        Err(err) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let Ok(code) = body.code.trim().parse::<u32>() else {
        return json_error_code(StatusCode::BAD_REQUEST, "TOTP_FORMAT", "TOTP code must be 6 digits");
    };

    if !totp::verify(&secret, code, now_secs() as u64) {
        return json_error_code(StatusCode::BAD_REQUEST, "TOTP_INVALID", "Invalid TOTP code");
    }

    if let Err(err) = state.auth.set_totp(auth_user.id, &secret, true) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    Json(json!({ "status": "ok", "totp_enabled": true })).into_response()
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
        return json_error_code(StatusCode::UNAUTHORIZED, "AUTH_USER_MISSING", "User not found");
    };

    if !verify_password(&hash, &body.password) {
        return json_error_code(StatusCode::BAD_REQUEST, "PASSWORD_WRONG", "Password is incorrect");
    }

    if let Err(err) = state.auth.set_totp(user.id, "", false) {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, err);
    }

    Json(json!({ "status": "ok", "totp_enabled": false })).into_response()
}

#[cfg(test)]
mod tests {
    use super::{origin_matches_host, normalize_authority};
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
        let map = headers(&[("host", "nas.local:4242"), ("origin", "http://evil.example")]);
        assert!(origin_matches_host(&map, &Method::GET));
        assert!(origin_matches_host(&map, &Method::HEAD));
    }

    #[test]
    fn csrf_accepts_same_origin() {
        let map = headers(&[("host", "nas.local:4242"), ("origin", "http://nas.local:4242")]);
        assert!(origin_matches_host(&map, &Method::POST));
    }

    #[test]
    fn csrf_rejects_cross_site_origin() {
        let map = headers(&[("host", "nas.local:4242"), ("origin", "https://evil.example")]);
        assert!(!origin_matches_host(&map, &Method::DELETE));
    }

    #[test]
    fn csrf_ignores_default_port_difference() {
        let map = headers(&[("host", "nas.local"), ("origin", "https://nas.local:443")]);
        assert!(origin_matches_host(&map, &Method::PUT));
    }

    #[test]
    fn csrf_uses_referer_when_origin_missing() {
        let map = headers(&[("host", "nas.local:4242"), ("referer", "http://nas.local:4242/files/m0/a")]);
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
}
