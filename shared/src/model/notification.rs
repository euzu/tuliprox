//! Open-world notification event identity.
//!
//! [`MsgKind`](crate::model::MsgKind) is a closed enum: adding one event
//! kind meant editing eight sites across three crates, and two of those
//! sites failed silently rather than at compile time. This module is the
//! replacement extension point.
//!
//! An event is identified by a dotted `domain.event` string. Subscriptions
//! are glob patterns. Adding an event is one [`EventId`] const plus one
//! emit call - no match arms, no template-context field, no discovery
//! array to keep in sync.
//!
//! # Backward compatibility
//!
//! The eight legacy `MsgKind` wire names (`info`, `stats`, `disk_alert`,
//! ...) stay valid wherever they were accepted before: as `notify_on`
//! entries and as template filenames. [`EventId::from_wire`] resolves them
//! through [`LEGACY_ALIASES`], so an existing `config.yml` keeps working
//! untouched.

/// Stable dotted identity of a notification event.
///
/// The inner string is the wire form: it appears in `notify_on` patterns,
/// in the outbox file, in template filenames and in metric labels, so it
/// must stay stable across releases once published.
///
/// Backed by `&'static str` because every event is declared as a const in
/// [`registry`]. Plugin-registered events would need an owned variant;
/// that is deliberately deferred until the plugin host exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(&'static str);

/// The known event ids.
///
/// Adding an event is a const here plus an entry in [`ALL`] - and then one
/// emit call at the site that knows the event happened. Nothing else in
/// the notification path needs to change.
pub mod registry;

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// One `notify_on` entry.
///
/// Grammar, deliberately small enough to explain in a config comment:
///
/// | pattern                  | matches                                     |
/// |--------------------------|---------------------------------------------|
/// | `*`                      | every event                                 |
/// | `recording.*`            | every event under `recording`                |
/// | `recording.completed`    | that event only                             |
/// | `provider.*.expired`     | one wildcard segment                        |
/// | `!recording.started`     | excludes, whatever else matched             |
///
/// A leading `!` negates. A subscription matches when at least one positive
/// pattern matches and no negative pattern does, so `["*", "!system.info"]`
/// reads the way it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPattern {
    segments: Vec<Segment>,
    /// `true` when the pattern is a `!` exclusion.
    pub negated: bool,
    raw: String,
}

/// A parsed `notify_on` list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventSubscription {
    patterns: Vec<EventPattern>,
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Quiet hours
// ---------------------------------------------------------------------------

/// A local-time window during which a channel stays silent.
///
/// Notifications landing inside the window are *deferred* by the outbox,
/// never dropped: an overnight outage that nobody is told about afterwards
/// is worse than one that arrives late.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    /// Minutes since local midnight.
    start_min: u16,
    end_min: u16,
}

#[cfg(test)]
mod quiet_hours_tests;

mod identity;
mod quiet_hours;
mod subscription;
pub use identity::{EventDescriptor, Severity, LEGACY_ALIASES};
use subscription::Segment;
