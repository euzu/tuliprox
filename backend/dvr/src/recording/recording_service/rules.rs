use super::{
    control::{cancel_future_rule_recordings_in_candidate, map_queue_error, CancelOrigin},
    identity::locate_recording,
    CancelledRuleRecording, RecordingService, ServiceError,
};
use crate::recording::recording_queue::mutate;

impl RecordingService {
    pub async fn cancel_future_rule_recordings(
        &self,
        claims: &shared::model::Claims,
        rule_id: &str,
        now_secs: i64,
    ) -> Result<Vec<CancelledRuleRecording>, ServiceError> {
        let _ = Self::subject_id(claims)?;
        if !claims.permissions.contains(shared::model::Permission::RecordingManage) {
            return Err(ServiceError::Forbidden);
        }
        let mut cancelled = Vec::new();
        mutate(&self.recordings, |candidate| {
            cancelled = cancel_future_rule_recordings_in_candidate(candidate, rule_id, now_secs);
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))?;
        Ok(cancelled)
    }

    /// Compensating transaction for [`Self::cancel_future_rule_recordings`].
    ///
    /// Moves each task back out of `finished` into the list it came from,
    /// restoring the exact record that was captured before the cancel
    /// (including `reserved_bytes`, which the cancel zeroed). A uuid that
    /// something else has since claimed is left alone: a real
    /// create/edit always wins over an undo.
    pub async fn restore_cancelled_rule_recordings(
        &self,
        cancelled: &[CancelledRuleRecording],
    ) -> Result<(), ServiceError> {
        if cancelled.is_empty() {
            return Ok(());
        }
        mutate(&self.recordings, |candidate| {
            for entry in cancelled {
                let uuid = entry.task.uuid.as_str();
                candidate.finished.retain(|task| task.uuid != uuid);
                if locate_recording(candidate, uuid).is_some() {
                    continue;
                }
                match entry.origin {
                    CancelOrigin::Scheduled => candidate.scheduled.push(entry.task.clone()),
                    CancelOrigin::Queue => candidate.queue.push(entry.task.clone()),
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))
    }
}
