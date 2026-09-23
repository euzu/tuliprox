//! Encoding and decoding of authenticated resource links.
//!
//! Two legacy encodings predate this module and are neither confidential nor authenticated:
//! `/resource/epg/...` uses `obscure_text`, the Web UI resource route uses `obfuscate_text`.
//! Both carry a bare URL, so a token cannot be traced back to the input that supplied the URL and
//! every legacy link is treated as public-only. New links use the payload defined here, which
//! carries the origin and is protected by the existing authenticated helper.

use crate::utils::crypto_utils::{deobscure_authenticated_bytes, obscure_authenticated_bytes};
use shared::model::ResourceToken;
use std::fmt;

/// Prefix that discriminates a new token from a legacy encoding. It marks the token type, not a
/// payload version: the payload version stays in the authenticated envelope.
pub const RESOURCE_TOKEN_PREFIX: &str = "a1_";

const RESOURCE_TOKEN_DOMAIN: &[u8] = b"tuliprox.resource-token.v1";

/// Largest accepted token. Checked against the raw string before base64 decoding, so an oversized
/// token is rejected before it allocates or deserializes anything.
pub const MAX_RESOURCE_TOKEN_BYTES: usize = 16384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceTokenError {
    TooLarge,
    MissingPrefix,
    Invalid,
}

impl fmt::Display for ResourceTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("resource token exceeds the size limit"),
            Self::MissingPrefix => f.write_str("resource token has no token prefix"),
            Self::Invalid => f.write_str("resource token is malformed or not authenticated"),
        }
    }
}

impl std::error::Error for ResourceTokenError {}

/// True when the encoded value is a new authenticated token rather than a legacy encoding.
pub fn has_resource_token_prefix(encoded: &str) -> bool { encoded.starts_with(RESOURCE_TOKEN_PREFIX) }

/// Encodes a token, or reports that the payload does not fit the accepted size.
pub fn encode_resource_token(secret: &[u8; 16], token: &ResourceToken) -> Result<String, ResourceTokenError> {
    let payload = rmp_serde::to_vec(token).map_err(|_| ResourceTokenError::Invalid)?;
    let encoded = obscure_authenticated_bytes(secret, RESOURCE_TOKEN_DOMAIN, &payload)
        .map_err(|_| ResourceTokenError::Invalid)?;
    if encoded.len() + RESOURCE_TOKEN_PREFIX.len() > MAX_RESOURCE_TOKEN_BYTES {
        return Err(ResourceTokenError::TooLarge);
    }
    Ok(format!("{RESOURCE_TOKEN_PREFIX}{encoded}"))
}

pub fn decode_resource_token(secret: &[u8; 16], encoded: &str) -> Result<ResourceToken, ResourceTokenError> {
    if encoded.len() > MAX_RESOURCE_TOKEN_BYTES {
        return Err(ResourceTokenError::TooLarge);
    }
    let payload = encoded.strip_prefix(RESOURCE_TOKEN_PREFIX).ok_or(ResourceTokenError::MissingPrefix)?;
    if payload.is_empty() {
        return Err(ResourceTokenError::Invalid);
    }
    let bytes = deobscure_authenticated_bytes(secret, RESOURCE_TOKEN_DOMAIN, payload)
        .map_err(|_| ResourceTokenError::Invalid)?;
    rmp_serde::from_slice(&bytes).map_err(|_| ResourceTokenError::Invalid)
}

pub fn resource_token(resource: &str) -> ResourceToken { ResourceToken { resource: resource.to_string() } }

#[cfg(test)]
mod tests {
    use super::{
        decode_resource_token, encode_resource_token, has_resource_token_prefix, resource_token, ResourceTokenError,
        MAX_RESOURCE_TOKEN_BYTES, RESOURCE_TOKEN_PREFIX,
    };
    use rand::Rng;

    fn random_secret() -> [u8; 16] { rand::rng().random() }

    #[test]
    fn token_round_trip_keeps_url_and_origin() {
        let secret = random_secret();
        let token = resource_token("resource://v1/example");
        let encoded = encode_resource_token(&secret, &token).expect("encode");

        assert!(has_resource_token_prefix(&encoded));
        assert_eq!(decode_resource_token(&secret, &encoded).expect("decode"), token);
    }

    #[test]
    fn raw_public_resource_round_trips() {
        let secret = random_secret();
        let token = resource_token("https://cdn.example.com/logo.png");
        let encoded = encode_resource_token(&secret, &token).expect("encode");
        let decoded = decode_resource_token(&secret, &encoded).expect("decode");

        assert_eq!(decoded.resource, "https://cdn.example.com/logo.png");
    }

    #[test]
    fn tampered_or_foreign_tokens_are_rejected() {
        let secret = random_secret();
        let token = resource_token("https://cdn.example.com/logo.png");
        let encoded = encode_resource_token(&secret, &token).expect("encode");

        let mut tampered = encoded.clone();
        let last = tampered.pop().expect("token char");
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert_eq!(decode_resource_token(&secret, &tampered), Err(ResourceTokenError::Invalid));

        // A different secret must not authenticate the same payload.
        assert_eq!(decode_resource_token(&random_secret(), &encoded), Err(ResourceTokenError::Invalid));

        // A legacy encoding has no prefix and is never decoded as a token.
        let legacy = shared::utils::obfuscate_text(&secret, "https://cdn.example.com/logo.png");
        assert_eq!(decode_resource_token(&secret, &legacy), Err(ResourceTokenError::MissingPrefix));
    }

    #[test]
    fn oversized_tokens_are_rejected_before_decoding() {
        let secret = random_secret();
        let oversized = format!("{RESOURCE_TOKEN_PREFIX}{}", "A".repeat(MAX_RESOURCE_TOKEN_BYTES));
        assert_eq!(decode_resource_token(&secret, &oversized), Err(ResourceTokenError::TooLarge));

        let long_url = format!("https://cdn.example.com/{}", "a".repeat(MAX_RESOURCE_TOKEN_BYTES));
        assert_eq!(encode_resource_token(&secret, &resource_token(&long_url)), Err(ResourceTokenError::TooLarge));
    }

    #[test]
    fn empty_payload_is_rejected() {
        let secret = random_secret();
        assert_eq!(decode_resource_token(&secret, RESOURCE_TOKEN_PREFIX), Err(ResourceTokenError::Invalid));
    }
}
