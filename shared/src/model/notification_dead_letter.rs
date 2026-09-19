use crate::model::notification::EventId;
use serde::{Deserialize, Serialize};

/// A notification that ran out of attempts and was dropped.
///
/// # Why this does not become a notification
///
/// `notification.dead_lettered` is registered, and this record reaches the
/// bus - but the notification bridge deliberately does not turn it into a
/// notification. Routing "delivery failed" through the delivery system is
/// the loop its registry entry warns about: the notice would be enqueued
/// into the same outbox, against the same channels that just failed, and
/// dead-letter again.
///
/// Plugins, the status endpoint and any other bus subscriber see it. What an
/// operator sees is the `notification::audit` log line and the
/// `dead_lettered` counter, neither of which depends on the path that broke.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationDeadLetter {
    /// The event whose notification was lost.
    pub event: EventId,
    /// How many delivery attempts were made before giving up.
    pub attempts: u32,
    /// The channels that never accepted it.
    pub channels: Vec<String>,
    /// When it first entered the outbox, as a Unix timestamp.
    pub enqueued_at: i64,
}

impl NotificationDeadLetter {
    #[must_use]
    pub fn new(event: EventId, attempts: u32, channels: Vec<String>, enqueued_at: i64) -> Self {
        Self { event, attempts, channels, enqueued_at }
    }
}
