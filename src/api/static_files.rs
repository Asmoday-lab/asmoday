//! 前端静态资源服务：rust-embed 把 `dist/` 整个吸进二进制（设计文档 2.3 / 阶段五）。
//!
//! - `/assets/*` 走长期强缓存（文件名带内容哈希）；
//! - `index.html` 强制 `no-cache`，避免前端更新后浏览器跑旧代码白屏；
//! - 其余非 `/api`、非 `/proxy` 的 GET 请求回落到 `index.html`，支持前端 History 路由；
//! - 子路径部署（`[server.proxy].base_url`）时，index.html 里的绝对路径会被重写成
//!   `/base/assets/...`，并注入 `window.__ASMODAY_BASE__` 供前端 Router / fetch 使用。

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::AppState;

#[derive(RustEmbed)]
#[folder = "dist/"]
struct Assets;

fn build_response(path: &str, body: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        Body::from(body),
    )
        .into_response()
}

/// 把 index.html 的站内绝对路径加上部署前缀，并注入前缀变量。
fn inject_base(html: &str, base: &str) -> String {
    let with_paths = html.replace("=\"/", &format!("=\"{base}/"));
    let script = format!("<script>window.__ASMODAY_BASE__=\"{base}\";</script>");
    with_paths.replace("<head>", &format!("<head>{script}"))
}

fn index(base: &str) -> Response {
    match Assets::get("index.html") {
        Some(file) => {
            let html = String::from_utf8_lossy(&file.data).to_string();
            let html = if base.is_empty() {
                html
            } else {
                inject_base(&html, base)
            };
            let mut response = build_response("index.html", html.into_bytes());
            // 模板里可能带 hashed 资源引用，index 本身绝不能缓存
            if let Ok(value) = header::HeaderValue::from_str("no-cache") {
                response.headers_mut().insert(header::CACHE_CONTROL, value);
            }
            response
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "前端产物缺失：请先在 web/ 目录执行 npm run build",
        )
            .into_response(),
    }
}

/// SPA / 静态资源统一入口。
pub async fn serve(State(state): State<AppState>, uri: Uri) -> Response {
    let base = {
        let config = state.config.read().await;
        config.server.proxy.base_url.trim().to_string()
    };

    // 嵌套路由在个别版本里不会剥掉前缀，这里统一兜一层（幂等）
    let raw = uri.path();
    let stripped = if base.is_empty() {
        raw
    } else {
        raw.strip_prefix(base.as_str()).unwrap_or(raw)
    };
    let path = stripped.trim_start_matches('/');

    // 空路径 = 首屏
    if path.is_empty() {
        return index(&base);
    }

    // API / 代理路径未命中时不回落到 index.html
    if path.starts_with("api/") || path.starts_with("proxy/") || path.starts_with("dav/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            Body::from(r#"{"error":"接口不存在"}"#),
        )
            .into_response();
    }

    // 命中的真实静态文件
    if let Some(file) = Assets::get(path) {
        return build_response(path, file.data.into_owned());
    }

    // 前端路由（无扩展名）→ index.html；带扩展名视为真 404
    let has_extension = path.rsplit('/').next().is_some_and(|name| name.contains('.'));
    if !has_extension {
        return index(&base);
    }

    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// 供 main 判断前端产物是否已内嵌。
#[allow(dead_code)]
pub fn embedded_files() -> usize {
    Assets::iter().count()
}
