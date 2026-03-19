use crate::model::{PlaylistItemType, UUIDType};
use base64::{engine::general_purpose, Engine};
use url::Url;

#[inline]
pub fn hash_bytes(bytes: &[u8]) -> UUIDType { UUIDType(blake3::hash(bytes).into()) }

/// generates a hash from a string
#[inline]
pub fn hash_string(text: &str) -> UUIDType { hash_bytes(text.as_bytes()) }

pub fn short_hash(text: &str) -> String {
    let hash = blake3::hash(text.as_bytes());
    hex_encode(&hash.as_bytes()[..8])
}

#[inline]
pub fn hex_encode(bytes: &[u8]) -> String { hex::encode_upper(bytes) }
pub fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("hex string must have even length".to_string());
    }

    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| format!("invalid hex at position {i}: {e}")))
        .collect()
}

pub fn hash_string_as_hex(url: &str) -> String { hex_encode(hash_string(url).as_ref()) }

/// Extracts the numeric ID from the last path segment of a URL.
/// Returns `Some(id)` only when the segment after the last `/` (before any extension)
/// is composed entirely of ASCII digits.
/// Example: `"http://srv.com/live/user/pass/950327.ts"` → `Some(950327)`
pub fn extract_numeric_id_from_url(url: &str) -> Option<u32> {
    let url_no_trailing = url.trim_end_matches('/');

    // Strip query string / fragment so "…/12345.ts?token=abc" works correctly.
    let path_end = url_no_trailing.find('?').unwrap_or(url_no_trailing.len());
    let path = &url_no_trailing[..path_end];

    // The numeric id must follow a '/' to avoid matching random digits.
    let slash_pos = path.rfind('/')?;
    let last_segment = &path[slash_pos + 1..];
    let name_part = last_segment.split('.').next().unwrap_or("");

    if !name_part.is_empty() && name_part.chars().all(|c| c.is_ascii_digit()) {
        name_part.parse::<u32>().ok()
    } else {
        None
    }
}

pub fn extract_id_from_url(url: &str) -> String {
    if let Some(id) = extract_numeric_id_from_url(url) {
        return id.to_string();
    }

    let url_no_trailing = url.trim_end_matches('/');
    let cleaned_for_hash = url_no_trailing.trim_start_matches("https://").trim_start_matches("http://");
    short_hash(cleaned_for_hash)
}

pub fn get_provider_id(provider_id: &str, url: &str) -> Option<u32> {
    provider_id.parse::<u32>().ok().or_else(|| extract_numeric_id_from_url(url))
}

fn url_path_and_more(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;

    let mut out = u.path().to_string();

    if let Some(q) = u.query() {
        out.push('?');
        out.push_str(q);
    }

    if let Some(f) = u.fragment() {
        out.push('#');
        out.push_str(f);
    }

    Some(out)
}

pub fn generate_playlist_uuid(key: &str, provider_id: &str, item_type: PlaylistItemType, url: &str) -> UUIDType {
    if provider_id.is_empty() || provider_id == "0" {
        if let Some(url_path) = url_path_and_more(url) {
            return hash_string(&url_path);
        }
    }
    hash_string(&format!("{key}{provider_id}{item_type}"))
}

pub fn u32_to_base64(value: u32) -> String {
    // big-endian is safer and more portable when you care about consistent ordering or cross-platform data
    let bytes = value.to_be_bytes();
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64_to_u32(encoded: &str) -> Option<u32> {
    let decoded = general_purpose::URL_SAFE_NO_PAD.decode(encoded).ok()?;

    if decoded.len() != 4 {
        return None;
    }

    let arr: [u8; 4] = decoded.as_slice().try_into().ok()?;
    Some(u32::from_be_bytes(arr))
}

pub fn parse_uuid_hex(s: &str) -> Option<[u8; 16]> {
    // Quick length check
    if s.len() != 36 {
        return None;
    }

    // Remove hyphens
    let mut buf = [0u8; 32];
    let mut j = 0;

    for &b in s.as_bytes() {
        if b == b'-' {
            continue;
        }
        if j >= 32 {
            return None;
        }
        buf[j] = b;
        j += 1;
    }

    if j != 32 {
        return None;
    }

    let decoded = hex::decode(buf).ok()?;
    decoded.try_into().ok()
}

pub fn create_alias_uuid(base_uuid: &UUIDType, mapping_id: &str) -> UUIDType {
    let mut data = Vec::with_capacity(base_uuid.len() + mapping_id.len());
    data.extend_from_slice(base_uuid.as_ref());
    data.extend_from_slice(mapping_id.as_bytes());
    hash_bytes(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_id_from_url ────────────────────────────────────────────

    #[test]
    fn numeric_id_with_ts_extension() {
        assert_eq!(extract_id_from_url("http://server.com/live/user/pass/950327.ts"), "950327");
    }

    #[test]
    fn numeric_id_with_mkv_extension() {
        assert_eq!(extract_id_from_url("http://server.com/movie/user/pass/12345.mkv"), "12345");
    }

    #[test]
    fn numeric_id_without_extension() {
        assert_eq!(extract_id_from_url("http://server.com/live/user/pass/950327"), "950327");
    }

    #[test]
    fn numeric_id_with_query_string() {
        assert_eq!(
            extract_id_from_url("http://server.com/live/user/pass/1905905.ts?expires=1712345678&token=abc"),
            "1905905"
        );
    }

    #[test]
    fn numeric_id_with_trailing_slash() {
        assert_eq!(extract_id_from_url("http://server.com/live/user/pass/42/"), "42");
    }

    #[test]
    fn non_numeric_last_segment_returns_hash() {
        let result = extract_id_from_url("http://server.com/live/user/pass/hello.ts");
        assert!(!result.chars().all(|c| c.is_ascii_digit()), "should be a hash, not a numeric id");
        assert!(!result.is_empty());
    }

    #[test]
    fn mixed_alphanumeric_segment_returns_hash() {
        let result = extract_id_from_url("http://server.com/live/user/pass/abc123.ts");
        assert!(!result.chars().all(|c| c.is_ascii_digit()), "mixed segment should not be treated as numeric id");
    }

    #[test]
    fn no_slash_returns_hash() {
        let result = extract_id_from_url("12345");
        assert!(!result.chars().all(|c| c.is_ascii_digit()), "bare number without slash must not be parsed as id");
    }

    #[test]
    fn empty_url_returns_hash() {
        let result = extract_id_from_url("");
        assert!(!result.is_empty());
    }

    #[test]
    fn only_slashes_returns_hash() {
        let result = extract_id_from_url("///");
        assert!(!result.is_empty());
    }

    #[test]
    fn numeric_id_after_scheme() {
        assert_eq!(extract_id_from_url("http://server.com/42"), "42");
    }

    #[test]
    fn same_url_same_hash() {
        let a = extract_id_from_url("http://server.com/stream/hello.m3u8");
        let b = extract_id_from_url("http://server.com/stream/hello.m3u8");
        assert_eq!(a, b, "same non-numeric url should produce identical hash");
    }

    #[test]
    fn different_url_different_hash() {
        let a = extract_id_from_url("http://a.com/hello.ts");
        let b = extract_id_from_url("http://b.com/world.ts");
        assert_ne!(a, b);
    }

    #[test]
    fn query_string_ignored_for_numeric_extraction() {
        // The query string should not interfere with numeric extraction
        assert_eq!(extract_id_from_url("http://server.com/99999.ts?token=xyz"), "99999");
    }

    #[test]
    fn fragment_after_numeric_id() {
        // Fragments don't contain '?' so they stay in the path — still numeric after '/'
        assert_eq!(extract_id_from_url("http://server.com/live/55555.ts#start"), "55555");
    }

    #[test]
    fn non_numeric_query() {
        assert_eq!(
            extract_id_from_url("http://server.com/player_api.php?username=123443&password=1000"),
            "7D2F521803B23A43"
        );
    }

    // ── extract_numeric_id_from_url ────────────────────────────────────

    #[test]
    fn numeric_url_id_standard() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/user/pass/950327.ts"), Some(950327));
    }

    #[test]
    fn numeric_url_id_no_extension() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/user/pass/42"), Some(42));
    }

    #[test]
    fn numeric_url_id_with_query() {
        assert_eq!(
            extract_numeric_id_from_url("http://server.com/live/user/pass/1905905.ts?token=abc&expires=999"),
            Some(1905905)
        );
    }

    #[test]
    fn numeric_url_id_trailing_slash() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/path/123/"), Some(123));
    }

    #[test]
    fn numeric_url_id_zero() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/0.ts"), Some(0));
    }

    #[test]
    fn numeric_url_id_non_numeric_segment() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/hello.ts"), None);
    }

    #[test]
    fn numeric_url_id_mixed_alphanumeric() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/abc123.ts"), None);
    }

    #[test]
    fn numeric_url_id_no_slash() {
        assert_eq!(extract_numeric_id_from_url("12345"), None);
    }

    #[test]
    fn numeric_url_id_empty() {
        assert_eq!(extract_numeric_id_from_url(""), None);
    }

    #[test]
    fn numeric_url_id_query_digits_not_matched() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/player_api.php?username=123"), None);
    }

    #[test]
    fn numeric_url_id_overflow_returns_none() {
        // u32::MAX = 4294967295, this exceeds it
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/99999999999.ts"), None);
    }

    #[test]
    fn numeric_url_id_fragment() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/55555.ts#start"), Some(55555));
    }
}
