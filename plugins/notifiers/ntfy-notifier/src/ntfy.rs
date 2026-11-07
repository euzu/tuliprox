//! The ntfy side of the plugin: build the publish request and say how the
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST {url}` with `Content-Type: application/json` (ntfy's JSON publishing,
//! which takes the topic in the body). Unlike the header form, a title or a body
//! can carry any Unicode text: ntfy headers must be ASCII, which is why the
//! plain form has to strip emoji from a title. The body is
//! `topic`, `title`, `message`, `priority` (ntfy's 1-5 scale: 2 for info, 3 for a
//! warning, 4 for an error; a critical event reaches a plugin as an error) and
//! `tags`: the event's domain (`playlist`, `recording`, ...) and, for a warning
//! or an error, the tag ntfy shows as a warning sign or a rotating light.
//!
//! A protected topic takes `Authorization: Bearer <token>`. The token is only ever
//! in that header.
//!
//! Two styles:
//!
//! * `text` (default): `message` is the event's plain-text body.
//! * `markdown`: `message` is the event's sections as a bold heading with its lines
//!   below (else the body), markdown-escaped, with `"markdown": true`, which the
//!   ntfy web app renders. Mobile apps show the markdown source.
//!
//! `message` is cut at ntfy's default `message-size-limit` of 4096 **bytes**, on a
//! character boundary, with a visible `…`: a larger message would become an
//! attachment, or be refused by a server without an attachment cache.
//!
//! # Retry contract
//!
//! The status mapping and the `Retry-After` handling are the SDK's
//! (`tuliprox_plugin_sdk::notify`): 2xx delivered, 408 and 429 retried after
//! `Retry-After`, 5xx except 501 and 505 transient, everything else permanent.
//! No answer at all is transient; a missing or unusable setting is permanent.
//!
//! # The token is a credential
//!
//! No error string, log line or reason contains it, nor anything a transport
//! error says. Messages are built from the status or a fixed phrase.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// ntfy's default `message-size-limit`, in bytes.
const MESSAGE_LIMIT: usize = 4096;
/// A title longer than this is useless in a notification.
const TITLE_LIMIT: usize = 250;

/// Below the host's 30 s send timeout, so a silent server surfaces as a
/// transient error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// How a notification is shown by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// The plain-text body.
    #[default]
    Text,
    /// The sections as markdown, else the escaped body.
    Markdown,
}

/// Where and how to publish.
pub struct Target<'a> {
    pub url: &'a str,
    pub topic: &'a str,
    /// Empty for a public topic.
    pub token: &'a str,
    pub style: Style,
}

/// What this plugin reads of the event JSON besides the notification itself.
#[derive(Deserialize, Default)]
struct Event {
    /// The event id, `playlist.update.completed`.
    #[serde(default)]
    id: Option<String>,
}

/// ntfy's 1-5 priority scale. There is no `critical`: it is delivered as `error`.
const fn priority(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 2,
        Severity::Warn => 3,
        Severity::Error => 4,
    }
}

/// The tags of a notification: the event's domain, then the severity tag ntfy
/// turns into an emoji.
fn tags(payload: &str, severity: Severity) -> Vec<String> {
    let mut tags = Vec::new();
    let event = serde_json::from_str::<Event>(payload).unwrap_or_default();
    if let Some(domain) = event.id.as_deref().and_then(|id| id.split('.').next()).filter(|domain| !domain.is_empty()) {
        tags.push(domain.to_string());
    }
    match severity {
        Severity::Info => {}
        Severity::Warn => tags.push("warning".to_string()),
        Severity::Error => tags.push("rotating_light".to_string()),
    }
    tags
}

/// `text` cut to at most `limit` bytes on a character boundary; a cut ends in `…`
/// (which counts toward the limit).
fn cut_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let room = limit.saturating_sub('…'.len_utf8());
    let mut end = room;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The request body for `msg` on `topic`.
pub fn request_body(topic: &str, style: Style, msg: &Notification) -> String {
    let mut body = json!({
        "topic": topic.trim(),
        "title": cut_chars(&msg.title, TITLE_LIMIT),
        "priority": priority(msg.severity),
    });
    let message = match style {
        Style::Text => msg.body.clone(),
        Style::Markdown => {
            body["markdown"] = Value::Bool(true);
            notify::markdown::sections(&notify::sections_of(&msg.payload))
                .unwrap_or_else(|| notify::markdown::lines(msg.body.lines()))
        }
    };
    body["message"] = Value::String(cut_bytes(&message, MESSAGE_LIMIT));
    let tags = tags(&msg.payload, msg.severity);
    if !tags.is_empty() {
        body["tags"] = json!(tags);
    }
    body.to_string()
}

fn cut_chars(text: &str, limit: usize) -> String { text.chars().take(limit).collect() }

/// Publish `msg` to `target` and say how the attempt ended.
pub fn deliver(target: &Target<'_>, msg: &Notification) -> Result<(), DeliveryError> {
    let url = target.url.trim().trim_end_matches('/');
    if url.is_empty() || target.topic.trim().is_empty() {
        return Err(DeliveryError::Permanent(
            "ntfy-notifier is not configured: `url` and `topic` are required".to_string(),
        ));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(DeliveryError::Permanent("`url` must be an http:// or https:// URL".to_string()));
    }
    let mut request = http::Request::post(url)
        .header("content-type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .body(request_body(target.topic, target.style, msg).into_bytes());
    if !target.token.trim().is_empty() {
        request = request.bearer(target.token.trim());
    }
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Permanent("ntfy request failed: the url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("ntfy request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to ntfy"),
        Err(DeliveryError::Permanent(reason)) => warn!("ntfy refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("ntfy will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("ntfy is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const BASE: &str = "https://ntfy.test";

    fn note(body: &str, payload: &str) -> Notification {
        Notification {
            title: "Title".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: payload.to_string(),
        }
    }

    fn target<'a>(token: &'a str, style: Style) -> Target<'a> { Target { url: BASE, topic: "my-topic", token, style } }

    fn parsed(msg: &Notification, style: Style) -> Value {
        serde_json::from_str(&request_body("my-topic", style, msg)).expect("the body is json")
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
    fn severity_maps_onto_the_ntfy_priority_scale() {
        assert_eq!((priority(Severity::Info), priority(Severity::Warn), priority(Severity::Error)), (2, 3, 4));
        for severity in [Severity::Info, Severity::Warn, Severity::Error] {
            let mut msg = note("b", "{}");
            msg.severity = severity;
            assert_eq!(parsed(&msg, Style::Text)["priority"], priority(severity));
        }
    }

    #[test]
    fn tags_are_the_event_domain_and_a_severity_tag_for_anything_not_routine() {
        let payload = r#"{"id":"playlist.update.completed"}"#;
        assert_eq!(tags(payload, Severity::Info), ["playlist"]);
        assert_eq!(tags(payload, Severity::Warn), ["playlist", "warning"]);
        assert_eq!(tags(payload, Severity::Error), ["playlist", "rotating_light"]);
        assert_eq!(tags("{}", Severity::Error), ["rotating_light"], "no id, no domain");
        assert!(tags("not json", Severity::Info).is_empty());
        assert_eq!(tags(r#"{"id":"system"}"#, Severity::Info), ["system"], "an id without a dot is its own domain");
        // No tags at all means no `tags` key.
        assert!(parsed(&note("b", "{}"), Style::Text).get("tags").is_none());
    }

    #[test]
    fn text_style_publishes_the_topic_title_body_and_priority_with_any_unicode() {
        let mut msg = note("line 1\nline 2 \u{1F534}", r#"{"id":"recording.completed"}"#);
        msg.title = "Recording completed \u{1F534} \u{65E5}\u{672C}".to_string();
        let value = parsed(&msg, Style::Text);
        assert_eq!(
            value,
            json!({
                "topic": "my-topic",
                "title": "Recording completed \u{1F534} \u{65E5}\u{672C}",
                "message": "line 1\nline 2 \u{1F534}",
                "priority": 2,
                "tags": ["recording"],
            }),
            "an emoji title is kept: JSON publishing has no ASCII header limit"
        );
        assert!(value.get("markdown").is_none());
    }

    #[test]
    fn markdown_style_renders_sections_and_asks_the_server_to_render_them() {
        let msg = note(
            "fallback body",
            r#"{"id":"playlist.update.completed","sections":[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels"]},{"heading":"Targets","lines":["main: ok"]}]}"#,
        );
        let value = parsed(&msg, Style::Markdown);
        assert_eq!(
            value["message"],
            "**Sources**  \ndemo-in (M3u)  \n1/2 -> 1/2 groups/channels\n\n**Targets**  \nmain: ok"
        );
        assert_eq!(value["markdown"], true);
        assert_eq!(parsed(&msg, Style::Text)["message"], "fallback body");
        // Without sections the escaped body shows, one line per line.
        assert_eq!(parsed(&note("my_list\nnext", "{}"), Style::Markdown)["message"], "my\\_list  \nnext");
    }

    #[test]
    fn the_message_is_cut_at_4096_bytes_on_a_character_boundary_with_an_ellipsis() {
        assert_eq!(cut_bytes("short", MESSAGE_LIMIT), "short");
        let exact = "a".repeat(MESSAGE_LIMIT);
        assert_eq!(cut_bytes(&exact, MESSAGE_LIMIT), exact, "exactly the limit is not cut");
        let ascii = cut_bytes(&"a".repeat(5000), MESSAGE_LIMIT);
        assert_eq!(ascii.len(), MESSAGE_LIMIT);
        assert!(ascii.ends_with('…'));
        // 3-byte and 4-byte characters never leave half a character.
        for unit in ["\u{65E5}", "\u{1F534}", "\u{00E9}"] {
            let cut = cut_bytes(&unit.repeat(5000), MESSAGE_LIMIT);
            assert!(cut.len() <= MESSAGE_LIMIT, "{} bytes", cut.len());
            assert!(cut.ends_with('…'));
            assert!(cut.chars().rev().skip(1).all(|ch| unit.starts_with(ch)), "only whole characters");
        }
        let long = parsed(&note(&"x".repeat(9000), "{}"), Style::Text);
        assert_eq!(long["message"].as_str().unwrap().len(), MESSAGE_LIMIT);
        let mut titled = note("b", "{}");
        titled.title = "t".repeat(400);
        assert_eq!(parsed(&titled, Style::Text)["title"].as_str().unwrap().chars().count(), 250);
    }

    #[test]
    fn the_request_is_a_json_post_to_the_server_root_with_a_bearer_only_when_there_is_a_token() {
        mock::reset();
        mock::http_respond(http::Method::Post, BASE, 200, "{}");
        assert!(deliver(&target("SECRET-TOKEN", Style::Text), &note("Hello", "{}")).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1, "exactly one attempt per call");
        assert_eq!(sent[0].method(), http::Method::Post);
        assert_eq!(sent[0].url(), BASE);
        assert!(sent[0].headers().contains(&("content-type".to_string(), "application/json".to_string())));
        assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer SECRET-TOKEN".to_string())));
        assert_eq!(sent[0].body_bytes(), request_body("my-topic", Style::Text, &note("Hello", "{}")).as_bytes());
        assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
        // A public topic sends no Authorization header, and a trailing slash reaches the same server.
        mock::reset();
        mock::http_respond(http::Method::Post, BASE, 200, "{}");
        let slashed = Target { url: "https://ntfy.test/", topic: "t", token: "  ", style: Style::Text };
        assert!(deliver(&slashed, &note("Hello", "{}")).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent[0].url(), BASE);
        assert!(sent[0].headers().iter().all(|(name, _)| name != "authorization"));
    }

    #[test]
    fn deliver_maps_each_outcome_end_to_end_without_leaking_the_token() {
        type Answer<'a> = Result<(u16, Option<&'a str>), &'a str>;
        let cases: [(Answer<'_>, &str); 6] = [
            (Ok((200, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((500, None)), "transient"),
            (Ok((401, None)), "permanent"),
            (Err("connection refused (Bearer SECRET-TOKEN)"), "transient"),
            (Ok((302, Some("https://ntfy.test/?auth=SECRET-TOKEN"))), "permanent"),
        ];
        for (answer, want) in cases {
            mock::reset();
            match answer {
                Ok((status, retry)) => {
                    let headers =
                        retry.map(|value| ("Retry-After".to_string(), value.to_string())).into_iter().collect();
                    mock::http_respond_with(http::Method::Post, BASE, http::Response::new(status, headers, Vec::new()));
                }
                Err(message) => mock::http_fail(http::Method::Post, BASE, message),
            }
            let outcome = deliver(&target("SECRET-TOKEN", Style::Text), &note("Hello", "{}"));
            assert_eq!(shape(&outcome), want);
            if let Err(err) = &outcome {
                assert!(!format!("{err:?}").contains("SECRET-TOKEN"), "the token is in no error: {err:?}");
            }
            assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")), "nor in a log line");
        }
    }

    #[test]
    fn a_missing_or_unusable_setting_is_permanent_and_sends_nothing() {
        mock::reset();
        for (url, topic) in [("", "t"), (BASE, ""), ("  ", "  "), ("ftp://ntfy.test", "t"), ("ntfy.test", "t")] {
            let target = Target { url, topic, token: "", style: Style::Text };
            assert_eq!(shape(&deliver(&target, &note("Hello", "{}"))), "permanent", "{url:?} {topic:?}");
        }
        assert!(mock::http_requests().is_empty());
    }
}
