//! Outbound notification delivery.
//!
//! Renders notification events and sends them over the configured channels.
//! The playlist pipeline, the recording supervisor and the API all notify
//! through here, and none of them are named by this crate.
//!
//! Channels are an open set: see [`channel::NotificationChannel`]. Event
//! kinds are an open set too - see [`shared::model::notification`]. Adding
//! either no longer means editing a match in this file.

// The notification outbox spawns a generic async worker whose `Send` proof makes the
// trait solver recurse deeply; the default recursion limit overflows on newer
// nightlies (recursion_depth_exceeding_limit). Raise it rather than restructure the
// worker's generic bounds.
#![recursion_limit = "256"]

pub mod channel;
pub mod channels;
pub mod outbox;
pub mod rate_limit;

use crate::{
    channel::{Delivery, RenderedMessage},
    channels::{Channel, ChannelSet},
};
use log::{debug, error};
use shared::model::{
    notification::{EventId, Severity},
    MsgKind,
};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, MessageContent, MessagingConfig, NotificationEvent};

/// Is this event subscribed by `notify_on`?
fn is_enabled(id: EventId, cfg: &MessagingConfig) -> bool { cfg.subscription().matches(id) }

/// The channels currently configured for `id`, by stable channel id.
///
/// The outbox persists these strings, so a channel added in a later release
/// no longer makes an older build reject the whole outbox file.
pub fn configured_channels(app_config: &Arc<AppConfig>, id: EventId) -> Vec<String> {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Vec::new();
    };
    if !is_enabled(id, messaging) {
        return Vec::new();
    }
    channels::channels()
        .into_iter()
        .filter(|channel| channel.wants(id, severity_of(id)))
        .map(|channel| channel.id().to_string())
        .collect()
}

fn severity_of(id: EventId) -> Severity { shared::model::notification::registry::default_severity(id) }

/// Send `event` to exactly one channel, by its stable id.
///
/// This is the entry point the outbox worker drives: it retries per
/// channel, so a message that reached one channel but not another is re-sent
/// only to the other.
pub async fn send_event_to_channel(
    app_config: &Arc<AppConfig>,
    event: &NotificationEvent,
    channel_id: &str,
) -> Delivery {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return Delivery::Skipped;
    };
    if !is_enabled(event.id, messaging) {
        return Delivery::Skipped;
    }
    let Some(channel) = channels::channels().into_iter().find(|c| c.id() == channel_id) else {
        // The channel was removed from the config between enqueue and
        // delivery. Nothing to do, and nothing to retry.
        return Delivery::Skipped;
    };
    if !channel.wants(event.id, event.severity) {
        return Delivery::Skipped;
    }
    let routing = channel.routing();
    if let Some(reason) =
        rate_limit::admit(channel.id(), event.dedup_key.as_deref(), routing.dedup_window, routing.max_per_hour)
    {
        return suppression_outcome(&channel, event, reason);
    }
    channel.send(&RenderedMessage::of(event)).await
}

/// Turn a suppression decision into a delivery outcome.
///
/// A rate-limit ceiling reports itself once so the resulting silence is
/// distinguishable from a notifier that has died.
fn suppression_outcome(channel: &Channel, event: &NotificationEvent, reason: rate_limit::Suppression) -> Delivery {
    match reason {
        rate_limit::Suppression::Duplicate => {
            debug!("Notification {} suppressed on {} as a duplicate", event.id, channel.id());
            Delivery::Skipped
        }
        rate_limit::Suppression::RateLimited => Delivery::Skipped,
        rate_limit::Suppression::RateLimitReached => {
            error!(
                target: "notification::audit",
                "notification_rate_limit_reached: channel={} event={} - further notifications suppressed this hour",
                channel.id(), event.id
            );
            Delivery::Skipped
        }
    }
}

/// Minutes since local midnight, for quiet-hours evaluation.
fn local_minutes_now() -> u16 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(0)
}

/// How long delivery to `channel` should be deferred for quiet hours.
///
/// Quiet hours defer rather than drop: an overnight outage nobody hears
/// about afterwards is worse than one that arrives late.
#[must_use]
pub fn quiet_hours_defer(channel: &Channel) -> Option<std::time::Duration> {
    let window = channel.routing().quiet_hours?;
    let minutes = local_minutes_now();
    if !window.contains(minutes) {
        return None;
    }
    Some(std::time::Duration::from_secs(u64::from(window.minutes_until_end(minutes)) * 60))
}

/// Notify.
///
/// Hands the event to the durable outbox where one is installed, so a
/// transient provider error is retried instead of being logged and lost -
/// which is what happened to every non-recording notification before the
/// outbox was promoted out of the recording supervisor. Falls back to a
/// direct best-effort send when the outbox is absent (unit tests, early
/// startup) or full.
pub async fn send_event(app_config: &Arc<AppConfig>, event: NotificationEvent) {
    let cfg = app_config.config.load();
    let Some(messaging) = cfg.messaging.as_ref() else {
        return;
    };
    if !is_enabled(event.id, messaging) {
        return;
    }
    let event = match outbox::notification_outbox() {
        Some(outbox) => match outbox.enqueue(event) {
            None => return,
            Some(rejected) => rejected,
        },
        None => event,
    };
    let set: ChannelSet = channels::channels();
    dispatch(&event, &set).await;
}

/// Deliver to every channel in `set` at once.
///
/// Concurrent rather than sequential: one slow provider must not delay the
/// others.
async fn dispatch(event: &NotificationEvent, set: &ChannelSet) {
    let sends = set.iter().filter(|channel| channel.wants(event.id, event.severity)).map(|channel| async move {
        let routing = channel.routing();
        if let Some(reason) =
            rate_limit::admit(channel.id(), event.dedup_key.as_deref(), routing.dedup_window, routing.max_per_hour)
        {
            return (channel.id(), suppression_outcome(channel, event, reason));
        }
        let outcome = channel.send(&RenderedMessage::of(event)).await;
        (channel.id(), outcome)
    });
    for (id, outcome) in futures::future::join_all(sends).await {
        match outcome {
            Delivery::Delivered | Delivery::Skipped => debug!("Notification {} handled by {id}", event.id),
            Delivery::Retry { reason, .. } => error!("Notification {} to {id} failed: {reason}", event.id),
            Delivery::Permanent { reason } => {
                error!("Notification {} to {id} permanently rejected: {reason}", event.id);
            }
        }
    }
}

/// Send a legacy [`MessageContent`], lifted into the envelope.
///
/// Kept so the existing emitters need no change.
pub async fn send_message(app_config: &Arc<AppConfig>, content: MessageContent) {
    send_event(app_config, NotificationEvent::from_content(&content)).await;
}

/// Legacy shim: the channels configured for a [`MsgKind`].
pub fn configured_channels_for_kind(app_config: &Arc<AppConfig>, kind: MsgKind) -> Vec<String> {
    match EventId::from_wire(kind.wire_name()) {
        Some(id) => configured_channels(app_config, id),
        None => Vec::new(),
    }
}
/// One channel's result from a test send.
pub struct TestOutcome {
    pub channel: String,
    /// `delivered`, `skipped`, `retry`, `permanent`, or `preview`.
    pub outcome: String,
    pub reason: Option<String>,
    /// Exactly what the channel was asked to send.
    pub rendered: String,
}

/// A representative event for `id`, for testing a channel.
#[must_use]
pub fn test_event(id: EventId) -> NotificationEvent {
    let description = shared::model::notification::registry::describe(id)
        .map_or("Test notification", |descriptor| descriptor.description);
    NotificationEvent::new(
        id,
        format!("Test: {id}"),
        format!("{description}\n\nThis is a test notification from tuliprox."),
    )
    .with_fields(&serde_json::json!({ "test": true }))
}

/// Show `event` for each requested channel, and send unless `preview`.
///
/// Bypasses `notify_on` and the suppression window deliberately: the
/// operator asked for this one explicitly, and a test that silently does
/// nothing because of a dedup window would be worse than useless.
pub async fn render_and_send_test(
    app_config: &Arc<AppConfig>,
    event: &NotificationEvent,
    only_channel: Option<&str>,
    preview: bool,
) -> Vec<TestOutcome> {
    if app_config.config.load().messaging.is_none() {
        return Vec::new();
    }
    let set = channels::channels();
    let mut results = Vec::with_capacity(set.len());
    for channel in set.iter().filter(|c| only_channel.is_none_or(|want| c.id() == want)) {
        let msg = RenderedMessage::of(event);
        if preview {
            results.push(TestOutcome {
                channel: channel.id().to_string(),
                outcome: "preview".to_string(),
                reason: None,
                rendered: msg.body.clone(),
            });
            continue;
        }
        let (outcome, reason) = match channel.send(&msg).await {
            Delivery::Delivered => ("delivered".to_string(), None),
            Delivery::Skipped => ("skipped".to_string(), None),
            Delivery::Retry { reason, .. } => ("retry".to_string(), Some(reason)),
            Delivery::Permanent { reason } => ("permanent".to_string(), Some(reason)),
        };
        results.push(TestOutcome { channel: channel.id().to_string(), outcome, reason, rendered: msg.body.clone() });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_alert_kind_is_msg_kind_disk_alert() {
        let alert = shared::model::DiskAlert {
            level: shared::model::DiskAlertLevel::Critical,
            total_bytes: 1_000,
            free_bytes: 50,
            used_bytes: 950,
            percent: 95.0,
        };
        let content = MessageContent::DiskAlert(alert);
        assert_eq!(content.kind(), shared::model::MsgKind::DiskAlert);
    }

    #[test]
    fn a_disk_alert_body_names_the_level_and_the_percent() {
        let alert = shared::model::DiskAlert {
            level: shared::model::DiskAlertLevel::Warn,
            total_bytes: 1_000_000_000,
            free_bytes: 150_000_000,
            used_bytes: 850_000_000,
            percent: 85.0,
        };
        let event = NotificationEvent::from_content(&MessageContent::DiskAlert(alert));
        assert!(event.body.contains("warning"), "the body must mention the alert level, got {}", event.body);
        assert!(event.body.contains("85.0%"), "the body must include the percent, got {}", event.body);
    }

    #[test]
    fn a_rendered_message_carries_the_event_body() {
        let event = test_event(shared::model::notification::registry::SYSTEM_INFO);
        assert_eq!(RenderedMessage::of(&event).body, event.body);
    }
}
