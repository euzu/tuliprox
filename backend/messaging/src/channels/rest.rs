//! Generic REST/webhook channel.

use crate::channel::{
    delivery_for_status, parse_retry_after, ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage,
    SendFuture,
};
use log::debug;
use reqwest::{header, Method};
use shared::model::notification::{EventId, Severity};
use std::str::FromStr;
use tuliprox_core::model::{ChannelRouting, RestMessagingConfig};

pub struct RestChannel {
    config: RestMessagingConfig,
    client: reqwest::Client,
}

impl RestChannel {
    pub fn new(config: RestMessagingConfig, client: reqwest::Client) -> Self { Self { config, client } }
}

impl NotificationChannel for RestChannel {
    fn id(&self) -> &'static str { "rest" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            let method = Method::from_str(&self.config.method).unwrap_or(Method::POST);
            let mut rb = self.client.request(method, &self.config.url);

            let has_content_type = self.config.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"));
            if !has_content_type {
                rb = rb.header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string());
            }
            for (key, value) in &self.config.headers {
                rb = rb.header(key, value);
            }

            match rb.body(msg.body.clone()).send().await {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = parse_retry_after(response.headers());
                    if status.is_success() {
                        debug!("Notification delivered to rest api");
                    }
                    delivery_for_status(status, retry_after)
                }
                Err(err) => Delivery::retry(format!("rest request failed: {err}")),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}
