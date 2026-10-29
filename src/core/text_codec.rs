//! 编辑器文本编解码（设计文档 3.2.2.1）：换行符 / 编码 / BOM 的探测、保留与回写。
//!
//! 三条硬规则：
//! 1. **换行符按文件原样保留**。读取时探出 LF / CRLF（混合时记 `mixed_eol`），
//!    传输给前端一律归一为 `\n`，保存时再还原成同一风格 —— 绝不能把 Linux 的
//!    `.sh` 存成 CRLF，那会让脚本在 Linux 上直接报 `^M: bad interpreter`。
//! 2. **编码 UTF-8 优先**，解不出来再按 GBK / GB18030 兜底（国内常见旧文件）；
//!    UTF-16 只在 BOM 明确声明时才认，不做盲目猜测。
//! 3. **BOM 有就剥、有就加回，没有绝不添加**。给 JSON / Shell 脚本私自加 BOM
//!    会让解析器直接崩；反过来剥掉却不加回，会悄悄改动文件字节。
//!
//! 回写时若目标编码表示不了正文（比如 GBK 文件里粘进了 emoji），返回
//! [`CodecError::Unmappable`] 交由调用方拒绝写盘 —— 绝不把乱码落盘。

/// 可编辑文本支持的字符编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    /// GBK（编码器覆盖 GB18030，够用于国内旧文件）
    Gbk,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    /// 对外字符串（接口字段 / 前端展示取值）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Gbk => "gbk",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
        }
    }

    /// 状态栏展示用的大写标签
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gbk => "GBK",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "utf-8" | "utf8" => Some(Self::Utf8),
            "gbk" | "gb2312" | "gb18030" => Some(Self::Gbk),
            "utf-16le" | "utf16le" => Some(Self::Utf16Le),
            "utf-16be" | "utf16be" => Some(Self::Utf16Be),
            _ => None,
        }
    }

    /// GBK 没有 BOM 概念；UTF-16 必有 BOM 才有意义（无 BOM 的 UTF-16 无法可靠识别）
    const fn supports_bom(self) -> bool {
        !matches!(self, Self::Gbk)
    }

    const fn codec(self) -> &'static encoding_rs::Encoding {
        match self {
            Self::Utf8 => encoding_rs::UTF_8,
            Self::Gbk => encoding_rs::GBK,
            Self::Utf16Le => encoding_rs::UTF_16LE,
            Self::Utf16Be => encoding_rs::UTF_16BE,
        }
    }

    /// 该编码的 BOM 字节
    const fn bom_bytes(self) -> &'static [u8] {
        match self {
            Self::Utf8 => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
            Self::Gbk => &[],
        }
    }
}

/// 文件换行符风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "lf",
            Self::Crlf => "crlf",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lf" | "\n" => Some(Self::Lf),
            "crlf" | "\r\n" => Some(Self::Crlf),
            _ => None,
        }
    }
}

/// 解码结果。`text` 的换行符已统一成 `\n`（回写时按 [`Self::eol`] 还原）。
#[derive(Debug, Clone)]
pub struct DecodedText {
    pub text: String,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub has_bom: bool,
    /// 原文同时存在两种换行符（保存后会统一成 `eol`）
    pub mixed_eol: bool,
}

/// 编解码失败：一律在写盘前抛出，不允许「尽力而为」地写坏文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// 含 NUL 字节，几乎不可能是文本
    Binary,
    /// UTF-8 与 GBK 都解不干净
    UnknownEncoding,
    /// 正文里有目标编码表示不了的字符
    Unmappable(Vec<char>),
}

impl CodecError {
    /// 给用户看的中文说明（前端直接把这句话显示在编辑器里）
    pub fn message(&self) -> String {
        match self {
            Self::Binary => "文件包含二进制字节（NUL），不能用文本编辑器打开".to_string(),
            Self::UnknownEncoding => {
                "无法识别的文本编码（既不是 UTF-8 也不是 GBK/GB18030），已拒绝以乱码打开"
                    .to_string()
            }
            Self::Unmappable(chars) => {
                let sample: String = chars.iter().collect();
                format!(
                    "这些字符无法用目标编码保存：{sample}。请先切换为 UTF-8 再保存，避免文件变成乱码"
                )
            }
        }
    }
}

/// 探测并解码（设计文档 3.2.2.1「读取」分支）。
pub fn decode(bytes: &[u8]) -> Result<DecodedText, CodecError> {
    // 1. UTF-16：只有带 BOM 才认，否则没有任何可靠判据
    if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(finish(
            decode_utf16(body, TextEncoding::Utf16Le)?,
            TextEncoding::Utf16Le,
            true,
        ));
    }
    if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(finish(
            decode_utf16(body, TextEncoding::Utf16Be)?,
            TextEncoding::Utf16Be,
            true,
        ));
    }

    // 2. UTF-8（可带 BOM：剥掉但记下来，保存时加回）
    if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let text = std::str::from_utf8(body)
            .map_err(|_| CodecError::UnknownEncoding)?
            .to_string();
        return Ok(finish(text, TextEncoding::Utf8, true));
    }

    // 3. 无 BOM：UTF-8 优先
    if let Ok(text) = std::str::from_utf8(bytes) {
        // 纯 ASCII 也走这条；utf8 成功即无需再猜
        if !bytes.contains(&0u8) {
            return Ok(finish(text.to_string(), TextEncoding::Utf8, false));
        }
    }

    // 4. NUL 字节 = 二进制（UTF-8 解码可能「成功」但内容无意义）
    if bytes.contains(&0u8) {
        return Err(CodecError::Binary);
    }

    // 5. GBK / GB18030 兜底
    let (text, had_errors) = encoding_rs::GBK.decode_without_bom_handling(bytes);
    if had_errors {
        return Err(CodecError::UnknownEncoding);
    }
    Ok(finish(text.into_owned(), TextEncoding::Gbk, false))
}

fn decode_utf16(body: &[u8], encoding: TextEncoding) -> Result<String, CodecError> {
    // 手写而非 encoding_rs：后者对 UTF-16 的编码方向支持不完整（输出会退化成
    // 单字节），解码方向也因此统一自己实现，避免两个方向行为不一致。
    if !body.len().is_multiple_of(2) {
        return Err(CodecError::UnknownEncoding);
    }
    let little = encoding == TextEncoding::Utf16Le;
    let units: Vec<u16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| CodecError::UnknownEncoding)
}

/// 统一下来：判 EOL → 归一为 `\n`
fn finish(text: String, encoding: TextEncoding, has_bom: bool) -> DecodedText {
    let (eol, mixed_eol) = detect_eol(&text);
    DecodedText {
        text: normalize_eol(&text),
        encoding,
        eol,
        has_bom,
        mixed_eol,
    }
}

/// 统计换行符：以多数派作为文件风格，同时报出是否混合。
pub fn detect_eol(text: &str) -> (Eol, bool) {
    let crlf = text.matches("\r\n").count();
    let lf_total = text.matches('\n').count();
    let cr_total = text.matches('\r').count();
    let lf = lf_total - crlf; // 独占 LF
    let cr = cr_total - crlf; // 老式 Mac 的独占 CR

    let eol = if crlf > lf { Eol::Crlf } else { Eol::Lf };
    let mixed = (crlf > 0 && lf > 0) || cr > 0;
    (eol, mixed)
}

/// 所有换行风格 → `\n`（前端与内部一律用 LF 处理，仅在写盘前还原）
fn normalize_eol(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// 按目标风格回写（设计文档 3.2.2.1「保存」分支）。
///
/// `text` 只需是任意换行风格：内部先归一再输出，避免前端把 `\r\n` 又乘一遍
/// （`\r\n` → `\r\r\n` 是这套逻辑最常见的经典 bug）。
pub fn encode(
    text: &str,
    encoding: TextEncoding,
    eol: Eol,
    bom: bool,
) -> Result<Vec<u8>, CodecError> {
    let lf = normalize_eol(text);
    let body_text = match eol {
        Eol::Lf => lf.clone(),
        Eol::Crlf => lf.replace('\n', "\r\n"),
    };

    if encoding == TextEncoding::Utf8 {
        let mut out = Vec::with_capacity(body_text.len() + 3);
        if bom {
            out.extend_from_slice(TextEncoding::Utf8.bom_bytes());
        }
        out.extend_from_slice(body_text.as_bytes());
        return Ok(out);
    }

    if matches!(encoding, TextEncoding::Utf16Le | TextEncoding::Utf16Be) {
        let little = encoding == TextEncoding::Utf16Le;
        let mut out = Vec::with_capacity(body_text.len() * 2 + 2);
        if bom {
            out.extend_from_slice(encoding.bom_bytes());
        }
        for unit in body_text.encode_utf16() {
            let bytes = if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            };
            out.extend_from_slice(&bytes);
        }
        return Ok(out);
    }

    let (encoded, _, had_errors) = encoding.codec().encode(&body_text);
    if had_errors {
        return Err(CodecError::Unmappable(unmappable_chars(&lf, encoding)));
    }

    let mut out = Vec::with_capacity(encoded.len() + 2);
    if bom && encoding.supports_bom() {
        out.extend_from_slice(encoding.bom_bytes());
    }
    out.extend_from_slice(encoded.as_ref());
    Ok(out)
}

/// 找出目标编码表示不了的字符（去重、最多 8 个，仅用于错误提示）
fn unmappable_chars(text: &str, encoding: TextEncoding) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for ch in text.chars().take(200_000) {
        if ch == '\n' || out.contains(&ch) {
            continue;
        }
        let (_, _, had_errors) = encoding.codec().encode(ch.encode_utf8(&mut [0u8; 4]));
        if had_errors {
            out.push(ch);
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GBK_ZHONGWEN: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4]; // 「中文」

    #[test]
    fn plain_utf8_without_bom_is_detected() {
        let decoded = decode("a\nb\n".as_bytes()).expect("utf8");
        assert_eq!(decoded.encoding, TextEncoding::Utf8);
        assert_eq!(decoded.eol, Eol::Lf);
        assert!(!decoded.has_bom);
        assert!(!decoded.mixed_eol);
        assert_eq!(decoded.text, "a\nb\n");
    }

    #[test]
    fn crlf_is_detected_and_normalized() {
        let decoded = decode(b"#!/bin/sh\r\necho hi\r\n").expect("crlf");
        assert_eq!(decoded.eol, Eol::Crlf);
        assert!(!decoded.mixed_eol);
        // 传输给前端时统一是 \n，前端 CodeMirror 不会看到 \r
        assert_eq!(decoded.text, "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn mixed_eol_reports_majority_and_flag() {
        let decoded = decode(b"a\r\nb\r\nc\n").expect("mixed");
        assert_eq!(decoded.eol, Eol::Crlf);
        assert!(decoded.mixed_eol);
        assert_eq!(decoded.text, "a\nb\nc\n");
    }

    #[test]
    fn lone_cr_is_treated_as_mixed() {
        let decoded = decode(b"a\rb\r\n").expect("cr");
        assert!(decoded.mixed_eol);
        assert_eq!(decoded.text, "a\nb\n");
    }

    #[test]
    fn utf8_bom_is_stripped_and_flagged() {
        let decoded = decode(b"\xEF\xBB\xBF{\"a\":1}\n").expect("bom");
        assert!(decoded.has_bom);
        assert_eq!(decoded.text, "{\"a\":1}\n");
        // 原样加回
        let bytes = encode(
            &decoded.text,
            decoded.encoding,
            decoded.eol,
            decoded.has_bom,
        )
        .unwrap();
        assert_eq!(bytes, b"\xEF\xBB\xBF{\"a\":1}\n");
    }

    #[test]
    fn bom_is_never_added_when_absent() {
        let decoded = decode(b"{\"a\":1}\n").expect("no bom");
        let bytes = encode(
            &decoded.text,
            decoded.encoding,
            decoded.eol,
            decoded.has_bom,
        )
        .unwrap();
        assert_eq!(bytes, b"{\"a\":1}\n");
    }

    #[test]
    fn gbk_fallback_decodes_chinese() {
        let mut raw = GBK_ZHONGWEN.to_vec();
        raw.push(b'\n');
        let decoded = decode(&raw).expect("gbk");
        assert_eq!(decoded.encoding, TextEncoding::Gbk);
        assert_eq!(decoded.text, "中文\n");
        assert!(!decoded.has_bom);
    }

    #[test]
    fn gbk_roundtrip_keeps_original_bytes() {
        let raw = [GBK_ZHONGWEN, b"\r\n"].concat();
        let decoded = decode(&raw).expect("gbk");
        let bytes = encode(
            &decoded.text,
            decoded.encoding,
            decoded.eol,
            decoded.has_bom,
        )
        .unwrap();
        assert_eq!(bytes, raw);
    }

    #[test]
    fn gbk_refuses_unmappable_characters() {
        let err = encode("中文😀", TextEncoding::Gbk, Eol::Lf, false).unwrap_err();
        assert_eq!(err, CodecError::Unmappable(vec!['😀']));
        assert!(err.message().contains("😀"));
    }

    #[test]
    fn utf16_bom_roundtrip() {
        let raw: Vec<u8> = [
            &[0xFF, 0xFE][..],
            &"hi\nthere\n"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>(),
        ]
        .concat();
        let decoded = decode(&raw).expect("utf16le");
        assert_eq!(decoded.encoding, TextEncoding::Utf16Le);
        assert!(decoded.has_bom);
        assert_eq!(decoded.text, "hi\nthere\n");
        let bytes = encode(
            &decoded.text,
            decoded.encoding,
            decoded.eol,
            decoded.has_bom,
        )
        .unwrap();
        assert_eq!(bytes, raw);
    }

    #[test]
    fn binary_content_is_rejected() {
        assert_eq!(
            decode(b"\x00\x01\x02binary").unwrap_err(),
            CodecError::Binary
        );
    }

    #[test]
    fn crlf_output_is_not_doubled() {
        // 正文里已经带 \r\n 也必须只写一份
        let bytes = encode("a\r\nb", TextEncoding::Utf8, Eol::Crlf, false).unwrap();
        assert_eq!(bytes, b"a\r\nb");
    }

    #[test]
    fn file_without_trailing_newline_is_untouched() {
        let decoded = decode(b"no newline").expect("utf8");
        let bytes = encode(
            &decoded.text,
            decoded.encoding,
            decoded.eol,
            decoded.has_bom,
        )
        .unwrap();
        assert_eq!(bytes, b"no newline");
    }

    #[test]
    fn encoding_aliases_parse() {
        assert_eq!(TextEncoding::parse("UTF8"), Some(TextEncoding::Utf8));
        assert_eq!(TextEncoding::parse("gb18030"), Some(TextEncoding::Gbk));
        assert_eq!(TextEncoding::parse("UTF-16LE"), Some(TextEncoding::Utf16Le));
        assert_eq!(TextEncoding::parse("koi8-r"), None);
    }
}
