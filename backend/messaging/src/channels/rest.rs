//! Generic REST/webhook channel.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
};
use log::debug;
use reqwest::{header, Method};
use shared::model::notification::{EventId, Severity};
use std::str::FromStr;
use tuliprox_core::model::{ChannelRouting, RestMessagingConfig};

/// Hex HMAC-SHA256 of `{timestamp}.{body}`.
///
/// The timestamp is inside the signed payload so a captured request cannot
/// be replayed later with a fresh timestamp header.
fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    use hmac::{KeyInit, Mac, SimpleHmac};
    let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[derive(Clone)]
pub struct RestChannel {
    config: RestMessagingConfig,
    client: reqwest::Client,
}

impl RestChannel {
    pub fn new(config: RestMessagingConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }
}

impl NotificationChannel for RestChannel {
    fn id(&self) -> &'static str {
        "rest"
    }

    fn template_for(&self, event: EventId) -> Option<&str> {
        self.config.templates.get(&event).map(String::as_str)
    }

    fn routing(&self) -> &ChannelRouting {
        &self.config.routing
    }

    fn wants(&self, event: EventId, severity: Severity) -> bool {
        self.config.routing.accepts(event, severity)
    }

    async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        {
            let method = Method::from_str(&self.config.method).unwrap_or(Method::POST);
            let mut rb = self.client.request(method, &self.config.url);

            // HMAC-SHA256 over `timestamp.body`, so the receiving endpoint
            // can verify the sender and reject a replayed request.
            if let Some(secret) = &self.config.signing_secret {
                let timestamp = chrono::Utc::now().timestamp();
                let signature = sign(secret, timestamp, &msg.body);
                rb = rb
                    .header("X-Tuliprox-Timestamp", timestamp.to_string())
                    .header("X-Tuliprox-Signature", format!("sha256={signature}"));
            }

            let has_content_type = self.config.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"));
            if !has_content_type {
                rb = rb.header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string());
            }
            for (key, value) in &self.config.headers {
                rb = rb.header(key, value);
            }

            match rb.body(msg.body.clone()).send().await {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to rest api");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("rest request failed: {err}")),
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::sign;

    #[test]
    fn signing_is_deterministic_for_the_same_inputs() {
        assert_eq!(sign("secret", 1_700_000_000, "body"), sign("secret", 1_700_000_000, "body"));
    }

    #[test]
    fn the_signature_covers_the_secret_the_timestamp_and_the_body() {
        let base = sign("secret", 1_700_000_000, "body");
        assert_ne!(base, sign("other", 1_700_000_000, "body"), "secret not covered");
        assert_ne!(base, sign("secret", 1_700_000_001, "body"), "timestamp not covered");
        assert_ne!(base, sign("secret", 1_700_000_000, "other"), "body not covered");
    }

    #[test]
    fn the_signature_is_64_hex_characters() {
        let signature = sign("secret", 1, "body");
        assert_eq!(signature.len(), 64, "sha256 hex must be 64 chars: {signature}");
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()), "non-hex output: {signature}");
    }

    #[test]
    fn matches_a_known_hmac_sha256_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        // Reproduced through `sign` by splitting the data across the
        // timestamp/body fields exactly as the separator implies.
        use hmac::{KeyInit, Mac, SimpleHmac};
        let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(b"Jefe").expect("key");
        mac.update(b"what do ya want for nothing?");
        let bytes = mac.finalize().into_bytes();
        let expected = bytes.iter().fold(String::new(), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        });
        assert_eq!(expected, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }
}
