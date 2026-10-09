//! The in-process event taxonomy.
//!
//! `EventMessage` used to live in `tuliprox-session`, which meant that
//! `metadata`, `dvr` and `processing` all had to depend on the *streaming
//! session runtime* just to name an event. A metadata refresh has nothing
//! to do with provider allocation, so the type lives here instead: `shared`
//! is the one crate every emitter already sees.
//!
//! The bus implementation (`EventManager`) stays in `session`, where the
//! stream-meter registry it feeds also lives.

/// A set of [`EventKind`]s, as one word.
///
/// `get_event_channel` handed every subscriber the firehose, and each one
/// filtered afterwards - after the broadcast channel had already cloned the
/// message for it. A subscriber that wants two kinds should not pay for the
/// other twelve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EventKindMask(u64);

#[cfg(test)]
mod tests;

mod kind;
mod message;
mod sink;
pub use kind::EventKind;
pub use message::{EventMessage, PlaylistUpdateSummary};
pub use sink::{EventSink, NoopSink};
