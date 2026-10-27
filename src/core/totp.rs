//! TOTP（RFC 6238）双因素认证，供「系统设置」的可选开启项使用。
//!
//! 只依赖 `hmac` + `sha1` + `data-encoding`，不引入任何外部服务。

use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// 生成 20 字节随机密钥，Base32 编码（Authenticator App 通用格式）
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 20];
    rand::Rng::fill(&mut rand::rng(), &mut bytes[..]);
    BASE32_NOPAD.encode(&bytes)
}

fn decode_secret(secret: &str) -> Option<Vec<u8>> {
    let normalized: String = secret
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let trimmed = normalized.trim_end_matches('=');
    BASE32_NOPAD.decode(trimmed.as_bytes()).ok()
}

/// 计算指定时间片（30 秒）的动态码
pub fn code_at(secret: &str, unix_secs: u64) -> Option<u32> {
    let key = decode_secret(secret)?;
    let counter = unix_secs / 30;

    let mut mac = HmacSha1::new_from_slice(&key).ok()?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();

    let offset = (digest[19] & 0x0f) as usize;
    let binary = ((digest[offset] as u32 & 0x7f) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | (digest[offset + 3] as u32);

    Some(binary % 1_000_000)
}

/// 当前时间片的动态码（调试/测试辅助）
#[allow(dead_code)]
pub fn current_code(secret: &str) -> Option<u32> {
    code_at(secret, crate::core::auth::now_secs() as u64)
}

/// 校验动态码，容忍前后各一个时间片（时钟漂移）
pub fn verify(secret: &str, code: u32, unix_secs: u64) -> bool {
    const STEP: u64 = 30;
    for delta in [-1i64, 0, 1] {
        let shifted = (unix_secs as i64 + delta * STEP as i64).max(0) as u64;
        if code_at(secret, shifted) == Some(code) {
            return true;
        }
    }
    false
}

/// 生成 otpauth:// 链接，前端可直接渲染成二维码（离线生成，不依赖外部服务）
pub fn otpauth_url(issuer: &str, account: &str, secret: &str) -> String {
    let label = format!("{issuer}:{account}");
    format!(
        "otpauth://totp/{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        urlencode(&label),
        secret,
        urlencode(issuer)
    )
}

fn urlencode(input: &str) -> String {
    input
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 附录 B 的 SHA1 测试向量（密钥 "12345678901234567890"）
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn matches_rfc6238_vectors() {
        // T=59s → 94287082（取低 6 位 287082）
        assert_eq!(code_at(RFC_SECRET, 59), Some(287082));
        // T=1111111109s → 07081804
        assert_eq!(code_at(RFC_SECRET, 1_111_111_109), Some(81804));
    }

    #[test]
    fn verify_allows_one_step_drift() {
        let secret = generate_secret();
        let now = 1_700_000_000u64;
        let code = code_at(&secret, now).unwrap();
        assert!(verify(&secret, code, now));
        assert!(verify(&secret, code, now + 30));
        assert!(!verify(&secret, code, now + 300));
    }

    #[test]
    fn secret_roundtrip_and_url() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        assert!(decode_secret(&secret).is_some());
        let url = otpauth_url("Asmoday", "admin", &secret);
        assert!(url.starts_with("otpauth://totp/Asmoday%3Aadmin?secret="));
    }
}
