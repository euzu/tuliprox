//! Runtime supervisors for the DVR.
//!
//! The recording feature ships three pure decision layers whose runners
//! were never started, so in production the DVR worked on the happy path
//! but could not bound its disk use or heal itself after a crash:
//!
//! - [`recording_retention`](super::recording_retention) computes the
//!   age/count candidates and
//!   [`recording_worker_runner`](super::recording_worker_runner) computes
//!   the disk-pressure candidates, but nothing called them.
//! - [`recording_reconciliation::reconcile`](super::recording_reconciliation::reconcile)
//!   decides how to repair queue/rule drift, but nothing called it, so a
//!   task left in `Deleting` by a crash stayed there forever.
//! - Lifecycle notifications were fired with a bare `tokio::spawn` from
//!   inside the download worker, so a transient provider error dropped
//!   the notification with no retry and no record.
//!
//! This module owns the runners. Each one is cancellation-aware, never
//! overlaps its own passes, and re-reads its configuration every tick so
//! a config reload takes effect without a restart.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use log::{debug, error, info, warn};
use shared::model::recording_rule::{RecordingTombstone, TombstoneKind};
use shared::model::{Claims, Permission, PermissionSet, CURRENT_PERMISSION_SCHEMA_VERSION, ROLE_ADMIN};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::model::download::{mutate, FileDownload};
use crate::api::model::recording_deletion::{
    apply_recovery_to_candidate, recovery_action_for, RecoveryAction,
};
use crate::api::model::recording_service::{RecordingService, ServiceError};
use crate::api::model::recording_worker_runner::{DeleteOutcome, DiskConfig};
use crate::api::model::{AppState, EventMessage};
use crate::messaging::{configured_channels, send_message_to_channel, MessagingChannel};
use crate::model::{MessageContent, RecordingConfig, RecordingNotificationConfig};
use crate::repository::recording_rule_repository::RecordingRuleRepository;

/// Floor on how often the recording root is measured. `statvfs` is cheap
/// but not free, and it would be wasteful to re-measure on a tick that
/// fires seconds after the last one (which a small
/// `disk.cleanup_interval_secs` would do).
const MIN_DISK_PRESSURE_INTERVAL_SECS: u64 = 30;

/// Fallback watermark-check cadence when `disk.cleanup_interval_secs` is
/// unset.
const DEFAULT_WATERMARK_CHECK_INTERVAL_SECS: u64 = 60;

/// Outbox file name under `storage_dir`.
const NOTIFICATION_OUTBOX_FILE: &str = "recording_notification_outbox.json";

// ---------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------

/// Last-tick timestamps, so an operator can tell a healthy supervisor
/// from one that died. Read by the health endpoint; written by the
/// supervisors themselves.
#[derive(Debug, Default)]
pub struct SupervisorHealth {
    reconciliation_last_run: AtomicI64,
    retention_last_tick: AtomicI64,
    notification_last_drain: AtomicI64,
    notification_outbox_depth: AtomicI64,
    notification_dead_lettered: AtomicI64,
}

impl SupervisorHealth {
    fn stamp(field: &AtomicI64, now: i64) {
        field.store(now, Ordering::Relaxed);
    }

    pub fn reconciliation_last_run(&self) -> Option<i64> {
        non_zero(self.reconciliation_last_run.load(Ordering::Relaxed))
    }
    pub fn retention_last_tick(&self) -> Option<i64> {
        non_zero(self.retention_last_tick.load(Ordering::Relaxed))
    }
    pub fn notification_last_drain(&self) -> Option<i64> {
        non_zero(self.notification_last_drain.load(Ordering::Relaxed))
    }
    pub fn notification_outbox_depth(&self) -> i64 {
        self.notification_outbox_depth.load(Ordering::Relaxed)
    }
    pub fn notification_dead_lettered(&self) -> i64 {
        self.notification_dead_lettered.load(Ordering::Relaxed)
    }
}

fn non_zero(value: i64) -> Option<i64> {
    (value != 0).then_some(value)
}

/// Process-wide health, so the health endpoint does not need a handle
/// threaded through `AppState` (which is rebuilt on every config
/// reload, whereas the supervisors outlive one).
pub fn supervisor_health() -> &'static SupervisorHealth {
    static HEALTH: OnceLock<SupervisorHealth> = OnceLock::new();
    HEALTH.get_or_init(SupervisorHealth::default)
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// The effective recording configuration, cloned out of the `ArcSwap`
/// guard so no guard is held across an await.
fn recording_config(app_state: &AppState) -> Option<RecordingConfig> {
    app_state
        .app_config
        .config
        .load()
        .video
        .as_ref()
        .and_then(|video| video.download.as_ref())
        .and_then(|download| download.recording.clone())
}

/// `true` when the DVR is switched on.
///
/// The single predicate behind every `recording.enabled` gate — the REST
/// routes, the rule scheduler, the supervisors, and the WebSocket
/// filters. Keeping one definition is the point: four copies of
/// "is the DVR on?" would eventually disagree, and a half-disabled DVR
/// (routes refusing but the scheduler still materializing) is worse than
/// either state.
///
/// An absent `recording:` block means "use the defaults", and the default
/// is enabled.
pub fn recording_enabled(app_state: &AppState) -> bool {
    recording_config(app_state).is_none_or(|cfg| cfg.enabled)
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Claims for a system-initiated action. The retention worker is not a
/// user; it holds the administrator role so it can act on shared and
/// legacy-owned recordings, and `RecordingWrite` so the service-level
/// permission checks pass.
fn system_claims() -> Claims {
    let mut permissions = PermissionSet::new();
    permissions.set(Permission::RecordingWrite);
    permissions.set(Permission::RecordingRead);
    let now = now_ts();
    Claims {
        username: "recording-supervisor".to_string(),
        iss: "tuliprox".to_string(),
        iat: now,
        exp: now + 3600,
        roles: vec![ROLE_ADMIN.to_string()],
        permissions,
        pwd_version: 0,
        subject_id: Some(shared::model::UserId::builtin_admin()),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

/// A guard that makes a supervisor's passes strictly non-overlapping.
struct PassGuard(Arc<AtomicBool>);

impl PassGuard {
    fn try_claim(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------
// Startup reconciliation
// ---------------------------------------------------------------------

/// Repair the DVR state left behind by the previous process.
///
/// Two independent repairs run here:
///
/// 1. **Stuck deletions.** A crash between the `Deleting` state flip and
///    the queue removal leaves a task the UI shows as terminal but which
///    can never be deleted again. For each such task the physical file
///    decides: gone → finish the deletion and drop the task; present and
///    inside the recording root → restore the prior terminal state;
///    present but at a path outside the recording root → restore the
///    task, leave the file alone, and record an audit line. Restoring
///    rather than dropping is deliberate: dropping the task would orphan
///    a file nothing tracks any more, whereas a restored task stays
///    visible for an operator to resolve.
/// 2. **Queue/rule drift.** The queue and the rule repository are two
///    stores; either write can fail alone.
///    [`reconcile`](super::recording_reconciliation::reconcile) decides
///    the repair and this function applies it, honouring the fixed
///    cross-store order (queue boundary first, then the rule file).
///
/// Errors are logged, never propagated: a server that cannot reconcile
/// must still start, otherwise a single corrupt tombstone would make the
/// process unbootable.
pub async fn run_startup_reconciliation(app_state: &Arc<AppState>) {
    if !recording_enabled(app_state.as_ref()) {
        debug!("Recording disabled; skipping DVR startup reconciliation");
        return;
    }
    let stuck = recover_stuck_deletions(app_state).await;
    let drift = reconcile_rule_drift(app_state).await;
    SupervisorHealth::stamp(&supervisor_health().reconciliation_last_run, now_ts());
    if stuck > 0 || drift > 0 {
        info!("DVR startup reconciliation: repaired {stuck} interrupted deletion(s), {drift} rule drift item(s)");
        let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
    }
}

/// Finish or undo every deletion the previous process left half-done.
async fn recover_stuck_deletions(app_state: &Arc<AppState>) -> usize {
    let (_revision, tasks) = app_state.downloads.committed_snapshot().await;
    let pending: Vec<FileDownload> = tasks
        .into_iter()
        .filter(|task| {
            task.recording
                .as_ref()
                .is_some_and(|meta| meta.deleting_previous_state.is_some())
        })
        .collect();
    if pending.is_empty() {
        return 0;
    }
    let recording_root = recording_config(app_state.as_ref())
        .map(|cfg| PathBuf::from(cfg.directory))
        .filter(|dir| !dir.as_os_str().is_empty());
    let mut repaired = 0;
    for task in pending {
        let action = recovery_action_for(&task, recording_root.as_deref()).await;
        match action {
            RecoveryAction::NotDeleting => continue,
            RecoveryAction::UnsafeRestore => {
                // Do not touch a file we cannot prove is ours. Restore the
                // task so the operator can see it and decide.
                warn!(
                    target: "recording::audit",
                    "recording_reconciliation_unsafe_path: task {} points outside the recording root; \
                     restoring the task and leaving the file alone",
                    task.uuid
                );
            }
            RecoveryAction::FinishDeletion | RecoveryAction::RestorePrevious => {}
        }
        let uuid = task.uuid.clone();
        let finish = matches!(action, RecoveryAction::FinishDeletion);
        let outcome = mutate(&app_state.downloads, move |candidate| {
            apply_recovery_to_candidate(candidate, &uuid, action);
            if finish {
                // `apply_recovery_to_candidate` only clears the marker; the
                // task itself still has to leave the queue.
                candidate.queue.retain(|task| task.uuid != uuid);
                candidate.scheduled.retain(|task| task.uuid != uuid);
                candidate.finished.retain(|task| task.uuid != uuid);
                if candidate.active.as_ref().is_some_and(|task| task.uuid == uuid) {
                    candidate.active = None;
                }
            }
            Ok(())
        })
        .await;
        match outcome {
            Ok(()) => {
                repaired += 1;
                debug!("Recovered interrupted deletion for recording {} ({action:?})", task.uuid);
            }
            Err(err) => error!(
                "Failed to recover interrupted deletion for recording {}: {err}",
                task.uuid
            ),
        }
    }
    repaired
}

/// Apply the reconciliation plan for queue/rule drift.
async fn reconcile_rule_drift(app_state: &Arc<AppState>) -> usize {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let repo = RecordingRuleRepository::new(storage_dir);
    let mut file = match repo.load().await {
        Ok(file) => file,
        Err(err) => {
            error!("DVR reconciliation could not load the rule repository: {err}");
            return 0;
        }
    };
    let tasks = super::recording_rule_scheduler::reconcilable_tasks(app_state.as_ref()).await;
    let now = now_ts();
    let actions = super::recording_reconciliation::reconcile(&file.rules, &tasks, &file.tombstones, now);
    if actions.is_empty() {
        return 0;
    }

    // Queue-side actions first — the fixed cross-store order is
    // "queue mutation boundary -> rule repository mutation".
    let finalize: Vec<String> = actions
        .iter()
        .filter_map(|action| match action {
            super::recording_reconciliation::ReconcileAction::Finalize { uuid } => Some(uuid.clone()),
            _ => None,
        })
        .collect();
    let mut applied = 0;
    if !finalize.is_empty() {
        let targets: HashSet<String> = finalize.iter().cloned().collect();
        let count = targets.len();
        match mutate(&app_state.downloads, move |candidate| {
            candidate.queue.retain(|task| !targets.contains(&task.uuid));
            candidate.scheduled.retain(|task| !targets.contains(&task.uuid));
            candidate.finished.retain(|task| !targets.contains(&task.uuid));
            Ok(())
        })
        .await
        {
            Ok(()) => applied += count,
            Err(err) => error!("DVR reconciliation could not finalize cancelled occurrences: {err}"),
        }
    }

    // Rule-side actions: one save for the whole plan.
    let mut tombstones_changed = false;
    for action in &actions {
        use super::recording_reconciliation::ReconcileAction as Action;
        match action {
            Action::AddScheduledTombstone { rule_id, occurrence_key } => {
                if !has_tombstone(&file.tombstones.tombstones, rule_id, occurrence_key) {
                    file.tombstones.tombstones.push(RecordingTombstone {
                        rule_id: rule_id.clone(),
                        occurrence_key: occurrence_key.clone(),
                        kind: TombstoneKind::Scheduled,
                        created_at: now,
                        expires_at: super::recording_reconciliation::tombstone_expires_at(now, None),
                    });
                    tombstones_changed = true;
                    applied += 1;
                }
            }
            Action::UpdateTombstone { rule_id, occurrence_key, new_kind } => {
                if let Some(tombstone) = file
                    .tombstones
                    .tombstones
                    .iter_mut()
                    .find(|t| &t.rule_id == rule_id && &t.occurrence_key == occurrence_key)
                {
                    if tombstone.kind != *new_kind {
                        tombstone.kind = *new_kind;
                        tombstones_changed = true;
                        applied += 1;
                    }
                }
            }
            Action::Materialize { rule_id, occurrence_key } => {
                // The task for a still-live `Scheduled` tombstone is gone.
                // The occurrence key alone cannot be turned back into a
                // programme window, so drop the orphan tombstone and let
                // the rule scheduler re-plan the occurrence from the rule
                // and the EPG on its next tick.
                let before = file.tombstones.tombstones.len();
                file.tombstones.tombstones.retain(|t| {
                    !(&t.rule_id == rule_id
                        && &t.occurrence_key == occurrence_key
                        && matches!(t.kind, TombstoneKind::Scheduled))
                });
                if file.tombstones.tombstones.len() != before {
                    tombstones_changed = true;
                    applied += 1;
                    debug!("DVR reconciliation released orphan occurrence {rule_id}/{occurrence_key} for re-planning");
                }
            }
            Action::PruneTombstone { rule_id, occurrence_key } => {
                let before = file.tombstones.tombstones.len();
                file.tombstones
                    .tombstones
                    .retain(|t| !(&t.rule_id == rule_id && &t.occurrence_key == occurrence_key && t.expires_at <= now));
                if file.tombstones.tombstones.len() != before {
                    tombstones_changed = true;
                    applied += 1;
                }
            }
            Action::ConflictingIntent { uuid, intent } => {
                // Never cancel an active recording because of a stale
                // intent. Surface it and let the operator decide.
                warn!(
                    target: "recording::audit",
                    "recording_reconciliation_conflicting_intent: recording {uuid} is active but the \
                     rule store recorded a {intent:?} intent; leaving the recording running"
                );
            }
            Action::Finalize { .. } | Action::Noop => {}
        }
    }
    if tombstones_changed {
        if let Err(err) = repo.save(&file).await {
            error!("DVR reconciliation could not persist repaired tombstones: {err}");
            return applied;
        }
        let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
    }
    applied
}

fn has_tombstone(tombstones: &[RecordingTombstone], rule_id: &str, occurrence_key: &str) -> bool {
    tombstones
        .iter()
        .any(|t| t.rule_id == rule_id && t.occurrence_key == occurrence_key)
}

// ---------------------------------------------------------------------
// Retention supervisor
// ---------------------------------------------------------------------

/// Start the retention supervisor.
///
/// One task drives both sweeps so they can never delete concurrently:
///
/// - the **policy sweep** (`keep_last_per_channel` / `delete_after_days`)
///   runs every `retention.sweep_interval_secs`;
/// - the **disk-pressure sweep** runs on the shorter
///   `disk.cleanup_interval_secs` cadence, and only actually measures the
///   filesystem when at least [`MIN_DISK_PRESSURE_INTERVAL_SECS`] have
///   passed since the last measurement.
///
/// Both sweeps delete through `RecordingService::system_retention_delete`,
/// so there is exactly one deletion path in the system.
pub fn spawn_retention_supervisor(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let app_state = Arc::clone(app_state);
    let cancel_token = cancel_token.clone();
    let running = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        let mut next_policy_sweep_at = 0i64;
        let mut last_disk_measurement_at = 0i64;
        loop {
            let tick_interval = watermark_check_interval(app_state.as_ref());
            tokio::select! {
                () = cancel_token.cancelled() => break,
                () = tokio::time::sleep(tick_interval) => {}
            }
            if !recording_enabled(app_state.as_ref()) {
                continue;
            }
            // Skip the tick entirely if the previous pass is still
            // deleting; passes must never overlap.
            let Some(_guard) = PassGuard::try_claim(&running) else {
                debug!("Retention supervisor tick skipped: previous pass still running");
                continue;
            };
            let now = now_ts();
            SupervisorHealth::stamp(&supervisor_health().retention_last_tick, now);

            let mut deleted = 0u64;
            if now >= next_policy_sweep_at {
                deleted += run_policy_sweep(&app_state, now).await;
                next_policy_sweep_at = now.saturating_add(policy_sweep_interval_secs(app_state.as_ref()));
            }
            if now.saturating_sub(last_disk_measurement_at) >= i64::try_from(MIN_DISK_PRESSURE_INTERVAL_SECS).unwrap_or(30) {
                last_disk_measurement_at = now;
                deleted += run_disk_pressure_sweep(&app_state).await;
            }
            if deleted > 0 {
                let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            }
        }
        debug!("Retention supervisor stopped");
    });
}

fn policy_sweep_interval_secs(app_state: &AppState) -> i64 {
    let secs = recording_config(app_state)
        .and_then(|cfg| cfg.retention.map(|retention| retention.sweep_interval_secs))
        .unwrap_or_else(shared::model::default_recording_retention_sweep_interval_secs);
    i64::try_from(secs.max(1)).unwrap_or(3600)
}

fn watermark_check_interval(app_state: &AppState) -> Duration {
    let secs = recording_config(app_state)
        .and_then(|cfg| cfg.disk.and_then(|disk| disk.cleanup_interval_secs))
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_WATERMARK_CHECK_INTERVAL_SECS);
    // Never tick faster than the disk measurement floor: a tighter
    // cadence would only burn wake-ups.
    Duration::from_secs(secs.max(1).min(3600))
}

/// Age + count retention.
async fn run_policy_sweep(app_state: &Arc<AppState>, now: i64) -> u64 {
    let Some(config) = recording_config(app_state.as_ref()) else {
        return 0;
    };
    let Some(retention) = config.retention.as_ref() else {
        return 0;
    };
    let policy = super::recording_retention::RetentionConfig {
        keep_last_per_channel: retention.keep_last_per_channel,
        delete_after_days: retention.delete_after_days,
    };
    if policy.keep_last_per_channel.is_none() && policy.delete_after_days.is_none() {
        return 0;
    }
    let (_revision, tasks) = app_state.downloads.committed_snapshot().await;
    let candidates = super::recording_retention::compute_candidates(&tasks, &policy, now);
    if candidates.is_empty() {
        return 0;
    }
    let service = RecordingService::from_app_state(app_state);
    let claims = system_claims();
    let mut deleted = 0u64;
    for candidate in &candidates {
        match delete_for_retention(&service, &claims, &candidate.uuid).await {
            DeleteOutcome::Ok => {
                deleted += 1;
                info!(
                    target: "recording::audit",
                    "recording_retention_delete: reason={:?}", candidate.reason
                );
            }
            DeleteOutcome::Skipped => {}
            DeleteOutcome::Failed => {}
        }
    }
    if deleted > 0 {
        info!(
            "Retention policy sweep deleted {deleted} of {} candidate recording(s)",
            candidates.len()
        );
    }
    deleted
}

/// Free-space driven retention. Only runs when both watermarks are
/// configured and the recording root is measurable.
async fn run_disk_pressure_sweep(app_state: &Arc<AppState>) -> u64 {
    let Some(config) = recording_config(app_state.as_ref()) else {
        return 0;
    };
    let Some(disk) = config.disk.as_ref() else {
        return 0;
    };
    let disk_config = DiskConfig {
        high_water_percent: disk.high_water_percent,
        low_water_percent: disk.low_water_percent,
        safety_bytes: disk.safety_bytes,
    };
    if disk_config.high_water_percent.is_none() || disk_config.low_water_percent.is_none() {
        return 0;
    }
    let root = PathBuf::from(&config.directory);
    if root.as_os_str().is_empty() {
        return 0;
    }
    // Measure the recording root itself, never `storage_dir` or the
    // generic download directory — those can be on another filesystem.
    let Some((total_bytes, free_bytes)) = super::recording_disk::filesystem_capacity_for(&root) else {
        debug!("Disk-pressure sweep skipped: cannot measure {}", root.display());
        return 0;
    };
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let used_percent = u8::try_from(used.saturating_mul(100) / total_bytes).unwrap_or(100);

    let (_revision, tasks) = app_state.downloads.committed_snapshot().await;
    // The candidate ordering and the admission conditions stay in the
    // pure runner; only the delete side effect lives here, so the loop
    // can `await` instead of blocking a worker thread.
    let Some(candidates) = super::recording_worker_runner::disk_pressure_candidates(
        &tasks,
        &disk_config,
        used_percent,
        true,
    ) else {
        return 0;
    };
    let low = disk_config.low_water_percent.unwrap_or(0);
    let service = RecordingService::from_app_state(app_state);
    let claims = system_claims();
    let mut deleted = 0u64;
    let mut reclaimed = 0u64;
    for candidate in &candidates {
        if super::recording_worker_runner::pressure_relieved(total_bytes, free_bytes, reclaimed, low) {
            break;
        }
        let reclaimable = super::recording_worker_runner::reclaimable_bytes_for(&tasks, &candidate.uuid);
        if matches!(
            delete_for_retention(&service, &claims, &candidate.uuid).await,
            DeleteOutcome::Ok
        ) {
            deleted += 1;
            reclaimed = reclaimed.saturating_add(reclaimable);
        }
    }
    info!(
        target: "recording::audit",
        "recording_retention_delete: reason=watermark used_percent={used_percent} candidates={} deleted={deleted} reclaimed_bytes={reclaimed}",
        candidates.len()
    );
    deleted
}

async fn delete_for_retention(
    service: &RecordingService,
    claims: &Claims,
    uuid: &str,
) -> DeleteOutcome {
    match service.system_retention_delete(claims, uuid).await {
        Ok(()) => DeleteOutcome::Ok,
        // The task moved on (already deleted, no longer terminal, or not
        // safe to touch). Not an error: the next sweep re-evaluates.
        Err(ServiceError::UnknownRecording | ServiceError::InvalidState | ServiceError::Forbidden) => {
            DeleteOutcome::Skipped
        }
        Err(err) => {
            error!("Retention delete failed for recording {uuid}: {err}");
            DeleteOutcome::Failed
        }
    }
}

// ---------------------------------------------------------------------
// Notification outbox
// ---------------------------------------------------------------------

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
pub fn notification_outbox() -> Option<&'static NotificationOutbox> {
    OUTBOX.get()
}

/// Start the notification outbox worker.
///
/// The recorder used to `tokio::spawn(send_message(..))` straight from
/// its persist path, so a transient provider error lost the notification
/// permanently and a crash between the persist and the spawn lost it
/// too. The worker owns delivery instead: entries are persisted to
/// `storage_dir/recording_notification_outbox.json` before the first
/// attempt, retried per channel with capped exponential backoff, and
/// dead-lettered with a log line after `max_attempts`.
///
/// Idempotent: calling it twice installs only the first worker.
pub fn spawn_notification_outbox(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let config = notification_config(app_state.as_ref());
    let (sender, receiver) = mpsc::channel(config.outbox_buffer);
    if OUTBOX.set(NotificationOutbox { sender }).is_err() {
        debug!("Recording notification outbox already installed");
        return;
    }
    let app_state = Arc::clone(app_state);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        run_notification_outbox(app_state, receiver, cancel_token).await;
    });
}

fn notification_config(app_state: &AppState) -> RecordingNotificationConfig {
    recording_config(app_state).map_or_else(RecordingNotificationConfig::default, |cfg| cfg.notifications)
}

fn outbox_path(app_state: &AppState) -> PathBuf {
    PathBuf::from(app_state.app_config.config.load().storage_dir.as_str())
        .join(NOTIFICATION_OUTBOX_FILE)
}

async fn run_notification_outbox(
    app_state: Arc<AppState>,
    mut receiver: mpsc::Receiver<MessageContent>,
    cancel_token: CancellationToken,
) {
    let path = outbox_path(app_state.as_ref());
    let mut file = load_outbox(&path).await;
    if !file.entries.is_empty() {
        info!(
            "Recording notification outbox recovered {} undelivered notification(s)",
            file.entries.len()
        );
    }
    // Once every sender is gone `recv()` completes instantly and forever,
    // so the loop must stop polling it or it would spin.
    let mut senders_gone = false;
    loop {
        let sleep_for = next_wakeup(&file, now_ts());
        let mut received = None;
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
                        Some(content) => received = Some(content),
                        None => senders_gone = true,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
            }
        }
        if let Some(content) = received {
            admit(&mut file, content);
            // Drain whatever else is already queued so a burst of
            // completions costs one persist, not one per event.
            while let Ok(content) = receiver.try_recv() {
                admit(&mut file, content);
            }
            persist_outbox(&path, &file).await;
        }
        if drain_due_entries(&app_state, &mut file).await {
            persist_outbox(&path, &file).await;
        }
        supervisor_health()
            .notification_outbox_depth
            .store(i64::try_from(file.entries.len()).unwrap_or(i64::MAX), Ordering::Relaxed);
    }
    debug!("Recording notification outbox stopped");
}

/// How long to sleep before the next delivery attempt is due.
fn next_wakeup(file: &OutboxFile, now: i64) -> Duration {
    file.entries
        .iter()
        .map(|entry| entry.next_attempt_at.saturating_sub(now).max(0))
        .min()
        .map_or(Duration::from_secs(3600), |secs| {
            Duration::from_secs(u64::try_from(secs).unwrap_or(0))
        })
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
async fn drain_due_entries(app_state: &Arc<AppState>, file: &mut OutboxFile) -> bool {
    let now = now_ts();
    if !file.entries.iter().any(|entry| entry.next_attempt_at <= now) {
        return false;
    }
    SupervisorHealth::stamp(&supervisor_health().notification_last_drain, now);
    let config = notification_config(app_state.as_ref());
    let client = app_state.http_client.load_full();
    let app_config = Arc::clone(&app_state.app_config);
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
            supervisor_health()
                .notification_dead_lettered
                .fetch_add(1, Ordering::Relaxed);
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
    let delay = config
        .backoff_initial_secs
        .saturating_mul(1u64 << shift)
        .min(config.backoff_max_secs);
    i64::try_from(delay).unwrap_or(i64::MAX)
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
            if let Err(err) = crate::utils::atomic_json_store::write_json_atomic(path, &bytes).await {
                error!("Could not persist the recording notification outbox: {err}");
            }
        }
        Err(err) => error!("Could not serialize the recording notification outbox: {err}"),
    }
}

// ---------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------

/// Start every DVR supervisor. Called once the HTTP listener is bound so
/// the reconciliation pass cannot delay the bind.
pub async fn start_recording_supervisors(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    if !recording_enabled(app_state.as_ref()) {
        info!("Recording is disabled; DVR supervisors not started");
        return;
    }
    // Reconcile before anything else can materialize or sweep, so the
    // scheduler never plans against half-repaired state.
    run_startup_reconciliation(app_state).await;
    spawn_notification_outbox(app_state, cancel_token);
    spawn_retention_supervisor(app_state, cancel_token);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecordingLifecycleMessage;
    use shared::model::MsgKind;

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
        assert_eq!(next_wakeup(&file, 100), Duration::from_secs(300));
    }

    #[test]
    fn next_wakeup_on_an_empty_outbox_is_a_long_idle_sleep() {
        assert_eq!(next_wakeup(&OutboxFile::default(), 0), Duration::from_secs(3600));
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
        assert_eq!(
            restored.entries[0].pending,
            vec![MessagingChannel::Telegram, MessagingChannel::Discord]
        );
    }

    #[test]
    fn a_full_outbox_hands_the_notification_back_instead_of_blocking() {
        let (sender, _receiver) = mpsc::channel(1);
        let outbox = NotificationOutbox { sender };
        assert!(outbox.enqueue(lifecycle()).is_none());
        assert!(outbox.enqueue(lifecycle()).is_some());
    }

    #[test]
    fn an_absent_recording_block_means_enabled() {
        // The DVR must be on for a deployment that never mentions
        // `recording:` — otherwise upgrading would silently switch off a
        // feature the operator was already using.
        let config = crate::model::Config::default();
        let app_state = crate::api::model::create_test_app_state(config);
        assert!(recording_enabled(app_state.as_ref()));
        assert!(recording_config(app_state.as_ref()).is_none());
    }

    #[test]
    fn pass_guard_prevents_overlapping_passes() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = PassGuard::try_claim(&flag).expect("first claim");
        assert!(PassGuard::try_claim(&flag).is_none());
        drop(first);
        assert!(PassGuard::try_claim(&flag).is_some());
    }
}
