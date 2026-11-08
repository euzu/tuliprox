//! The Pushover side of the plugin: build the message request and say how the
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST {url}` (Pushover's messages API) with an
//! `application/x-www-form-urlencoded` body of `token`, `user`, `title`, `message`,
//! `priority`, `timestamp` and, in the `html` style, `html=1`. The credentials are
//! in the body, never in the URL.
//!
//! * `title` is cut at 250 characters and left out when empty (Pushover then shows
//!   the application name).
//! * `priority` is Pushover's scale: -1 (quiet) for info, 0 for a warning, 1 (high
//!   priority) for an error. A critical event reaches a plugin as an error.
//! * `timestamp` is the event's time, so a message delivered late through the
//!   outbox still shows when it happened.
//! * `message` is at most 1024 characters. A cut ends in `…`.
//!
//! Two styles:
//!
//! * `text` (default): `message` is the event's plain-text body.
//! * `html`: `message` is the event's sections as a bold heading with its lines below
//!   (else the escaped body), `html=1`. `&`, `<` and `>` are written as entities. The
//!   message is assembled from whole lines, so the 1024-character cut can never split
//!   an entity or a tag.
//!
//! # Retry contract
//!
//! The status mapping and the `Retry-After` handling are the SDK's
//! (`tuliprox_plugin_sdk::notify`): 2xx delivered, 408 and 429 retried after
//! `Retry-After`, 5xx except 501 and 505 transient, everything else permanent (a
//! wrong token or user key is a 400). No answer at all is transient; a missing or
//! unusable setting is permanent.
//!
//! # The token and the user key are credentials
//!
//! No error string, log line or reason contains them, nor anything a transport error
//! says, nor anything a server answered. Messages are built from the status or a fixed
//! phrase.

use serde::Deserialize;
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// Pushover's messages endpoint.
pub const DEFAULT_URL: &str = "https://api.pushover.net/1/messages.json";

/// Pushover truncates a message here, in characters.
const MESSAGE_LIMIT: usize = 1024;
/// And a title here.
const TITLE_LIMIT: usize = 250;

/// Below the host's 30 s send timeout, so a silent server surfaces as a
/// transient error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// How a notification is shown in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// The plain-text body.
    #[default]
    Text,
    /// The sections as HTML, else the escaped body.
    Html,
}

/// Where and as whom to publish.
pub struct Target<'a> {
    /// Empty means Pushover's own endpoint.
    pub url: &'a str,
    pub token: &'a str,
    pub user: &'a str,
    pub style: Style,
}

/// What this plugin reads of the event JSON besides the notification itself.
#[derive(Deserialize, Default)]
struct Event {
    /// Unix seconds.
    #[serde(default)]
    timestamp: Option<i64>,
}

/// Pushover's priority scale. There is no `critical`: it is delivered as `error`.
const fn priority(severity: Severity) -> i8 {
    match severity {
        Severity::Info => -1,
        Severity::Warn => 0,
        Severity::Error => 1,
    }
}

fn chars(text: &str) -> usize { text.chars().count() }

/// `text` cut to at most `limit` characters; a cut ends in `…` (which counts).
fn cut_chars(text: &str, limit: usize) -> String {
    if chars(text) <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// `text` with `&`, `<` and `>` written as entities.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `text` escaped, then cut to `limit` characters without cutting an entity in half;
/// a cut ends in `…`.
fn escape_html_cut(text: &str, limit: usize) -> String {
    let full = escape_html(text);
    if chars(&full) <= limit {
        return full;
    }
    let room = limit.saturating_sub(1);
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        let piece = escape_html(ch.encode_utf8(&mut [0; 4]));
        if count + chars(&piece) > room {
            break;
        }
        out.push_str(&piece);
        count += chars(&piece);
    }
    out.push('…');
    out
}

/// The message in the `html` style: the sections as a bold heading and its lines, a blank
/// line between sections; else the escaped body. Whole lines are added until the limit, the
/// first one that does not fit is cut, and the rest is left out under a `…`.
fn html_message(msg: &Notification) -> String {
    /// One output line: its HTML, and the plain text it came from when it can be cut.
    struct Line<'a> {
        html: String,
        raw: Option<&'a str>,
    }
    let sections = notify::sections_of(&msg.payload);
    let mut lines: Vec<Line<'_>> = Vec::new();
    for section in &sections {
        let kept: Vec<&str> =
            section.lines.iter().map(|line| line.trim_end()).filter(|line| !line.trim().is_empty()).collect();
        if kept.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line { html: String::new(), raw: None });
        }
        let heading = section.heading.trim();
        if !heading.is_empty() {
            lines.push(Line { html: format!("<b>{}</b>", escape_html(heading)), raw: None });
        }
        lines.extend(kept.into_iter().map(|line| Line { html: escape_html(line), raw: Some(line) }));
    }
    if lines.is_empty() {
        return escape_html_cut(&msg.body, MESSAGE_LIMIT);
    }
    let joined_len = lines.iter().map(|line| chars(&line.html)).sum::<usize>() + lines.len() - 1;
    if joined_len <= MESSAGE_LIMIT {
        return lines.iter().map(|line| line.html.as_str()).collect::<Vec<_>>().join("\n");
    }
    // Too long: whole lines while they fit, with room kept for a closing `\n…`, so a tag or an
    // entity is never half in. A text line that does not fit is cut at an entity boundary.
    let mut out = String::new();
    for line in &lines {
        let separator = usize::from(!out.is_empty());
        if chars(&out) + separator + chars(&line.html) + 2 > MESSAGE_LIMIT {
            let room = MESSAGE_LIMIT.saturating_sub(chars(&out) + separator);
            if let Some(raw) = line.raw.filter(|_| room >= 10 && separator == 1) {
                out.push('\n');
                // Cut below the line's own length even when it would just fit: more follows, and the
                // `…` is what tells the reader so.
                out.push_str(&escape_html_cut(raw, room.min(chars(&line.html).saturating_sub(1))));
                return out;
            }
            break;
        }
        if separator == 1 {
            out.push('\n');
        }
        out.push_str(&line.html);
    }
    if out.is_empty() {
        // Not even the first line fits (a very long heading): fall back to the escaped body.
        return escape_html_cut(&msg.body, MESSAGE_LIMIT);
    }
    out.push_str("\n…");
    out
}

/// One form field, percent-encoded as `application/x-www-form-urlencoded` wants it: unreserved
/// characters stay, a space is `+`, everything else is `%XX` per UTF-8 byte.
fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(char::from(byte)),
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The form body for `msg`.
pub fn request_body(token: &str, user: &str, style: Style, msg: &Notification) -> String {
    let event = serde_json::from_str::<Event>(&msg.payload).unwrap_or_default();
    let message = match style {
        Style::Text => cut_chars(&msg.body, MESSAGE_LIMIT),
        Style::Html => html_message(msg),
    };
    let mut fields: Vec<(&str, String)> = vec![("token", token.trim().to_string()), ("user", user.trim().to_string())];
    let title = cut_chars(msg.title.trim(), TITLE_LIMIT);
    if !title.is_empty() {
        fields.push(("title", title));
    }
    fields.push(("message", message));
    fields.push(("priority", priority(msg.severity).to_string()));
    if let Some(time) = event.timestamp {
        fields.push(("timestamp", time.to_string()));
    }
    if style == Style::Html {
        fields.push(("html", "1".to_string()));
    }
    fields.iter().map(|(name, value)| format!("{name}={}", form_encode(value))).collect::<Vec<_>>().join("&")
}

/// Send `msg` to Pushover as `target` and say how the attempt ended.
pub fn deliver(target: &Target<'_>, msg: &Notification) -> Result<(), DeliveryError> {
    if target.token.trim().is_empty() || target.user.trim().is_empty() {
        return Err(DeliveryError::Permanent(
            "pushover-notifier is not configured: `token` and `user` are required".to_string(),
        ));
    }
    let url = if target.url.trim().is_empty() { DEFAULT_URL } else { target.url.trim() };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(DeliveryError::Permanent("`url` must be an http:// or https:// URL".to_string()));
    }
    let request = http::Request::post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .timeout(HTTP_TIMEOUT)
        .body(request_body(target.token, target.user, target.style, msg).into_bytes());
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Permanent("pushover request failed: the url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("pushover request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to Pushover"),
        Err(DeliveryError::Permanent(reason)) => warn!("pushover refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("pushover will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("pushover is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    fn note(body: &str, payload: &str) -> Notification {
        Notification {
            title: "Title".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: payload.to_string(),
        }
    }

    fn target(style: Style) -> Target<'static> { Target { url: "", token: "SECRET-APP", user: "SECRET-USER", style } }

    /// The form body as (name, value) pairs, percent-decoded.
    fn decode(body: &str) -> Vec<(String, String)> {
        body.split('&')
            .map(|pair| {
                let (name, value) = pair.split_once('=').expect("name=value");
                let mut bytes = Vec::new();
                let raw = value.replace('+', " ");
                let raw = raw.as_bytes();
                let mut at = 0;
                while at < raw.len() {
                    if raw[at] == b'%' {
                        bytes.push(u8::from_str_radix(std::str::from_utf8(&raw[at + 1..at + 3]).unwrap(), 16).unwrap());
                        at += 3;
                    } else {
                        bytes.push(raw[at]);
                        at += 1;
                    }
                }
                (name.to_string(), String::from_utf8(bytes).expect("utf-8"))
            })
            .collect()
    }

    fn field(fields: &[(String, String)], name: &str) -> Option<String> {
        fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }

    fn fields(msg: &Notification, style: Style) -> Vec<(String, String)> {
        decode(&request_body("tok", "usr", style, msg))
    }

    fn sectioned(sections: &str) -> Notification {
        note("fallback body", &format!(r#"{{"timestamp":784111777,"sections":{sections}}}"#))
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
    fn severity_maps_onto_the_pushover_priority_scale() {
        assert_eq!((priority(Severity::Info), priority(Severity::Warn), priority(Severity::Error)), (-1, 0, 1));
        for severity in [Severity::Info, Severity::Warn, Severity::Error] {
            let mut msg = note("b", "{}");
            msg.severity = severity;
            assert_eq!(field(&fields(&msg, Style::Text), "priority").unwrap(), priority(severity).to_string());
        }
    }

    #[test]
    fn a_form_value_is_percent_encoded_like_application_x_www_form_urlencoded() {
        assert_eq!(form_encode("abc XYZ-_.*09"), "abc+XYZ-_.*09");
        assert_eq!(form_encode("a&b=c+d%e/f?g"), "a%26b%3Dc%2Bd%25e%2Ff%3Fg");
        assert_eq!(form_encode("é\u{65E5}\u{1F534}\n"), "%C3%A9%E6%97%A5%F0%9F%94%B4%0A");
        // Every value survives the trip through a decoder.
        let msg = note("a&b=c d\n\u{65E5}\u{1F534} 100%", "{}");
        assert_eq!(field(&fields(&msg, Style::Text), "message").unwrap(), "a&b=c d\n\u{65E5}\u{1F534} 100%");
    }

    #[test]
    fn text_style_sends_the_credentials_title_body_priority_and_the_event_time() {
        let msg = note("the body", r#"{"timestamp":784111777}"#);
        let body = request_body("tok", "usr", Style::Text, &msg);
        let fields = decode(&body);
        assert_eq!(
            fields,
            [
                ("token", "tok"),
                ("user", "usr"),
                ("title", "Title"),
                ("message", "the body"),
                ("priority", "-1"),
                ("timestamp", "784111777"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );
        assert!(field(&fields, "html").is_none());
        // No event time, no title: neither field is sent.
        let mut bare = note("b", "{}");
        bare.title = "  ".to_string();
        let fields = fields_of(&bare);
        assert!(field(&fields, "title").is_none() && field(&fields, "timestamp").is_none());
    }

    fn fields_of(msg: &Notification) -> Vec<(String, String)> { fields(msg, Style::Text) }

    #[test]
    fn title_and_message_are_cut_at_pushovers_limits_by_character() {
        let mut msg = note(&"b".repeat(3000), "{}");
        msg.title = "t".repeat(400);
        let got = fields_of(&msg);
        assert_eq!(chars(&field(&got, "title").unwrap()), 250);
        let message = field(&got, "message").unwrap();
        assert_eq!(chars(&message), 1024);
        assert!(message.ends_with('…'));
        // Characters, not bytes and not UTF-16 units.
        for unit in ["\u{65E5}", "\u{1F534}"] {
            let cut = field(&fields_of(&note(&unit.repeat(2000), "{}")), "message").unwrap();
            assert_eq!(chars(&cut), 1024);
        }
        let exact = "x".repeat(1024);
        assert_eq!(field(&fields_of(&note(&exact, "{}")), "message").unwrap(), exact, "exactly the limit is not cut");
    }

    #[test]
    fn html_style_renders_sections_as_bold_headings_and_sets_the_html_flag() {
        let msg = sectioned(
            r#"[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels"]},{"heading":"Targets","lines":["main: ok"]}]"#,
        );
        let got = fields(&msg, Style::Html);
        assert_eq!(
            field(&got, "message").unwrap(),
            "<b>Sources</b>\ndemo-in (M3u)\n1/2 -&gt; 1/2 groups/channels\n\n<b>Targets</b>\nmain: ok"
        );
        assert_eq!(field(&got, "html").unwrap(), "1");
        assert_eq!(field(&fields(&msg, Style::Text), "message").unwrap(), "fallback body");
        // Without sections the escaped body shows.
        assert_eq!(field(&fields(&note("a <b> & c", "{}"), Style::Html), "message").unwrap(), "a &lt;b&gt; &amp; c");
    }

    #[test]
    fn html_headings_and_lines_are_escaped_and_empty_sections_are_left_out() {
        let msg = sectioned(
            r#"[{"heading":"R&D <team>","lines":["a < b"]},{"heading":"Empty","lines":[]},{"heading":"Blank","lines":["  "]},{"heading":" ","lines":["kept"]}]"#,
        );
        assert_eq!(
            field(&fields(&msg, Style::Html), "message").unwrap(),
            "<b>R&amp;D &lt;team&gt;</b>\na &lt; b\n\nkept"
        );
        let only_empty = sectioned(r#"[{"heading":"Empty","lines":[]}]"#);
        assert_eq!(field(&fields(&only_empty, Style::Html), "message").unwrap(), "fallback body");
    }

    #[test]
    fn the_html_message_stays_within_the_limit_and_never_splits_a_tag_or_an_entity() {
        // 60 sections of two lines: far more than 1024 characters.
        let many: Vec<String> =
            (0..60).map(|n| format!(r#"{{"heading":"S{n}","lines":["a & b < c","second line"]}}"#)).collect();
        let message = field(&fields(&sectioned(&format!("[{}]", many.join(","))), Style::Html), "message").unwrap();
        assert!(chars(&message) <= 1024, "{} characters", chars(&message));
        assert!(message.ends_with('…'));
        let opened = message.matches("<b>").count();
        assert_eq!(opened, message.matches("</b>").count(), "every opened tag is closed");
        let body = message.trim_end_matches('…');
        for (at, _) in body.match_indices('&') {
            let rest = &body[at..];
            assert!(["&amp;", "&lt;", "&gt;"].iter().any(|entity| rest.starts_with(entity)), "split entity: {rest:.8}");
        }
        // One very long line is cut inside, at an entity boundary.
        let cut = escape_html_cut(&"&".repeat(1000), 11);
        assert_eq!(cut, format!("{}…", "&amp;".repeat(2)));
        let long_body = field(&fields(&note(&"<".repeat(2000), "{}"), Style::Html), "message").unwrap();
        assert!(chars(&long_body) <= 1024 && long_body.ends_with('…'));
        assert!(long_body.trim_end_matches('…').split("&lt;").all(str::is_empty), "only whole entities");
    }

    #[test]
    fn a_long_line_is_cut_to_what_fits_and_a_full_message_still_leaves_room_for_the_ellipsis() {
        // A heading and a body line that would sum to exactly 1024 characters, then more sections.
        let line = "x".repeat(MESSAGE_LIMIT - "<b>H</b>".chars().count() - 1);
        let msg =
            sectioned(&format!(r#"[{{"heading":"H","lines":["{line}"]}},{{"heading":"More","lines":["tail"]}}]"#));
        let message = field(&fields(&msg, Style::Html), "message").unwrap();
        assert!(chars(&message) <= MESSAGE_LIMIT, "{} characters", chars(&message));
        assert!(message.starts_with("<b>H</b>\nxxx"), "the heading and as much of the line as fits");
        assert!(message.ends_with('…'));
        assert!(!message.contains("tail"), "what did not fit is left out");
        // A cut line never splits an entity.
        let amp = sectioned(&format!(r#"[{{"heading":"H","lines":["{}"]}}]"#, "&".repeat(600)));
        let cut = field(&fields(&amp, Style::Html), "message").unwrap();
        assert!(chars(&cut) <= MESSAGE_LIMIT && cut.ends_with('…'));
        assert!(cut.trim_start_matches("<b>H</b>\n").trim_end_matches('…').split("&amp;").all(str::is_empty));
        // Everything that fits is sent whole, with no ellipsis.
        let fits = sectioned(r#"[{"heading":"H","lines":["short"]}]"#);
        assert_eq!(field(&fields(&fits, Style::Html), "message").unwrap(), "<b>H</b>\nshort");
    }

    #[test]
    fn the_request_is_a_form_post_with_the_credentials_in_the_body_and_never_in_the_url() {
        mock::reset();
        mock::http_respond(http::Method::Post, DEFAULT_URL, 200, r#"{"status":1}"#);
        assert!(deliver(&target(Style::Text), &note("Hello", "{}")).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1, "exactly one attempt per call");
        assert_eq!(sent[0].method(), http::Method::Post);
        assert_eq!(sent[0].url(), DEFAULT_URL);
        assert!(sent[0]
            .headers()
            .contains(&("content-type".to_string(), "application/x-www-form-urlencoded".to_string())));
        assert_eq!(
            sent[0].body_bytes(),
            request_body("SECRET-APP", "SECRET-USER", Style::Text, &note("Hello", "{}")).as_bytes()
        );
        assert!(String::from_utf8_lossy(sent[0].body_bytes()).contains("token=SECRET-APP"));
        assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
        // A configured endpoint is used as given.
        mock::reset();
        mock::http_respond(http::Method::Post, "https://push.test/1/messages.json", 200, "{}");
        let custom = Target { url: " https://push.test/1/messages.json ", ..target(Style::Text) };
        assert!(deliver(&custom, &note("Hello", "{}")).is_ok());
        assert_eq!(mock::http_requests()[0].url(), "https://push.test/1/messages.json");
    }

    #[test]
    fn deliver_maps_each_outcome_end_to_end_without_leaking_a_credential() {
        type Answer<'a> = Result<(u16, Option<&'a str>), &'a str>;
        let cases: [(Answer<'_>, &str); 6] = [
            (Ok((200, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((500, None)), "transient"),
            (Ok((400, None)), "permanent"),
            (Err("connection refused (token=SECRET-APP&user=SECRET-USER)"), "transient"),
            (Ok((302, Some("https://push.test/?token=SECRET-APP"))), "permanent"),
        ];
        for (answer, want) in cases {
            mock::reset();
            match answer {
                Ok((status, retry)) => {
                    let headers =
                        retry.map(|value| ("Retry-After".to_string(), value.to_string())).into_iter().collect();
                    mock::http_respond_with(
                        http::Method::Post,
                        DEFAULT_URL,
                        http::Response::new(status, headers, Vec::new()),
                    );
                }
                Err(message) => mock::http_fail(http::Method::Post, DEFAULT_URL, message),
            }
            let outcome = deliver(&target(Style::Text), &note("Hello", "{}"));
            assert_eq!(shape(&outcome), want);
            if let Err(err) = &outcome {
                assert!(!format!("{err:?}").contains("SECRET"), "no credential is in an error: {err:?}");
            }
            assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET")), "nor in a log line");
        }
    }

    #[test]
    fn a_missing_or_unusable_setting_is_permanent_and_sends_nothing() {
        mock::reset();
        for (token, user, url) in
            [("", "u", ""), ("t", "", ""), ("  ", "  ", ""), ("t", "u", "ftp://push.test"), ("t", "u", "push.test")]
        {
            let target = Target { url, token, user, style: Style::Text };
            assert_eq!(shape(&deliver(&target, &note("Hello", "{}"))), "permanent", "{token:?} {user:?} {url:?}");
        }
        assert!(mock::http_requests().is_empty());
    }
}
