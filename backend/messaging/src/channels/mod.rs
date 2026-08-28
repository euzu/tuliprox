//! The built-in channels.
//!
//! Each is one [`NotificationChannel`](crate::channel::NotificationChannel)
//! impl plus a [`Channel`] variant. Adding another means a module here, an
//! enum variant, a config field and a line in [`build`] - the dispatcher,
//! the outbox and the renderer stay untouched.
//!
//! [`Channel`] is what makes dispatch static: the match monomorphizes into
//! a direct call, so there is no vtable and no boxed future anywhere on the
//! send path.

pub mod command;
pub mod discord;
pub mod gotify;
pub mod ntfy;
pub mod pushover;
pub mod rest;
pub mod slack;
pub mod telegram;

use crate::channel::{ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage};
use log::warn;
use shared::model::notification::{EventId, Severity};
use std::sync::{Arc, OnceLock, RwLock};
use tuliprox_core::{
    model::{AppConfig, ChannelRouting, MessagingConfig},
    utils::request::create_client,
};

/// One configured channel.
///
/// An enum rather than a trait object, so every dispatch is a direct call
/// the compiler can inline. `NotificationChannel::send` returns an opaque
/// future, which makes the trait non-object-safe by construction - this is
/// the only way an implementation is reached.
#[derive(Clone)]
pub enum Channel {
    Telegram(telegram::TelegramChannel),
    Rest(rest::RestChannel),
    Pushover(pushover::PushoverChannel),
    Discord(discord::DiscordChannel),
    Ntfy(ntfy::NtfyChannel),
    Gotify(gotify::GotifyChannel),
    Slack(slack::SlackChannel),
    Command(command::CommandChannel),
}

/// Forward one `&self` method to whichever variant is present.
macro_rules! delegate {
    ($self:ident, $inner:ident => $call:expr) => {
        match $self {
            Channel::Telegram($inner) => $call,
            Channel::Rest($inner) => $call,
            Channel::Pushover($inner) => $call,
            Channel::Discord($inner) => $call,
            Channel::Ntfy($inner) => $call,
            Channel::Gotify($inner) => $call,
            Channel::Slack($inner) => $call,
            Channel::Command($inner) => $call,
        }
    };
}

impl Channel {
    /// Stable wire id: config key, outbox key, metric label, template prefix.
    #[must_use]
    pub fn id(&self) -> &'static str {
        delegate!(self, c => c.id())
    }

    #[must_use]
    pub fn capabilities(&self) -> ChannelCapabilities {
        delegate!(self, c => c.capabilities())
    }

    #[must_use]
    pub fn template_for(&self, event: EventId) -> Option<&str> {
        delegate!(self, c => c.template_for(event))
    }

    #[must_use]
    pub fn routing(&self) -> &ChannelRouting {
        delegate!(self, c => c.routing())
    }

    #[must_use]
    pub fn wants(&self, event: EventId, severity: Severity) -> bool {
        delegate!(self, c => c.wants(event, severity))
    }

    /// Deliver through the concrete channel.
    ///
    /// Each arm awaits a distinct concrete future type; the `async fn` is
    /// what unifies them without boxing.
    pub async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        match self {
            Channel::Telegram(c) => c.send(msg).await,
            Channel::Rest(c) => c.send(msg).await,
            Channel::Pushover(c) => c.send(msg).await,
            Channel::Discord(c) => c.send(msg).await,
            Channel::Ntfy(c) => c.send(msg).await,
            Channel::Gotify(c) => c.send(msg).await,
            Channel::Slack(c) => c.send(msg).await,
            Channel::Command(c) => c.send(msg).await,
        }
    }
}

/// The channels configured right now, in a stable order.
pub type ChannelSet = Vec<Channel>;

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

fn cache() -> &'static RwLock<Option<ChannelSet>> {
    CACHE.get_or_init(|| RwLock::new(None))
}

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
        channels.push(Channel::Telegram(telegram::TelegramChannel::new(
            cfg.clone(),
            Arc::clone(app_config),
            client.clone(),
        )));
    }
    if let Some(cfg) = messaging.rest.as_ref() {
        channels.push(Channel::Rest(rest::RestChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.pushover.as_ref() {
        channels.push(Channel::Pushover(pushover::PushoverChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.discord.as_ref() {
        channels.push(Channel::Discord(discord::DiscordChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.ntfy.as_ref() {
        channels.push(Channel::Ntfy(ntfy::NtfyChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.gotify.as_ref() {
        channels.push(Channel::Gotify(gotify::GotifyChannel::new(cfg.clone(), client.clone())));
    }
    if let Some(cfg) = messaging.slack.as_ref() {
        channels.push(Channel::Slack(slack::SlackChannel::new(cfg.clone(), client)));
    }
    if let Some(cfg) = messaging.command.as_ref() {
        // No HTTP client: this one shells out.
        channels.push(Channel::Command(command::CommandChannel::new(cfg.clone())));
    }
    channels
}
