use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};

const PROXY_SESSION_ID_LEN: usize = 22;
const PROXY_SESSION_ID_KEY_CONTEXT: &str = "tuliprox:hls-cache:proxy-session-id-key:v1";

/// Stable Tuliprox content identity for a live HLS source.
///
/// `stream_ref` is the immutable input-stream ID captured before target mapping.
/// Together with `input_id`, it identifies the origin content across targets;
/// target IDs and virtual IDs must never be used as `stream_ref`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HlsSessionKey {
    /// Internal ID of the configured Tuliprox input.
    pub input_id: u16,
    /// Exact, non-empty origin/provider stream ID represented as a string.
    pub stream_ref: String,
    /// Archive start timestamp; absent for live playback.
    pub archive_reference: Option<i64>,
    /// Opaque identity of the complete archive request; absent for live playback.
    pub archive_identity: Option<String>,
}

impl HlsSessionKey {
    /// Creates a key from the configured input ID and its immutable input-stream ID.
    ///
    /// Callers must pass the origin/provider ID captured before target mapping, not
    /// a target-specific or virtual ID.
    pub fn new(input_id: u16, stream_ref: impl Into<String>) -> Self {
        Self { input_id, stream_ref: stream_ref.into(), archive_reference: None, archive_identity: None }
    }

    pub const fn with_archive_reference(mut self, archive_reference: i64) -> Self {
        self.archive_reference = Some(archive_reference);
        self
    }

    pub fn with_archive_identity(mut self, archive_identity: impl Into<String>) -> Self {
        self.archive_identity = Some(archive_identity.into());
        self
    }

    pub fn canonical(&self) -> String {
        self.archive_reference.map_or_else(
            || format!("input:{}|hls|{}", self.input_id, self.stream_ref),
            |timestamp| {
                let base = format!("input:{}|hls|{}|archive|{timestamp}", self.input_id, self.stream_ref);
                match self.archive_identity.as_ref() {
                    Some(identity) => format!("{base}|{identity}"),
                    None => base,
                }
            },
        )
    }

    pub fn stable_value(&self) -> String {
        self.canonical()
    }
}

/// Public opaque lookup token for HLS proxy URLs.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProxySessionId(pub String);

/// Builds the public opaque proxy session token from the stable session key.
pub fn build_proxy_session_id(key: &HlsSessionKey, reverse_proxy_rewrite_secret: &[u8]) -> ProxySessionId {
    let hls_session_key = blake3::derive_key(PROXY_SESSION_ID_KEY_CONTEXT, reverse_proxy_rewrite_secret);
    let digest = blake3::keyed_hash(&hls_session_key, key.stable_value().as_bytes());
    let token = general_purpose::URL_SAFE_NO_PAD.encode(digest.as_bytes());
    ProxySessionId(token.chars().take(PROXY_SESSION_ID_LEN).collect())
}

/// Stand-in for a lease id in a manifest template, replaced per request.
///
/// The manifest rewriter emits it and the playback path substitutes it, so it
/// sits with the other identifier vocabulary rather than with either side.
pub const HLS_ACCESS_LEASE_ID_PLACEHOLDER: &str = "__hls_access_lease_id__";

#[cfg(test)]
mod tests {
    use super::{build_proxy_session_id, HlsSessionKey};

    #[test]
    fn live_hls_session_key_uses_tuliprox_input_and_stream_ref() {
        let first = HlsSessionKey::new(7, "80510");
        let second = HlsSessionKey::new(7, "80510");

        assert_eq!(first, second);
        assert_eq!(first.stable_value(), "input:7|hls|80510");
    }

    #[test]
    fn live_hls_session_key_changes_for_different_input_or_stream_ref() {
        let first = HlsSessionKey::new(7, "80510");
        let different_input = HlsSessionKey::new(8, "80510");
        let different_stream = HlsSessionKey::new(7, "80511");

        assert_ne!(first, different_input);
        assert_ne!(first, different_stream);
    }

    #[test]
    fn archive_hls_session_key_changes_for_different_archive_request() {
        let first = HlsSessionKey::new(7, "80510").with_archive_reference(1_784_898_000).with_archive_identity("first");
        let second =
            HlsSessionKey::new(7, "80510").with_archive_reference(1_784_898_000).with_archive_identity("second");

        assert_ne!(first, second);
        assert_ne!(first.stable_value(), second.stable_value());
    }

    #[test]
    fn live_hls_session_key_preserves_alphanumeric_input_stream_id() {
        let key = HlsSessionKey::new(7, "m3u-channel_A42");

        assert_eq!(key.stream_ref, "m3u-channel_A42");
        assert_eq!(key.stable_value(), "input:7|hls|m3u-channel_A42");
    }

    #[test]
    fn live_hls_session_key_does_not_contain_origin_or_provider_url_parts() {
        let key = HlsSessionKey::new(7, "80510");
        let stable = key.stable_value();

        assert!(!stable.contains("provider://"));
        assert!(!stable.contains("origin.example.com"));
        assert!(!stable.contains("user"));
        assert!(!stable.contains("pass"));
        assert!(!stable.contains(".m3u8"));
    }

    #[test]
    fn proxy_session_id_is_stable_for_same_key_and_secret() {
        let key = HlsSessionKey::new(7, "80510");
        let secret = b"0011223344556677";

        assert_eq!(build_proxy_session_id(&key, secret), build_proxy_session_id(&key, secret));
    }

    #[test]
    fn proxy_session_id_changes_for_different_secret() {
        let key = HlsSessionKey::new(7, "80510");

        assert_ne!(
            build_proxy_session_id(&key, b"0011223344556677"),
            build_proxy_session_id(&key, b"8899aabbccddeeff")
        );
    }

    #[test]
    fn proxy_session_id_is_truncated_to_opaque_token_length() {
        let key = HlsSessionKey::new(7, "80510");

        assert_eq!(build_proxy_session_id(&key, b"0011223344556677").0.len(), 22);
    }
}
