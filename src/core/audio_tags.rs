//! 音频标签读取（音乐库标题/艺术家/专辑 + 内嵌封面）。
//!
//! 解析交给 [`lofty`]：它是 Rust 生态里音频元数据的标准库，ID3v2（含 2.2/2.3/2.4）、
//! FLAC / Ogg Vorbis / Opus、MP4 / M4A、WAV、AIFF、APE、WavPack 全部覆盖，
//! 编码（UTF-16 / Latin-1 / Shift-JIS）与 APIC/PIC 布局的历史坑它都踩过了。
//!
//! 早先这里是自研的极简 ID3v2 解析（约 300 行）：只认 ID3v2，`.flac`、`.m4a`
//! 一律没有标签（表现为音乐库整页空白），ID3v2.2 的 3 字符帧与 UTF-16 BOM
//! 也要自己处理。换成成熟库后这些边界不用再维护，代码量还少了三分之二。

use std::path::Path;

use lofty::prelude::{Accessor, AudioFile, TaggedFileExt};
use lofty::tag::Tag;

/// 单张封面的大小上限，超过就当没有（不把几十 MB 的图吐给前端）
const MAX_COVER_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// 是否存在可用内嵌封面，前端据此决定要不要请求 `/api/media/cover`
    pub has_cover: bool,
}

impl TrackTags {
    /// 只在测试里用（正常流程直接读字段）
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none() && !self.has_cover
    }
}

/// 内嵌封面（设计文档 3.4：音乐库读取专辑/歌手/封面）。
#[derive(Debug, Clone)]
pub struct Cover {
    /// 已按魔数校正过的 MIME，保证直接能当 `Content-Type`
    pub mime: String,
    pub data: Vec<u8>,
}

/// 读取文件的音频标签；无标签或格式不认识时返回空结构（绝不报错）。
pub fn read_tags(path: &Path) -> TrackTags {
    let Some(tag) = open_tag(path) else {
        return TrackTags::default();
    };
    let title = clean(tag.title().as_deref());
    let artist = clean(tag.artist().as_deref());
    let album = clean(tag.album().as_deref());
    TrackTags {
        has_cover: pick_cover(&tag).is_some(),
        title,
        artist,
        album,
    }
}

/// 读音频时长（毫秒）。
///
/// 与 [`read_tags`] 分开是刻意的：那边 `read_properties(false)`，因为算时长要把音频流
/// 走一遍（MP3 缺 Xing 头时尤其慢），目录扫描每首歌来一次会拖垮整轮索引。
/// 这里只在「这一页真的要看时长」时调用，结果按 `(path, mtime, size)` 落
/// `media_durations` 表，第二次起零成本（见 `api::media::build_items`）。
pub fn read_duration_ms(path: &Path) -> Option<u64> {
    use lofty::config::ParseOptions;

    let mut options = ParseOptions::new();
    options.read_properties(true);
    let tagged = lofty::probe::Probe::open(path)
        .ok()?
        .options(options)
        .read()
        .ok()?;
    let ms = tagged.properties().duration().as_millis() as u64;
    (ms > 0).then_some(ms)
}

/// 读取内嵌封面；没有就返回 `None`。
pub fn read_cover(path: &Path) -> Option<Cover> {
    let tag = open_tag(path)?;
    let (mime, data) = pick_cover(&tag)?;
    Some(Cover { mime, data })
}

/// 打开文件并取「首选标签」。
///
/// 关掉 `read_properties`：默认会为了算时长（码率/采样率/总时长）把整个音频流
/// 走一遍，音乐库扫描是**每首歌一次**，大目录下这一项能占到九成耗时。
/// 我们只要标签与封面，不需要时长。
///
/// 一个容器里可能同时存在多个标签（MP3 既有 ID3v2 又有 APE，M4A 有 ilst），
/// `primary_tag()` 按格式给出最该用的那个，退回 `first_tag()` 兜住边角情况。
fn open_tag(path: &Path) -> Option<Tag> {
    use lofty::config::ParseOptions;

    let mut options = ParseOptions::new();
    options.read_properties(false);

    let tagged = lofty::probe::Probe::open(path)
        .ok()?
        .options(options)
        .read()
        .ok()?;
    tagged.primary_tag().or_else(|| tagged.first_tag()).cloned()
}

/// 选出可用的封面：优先「正面封面」，否则取第一张能识别的图。
fn pick_cover(tag: &Tag) -> Option<(String, Vec<u8>)> {
    use lofty::picture::PictureType;

    let pictures = tag.pictures();
    let preferred = pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;

    let data = preferred.data();
    if data.is_empty() || data.len() > MAX_COVER_BYTES {
        return None;
    }
    // 标签里的 MIME 经常写错（image/jpg、空串、jpeg），统一按魔数兜底
    let declared = preferred
        .mime_type()
        .map(|mime| mime.as_str().to_string())
        .unwrap_or_default();
    let mime = normalize_mime(&declared, data);
    if mime == "application/octet-stream" {
        return None;
    }
    Some((mime, data.to_vec()))
}

/// 去掉首尾空白与 NUL，空串按「没有」处理；顺手修历史乱码。
fn clean(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(repair_mojibake(trimmed).unwrap_or_else(|| trimmed.to_string()))
    }
}

/// 修复「内容按 GBK/UTF-8 写、帧头却声明 ISO-8859-1」的历史标签（v4.9.21）。
///
/// 国产抓轨/转换工具（EAC + LAME、千千静听之类）会把中文按 GBK 塞进 ID3v2
/// 文本帧，编码字节却填 `0x00`（规范含义是 ISO-8859-1）。lofty 严格按声明解码，
/// 于是「林肯公园」显示成「ÁÖ¿Ï¹«Ô°」—— 实测就是这么来的。
///
/// 这里只做**有把握**的回退：原串不含 CJK、每个字符都落在单字节范围、非 ASCII
/// 部分能按 GBK 双字节成对消费。`Motörhead` / `Café` 这类真·西欧文本过不了
/// 成对校验，不会被误伤；`Björk` 这种高字节不足两个的也直接放过。
fn repair_mojibake(text: &str) -> Option<String> {
    if text.chars().any(is_cjk) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len());
    let mut high = 0usize;
    for ch in text.chars() {
        let code = ch as u32;
        // 超出 U+00FF 说明不是「按 latin1 读错」，别自作聪明
        if code > 0xFF {
            return None;
        }
        if code > 0x7F {
            high += 1;
        }
        bytes.push(code as u8);
    }
    if high < 2 {
        return None;
    }

    // 情形一：UTF-8 被当 latin1 读（`ä¸­æ–‡` → 中文）
    if let Ok(utf8) = std::str::from_utf8(&bytes)
        && utf8 != text
        && utf8.chars().any(is_cjk)
    {
        return Some(utf8.to_string());
    }

    // 情形二：GBK/GB18030 被当 latin1 读（本项目的实况）
    if !gbk_pairs(&bytes) {
        return None;
    }
    let (decoded, _, had_errors) = encoding_rs::GBK.decode(&bytes);
    if had_errors || !decoded.chars().any(is_cjk) {
        return None;
    }
    Some(decoded.into_owned())
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// 非 ASCII 段必须是合法 GBK 双字节对：首字节 0x81-0xFE，尾字节 0x40-0xFE 且非 0x7F。
fn gbk_pairs(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < 0x80 {
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            return false;
        }
        let (lead, trail) = (bytes[i], bytes[i + 1]);
        if !(0x81..=0xFE).contains(&lead) || !(0x40..=0xFE).contains(&trail) || trail == 0x7F {
            return false;
        }
        i += 2;
    }
    true
}

/// 标签里的 MIME 经常写错（`image/jpg`、空串、`jpeg`），按文件魔数兜底。
fn normalize_mime(mime: &str, data: &[u8]) -> String {
    let declared = match mime.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "image/jpg" | "image/pjpeg" => Some("image/jpeg".to_string()),
        value if value.starts_with("image/") => Some(value.to_string()),
        _ => None,
    };

    declared.unwrap_or_else(|| {
        if data.starts_with(&[0xFF, 0xD8]) {
            "image/jpeg".to_string()
        } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png".to_string()
        } else if data.starts_with(b"GIF8") {
            "image/gif".to_string()
        } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
            "image/webp".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// ID3v2.3 文本帧（编码字节 3 = UTF-8）
    fn text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(3);
        payload.extend_from_slice(text.as_bytes());

        let mut frame = Vec::new();
        frame.extend_from_slice(id);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&payload);
        frame
    }

    fn picture_frame(picture: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(3); // UTF-8
        payload.extend_from_slice(b"image/jpeg\0");
        payload.push(3); // 封面（front cover）
        payload.push(0); // 空描述
        payload.extend_from_slice(picture);

        let mut frame = Vec::new();
        frame.extend_from_slice(b"APIC");
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&payload);
        frame
    }

    /// 一个最小可识别的 MPEG-1 Layer III 帧（128 kbps / 44.1 kHz / 无填充 → 417 字节）。
    ///
    /// 必须连写**至少三帧**：lofty 读到单帧会认为「不足以确认这是 MP3」而整体报错
    /// （实测 1 帧 → `FileParseError { ty: Some(Mpeg) }`，3 帧才正常解析出标签）。
    fn mpeg_frame() -> Vec<u8> {
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        frame
    }

    fn write_mp3(name: &str, tag_body: &[u8]) -> std::path::PathBuf {
        let size = tag_body.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[3, 0, 0]);
        // 同步安全整数（每字节 7 位）
        bytes.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        bytes.extend_from_slice(tag_body);
        for _ in 0..3 {
            bytes.extend_from_slice(&mpeg_frame());
        }

        let path = scratch_dir().join(name);
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    #[test]
    fn parses_title_artist_and_album() {
        let mut body = Vec::new();
        body.extend(text_frame(b"TIT2", "夜航"));
        body.extend(text_frame(b"TPE1", "Asmoday"));
        body.extend(text_frame(b"TALB", "Sessions"));

        let path = write_mp3("tags-basic.mp3", &body);
        let tags = read_tags(&path);
        assert_eq!(tags.title.as_deref(), Some("夜航"));
        assert_eq!(tags.artist.as_deref(), Some("Asmoday"));
        assert_eq!(tags.album.as_deref(), Some("Sessions"));
        assert!(!tags.has_cover);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detects_and_extracts_embedded_cover() {
        // 最小可识别的 JPEG 头 + 假数据，够验证解析与 MIME 校正
        let fake = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        let mut body = Vec::new();
        body.extend(text_frame(b"TIT2", "封面曲"));
        body.extend(picture_frame(&fake));

        let path = write_mp3("tags-cover.mp3", &body);
        let tags = read_tags(&path);
        assert!(tags.has_cover);

        let cover = read_cover(&path).expect("应当能取出封面");
        assert_eq!(cover.mime, "image/jpeg");
        assert_eq!(cover.data, fake);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn returns_empty_for_plain_file() {
        let path = scratch_dir().join("no-tags.bin");
        std::fs::write(&path, b"not an audio file at all").unwrap();
        assert!(read_tags(&path).is_empty());
        assert!(read_cover(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        // 扫描时文件可能刚好被删/被移走：这里必须安静返回空，不能 panic
        let path = scratch_dir().join("definitely-missing.mp3");
        let _ = std::fs::remove_file(&path);
        assert!(read_tags(&path).is_empty());
        assert!(read_cover(&path).is_none());
    }

    #[test]
    fn fixes_mislabelled_mime_by_magic_bytes() {
        // 标签写 image/jpg（常见的非标准写法）要归一成 image/jpeg；
        // 完全没写就按魔数猜；猜不出来就当没有封面。
        assert_eq!(normalize_mime("image/jpg", &[0xFF, 0xD8]), "image/jpeg");
        assert_eq!(normalize_mime("", &[0x89, b'P', b'N', b'G']), "image/png");
        assert_eq!(normalize_mime("", b"GIF89a"), "image/gif");
        assert_eq!(normalize_mime("", b"????"), "application/octet-stream");
    }

    /// ID3v2.3 文本帧：显式指定编码字节 + 原始字节（构造历史乱码样本用）
    fn raw_text_frame(id: &[u8; 4], encoding: u8, bytes: &[u8]) -> Vec<u8> {
        let mut payload = vec![encoding];
        payload.extend_from_slice(bytes);
        let mut frame = Vec::new();
        frame.extend_from_slice(id);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&payload);
        frame
    }

    #[test]
    fn repairs_gbk_declared_as_latin1() {
        // 实况：EAC + LAME 压的 MP3，TPE1 声明 ISO-8859-1，内容是 GBK 的「林肯公园」
        let mut body = raw_text_frame(b"TIT2", 0, b"Numb");
        body.extend(raw_text_frame(
            b"TPE1",
            0,
            &[0xC1, 0xD6, 0xBF, 0xCF, 0xB9, 0xAB, 0xD4, 0xB0],
        ));
        let path = write_mp3("tags-gbk.mp3", &body);
        let tags = read_tags(&path);
        assert_eq!(tags.artist.as_deref(), Some("林肯公园"));
        // LATIN-1 的纯 ASCII 帧不受影响
        assert_eq!(tags.title.as_deref(), Some("Numb"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repairs_utf8_declared_as_latin1() {
        // UTF-8 被当 latin1 读：`ä¸­æ–‡` 这种形态
        let body = raw_text_frame(b"TPE1", 0, "中文".as_bytes());
        let path = write_mp3("tags-utf8-latin1.mp3", &body);
        let tags = read_tags(&path);
        assert_eq!(tags.artist.as_deref(), Some("中文"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keeps_western_accents_intact() {
        // 真·西欧文本必须原样保留：成对校验过不了，不能瞎修
        let mut body = raw_text_frame(b"TPE1", 0, b"Mot\xF6rhead");
        body.extend(raw_text_frame(b"TALB", 0, b"Caf\xE9"));
        let path = write_mp3("tags-latin1.mp3", &body);
        let tags = read_tags(&path);
        assert_eq!(tags.artist.as_deref(), Some("Motörhead"));
        assert_eq!(tags.album.as_deref(), Some("Café"));
        let _ = std::fs::remove_file(&path);
    }
}
