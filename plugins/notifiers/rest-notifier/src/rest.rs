//! The REST side of the plugin: build the request and say how the answer ends the
//! attempt for the host's outbox.
//!
//! # Request
//!
//! `{method} {url}` (POST, PUT or PATCH) with the body of the `body` setting:
//!
//! * `event` (default): the event as JSON, exactly the `payload` the host hands every
//!   notification plugin: `id`, `severity`, `timestamp`, `instance`, `title`, `body`,
//!   `fields` (the typed payload) and `sections`. `Content-Type: application/json`.
//! * `text`: the event's plain-text body. `Content-Type: text/plain; charset=utf-8`.
//!
//! `headers` are extra `Name: value` headers, and `secret_headers` the same for
//! credentials (the whole entry is masked in the settings UI). A `Content-Type` among
//! them replaces the default. A body shaped for one particular service belongs in a
//! plugin for that service; this one sends the event as it is.
//!
//! # Signing
//!
//! With a `signing_secret` every request carries `X-Tuliprox-Timestamp` (Unix seconds)
//! and `X-Tuliprox-Signature: sha256=<hex>`, the hex HMAC-SHA256 of `{timestamp}.{body}`.
//! The timestamp is inside the signed payload, so a captured request cannot be replayed
//! later under a fresh timestamp header. The format is the one the built-in channel had.
//!
//! # Retry contract
//!
//! The status mapping and the `Retry-After` handling are the SDK's
//! (`tuliprox_plugin_sdk::notify`): 2xx delivered, 408 and 429 retried after
//! `Retry-After`, 5xx except 501 and 505 transient, everything else permanent. No answer at
//! all is transient; a missing or unusable setting, or a body over 1 MiB, is permanent.
//!
//! # Secrets
//!
//! A secret header and the signing secret appear in no error, log line or reason, nor does
//! anything a transport error says or a server answered. A malformed `headers` entry is
//! reported by its position, never by its text.

use hmac::{KeyInit, Mac, SimpleHmac};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// A request body over this is refused locally: it is a runaway payload, not a notification.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Request headers a plugin may add.
const MAX_HEADERS: usize = 64;
/// Bytes of header names and values together.
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Below the host's 30 s send timeout, so a silent endpoint surfaces as a transient
/// error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// The header names the signature uses; a header of the same name in the settings is dropped
/// when signing is on, so the receiver never sees two.
const TIMESTAMP_HEADER: &str = "X-Tuliprox-Timestamp";
const SIGNATURE_HEADER: &str = "X-Tuliprox-Signature";

/// The HTTP method of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    #[default]
    Post,
    Put,
    Patch,
}

impl Method {
    const fn http(self) -> http::Method {
        match self {
            Self::Post => http::Method::Post,
            Self::Put => http::Method::Put,
            Self::Patch => http::Method::Patch,
        }
    }
}

/// What the request body holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Body {
    /// The event as JSON.
    #[default]
    Event,
    /// The plain-text body.
    Text,
}

/// Where and how to send.
pub struct Target<'a> {
    pub url: &'a str,
    pub method: Method,
    pub body: Body,
    /// `Name: value` entries.
    pub headers: &'a [String],
    /// `Name: value` entries whose value is a credential.
    pub secret_headers: &'a [&'a str],
    /// Empty means unsigned.
    pub signing_secret: &'a str,
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warn => "warn",
        Severity::Error => "error",
    }
}

/// The request body for `msg`: the event JSON, or the plain-text body.
pub fn request_body(msg: &Notification, body: Body) -> String {
    match body {
        Body::Text => msg.body.clone(),
        Body::Event => {
            if serde_json::from_str::<Value>(&msg.payload).is_ok_and(|value| value.is_object()) {
                return msg.payload.clone();
            }
            // The host always sends an object; this keeps the receiver's contract if it ever did not.
            json!({ "title": msg.title, "body": msg.body, "severity": severity_name(msg.severity) }).to_string()
        }
    }
}

const fn default_content_type(body: Body) -> &'static str {
    match body {
        Body::Event => "application/json",
        Body::Text => "text/plain; charset=utf-8",
    }
}

/// `Name: value` as a header, or `None` when it is not one: the name must be an HTTP token, and
/// neither part may hold a line break.
fn parse_header(entry: &str) -> Option<(String, String)> {
    let (name, value) = entry.split_once(':')?;
    let (name, value) = (name.trim(), value.trim());
    let token = |ch: char| ch.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(ch);
    (!name.is_empty() && name.chars().all(token) && !value.contains(['\r', '\n']))
        .then(|| (name.to_string(), value.to_string()))
}

/// The hex HMAC-SHA256 of `data` under `key`.
fn hmac_sha256_hex(key: &str, data: &[u8]) -> String {
    let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(key.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().iter().fold(String::with_capacity(64), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Hex HMAC-SHA256 of `{timestamp}.{body}`.
fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    hmac_sha256_hex(secret, format!("{timestamp}.{body}").as_bytes())
}

/// Every header of the request, in order, or the reason the settings cannot make a request.
fn request_headers(target: &Target<'_>, body: &str, now: i64) -> Result<Vec<(String, String)>, DeliveryError> {
    let signing = !target.signing_secret.trim().is_empty();
    let mut headers: Vec<(String, String)> = Vec::new();
    let entries = target
        .headers
        .iter()
        .map(|entry| ("headers", entry.as_str()))
        .chain(target.secret_headers.iter().map(|entry| ("secret_headers", *entry)));
    for (at, (list, entry)) in entries.enumerate() {
        if entry.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = parse_header(entry) else {
            // By position, never by text: the entry may hold a credential.
            return Err(DeliveryError::Permanent(format!(
                "entry {} of `{list}` is not a header written as `Name: value`",
                at + 1
            )));
        };
        let own = name.eq_ignore_ascii_case(TIMESTAMP_HEADER) || name.eq_ignore_ascii_case(SIGNATURE_HEADER);
        if signing && own {
            continue;
        }
        headers.push((name, value));
    }
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type")) {
        headers.insert(0, ("Content-Type".to_string(), default_content_type(target.body).to_string()));
    }
    if signing {
        headers.push((TIMESTAMP_HEADER.to_string(), now.to_string()));
        headers.push((SIGNATURE_HEADER.to_string(), format!("sha256={}", sign(target.signing_secret, now, body))));
    }
    let bytes: usize = headers.iter().map(|(name, value)| name.len() + value.len()).sum();
    if headers.len() > MAX_HEADERS || bytes > MAX_HEADER_BYTES {
        return Err(DeliveryError::Permanent("the request has too many or too large headers".to_string()));
    }
    Ok(headers)
}

/// Send `msg` to `target` and say how the attempt ended.
pub fn deliver(target: &Target<'_>, msg: &Notification) -> Result<(), DeliveryError> {
    let url = target.url.trim();
    if url.is_empty() {
        return Err(DeliveryError::Permanent("rest-notifier is not configured: `url` is required".to_string()));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(DeliveryError::Permanent("`url` must be an http:// or https:// URL".to_string()));
    }
    let body = request_body(msg, target.body);
    if body.len() > MAX_BODY_BYTES {
        return Err(DeliveryError::Permanent("the request body is larger than 1 MiB".to_string()));
    }
    let headers = request_headers(target, &body, clock::now_unix())?;
    let mut request = http::Request::new(target.method.http(), url).timeout(HTTP_TIMEOUT).body(body.into_bytes());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Permanent("rest request failed: the url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("rest request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to the rest endpoint"),
        Err(DeliveryError::Permanent(reason)) => warn!("rest endpoint refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("rest endpoint will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("rest endpoint is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const URL: &str = "https://hooks.test/in";
    const PAYLOAD: &str =
        r#"{"id":"playlist.update.completed","severity":"info","timestamp":784111777,"title":"T","body":"B"}"#;

    fn note(body: &str, payload: &str) -> Notification {
        Notification {
            title: "Title".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: payload.to_string(),
        }
    }

    fn plain_target() -> Target<'static> {
        Target {
            url: URL,
            method: Method::Post,
            body: Body::Event,
            headers: &[],
            secret_headers: &[],
            signing_secret: "",
        }
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn shape(outcome: &Result<(), DeliveryError>) -> String {
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(DeliveryError::Transient(_)) => "transient".to_string(),
            Err(DeliveryError::Permanent(_)) => "permanent".to_string(),
            Err(DeliveryError::RetryAfter(secs)) => format!("retry-after:{secs}"),
        }
    }

    #[test]
    fn hmac_sha256_matches_the_published_test_vector() {
        // RFC 4231, test case 2.
        assert_eq!(
            hmac_sha256_hex("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn the_signature_covers_the_timestamp_and_the_body() {
        let signature = sign("secret", 1_700_000_000, "{}");
        assert_eq!(signature, hmac_sha256_hex("secret", b"1700000000.{}"));
        assert_eq!(signature.len(), 64);
        assert_ne!(signature, sign("secret", 1_700_000_001, "{}"), "a new timestamp is a new signature");
        assert_ne!(signature, sign("secret", 1_700_000_000, "{ }"), "a changed body is a new signature");
        assert_ne!(signature, sign("other", 1_700_000_000, "{}"), "a different secret is a new signature");
    }

    #[test]
    fn the_event_body_is_the_event_json_untouched_and_text_is_the_plain_body() {
        assert_eq!(request_body(&note("plain", PAYLOAD), Body::Event), PAYLOAD);
        assert_eq!(request_body(&note("plain body", PAYLOAD), Body::Text), "plain body");
        // A payload that is not an object never reaches the receiver as something else.
        let fallback = request_body(&note("B", "not json"), Body::Event);
        let value: Value = serde_json::from_str(&fallback).unwrap();
        assert_eq!(value, json!({"title": "Title", "body": "B", "severity": "info"}));
        let array = request_body(&note("B", "[1]"), Body::Event);
        assert!(serde_json::from_str::<Value>(&array).unwrap().is_object());
    }

    #[test]
    fn headers_are_parsed_as_name_colon_value_and_a_bad_entry_is_reported_by_position_only() {
        assert_eq!(parse_header("X-Env: prod"), Some(("X-Env".to_string(), "prod".to_string())));
        assert_eq!(
            parse_header("Authorization:Bearer a:b"),
            Some(("Authorization".to_string(), "Bearer a:b".to_string()))
        );
        for bad in ["no colon", ": value", "Bad Name: v", "X: a\r\nInjected: b", "X: a\nb"] {
            assert_eq!(parse_header(bad), None, "{bad:?}");
        }
        let secret = ["Authorization Bearer SECRET-BEARER"];
        let target = Target { secret_headers: &secret, ..plain_target() };
        let err = request_headers(&target, "{}", 1).unwrap_err();
        let DeliveryError::Permanent(reason) = err else { panic!("permanent") };
        assert!(reason.contains("entry 1 of `secret_headers`"), "{reason}");
        assert!(!reason.contains("SECRET"), "the entry's text is never echoed: {reason}");
    }

    #[test]
    fn the_default_content_type_is_replaced_by_one_the_operator_sets() {
        let headers = request_headers(&plain_target(), "{}", 1).unwrap();
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        let text = Target { body: Body::Text, ..plain_target() };
        assert_eq!(header(&request_headers(&text, "x", 1).unwrap(), "Content-Type"), Some("text/plain; charset=utf-8"));
        let own = ["content-type: application/vnd.api+json".to_string()];
        let custom = Target { headers: &own, ..plain_target() };
        let headers = request_headers(&custom, "{}", 1).unwrap();
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("content-type")).count(), 1);
        assert_eq!(header(&headers, "content-type"), Some("application/vnd.api+json"));
    }

    #[test]
    fn signing_adds_the_timestamp_and_signature_headers_and_drops_a_same_named_one_of_the_settings() {
        let own = ["X-Tuliprox-Signature: forged".to_string(), "X-Env: prod".to_string()];
        let target = Target { headers: &own, signing_secret: "s3cret", ..plain_target() };
        let headers = request_headers(&target, "{\"a\":1}", 1_700_000_000).unwrap();
        assert_eq!(header(&headers, "x-tuliprox-timestamp"), Some("1700000000"));
        assert_eq!(
            header(&headers, "x-tuliprox-signature"),
            Some(format!("sha256={}", sign("s3cret", 1_700_000_000, "{\"a\":1}")).as_str())
        );
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("x-tuliprox-signature")).count(), 1);
        assert_eq!(header(&headers, "x-env"), Some("prod"));
        // Unsigned: neither header is sent.
        let unsigned = request_headers(&plain_target(), "{}", 1).unwrap();
        assert!(
            header(&unsigned, "x-tuliprox-signature").is_none() && header(&unsigned, "x-tuliprox-timestamp").is_none()
        );
        // Blank entries are skipped.
        let blanks = ["  ".to_string(), String::new()];
        assert!(request_headers(&Target { headers: &blanks, ..plain_target() }, "{}", 1).is_ok());
    }

    #[test]
    fn too_many_headers_and_an_oversized_body_are_refused_locally() {
        let many: Vec<String> = (0..70).map(|n| format!("X-H{n}: v")).collect();
        let target = Target { headers: &many, ..plain_target() };
        assert_eq!(shape(&request_headers(&target, "{}", 1).map(|_| ())), "permanent");
        mock::reset();
        let huge = note("b", &format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_BODY_BYTES)));
        assert_eq!(shape(&deliver(&plain_target(), &huge)), "permanent");
        assert!(mock::http_requests().is_empty(), "nothing was sent");
    }

    #[test]
    fn the_request_uses_the_configured_method_url_headers_and_a_timeout() {
        for (method, expected) in
            [(Method::Post, http::Method::Post), (Method::Put, http::Method::Put), (Method::Patch, http::Method::Patch)]
        {
            mock::reset();
            mock::http_respond(expected, URL, 204, "");
            let secrets = ["Authorization: Bearer SECRET-BEARER"];
            let plain = ["X-Env: prod".to_string()];
            let target = Target {
                method,
                headers: &plain,
                secret_headers: &secrets,
                signing_secret: "SECRET-SIGN",
                ..plain_target()
            };
            assert!(deliver(&target, &note("Hello", PAYLOAD)).is_ok());
            let sent = mock::http_requests();
            assert_eq!(sent.len(), 1, "exactly one attempt per call");
            assert_eq!(sent[0].method(), expected);
            assert_eq!(sent[0].url(), URL);
            assert_eq!(sent[0].body_bytes(), PAYLOAD.as_bytes(), "the event JSON is sent as it is");
            assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
            let headers = sent[0].headers();
            assert!(headers.contains(&("authorization".to_string(), "Bearer SECRET-BEARER".to_string())));
            assert!(headers.contains(&("x-env".to_string(), "prod".to_string())));
            assert!(headers.iter().any(|(name, _)| name == "content-type"));
        }
    }

    #[test]
    fn deliver_maps_each_outcome_end_to_end_without_leaking_a_secret() {
        type Answer<'a> = Result<(u16, Option<&'a str>), &'a str>;
        let cases: [(Answer<'_>, &str); 6] = [
            (Ok((200, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((503, None)), "transient"),
            (Ok((401, None)), "permanent"),
            (Err("connection refused (Authorization: Bearer SECRET-BEARER)"), "transient"),
            (Ok((302, Some("https://hooks.test/?sig=SECRET-SIGN"))), "permanent"),
        ];
        for (answer, want) in cases {
            mock::reset();
            match answer {
                Ok((status, retry)) => {
                    let headers =
                        retry.map(|value| ("Retry-After".to_string(), value.to_string())).into_iter().collect();
                    mock::http_respond_with(http::Method::Post, URL, http::Response::new(status, headers, Vec::new()));
                }
                Err(message) => mock::http_fail(http::Method::Post, URL, message),
            }
            let secrets = ["Authorization: Bearer SECRET-BEARER"];
            let target = Target { secret_headers: &secrets, signing_secret: "SECRET-SIGN", ..plain_target() };
            let outcome = deliver(&target, &note("Hello", PAYLOAD));
            assert_eq!(shape(&outcome), want);
            if let Err(err) = &outcome {
                assert!(!format!("{err:?}").contains("SECRET"), "no secret is in an error: {err:?}");
            }
            assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")), "nor in a log line");
        }
    }

    #[test]
    fn a_missing_or_unusable_url_is_permanent_and_sends_nothing() {
        mock::reset();
        for url in ["", "  ", "ftp://hooks.test", "hooks.test/in"] {
            let target = Target { url, ..plain_target() };
            assert_eq!(shape(&deliver(&target, &note("Hello", PAYLOAD))), "permanent", "{url:?}");
        }
        assert!(mock::http_requests().is_empty());
    }
}
