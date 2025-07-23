use shared::utils::{hex_decode, hex_encode};
use chrono::Utc;
use serde::{Deserialize, Serialize};

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize, Deserialize, Debug)]
struct AccessToken {
    ts: i64,
    ttl: i64,
    sig: String,
}

pub fn create_access_token(secret: &[u8; 32], ttl_secs: u16) -> String {
    let timestamp = Utc::now().timestamp();
    let timestamp_bytes = timestamp.to_le_bytes();
    let ttl_secs_bytes = ttl_secs.to_le_bytes();
    let hash = blake3::keyed_hash(secret, &timestamp_bytes);
    let signature = hex_encode(hash.as_bytes());
    format!("{}{}{signature}", hex_encode(&timestamp_bytes), hex_encode(&ttl_secs_bytes))
}

pub fn verify_access_token(token_str: &str, secret: &[u8; 32]) -> bool {
    if token_str.len() < 52 {
        return false;
    }

    let timestamp_bytes = hex_decode(&token_str[0..16]).unwrap_or_default();
    if timestamp_bytes.len() != 8 {
        return false;
    }

    let timestamp = i64::from_le_bytes(timestamp_bytes.try_into().unwrap_or([0; 8]));

    if timestamp == 0 {
        return false;
    }

    let ttl_bytes = hex_decode(&token_str[16..20]).unwrap_or_default();
    if ttl_bytes.len() != 2 {
        return false;
    }
    let ttl_secs = u16::from_le_bytes(ttl_bytes.try_into().unwrap_or([0; 2]));
    let signature = hex_decode(&token_str[20..]).unwrap_or_default();

    let current_timestamp = Utc::now().timestamp();
    if current_timestamp - timestamp > i64::from(ttl_secs) {
        return false;
    }

    let expected = blake3::keyed_hash(secret, &timestamp.to_le_bytes());
    constant_time_eq(expected.as_bytes(), &signature)
}

#[cfg(test)]
mod tests {
    use crate::auth::access_token::{create_access_token, verify_access_token};
    use std::thread;

    #[test]
    fn test_valid_token() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token(secret, 1);
        assert!(verify_access_token(token.as_str(), secret));
        thread::sleep(std::time::Duration::from_secs(2));
        assert!(!verify_access_token(token.as_str(), secret));
    }
}