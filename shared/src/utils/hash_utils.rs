use base64::Engine;
use base64::engine::general_purpose;
use url::Url;
use crate::model::PlaylistItemType;
use crate::model::UUIDType;

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
pub fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("hex string must have even length".to_string());
    }

    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i+2], 16)
                .map_err(|e| format!("invalid hex at position {i}: {e}"))
        })
        .collect()
}

pub fn hash_string_as_hex(url: &str) -> String {
    hex_encode(hash_string(url).as_ref())
}

pub fn extract_id_from_url(url: &str) -> Option<String> {
    if let Some(possible_id_and_ext) = url.split('/').next_back() {
        return possible_id_and_ext.rfind('.').map_or_else(|| Some(possible_id_and_ext.to_string()), |index| Some(possible_id_and_ext[..index].to_string()));
    }
    None
}

pub fn get_provider_id(provider_id: &str, url: &str) -> Option<u32> {
    provider_id.parse::<u32>().ok().or_else(|| {
        extract_id_from_url(url)?.parse::<u32>().ok()
    })
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

    let arr: [u8; 4] = decoded
        .as_slice()
        .try_into().ok()?;
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