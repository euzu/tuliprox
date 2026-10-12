//! Telegram channel.

use crate::channel::{ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage};
use log::debug;
use shared::{
    model::notification::{EventId, Severity},
    utils::{escape_markdown_v2, json_str_to_markdown},
};
use std::{borrow::Cow, sync::Arc};
use tuliprox_core::{
    model::{AppConfig, ChannelRouting, TelegramMessagingConfig},
    utils::{telegram_create_instance, telegram_send_message, SendMessageOption, SendMessageParseMode},
};

/// Telegram's message limit, in UTF-16 code units. It applies after entity
/// parsing, so measuring the raw `MarkdownV2` text is conservative.
const TELEGRAM_MESSAGE_LIMIT: usize = 4096;

/// Cut `text` to what Telegram accepts.
///
/// Done here rather than through `ChannelCapabilities::max_body_bytes`: the
/// limit counts UTF-16 units, not bytes, and `MarkdownV2` escaping grows the
/// text after the dispatcher has truncated it. Always borrows.
fn fit_message(text: &str) -> &str {
    // A UTF-16 unit is never fewer than one byte, so short text needs no scan.
    if text.len() <= TELEGRAM_MESSAGE_LIMIT {
        return text;
    }
    let mut units = 0usize;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > TELEGRAM_MESSAGE_LIMIT {
            let cut = &text[..index];
            // An odd run of trailing backslashes is an escape cut off from the
            // character it escapes; Telegram rejects that as a parse error.
            let backslashes = cut.len() - cut.trim_end_matches('\\').len();
            return if backslashes % 2 == 1 { &cut[..cut.len() - 1] } else { cut };
        }
    }
    text
}

#[derive(Clone)]
pub struct TelegramChannel {
    config: TelegramMessagingConfig,
    app_config: Arc<AppConfig>,
    client: reqwest::Client,
}

impl TelegramChannel {
    pub fn new(config: TelegramMessagingConfig, app_config: Arc<AppConfig>, client: reqwest::Client) -> Self {
        Self { config, app_config, client }
    }
}

impl NotificationChannel for TelegramChannel {
    fn id(&self) -> &'static str { "telegram" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        {
            let raw = &msg.body;
            let (message, options) = if self.config.markdown {
                if let Ok(md) = json_str_to_markdown(raw) {
                    (Cow::Owned(md), Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }))
                } else if msg.templated {
                    // Keep deliberate template markdown as-is.
                    (Cow::Borrowed(raw), Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }))
                } else {
                    (
                        Cow::Owned(escape_markdown_v2(raw)),
                        Some(SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 }),
                    )
                }
            } else {
                (Cow::Borrowed(raw), None)
            };

            // A chat that is already delivered must not be re-sent when
            // another chat fails, so failures are reported per chat id and
            // the outbox retries only what is still pending.
            let mut failed: Vec<String> = Vec::new();
            for chat_id in &self.config.chat_ids {
                let bot = telegram_create_instance(&self.config.bot_token, chat_id);
                let result = telegram_send_message(
                    &self.app_config,
                    &self.client,
                    &bot,
                    fit_message(&message),
                    options.as_ref(),
                )
                .await;
                let mut delivered = result.delivered;
                if self.config.markdown && msg.templated && result.parse_error && !delivered {
                    // Template output can carry dynamic fields that break
                    // MarkdownV2. Retry once, escaped.
                    let escaped = escape_markdown_v2(raw);
                    let escaped_options = SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 };
                    delivered = telegram_send_message(
                        &self.app_config,
                        &self.client,
                        &bot,
                        fit_message(&escaped),
                        Some(&escaped_options),
                    )
                    .await
                    .delivered;
                }
                if delivered {
                    debug!("Notification delivered to Telegram chat {chat_id}");
                } else {
                    failed.push(chat_id.clone());
                }
            }

            if failed.is_empty() {
                Delivery::Delivered
            } else {
                Delivery::retry(format!("telegram chat id(s) not delivered: {}", failed.join(", ")))
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}

#[cfg(test)]
mod tests {
    use super::{fit_message, TELEGRAM_MESSAGE_LIMIT};

    #[test]
    fn a_short_message_is_borrowed_unchanged() {
        let text = "Watch changes in sports.";
        assert!(std::ptr::eq(fit_message(text), text));
    }

    #[test]
    fn a_long_message_is_cut_to_the_limit() {
        let text = "a".repeat(TELEGRAM_MESSAGE_LIMIT + 900);
        assert_eq!(fit_message(&text).len(), TELEGRAM_MESSAGE_LIMIT);
    }

    /// Telegram counts UTF-16 code units; an emoji outside the BMP is two.
    #[test]
    fn the_limit_counts_utf16_units_not_bytes_or_chars() {
        let text = "\u{1F534}".repeat(TELEGRAM_MESSAGE_LIMIT);
        let fitted = fit_message(&text);
        assert_eq!(fitted.chars().count(), TELEGRAM_MESSAGE_LIMIT / 2);
        let cyrillic = "ж".repeat(TELEGRAM_MESSAGE_LIMIT);
        assert_eq!(fit_message(&cyrillic).chars().count(), TELEGRAM_MESSAGE_LIMIT, "2-byte chars are 1 unit each");
    }

    /// Cutting between a `MarkdownV2` escape and the character it escapes would
    /// leave a dangling backslash, which Telegram rejects as a parse error.
    #[test]
    fn a_cut_escape_drops_its_dangling_backslash() {
        let text = format!("{}\\.tail", "a".repeat(TELEGRAM_MESSAGE_LIMIT - 1));
        assert_eq!(fit_message(&text), "a".repeat(TELEGRAM_MESSAGE_LIMIT - 1));
    }

    #[test]
    fn an_escaped_backslash_at_the_cut_is_kept() {
        let text = format!("{}\\\\tail", "a".repeat(TELEGRAM_MESSAGE_LIMIT - 2));
        assert_eq!(fit_message(&text), format!("{}\\\\", "a".repeat(TELEGRAM_MESSAGE_LIMIT - 2)));
    }
}
