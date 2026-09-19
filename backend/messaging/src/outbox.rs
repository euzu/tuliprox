//! The notification outbox.
//!
//! This used to live in the recording supervisor and serve recordings only:
//! it was gated on the recording config and wrote
//! `recording_notification_outbox.json`. Everything else - playlist stats,
//! watch changes, disk alerts, provider account warnings - called
//! `send_message`, which fanned out and discarded every outcome with
//! `let _ =`. A transient 502 lost those permanently and logged one line.
//!
//! Now every notification goes through here: persisted before the first
//! attempt, retried per channel with capped exponential backoff, and
//! dead-lettered after `max_attempts`.
//!
//! Per-channel retry is what makes delivery at-most-once *per channel*: a
//! message that reached Telegram but not Discord is retried only against
//! Discord, so a retry can never duplicate a delivered message.

use crate::{channel::Delivery, configured_channels, send_event_to_channel};
use log::{debug, error, info};
use shared::model::{EventMessage, EventSink, NotificationDeadLetter};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{AppConfig, NotificationEvent, RecordingNotificationConfig};

/// Outbox file name under `storage_dir`.
const OUTBOX_FILE: &str = "notification_outbox.json";

/// The name this file had while the outbox was recording-only. Read once at
/// startup so pending recording notifications survive the upgrade instead
/// of being silently dropped.
const LEGACY_OUTBOX_FILE: &str = "recording_notification_outbox.json";

/// Delivery counters, surfaced on the status endpoint.
#[derive(Debug, Default)]
pub struct NotificationHealth {
    pub outbox_depth: AtomicI64,
    pub dead_lettered: AtomicU64,
    pub delivered: AtomicU64,
    pub failed: AtomicU64,
    pub last_drain_at: AtomicI64,
}

static HEALTH: OnceLock<NotificationHealth> = OnceLock::new();

/// Process-wide delivery counters.
pub fn health() -> &'static NotificationHealth { HEALTH.get_or_init(NotificationHealth::default) }

/// One queued notification.
///
/// `pending` holds the channels that have *not* accepted it yet, which is
/// what keeps a retry from re-delivering to a channel that already took it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OutboxEntry {
    id: u64,
    event: NotificationEvent,
    /// Stable channel ids. Strings rather than an enum so an outbox written
    /// by a build that knows a newer channel still deserializes here.
    #[serde(default)]
    pending: Vec<String>,
    attempts: u32,
    enqueued_at: i64,
    next_attempt_at: i64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct OutboxFile {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    entries: Vec<OutboxEntry>,
}

/// Sender side of the outbox. Cloneable and cheap; `enqueue` never blocks.
#[derive(Debug, Clone)]
pub struct NotificationOutbox {
    sender: mpsc::Sender<NotificationEvent>,
}

impl NotificationOutbox {
    /// Hand a notification to the outbox worker.
    ///
    /// Never blocks and never awaits: a recording, or a playlist update,
    /// must not stall because a messaging provider is slow. Returns the
    /// event back when the outbox cannot take it so the caller can fall
    /// back to a direct best-effort send.
    pub fn enqueue(&self, event: NotificationEvent) -> Option<NotificationEvent> {
        match self.sender.try_send(event) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(event)) => {
                error!("Notification outbox is full; falling back to a direct send");
                Some(event)
            }
            Err(mpsc::error::TrySendError::Closed(event)) => Some(event),
        }
    }
}

static OUTBOX: OnceLock<NotificationOutbox> = OnceLock::new();

/// The installed outbox, if the worker has started. Callers that find
/// `None` (unit tests, early startup) fall back to a direct send.
pub fn notification_outbox() -> Option<&'static NotificationOutbox> { OUTBOX.get() }

/// Start the outbox worker. Idempotent: only the first call installs one.
pub fn spawn_notification_outbox<E: EventSink + 'static>(
    app_config: &Arc<AppConfig>,
    http_client: reqwest::Client,
    config: RecordingNotificationConfig,
    cancel_token: &CancellationToken,
    events: E,
) {
    let (sender, receiver) = mpsc::channel(config.outbox_buffer.max(1));
    if OUTBOX.set(NotificationOutbox { sender }).is_err() {
        debug!("Notification outbox already installed");
        return;
    }
    let app_config = Arc::clone(app_config);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        run(app_config, http_client, config, receiver, cancel_token, events).await;
    });
}

fn now_ts() -> i64 { chrono::Utc::now().timestamp() }

fn outbox_path(app_config: &Arc<AppConfig>) -> PathBuf {
    PathBuf::from(app_config.config.load().storage_dir.as_str()).join(OUTBOX_FILE)
}

fn legacy_outbox_path(app_config: &Arc<AppConfig>) -> PathBuf {
    PathBuf::from(app_config.config.load().storage_dir.as_str()).join(LEGACY_OUTBOX_FILE)
}

async fn run<E: EventSink>(
    app_config: Arc<AppConfig>,
    http_client: reqwest::Client,
    config: RecordingNotificationConfig,
    mut receiver: mpsc::Receiver<NotificationEvent>,
    cancel_token: CancellationToken,
    events: E,
) {
    let path = outbox_path(&app_config);
    let mut file = load(&path, &legacy_outbox_path(&app_config)).await;
    if !file.entries.is_empty() {
        info!("Notification outbox recovered {} undelivered notification(s)", file.entries.len());
    }
    // Once every sender is gone `recv()` completes instantly and forever, so
    // the loop must stop polling it or it would spin.
    let mut senders_gone = false;
    loop {
        let sleep_for = next_wakeup(&file, now_ts());
        let mut inbound = None;
        if senders_gone {
            if file.entries.is_empty() {
                break;
            }
            tokio::select! {
                () = cancel_token.cancelled() => break,
                () = tokio::time::sleep(sleep_for) => {}
            }
        } else {
            tokio::select! {
                () = cancel_token.cancelled() => break,
                message = receiver.recv() => {
                    match message {
                        Some(event) => inbound = Some(event),
                        None => senders_gone = true,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
            }
        }
        if let Some(event) = inbound {
            admit(&mut file, event);
            // Drain whatever else is queued so a burst costs one persist,
            // not one per event.
            while let Ok(event) = receiver.try_recv() {
                admit(&mut file, event);
            }
            persist(&path, &file).await;
        }
        if drain_due(&app_config, &http_client, &config, &mut file, &events).await {
            persist(&path, &file).await;
        }
        health().outbox_depth.store(i64::try_from(file.entries.len()).unwrap_or(i64::MAX), Ordering::Relaxed);
    }
    debug!("Notification outbox stopped");
}

/// How long to sleep before the next delivery attempt is due.
fn next_wakeup(file: &OutboxFile, now: i64) -> Duration {
    file.entries
        .iter()
        .map(|entry| entry.next_attempt_at.saturating_sub(now).max(0))
        .min()
        .map_or(Duration::from_hours(1), |secs| Duration::from_secs(u64::try_from(secs).unwrap_or(0)))
}

/// Accept a new notification into the outbox.
fn admit(file: &mut OutboxFile, event: NotificationEvent) {
    let now = now_ts();
    file.next_id = file.next_id.wrapping_add(1);
    file.entries.push(OutboxEntry {
        id: file.next_id,
        event,
        // Resolved at attempt time, not here, so a config reload between
        // enqueue and delivery is honoured.
        pending: Vec::new(),
        attempts: 0,
        enqueued_at: now,
        next_attempt_at: now,
    });
}

/// Attempt every due entry. Returns `true` when the outbox changed.
async fn drain_due<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    config: &RecordingNotificationConfig,
    file: &mut OutboxFile,
    events: &E,
) -> bool {
    let now = now_ts();
    if !file.entries.iter().any(|entry| entry.next_attempt_at <= now) {
        return false;
    }
    health().last_drain_at.store(now, Ordering::Relaxed);
    let mut changed = false;
    let mut keep: Vec<OutboxEntry> = Vec::with_capacity(file.entries.len());
    for mut entry in std::mem::take(&mut file.entries) {
        if entry.next_attempt_at > now {
            keep.push(entry);
            continue;
        }
        changed = true;
        if entry.pending.is_empty() && entry.attempts == 0 {
            entry.pending = configured_channels(app_config, entry.event.id);
        }
        if entry.pending.is_empty() {
            // No channel wants this event. Nothing to deliver, nothing to
            // retry.
            continue;
        }
        // Quiet hours defer, they do not drop: hold the whole entry until
        // the window closes rather than losing an overnight outage.
        if let Some(defer) = quiet_hours_defer_for(app_config, &entry.pending) {
            entry.next_attempt_at = now.saturating_add(i64::try_from(defer.as_secs()).unwrap_or(0).max(60));
            debug!("Notification {} deferred {}s for quiet hours", entry.event.id, defer.as_secs());
            keep.push(entry);
            continue;
        }
        entry.attempts = entry.attempts.saturating_add(1);

        // Concurrent, not sequential: one webhook host that accepts the
        // connection and never answers must not stall every other channel
        // or every other pending notification.
        let attempts = std::mem::take(&mut entry.pending).into_iter().map(|channel_id| {
            let event = &entry.event;
            async move {
                let outcome = send_event_to_channel(app_config, client, event, &channel_id).await;
                (channel_id, outcome)
            }
        });
        let results = futures::future::join_all(attempts).await;

        let mut still_pending = Vec::new();
        let mut provider_delay: Option<Duration> = None;
        for (channel_id, outcome) in results {
            match outcome {
                Delivery::Delivered => {
                    health().delivered.fetch_add(1, Ordering::Relaxed);
                }
                // The channel is no longer configured or does not want this
                // event: not a failure, and must not be retried.
                Delivery::Skipped => {}
                Delivery::Permanent { reason } => {
                    // Retrying cannot help. Dead-letter this channel now
                    // instead of burning every remaining attempt on it.
                    health().failed.fetch_add(1, Ordering::Relaxed);
                    health().dead_lettered.fetch_add(1, Ordering::Relaxed);
                    error!(
                        target: "notification::audit",
                        "notification_permanently_rejected: event={} channel={channel_id} reason={reason}",
                        entry.event.id
                    );
                }
                Delivery::Retry { reason, after } => {
                    health().failed.fetch_add(1, Ordering::Relaxed);
                    debug!("Notification {} to {channel_id} will be retried: {reason}", entry.event.id);
                    if let Some(after) = after {
                        provider_delay = Some(provider_delay.map_or(after, |current: Duration| current.max(after)));
                    }
                    still_pending.push(channel_id);
                }
            }
        }

        if still_pending.is_empty() {
            debug!("Notification {} settled after {} attempt(s)", entry.id, entry.attempts);
            continue;
        }
        entry.pending = still_pending;
        if entry.attempts >= config.max_attempts {
            health().dead_lettered.fetch_add(1, Ordering::Relaxed);
            error!(
                target: "notification::audit",
                "notification_dead_lettered: event={} attempts={} channels={:?} enqueued_at={}",
                entry.event.id, entry.attempts, entry.pending, entry.enqueued_at
            );
            // The bus, not the outbox. Enqueueing a "delivery failed" notice
            // against the channels that just failed to deliver is how this
            // loops; the notification bridge drops this kind for that reason.
            events.emit(EventMessage::NotificationDeadLettered(NotificationDeadLetter::new(
                entry.event.id,
                entry.attempts,
                entry.pending.clone(),
                entry.enqueued_at,
            )));
            continue;
        }
        // A provider that named a `Retry-After` wins over our own backoff:
        // retrying earlier just walks back into the same rate limit.
        let backoff = backoff_secs(config, entry.attempts);
        let delay = provider_delay
            .map_or(backoff, |after| i64::try_from(after.as_secs()).unwrap_or(backoff).max(backoff.min(1)));
        entry.next_attempt_at = now.saturating_add(delay);
        keep.push(entry);
    }
    file.entries = keep;
    changed
}

/// Capped exponential backoff: `initial * 2^(attempts-1)`, clamped.
/// The shortest quiet-hours deferral across the still-pending channels.
///
/// `None` when at least one pending channel is awake, so a notification is
/// never held back by a channel that is not the one blocking it.
fn quiet_hours_defer_for(app_config: &Arc<AppConfig>, pending: &[String]) -> Option<Duration> {
    let cfg = app_config.config.load();
    let messaging = cfg.messaging.as_ref()?;
    let set = crate::channels::channels(app_config, messaging);
    let mut shortest: Option<Duration> = None;
    for channel_id in pending {
        let channel = set.iter().find(|c| c.id() == channel_id)?;
        let defer = crate::quiet_hours_defer(channel)?;
        shortest = Some(shortest.map_or(defer, |current: Duration| current.min(defer)));
    }
    shortest
}

fn backoff_secs(config: &RecordingNotificationConfig, attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(16);
    let delay = config.backoff_initial_secs.saturating_mul(1u64 << shift).min(config.backoff_max_secs);
    i64::try_from(delay).unwrap_or(i64::MAX)
}

async fn read_outbox_file(path: &Path) -> Option<OutboxFile> {
    match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<OutboxFile>(&bytes) {
            Ok(file) => Some(file),
            Err(err) => {
                // A corrupt outbox must not stop the server. Losing
                // undelivered notifications is the lesser failure.
                error!("Notification outbox at {} is unreadable, ignoring it: {err}", path.display());
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            error!("Could not read the notification outbox at {}: {err}", path.display());
            None
        }
    }
}

/// Load the outbox, adopting anything left in the pre-rename file.
async fn load(path: &Path, legacy_path: &Path) -> OutboxFile {
    let mut file = read_outbox_file(path).await.unwrap_or_default();
    if let Some(legacy) = read_outbox_file(legacy_path).await {
        if !legacy.entries.is_empty() {
            info!(
                "Adopting {} notification(s) from the pre-rename outbox at {}",
                legacy.entries.len(),
                legacy_path.display()
            );
            file.next_id = file.next_id.max(legacy.next_id);
            file.entries.extend(legacy.entries);
        }
        // Remove it either way, so the adoption happens exactly once.
        if let Err(err) = tokio::fs::remove_file(legacy_path).await {
            error!("Could not remove the pre-rename outbox at {}: {err}", legacy_path.display());
        }
    }
    file
}

async fn persist(path: &Path, file: &OutboxFile) {
    match serde_json::to_vec_pretty(file) {
        Ok(bytes) => {
            if let Err(err) = tuliprox_core::utils::atomic_json_store::write_json_atomic(path, &bytes).await {
                error!("Could not persist the notification outbox: {err}");
            }
        }
        Err(err) => error!("Could not serialize the notification outbox: {err}"),
    }
}
