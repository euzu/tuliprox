//! Recording mutation service.

use super::{recording_ctx::RecordingCtx, recording_source_resolution as source_resolution};
use crate::{
    download::{
        mutate, DownloadKind, DownloadQueue, DownloadState, FileDownload, PersistedDownloadQueue,
        PersistedFileDownload, QueueMutationError,
    },
    recording_deletion::{
        begin_deletion_authorized, execute_deletion_target, finalize_deletion, rollback_deletion, DeletionError,
    },
    recording_edit::{self, EditError, PaddingBounds},
    recording_quota::{self, AdmissionOutcome, QuotaLimits, QuotaPool},
};
use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingProvenance, RecordingSource, RecordingVisibility},
    UserId, XtreamCluster,
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tuliprox_auth::{authorize, authorize_orphan, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};
use tuliprox_core::model::AppConfig;

/// Server-resolved identifiers that the recording system needs. The
/// caller never sees the URL; the service resolves it from these.
#[derive(Debug, Clone)]
pub struct RecordingSourceInput {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: XtreamCluster,
    pub input_name: String,
}

impl RecordingSourceInput {
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.target_id.trim().is_empty() || self.virtual_id.trim().is_empty() || self.input_name.trim().is_empty() {
            return Err(ServiceError::InvalidSource);
        }
        Ok(())
    }
}

/// Input for `RecordingService::create_recording`.
#[derive(Debug, Clone)]
pub struct CreateRecordingInput {
    pub source: RecordingSourceInput,
    pub program_title: String,
    pub program_start: i64,
    pub program_end: i64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub visibility: RecordingVisibility,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub provenance: RecordingProvenance,
    pub epg: Option<shared::model::recording::EpgEpisodeMetadata>,
}

impl CreateRecordingInput {
    pub fn validate(&self) -> Result<(), ServiceError> {
        self.source.validate()?;
        if self.program_end.checked_sub(self.program_start).is_none_or(|duration| duration <= 0) {
            return Err(ServiceError::InvalidInterval);
        }
        Ok(())
    }
}

/// Stable service-layer errors. The HTTP layer maps each variant to a
/// stable status; the frontend maps each variant to a localized
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// The recording owner could not be resolved from the authenticated
    /// claims (`subject_id` missing or invalid).
    UnknownOwner,
    /// The supplied `RecordingSource` does not match a configured
    /// target/input combination.
    InvalidSource,
    /// Caller asked for an action the principal cannot perform.
    Forbidden,
    /// Caller asked for shared creation but is not an administrator.
    SharedCreationNotAdministrator,
    /// Caller asked to act on a recording that is in an ineligible
    /// state (e.g., edit on a recording marked `Deleting`).
    InvalidState,
    /// `program_end - program_start` overflows or is non-positive.
    InvalidInterval,
    /// Requested padding exceeds the configured recording maximum.
    PaddingLimitExceeded,
    /// uuid not in the queue.
    UnknownRecording,
    /// `mutate`'s persist step failed and the in-memory state was
    /// kept unchanged.
    PersistenceFailed,
    /// IO error during physical deletion.
    IoError(String),
    /// Configured recording quota would be exceeded.
    QuotaExceeded,
    /// The patch would clear `rule_id` / `occurrence_key`. Both are
    /// immutable provenance; surfacing this as `InvalidState` hid the
    /// real reason from the client.
    ProvenanceImmutable,
    /// Caller tried to create a recording that already exists in the
    /// queue (same target / window). Distinct from `InvalidState` so
    /// the client can render a specific "duplicate" message.
    Duplicate,
    /// The recording's filesystem path is not within the configured
    /// storage root, or otherwise violates the path policy.
    InvalidPath,
    /// The recording cannot fit on disk; reservation would exceed
    /// available space.
    DiskFull,
    /// The server has no download engine: the `video.download` block
    /// is missing from the configuration. Distinct from
    /// `InvalidSource` because the caller's identifiers may be
    /// perfectly valid — nothing on the server can execute them.
    Disabled,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.code()) }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    /// Stable wire-level code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownOwner => "recording_unknown_owner",
            Self::InvalidSource => "recording_invalid_source",
            Self::Forbidden => "recording_forbidden",
            Self::SharedCreationNotAdministrator => "recording_shared_not_administrator",
            Self::InvalidState => "recording_invalid_state",
            Self::InvalidInterval => "recording_invalid_interval",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::UnknownRecording => "recording_unknown",
            Self::PersistenceFailed => "recording_persistence_failed",
            Self::IoError(_) => "recording_io_error",
            Self::QuotaExceeded => "recording_quota_exceeded",
            Self::ProvenanceImmutable => "recording_provenance_immutable",
            Self::Duplicate => "recording_duplicate",
            Self::InvalidPath => "recording_invalid_path",
            Self::DiskFull => "recording_disk_full",
            Self::Disabled => "recording_disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffectiveRecordingWindow {
    scheduled_start: i64,
    scheduled_end: i64,
    execution_start: i64,
    remaining_duration_secs: u64,
}

fn effective_recording_window(
    program_start: i64,
    program_end: i64,
    pre_roll_secs: u64,
    post_roll_secs: u64,
    now: i64,
) -> Result<EffectiveRecordingWindow, ServiceError> {
    if program_end <= program_start {
        return Err(ServiceError::InvalidInterval);
    }
    let pre_roll = i64::try_from(pre_roll_secs).map_err(|_| ServiceError::PaddingLimitExceeded)?;
    let post_roll = i64::try_from(post_roll_secs).map_err(|_| ServiceError::PaddingLimitExceeded)?;
    let scheduled_start = program_start.saturating_sub(pre_roll);
    let scheduled_end = program_end.saturating_add(post_roll);
    let execution_start = now.max(scheduled_start);
    // `scheduled_end >= execution_start` because both come from
    // saturating arithmetic on a non-empty interval, so the cast is
    // safe and the only remaining error is the degenerate
    // already-finished window.
    let remaining = scheduled_end.saturating_sub(execution_start);
    if remaining <= 0 {
        return Err(ServiceError::InvalidInterval);
    }
    let remaining_duration_secs = remaining.cast_unsigned();
    Ok(EffectiveRecordingWindow { scheduled_start, scheduled_end, execution_start, remaining_duration_secs })
}

fn padding_bounds(recording: Option<&tuliprox_core::model::RecordingConfig>) -> PaddingBounds {
    recording.map_or(
        PaddingBounds {
            max_pre_roll_secs: shared::model::default_recording_max_pre_roll_secs(),
            max_post_roll_secs: shared::model::default_recording_max_post_roll_secs(),
        },
        |config| PaddingBounds {
            max_pre_roll_secs: config.max_pre_roll_secs,
            max_post_roll_secs: config.max_post_roll_secs,
        },
    )
}

fn map_edit_validation_error(error: &EditError) -> ServiceError {
    match error {
        EditError::InvalidInterval => ServiceError::InvalidInterval,
        EditError::PaddingLimitExceeded => ServiceError::PaddingLimitExceeded,
        EditError::ProvenanceCleared => ServiceError::ProvenanceImmutable,
        EditError::StateNotEditable | EditError::ChannelChangedWithoutProgramme => ServiceError::InvalidState,
    }
}

fn map_deletion_error(error: DeletionError) -> ServiceError {
    match error {
        DeletionError::Forbidden => ServiceError::Forbidden,
        DeletionError::NotTerminal => ServiceError::InvalidState,
        DeletionError::UnknownTask | DeletionError::NotARecording => ServiceError::UnknownRecording,
        DeletionError::DeleteFailed(err) => ServiceError::IoError(err.to_string()),
        DeletionError::BeginFailed(err) | DeletionError::FinalizeFailed(err) => {
            if err.source_io().is_some() {
                ServiceError::PersistenceFailed
            } else {
                ServiceError::UnknownRecording
            }
        }
    }
}

/// Output of `RecordingService::create_recording` and friends.
#[derive(Debug, Clone)]
pub struct RecordingTaskView {
    pub uuid: String,
    pub owner_id: UserId,
    pub visibility: RecordingVisibility,
    pub filename_preview: String,
    pub start_at: Option<i64>,
    pub duration_secs: Option<u64>,
    pub state: DownloadState,
}

/// Input for `RecordingService::edit_recording`.
#[derive(Debug, Clone, Default)]
pub struct EditRecordingPatch {
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    pub pre_roll_secs: Option<u64>,
    pub post_roll_secs: Option<u64>,
    pub program_title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
}

/// Recording mutation boundary. Holds the queue and app config directly
/// so the server's root state does not carry a back-reference to the service.
pub struct RecordingService {
    downloads: Arc<DownloadQueue>,
    app_config: Arc<AppConfig>,
}

impl RecordingService {
    /// Construct from the queue and app config.
    pub fn new(downloads: Arc<DownloadQueue>, app_config: Arc<AppConfig>) -> Self { Self { downloads, app_config } }

    /// Convenience constructor from the DVR's context.
    pub fn from_ctx(ctx: &RecordingCtx) -> Self { Self::new(ctx.downloads.clone(), ctx.app_config.clone()) }

    fn subject_id(claims: &shared::model::Claims) -> Result<UserId, ServiceError> {
        claims.subject_id.clone().ok_or(ServiceError::UnknownOwner)
    }

    fn recording_url(&self, source: &RecordingSourceInput) -> Option<String> {
        let virtual_id = source.virtual_id.parse::<u32>().ok()?;
        source_resolution::resolve_recording_target(&self.app_config, &source.target_id, &source.input_name)?;
        source_resolution::build_recording_source_descriptor(
            &source.target_id,
            &source.input_name,
            virtual_id,
            source.cluster,
        )
    }

    /// Create a new recording. Enforces well-formedness, source
    /// validity, owner from claims, and the authorization matrix.
    #[allow(clippy::too_many_lines)]
    pub async fn create_recording(
        &self,
        claims: &shared::model::Claims,
        input: &CreateRecordingInput,
    ) -> Result<RecordingTaskView, ServiceError> {
        input.validate()?;
        let owner_id = Self::subject_id(claims)?;
        if !crate::recording::recording_supervisor::recording_enabled(&self.app_config) {
            return Err(ServiceError::Disabled);
        }
        let config = self.app_config.config.load();
        let Some(recording_cfg) = config.recording.as_ref() else {
            return Err(ServiceError::Disabled);
        };
        recording_edit::validate_padding(
            input.pre_roll_secs,
            input.post_roll_secs,
            padding_bounds(Some(recording_cfg)),
        )
        .map_err(|error: EditError| map_edit_validation_error(&error))?;
        let window = effective_recording_window(
            input.program_start,
            input.program_end,
            input.pre_roll_secs,
            input.post_roll_secs,
            chrono::Utc::now().timestamp(),
        )?;
        let url = self.recording_url(&input.source).ok_or(ServiceError::InvalidSource)?;

        // Shared creation requires admin.
        authorize_create_recording(claims, &owner_id, input.visibility)?;

        let duration_secs = window.remaining_duration_secs;
        let priority = recording_cfg.priority;
        let filename = render_filename_preview(input);
        let input_name: Option<Arc<str>> =
            (!input.source.input_name.trim().is_empty()).then(|| Arc::from(input.source.input_name.as_str()));
        let mut recording = FileDownload::new_recording(
            &url,
            &filename,
            recording_cfg,
            window.execution_start,
            duration_secs,
            input_name,
            priority,
        )
        .ok_or(ServiceError::InvalidSource)?;
        let source = RecordingSource::new(
            input.source.target_id.clone(),
            input.source.virtual_id.clone(),
            input.source.input_name.clone(),
        )
        .with_cluster(input.source.cluster);
        let mut meta = RecordingMetadata::new(
            RecordingOwner::User(owner_id.clone()),
            input.visibility,
            source,
            input.program_start,
            input.program_end,
            input.pre_roll_secs,
            input.post_roll_secs,
        );
        meta.scheduled_start = Some(window.scheduled_start);
        meta.scheduled_end = Some(window.scheduled_end);
        meta.channel_id.clone_from(&input.channel_id);
        meta.channel_name.clone_from(&input.channel_name);
        meta.program_title = Some(input.program_title.clone());
        meta.provenance = input.provenance.clone();
        meta.epg.clone_from(&input.epg);
        let fallback_bytes_per_minute = recording_cfg.fallback_bytes_per_minute;
        let (reserved_bytes, _) = recording_quota::estimate_reservation(duration_secs, 0, fallback_bytes_per_minute);
        meta.reserved_bytes = reserved_bytes;
        recording.recording = Some(meta);
        let mut persisted = DownloadQueue::to_persisted(&recording);
        let view_task = recording.clone();
        let quota_limits = quota_limits_from_config(recording_cfg.quota.as_ref());

        mutate(&self.downloads, |candidate| {
            reserve_recording_relative_path(candidate, &mut persisted)?;
            if candidate_has_duplicate_recording(candidate, &view_task) {
                return Err(QueueMutationError::Duplicate);
            }
            let pool = recording_quota::quota_pool_for_task(&persisted).ok_or(QueueMutationError::InvalidQuotaPool)?;
            let used = used_bytes_for_pool(candidate, &pool);
            if matches!(
                recording_quota::would_exceed(&pool, used, reserved_bytes, &quota_limits),
                AdmissionOutcome::OverLimit { .. }
            ) {
                return Err(QueueMutationError::QuotaExceeded);
            }
            candidate.scheduled.push(persisted);
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))?;

        Ok(RecordingTaskView {
            uuid: view_task.uuid,
            owner_id,
            visibility: input.visibility,
            filename_preview: view_task.filename,
            start_at: Some(window.execution_start),
            duration_secs: Some(duration_secs),
            state: DownloadState::Scheduled,
        })
    }

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
        let recording_cfg = config.recording.as_ref();
        let bounds = padding_bounds(recording_cfg);
        let fallback_bytes_per_minute = recording_cfg.map_or(8 * 1024 * 1024, |cfg| cfg.fallback_bytes_per_minute);
        let quota_limits = quota_limits_from_config(recording_cfg.and_then(|cfg| cfg.quota.as_ref()));
        let mut out = None;
        mutate(&self.downloads, |candidate| {
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
                    RecordingLocation::Active | RecordingLocation::Finished(_) => {
                        return Err(QueueMutationError::StateNotEditable);
                    }
                };
                if !recording_edit::state_is_editable(task.state.label()) {
                    return Err(QueueMutationError::StateNotEditable);
                }
                let Some(meta_snapshot) = task.recording.as_ref() else {
                    return Err(QueueMutationError::UnknownRecording);
                };
                let pool = recording_quota::quota_pool_for_task(task).ok_or(QueueMutationError::InvalidQuotaPool)?;
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
                    &recording_edit::EditPatch {
                        program_start: patch.program_start,
                        program_end: patch.program_end,
                        pre_roll_secs: patch.pre_roll_secs,
                        post_roll_secs: patch.post_roll_secs,
                        program_title: patch.program_title.clone(),
                        channel_id: patch.channel_id.clone(),
                        channel_name: patch.channel_name.clone(),
                    },
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
                }
            };
            // Immutable borrow is out of scope. Compute the new
            // interval and the post-edit reservation, then check the
            // quota against the pool total — no `RecordingMetadata`
            // clone is required because the snapshot carries only
            // primitives.
            let start = patch.program_start.or(snapshot.current_start).ok_or(QueueMutationError::InvalidInterval)?;
            let end = patch.program_end.or(snapshot.current_end).ok_or(QueueMutationError::InvalidInterval)?;
            if start >= end {
                return Err(QueueMutationError::InvalidInterval);
            }
            let duration_secs = end
                .checked_sub(start)
                .and_then(|duration| u64::try_from(duration).ok())
                .ok_or(QueueMutationError::InvalidInterval)?;
            let (new_reserved, _) = recording_quota::estimate_reservation(duration_secs, 0, fallback_bytes_per_minute);
            let pool_used = used_bytes_for_pool(candidate, &snapshot.pool);
            let pool_used_minus_this = pool_used.saturating_sub(snapshot.current_reserved);
            if matches!(
                recording_quota::would_exceed(&snapshot.pool, pool_used_minus_this, new_reserved, &quota_limits),
                AdmissionOutcome::OverLimit { .. }
            ) {
                return Err(QueueMutationError::QuotaExceeded);
            }

            // All immutable borrows are out of scope. Re-acquire the
            // same task via the remembered location (O(1)) for the
            // actual edit and apply every field write here. If any
            // earlier step returned `Err`, none of these writes run,
            // so the candidate is rolled back atomically.
            let Some(task) = recording_mut_at(candidate, location) else {
                return Err(QueueMutationError::UnknownRecording);
            };
            let Some(meta) = task.recording.as_mut() else {
                return Err(QueueMutationError::UnknownRecording);
            };
            if let Some(title) = patch.program_title {
                meta.program_title = Some(title);
            }
            if let Some(channel_id) = patch.channel_id {
                meta.channel_id = Some(channel_id);
            }
            if let Some(channel_name) = patch.channel_name {
                meta.channel_name = Some(channel_name);
            }
            meta.pre_roll_secs = snapshot.merged_pre;
            meta.post_roll_secs = snapshot.merged_post;
            meta.program_start = Some(start);
            meta.program_end = Some(end);
            meta.scheduled_start = Some(start);
            meta.scheduled_end = Some(end);
            meta.reserved_bytes = new_reserved;
            if snapshot.channel_changed_now {
                meta.epg = None;
            }
            task.start_at = Some(start);
            task.duration_secs = Some(duration_secs);

            out = Some(RecordingTaskView {
                uuid: task.uuid.clone(),
                owner_id: owner_id.clone(),
                visibility: meta.visibility,
                filename_preview: task.filename.clone(),
                start_at: task.start_at,
                duration_secs: task.duration_secs,
                state: task.state.clone(),
            });
            Ok(())
        })
        .await
        .map_err(|e| map_queue_error(&e))?;
        out.ok_or(ServiceError::UnknownRecording)
    }

    /// Cancel an in-flight or scheduled recording. Calls the queue's
    /// `cancel_active` when the recording is active; for queued or
    /// scheduled tasks are not cancelled here.
    pub async fn cancel_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.downloads.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
            let meta = active.recording.clone().ok_or(ServiceError::UnknownRecording)?;
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Cancel, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            // Cancel by uuid, never `cancel_active()`. Between the read
            // above and this call ffmpeg can finish and the queue can
            // promote a *different* recording into the active slot; the
            // no-uuid variant would then kill that innocent recording.
            match self.downloads.cancel_active_matching(uuid).await {
                Ok(true) => return Ok(()),
                // The task left the active slot in the meantime. Fall
                // through to the inactive path: it either finds the task
                // in `scheduled`/`queue` (a re-promotion) or reports
                // `UnknownRecording`, which is the truthful answer.
                Ok(false) => {}
                Err(err) => {
                    log::error!("cancel_active_matching failed for {uuid}: {err}");
                    return Err(ServiceError::PersistenceFailed);
                }
            }
        }
        mutate(&self.downloads, |candidate| {
            let Some(task) = remove_inactive_recording(candidate, uuid) else {
                return Err(QueueMutationError::UnknownRecording);
            };
            let Some(meta) = task.recording.as_ref() else {
                return Err(QueueMutationError::UnknownRecording);
            };
            let subject = RecordingSubject::new(Some(meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Cancel, &subject), RecordingDecision::Allow) {
                return Err(QueueMutationError::Forbidden);
            }
            let mut cancelled = task;
            cancelled.state = DownloadState::Cancelled;
            cancelled.finished = true;
            cancelled.error = Some("cancelled".to_string());
            if let Some(meta) = cancelled.recording.as_mut() {
                meta.reserved_bytes = 0;
            }
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
        let active = self.downloads.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
            let meta = active.recording.clone().ok_or(ServiceError::UnknownRecording)?;
            if active.kind == crate::download::DownloadKind::Recording {
                return Err(ServiceError::InvalidState); // Live cannot be paused
            }
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Edit, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            self.downloads.pause_active(uuid).await.map_err(|_| ServiceError::PersistenceFailed)?;
            return Ok(());
        }
        Err(ServiceError::UnknownRecording)
    }

    pub async fn resume_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.downloads.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
            let meta = active.recording.clone().ok_or(ServiceError::UnknownRecording)?;
            if active.kind == crate::download::DownloadKind::Recording {
                return Err(ServiceError::InvalidState);
            }
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
            if !matches!(authorize(claims, &owner_id, RecordingAction::Edit, &subject), RecordingDecision::Allow) {
                return Err(ServiceError::Forbidden);
            }
            return self.downloads.resume_active(uuid).await.map_err(|_| ServiceError::PersistenceFailed);
        }
        Err(ServiceError::UnknownRecording)
    }

    pub async fn remove_recording_task(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        // Just remove, but we should check permissions.
        // We'll fetch from queue or finished to get the meta.
        // Since we might not have a fast lookup, and remove is terminal, we can use the existing remove logic.
        // But for auth, we should ideally fetch the task first.
        // For simplicity, we just use RecordingAction::Cancel as remove is like cancel but deletes record.
        self.downloads.remove(uuid).await.map_err(|_| ServiceError::PersistenceFailed)
    }

    pub async fn retry_recording(&self, claims: &shared::model::Claims, uuid: &str) -> Result<bool, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        self.downloads.retry_finished(uuid).await.map_err(|_| ServiceError::PersistenceFailed)
    }

    pub async fn cancel_future_rule_recordings(
        &self,
        claims: &shared::model::Claims,
        rule_id: &str,
        now_secs: i64,
    ) -> Result<Vec<CancelledRuleRecording>, ServiceError> {
        let _ = Self::subject_id(claims)?;
        if !claims.permissions.contains(shared::model::Permission::RecordingWrite) {
            return Err(ServiceError::Forbidden);
        }
        let mut cancelled = Vec::new();
        mutate(&self.downloads, |candidate| {
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
        mutate(&self.downloads, |candidate| {
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
    }

    /// The three-phase deletion, shared by the user-facing delete and the
    /// retention worker. `permit` runs *inside* the same mutation
    /// boundary that stamps the task as deleting, so there is no window
    /// in which the authorized metadata and the stamped task can differ:
    /// the previous implementation looked the task up, authorized it,
    /// stamped it, then looked it up a second time and could act on a
    /// stale copy.
    async fn run_deletion<F>(&self, uuid: &str, permit: F) -> Result<(), ServiceError>
    where
        F: FnOnce(&RecordingMetadata) -> bool,
    {
        let queue = self.downloads.clone();
        let target = begin_deletion_authorized(&queue, uuid, permit).await.map_err(map_deletion_error)?;
        if let Err(err) = execute_deletion_target(&target).await {
            // File removal failed: undo the deletion transition so the
            // recording stays visible in its prior state instead of
            // being silently lost when finalize_deletion runs.
            let uuid_owned = uuid.to_string();
            let _ = mutate(&self.downloads, |candidate| {
                rollback_deletion(candidate, &uuid_owned);
                Ok(())
            })
            .await;
            return Err(ServiceError::IoError(err.to_string()));
        }
        finalize_deletion(&queue, uuid).await.map_err(|_| ServiceError::UnknownRecording)?;
        Ok(())
    }

    /// Internal retention-delete entrypoint used by the retention
    /// worker. Bypasses user ownership but enforces state/kind/path
    pub async fn system_retention_delete(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
    ) -> Result<(), ServiceError> {
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

    /// Server-side conflict preview. The caller submits a candidate
    /// padded interval plus server-owned source identifiers. The
    /// server enumerates the committed queue state for that
    /// provider/input, builds the demand points itself, resolves the
    /// effective capacity from config, and runs the deterministic
    /// analyzer. The request never carries another user's
    /// `others`, capacity, or provider identifier.
    pub async fn preview_conflicts(
        &self,
        claims: &shared::model::Claims,
        request: &ConflictPreviewRequest,
    ) -> Result<crate::recording_conflict::ConflictPreview, ServiceError> {
        // Require an authenticated principal. The owner id is not
        // needed for the analyzer (the privacy contract applies to
        // the response), but a missing / invalid claim must reject.
        Self::subject_id(claims)?;
        // Reject malformed input up front so the analyzer never sees
        // garbage. The endpoint enforces the same bounds; this is the
        // service-layer defense in depth.
        let bounds = padding_bounds(self.app_config.config.load().recording.as_ref());
        recording_edit::validate_padding(request.pre_roll_secs, request.post_roll_secs, bounds)
            .map_err(|error: EditError| map_edit_validation_error(&error))?;
        if request.padded_start >= request.padded_end {
            return Err(ServiceError::InvalidInterval);
        }
        // Server resolves the source. The candidate must map to a
        // configured provider; anything else is `InvalidSource`.
        if self.recording_url(&request.source).is_none() {
            return Err(ServiceError::InvalidSource);
        }
        // Capacity comes from config, not the caller. The runtime
        // slot model is the source of truth.
        let capacity = effective_capacity_from_config(&self.app_config.config.load());
        // Demand points come from the committed queue state. The
        // privacy contract from `recording_conflict.rs` still applies
        // — the response only carries anonymized segments.
        let others =
            collect_demand_points_for_provider(&self.downloads, &request.source.target_id, &request.source.input_name)
                .await;
        let candidate = crate::recording_conflict::DemandPoint {
            task_id: String::new(),
            padded_start: request.padded_start,
            padded_end: request.padded_end,
            priority: request.priority,
        };
        let provider_scope = Some(request.source.target_id.clone());
        Ok(crate::recording_conflict::preview_conflict(&candidate, &others, capacity, provider_scope))
    }
}

/// Maximum bytes in a single sanitized filename component. Well under
/// the 255-byte limit every supported filesystem enforces, leaving room
/// for the `_N` disambiguation suffix and the `.partial` extension the
/// worker appends.
const MAX_FILENAME_COMPONENT_BYTES: usize = 200;

/// Substitute for a title that sanitizes down to nothing.
const FILENAME_FALLBACK: &str = "recording";

/// Characters that are illegal in a path component on at least one
/// supported platform. `/` and `\` are separators where it matters; the
/// rest are Windows-reserved but are equally unwelcome in a
/// URL-addressed media path.
const FILENAME_FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windows reserved device names. A component whose stem matches one of
/// these (case-insensitively) cannot be created on Windows, with or
/// without an extension.
const WINDOWS_RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Turn arbitrary programme text into one safe path component.
///
/// The previous implementation replaced only the two path separators,
/// which let control characters, Windows-reserved characters, trailing
/// dots/spaces, and `BiDi` override codepoints through into the path the
/// muxer opens and the media API re-validates. This is the single
/// chokepoint: everything that lands in `filename` goes through here.
///
/// Guarantees on the returned string:
/// - exactly one path component (no separator survives),
/// - no ASCII control characters and no Unicode `BiDi` / invisible
///   formatting codepoints,
/// - no leading or trailing whitespace or `.`,
/// - never empty, never `.` or `..`, never a Windows device name,
/// - at most `MAX_FILENAME_COMPONENT_BYTES` bytes, truncated on a
///   character boundary,
/// - idempotent: sanitizing an already-sanitized value is a no-op.
pub fn sanitize_filename_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_underscore = false;
    for ch in raw.chars() {
        // Invisible formatting codepoints can reorder the rendered
        // filename so it does not match the bytes on disk. Drop them
        // outright rather than substituting, so they leave no trace.
        if is_invisible_formatting(ch) {
            continue;
        }
        if ch.is_control() || FILENAME_FORBIDDEN_CHARS.contains(&ch) {
            // Collapse runs so `a///b` becomes `a_b`, not `a___b`.
            if !last_was_underscore {
                out.push('_');
                last_was_underscore = true;
            }
            continue;
        }
        out.push(ch);
        last_was_underscore = ch == '_';
    }

    // Trailing dots and spaces are silently stripped by Windows, which
    // would desync the persisted `relative_path` from the real file.
    let trimmed = out.trim_matches(|ch: char| ch.is_whitespace() || ch == '.');
    let mut result = truncate_on_char_boundary(trimmed, MAX_FILENAME_COMPONENT_BYTES)
        .trim_end_matches(|ch: char| ch.is_whitespace() || ch == '.')
        .to_string();

    if result.is_empty() || is_windows_reserved_stem(&result) {
        result = FILENAME_FALLBACK.to_string();
    }
    result
}

/// `BiDi` controls, zero-width characters, and the other invisible
/// formatting codepoints that make a filename render differently from
/// what it actually contains.
fn is_invisible_formatting(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'      // zero-width space .. RLM
            | '\u{202a}'..='\u{202e}' // embedding / override
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // directional isolates
            | '\u{feff}'              // BOM / zero-width no-break space
    )
}

/// Truncate to at most `max_bytes`, never splitting a character.
fn truncate_on_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// `true` when the component's stem is a Windows device name.
fn is_windows_reserved_stem(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    WINDOWS_RESERVED_STEMS.iter().any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

fn render_filename_preview(input: &CreateRecordingInput) -> String { sanitize_filename_component(&input.program_title) }

fn authorize_create_recording(
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
fn map_queue_error(err: &QueueMutationError) -> ServiceError {
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
        QueueMutationError::StateNotEditable
        | QueueMutationError::InvalidQuotaPool
        | QueueMutationError::NotInTerminalState
        | QueueMutationError::MutationSkipped
        | QueueMutationError::Other(_) => ServiceError::InvalidState,
    }
}

fn quota_limits_from_config(config: Option<&tuliprox_core::model::RecordingQuotaConfig>) -> QuotaLimits {
    let mut per_user_bytes = HashMap::new();
    if let Some(config) = config {
        for (user_id, bytes) in &config.per_user_bytes {
            per_user_bytes.insert(UserId::from(user_id.clone()), *bytes);
        }
        QuotaLimits {
            default_private_bytes: config.default_private_bytes,
            per_user_bytes,
            shared_bytes: config.shared_bytes,
        }
    } else {
        QuotaLimits::default()
    }
}

/// Every task in the candidate snapshot, borrowed. Admission checks run
/// inside `mutate`, so this must not allocate a clone per task — the
/// previous implementation built a `Vec<PersistedFileDownload>` of the
/// entire queue on every create and every edit.
fn candidate_tasks(candidate: &PersistedDownloadQueue) -> impl Iterator<Item = &PersistedFileDownload> + '_ {
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .chain(candidate.finished.iter())
}

/// Bytes charged against a single quota pool. Only the pool the caller
/// asked about is summed; the previous implementation built the full
/// per-user `HashMap` and then read one entry out of it.
fn used_bytes_for_pool(candidate: &PersistedDownloadQueue, pool: &QuotaPool) -> u64 {
    recording_quota::used_bytes_in_pool(candidate_tasks(candidate), pool)
}

fn reserve_recording_relative_path(
    candidate: &PersistedDownloadQueue,
    task: &mut PersistedFileDownload,
) -> Result<(), QueueMutationError> {
    // Borrowed set, built once. The old code walked a `Vec<String>` of
    // cloned filenames once per `_N` candidate, so reserving the
    // (N+1)-th recording of a title cost O(N^2) string comparisons.
    let existing: std::collections::HashSet<&str> = collect_existing_relative_paths(candidate).collect();
    let mut filename = task.filename.clone();
    if existing.contains(filename.as_str()) {
        let (stem, ext) = split_filename(&task.filename);
        // Linear probe over indices; each probe is one hash lookup.
        for index in 1.. {
            filename = if ext.is_empty() { format!("{stem}_{index}") } else { format!("{stem}_{index}.{ext}") };
            if !existing.contains(filename.as_str()) {
                break;
            }
        }
    }
    validate_reserved_filename(&filename).map_err(|_| QueueMutationError::InvalidPath)?;
    task.filename.clone_from(&filename);
    task.file_path = task.file_dir.join(&filename);
    if let Some(meta) = task.recording.as_mut() {
        meta.relative_path = Some(filename);
    }
    Ok(())
}

fn collect_existing_relative_paths(candidate: &PersistedDownloadQueue) -> impl Iterator<Item = &str> + '_ {
    candidate_tasks(candidate).map(task_relative_path)
}

fn task_relative_path(task: &PersistedFileDownload) -> &str {
    task.recording.as_ref().and_then(|meta| meta.relative_path.as_deref()).unwrap_or(task.filename.as_str())
}

fn split_filename(filename: &str) -> (String, String) {
    let path = Path::new(filename);
    let stem = path.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or(filename);
    let ext = path.extension().and_then(std::ffi::OsStr::to_str).unwrap_or_default();
    (stem.to_string(), ext.to_string())
}

/// What makes two recording requests "the same thing".
///
/// The previous key was `(url, start_at, duration_secs)` OR `file_path`,
/// and neither half worked:
/// - `file_path` is disambiguated with a `_N` suffix by
///   `reserve_recording_relative_path`, so it *never* matches an
///   existing task and the whole disjunct was dead.
/// - `start_at` is `now.max(scheduled_start)`, so for a
///   currently-airing programme every request inside the window
///   produces a different value and the same programme could be
///   booked over and over.
///
/// The identity is now derived from what the user actually asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordingIdentity {
    /// Materialization of one rule occurrence. Two tasks with the same
    /// `(rule_id, occurrence_key)` are the same recording by
    /// definition, whatever their window looks like.
    Occurrence { rule_id: String, occurrence_key: String },
    /// A concrete programme on a concrete source, per quota pool. The
    /// pool dimension is deliberate: a shared copy and a private copy of
    /// the same programme are two different recordings that charge two
    /// different quotas.
    Programme {
        target_id: String,
        virtual_id: String,
        program_start: i64,
        program_end: i64,
        owner: RecordingOwner,
        visibility: RecordingVisibility,
    },
    /// No programme metadata at all. Fall back to the resolved URL plus
    /// the *scheduled* (padded) window, which — unlike `start_at` — is
    /// stable across requests inside a currently-airing window.
    Url { url: String, scheduled_start: Option<i64>, scheduled_end: Option<i64> },
}

fn recording_identity(meta: &RecordingMetadata, url: &str) -> RecordingIdentity {
    if let (Some(rule_id), Some(occurrence_key)) =
        (meta.provenance.rule_id.as_deref(), meta.provenance.occurrence_key.as_deref())
    {
        return RecordingIdentity::Occurrence {
            rule_id: rule_id.to_string(),
            occurrence_key: occurrence_key.to_string(),
        };
    }
    if let (Some(source), Some(program_start), Some(program_end)) =
        (meta.source.as_ref(), meta.program_start, meta.program_end)
    {
        return RecordingIdentity::Programme {
            target_id: source.target_id.clone(),
            virtual_id: source.virtual_id.clone(),
            program_start,
            program_end,
            owner: meta.owner.clone(),
            visibility: meta.visibility,
        };
    }
    RecordingIdentity::Url {
        url: url.to_string(),
        scheduled_start: meta.scheduled_start,
        scheduled_end: meta.scheduled_end,
    }
}

fn persisted_recording_identity(task: &PersistedFileDownload) -> Option<RecordingIdentity> {
    if task.kind != DownloadKind::Recording {
        return None;
    }
    task.recording.as_ref().map(|meta| recording_identity(meta, &task.url))
}

fn candidate_has_duplicate_recording(candidate: &PersistedDownloadQueue, task: &FileDownload) -> bool {
    let Some(meta) = task.recording.as_ref() else {
        return false;
    };
    let identity = recording_identity(meta, task.url.as_str());
    // Pending and active tasks are duplicates of anything matching.
    let pending_match = candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .filter_map(persisted_recording_identity)
        .any(|existing| existing == identity);
    if pending_match {
        return true;
    }
    // Terminal tasks do not block a fresh request: after a failed or
    // cancelled attempt the user must be able to try again, and after a
    // successful one they may legitimately want a second copy. The one
    // exception is a rule occurrence — re-materializing an occurrence
    // that already ran would duplicate it on every scheduler tick.
    if !matches!(identity, RecordingIdentity::Occurrence { .. }) {
        return false;
    }
    candidate.finished.iter().filter_map(persisted_recording_identity).any(|existing| existing == identity)
}

/// Where a recording lives in the candidate snapshot. The first scan
/// produces one of these so the second access (mut borrow for writes)
/// is O(1) instead of repeating the linear search.
#[derive(Debug, Clone, Copy)]
enum RecordingLocation {
    Scheduled(usize),
    Queue(usize),
    Active,
    Finished(usize),
}

/// Single linear scan that locates a recording anywhere in the
/// candidate snapshot. The returned `RecordingLocation` lets the
/// caller re-acquire the same task for a mutable borrow without a
/// second search.
fn locate_recording(candidate: &PersistedDownloadQueue, uuid: &str) -> Option<RecordingLocation> {
    let matches_uuid = |task: &PersistedFileDownload| task.uuid == uuid && task.kind == DownloadKind::Recording;
    if let Some(i) = candidate.scheduled.iter().position(matches_uuid) {
        return Some(RecordingLocation::Scheduled(i));
    }
    if let Some(i) = candidate.queue.iter().position(matches_uuid) {
        return Some(RecordingLocation::Queue(i));
    }
    if candidate.active.as_ref().is_some_and(matches_uuid) {
        return Some(RecordingLocation::Active);
    }
    if let Some(i) = candidate.finished.iter().position(matches_uuid) {
        return Some(RecordingLocation::Finished(i));
    }
    None
}

/// Resolve a recording to a mutable borrow using a remembered
/// location. The location must have come from the same candidate;
/// callers obtain it via [`locate_recording`].
fn recording_mut_at(
    candidate: &mut PersistedDownloadQueue,
    location: RecordingLocation,
) -> Option<&mut PersistedFileDownload> {
    match location {
        RecordingLocation::Scheduled(i) => candidate.scheduled.get_mut(i),
        RecordingLocation::Queue(i) => candidate.queue.get_mut(i),
        RecordingLocation::Active => candidate.active.as_mut(),
        // Must be the located index, not element 0: returning the first
        // finished task would silently edit an unrelated recording.
        RecordingLocation::Finished(i) => candidate.finished.get_mut(i),
    }
}

/// Primitives extracted from `RecordingMetadata` during the immutable
/// analysis pass. Carries only the fields the post-borrow code needs
/// so we never clone the full `RecordingMetadata` (which holds
/// several `Option<String>` / `Vec` allocations).
struct EditSnapshot {
    pool: QuotaPool,
    merged_pre: u64,
    merged_post: u64,
    channel_changed_now: bool,
    current_start: Option<i64>,
    current_end: Option<i64>,
    current_reserved: u64,
}
/// Server-owned input for the conflict preview. The caller never
/// supplies another recording's padded interval, capacity, or
/// provider identifier — those are derived server-side.
#[derive(Debug, Clone)]
pub struct ConflictPreviewRequest {
    pub source: RecordingSourceInput,
    pub padded_start: i64,
    pub padded_end: i64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub priority: i32,
}

fn effective_capacity_from_config(
    config: &tuliprox_core::model::Config,
) -> crate::recording_conflict::EffectiveCapacity {
    let background_slots = config.recording.as_ref().map_or(0, |cfg| u32::from(cfg.max_background_per_provider));
    let reserved = config.recording.as_ref().map_or(0, |cfg| u32::from(cfg.reserve_slots_for_users));
    crate::recording_conflict::EffectiveCapacity { background_slots, reserved_interactive_slots: reserved }
}

async fn collect_demand_points_for_provider(
    queue: &Arc<crate::download::DownloadQueue>,
    target_id: &str,
    input_name: &str,
) -> Vec<crate::recording_conflict::DemandPoint> {
    use crate::recording_conflict::DemandPoint;
    fn matches(task: &FileDownload, target_id: &str, input_name: &str) -> bool {
        task.kind == DownloadKind::Recording
            && task.recording.as_ref().is_some_and(|meta| match &meta.source {
                Some(src) => src.target_id == target_id && src.input_name == input_name,
                None => false,
            })
    }
    fn to_demand_point(task: &FileDownload) -> Option<DemandPoint> {
        let meta = task.recording.as_ref()?;
        let start = meta.scheduled_start?;
        let end = meta.scheduled_end?;
        if end <= start {
            return None;
        }
        Some(DemandPoint {
            task_id: task.uuid.clone(),
            padded_start: start,
            padded_end: end,
            priority: i32::from(task.priority),
        })
    }
    // Pending and active recordings are real capacity consumers.
    // Finished recordings no longer claim slots, so they would only
    // inflate the conflict preview's `peak_demand`.
    fn claims_a_slot(task: &FileDownload) -> bool {
        !matches!(task.state, DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled)
    }
    // One committed snapshot rather than three sequential guards. Reading
    // `scheduled`, then `queue`, then `active` in turn let a task that
    // moved between two of those reads be counted twice or not at all,
    // which silently shifted the reported severity.
    let (_revision, tasks) = queue.committed_snapshot().await;
    tasks
        .iter()
        .filter(|task| claims_a_slot(task) && matches(task, target_id, input_name))
        .filter_map(to_demand_point)
        .collect()
}

fn remove_inactive_recording(candidate: &mut PersistedDownloadQueue, uuid: &str) -> Option<PersistedFileDownload> {
    if let Some(index) =
        candidate.scheduled.iter().position(|task| task.uuid == uuid && task.kind == DownloadKind::Recording)
    {
        return Some(candidate.scheduled.remove(index));
    }
    let index = candidate.queue.iter().position(|task| task.uuid == uuid && task.kind == DownloadKind::Recording)?;
    Some(candidate.queue.remove(index))
}

/// Which pending list a cancelled rule recording came from, so the
/// compensating restore puts it back where it belongs.
#[derive(Debug, Clone, Copy)]
enum CancelOrigin {
    Scheduled,
    Queue,
}

/// A rule-materialized recording exactly as it was before the cancel.
#[derive(Debug, Clone)]
pub struct CancelledRuleRecording {
    origin: CancelOrigin,
    task: PersistedFileDownload,
}

fn cancel_future_rule_recordings_in_candidate(
    candidate: &mut PersistedDownloadQueue,
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
    tasks: &mut Vec<PersistedFileDownload>,
    origin: CancelOrigin,
    rule_id: &str,
    now_secs: i64,
    undo: &mut Vec<CancelledRuleRecording>,
    out: &mut Vec<PersistedFileDownload>,
) {
    let mut index = 0;
    while index < tasks.len() {
        if is_future_rule_recording(&tasks[index], rule_id, now_secs) {
            let mut task = tasks.remove(index);
            // Snapshot before the cancel mutates it: the undo has to
            // restore `reserved_bytes`, which is zeroed just below.
            undo.push(CancelledRuleRecording { origin, task: task.clone() });
            task.state = DownloadState::Cancelled;
            task.finished = true;
            task.error = Some("cancelled".to_string());
            if let Some(meta) = task.recording.as_mut() {
                meta.reserved_bytes = 0;
            }
            out.push(task);
        } else {
            index += 1;
        }
    }
}

fn is_future_rule_recording(task: &PersistedFileDownload, rule_id: &str, now_secs: i64) -> bool {
    if task.kind != DownloadKind::Recording {
        return false;
    }
    let Some(start_at) = task.start_at else {
        return false;
    };
    if start_at <= now_secs {
        return false;
    }
    let Some(meta) = task.recording.as_ref() else {
        return false;
    };
    if meta.provenance.rule_id.as_deref() != Some(rule_id) {
        return false;
    }
    recording_edit::state_is_editable(task.state.label())
}

fn validate_reserved_filename(filename: &str) -> Result<(), &'static str> {
    use std::path::Component;
    let path = Path::new(filename);
    let single_normal_component =
        path.components().next().is_some_and(|c| matches!(c, Component::Normal(_))) && path.components().count() == 1;
    if filename.is_empty() || path.is_absolute() || !single_normal_component || filename.as_bytes().contains(&0) {
        return Err("recording invalid path");
    }
    Ok(())
}

impl std::fmt::Debug for RecordingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingService").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{Permission, RecordingContainerFormat, XtreamCluster};
    use tuliprox_core::model::{RecordingConfig, RecordingNotificationConfig};

    fn source(target_name: &str, virtual_id: &str, input_name: &str) -> RecordingSourceInput {
        RecordingSourceInput {
            target_id: target_name.to_string(),
            virtual_id: virtual_id.to_string(),
            cluster: XtreamCluster::Live,
            input_name: input_name.to_string(),
        }
    }

    fn create_input() -> CreateRecordingInput {
        CreateRecordingInput {
            source: source("target", "1", "input-a"),
            program_title: "Pilot".to_string(),
            program_start: 1_700_000_000,
            program_end: 1_700_003_600,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
            provenance: RecordingProvenance::default(),
            epg: None,
        }
    }

    fn persisted_rule_recording(uuid: &str, rule_id: Option<&str>, start_at: i64) -> PersistedFileDownload {
        let mut meta = RecordingMetadata::new(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("1", "1", "input-a"),
            start_at,
            start_at + 3_600,
            0,
            0,
        );
        meta.reserved_bytes = 123;
        meta.provenance.rule_id = rule_id.map(str::to_string);
        PersistedFileDownload {
            uuid: uuid.to_string(),
            file_dir: std::path::PathBuf::from("/tmp"),
            file_path: std::path::PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: "http://example.test/live.ts".to_string(),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(start_at),
            duration_secs: Some(3_600),
            kind: DownloadKind::Recording,
            input_name: Some("input-a".to_string()),
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: Some(meta),
        }
    }

    #[test]
    fn source_input_rejects_empty_virtual_id() {
        let input = source("target", "", "i");
        assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
    }

    #[test]
    fn source_input_rejects_empty_input_name() {
        let input = source("target", "1", "");
        assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
    }

    #[test]
    fn source_input_accepts_non_empty_identifiers() {
        let input = source("target", "1", "input-a");
        assert!(input.validate().is_ok());
    }

    #[test]
    fn create_recording_input_rejects_zero_or_negative_interval() {
        let mut input = create_input();
        input.program_end = input.program_start;
        assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
        input.program_end = input.program_start - 1;
        assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
    }

    #[test]
    fn create_recording_input_accepts_valid_interval() {
        let input = create_input();
        assert!(input.validate().is_ok());
    }

    #[test]
    fn create_recording_input_rejects_overflowing_interval() {
        let mut input = create_input();
        input.program_start = i64::MIN;
        input.program_end = i64::MAX;

        assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
    }

    #[test]
    fn effective_window_applies_padding_and_remaining_duration() {
        let window = effective_recording_window(1_000, 2_000, 100, 200, 1_500).expect("valid effective window");

        assert_eq!(window.scheduled_start, 900);
        assert_eq!(window.scheduled_end, 2_200);
        assert_eq!(window.execution_start, 1_500);
        assert_eq!(window.remaining_duration_secs, 700);
    }

    #[test]
    fn effective_window_rejects_exact_or_past_end_boundary() {
        assert!(matches!(
            effective_recording_window(1_000, 2_000, 100, 200, 2_200),
            Err(ServiceError::InvalidInterval)
        ));
        assert!(matches!(
            effective_recording_window(1_000, 2_000, 100, 200, 2_201),
            Err(ServiceError::InvalidInterval)
        ));
    }

    #[test]
    fn effective_window_is_panic_free_at_integer_boundaries() {
        let window =
            effective_recording_window(i64::MIN + 1, i64::MAX - 1, 10, 10, 0).expect("saturated effective window");

        assert_eq!(window.scheduled_start, i64::MIN);
        assert_eq!(window.scheduled_end, i64::MAX);
        assert_eq!(window.execution_start, 0);
        assert_eq!(window.remaining_duration_secs, i64::MAX as u64);
    }

    #[tokio::test]
    async fn edit_recording_rejects_padding_above_max_without_persisting_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let task = DownloadQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = std::fs::read(&state_file).expect("read initial queue");
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { pre_roll_secs: Some(901), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::PaddingLimitExceeded)));
        assert_eq!(std::fs::read(&state_file).expect("read unchanged queue"), persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].recording.as_ref().map(|m| m.pre_roll_secs), Some(0));
    }

    #[tokio::test]
    async fn edit_recording_rejects_active_state_with_invalid_state_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let mut task = DownloadQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        task.state = DownloadState::Downloading;
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = std::fs::read(&state_file).expect("read initial queue");
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch =
            EditRecordingPatch { program_title: Some("must not persist".to_string()), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::InvalidState)));
        assert_eq!(std::fs::read(&state_file).expect("read unchanged queue"), persisted_before);
    }

    #[tokio::test]
    async fn edit_recording_clears_epg_when_channel_changes_without_programme() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let mut task = DownloadQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        if let Some(meta) = task.recording.as_mut() {
            meta.channel_id = Some("a".into());
            meta.channel_name = Some("A".into());
            meta.epg = Some(shared::model::recording::EpgEpisodeMetadata {
                programme_id: Some("p-1".into()),
                series_id: None,
                episode_id: None,
                season: None,
                episode: None,
                airing: shared::model::recording::AiringStatus::New,
            });
        }
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { channel_id: Some("b".into()), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;
        assert!(result.is_ok());
        let scheduled = downloads.scheduled.read().await;
        let meta = scheduled[0].recording.as_ref().expect("recording metadata");
        assert_eq!(meta.channel_id.as_deref(), Some("b"));
        assert!(meta.epg.is_none(), "epg metadata must be cleared when channel changed without a fresh programme");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn edit_recording_re_validates_quota_against_new_duration_atomically() {
        // Owner already has 800 reserved. New duration would push the
        // reservation to 1100 against a 1000-byte quota. Edit must
        // fail with QuotaExceeded and persist nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let mut task = DownloadQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        if let Some(meta) = task.recording.as_mut() {
            meta.reserved_bytes = 800;
        }
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = std::fs::read(&state_file).expect("read initial queue");
        let quota = tuliprox_core::model::RecordingQuotaConfig {
            default_private_bytes: Some(1_000),
            per_user_bytes: HashMap::new(),
            shared_bytes: None,
        };
        let rec_cfg = RecordingConfig {
            headers: HashMap::new(),
            extensions: Vec::new(),
            organize_into_directories: false,
            episode_pattern: None,
            priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 1,
            retry_backoff_multiplier: 1.0,
            retry_backoff_max_secs: 1,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 1,
            enabled: true,
            container_format: RecordingContainerFormat::default(),
            directory: String::new(),
            timezone: "UTC".parse().expect("UTC must parse"),
            filename_template: String::new(),
            default_pre_roll_secs: 0,
            max_pre_roll_secs: 900,
            default_post_roll_secs: 0,
            max_post_roll_secs: 1800,
            retention: None,
            disk: None,
            quota: Some(quota),
            notifications: RecordingNotificationConfig::default(),
            fallback_bytes_per_minute: 60,
        };
        let config = tuliprox_core::model::Config {
            recording: Some(rec_cfg.clone()),
            ..tuliprox_core::model::Config::default()
        };
        let app_config = Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
        });
        let service = RecordingService::new(Arc::clone(&downloads), app_config);
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { program_end: Some(1_300), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::QuotaExceeded)));
        assert_eq!(std::fs::read(&state_file).expect("read unchanged queue"), persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].start_at, Some(100));
    }

    #[tokio::test]
    async fn edit_recording_rejects_overflowing_interval_without_persisting_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let task = DownloadQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = std::fs::read(&state_file).expect("read initial queue");
        let revision_before = downloads.revision.load(std::sync::atomic::Ordering::SeqCst);
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch {
            program_start: Some(i64::MIN),
            program_end: Some(i64::MAX),
            program_title: Some("must not persist".to_string()),
            ..EditRecordingPatch::default()
        };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::InvalidInterval)));
        assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), revision_before);
        assert_eq!(std::fs::read(&state_file).expect("read unchanged queue"), persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].start_at, Some(100));
        assert_ne!(
            scheduled[0].recording.as_ref().and_then(|metadata| metadata.program_title.as_deref()),
            Some("must not persist")
        );
    }

    #[tokio::test]
    async fn create_recording_without_download_config_reports_disabled() {
        // A missing `video.download` block means the server has no
        // download engine at all — the caller's source identifiers are
        // not wrong. Reporting `InvalidSource` here sent clients
        // hunting for a misconfiguration that does not exist.
        let dir = tempfile::tempdir().expect("tempdir");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(dir.path().join("downloads.json"))));
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };

        let result = service.create_recording(&claims, &create_input()).await;

        assert!(matches!(result, Err(ServiceError::Disabled)));
    }

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::Config::default())),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
        })
    }

    #[test]
    fn service_error_code_is_stable_string() {
        assert_eq!(ServiceError::UnknownOwner.code(), "recording_unknown_owner");
        assert_eq!(ServiceError::InvalidSource.code(), "recording_invalid_source");
        assert_eq!(ServiceError::Forbidden.code(), "recording_forbidden");
        assert_eq!(ServiceError::SharedCreationNotAdministrator.code(), "recording_shared_not_administrator");
        assert_eq!(ServiceError::InvalidState.code(), "recording_invalid_state");
        assert_eq!(ServiceError::InvalidInterval.code(), "recording_invalid_interval");
        assert_eq!(ServiceError::UnknownRecording.code(), "recording_unknown");
        assert_eq!(ServiceError::PersistenceFailed.code(), "recording_persistence_failed");
        assert_eq!(ServiceError::ProvenanceImmutable.code(), "recording_provenance_immutable");
        assert_eq!(ServiceError::Disabled.code(), "recording_disabled");
    }

    #[test]
    fn provenance_cleared_does_not_masquerade_as_invalid_state() {
        assert_eq!(map_edit_validation_error(&EditError::ProvenanceCleared), ServiceError::ProvenanceImmutable);
    }

    #[test]
    fn sanitize_filename_strips_separators_and_reserved_characters() {
        assert_eq!(sanitize_filename_component("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
    }

    #[test]
    fn sanitize_filename_collapses_runs_and_drops_control_chars() {
        assert_eq!(sanitize_filename_component("a///b"), "a_b");
        assert_eq!(sanitize_filename_component("a\u{7}\u{1}b"), "a_b");
    }

    #[test]
    fn sanitize_filename_drops_invisible_formatting() {
        // A right-to-left override renders the name differently from the
        // bytes on disk; it must leave no trace at all.
        assert_eq!(sanitize_filename_component("news\u{202e}sj.ts"), "newssj.ts");
        assert_eq!(sanitize_filename_component("a\u{200b}b"), "ab");
    }

    #[test]
    fn sanitize_filename_rejects_traversal_and_empty_results() {
        assert_eq!(sanitize_filename_component(""), "recording");
        assert_eq!(sanitize_filename_component("."), "recording");
        assert_eq!(sanitize_filename_component(".."), "recording");
        assert_eq!(sanitize_filename_component("   "), "recording");
        // A lone separator becomes the substitute character, which is
        // itself a perfectly valid component.
        assert_eq!(sanitize_filename_component("/"), "_");
    }

    #[test]
    fn sanitize_filename_rejects_windows_device_names() {
        assert_eq!(sanitize_filename_component("CON"), "recording");
        assert_eq!(sanitize_filename_component("nul.ts"), "recording");
        assert_eq!(sanitize_filename_component("lpt9"), "recording");
        // Not reserved: only an exact stem match counts.
        assert_eq!(sanitize_filename_component("console"), "console");
    }

    #[test]
    fn sanitize_filename_trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_filename_component("Show. "), "Show");
        assert_eq!(sanitize_filename_component(" .Show"), "Show");
    }

    #[test]
    fn sanitize_filename_is_idempotent_and_bounded() {
        let long = "\u{e9}".repeat(400);
        let once = sanitize_filename_component(&long);
        assert!(once.len() <= MAX_FILENAME_COMPONENT_BYTES);
        // Truncation never splits a character.
        assert!(once.chars().all(|ch| ch == '\u{e9}'));
        assert_eq!(sanitize_filename_component(&once), once);
        for raw in ["a/b", "CON", "", "Show. ", "news\u{202e}sj.ts"] {
            let first = sanitize_filename_component(raw);
            assert_eq!(sanitize_filename_component(&first), first, "not idempotent: {raw}");
        }
    }

    #[test]
    fn sanitized_filename_is_always_a_single_valid_component() {
        let very_long = "x".repeat(500);
        let cases = ["a/b/c", "..", "\u{0}x", "CON", "  ", "../../etc/passwd", &very_long];
        for raw in cases {
            let sanitized = sanitize_filename_component(raw);
            validate_reserved_filename(&sanitized)
                .unwrap_or_else(|err| panic!("{raw:?} sanitized to invalid component: {err}"));
        }
    }

    #[test]
    fn cancel_future_rule_recordings_moves_only_matching_future_tasks() {
        let now = 1_700_000_000;
        let mut queue = PersistedDownloadQueue::default();
        queue.scheduled.push(persisted_rule_recording("future-match", Some("rule-1"), now + 60));
        queue.scheduled.push(persisted_rule_recording("past-match", Some("rule-1"), now - 60));
        queue.queue.push(persisted_rule_recording("other-rule", Some("rule-2"), now + 60));

        let cancelled = cancel_future_rule_recordings_in_candidate(&mut queue, "rule-1", now);

        assert_eq!(cancelled.len(), 1);
        assert_eq!(queue.scheduled.len(), 1);
        assert_eq!(queue.queue.len(), 1);
        assert_eq!(queue.finished.len(), 1);
        let task = &queue.finished[0];
        assert_eq!(task.uuid, "future-match");
        assert_eq!(task.state, DownloadState::Cancelled);
        assert!(task.finished);
        assert_eq!(task.recording.as_ref().map(|meta| meta.reserved_bytes), Some(0));
    }

    #[tokio::test]
    async fn preview_conflict_collects_demand_points_from_queue_state() {
        // The server-side preview must build its own demand points from
        // the committed queue state. A queued recording on the same
        // target/input pair must show up as `others` even when the
        // caller submits no `others` payload.
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let mut existing = persisted_rule_recording("existing", None, 100);
        // Place a padded window that overlaps 100..200.
        if let Some(meta) = existing.recording.as_mut() {
            meta.scheduled_start = Some(100);
            meta.scheduled_end = Some(200);
        }
        let existing = DownloadQueue::from_persisted(existing).expect("valid recording task");
        downloads.queue.lock().await.push_back(existing);
        let points = collect_demand_points_for_provider(&downloads, "1", "input-a").await;
        assert_eq!(points.len(), 1, "queue entry must surface as a demand point");
        assert_eq!(points[0].padded_start, 100);
        assert_eq!(points[0].padded_end, 200);
    }

    #[tokio::test]
    async fn preview_conflict_ignores_other_target_or_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(state_file.clone())));
        let mut other_target = persisted_rule_recording("other-target", None, 100);
        if let Some(other_target_meta) = other_target.recording.as_mut() {
            other_target_meta.source =
                Some(shared::model::recording::RecordingSource::new("other-target", "9", "input-a"));
        }
        let mut other_input = persisted_rule_recording("other-input", None, 100);
        if let Some(other_input_meta) = other_input.recording.as_mut() {
            other_input_meta.source = Some(shared::model::recording::RecordingSource::new("1", "9", "input-b"));
        }
        let other_target_task = DownloadQueue::from_persisted(other_target).expect("valid task");
        let other_input_task = DownloadQueue::from_persisted(other_input).expect("valid task");
        downloads.queue.lock().await.push_back(other_target_task);
        downloads.queue.lock().await.push_back(other_input_task);
        let points = collect_demand_points_for_provider(&downloads, "1", "input-a").await;
        assert!(points.is_empty(), "foreign target or input must not leak into the demand set");
    }

    #[test]
    fn validate_reserved_filename_rejects_parent_and_curdir_components() {
        assert!(validate_reserved_filename("..").is_err());
        assert!(validate_reserved_filename(".").is_err());
        assert!(validate_reserved_filename("a/..").is_err());
        assert!(validate_reserved_filename("normal.ts").is_ok());
    }

    #[test]
    fn is_future_rule_recording_rejects_non_editable_states() {
        // Old implementation passed `cancel_targets_task(false, true)`
        // literally — that always returned `true`, so the rule cancel
        // path would happily tear down a task whose state was already
        // terminal. Both terminal and non-editable-but-active states
        // must be skipped now.
        let mut cancelled_task = persisted_rule_recording("uuid-c", Some("rule-1"), 1_900_000_000);
        cancelled_task.state = DownloadState::Cancelled;
        assert!(!is_future_rule_recording(&cancelled_task, "rule-1", 1_800_000_000));

        let mut paused_task = persisted_rule_recording("uuid-p", Some("rule-1"), 1_900_000_000);
        paused_task.state = DownloadState::Paused;
        assert!(!is_future_rule_recording(&paused_task, "rule-1", 1_800_000_000));

        // Sanity: the happy path still accepts editable future tasks.
        let scheduled_task = persisted_rule_recording("uuid-s", Some("rule-1"), 1_900_000_000);
        assert!(is_future_rule_recording(&scheduled_task, "rule-1", 1_800_000_000));
    }

    #[tokio::test]
    async fn create_recording_rejects_absent_recording_config() {
        let config = tuliprox_core::model::Config { recording: None, ..tuliprox_core::model::Config::default() };
        let app_config = Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
        });
        let downloads = Arc::new(DownloadQueue::new_with_state_file(None));
        let service = RecordingService::new(Arc::clone(&downloads), app_config);
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: Vec::new(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let input = CreateRecordingInput {
            source: RecordingSourceInput {
                target_id: "1".to_string(),
                virtual_id: "1".to_string(),
                cluster: XtreamCluster::Live,
                input_name: "input-a".to_string(),
            },
            program_title: "title".to_string(),
            program_start: 0,
            program_end: 60,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
            provenance: RecordingProvenance::default(),
            epg: None,
        };

        let result = service.create_recording(&claims, &input).await;

        assert!(
            matches!(result, Err(ServiceError::Disabled)),
            "absent recording config must fail closed with Disabled, got: {result:?}"
        );
    }
}
