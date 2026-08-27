//! Pushover channel.
//!
//! Pushover had no `templates` map at all, so every notification took the
//! built-in text - which for watch changes and playlist stats was a raw
//! `serde_json` dump pushed to a phone. It now renders templates like every
//! other channel, and sends `title` and `body` separately so the push
//! notification has a usable headline.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
    SendFuture,
};
use log::debug;
use reqwest::header;
use shared::model::notification::{EventId, Severity};
use tuliprox_core::model::PushoverMessagingConfig;

/// Pushover truncates the message body here.
const PUSHOVER_BODY_LIMIT: usize = 1024;
/// And the title here.
const PUSHOVER_TITLE_LIMIT: usize = 250;

pub struct PushoverChannel {
    config: PushoverMessagingConfig,
    client: reqwest::Client,
}

impl PushoverChannel {
    pub fn new(config: PushoverMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }

    /// Pushover's own priority scale, from the event severity.
    fn priority(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => "-1",
            Severity::Warn => "0",
            Severity::Error | Severity::Critical => "1",
        }
    }
}

impl NotificationChannel for PushoverChannel {
    fn id(&self) -> &'static str { "pushover" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let body: String = msg.body.chars().take(PUSHOVER_BODY_LIMIT).collect();
            let title: String = msg.event.title.chars().take(PUSHOVER_TITLE_LIMIT).collect();
            let encoded: String = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("token", self.config.token.as_str())
                .append_pair("user", self.config.user.as_str())
                .append_pair("title", &title)
                .append_pair("message", &body)
                .append_pair("priority", Self::priority(msg.event.severity))
                .finish();

            match self
                .client
                .post(&self.config.url)
                .header(header::CONTENT_TYPE, mime::APPLICATION_WWW_FORM_URLENCODED.to_string())
                .body(encoded)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to Pushover");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("pushover request failed: {err}")),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities { supports_templates: true, max_body_bytes: Some(PUSHOVER_BODY_LIMIT) }
    }
}

#[cfg(test)]
mod tests {
    use super::{PushoverChannel, Severity};

    #[test]
    fn severity_maps_onto_the_pushover_priority_scale() {
        assert_eq!(PushoverChannel::priority(Severity::Info), "-1");
        assert_eq!(PushoverChannel::priority(Severity::Warn), "0");
        assert_eq!(PushoverChannel::priority(Severity::Error), "1");
        assert_eq!(PushoverChannel::priority(Severity::Critical), "1");
    }
}
