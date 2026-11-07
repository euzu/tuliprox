//! The Discord side of the plugin: build the webhook request and say how the
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST {url}` with `Content-Type: application/json`. Two styles:
//!
//! * `embed` (default): one embed with the event title, the body, a colour per
//!   severity and the instance name as footer. `allowed_mentions` is empty, so
//!   event text can never ping `@everyone` or a role.
//! * `text`: `{"content": <body>}`, cut to Discord's 2000 characters.
//!
//! Cuts are by character (not bytes, not UTF-16 units) at Discord's limits:
//! 256 for a title, 4096 for an embed description, 2000 for `content`.
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
use std::time::Duration;
use tuliprox_plugin_sdk::prelude::*;

/// Discord rejects a webhook payload whose `content` exceeds this.
const CONTENT_LIMIT: usize = 2000;
/// Embed title limit.
const TITLE_LIMIT: usize = 256;
/// Embed description limit.
const DESCRIPTION_LIMIT: usize = 4096;
/// Embed footer text limit.
const FOOTER_LIMIT: usize = 2048;
/// Embed field name limit.
const FIELD_NAME_LIMIT: usize = 256;
/// Embed field value limit.
const FIELD_VALUE_LIMIT: usize = 1024;
/// Fields in one embed.
const MAX_FIELDS: usize = 25;
/// Characters in one embed: title, description, footer and every field name and value together.
const EMBED_TOTAL_LIMIT: usize = 6000;

/// Below the host's 30 s send timeout, so a silent webhook host surfaces as a
/// transient error from here rather than as a host-side timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

/// How a notification is shown in the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// One embed: title, the event's sections as fields (else the body),
    /// severity colour, instance footer and time.
    #[default]
    Embed,
    /// Plain `content`.
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
    instance: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    sections: Vec<Section>,
}

fn cut(text: &str, limit: usize) -> String { text.chars().take(limit).collect() }

/// `text` with the characters Discord reads as markdown escaped, so a playlist
/// called `my_list*` shows as written. `>` and `#` only count at the start of a
/// line, so `a -> b` stays readable. At most `limit` characters come out and an
/// escape is never cut from its character.
fn escape_cut(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut line_start = true;
    for ch in text.chars() {
        let needs_escape = match ch {
            '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']' => true,
            '>' | '#' => line_start,
            _ => false,
        };
        let width = if needs_escape { 2 } else { 1 };
        if count + width > limit {
            break;
        }
        if needs_escape {
            out.push('\\');
        }
        out.push(ch);
        count += width;
        line_start = ch == '\n' || (line_start && ch.is_whitespace());
    }
    out
}

fn chars(text: &str) -> usize { text.chars().count() }

const fn color(severity: Severity) -> u32 {
    match severity {
        Severity::Info => 0x0034_98DB,
        Severity::Warn => 0x00F1_C40F,
        Severity::Error => 0x00E7_4C3C,
    }
}

/// `text` escaped and cut to `limit` characters, ending in `…` when something was cut.
fn escape_ellipsis(text: &str, limit: usize) -> String {
    let full = escape_cut(text, usize::MAX);
    if chars(&full) <= limit {
        return full;
    }
    format!("{}…", escape_cut(text, limit.saturating_sub(1)))
}

/// The sections as embed fields, within Discord's limits: at most 25 fields, 1024
/// characters a value, and `budget` characters for all of them together. A
/// section with no line is skipped, a value that does not fit ends in `…`, and
/// what had to be left out is said in a last field.
fn section_fields(sections: &[Section], budget: usize) -> Vec<serde_json::Value> {
    /// Characters kept back for the closing note.
    const NOTE_RESERVE: usize = 64;
    let usable: Vec<(&str, String)> = sections
        .iter()
        .filter_map(|section| {
            let lines: Vec<&str> =
                section.lines.iter().map(|line| line.trim_end()).filter(|line| !line.trim().is_empty()).collect();
            (!lines.is_empty()).then(|| (section.heading.trim(), lines.join("\n")))
        })
        .collect();
    // One slot stays free for the note when there are more sections than slots.
    let slots = if usable.len() > MAX_FIELDS { MAX_FIELDS - 1 } else { MAX_FIELDS };
    let mut left = budget;
    let mut fields = Vec::new();
    for (heading, text) in usable.iter().take(slots) {
        let name = if heading.is_empty() { "\u{200b}".to_string() } else { escape_cut(heading, FIELD_NAME_LIMIT) };
        let room = left.saturating_sub(NOTE_RESERVE);
        if chars(&name) + 2 > room {
            break;
        }
        let value = escape_ellipsis(text, FIELD_VALUE_LIMIT.min(room - chars(&name)));
        left = left.saturating_sub(chars(&name) + chars(&value));
        fields.push(serde_json::json!({ "name": name, "value": value, "inline": false }));
    }
    let omitted = usable.len() - fields.len();
    if omitted > 0 {
        fields.push(serde_json::json!({
            "name": "…",
            "value": format!("{omitted} more section(s) not shown"),
            "inline": false,
        }));
    }
    fields
}

/// The request body for `msg`.
pub fn request_body(msg: &Notification, style: Style) -> String {
    match style {
        Style::Text => serde_json::json!({
            "content": escape_cut(&msg.body, CONTENT_LIMIT),
            "allowed_mentions": { "parse": [] },
        })
        .to_string(),
        Style::Embed => {
            let event = serde_json::from_str::<Event>(&msg.payload).unwrap_or_default();
            let footer = event
                .instance
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| "tuliprox".to_string(), |name| cut(name, FOOTER_LIMIT));
            let title = cut(&msg.title, TITLE_LIMIT);
            // With sections the detail is in the fields: the body is the same
            // facts as one text, so it is left out. Without them it is the detail.
            let has_sections =
                event.sections.iter().any(|section| section.lines.iter().any(|line| !line.trim().is_empty()));
            let description = if has_sections || msg.body.trim() == msg.title.trim() {
                String::new()
            } else {
                escape_cut(&msg.body, DESCRIPTION_LIMIT)
            };
            let budget = EMBED_TOTAL_LIMIT.saturating_sub(chars(&title) + chars(&description) + chars(&footer));
            let mut embed = serde_json::json!({
                "title": title,
                "description": description,
                "color": color(msg.severity),
                "footer": { "text": footer },
            });
            if has_sections {
                embed["fields"] = serde_json::Value::Array(section_fields(&event.sections, budget));
            }
            if let Some(time) = event.timestamp.and_then(notify::iso8601) {
                embed["timestamp"] = serde_json::Value::String(time);
            }
            serde_json::json!({ "embeds": [embed], "allowed_mentions": { "parse": [] } }).to_string()
        }
    }
}

/// Post `msg` to the webhook `url` and say how the attempt ended.
pub fn deliver(url: &str, style: Style, msg: &Notification) -> Result<(), DeliveryError> {
    let request = http::Request::post(url)
        .header("content-type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .body(request_body(msg, style).into_bytes());
    let outcome = match request.send() {
        Ok(response) => notify::delivery_for_status(response.status(), response.header("retry-after")),
        // The built-in channel retries these too (an unusable url is a request
        // builder error there), so a corrected url still gets the queue.
        Err(http::Error::InvalidRequest(_)) => {
            Err(DeliveryError::Transient("discord request failed: the webhook url is not usable".to_string()))
        }
        Err(_) => Err(DeliveryError::Transient("discord request failed".to_string())),
    };
    match &outcome {
        Ok(()) => debug!("notification delivered to Discord"),
        Err(DeliveryError::Permanent(reason)) => warn!("discord refused the notification: {reason}"),
        Err(DeliveryError::Transient(reason)) => warn!("discord will be retried: {reason}"),
        Err(DeliveryError::RetryAfter(secs)) => warn!("discord is rate limiting, retry in {secs}s"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const URL: &str = "https://discord.test/api/webhooks/42/SECRET-TOKEN";

    fn note(body: &str, payload: &str) -> Notification {
        Notification {
            title: "Title".to_string(),
            body: body.to_string(),
            severity: Severity::Info,
            payload: payload.to_string(),
        }
    }

    fn shape(outcome: &Result<(), DeliveryError>) -> String {
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(DeliveryError::Transient(_)) => "transient".to_string(),
            Err(DeliveryError::Permanent(_)) => "permanent".to_string(),
            Err(DeliveryError::RetryAfter(secs)) => format!("retry-after:{secs}"),
        }
    }

    fn parsed(msg: &Notification, style: Style) -> serde_json::Value {
        serde_json::from_str(&request_body(msg, style)).expect("the body is json")
    }

    #[test]
    fn an_embed_carries_title_body_severity_colour_and_instance_and_pings_nobody() {
        let mut msg = note("Done in 3s", r#"{"instance":"tv-box"}"#);
        msg.title = "Playlist updated".to_string();
        msg.severity = Severity::Warn;
        let value = parsed(&msg, Style::Embed);
        let embed = &value["embeds"][0];
        assert_eq!(embed["title"], "Playlist updated");
        assert_eq!(embed["description"], "Done in 3s");
        assert_eq!(embed["color"], 0x00F1_C40F);
        assert_eq!(embed["footer"]["text"], "tv-box");
        assert_eq!(value["allowed_mentions"], serde_json::json!({"parse": []}));
        assert!(value.get("content").is_none());
    }

    #[test]
    fn each_severity_has_its_own_colour_and_a_missing_instance_falls_back() {
        let colours: Vec<_> = [Severity::Info, Severity::Warn, Severity::Error]
            .into_iter()
            .map(|severity| {
                let mut msg = note("body", "not json");
                msg.severity = severity;
                let value = parsed(&msg, Style::Embed);
                assert_eq!(value["embeds"][0]["footer"]["text"], "tuliprox");
                value["embeds"][0]["color"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(colours.len(), 3);
        assert!(colours[0] != colours[1] && colours[1] != colours[2] && colours[0] != colours[2]);
    }

    #[test]
    fn a_body_that_only_repeats_the_title_is_not_shown_twice() {
        let mut msg = note("Same", "{}");
        msg.title = "Same".to_string();
        assert_eq!(parsed(&msg, Style::Embed)["embeds"][0]["description"], "");
    }

    #[test]
    fn text_style_is_plain_content_without_pings() {
        let value = parsed(&note("Hello \"world\"\nline 2", "{}"), Style::Text);
        assert_eq!(value["content"], "Hello \"world\"\nline 2");
        assert_eq!(value["allowed_mentions"], serde_json::json!({"parse": []}));
        assert!(value.get("embeds").is_none());
    }

    #[test]
    fn every_field_is_cut_at_discords_limit_by_character() {
        // 3-byte and 4-byte characters (2 UTF-16 units) count as one each.
        for unit in ["a", "漢", "😀"] {
            let mut msg = note(&unit.repeat(5000), &format!(r#"{{"instance":"{}"}}"#, unit.repeat(3000)));
            msg.title = unit.repeat(400);
            let embed = &parsed(&msg, Style::Embed)["embeds"][0];
            assert_eq!(embed["title"].as_str().unwrap().chars().count(), 256);
            assert_eq!(embed["description"].as_str().unwrap().chars().count(), 4096);
            assert_eq!(embed["footer"]["text"].as_str().unwrap().chars().count(), 2048);
            let text = parsed(&note(&unit.repeat(2500), "{}"), Style::Text);
            assert_eq!(text["content"].as_str().unwrap().chars().count(), 2000);
        }
        let exact = "é".repeat(2000);
        assert_eq!(parsed(&note(&exact, "{}"), Style::Text)["content"], exact.as_str());
    }

    fn sectioned(sections: &str) -> Notification {
        note("fallback body", &format!(r#"{{"instance":"tv-box","timestamp":784111777,"sections":{sections}}}"#))
    }

    #[test]
    fn sections_become_fields_and_replace_the_body_text() {
        let msg = sectioned(
            r#"[{"heading":"Sources","lines":["demo-in (M3u)","1/2 -> 1/2 groups/channels, 0 error(s), 3 secs"]},
                {"heading":"Targets","lines":["main: ok"]}]"#,
        );
        let embed = &parsed(&msg, Style::Embed)["embeds"][0];
        assert_eq!(embed["description"], "", "the sections carry the body's facts");
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["name"], "Sources");
        assert_eq!(fields[0]["value"], "demo-in (M3u)\n1/2 -> 1/2 groups/channels, 0 error(s), 3 secs");
        assert_eq!(fields[0]["inline"], false);
        assert_eq!(fields[1]["name"], "Targets");
        assert_eq!(embed["timestamp"], "1994-11-06T08:49:37Z");
        assert_eq!(embed["footer"]["text"], "tv-box");
    }

    #[test]
    fn an_event_without_sections_keeps_the_body_and_has_no_fields_key() {
        let embed = &parsed(&note("the body", r#"{"sections":[]}"#), Style::Embed)["embeds"][0];
        assert_eq!(embed["description"], "the body");
        assert!(embed.get("fields").is_none());
        assert!(embed.get("timestamp").is_none(), "no time in the event, none in the embed");
    }

    #[test]
    fn markdown_in_names_is_escaped_in_fields_the_body_and_plain_content() {
        let msg = sectioned(r#"[{"heading":"my_list*","lines":["a_b *c* ~d~ `e` |f| >g #h [i]"]}]"#);
        let field = &parsed(&msg, Style::Embed)["embeds"][0]["fields"][0];
        assert_eq!(field["name"], "my\\_list\\*");
        assert_eq!(field["value"], "a\\_b \\*c\\* \\~d\\~ \\`e\\` \\|f\\| >g #h \\[i\\]");
        assert_eq!(parsed(&note("x_y", "{}"), Style::Text)["content"], "x\\_y");
        assert_eq!(parsed(&note("x_y", "{}"), Style::Embed)["embeds"][0]["description"], "x\\_y");
    }

    #[test]
    fn a_quote_or_heading_marker_is_escaped_only_at_the_start_of_a_line() {
        assert_eq!(escape_cut("a -> b # c", 100), "a -> b # c");
        assert_eq!(
            escape_cut("> quote\n# head\n  > indented\nx > y", 100),
            "\\> quote\n\\# head\n  \\> indented\nx > y"
        );
    }

    #[test]
    fn empty_sections_and_blank_headings_are_handled() {
        let msg = sectioned(
            r#"[{"heading":"Empty","lines":[]},{"heading":"Blank","lines":["  ",""]},{"heading":" ","lines":["kept"]}]"#,
        );
        let fields = parsed(&msg, Style::Embed)["embeds"][0]["fields"].as_array().unwrap().clone();
        assert_eq!(fields.len(), 1, "a section with no text is not a field");
        assert_eq!(fields[0]["name"], "\u{200b}", "Discord needs a non-empty name");
        assert_eq!(fields[0]["value"], "kept");
        // Only empty sections: it is as if the event had none, so the body shows.
        let embed = &parsed(&sectioned(r#"[{"heading":"Empty","lines":[]}]"#), Style::Embed)["embeds"][0];
        assert_eq!(embed["description"], "fallback body");
        assert!(embed.get("fields").is_none());
    }

    #[test]
    fn a_long_value_is_cut_with_an_ellipsis_and_the_embed_stays_inside_discords_limits() {
        let long = "x".repeat(3000);
        let msg = sectioned(&format!(r#"[{{"heading":"Big","lines":["{long}"]}}]"#));
        let field = &parsed(&msg, Style::Embed)["embeds"][0]["fields"][0];
        let value = field["value"].as_str().unwrap();
        assert_eq!(value.chars().count(), 1024);
        assert!(value.ends_with('…'));

        // 60 sections of 200 characters: far past 25 fields and 6000 characters.
        let many: Vec<String> =
            (0..60).map(|n| format!(r#"{{"heading":"Section {n}","lines":["{}"]}}"#, "y".repeat(200))).collect();
        let embed = parsed(&sectioned(&format!("[{}]", many.join(","))), Style::Embed)["embeds"][0].clone();
        let fields = embed["fields"].as_array().unwrap();
        assert!(fields.len() <= 25, "{} fields", fields.len());
        let total: usize =
            ["title", "description"].iter().map(|key| embed[key].as_str().unwrap().chars().count()).sum::<usize>()
                + embed["footer"]["text"].as_str().unwrap().chars().count()
                + fields
                    .iter()
                    .map(|f| f["name"].as_str().unwrap().chars().count() + f["value"].as_str().unwrap().chars().count())
                    .sum::<usize>();
        assert!(total <= 6000, "{total} characters");
        let last = fields.last().unwrap();
        assert_eq!(last["name"], "…");
        assert!(last["value"].as_str().unwrap().contains("more section(s) not shown"));
    }

    #[test]
    fn the_request_is_a_json_post_with_the_body_and_a_timeout() {
        mock::reset();
        mock::http_respond(http::Method::Post, URL, 204, "");
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
            (Ok((204, None)), "ok"),
            (Ok((429, Some("3"))), "retry-after:3"),
            (Ok((500, None)), "transient"),
            (Ok((404, None)), "permanent"),
            (Err("connection refused: https://discord.test/api/webhooks/42/SECRET-TOKEN"), "transient"),
            (Ok((302, Some("https://discord.test/api/webhooks/42/SECRET-TOKEN"))), "permanent"),
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
            let outcome = deliver(URL, Style::Embed, &note("Hello", "{}"));
            assert_eq!(shape(&outcome), want);
            if let Err(err) = &outcome {
                assert!(!format!("{err:?}").contains("SECRET-TOKEN"), "the url is in no error: {err:?}");
            }
            assert!(mock::logs().iter().all(|line| !line.message.contains("SECRET-TOKEN")), "nor in a log line");
        }
    }

    #[test]
    fn an_unusable_url_is_retried_like_the_built_in_channel_and_never_echoed() {
        mock::reset();
        mock::http_fail(http::Method::Post, "", "relative URL without a base");
        let outcome = deliver("", Style::Embed, &note("Hello", "{}"));
        assert_eq!(shape(&outcome), "transient");
    }
}
