//! The Telegram side of the plugin: build the Bot API requests and say how each
//! answer ends the attempt for the host's outbox.
//!
//! # Request
//!
//! `POST https://api.telegram.org/bot<token>/sendMessage` with a JSON body: `chat_id`,
//! `text`, `parse_mode` (`HTML` in the `html` style) and, for a chat entry written
//! `<chat>:<thread>`, `message_thread_id` (a topic of a forum group). A chat is a
//! number (`123456`, `-1001234567890`) or a channel name (`@tuliprox_news`).
//!
//! # Layout
//!
//! The message is the title (bold in `html`, prefixed `⚠️` for a warning and `🚨` for an
//! error), then the event's sections as a bold heading with its lines, else the body.
//! `html` only escapes `& < >`, so nothing in an event can become markup. Telegram
//! limits a message to 4096 characters after parsing; the message is split into
//! parts of whole lines of at most 4000 UTF-16 units (Telegram counts those),
//! and at most [`MAX_CHUNKS`] parts are sent, the last one ending in `…`.
//! If Telegram still cannot parse an HTML part it is sent again as plain text.
//!
//! # Retry contract
//!
//! A *unit* is one part for one chat. Units already delivered are remembered
//! ([`crate::state`]) so a retry only sends what is missing:
//!
//! * a transient failure keeps the retry error and moves on to the next chat;
//! * a permanent failure (the bot was removed from the chat, the chat does not
//!   exist) ends that chat only; it is not retried;
//! * the attempt stops once it has used [`BUDGET_SECS`], and the rest is retried;
//! * when every unit is settled the result is `Ok` if anything reached a chat,
//!   else `Permanent`.
//!
//! Status mapping is the SDK's (`tuliprox_plugin_sdk::notify`), with Telegram's own
//! `retry_after` read from a `429` body.
//!
//! # The bot token is a credential
//!
//! It is part of the URL, so no error string, log line or reason is ever built
//! from a URL or from what a transport error says: they are fixed phrases or a
//! status plus Telegram's own `description`. A token that cannot sit in a URL path
//! is refused.

use crate::state::{fingerprint, State};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeSet, time::Duration};
use tuliprox_plugin_sdk::prelude::*;

const API: &str = "https://api.telegram.org";
/// Telegram allows 4096 characters; stay clear of it.
const CHUNK_LIMIT: usize = 4000;
const MAX_CHUNKS: usize = 8;
/// Below the host's 30 s limit for a plugin call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one attempt may keep sending before it leaves the rest to a retry.
pub const BUDGET_SECS: i64 = 20;
const MAX_RETRY_AFTER_SECS: u64 = 86_400;
const DESCRIPTION_LIMIT: usize = 160;

/// How a notification is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Bold title and section headings.
    #[default]
    Html,
    /// Plain text.
    Text,
}

/// What `deliver` needs from the settings.
pub struct Target<'a> {
    pub token: &'a str,
    pub chat_ids: &'a [String],
    pub style: Style,
}

/// One destination: a chat, and a topic of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub thread: Option<u64>,
}

impl Chat {
    /// The id of this destination in the delivery memory.
    fn key(&self) -> String {
        match self.thread {
            Some(thread) => format!("{}:{thread}", self.id),
            None => self.id.clone(),
        }
    }
}

/// `-1001234567890`, `123456`, `@channel`, each optionally followed by `:<topic>`.
pub fn parse_chat(raw: &str) -> Option<Chat> {
    let raw = raw.trim();
    let (id, thread) = match raw.split_once(':') {
        Some((id, thread)) => (id, Some(thread.parse::<u64>().ok().filter(|thread| *thread > 0)?)),
        None => (raw, None),
    };
    let numeric = {
        let digits = id.strip_prefix('-').unwrap_or(id);
        (1..=20).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit())
    };
    let channel = id.strip_prefix('@').is_some_and(|name| {
        (5..=32).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    });
    (numeric || channel).then(|| Chat { id: id.to_string(), thread })
}

/// A bot token is `<bot id>:<secret>`. Anything that could leave the URL path
/// (a slash, `?`, whitespace) is refused, so the token cannot rewrite the request.
pub fn token_ok(token: &str) -> bool {
    token.len() <= 128
        && token.contains(':')
        && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
}

/// One line of a message, with whether it is a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    raw: String,
    bold: bool,
}

impl Line {
    fn plain(raw: impl Into<String>) -> Self { Self { raw: raw.into(), bold: false } }

    fn bold(raw: impl Into<String>) -> Self { Self { raw: raw.into(), bold: true } }

    fn html(&self) -> String {
        let escaped = escape_html(&self.raw);
        if self.bold && !escaped.is_empty() {
            format!("<b>{escaped}</b>")
        } else {
            escaped
        }
    }

    fn units(&self) -> usize { utf16_len(&self.raw) }

    fn is_blank(&self) -> bool { self.raw.trim().is_empty() }
}

fn utf16_len(text: &str) -> usize { text.encode_utf16().count() }

/// Telegram's HTML mode reads these three and nothing else needs escaping.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// The lines of the message for `msg`.
pub fn lines(msg: &Notification) -> Vec<Line> {
    let marker = match msg.severity {
        Severity::Info => "",
        Severity::Warn => "⚠️ ",
        Severity::Error => "🚨 ",
    };
    let title = msg.title.trim();
    let title = if title.is_empty() { "tuliprox" } else { title };
    let mut out = vec![Line::bold(format!("{marker}{title}"))];
    let mut first = true;
    for section in notify::sections_of(&msg.payload) {
        let kept: Vec<&str> =
            section.lines.iter().map(|line| line.trim_end()).filter(|line| !line.trim().is_empty()).collect();
        if kept.is_empty() {
            continue;
        }
        first = false;
        out.push(Line::plain(""));
        if !section.heading.trim().is_empty() {
            out.push(Line::bold(section.heading.trim()));
        }
        out.extend(kept.into_iter().map(Line::plain));
    }
    if first && !msg.body.trim().is_empty() && msg.body.trim() != title {
        out.push(Line::plain(""));
        out.extend(msg.body.trim().lines().map(|line| Line::plain(line.trim_end())));
    }
    out
}

/// One message to send: its HTML and plain-text forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    html: String,
    plain: String,
}

/// `raw` cut into pieces of at most `CHUNK_LIMIT` UTF-16 units, at character boundaries.
fn split_long(line: &Line) -> Vec<Line> {
    if line.units() <= CHUNK_LIMIT {
        return vec![line.clone()];
    }
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut units = 0;
    for ch in line.raw.chars() {
        let width = ch.len_utf16();
        if units + width > CHUNK_LIMIT {
            pieces.push(Line { raw: std::mem::take(&mut piece), bold: line.bold });
            units = 0;
        }
        piece.push(ch);
        units += width;
    }
    if !piece.is_empty() {
        pieces.push(Line { raw: piece, bold: line.bold });
    }
    pieces
}

fn flush(batch: &mut Vec<Line>, chunks: &mut Vec<Chunk>) {
    let start = batch.iter().position(|line| !line.is_blank());
    let end = batch.iter().rposition(|line| !line.is_blank());
    if let (Some(start), Some(end)) = (start, end) {
        let kept = &batch[start..=end];
        chunks.push(Chunk {
            html: kept.iter().map(Line::html).collect::<Vec<_>>().join("\n"),
            plain: kept.iter().map(|line| line.raw.as_str()).collect::<Vec<_>>().join("\n"),
        });
    }
    batch.clear();
}

/// The lines grouped into messages of whole lines, at most [`MAX_CHUNKS`] of them.
pub fn chunks(lines: &[Line]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut batch: Vec<Line> = Vec::new();
    let mut used = 0;
    for line in lines.iter().flat_map(split_long) {
        let cost = line.units() + usize::from(!batch.is_empty());
        if used + cost > CHUNK_LIMIT {
            flush(&mut batch, &mut chunks);
            used = 0;
        }
        used += line.units() + usize::from(!batch.is_empty());
        batch.push(line);
    }
    flush(&mut batch, &mut chunks);
    if chunks.len() > MAX_CHUNKS {
        chunks.truncate(MAX_CHUNKS);
        if let Some(last) = chunks.last_mut() {
            last.html.push_str("\n…");
            last.plain.push_str("\n…");
        }
    }
    chunks
}

/// The JSON body of one `sendMessage`.
pub fn request_body(chat: &Chat, chunk: &Chunk, html: bool) -> String {
    let mut body = json!({ "chat_id": chat.id, "text": if html { &chunk.html } else { &chunk.plain } });
    if html {
        body["parse_mode"] = Value::from("HTML");
    }
    if let Some(thread) = chat.thread {
        body["message_thread_id"] = Value::from(thread);
    }
    body.to_string()
}

/// What Telegram answered, as far as delivery cares.
#[derive(Debug)]
enum Outcome {
    Sent,
    /// Telegram could not read the HTML of the message.
    ParseError,
    Failed(DeliveryError),
}

#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<ReplyParameters>,
}

#[derive(Deserialize, Default)]
struct ReplyParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

/// Telegram's explanation, safe for a log line: one line, cut short.
fn describe(description: &str) -> String {
    description.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).take(DESCRIPTION_LIMIT).collect()
}

fn classify(response: &http::Response, now: i64) -> Outcome {
    let status = response.status();
    if response.is_success() {
        return Outcome::Sent;
    }
    let reply: Reply = response.json().unwrap_or_default();
    let description = reply.description.as_deref().map(describe).filter(|text| !text.trim().is_empty());
    if status == 400
        && description.as_deref().is_some_and(|text| text.to_ascii_lowercase().contains("can't parse entities"))
    {
        return Outcome::ParseError;
    }
    if status == 429 {
        let hinted = reply.parameters.and_then(|parameters| parameters.retry_after);
        if let Some(secs) = hinted.map(|secs| secs.clamp(1, MAX_RETRY_AFTER_SECS)) {
            return Outcome::Failed(DeliveryError::RetryAfter(u32::try_from(secs).unwrap_or(u32::MAX)));
        }
    }
    let Err(error) = notify::delivery_for_status_at(status, response.header("retry-after"), now) else {
        return Outcome::Sent;
    };
    let with_description = |reason: String| match &description {
        Some(text) => format!("{reason}: {text}"),
        None => reason,
    };
    Outcome::Failed(match error {
        DeliveryError::Permanent(reason) => DeliveryError::Permanent(with_description(reason)),
        DeliveryError::Transient(reason) => DeliveryError::Transient(with_description(reason)),
        retry @ DeliveryError::RetryAfter(_) => retry,
    })
}

fn post(token: &str, body: String) -> Outcome {
    let request = http::Request::post(format!("{API}/bot{token}/sendMessage"))
        .header("content-type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .body(body.into_bytes());
    match request.send() {
        Ok(response) => classify(&response, clock::now_unix()),
        Err(http::Error::InvalidRequest(_)) => {
            Outcome::Failed(DeliveryError::Permanent("telegram request failed: the request is not usable".to_string()))
        }
        Err(_) => Outcome::Failed(DeliveryError::Transient("telegram request failed".to_string())),
    }
}

/// Send one part to one chat; HTML Telegram cannot read is sent again as plain text.
fn send_chunk(target: &Target<'_>, chat: &Chat, chunk: &Chunk) -> Result<(), DeliveryError> {
    let mut html = target.style == Style::Html;
    loop {
        match post(target.token, request_body(chat, chunk, html)) {
            Outcome::Sent => return Ok(()),
            Outcome::ParseError if html => {
                debug!("telegram could not parse the HTML of a part; sending it as plain text");
                html = false;
            }
            Outcome::ParseError => {
                return Err(DeliveryError::Permanent("telegram rejected the message text".to_string()));
            }
            Outcome::Failed(error) => return Err(error),
        }
    }
}

/// Combine two retryable failures: a stated delay beats none, the longer delay wins.
fn merge(current: Option<DeliveryError>, next: DeliveryError) -> DeliveryError {
    match (current, next) {
        (Some(DeliveryError::RetryAfter(a)), DeliveryError::RetryAfter(b)) => DeliveryError::RetryAfter(a.max(b)),
        (Some(kept @ DeliveryError::RetryAfter(_)), _) => kept,
        (_, next) => next,
    }
}

/// The chats of `chat_ids`, or why they are unusable. Entries are named by position, never echoed.
fn chats_of(chat_ids: &[String]) -> Result<Vec<Chat>, DeliveryError> {
    if chat_ids.is_empty() {
        return Err(DeliveryError::Permanent(
            "telegram-notifier is not configured: `chat_ids` needs a chat".to_string(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut chats = Vec::new();
    for (position, raw) in chat_ids.iter().enumerate() {
        let chat = parse_chat(raw).ok_or_else(|| {
            DeliveryError::Permanent(format!("`chat_ids` entry {} is not a chat id or @channel", position + 1))
        })?;
        if seen.insert(chat.key()) {
            chats.push(chat);
        }
    }
    Ok(chats)
}

/// [`deliver_with_budget`] with the production budget.
pub fn deliver(target: &Target<'_>, msg: &Notification) -> Result<(), DeliveryError> {
    deliver_with_budget(target, msg, BUDGET_SECS)
}

/// Send `msg` to every chat and say how the attempt ended.
pub fn deliver_with_budget(target: &Target<'_>, msg: &Notification, budget_secs: i64) -> Result<(), DeliveryError> {
    if target.token.trim().is_empty() || !token_ok(target.token) {
        return Err(DeliveryError::Permanent(
            "telegram-notifier is not configured: `bot_token` is missing or not a Telegram bot token".to_string(),
        ));
    }
    let chats = chats_of(target.chat_ids)?;
    let parts = chunks(&lines(msg));
    let fingerprint = fingerprint(msg);
    let mut state = State::load(&fingerprint);
    let deadline = clock::now_unix().saturating_add(budget_secs);
    let mut retry: Option<DeliveryError> = None;
    let mut refused: Option<String> = None;
    let mut refused_chats = 0_usize;

    'chats: for (position, chat) in chats.iter().enumerate() {
        for (part, chunk) in parts.iter().enumerate() {
            let unit = format!("{}#{part}", chat.key());
            if state.settled(&unit) {
                continue;
            }
            if clock::now_unix() > deadline {
                retry = Some(merge(
                    retry,
                    DeliveryError::Transient("telegram delivery used its time budget; the rest is retried".to_string()),
                ));
                break 'chats;
            }
            match send_chunk(target, chat, chunk) {
                Ok(()) => {
                    state.mark_done(&unit);
                    state.save(&fingerprint);
                }
                Err(DeliveryError::Permanent(reason)) => {
                    warn!("telegram refused chat {}: {reason}", position + 1);
                    for rest in part..parts.len() {
                        state.mark_dead(&format!("{}#{rest}", chat.key()));
                    }
                    state.save(&fingerprint);
                    refused_chats += 1;
                    refused = Some(reason);
                    break;
                }
                Err(error) => {
                    warn!("telegram will retry chat {}", position + 1);
                    retry = Some(merge(retry, error));
                    break;
                }
            }
        }
    }

    if let Some(error) = retry {
        return Err(error);
    }
    let delivered = state.any_done();
    State::release(&fingerprint);
    if delivered {
        if refused_chats > 0 {
            warn!("telegram refused {refused_chats} chat(s); the others have the notification");
        }
        Ok(())
    } else {
        Err(DeliveryError::Permanent(refused.unwrap_or_else(|| "telegram had no chat to deliver to".to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    const TOKEN: &str = "123456:abc-DEF_1";
    const URL: &str = "https://api.telegram.org/bot123456:abc-DEF_1/sendMessage";

    fn note(title: &str, body: &str, severity: Severity) -> Notification {
        Notification { title: title.to_string(), body: body.to_string(), severity, payload: "{}".to_string() }
    }

    fn target<'a>(chats: &'a [String], style: Style) -> Target<'a> { Target { token: TOKEN, chat_ids: chats, style } }

    fn chat_list(ids: &[&str]) -> Vec<String> { ids.iter().map(ToString::to_string).collect() }

    fn answer(status: u16, body: &str) { mock::http_respond(http::Method::Post, URL, status, body); }

    fn ok() { answer(200, r#"{"ok":true}"#); }

    fn setup() {
        mock::reset();
        mock::set_now(10_000);
    }

    fn sent_bodies() -> Vec<Value> {
        mock::http_requests().iter().map(|request| serde_json::from_slice(request.body_bytes()).unwrap()).collect()
    }

    fn texts(chunks: &[Chunk]) -> Vec<&str> { chunks.iter().map(|chunk| chunk.plain.as_str()).collect() }

    #[test]
    fn chats_are_numbers_or_channels_with_an_optional_topic() {
        let chat = |id: &str, thread| Some(Chat { id: id.to_string(), thread });
        assert_eq!(parse_chat("123456"), chat("123456", None));
        assert_eq!(parse_chat(" -1001234567890 "), chat("-1001234567890", None));
        assert_eq!(parse_chat("-1001234567890:42"), chat("-1001234567890", Some(42)));
        assert_eq!(parse_chat("@tuliprox_news"), chat("@tuliprox_news", None));
        assert_eq!(parse_chat("@tuliprox_news:7"), chat("@tuliprox_news", Some(7)));
        for bad in ["", "-", "abc", "12a", "@abc", "@bad-name!", "123:", "123:x", "123:0", "123:-1", "1:2:3", "--5"] {
            assert_eq!(parse_chat(bad), None, "{bad:?}");
        }
        assert_eq!(parse_chat(&"1".repeat(21)), None);
    }

    #[test]
    fn a_token_that_could_rewrite_the_url_is_refused() {
        assert!(token_ok("123456:ABC-def_123"));
        for bad in ["", "no-colon", "1:a/b", "1:a?x=1", "1:a b", "1:a#b", "1:a@b", &format!("1:{}", "a".repeat(200))] {
            assert!(!token_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_message_is_a_marked_title_then_sections_else_the_body() {
        let plain = |msg: &Notification| lines(msg).iter().map(|line| line.raw.clone()).collect::<Vec<_>>();
        assert_eq!(plain(&note("Update", "all done\nsecond", Severity::Info)), ["Update", "", "all done", "second"]);
        assert_eq!(plain(&note("Update", "Update", Severity::Warn)), ["⚠️ Update"]);
        assert_eq!(plain(&note("Down", "", Severity::Error)), ["🚨 Down"]);
        assert_eq!(plain(&note("  ", "", Severity::Info)), ["tuliprox"]);
        let mut msg = note("Stats", "ignored body", Severity::Info);
        msg.payload = r#"{"sections":[{"heading":"Groups","lines":["a","  ","b"]},{"heading":"","lines":["only"]},{"heading":"Empty","lines":[" "]}]}"#.to_string();
        assert_eq!(plain(&msg), ["Stats", "", "Groups", "a", "b", "", "only"]);
        let first = lines(&msg);
        assert!(first[0].bold && first[2].bold && !first[3].bold);
    }

    #[test]
    fn html_escapes_only_what_telegram_reads_and_bolds_headings() {
        let msg = note("A <b> & C", "x < y && z > w", Severity::Info);
        let parts = chunks(&lines(&msg));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].html, "<b>A &lt;b&gt; &amp; C</b>\n\nx &lt; y &amp;&amp; z &gt; w");
        assert_eq!(parts[0].plain, "A <b> & C\n\nx < y && z > w");
    }

    #[test]
    fn a_long_message_is_split_between_lines_by_utf16_units() {
        // Each emoji is 2 UTF-16 units: 999 of them are 1998 units, so two lines fit a part and three do not.
        let line = "😀".repeat(999);
        let body = format!("{line}\n{line}\n{line}\n{line}\n{line}");
        let parts = chunks(&lines(&note("T", &body, Severity::Info)));
        assert_eq!(parts.len(), 3, "{:?}", parts.iter().map(|part| utf16_len(&part.plain)).collect::<Vec<_>>());
        for part in &parts {
            assert!(utf16_len(&part.plain) <= CHUNK_LIMIT);
            assert!(!part.plain.starts_with('\n') && !part.plain.ends_with('\n'));
        }
        assert_eq!(texts(&parts)[0], format!("T\n\n{line}\n{line}"));
        assert_eq!(texts(&parts)[2], line);
    }

    #[test]
    fn a_line_longer_than_a_part_is_cut_at_character_boundaries() {
        let long = "é".repeat(9000);
        let parts = chunks(&lines(&note("T", &long, Severity::Info)));
        assert!(parts.len() >= 3);
        for part in &parts {
            assert!(utf16_len(&part.plain) <= CHUNK_LIMIT);
        }
        let all: String = parts.iter().flat_map(|part| part.plain.chars()).filter(|ch| *ch == 'é').collect();
        assert_eq!(all.chars().count(), 9000, "no character is lost or split");
    }

    #[test]
    fn at_most_eight_parts_are_sent_and_the_last_is_cut_visibly() {
        let body = (0..40).map(|n| format!("{n}{}", "x".repeat(3500))).collect::<Vec<_>>().join("\n");
        let parts = chunks(&lines(&note("T", &body, Severity::Info)));
        assert_eq!(parts.len(), MAX_CHUNKS);
        assert!(parts[MAX_CHUNKS - 1].plain.ends_with("\n…") && parts[MAX_CHUNKS - 1].html.ends_with("\n…"));
        assert!(!parts[0].plain.ends_with('…'));
        assert!(parts.iter().all(|part| utf16_len(&part.plain) <= 4096));
    }

    #[test]
    fn request_bodies_carry_html_mode_only_in_html_and_an_integer_topic() {
        let chunk = Chunk { html: "<b>t</b>".to_string(), plain: "t".to_string() };
        let chat = Chat { id: "-100".to_string(), thread: Some(5) };
        let html: Value = serde_json::from_str(&request_body(&chat, &chunk, true)).unwrap();
        assert_eq!(html, json!({"chat_id": "-100", "text": "<b>t</b>", "parse_mode": "HTML", "message_thread_id": 5}));
        let text: Value =
            serde_json::from_str(&request_body(&Chat { id: "@news_x".to_string(), thread: None }, &chunk, false))
                .unwrap();
        assert_eq!(text, json!({"chat_id": "@news_x", "text": "t"}));
    }

    #[test]
    fn answers_are_classified_for_the_outbox() {
        let at = |status: u16, headers: Vec<(&str, &str)>, body: &str| {
            let headers = headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            classify(&http::Response::new(status, headers, body.as_bytes().to_vec()), 1_000)
        };
        assert!(matches!(at(200, vec![], r#"{"ok":true}"#), Outcome::Sent));
        assert!(matches!(
            at(400, vec![], r#"{"description":"Bad Request: can't parse entities: Unsupported start tag"}"#),
            Outcome::ParseError
        ));
        assert!(matches!(
            at(429, vec![], r#"{"description":"Too Many Requests","parameters":{"retry_after":5}}"#),
            Outcome::Failed(DeliveryError::RetryAfter(5))
        ));
        assert!(matches!(
            at(429, vec![], r#"{"parameters":{"retry_after":0}}"#),
            Outcome::Failed(DeliveryError::RetryAfter(1))
        ));
        assert!(matches!(
            at(429, vec![], r#"{"parameters":{"retry_after":99999999}}"#),
            Outcome::Failed(DeliveryError::RetryAfter(86_400))
        ));
        assert!(matches!(at(429, vec![("Retry-After", "12")], "{}"), Outcome::Failed(DeliveryError::RetryAfter(12))));
        assert!(matches!(at(429, vec![], "not json"), Outcome::Failed(DeliveryError::Transient(_))));
        match at(403, vec![], r#"{"description":"Forbidden: bot was blocked by the user"}"#) {
            Outcome::Failed(DeliveryError::Permanent(reason)) => {
                assert_eq!(reason, "status 403: Forbidden: bot was blocked by the user");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            at(400, vec![], r#"{"description":"Bad Request: chat not found"}"#),
            Outcome::Failed(DeliveryError::Permanent(_))
        ));
        assert!(
            matches!(at(502, vec![], "<html>"), Outcome::Failed(DeliveryError::Transient(reason)) if reason == "status 502")
        );
    }

    #[test]
    fn a_description_is_one_short_line() {
        let described = describe(&format!("a\nb\t{}", "x".repeat(500)));
        assert!(!described.contains('\n') && !described.contains('\t'));
        assert_eq!(described.chars().count(), DESCRIPTION_LIMIT);
    }

    #[test]
    fn a_message_goes_to_every_chat_and_nothing_is_remembered_afterwards() {
        setup();
        ok();
        ok();
        let chats = chat_list(&["-100", "-200:5", "-100"]);
        assert!(deliver(&target(&chats, Style::Html), &note("Update", "done", Severity::Info)).is_ok());
        let bodies = sent_bodies();
        assert_eq!(bodies.len(), 2, "a repeated chat is sent once");
        assert_eq!(bodies[0]["chat_id"], "-100");
        assert_eq!(bodies[0]["parse_mode"], "HTML");
        assert_eq!(bodies[1]["chat_id"], "-200");
        assert_eq!(bodies[1]["message_thread_id"], 5);
        assert!(mock::kv_snapshot().is_empty());
    }

    #[test]
    fn the_text_style_sends_plain_text() {
        setup();
        ok();
        let chats = chat_list(&["1"]);
        assert!(deliver(&target(&chats, Style::Text), &note("A & B", "x", Severity::Info)).is_ok());
        let bodies = sent_bodies();
        assert_eq!(bodies[0]["text"], "A & B\n\nx");
        assert!(bodies[0].get("parse_mode").is_none());
    }

    #[test]
    fn a_retry_sends_only_what_is_missing() {
        setup();
        answer(200, r#"{"ok":true}"#);
        answer(502, "{}");
        let chats = chat_list(&["-100", "-200"]);
        let msg = note("Update", "done", Severity::Info);
        assert!(matches!(deliver(&target(&chats, Style::Html), &msg), Err(DeliveryError::Transient(_))));
        assert!(!mock::kv_snapshot().is_empty(), "the first chat is remembered");
        ok();
        assert!(deliver(&target(&chats, Style::Html), &msg).is_ok());
        let bodies = sent_bodies();
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[2]["chat_id"], "-200", "only the chat that failed is sent again");
        assert!(mock::kv_snapshot().is_empty());
    }

    #[test]
    fn the_parts_of_a_long_message_resume_where_they_stopped() {
        setup();
        let body = (0..6).map(|n| format!("{n}{}", "x".repeat(1500))).collect::<Vec<_>>().join("\n");
        let msg = note("Big", &body, Severity::Info);
        let chats = chat_list(&["-100"]);
        ok();
        answer(429, r#"{"parameters":{"retry_after":7}}"#);
        assert!(matches!(deliver(&target(&chats, Style::Html), &msg), Err(DeliveryError::RetryAfter(7))));
        ok();
        ok();
        assert!(deliver(&target(&chats, Style::Html), &msg).is_ok());
        let bodies = sent_bodies();
        assert_eq!(bodies.len(), 4, "3 parts, one of them twice");
        assert_eq!(bodies[1]["text"], bodies[2]["text"], "the failed part is the one sent again");
        assert_ne!(bodies[0]["text"], bodies[2]["text"], "the part that was delivered is not");
        assert_ne!(bodies[2]["text"], bodies[3]["text"]);
    }

    #[test]
    fn a_refused_chat_does_not_fail_the_chats_that_have_it() {
        setup();
        answer(403, r#"{"description":"Forbidden: bot was kicked from the group chat"}"#);
        ok();
        let chats = chat_list(&["-100", "-200"]);
        assert!(deliver(&target(&chats, Style::Html), &note("T", "b", Severity::Info)).is_ok());
        assert_eq!(sent_bodies().len(), 2);
        assert!(mock::kv_snapshot().is_empty());
    }

    #[test]
    fn when_every_chat_refuses_the_notification_is_permanent() {
        setup();
        answer(403, r#"{"description":"Forbidden: bot was blocked by the user"}"#);
        answer(400, r#"{"description":"Bad Request: chat not found"}"#);
        let chats = chat_list(&["-100", "-200"]);
        match deliver(&target(&chats, Style::Html), &note("T", "b", Severity::Info)) {
            Err(DeliveryError::Permanent(reason)) => assert!(reason.contains("chat not found"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(mock::kv_snapshot().is_empty());
    }

    #[test]
    fn html_telegram_cannot_parse_is_sent_again_as_plain_text() {
        setup();
        answer(400, r#"{"description":"Bad Request: can't parse entities: Unsupported start tag"}"#);
        ok();
        let chats = chat_list(&["-100"]);
        assert!(deliver(&target(&chats, Style::Html), &note("T <x>", "b", Severity::Info)).is_ok());
        let bodies = sent_bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["parse_mode"], "HTML");
        assert!(bodies[1].get("parse_mode").is_none());
        assert_eq!(bodies[1]["text"], "T <x>\n\nb");
    }

    #[test]
    fn text_telegram_cannot_parse_is_permanent() {
        setup();
        answer(400, r#"{"description":"Bad Request: can't parse entities"}"#);
        let chats = chat_list(&["-100"]);
        assert!(matches!(
            deliver(&target(&chats, Style::Text), &note("T", "b", Severity::Info)),
            Err(DeliveryError::Permanent(_))
        ));
    }

    #[test]
    fn rate_limits_beat_plain_failures_and_the_longer_delay_wins() {
        setup();
        answer(502, "{}");
        answer(429, r#"{"parameters":{"retry_after":9}}"#);
        answer(429, r#"{"parameters":{"retry_after":3}}"#);
        let chats = chat_list(&["1", "2", "3"]);
        assert!(matches!(
            deliver(&target(&chats, Style::Html), &note("T", "b", Severity::Info)),
            Err(DeliveryError::RetryAfter(9))
        ));
    }

    #[test]
    fn an_exhausted_budget_sends_nothing_and_asks_for_a_retry() {
        setup();
        let chats = chat_list(&["-100"]);
        let result = deliver_with_budget(&target(&chats, Style::Html), &note("T", "b", Severity::Info), -1);
        assert!(matches!(result, Err(DeliveryError::Transient(_))), "{result:?}");
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn unusable_settings_are_permanent_and_never_echo_the_entry() {
        setup();
        let msg = note("T", "b", Severity::Info);
        let chats = chat_list(&["-100"]);
        let no_token = Target { token: "", chat_ids: &chats, style: Style::Html };
        assert!(matches!(deliver(&no_token, &msg), Err(DeliveryError::Permanent(_))));
        let bad_token = Target { token: "not a token", chat_ids: &chats, style: Style::Html };
        match deliver(&bad_token, &msg) {
            Err(DeliveryError::Permanent(reason)) => assert!(!reason.contains("not a token")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(deliver(&target(&[], Style::Html), &msg), Err(DeliveryError::Permanent(_))));
        match deliver(&target(&chat_list(&["-100", "secret-chat"]), Style::Html), &msg) {
            Err(DeliveryError::Permanent(reason)) => {
                assert!(reason.contains("entry 2") && !reason.contains("secret-chat"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(mock::http_requests().is_empty());
    }

    #[test]
    fn the_token_is_in_the_url_but_never_in_an_error_or_a_log() {
        setup();
        mock::http_fail(http::Method::Post, URL, &format!("connect to {URL} failed"));
        let chats = chat_list(&["-100"]);
        let err = deliver(&target(&chats, Style::Html), &note("T", "b", Severity::Info)).unwrap_err();
        assert!(!format!("{err:?}").contains(TOKEN));
        assert!(mock::http_requests()[0].url().contains(TOKEN));
        assert!(mock::logs().iter().all(|line| !line.message.contains(TOKEN)));
    }
}
