//! WebDAV 服务端（设计文档 3.10 / 6.6）。
//!
//! **协议由 [`dav_server`] 实现，本文件只做三件事**：Basic 认证、挂载点路由、
//! 安全边界（路径逃逸 / 系统垃圾 / 只读 / 令牌范围）。
//!
//! 为什么不再手写：自研那版把 `PROPFIND` / `PUT` / `MOVE` 都实现了，但客户端实测
//! 加不上——`/dav`、`/dav/` 这种「虚拟根」直接 404，`PROPFIND` 又缺 `getetag` /
//! `creationdate` / `supportedlock`，目录 href 没有尾斜杠。这些全是 RFC 4918 的边角，
//! 客户端却恰好拿它们当「是不是一个可用的 WebDAV 服务器」的判据。
//!
//! 分工：
//! - `dav_server::localfs::LocalFs` 负责文件系统语义（含多段 Range、原子 rename、ETag）；
//! - `dav_server::memls::MemLs` 负责真锁（替换原先手写的 `core::dav_locks`）；
//! - [`crate::api::dav_fs::CleanFs`] 在 `LocalFs` 外面挡掉 `.part` 与系统垃圾；
//! - 本文件负责认证与 `path_guard` 的逃逸校验，然后按挂载点把请求转给对应的 handler。
//!
//! URL 形态：`/dav/<mount_id>/<path>`。`/dav/` 是虚拟根，`PROPFIND` 列出所有挂载点
//! ——客户端只要填 `http://host:port/dav/` 就能添加，不必先知道 `mount_id`。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use data_encoding::BASE64;
use dav_server::localfs::LocalFs;
use dav_server::memls::MemLs;
use dav_server::{DavHandler, DavMethodSet};

use crate::AppState;
use crate::api::dav_fs::CleanFs;
use crate::core::prefix::RuntimePrefix;
use crate::core::{junk, path_guard};

const PREFIX: &str = "/dav";

/// `OPTIONS` / `Allow` 里列出的方法：与 dav-server 的全量实现对齐。
const DAV_ALLOW: &str =
    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, COPY, LOCK, UNLOCK";
const RO_METHODS: DavMethodSet = DavMethodSet::WEBDAV_RO;
const RW_METHODS: DavMethodSet = DavMethodSet::WEBDAV_RW;

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Asmoday WebDAV\"")],
        "Unauthorized",
    )
        .into_response()
}

fn dav_error(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

/// DAV 响应统一标 `Cache-Control: no-transform`（已有指令保留、追加）。
///
/// 为什么：最外层 `api::compress::layer()` 的白名单会压缩 text/html /
/// application/xml，而 dav-server 的**流式 body** 被压缩层包住后会静默断流——
/// 服务端日志记 200、handler 正常返回，客户端却只看到连接被关
/// （Chromium `ERR_EMPTY_RESPONSE`，2026-10-29 实测复现；curl 不带
/// `Accept-Encoding: gzip/br`，所以全量 curl 回归没暴露）。
/// DAV 响应体普遍很小，压缩收益趋近零，直接整体跳过；顺带规避
/// Content-Encoding 与 Range / If 头的语义相互干扰。
fn mark_no_transform<B>(mut response: Response<B>) -> Response<B> {
    let merged = match response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
    {
        Some(existing) => format!("{existing}, no-transform"),
        None => "no-transform".to_string(),
    };
    if let Ok(value) = merged.parse() {
        response.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    response
}

struct Credentials {
    username: String,
    token: String,
}

fn basic_credentials(headers: &HeaderMap) -> Option<Credentials> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = BASE64.decode(encoded.trim().as_bytes()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, token) = text.split_once(':')?;
    // 设计文档 3.10：用户名 = token_name（不再是「随意填」）
    Some(Credentials {
        username: username.trim().to_string(),
        token: token.to_string(),
    })
}

fn percent_decode_segment(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode_segment(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 把 `/dav/<mount_id>/<rest>` 拆成（mount_id, 剩余路径）。
/// **先按 `/` 分段再逐段解码**：`%2F` 解码出的分隔符不会改变路由结构。
fn split_route(uri_path: &str) -> Result<(Option<String>, String), Response> {
    let rest = uri_path.strip_prefix(PREFIX).unwrap_or("");
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Ok((None, String::new()));
    }
    let mut segments: Vec<String> = Vec::new();
    for raw_seg in rest.split('/') {
        let decoded = percent_decode_segment(raw_seg)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "路径编码非法").into_response())?;
        // 显式拒绝 `.` / `..`（%2e%2e 也逃不掉），穿越防护交给 canonicalize
        if decoded == "." || decoded == ".." {
            return Err((StatusCode::FORBIDDEN, "禁止路径穿越").into_response());
        }
        segments.push(decoded);
    }
    let tail = segments.split_off(1).join("/");
    Ok((Some(segments.into_iter().next().unwrap_or_default()), tail))
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 虚拟根 / 挂载点在 `multistatus` 里的一条 `<response>`。
fn collection_xml(href: &str, name: &str, mtime: i64) -> String {
    let rfc1123 = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.max(0) as u64),
    );
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>\
<D:displayname>{}</D:displayname>\
<D:resourcetype><D:collection/></D:resourcetype>\
<D:getcontenttype>httpd/unix-directory</D:getcontenttype>\
<D:getlastmodified>{}</D:getlastmodified>\
<D:supportedlock><D:lockentry><D:lockscope><D:exclusive/></D:lockscope>\
<D:locktype><D:write/></D:locktype></D:lockentry></D:supportedlock>\
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        escape_xml(href),
        escape_xml(name),
        rfc1123,
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(now_secs)
}

// ---------------------------------------------------------------------------
// 每个挂载点一个 handler（LocalFs + MemLs），按配置指纹缓存
// ---------------------------------------------------------------------------

/// `DavHandler` 持有 `LocalFs` 与 `MemLs`（锁表必须跨请求存活，否则 LOCK 立刻失效），
/// 所以不能每次请求都新建。
///
/// 缓存键含 `root` / `readonly` / `prefix`：配置热重载换了路径或权限，会自然落到新条目；
/// 旧条目留着无害，超过上限时整体清空（这时丢锁也是合理的——挂载点都变了）。
#[derive(Default)]
pub struct DavCache {
    inner: tokio::sync::RwLock<HashMap<String, Arc<DavHandler>>>,
}

impl std::fmt::Debug for DavCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DavCache")
    }
}

impl DavCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get_or_build(
        &self,
        key: String,
        root: &Path,
        prefix: &str,
        readonly: bool,
    ) -> Arc<DavHandler> {
        if let Some(existing) = self.inner.read().await.get(&key) {
            return Arc::clone(existing);
        }
        let handler = Arc::new(build_handler(root, prefix, readonly));
        let mut guard = self.inner.write().await;
        if guard.len() >= 32 {
            guard.clear();
        }
        Arc::clone(guard.entry(key).or_insert(handler))
    }
}

fn build_handler(root: &Path, strip_prefix: &str, readonly: bool) -> DavHandler {
    let fs = CleanFs::new(LocalFs::new(
        root,
        // public=true：新建文件 0644 / 目录 0755。媒体库常被其它进程（转码、相册）
        // 读取，0600 会让它们全部 permission denied。
        true,
        cfg!(windows),
        cfg!(target_os = "macos"),
    ));
    DavHandler::builder()
        .filesystem(fs)
        .locksystem(MemLs::new())
        .strip_prefix(strip_prefix)
        // 软链接当普通条目：逃逸防护由 path_guard 在入口做（跟随后 canonicalize 校验根）
        .hide_symlinks(false)
        // 浏览器直接打开目录能看到文件列表，省掉「装个客户端才能看」的门槛。
        // 注意**不能**同时设 `indexfile`：dav-server 里设了 indexfile 就只找那一个文件，
        // 找不到直接 404，不会回落到 autoindex（目录页于是打不开）。
        .autoindex(true)
        // 目录请求缺尾斜杠时 302 补上：客户端靠它判断「这是集合不是文件」
        .redirect(true)
        .methods(if readonly { RO_METHODS } else { RW_METHODS })
        .build_handler()
}

// ---------------------------------------------------------------------------
// 安全边界
// ---------------------------------------------------------------------------

async fn mount_load(state: &AppState) -> path_guard::MountSet {
    let config = state.config.read().await;
    path_guard::load(&config, &state.cwd)
}

/// 请求路径的逃逸 / 垃圾文件校验（同步实现，调用方包 `spawn_blocking`）。
///
/// - 已存在的路径：`resolve_existing`（canonicalize 后必须仍在挂载点内）；
/// - 待创建的目标：`resolve_child`（父目录必须存在，名字逐条校验）。
fn guard_path(mount: &path_guard::MountEntry, rest: &str, writing: bool) -> Result<(), Response> {
    // 目录请求常带尾斜杠（`MKCOL /dav/m0/newdir/`）：去掉再做名字校验，
    // 否则最后一段会变成空串，被当成「名字含 /」直接 400。
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Ok(());
    }
    let (parent, name) = match rest.rsplit_once('/') {
        Some((parent, name)) if !name.is_empty() => (parent.to_string(), name.to_string()),
        _ => (String::new(), rest.to_string()),
    };

    // 系统垃圾与运行期临时文件：读侧当不存在（404），写侧明确拒绝（403）
    if path_guard::is_part_file(&name) || junk::is_system_junk(&name) {
        return Err(dav_error(
            if writing {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            },
            "该文件名属于系统垃圾或运行期临时文件，已被服务器拒绝",
        ));
    }

    let root = mount.canon_root().map_err(|err| {
        dav_error(
            StatusCode::NOT_FOUND,
            &format!("挂载点不可用：{}", err.message()),
        )
    })?;
    let candidate = root.join(rest);
    let outcome = if path_guard::symlink_exists(&candidate) {
        path_guard::resolve_existing(mount, rest)
    } else if writing {
        path_guard::resolve_child(mount, &parent, &name)
    } else {
        // 读一个不存在的路径：不用再 canonicalize，交给 dav-server 回 404
        return Ok(());
    };
    outcome.map(|_| ()).map_err(|err| match err {
        // 写操作的父目录不存在：RFC 4918 要求 409 Conflict（客户端据此提示「目录已不在」）
        crate::core::fs::FsError::NotFound(_) if writing => {
            dav_error(StatusCode::CONFLICT, "父目录不存在")
        }
        crate::core::fs::FsError::NotFound(_) => dav_error(StatusCode::NOT_FOUND, "路径不存在"),
        crate::core::fs::FsError::SymlinkEscape(_) => {
            dav_error(StatusCode::FORBIDDEN, "禁止经软链接逃逸")
        }
        crate::core::fs::FsError::PathTraversal(_) => {
            dav_error(StatusCode::FORBIDDEN, "禁止路径穿越")
        }
        crate::core::fs::FsError::Readonly(_) => dav_error(StatusCode::FORBIDDEN, "挂载点为只读"),
        other => dav_error(StatusCode::BAD_REQUEST, &other.message()),
    })
}

/// `MOVE` / `COPY` 的 `Destination` 头也要过同一道闸——
/// 否则客户端可以用 `/dav/m0/指向外部的软链接/x.txt` 把文件写到挂载点外面。
async fn guard_destination(
    mount: &path_guard::MountEntry,
    mount_id: &str,
    prefix: &RuntimePrefix,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let Some(value) = headers
        .get("destination")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    let path = match value.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|idx| &rest[idx..]).unwrap_or("/"),
        None => value,
    };
    // 子路径部署时客户端发的 Destination 带 `/asmoday`，先剥掉再解析，
    // 否则会被当成「另一个挂载点」直接 403。
    let path = prefix.strip(path);
    let (dest_mount, dest_rest) = split_route(path)?;
    if dest_mount.as_deref() != Some(mount_id) {
        return Err(dav_error(
            StatusCode::FORBIDDEN,
            "禁止跨挂载点操作（Destination 必须指向同一挂载点）",
        ));
    }
    let mount = mount.clone();
    tokio::task::spawn_blocking(move || guard_path(&mount, &dest_rest, true))
        .await
        .unwrap_or_else(|err| {
            Err(dav_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("task failed: {err}"),
            ))
        })
}

// ---------------------------------------------------------------------------
// 虚拟根：`/dav` 与 `/dav/`
// ---------------------------------------------------------------------------

async fn virtual_root(
    state: &AppState,
    access: &crate::api::tokens::TokenAccess,
    prefix: &RuntimePrefix,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let base = format!("{}/dav/", prefix.as_str().trim_end_matches('/'));
    match method.as_str() {
        "OPTIONS" => (
            StatusCode::OK,
            [
                (HeaderName::from_static("dav"), "1, 2".to_string()),
                // Windows 资源管理器 / Office 认这个头才肯把共享当「可写 WebDAV」
                (HeaderName::from_static("ms-author-via"), "DAV".to_string()),
                (header::ALLOW, DAV_ALLOW.to_string()),
            ],
        )
            .into_response(),
        "PROPFIND" => {
            let depth = headers
                .get("depth")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("1");
            let set = mount_load(state).await;
            let mut xml = String::from(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">",
            );
            xml.push_str(&collection_xml(&base, "Asmoday", now_secs()));
            if depth != "0" {
                for mount in &set.mounts {
                    // 令牌限定挂载点时，列表里也不该出现别的目录
                    if access
                        .mount_scope
                        .as_deref()
                        .is_some_and(|scope| scope != mount.info.id)
                    {
                        continue;
                    }
                    let href = format!("{}{}/", base, percent_encode_segment(&mount.info.id));
                    let mtime = mount.root.as_deref().map(mtime_of).unwrap_or_else(now_secs);
                    xml.push_str(&collection_xml(&href, &mount.info.name, mtime));
                }
            }
            xml.push_str("</D:multistatus>");
            (
                StatusCode::MULTI_STATUS,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                xml,
            )
                .into_response()
        }
        "GET" | "HEAD" => {
            // 浏览器直接打开 `/dav/` 时的落地页：列出可挂载的目录
            let set = mount_load(state).await;
            let mut html = String::from(
                "<!doctype html><meta charset=\"utf-8\"><title>Asmoday WebDAV</title>\
<style>body{font:15px/1.6 system-ui,sans-serif;margin:2.5rem auto;max-width:38rem;padding:0 1rem}\
a{color:#2563eb;text-decoration:none}a:hover{text-decoration:underline}li{margin:.35rem 0}</style>\
<h1>Asmoday WebDAV</h1><p>把下面的地址添加为 WebDAV 服务器（用户名 = 令牌名，密码 = 令牌）。</p><ul>",
            );
            for mount in &set.mounts {
                if access
                    .mount_scope
                    .as_deref()
                    .is_some_and(|scope| scope != mount.info.id)
                {
                    continue;
                }
                html.push_str(&format!(
                    "<li><a href=\"{}{}/\">{}</a> <small>{}</small></li>",
                    base,
                    percent_encode_segment(&mount.info.id),
                    escape_xml(&mount.info.name),
                    if mount.info.readonly {
                        "只读"
                    } else {
                        "读写"
                    }
                ));
            }
            html.push_str("</ul>");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response()
        }
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, DAV_ALLOW.to_string())],
            "请在挂载点地址上使用 WebDAV 方法，例如 /dav/<mount_id>/",
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// WebDAV 统一入口（`nest("/dav")` 之后，任何 `/dav` 下的路径都会到这里）。
pub async fn handle(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
) -> Response {
    // 未初始化严防死守：初始化完成前 WebDAV 一律 503，与 /api/* 同口径
    // （设计文档 v2.7.0 3.1.1，防止未鉴权实例被扫描器探测）
    if state.auth.user_count().unwrap_or(0) == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Instance is not initialized yet",
        )
            .into_response();
    }

    // ---- 认证（独立防爆破池，设计文档 3.1.5 / 3.10）----
    let Some(creds) = basic_credentials(req.headers()) else {
        return unauthorized();
    };
    let guard_key = format!("{}|{}", crate::api::auth::peer_ip(peer), creds.username);
    if let Err(remaining) = state.dav_guard.check(&guard_key) {
        return dav_error(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("Too many failed attempts, retry in {remaining}s"),
        );
    }
    let Some(access) = crate::api::tokens::verify_token(&state, &creds.username, &creds.token)
    else {
        state.dav_guard.record_failure(&guard_key);
        return unauthorized();
    };
    state.dav_guard.record_success(&guard_key);

    let prefix = req
        .extensions()
        .get::<RuntimePrefix>()
        .cloned()
        .unwrap_or_default();
    let method = req.method().clone();

    // ---- 路由与挂载点 ----
    // 路径来源：`nest("/dav")` 已经把前缀剥掉，`req.uri().path()` 是 `/` 或 `/m0/...`，
    // 拼回 `/dav` 才是对外 URL 形态。
    //
    // 不能用 `OriginalUri`：它由**最外层** Router 插入，子路径部署时带的是
    // `/asmoday/dav/...`，`strip_prefix("/dav")` 直接失败，所有请求会被当成
    // 虚拟根（GET 看似 200，其实回的是目录页；MOVE 则 405）。
    // 两个入口的路径形态不同：
    // - `nest("/dav")` 命中时 axum 已把 `/dav` 剥掉（`/m0/x`）；
    // - `route("/dav/")` 命中时路径原样保留（`/dav/`）。
    // 统一成对外形态后再解析，否则尾斜杠那条会拼成 `/dav/dav/`。
    let inner_path = req.uri().path();
    let dav_path = if inner_path == PREFIX
        || inner_path
            .strip_prefix(PREFIX)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        inner_path.to_string()
    } else {
        format!("{PREFIX}{inner_path}")
    };
    let (mount_id, rest) = match split_route(&dav_path) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let set = mount_load(&state).await;
    let Some(mount_id) = mount_id else {
        return mark_no_transform(
            virtual_root(&state, &access, &prefix, &method, req.headers()).await,
        );
    };
    // 令牌限定挂载点：泄漏的令牌只影响一个目录，而不是全库
    if let Some(scope) = access.mount_scope.as_deref()
        && scope != mount_id
    {
        return dav_error(
            StatusCode::FORBIDDEN,
            "This token is restricted to another mount point",
        );
    }
    let Some(mount) = set.find(&mount_id) else {
        return dav_error(StatusCode::NOT_FOUND, "挂载点不存在");
    };
    let Ok(root) = mount.canon_root() else {
        return dav_error(StatusCode::NOT_FOUND, "挂载点当前不可用");
    };
    let root = root.clone();
    let mount_owned = mount.clone();
    let readonly = access.readonly || mount.info.readonly;

    // 只读令牌：写方法直接 403，不让 dav-server 先落盘再报错
    if readonly && !matches!(method.as_str(), "OPTIONS" | "GET" | "HEAD" | "PROPFIND") {
        return dav_error(
            StatusCode::FORBIDDEN,
            if access.readonly {
                "该令牌为只读"
            } else {
                "挂载点为只读"
            },
        );
    }

    // 挂载点根不可删除 / 移动：dav-server 会把 `/` 当成目录，真删了就是整盘消失
    if rest.is_empty() && matches!(method.as_str(), "DELETE" | "MOVE") {
        return dav_error(StatusCode::FORBIDDEN, "挂载点根目录不可执行该操作");
    }

    // 路径逃逸校验（含 Destination）。
    // `writing` 看**方法**而不是挂载点权限：可写挂载点上的 GET 不能被当成写请求，
    // 否则读一个已存在的 `.part` 临时文件会回 403 而不是「不存在」。
    let writing = matches!(
        method.as_str(),
        "PUT" | "POST" | "MKCOL" | "DELETE" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK"
    );
    let guard_mount = mount_owned.clone();
    let guard_rest = rest.clone();
    if let Err(response) =
        tokio::task::spawn_blocking(move || guard_path(&guard_mount, &guard_rest, writing))
            .await
            .unwrap_or_else(|err| {
                Err(dav_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("task failed: {err}"),
                ))
            })
    {
        return response;
    }
    if matches!(method.as_str(), "MOVE" | "COPY")
        && let Err(response) =
            guard_destination(&mount_owned, &mount_id, &prefix, req.headers()).await
    {
        return response;
    }

    // ---- 转交 dav-server ----
    // `strip_prefix` 与请求 URI 必须同时带上部署前缀，否则多挂载点与子路径部署
    // 两头都会错位（href 少一层，Destination 对不上）。
    let scope_prefix = format!("{}/dav/{}", prefix.as_str().trim_end_matches('/'), mount_id);
    let cache_key = format!("{scope_prefix}|{}|{readonly}", root.display());
    let handler = state
        .dav
        .get_or_build(cache_key, &root, &scope_prefix, readonly)
        .await;

    // nest 会把 `/dav` 剥掉（handler 看到的是 `/m0/...`），这里用原始路径重建，
    // 让 dav-server 的 strip_prefix 能对上。
    // MKCOL 带尾斜杠（cadaver / 部分 Java 客户端会这么发）时 dav-server 直接判 400，
    // RFC 4918 允许这种写法，去掉尾斜杠再交给它。
    let mut target_path = dav_path.as_str();
    if method.as_str() == "MKCOL" && target_path.ends_with('/') {
        target_path = target_path.trim_end_matches('/');
        if target_path.is_empty() || target_path == PREFIX {
            return dav_error(StatusCode::FORBIDDEN, "挂载点根目录已存在");
        }
    }
    match rebuild_uri(target_path, prefix.as_str(), req.uri()) {
        Some(uri) => *req.uri_mut() = uri,
        None => return dav_error(StatusCode::BAD_REQUEST, "路径无法重建"),
    }

    let response = handler.handle(req).await;
    let (parts, body) = response.into_parts();
    mark_no_transform(Response::from_parts(parts, Body::new(body)))
}

/// 把剥过部署前缀的路径重新拼上前缀（dav-server 的 strip_prefix 用的是完整路径）。
fn rebuild_uri(original: &str, prefix: &str, current: &Uri) -> Option<Uri> {
    let path = if prefix.is_empty() {
        original.to_string()
    } else {
        format!("{prefix}{original}")
    };
    let text = match current.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    text.parse::<Uri>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_transform_appends_to_existing_cache_control() {
        let mut resp = Response::builder()
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::empty())
            .unwrap();
        let resp = mark_no_transform(resp);
        assert_eq!(
            resp.headers()[header::CACHE_CONTROL],
            "no-cache, no-transform"
        );
    }

    #[test]
    fn no_transform_inserts_when_missing() {
        let resp = Response::builder().body(Body::empty()).unwrap();
        let resp = mark_no_transform(resp);
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-transform");
    }

    #[test]
    fn split_route_decodes_per_segment() {
        let (mid, rest) = split_route("/dav/m0/%E7%9B%B8%E5%86%8C/a%20b.txt").unwrap();
        assert_eq!(mid.as_deref(), Some("m0"));
        assert_eq!(rest, "相册/a b.txt");
        let (mid2, rest2) = split_route("/dav/m0/a%2Fb.txt").unwrap();
        assert_eq!(mid2.as_deref(), Some("m0"));
        assert_eq!(rest2, "a/b.txt");
    }

    #[test]
    fn split_route_handles_root_forms() {
        assert_eq!(split_route("/dav").unwrap().0, None);
        assert_eq!(split_route("/dav/").unwrap().0, None);
        assert_eq!(
            split_route("/dav/m0/").unwrap(),
            (Some("m0".into()), String::new())
        );
    }

    #[test]
    fn split_route_rejects_dot_segments() {
        let err = split_route("/dav/m0/%2e%2e/secret").unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn rebuild_uri_keeps_prefix_and_query() {
        let current: Uri = "/m0/docs?x=1".parse().unwrap();
        let uri = rebuild_uri("/dav/m0/docs", "/asmoday", &current).unwrap();
        assert_eq!(uri.path(), "/asmoday/dav/m0/docs");
        assert_eq!(uri.query(), Some("x=1"));
        // 根路径部署时原样返回
        let uri = rebuild_uri("/dav/m0/", "", &"/m0/".parse().unwrap()).unwrap();
        assert_eq!(uri.path(), "/dav/m0/");
    }

    #[test]
    fn collection_xml_escapes_names() {
        let xml = collection_xml("/dav/m0/", "A & B <c>", 0);
        assert!(xml.contains("A &amp; B &lt;c&gt;"));
        assert!(xml.contains("<D:collection/>"));
    }
}
