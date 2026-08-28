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
    fn id(&self) -> &'static str {
        "telegram"
    }

    fn template_for(&self, event: EventId) -> Option<&str> {
        self.config.templates.get(&event).map(String::as_str)
    }

    fn routing(&self) -> &ChannelRouting {
        &self.config.routing
    }

    fn wants(&self, event: EventId, severity: Severity) -> bool {
        self.config.routing.accepts(event, severity)
    }

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
                let result =
                    telegram_send_message(&self.app_config, &self.client, &bot, &message, options.as_ref()).await;
                let mut delivered = result.delivered;
                if self.config.markdown && msg.templated && result.parse_error && !delivered {
                    // Template output can carry dynamic fields that break
                    // MarkdownV2. Retry once, escaped.
                    let escaped = escape_markdown_v2(raw);
                    let escaped_options = SendMessageOption { parse_mode: SendMessageParseMode::MarkdownV2 };
                    delivered =
                        telegram_send_message(&self.app_config, &self.client, &bot, &escaped, Some(&escaped_options))
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

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }
}
