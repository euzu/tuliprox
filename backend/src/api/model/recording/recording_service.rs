//! Recording mutation service.

use std::{collections::HashMap, path::Path, sync::Arc};

use crate::api::model::app_state::AppState;
use crate::api::endpoints::v1_api_playlist;
use crate::api::model::download::{
    mutate, DownloadKind, DownloadQueue, DownloadState, FileDownload, PersistedDownloadQueue,
    PersistedFileDownload, QueueMutationError,
};
use crate::api::model::recording_quota::{self, AdmissionOutcome, QuotaLimits, QuotaPool};
use crate::auth::{
    authorize, authorize_orphan, RecordingAction, RecordingDecision, RecordingSubject, TerminalState,
};
use crate::api::model::recording_deletion::{
    begin_deletion, execute_deletion, finalize_deletion,
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
        if self.target_id.trim().is_empty() || self.virtual_id.trim().is_empty() {
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
}

impl CreateRecordingInput {
    pub fn validate(&self) -> Result<(), ServiceError> {
        self.source.validate()?;
        if self.program_start >= self.program_end {
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
            Self::UnknownRecording => "recording_unknown",
            Self::PersistenceFailed => "recording_persistence_failed",
            Self::IoError(_) => "recording_io_error",
            Self::QuotaExceeded => "recording_quota_exceeded",
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

    /// Create a new recording. Enforces well-formedness, source
    /// validity, owner from claims, and the authorization matrix.
    pub async fn create_recording(
        &self,
        claims: &shared::model::Claims,
        input: &CreateRecordingInput,
    ) -> Result<RecordingTaskView, ServiceError> {
        input.validate()?;
        let owner_id = Self::subject_id(claims)?;
        let target_id = input.source.target_id.parse::<u16>().map_err(|_| ServiceError::InvalidSource)?;
        let virtual_id = input.source.virtual_id.parse::<u32>().map_err(|_| ServiceError::InvalidSource)?;
        let config = self.app_config.config.load();
        let Some(download_cfg) = config.video.as_ref().and_then(|v| v.download.as_ref()) else {
            return Err(ServiceError::InvalidSource);
        };
        let url = v1_api_playlist::build_webplayer_recording_url(
            &self.app_config,
            target_id,
            virtual_id,
            input.source.cluster,
        )
        .ok_or(ServiceError::InvalidSource)?;

        // Shared creation requires admin.
        authorize_create_recording(claims, &owner_id, input.visibility)?;

        let duration_secs = u64::try_from(input.program_end - input.program_start).map_err(|_| ServiceError::InvalidInterval)?;
        let priority = download_cfg.recording_priority;
        let filename = render_filename_preview(claims, input);
        let input_name: Option<Arc<str>> = (!input.source.input_name.trim().is_empty())
            .then(|| Arc::from(input.source.input_name.as_str()));
        let mut recording = FileDownload::new_recording(
            &url,
            &filename,
            download_cfg,
            input.program_start,
            duration_secs,
            input_name,
            priority,
        )
        .ok_or(ServiceError::InvalidSource)?;
        let source = RecordingSource::new(
            input.source.target_id.clone(),
            input.source.virtual_id.clone(),
            input.source.input_name.clone(),
        );
        let mut meta = RecordingMetadata::new(
            RecordingOwner::User(owner_id.clone()),
            input.visibility,
            source,
            input.program_start,
            input.program_end,
            input.pre_roll_secs,
            input.post_roll_secs,
        );
        meta.channel_id.clone_from(&input.channel_id);
        meta.channel_name.clone_from(&input.channel_name);
        meta.program_title = Some(input.program_title.clone());
        meta.provenance = input.provenance.clone();
        let recording_cfg = download_cfg.recording.as_ref();
        let fallback_bytes_per_minute = recording_cfg.map_or(8 * 1024 * 1024, |cfg| cfg.fallback_bytes_per_minute);
        let (reserved_bytes, _) = recording_quota::estimate_reservation(duration_secs, 0, fallback_bytes_per_minute);
        meta.reserved_bytes = reserved_bytes;
        recording.recording = Some(meta);
        let mut persisted = DownloadQueue::to_persisted(&recording);
        let view_task = recording.clone();
        let quota_limits = quota_limits_from_config(recording_cfg.and_then(|cfg| cfg.quota.as_ref()));

        mutate(&self.downloads, |candidate| {
            reserve_recording_relative_path(candidate, &mut persisted).map_err(QueueMutationError::new)?;
            if candidate_has_duplicate_recording(candidate, &view_task) {
                return Err(QueueMutationError::new("recording duplicate"));
            }
            let pool = recording_quota::quota_pool_for_task(&persisted)
                .ok_or_else(|| QueueMutationError::new("recording invalid quota pool"))?;
            let used = used_bytes_for_pool(candidate, &pool);
            if matches!(
                recording_quota::would_exceed(&pool, used, reserved_bytes, &quota_limits),
                AdmissionOutcome::OverLimit { .. }
            ) {
                return Err(QueueMutationError::new("recording quota exceeded"));
            }
            candidate.scheduled.push(persisted);
            Ok(())
        })
        .await
        .map_err(|err| {
            if err.message() == "recording duplicate" {
                ServiceError::InvalidState
            } else if err.message() == "recording quota exceeded" {
                ServiceError::QuotaExceeded
            } else if err.source_io().is_some() {
                ServiceError::PersistenceFailed
            } else {
                ServiceError::InvalidState
            }
        })?;

        Ok(RecordingTaskView {
            uuid: view_task.uuid,
            owner_id,
            visibility: input.visibility,
            filename_preview: view_task.filename,
            start_at: Some(input.program_start),
            duration_secs: Some(duration_secs),
            state: DownloadState::Scheduled,
        })
    }

    /// Edit an existing recording.
    pub async fn edit_recording(
        &self,
        claims: &shared::model::Claims,
        uuid: &str,
        patch: EditRecordingPatch,
    ) -> Result<RecordingTaskView, ServiceError> {
        let owner_id = Self::subject_id(claims)?;
        let mut out = None;
        mutate(&self.downloads, |candidate| {
            let Some(task) = find_editable_recording_mut(candidate, uuid) else {
                return Err(QueueMutationError::new("recording unknown"));
            };
            let Some(meta) = task.recording.as_mut() else {
                return Err(QueueMutationError::new("recording unknown"));
            };
            let subject = RecordingSubject::new(Some(meta), TerminalState::Active, true);
            if !matches!(
                authorize(claims, &owner_id, RecordingAction::Edit, &subject),
                RecordingDecision::Allow
            ) {
                return Err(QueueMutationError::new("recording forbidden"));
            }
            if let Some(title) = patch.program_title {
                meta.program_title = Some(title);
            }
            if let Some(channel_id) = patch.channel_id {
                meta.channel_id = Some(channel_id);
            }
            if let Some(channel_name) = patch.channel_name {
                meta.channel_name = Some(channel_name);
            }
            if let Some(pre_roll_secs) = patch.pre_roll_secs {
                meta.pre_roll_secs = pre_roll_secs;
            }
            if let Some(post_roll_secs) = patch.post_roll_secs {
                meta.post_roll_secs = post_roll_secs;
            }

            let start = patch.program_start.or(task.start_at).ok_or_else(|| QueueMutationError::new("recording invalid interval"))?;
            let current_end = task
                .start_at
                .zip(task.duration_secs)
                .and_then(|(s, d)| i64::try_from(d).ok().map(|d| s.saturating_add(d)));
            let end = patch.program_end.or(current_end).ok_or_else(|| QueueMutationError::new("recording invalid interval"))?;
            if start >= end {
                return Err(QueueMutationError::new("recording invalid interval"));
            }
            let duration_secs = u64::try_from(end - start).map_err(|_| QueueMutationError::new("recording invalid interval"))?;
            task.start_at = Some(start);
            task.duration_secs = Some(duration_secs);
            meta.program_start = Some(start);
            meta.program_end = Some(end);
            meta.scheduled_start = Some(start);
            meta.scheduled_end = Some(end);
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
        .map_err(|err| match err.message() {
            "recording unknown" => ServiceError::UnknownRecording,
            "recording forbidden" => ServiceError::Forbidden,
            "recording invalid interval" => ServiceError::InvalidInterval,
            _ if err.source_io().is_some() => ServiceError::PersistenceFailed,
            _ => ServiceError::InvalidState,
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
                    let _ = self.downloads.cancel_active().await;
                    return Ok(());
                }
                return Err(ServiceError::Forbidden);
            }
        }
        mutate(&self.downloads, |candidate| {
            let Some(task) = remove_inactive_recording(candidate, uuid) else {
                return Err(QueueMutationError::new("recording unknown"));
            };
            let Some(meta) = task.recording.as_ref() else {
                return Err(QueueMutationError::new("recording unknown"));
            };
            let subject = RecordingSubject::new(Some(meta), TerminalState::Active, true);
            if !matches!(
                authorize(claims, &owner_id, RecordingAction::Cancel, &subject),
                RecordingDecision::Allow
            ) {
                return Err(QueueMutationError::new("recording forbidden"));
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
        .map_err(|err| match err.message() {
            "recording unknown" => ServiceError::UnknownRecording,
            "recording forbidden" => ServiceError::Forbidden,
            _ if err.source_io().is_some() => ServiceError::PersistenceFailed,
            _ => ServiceError::InvalidState,
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
        .map_err(|err| {
            if err.source_io().is_some() {
                ServiceError::PersistenceFailed
            } else {
                ServiceError::InvalidState
            }
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
            .ok_or(ServiceError::UnknownRecording)?;
        let _ = execute_deletion(&recording, None).map_err(|e| ServiceError::IoError(e.to_string()))?;
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
            .ok_or(ServiceError::UnknownRecording)?;
        let _ = execute_deletion(&recording, None).map_err(|e| ServiceError::IoError(e.to_string()))?;
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
}

/// Look up a recording in the queue by uuid.
fn lookup_recording(queue: &Arc<crate::api::model::download::DownloadQueue>, uuid: &str) -> Option<FileDownload> {
    if let Some(d) = queue.queue.try_lock().ok().and_then(|q| {
        q.iter().find(|d| d.uuid == uuid).cloned()
    }) {
        return Some(d);
    }
    if let Some(d) = queue.scheduled.try_read().ok().and_then(|q| {
        q.iter().find(|d| d.uuid == uuid).cloned()
    }) {
        return Some(d);
    }
    if let Some(d) = queue.active.try_read().ok().and_then(|q| {
        q.as_ref().filter(|d| d.uuid == uuid).cloned()
    }) {
        return Some(d);
    }
    queue
        .finished
        .try_read()
        .ok()
        .and_then(|q| q.iter().find(|d| d.uuid == uuid).cloned())
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
) -> Result<(), &'static str> {
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
    validate_reserved_filename(&filename)?;
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

fn find_editable_recording_mut<'a>(
    candidate: &'a mut PersistedDownloadQueue,
    uuid: &str,
) -> Option<&'a mut PersistedFileDownload> {
    candidate
        .scheduled
        .iter_mut()
        .chain(candidate.queue.iter_mut())
        .find(|task| task.uuid == uuid && task.kind == DownloadKind::Recording)
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
    let path = Path::new(filename);
    if filename.is_empty() || path.is_absolute() || path.components().count() != 1 || filename.as_bytes().contains(&0) {
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
    use shared::model::XtreamCluster;

    fn source(target_id: &str, virtual_id: &str, input_name: &str) -> RecordingSourceInput {
        RecordingSourceInput {
            target_id: target_id.to_string(),
            virtual_id: virtual_id.to_string(),
            cluster: XtreamCluster::Live,
            input_name: input_name.to_string(),
        }
    }

    fn create_input() -> CreateRecordingInput {
        CreateRecordingInput {
            source: source("1", "1", "input-a"),
            program_title: "Pilot".to_string(),
            program_start: 1_700_000_000,
            program_end: 1_700_003_600,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
            provenance: RecordingProvenance::default(),
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
    fn source_input_rejects_empty_target_id() {
        let input = source("", "1", "i");
        assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
    }

    #[test]
    fn source_input_rejects_empty_virtual_id() {
        let input = source("1", "", "i");
        assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
    }

    #[test]
    fn source_input_accepts_empty_input_name_for_epg() {
        let input = source("1", "1", "");
        assert!(input.validate().is_ok());
    }

    #[test]
    fn source_input_accepts_non_empty_identifiers() {
        let input = source("1", "1", "input-a");
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
}
