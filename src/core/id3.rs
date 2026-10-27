//! 极简 ID3v2 读取（音乐库标题/艺术家/专辑 + 内嵌封面）。
//!
//! 只解析 ID3v2.2/2.3/2.4 的文本帧与图片帧（`APIC` / `PIC`），
//! 不做编码猜测以外的处理，也不联网补全任何信息。

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// 标签体最多读这么多：正常标签几十 KB，带大封面的也不过 1–2 MB
const MAX_TAG_BYTES: usize = 8 * 1024 * 1024;
/// 单张封面的大小上限，超过就当没有（不把几十 MB 的图吐给前端）
const MAX_COVER_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// 是否存在内嵌封面（`APIC` / `PIC` 帧），前端据此决定要不要请求 `/api/media/cover`
    pub has_cover: bool,
}

impl TrackTags {
    /// 只在测试里用（正常流程直接读字段），避免非测试构建报 dead_code。
    #[allow(dead_code)]
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

/// 读取文件头部 ID3v2 标签；失败或无标签返回空结构。
pub fn read_tags(path: &Path) -> TrackTags {
    let mut tags = TrackTags::default();
    let Some((version, body)) = read_tag(path) else {
        return tags;
    };

    for (id, data) in frames(version, &body) {
        match id {
            b"TIT2" | b"TT2" => tags.title = decode_text(data),
            b"TPE1" | b"TP1" => tags.artist = decode_text(data),
            b"TALB" | b"TAL" => tags.album = decode_text(data),
            b"APIC" | b"PIC" => tags.has_cover = true,
            _ => {}
        }
    }

    tags
}

/// 读第一张内嵌封面；没有则 `None`。
pub fn read_cover(path: &Path) -> Option<Cover> {
    let (version, body) = read_tag(path)?;
    for (id, data) in frames(version, &body) {
        if id == b"APIC" || id == b"PIC" {
            if let Some(cover) = parse_picture(version, data) {
                return Some(cover);
            }
        }
    }
    None
}

/// 打开文件并读出标签体：返回 `(主版本号, 帧区字节)`。
fn read_tag(path: &Path) -> Option<(u8, Vec<u8>)> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }

    let version = header[3];
    let size = synchsafe(&header[6..10]);
    if size == 0 {
        return None;
    }

    let limit = size.min(MAX_TAG_BYTES) as u64;
    let mut body = Vec::with_capacity(limit.min(64 * 1024) as usize);
    file.take(limit).read_to_end(&mut body).ok()?;
    Some((version, body))
}

/// 遍历帧，返回 `(帧 ID, 帧数据)`。遇到填充区或坏帧即停止。
fn frames<'a>(version: u8, body: &'a [u8]) -> Vec<(&'a [u8], &'a [u8])> {
    let id_len = if version == 2 { 3 } else { 4 };
    let header_len = if version == 2 { 6 } else { 10 };
    let mut out = Vec::new();
    let mut cursor = 0usize;

    while cursor + header_len <= body.len() {
        let id = &body[cursor..cursor + id_len];
        // 填充区是全 0；帧 ID 只可能是大写字母与数字
        if id.iter().all(|byte| *byte == 0)
            || !id
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            break;
        }

        let size = if version == 2 {
            ((body[cursor + 3] as usize) << 16)
                | ((body[cursor + 4] as usize) << 8)
                | (body[cursor + 5] as usize)
        } else if version >= 4 {
            synchsafe(&body[cursor + 4..cursor + 8])
        } else {
            u32::from_be_bytes([
                body[cursor + 4],
                body[cursor + 5],
                body[cursor + 6],
                body[cursor + 7],
            ]) as usize
        };

        let start = cursor + header_len;
        if size == 0 || start + size > body.len() {
            break;
        }
        out.push((id, &body[start..start + size]));
        cursor = start + size;
    }

    out
}

/// ID3v2 的「同步安全整数」：每字节只有低 7 位有效。
fn synchsafe(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take(4)
        .fold(0usize, |acc, byte| (acc << 7) | (*byte as usize & 0x7f))
}

/// 首字节是编码标记：0=Latin1，1=UTF-16(BOM)，2=UTF-16BE，3=UTF-8
fn decode_text(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let encoding = data[0];
    let payload = &data[1..];

    let text = match encoding {
        0 => payload.iter().map(|byte| *byte as char).collect::<String>(),
        1 => decode_utf16(payload, true),
        2 => decode_utf16(payload, false),
        3 => String::from_utf8_lossy(payload).to_string(),
        _ => return None,
    };

    let trimmed = text.trim_matches('\0').trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn decode_utf16(payload: &[u8], with_bom: bool) -> String {
    let mut units: Vec<u16> = payload
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();

    if with_bom && !units.is_empty() {
        // 0xFFFE 表示小端
        if units[0] == 0xFFFE {
            units = payload
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            units.remove(0);
        } else if units[0] == 0xFEFF {
            units.remove(0);
        }
    }

    char::decode_utf16(units)
        .map(|item| item.unwrap_or('\u{FFFD}'))
        .collect()
}

/// 解析 `APIC`（2.3/2.4）与 `PIC`（2.2）帧。
///
/// 布局：编码(1) + MIME 或 3 字节格式 + 图片类型(1) + 描述(按编码终止) + 图片字节。
fn parse_picture(version: u8, data: &[u8]) -> Option<Cover> {
    if data.len() < 6 {
        return None;
    }
    let encoding = data[0];

    let (mime, rest) = if version == 2 {
        let mime = match &data[1..4] {
            b"JPG" => "image/jpeg",
            b"PNG" => "image/png",
            _ => "",
        };
        // 编码(1) + 格式(3) + 类型(1) 之后是描述与数据
        (mime.to_string(), &data[5..])
    } else {
        let nul = 1 + data[1..].iter().position(|byte| *byte == 0)?;
        let mime = String::from_utf8_lossy(&data[1..nul])
            .trim()
            .to_ascii_lowercase();
        // nul 处是 MIME 的终止符，紧跟一个图片类型字节，之后才是描述
        (mime, &data[nul + 1..])
    };

    let picture = skip_description(rest, encoding);
    if picture.is_empty() || picture.len() > MAX_COVER_BYTES {
        return None;
    }

    let mime = normalize_mime(&mime, picture);
    if mime == "application/octet-stream" {
        return None;
    }
    Some(Cover {
        mime,
        data: picture.to_vec(),
    })
}

/// 跳过描述字段（UTF-16 用双字节 0 终止，其余用单字节 0）。
fn skip_description(data: &[u8], encoding: u8) -> &[u8] {
    if encoding == 1 || encoding == 2 {
        let mut index = 0;
        while index + 1 < data.len() {
            if data[index] == 0 && data[index + 1] == 0 {
                return &data[index + 2..];
            }
            index += 2;
        }
        return &data[data.len()..];
    }
    match data.iter().position(|byte| *byte == 0) {
        Some(index) => &data[index + 1..],
        None => &data[data.len()..],
    }
}

/// 标签里的 MIME 经常写错（`image/jpg`、空串、`jpeg`），按文件魔数兜底。
fn normalize_mime(mime: &str, data: &[u8]) -> String {
    let declared = match mime {
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

    fn build_frame(id: &[u8], text: &str) -> Vec<u8> {
        let payload = text.as_bytes();
        let mut frame = Vec::new();
        let size = payload.len() as u32 + 1;
        frame.extend_from_slice(id);
        if id.len() == 3 {
            frame.extend_from_slice(&size.to_be_bytes()[1..]);
        } else {
            frame.extend_from_slice(&size.to_be_bytes());
        }
        if id.len() == 4 {
            frame.extend_from_slice(&[0, 0]); // flags
        }
        frame.push(3); // UTF-8
        frame.extend_from_slice(payload);
        frame
    }

    /// 构造一个 APIC 帧：UTF-8、MIME=image/jpeg、描述为空
    fn build_picture_frame(picture: &[u8]) -> Vec<u8> {
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

    fn write_mp3(name: &str, body: &[u8], version: u8) -> std::path::PathBuf {
        let size = body.len();
        let mut file_bytes = Vec::new();
        file_bytes.extend_from_slice(b"ID3");
        file_bytes.extend_from_slice(&[version, 0, 0]);
        file_bytes.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        file_bytes.extend_from_slice(body);

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        std::fs::write(&path, &file_bytes).unwrap();
        path
    }

    #[test]
    fn parses_id3v2_title_and_artist() {
        let mut body = Vec::new();
        body.extend(build_frame(b"TIT2", "夜航"));
        body.extend(build_frame(b"TPE1", "Asmoday"));
        body.extend(build_frame(b"TALB", "Sessions"));

        let path = write_mp3("id3-sample.mp3", &body, 3);
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
        body.extend(build_frame(b"TIT2", "封面曲"));
        body.extend(build_picture_frame(&fake));

        let path = write_mp3("id3-cover.mp3", &body, 3);
        let tags = read_tags(&path);
        assert!(tags.has_cover);

        let cover = read_cover(&path).unwrap();
        assert_eq!(cover.mime, "image/jpeg");
        assert_eq!(cover.data, fake);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_id3v22_frames() {
        // v2.2 帧头只有 6 字节，ID 是 3 字符
        let mut body = Vec::new();
        body.extend(build_frame(b"TT2", "旧标签"));
        body.extend(build_frame(b"TP1", "老格式"));

        let path = write_mp3("id3-v22.mp3", &body, 2);
        let tags = read_tags(&path);
        assert_eq!(tags.title.as_deref(), Some("旧标签"));
        assert_eq!(tags.artist.as_deref(), Some("老格式"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn returns_empty_for_plain_file() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("no-tags.bin");
        std::fs::write(&path, b"not an mp3").unwrap();
        assert!(read_tags(&path).is_empty());
        assert!(read_cover(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
