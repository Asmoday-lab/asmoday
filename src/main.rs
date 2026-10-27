//! Asmoday — 个人媒体与设备控制中枢
//!
//! CLI 参数 > 环境变量 > config.toml（设计文档第 5 章）。

mod api;
mod core;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use clap::Parser;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::core::auth::AuthStore;
use crate::core::config::Config;
use crate::core::terminal::TerminalManager;
use crate::core::thumbnail::Thumbnailer;

/// 全应用共享状态。
#[derive(Clone)]
pub struct AppState {
    /// 配置文件路径（config.toml）
    pub config_path: PathBuf,
    /// 数据目录（SQLite / 缩略图缓存）
    pub data_dir: PathBuf,
    /// 内存中的当前配置，POST /api/config 时热更新
    pub config: Arc<RwLock<Config>>,
    /// 进程启动时的工作目录：未配置挂载点时作为兜底可浏览根
    pub cwd: PathBuf,
    /// PTY 会话池
    pub terminals: TerminalManager,
    /// 缩略图生成器（信号量 + 磁盘缓存）
    pub thumbnails: Arc<Thumbnailer>,
    /// 反代网关的 HTTP 客户端（连接池复用）
    pub http: reqwest::Client,
    /// 用户/会话存储（SQLite）
    pub auth: Arc<AuthStore>,
    /// 登录失败限流
    pub login_guard: Arc<core::auth::LoginGuard>,
}

impl AppState {
    /// 打开一次 SQLite 连接（令牌表等轻量查询用）
    pub fn auth_connection(&self) -> Result<rusqlite::Connection, String> {
        rusqlite::Connection::open(self.auth.db_path())
            .map_err(|e| format!("打开数据库失败：{e}"))
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "asmoday",
    version,
    about = "Asmoday — 个人媒体与设备控制中枢（单二进制）"
)]
struct Cli {
    /// 指定配置文件路径
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// 指定运行数据目录（SQLite / 缓存）
    #[arg(short = 'd', long)]
    data_dir: Option<PathBuf>,
    /// 强制覆盖监听端口
    #[arg(short, long)]
    port: Option<u16>,
    /// 生成默认配置与数据目录，并输出一次性管理员密码
    #[arg(long)]
    init: bool,
    /// 重置管理员密码，输出一次性新密码
    #[arg(long = "reset-password")]
    reset_password: bool,
}

async fn status() -> Json<Value> {
    Json(json!({ "status": "Asmoday is running" }))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(message) = run(cli).await {
        core::logging::error(&format!("启动失败：{message}"));
        eprintln!("[asmoday] 启动失败：{message}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    // ---- 配置 ----
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(core::config::resolve_path);
    let (mut config, created) = core::config::load_or_create(&config_path).await?;
    if created {
        println!("[asmoday] 未找到配置，已生成默认配置：{}", config_path.display());
    }
    if let Ok(port) = std::env::var("ASMODAY_PORT") {
        if let Ok(port) = port.trim().parse::<u16>() {
            config.server.port = port;
        }
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let data_dir = cli
        .data_dir
        .clone()
        .or_else(|| std::env::var("ASMODAY_DATA_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| cwd.join("data"));

    // ---- 运行日志（data/logs，10 MiB × 5 轮转）----
    core::logging::init(&data_dir);
    core::logging::info(&format!("Asmoday {} 启动", env!("CARGO_PKG_VERSION")));

    // ---- 数据库 / 认证 ----
    let auth = Arc::new(AuthStore::new(data_dir.join("asmoday.db")));
    auth.init()?;

    // Docker / 脚本场景：ASMODAY_RESET_PASSWORD 直接指定新密码（证明系统级权限）
    if let Ok(raw) = std::env::var("ASMODAY_RESET_PASSWORD") {
        let password = raw.trim();
        if !password.is_empty() {
            let Some(user) = auth.first_user()? else {
                return Err("还没有任何用户，请先用 --init 初始化".to_string());
            };
            auth.update_password(user.id, password)?;
            core::logging::warn("已通过 ASMODAY_RESET_PASSWORD 重置管理员密码（请及时删除该环境变量）");
        }
    }

    if cli.init {
        return bootstrap_admin(&auth, &data_dir).await;
    }

    if cli.reset_password {
        return reset_admin_password(&auth).await;
    }

    // ---- 共享状态 ----
    let media = config.media.clone();
    let state = AppState {
        config_path: config_path.clone(),
        data_dir: data_dir.clone(),
        config: Arc::new(RwLock::new(config)),
        thumbnails: Arc::new(Thumbnailer::new(data_dir.join("thumbnails"))),
        http: reqwest::Client::builder()
            .user_agent("Asmoday/0.1 (reverse-proxy)")
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?,
        terminals: TerminalManager::new(),
        auth: Arc::clone(&auth),
        login_guard: Arc::new(core::auth::LoginGuard::new()),
        cwd,
    };

    state
        .thumbnails
        .apply_limits(media.limit_bytes(), media.max_age_secs());

    let port = state.config.read().await.server.port;
    let base_url = state.config.read().await.server.proxy.base_url.clone();

    let mut app = build_router(state.clone());
    // 子路径部署：整站挂到 base_url 下，根路径跳转到前缀（设计文档 3.1 / 4.2）
    if !base_url.is_empty() {
        let base = base_url.clone();
        // 规范形式不带尾斜杠：axum 的 nest 只认 "/base" 与 "/base/*rest"，
        // "/base/" 落不到内层，所以这里把尾斜杠也重定向过去。
        let root_base = base.clone();
        app = Router::new()
            .route(
                "/",
                get(move || {
                    let base = root_base.clone();
                    async move { axum::response::Redirect::temporary(&base) }
                }),
            )
            .nest(&base, app)
            .fallback(move |uri: axum::http::Uri| {
                let base = base.clone();
                async move {
                    let path = uri.path();
                    if path.len() > 1 && path.trim_end_matches('/') == base {
                        return axum::response::Redirect::temporary(&base).into_response();
                    }
                    (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response()
                }
            });
    }
    // config.toml 热重载（设计文档 3.5）
    core::config::spawn_watcher(state.clone());
    // 缩略图缓存后台淘汰（容量 + 过期天数，设计文档 3.4）
    spawn_media_janitor(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {port} 监听失败：{e}"))?;

    core::logging::info(&format!("配置：{}", config_path.display()));
    core::logging::info(&format!("数据：{}", data_dir.display()));
    core::logging::info(&format!("监听：http://127.0.0.1:{port}"));
    core::logging::info(&format!(
        "内嵌前端资源：{} 个文件",
        api::static_files::embedded_files()
    ));
    if !base_url.is_empty() {
        core::logging::info(&format!("子路径前缀：{base_url}"));
    }
    if auth.user_count().unwrap_or(0) == 0 {
        core::logging::warn("尚未初始化管理员：请打开网页完成初始化，或使用 --init 生成一次性密码");
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|e| format!("服务异常退出：{e}"))?;

    state.terminals.shutdown();
    Ok(())
}

/// 公开路由（无需登录）
fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(status))
        .route("/api/auth/status", get(api::auth::status))
        .route("/api/auth/setup", post(api::auth::setup))
        .route("/api/auth/login", post(api::auth::login))
        .route("/api/auth/logout", post(api::auth::logout))
        // 公开分享（令牌/口令/有效期/防盗链由处理器自行校验）
        .route("/api/public/share/{token}", get(api::shares::info))
        .route("/api/public/share/{token}/raw", get(api::shares::raw))
        .route("/api/public/share/{token}/download", get(api::shares::download))
        .route("/api/public/share/{token}/list", get(api::shares::list_entries))
}

/// 受保护路由（/api/* 与 /proxy/* 全量拦截）
fn protected_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/config",
            get(api::config::get_config).post(api::config::update_config),
        )
        .route(
            "/api/config/raw",
            get(api::config::get_config_raw).post(api::config::update_config_raw),
        )
        .route("/api/auth/me", get(api::auth::me))
        .route("/api/system/ffmpeg-status", get(api::system::ffmpeg_status))
        .route("/api/system/test-ffmpeg", post(api::system::test_ffmpeg))
        .route("/api/auth/password", post(api::auth::change_password))
        .route("/api/auth/totp/setup", get(api::auth::totp_setup))
        .route("/api/auth/totp/enable", post(api::auth::totp_enable))
        .route("/api/auth/totp/disable", post(api::auth::totp_disable))
        .route("/api/files/roots", get(api::files::roots))
        .route("/api/files/list", get(api::files::list))
        .route(
            "/api/files/raw",
            get(api::files::raw)
                .put(api::files::write_text)
                .delete(api::files::delete),
        )
        .route("/api/files/thumb", get(api::media::thumb))
        .route(
            "/api/files/upload",
            axum::routing::put(api::files::upload),
        )
        .route("/api/files/search", get(api::files::search))
        .route("/api/files/subtitle", get(api::files::subtitle))
        .route("/api/media/browse", get(api::media::browse))
        .route("/api/media/cover", get(api::media::cover))
        .route("/api/media/progress", post(api::media::progress))
        .route("/api/media/history", get(api::media::history))
        .route("/api/media/favorite", post(api::media::favorite))
        .route("/api/media/favorites", get(api::media::favorites))
        .route("/api/media/cache", get(api::media::cache_stats))
        .route("/api/media/cache/clear", post(api::media::clear_cache))
        .route("/api/shares", get(api::shares::list).post(api::shares::create))
        .route("/api/shares/{token}", axum::routing::delete(api::shares::delete))
        .route("/api/terminal/ws", get(api::terminal::ws_handler))
        .route("/api/terminal/sessions", get(api::terminal::list_sessions))
        .route("/proxy/{service_id}", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/{*rest}", any(api::proxy::handle_rest))
        .route("/api/tokens", get(api::tokens::list).post(api::tokens::create))
        .route("/api/tokens/{id}", axum::routing::delete(api::tokens::delete))
}

/// WebDAV：独立入口，用 App Token 做 Basic 认证，不走 Cookie 中间件
fn dav_routes() -> Router<AppState> {
    Router::new()
        .route("/dav", any(api::webdav::handle))
        .route("/dav/{*rest}", any(api::webdav::handle))
}

fn build_router(state: AppState) -> Router {
    let protected = protected_routes()
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api::auth::require_auth,
        ));

    public_routes()
        .merge(protected)
        // WebDAV 自带 Basic 认证，不经 Cookie 中间件
        .merge(dav_routes())
        // 上传/代理大文件：放宽请求体上限
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .fallback(api::static_files::serve)
        .with_state(state)
}

/// 缩略图缓存后台淘汰：每 30 分钟按 `[media]` 策略清理一次。
fn spawn_media_janitor(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        // 第一次 tick 会立即返回，跳过它，避免刚启动就扫盘
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let thumbnails = Arc::clone(&state.thumbnails);
            let config = state.config.read().await.media.clone();
            thumbnails.apply_limits(config.limit_bytes(), config.max_age_secs());
            let cleaned = tokio::task::spawn_blocking(move || thumbnails.run_cleanup()).await;
            match cleaned {
                Ok(Ok(bytes)) if bytes > 0 => {
                    core::logging::info(&format!(
                        "缩略图缓存淘汰完成，释放 {:.1} MiB",
                        bytes as f64 / 1_048_576.0
                    ));
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => core::logging::warn(&format!("缩略图缓存清理失败：{err}")),
                Err(_) => {}
            }
        }
    });
}

/// `--init`：确保有管理员账号，并打印一次性密码
async fn bootstrap_admin(auth: &AuthStore, data_dir: &PathBuf) -> Result<(), String> {
    core::logging::info(&format!("数据目录：{}", data_dir.display()));
    match auth.user_count() {
        Ok(0) => {}
        Ok(_) => {
            core::logging::info("已存在管理员账号，--init 不会覆盖密码");
            return Ok(());
        }
        Err(err) => return Err(err),
    }

    let password = core::auth::random_password();
    let user = auth.create_user("admin", &password)?;
    auth.set_must_change_password(user.id, true)?;
    core::logging::info(&format!("已创建管理员：{}", user.username));
    core::logging::warn(&format!("一次性初始密码（登录后必须修改）：{password}"));
    Ok(())
}

/// `--reset-password`：重置管理员密码并打印新密码
async fn reset_admin_password(auth: &AuthStore) -> Result<(), String> {
    let Some(user) = auth.first_user()? else {
        return Err("还没有任何用户，请先用 --init 初始化".to_string());
    };

    let password = core::auth::random_password();
    auth.update_password(user.id, &password)?;
    // 重置出来的是一次性密码，登录后强制修改
    auth.set_must_change_password(user.id, true)?;
    core::logging::info(&format!("已重置 {} 的密码", user.username));
    core::logging::warn(&format!("新密码（登录后必须修改）：{password}"));
    Ok(())
}

/// Ctrl+C / SIGTERM 时优雅退出。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    println!("[asmoday] 收到退出信号，正在清理 PTY 会话…");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_and_validates() {
        let config: Config = toml::from_str(core::config::DEFAULT_CONFIG_TOML)
            .expect("默认配置必须能被自己解析");
        config.validate().expect("默认配置必须通过校验");
        assert_eq!(config.server.port, 4242);
        assert!(config.system.mounts.is_empty());
        assert!(config.services.is_empty());
        assert!(config.auth.local_enabled);
    }

    #[test]
    fn config_roundtrip_keeps_semantics() {
        let config: Config = toml::from_str(core::config::DEFAULT_CONFIG_TOML).unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        let reparsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(config, reparsed);
    }

    #[test]
    fn auth_cannot_be_fully_disabled() {
        let mut config: Config = toml::from_str(core::config::DEFAULT_CONFIG_TOML).unwrap();
        config.auth.local_enabled = false;
        config.auth.proxy_trust.enabled = false;
        assert!(config.validate().is_err());

        config.auth.proxy_trust.enabled = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // 红线：不存在“关闭认证”的隐藏开关，未知字段一律拒绝
        let bad = r#"
[server]
port = 4242

[auth]
local_enabled = true
disable_auth = true
"#;
        assert!(toml::from_str::<Config>(bad).is_err());
    }

    #[test]
    fn mounts_and_services_roundtrip() {
        let source = r#"
[server]
port = 4242

[[system.mounts]]
name = "Local Storage"
path = "/mnt/nas"

[[services]]
id = "grafana"
name = "监控面板"
type = "external"
target_url = "http://192.168.1.50:3000"

[services.inject_headers]
"X-WEBAUTH-USER" = "admin"
"#;
        let config: Config = toml::from_str(source).expect("合法配置应可解析");
        config.validate().expect("合法配置应通过校验");

        let text = toml::to_string_pretty(&config).expect("应可序列化");
        let reparsed: Config = toml::from_str(&text).expect("自身产物必须可被重新解析");

        assert_eq!(config, reparsed);
        assert_eq!(
            reparsed.services[0]
                .inject_headers
                .get("X-WEBAUTH-USER")
                .map(String::as_str),
            Some("admin")
        );
        assert_eq!(reparsed.system.mounts[0].path, "/mnt/nas");
    }
}
