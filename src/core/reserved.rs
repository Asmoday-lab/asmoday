//! 进程保留目录：应用自己的数据目录（SQLite / 日志 / 缩略图缓存）。
//!
//! FileBrowser 把数据库与图片缓存放在 `--database` / `--cache-dir` 这类
//! **可浏览根之外**的位置，因此缓存产物永远不会出现在用户的文件列表里。
//! 我们的默认值是 `data/` + 「未配置挂载点时以 cwd 为根」，两者天生重叠：
//! 一旦用户把项目目录整个挂出来，缩略图缓存就成了普通目录 ——
//! 列表会给每张缓存图再生成一张缓存图（缓存文件名带 `.webp`/`.jpg`，
//! 被当成图片），下一轮刷新又把这些新文件当图片……按宽度倍数指数增殖。
//!
//! 这里维护一份保留目录清单，所有入口（列表 / 路径解析 / 媒体扫描 / 搜索 /
//! WebDAV）统一以此为准：既不显示，也不允许访问。判断同时支持
//! `C:/x/y` 与 `\\?\C:\x\y` 两种写法（Windows 上 canonicalize 会带前缀）。

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

static RESERVED: OnceLock<RwLock<Vec<PathBuf>>> = OnceLock::new();

fn store() -> &'static RwLock<Vec<PathBuf>> {
    RESERVED.get_or_init(|| RwLock::new(Vec::new()))
}

/// 在只读锁内做判定，避免每个目录项都克隆一份清单。
fn with_roots<T>(f: impl FnOnce(&[PathBuf]) -> T) -> T {
    let guard = store().read().unwrap_or_else(|err| err.into_inner());
    f(&guard)
}

/// 注册一个保留目录。数据目录在启动时注册；重复注册是幂等的。
pub fn register(path: &Path) {
    let mut candidates = vec![path.to_path_buf()];
    if let Ok(canon) = std::fs::canonicalize(path) {
        candidates.push(canon);
    }
    let mut guard = store().write().unwrap_or_else(|err| err.into_inner());
    for candidate in candidates {
        if !guard.contains(&candidate) {
            guard.push(candidate);
        }
    }
}

/// 把路径切成归一化段：统一分隔符、剥掉 `\\?\`、丢掉空段与 `.`。
fn segments(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
    text.split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(str::to_string)
        .collect()
}

fn same_segment(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn same_or_under(candidate: &Path, root: &Path) -> bool {
    let cand = segments(candidate);
    let base = segments(root);
    !base.is_empty()
        && cand.len() >= base.len()
        && base
            .iter()
            .zip(cand.iter())
            .all(|(a, b)| same_segment(a, b))
}

/// 纯字面判定（不碰磁盘）：`path` 是否落在某个保留目录内（含自身）。
///
/// 列表过滤要用它 —— 每个目录项都 canonicalize 一次会白白多出上万次系统调用，
/// 而目录列表的 `dir` 本身已经解析过，直接按段比较就够。
pub fn is_reserved_lexical(path: &Path) -> bool {
    with_roots(|roots| roots.iter().any(|root| same_or_under(path, root)))
}

/// 解析后判定：字面没命中时再 canonicalize 一次（防大小写 / 链接写法差异）。
pub fn is_reserved(path: &Path) -> bool {
    if is_reserved_lexical(path) {
        return true;
    }
    match std::fs::canonicalize(path) {
        Ok(canon) => is_reserved_lexical(&canon),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "asmoday-reserved-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn detects_self_child_and_sibling() {
        let base = unique_dir("detect");
        let data = base.join("data");
        std::fs::create_dir_all(data.join("thumbnails")).unwrap();
        register(&data);

        assert!(is_reserved_lexical(&data), "自身要命中");
        assert!(
            is_reserved_lexical(&data.join("thumbnails")),
            "子目录要命中"
        );
        assert!(is_reserved(&data.join("thumbnails").join("a.webp")));
        assert!(
            !is_reserved_lexical(&base.join("photos")),
            "同级目录不能误伤"
        );
        assert!(!is_reserved_lexical(&base), "父目录不是保留目录");

        // `C:/x/y` 与 `\\?\C:\x\y` 必须等价（Windows canonicalize 会带前缀）
        let display = crate::core::fs::normalize_display(&data);
        assert!(is_reserved_lexical(Path::new(&display)));

        let _ = std::fs::remove_dir_all(&base);
    }
}
