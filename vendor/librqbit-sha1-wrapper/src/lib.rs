//! librqbit-sha1-wrapper 的本地替身（纯 Rust）。
//!
//! 上游 4.1.0 只提供两套实现：
//! - `sha1-crypto-hash` → `crypto-hash`，在 unix 上链接 openssl-sys；
//! - `sha1-ring`（名字骗人）→ `aws-lc-rs`，构建时需要 cmake/perl/nasm。
//!
//! 两者都会给「单文件 + musl 静态」的发版矩阵引入 C 工具链依赖：
//! cargo-zigbuild 交叉到 aarch64-musl 时 aws-lc-sys 的 cmake 构建拿不到
//! 正确的交叉编译器，Windows 侧也要额外装 nasm。SHA-1 在这里只是分块校验，
//! RustCrypto 的实现（本来就在依赖树里）吞吐足够，直接替换掉两套外部实现。
//!
//! 对外 API 与上游完全一致：`ISha1` trait + `Sha1` 类型别名，
//! 因此 librqbit / librqbit-bencode 不需要任何改动。

pub trait ISha1 {
    fn new() -> Self;
    fn update(&mut self, buf: &[u8]);
    fn finish(self) -> [u8; 20];
}

#[cfg(any(feature = "sha1-crypto-hash", feature = "sha1-ring"))]
mod rustcrypto_impl {
    use super::ISha1;
    use sha1::{Digest, Sha1 as Inner};

    pub struct Sha1RustCrypto {
        inner: Inner,
    }

    impl ISha1 for Sha1RustCrypto {
        fn new() -> Self {
            Self { inner: Inner::new() }
        }

        fn update(&mut self, buf: &[u8]) {
            self.inner.update(buf);
        }

        fn finish(self) -> [u8; 20] {
            let out = self.inner.finalize();
            let mut bytes = [0u8; 20];
            bytes.copy_from_slice(&out);
            bytes
        }
    }
}

#[cfg(any(feature = "sha1-crypto-hash", feature = "sha1-ring"))]
pub type Sha1 = rustcrypto_impl::Sha1RustCrypto;

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: [u8; 20]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_known_vector() {
        // FIPS 180 示例："abc"
        let mut hasher = Sha1::new();
        hasher.update(b"abc");
        assert_eq!(hex(hasher.finish()), "a9993e364706816aba3e25717850c26c9cd0d89d");

        // 分片喂入必须与一次性喂入等价（分块校验依赖这一点）
        let mut chunked = Sha1::new();
        for part in [b"a".as_slice(), b"b", b"c"] {
            chunked.update(part);
        }
        assert_eq!(hex(chunked.finish()), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
