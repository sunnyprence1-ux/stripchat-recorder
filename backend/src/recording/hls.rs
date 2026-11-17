//! HLS 播放列表解析与 Mouflon 解密 / HLS Playlist Parsing and Mouflon Decryption
//!
//! 解析 Stripchat 的 HLS m3u8 播放列表，提取分片 URL 和 fMP4 初始化段 URL。
//! 支持 Mouflon 加密系统：通过 SHA-256 密钥对分片 URL 进行 XOR 解密。
//!
//! Parses Stripchat's HLS m3u8 playlists, extracting segment URLs and fMP4 init segment URLs.
//! Supports the Mouflon encryption system: XOR-decrypts segment URLs using SHA-256 keys.

use crate::core::error::{AppError, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;

/// 用于从加密 URL 中提取加密字符串和序号的正则表达式。
/// Regex for extracting the encrypted string and sequence number from an encrypted URL.
static SEGMENT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_([^_]+)_(\d+(?:_part\d+)?)\.mp4(?:[?#].*)?").unwrap());

/// HLS 分片信息 / HLS segment information
#[derive(Debug, Clone)]
pub struct HlsSegment {
    /// 分片的完整 URL（已解密）/ Full segment URL (decrypted)
    pub url: String,
    /// 分片序号（用于去重）/ Segment sequence number (for deduplication)
    pub sequence: u32,
    /// Media duration declared by EXTINF, in seconds.
    pub duration_secs: f64,
}

/// 解析 HLS m3u8 播放列表，返回分片列表和 fMP4 初始化段 URL。
/// Parse an HLS m3u8 playlist, returning the segment list and fMP4 init segment URL.
///
/// # 参数 / Parameters
/// - `playlist`: m3u8 文本内容 / m3u8 text content
/// - `url_prefix`: 用于将相对路径转为绝对 URL 的前缀 / Prefix for converting relative paths to absolute URLs
/// - `mouflon_keys`: Mouflon 解密密钥表（pkey -> pdkey）/ Mouflon decryption key map (pkey -> pdkey)
///
/// # 返回值 / Returns
/// `(segments, init_url)` 元组 / Tuple of `(segments, init_url)`
pub fn parse_playlist(
    playlist: &str,
    url_prefix: &str,
    mouflon_keys: &HashMap<String, String>,
) -> Result<(Vec<HlsSegment>, Option<String>)> {
    let mut segments = Vec::new();
    let mut mp4_header_url = None;
    let mut current_pkey: Option<&str> = None;
    let mut duration_secs = 0.0;
    let mut media_sequence: Option<u64> = None;
    let mut pending_mouflon_uri: Option<String> = None;

    let lines: Vec<&str> = playlist.lines().collect();

    for line in &lines {
        if let Some(val) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if let Ok(seq) = val.trim().parse::<u64>() {
                media_sequence = Some(seq);
                break;
            }
        }
    }

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(value) = line.strip_prefix("#EXTINF:") {
            duration_secs = value
                .split(',')
                .next()
                .unwrap_or("")
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.0);
            continue;
        }

        // 解析 Mouflon 加密标签，获取当前 pkey 对应的解密密钥
        // Parse Mouflon encryption tag to get the decryption key for the current pkey
        if line.contains("#EXT-X-MOUFLON:PSCH") {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 4 {
                let pkey = parts[3];
                current_pkey = mouflon_keys.get(pkey).map(|s| s.as_str());
            }
            continue;
        }

        // 解析 Mouflon URI 标签（无论与 #EXTINF 的先后顺序均能正确缓存）
        // Parse Mouflon URI tag (cached reliably regardless of order relative to #EXTINF)
        if let Some(raw_url) = line.strip_prefix("#EXT-X-MOUFLON:URI:") {
            pending_mouflon_uri = Some(raw_url.trim().to_string());
            continue;
        }

        // 解析 fMP4 初始化段 URL（EXT-X-MAP）
        // Parse fMP4 init segment URL (EXT-X-MAP)
        if line.contains("EXT-X-MAP:URI")
            && let Some(start) = line.find('"')
            && let Some(end) = line[start + 1..].find('"')
        {
            let header_path = &line[start + 1..start + 1 + end];
            mp4_header_url = Some(if header_path.starts_with("http") {
                header_path.to_string()
            } else {
                format!("{}/{}", url_prefix, header_path)
            });
            continue;
        }

        if line.starts_with('#') {
            continue;
        }

        // 到达实际分片行，优先使用 Mouflon URI 标签（若存在），否则使用当前分片行 URL
        let url = if let Some(raw_url) = pending_mouflon_uri.take() {
            let encoded_url = if raw_url.starts_with("https://") {
                raw_url
            } else if raw_url.starts_with("//") {
                format!("https:{}", raw_url)
            } else {
                format!("https://{}", raw_url)
            };

            // 若有解密密钥则解密 URL，否则直接使用
            // Decrypt URL if key is available, otherwise use as-is
            if let Some(key) = current_pkey {
                decrypt_segment_url(&encoded_url, key).unwrap_or(encoded_url)
            } else {
                encoded_url
            }
        } else if line.starts_with("http") {
            line.to_string()
        } else {
            format!("{}/{}", url_prefix, line)
        };

        // 序号提取：优先提取 URL 中的真实分片号（源端生成，不受 CDN 切片或重放影响）
        // 若分片 URL 未包含序号，则遵循 RFC 8216 EXT-X-MEDIA-SEQUENCE 规范
        let sequence = if let Some(seq) = extract_sequence(&url) {
            seq
        } else if let Some(ms) = media_sequence {
            (ms + segments.len() as u64) as u32
        } else {
            segments.len() as u32
        };

        segments.push(HlsSegment {
            url,
            sequence,
            duration_secs,
        });
        duration_secs = 0.0;
    }

    Ok((segments, mp4_header_url))
}

/// 从完整 URL 中提取 URL 前缀（去掉最后一个路径段）。
/// Extract the URL prefix from a full URL (removes the last path segment).
pub fn get_url_prefix(url: &str) -> String {
    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() > 1 {
        parts[..parts.len() - 1].join("/")
    } else {
        url.to_string()
    }
}

/// 使用 SHA-256 密钥对 Mouflon 加密的分片 URL 进行 XOR 解密。
/// Decrypt a Mouflon-encrypted segment URL using XOR with a SHA-256 key.
///
/// 解密流程：提取加密字符串 → Base64 解码（反转后补齐）→ SHA-256(key) XOR 解密 → 替换回 URL
/// Decryption flow: extract encrypted string → Base64 decode (reversed + padded) → SHA-256(key) XOR decrypt → replace in URL
fn decrypt_segment_url(encoded_url: &str, key: &str) -> Result<String> {
    let captures = SEGMENT_REGEX
        .captures(encoded_url)
        .ok_or_else(|| AppError::Other("Cannot parse encrypted URL".to_string()))?;

    let encrypted_str = captures.get(1).unwrap().as_str();

    // 反转字符串并补齐 Base64 填充 / Reverse string and pad for Base64
    let mut reversed: String = encrypted_str.chars().rev().collect();
    while !reversed.len().is_multiple_of(4) {
        reversed.push('=');
    }

    let encrypted_bytes = STANDARD
        .decode(&reversed)
        .map_err(|e| AppError::Other(format!("Base64 decode error: {}", e)))?;

    // 使用 SHA-256(key) 作为 XOR 密钥流 / Use SHA-256(key) as XOR keystream
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let key_bytes = hasher.finalize();

    let decrypted: Vec<u8> = encrypted_bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key_bytes[i % key_bytes.len()])
        .collect();

    let decrypted_str = String::from_utf8_lossy(&decrypted);
    Ok(encoded_url.replace(encrypted_str, &decrypted_str))
}

/// 从分片 URL 的文件名或参数中提取序号（避免将前缀模型ID或清晰度标签误认为序号）。
/// Extract the sequence number from a segment URL's filename or query parameters.
fn extract_sequence(url: &str) -> Option<u32> {
    // 去除 URL 查询参数和片段标识符 / Strip query string and fragment
    let clean_url = url.split(['?', '#']).next().unwrap_or(url);
    let filename = clean_url.split('/').next_back()?;

    // 优先匹配标准分片正则中的序号 / Match sequence from SEGMENT_REGEX if possible
    if let Some(caps) = SEGMENT_REGEX.captures(filename) {
        if let Some(m) = caps.get(2) {
            let part_str = m.as_str().split('_').next().unwrap_or(m.as_str());
            if let Ok(seq) = part_str.parse::<u32>() {
                return Some(seq);
            }
        }
    }

    // 去除扩展名后，从后往前查找纯数字的段（避免将 1080p, 720p 等分辨率误识别为序号，且若有多段则跳过第一段模型ID）
    // Search backwards for purely numeric token (avoid mistaking resolution or model ID for sequence)
    let stem = filename.split('.').next().unwrap_or(filename);
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() > 1 {
        // 跳过 parts[0]（通常为模型ID或流名前缀），仅从后缀部分倒序寻找纯数字序号
        for &part in parts[1..].iter().rev() {
            if part.starts_with("part") && part[4..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(seq) = part.parse::<u32>() {
                    return Some(seq);
                }
            }
        }
    } else if let Some(&part) = parts.first() {
        if !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(seq) = part.parse::<u32>() {
                return Some(seq);
            }
        }
    }

    // 若文件名无序号，检查 URL 查询参数中是否包含明确的序号标记（如 ?seq=123 或 ?segment=123）
    if let Some(query) = url.split(['?', '#']).nth(1) {
        for param in query.split('&') {
            if let Some((k, v)) = param.split_once('=') {
                if (k.eq_ignore_ascii_case("seq")
                    || k.eq_ignore_ascii_case("segment")
                    || k.eq_ignore_ascii_case("chunk"))
                    && !v.is_empty()
                    && v.chars().all(|c| c.is_ascii_digit())
                {
                    if let Ok(seq) = v.parse::<u32>() {
                        return Some(seq);
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_sequence_standard() {
        assert_eq!(extract_sequence("https://cdn.example.com/stream_12345.mp4"), Some(12345));
        assert_eq!(extract_sequence("https://cdn.example.com/hls/model_100.ts"), Some(100));
    }

    #[test]
    fn test_extract_sequence_with_query_and_token() {
        // Query param contains underscores
        let url = "https://b-hls-12.strpst.com/hls/123456/123456_fmp4_abc_789.mp4?hdnts=exp=1730000000_acl=*~data=123";
        assert_eq!(extract_sequence(url), Some(789));
    }

    #[test]
    fn test_extract_sequence_with_part_suffix() {
        let url = "https://cdn.example.com/hls/stream_456_part1.mp4";
        assert_eq!(extract_sequence(url), Some(456));
    }

    #[test]
    fn test_extract_sequence_resolution_not_sequence() {
        assert_eq!(extract_sequence("https://cdn.example.com/hls/model_1080p.mp4"), None);
        assert_eq!(extract_sequence("https://cdn.example.com/hls/12345_1080p.mp4"), None);
        assert_eq!(extract_sequence("https://cdn.example.com/hls/model_720p_456.mp4"), Some(456));
        assert_eq!(extract_sequence("https://cdn.example.com/hls/12345_720p_456.mp4"), Some(456));
        assert_eq!(extract_sequence("https://cdn.example.com/hls/stream.mp4?seq=789"), Some(789));
        assert_eq!(extract_sequence("https://cdn.example.com/hls/chunk.mp4?segment=999"), Some(999));
    }

    #[test]
    fn test_parse_playlist_media_sequence_and_extinf() {
        let m3u8 = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:500
#EXT-X-MAP:URI="init.mp4"
#EXTINF:2.000,
seg1.mp4
#EXTINF:1.850,
seg2.mp4
"#;
        let keys = HashMap::new();
        let (segments, init) = parse_playlist(m3u8, "https://cdn.example.com", &keys).unwrap();
        assert_eq!(init, Some("https://cdn.example.com/init.mp4".to_string()));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].duration_secs, 2.0);
        assert_eq!(segments[0].sequence, 500);
        assert_eq!(segments[1].duration_secs, 1.85);
        assert_eq!(segments[1].sequence, 501);
    }

    #[test]
    fn test_parse_playlist_mouflon_uri_order() {
        let m3u8 = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA-SEQUENCE:100
#EXT-X-MOUFLON:URI:https://cdn.example.com/mouflon_seg1.mp4
#EXTINF:2.000,
fallback_seg1.mp4
#EXTINF:1.500,
#EXT-X-MOUFLON:URI:https://cdn.example.com/mouflon_seg2.mp4
fallback_seg2.mp4
"#;
        let keys = HashMap::new();
        let (segments, _) = parse_playlist(m3u8, "https://cdn.example.com", &keys).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].url, "https://cdn.example.com/mouflon_seg1.mp4");
        assert_eq!(segments[0].sequence, 100);
        assert_eq!(segments[0].duration_secs, 2.0);
        assert_eq!(segments[1].url, "https://cdn.example.com/mouflon_seg2.mp4");
        assert_eq!(segments[1].sequence, 101);
        assert_eq!(segments[1].duration_secs, 1.5);
    }
}
