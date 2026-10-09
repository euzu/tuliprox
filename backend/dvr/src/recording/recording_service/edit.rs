use super::{
    conflicts::used_bytes_for_pool,
    control::map_queue_error,
    filenames::reserve_recording_relative_path,
    identity::{locate_recording, recording_mut_at, RecordingLocation},
    quota_limits_from_config, recording_identity_key,
    window::{effective_recording_window, padding_bounds},
    EditRecordingPatch, EditSnapshot, RecordingService, RecordingTaskView, ServiceError,
};
use crate::{
    recording::recording_queue::{mutate, QueueMutationError},
    recording_edit,
    recording_edit::EditError,
    recording_quota,
    recording_quota::AdmissionOutcome,
};
use shared::model::RecordingKind;
use tuliprox_auth::{authorize, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};

impl RecordingService {
    /// Edit an existing recording.
    #[allow(clippy::too_many_lines)]
    pub async fn edit_recording(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
        patch: EditRecordingPatch,
    ) -> Result<RecordingTaskView, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let config = self.app_config.config.load();
        // Edit does not re-resolve a URL. The recording config is
        // only needed for padding bounds, the unknown-bitrate fallback,
        // and quota limits. When the recording block is absent the
        // helper falls back to the shared-model defaults, so a
        // configured-without-recording deployment still validates
        // edits.
        let recording_cfg = config.recording();
        let bounds = padding_bounds(recording_cfg);
        let fallback_bytes_per_minute = recording_cfg.map_or(8 * 1024 * 1024, |cfg| cfg.fallback_bytes_per_minute);
        let quota_limits = quota_limits_from_config(recording_cfg.and_then(|cfg| cfg.quota.as_ref()));
        let mut out = None;
        mutate(&self.recordings, |candidate| {
            // Single linear scan: locate the recording, snapshot the
            // primitives we need for the immutable analysis, drop the
            // borrow, run the checks, then re-acquire the same task
            // via the remembered location for the O(1) write phase.
            let location = locate_recording(candidate, uuid).ok_or(QueueMutationError::UnknownRecording)?;
            let snapshot = {
                // `Active` and `Finished` are not in `scheduled` /
                // `queue`, but `locate_recording` returns them anyway —
                // short-circuit with `StateNotEditable` so the match
                // arms below stay narrowed to the editable lists.
                let task = match location {
                    RecordingLocation::Scheduled(i) => &candidate.scheduled[i],
                    RecordingLocation::Queue(i) => &candidate.queue[i],
                    RecordingLocation::Active(_) | RecordingLocation::Finished(_) => {
                        return Err(QueueMutationError::StateNotEditable);
                    }
                };
                if !recording_edit::state_is_editable(task.state) {
                    return Err(QueueMutationError::StateNotEditable);
                }
                let meta_snapshot = &task.recording;
                let pool = recording_quota::quota_pool_for_task(task);
                let subject = RecordingSubject::new(Some(meta_snapshot), TerminalState::Active, true);
                if !matches!(authorize(claims, &owner_id, RecordingAction::Edit, &subject), RecordingDecision::Allow) {
                    return Err(QueueMutationError::Forbidden);
                }
                let merged_pre = patch.pre_roll_secs.unwrap_or(meta_snapshot.pre_roll_secs);
                let merged_post = patch.post_roll_secs.unwrap_or(meta_snapshot.post_roll_secs);
                recording_edit::validate_padding(merged_pre, merged_post, bounds).map_err(|err| match err {
                    EditError::PaddingLimitExceeded => QueueMutationError::PaddingLimitExceeded,
                    EditError::InvalidInterval => QueueMutationError::InvalidInterval,
                    EditError::StateNotEditable | EditError::ChannelChangedWithoutProgramme => {
                        QueueMutationError::StateNotEditable
                    }
                    EditError::ProvenanceCleared => QueueMutationError::Forbidden,
                })?;
                let channel_changed_now = recording_edit::channel_changed(
                    patch.channel_id.as_deref(),
                    patch.channel_name.as_deref(),
                    meta_snapshot.channel_id.as_deref(),
                    meta_snapshot.channel_name.as_deref(),
                );
                EditSnapshot {
                    pool,
                    merged_pre,
                    merged_post,
                    channel_changed_now,
                    current_start: meta_snapshot.program_start,
                    current_end: meta_snapshot.program_end,
                    current_reserved: meta_snapshot.reserved_bytes,
                    is_live: task.kind == RecordingKind::Live,
                    shares_media: crate::recording::recording_queue::media_is_still_referenced(candidate, uuid),
                }
            };
            // A programme window and its padding exist for live captures
            // only; the create path refuses them for VOD and series too.
            let edits_window = patch.program_start.is_some()
                || patch.program_end.is_some()
                || patch.pre_roll_secs.is_some()
                || patch.post_roll_secs.is_some();
            if !snapshot.is_live && edits_window {
                return Err(QueueMutationError::InvalidInterval);
            }
            // A changed window is derived the way the create path derives
            // it: padded, and reserved for what is left of it. An edit that
            // leaves the window alone leaves its schedule and reservation
            // alone too.
            let window = if edits_window {
                let start =
                    patch.program_start.or(snapshot.current_start).ok_or(QueueMutationError::InvalidInterval)?;
                let end = patch.program_end.or(snapshot.current_end).ok_or(QueueMutationError::InvalidInterval)?;
                // Same rule as a new request: the interval itself must be
                // representable, not only its padded ends.
                if end.checked_sub(start).is_none_or(|duration| duration <= 0) {
                    return Err(QueueMutationError::InvalidInterval);
                }
                let window = effective_recording_window(
                    start,
                    end,
                    snapshot.merged_pre,
                    snapshot.merged_post,
                    chrono::Utc::now().timestamp(),
                )
                .map_err(|error| match error {
                    ServiceError::PaddingLimitExceeded => QueueMutationError::PaddingLimitExceeded,
                    _ => QueueMutationError::InvalidInterval,
                })?;
                let (reserved, _) =
                    recording_quota::estimate_reservation(window.remaining_duration_secs, 0, fallback_bytes_per_minute);
                let pool_used = used_bytes_for_pool(candidate, &snapshot.pool);
                let pool_used_minus_this = pool_used.saturating_sub(snapshot.current_reserved);
                if matches!(
                    recording_quota::would_exceed(&snapshot.pool, pool_used_minus_this, reserved, &quota_limits),
                    AdmissionOutcome::OverLimit { .. }
                ) {
                    return Err(QueueMutationError::QuotaExceeded);
                }
                Some((start, end, window, reserved))
            } else {
                None
            };

            // All immutable borrows are out of scope. Re-acquire the
            // same task via the remembered location (O(1)) for the
            // actual edit and apply every field write here. If any
            // earlier step returned `Err`, none of these writes run,
            // so the candidate is rolled back atomically.
            let Some(task) = recording_mut_at(candidate, location) else {
                return Err(QueueMutationError::UnknownRecording);
            };
            let meta = &mut task.recording;
            if let Some(title) = patch.program_title {
                meta.program_title = Some(title);
            }
            if let Some(channel_id) = patch.channel_id {
                meta.channel_id = Some(channel_id);
            }
            if let Some(channel_name) = patch.channel_name {
                meta.channel_name = Some(channel_name);
            }
            if let Some((start, end, window, reserved)) = window {
                meta.pre_roll_secs = snapshot.merged_pre;
                meta.post_roll_secs = snapshot.merged_post;
                meta.program_start = Some(start);
                meta.program_end = Some(end);
                meta.scheduled_start = Some(window.scheduled_start);
                meta.scheduled_end = Some(window.scheduled_end);
                meta.reserved_bytes = reserved;
            }
            if snapshot.channel_changed_now {
                meta.epg = None;
            }

            // The window is part of what makes two requests the same media.
            // An edit that changes it leaves the media this entry shared,
            // so it must not keep the shared file's path either: two
            // captures would write one file, and deleting either would
            // take the other's.
            let identity = recording_identity_key(&task.recording, &task.url);
            let leaves_shared_media = identity != task.media_identity && snapshot.shares_media;
            task.media_identity = identity;
            if leaves_shared_media {
                let mut detached = task.clone();
                reserve_recording_relative_path(candidate, &mut detached)?;
                let Some(task) = recording_mut_at(candidate, location) else {
                    return Err(QueueMutationError::UnknownRecording);
                };
                *task = detached;
            }

            let Some(task) = recording_mut_at(candidate, location) else {
                return Err(QueueMutationError::UnknownRecording);
            };
            out = Some(RecordingTaskView {
                uuid: task.uuid.clone(),
                owner_id: owner_id.clone(),
                visibility: task.recording.visibility,
                filename_preview: task.filename.clone(),
                start_at: task.recording.scheduled_start,
                duration_secs: window.map(|(_, _, window, _)| window.remaining_duration_secs),
                state: task.state,
            });
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))?;
        out.ok_or(ServiceError::UnknownRecording)
    }
}
