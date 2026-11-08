//! The Slack side of the plugin: build the webhook request and say how the
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST {url}` with `Content-Type: application/json`. Two styles:
//!
//! * `blocks` (default): a header with the title (a `[warn]` or `[error]` marker
//!   in front when it is not routine), one section block per section of the
//!   event (else one for the body), and a footer with the event id, the instance
//!   and the time. The top-level `text` repeats the header, as Slack shows it in
//!   notifications.
//! * `text`: `{"text": <body>}`.
//!
//! Text is Slack `mrkdwn`: `&`, `<` and `>` are written as entities, which also
//! keeps event text from turning into a mention (`<!channel>`) or a link. Cuts are
//! by character at Slack's limits: 150 for a header, 3000 for a section and for
//! `text`, 50 blocks in all.
//!
//! # Retry contract
//!
//! The status mapping and the `Retry-After` handling are the SDK's
//! (`tuliprox_plugin_sdk::notify`): 2xx delivered, 408 and 429 retried after
//! `Retry-After`, 5xx except 501 and 505 transient, everything else permanent.
//! No answer at all is transient.
//!
//! # The webhook URL is a credential
//!
//! No error string, log line or reason contains it, nor anything a transport
//! error says: those can embed the URL. Messages are built from the status or a
//! fixed phrase.

use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// Slack rejects a section or `text` longer than this.
const TEXT_LIMIT: usize = 3000;
/// Slack rejects a header block text longer than this.
const HEADER_LIMIT: usize = 150;
/// Blocks in one message.
const MAX_BLOCKS: usize = 50;

/// Below the host's 30 s send timeout, so a silent webhook host surfaces as a
/// transient error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// How a notification is shown in the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Block Kit: header, sections (else the body), footer.
    #[default]
    Blocks,
    /// Plain `text`.
    Text,
}

/// A titled block of plain-text lines of the event (`sections` of the event JSON).
#[derive(Deserialize)]
struct Section {
    #[serde(default)]
    heading: String,
    #[serde(default)]
    lines: Vec<String>,
}

/// What this plugin reads of the event JSON besides the notification itself.
#[derive(Deserialize, Default)]
struct Event {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    instance: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    sections: Vec<Section>,
}

fn chars(text: &str) -> usize { text.chars().count() }

fn cut(text: &str, limit: usize) -> String { text.chars().take(limit).collect() }

/// Slack has no severity field, so it goes in the header text where a human
/// will see it; a routine notification stays unmarked.
const fn marker(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "",
        Severity::Warn => "[warn] ",
        Severity::Error => "[error] ",
    }
}

/// `text` with `&`, `<` and `>` written as the entities Slack requires, cut to
/// `limit` characters without cutting an entity in half.
fn mrkdwn_cut(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        let piece = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            _ => "",
        };
        let width = if piece.is_empty() { 1 } else { piece.len() };
        if count + width > limit {
            break;
        }
        if piece.is_empty() {
            out.push(ch);
        } else {
            out.push_str(piece);
        }
        count += width;
    }
    out
}

/// [`mrkdwn_cut`], ending in `…` when something was cut.
fn mrkdwn_ellipsis(text: &str, limit: usize) -> String {
    let full = mrkdwn_cut(text, usize::MAX);
    if chars(&full) <= limit {
        return full;
    }
    format!("{}…", mrkdwn_cut(text, limit.saturating_sub(1)))
}

fn section_block(text: &str) -> Value { json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }) }

/// One section block per section that has a line, at most `slots` of them; what
/// had to be left out is said in a last block.
fn section_blocks(sections: &[Section], slots: usize) -> Vec<Value> {
    let usable: Vec<(&str, String)> = sections
        .iter()
        .filter_map(|section| {
            let lines: Vec<&str> =
                section.lines.iter().map(|line| line.trim_end()).filter(|line| !line.trim().is_empty()).collect();
            (!lines.is_empty()).then(|| (section.heading.trim(), lines.join("\n")))
        })
        .collect();
    // One slot stays free for the note when there are more sections than slots.
    let shown = if usable.len() > slots { slots.saturating_sub(1) } else { usable.len() };
    let mut blocks: Vec<Value> = usable
        .iter()
        .take(shown)
        .map(|(heading, text)| {
            let text = if heading.is_empty() { text.clone() } else { format!("*{heading}*\n{text}") };
            section_block(&mrkdwn_ellipsis(&text, TEXT_LIMIT))
        })
        .collect();
    let omitted = usable.len() - shown;
    if omitted > 0 {
        blocks.push(section_block(&format!("… {omitted} more section(s) not shown")));
    }
    blocks
}

/// The request body for `msg`.
pub fn request_body(msg: &Notification, style: Style) -> String {
    match style {
        Style::Text => json!({ "text": mrkdwn_cut(&msg.body, TEXT_LIMIT) }).to_string(),
        Style::Blocks => {
            let event = serde_json::from_str::<Event>(&msg.payload).unwrap_or_default();
            let header = cut(&format!("{}{}", marker(msg.severity), msg.title), HEADER_LIMIT);
            let mut blocks = vec![json!({
                "type": "header",
                "text": { "type": "plain_text", "text": header, "emoji": true },
            })];
            // The footer is the last block; the header is the first.
            let footer = footer(&event);
            let slots = MAX_BLOCKS - 1 - usize::from(footer.is_some());
            let has_sections =
                event.sections.iter().any(|section| section.lines.iter().any(|line| !line.trim().is_empty()));
            if has_sections {
                // With sections the detail is in the blocks: the body is the same facts as one text.
                blocks.extend(section_blocks(&event.sections, slots));
            } else if msg.body.trim() != msg.title.trim() {
                blocks.push(section_block(&mrkdwn_ellipsis(&msg.body, TEXT_LIMIT)));
            }
            blocks.extend(footer);
            json!({ "text": header, "blocks": blocks }).to_string()
        }
    }
}

/// The context block under the message: the event id, the instance and the time.
fn footer(event: &Event) -> Option<Value> {
    let time = event.timestamp.and_then(notify::iso8601);
    let id = event.id.as_deref().filter(|id| !id.is_empty()).map(|id| format!("`{}`", mrkdwn_cut(id, 200)));
    let instance = event.instance.as_deref().filter(|name| !name.trim().is_empty()).map(|name| mrkdwn_cut(name, 200));
    let parts: Vec<String> = [id, instance, time].into_iter().flatten().collect();
    (!parts.is_empty())
        .then(|| json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": parts.join(" · ") }] }))
}

/// Post `msg` to the webhook `url` and say how the attempt ended.
pub fn deliver(url: &str, style: Style, msg: &Notification) -> Result<(), DeliveryError> {
    let request = http::Request::post(url)
        .header("content-type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .body(request_body(msg, style).into_bytes());
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        // An unusable url is retried too, so a corrected url still gets the queue.
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Transient("slack request failed: the webhook url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("slack request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to Slack"),
        Err(DeliveryError::Permanent(reason)) => warn!("slack refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("slack will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("slack is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const URL: &str = "https://hooks.slack.test/services/T000/B000/SECRET-TOKEN";

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

    fn sectioned(sections: &str) -> Notification {
        note(
            "fallback body",
            &format!(
                r#"{{"id":"playlist.update.completed","instance":"tv-box","timestamp":784111777,"sections":{sections}}}"#
            ),
        )
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
    fn only_non_routine_severities_get_a_marker_in_the_header_and_the_fallback_text() {
        for (severity, want) in
            [(Severity::Info, "Title"), (Severity::Warn, "[warn] Title"), (Severity::Error, "[error] Title")]
        {
            let mut msg = note("body", "{}");
            msg.severity = severity;
            let value = parsed(&msg, Style::Blocks);
            assert_eq!(value["text"], want);
            assert_eq!(value["blocks"][0]["type"], "header");
            assert_eq!(value["blocks"][0]["text"]["text"], want);
            assert_eq!(value["blocks"][0]["text"]["type"], "plain_text");
        }
    }

    #[test]
    fn sections_become_blocks_with_a_footer_and_replace_the_body_text() {
        let msg = sectioned(
            r#"[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels"]},{"heading":"Targets","lines":["main: ok"]}]"#,
        );
        let blocks = parsed(&msg, Style::Blocks)["blocks"].as_array().unwrap().clone();
        let kinds: Vec<_> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["header", "section", "section", "context"]);
        assert_eq!(blocks[1]["text"]["text"], "*Sources*\ndemo-in (M3u)\n1/2 -&gt; 1/2 groups/channels");
        assert_eq!(blocks[2]["text"]["text"], "*Targets*\nmain: ok");
        assert_eq!(blocks[3]["elements"][0]["text"], "`playlist.update.completed` · tv-box · 1994-11-06T08:49:37Z");
    }

    #[test]
    fn an_event_without_sections_shows_its_body_and_one_without_detail_shows_no_footer() {
        let blocks = parsed(&note("the body", "{}"), Style::Blocks)["blocks"].as_array().unwrap().clone();
        let kinds: Vec<_> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["header", "section"], "no id, instance or time means no footer");
        assert_eq!(blocks[1]["text"]["text"], "the body");
        // A body that only repeats the title adds nothing under it.
        let mut same = note("Title", "{}");
        same.title = "Title".to_string();
        let kinds: Vec<_> = parsed(&same, Style::Blocks)["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["header"]);
    }

    #[test]
    fn the_three_characters_slack_reads_as_markup_are_written_as_entities() {
        assert_eq!(mrkdwn_cut("a & b <!channel> <@U1|x> > c", 100), "a &amp; b &lt;!channel&gt; &lt;@U1|x&gt; &gt; c");
        let value = parsed(&note("<!here> all & sundry", "{}"), Style::Blocks);
        assert_eq!(value["blocks"][1]["text"]["text"], "&lt;!here&gt; all &amp; sundry");
        assert_eq!(parsed(&note("<!here>", "{}"), Style::Text)["text"], "&lt;!here&gt;");
        // The header is plain text: Slack does not read markup there, so nothing is rewritten.
        let mut msg = note("body", "{}");
        msg.title = "a & b".to_string();
        assert_eq!(parsed(&msg, Style::Blocks)["blocks"][0]["text"]["text"], "a & b");
    }

    #[test]
    fn a_heading_is_escaped_once_with_the_text_it_heads() {
        let msg = sectioned(r#"[{"heading":"R&D <team>","lines":["a & b"]}]"#);
        let text = parsed(&msg, Style::Blocks)["blocks"][1]["text"]["text"].as_str().unwrap().to_string();
        assert_eq!(text, "*R&amp;D &lt;team&gt;*\na &amp; b");
    }

    #[test]
    fn a_cut_never_splits_an_entity_and_counts_what_slack_counts() {
        // "&amp;" is five characters: at a limit of 7 two of them fit (10 would not), a third does not.
        assert_eq!(mrkdwn_cut("&&&", 7), "&amp;");
        assert_eq!(mrkdwn_cut("&&&", 10), "&amp;&amp;");
        assert_eq!(mrkdwn_cut("é漢😀", 3), "é漢😀", "characters, not bytes");
        assert_eq!(chars(&mrkdwn_ellipsis(&"x".repeat(5000), 3000)), 3000);
        assert!(mrkdwn_ellipsis(&"x".repeat(5000), 3000).ends_with('…'));
        assert_eq!(mrkdwn_ellipsis("short", 3000), "short");
    }

    #[test]
    fn header_section_and_text_are_cut_at_slacks_limits() {
        let mut msg = note(&"b".repeat(5000), "{}");
        msg.title = "t".repeat(400);
        let value = parsed(&msg, Style::Blocks);
        assert_eq!(chars(value["blocks"][0]["text"]["text"].as_str().unwrap()), 150);
        assert_eq!(chars(value["text"].as_str().unwrap()), 150);
        let section = value["blocks"][1]["text"]["text"].as_str().unwrap();
        assert_eq!(chars(section), 3000);
        assert!(section.ends_with('…'));
        assert_eq!(chars(parsed(&note(&"c".repeat(4000), "{}"), Style::Text)["text"].as_str().unwrap()), 3000);
    }

    #[test]
    fn empty_sections_and_blank_headings_are_handled() {
        let msg = sectioned(
            r#"[{"heading":"Empty","lines":[]},{"heading":"Blank","lines":["  ",""]},{"heading":" ","lines":["kept"]}]"#,
        );
        let blocks = parsed(&msg, Style::Blocks)["blocks"].as_array().unwrap().clone();
        let sections: Vec<_> = blocks.iter().filter(|b| b["type"] == "section").collect();
        assert_eq!(sections.len(), 1, "a section with no text is not a block");
        assert_eq!(sections[0]["text"]["text"], "kept", "no heading, no bold line");
        // Only empty sections: it is as if the event had none, so the body shows.
        let blocks = parsed(&sectioned(r#"[{"heading":"Empty","lines":[]}]"#), Style::Blocks)["blocks"].clone();
        assert_eq!(blocks[1]["text"]["text"], "fallback body");
    }

    #[test]
    fn a_message_never_has_more_than_50_blocks_and_names_what_was_left_out() {
        let many: Vec<String> = (0..120).map(|n| format!(r#"{{"heading":"S{n}","lines":["line"]}}"#)).collect();
        let blocks =
            parsed(&sectioned(&format!("[{}]", many.join(","))), Style::Blocks)["blocks"].as_array().unwrap().clone();
        assert_eq!(blocks.len(), 50, "header + 47 sections + the note + the footer");
        assert_eq!(blocks.first().unwrap()["type"], "header");
        assert_eq!(blocks.last().unwrap()["type"], "context");
        let note_block = &blocks[blocks.len() - 2];
        assert_eq!(note_block["text"]["text"], "… 73 more section(s) not shown");
    }

    #[test]
    fn text_style_is_plain_text_without_blocks() {
        let value = parsed(&note("hello\nworld", "{}"), Style::Text);
        assert_eq!(value["text"], "hello\nworld");
        assert!(value.get("blocks").is_none());
    }

    #[test]
    fn the_request_is_a_json_post_with_the_body_and_a_timeout() {
        mock::reset();
        mock::http_respond(http::Method::Post, URL, 200, "ok");
        assert!(deliver(URL, Style::Text, &note("Hello", "{}")).is_ok());
        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1, "exactly one attempt per call");
        assert_eq!(sent[0].method(), http::Method::Post);
        assert_eq!(sent[0].url(), URL);
        assert!(sent[0].headers().contains(&("content-type".to_string(), "application/json".to_string())));
        assert_eq!(sent[0].body_bytes(), request_body(&note("Hello", "{}"), Style::Text).as_bytes());
        assert_eq!(sent[0].timeout_value(), Some(HTTP_TIMEOUT));
    }

    #[test]
    fn deliver_maps_each_outcome_end_to_end_without_leaking_the_url() {
        type Answer<'a> = Result<(u16, Option<&'a str>), &'a str>;
        let cases: [(Answer<'_>, &str); 6] = [
            (Ok((200, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((500, None)), "transient"),
            (Ok((404, None)), "permanent"),
            (Err("connection refused: https://hooks.slack.test/services/T000/B000/SECRET-TOKEN"), "transient"),
            (Ok((302, Some("https://hooks.slack.test/services/T000/B000/SECRET-TOKEN"))), "permanent"),
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
            let outcome = deliver(URL, Style::Blocks, &note("Hello", "{}"));
            assert_eq!(shape(&outcome), want);
            if let Err(err) = &outcome {
                assert!(!format!("{err:?}").contains("SECRET-TOKEN"), "the url is in no error: {err:?}");
            }
            assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")), "nor in a log line");
        }
    }

    #[test]
    fn an_unusable_url_is_retried_and_never_echoed() {
        mock::reset();
        mock::http_fail(http::Method::Post, "", "relative URL without a base");
        assert_eq!(shape(&deliver("", Style::Blocks, &note("Hello", "{}"))), "transient");
    }
}
