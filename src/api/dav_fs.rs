//! WebDAV 文件系统装饰层。
//!
//! `dav-server` 的 `LocalFs` 会把目录里的一切原样列出去，其中两类名字必须挡掉：
//!
//! - `.asmoday-part.*`：我们自己的上传临时文件，对客户端不可见（设计文档 3.3.7）；
//! - `.DS_Store` / `._*` / `Thumbs.db` / `.Trashes` 等系统垃圾：macOS 与 Windows
//!   双端挂载时，一方留下的垃圾会被另一方当成普通文件往回写，形成无限同步。
//!
//! 挡的方式是「读侧不列 + 写侧拒绝」：只过滤 `read_dir` 而不拒绝写，客户端照样能
//! 创建 `.DS_Store`；只拒绝写而不过滤，历史遗留的垃圾会一直出现在列表里。

use std::fmt;
use std::path::{Path, PathBuf};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsStream, OpenOptions,
    ReadDirMeta,
};
use futures_util::StreamExt;

use crate::core::{junk, path_guard, reserved};

/// 只按最后一段名字判定（临时文件 / 系统垃圾）。
fn hidden_by_name(path: &DavPath) -> bool {
    path.file_name().is_some_and(is_hidden)
}

/// 客户端不该看见、也不该写入的名字。
fn is_hidden(name: &str) -> bool {
    name.is_empty() || path_guard::is_part_file(name) || junk::is_system_junk(name)
}

/// 把任意 `DavFileSystem` 包成「隐藏临时文件与系统垃圾」的版本。
#[derive(Clone)]
pub struct CleanFs {
    inner: std::sync::Arc<dyn DavFileSystem + Send + Sync>,
    /// 该 handler 对应的挂载点根：用来把 DavPath 还原成绝对路径，
    /// 判断是否落在应用数据目录里（缩略图缓存就在其中）。
    root: PathBuf,
}

impl CleanFs {
    pub fn new(inner: Box<dyn DavFileSystem + Send + Sync>, root: &Path) -> Box<Self> {
        Box::new(Self {
            inner: std::sync::Arc::from(inner),
            root: root.to_path_buf(),
        })
    }

    /// 该 DavPath 是否指向应用数据目录（或其中任何位置）。
    fn is_reserved(&self, path: &DavPath) -> bool {
        reserved::is_reserved_lexical(&self.root.join(path.as_rel_ospath()))
    }

    fn denied(&self, path: &DavPath) -> bool {
        hidden_by_name(path) || self.is_reserved(path)
    }
}

impl fmt::Debug for CleanFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CleanFs")
    }
}

impl DavFileSystem for CleanFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            if self.denied(path) {
                // 读：当作不存在；写：直接拒绝（客户端会看到 403，而不是「写成功但看不见」）
                return Err(if options.write {
                    FsError::Forbidden
                } else {
                    FsError::NotFound
                });
            }
            let file = self.inner.open(path, options).await?;
            Ok(Box::new(CleanFile { inner: file }) as Box<dyn DavFile>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let stream = self.inner.read_dir(path, meta).await?;
            let base = self.root.join(path.as_rel_ospath());
            let filtered = stream.filter(move |entry| {
                let keep = match entry {
                    Ok(entry) => {
                        let name = String::from_utf8_lossy(&entry.name()).to_string();
                        // 名字过滤 + 保留目录过滤：
                        // 应用数据目录对 WebDAV 客户端也必须是「不存在」，
                        // 否则客户端会去同步一个永远 403 的目录。
                        !is_hidden(&name) && !reserved::is_reserved_lexical(&base.join(&name))
                    }
                    // 出错项原样放行，交给上层决定怎么回（通常直接跳过）
                    Err(_) => true,
                };
                std::future::ready(keep)
            });
            Ok(Box::pin(filtered) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            if self.denied(path) {
                return Err(FsError::NotFound);
            }
            self.inner.metadata(path).await
        })
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            if self.denied(path) {
                return Err(FsError::NotFound);
            }
            self.inner.symlink_metadata(path).await
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            if self.denied(path) {
                return Err(FsError::Forbidden);
            }
            // Windows 上对已存在的目录调 `create_dir` 得到的是 Win32 错误码 183
            // （`ERROR_ALREADY_EXISTS`），而 dav-server 的错误映射只认 unix errno
            // （`EEXIST` = 17），于是「重复 MKCOL」会变成 500 而不是 RFC 要求的 405。
            // 自己先探一次存在性，两端行为就一致了。
            if self.inner.metadata(path).await.is_ok() {
                return Err(FsError::Exists);
            }
            self.inner.create_dir(path).await
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.remove_dir(path)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.remove_file(path)
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            if self.denied(to) {
                return Err(FsError::Forbidden);
            }
            self.inner.rename(from, to).await
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            if self.denied(to) {
                return Err(FsError::Forbidden);
            }
            self.inner.copy(from, to).await
        })
    }

    fn set_accessed<'a>(
        &'a self,
        path: &'a DavPath,
        tm: std::time::SystemTime,
    ) -> FsFuture<'a, ()> {
        self.inner.set_accessed(path, tm)
    }

    fn set_modified<'a>(
        &'a self,
        path: &'a DavPath,
        tm: std::time::SystemTime,
    ) -> FsFuture<'a, ()> {
        self.inner.set_modified(path, tm)
    }

    fn have_props<'a>(
        &'a self,
        path: &'a DavPath,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        self.inner.have_props(path)
    }

    fn patch_props<'a>(
        &'a self,
        path: &'a DavPath,
        patch: Vec<(bool, dav_server::fs::DavProp)>,
    ) -> FsFuture<'a, Vec<(axum::http::StatusCode, dav_server::fs::DavProp)>> {
        self.inner.patch_props(path, patch)
    }

    fn get_props<'a>(
        &'a self,
        path: &'a DavPath,
        do_content: bool,
    ) -> FsFuture<'a, Vec<dav_server::fs::DavProp>> {
        self.inner.get_props(path, do_content)
    }

    fn get_prop<'a>(
        &'a self,
        path: &'a DavPath,
        prop: dav_server::fs::DavProp,
    ) -> FsFuture<'a, Vec<u8>> {
        self.inner.get_prop(path, prop)
    }

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        self.inner.get_quota()
    }
}

/// 纯转发的 `DavFile`：目前没有要改的行为，但 `open` 必须返回我们自己的类型，
/// 才能保证未来加「写临时文件再原子改名」时不用再动一次调用方。
struct CleanFile {
    inner: Box<dyn DavFile>,
}

impl fmt::Debug for CleanFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CleanFile")
    }
}

impl DavFile for CleanFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        self.inner.metadata()
    }

    fn write_buf(&'_ mut self, buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        self.inner.write_buf(buf)
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        self.inner.write_bytes(buf)
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        self.inner.read_bytes(count)
    }

    fn seek(&'_ mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        self.inner.seek(pos)
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        self.inner.flush()
    }

    fn redirect_url(&'_ mut self) -> FsFuture<'_, Option<String>> {
        self.inner.redirect_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_names_cover_parts_and_junk() {
        assert!(is_hidden(".asmoday-part.deadbeef"));
        assert!(is_hidden(".DS_Store"));
        assert!(is_hidden("Thumbs.db"));
        assert!(is_hidden(""));
        assert!(!is_hidden("photo.jpg"));
        assert!(!is_hidden("相册"));
    }

    #[test]
    fn denied_uses_last_segment() {
        let path = DavPath::new("/docs/.DS_Store").unwrap();
        assert!(hidden_by_name(&path));
        let ok = DavPath::new("/docs/a.txt").unwrap();
        assert!(!hidden_by_name(&ok));
        // 目录请求的 file_name 可能是空，不能被误判成「隐藏」
        let root = DavPath::new("/").unwrap();
        assert!(!hidden_by_name(&root));
    }

    /// 应用数据目录对 WebDAV 也必须「不存在」：
    /// 不拦的话客户端会去同步一个永远 403 的目录，缩略图缓存也会被当成普通图片。
    #[test]
    fn reserved_dir_is_denied_for_dav() {
        use dav_server::localfs::LocalFs;

        let base = std::env::temp_dir().join(format!(
            "asmoday-dav-reserved-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(base.join("data").join("thumbnails")).unwrap();
        std::fs::create_dir_all(base.join("photos")).unwrap();
        let root = std::fs::canonicalize(&base).unwrap();
        crate::core::reserved::register(&root.join("data"));

        let fs = CleanFs::new(
            LocalFs::new(&root, true, cfg!(windows), cfg!(target_os = "macos")),
            &root,
        );
        assert!(fs.is_reserved(&DavPath::new("/data/thumbnails/a.webp").unwrap()));
        assert!(fs.is_reserved(&DavPath::new("/data").unwrap()));
        assert!(!fs.is_reserved(&DavPath::new("/photos/a.jpg").unwrap()));

        let _ = std::fs::remove_dir_all(&base);
    }
}
