use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;
const PROXY_SESSION_ID_LEN: usize = 22;
const PROXY_SESSION_ID_HMAC_KEY_DOMAIN: &[u8] = b"tuliprox:hls-cache:proxy-session-id-key:v1";

/// Stable Tuliprox content identity for a live HLS source.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HlsSessionKey {
    pub input_id: u16,
    pub stream_ref: String,
}

impl HlsSessionKey {
    pub fn new(input_id: u16, stream_ref: impl Into<String>) -> Self {
        Self { input_id, stream_ref: stream_ref.into() }
    }

    pub fn canonical(&self) -> String { format!("input:{}|hls|{}", self.input_id, self.stream_ref) }

    pub fn stable_value(&self) -> String { self.canonical() }
}

/// Public opaque lookup token for HLS proxy URLs.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProxySessionId(pub String);

/// Builds the public opaque proxy session token from the stable session key.
///
/// # Panics
///
/// Panics only if the HMAC implementation rejects the rewrite secret. HMAC-SHA256 accepts keys of any length.
pub fn build_proxy_session_id(key: &HlsSessionKey, reverse_proxy_rewrite_secret: &[u8]) -> ProxySessionId {
    let hls_session_key = derive_proxy_session_hmac_key(reverse_proxy_rewrite_secret);
    let mut mac =
        HmacSha256::new_from_slice(&hls_session_key).expect("HMAC-SHA256 accepts 32-byte derived keys");
    mac.update(key.stable_value().as_bytes());
    let digest = mac.finalize().into_bytes();
    let token = general_purpose::URL_SAFE_NO_PAD.encode(digest);
    ProxySessionId(token[..PROXY_SESSION_ID_LEN].to_string())
}

fn derive_proxy_session_hmac_key(reverse_proxy_rewrite_secret: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(reverse_proxy_rewrite_secret)
        .expect("HMAC-SHA256 accepts rewrite secrets of any length");
    mac.update(PROXY_SESSION_ID_HMAC_KEY_DOMAIN);
    let digest = mac.finalize().into_bytes();
    digest.into()
}

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
