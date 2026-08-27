//! The built-in channels.
//!
//! Each is one [`NotificationChannel`](crate::channel::NotificationChannel)
//! impl. Adding another means adding a module here, a config field, and a
//! line in [`build`] - the dispatcher, the outbox and the renderer stay
//! untouched.

pub mod discord;
pub mod pushover;
pub mod rest;
pub mod telegram;

use crate::channel::ChannelSet;
use log::warn;
use std::sync::Arc;
use tuliprox_core::{
    model::{AppConfig, MessagingConfig},
    utils::request::create_client,
};

/// Per-channel request timeout.
///
/// The shared client sets no request timeout at all, and the outbox awaits
/// sends in sequence - so one webhook host that accepts a connection and
/// never answers used to stall every pending notification, including the
/// recording ones the outbox exists to protect.
const DEFAULT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Build an HTTP client for a channel, with a bounded request timeout.
fn channel_client(app_config: &Arc<AppConfig>) -> reqwest::Client {
    create_client(app_config).timeout(DEFAULT_SEND_TIMEOUT).build().unwrap_or_else(|err| {
        warn!("Falling back to a default HTTP client for notifications: {err}");
        reqwest::Client::new()
    })
}

/// Every channel the current config declares, in a stable order.
///
/// Order matters only for reproducible logs and outbox entries; delivery
/// is concurrent.
#[must_use]
pub fn build(app_config: &Arc<AppConfig>, messaging: &MessagingConfig) -> ChannelSet {
    let client = channel_client(app_config);
    let mut channels: ChannelSet = Vec::with_capacity(4);
    if let Some(cfg) = messaging.telegram.as_ref() {
        channels.push(Arc::new(telegram::TelegramChannel::new(cfg.clone(), Arc::clone(app_config), client.clone())));
    }
    if let Some(cfg) = messaging.rest.as_ref() {
        channels.push(Arc::new(rest::RestChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.pushover.as_ref() {
        channels.push(Arc::new(pushover::PushoverChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.discord.as_ref() {
        channels.push(Arc::new(discord::DiscordChannel::new(cfg.clone(), client)));
    }
    channels
}
