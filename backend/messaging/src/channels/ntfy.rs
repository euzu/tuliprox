//! ntfy channel - self-hosted push, no account, no bot token.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
    SendFuture,
};
use log::debug;
use reqwest::header;
use shared::model::notification::{EventId, Severity};
use tuliprox_core::model::{ChannelRouting, NtfyMessagingConfig};

pub struct NtfyChannel {
    config: NtfyMessagingConfig,
    client: reqwest::Client,
}

impl NtfyChannel {
    pub fn new(config: NtfyMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }

    /// ntfy's 1-5 priority scale.
    fn priority(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => "2",
            Severity::Warn => "3",
            Severity::Error => "4",
            Severity::Critical => "5",
        }
    }

    /// ntfy headers must be ASCII, and a title with an emoji in it is
    /// common. Strip rather than fail the send.
    fn ascii_title(title: &str) -> String {
        let cleaned: String = title.chars().filter(char::is_ascii).collect();
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            "tuliprox".to_string()
        } else {
            trimmed.chars().take(200).collect()
        }
    }
}

impl NotificationChannel for NtfyChannel {
    fn id(&self) -> &'static str { "ntfy" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let url = format!("{}/{}", self.config.url, self.config.topic);
            let mut rb = self
                .client
                .post(&url)
                .header("X-Title", Self::ascii_title(&msg.event.title))
                .header("X-Priority", Self::priority(msg.event.severity))
                .header("X-Tags", msg.event.id.domain())
                .body(msg.body.clone());
            if let Some(token) = &self.config.token {
                rb = rb.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            match rb.send().await {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to ntfy");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("ntfy request failed: {err}")),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}

#[cfg(test)]
mod tests {
    use super::{NtfyChannel, Severity};

    #[test]
    fn severity_maps_onto_the_ntfy_priority_scale() {
        assert_eq!(NtfyChannel::priority(Severity::Info), "2");
        assert_eq!(NtfyChannel::priority(Severity::Warn), "3");
        assert_eq!(NtfyChannel::priority(Severity::Error), "4");
        assert_eq!(NtfyChannel::priority(Severity::Critical), "5");
    }

    #[test]
    fn a_title_with_emoji_is_stripped_to_ascii_rather_than_failing_the_send() {
        // ntfy headers must be ASCII, and titles routinely carry emoji.
        assert_eq!(NtfyChannel::ascii_title("Recording completed \u{1F534}"), "Recording completed");
    }

    #[test]
    fn a_title_that_is_entirely_non_ascii_falls_back_rather_than_going_out_empty() {
        assert_eq!(NtfyChannel::ascii_title("\u{1F534}\u{1F535}"), "tuliprox");
    }
}
