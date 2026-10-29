//! 传输层压缩（设计文档 2.4）。
//!
//! 用 `tower-http` 的 `CompressionLayer` 在响应出站前统一压一遍，而不是手写
//! `Content-Encoding`：分块编码、`Vary`、`Content-Length` 移除、br/gzip 协商
//! 都由社区实现兜住，自己写迟早漏一个。
//!
//! 判定逻辑没用现成的 `DefaultPredicate`：它只排除了图片 / gRPC / SSE，
//! 剩下的**什么都压**，于是 `video/mp4`、`application/zip` 这些已经压过的
//! 格式会被白白过一遍 CPU（还可能变大）。这里改成**白名单**：
//!
//! - 只压文本类与 JSON / JS / XML / SVG / wasm；
//! - **206 一律不压**：`/api/files/raw` 的字节区间按未压缩文件算，
//!   压完 `Content-Range` 就失真，视频拖动直接花屏；
//! - 已经带 `Content-Encoding` 的不二次压缩；
//! - 小于 1 KiB 的不压（压缩头 + 分块开销比省下的还多）；
//! - 显式 `Cache-Control: no-transform` 的尊重上游意愿。

use axum::http::{Response, StatusCode, header};
use tower_http::compression::{CompressionLayer, Predicate};

/// 小于这个字节数就不压：gzip 头 + 分块边界大约 20–40 字节，
/// 1 KiB 以下压缩率的绝对值太小，反而多一次 CPU。
const MIN_BYTES: u64 = 1024;

/// 可压缩的 Content-Type 前缀 / 精确值（全部小写比较）。
const COMPRESSIBLE: &[&str] = &[
    "text/",
    "application/javascript",
    "application/json",
    "application/xml",
    "application/xhtml+xml",
    "application/manifest+json",
    "application/wasm",
    "image/svg+xml",
    "image/x-icon",
];

fn is_compressible(content_type: &str) -> bool {
    // "text/html; charset=utf-8" → 只看分号前的部分
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    COMPRESSIBLE.iter().any(|rule| {
        if rule.ends_with('/') {
            base.starts_with(rule)
        } else {
            base == *rule
        }
    })
}

/// 只压白名单里的文本类响应。
#[derive(Clone, Copy, Default)]
pub struct CompressibleText;

impl Predicate for CompressibleText {
    fn should_compress<B>(&self, response: &Response<B>) -> bool
    where
        B: http_body::Body,
    {
        // 字节区间切片：压缩会破坏 Content-Range 的语义
        if response.status() == StatusCode::PARTIAL_CONTENT {
            return false;
        }
        let headers = response.headers();
        // 已经压过（或上游明确标了 identity 之外的值）：不再叠一层
        if headers.contains_key(header::CONTENT_ENCODING) {
            return false;
        }
        if headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("no-transform"))
        {
            return false;
        }
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !is_compressible(content_type) {
            return false;
        }
        // 体积：body 的精确长度优先，其次 Content-Length；都拿不到（流式）就压
        let size = response
            .body()
            .size_hint()
            .exact()
            .or_else(|| {
                headers
                    .get(header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(u64::MAX);
        size >= MIN_BYTES
    }
}

/// 带压缩的响应层（br 优先，gzip 兜底）。
pub fn layer() -> CompressionLayer<CompressibleText> {
    CompressionLayer::new().compress_when(CompressibleText)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn response(status: StatusCode, content_type: &str, body_len: usize) -> Response<Body> {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, body_len.to_string())
            .body(Body::from(vec![0u8; body_len]))
            .unwrap()
    }

    #[test]
    fn compresses_text_but_not_binary() {
        assert!(CompressibleText.should_compress(&response(StatusCode::OK, "text/html", 2048)));
        assert!(CompressibleText.should_compress(&response(
            StatusCode::OK,
            "application/javascript; charset=utf-8",
            2048
        )));
        assert!(CompressibleText.should_compress(&response(StatusCode::OK, "image/svg+xml", 2048)));
        // 这两类是「压了也白压」，之前的 DefaultPredicate 会照压不误
        assert!(!CompressibleText.should_compress(&response(StatusCode::OK, "video/mp4", 2048)));
        assert!(!CompressibleText.should_compress(&response(StatusCode::OK, "image/jpeg", 2048)));
        assert!(!CompressibleText.should_compress(&response(
            StatusCode::OK,
            "application/zip",
            2048
        )));
    }

    #[test]
    fn leaves_partial_content_alone() {
        // 同一份 text/plain：完整响应可压，206 切片不可压（否则 Content-Range 失真）
        assert!(CompressibleText.should_compress(&response(StatusCode::OK, "text/plain", 2048)));
        assert!(!CompressibleText.should_compress(&response(
            StatusCode::PARTIAL_CONTENT,
            "text/plain",
            2048
        )));
    }

    #[test]
    fn skips_tiny_and_precompressed_bodies() {
        assert!(!CompressibleText.should_compress(&response(
            StatusCode::OK,
            "application/json",
            64
        )));

        let already = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/css")
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(vec![0u8; 4096]))
            .unwrap();
        assert!(!CompressibleText.should_compress(&already));

        let no_transform = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::CACHE_CONTROL, "no-store, no-transform")
            .body(Body::from(vec![0u8; 4096]))
            .unwrap();
        assert!(!CompressibleText.should_compress(&no_transform));
    }
}
