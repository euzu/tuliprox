use super::{charge_waiting_siblings, RecordingNotificationPlan};
use crate::recording::{
    recording_queue::{
        mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingControl,
        RecordingQueue, RecordingTask, RecordingTaskState,
    },
    recording_sidecar,
};
use log::warn;
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
    candidate.active.iter().position(|active| active.uuid == uuid).map(|index| candidate.active.remove(index))
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

/// Commit a terminal failure for the active task and move on.
pub(super) async fn fail_active_download(
    download_queue: &RecordingQueue,
    uuid: &str,
    reason: &str,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(mut failed) = take_active(candidate, uuid) else {
            return Ok(None);
        };
        failed.finished = true;
        failed.paused = false;
        failed.next_retry_at = None;
        failed.state = RecordingTaskState::Failed;
        failed.error = Some(reason.to_string());
        failed.recording.reserved_bytes = 0;
        candidate.finished.push(failed);
        crate::recording::recording_queue::promote_from_queue(candidate);
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
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
    download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut active) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            let notification = finish(&mut active);
            let media = active.media_identity.clone();
            let measured = active.recording.measured_bytes;
            candidate.finished.push(active);
            if let Some(limits) = waiting_quota {
                let _ = charge_waiting_siblings(candidate, &media, measured, limits);
            }
            crate::recording::recording_queue::promote_from_queue(candidate);
            Ok(Some(notification))
        })
        .await
}

pub(super) async fn cancel_active_and_promote(
    download_queue: &RecordingQueue,
    uuid: &str,
) -> Result<bool, QueueMutationError> {
    Ok(download_queue
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
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false))
}
