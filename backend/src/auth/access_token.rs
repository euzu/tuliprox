use shared::utils::{hex_decode, hex_encode};
use chrono::Utc;

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// #[derive(Serialize, Deserialize, Debug)]
// struct AccessToken {
//     ts: i64,
//     ttl: i64,
//     sig: String,
// }

pub fn create_access_token(secret: &[u8; 32], ttl_secs: u16) -> String {
    create_access_token_at(secret, ttl_secs, Utc::now().timestamp())
}

fn create_access_token_at(secret: &[u8; 32], ttl_secs: u16, timestamp: i64) -> String {
    let timestamp_bytes = timestamp.to_le_bytes();
    let ttl_secs_bytes = ttl_secs.to_le_bytes();
    let mut payload = Vec::with_capacity(timestamp_bytes.len() + ttl_secs_bytes.len());
    payload.extend_from_slice(&timestamp_bytes);
    payload.extend_from_slice(&ttl_secs_bytes);
    let hash = blake3::keyed_hash(secret, &payload);
    let signature = hex_encode(hash.as_bytes());
    format!("{}{}{signature}", hex_encode(&timestamp_bytes), hex_encode(&ttl_secs_bytes))
}

pub fn verify_access_token(token_str: &str, secret: &[u8; 32]) -> bool {
    verify_access_token_at(token_str, secret, Utc::now().timestamp())
}

fn verify_access_token_at(token_str: &str, secret: &[u8; 32], current_timestamp: i64) -> bool {
    const TOKEN_LEN: usize = 84;
    const TIMESTAMP_END: usize = 16;
    const TTL_END: usize = 20;

    if token_str.len() != TOKEN_LEN {
        return false;
    }
    if !token_str.is_ascii() || !token_str.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }

    let timestamp_bytes = hex_decode(&token_str[..TIMESTAMP_END]).unwrap_or_default();
    if timestamp_bytes.len() != 8 {
        return false;
    }

    let timestamp = i64::from_le_bytes(timestamp_bytes.as_slice().try_into().unwrap_or([0; 8]));

    if timestamp == 0 {
        return false;
    }

    let ttl_bytes = hex_decode(&token_str[TIMESTAMP_END..TTL_END]).unwrap_or_default();
    if ttl_bytes.len() != 2 {
        return false;
    }
    let ttl_secs = u16::from_le_bytes(ttl_bytes.as_slice().try_into().unwrap_or([0; 2]));
    let signature = hex_decode(&token_str[TTL_END..]).unwrap_or_default();

    let mut payload = Vec::with_capacity(timestamp_bytes.len() + ttl_bytes.len());
    payload.extend_from_slice(&timestamp_bytes);
    payload.extend_from_slice(&ttl_bytes);
    let expected = blake3::keyed_hash(secret, &payload);
    if !constant_time_eq(expected.as_bytes(), &signature) {
        return false;
    }

    if current_timestamp.saturating_sub(timestamp) > i64::from(ttl_secs) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::auth::access_token::{create_access_token_at, verify_access_token, verify_access_token_at};
    use std::panic::catch_unwind;

    #[test]
    fn test_valid_token() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 30, 1_700_000_000);
        assert!(verify_access_token_at(token.as_str(), secret, 1_700_000_030));
        assert!(!verify_access_token_at(token.as_str(), secret, 1_700_000_031));
        assert_ne!(token, create_access_token_at(secret, 30, 1_700_000_001));
    }

    #[test]
    fn test_expiry_check_handles_extreme_timestamps_without_overflow() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 30, i64::MIN);

        assert!(!verify_access_token_at(&token, secret, i64::MAX));
    }

    #[test]
    fn test_ttl_tampering_invalidates_token() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 1, 1_700_000_000);

        let mut tampered = token.clone();
        tampered.replace_range(16..20, "ffff");

        assert!(!verify_access_token(tampered.as_str(), secret));
    }

    #[test]
    fn test_verify_access_token_rejects_non_ascii_without_panicking() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let invalid = "é".repeat(84);

        let result = catch_unwind(|| verify_access_token(&invalid, secret));

        assert!(result.is_ok(), "verification should not panic on non-ascii input");
        assert!(!result.unwrap_or(true));
    }
}
