//! Recording mutation service.

use std::{collections::HashMap, path::Path, sync::Arc};

use crate::api::model::app_state::AppState;
use crate::api::endpoints::v1_api_playlist;
use crate::api::model::download::{
    mutate, DownloadKind, DownloadQueue, DownloadState, FileDownload, PersistedDownloadQueue,
    PersistedFileDownload, QueueMutationError,
};
use crate::api::model::recording_quota::{self, AdmissionOutcome, QuotaLimits, QuotaPool};
use crate::api::model::recording_edit::{self, EditError, PaddingBounds};
use crate::auth::{
    authorize, authorize_orphan, RecordingAction, RecordingDecision, RecordingSubject, TerminalState,
};
use crate::api::model::recording_deletion::{
    begin_deletion, execute_deletion, finalize_deletion, rollback_deletion,
};
use crate::model::AppConfig;
use shared::model::recording::{
    RecordingMetadata, RecordingOwner, RecordingProvenance, RecordingSource, RecordingVisibility,
};
use shared::model::{UserId, XtreamCluster};

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
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
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
    let pre_roll = i64::try_from(pre_roll_secs).map_err(|_| ServiceError::InvalidInterval)?;
    let post_roll = i64::try_from(post_roll_secs).map_err(|_| ServiceError::InvalidInterval)?;
    let scheduled_start = program_start.saturating_sub(pre_roll);
    let scheduled_end = program_end.saturating_add(post_roll);
    let execution_start = now.max(scheduled_start);
    let remaining_duration_secs = scheduled_end
        .checked_sub(execution_start)
        .and_then(|duration| u64::try_from(duration).ok())
        .filter(|duration| *duration > 0)
        .ok_or(ServiceError::InvalidInterval)?;
    Ok(EffectiveRecordingWindow {
        scheduled_start,
        scheduled_end,
        execution_start,
        remaining_duration_secs,
    })
}

fn padding_bounds(recording: Option<&crate::model::RecordingConfig>) -> PaddingBounds {
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
        _ => ServiceError::InvalidState,
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
/// so `AppState` does not carry a back-reference to the service.
pub struct RecordingService {
    downloads: Arc<DownloadQueue>,
    app_config: Arc<AppConfig>,
}

impl RecordingService {
    /// Construct from the queue and app config.
    pub fn new(downloads: Arc<DownloadQueue>, app_config: Arc<AppConfig>) -> Self {
        Self { downloads, app_config }
    }

    /// Convenience constructor from `Arc<AppState>`. Avoids a
    /// back-reference cycle by extracting the dependencies the
    /// service actually needs.
    pub fn from_app_state(app_state: &Arc<AppState>) -> Self {
        Self::new(app_state.downloads.clone(), app_state.app_config.clone())
    }

    fn subject_id(claims: &shared::model::Claims) -> Result<UserId, ServiceError> {
        claims.subject_id.clone().ok_or(ServiceError::UnknownOwner)
    }

    fn recording_url(&self, source: &RecordingSourceInput) -> Option<String> {
        let virtual_id = source.virtual_id.parse::<u32>().ok()?;
        v1_api_playlist::resolve_recording_target(&self.app_config, &source.target_id, &source.input_name)?;
        v1_api_playlist::build_recording_source_descriptor(
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
        let config = self.app_config.config.load();
        let Some(download_cfg) = config.video.as_ref().and_then(|v| v.download.as_ref()) else {
            return Err(ServiceError::InvalidSource);
        };
        let recording_cfg = download_cfg.recording.as_ref();
        recording_edit::validate_padding(
            input.pre_roll_secs,
            input.post_roll_secs,
            padding_bounds(recording_cfg),
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
        let priority = download_cfg.recording_priority;
        let filename = render_filename_preview(claims, input);
        let input_name: Option<Arc<str>> = (!input.source.input_name.trim().is_empty())
            .then(|| Arc::from(input.source.input_name.as_str()));
        let mut recording = FileDownload::new_recording(
            &url,
            &filename,
            download_cfg,
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
        let fallback_bytes_per_minute = recording_cfg.map_or(8 * 1024 * 1024, |cfg| cfg.fallback_bytes_per_minute);
        let (reserved_bytes, _) = recording_quota::estimate_reservation(duration_secs, 0, fallback_bytes_per_minute);
        meta.reserved_bytes = reserved_bytes;
        recording.recording = Some(meta);
        let mut persisted = DownloadQueue::to_persisted(&recording);
        let view_task = recording.clone();
        let quota_limits = quota_limits_from_config(recording_cfg.and_then(|cfg| cfg.quota.as_ref()));

        mutate(&self.downloads, |candidate| {
            reserve_recording_relative_path(candidate, &mut persisted)?;
            if candidate_has_duplicate_recording(candidate, &view_task) {
                return Err(QueueMutationError::Duplicate);
            }
            let pool = recording_quota::quota_pool_for_task(&persisted)
                .ok_or(QueueMutationError::InvalidQuotaPool)?;
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
        .map_err(|err| match err {
            QueueMutationError::QuotaExceeded => ServiceError::QuotaExceeded,
            QueueMutationError::Io(_) => ServiceError::PersistenceFailed,
            QueueMutationError::Duplicate
            | QueueMutationError::InvalidQuotaPool
            | QueueMutationError::InvalidPath
            | QueueMutationError::Forbidden
            | QueueMutationError::UnknownRecording
            | QueueMutationError::StateNotEditable
            | QueueMutationError::InvalidInterval
            | QueueMutationError::PaddingLimitExceeded
            | QueueMutationError::NotInTerminalState
            | QueueMutationError::DiskFull
            | QueueMutationError::MutationSkipped
            | QueueMutationError::Other(_) => ServiceError::InvalidState,
        })?;

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
        let recording_cfg = config
            .video
            .as_ref()
            .and_then(|v| v.download.as_ref())
            .and_then(|dl| dl.recording.as_ref());
        let bounds = padding_bounds(recording_cfg);
        let fallback_bytes_per_minute =
            recording_cfg.map_or(8 * 1024 * 1024, |cfg| cfg.fallback_bytes_per_minute);
        let quota_limits = quota_limits_from_config(recording_cfg.and_then(|cfg| cfg.quota.as_ref()));
        let mut out = None;
        mutate(&self.downloads, |candidate| {
            // Single linear scan: locate the recording, snapshot the
            // primitives we need for the immutable analysis, drop the
            // borrow, run the checks, then re-acquire the same task
            // via the remembered location for the O(1) write phase.
            let location = locate_recording(candidate, uuid)
                .ok_or(QueueMutationError::UnknownRecording)?;
            // State gate first — a patch on an immutable task
            // (Downloading / terminal / `Deleting`) reports
            // `recording_state_not_editable` regardless of any padding
            // / interval validity. Immutable locations also short-
            // circuit before the per-list lookup below.
            if !location.is_in_editable_list() {
                return Err(QueueMutationError::StateNotEditable);
            }
            let snapshot = {
                let task = match location {
                    RecordingLocation::Scheduled(i) => &candidate.scheduled[i],
                    RecordingLocation::Queue(i) => &candidate.queue[i],
                    RecordingLocation::Active | RecordingLocation::Finished => {
                        return Err(QueueMutationError::StateNotEditable);
                    }
                };
                if !recording_edit::state_is_editable(state_label_for(&task.state)) {
                    return Err(QueueMutationError::StateNotEditable);
                }
                let Some(meta_snapshot) = task.recording.as_ref() else {
                    return Err(QueueMutationError::UnknownRecording);
                };
                let pool = recording_quota::quota_pool_for_task(task)
                    .ok_or(QueueMutationError::InvalidQuotaPool)?;
                let subject = RecordingSubject::new(Some(meta_snapshot), TerminalState::Active, true);
                if !matches!(
                    authorize(claims, &owner_id, RecordingAction::Edit, &subject),
                    RecordingDecision::Allow
                ) {
                    return Err(QueueMutationError::Forbidden);
                }
                let merged_pre = patch.pre_roll_secs.unwrap_or(meta_snapshot.pre_roll_secs);
                let merged_post = patch.post_roll_secs.unwrap_or(meta_snapshot.post_roll_secs);
                recording_edit::validate_padding(merged_pre, merged_post, bounds)
                    .map_err(|error: EditError| map_edit_validation_error(&error))
                    .map_err(|_| QueueMutationError::PaddingLimitExceeded)?;
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
            let start = patch
                .program_start
                .or(snapshot.current_start)
                .ok_or(QueueMutationError::InvalidInterval)?;
            let end = patch
                .program_end
                .or(snapshot.current_end)
                .ok_or(QueueMutationError::InvalidInterval)?;
            if start >= end {
                return Err(QueueMutationError::InvalidInterval);
            }
            let duration_secs = end
                .checked_sub(start)
                .and_then(|duration| u64::try_from(duration).ok())
                .ok_or(QueueMutationError::InvalidInterval)?;
            let (new_reserved, _) =
                recording_quota::estimate_reservation(duration_secs, 0, fallback_bytes_per_minute);
            let pool_used = used_bytes_for_pool(candidate, &snapshot.pool);
            let pool_used_minus_this = pool_used.saturating_sub(snapshot.current_reserved);
            if matches!(
                recording_quota::would_exceed(
                    &snapshot.pool,
                    pool_used_minus_this,
                    new_reserved,
                    &quota_limits
                ),
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
        .map_err(|err| match err {
            QueueMutationError::UnknownRecording => ServiceError::UnknownRecording,
            QueueMutationError::Forbidden => ServiceError::Forbidden,
            QueueMutationError::InvalidInterval => ServiceError::InvalidInterval,
            QueueMutationError::PaddingLimitExceeded => ServiceError::PaddingLimitExceeded,
            QueueMutationError::QuotaExceeded => ServiceError::QuotaExceeded,
            QueueMutationError::Io(_) => ServiceError::PersistenceFailed,
            QueueMutationError::StateNotEditable
            | QueueMutationError::Duplicate
            | QueueMutationError::InvalidQuotaPool
            | QueueMutationError::InvalidPath
            | QueueMutationError::NotInTerminalState
            | QueueMutationError::DiskFull
            | QueueMutationError::MutationSkipped
            | QueueMutationError::Other(_) => ServiceError::InvalidState,
        })?;
        out.ok_or(ServiceError::UnknownRecording)
    }

    /// Cancel an in-flight or scheduled recording. Calls the queue's
    /// `cancel_active` when the recording is active; for queued or
    /// scheduled tasks are not cancelled here.
    pub async fn cancel_recording(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
    ) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let active = self.downloads.active.read().await.clone();
        if let Some(active) = active {
            if active.uuid == uuid {
                let meta = active
                    .recording
                    .clone()
                    .ok_or(ServiceError::UnknownRecording)?;
                let subject = RecordingSubject::new(Some(&meta), TerminalState::Active, true);
                if matches!(
                    authorize(claims, &owner_id, RecordingAction::Cancel, &subject),
                    RecordingDecision::Allow
                ) {
                    if let Err(err) = self.downloads.cancel_active().await {
                        log::error!("cancel_active failed for {uuid}: {err}");
                        return Err(ServiceError::PersistenceFailed);
                    }
                    return Ok(());
                }
                return Err(ServiceError::Forbidden);
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
            if !matches!(
                authorize(claims, &owner_id, RecordingAction::Cancel, &subject),
                RecordingDecision::Allow
            ) {
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
        .map_err(|err| match err {
            QueueMutationError::UnknownRecording => ServiceError::UnknownRecording,
            QueueMutationError::Forbidden => ServiceError::Forbidden,
            QueueMutationError::Io(_) => ServiceError::PersistenceFailed,
            QueueMutationError::StateNotEditable
            | QueueMutationError::InvalidInterval
            | QueueMutationError::PaddingLimitExceeded
            | QueueMutationError::QuotaExceeded
            | QueueMutationError::Duplicate
            | QueueMutationError::InvalidQuotaPool
            | QueueMutationError::InvalidPath
            | QueueMutationError::NotInTerminalState
            | QueueMutationError::DiskFull
            | QueueMutationError::MutationSkipped
            | QueueMutationError::Other(_) => ServiceError::InvalidState,
        })
    }

    /// Cancel future inactive recordings that were materialized from a
    /// recurring rule. Active recordings are intentionally left untouched.
    pub async fn cancel_future_rule_recordings(
        &self,
        claims: &shared::model::Claims,
        rule_id: &str,
        now_secs: i64,
    ) -> Result<usize, ServiceError> {
        let _ = Self::subject_id(claims)?;
        if !claims.permissions.contains(shared::model::Permission::RecordingWrite) {
            return Err(ServiceError::Forbidden);
        }
        let mut cancelled_count = 0;
        mutate(&self.downloads, |candidate| {
            cancelled_count = cancel_future_rule_recordings_in_candidate(candidate, rule_id, now_secs);
            Ok(())
        })
        .await
        .map_err(|err| match err {
            QueueMutationError::Io(_) => ServiceError::PersistenceFailed,
            QueueMutationError::UnknownRecording
            | QueueMutationError::StateNotEditable
            | QueueMutationError::Forbidden
            | QueueMutationError::InvalidInterval
            | QueueMutationError::PaddingLimitExceeded
            | QueueMutationError::QuotaExceeded
            | QueueMutationError::Duplicate
            | QueueMutationError::InvalidQuotaPool
            | QueueMutationError::InvalidPath
            | QueueMutationError::NotInTerminalState
            | QueueMutationError::DiskFull
            | QueueMutationError::MutationSkipped
            | QueueMutationError::Other(_) => ServiceError::InvalidState,
        })?;
        Ok(cancelled_count)
    }

    /// Delete a finished recording via the three-step service.
    /// Marks the task as `Deleting` (atomic), unlinks the file
    /// (outside the boundary), then removes the task (atomic).
    pub async fn delete_recording(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
    ) -> Result<(), ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let queue = self.downloads.clone();

        let decision = {
            let recording = lookup_recording(&queue, uuid)
                .await
                .ok_or(ServiceError::UnknownRecording)?;
            let meta = recording
                .recording
                .clone()
                .ok_or(ServiceError::UnknownRecording)?;
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
            authorize(claims, &owner_id, RecordingAction::Delete, &subject)
        };
        if !matches!(decision, RecordingDecision::Allow) {
            return Err(ServiceError::Forbidden);
        }

        begin_deletion(&queue, uuid)
            .await
            .map_err(|_| ServiceError::UnknownRecording)?;
        let recording = lookup_recording(&queue, uuid)
            .await
            .ok_or(ServiceError::UnknownRecording)?;
        if let Err(err) = execute_deletion(&recording, None).await {
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
        finalize_deletion(&queue, uuid)
            .await
            .map_err(|_| ServiceError::UnknownRecording)?;
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
        let queue = self.downloads.clone();

        let decision = {
            let recording = lookup_recording(&queue, uuid)
                .await
                .ok_or(ServiceError::UnknownRecording)?;
            let meta = recording
                .recording
                .clone()
                .ok_or(ServiceError::UnknownRecording)?;
            let subject = RecordingSubject::new(Some(&meta), TerminalState::Completed, true);
            authorize(claims, &owner_id, RecordingAction::SystemRetentionDelete, &subject)
        };
        if !matches!(decision, RecordingDecision::Allow) {
            return Err(ServiceError::Forbidden);
        }

        begin_deletion(&queue, uuid)
            .await
            .map_err(|_| ServiceError::UnknownRecording)?;
        let recording = lookup_recording(&queue, uuid)
            .await
            .ok_or(ServiceError::UnknownRecording)?;
        if let Err(err) = execute_deletion(&recording, None).await {
            let uuid_owned = uuid.to_string();
            let _ = mutate(&self.downloads, |candidate| {
                rollback_deletion(candidate, &uuid_owned);
                Ok(())
            })
            .await;
            return Err(ServiceError::IoError(err.to_string()));
        }
        finalize_deletion(&queue, uuid)
            .await
            .map_err(|_| ServiceError::UnknownRecording)?;
        Ok(())
    }

    /// Re-export the orphan policy for callers that need it.
    pub fn authorize_orphan_read(
        &self,
        claims: &shared::model::Claims,
    ) -> Result<(), ServiceError> {
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
    ) -> Result<crate::api::model::recording_conflict::ConflictPreview, ServiceError> {
        // Require an authenticated principal. The owner id is not
        // needed for the analyzer (the privacy contract applies to
        // the response), but a missing / invalid claim must reject.
        Self::subject_id(claims)?;
        // Reject malformed input up front so the analyzer never sees
        // garbage. The endpoint enforces the same bounds; this is the
        // service-layer defense in depth.
        let bounds = padding_bounds(
            self.app_config
                .config
                .load()
                .video
                .as_ref()
                .and_then(|v| v.download.as_ref())
                .and_then(|dl| dl.recording.as_ref()),
        );
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
        let others = collect_demand_points_for_provider(
            &self.downloads,
            &request.source.target_id,
            &request.source.input_name,
        )
        .await;
        let candidate = crate::api::model::recording_conflict::DemandPoint {
            task_id: String::new(),
            padded_start: request.padded_start,
            padded_end: request.padded_end,
            priority: request.priority,
        };
        let provider_scope = Some(request.source.target_id.clone());
        Ok(crate::api::model::recording_conflict::preview_conflict(
            &candidate,
            &others,
            capacity,
            provider_scope,
        ))
    }
}

/// Look up a recording in the queue by uuid. Awaits each guard in turn so
/// callers do not silently see `None` under transient lock contention.
async fn lookup_recording(queue: &Arc<crate::api::model::download::DownloadQueue>, uuid: &str) -> Option<FileDownload> {
    if let Some(d) = queue
        .queue
        .lock()
        .await
        .iter()
        .find(|d| d.uuid == uuid)
        .cloned()
    {
        return Some(d);
    }
    if let Some(d) = queue
        .scheduled
        .read()
        .await
        .iter()
        .find(|d| d.uuid == uuid)
        .cloned()
    {
        return Some(d);
    }
    if let Some(d) = queue
        .active
        .read()
        .await
        .as_ref()
        .filter(|d| d.uuid == uuid)
        .cloned()
    {
        return Some(d);
    }
    queue
        .finished
        .read()
        .await
        .iter()
        .find(|d| d.uuid == uuid)
        .cloned()
}

fn render_filename_preview(claims: &shared::model::Claims, input: &CreateRecordingInput) -> String {
    let _ = claims;
    input.program_title.replace(['/', '\\'], "_")
}

fn authorize_create_recording(
    claims: &shared::model::Claims,
    owner_id: &UserId,
    visibility: RecordingVisibility,
) -> Result<(), ServiceError> {
    let action = match visibility {
        RecordingVisibility::Private => RecordingAction::CreatePrivate,
        RecordingVisibility::Shared => RecordingAction::CreateShared,
    };
    match authorize(
        claims,
        owner_id,
        action,
        &RecordingSubject::new(None, TerminalState::Active, true),
    ) {
        RecordingDecision::Allow => Ok(()),
        RecordingDecision::Deny(crate::auth::DenyReason::NotAdministrator) => {
            Err(ServiceError::SharedCreationNotAdministrator)
        }
        RecordingDecision::Deny(_) => Err(ServiceError::Forbidden),
    }
}

fn quota_limits_from_config(config: Option<&crate::model::RecordingQuotaConfig>) -> QuotaLimits {
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

fn used_bytes_for_pool(candidate: &PersistedDownloadQueue, pool: &QuotaPool) -> u64 {
    let mut tasks = Vec::with_capacity(
        candidate.queue.len()
            + candidate.scheduled.len()
            + candidate.finished.len()
            + usize::from(candidate.active.is_some()),
    );
    tasks.extend(candidate.queue.iter().cloned());
    tasks.extend(candidate.scheduled.iter().cloned());
    if let Some(active) = &candidate.active {
        tasks.push(active.clone());
    }
    tasks.extend(candidate.finished.iter().cloned());
    let totals = recording_quota::compute_totals(&tasks);
    match pool {
        QuotaPool::Private(uid) => totals.private.get(uid).copied().unwrap_or(0),
        QuotaPool::Shared => totals.shared,
    }
}

fn reserve_recording_relative_path(
    candidate: &PersistedDownloadQueue,
    task: &mut PersistedFileDownload,
) -> Result<(), QueueMutationError> {
    let mut existing = Vec::new();
    collect_existing_relative_paths(candidate, &mut existing);
    let (stem, ext) = split_filename(&task.filename);
    let mut filename = task.filename.clone();
    if existing.iter().any(|path| path == &filename) {
        for index in 1.. {
            filename = if ext.is_empty() {
                format!("{stem}_{index}")
            } else {
                format!("{stem}_{index}.{ext}")
            };
            if !existing.iter().any(|path| path == &filename) {
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

fn collect_existing_relative_paths(candidate: &PersistedDownloadQueue, out: &mut Vec<String>) {
    for task in candidate.queue.iter().chain(candidate.scheduled.iter()).chain(candidate.finished.iter()) {
        collect_task_relative_path(task, out);
    }
    if let Some(task) = &candidate.active {
        collect_task_relative_path(task, out);
    }
}

fn collect_task_relative_path(task: &PersistedFileDownload, out: &mut Vec<String>) {
    if let Some(meta) = &task.recording {
        if let Some(path) = &meta.relative_path {
            out.push(path.clone());
            return;
        }
    }
    out.push(task.filename.clone());
}

fn split_filename(filename: &str) -> (String, String) {
    let path = Path::new(filename);
    let stem = path.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or(filename);
    let ext = path.extension().and_then(std::ffi::OsStr::to_str).unwrap_or_default();
    (stem.to_string(), ext.to_string())
}

fn candidate_has_duplicate_recording(candidate: &crate::api::model::download::PersistedDownloadQueue, task: &FileDownload) -> bool {
    fn same(candidate: &crate::api::model::download::PersistedFileDownload, task: &FileDownload) -> bool {
        candidate.kind == DownloadKind::Recording
            && ((candidate.url == task.url.as_str()
                && candidate.start_at == task.start_at
                && candidate.duration_secs == task.duration_secs)
                || candidate.file_path == task.file_path)
    }
    candidate.queue.iter().any(|d| same(d, task))
        || candidate.scheduled.iter().any(|d| same(d, task))
        || candidate.active.as_ref().is_some_and(|d| same(d, task))
        || candidate.finished.iter().any(|d| same(d, task))
}

/// Where a recording lives in the candidate snapshot. The first scan
/// produces one of these so the second access (mut borrow for writes)
/// is O(1) instead of repeating the linear search.
#[derive(Debug, Clone, Copy)]
enum RecordingLocation {
    Scheduled(usize),
    Queue(usize),
    Active,
    Finished,
}

impl RecordingLocation {
    /// True when the task is in `Scheduled` or `Queue` (the editable
    /// list). `Active` and `Finished` are not editable.
    fn is_in_editable_list(self) -> bool {
        matches!(self, Self::Scheduled(_) | Self::Queue(_))
    }
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
    if candidate.finished.iter().any(matches_uuid) {
        return Some(RecordingLocation::Finished);
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
        RecordingLocation::Finished => candidate.finished.first_mut(),
    }
}

/// Map a `DownloadState` to the label expected by
/// `recording_edit::state_is_editable`. `Deleting` is handled by the
/// caller (the boundary rejects any edit on a deleting task before it
/// reaches this helper).
fn state_label_for(state: &DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "Queued",
        DownloadState::Scheduled => "Scheduled",
        DownloadState::WaitingForCapacity => "WaitingForCapacity",
        DownloadState::RetryWaiting => "RetryWaiting",
        DownloadState::Downloading => "Downloading",
        DownloadState::Paused => "Paused",
        DownloadState::Completed => "Completed",
        DownloadState::Failed => "Failed",
        DownloadState::Cancelled => "Cancelled",
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
    config: &crate::model::Config,
) -> crate::api::model::recording_conflict::EffectiveCapacity {
    // Background slots come from the recording provider's
    // `max_background_per_provider`. Reserved interactive slots are
    // a coarse approximation of the number of users currently
    // streaming on the same provider; the analyzer treats the value
    // as a subtraction. When the provider cannot be resolved, fall
    // back to a zero headroom so the worst case is `LikelyMissedWindow`
    // and never a silent `NoKnownConflict`.
    let download_cfg = config.video.as_ref().and_then(|v| v.download.as_ref());
    let background_slots = download_cfg.map_or(0, |dl| u32::from(dl.max_background_per_provider));
    let reserved = u32::from(download_cfg.map_or(0, |dl| dl.reserve_slots_for_users));
    crate::api::model::recording_conflict::EffectiveCapacity {
        background_slots,
        reserved_interactive_slots: reserved,
    }
}

async fn collect_demand_points_for_provider(
    queue: &Arc<crate::api::model::download::DownloadQueue>,
    target_id: &str,
    input_name: &str,
) -> Vec<crate::api::model::recording_conflict::DemandPoint> {
    use crate::api::model::recording_conflict::DemandPoint;
    fn matches(task: &FileDownload, target_id: &str, input_name: &str) -> bool {
        task.kind == DownloadKind::Recording
            && task
                .recording
                .as_ref()
                .is_some_and(|meta| match &meta.source {
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
    let mut points = Vec::new();
    for task in queue.scheduled.read().await.iter() {
        if matches(task, target_id, input_name) {
            if let Some(point) = to_demand_point(task) {
                points.push(point);
            }
        }
    }
    for task in queue.queue.lock().await.iter() {
        if matches(task, target_id, input_name) {
            if let Some(point) = to_demand_point(task) {
                points.push(point);
            }
        }
    }
    if let Some(active) = queue.active.read().await.as_ref() {
        if matches(active, target_id, input_name) {
            if let Some(point) = to_demand_point(active) {
                points.push(point);
            }
        }
    }
    points
}

fn remove_inactive_recording(candidate: &mut PersistedDownloadQueue, uuid: &str) -> Option<PersistedFileDownload> {
    if let Some(index) = candidate.scheduled.iter().position(|task| task.uuid == uuid && task.kind == DownloadKind::Recording) {
        return Some(candidate.scheduled.remove(index));
    }
    let index = candidate.queue.iter().position(|task| task.uuid == uuid && task.kind == DownloadKind::Recording)?;
    Some(candidate.queue.remove(index))
}

fn cancel_future_rule_recordings_in_candidate(
    candidate: &mut PersistedDownloadQueue,
    rule_id: &str,
    now_secs: i64,
) -> usize {
    let mut cancelled = Vec::new();
    drain_future_rule_recordings(&mut candidate.scheduled, rule_id, now_secs, &mut cancelled);
    drain_future_rule_recordings(&mut candidate.queue, rule_id, now_secs, &mut cancelled);
    let count = cancelled.len();
    candidate.finished.extend(cancelled);
    count
}

fn drain_future_rule_recordings(
    tasks: &mut Vec<PersistedFileDownload>,
    rule_id: &str,
    now_secs: i64,
    out: &mut Vec<PersistedFileDownload>,
) {
    let mut index = 0;
    while index < tasks.len() {
        if is_future_rule_recording(&tasks[index], rule_id, now_secs) {
            let mut task = tasks.remove(index);
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
    task.kind == DownloadKind::Recording
        && task.start_at.is_some_and(|start| start > now_secs)
        && task.recording.as_ref().is_some_and(|meta| {
            meta.provenance.rule_id.as_deref() == Some(rule_id)
                && crate::api::model::recording_rule_service::cancel_targets_task(false, true)
        })
}

fn validate_reserved_filename(filename: &str) -> Result<(), &'static str> {
    use std::path::Component;
    let path = Path::new(filename);
    let single_normal_component = path
        .components()
        .next()
        .is_some_and(|c| matches!(c, Component::Normal(_)))
        && path.components().count() == 1;
    if filename.is_empty()
        || path.is_absolute()
        || !single_normal_component
        || filename.as_bytes().contains(&0)
    {
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
    use shared::model::{Permission, XtreamCluster};

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
        let window = effective_recording_window(1_000, 2_000, 100, 200, 1_500)
            .expect("valid effective window");

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
        let window = effective_recording_window(i64::MIN + 1, i64::MAX - 1, 10, 10, 0)
            .expect("saturated effective window");

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
        let patch = EditRecordingPatch {
            pre_roll_secs: Some(901),
            ..EditRecordingPatch::default()
        };

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
        let patch = EditRecordingPatch {
            program_title: Some("must not persist".to_string()),
            ..EditRecordingPatch::default()
        };

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
        let patch = EditRecordingPatch {
            channel_id: Some("b".into()),
            ..EditRecordingPatch::default()
        };

        let result = service.edit_recording(&claims, "recording", patch).await;
        assert!(result.is_ok());
        let scheduled = downloads.scheduled.read().await;
        let meta = scheduled[0].recording.as_ref().expect("recording metadata");
        assert_eq!(meta.channel_id.as_deref(), Some("b"));
        assert!(meta.epg.is_none(), "epg metadata must be cleared when channel changed without a fresh programme");
    }

    #[tokio::test]
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
        let quota = crate::model::RecordingQuotaConfig {
            default_private_bytes: Some(1_000),
            per_user_bytes: HashMap::new(),
            shared_bytes: None,
        };
        let rec_cfg = crate::model::RecordingConfig {
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
            fallback_bytes_per_minute: 60,
        };
        let dl_cfg = crate::model::VideoDownloadConfig {
            headers: HashMap::new(),
            directory: String::new(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 1,
            retry_backoff_multiplier: 1.0,
            retry_backoff_max_secs: 1,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 1,
            recording: Some(rec_cfg),
        };
        let config = crate::model::Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                download: Some(dl_cfg),
                web_search: None,
            }),
            ..crate::model::Config::default()
        };
        let app_config = Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(crate::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
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
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
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
        let patch = EditRecordingPatch {
            program_end: Some(1_300),
            ..EditRecordingPatch::default()
        };

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

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(crate::model::Config::default())),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(crate::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
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
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        })
    }

    #[test]
    fn service_error_code_is_stable_string() {
        assert_eq!(ServiceError::UnknownOwner.code(), "recording_unknown_owner");
        assert_eq!(ServiceError::InvalidSource.code(), "recording_invalid_source");
        assert_eq!(ServiceError::Forbidden.code(), "recording_forbidden");
        assert_eq!(
            ServiceError::SharedCreationNotAdministrator.code(),
            "recording_shared_not_administrator"
        );
        assert_eq!(ServiceError::InvalidState.code(), "recording_invalid_state");
        assert_eq!(ServiceError::InvalidInterval.code(), "recording_invalid_interval");
        assert_eq!(ServiceError::UnknownRecording.code(), "recording_unknown");
        assert_eq!(ServiceError::PersistenceFailed.code(), "recording_persistence_failed");
    }

    #[test]
    fn cancel_future_rule_recordings_moves_only_matching_future_tasks() {
        let now = 1_700_000_000;
        let mut queue = PersistedDownloadQueue::default();
        queue.scheduled.push(persisted_rule_recording("future-match", Some("rule-1"), now + 60));
        queue.scheduled.push(persisted_rule_recording("past-match", Some("rule-1"), now - 60));
        queue.queue.push(persisted_rule_recording("other-rule", Some("rule-2"), now + 60));

        let cancelled = cancel_future_rule_recordings_in_candidate(&mut queue, "rule-1", now);

        assert_eq!(cancelled, 1);
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
            other_target_meta.source = Some(shared::model::recording::RecordingSource::new(
                "other-target",
                "9",
                "input-a",
            ));
        }
        let mut other_input = persisted_rule_recording("other-input", None, 100);
        if let Some(other_input_meta) = other_input.recording.as_mut() {
            other_input_meta.source = Some(shared::model::recording::RecordingSource::new(
                "1",
                "9",
                "input-b",
            ));
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
}
