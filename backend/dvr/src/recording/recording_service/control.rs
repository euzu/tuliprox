use super::{
    conflicts::{authorize_task_in_candidate, candidate_tasks},
    CancelledRuleRecording, RecordingService, ServiceError,
};
use crate::{
    recording::recording_queue::{
        mutate, mutate_prepared, mutate_then, PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError,
        RecordingTaskState,
    },
    recording_edit,
};
use shared::model::{recording::RecordingVisibility, RecordingKind, UserId};
use tuliprox_auth::{authorize, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};

impl RecordingService {
    /// Cancel an in-flight or scheduled recording.
    ///
    /// An active recording is asked to stop, not marked stopped: the worker
    /// still owns the file and the provider slot, so it must release them
    /// first and is the one that commits `Cancelled`. Marking it terminal
    /// here would show it as a finished recording while it is still writing,
    /// and a later remove would then find it in the active slot, leave it
    /// there, and report success.
    pub async fn cancel_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.recordings.active.read().await.iter().find(|task| task.uuid == uuid).cloned();
        if let Some(active) = active {
            let meta = active.recording.clone();
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Cancel, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            // Cancel by uuid. Between the read above and this call ffmpeg
            // can finish and the queue can promote a *different* recording
            // into the active slot; cancelling whatever is active would then
            // kill that innocent recording.
            match self.recordings.cancel_requested(uuid).await {
                // The task is active (running, waiting, or paused). A paused
                // one has already been moved to `finished`; a worker-owned one
                // is now `Cancelling` and the worker will finish it.
                Ok(Some(_)) => return Ok(()),
                // The task left the active slot in the meantime. Fall
                // through to the inactive path: it either finds the task
                // in `scheduled`/`queue` (a re-promotion) or reports
                // `UnknownRecording`, which is the truthful answer.
                Ok(None) => {}
                Err(err) => {
                    log::error!("cancel_requested failed for {uuid}: {err}");
                    return Err(ServiceError::PersistenceFailed);
                }
            }
        }
        mutate(&self.recordings, |candidate| {
            let Some(task) = remove_inactive_recording(candidate, uuid) else {
                return Err(QueueMutationError::UnknownRecording);
            };
            let meta = &task.recording;
            let subject = RecordingSubject::new(Some(meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Cancel, &subject), RecordingDecision::Allow) {
                return Err(QueueMutationError::Forbidden);
            }
            let mut cancelled = task;
            cancelled.state = RecordingTaskState::Cancelled;
            cancelled.finished = true;
            cancelled.error = Some("cancelled".to_string());
            cancelled.recording.reserved_bytes = 0;
            candidate.finished.push(cancelled);
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))
    }

    /// Cancel future inactive recordings that were materialized from a
    /// recurring rule. Active recordings are intentionally left untouched.
    /// Returns the pre-cancel snapshots of everything it cancelled. The
    /// caller is mid-way through a two-store operation (cancel the
    /// occurrences, then delete the rule) that cannot be made atomic, so
    /// it keeps these to undo the queue side if the rule store fails —
    /// see [`Self::restore_cancelled_rule_recordings`].
    pub async fn pause_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.recordings.active.read().await.iter().find(|task| task.uuid == uuid).cloned();
        if let Some(active) = active {
            let meta = active.recording.clone();
            if !active.kind.is_resumable() {
                return Err(ServiceError::InvalidState); // Live cannot be paused
            }
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Edit, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            self.recordings.pause_active(uuid).await.map_err(|_| ServiceError::PersistenceFailed)?;
            return Ok(());
        }
        Err(ServiceError::UnknownRecording)
    }

    pub async fn resume_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.recordings.active.read().await.iter().find(|task| task.uuid == uuid).cloned();
        if let Some(active) = active {
            let meta = active.recording.clone();
            if !active.kind.is_resumable() {
                return Err(ServiceError::InvalidState);
            }
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Edit, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            return self.recordings.resume_active(uuid).await.map_err(|_| ServiceError::PersistenceFailed);
        }
        Err(ServiceError::UnknownRecording)
    }

    /// Remove a task from the queue. The ownership check runs inside the
    /// same mutation that removes it, so a task cannot change hands
    /// between the check and the write.
    pub async fn remove_recording_task(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
    ) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let (removed, _) = mutate_then(
            &self.recordings,
            |candidate| {
                authorize_task_in_candidate(candidate, uuid, claims, &owner_id, RecordingAction::Delete)?;
                // The active slot is owned by the worker. Removing a task from
                // it here would leave the worker writing to a file no entry
                // names, so refuse instead of silently doing nothing (the retain
                // below cannot reach the active slot).
                if candidate.active.iter().any(|active| active.uuid == uuid) {
                    return Err(QueueMutationError::StateNotEditable);
                }
                // Removing an entry keeps a finished recording on disk; that is
                // what separates it from deleting the file. A partial is
                // different: with no entry and no worker left on its media, it
                // is never resumed. A worker on the same media holds the active
                // slot, so it counts as a reference here.
                let orphaned_partial = candidate_tasks(candidate)
                    .find(|task| task.uuid == uuid)
                    .filter(|_| !crate::recording::recording_queue::media_is_still_referenced(candidate, uuid))
                    .map(|task| task.file_path.clone());
                let original = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
                candidate.queue.retain(|task| task.uuid != uuid);
                candidate.scheduled.retain(|task| task.uuid != uuid);
                candidate.finished.retain(|task| task.uuid != uuid);
                let current = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
                let removed = current != original;
                Ok((removed, orphaned_partial.filter(|_| removed)))
            },
            // Still under the guard that decided the partial is orphaned: a
            // request admitted after it may reuse the path, and must find it
            // either still claimed or already empty, never emptied under it.
            |(_, orphaned_partial)| {
                let orphaned_partial = orphaned_partial.clone();
                async move {
                    let Some(file_path) = orphaned_partial else {
                        return;
                    };
                    if let Err(err) = crate::recording_deletion::remove_orphaned_partial(&file_path).await {
                        log::warn!("Removed a recording, but could not remove its leftover partial: {err}");
                    }
                }
            },
        )
        .await
        .map_err(|e| map_queue_error(&e))?;
        Ok(removed)
    }

    /// Requeue a finished VOD/Series transfer. Live is rejected by the
    /// queue itself: its programme window is gone.
    pub async fn retry_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        self.retry_recording_inner(claims, uuid, false).await
    }

    /// Discard a partial only after the caller confirmed that a provider
    /// without byte-range support should download the file from the start.
    pub async fn restart_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        self.retry_recording_inner(claims, uuid, true).await
    }

    pub(super) async fn retry_recording_inner(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
        restart_from_beginning: bool,
    ) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let recordings = &self.recordings;
        let requester = &owner_id;
        mutate_prepared(
            recordings,
            // A restart discards the partial before the task is queued
            // again, so a worker can never resume from bytes the provider
            // cannot continue. It runs once the request is known to be
            // allowed, and before the mutation, under the same guard.
            || async move {
                if !restart_from_beginning {
                    return Ok(());
                }
                let partial = {
                    let finished = recordings.finished.read().await;
                    let Some(task) = finished.iter().find(|task| task.uuid == uuid) else {
                        return Ok(());
                    };
                    let subject = RecordingSubject::new(Some(&task.recording), TerminalState::Active, true);
                    if !matches!(
                        authorize(claims, requester, RecordingAction::Edit, &subject),
                        RecordingDecision::Allow
                    ) {
                        return Err(QueueMutationError::Forbidden);
                    }
                    if task.error.as_deref() != Some(super::super::recording_transfer::RANGE_UNSUPPORTED_ERROR) {
                        return Err(QueueMutationError::StateNotEditable);
                    }
                    crate::recording::recording_worker::recording_partial_path(&task.file_path)
                };
                match tokio::fs::remove_file(&partial).await {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(QueueMutationError::from_io(error)),
                }
            },
            |candidate, ()| {
                authorize_task_in_candidate(candidate, uuid, claims, &owner_id, RecordingAction::Edit)?;
                let Some(pos) = candidate.finished.iter().position(|task| task.uuid == uuid) else {
                    return Ok(false);
                };
                if !candidate.finished[pos].kind.is_resumable() {
                    return Err(QueueMutationError::StateNotEditable);
                }
                let range_unsupported = candidate.finished[pos].error.as_deref()
                    == Some(super::super::recording_transfer::RANGE_UNSUPPORTED_ERROR);
                if range_unsupported != restart_from_beginning {
                    return Err(QueueMutationError::StateNotEditable);
                }
                let mut task = candidate.finished.remove(pos);
                task.finished = false;
                task.size = 0;
                if restart_from_beginning {
                    task.total_size = None;
                    task.recording.resume_etag = None;
                    task.recording.resume_last_modified = None;
                }
                task.paused = false;
                task.error = None;
                task.state = RecordingTaskState::Queued;
                task.retry_attempts = 0;
                task.next_retry_at = None;
                candidate.queue.push(task);
                Ok(true)
            },
        )
        .await
        .map_err(|e| map_queue_error(&e))
    }
}

pub(super) fn authorize_create_recording(
    claims: &shared::model::Claims,
    owner_id: &UserId,
    visibility: RecordingVisibility,
) -> Result<(), ServiceError> {
    let action = match visibility {
        RecordingVisibility::Private => RecordingAction::CreatePrivate,
        RecordingVisibility::Shared => RecordingAction::CreateShared,
    };
    match authorize(claims, owner_id, action, &RecordingSubject::new(None, TerminalState::Active, true)) {
        RecordingDecision::Allow => Ok(()),
        RecordingDecision::Deny(tuliprox_auth::DenyReason::NotAdministrator) => {
            Err(ServiceError::SharedCreationNotAdministrator)
        }
        RecordingDecision::Deny(_) => Err(ServiceError::Forbidden),
    }
}

/// Map a queue-mutation failure onto the service error surface.
///
/// Every call site used to enumerate all twelve `QueueMutationError`
/// variants inline, so adding a variant meant editing four or more
/// matches. The variants that carry no site-specific meaning collapse
/// here; a site that needs a different mapping for one variant still
/// handles it before delegating.
pub(super) fn map_queue_error(err: &QueueMutationError) -> ServiceError {
    match err {
        QueueMutationError::Io(_) => ServiceError::PersistenceFailed,
        QueueMutationError::UnknownRecording => ServiceError::UnknownRecording,
        QueueMutationError::Forbidden => ServiceError::Forbidden,
        QueueMutationError::InvalidInterval => ServiceError::InvalidInterval,
        QueueMutationError::PaddingLimitExceeded => ServiceError::PaddingLimitExceeded,
        QueueMutationError::QuotaExceeded => ServiceError::QuotaExceeded,
        QueueMutationError::Duplicate => ServiceError::Duplicate,
        QueueMutationError::InvalidPath => ServiceError::InvalidPath,
        QueueMutationError::DiskFull => ServiceError::DiskFull,
        QueueMutationError::IdempotentReplay { recording_id } => {
            ServiceError::IdempotentReplay { recording_id: recording_id.clone() }
        }
        QueueMutationError::IdempotencyConflict => ServiceError::IdempotencyConflict,
        QueueMutationError::StateNotEditable
        | QueueMutationError::InvalidQuotaPool
        | QueueMutationError::NotInTerminalState
        | QueueMutationError::MutationSkipped
        | QueueMutationError::Other(_) => ServiceError::InvalidState,
    }
}

fn remove_inactive_recording(candidate: &mut PersistedRecordingQueue, uuid: &str) -> Option<PersistedRecordingTask> {
    if let Some(index) = candidate.scheduled.iter().position(|task| task.uuid == uuid) {
        return Some(candidate.scheduled.remove(index));
    }
    let index = candidate.queue.iter().position(|task| task.uuid == uuid)?;
    Some(candidate.queue.remove(index))
}

/// Which pending list a cancelled rule recording came from, so the
/// compensating restore puts it back where it belongs.
#[derive(Debug, Clone, Copy)]
pub(super) enum CancelOrigin {
    Scheduled,
    Queue,
}

pub(super) fn cancel_future_rule_recordings_in_candidate(
    candidate: &mut PersistedRecordingQueue,
    rule_id: &str,
    now_secs: i64,
) -> Vec<CancelledRuleRecording> {
    let mut undo = Vec::new();
    let mut moved = Vec::new();
    drain_future_rule_recordings(
        &mut candidate.scheduled,
        CancelOrigin::Scheduled,
        rule_id,
        now_secs,
        &mut undo,
        &mut moved,
    );
    drain_future_rule_recordings(&mut candidate.queue, CancelOrigin::Queue, rule_id, now_secs, &mut undo, &mut moved);
    candidate.finished.extend(moved);
    undo
}

fn drain_future_rule_recordings(
    tasks: &mut Vec<PersistedRecordingTask>,
    origin: CancelOrigin,
    rule_id: &str,
    now_secs: i64,
    undo: &mut Vec<CancelledRuleRecording>,
    out: &mut Vec<PersistedRecordingTask>,
) {
    let mut index = 0;
    while index < tasks.len() {
        if is_future_rule_recording(&tasks[index], rule_id, now_secs) {
            let mut task = tasks.remove(index);
            // Snapshot before the cancel mutates it: the undo has to
            // restore `reserved_bytes`, which is zeroed just below.
            undo.push(CancelledRuleRecording { origin, task: task.clone() });
            task.state = RecordingTaskState::Cancelled;
            task.finished = true;
            task.error = Some("cancelled".to_string());
            task.recording.reserved_bytes = 0;
            out.push(task);
        } else {
            index += 1;
        }
    }
}

pub(super) fn is_future_rule_recording(task: &PersistedRecordingTask, rule_id: &str, now_secs: i64) -> bool {
    if task.kind != RecordingKind::Live {
        return false;
    }
    let meta = &task.recording;
    let Some(start_at) = meta.scheduled_start else {
        return false;
    };
    if start_at <= now_secs {
        return false;
    }
    if meta.provenance.rule_id.as_deref() != Some(rule_id) {
        return false;
    }
    recording_edit::state_is_editable(task.state)
}
