//! Notification outbox.
//!
//! The recorder used to `tokio::spawn(send_message(..))` straight from
//! its persist path, so a transient provider error lost the
//! notification permanently and a crash between the persist and the
//! spawn lost it too. The worker in this module owns delivery instead:
//! entries are persisted to
//! `storage_dir/recording_notification_outbox.json` before the first
//! attempt, retried per channel with capped exponential backoff, and
//! dead-lettered with a log line after `max_attempts`.
//!
//! Per-channel retry is what makes the retry at-most-once *per channel*:
//! a message that reached Telegram but not Discord is retried only
//! against Discord, so a retry can never duplicate a delivered message.

use super::{
    super::recording_ctx::RecordingCtx,
    health::{supervisor_health, SupervisorHealth},
    now_ts, recording_config,
};
use log::{debug, error, info};
use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc, OnceLock},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{MessageContent, RecordingNotificationConfig};
use tuliprox_messaging::{configured_channels, send_message_to_channel, MessagingChannel};

/// Outbox file name under `storage_dir`.
const NOTIFICATION_OUTBOX_FILE: &str = "recording_notification_outbox.json";

/// One queued notification, per channel.
///
/// `pending` is what makes the retry at-most-once *per channel*: a
/// message that reached Telegram but not Discord is retried only against
/// Discord, so a retry can never duplicate a delivered message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OutboxEntry {
    id: u64,
    content: MessageContent,
    pending: Vec<MessagingChannel>,
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

/// Sender side of the outbox. Cloneable and cheap; `enqueue` never
/// blocks the caller.
#[derive(Debug, Clone)]
pub struct NotificationOutbox {
    sender: mpsc::Sender<MessageContent>,
}

impl NotificationOutbox {
    /// Hand a notification to the outbox worker.
    ///
    /// Never blocks and never awaits: a recording must not stall because
    /// a messaging provider is slow. Returns the notification back when
    /// the outbox cannot take it (bounded channel full, or the worker has
    /// shut down) so the caller can decide — the download worker falls
    /// back to a direct best-effort send.
    pub fn enqueue(&self, content: MessageContent) -> Option<MessageContent> {
        match self.sender.try_send(content) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(content)) => {
                error!("Recording notification outbox is full; falling back to a direct send");
                Some(content)
            }
            Err(mpsc::error::TrySendError::Closed(content)) => Some(content),
        }
    }
}

/// The process-wide outbox handle, installed by
/// [`spawn_notification_outbox`].
static OUTBOX: OnceLock<NotificationOutbox> = OnceLock::new();

/// The installed outbox, if the supervisor has started. Callers that
/// find `None` (unit tests, early startup) fall back to a direct send.
pub fn notification_outbox() -> Option<&'static NotificationOutbox> { OUTBOX.get() }

/// Start the notification outbox worker.
///
/// Idempotent: calling it twice installs only the first worker.
pub fn spawn_notification_outbox(ctx: &RecordingCtx, cancel_token: &CancellationToken) {
    let config = notification_config(ctx);
    let (sender, receiver) = mpsc::channel(config.outbox_buffer);
    if OUTBOX.set(NotificationOutbox { sender }).is_err() {
        debug!("Recording notification outbox already installed");
        return;
    }
    let ctx = ctx.clone();
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        run_notification_outbox(ctx, receiver, cancel_token).await;
    });
}

fn notification_config(ctx: &RecordingCtx) -> RecordingNotificationConfig {
    recording_config(&ctx.app_config).map_or_else(RecordingNotificationConfig::default, |cfg| cfg.notifications)
}

fn outbox_path(ctx: &RecordingCtx) -> PathBuf {
    PathBuf::from(ctx.app_config.config.load().storage_dir.as_str()).join(NOTIFICATION_OUTBOX_FILE)
}

async fn run_notification_outbox(
    ctx: RecordingCtx,
    mut receiver: mpsc::Receiver<MessageContent>,
    cancel_token: CancellationToken,
) {
    let path = outbox_path(&ctx);
    let mut file = load_outbox(&path).await;
    if !file.entries.is_empty() {
        info!("Recording notification outbox recovered {} undelivered notification(s)", file.entries.len());
    }
    // Once every sender is gone `recv()` completes instantly and forever,
    // so the loop must stop polling it or it would spin.
    let mut senders_gone = false;
    loop {
        let sleep_for = next_wakeup(&file, now_ts());
        let mut received_content = None;
        if senders_gone {
            // Nothing new can arrive, but the existing backlog still
            // deserves its remaining attempts.
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
                        Some(content) => received_content = Some(content),
                        None => senders_gone = true,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
            }
        }
        if let Some(content) = received_content {
            admit(&mut file, content);
            // Drain whatever else is already queued so a burst of
            // completions costs one persist, not one per event.
            while let Ok(content) = receiver.try_recv() {
                admit(&mut file, content);
            }
            persist_outbox(&path, &file).await;
        }
        if drain_due_entries(&ctx, &mut file).await {
            persist_outbox(&path, &file).await;
        }
        supervisor_health()
            .notification_outbox_depth
            .store(super::super::recording_math::sat_i64_from_u64(file.entries.len() as u64), Ordering::Relaxed);
    }
    debug!("Recording notification outbox stopped");
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
fn admit(file: &mut OutboxFile, content: MessageContent) {
    let now = now_ts();
    file.next_id = file.next_id.wrapping_add(1);
    file.entries.push(OutboxEntry {
        id: file.next_id,
        content,
        // Resolved at attempt time, not here: the channel set is read from
        // the live config so a reload between enqueue and delivery is
        // honoured.
        pending: Vec::new(),
        attempts: 0,
        enqueued_at: now,
        next_attempt_at: now,
    });
}

/// Attempt every due entry. Returns `true` when the outbox changed and
/// has to be persisted.
async fn drain_due_entries(ctx: &RecordingCtx, file: &mut OutboxFile) -> bool {
    let now = now_ts();
    if !file.entries.iter().any(|entry| entry.next_attempt_at <= now) {
        return false;
    }
    SupervisorHealth::stamp(&supervisor_health().notification_last_drain, now);
    let config = notification_config(ctx);
    let client = ctx.http_client.load_full();
    let app_config = Arc::clone(&ctx.app_config);
    let mut changed = false;
    let mut keep: Vec<OutboxEntry> = Vec::with_capacity(file.entries.len());
    for mut entry in std::mem::take(&mut file.entries) {
        if entry.next_attempt_at > now {
            keep.push(entry);
            continue;
        }
        changed = true;
        if entry.pending.is_empty() && entry.attempts == 0 {
            entry.pending = configured_channels(&app_config, entry.content.kind());
        }
        if entry.pending.is_empty() {
            // No channel wants this message kind. Nothing to deliver and
            // nothing to retry.
            continue;
        }
        entry.attempts = entry.attempts.saturating_add(1);
        let mut still_pending = Vec::new();
        for channel in std::mem::take(&mut entry.pending) {
            match send_message_to_channel(&app_config, &client, &entry.content, channel).await {
                // Delivered, or the channel is no longer configured: either
                // way it must not be retried.
                Some(true) | None => {}
                Some(false) => still_pending.push(channel),
            }
        }
        if still_pending.is_empty() {
            debug!("Recording notification {} delivered after {} attempt(s)", entry.id, entry.attempts);
            continue;
        }
        entry.pending = still_pending;
        if entry.attempts >= config.max_attempts {
            supervisor_health().notification_dead_lettered.fetch_add(1, Ordering::Relaxed);
            error!(
                target: "recording::audit",
                "recording_notification_dead_lettered: kind={:?} attempts={} channels={:?} enqueued_at={}",
                entry.content.kind(), entry.attempts, entry.pending, entry.enqueued_at
            );
            continue;
        }
        entry.next_attempt_at = now.saturating_add(backoff_secs(&config, entry.attempts));
        keep.push(entry);
    }
    file.entries = keep;
    changed
}

/// Capped exponential backoff: `initial * 2^(attempts-1)`, clamped to
/// `backoff_max_secs`.
fn backoff_secs(config: &RecordingNotificationConfig, attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(16);
    let delay = config.backoff_initial_secs.saturating_mul(1u64 << shift).min(config.backoff_max_secs);
    super::super::recording_math::sat_i64_from_u64(delay)
}

async fn load_outbox(path: &std::path::Path) -> OutboxFile {
    match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<OutboxFile>(&bytes) {
            Ok(file) => file,
            Err(err) => {
                // A corrupt outbox must not stop the server. Losing
                // undelivered notifications is the lesser failure.
                error!("Recording notification outbox at {} is unreadable, starting empty: {err}", path.display());
                OutboxFile::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => OutboxFile::default(),
        Err(err) => {
            error!("Could not read the recording notification outbox at {}: {err}", path.display());
            OutboxFile::default()
        }
    }
}

async fn persist_outbox(path: &std::path::Path, file: &OutboxFile) {
    match serde_json::to_vec_pretty(file) {
        Ok(bytes) => {
            if let Err(err) = tuliprox_core::utils::atomic_json_store::write_json_atomic(path, &bytes).await {
                error!("Could not persist the recording notification outbox: {err}");
            }
        }
        Err(err) => error!("Could not serialize the recording notification outbox: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::MsgKind;
    use tuliprox_core::model::RecordingLifecycleMessage;

    fn config() -> RecordingNotificationConfig {
        RecordingNotificationConfig {
            outbox_buffer: 8,
            max_attempts: 4,
            backoff_initial_secs: 5,
            backoff_max_secs: 100,
        }
    }

    fn lifecycle() -> MessageContent {
        MessageContent::RecordingLifecycle(RecordingLifecycleMessage {
            event: MsgKind::RecordingCompleted,
            programme_title: Some("Programme".into()),
            channel: Some("Channel".into()),
            effective_start: Some(1_700_000_000),
            effective_end: Some(1_700_003_600),
            visibility: Some("shared".into()),
            output_filename: Some("programme.ts".into()),
            failure_reason: None,
        })
    }

    #[test]
    fn backoff_grows_then_saturates_at_the_configured_ceiling() {
        let config = config();
        assert_eq!(backoff_secs(&config, 1), 5);
        assert_eq!(backoff_secs(&config, 2), 10);
        assert_eq!(backoff_secs(&config, 3), 20);
        assert_eq!(backoff_secs(&config, 4), 40);
        assert_eq!(backoff_secs(&config, 5), 80);
        // Clamped, and never overflows however many attempts are asked for.
        assert_eq!(backoff_secs(&config, 6), 100);
        assert_eq!(backoff_secs(&config, u32::MAX), 100);
    }

    #[test]
    fn admitted_entries_are_due_immediately_and_get_distinct_ids() {
        let mut file = OutboxFile::default();
        admit(&mut file, lifecycle());
        admit(&mut file, lifecycle());
        assert_eq!(file.entries.len(), 2);
        assert_ne!(file.entries[0].id, file.entries[1].id);
        let now = now_ts();
        assert!(file.entries.iter().all(|entry| entry.next_attempt_at <= now));
        // Channels are resolved at attempt time, from the live config.
        assert!(file.entries.iter().all(|entry| entry.pending.is_empty()));
    }

    #[test]
    fn next_wakeup_is_zero_when_something_is_already_due() {
        let mut file = OutboxFile::default();
        admit(&mut file, lifecycle());
        assert_eq!(next_wakeup(&file, now_ts()), Duration::from_secs(0));
    }

    #[test]
    fn next_wakeup_picks_the_earliest_pending_attempt() {
        let mut file = OutboxFile::default();
        admit(&mut file, lifecycle());
        admit(&mut file, lifecycle());
        file.entries[0].next_attempt_at = 1_000;
        file.entries[1].next_attempt_at = 400;
        assert_eq!(next_wakeup(&file, 100), Duration::from_mins(5));
    }

    #[test]
    fn next_wakeup_on_an_empty_outbox_is_a_long_idle_sleep() {
        assert_eq!(next_wakeup(&OutboxFile::default(), 0), Duration::from_hours(1));
    }

    #[test]
    fn outbox_file_round_trips_through_json() {
        let mut file = OutboxFile::default();
        admit(&mut file, lifecycle());
        file.entries[0].pending = vec![MessagingChannel::Telegram, MessagingChannel::Discord];
        file.entries[0].attempts = 2;
        let bytes = serde_json::to_vec(&file).expect("serialize");
        let restored: OutboxFile = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].attempts, 2);
        assert_eq!(restored.entries[0].pending, vec![MessagingChannel::Telegram, MessagingChannel::Discord]);
    }

    #[test]
    fn a_full_outbox_hands_the_notification_back_instead_of_blocking() {
        let (sender, _receiver) = mpsc::channel(1);
        let outbox = NotificationOutbox { sender };
        assert!(outbox.enqueue(lifecycle()).is_none());
        assert!(outbox.enqueue(lifecycle()).is_some());
    }
}
