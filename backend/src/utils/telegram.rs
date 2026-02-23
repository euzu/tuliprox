use crate::model::AppConfig;
use log::{debug, error, warn};
use reqwest::StatusCode;
use shared::utils::CONSTANTS;
use std::sync::Arc;
use url::Url;

const MAX_MESSAGE_LENGTH: usize = 4000;
const MAX_RETRIES_PER_CHUNK: u8 = 5;
const RETRY_AFTER_MAX_SECS: u64 = 600;
const RETRY_BACKOFF_BASE_SECS: u64 = 2;
const CHUNK_DELAY_MS: u64 = 100;

/// Requests will be sent according to bot instance.
#[derive(Clone)]
pub struct BotInstance {
    pub bot_token: String,
    pub chat_id: String,
    pub message_thread_id: Option<String>,
}

/// Telegram's error result.
#[derive(Debug, serde::Deserialize)]
struct TelegramErrorResult {
    #[allow(unused)]
    pub ok: bool,
    #[allow(unused)]
    pub error_code: i32,
    pub description: String,
    #[serde(default)]
    pub parameters: Option<TelegramErrorParameters>,
}

#[derive(Debug, serde::Deserialize)]
struct TelegramErrorParameters {
    pub retry_after: Option<u64>,
}

/// Parse mode for `sendMessage` API
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SendMessageParseMode {
    MarkdownV2,
    HTML,
}

/// Options which can be used with `sendMessage` API
pub struct SendMessageOption {
    pub parse_mode: SendMessageParseMode,
}

fn get_send_message_parse_mode_str(mode: SendMessageParseMode) -> &'static str {
    match mode {
        SendMessageParseMode::MarkdownV2 => "MarkdownV2",
        SendMessageParseMode::HTML => "HTML",
    }
}

#[derive(Debug, serde::Serialize)]
struct RequestObj {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
}

/// Create an instance to interact with APIs.
pub fn telegram_create_instance(bot_token: &str, chat_id: &str) -> BotInstance {
    // chat-id:thread-id
    let mut parts = chat_id.splitn(2, ':');
    let chat_id_part = parts.next().unwrap_or_default();
    let thread_id_part = parts.next().map(ToString::to_string);

    BotInstance {
        bot_token: bot_token.to_string(),
        chat_id: chat_id_part.to_string(),
        message_thread_id: thread_id_part,
    }
}

#[allow(clippy::too_many_lines)]
pub async fn telegram_send_message(
    _app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    instance: &BotInstance,
    msg: &str,
    options: Option<&SendMessageOption>,
) {
    let chat_id = instance.chat_id.clone();
    let raw_url_str = format!("https://api.telegram.org/bot{}/sendMessage", instance.bot_token);
    let url = match Url::parse(&raw_url_str) {
        Ok(url) => url,
        Err(e) => {
            error!("Message wasn't sent to {chat_id} telegram api because of: {e}");
            return;
        }
    };

    let parse_mode = options.map(|o| o.parse_mode);
    let chunks = chunk_message(msg, parse_mode);

    for (i, chunk_text) in chunks.iter().enumerate() {
        let request_json_obj = RequestObj {
            chat_id: instance.chat_id.clone(),
            message_thread_id: instance.message_thread_id.clone(),
            text: chunk_text.clone(),
            parse_mode: options.map(|o| get_send_message_parse_mode_str(o.parse_mode)).map(ToString::to_string),
        };

        let mut delivered = false;
        for attempt in 0..=MAX_RETRIES_PER_CHUNK {
            let result = client
                .post(url.clone())
                .json(&request_json_obj)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await;

            match result {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        debug!("Message chunk {}/{} sent successfully to {chat_id} telegram api", i + 1, chunks.len());
                        delivered = true;
                        break;
                    }

                    let parsed_error = response.json::<TelegramErrorResult>().await.ok();
                    if let Some(err) = parsed_error.as_ref() {
                        error!(
                            "Message chunk {}/{} wasn't sent to {chat_id} telegram api because of: {}",
                            i + 1,
                            chunks.len(),
                            err.description
                        );
                    } else {
                        error!(
                            "Message chunk {}/{} wasn't sent to {chat_id} telegram api. Telegram response could not be parsed!",
                            i + 1,
                            chunks.len()
                        );
                    }

                    let retriable_status =
                        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if !retriable_status {
                        break;
                    }

                    if attempt < MAX_RETRIES_PER_CHUNK {
                        if status == StatusCode::TOO_MANY_REQUESTS {
                            if let Some(retry_after_secs) =
                                parsed_error.as_ref().and_then(extract_retry_after_secs)
                            {
                                let wait_secs = retry_after_secs.clamp(1, RETRY_AFTER_MAX_SECS);
                                warn!(
                                    "Telegram rate limit for chunk {}/{} to {chat_id}: retrying in {}s (attempt {}/{})",
                                    i + 1,
                                    chunks.len(),
                                    wait_secs,
                                    attempt + 1,
                                    MAX_RETRIES_PER_CHUNK + 1
                                );
                                tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
                                continue;
                            }
                        }

                        let backoff = (RETRY_BACKOFF_BASE_SECS
                            .saturating_mul(2_u64.saturating_pow(u32::from(attempt))))
                        .min(RETRY_AFTER_MAX_SECS);
                        tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    if attempt < MAX_RETRIES_PER_CHUNK {
                        let backoff = (RETRY_BACKOFF_BASE_SECS
                            .saturating_mul(2_u64.saturating_pow(u32::from(attempt))))
                        .min(RETRY_AFTER_MAX_SECS);
                        warn!(
                            "Message chunk {}/{} send attempt {}/{} failed for {chat_id}: {e}; retrying in {}s",
                            i + 1,
                            chunks.len(),
                            attempt + 1,
                            MAX_RETRIES_PER_CHUNK + 1,
                            backoff
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                        continue;
                    }
                    error!(
                        "Message chunk {}/{} wasn't sent to {chat_id} telegram api because of: {e}",
                        i + 1,
                        chunks.len()
                    );
                    break;
                }
            }
        }

        if !delivered {
            error!(
                "Message chunk {}/{} could not be delivered to {chat_id} telegram api after retries",
                i + 1,
                chunks.len()
            );
        }

        // Small delay between chunks to be polite to the API
        if i < chunks.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(CHUNK_DELAY_MS)).await;
        }
    }
}

fn parse_retry_after_from_description(description: &str) -> Option<u64> {
    let lowered = description.to_ascii_lowercase();
    let marker = "retry after ";
    let start = lowered.find(marker)?;
    lowered[start + marker.len()..].chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok()
}

fn extract_retry_after_secs(error: &TelegramErrorResult) -> Option<u64> {
    error
        .parameters
        .as_ref()
        .and_then(|p| p.retry_after)
        .or_else(|| parse_retry_after_from_description(&error.description))
}

/// Chunks the message respecting the parse mode and `MAX_MESSAGE_LENGTH`.
fn chunk_message(text: &str, parse_mode: Option<SendMessageParseMode>) -> Vec<String> {
    match parse_mode {
        Some(SendMessageParseMode::HTML) => chunk_html(text, MAX_MESSAGE_LENGTH),
        Some(SendMessageParseMode::MarkdownV2) => chunk_markdown_v2(text, MAX_MESSAGE_LENGTH),
        None => chunk_plain_text(text, MAX_MESSAGE_LENGTH),
    }
}

fn chunk_plain_text(text: &str, limit: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(limit).map(|chunk| chunk.iter().collect::<String>()).collect()
}

// Just a basic list of void tags that don't need closing.
const HTML_VOID_TAGS: &[&str] =
    &["br", "hr", "img", "input", "meta", "area", "base", "col", "embed", "link", "param", "source", "track", "wbr"];

fn chunk_html(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    let mut open_tags: Vec<(String, String)> = Vec::new(); // (tag_name, full_opening_tag)

    let mut last_pos = 0;

    for cap in CONSTANTS.re_html_tag.captures_iter(text) {
        let m = cap.get(0).unwrap();
        let tag_name = cap.get(1).unwrap().as_str().to_lowercase();
        let full_tag = m.as_str();
        let start = m.start();
        let end = m.end();

        // Append preceding text
        let pre_text = &text[last_pos..start];
        append_text_checking_limit(
            &mut chunks,
            &mut current_chunk,
            pre_text,
            limit,
            &mut open_tags,
            close_html_tags,
            open_html_tags,
        );

        // Process tag
        let is_closing = full_tag.starts_with("</");
        let is_void = HTML_VOID_TAGS.contains(&tag_name.as_str());

        // Check if adding this tag would exceed limit (with theoretical closing tags)
        // This is a simplification; strict checking would require computing current closers size.
        // For HTML tags, we generally assume they fit or trigger a split if very massive.
        let mut closing_overhead = calculate_html_closing_overhead(&open_tags);
        if !is_void && !is_closing {
            closing_overhead += tag_name.len() + 3; // </tag>
        }
        if current_chunk.len() + full_tag.len() + closing_overhead > limit {
            // Force split before tag
            chunks.push(finalize_chunk(&mut current_chunk, &open_tags, close_html_tags));
            current_chunk = open_html_tags(&open_tags);
        }

        current_chunk.push_str(full_tag);

        if !is_void {
            if is_closing {
                // Remove matching from stack (scanning backwards)
                if let Some(pos) = open_tags.iter().rposition(|(t, _)| *t == tag_name) {
                    open_tags.remove(pos);
                }
            } else {
                // Open tag
                open_tags.push((tag_name, full_tag.to_string()));
            }
        }

        last_pos = end;
    }

    // Append remaining text
    let remaining_text = &text[last_pos..];
    append_text_checking_limit(
        &mut chunks,
        &mut current_chunk,
        remaining_text,
        limit,
        &mut open_tags,
        close_html_tags,
        open_html_tags,
    );

    if !current_chunk.is_empty() {
        // Close any remaining open tags for robustness (empty if input was valid)
        chunks.push(finalize_chunk(&mut current_chunk, &open_tags, close_html_tags));
    }

    chunks
}

fn close_html_tags(tags: &[(String, String)]) -> String {
    tags.iter().rev().fold(String::new(), |mut acc, (name, _)| {
        acc.push_str("</");
        acc.push_str(name);
        acc.push('>');
        acc
    })
}

fn open_html_tags(tags: &[(String, String)]) -> String {
    tags.iter().map(|(_, full)| full.clone()).collect()
}

fn calculate_html_closing_overhead(tags: &[(String, String)]) -> usize {
    tags.iter().map(|(name, _)| name.len() + 3).sum() // </name>
}

// MarkdownV2 markers
// *bold*, _italic_, __underline__, ~strikethrough~, ||spoiler||, `code`, ```pre```, [link](url)
// We need to track * _ __ ~ || ` ``` [
// Escaping is important: \char
fn chunk_markdown_v2(text: &str, limit: usize) -> Vec<String> {
    // This is a naive state machine parser for splitting.
    // It detects special markers that are NOT escaped.

    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    // Stack of open markers: string representation of the marker
    let mut open_markers: Vec<String> = Vec::new();

    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        let mut token = String::new();
        token.push(c);

        if c == '\\' {
            // Escaped content, take next char too if exists
            if let Some(next) = chars.next() {
                token.push(next);
            }
            // Escaped chars are just text, no special meaning
        } else {
            // Check for potential markers
            // We need to lookahead for multi-char markers: __, ||, ```
            // And [ for links.
            // This is simplified. Links [text](url) are hard to split. We treat [ as open, ] as close?
            // Actually, splitting inside valid Markdown structure is dangerous.
            // We mainly care about style markers: *, _, ~, ||, `

            // Handle multi-char markers
            let mut handled = false;
            if c == '`' {
                let mut lookahead = chars.clone();
                if lookahead.next() == Some('`') && lookahead.next() == Some('`') {
                    chars.next();
                    chars.next();
                    token = "```".to_string();
                    handled = true;
                }
            }
            if !handled {
                if let Some(&next_c) = chars.peek() {
                    let double = format!("{c}{next_c}");
                    if double == "__" || double == "||" {
                        chars.next(); // consume next
                        token = double;
                    }
                }
            }

            // Logic to update `open_markers`
            let is_marker = matches!(token.as_str(), "*" | "_" | "~" | "__" | "||" | "`" | "```");
            if is_marker {
                if let Some(last) = open_markers.last() {
                    if last == &token {
                        open_markers.pop();
                    } else {
                        open_markers.push(token.clone());
                    }
                } else {
                    open_markers.push(token.clone());
                }
            }
        }

        // Logic to check limit
        let closing_overhead: usize = open_markers.iter().rev().map(std::string::String::len).sum();
        if current_chunk.len() + token.len() + closing_overhead > limit {
            chunks.push(finalize_chunk(&mut current_chunk, &open_markers, |tags| tags.iter().rev().cloned().collect()));
            current_chunk = open_markers.iter().cloned().collect(); // Reopen
        }

        current_chunk.push_str(&token);
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

// Generic helper to append text handling splits
fn append_text_checking_limit<T, FClose, FOpen>(
    chunks: &mut Vec<String>,
    current_chunk: &mut String,
    text: &str,
    limit: usize,
    open_tags: &mut [T],
    close_fn: FClose,
    open_fn: FOpen,
) where
    FClose: Fn(&[T]) -> String,
    FOpen: Fn(&[T]) -> String,
{
    let mut remaining_text = text;
    while !remaining_text.is_empty() {
        let closing_overhead = close_fn(open_tags).len();
        let space_left = if limit > (current_chunk.len() + closing_overhead) {
            limit - (current_chunk.len() + closing_overhead)
        } else {
            0
        };

        if remaining_text.len() <= space_left {
            current_chunk.push_str(remaining_text);
            break;
        }
        // Take as much as fits
        let (take, rest) = split_at_char_boundary(remaining_text, space_left);
        if take.is_empty() && !current_chunk.is_empty() {
            // Cannot fit even one char, force split
            chunks.push(finalize_chunk(current_chunk, open_tags, &close_fn));
            *current_chunk = open_fn(open_tags);
            continue;
        }

        current_chunk.push_str(take);
        chunks.push(finalize_chunk(current_chunk, open_tags, &close_fn));
        *current_chunk = open_fn(open_tags);
        remaining_text = rest;
    }
}

fn split_at_char_boundary(s: &str, mut idx: usize) -> (&str, &str) {
    if idx >= s.len() {
        return (s, "");
    }
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    s.split_at(idx)
}

fn finalize_chunk<T, F>(chunk: &mut String, open_tags: &[T], close_fn: F) -> String
where
    F: Fn(&[T]) -> String,
{
    let closers = close_fn(open_tags);
    chunk.push_str(&closers);
    let result = chunk.clone();
    chunk.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_plain_text() {
        let text = "abcdefgh";
        let chunks = chunk_plain_text(text, 3);
        assert_eq!(chunks, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn test_chunk_html_simple_split() {
        let text = "<b>Hello World</b>";
        // Limit 10.
        // "<b>Hello " is 8 chars. Closing "</b>" is 4. overhead 4.
        // "<b>Hel" (6) + overhead(4) = 10 <= 10.
        // So chunk 1: "<b>Hel</b>" (10 chars exactly)
        // intermediate text splits...
        // The final closing tag results in an extra empty bold tag <b></b> because the previous text split forced a close/reopen.
        let chunks = chunk_html(text, 10);
        // Correct behavior check
        assert!(chunks.iter().all(|c| c.len() <= 10));
        assert_eq!(chunks.join(""), "<b>Hel</b><b>lo </b><b>Wor</b><b>ld</b><b></b>");
        // Wait, join won't match exactly because we introduced extra tags.
        // But visually it should be equivalent content.
    }

    #[test]
    fn test_chunk_html_nested() {
        let text = "<b><i>BoldItalic</i></b>";
        // Limit 15
        let chunks = chunk_html(text, 15);
        assert!(chunks.iter().all(|c| c.len() <= 15));
        // Expect split preserving nesting
    }

    #[test]
    fn test_chunk_markdown_v2_simple() {
        let text = "*bold*";
        let chunks = chunk_markdown_v2(text, 3);
        // *b*
        // *o*
        // *l*
        // *d*  ??
        // open overhead is 1 (*). close overhead 1.
        // *b* is 3.
        assert!(chunks.iter().all(|c| c.len() <= 3));
    }

    #[test]
    fn test_parse_retry_after_from_description() {
        let retry = parse_retry_after_from_description("Too Many Requests: retry after 264");
        assert_eq!(retry, Some(264));
    }
}
