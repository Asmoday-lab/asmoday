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
use axum::routing::{any, delete, get, post};
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
    /// 音乐封面缓存（v4.9.21）：SMB 上每读一次 ID3 要几秒，而封面几乎不变
    pub covers: Arc<core::cover_cache::CoverCache>,
    /// 挂载点集合缓存（v4.9.21）：`path_guard::load` 每次都要对每个挂载点做
    /// canonicalize + is_dir，网络盘上一次几百毫秒；前端 10 秒一轮轮询，不缓存
    /// 就等于常驻一条 SMB 往返。
    pub mounts_cache: Arc<tokio::sync::Mutex<Option<api::files::MountsCache>>>,
    /// 反代网关的 HTTP 客户端（连接池复用）
    pub http: reqwest::Client,
    /// 用户/会话存储（SQLite）
    pub auth: Arc<AuthStore>,
    /// 目录快照缓存（SQLite，递归搜索用；见 core/dir_cache.rs）
    pub dir_cache: Arc<core::dir_cache::DirCache>,
    /// 媒体索引（SQLite，媒体库分页 / 计数 / 相册聚合；见 core/media_index.rs）
    pub media_index: Arc<core::media_index::MediaIndex>,
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
    /// aria2 兼容下载引擎（设计文档 3.10，v4.9.26）
    pub downloads: Arc<core::download::DownloadManager>,
    /// tus 分块上传会话表（设计文档 3.2.3.1）
    pub tus: Arc<api::upload_tus::TusStore>,
    /// 哈希并发槽位（上限 2，设计文档 6.2）
    pub hash_slots: Arc<tokio::sync::Semaphore>,
    /// 进程启动时刻（`/api/system/health` 的 uptime）
    pub started: Instant,
    /// 批量重建缩略图的进度
    pub rebuild: Arc<api::system::RebuildState>,
    /// 预生成缩略图的进度（v4.9.11）：与「重建」分开，两者语义不同
    pub preheat: Arc<api::system::RebuildState>,
    /// 缩略图后台任务的互斥位（v4.9.12）：预生成与全量重建一次只跑一个
    pub thumb_jobs: Arc<api::system::ThumbJobSlot>,
    /// 挂载点版本号：配置里 `system.mounts` 每变一次 +1。
    ///
    /// 前端拿它判断「后端认识的那套挂载点是不是已经换了」—— 配置热重载后浏览器
    /// 自己不知道，不通知就只能靠用户手动刷新页面（v4.9.2）。
    pub mounts_revision: Arc<std::sync::atomic::AtomicU64>,
    /// 配置变更信号：后台索引器在 sleep 里 select 它，改完挂载点不必再等一整轮
    pub config_changed: Arc<tokio::sync::Notify>,
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
    about = "Asmoday - personal media and device hub (single binary)"
)]
struct Cli {
    /// Path to the config file
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Directory for runtime data (SQLite / cache)
    #[arg(short = 'd', long)]
    data_dir: Option<PathBuf>,
    /// Override the listening port
    #[arg(short, long)]
    port: Option<u16>,
    /// Override the listening address (0.0.0.0 / 127.0.0.1 / :: / auto)
    #[arg(short = 'H', long)]
    host: Option<String>,
    /// Write the default config file and exit (no server, no database)
    #[arg(long)]
    init: bool,
    /// Reset the admin password and print a new one-time password
    #[arg(long = "reset-password")]
    reset_password: bool,
    /// Clear TOTP settings and disable 2FA (current password required)
    #[arg(long = "reset-2fa")]
    reset_2fa: bool,
    /// Validate config syntax and safety issues, then exit without starting the server
    #[arg(long = "check-config")]
    check_config: bool,
    /// Hot-backup the database to PATH and exit (works while the service is running)
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

    // panic hook（2026-11-04 审查）：panic=unwind 后单连接可恢复，
    // 但没有 hook 的话 panic 是无声的——客户端只看到连接重置，日志里一片空白。
    // 打进 error 通道（含文件），keep-alive 的连接不背这个锅，谁 panic 谁自己 500。
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        crate::log_error!("panic at {location}: {message}");
    }));

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

    // `--init`：只写默认配置，尽早退出（v4.9.63）。
    // 以前它会建库、建 admin、打一次性密码横幅和 Open 地址，但服务根本没监听：
    // 地址是死的，那串密码在下次正常启动时还会被「待改密补发」直接顶掉作废。
    // 首启引导本来就在正常启动路径里，凭证挪到那时发，地址与密码才都是真的。
    if cli.init {
        if !created {
            println!("[asmoday] config already exists: {}", config_path.display());
        }
        println!(
            "[asmoday] config ready; start asmoday without --init to create the data dir and print the one-time admin password"
        );
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
    // 必须在打开数据库之前：`--reset-password` 也要写库，同样不能并发。
    // 内存 per-path 锁与 `.asmoday-part` 启动清理都隐含「只有一个进程」的假设。
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create data dir {}: {e}", data_dir.display()))?;

    // 应用数据目录（SQLite / 日志 / 缩略图缓存）登记为「保留目录」：
    // 列表里不出现，接口一律 403。默认挂载点是 cwd、默认数据目录是 cwd/data，
    // 两者天生重叠 —— 不拦的话缩略图缓存会作为普通图片被再次缩略图，
    // 产物写回同一目录，下一轮又成新图片，按宽度倍数指数增殖。
    core::reserved::register(&data_dir);

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

    // 物理删除多余账号（迁移 v19）之后，终端历史目录 `terminal_history/u<id>` 也要
    // 跟着清掉，否则被删账号的 shell 历史会一直躺在 data 目录里（v4.9.29）
    if let Ok(Some(owner)) = auth.first_user() {
        let removed = core::terminal::purge_orphan_history(&data_dir, owner.id);
        if removed > 0 {
            crate::log_info!("purged {removed} terminal history dirs of removed accounts");
        }
    }

    // 启动时清理 30 天前的分享 tombstone（设计文档 4.1：不引入常驻定时任务）
    match auth.purge_share_tombstones(core::auth::now_secs()) {
        Ok(0) => {}
        Ok(count) => crate::log_info!("purged {count} expired share tombstones"),
        Err(err) => crate::log_warn!("failed to purge share tombstones: {err}"),
    }

    // 控制台入口地址：一次性密码横幅与启动日志共用同一份（v4.9.42）。
    // 只有真正起服务的路径才用得上；`--reset-password` 不起服务，横幅里不带地址。
    let urls = access_urls(&config.server);

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

    // aria2 兼容下载引擎（设计文档 3.10）：RPC 密钥由**数据库**持有
    // （`settings.download_rpc_secret`，v4.9.36），前端「刷新密钥」写库 +
    // 换内存，立即生效且重启后仍是新值。以前是 `<data>/download_rpc_secret`
    // 文件——文件与内存两份真相，前端既改不了也删不干净，已废弃。
    // `[download].rpc_secret` 只保留一次性种子语义：DB 里还没有值时才导入，
    // 之后一律以数据库为准（配置值不再有任何效果，只 WARN 一次）。
    let download_config = config.download.clone();
    let download_secret = {
        const KEY: &str = core::download::RPC_SECRET_SETTING;
        match auth
            .setting(KEY)
            .map_err(|e| format!("read download rpc secret failed: {e}"))?
        {
            Some(secret) if !secret.trim().is_empty() => secret,
            _ => {
                let seeded = download_config.rpc_secret.trim();
                let secret = if seeded.is_empty() {
                    core::auth::random_password()
                } else {
                    crate::log_warn!(
                        "[download].rpc_secret is deprecated: importing the configured value into \
                         the database once; rotate it from the downloads page from now on"
                    );
                    seeded.to_string()
                };
                auth.put_setting(KEY, &secret)
                    .map_err(|e| format!("persist download rpc secret failed: {e}"))?;
                secret
            }
        }
    };
    // 明文不进日志/控制台：密钥只在前端下载页可见（可显隐、可复制、可刷新）
    crate::log_info!(
        "download RPC secret loaded from database (view or rotate it in the downloads page)"
    );
    // 下载落盘目录（v4.9.37）：数据库存「挂载点 id + 子路径」，启动时解析成
    // 绝对路径给引擎。为什么不再用裸绝对路径：下载目录必须是文件管理里看得见的
    // 目录，否则下完的文件在 asmoday 里找不到；挂载点已经是「可写 + 受管」的
    // 权威清单，取它的子路径同时继承只读标记与掉线状态。
    //
    // 页面上改这个值**不需要重启**：`POST /api/downloads/roots` 写库 + 热换内存。
    // `[download].roots` 只在库里还没有值时当一次性种子（能映射到挂载点的才导入）。
    let mounts_at_start = core::path_guard::load(&config, &cwd);
    let download_roots: Vec<core::download::DownloadRoot> = {
        const KEY: &str = core::download::ROOTS_SETTING;
        let stored = auth
            .setting(KEY)
            .map_err(|e| format!("read download roots failed: {e}"))?;
        let specs: Vec<core::download::RootSpec> =
            match stored.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(json) => serde_json::from_str(json)
                    .map_err(|e| format!("stored {KEY} is not valid JSON: {e}"))?,
                None => {
                    let (mut seeded, warnings) =
                        core::download::seed_root_specs(&download_config.roots, &mounts_at_start);
                    for warning in &warnings {
                        crate::log_warn!("[download].roots: {warning}");
                    }
                    if seeded.is_empty()
                        && let Some(fallback) = core::download::default_root_spec(&mounts_at_start)
                    {
                        seeded.push(fallback);
                    }
                    let json = serde_json::to_string(&seeded)
                        .map_err(|e| format!("encode download roots failed: {e}"))?;
                    auth.put_setting(KEY, &json)
                        .map_err(|e| format!("persist download roots failed: {e}"))?;
                    seeded
                }
            };
        let (roots, warnings) = core::download::resolve_roots(&mounts_at_start, &specs);
        for warning in &warnings {
            crate::log_warn!("download root skipped: {warning}");
        }
        if roots.is_empty() {
            crate::log_warn!(
                "no usable download directory: configure one in the downloads page \
                 (needs an online, writable mount point)"
            );
        }
        for root in &roots {
            if let Err(err) = std::fs::create_dir_all(&root.path) {
                crate::log_warn!("create download root {} failed: {err}", root.path.display());
            }
        }
        roots
    };

    // BitTorrent 会话（v4.9.27）：DHT / 入站监听都在这里起。
    // 起不来只降级成 HTTP-only，不让整个服务跟着挂 —— BT 是附加能力。
    let bt_engine = if download_config.enable && download_config.enable_bt {
        // BT 会话需要一个具体的下载目录（librqbit 的 session 默认路径），取
        // 白名单里第一个在线条目；一个都没有时退回 `<data>/downloads` 只为让
        // 会话能起来，实际 add 会被 `resolve_dir` 拒绝并提示去页面配目录。
        let default_root = download_roots
            .iter()
            .find(|root| root.online)
            .map(|root| root.path.clone())
            .unwrap_or_else(|| data_dir.join("downloads"));
        match core::bt::BtEngine::start(
            default_root,
            &data_dir,
            download_config.bt_port_range(),
            download_config.bt_disable_dht,
        )
        .await
        {
            Ok(engine) => {
                match engine.listen_port() {
                    Some(port) => crate::log_info!("bittorrent enabled; listening on tcp/{port}"),
                    None => crate::log_info!("bittorrent enabled; outbound peers only"),
                }
                if download_config.bt_disable_dht {
                    crate::log_info!("bittorrent DHT disabled (tracker-only mode)");
                }
                Some(engine)
            }
            Err(err) => {
                crate::log_warn!("bittorrent disabled: {err}");
                None
            }
        }
    } else {
        None
    };

    // BT Tracker 列表（v4.9.39）：数据库是唯一真相，`[download].bt_trackers` 只在
    // 库里还没有值时当一次性种子。语义对齐 aria2 的 `bt-tracker`——追加到每个**新**
    // 任务的 announce 列表；改完立即对之后添加的种子生效，运行中的种子不追溯
    // （librqbit 在 add_torrent 时就把 tracker 轮询流建好了）。
    let download_trackers: Vec<String> = {
        const KEY: &str = core::download::TRACKERS_SETTING;
        let stored = auth
            .setting(KEY)
            .map_err(|e| format!("read download trackers failed: {e}"))?;
        match stored.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(json) => serde_json::from_str(json)
                .map_err(|e| format!("stored {KEY} is not valid JSON: {e}"))?,
            None => {
                let update = core::download::normalize_trackers(&download_config.bt_trackers);
                for rejected in &update.rejected {
                    crate::log_warn!("[download].bt_trackers: dropped {rejected}");
                }
                if !download_config.bt_trackers.is_empty() {
                    crate::log_warn!(
                        "[download].bt_trackers is deprecated: imported into the database once; \
                         edit the list on the download engine page from now on"
                    );
                }
                let json = serde_json::to_string(&update.trackers)
                    .map_err(|e| format!("encode download trackers failed: {e}"))?;
                auth.put_setting(KEY, &json)
                    .map_err(|e| format!("persist download trackers failed: {e}"))?;
                update.trackers
            }
        }
    };

    // 缩略图缓存根：缩略器与索引共用同一个串（索引靠它回收外部删除的缓存单元）
    let thumb_dir = data_dir.join("thumbnails");
    let downloads = Arc::new(core::download::DownloadManager::new(
        // 重定向由 download::send_following 自己跟：reqwest 的自动跟随会在
        // 跨源跳转时丢掉 Cookie / Referer / Authorization（插件传下来的鉴权头
        // 就白给了），而 aria2 的 `--header` 是每一跳原样重发的
        reqwest::Client::builder()
            .user_agent("aria2/1.37.0")
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("failed to build download HTTP client: {e}"))?,
        download_secret,
        download_roots,
        download_config.segments,
        download_config.max_concurrent,
        download_config.enable,
        bt_engine,
    ));
    // 启动时装载 Tracker 列表（之后改列表走页面 / RPC，不重启）
    downloads.replace_trackers(download_trackers);

    let state = AppState {
        config_path: config_path.clone(),
        data_dir: data_dir.clone(),
        config: Arc::new(RwLock::new(config)),
        thumbnails: Arc::new(Thumbnailer::new(thumb_dir.clone())),
        covers: Arc::new(core::cover_cache::CoverCache::new(
            96 * 1024 * 1024,
            4096,
            4,
        )),
        mounts_cache: Arc::new(tokio::sync::Mutex::new(None)),
        http: reqwest::Client::builder()
            .user_agent("Asmoday/0.1 (reverse-proxy)")
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?,
        terminals: TerminalManager::with_limits(
            terminal.max_sessions,
            std::time::Duration::from_secs(terminal.idle_kill_seconds),
        ),
        auth: Arc::clone(&auth),
        dir_cache: Arc::new(core::dir_cache::DirCache::new(auth.db_path().clone())),
        media_index: Arc::new(
            core::media_index::MediaIndex::new(auth.db_path().clone())
                .with_thumb_cache(thumb_dir.clone()),
        ),
        login_guard: Arc::new(core::auth::LoginGuard::new()),
        share_guard: Arc::new(core::auth::LoginGuard::new()),
        dav_guard: Arc::new(core::auth::LoginGuard::new()),
        path_locks: Arc::new(core::locks::PathLocks::new()),
        dav: Arc::new(api::webdav::DavCache::new()),
        delete_tickets: Arc::new(api::files_mutate::DeleteTickets::default()),
        downloads,
        tus: Arc::new(api::upload_tus::TusStore::new(data_dir.join("uploads_tmp"))),
        hash_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        started,
        rebuild: Arc::new(api::system::RebuildState::default()),
        preheat: Arc::new(api::system::RebuildState::default()),
        thumb_jobs: Arc::new(api::system::ThumbJobSlot::default()),
        mounts_revision: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        config_changed: Arc::new(tokio::sync::Notify::new()),
        cwd,
    };

    // 首启自动引导（设计文档 3.1.1）：还没有管理员时生成 admin + 一次性随机密码，
    // 打印到控制台。用户拿它在登录页登录（网页只有密码框，无用户名），会话
    // must_change_password=1，登录后弹「设置新密码」对话框一步完成建权——
    // 一次性密码就是「守在控制台前的人」的所有权凭证，没有它，先打开页面的
    // 路人就能抢注成主人。密码只走 println（不进 asmoday.log）——日志文件会被
    // 轮转/采集，明文凭据不能落盘；与 --reset-password 口径一致。
    if auth.user_count().unwrap_or(1) == 0 {
        let password = core::auth::random_password();
        let user = auth
            .create_user("admin", &password)
            .map_err(|e| format!("auto-bootstrap admin failed: {e}"))?;
        auth.set_must_change_password(user.id, true)?;
        crate::log_info!("no administrator yet; created user 'admin'");
        print_one_time_password(&password, Some(&urls), None);
    } else if let Some(password) = auth.reissue_pending_password()? {
        // 账号建好了、但还没人用一次性密码登录过（must_change_password 仍是 1）。
        // 上一次打的那串只在控制台出现过一次；用户没记下来就把进程关掉的话，重启后
        // 既没有密码也没有提示 —— 永久锁死（2026-11-06 用户反馈）。能重启进程的人
        // 就在控制台前，按这条凭证的语义重新签发一个，并明确告知旧的已作废。
        crate::log_info!(
            "administrator has not set a password yet; issued a new one-time password \
             (an earlier one is now invalid)"
        );
        print_one_time_password(
            &password,
            Some(&urls),
            Some(
                "issued again because no password has been set yet; an earlier one is now invalid.",
            ),
        );
    }

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
        build_router(state.clone()).await,
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
    // 媒体索引预建 + 定时增量（v4.8.0）
    spawn_media_indexer(state.clone());
    // `.asmoday-part` 残留清理：不阻塞启动（设计文档 3.2.6.4）
    spawn_part_cleanup(state.clone());

    // 监听地址：IP 字面量或 `auto`（容器里最常见的写法，见 BindPlan）。
    // 返回的是**实际**绑上的地址：`::` 在无 IPv6 的环境里会降级成 `0.0.0.0`，
    // 下面的日志与访问链接必须反映真实情况。
    let plan = parse_bind_plan(&listen.0)?;
    let (listener, addr) = bind_listener(plan, port).await?;

    crate::log_info!("config file: {}", config_path.display());
    crate::log_info!("data directory: {}", data_dir.display());
    // 显式把数据目录（或它的子目录）挂出来时，该挂载点会整体不可见 —— 明确告知，
    // 免得用户以为「挂载点掉了」。数据目录永远不对外，这是安全红线。
    for mount in &state.config.read().await.system.mounts {
        if core::reserved::is_reserved(std::path::Path::new(&mount.path)) {
            crate::log_warn!(
                "mount '{}' ({}) is inside the application data directory and will be hidden",
                mount.name.as_deref().unwrap_or(&mount.path),
                mount.path
            );
        }
    }
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
    // 带 scheme 的完整 URL，并且用 OSC 8 包成终端超链接：Windows Terminal / iTerm /
    // VS Code / GNOME Terminal 里直接 Ctrl（⌘）+ 点击就能打开；不支持的终端只看到
    // 普通 URL（`0.0.0.0:4242` 这种裸地址则连自动识别都过不了，v4.9.42/43）。
    //
    // 本机 + 局域网各一条（v4.9.45）：Vite / Next / Jellyfin 都是这个格式，绑
    // `0.0.0.0` 时只打 localhost，用户得自己去 ipconfig 里翻该填哪个地址。
    crate::log_info!("local:   {}", crate::core::logging::link(&urls.local));
    if let Some(network) = &urls.network {
        crate::log_info!("network: {}", crate::core::logging::link(network));
    }
    // 不再替用户拉起浏览器（v4.9.64）：`--open` / `ASMODAY_OPEN=1` 已移除。
    // 地址本来就在横幅与启动日志里，现代终端可点、经典 conhost 可复制；
    // 由服务端进程拉浏览器还会在提权 / 无图形会话下引出混权弹窗与无效 WARN。
    crate::log_info!(
        "frontend build: {}",
        option_env!("ASMODAY_BUILD_ID").unwrap_or("dev")
    );
    if !base_url.is_empty() {
        crate::log_info!("base url prefix: {base_url}");
    }
    // 嵌套挂载点（父目录里又挂了子目录）是合法配置，但只读按「最深命中」判定：
    // 通过父挂载点的路径访问子目录时用的是父的权限。明确告知，免得当成 bug 排查。
    let mount_set = core::path_guard::load(&*state.config.read().await, &state.cwd);
    warn_nested_mounts(&mount_set);

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
/// 嵌套挂载点告警：父目录与子目录各自挂了，路径归属按最长前缀（子胜出），
/// 于是「经父挂载点访问子目录」和「直接访问子挂载点」拿到的 readonly 可能不同。
fn warn_nested_mounts(set: &core::path_guard::MountSet) {
    for (index, outer) in set.mounts.iter().enumerate() {
        let Some(outer_root) = outer.root.as_ref() else {
            continue;
        };
        for inner in set.mounts.iter().skip(index + 1) {
            let Some(inner_root) = inner.root.as_ref() else {
                continue;
            };
            if outer_root == inner_root {
                continue;
            }
            let (parent, child) = if inner_root.starts_with(outer_root) {
                (outer, inner)
            } else if outer_root.starts_with(inner_root) {
                (inner, outer)
            } else {
                continue;
            };
            crate::log_warn!(
                "nested mount: '{}' ({}) sits inside '{}' ({}); readonly is resolved per path, \
                 so access through the outer mount keeps the outer flags",
                child.info.name,
                child.info.path,
                parent.info.name,
                parent.info.path
            );
        }
    }
}

/// 控制台要打印的入口地址（v4.9.45）。
///
/// 社区惯例（Vite / Next.js / Nuxt / Jellyfin）都是**本机 + 局域网各打一条**：
/// 只打 `localhost` 的话，用户把 `host` 改成 `0.0.0.0` 也看不出「手机该填什么
/// 地址」，只能自己去 ipconfig 里翻（2026-11-06 反馈）。
struct AccessUrls {
    /// 本机可用的地址：回环监听 / 具体网卡 / 反代域名都是这一条
    local: String,
    /// 局域网内其它设备能用的地址：只有绑通配地址时才去探测，探测不到就是 None
    network: Option<String>,
}

/// 通配地址不能直接给人点：浏览器打开 `http://0.0.0.0:4242` 会被拒（v4.9.42），
/// 所以本机那条换成 `localhost`；绑具体网卡地址时原样用它。`base_url` 已经是
/// 完整 URL 时直接用它（反代场景，本机/局域网都是同一个域名）。
fn access_urls(server: &core::config::ServerConfig) -> AccessUrls {
    let base = server.proxy.base_url.trim();
    if base.starts_with("http://") || base.starts_with("https://") {
        let url = base.trim_end_matches('/').to_string();
        return AccessUrls {
            local: url,
            network: None,
        };
    }
    let scheme = if server.ssl.enabled { "https" } else { "http" };
    let suffix = base.trim_end_matches('/');
    let build = |host: &str| format!("{scheme}://{host}:{}{suffix}", server.port);

    let host = server.host.trim();
    // `auto` 的落点只有三种：双栈 → 纯 IPv4 → 回环。前两种都是通配地址，
    // 链接形态与 `0.0.0.0` 完全一致（本机给 localhost，再探一条局域网地址）；
    // 第三种只在容器里连 IPv4 都绑不上时才可能出现，那时没有第二条链接可给。
    if host.eq_ignore_ascii_case("auto") {
        return AccessUrls {
            local: build("localhost"),
            network: lan_ip().map(|ip| build(&ip.to_string())),
        };
    }
    match host.parse::<std::net::IpAddr>() {
        // 通配地址：本机走 localhost，局域网地址实测（0.0.0.0 本身谁都连不上）
        Ok(ip) if ip.is_unspecified() => AccessUrls {
            local: build("localhost"),
            network: lan_ip().map(|ip| build(&ip.to_string())),
        },
        Ok(ip) if ip.is_loopback() => AccessUrls {
            local: build("localhost"),
            network: None,
        },
        // IPv6 字面量在 URL 里必须加方括号，否则解析器会把端口当成地址的一部分
        Ok(std::net::IpAddr::V6(ip)) => AccessUrls {
            local: build(&format!("[{ip}]")),
            network: None,
        },
        Ok(ip) => AccessUrls {
            local: build(&ip.to_string()),
            network: None,
        },
        // 配置校验只放行 IP 字面量，这里兜底主机名 / 空值
        Err(_) => AccessUrls {
            local: build(if host.is_empty() { "localhost" } else { host }),
            network: None,
        },
    }
}

/// 本机的出口地址：**不发包**的 UDP `connect` 只做一次路由查找。
///
/// 不用 `getifaddrs` / `GetAdaptersAddresses`：那要按平台写两套 unsafe，还得在
/// 代理容器 / VPN / 多网卡里挑「哪块才算局域网」。这里要回答的只是「别的设备大概
/// 该填哪个地址」，默认路由那块网卡就够（Vite / Next 也只打一条 Network）。
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1（RFC 5737，保留给文档）：没有默认路由时这里返回 ENETUNREACH，
    // 有路由时也只完成绑定/选路，不会真的发出一个包
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        ip if ip.is_loopback() || ip.is_unspecified() => None,
        ip => Some(ip),
    }
}

/// 双击 exe 打开的**经典 conhost** 不实现 OSC 8：文本就是一堆字符，点不动。
///
/// v4.9.64 起唯一用途是在密码横幅里补一句「复制地址」；不再有任何自动或显式
/// 拉起浏览器的旁路，地址只能由用户自己点（支持的终端）或复制（conhost）。
///
/// 判断依据是「Windows 且没有 Windows Terminal / VS Code 这类宿主留下的环境变量」——
/// conhost 不设 `WT_SESSION` 也不设 `TERM_PROGRAM`。只在真控制台里提示，重定向
/// 到文件时不打（那里本来就没有「点」这回事）。
fn legacy_console() -> bool {
    if !cfg!(windows) {
        return false;
    }
    if std::env::var_os("WT_SESSION").is_some() || std::env::var_os("TERM_PROGRAM").is_some() {
        return false;
    }
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// 一次性密码必须「一眼看见」：框起来、单独成行、附上能直接点的入口地址。
///
/// 只走 stdout —— 绝不 log_*!，那会落进 data/logs/asmoday.log 并随轮转留存
/// （明文凭据不落盘，2026-11-04 审查）。
///
/// 颜色跟着日志系统同一个判据（`logging::color_enabled()`：TTY 且非 JSON），
/// 重定向到文件时不带任何转义码；正文字符全 ASCII，GBK 代码页的 cmd 也不会乱码。
///
/// `note` 是横幅里多出来的一句交代（例如「上一个一次性密码已作废」）：重启补发时
/// 必须说清楚，否则用户会拿旧密码反复试。（v4.9.44）
///
/// 入口地址按「本机 / 局域网」两行给（v4.9.45）：绑 `0.0.0.0` 时只打 localhost，
/// 手机 / 平板那头根本不知道该填什么。
///
/// `--reset-password` 不起服务，传 `urls = None`，横幅里根本不出现地址（v4.9.63）。
fn print_one_time_password(password: &str, urls: Option<&AccessUrls>, note: Option<&str>) {
    let color = crate::core::logging::color_enabled();
    let (title, value, dim, reset) = if color {
        ("\x1b[1;36m", "\x1b[1;7;33m", "\x1b[90m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let bar = "=".repeat(68);
    println!();
    println!("{dim}{bar}{reset}");
    println!("{title}  ASMODAY - ADMINISTRATOR CREDENTIAL{reset}");
    println!("{dim}{bar}{reset}");
    println!("  One-time password (sign in with it, then set your own):");
    println!();
    // 密码整行反显：深色终端里是一块亮色，眼睛不用在日志里找。
    // 无色模式下退回纯缩进，别留行尾空格（复制密码时会带进去）
    if color {
        // 行尾不留空格：反显块收在密码最后一个字符上，复制时不带多余空白
        println!("      {value} {password}{reset}");
    } else {
        println!("      {password}");
    }
    println!();
    if let Some(urls) = urls {
        // OSC 8：支持的终端里是可点的真链接；不支持则原样打 URL（v4.9.43）
        println!("  Open: {}", crate::core::logging::link(&urls.local));
        if let Some(network) = &urls.network {
            println!("  LAN:  {}", crate::core::logging::link(network));
        }
        // 经典 conhost 里链接点不动，明说一句让用户复制地址（v4.9.64）。
        // 只在真控制台提示，重定向到文件时没意义。
        if legacy_console() {
            println!(
                "{dim}        (this classic console cannot open links; copy the address above){reset}"
            );
        }
    }
    if let Some(note) = note {
        println!("{dim}  Note: {note}{reset}");
    }
    println!("{dim}  Shown only once; it is never written to the log file.{reset}");
    println!("{dim}{bar}{reset}");
    println!();
}

/// 绑定失败时补一句人话（v4.9.5）。
///
/// Windows 上 `PermissionDenied`（WSAEACCES 10013）最常见的来源**不是**端口被监听
/// （那是 `AddrInUse` / 10048），而是端口正好落在系统动态端口范围里、被别的程序的
/// 出站连接临时占了源端口 —— 浏览器 / WebView2 / 代理动辄几百条长连接，撞上就起不来，
/// 连接一断又能绑定。用户只看到一句「以一种访问权限不允许的方式做了一个访问套接字的
/// 尝试」，完全无从下手，所以这里把原因和两条出路直接打出来。
///
/// 这段会出现在控制台（`startup failed: …` 后面），所以逐条用英文：控制台不保证
/// UTF-8 代码页，中文在 GBK 的 cmd 里是乱码（v4.9.42）。
fn bind_hint(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::AddrInUse => {
            "\nHint: the port is already taken by another program. Close it, or change [server] port."
        }
        ErrorKind::PermissionDenied if cfg!(windows) => {
            "\nHint: on Windows \"access denied\" usually does not mean missing privileges: the port\n             sits inside the system's dynamic port range and was temporarily taken as an outbound\n             source port by another program (browser / proxy / WebView). Either:\n             - pick another port ([server] port, e.g. 42420); or\n             - as administrator, restore the default dynamic range:\n             netsh int ipv4 set dynamicport tcp start=49152 num=16384\n             netsh int ipv6 set dynamicport tcp start=49152 num=16384"
        }
        ErrorKind::PermissionDenied => {
            "\nHint: no permission to bind this port (ports below 1024 usually need root)."
        }
        _ => "",
    }
}

/// 监听地址的解析结果。
///
/// `auto` 只服务一种场景：**不知道运行环境有没有 IPv6**。首当其冲是 Docker 的
/// 默认 bridge 网络——那里压根没有 IPv6 地址，`bind("[::]:p")` 直接 EADDRNOTAVAIL，
/// 进程起不来；而这个错误在宿主机上永远复现不了（开发机的 ::1 永远在）。
///
/// 显式写 `::` 也走同一条降级链（只降一级到 `0.0.0.0`）：用户要的是「对外监听」，
/// 环境给不了双栈就给 v4，而不是让一个能跑的服务起不来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindPlan {
    /// `::`：双栈（显式关掉 `IPV6_V6ONLY`），v4 连接的 peer 是 `::ffff:a.b.c.d`
    DualStack,
    /// `auto`：`::` → `0.0.0.0` → `127.0.0.1`，逐级降级
    Auto,
    /// 具体地址（`0.0.0.0` / `127.0.0.1` / 具体网卡），绑不上就是绑不上
    Addr(std::net::IpAddr),
}

fn parse_bind_plan(raw: &str) -> Result<BindPlan, String> {
    let text = raw.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Ok(BindPlan::Auto);
    }
    let ip: std::net::IpAddr = text.parse().map_err(|_| {
        format!(
            "invalid listen address {text:?}: expected an IP literal \
             (0.0.0.0, 127.0.0.1, 192.168.1.10, ::) or \"auto\""
        )
    })?;
    Ok(match ip {
        std::net::IpAddr::V6(v6) if v6.is_unspecified() => BindPlan::DualStack,
        other => BindPlan::Addr(other),
    })
}

/// 错误是不是「这个环境根本没有这个协议族」，而不是「端口被占」「没权限」。
///
/// 只有前者才允许降级：`AddrInUse` 换地址照样占不上，降级只会把真正的原因藏起来
/// （用户看到「绑 0.0.0.0 失败」，却猜不到是 4242 已被别的程序占了）。
fn family_unavailable(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
    ) {
        return true;
    }
    // Windows: WSAEAFNOSUPPORT 10047 / WSAEADDRNOTAVAIL 10049
    // Linux:   EAFNOSUPPORT 97 / EADDRNOTAVAIL 99
    matches!(
        err.raw_os_error(),
        Some(10047) | Some(10049) | Some(97) | Some(99)
    )
}

/// `io::Error` 的 Display 在 Windows 上是**系统语言**的本地化文本
/// （「通常每个套接字地址…」），控制台要全英文就得按错误码自己写常见项。
fn bind_error_text(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(10048) => "address already in use (os error 10048)".to_string(),
        Some(10013) => "permission denied (os error 10013)".to_string(),
        Some(10049) => "cannot assign the requested address (os error 10049)".to_string(),
        _ => err.to_string(),
    }
}

async fn bind_one(addr: SocketAddr) -> Result<tokio::net::TcpListener, (std::io::Error, String)> {
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
    .map_err(|e| {
        let message = format!(
            "failed to bind {addr}: {}{}",
            bind_error_text(&e),
            bind_hint(&e)
        );
        (e, message)
    })?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener).map_err(|e| {
        let message = format!("failed to register listener for {addr}: {e}");
        (e, message)
    })
}

/// 按计划逐个尝试，返回**实际**绑上的地址（后续日志与链接都用它，而不是配置值）。
async fn bind_listener(
    plan: BindPlan,
    port: u16,
) -> Result<(tokio::net::TcpListener, SocketAddr), String> {
    use std::net::{Ipv4Addr, Ipv6Addr};

    let candidates: Vec<SocketAddr> = match plan {
        BindPlan::DualStack => vec![
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
        ],
        BindPlan::Auto => vec![
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        ],
        BindPlan::Addr(ip) => vec![SocketAddr::new(ip, port)],
    };

    let count = candidates.len();
    let mut previous: Option<(SocketAddr, std::io::Error)> = None;
    for (index, addr) in candidates.iter().enumerate() {
        match bind_one(*addr).await {
            Ok(listener) => {
                if let Some((failed, err)) = previous {
                    // 只打 kind：Display 在 Windows 上是本地化文本，日志要全英文
                    crate::log_warn!(
                        "cannot listen on {failed} ({:?}); falling back to {addr}",
                        err.kind()
                    );
                }
                return Ok((listener, *addr));
            }
            Err((err, message)) => {
                if index + 1 == count || !family_unavailable(&err) {
                    return Err(message);
                }
                previous = Some((*addr, err));
            }
        }
    }
    unreachable!("bind_listener always has at least one candidate")
}

/// 公开路由（无需登录）
fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(status))
        .route("/api/auth/status", get(api::auth::status))
        .route("/api/auth/login", post(api::auth::login))
        .route("/api/auth/logout", post(api::auth::logout))
        // 公开分享（令牌/口令/有效期/防盗链由处理器自行校验）
        .route("/api/public/share/{token}", get(api::shares::info))
        .route("/api/public/share/{token}/raw", get(api::shares::raw))
        .route(
            "/api/public/share/{token}/download",
            get(api::shares::download),
        )
        // aria2 兼容 RPC（v4.9.26）：自带 token:secret 鉴权，不经 Cookie 中间件；
        // [download].enable=false 时不注册（客户端连接即 404，无引擎暴露面）
        .route("/jsonrpc", any(api::aria2_rpc::handle))
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
        // 下载页（v4.9.31，设计文档 3.10）：登录态直接读写任务。
        // 外部客户端仍走 /jsonrpc（token 鉴权、不经 Cookie），两条通道共用同一个管理器。
        .route(
            "/api/downloads",
            get(api::downloads::snapshot).post(api::downloads::add),
        )
        .route("/api/downloads/engine", get(api::downloads::engine))
        .route("/api/downloads/roots", post(api::downloads::set_roots))
        .route("/api/downloads/secret", post(api::downloads::rotate_secret))
        .route(
            "/api/downloads/trackers",
            post(api::downloads::set_trackers),
        )
        .route("/api/downloads/torrents", post(api::downloads::add_torrent))
        .route("/api/downloads/stopped", delete(api::downloads::purge))
        .route("/api/downloads/{gid}/pause", post(api::downloads::pause))
        .route(
            "/api/downloads/{gid}/unpause",
            post(api::downloads::unpause),
        )
        .route("/api/downloads/{gid}", delete(api::downloads::remove))
        .route("/api/auth/me", get(api::auth::me))
        .route("/api/system/health", get(api::system::health))
        // 数据库热备下载（设计文档 4.1）：高敏，需近期验证过密码
        .route("/api/system/backup", post(api::system::backup))
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
        .route(
            "/api/system/thumbnails/rebuild/stop",
            post(api::system::rebuild_stop),
        )
        // 预生成（v4.9.11）：把最近 N 天的图片提前渲好，浏览时直接命中缓存，
        // 不再每张都从网盘读原图。与「重建」分开：重建是全量重做，预生成是补齐。
        .route(
            "/api/system/preheat-thumbnails",
            post(api::system::preheat_thumbnails),
        )
        .route(
            "/api/system/preheat-thumbnails/status",
            get(api::system::preheat_status),
        )
        .route(
            "/api/system/preheat-thumbnails/stop",
            post(api::system::preheat_stop),
        )
        .route("/api/system/test-ffmpeg", post(api::system::test_ffmpeg))
        .route("/api/auth/password", post(api::auth::change_password))
        // 用户管理（v4.9.24 多用户）：AdminUser 提取器拦非管理员
        .route(
            "/api/auth/verify-password",
            post(api::auth::reverify_password),
        )
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
        .route("/api/files/text", get(api::files::read_text))
        .route("/api/files/thumb", get(api::media::thumb))
        .route("/api/files/archive", get(api::files::archive))
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

/// 大请求体路由（2026-11-04 审查）：上传 / tus 分块 / 编辑器保存 / 代理转发。
///
/// 全局默认上限收紧到 2 MiB（见 `build_router`）后，这几类是仅有的合法大 body
/// 消费者，单独挂在 `upload_limit_mb` 的上限下——登录、配置、媒体收藏这类
/// JSON 接口从此与「4 GiB 请求」绝缘。
fn big_body_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/files/raw",
            get(api::files::raw)
                .put(api::files::write_text)
                .delete(api::files::delete),
        )
        .route("/api/files/upload", axum::routing::put(api::files::upload))
        // tus 1.0.0 子集：分块上传与断点续传（设计文档 3.2.3.1）
        .route(
            "/api/files/upload/tus",
            axum::routing::post(api::upload_tus::tus_init),
        )
        .route(
            "/api/files/upload/tus/{id}",
            axum::routing::head(api::upload_tus::tus_head)
                .patch(api::upload_tus::tus_patch)
                .delete(api::upload_tus::tus_delete),
        )
        .route("/proxy/{service_id}", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/", any(api::proxy::handle_root))
        .route("/proxy/{service_id}/{*rest}", any(api::proxy::handle_rest))
}

async fn build_router(state: AppState) -> Router {
    // 请求体上限跟 `[media].upload_max_size_mb` 对齐（安全审查 2026-11-02）：
    // 「0 = 不限制」不再被写死的 64 MiB 卡住；未配置时维持 64 MiB。
    // 上传是流式落盘的，放大上限不会放大内存占用。
    let upload_limit_mb = state.config.read().await.media.upload_max_size_mb;
    let body_limit: usize = if upload_limit_mb == 0 {
        4 * 1024 * 1024 * 1024
    } else {
        upload_limit_mb
            .saturating_mul(1024 * 1024)
            .min(usize::MAX as u64) as usize
    };
    let protected = protected_routes().route_layer(middleware::from_fn_with_state(
        state.clone(),
        api::auth::require_auth,
    ));
    // 大 body 路由同样要过鉴权，只是限额不同（安全审查 2026-11-04：
    // 全局 4 GiB 曾对未登录接口敞开——登录/配置这类接口现在被默认 2 MiB 罩住）。
    // 内层 limit 会覆盖外层：axum 的 DefaultBodyLimit 只往 extensions 插值，
    // 谁后插入谁生效，所以这里的大上限能压过下面的全局 2 MiB。
    let big_body = big_body_routes()
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api::auth::require_auth,
        ))
        .layer(DefaultBodyLimit::max(body_limit));

    public_routes()
        .merge(protected)
        .merge(big_body)
        // WebDAV 自带 Basic 认证，不经 Cookie 中间件；PUT/PROPPATCH 是真大 body
        .merge(dav_routes().layer(DefaultBodyLimit::max(body_limit)))
        // 其余接口（登录 / 配置 / 媒体收藏……）的 body 都是 KB 级 JSON：
        // 2 MiB 足够宽裕，但足以让「4 GiB 打内存」这类请求在进入 handler 前被拒
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
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
        // panic 隔离（2026-11-04 审查）：最外层兜底，单个 handler / 中间件
        // panic 只影响当前连接（500），不再随 panic=unwind 波及进程。
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            |_: Box<dyn std::any::Any + Send>| {
                crate::api::error::detail(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error, check server logs",
                )
            },
        ))
        .with_state(state)
}

/// 媒体索引后台维护（v4.8.0）：启动后预建一遍，之后按 `[media].index_scan_interval_secs` 轮询。
///
/// 两件事一起解决：
/// 1. **预建**：挂载点一进配置就开始建索引，用户第一次点开媒体库不必等整棵树；
/// 2. **保鲜**：外部拷进来的文件下一轮就能看到，不再依赖「用户请求自己扫一遍」。
///
/// 刻意用**独立连接**（不是 `state.media_index`）：走一棵两万条的树在 NAS 上要几十秒，
/// 共用那把 `Mutex` 会让期间所有浏览请求排队等锁 —— 后台宁可自己慢，也不能堵前台。
/// WAL + `busy_timeout=5000` 在 `MediaIndex::open` 里已设好，两条连接只会在提交时排队。
fn spawn_media_indexer(state: AppState) {
    // 取相册墙的深度上限：一次走到底，时间线（depth=2）与相册（depth=3）都能命中窗口
    const DEPTH: usize = 3;

    tokio::spawn(async move {
        let index = Arc::new(
            core::media_index::MediaIndex::new(state.auth.db_path().clone())
                .with_thumb_cache(state.thumbnails.cache_dir().to_path_buf()),
        );
        // 先让监听与静态资源就绪：这活一跑就是几十秒的读盘
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        loop {
            let (interval, mounts, keep) = {
                let config = state.config.read().await;
                let mounts = config
                    .system
                    .mounts
                    .iter()
                    .map(|mount| resolve_mount_root(&mount.path))
                    .collect::<Vec<_>>();
                let mut keep: Vec<String> = mounts
                    .iter()
                    .map(|path| core::fs::normalize_display(path))
                    .collect();
                if keep.is_empty() {
                    // 没有显式挂载点时浏览落回 cwd（path_guard::load 的隐式根），
                    // 索引也得认它，否则第一轮就把自己刚建的树清掉
                    keep.push(core::fs::normalize_display(&state.cwd));
                }
                (config.media.index_scan_interval_secs, mounts, keep)
            };

            // 清掉不属于当前挂载点的旧行（v4.8.1）。换挂载点之后旧根再也走不到，
            // 目录 / 条目 / 新鲜窗口全是永久垃圾；一旦配置回落到「全部离线 → cwd
            // 隐式根」，上一次挂载留下的相册就会冒出来（2026-11-03 的反馈）。
            // 全离线时一律不清：那通常只是网络还没起来，清掉等于把索引白扔。
            if mounts.iter().any(|path| path.is_dir()) {
                let prune_index = Arc::clone(&index);
                let prune_keep = keep.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    prune_index.with_conn(|conn| core::media_index::retain_roots(conn, &prune_keep))
                })
                .await
                .ok()
                .flatten();
                if let Some(outcome) = outcome {
                    if outcome.dirs > 0 {
                        crate::log_info!(
                            "media index garbage-collected: {} stale dir(s) left over from old mounts",
                            outcome.dirs
                        );
                    }
                    // 旧根的条目路径回收派生数据（v4.9.20）：缩略图缓存单元
                    // 照旧占着 limit_bytes 会挤掉当前挂载点的活跃缓存；
                    // ThumbHash 行（键 = 路径指纹）不清的话色值还会喂给前端。
                    // 文件操作在 with_conn 之外，不占索引连接。
                    if !outcome.assets.is_empty() {
                        if let Some(cache_dir) = index.thumb_cache() {
                            let units = core::thumbnail::purge_paths(cache_dir, &outcome.assets);
                            if units > 0 {
                                crate::log_info!(
                                    "stale mount cache purged: {units} thumbnail unit(s)"
                                );
                            }
                        }
                        let keys: Vec<String> = outcome
                            .assets
                            .iter()
                            .map(|p| core::thumbnail::Thumbnailer::hash_key(Path::new(p)))
                            .collect();
                        state.auth.thumb_hashes_forget(&keys);
                    }
                }
            }

            if interval > 0 && !mounts.is_empty() {
                let mount_count = mounts.len();
                let started = Instant::now();
                let mut scanned_total = 0usize;
                for root in mounts {
                    let index = Arc::clone(&index);
                    let cache = Arc::clone(&state.dir_cache);
                    let probe = root.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        api::media::sync_index(
                            &index,
                            &root,
                            DEPTH,
                            api::media::SyncMode::Background,
                            Some(&cache),
                        )
                    })
                    .await;
                    match outcome {
                        Ok(Some(scanned)) => scanned_total += scanned,
                        Ok(None) => {
                            crate::log_warn!("media index unavailable for {}", probe.display())
                        }
                        Err(err) => crate::log_warn!("media index task failed: {err}"),
                    }
                }
                crate::log_info!(
                    "media index scan: {} mount(s), {} entries in {}ms",
                    mount_count,
                    scanned_total,
                    started.elapsed().as_millis()
                );
                // 音频标签预热（v4.9.8）：音乐库列表只查缓存，冷缓存首开要读 120 次 ID3，
                // 网盘上就是二三十秒。这里趁索引刚扫完顺手补掉，用户不用等第一次。
                let warm_index = Arc::clone(&index);
                let warmed =
                    tokio::task::spawn_blocking(move || api::media::warm_audio_meta(&warm_index))
                        .await
                        .unwrap_or(0);
                if warmed > 0 {
                    crate::log_info!("audio metadata warmed: {} track(s)", warmed);
                }

                // 预生成缩略图（v4.9.11）：每轮补一小批，几轮之后常用区就是秒开。
                // 单独 spawn，不拖慢索引循环（一轮 200 张 × 1.6s / 3 路 ≈ 100 秒）。
                let preheat_limit = state.config.read().await.media.thumb_preheat_per_scan;
                if preheat_limit > 0 {
                    let preheat_state = state.clone();
                    tokio::spawn(async move {
                        api::system::preheat_batch(&preheat_state, preheat_limit).await;
                    });
                }
            }

            // interval=0 时也要睡着，否则变成忙等；配置热重载后下一轮自然生效
            let sleep_for = if interval > 0 { interval } else { 300 };
            // 改了挂载点就不该再干等一整轮（默认 300s）：配置看门狗会 notify，
            // 这里被叫醒后立刻走下一次循环（retain + 增量扫描新根）
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_for)) => {}
                _ = state.config_changed.notified() => {
                    crate::log_info!("media index woken by config change, rescanning now");
                }
            }
        }
    });
}

/// 与 `path_guard::load` 同一个口径：能 canonicalize 就用规范路径。
///
/// 索引里的 root 是**展示串**（`//server/share`），由 canonical 路径经
/// `normalize_display` 得来。这里若直接拿配置里的原始串，UNC 挂载点会被写成
/// `\\server\share`，与前端传的 `//server/share` 对不上 —— 新鲜窗口永远失效，
/// 等于每轮请求都在重扫。
fn resolve_mount_root(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    std::fs::canonicalize(&path)
        .ok()
        .filter(|resolved| resolved.is_dir())
        .unwrap_or(path)
}

/// 缩略图缓存后台淘汰：每 30 分钟按 `[media]` 策略清理一次。
fn spawn_media_janitor(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        // 第一次 tick 会立即返回，跳过它，避免刚启动就扫盘
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // 与重建/预生成/手动清理互斥（v4.9.20）：清理删整单元目录，
            // 撞上正在 rename 的生成任务会制造假失败 + 负缓存。抢不到就跳过本轮。
            let Some(_job) = state.thumb_jobs.try_claim() else {
                crate::log_debug!("thumbnail janitor skipped: another thumbnail job is running");
                continue;
            };
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

/// `--reset-password`：重置管理员密码。
///
/// 密码来源二选一：
/// 1. 环境变量 `ASMODAY_RESET_PASSWORD`——**只在带 `--reset-password` 启动时生效**，
///    且同一个值只能用一次（指纹存库，重复出现拒绝并记 WARN，设计文档 3.1.2）；
/// 2. 未提供环境变量时随机生成一次性密码并打印。
async fn reset_admin_password(auth: &AuthStore) -> Result<(), String> {
    let Some(user) = auth.first_user()? else {
        return Err("no user yet; start the server once to create the administrator".to_string());
    };
    auth.invalidate_sessions()?;

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
            // 同首启引导：只打 stdout，不落日志文件；不起服务，横幅里不带地址
            print_one_time_password(&password, None, Some("password was reset."));
        }
    }
    Ok(())
}

/// `--reset-2fa`：清除 TOTP 配置（需要当前密码，防止可用性死锁）
async fn reset_two_factor(auth: &AuthStore) -> Result<(), String> {
    let Some(user) = auth.first_user()? else {
        return Err("no user yet; start the server once to create the administrator".to_string());
    };
    println!("[asmoday] reset 2FA for user '{}'", user.username);
    let Some((_, hash, _)) = auth.find_user(&user.username)? else {
        return Err("user row not found".to_string());
    };
    let password = prompt_line("current password")?;
    if !core::auth::verify_password_lenient(&hash, &password) {
        return Err("wrong password, 2FA not reset".to_string());
    }
    auth.clear_totp(user.id)?;
    auth.invalidate_sessions()?;
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

/// CLI 重置时的密码规则（与网页同一个判定函数，文案走英文——控制台编码不保证）
fn check_password_strength(password: &str) -> Result<(), String> {
    core::auth::check_password_rules(password).map_or(Ok(()), |rule| Err(rule.en()))
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

    /// 监听地址解析（v4.9.53）：`auto` 是唯一允许降级的写法。
    #[test]
    fn bind_plan_parses_auto_and_ip_literals() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        assert_eq!(parse_bind_plan("auto").unwrap(), BindPlan::Auto);
        assert_eq!(parse_bind_plan("AUTO ").unwrap(), BindPlan::Auto);
        assert_eq!(parse_bind_plan("::").unwrap(), BindPlan::DualStack);
        assert_eq!(
            parse_bind_plan("0.0.0.0").unwrap(),
            BindPlan::Addr(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        );
        assert_eq!(
            parse_bind_plan("127.0.0.1").unwrap(),
            BindPlan::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        assert_eq!(
            parse_bind_plan("::1").unwrap(),
            BindPlan::Addr(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        // 主机名不是合法值：解析失败会拖成「启动时卡住等 DNS」，不如直接拒绝
        for bad in ["localhost", "", "0.0.0.0:4242"] {
            let err = parse_bind_plan(bad).unwrap_err();
            assert!(err.contains("invalid listen address"), "{bad}: {err}");
        }
    }

    /// 只有「协议族不可用」才降级；端口被占必须直接失败，否则真正原因会被藏起来。
    #[test]
    fn only_missing_family_triggers_fallback() {
        use std::io::{Error, ErrorKind};

        assert!(family_unavailable(&Error::from(
            ErrorKind::AddrNotAvailable
        )));
        assert!(family_unavailable(&Error::from(ErrorKind::Unsupported)));
        assert!(family_unavailable(&Error::from_raw_os_error(10049)));
        assert!(!family_unavailable(&Error::from(ErrorKind::AddrInUse)));
        assert!(!family_unavailable(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    /// `auto` 的链接形态与通配地址一致：本机 localhost，局域网另给一条
    #[test]
    fn auto_host_advertises_local_and_lan() {
        let mut config = core::config::Config::default();
        config.server.host = "auto".to_string();
        config.validate().expect("auto 必须被配置校验放行");

        let urls = access_urls(&config.server);
        assert_eq!(urls.local, "http://localhost:4242");
        if let Some(network) = urls.network {
            assert!(network.starts_with("http://"), "{network}");
            assert!(!network.contains("0.0.0.0"), "{network}");
        }
    }

    #[test]
    fn default_config_parses_and_validates() {
        let config: Config =
            toml::from_str(core::config::DEFAULT_CONFIG_TOML).expect("默认配置必须能被自己解析");
        config.validate().expect("默认配置必须通过校验");
        assert_eq!(config.server.port, 4242);
        // 首启默认只监听回环（v4.9.42）：把这行钉住，别让模板和 Default 又漂回 0.0.0.0
        assert_eq!(config.server.host, "127.0.0.1");
        assert!(config.system.mounts.is_empty());
        assert!(config.services.is_empty());
        assert!(config.auth.local_enabled);
    }

    /// 控制台入口地址（v4.9.45）：带 scheme、通配地址换成 localhost，局域网另给一条。
    #[test]
    fn console_urls_cover_local_and_lan() {
        let mut server = core::config::ServerConfig::default();
        assert_eq!(server.host, "127.0.0.1");
        let urls = access_urls(&server);
        assert_eq!(urls.local, "http://localhost:4242");
        // 只监听回环时，报任何局域网地址都是假的
        assert!(urls.network.is_none());

        // 0.0.0.0 / ::：本机那条仍是 localhost（浏览器拒绝 http://0.0.0.0:4242），
        // 局域网那条要么是探测到的网卡地址，要么没有 —— 绝不能把通配地址打给人
        for wildcard in ["0.0.0.0", "::"] {
            server.host = wildcard.to_string();
            let urls = access_urls(&server);
            assert_eq!(urls.local, "http://localhost:4242");
            if let Some(network) = urls.network {
                assert!(network.starts_with("http://"), "{network}");
                assert!(!network.contains("0.0.0.0"), "{network}");
                assert!(!network.contains("localhost"), "{network}");
            }
        }

        // 绑具体网卡地址时原样用它（那种配置就是要从别的设备访问）
        server.host = "192.168.1.50".to_string();
        assert_eq!(access_urls(&server).local, "http://192.168.1.50:4242");

        // IPv6 字面量必须加方括号，否则端口会被当成地址的一部分
        server.host = "fd00::1".to_string();
        assert_eq!(access_urls(&server).local, "http://[fd00::1]:4242");

        // 子路径部署与 SSL
        server.host = "127.0.0.1".to_string();
        server.proxy.base_url = "/asmoday/".to_string();
        assert_eq!(access_urls(&server).local, "http://localhost:4242/asmoday");
        server.ssl.enabled = true;
        assert_eq!(access_urls(&server).local, "https://localhost:4242/asmoday");

        // base_url 已经是完整 URL（反代域名）时原样给出
        server.proxy.base_url = "https://nas.example.com/asmoday".to_string();
        assert_eq!(
            access_urls(&server).local,
            "https://nas.example.com/asmoday"
        );
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
