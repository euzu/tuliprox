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

#[inline]
pub fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
        let hi = hex_digit(bytes[i]).ok_or_else(|| format!("invalid hex at position {i}"))?;
        let lo = hex_digit(bytes[i + 1]).ok_or_else(|| format!("invalid hex at position {}", i + 1))?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

pub fn hash_string_as_hex(url: &str) -> String { hex_encode(hash_string(url).as_ref()) }

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
        let hi = hex_digit(src[si])?;
        let lo = hex_digit(src[si + 1])?;
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
mod tests;
