//! Gotify channel.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
    SendFuture,
};
use log::debug;
use serde_json::json;
use shared::model::notification::{EventId, Severity};
use tuliprox_core::model::{ChannelRouting, GotifyMessagingConfig};

pub struct GotifyChannel {
    config: GotifyMessagingConfig,
    client: reqwest::Client,
}

impl GotifyChannel {
    pub fn new(config: GotifyMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }

    /// Gotify's 0-10 priority scale.
    fn priority(severity: Severity) -> u8 {
        match severity {
            Severity::Info => 2,
            Severity::Warn => 5,
            Severity::Error => 7,
            Severity::Critical => 9,
        }
    }
}

impl NotificationChannel for GotifyChannel {
    fn id(&self) -> &'static str { "gotify" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            // A template is expected to produce the whole payload; without
            // one, build the minimal valid shape.
            let body = if msg.templated {
                msg.body.clone()
            } else {
                json!({
                    "title": msg.event.title,
                    "message": msg.body,
                    "priority": Self::priority(msg.event.severity),
                })
                .to_string()
            };
            let url = format!("{}/message", self.config.url);
            match self
                .client
                .post(&url)
                .query(&[("token", self.config.token.as_str())])
                .header(reqwest::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to Gotify");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("gotify request failed: {err}")),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}

#[cfg(test)]
mod tests {
    use super::{GotifyChannel, Severity};

    #[test]
    fn severity_maps_onto_the_gotify_priority_scale() {
        assert!(GotifyChannel::priority(Severity::Info) < GotifyChannel::priority(Severity::Warn));
        assert!(GotifyChannel::priority(Severity::Warn) < GotifyChannel::priority(Severity::Error));
        assert!(GotifyChannel::priority(Severity::Error) < GotifyChannel::priority(Severity::Critical));
        assert!(GotifyChannel::priority(Severity::Critical) <= 10);
    }
}
