use crate::model::{PlaylistItemType, UUIDType};
use base64::{engine::general_purpose, Engine};
use url::Url;

#[inline]
pub fn hash_bytes(bytes: &[u8]) -> UUIDType {
    UUIDType(blake3::hash(bytes).into())
}

/// generates a hash from a string
#[inline]
pub fn hash_string(text: &str) -> UUIDType {
    hash_bytes(text.as_bytes())
}

pub fn short_hash(text: &str) -> String {
    let hash = blake3::hash(text.as_bytes());
    hex_encode(&hash.as_bytes()[..8])
}

#[inline]
pub fn hex_encode(bytes: &[u8]) -> String {
    hex::encode_upper(bytes)
}

#[inline]
fn hex_nibble(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

pub fn hex_decode(hex_str: &str) -> Result<Vec<u8>, String> {
    let bytes = hex_str.as_bytes();
    if bytes.len() & 1 != 0 {
        return Err("hex string must have even length".to_string());
    }

    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i]).map_err(|()| format!("invalid hex at position {i}"))?;
        let lo = hex_nibble(bytes[i + 1]).map_err(|()| format!("invalid hex at position {}", i + 1))?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

pub fn hash_string_as_hex(url: &str) -> String {
    hex_encode(hash_string(url).as_ref())
}

/// Extracts the numeric ID from the last path segment of a URL.
/// Returns `Some(id)` only when the segment after the last `/` (before any extension)
/// is composed entirely of ASCII digits.
/// Example: `"http://srv.com/live/user/pass/950327.ts"` -> `Some(950327)`
pub fn extract_numeric_id_from_url(url: &str) -> Option<u32> {
    let bytes = url.as_bytes();

    // Trim trailing slashes
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }

    // Strip query string / fragment so "…/12345.ts?token=abc" and "…/12345#start" work correctly.
    if let Some(delim) = bytes[..end].iter().position(|&b| b == b'?' || b == b'#') {
        end = delim;
    }

    // Trim any trailing slashes that were before the query/fragment (e.g. "/123/?foo")
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }

    // Find the last '/' — the numeric id must follow one.
    let slash_pos = bytes[..end].iter().rposition(|&b| b == b'/')?;
    let segment = &bytes[slash_pos + 1..end];

    // Find the name part (before first '.').
    let name_end = segment.iter().position(|&b| b == b'.').unwrap_or(segment.len());
    if name_end == 0 {
        return None;
    }

    // Parse digits inline — single pass, no allocation.
    let mut value: u32 = 0;
    for &b in &segment[..name_end] {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(value)
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

pub fn generate_provider_playlist_uuid(key: &str, provider_id: &str, item_type: PlaylistItemType) -> UUIDType {
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.update(provider_id.as_bytes());
    hasher.update(item_type.as_str().as_bytes());
    UUIDType(hasher.finalize().into())
}

pub fn generate_local_playlist_uuid(key: &str, item_type: PlaylistItemType, url: &str) -> UUIDType {
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.update(item_type.as_str().as_bytes());

    if let Some(url_path) = url_path_and_more(url) {
        hasher.update(url_path.as_bytes());
    } else {
        hasher.update(url.as_bytes());
    }

    UUIDType(hasher.finalize().into())
}

pub fn generate_runtime_playlist_uuid(
    key: &str,
    provider_id: &str,
    item_type: PlaylistItemType,
    url: &str,
) -> UUIDType {
    if item_type.is_local() {
        generate_local_playlist_uuid(key, item_type, url)
    } else {
        generate_provider_playlist_uuid(key, provider_id, item_type)
    }
}

pub fn u32_to_base64(value: u32) -> String {
    let bytes = value.to_be_bytes();
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64_to_u32(encoded: &str) -> Option<u32> {
    let mut buf = [0u8; 4];
    let n = general_purpose::URL_SAFE_NO_PAD.decode_slice(encoded, &mut buf).ok()?;
    if n != 4 {
        return None;
    }
    Some(u32::from_be_bytes(buf))
}

pub fn parse_uuid_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 36 {
        return None;
    }

    let src = s.as_bytes();
    let mut out = [0u8; 16];
    let mut si = 0;
    let mut di = 0;

    while si < src.len() {
        if src[si] == b'-' {
            si += 1;
            continue;
        }
        if di >= 16 || si + 1 >= src.len() {
            return None;
        }
        let hi = hex_nibble(src[si]).ok()?;
        let lo = hex_nibble(src[si + 1]).ok()?;
        out[di] = (hi << 4) | lo;
        si += 2;
        di += 1;
    }

    if di == 16 {
        Some(out)
    } else {
        None
    }
}

pub fn create_alias_uuid(base_uuid: &UUIDType, mapping_id: &str) -> UUIDType {
    let mut hasher = blake3::Hasher::new();
    hasher.update(base_uuid.as_ref());
    hasher.update(mapping_id.as_bytes());
    UUIDType(hasher.finalize().into())
}

// === FNV-1a 32-bit hash for stable numeric IDs. ===
//
// Used when an upstream id is missing (e.g. M3U episodes, Stalker synthetic
// storage ids). Deterministic across runs; collisions are extremely rare but
// callers should still de-duplicate within a snapshot batch.

pub fn fnv1a_32(key: &str) -> u32 {
    let hash = key.bytes().fold(2_166_136_261_u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(16_777_619));
    hash.max(1)
}

/// Streaming FNV-1a over a sequence of string parts joined by `:` without
/// allocating an intermediate `String`. Equivalent to
/// `fnv1a_32(&parts.join(":"))` for any non-empty `parts`, with an empty
/// separator byte between consecutive parts.
pub fn fnv1a_32_parts(parts: &[&str]) -> u32 {
    let mut hash = 2_166_136_261_u32;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            hash ^= u32::from(b':');
            hash = hash.wrapping_mul(16_777_619);
        }
        for &byte in part.as_bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(16_777_619);
        }
    }
    hash.max(1)
}

pub fn stable_episode_storage_id(series_id: u32, season_number: u32, episode_id: &str, episode_number: u32) -> u32 {
    let key = format!("{series_id}:{season_number}:{episode_id}:{episode_number}");
    fnv1a_32(&key)
}

// === Season/episode extraction from a configured regex. ===
//
// The regex must expose named `season` and `episode` captures. Supported
// shapes include the compact `SxxEyy` / `NxNN` forms and the verbose
// `Season X Episode Y` / `Episode Y` forms. When the regex matches a
// shape without an explicit season (e.g. bare `Episode 5`), the season
// defaults to `1` so the caller still receives a usable pair.
//
// `crate::defaults::default_episode_pattern` exposes `EPISODE_PATTERN`
// as the user-facing default.

pub fn parse_season_episode(title: &str, pattern: &regex::Regex) -> Option<(u32, u32)> {
    let caps = pattern.captures(title)?;
    // The CONSTANTS pattern uses branch-specific capture names
    // (`season_s`/`episode_s`, `season_n`/`episode_n`,
    // `season_v`/`episode_vf`/`episode_vo`); the simpler DEFAULT
    // pattern uses `season`/`episode`. Whichever branch matched,
    // pick the first populated capture.
    let episode_str = caps
        .name("episode")
        .or_else(|| caps.name("episode_s"))
        .or_else(|| caps.name("episode_n"))
        .or_else(|| caps.name("episode_vf"))
        .or_else(|| caps.name("episode_vo"))?
        .as_str()
        .trim();
    let season_str = caps
        .name("season")
        .or_else(|| caps.name("season_s"))
        .or_else(|| caps.name("season_n"))
        .or_else(|| caps.name("season_v"))
        .map(|m| m.as_str().trim());

    // New-format patterns expose season and episode as separate
    // numeric captures. For legacy user-configured patterns that
    // still wrap `SxxEyy` inside a single named capture (e.g.
    // `(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2})`), fall back to
    // splitting the captured string.
    let (season, episode) = match (season_str, episode_str.parse::<u32>()) {
        (Some(s), Ok(e)) => (s.parse::<u32>().ok()?, e),
        (None, Ok(e)) => (1, e),
        (_, Err(_)) => parse_sxxeyy_fallback(episode_str)?,
    };
    Some((season, episode))
}

/// Legacy fallback: a user-configured pattern may still expose the
/// whole `SxxEyy` token under one named capture. Strip the leading
/// `S`/`s`, find the next letter, and parse the digit runs around it.
fn parse_sxxeyy_fallback(token: &str) -> Option<(u32, u32)> {
    let after_s = token.strip_prefix(|c: char| c.is_ascii_alphabetic()).unwrap_or(token);
    let e_idx = after_s.find(|c: char| c.is_ascii_alphabetic())?;
    let (s_str, e_str) = after_s.split_at(e_idx);
    let season = s_str.trim().parse().ok()?;
    let episode = e_str[1..].trim().parse().ok()?;
    Some((season, episode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{constants::EPISODE_PATTERN, CONSTANTS};
    use regex::Regex;

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

    #[test]
    fn numeric_url_id_fragment_no_extension() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/live/55555#start"), Some(55555));
    }

    #[test]
    fn numeric_url_id_trailing_slash_before_query() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/123/?foo=bar"), Some(123));
    }

    #[test]
    fn numeric_url_id_trailing_slash_before_fragment() {
        assert_eq!(extract_numeric_id_from_url("http://server.com/456/#section"), Some(456));
    }

    // ── hash_bytes / hash_string ─────────────────────────────────────

    #[test]
    fn hash_bytes_deterministic() {
        let a = hash_bytes(b"hello");
        let b = hash_bytes(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_bytes_different_input_different_output() {
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
    }

    #[test]
    fn hash_string_matches_hash_bytes() {
        assert_eq!(hash_string("test"), hash_bytes(b"test"));
    }

    // ── short_hash ───────────────────────────────────────────────────

    #[test]
    fn short_hash_deterministic() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
    }

    #[test]
    fn short_hash_length_is_16_hex_chars() {
        // 8 bytes -> 16 hex characters
        assert_eq!(short_hash("anything").len(), 16);
    }

    #[test]
    fn short_hash_different_input() {
        assert_ne!(short_hash("abc"), short_hash("xyz"));
    }

    // ── hex_encode / hex_decode ──────────────────────────────────────

    #[test]
    fn hex_encode_known_value() {
        assert_eq!(hex_encode(&[0xDE, 0xAD, 0xBE, 0xEF]), "DEADBEEF");
    }

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_decode_known_value() {
        assert_eq!(hex_decode("DEADBEEF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn hex_decode_lowercase() {
        assert_eq!(hex_decode("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn hex_decode_empty() {
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_decode_odd_length_error() {
        assert!(hex_decode("ABC").is_err());
    }

    #[test]
    fn hex_decode_invalid_char_error() {
        assert!(hex_decode("ZZZZ").is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let original = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
        assert_eq!(hex_decode(&hex_encode(&original)).unwrap(), original);
    }

    // ── hash_string_as_hex ───────────────────────────────────────────

    #[test]
    fn hash_string_as_hex_deterministic() {
        assert_eq!(hash_string_as_hex("url"), hash_string_as_hex("url"));
    }

    #[test]
    fn hash_string_as_hex_length() {
        // UUIDType is 32 bytes (blake3) -> 64 hex chars
        assert_eq!(hash_string_as_hex("test").len(), 64);
    }

    // ── get_provider_id ──────────────────────────────────────────────

    #[test]
    fn get_provider_id_parses_numeric_string() {
        assert_eq!(get_provider_id("12345", "http://x.com/other.ts"), Some(12345));
    }

    #[test]
    fn get_provider_id_falls_back_to_url() {
        assert_eq!(get_provider_id("not_a_number", "http://x.com/live/999.ts"), Some(999));
    }

    #[test]
    fn get_provider_id_none_when_both_fail() {
        assert_eq!(get_provider_id("abc", "http://x.com/live/hello.ts"), None);
    }

    #[test]
    fn get_provider_id_empty_string_falls_back() {
        assert_eq!(get_provider_id("", "http://x.com/42.ts"), Some(42));
    }

    #[test]
    fn get_provider_id_prefers_parsed_string() {
        // provider_id string wins even if URL has a different numeric id
        assert_eq!(get_provider_id("100", "http://x.com/live/200.ts"), Some(100));
    }

    // ── provider/local playlist uuid ────────────────────────────────

    #[test]
    fn generate_provider_playlist_uuid_deterministic() {
        let a = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
        let b = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
        assert_eq!(a, b);
    }

    #[test]
    fn generate_provider_playlist_uuid_different_key() {
        let a = generate_provider_playlist_uuid("k1", "1", PlaylistItemType::Live);
        let b = generate_provider_playlist_uuid("k2", "1", PlaylistItemType::Live);
        assert_ne!(a, b);
    }

    #[test]
    fn generate_local_playlist_uuid_uses_url_path() {
        let a = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/path/to/stream");
        let b = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://y.com/path/to/stream");
        assert_eq!(a, b);
    }

    #[test]
    fn generate_local_playlist_uuid_is_deterministic() {
        let a = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/stream");
        let b = generate_local_playlist_uuid("k", PlaylistItemType::LocalSeries, "http://x.com/stream");
        assert_eq!(a, b);
    }

    #[test]
    fn generate_local_playlist_uuid_keeps_local_series_info_and_episode_distinct() {
        let url = "file:///library/show/Season%201/Episode%2001.mkv";
        let series_info = generate_local_playlist_uuid("library", PlaylistItemType::LocalSeriesInfo, url);
        let episode = generate_local_playlist_uuid("library", PlaylistItemType::LocalSeries, url);

        assert_ne!(series_info, episode);
    }

    #[test]
    fn generate_provider_playlist_uuid_different_type() {
        let a = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Live);
        let b = generate_provider_playlist_uuid("k", "1", PlaylistItemType::Video);
        assert_ne!(a, b);
    }

    #[test]
    fn generate_runtime_playlist_uuid_dispatches_by_item_type() {
        let provider = generate_runtime_playlist_uuid("input", "100", PlaylistItemType::Series, "file:///ignored.mkv");
        let local =
            generate_runtime_playlist_uuid("input", "100", PlaylistItemType::LocalSeries, "file:///ignored.mkv");

        assert_ne!(provider, local);
    }

    // ── u32_to_base64 / base64_to_u32 ───────────────────────────────

    #[test]
    fn base64_roundtrip_zero() {
        assert_eq!(base64_to_u32(&u32_to_base64(0)), Some(0));
    }

    #[test]
    fn base64_roundtrip_max() {
        assert_eq!(base64_to_u32(&u32_to_base64(u32::MAX)), Some(u32::MAX));
    }

    #[test]
    fn base64_roundtrip_arbitrary() {
        assert_eq!(base64_to_u32(&u32_to_base64(950327)), Some(950327));
    }

    #[test]
    fn base64_to_u32_invalid_input() {
        assert_eq!(base64_to_u32("!!!"), None);
    }

    #[test]
    fn base64_to_u32_wrong_length() {
        assert_eq!(base64_to_u32("AAAAAAA"), None);
    }

    #[test]
    fn base64_to_u32_empty() {
        assert_eq!(base64_to_u32(""), None);
    }

    // ── parse_uuid_hex ───────────────────────────────────────────────

    #[test]
    fn parse_uuid_hex_valid() {
        let result = parse_uuid_hex("550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            result,
            Some([0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00])
        );
    }

    #[test]
    fn parse_uuid_hex_wrong_length() {
        assert_eq!(parse_uuid_hex("550e8400-e29b-41d4-a716"), None);
    }

    #[test]
    fn parse_uuid_hex_no_hyphens_wrong_format() {
        // 32 hex chars without hyphens -> length 32 ≠ 36
        assert_eq!(parse_uuid_hex("550e8400e29b41d4a716446655440000"), None);
    }

    #[test]
    fn parse_uuid_hex_invalid_hex_char() {
        assert_eq!(parse_uuid_hex("ZZZZZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZZZZZZZZZ"), None);
    }

    #[test]
    fn parse_uuid_hex_empty() {
        assert_eq!(parse_uuid_hex(""), None);
    }

    // ── create_alias_uuid ────────────────────────────────────────────

    #[test]
    fn create_alias_uuid_deterministic() {
        let base = hash_string("base");
        let a = create_alias_uuid(&base, "mapping1");
        let b = create_alias_uuid(&base, "mapping1");
        assert_eq!(a, b);
    }

    #[test]
    fn create_alias_uuid_different_mapping() {
        let base = hash_string("base");
        let a = create_alias_uuid(&base, "mapping1");
        let b = create_alias_uuid(&base, "mapping2");
        assert_ne!(a, b);
    }

    #[test]
    fn create_alias_uuid_different_base() {
        let a = create_alias_uuid(&hash_string("base1"), "m");
        let b = create_alias_uuid(&hash_string("base2"), "m");
        assert_ne!(a, b);
    }

    #[test]
    fn create_alias_uuid_differs_from_base() {
        let base = hash_string("base");
        let alias = create_alias_uuid(&base, "mapping");
        assert_ne!(base, alias);
    }

    /// Swapping `to_string()` for `as_str()` in the UUID hashers must not change a
    /// single byte — every persisted playlist UUID depends on it. Guards against a
    /// future `Display` impl that stops delegating to `as_str()`.
    #[test]
    fn item_type_label_is_hash_stable_against_display() {
        for item_type in [
            PlaylistItemType::Live,
            PlaylistItemType::LiveHls,
            PlaylistItemType::Video,
            PlaylistItemType::LocalVideo,
            PlaylistItemType::LocalSeries,
            PlaylistItemType::LocalSeriesInfo,
            PlaylistItemType::Series,
            PlaylistItemType::SeriesInfo,
            PlaylistItemType::Catchup,
        ] {
            assert_eq!(item_type.as_str(), item_type.to_string(), "{item_type:?}");

            let mut expected = blake3::Hasher::new();
            expected.update(b"key");
            expected.update(b"provider");
            expected.update(item_type.to_string().as_bytes());
            assert_eq!(
                generate_provider_playlist_uuid("key", "provider", item_type),
                UUIDType(expected.finalize().into()),
                "{item_type:?}"
            );
        }
    }

    #[test]
    fn fnv1a_32_is_deterministic_and_nonzero() {
        let a = fnv1a_32("input_a:Show Name:https://example.test/series/ep1");
        let b = fnv1a_32("input_a:Show Name:https://example.test/series/ep1");
        assert_eq!(a, b);
        assert!(fnv1a_32("") > 0);
        assert!(fnv1a_32("0") > 0);
        assert_ne!(a, fnv1a_32("input_a:Show Name:https://example.test/series/ep2"));
    }

    #[test]
    fn fnv1a_32_matches_known_vector() {
        // FNV-1a 32-bit of "" is 0x811c9dc5 (offset basis). Our implementation
        // forces the result to `>= 1` so the sentinel-zero guard kicks in.
        assert_eq!(fnv1a_32(""), 0x811c9dc5);
    }

    #[test]
    fn fnv1a_32_parts_matches_joined_string() {
        let joined = fnv1a_32("a:b:c");
        let parts = fnv1a_32_parts(&["a", "b", "c"]);
        assert_eq!(joined, parts);
        // Empty parts list still produces the offset-basis sentinel.
        assert_eq!(fnv1a_32_parts(&[]), fnv1a_32(""));
        // Single part produces no separator byte.
        assert_eq!(fnv1a_32_parts(&["only"]), fnv1a_32("only"));
    }

    #[test]
    fn stable_episode_storage_id_keys_separate_contexts() {
        // Same (series, season, episode_num, episode_id) tuple must hash
        // identically regardless of how the caller order is.
        let a = stable_episode_storage_id(1, 1, "10", 1);
        let b = stable_episode_storage_id(1, 1, "10", 1);
        assert_eq!(a, b);
        // Different episode_num must not collide.
        assert_ne!(a, stable_episode_storage_id(1, 1, "10", 2));
        // Different series must not collide.
        assert_ne!(a, stable_episode_storage_id(2, 1, "10", 1));
    }

    #[test]
    fn parse_season_episode_extracts_with_default_pattern() {
        let pattern = Regex::new(EPISODE_PATTERN).unwrap();
        assert_eq!(parse_season_episode("Show S02E05 [1080p]", &pattern), Some((2, 5)));
        assert_eq!(parse_season_episode("Show s8e2", &pattern), Some((8, 2)));
        assert_eq!(parse_season_episode("Show without episode", &pattern), None);
        // Pattern only matches the digits grouped after S and E.
        assert_eq!(parse_season_episode("Pilot", &pattern), None);
    }

    #[test]
    fn parse_season_episode_legacy_user_pattern_with_wrapped_episode_capture() {
        // A user-configured pattern that still wraps the whole
        // `SxxEyy` token inside a single named capture (the
        // pre-refactor format) must keep working — the function
        // falls back to splitting the captured string.
        let legacy = Regex::new(r".*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*").unwrap();
        assert_eq!(parse_season_episode("Show S02E05 [1080p]", &legacy), Some((2, 5)));
        assert_eq!(parse_season_episode("Show s8e2", &legacy), Some((8, 2)));
    }

    #[test]
    fn parse_season_episode_handles_verbose_and_fallback_shapes() {
        // The CONSTANTS regex covers the four historical shapes.
        let pattern = Regex::new(CONSTANTS.re_episode_code.as_str()).unwrap();
        // Verbose `Season X Episode Y`
        assert_eq!(parse_season_episode("Show - Season 2 Episode 5", &pattern), Some((2, 5)),);
        assert_eq!(parse_season_episode("Show Season 02 Episode 05", &pattern), Some((2, 5)),);
        // `NxNN`
        assert_eq!(parse_season_episode("Show 1x05", &pattern), Some((1, 5)));
        // Bare `Episode Y` falls back to season 1.
        assert_eq!(parse_season_episode("Show Episode 7", &pattern), Some((1, 7)));
        // No match → None.
        assert_eq!(parse_season_episode("Pilot", &pattern), None);
    }
}
