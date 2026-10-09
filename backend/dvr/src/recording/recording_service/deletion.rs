use super::{conflicts::candidate_tasks, error::map_deletion_error, RecordingService, ServiceError};
use crate::{
    recording::recording_queue::{mutate, PersistedRecordingQueue, PersistedRecordingTask, RecordingTaskState},
    recording_deletion::{begin_deletion_authorized, execute_deletion_target, finalize_deletion, rollback_deletion},
};
use shared::model::recording::RecordingMetadata;
use tuliprox_auth::{authorize, authorize_orphan, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};

impl RecordingService {
    /// Delete a finished recording via the three-step service.
    /// Marks the task as `Deleting` (atomic), unlinks the file
    /// (outside the boundary), then removes the task (atomic).
    pub async fn delete_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        self.run_deletion(uuid, |meta| {
            let subject = RecordingSubject::new(Some(meta), TerminalState::Completed, true);
            matches!(authorize(claims, &owner_id, RecordingAction::Delete, &subject), RecordingDecision::Allow)
        })
        .await
        .map(|_| ())
    }

    /// The three-phase deletion, shared by the user-facing delete and the
    /// retention worker. `permit` runs *inside* the same mutation
    /// boundary that stamps the task as deleting, so there is no window
    /// in which the authorized metadata and the stamped task can differ:
    /// the previous implementation looked the task up, authorized it,
    /// stamped it, then looked it up a second time and could act on a
    /// stale copy.
    /// Returns `true` when the file was actually unlinked. `false` means the
    /// entry is gone but another library entry still holds its bytes, so the
    /// caller must not count the space as reclaimed.
    pub(super) async fn run_deletion<F>(&self, uuid: &str, permit: F) -> Result<bool, ServiceError>
    where
        F: FnOnce(&RecordingMetadata) -> bool,
    {
        let queue = self.recordings.clone();
        let target = begin_deletion_authorized(&queue, uuid, permit).await.map_err(map_deletion_error)?;
        if let Err(err) = execute_deletion_target(&target).await {
            // File removal failed: undo the deletion transition so the
            // recording stays visible in its prior state instead of
            // being silently lost when finalize_deletion runs.
            let uuid_owned = uuid.to_string();
            let _ = mutate(&self.recordings, |candidate| {
                rollback_deletion(candidate, &uuid_owned);
                Ok(())
            })
            .await;
            return Err(ServiceError::IoError(err.to_string()));
        }
        finalize_deletion(&queue, uuid).await.map_err(|_| ServiceError::UnknownRecording)?;
        Ok(!target.still_referenced)
    }

    /// Internal retention-delete entrypoint used by the retention
    /// worker. Bypasses user ownership but enforces state/kind/path
    pub async fn system_retention_delete(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
    ) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        self.run_deletion(uuid, |meta| {
            let subject = RecordingSubject::new(Some(meta), TerminalState::Completed, true);
            matches!(
                authorize(claims, &owner_id, RecordingAction::SystemRetentionDelete, &subject,),
                RecordingDecision::Allow
            )
        })
        .await
    }

    /// Re-export the orphan policy for callers that need it.
    pub fn authorize_orphan_read(&self, claims: &shared::model::Claims) -> Result<(), ServiceError> {
        match authorize_orphan(claims) {
            RecordingDecision::Allow => Ok(()),
            RecordingDecision::Deny(_) => Err(ServiceError::Forbidden),
        }
    }
}

/// The size of the media `task` refers to, as far as another entry already
/// knows it: the measured size of a completed file, or the total a transfer
/// learned from its provider. `0` when nobody knows yet.
pub(super) fn known_media_size(candidate: &PersistedRecordingQueue, task: &PersistedRecordingTask) -> u64 {
    if task.media_identity.is_empty() {
        return 0;
    }
    candidate_tasks(candidate)
        .filter(|other| other.media_identity == task.media_identity)
        .filter_map(|other| {
            if other.state == RecordingTaskState::Completed {
                Some(other.recording.measured_bytes.max(other.size))
            } else {
                other.total_size
            }
        })
        .max()
        .unwrap_or(0)
}

pub(super) fn collect_existing_relative_paths(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &str> + '_ {
    candidate_tasks(candidate).map(task_relative_path)
}

fn task_relative_path(task: &PersistedRecordingTask) -> &str {
    task.recording.relative_path.as_deref().unwrap_or(task.filename.as_str())
}
