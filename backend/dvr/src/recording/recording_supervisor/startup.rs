//! Startup reconciliation.
//!
//! Two independent repairs run once the HTTP listener is bound:
//!
//! 1. **Stuck deletions.** A crash between the `Deleting` state flip and
//!    the queue removal leaves a task the UI shows as terminal but which
//!    can never be deleted again. For each such task the physical file
//!    decides: gone → finish the deletion and drop the task; present and
//!    inside the recording root → restore the prior terminal state;
//!    present but at a path outside the recording root → restore the
//!    task, leave the file alone, and record an audit line. Restoring
//!    rather than dropping is deliberate: dropping the task would orphan
//!    a file nothing tracks any more, whereas a restored task stays
//!    visible for an operator to resolve.
//! 2. **Queue/rule drift.** The queue and the rule repository are two
//!    stores; either write can fail alone. The pure planner in
//!    [`super::super::recording_reconciliation`] decides the repair and
//!    this module applies it, honouring the fixed cross-store order
//!    (queue boundary first, then the rule file).
//!
//! Errors are logged, never propagated: a server that cannot reconcile
//! must still start, otherwise a single corrupt tombstone would make
//! the process unbootable.

use super::{
    super::recording_ctx::RecordingCtx,
    health::{supervisor_health, SupervisorHealth},
    now_ts, recording_config, recording_enabled,
};
use crate::{
    recording::recording_queue::{mutate, RecordingTask},
    recording_deletion::{apply_recovery_to_candidate, recovery_action_for, RecoveryAction},
    recording_reconciliation::ReconcileAction,
};
use log::{debug, error, info, warn};
use shared::model::recording_rule::{RecordingRulesFile, RecordingTombstone, TombstoneKind};
use std::{collections::HashSet, path::PathBuf};
use tuliprox_repository::recording_rule_repository::RecordingRuleRepository;
use tuliprox_session::EventMessage;

/// Repair the DVR state left behind by the previous process.
pub async fn run_startup_reconciliation(ctx: &RecordingCtx) {
    if !recording_enabled(&ctx.app_config) {
        debug!("Recording disabled; skipping DVR startup reconciliation");
        return;
    }
    let stuck = recover_stuck_deletions(ctx).await;
    let drift = reconcile_rule_drift(ctx).await;
    SupervisorHealth::stamp(&supervisor_health().reconciliation_last_run, now_ts());
    if stuck > 0 || drift > 0 {
        info!("DVR startup reconciliation: repaired {stuck} interrupted deletion(s), {drift} rule drift item(s)");
        let _ = ctx.event_manager.send_event(EventMessage::RecordingChanged);
    }
}

/// Finish or undo every deletion the previous process left half-done.
async fn recover_stuck_deletions(ctx: &RecordingCtx) -> usize {
    let (_revision, tasks) = ctx.recordings.committed_snapshot().await;
    let pending: Vec<RecordingTask> =
        tasks.into_iter().filter(|task| task.recording.deleting_previous_state.is_some()).collect();
    if pending.is_empty() {
        return 0;
    }
    let recording_root = recording_config(&ctx.app_config)
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
        let outcome = mutate(&ctx.recordings, move |candidate| {
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
            Err(err) => error!("Failed to recover interrupted deletion for recording {}: {err}", task.uuid),
        }
    }
    repaired
}

/// Apply the reconciliation plan for queue/rule drift.
async fn reconcile_rule_drift(ctx: &RecordingCtx) -> usize {
    let storage_dir = ctx.app_config.config.load().storage_dir.clone();
    let repo = RecordingRuleRepository::new(storage_dir);
    let mut file = match repo.load().await {
        Ok(file) => file,
        Err(err) => {
            error!("DVR reconciliation could not load the rule repository: {err}");
            return 0;
        }
    };
    let tasks = super::super::recording_rule_scheduler::reconcilable_tasks(ctx).await;
    let now = now_ts();
    let actions = super::super::recording_reconciliation::reconcile(&file.rules, &tasks, &file.tombstones, now);
    if actions.is_empty() {
        return 0;
    }

    // Queue-side actions first — the fixed cross-store order is
    // "queue mutation boundary -> rule repository mutation".
    let mut applied = finalize_cancelled_occurrences_in_queue(&actions, &ctx.recordings).await;

    // Rule-side actions: one save for the whole plan.
    let (more, changed) = apply_rule_actions_to_tombstones(&actions, &mut file, now);
    applied += more;
    if changed {
        if let Err(err) = repo.save(&file).await {
            error!("DVR reconciliation could not persist repaired tombstones: {err}");
            return applied;
        }
        let _ = ctx.event_manager.send_event(EventMessage::RecordingRulesChanged);
    }
    applied
}

/// Drop every queue-resident task the planner wants finalized, under a
/// single mutation boundary. The closure borrows the borrowed
/// `HashSet<&str>` rather than cloning uuids into a second owned set.
async fn finalize_cancelled_occurrences_in_queue(
    actions: &[super::super::recording_reconciliation::ReconcileAction],
    recordings: &crate::recording::recording_queue::RecordingQueue,
) -> usize {
    let finalize: Vec<&str> = actions
        .iter()
        .filter_map(|action| match action {
            super::super::recording_reconciliation::ReconcileAction::Finalize { uuid } => Some(uuid.as_str()),
            _ => None,
        })
        .collect();
    if finalize.is_empty() {
        return 0;
    }
    let count = finalize.len();
    let targets: HashSet<&str> = finalize.iter().copied().collect();
    match mutate(recordings, |candidate| {
        candidate.queue.retain(|task| !targets.contains(task.uuid.as_str()));
        candidate.scheduled.retain(|task| !targets.contains(task.uuid.as_str()));
        candidate.finished.retain(|task| !targets.contains(task.uuid.as_str()));
        Ok(())
    })
    .await
    {
        Ok(()) => count,
        Err(err) => {
            error!("DVR reconciliation could not finalize cancelled occurrences: {err}");
            0
        }
    }
}

/// Apply the rule-side actions to the in-memory tombstone list and
/// return `(applied, changed)`. `changed` is what the caller uses to
/// decide whether a save is necessary.
fn apply_rule_actions_to_tombstones(
    actions: &[ReconcileAction],
    file: &mut RecordingRulesFile,
    now: i64,
) -> (usize, bool) {
    let mut applied = 0;
    let mut tombstones_changed = false;
    for action in actions {
        match action {
            ReconcileAction::AddScheduledTombstone { rule_id, occurrence_key } => {
                if !has_tombstone(&file.tombstones.tombstones, rule_id, occurrence_key) {
                    file.tombstones.tombstones.push(RecordingTombstone {
                        rule_id: rule_id.clone(),
                        occurrence_key: occurrence_key.clone(),
                        kind: TombstoneKind::Scheduled,
                        created_at: now,
                        expires_at: super::super::recording_reconciliation::tombstone_expires_at(now, None),
                    });
                    tombstones_changed = true;
                    applied += 1;
                }
            }
            ReconcileAction::UpdateTombstone { rule_id, occurrence_key, new_kind } => {
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
            ReconcileAction::Materialize { rule_id, occurrence_key } => {
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
            ReconcileAction::PruneTombstone { rule_id, occurrence_key } => {
                let before = file.tombstones.tombstones.len();
                file.tombstones
                    .tombstones
                    .retain(|t| !(&t.rule_id == rule_id && &t.occurrence_key == occurrence_key && t.expires_at <= now));
                if file.tombstones.tombstones.len() != before {
                    tombstones_changed = true;
                    applied += 1;
                }
            }
            ReconcileAction::ConflictingIntent { uuid, intent } => {
                // Never cancel an active recording because of a stale
                // intent. Surface it and let the operator decide.
                warn!(
                    target: "recording::audit",
                    "recording_reconciliation_conflicting_intent: recording {uuid} is active but the \
                     rule store recorded a {intent:?} intent; leaving the recording running"
                );
            }
            ReconcileAction::Finalize { .. } | ReconcileAction::Noop => {}
        }
    }
    (applied, tombstones_changed)
}

fn has_tombstone(tombstones: &[RecordingTombstone], rule_id: &str, occurrence_key: &str) -> bool {
    tombstones.iter().any(|t| t.rule_id == rule_id && t.occurrence_key == occurrence_key)
}
