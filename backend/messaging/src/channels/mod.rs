//! The channels: the built-in command channel and the plugin channels.
//!
//! Each is one [`NotificationChannel`](crate::channel::NotificationChannel)
//! impl plus a [`Channel`] variant - the dispatcher, the outbox and the
//! renderer stay untouched.
//!
//! [`Channel`] is what makes dispatch static: the match monomorphizes into
//! a direct call, so there is no vtable and no boxed future anywhere on the
//! send path.

pub mod command;
pub mod plugin;

use crate::channel::{ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage};
use shared::model::{
    notification::{EventId, Severity},
    PluginNotifier,
};
use std::sync::{Arc, OnceLock, RwLock};
use tuliprox_core::model::{ChannelRouting, MessagingConfig};

/// One configured channel.
///
/// An enum rather than a trait object, so every dispatch is a direct call
/// the compiler can inline. `NotificationChannel::send` returns an opaque
/// future, which makes the trait non-object-safe by construction - this is
/// the only way an implementation is reached.
#[derive(Clone)]
pub enum Channel {
    Command(command::CommandChannel),
    /// A wasm plugin that exports `send-notification`; reached through the
    /// registered [`PluginNotifier`].
    Plugin(plugin::PluginChannel),
}

/// Forward one `&self` method to whichever variant is present.
macro_rules! delegate {
    ($self:ident, $inner:ident => $call:expr) => {
        match $self {
            Channel::Command($inner) => $call,
            Channel::Plugin($inner) => $call,
        }
    };
}

impl Channel {
    /// Stable wire id: config key, outbox key, metric label, template prefix.
    #[must_use]
    pub fn id(&self) -> &'static str { delegate!(self, c => c.id()) }

    #[must_use]
    pub fn capabilities(&self) -> ChannelCapabilities { delegate!(self, c => c.capabilities()) }

    #[must_use]
    pub fn template_for(&self, event: EventId) -> Option<&str> { delegate!(self, c => c.template_for(event)) }

    #[must_use]
    pub fn routing(&self) -> &ChannelRouting { delegate!(self, c => c.routing()) }

    #[must_use]
    pub fn wants(&self, event: EventId, severity: Severity) -> bool { delegate!(self, c => c.wants(event, severity)) }

    /// Deliver through the concrete channel.
    ///
    /// Each arm awaits a distinct concrete future type; the `async fn` is
    /// what unifies them without boxing.
    pub async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        match self {
            Channel::Command(c) => c.send(msg).await,
            Channel::Plugin(c) => c.send(msg).await,
        }
    }
}

/// The channels configured right now, in a stable order.
pub type ChannelSet = Vec<Channel>;

/// Per-channel send timeout.
///
/// The outbox awaits sends in sequence, so one channel that accepts a call and
/// never answers would stall every pending notification, including the
/// recording ones the outbox exists to protect.
const DEFAULT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The built channel set, so a notification does not rebuild every channel on every send.
///
/// Invalidated on config reload via [`invalidate`].
static CACHE: OnceLock<RwLock<Option<ChannelSet>>> = OnceLock::new();

fn cache() -> &'static RwLock<Option<ChannelSet>> { CACHE.get_or_init(|| RwLock::new(None)) }

fn drop_cache() {
    if let Ok(mut guard) = cache().write() {
        *guard = None;
    }
}

/// Drop the cached channel set. Call on config reload and on every plugin
/// registry change (`PluginSystem::changes()`).
pub fn invalidate() {
    drop_cache();
    crate::rate_limit::reset();
}

/// The plugin host's notification seam, `None` while plugins are off.
static PLUGIN_NOTIFIER: RwLock<Option<Arc<dyn PluginNotifier>>> = RwLock::new(None);

/// Register (or clear) the plugin notifier. [`build`] appends one
/// `Channel::Plugin` per channel it declares, so the cached set is dropped.
pub fn set_plugin_notifier(notifier: Option<Arc<dyn PluginNotifier>>) {
    if let Ok(mut guard) = PLUGIN_NOTIFIER.write() {
        *guard = notifier;
    }
    drop_cache();
}

/// Every channel the current config declares, in a stable order.
///
/// Cached: repeated calls return the same channels, so per-channel state
/// (rate limiting, suppression) survives between notifications.
#[must_use]
pub fn channels(messaging: &MessagingConfig) -> ChannelSet {
    if let Ok(guard) = cache().read() {
        if let Some(set) = guard.as_ref() {
            return set.clone();
        }
    }
    let set = build(messaging);
    if let Ok(mut guard) = cache().write() {
        *guard = Some(set.clone());
    }
    set
}

/// Construct the channel set from config, bypassing the cache.
#[must_use]
pub fn build(messaging: &MessagingConfig) -> ChannelSet {
    let mut channels: ChannelSet = Vec::with_capacity(8);
    if let Some(cfg) = messaging.command.as_ref() {
        channels.push(Channel::Command(command::CommandChannel::new(cfg.clone())));
    }
    let notifier = PLUGIN_NOTIFIER.read().ok().and_then(|guard| guard.clone());
    if let Some(notifier) = notifier {
        channels.extend(plugin::channels_of(&notifier).into_iter().map(Channel::Plugin));
    }
    channels
}
