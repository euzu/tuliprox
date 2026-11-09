//! The Matrix side of the plugin: render a notification, pick a transaction id,
//! `PUT` it to the room and classify the answer for the host's outbox.
//!
//! Nothing here reads the plugin settings type: [`deliver`] takes a [`Target`],
//! so the logic is testable on the mock host without the settings static.
//!
//! # Retry contract
//!
//! The outbox owns every retry. This module only says how the attempt ended:
//!
//! | Matrix answered | Delivery error |
//! | --- | --- |
//! | 2xx | `Ok` |
//! | 429 | `retry-after` (seconds, from `retry_after_ms`) |
//! | 5xx, connect error, timeout | `transient` |
//! | any other status, an unusable URL | `permanent` |
//!
//! # Idempotency
//!
//! `PUT .../send/m.room.message/{txnId}` is idempotent on the homeserver: the
//! same transaction id from the same access token yields the same event, not a
//! second message. So a retry after a lost acknowledgement must reuse the id.
//! The id is `tuliprox-<fingerprint>-<first attempt unix time>`; the first
//! attempt time of a message that has not been accepted yet is remembered in the
//! KV (`txn-index`), so every retry of it derives the same id. The record is
//! dropped when the message is accepted or refused for good, which is what lets
//! a later, textually identical notification be a new message.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};
use tuliprox_plugin_sdk::prelude::*;

/// Matrix client-server API path of "send a room event".
const SEND_PATH: &str = "_matrix/client/v3/rooms";

/// Below the host's default 5 s hook deadline (`plugins.max_exec`), so a stuck
/// homeserver becomes a `transient` answer, not a trap.
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);

/// Used when a 429 names no delay.
const DEFAULT_RETRY_AFTER_SECS: u32 = 5;
const MAX_RETRY_AFTER_SECS: u32 = 3600;

/// Longest message body sent; a homeserver rejects events above 64 KiB.
const MAX_BODY_CHARS: usize = 8000;

/// KV key of the pending-transaction index.
const INDEX_KEY: &str = "txn-index";
/// How long an unaccepted message keeps its transaction id. The outbox gives up
/// after `max_attempts` (6 by default, backoff capped at 15 minutes).
const TXN_WINDOW_SECS: i64 = 6 * 3600;
const MAX_PENDING: usize = 1024;

#[must_use]
pub fn default_homeserver() -> String { "http://127.0.0.1:18972".to_string() }

/// The `min_severity` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MinSeverity {
    #[default]
    Info,
    Warn,
    Error,
}

impl MinSeverity {
    const fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warn => 1,
            Self::Error => 2,
        }
    }
}

const fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Warn => 1,
        Severity::Error => 2,
    }
}

const fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "INFO",
        Severity::Warn => "WARN",
        Severity::Error => "ERROR",
    }
}

/// Where and how to deliver.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub homeserver: &'a str,
    pub room_id: &'a str,
    pub access_token: &'a str,
    pub min_severity: MinSeverity,
}

/// Delivers `msg` to the room, or says why not.
///
/// Order of the gates: the host has already applied `settings.routing`
/// (`notify_on`, `min_severity`, quiet hours, suppression, `max_per_hour`) when
/// it decided to call this at all; the `min_severity` setting is the plugin's
/// own second floor and, below it, the message is acknowledged without being
/// sent (the outbox counts that as delivered).
pub fn deliver(target: &Target<'_>, msg: &Notification) -> Result<(), DeliveryError> {
    if severity_rank(msg.severity) < target.min_severity.rank() {
        debug!(
            "skipped `{}`: {} is below min_severity {:?}",
            clean(&msg.title, 80),
            severity_label(msg.severity),
            target.min_severity
        );
        return Ok(());
    }
    let base = base_url(target.homeserver)?;
    if target.room_id.trim().is_empty() || target.access_token.is_empty() {
        return Err(DeliveryError::Permanent(
            "matrix-notifier is not configured: `room_id` and `access_token` are required".to_string(),
        ));
    }

    let fingerprint = fingerprint(msg);
    let txn = acquire_txn(&fingerprint);
    let url = format!(
        "{base}/{SEND_PATH}/{}/send/m.room.message/{}",
        percent_encode(target.room_id.trim()),
        percent_encode(&txn)
    );
    let request = http::Request::put(url)
        .header("accept", "application/json")
        .bearer(target.access_token)
        .timeout(HTTP_TIMEOUT)
        .json(&render(msg))
        .map_err(|err| DeliveryError::Permanent(err.to_string()))?;

    let outcome = match request.send() {
        Ok(response) => {
            classify(response.status(), &response.text().unwrap_or_default(), response.header("retry-after"))
        }
        Err(http::Error::InvalidRequest(reason)) => Err(DeliveryError::Permanent(format!("invalid request: {reason}"))),
        Err(err) => Err(DeliveryError::Transient(err.to_string())),
    };
    match &outcome {
        Ok(()) => {
            release_txn(&fingerprint);
            info!("sent [{}] `{}` as {txn}", severity_label(msg.severity), clean(&msg.title, 80));
        }
        Err(DeliveryError::Permanent(reason)) => {
            release_txn(&fingerprint);
            warn!("refused `{}` ({txn}): {reason}", clean(&msg.title, 80));
        }
        Err(DeliveryError::Transient(reason)) => {
            warn!("will retry `{}` ({txn}): {reason}", clean(&msg.title, 80));
        }
        Err(DeliveryError::RetryAfter(secs)) => {
            warn!("rate limited, retry `{}` ({txn}) in {secs}s", clean(&msg.title, 80));
        }
    }
    outcome
}

/// How a Matrix answer ends the attempt.
///
/// `body` is only read for a 429 (`retry_after_ms`) and for the error text of a
/// refusal; `retry_header` is the `Retry-After` header (seconds), the fallback
/// of a 429 without `retry_after_ms`.
pub fn classify(status: u16, body: &str, retry_header: Option<&str>) -> Result<(), DeliveryError> {
    match status {
        200..=299 => Ok(()),
        429 => Err(DeliveryError::RetryAfter(retry_after_secs(body, retry_header))),
        500..=599 => Err(DeliveryError::Transient(format!("homeserver answered {status}{}", matrix_error(body)))),
        _ => Err(DeliveryError::Permanent(format!("homeserver answered {status}{}", matrix_error(body)))),
    }
}

fn retry_after_secs(body: &str, retry_header: Option<&str>) -> u32 {
    let from_body = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json.get("retry_after_ms").and_then(serde_json::Value::as_u64))
        .map(|ms| ms.div_ceil(1000));
    let from_header = retry_header.and_then(|value| value.trim().parse::<u64>().ok());
    let secs = from_body.or(from_header).unwrap_or(u64::from(DEFAULT_RETRY_AFTER_SECS));
    u32::try_from(secs).unwrap_or(MAX_RETRY_AFTER_SECS).clamp(1, MAX_RETRY_AFTER_SECS)
}

/// `: M_FORBIDDEN: not in the room` from a Matrix error body, or nothing.
fn matrix_error(body: &str) -> String {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else { return String::new() };
    let code = json.get("errcode").and_then(serde_json::Value::as_str).unwrap_or_default();
    let text = json.get("error").and_then(serde_json::Value::as_str).unwrap_or_default();
    match (code.is_empty(), text.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(": {}", clean(code, 40)),
        (true, false) => format!(": {}", clean(text, 160)),
        (false, false) => format!(": {}: {}", clean(code, 40), clean(text, 160)),
    }
}

/// The homeserver URL without a trailing slash, if it is usable.
fn base_url(homeserver: &str) -> Result<String, DeliveryError> {
    let base = homeserver.trim().trim_end_matches('/');
    if base.starts_with("http://") || base.starts_with("https://") {
        Ok(base.to_string())
    } else {
        Err(DeliveryError::Permanent("`homeserver` must be an http:// or https:// URL".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Message content
// ---------------------------------------------------------------------------

/// The `m.room.message` content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Content {
    pub msgtype: &'static str,
    pub body: String,
    pub format: &'static str,
    pub formatted_body: String,
}

/// What the plugin reads from the event JSON the host passes as `payload`.
#[derive(Debug, Default, Deserialize)]
struct Meta {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    instance: Option<String>,
}

/// `m.text` with a plain `body` and an HTML `formatted_body`.
#[must_use]
pub fn render(msg: &Notification) -> Content {
    let meta: Meta = serde_json::from_str(&msg.payload).unwrap_or_default();
    let title = msg.title.trim();
    let detail = detail_of(title, &msg.body);
    let label = severity_label(msg.severity);
    let footer = match (&meta.id, &meta.instance) {
        (Some(id), Some(instance)) => Some(format!("{id} @ {instance}")),
        (Some(id), None) => Some(id.clone()),
        (None, Some(instance)) => Some(instance.clone()),
        (None, None) => None,
    };

    let mut plain = format!("[{label}] {title}");
    let mut html = format!("<strong>[{label}] {}</strong>", escape_html(title));
    if !detail.is_empty() {
        plain.push_str("\n\n");
        plain.push_str(&detail);
        html.push_str("<br/>");
        html.push_str(&escape_html(&detail).replace('\n', "<br/>"));
    }
    if let Some(footer) = footer {
        plain.push_str("\n\n-- ");
        plain.push_str(&footer);
        html.push_str("<br/><code>");
        html.push_str(&escape_html(&footer));
        html.push_str("</code>");
    }
    Content { msgtype: "m.text", body: plain, format: "org.matrix.custom.html", formatted_body: html }
}

/// The body without a repeat of the title: most bodies start with it.
fn detail_of(title: &str, body: &str) -> String {
    let body = body.trim();
    let detail = body.strip_prefix(title).map_or(body, str::trim_start);
    truncate(detail, MAX_BODY_CHARS)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

/// A short, single-line rendering of untrusted text for a log line.
#[must_use]
pub fn clean(text: &str, max_chars: usize) -> String {
    let flat: String = text.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
    truncate(flat.trim(), max_chars)
}

// ---------------------------------------------------------------------------
// Transaction ids
// ---------------------------------------------------------------------------

const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// FNV-1a over the parts, each followed by a 0xff separator.
fn fnv1a(seed: u64, parts: &[&[u8]]) -> u64 {
    let mut hash = seed;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 128 bits of FNV-1a (two seeds) over everything that makes a notification
/// this notification. Pure: the same notification always has the same value.
#[must_use]
pub fn fingerprint(msg: &Notification) -> String {
    let severity = severity_label(msg.severity);
    let parts: [&[u8]; 4] = [msg.title.as_bytes(), msg.body.as_bytes(), severity.as_bytes(), msg.payload.as_bytes()];
    let high = fnv1a(0xcbf2_9ce4_8422_2325, &parts);
    let low = fnv1a(0x8422_2325_cbf2_9ce4, &parts);
    format!("{high:016x}{low:016x}")
}

/// The Matrix transaction id of the message `fingerprint` first tried at
/// `first_attempt`.
#[must_use]
pub fn txn_id(fingerprint: &str, first_attempt: i64) -> String { format!("tuliprox-{fingerprint}-{first_attempt}") }

type Pending = BTreeMap<String, i64>;

fn load_pending() -> Result<Pending, kv::Error> {
    // An unreadable index is the same as an empty one: the worst case is one
    // message that gets a fresh id on its next attempt.
    Ok(kv::get_json::<Pending>(INDEX_KEY).unwrap_or_default().unwrap_or_default())
}

/// The id for this attempt: the remembered one of an unaccepted message, else a
/// new one stamped with `now-unix`. A KV failure still yields an id (a retry of
/// it will then get a fresh one, which only risks a duplicate message).
fn acquire_txn(fingerprint: &str) -> String {
    let now = clock::now_unix();
    let mut index = match load_pending() {
        Ok(index) => index,
        Err(err) => {
            warn!("kv unreadable ({err}); the transaction id will not survive a retry");
            return txn_id(fingerprint, now);
        }
    };
    index.retain(|_, first| now.saturating_sub(*first) <= TXN_WINDOW_SECS);
    let first_attempt = *index.entry(fingerprint.to_string()).or_insert(now);
    while index.len() > MAX_PENDING {
        let Some(oldest) =
            index.iter().min_by_key(|(key, first)| (**first, (*key).clone())).map(|(key, _)| key.clone())
        else {
            break;
        };
        index.remove(&oldest);
    }
    if let Err(err) = kv::set_json(INDEX_KEY, &index) {
        warn!("kv write failed ({err}); the transaction id will not survive a retry");
    }
    txn_id(fingerprint, first_attempt)
}

/// Forgets the pending id of an accepted or refused message.
fn release_txn(fingerprint: &str) {
    let Ok(mut index) = load_pending() else { return };
    if index.remove(fingerprint).is_some() {
        if let Err(err) = kv::set_json(INDEX_KEY, &index) {
            warn!("kv write failed ({err})");
        }
    }
}

/// RFC 3986 unreserved characters stay, everything else is `%XX`.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    /// What the mock homeserver answers: a status and body, or a transport failure.
    type Answer = Result<(u16, &'static str), &'static str>;

    const HOME: &str = "http://127.0.0.1:18972";
    const ROOM: &str = "!demo:localhost";

    fn target(min_severity: MinSeverity) -> Target<'static> {
        Target { homeserver: HOME, room_id: ROOM, access_token: "s3cret-token", min_severity }
    }

    fn note(title: &str, severity: Severity) -> Notification {
        Notification {
            title: title.to_string(),
            body: format!("{title}\n\nmore <detail> & numbers"),
            severity,
            payload: r#"{"id":"playlist.update.completed","instance":"tv-1","timestamp":1700000000}"#.to_string(),
        }
    }

    fn url(txn: &str) -> String {
        format!("{HOME}/_matrix/client/v3/rooms/%21demo%3Alocalhost/send/m.room.message/{txn}")
    }

    /// Compact, comparable form of an outcome.
    fn shape(outcome: &Result<(), DeliveryError>) -> String {
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(DeliveryError::Transient(_)) => "transient".to_string(),
            Err(DeliveryError::RetryAfter(secs)) => format!("retry-after:{secs}"),
            Err(DeliveryError::Permanent(_)) => "permanent".to_string(),
        }
    }

    fn sent_txns() -> Vec<String> {
        mock::http_requests()
            .iter()
            .map(|request| request.url().rsplit('/').next().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn matrix_answers_map_onto_the_outbox_contract() {
        let cases: [(u16, &str, Option<&str>, &str); 13] = [
            (200, "{}", None, "ok"),
            (204, "", None, "ok"),
            (429, r#"{"errcode":"M_LIMIT_EXCEEDED","retry_after_ms":2500}"#, None, "retry-after:3"),
            (429, r#"{"retry_after_ms":1000}"#, None, "retry-after:1"),
            (429, r#"{"retry_after_ms":0}"#, None, "retry-after:1"),
            (429, "", Some("7"), "retry-after:7"),
            (429, "not json", None, "retry-after:5"),
            (429, r#"{"retry_after_ms":99999999999}"#, None, "retry-after:3600"),
            (500, "", None, "transient"),
            (503, r#"{"errcode":"M_UNKNOWN","error":"down"}"#, None, "transient"),
            (400, "", None, "permanent"),
            (403, r#"{"errcode":"M_FORBIDDEN","error":"not in the room"}"#, None, "permanent"),
            (302, "", None, "permanent"),
        ];
        for (status, body, header, want) in cases {
            assert_eq!(shape(&classify(status, body, header)), want, "status {status} body {body:?}");
        }
        let Err(DeliveryError::Permanent(reason)) = classify(403, r#"{"errcode":"M_FORBIDDEN","error":"no"}"#, None)
        else {
            panic!("403 is permanent")
        };
        assert_eq!(reason, "homeserver answered 403: M_FORBIDDEN: no");
    }

    #[test]
    fn deliver_maps_each_http_outcome_end_to_end() {
        let note = note("Update", Severity::Info);
        let txn = txn_id(&fingerprint(&note), 100);
        let cases: [(Answer, &str); 5] = [
            (Ok((200, r#"{"event_id":"$e"}"#)), "ok"),
            (Ok((429, r#"{"retry_after_ms":1500}"#)), "retry-after:2"),
            (Ok((502, "")), "transient"),
            (Ok((401, r#"{"errcode":"M_UNKNOWN_TOKEN"}"#)), "permanent"),
            (Err("connection refused"), "transient"),
        ];
        for (answer, want) in cases {
            mock::reset();
            mock::set_now(100);
            match answer {
                Ok((status, body)) => mock::http_respond(http::Method::Put, &url(&txn), status, body),
                Err(message) => mock::http_fail(http::Method::Put, &url(&txn), message),
            }
            assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), want);
            assert_eq!(mock::http_requests().len(), 1, "exactly one attempt per call");
        }
    }

    #[test]
    fn an_unusable_target_is_permanent_and_sends_nothing() {
        mock::reset();
        let note = note("Update", Severity::Error);
        let no_room = Target { room_id: " ", ..target(MinSeverity::Info) };
        let no_token = Target { access_token: "", ..target(MinSeverity::Info) };
        let bad_url = Target { homeserver: "matrix.example.org", ..target(MinSeverity::Info) };
        for broken in [no_room, no_token, bad_url] {
            assert_eq!(shape(&deliver(&broken, &note)), "permanent");
        }
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn the_request_is_a_bearer_put_with_an_encoded_path_and_an_html_body() {
        mock::reset();
        mock::set_now(100);
        let note = note("Playlist <ok>", Severity::Warn);
        mock::http_respond(http::Method::Put, &url(&txn_id(&fingerprint(&note), 100)), 200, "{}");
        assert_eq!(
            shape(&deliver(&Target { homeserver: "http://127.0.0.1:18972/ ", ..target(MinSeverity::Info) }, &note)),
            "ok"
        );

        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method(), http::Method::Put);
        assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer s3cret-token".to_string())));
        assert!(sent[0].headers().contains(&("content-type".to_string(), "application/json".to_string())));
        assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
        let body: serde_json::Value = serde_json::from_slice(sent[0].body_bytes()).unwrap();
        assert_eq!(body["msgtype"], "m.text");
        assert_eq!(body["format"], "org.matrix.custom.html");
        assert_eq!(
            body["body"],
            "[WARN] Playlist <ok>\n\nmore <detail> & numbers\n\n-- playlist.update.completed @ tv-1"
        );
        assert_eq!(
            body["formatted_body"],
            "<strong>[WARN] Playlist &lt;ok&gt;</strong><br/>more &lt;detail&gt; &amp; numbers<br/><code>playlist.update.completed @ tv-1</code>"
        );
        let logs = mock::logs();
        assert!(logs.iter().any(|line| line.message.starts_with("sent [WARN]")));
        assert!(logs.iter().all(|line| !line.message.contains("s3cret-token")), "the token never reaches a log");
    }

    #[test]
    fn a_body_that_is_only_the_title_is_not_repeated_and_a_bad_payload_adds_no_footer() {
        let mut plain = note("Disk almost full", Severity::Error);
        plain.body = "Disk almost full".to_string();
        plain.payload = "not json".to_string();
        let content = render(&plain);
        assert_eq!(content.body, "[ERROR] Disk almost full");
        assert_eq!(content.formatted_body, "<strong>[ERROR] Disk almost full</strong>");
    }

    #[test]
    fn a_long_body_is_cut() {
        let mut long = note("Big", Severity::Info);
        long.body = "x".repeat(MAX_BODY_CHARS + 50);
        let content = render(&long);
        assert!(content.body.chars().count() < MAX_BODY_CHARS + 100);
        assert!(content.body.contains('…'));
    }

    #[test]
    fn notifications_below_min_severity_are_acknowledged_without_a_request() {
        mock::reset();
        let info = note("Quiet", Severity::Info);
        assert_eq!(shape(&deliver(&target(MinSeverity::Warn), &info)), "ok");
        assert_eq!(shape(&deliver(&target(MinSeverity::Error), &note("Mid", Severity::Warn))), "ok");
        assert!(mock::http_requests().is_empty(), "nothing was sent");
        assert!(mock::kv_snapshot().is_empty(), "and no transaction id was reserved");

        mock::http_respond(http::Method::Put, &url(&txn_id(&fingerprint(&info), 0)), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &info)), "ok");
        assert_eq!(mock::http_requests().len(), 1, "at or above the floor it is sent");
        let loud = note("Loud", Severity::Error);
        mock::http_respond(http::Method::Put, &url(&txn_id(&fingerprint(&loud), 0)), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Error), &loud)), "ok");
        assert_eq!(mock::http_requests().len(), 2);
    }

    #[test]
    fn the_fingerprint_and_the_txn_id_are_pure() {
        let note = note("Update", Severity::Info);
        assert_eq!(fingerprint(&note), fingerprint(&note.clone()));
        assert_eq!(fingerprint(&note).len(), 32);
        let mut other = note.clone();
        other.body.push('!');
        assert_ne!(fingerprint(&note), fingerprint(&other));
        let mut louder = note.clone();
        louder.severity = Severity::Error;
        assert_ne!(fingerprint(&note), fingerprint(&louder));
        // The separator keeps ("ab", "c") apart from ("a", "bc").
        let (mut left, mut right) = (note.clone(), note.clone());
        (left.title, left.body) = ("ab".to_string(), "c".to_string());
        (right.title, right.body) = ("a".to_string(), "bc".to_string());
        assert_ne!(fingerprint(&left), fingerprint(&right));
        assert_eq!(txn_id("abc", 42), "tuliprox-abc-42");
    }

    #[test]
    fn a_retry_of_an_unaccepted_message_reuses_its_transaction_id() {
        mock::reset();
        mock::set_now(1_000);
        let note = note("Update", Severity::Info);
        let first = txn_id(&fingerprint(&note), 1_000);
        mock::http_respond(http::Method::Put, &url(&first), 503, "");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "transient");

        // The outbox retries 5 s, then 15 s later: still the first attempt's id.
        mock::advance(5);
        mock::http_fail(http::Method::Put, &url(&first), "connection refused");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "transient");
        mock::advance(10);
        mock::http_respond(http::Method::Put, &url(&first), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "ok");
        assert_eq!(sent_txns(), vec![first.clone(); 3]);
    }

    #[test]
    fn an_accepted_message_releases_its_id_so_an_identical_later_one_is_new() {
        mock::reset();
        mock::set_now(1_000);
        let note = note("Update", Severity::Info);
        let first = txn_id(&fingerprint(&note), 1_000);
        mock::http_respond(http::Method::Put, &url(&first), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "ok");
        assert_eq!(kv::get_json::<Pending>(INDEX_KEY).unwrap(), Some(Pending::new()), "nothing stays pending");

        mock::advance(60);
        let second = txn_id(&fingerprint(&note), 1_060);
        assert_ne!(first, second);
        mock::http_respond(http::Method::Put, &url(&second), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "ok");
        assert_eq!(sent_txns(), vec![first, second]);
    }

    #[test]
    fn a_refused_message_releases_its_id_and_a_stale_one_expires() {
        mock::reset();
        mock::set_now(1_000);
        let note = note("Update", Severity::Info);
        let first = txn_id(&fingerprint(&note), 1_000);
        mock::http_respond(http::Method::Put, &url(&first), 403, "");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "permanent");
        assert_eq!(kv::get_json::<Pending>(INDEX_KEY).unwrap(), Some(Pending::new()));

        // A transient failure that is never retried does not pin its id forever.
        mock::http_respond(http::Method::Put, &url(&first), 503, "");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "transient");
        mock::advance(TXN_WINDOW_SECS + 1);
        let fresh = txn_id(&fingerprint(&note), 1_000 + TXN_WINDOW_SECS + 1);
        mock::http_respond(http::Method::Put, &url(&fresh), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "ok");
        assert_eq!(sent_txns().last(), Some(&fresh));
    }

    #[test]
    fn the_pending_index_is_bounded() {
        mock::reset();
        for index in 0..(MAX_PENDING + 10) {
            mock::set_now(i64::try_from(index).unwrap());
            acquire_txn(&format!("fp{index:05}"));
        }
        let pending = kv::get_json::<Pending>(INDEX_KEY).unwrap().unwrap();
        assert_eq!(pending.len(), MAX_PENDING);
        assert!(!pending.contains_key("fp00000"), "the oldest are dropped first");
    }

    #[test]
    fn an_unavailable_kv_still_sends() {
        mock::reset();
        mock::set_now(7);
        mock::set_kv_unavailable("disabled");
        let note = note("Update", Severity::Info);
        mock::http_respond(http::Method::Put, &url(&txn_id(&fingerprint(&note), 7)), 200, "{}");
        assert_eq!(shape(&deliver(&target(MinSeverity::Info), &note)), "ok");
    }

    #[test]
    fn percent_encoding_keeps_only_unreserved_characters() {
        assert_eq!(percent_encode("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a b/é"), "a%20b%2F%C3%A9");
    }
}
