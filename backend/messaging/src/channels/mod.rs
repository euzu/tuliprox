//! The built-in channels.
//!
//! Each is one [`NotificationChannel`](crate::channel::NotificationChannel)
//! impl. Adding another means adding a module here, a config field, and a
//! line in [`build`] - the dispatcher, the outbox and the renderer stay
//! untouched.

pub mod command;
pub mod discord;
pub mod gotify;
pub mod ntfy;
pub mod pushover;
pub mod rest;
pub mod slack;
pub mod telegram;

use crate::channel::ChannelSet;
use log::warn;
use std::sync::{Arc, OnceLock, RwLock};
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

/// The built channel set, so a notification does not rebuild every channel
/// - and a fresh `reqwest::Client` with them - on every send.
///
/// Invalidated on config reload via [`invalidate`].
static CACHE: OnceLock<RwLock<Option<ChannelSet>>> = OnceLock::new();

fn cache() -> &'static RwLock<Option<ChannelSet>> { CACHE.get_or_init(|| RwLock::new(None)) }

/// Drop the cached channel set. Call on config reload.
pub fn invalidate() {
    if let Ok(mut guard) = cache().write() {
        *guard = None;
    }
    crate::rate_limit::reset();
}

/// Every channel the current config declares, in a stable order.
///
/// Cached: repeated calls return the same channels, so per-channel state
/// (rate limiting, suppression) survives between notifications.
#[must_use]
pub fn channels(app_config: &Arc<AppConfig>, messaging: &MessagingConfig) -> ChannelSet {
    if let Ok(guard) = cache().read() {
        if let Some(set) = guard.as_ref() {
            return set.clone();
        }
    }
    let set = build(app_config, messaging);
    if let Ok(mut guard) = cache().write() {
        *guard = Some(set.clone());
    }
    set
}

/// Construct the channel set from config, bypassing the cache.
#[must_use]
pub fn build(app_config: &Arc<AppConfig>, messaging: &MessagingConfig) -> ChannelSet {
    let client = channel_client(app_config);
    let mut channels: ChannelSet = Vec::with_capacity(8);
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
        channels.push(Arc::new(discord::DiscordChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.ntfy.as_ref() {
        channels.push(Arc::new(ntfy::NtfyChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.gotify.as_ref() {
        channels.push(Arc::new(gotify::GotifyChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.slack.as_ref() {
        channels.push(Arc::new(slack::SlackChannel::new(cfg.clone(), client)));
    }
    if let Some(cfg) = messaging.command.as_ref() {
        // No HTTP client: this one shells out.
        channels.push(Arc::new(command::CommandChannel::new(cfg.clone())));
    }
    channels
}
