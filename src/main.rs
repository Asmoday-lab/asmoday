//! Asmoday — 个人媒体与设备控制中枢
//!
//! CLI 参数 > 环境变量 > config.toml（设计文档第 5 章）。

// axum 的处理器普遍返回 `Result<_, Response>`：`Response` 自带 HeaderMap + Body，
// 体积不小，于是每条 `Err` 分支都会触发 clippy::result_large_err。要消掉只能给
// 每个处理器包一层 `Box`，那是给**错误路径**多加一次堆分配，纯亏；社区常见做法
// 就是整 crate allow，把精力留给真正的 lint。
#![allow(clippy::result_large_err)]

// 全局分配器（设计文档 9.1：单二进制 + 低尾延迟）。
//
// 只换分配器、不改任何调用点：`mimalloc` 实现的就是 `GlobalAlloc`，所以 `Vec` /
// `String` / `Box` 全部照旧。musl 目标（静态链接）用系统分配器，原因见 Cargo.toml。
#[cfg(not(target_env = "musl"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod api;
mod core;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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
    /// 分享口令尝试限流（与登录池**完全隔离**，设计文档 3.1.5）
    pub share_guard: Arc<core::auth::LoginGuard>,
    /// WebDAV Basic Auth 尝试限流（与登录 / 分享池**互相隔离**，设计文档 v2.5.0 3.10）
    pub dav_guard: Arc<core::auth::LoginGuard>,
    /// per-path 并发锁（设计文档 3.2.8）
    pub path_locks: Arc<core::locks::PathLocks>,
    /// WebDAV 协议处理器（每个挂载点一套 LocalFs + MemLs，按配置指纹缓存）
    pub dav: Arc<api::webdav::DavCache>,
    /// 批量删除的一次性令牌
    pub delete_tickets: Arc<api::files_mutate::DeleteTickets>,
    /// 哈希并发槽位（上限 2，设计文档 6.2）
    pub hash_slots: Arc<tokio::sync::Semaphore>,
    /// 进程启动时刻（`/api/system/health` 的 uptime）
    pub started: Instant,
    /// 批量重建缩略图的进度
    pub rebuild: Arc<api::system::RebuildState>,
}

impl AppState {
    /// 打开一次 SQLite 连接（令牌表等轻量查询用）
    pub fn auth_connection(&self) -> Result<rusqlite::Connection, String> {
        rusqlite::Connection::open(self.auth.db_path()).map_err(|e| format!("打开数据库失败：{e}"))
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
    /// 强制覆盖监听地址（如 0.0.0.0 / 127.0.0.1）
    #[arg(short = 'H', long)]
    host: Option<String>,
    /// 生成默认配置与数据目录，并输出一次性管理员密码
    #[arg(long)]
    init: bool,
    /// 重置管理员密码，输出一次性新密码
    #[arg(long = "reset-password")]
    reset_password: bool,
    /// 清除 TOTP 配置并关闭 2FA（需要当前密码）
    #[arg(long = "reset-2fa")]
    reset_2fa: bool,
    /// 只校验配置文件语法与危险配置，不启动服务
    #[arg(long = "check-config")]
    check_config: bool,
    /// 把数据库热备份到指定文件后退出（不启动服务；服务在跑时也能执行）
    #[arg(long, value_name = "PATH")]
    backup: Option<PathBuf>,
}

async fn status() -> Json<Value> {
    Json(json!({ "status": "Asmoday is running" }))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(message) = run(cli).await {
        crate::log_error!("startup failed: {message}");
        eprintln!("[asmoday] startup failed: {message}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let started = Instant::now();

    // ---- 配置 ----
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(core::config::resolve_path);
    let (mut config, created) = core::config::load_or_create(&config_path).await?;
    if created {
        println!(
            "[asmoday] default config created: {}",
            config_path.display()
        );
    }
    if let Ok(host) = std::env::var("ASMODAY_HOST") {
        let host = host.trim().to_string();
        if !host.is_empty() {
            config.server.host = host;
        }
    }
    if let Some(host) = cli.host.as_deref() {
        let host = host.trim().to_string();
        if !host.is_empty() {
            config.server.host = host;
        }
    }
    if let Ok(port) = std::env::var("ASMODAY_PORT")
        && let Ok(port) = port.trim().parse::<u16>()
    {
        config.server.port = port;
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }

    // `--check-config`：只校验语法与危险配置，不碰数据库、不监听端口（设计文档 5）
    if cli.check_config {
        config.validate()?;
        let issues = config.safety_issues();
        for issue in &issues {
            println!("[asmoday] {} {}", issue.level.to_uppercase(), issue.code);
        }
        // 有 error 级问题就不能报 OK，且要用非零退出码（脚本/CI 依赖这个信号）
        let errors = issues.iter().filter(|issue| issue.level == "error").count();
        if errors > 0 {
            return Err(format!(
                "{} dangerous setting(s) in {}",
                errors,
                config_path.display()
            ));
        }
        println!("[asmoday] config OK: {}", config_path.display());
        return Ok(());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let data_dir = cli
        .data_dir
        .clone()
        .or_else(|| std::env::var("ASMODAY_DATA_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| cwd.join("data"));

    // ---- 运行日志（data/logs，10 MiB × 5 轮转）----
    core::logging::init(&data_dir);
    // `[log]` 分级策略（控制台 INFO / 文件 DEBUG / API 通道 / JSON，支持热更新）
    core::logging::apply(&config.log);
    crate::log_info!(
        "Asmoday {} starting (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    // ---- 危险配置自检（只 WARN，proxy_trust 无白名单时强制关闭）----
    config.apply_safety();

    // ---- 单实例锁 ----
    // 必须在打开数据库之前：`--init` / `--reset-password` 也会写库，同样不能并发。
    // 内存 per-path 锁与 `.asmoday-part` 启动清理都隐含「只有一个进程」的假设。
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create data dir {}: {e}", data_dir.display()))?;

    // `--backup PATH`：热备份后直接退出（设计文档 4.1）。
    // 刻意放在单实例锁**之前** —— 备份是只读操作，服务正在跑的时候也要能备份，
    // 否则「先停服再备份」意味着一次意外停机窗口。VACUUM INTO 走独立连接，
    // 与线上 WAL 的读写互不阻塞。
    if let Some(target) = cli.backup.clone() {
        let db_path = data_dir.join("asmoday.db");
        if !db_path.is_file() {
            return Err(format!("database not found: {}", db_path.display()));
        }
        let auth = AuthStore::new(db_path);
        let bytes = tokio::task::spawn_blocking(move || auth.backup_to(&target))
            .await
            .map_err(|e| format!("backup task failed: {e}"))??;
        println!(
            "[asmoday] database backed up to {} ({bytes} bytes)",
            cli.backup
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        return Ok(());
    }

    core::instance_lock::acquire(&data_dir)?;

    // ---- 数据库 / 认证 ----
    let auth = Arc::new(AuthStore::new(data_dir.join("asmoday.db")));
    auth.init()?;
    crate::log_info!(
        "database ready: {} ({})",
        data_dir.join("asmoday.db").display(),
        auth.describe()
    );

    // 启动时清理 30 天前的分享 tombstone（设计文档 4.1：不引入常驻定时任务）
    match auth.purge_share_tombstones(core::auth::now_secs()) {
        Ok(0) => {}
        Ok(count) => crate::log_info!("purged {count} expired share tombstones"),
        Err(err) => crate::log_warn!("failed to purge share tombstones: {err}"),
    }

    if cli.init {
        return bootstrap_admin(&auth, &data_dir).await;
    }
    if cli.reset_password {
        return reset_admin_password(&auth).await;
    }
    if cli.reset_2fa {
        return reset_two_factor(&auth).await;
    }

    // ---- 共享状态 ----
    let media = config.media.clone();
    let terminal = config.terminal.clone();
    let listen = (config.server.host.clone(), config.server.ssl.enabled);
    let state = AppState {
        config_path: config_path.clone(),
        data_dir: data_dir.clone(),
        config: Arc::new(RwLock::new(config)),
        thumbnails: Arc::new(Thumbnailer::new(data_dir.join("thumbnails"))),
        http: reqwest::Client::builder()
            .user_agent("Asmoday/0.1 (reverse-proxy)")
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?,
        terminals: TerminalManager::with_limits(
            terminal.max_sessions,
            std::time::Duration::from_secs(terminal.idle_kill_seconds),
        ),
        auth: Arc::clone(&auth),
        login_guard: Arc::new(core::auth::LoginGuard::new()),
        share_guard: Arc::new(core::auth::LoginGuard::new()),
        dav_guard: Arc::new(core::auth::LoginGuard::new()),
        path_locks: Arc::new(core::locks::PathLocks::new()),
        dav: Arc::new(api::webdav::DavCache::new()),
        delete_tickets: Arc::new(api::files_mutate::DeleteTickets::default()),
        hash_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        started,
        rebuild: Arc::new(api::system::RebuildState::default()),
        cwd,
    };

    state
        .thumbnails
        .apply_limits(media.limit_bytes(), media.max_age_secs());

    let port = state.config.read().await.server.port;
    let base_url = state.config.read().await.server.proxy.base_url.clone();

    // 子路径前缀（设计文档 3.1 / 4.2）：解析 base_url / X-Forwarded-Prefix，并在
    // 路由匹配**之前**把路径上的前缀剥掉。
    //
    // 必须包在 Router 外面：axum 的 `Router::layer` 是「路由之后」执行的中间件，
    // 在里面改 uri 只会影响处理器看到的路径，不会重新匹配路由 —— 表现就是
    // `/asmoday/api/...` 全部落到静态回落（回 `{"error":"接口不存在"}`）。
    let mut app = Router::new().fallback_service(tower::Layer::layer(
        &middleware::from_fn_with_state(state.clone(), api::prefix::apply),
        build_router(state.clone()),
    ));
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
    // `.asmoday-part` 残留清理：不阻塞启动（设计文档 3.2.6.4）
    spawn_part_cleanup(state.clone());

    // 监听地址：配置校验已保证是合法 IP 字面量（设计文档 v2.7.0 3.1.1 / 5）
    let host_ip: std::net::IpAddr = listen
        .0
        .parse()
        .map_err(|e| format!("invalid server.host {}: {e}", listen.0))?;
    let addr = SocketAddr::new(host_ip, port);
    let listener = bind_listener(addr).await?;

    crate::log_info!("config file: {}", config_path.display());
    crate::log_info!("data directory: {}", data_dir.display());
    if addr.ip().is_unspecified() {
        let stack = if addr.is_ipv6() {
            " (dual stack: IPv4 + IPv6)"
        } else {
            ""
        };
        crate::log_info!(
            "listening on {addr}{stack} (all interfaces; put a reverse proxy in front for public access)"
        );
        // 设计文档 v2.7.0 6.x：全接口监听（0.0.0.0 / ::）且未启用 SSL 必须输出规范 WARN
        if !listen.1 {
            crate::log_warn!(
                "listening on all interfaces without SSL. If you plan to expose this to the internet, please use a reverse proxy like Caddy/Nginx with HTTPS."
            );
        }
    } else {
        crate::log_info!("listening on {addr} (local interface only)");
    }
    crate::log_info!(
        "frontend build: {}",
        option_env!("ASMODAY_BUILD_ID").unwrap_or("dev")
    );
    if !base_url.is_empty() {
        crate::log_info!("base url prefix: {base_url}");
    }
    if auth.user_count().unwrap_or(0) == 0 {
        crate::log_warn!(
            "no administrator yet: open the web UI to finish setup, or run with --init"
        );
    }
    // 终端历史已强制隔离到 data/terminal_history/（设计文档 3.3.5），
    // 受限令牌不再影响终端体验，因此启动日志也不再提示权限问题。
    crate::log_info!("startup completed in {} ms", started.elapsed().as_millis());

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|e| format!("server exited with error: {e}"))?;

    state.terminals.shutdown();
    crate::log_info!("shutdown complete");
    core::logging::flush();
    Ok(())
}

/// 绑定监听 socket。
///
/// IPv6 地址必须显式关掉 `IPV6_V6ONLY`：Windows 默认值是 on，直接
/// `TcpListener::bind("[::]:p")` 只收 IPv6，IPv4 客户端全部拒连；
/// Linux 默认 off 但属于系统行为，不该赌。统一走 socket2 设置后，
/// `host = "::"` 即双栈（v4 连接的 peer 是 `::ffff:a.b.c.d`，由
/// `auth::peer_ip` 归一回 IPv4 字面量）。IPv4 路径行为与
/// `TcpListener::bind` 完全一致。
async fn bind_listener(addr: SocketAddr) -> Result<tokio::net::TcpListener, String> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .and_then(|socket| {
        if addr.is_ipv6() {
            socket.set_only_v6(false)?;
        }
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        Ok(socket)
    })
    .map_err(|e| format!("failed to bind {addr}: {e}"))?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| format!("failed to register listener for {addr}: {e}"))
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
        .route(
            "/api/public/share/{token}/download",
            get(api::shares::download),
        )
        .route(
            "/api/public/share/{token}/list",
            get(api::shares::list_entries),
        )
        .route(
            "/api/public/share/{token}/verify",
            post(api::shares::verify),
        )
}

/// 受保护路由（/api/* 与 /proxy/* 全量拦截）
fn protected_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/config",
            get(api::config::get_config).post(api::config::update_config),
        )
        // 危险配置自检结果（设置页警告条，设计文档 3.5 / 9.3）
        .route("/api/config/safety", get(api::config::get_safety))
        .route(
            "/api/config/raw",
            get(api::config::get_config_raw).post(api::config::update_config_raw),
        )
        .route("/api/auth/me", get(api::auth::me))
        .route("/api/system/health", get(api::system::health))
        // 数据库热备下载（设计文档 4.1）：高敏，需近期验证过密码
        .route("/api/system/backup", get(api::system::backup))
        .route("/api/system/ffmpeg-status", get(api::system::ffmpeg_status))
        .route(
            "/api/system/cleanup/thumbnails",
            post(api::system::cleanup_thumbnails),
        )
        .route(
            "/api/system/cleanup/parts",
            post(api::system::cleanup_parts),
        )
        .route(
            "/api/system/cleanup/shares",
            post(api::system::cleanup_shares),
        )
        .route(
            "/api/system/thumbnails/rebuild",
            post(api::system::rebuild_thumbnails),
        )
        .route(
            "/api/system/thumbnails/rebuild/status",
            get(api::system::rebuild_status),
        )
        .route("/api/system/test-ffmpeg", post(api::system::test_ffmpeg))
        .route("/api/auth/password", post(api::auth::change_password))
        // 敏感操作前的二次验证（终端 / 令牌 / 配置写操作要求 5 分钟内验证过）
        .route("/api/auth/reauth", get(api::auth::reauth_status))
        .route("/api/auth/verify", post(api::auth::verify_recent))
        .route("/api/auth/totp/setup", get(api::auth::totp_setup))
        .route("/api/auth/totp/enable", post(api::auth::totp_enable))
        .route("/api/auth/totp/disable", post(api::auth::totp_disable))
        .route("/api/files/mounts", get(api::files::mounts))
        .route("/api/files/roots", get(api::files::roots))
        .route("/api/files/list", get(api::files::list))
        // 文件变更（设计文档 6.2）：全部走「归属校验 → 只读拦截 → per-path 锁」
        .route("/api/files/mkdir", post(api::files_mutate::mkdir))
        .route("/api/files/touch", post(api::files_mutate::touch))
        .route("/api/files/rename", post(api::files_mutate::rename))
        .route("/api/files/move", post(api::files_mutate::move_entries))
        .route("/api/files/copy", post(api::files_mutate::copy_entries))
        .route(
            "/api/files/delete-token",
            post(api::files_mutate::delete_token),
        )
        .route("/api/files/delete", post(api::files_mutate::delete))
        .route("/api/files/measure", post(api::files_mutate::measure))
        .route("/api/files/stat", get(api::files_mutate::stat))
        .route("/api/files/hash", post(api::files_mutate::hash))
        .route(
            "/api/files/raw",
            get(api::files::raw)
                .put(api::files::write_text)
                .delete(api::files::delete),
        )
        .route("/api/files/text", get(api::files::read_text))
        .route("/api/files/thumb", get(api::media::thumb))
        .route("/api/files/upload", axum::routing::put(api::files::upload))
        .route("/api/files/search", get(api::files::search))
        .route("/api/files/subtitle", get(api::files::subtitle))
        .route("/api/media/browse", get(api::media::browse))
        .route("/api/media/albums", get(api::media::albums))
        .route("/api/media/cover", get(api::media::cover))
        .route("/api/media/progress", post(api::media::progress))
        .route("/api/media/history", get(api::media::history))
        .route("/api/media/favorite", post(api::media::favorite))
        .route("/api/media/favorites", get(api::media::favorites))
        .route("/api/media/cache", get(api::media::cache_stats))
        .route("/api/media/cache/clear", post(api::media::clear_cache))
        .route(
            "/api/shares",
            get(api::shares::list).post(api::shares::create),
        )
        .route(
            "/api/shares/{token}",
            axum::routing::delete(api::shares::delete),
        )
        .route("/api/terminal/ws", get(api::terminal::ws_handler))
        .route("/api/terminal/sessions", get(api::terminal::list_sessions))
        .route(
            "/api/terminal/sessions/{id}",
            axum::routing::delete(api::terminal::kill_session),
        )
        .route("/proxy/{service_id}", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/{*rest}", any(api::proxy::handle_rest))
        .route(
            "/api/tokens",
            get(api::tokens::list).post(api::tokens::create),
        )
        .route(
            "/api/tokens/{id}",
            axum::routing::delete(api::tokens::delete),
        )
}

/// WebDAV：独立入口，用 App Token 做 Basic 认证，不走 Cookie 中间件。
///
/// 用 `nest` 而不是 `route("/dav") + route("/dav/{*rest}")`：后者在 matchit 里
/// 匹配不到 `/dav` 与 `/dav/`（catch-all 段不能为空），而客户端添加服务器时
/// 第一个请求恰恰就是打到这两个地址的 `OPTIONS` / `PROPFIND`——404 就是
/// 「在客户端里加不上」的直接原因。`nest` 保证 `/dav`、`/dav/`、`/dav/x` 全进处理器。
fn dav_routes() -> Router<AppState> {
    Router::new()
        .route("/dav/", any(api::webdav::handle))
        .nest("/dav", Router::new().fallback(any(api::webdav::handle)))
}

fn build_router(state: AppState) -> Router {
    let protected = protected_routes().route_layer(middleware::from_fn_with_state(
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
        // HTTP 访问日志（设计文档 3.7：独立 api_level 通道，只记方法/路径/状态码/耗时/IP）
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::access::track,
        ))
        // 默认安全响应头（设计文档 3.1.9）：放在最外层，index.html 回落也要带上
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::security::security_headers,
        ))
        // 传输层压缩（设计文档 2.4）：最外层，前端产物 / JSON / SVG 都压。
        // 必须包在路由之后、安全头之外，才能把 index.html 回落也覆盖到。
        .layer(api::compress::layer())
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
                    crate::log_info!(
                        "thumbnail cache eviction freed {:.1} MiB",
                        bytes as f64 / 1_048_576.0
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => crate::log_warn!("thumbnail cache cleanup failed: {err}"),
                Err(_) => {}
            }
        }
    });
}

/// `--init`：确保有管理员账号，并打印一次性密码
async fn bootstrap_admin(auth: &AuthStore, data_dir: &Path) -> Result<(), String> {
    crate::log_info!("data directory: {}", data_dir.display());
    match auth.user_count() {
        Ok(0) => {}
        Ok(_) => {
            crate::log_info!(
                "administrator already exists; --init does not overwrite the password"
            );
            return Ok(());
        }
        Err(err) => return Err(err),
    }

    let password = core::auth::random_password();
    let user = auth.create_user("admin", &password)?;
    auth.set_must_change_password(user.id, true)?;
    crate::log_info!("administrator created: {}", user.username);
    crate::log_warn!("one-time password (must be changed after login): {password}");
    Ok(())
}

/// `--reset-password`：重置管理员密码。
///
/// 密码来源二选一：
/// 1. 环境变量 `ASMODAY_RESET_PASSWORD`——**只在带 `--reset-password` 启动时生效**，
///    且同一个值只能用一次（指纹存库，重复出现拒绝并记 WARN，设计文档 3.1.2）；
/// 2. 未提供环境变量时随机生成一次性密码并打印。
async fn reset_admin_password(auth: &AuthStore) -> Result<(), String> {
    let Some(user) = auth.first_user()? else {
        return Err("no user yet; run with --init first".to_string());
    };
    auth.invalidate_sessions(user.id)?;

    let env_password = std::env::var("ASMODAY_RESET_PASSWORD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match env_password {
        Some(password) => {
            let fingerprint = core::auth::fingerprint(&password);
            if auth.setting("reset_password_fingerprint")?.as_deref() == Some(&fingerprint) {
                crate::log_warn!(
                    "ASMODAY_RESET_PASSWORD is unchanged since the last reset; refusing to apply again (remove the variable or change its value)"
                );
                return Ok(());
            }
            check_password_strength(&password)?;
            auth.update_password(user.id, &password)?;
            auth.set_must_change_password(user.id, false)?;
            auth.put_setting("reset_password_fingerprint", &fingerprint)?;
            // 日志只证明「重置成功」，绝不打印密码
            crate::log_info!("password reset from ASMODAY_RESET_PASSWORD");
            println!("[asmoday] password reset (value came from ASMODAY_RESET_PASSWORD)");
        }
        None => {
            let password = core::auth::random_password();
            auth.update_password(user.id, &password)?;
            auth.set_must_change_password(user.id, true)?;
            crate::log_info!("password reset for {}", user.username);
            crate::log_warn!("one-time password (must be changed after login): {password}");
        }
    }
    Ok(())
}

/// `--reset-2fa`：清除 TOTP 配置（需要当前密码，防止可用性死锁）
async fn reset_two_factor(auth: &AuthStore) -> Result<(), String> {
    let Some(user) = auth.first_user()? else {
        return Err("no user yet; run with --init first".to_string());
    };
    println!("[asmoday] reset 2FA for user '{}'", user.username);
    let Some((_, hash, _)) = auth.find_user(&user.username)? else {
        return Err("user row not found".to_string());
    };
    let password = prompt_line("current password")?;
    if !core::auth::verify_password(&hash, &password) {
        return Err("wrong password, 2FA not reset".to_string());
    }
    auth.clear_totp(user.id)?;
    auth.invalidate_sessions(user.id)?;
    crate::log_info!("2FA disabled for {}", user.username);
    println!("[asmoday] 2FA disabled");
    Ok(())
}

/// 读取一行标准输入（不会回显，适合密码）
fn prompt_line(label: &str) -> Result<String, String> {
    use std::io::{BufRead, Write};
    print!("{label}: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read stdin: {e}"))?;
    let value = line.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() {
        return Err("empty input".to_string());
    }
    Ok(value)
}

/// CLI 重置时的最低强度要求（与网页注册保持一致：至少 8 位且不全是数字）
fn check_password_strength(password: &str) -> Result<(), String> {
    if password.chars().count() < 8 || password.chars().all(|c| c.is_ascii_digit()) {
        return Err("password must be at least 8 characters and not all digits".to_string());
    }
    Ok(())
}

/// `.asmoday-part` 启动清理：后台执行，失败只记 WARN，绝不阻止服务起来
fn spawn_part_cleanup(state: AppState) {
    tokio::spawn(async move {
        let mounts = {
            let config = state.config.read().await;
            core::path_guard::load(&config, &state.cwd)
        };
        for mount in mounts.mounts {
            let Some(root) = mount.root else { continue };
            let label = mount.info.name.clone();
            match tokio::task::spawn_blocking(move || core::fs_ops::cleanup_parts(&root)).await {
                Ok((0, _)) => {}
                Ok((removed, freed)) => crate::log_info!(
                    "removed {removed} leftover .asmoday-part files ({:.1} MiB) under mount '{label}'",
                    freed as f64 / 1_048_576.0
                ),
                Err(err) => crate::log_warn!("part cleanup failed for mount '{label}': {err}"),
            }
        }
    });
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

    crate::log_info!("shutdown signal received, stopping gracefully");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_and_validates() {
        let config: Config =
            toml::from_str(core::config::DEFAULT_CONFIG_TOML).expect("默认配置必须能被自己解析");
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
