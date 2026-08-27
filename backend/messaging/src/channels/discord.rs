//! Discord webhook channel.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
    SendFuture,
};
use log::debug;
use reqwest::header;
use serde_json::json;
use shared::model::notification::EventId;
use tuliprox_core::model::DiscordMessagingConfig;

/// Discord rejects a webhook payload whose `content` exceeds this.
const DISCORD_CONTENT_LIMIT: usize = 2000;

pub struct DiscordChannel {
    config: DiscordMessagingConfig,
    client: reqwest::Client,
}

impl DiscordChannel {
    pub fn new(config: DiscordMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }
}

impl NotificationChannel for DiscordChannel {
    fn id(&self) -> &'static str { "discord" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            // A template is expected to produce the whole webhook payload.
            // Without one, wrap the plain body in the minimal valid shape.
            let body = if msg.templated {
                msg.body.clone()
            } else {
                let truncated: String = msg.body.chars().take(DISCORD_CONTENT_LIMIT).collect();
                json!({ "content": truncated }).to_string()
            };

            match self
                .client
                .post(&self.config.url)
                .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to Discord");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("discord request failed: {err}")),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}
