use super::{charge_waiting_siblings, mark_recording_metadata_notification, RecordingNotificationPlan};
use crate::recording::{
    recording_notification::LifecycleEvent,
    recording_queue::{
        mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingControl,
        RecordingQueue, RecordingTask, RecordingTaskState,
    },
    recording_sidecar,
};
use log::warn;
use shared::model::RecordingMetadata;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) async fn active_download_snapshot_for_worker(
    active: &RwLock<Vec<RecordingTask>>,
    worker_uuid: &str,
) -> Option<RecordingTask> {
    active.read().await.iter().find(|task| task.uuid == worker_uuid).cloned()
}

pub(super) async fn update_active_download_for_worker<F>(
    active: &RwLock<Vec<RecordingTask>>,
    worker_uuid: &str,
    update: F,
) -> bool
where
    F: FnOnce(&mut RecordingTask) -> bool,
{
    let mut active = active.write().await;
    let Some(task) = active.iter_mut().find(|task| task.uuid == worker_uuid) else {
        return false;
    };
    update(task)
}

pub(super) async fn set_active_download_state(
    download_queue: &RecordingQueue,
    uuid: &str,
    state: RecordingTaskState,
    error: Option<String>,
    paused: bool,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(task) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        if task.state == state && task.error == error && task.paused == paused && !task.finished {
            return Ok(None);
        }
        task.state = state;
        task.error = error;
        task.paused = paused;
        task.finished = false;
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

/// Take the active task out of the candidate, but only when it is still the
/// one this worker is executing.
pub(super) fn take_active(candidate: &mut PersistedRecordingQueue, uuid: &str) -> Option<PersistedRecordingTask> {
    let index = candidate.active.iter().position(|active| active.uuid == uuid)?;
    Some(candidate.active.remove(index))
}

/// Write the sidecar for the recording that just finished.
///
/// Best effort by design: the recording is on disk and committed either way,
/// and refusing to complete a transfer because a descriptive file could not be
/// written would trade a real recording for a diagnostic.
pub(super) async fn write_completion_sidecar(download_queue: &RecordingQueue, uuid: &str, measured_bytes: u64) {
    let Some(task) = download_queue.active.read().await.iter().find(|task| task.uuid == uuid).cloned() else {
        return;
    };
    if task.cancelled_by_owner {
        return;
    }
    let Some(relative_path) = task.recording.relative_path.clone() else {
        return;
    };
    let persisted = RecordingQueue::to_persisted(&task);
    let sidecar = recording_sidecar::RecordingSidecar {
        materialization_id: tuliprox_repository::recording_repository::materialization_id_for(&persisted),
        media_identity: persisted.media_identity,
        kind: task.kind,
        relative_path,
        size_bytes: measured_bytes,
        completed_at: chrono::Utc::now().timestamp(),
    };
    if let Err(error) = recording_sidecar::write_sidecar(&task.file_path, &sidecar).await {
        warn!("Could not write the sidecar for {}: {error}", task.file_path.display());
    }
}

pub(super) struct FailedActiveCommit {
    pub notification: RecordingNotificationPlan,
    pub metadata: Option<Arc<RecordingMetadata>>,
}

pub(super) async fn commit_failed_active_download(
    download_queue: &RecordingQueue,
    uuid: &str,
    reason: &str,
    clone_metadata: bool,
) -> Result<Option<FailedActiveCommit>, QueueMutationError> {
    let outcome = download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut failed) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            failed.finished = true;
            failed.paused = false;
            failed.next_retry_at = None;
            failed.state = RecordingTaskState::Failed;
            let reason_str = reason.to_string();
            failed.error = Some(reason_str.clone());
            failed.recording.reserved_bytes = 0;
            let metadata = if clone_metadata { Some(Arc::new(failed.recording.clone())) } else { None };
            let notification =
                mark_recording_metadata_notification(&mut failed.recording, LifecycleEvent::Failed, Some(reason_str));
            let failed_media = failed.media_identity.clone();
            candidate.finished.push(failed);

            let mut failed_clips = Vec::new();
            candidate.scheduled.retain(|scheduled_task| {
                if !scheduled_task.media_identity.is_empty()
                    && scheduled_task.media_identity == failed_media
                    && scheduled_task.clip.is_some()
                {
                    let mut f = scheduled_task.clone();
                    f.finished = true;
                    f.paused = false;
                    f.next_retry_at = None;
                    f.state = RecordingTaskState::Failed;
                    f.error = Some("recording_source_recording_failed".to_string());
                    f.recording.reserved_bytes = 0;
                    failed_clips.push(f);
                    false
                } else {
                    true
                }
            });
            candidate.finished.extend(failed_clips);

            crate::recording::recording_queue::promote_from_queue(candidate);
            let actions = RecordingQueue::cleanup_settled_cancelled_enclosings(candidate);
            Ok(Some((FailedActiveCommit { notification, metadata }, actions)))
        })
        .await?;

    if let Some((commit, actions)) = outcome {
        crate::recording::recording_deletion::apply_cancelled_enclosing_action(download_queue, actions).await;
        Ok(Some(commit))
    } else {
        Ok(None)
    }
}

/// Commit a terminal failure for the active task and move on.
pub async fn fail_active_download(
    download_queue: &RecordingQueue,
    uuid: &str,
    reason: &str,
) -> Result<bool, QueueMutationError> {
    commit_failed_active_download(download_queue, uuid, reason, false).await.map(|opt| opt.is_some())
}

pub(super) async fn promote_ready_downloads(download_queue: &RecordingQueue) -> Result<bool, QueueMutationError> {
    if !download_queue.has_promotable_queued().await {
        return Ok(false);
    }
    Ok(mutate_optional(download_queue, |candidate| {
        let queued = candidate.queue.len();
        while crate::recording::recording_queue::promote_from_queue(candidate).is_some() {}
        Ok((candidate.queue.len() != queued).then_some(true))
    })
    .await?
    .unwrap_or(false))
}

/// Move this worker's task to `finished` and promote the next runnable entry.
///
/// With `waiting_quota`, the task finished a file other entries may be
/// waiting to attach to. Its measured size is checked against each of their
/// quotas before promotion attaches them: a transfer of unknown size cannot
/// be checked when it starts, and attaching charges the whole file.
pub(super) async fn finish_active_and_promote<F>(
    download_queue: &RecordingQueue,
    uuid: &str,
    waiting_quota: Option<&crate::recording::recording_quota::QuotaLimits>,
    finish: F,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingTask) -> RecordingNotificationPlan,
{
    let outcome = download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut active) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            let notification = finish(&mut active);
            let media = active.media_identity.clone();
            let measured = active.recording.measured_bytes;
            candidate.finished.push(active);

            // Attached clips settle in `promote_due_scheduled`, which reads the
            // file outside the queue lock and applies the failure and gap rules.

            if let Some(limits) = waiting_quota {
                let _ = charge_waiting_siblings(candidate, &media, measured, limits);
            }
            crate::recording::recording_queue::promote_from_queue(candidate);
            let actions = RecordingQueue::cleanup_settled_cancelled_enclosings(candidate);
            Ok(Some((notification, actions)))
        })
        .await?;

    if let Some((notification, actions)) = outcome {
        crate::recording::recording_deletion::apply_cancelled_enclosing_action(download_queue, actions).await;
        Ok(Some(notification))
    } else {
        Ok(None)
    }
}

pub(super) async fn cancel_active_and_promote(
    download_queue: &RecordingQueue,
    uuid: &str,
) -> Result<bool, QueueMutationError> {
    let outcome = download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut active) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            active.finished = true;
            active.paused = false;
            active.next_retry_at = None;
            active.error.get_or_insert_with(|| "Cancelled by user".to_string());
            active.state = RecordingTaskState::Cancelled;
            // Nothing more will be written, so the space stops being spoken for.
            active.recording.reserved_bytes = 0;
            candidate.finished.push(active);
            crate::recording::recording_queue::promote_from_queue(candidate);
            let actions = RecordingQueue::cleanup_settled_cancelled_enclosings(candidate);
            Ok(Some(actions))
        })
        .await?;

    if let Some(actions) = outcome {
        crate::recording::recording_deletion::apply_cancelled_enclosing_action(download_queue, actions).await;
        Ok(true)
    } else {
        Ok(false)
    }
}
