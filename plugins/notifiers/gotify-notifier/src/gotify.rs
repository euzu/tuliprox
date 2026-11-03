//! The Gotify side of the plugin: build the push request and say how the
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST {url}/message` with `Content-Type: application/json` and the
//! application token in the `X-Gotify-Key` header (Gotify also takes it as a
//! `?token=` query parameter; the header keeps it out of the URL, so it cannot
//! end up in a proxy log or an error). The body is `title`, `message` and
//! `priority`, Gotify's 0-10 scale: 2 for info, 5 for a warning, 7 for an error.
//! A critical event reaches a plugin as an error.
//!
//! Two styles:
//!
//! * `text` (default): `message` is the event's plain-text body.
//! * `markdown`: `message` is the event's sections as a bold heading with its
//!   lines below (else the body), markdown-escaped, with the
//!   `client::display` extra that makes Gotify clients render it.
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
use serde_json::json;
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// Below the host's 30 s send timeout, so a silent server surfaces as a
/// transient error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// How a notification is shown in the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// The plain-text body.
    #[default]
    Text,
    /// The sections as markdown, else the escaped body.
    Markdown,
}

/// Gotify's 0-10 priority scale. There is no `critical`: it is delivered as `error`.
const fn priority(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 2,
        Severity::Warn => 5,
        Severity::Error => 7,
    }
}

/// The message endpoint of the server at `url`, without a doubled slash.
fn endpoint(url: &str) -> String { format!("{}/message", url.trim().trim_end_matches('/')) }

/// The request body for `msg`.
pub fn request_body(msg: &Notification, style: Style) -> String {
    let priority = priority(msg.severity);
    match style {
        Style::Text => json!({ "title": msg.title, "message": msg.body, "priority": priority }).to_string(),
        Style::Markdown => {
            let message = notify::markdown::sections(&notify::sections_of(&msg.payload))
                .unwrap_or_else(|| notify::markdown::lines(msg.body.lines()));
            json!({
                "title": msg.title,
                "message": message,
                "priority": priority,
                "extras": { "client::display": { "contentType": "text/markdown" } },
            })
            .to_string()
        }
    }
}

/// Post `msg` to the Gotify server at `url` with the application `token`, and
/// say how the attempt ended.
pub fn deliver(url: &str, token: &str, style: Style, msg: &Notification) -> Result<(), DeliveryError> {
    if url.trim().is_empty() || token.trim().is_empty() {
        return Err(DeliveryError::Permanent(
            "gotify-notifier is not configured: `url` and `token` are required".to_string(),
        ));
    }
    if !(url.trim().starts_with("http://") || url.trim().starts_with("https://")) {
        return Err(DeliveryError::Permanent("`url` must be an http:// or https:// URL".to_string()));
    }
    let request = http::Request::post(endpoint(url))
        .header("content-type", "application/json")
        .header("x-gotify-key", token.trim())
        .timeout(HTTP_TIMEOUT)
        .body(request_body(msg, style).into_bytes());
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Permanent("gotify request failed: the url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("gotify request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to Gotify"),
        Err(DeliveryError::Permanent(reason)) => warn!("gotify refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("gotify will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("gotify is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tuliprox_plugin_sdk::mock;

    const BASE: &str = "https://gotify.test";
    const MESSAGE_URL: &str = "https://gotify.test/message";

    fn note(body: &str, payload: &str) -> Notification {
        Notification {
            title: "Title".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: payload.to_string(),
        }
    }

    fn parsed(msg: &Notification, style: Style) -> Value {
        serde_json::from_str(&request_body(msg, style)).expect("the body is json")
    }

    fn sectioned(sections: &str) -> Notification { note("fallback body", &format!(r#"{{"sections":{sections}}}"#)) }

    fn shape(outcome: &Result<(), DeliveryError>) -> String {
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(DeliveryError::Transient(_)) => "transient".to_string(),
            Err(DeliveryError::Permanent(_)) => "permanent".to_string(),
            Err(DeliveryError::RetryAfter(secs)) => format!("retry-after:{secs}"),
        }
    }

    #[test]
    fn severity_maps_onto_the_gotify_priority_scale() {
        assert_eq!(priority(Severity::Info), 2);
        assert_eq!(priority(Severity::Warn), 5);
        assert_eq!(priority(Severity::Error), 7);
        for severity in [Severity::Info, Severity::Warn, Severity::Error] {
            let mut msg = note("b", "{}");
            msg.severity = severity;
            assert_eq!(parsed(&msg, Style::Text)["priority"], priority(severity));
        }
    }

    #[test]
    fn the_endpoint_has_no_doubled_slash() {
        for url in ["https://g.test", "https://g.test/", "  https://g.test//  "] {
            assert_eq!(endpoint(url), "https://g.test/message", "{url:?}");
        }
        assert_eq!(endpoint("https://g.test/sub/path/"), "https://g.test/sub/path/message");
    }

    #[test]
    fn text_style_sends_the_title_the_body_and_a_priority() {
        let value = parsed(&note("line 1\nline 2", "{}"), Style::Text);
        assert_eq!(value, json!({"title": "Title", "message": "line 1\nline 2", "priority": 2}));
    }

    #[test]
    fn markdown_style_renders_sections_with_bold_headings_and_hard_line_breaks() {
        let msg = sectioned(
            r#"[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels"]},{"heading":"Targets","lines":["main: ok"]}]"#,
        );
        let value = parsed(&msg, Style::Markdown);
        assert_eq!(
            value["message"],
            "**Sources**  \ndemo-in (M3u)  \n1/2 -> 1/2 groups/channels\n\n**Targets**  \nmain: ok"
        );
        assert_eq!(value["extras"]["client::display"]["contentType"], "text/markdown");
        assert_eq!(value["priority"], 2);
        // Text style keeps the plain body and does not ask for markdown.
        let plain = parsed(&msg, Style::Text);
        assert_eq!(plain["message"], "fallback body");
        assert!(plain.get("extras").is_none());
    }

    #[test]
    fn markdown_style_without_sections_shows_the_escaped_body_with_its_line_breaks() {
        let value = parsed(&note("first\nsecond", "{}"), Style::Markdown);
        assert_eq!(value["message"], "first  \nsecond");
        let empty = parsed(
            &sectioned(r#"[{"heading":"Empty","lines":[]},{"heading":"Blank","lines":["  "]}]"#),
            Style::Markdown,
        );
        assert_eq!(empty["message"], "fallback body", "only empty sections: the body shows");
    }

    #[test]
    fn the_request_carries_the_token_in_a_header_and_never_in_the_url() {
        mock::reset();
        mock::http_respond(http::Method::Post, MESSAGE_URL, 200, "{}");
        assert!(deliver(BASE, "SECRET-TOKEN", Style::Text, &note("Hello", "{}")).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1, "exactly one attempt per call");
        assert_eq!(sent[0].method(), http::Method::Post);
        assert_eq!(sent[0].url(), MESSAGE_URL);
        assert!(sent[0].headers().contains(&("x-gotify-key".to_string(), "SECRET-TOKEN".to_string())));
        assert!(sent[0].headers().contains(&("content-type".to_string(), "application/json".to_string())));
        assert_eq!(sent[0].body_bytes(), request_body(&note("Hello", "{}"), Style::Text).as_bytes());
        assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
        // A server URL with a trailing slash reaches the same endpoint.
        mock::reset();
        mock::http_respond(http::Method::Post, MESSAGE_URL, 200, "{}");
        assert!(deliver("https://gotify.test/", "t", Style::Text, &note("Hello", "{}")).is_ok());
        assert_eq!(mock::http_requests()[0].url(), MESSAGE_URL);
    }

    #[test]
    fn deliver_maps_each_outcome_end_to_end_without_leaking_the_token() {
        type Answer<'a> = Result<(u16, Option<&'a str>), &'a str>;
        let cases: [(Answer<'_>, &str); 6] = [
            (Ok((200, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((500, None)), "transient"),
            (Ok((401, None)), "permanent"),
            (Err("connection refused: https://gotify.test/message?token=SECRET-TOKEN"), "transient"),
            (Ok((302, Some("https://gotify.test/message?token=SECRET-TOKEN"))), "permanent"),
        ];
        for (answer, want) in cases {
            mock::reset();
            match answer {
                Ok((status, retry)) => {
                    let headers =
                        retry.map(|value| ("Retry-After".to_string(), value.to_string())).into_iter().collect();
                    mock::http_respond_with(
                        http::Method::Post,
                        MESSAGE_URL,
                        http::Response::new(status, headers, Vec::new()),
                    );
                }
                Err(message) => mock::http_fail(http::Method::Post, MESSAGE_URL, message),
            }
            let outcome = deliver(BASE, "SECRET-TOKEN", Style::Text, &note("Hello", "{}"));
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
        for (url, token) in [("", "t"), (BASE, ""), ("  ", "  "), ("ftp://gotify.test", "t"), ("gotify.test", "t")] {
            assert_eq!(
                shape(&deliver(url, token, Style::Text, &note("Hello", "{}"))),
                "permanent",
                "{url:?} {token:?}"
            );
        }
        assert!(mock::http_requests().is_empty());
    }
}
