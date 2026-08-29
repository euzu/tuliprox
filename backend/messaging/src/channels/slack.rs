//! Slack incoming webhook.
//!
//! Not a Discord clone: Block Kit differs enough from Discord embeds that
//! reusing the Discord payload shape produces bad output.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
};
use log::debug;
use serde_json::json;
use shared::model::notification::{EventId, Severity};
use tuliprox_core::model::{ChannelRouting, SlackMessagingConfig};

/// Slack rejects a `text` field longer than this.
const SLACK_TEXT_LIMIT: usize = 3000;

#[derive(Clone)]
pub struct SlackChannel {
    config: SlackMessagingConfig,
    client: reqwest::Client,
}

impl SlackChannel {
    pub fn new(config: SlackMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }

    /// Slack has no severity field, so it goes in the header text where a
    /// human will actually see it.
    fn severity_marker(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => "",
            Severity::Warn => "[warn] ",
            Severity::Error => "[error] ",
            Severity::Critical => "[CRITICAL] ",
        }
    }
}

impl NotificationChannel for SlackChannel {
    fn id(&self) -> &'static str { "slack" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        {
            let body = if msg.templated {
                msg.body.clone()
            } else {
                let text: String = msg.body.chars().take(SLACK_TEXT_LIMIT).collect();
                let header = format!("{}{}", Self::severity_marker(msg.event.severity), msg.event.title);
                json!({
                    "text": header,
                    "blocks": [
                        { "type": "header", "text": { "type": "plain_text", "text": header, "emoji": true } },
                        { "type": "section", "text": { "type": "mrkdwn", "text": text } },
                        { "type": "context", "elements": [
                            { "type": "mrkdwn", "text": format!("`{}` · {}", msg.event.id, msg.event.timestamp_rfc3339()) }
                        ] }
                    ]
                })
                .to_string()
            };
            match self
                .client
                .post(&self.config.url)
                .header(reqwest::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to Slack");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("slack request failed: {err}")),
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}

#[cfg(test)]
mod tests {
    use super::{Severity, SlackChannel};

    #[test]
    fn only_non_info_severities_get_a_marker() {
        // Slack has no severity field, so it has to go in the header text -
        // but tagging every routine notification would be noise.
        assert_eq!(SlackChannel::severity_marker(Severity::Info), "");
        assert!(SlackChannel::severity_marker(Severity::Warn).contains("warn"));
        assert!(SlackChannel::severity_marker(Severity::Critical).contains("CRITICAL"));
    }
}
