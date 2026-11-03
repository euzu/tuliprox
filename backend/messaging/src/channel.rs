//! The outbound channel abstraction.
//!
//! A channel is one [`NotificationChannel`] impl; the dispatcher never learns its name.
//! Today the only implementation is the plugin channel
//! ([`channels::plugin`](crate::channels::plugin)).
//!
//! Dispatch is static. [`NotificationChannel`] returns an opaque future
//! from `send` rather than a boxed one, which makes the trait deliberately
//! *not* object safe: there is no vtable and no allocation on the send
//! path. [`Channel`](crate::channels::Channel) enumerates the
//! implementations and every call monomorphizes.

use shared::model::notification::{EventId, Severity};
use std::future::Future;
use tuliprox_core::model::{ChannelRouting, NotificationEvent};

/// What happened to one delivery attempt.
///
/// The old `Option<bool>` could not tell "retry me" from "this URL is
/// malformed, retrying is pointless", so a typo'd webhook burned every
/// attempt before dead-lettering, and a `429` was retried straight back
/// into the rate limit it had just hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Landed. Never retry.
    Delivered,
    /// Transient. Retry after `after` if the provider named a delay,
    /// otherwise on the outbox's own backoff schedule.
    Retry { reason: String, after: Option<std::time::Duration> },
    /// Permanently undeliverable - malformed request, rejected credentials.
    /// Retrying cannot help, so dead-letter immediately.
    Permanent { reason: String },
    /// Nothing to do: the channel does not want this event. Not a failure,
    /// and must never be retried.
    Skipped,
}

impl Delivery {
    /// Transient failure with no provider-supplied delay.
    pub fn retry(reason: impl Into<String>) -> Self { Self::Retry { reason: reason.into(), after: None } }

    /// Transient failure honouring a provider `Retry-After`.
    pub fn retry_after(reason: impl Into<String>, after: std::time::Duration) -> Self {
        Self::Retry { reason: reason.into(), after: Some(after) }
    }

    pub fn permanent(reason: impl Into<String>) -> Self { Self::Permanent { reason: reason.into() } }

    #[must_use]
    pub fn is_delivered(&self) -> bool { matches!(self, Self::Delivered) }

    /// Should the outbox keep this channel pending?
    #[must_use]
    pub fn should_retry(&self) -> bool { matches!(self, Self::Retry { .. }) }
}

/// An event with its body ready for a channel.
#[derive(Debug, Clone)]
pub struct RenderedMessage<'a> {
    /// The event itself, for channels that build structured payloads.
    pub event: &'a NotificationEvent,
    /// The body to send: [`NotificationEvent::body`].
    pub body: String,
}

impl<'a> RenderedMessage<'a> {
    #[must_use]
    pub fn of(event: &'a NotificationEvent) -> Self { Self { event, body: event.body.clone() } }
}

/// One configured outbound channel.
pub trait NotificationChannel: Send + Sync {
    /// Stable wire id: outbox key and metric label. Must not change once
    /// released - the outbox persists it.
    fn id(&self) -> &'static str;

    /// This channel's routing rules - severity floor, quiet hours,
    /// suppression window and hourly ceiling.
    fn routing(&self) -> &ChannelRouting;

    /// Does this channel want this event at this severity?
    ///
    /// Default accepts everything; per-channel routing overrides it.
    fn wants(&self, _event: EventId, _severity: Severity) -> bool { true }

    /// Deliver. Must not panic and must not block - a slow provider has to
    /// surface as [`Delivery::Retry`], not a stalled worker.
    ///
    /// Returns an opaque future rather than a boxed one. That is what keeps
    /// the send path allocation-free, and it is why this trait is not object
    /// safe - implementations are reached through
    /// [`Channel`](crate::channels::Channel), never a trait object.
    fn send(&self, msg: &RenderedMessage<'_>) -> impl Future<Output = Delivery> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_and_permanent_are_never_retried() {
        assert!(!Delivery::Skipped.should_retry());
        assert!(!Delivery::permanent("nope").should_retry());
        assert!(!Delivery::Delivered.should_retry());
    }

    #[test]
    fn only_a_retry_is_kept_pending() {
        assert!(Delivery::retry("later").should_retry());
        assert!(Delivery::retry_after("later", std::time::Duration::from_secs(5)).should_retry());
    }
}
