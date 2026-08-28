use chrono::Utc;
use shared::utils::{hex_decode, hex_encode};
// `constant_time_eq` lives in `utils::crypto_utils`; re-exported here for the
// authentication call sites that have always used it under this path.
pub use tuliprox_core::utils::constant_time_eq;

// #[derive(Serialize, Deserialize, Debug)]
// struct AccessToken {
//     ts: i64,
//     ttl: i64,
//     sig: String,
// }

/// The capability an access token grants.
///
/// The signed payload used to be `(timestamp, ttl)` and nothing else, so every
/// valid token was valid everywhere a token was accepted - one token minted for
/// any purpose opened all of them. The scope is mixed into the keyed hash, so a
/// token minted for one capability does not verify against another.
///
/// A scope is a compile-time constant, never caller-supplied data: two sides of
/// the same handshake have to agree on the exact bytes or the signature fails,
/// so they must both name the same constant.
pub mod scope {
    /// The internal web player chain: the webplayer and recording stream
    /// entry points, the xtream handler they delegate to, and the
    /// custom-video-stream fallback the stream layer redirects into. One
    /// token travels that whole chain, so it is one scope.
    pub const INTERNAL_PLAYER: &str = "internal-player";
}

/// Build the bytes that get signed. Mint and verify must agree exactly.
fn signing_payload(timestamp_bytes: &[u8], ttl_bytes: &[u8], scope: &str) -> Vec<u8> {
    let scope_bytes = scope.as_bytes();
    let mut payload = Vec::with_capacity(timestamp_bytes.len() + ttl_bytes.len() + scope_bytes.len());
    payload.extend_from_slice(timestamp_bytes);
    payload.extend_from_slice(ttl_bytes);
    payload.extend_from_slice(scope_bytes);
    payload
}

pub fn create_access_token(secret: &[u8; 32], ttl_secs: u16, scope: &str) -> String {
    create_access_token_at(secret, ttl_secs, scope, Utc::now().timestamp())
}

fn create_access_token_at(secret: &[u8; 32], ttl_secs: u16, scope: &str, timestamp: i64) -> String {
    let timestamp_bytes = timestamp.to_le_bytes();
    let ttl_secs_bytes = ttl_secs.to_le_bytes();
    let hash = blake3::keyed_hash(secret, &signing_payload(&timestamp_bytes, &ttl_secs_bytes, scope));
    let signature = hex_encode(hash.as_bytes());
    format!("{}{}{signature}", hex_encode(&timestamp_bytes), hex_encode(&ttl_secs_bytes))
}

pub fn verify_access_token(token_str: &str, secret: &[u8; 32], scope: &str) -> bool {
    verify_access_token_at(token_str, secret, scope, Utc::now().timestamp())
}

fn verify_access_token_at(token_str: &str, secret: &[u8; 32], scope: &str, current_timestamp: i64) -> bool {
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

    let expected = blake3::keyed_hash(secret, &signing_payload(&timestamp_bytes, &ttl_bytes, scope));
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
    use crate::access_token::{create_access_token_at, scope, verify_access_token, verify_access_token_at};
    use std::panic::catch_unwind;

    const SCOPE: &str = scope::INTERNAL_PLAYER;

    #[test]
    fn test_valid_token() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 30, SCOPE, 1_700_000_000);
        assert!(verify_access_token_at(token.as_str(), secret, SCOPE, 1_700_000_030));
        assert!(!verify_access_token_at(token.as_str(), secret, SCOPE, 1_700_000_031));
        assert_ne!(token, create_access_token_at(secret, 30, SCOPE, 1_700_000_001));
    }

    #[test]
    fn test_expiry_check_handles_extreme_timestamps_without_overflow() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 30, SCOPE, i64::MIN);

        assert!(!verify_access_token_at(&token, secret, SCOPE, i64::MAX));
    }

    #[test]
    fn test_ttl_tampering_invalidates_token() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 1, SCOPE, 1_700_000_000);

        let mut tampered = token.clone();
        tampered.replace_range(16..20, "ffff");

        assert!(!verify_access_token(tampered.as_str(), secret, SCOPE));
    }

    #[test]
    fn test_token_does_not_verify_under_a_different_scope() {
        // The point of the scope: a token minted for one capability must not
        // open another. Before this, every valid token was valid everywhere.
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let token = create_access_token_at(secret, 30, SCOPE, 1_700_000_000);

        assert!(verify_access_token_at(&token, secret, SCOPE, 1_700_000_010));
        assert!(!verify_access_token_at(&token, secret, "some-other-capability", 1_700_000_010));
        assert!(!verify_access_token_at(&token, secret, "", 1_700_000_010));
    }

    #[test]
    fn test_verify_access_token_rejects_non_ascii_without_panicking() {
        let secret = b"37c30f739e83ba27b4c17b174c31f3a9";
        let invalid = "é".repeat(84);

        let result = catch_unwind(|| verify_access_token(&invalid, secret, SCOPE));

        assert!(result.is_ok(), "verification should not panic on non-ascii input");
        assert!(!result.unwrap_or(true));
    }
}
