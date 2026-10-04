//! Recording mutation service.

use super::{recording_ctx::RecordingCtx, recording_source_resolution as source_resolution};
use crate::{
    recording::{
        recording_disk, recording_path,
        recording_queue::{
            mutate, mutate_prepared, mutate_then, mutate_with_idempotency, IdempotencyOutcome, PersistedIdempotency,
            PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingQueue, RecordingTask,
            RecordingTaskState,
        },
    },
    recording_deletion::{
        begin_deletion_authorized, execute_deletion_target, finalize_deletion, rollback_deletion, DeletionError,
    },
    recording_edit::{self, EditError, PaddingBounds},
    recording_quota::{self, AdmissionOutcome, QuotaLimits, QuotaPool},
};
use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingProvenance, RecordingSource, RecordingVisibility},
    EventSink, RecordingKind, UserId, XtreamCluster,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
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
    /// Playlist group of the channel, when the caller already resolved it.
    /// `None` makes the service look it up for organised layouts.
    pub group: Option<String>,
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

/// Input for `RecordingService::create_media_recording_idempotent`: an
/// immediate VOD or series transfer. Title, extension and grouping come from
/// the playlist entry the caller already resolved.
#[derive(Debug, Clone)]
pub struct CreateMediaRecordingInput {
    pub source: RecordingSourceInput,
    pub title: String,
    pub extension: String,
    pub visibility: RecordingVisibility,
    pub group: Option<String>,
    pub series_name: Option<String>,
}

impl CreateMediaRecordingInput {
    pub fn validate(&self) -> Result<RecordingKind, ServiceError> {
        self.source.validate()?;
        if self.extension.trim_start_matches('.').trim().is_empty() {
            return Err(ServiceError::InvalidPath);
        }
        match self.source.cluster {
            XtreamCluster::Video => Ok(RecordingKind::Vod),
            XtreamCluster::Series => Ok(RecordingKind::Series),
            XtreamCluster::Live => Err(ServiceError::InvalidSource),
        }
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
    /// The server has no download engine: the `video.recording` block
    /// is missing from the configuration. Distinct from
    /// `InvalidSource` because the caller's identifiers may be
    /// perfectly valid — nothing on the server can execute them.
    Disabled,
    /// This exact request was already accepted under this idempotency key.
    /// Not a failure: the caller gets the original outcome.
    IdempotentReplay { recording_id: String },
    /// Same idempotency key, different request body.
    IdempotencyConflict,
}

/// An `Idempotency-Key` and a digest of the body it arrived with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdempotencyRequest {
    pub key: String,
    pub fingerprint: String,
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
            Self::IdempotentReplay { .. } => "recording_idempotent_replay",
            Self::IdempotencyConflict => "recording_idempotency_conflict",
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
    pub state: RecordingTaskState,
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
    recordings: Arc<RecordingQueue>,
    app_config: Arc<AppConfig>,
}

impl RecordingService {
    /// Construct from the queue and app config.
    pub fn new(recordings: Arc<RecordingQueue>, app_config: Arc<AppConfig>) -> Self { Self { recordings, app_config } }

    /// Convenience constructor from the DVR's context.
    pub fn from_ctx<E: EventSink + Clone + 'static>(ctx: &RecordingCtx<E>) -> Self {
        Self::new(ctx.recordings.clone(), ctx.app_config.clone())
    }

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
        self.create_recording_idempotent(claims, input, None).await
    }

    /// Builds the task a live request describes, and the space it reserves.
    ///
    /// Split out of `create_recording_idempotent` purely for length: this is
    /// the request-to-task translation, with no admission decisions in it.
    fn build_live_recording(
        owner_id: &UserId,
        owner_display: &str,
        input: &CreateRecordingInput,
        group: Option<String>,
        recording_cfg: &tuliprox_core::model::RecordingConfig,
        window: &EffectiveRecordingWindow,
        url: &str,
    ) -> Result<(RecordingTask, u64), ServiceError> {
        let duration_secs = window.remaining_duration_secs;
        let filename = render_live_filename(input, recording_cfg, window, owner_display);
        let input_name: Option<Arc<str>> =
            (!input.source.input_name.trim().is_empty()).then(|| Arc::from(input.source.input_name.as_str()));
        let source = RecordingSource::new(
            input.source.target_id.clone(),
            input.source.virtual_id.clone(),
            input.source.input_name.clone(),
        )
        .with_cluster(input.source.cluster);
        let mut meta = RecordingMetadata::new_live(
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
        meta.group = group;
        meta.provenance = input.provenance.clone();
        meta.epg.clone_from(&input.epg);
        let (reserved_bytes, _) =
            recording_quota::estimate_reservation(duration_secs, 0, recording_cfg.fallback_bytes_per_minute);
        meta.reserved_bytes = reserved_bytes;
        let recording = RecordingTask::new(
            RecordingKind::Live,
            url,
            &filename,
            recording_cfg,
            input_name,
            recording_cfg.priority,
            meta,
        )
        .ok_or(ServiceError::InvalidSource)?;
        Ok((recording, reserved_bytes))
    }

    /// Playlist group the live recording is filed under.
    ///
    /// Callers that did not resolve it (the rule scheduler) still get the
    /// organised layout the operator asked for. Without organisation the
    /// group is irrelevant, so the playlist is not read for it.
    async fn live_group(
        &self,
        input: &CreateRecordingInput,
        recording_cfg: &tuliprox_core::model::RecordingConfig,
    ) -> Option<String> {
        if input.group.is_some() || !recording_cfg.organize_into_directories {
            return input.group.clone();
        }
        source_resolution::resolve_live_group(
            &self.app_config,
            &input.source.target_id,
            &input.source.input_name,
            &input.source.virtual_id,
        )
        .await
    }

    /// `create_recording`, honouring an `Idempotency-Key`.
    ///
    /// A replay of an accepted request is answered from the stored record
    /// rather than run again; the same key with a different body is a
    /// conflict, because answering it with the first request's result would
    /// hide a caller bug.
    pub async fn create_recording_idempotent(
        &self,
        claims: &shared::model::Claims,
        input: &CreateRecordingInput,
        idempotency: Option<IdempotencyRequest>,
    ) -> Result<RecordingTaskView, ServiceError> {
        input.validate()?;
        let owner_id = Self::subject_id(claims)?;
        self.check_idempotency(&owner_id, idempotency.as_ref()).await?;
        let config = self.enabled_config()?;
        let Some(recording_cfg) = config.recording() else {
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

        // The layout is fixed when the task is built, so the group has to be
        // known first.
        let group = self.live_group(input, recording_cfg).await;
        let duration_secs = window.remaining_duration_secs;
        let (recording, reserved_bytes) =
            Self::build_live_recording(&owner_id, &claims.username, input, group, recording_cfg, &window, &url)?;
        self.admit(&recording, reserved_bytes, &owner_id, idempotency.as_ref(), recording_cfg).await?;

        Ok(RecordingTaskView {
            uuid: recording.uuid,
            owner_id,
            visibility: input.visibility,
            filename_preview: recording.filename,
            start_at: Some(window.execution_start),
            duration_secs: Some(duration_secs),
            state: RecordingTaskState::Scheduled,
        })
    }

    /// Queue an immediate VOD or series transfer, honouring an
    /// `Idempotency-Key`.
    ///
    /// Runs through the same admission as a live request: path reservation,
    /// per-principal duplicate check, quota and disk. The size of a transfer
    /// is unknown until the provider answers, so nothing is reserved up front;
    /// the worker re-checks quota and disk before it opens the destination.
    pub async fn create_media_recording_idempotent(
        &self,
        claims: &shared::model::Claims,
        input: &CreateMediaRecordingInput,
        idempotency: Option<IdempotencyRequest>,
    ) -> Result<RecordingTaskView, ServiceError> {
        let kind = input.validate()?;
        let owner_id = Self::subject_id(claims)?;
        self.check_idempotency(&owner_id, idempotency.as_ref()).await?;
        let config = self.enabled_config()?;
        let Some(recording_cfg) = config.recording() else {
            return Err(ServiceError::Disabled);
        };
        let url = self.recording_url(&input.source).ok_or(ServiceError::InvalidSource)?;
        authorize_create_recording(claims, &owner_id, input.visibility)?;

        let source = RecordingSource::new(
            input.source.target_id.clone(),
            input.source.virtual_id.clone(),
            input.source.input_name.clone(),
        )
        .with_cluster(input.source.cluster);
        let mut meta = RecordingMetadata::new_media(
            RecordingOwner::User(owner_id.clone()),
            input.visibility,
            source,
            input.title.clone(),
        );
        meta.group.clone_from(&input.group);
        meta.series_name.clone_from(&input.series_name);
        let filename = format!("{}.{}", input.title, input.extension.trim_start_matches('.'));
        let recording = RecordingTask::new(
            kind,
            &url,
            &filename,
            recording_cfg,
            Some(Arc::from(input.source.input_name.as_str())),
            recording_cfg.priority,
            meta,
        )
        .ok_or(ServiceError::InvalidPath)?;
        self.admit(&recording, 0, &owner_id, idempotency.as_ref(), recording_cfg).await?;

        Ok(RecordingTaskView {
            uuid: recording.uuid,
            owner_id,
            visibility: input.visibility,
            filename_preview: recording.filename,
            start_at: None,
            duration_secs: None,
            state: RecordingTaskState::Queued,
        })
    }

    /// Answer a replayed or conflicting `Idempotency-Key` before any work:
    /// a replay must not resolve sources, reserve a path or touch quota.
    async fn check_idempotency(
        &self,
        owner_id: &UserId,
        idempotency: Option<&IdempotencyRequest>,
    ) -> Result<(), ServiceError> {
        let Some(request) = idempotency else {
            return Ok(());
        };
        match self
            .recordings
            .lookup_idempotency(owner_id.0.as_str(), &request.key, &request.fingerprint)
            .await
            .map_err(|err| ServiceError::IoError(err.to_string()))?
        {
            IdempotencyOutcome::Fresh => Ok(()),
            IdempotencyOutcome::Replay { recording_id } => Err(ServiceError::IdempotentReplay { recording_id }),
            IdempotencyOutcome::Conflict => Err(ServiceError::IdempotencyConflict),
        }
    }

    fn enabled_config(&self) -> Result<Arc<tuliprox_core::model::Config>, ServiceError> {
        if !crate::recording::recording_supervisor::recording_enabled(&self.app_config) {
            return Err(ServiceError::Disabled);
        }
        Ok(self.app_config.config.load_full())
    }

    /// Admit a freshly built task into the queue: reserve its path, refuse a
    /// duplicate from the same principal, then check quota and disk, all in
    /// one mutation together with the idempotency record.
    ///
    /// Scheduled kinds land in `scheduled`, everything else in `queue`.
    async fn admit(
        &self,
        recording: &RecordingTask,
        reserved_bytes: u64,
        owner_id: &UserId,
        idempotency: Option<&IdempotencyRequest>,
        recording_cfg: &tuliprox_core::model::RecordingConfig,
    ) -> Result<(), ServiceError> {
        let mut persisted = RecordingQueue::to_persisted(recording);
        let quota_limits = quota_limits_from_config(recording_cfg.quota.as_ref());
        // Measured before the mutation: it is a syscall, and the lock is
        // held for the whole closure. `None` means the root could not be
        // measured, and an unmeasurable disk is not grounds to refuse.
        let disk_safety_bytes = recording_cfg.disk.as_ref().and_then(|disk| disk.safety_bytes).unwrap_or(0);
        let free_bytes = recording_disk::free_bytes_for(std::path::Path::new(&recording_cfg.directory));
        let idempotency_record = idempotency.map(|request| PersistedIdempotency {
            principal: owner_id.0.clone(),
            key: request.key.clone(),
            request_fingerprint: request.fingerprint.clone(),
            recording_id: recording.uuid.clone(),
            accepted_at: chrono::Utc::now().timestamp(),
        });

        let admit = |candidate: &mut PersistedRecordingQueue| -> Result<(), QueueMutationError> {
            reserve_recording_relative_path(candidate, &mut persisted)?;
            if candidate_has_duplicate_recording(candidate, recording) {
                return Err(QueueMutationError::Duplicate);
            }
            // Media another entry finished, or is fetching with a known size,
            // is charged at that size now: attaching copies the whole file
            // into this entry's charge, so admitting it at zero would let it
            // past the quota.
            let charge = reserved_bytes.max(known_media_size(candidate, &persisted));
            persisted.recording.reserved_bytes = charge;
            let pool = recording_quota::quota_pool_for_task(&persisted);
            let used = used_bytes_for_pool(candidate, &pool);
            if matches!(
                recording_quota::would_exceed(&pool, used, charge, &quota_limits),
                AdmissionOutcome::OverLimit { .. }
            ) {
                return Err(QueueMutationError::QuotaExceeded);
            }
            // Quota is per-owner and logical; this is the physical
            // question, and one can pass while the other fails.
            if let Some(free_bytes) = free_bytes {
                let active = recording_disk::active_disk_reservations(candidate_tasks(candidate));
                if matches!(
                    recording_disk::would_fit_on_disk(free_bytes, disk_safety_bytes, active, reserved_bytes),
                    recording_disk::DiskAdmission::Insufficient { .. }
                ) {
                    return Err(QueueMutationError::DiskFull);
                }
            }
            if recording.kind.is_scheduled() {
                candidate.scheduled.push(persisted.clone());
            } else {
                candidate.queue.push(persisted.clone());
            }
            Ok(())
        };

        match idempotency_record {
            Some(record) => mutate_with_idempotency(&self.recordings, record, admit).await,
            None => mutate(&self.recordings, admit).await,
        }
        .map_err(|e| map_queue_error(&e))
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
                    RecordingLocation::Active | RecordingLocation::Finished(_) => {
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
        let active = self.recordings.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
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
        let active = self.recordings.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
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
        let active = self.recordings.active.read().await.clone();
        if let Some(active) = active.filter(|active| active.uuid == uuid) {
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
                if candidate.active.as_ref().is_some_and(|active| active.uuid == uuid) {
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

    async fn retry_recording_inner(
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
                    if task.error.as_deref() != Some(super::recording_transfer::RANGE_UNSUPPORTED_ERROR) {
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
                    == Some(super::recording_transfer::RANGE_UNSUPPORTED_ERROR);
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
    async fn run_deletion<F>(&self, uuid: &str, permit: F) -> Result<bool, ServiceError>
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
        let bounds = padding_bounds(self.app_config.config.load().recording());
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
            collect_demand_points_for_provider(&self.recordings, &request.source.target_id, &request.source.input_name)
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

/// Live recording filename: `recording.filename_template` rendered for this
/// programme, plus the extension of the container the worker muxes into, so
/// players and the media API can tell the format from the name.
///
/// The sanitized programme title stands in when the template is unset or
/// cannot be rendered.
fn render_live_filename(
    input: &CreateRecordingInput,
    recording_cfg: &tuliprox_core::model::RecordingConfig,
    window: &EffectiveRecordingWindow,
    owner_display: &str,
) -> String {
    let title_stem = sanitize_filename_component(&input.program_title);
    let context = shared::utils::RecordingFilenameContext {
        task_id: title_stem.clone(),
        channel_id: input.channel_id.clone(),
        channel_name: input.channel_name.clone(),
        program_title: Some(input.program_title.clone()),
        episode_season: input.epg.as_ref().and_then(|epg| epg.season),
        episode_number: input.epg.as_ref().and_then(|epg| epg.episode),
        owner_display: (!owner_display.trim().is_empty()).then(|| owner_display.to_string()),
        program_start: Some(input.program_start),
        program_end: Some(input.program_end),
        scheduled_start: Some(window.scheduled_start),
        scheduled_end: Some(window.scheduled_end),
    };
    let stem = Some(recording_cfg.filename_template.as_str())
        .filter(|template| !template.trim().is_empty())
        .and_then(|template| {
            shared::utils::render_recording_stem(template, &context, &recording_cfg.timezone)
                .map_err(|err| log::warn!("Recording filename template could not be rendered: {err}"))
                .ok()
        })
        .unwrap_or(title_stem);
    let extension = recording_cfg.container_format.file_extension();
    // A template that already ends in the container extension must not double it.
    let stem_path = Path::new(&stem);
    let stem = match (stem_path.file_stem(), stem_path.extension()) {
        (Some(base), Some(ext)) if ext.eq_ignore_ascii_case(extension) => base.to_string_lossy(),
        _ => std::borrow::Cow::Borrowed(stem.as_str()),
    };
    format!("{stem}.{extension}")
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

pub fn quota_limits_from_config(config: Option<&tuliprox_core::model::RecordingQuotaConfig>) -> QuotaLimits {
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
/// previous implementation built a `Vec<PersistedRecordingTask>` of the
/// entire queue on every create and every edit.
/// Authorize an action against a task found in the candidate snapshot.
/// Called inside the queue mutation so the ownership decision and the
/// state change commit together.
fn authorize_task_in_candidate(
    candidate: &PersistedRecordingQueue,
    uuid: &str,
    claims: &shared::model::Claims,
    subject_id: &UserId,
    action: RecordingAction,
) -> Result<(), QueueMutationError> {
    let Some(task) = candidate_tasks(candidate).find(|task| task.uuid == uuid) else {
        return Err(QueueMutationError::UnknownRecording);
    };
    let subject = RecordingSubject::new(Some(&task.recording), TerminalState::Active, true);
    match authorize(claims, subject_id, action, &subject) {
        RecordingDecision::Allow => Ok(()),
        RecordingDecision::Deny(_) => Err(QueueMutationError::Forbidden),
    }
}

fn candidate_tasks(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &PersistedRecordingTask> + '_ {
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
fn used_bytes_for_pool(candidate: &PersistedRecordingQueue, pool: &QuotaPool) -> u64 {
    recording_quota::used_bytes_in_pool(candidate_tasks(candidate), pool)
}

fn reserve_recording_relative_path(
    candidate: &PersistedRecordingQueue,
    task: &mut PersistedRecordingTask,
) -> Result<(), QueueMutationError> {
    // Borrowed set, built once. The old code walked a `Vec<String>` of
    // cloned filenames once per `_N` candidate, so reserving the
    // (N+1)-th recording of a title cost O(N^2) string comparisons.
    let existing: std::collections::HashSet<&str> = collect_existing_relative_paths(candidate).collect();
    // The reservation is over the whole root-relative path, not the bare
    // filename: two series can legitimately hold an `e01.mkv` in different
    // season directories.
    let base = task.recording.relative_path.clone().unwrap_or_else(|| task.filename.clone());
    let mut relative = PathBuf::from(&base);
    if existing.contains(base.as_str()) {
        // Linear probe over indices; each probe is one hash lookup.
        for index in 1.. {
            relative = recording_path::with_collision_suffix(Path::new(&base), index);
            if !existing.contains(relative.to_string_lossy().as_ref()) {
                break;
            }
        }
    }
    let filename =
        relative.file_name().and_then(|name| name.to_str()).ok_or(QueueMutationError::InvalidPath)?.to_string();
    validate_reserved_filename(&filename).map_err(|_| QueueMutationError::InvalidPath)?;
    if !recording_path::is_contained_relative_path(&relative) {
        return Err(QueueMutationError::InvalidPath);
    }
    task.file_path = task.file_dir.join(&filename);
    task.filename = filename;
    task.recording.relative_path = Some(relative.to_string_lossy().into_owned());
    Ok(())
}

/// The size of the media `task` refers to, as far as another entry already
/// knows it: the measured size of a completed file, or the total a transfer
/// learned from its provider. `0` when nobody knows yet.
fn known_media_size(candidate: &PersistedRecordingQueue, task: &PersistedRecordingTask) -> u64 {
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

fn collect_existing_relative_paths(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &str> + '_ {
    candidate_tasks(candidate).map(task_relative_path)
}

fn task_relative_path(task: &PersistedRecordingTask) -> &str {
    task.recording.relative_path.as_deref().unwrap_or(task.filename.as_str())
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum RecordingIdentity {
    /// Materialization of one rule occurrence. Two tasks with the same
    /// `(rule_id, occurrence_key)` are the same recording by
    /// definition, whatever their window looks like.
    Occurrence { rule_id: String, occurrence_key: String },
    /// A concrete programme on a concrete source. Deliberately free of any
    /// owner: two users asking for the same programme are asking for the same
    /// file, and it is recorded once and linked twice.
    Programme { target_id: String, virtual_id: String, program_start: i64, program_end: i64 },
    /// No programme metadata at all: every VOD and series transfer, and a
    /// Live capture whose programme window is unknown. Falls back to the
    /// resolved URL plus the *scheduled* (padded) window, which — unlike
    /// `start_at` — is stable across requests inside a currently-airing
    /// window.
    ///
    /// Owner-free for the same reason `Programme` is: the same URL over the
    /// same window is the same bytes, whoever asked for them.
    Url { url: String, scheduled_start: Option<i64>, scheduled_end: Option<i64> },
}

/// A stable, field-named key for the media a request refers to.
///
/// Two tasks with the same key are the same recording, so they share one
/// physical file. The key is persisted, so it has to be stable across builds:
/// that is why it is serialised rather than formatted with `Debug`.
pub fn recording_identity_key(meta: &RecordingMetadata, url: &str) -> String {
    serde_json::to_string(&recording_identity(meta, url)).unwrap_or_else(|_| format!("url:{url}"))
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
    if let (Some(program_start), Some(program_end)) = (meta.program_start, meta.program_end) {
        return RecordingIdentity::Programme {
            target_id: meta.source.target_id.clone(),
            virtual_id: meta.source.virtual_id.clone(),
            program_start,
            program_end,
        };
    }
    RecordingIdentity::Url {
        url: url.to_string(),
        scheduled_start: meta.scheduled_start,
        scheduled_end: meta.scheduled_end,
    }
}

fn persisted_recording_identity(task: &PersistedRecordingTask) -> RecordingIdentity {
    recording_identity(&task.recording, &task.url)
}

fn candidate_has_duplicate_recording(candidate: &PersistedRecordingQueue, task: &RecordingTask) -> bool {
    let meta = &task.recording;
    let identity = recording_identity(meta, task.url.as_str());
    let pool = recording_quota::quota_pool_for_task(task);
    // Only the *same* principal asking twice is a duplicate. A different
    // principal asking for the same media gets their own library entry, which
    // attaches to the one physical file rather than producing a second.
    let pending_match = candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .filter(|existing| recording_quota::quota_pool_for_task(*existing) == pool)
        .map(persisted_recording_identity)
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
    candidate
        .finished
        .iter()
        .filter(|existing| recording_quota::quota_pool_for_task(*existing) == pool)
        .map(persisted_recording_identity)
        .any(|existing| existing == identity)
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
fn locate_recording(candidate: &PersistedRecordingQueue, uuid: &str) -> Option<RecordingLocation> {
    let matches_uuid = |task: &PersistedRecordingTask| task.uuid == uuid;
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
    candidate: &mut PersistedRecordingQueue,
    location: RecordingLocation,
) -> Option<&mut PersistedRecordingTask> {
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
    is_live: bool,
    /// Another entry holds the same media, and with it the same file path.
    shares_media: bool,
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
    let background_slots = config.recording().map_or(0, |cfg| u32::from(cfg.max_background_per_provider));
    let reserved = config.recording().map_or(0, |cfg| u32::from(cfg.reserve_slots_for_users));
    crate::recording_conflict::EffectiveCapacity { background_slots, reserved_interactive_slots: reserved }
}

async fn collect_demand_points_for_provider(
    queue: &Arc<crate::recording::recording_queue::RecordingQueue>,
    target_id: &str,
    input_name: &str,
) -> Vec<crate::recording_conflict::DemandPoint> {
    use crate::recording_conflict::DemandPoint;
    fn matches(task: &RecordingTask, target_id: &str, input_name: &str) -> bool {
        task.kind == RecordingKind::Live
            && task.recording.source.target_id == target_id
            && task.recording.source.input_name == input_name
    }
    fn to_demand_point(task: &RecordingTask) -> Option<DemandPoint> {
        let meta = &task.recording;
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
    fn claims_a_slot(task: &RecordingTask) -> bool {
        !matches!(
            task.state,
            RecordingTaskState::Completed | RecordingTaskState::Failed | RecordingTaskState::Cancelled
        )
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
enum CancelOrigin {
    Scheduled,
    Queue,
}

/// A rule-materialized recording exactly as it was before the cancel.
#[derive(Debug, Clone)]
pub struct CancelledRuleRecording {
    origin: CancelOrigin,
    task: PersistedRecordingTask,
}

fn cancel_future_rule_recordings_in_candidate(
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

fn is_future_rule_recording(task: &PersistedRecordingTask, rule_id: &str, now_secs: i64) -> bool {
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
            group: None,
            provenance: RecordingProvenance::default(),
            epg: None,
        }
    }

    /// A VOD transfer for `owner`, resolved to `url`.
    fn persisted_media(uuid: &str, owner: &str, visibility: RecordingVisibility, url: &str) -> PersistedRecordingTask {
        let meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from(owner)),
            visibility,
            RecordingSource::new("1", "1", "input-a"),
            "Film".to_string(),
        );
        PersistedRecordingTask {
            media_identity: String::new(),
            partition: crate::recording::recording_queue::RecordingPartition::default(),
            uuid: uuid.to_string(),
            kind: RecordingKind::Vod,
            file_dir: std::path::PathBuf::from("/tmp"),
            file_path: std::path::PathBuf::from(format!("/tmp/{uuid}.mp4")),
            filename: format!("{uuid}.mp4"),
            url: url.to_string(),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: RecordingTaskState::Queued,
            input_name: Some("input-a".to_string()),
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: meta,
        }
    }

    fn queued_candidate(tasks: Vec<PersistedRecordingTask>) -> PersistedRecordingQueue {
        PersistedRecordingQueue { queue: tasks, ..PersistedRecordingQueue::default() }
    }

    #[test]
    fn a_live_window_must_have_time_left_in_it() {
        // A window whose padded end is not after its padded start cannot
        // produce a recording, and admitting one means a scheduled capture that
        // can only ever fail.
        let now = 1_700_000_000;
        // Zero-length and inverted programmes.
        assert!(matches!(
            effective_recording_window(now + 100, now + 100, 0, 0, now),
            Err(ServiceError::InvalidInterval)
        ));
        assert!(matches!(
            effective_recording_window(now + 200, now + 100, 0, 0, now),
            Err(ServiceError::InvalidInterval)
        ));
        // A programme that finished before it was asked for.
        assert!(matches!(
            effective_recording_window(now - 7_200, now - 3_600, 0, 0, now),
            Err(ServiceError::InvalidInterval)
        ));
    }

    #[test]
    fn a_live_window_already_underway_records_only_what_is_left() {
        // Joining late is legal; it just cannot rewind. The padded bounds stay
        // as planned so the stop time is still the programme's.
        let now = 1_700_000_000;
        let window = effective_recording_window(now - 600, now + 600, 60, 120, now).expect("still running");

        assert_eq!(window.scheduled_start, now - 660, "the padded start is history, not moved to now");
        assert_eq!(window.scheduled_end, now + 720);
        assert_eq!(window.execution_start, now, "but recording starts now");
        assert_eq!(window.remaining_duration_secs, 720, "and runs to the padded end");
    }

    #[test]
    fn a_repeated_vod_request_is_a_duplicate() {
        // Regression: duplicate detection only produced an identity for Live,
        // so a user who asked for the same film twice got two downloads of it
        // written side by side as `film.mp4` and `film_1.mp4`.
        let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
        let repeat = RecordingQueue::from_persisted(persisted_media(
            "b",
            "web:alice",
            RecordingVisibility::Private,
            "http://p/film.mp4",
        ))
        .expect("valid task");
        assert!(candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &repeat));
    }

    #[test]
    fn a_different_film_is_not_a_duplicate() {
        let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
        let other = RecordingQueue::from_persisted(persisted_media(
            "b",
            "web:alice",
            RecordingVisibility::Private,
            "http://p/other.mp4",
        ))
        .expect("valid task");
        assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &other));
    }

    #[test]
    fn another_user_asking_for_the_same_film_is_not_refused() {
        // Until one physical file can carry several library entries, treating
        // this as a duplicate would tell the second user "already recording"
        // and leave them with nothing.
        let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
        let other_user = RecordingQueue::from_persisted(persisted_media(
            "b",
            "web:bob",
            RecordingVisibility::Private,
            "http://p/film.mp4",
        ))
        .expect("valid task");
        assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &other_user));
    }

    #[test]
    fn a_shared_copy_does_not_collide_with_a_private_one() {
        // The two charge different quota pools, so they are two recordings.
        let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
        let shared = RecordingQueue::from_persisted(persisted_media(
            "b",
            "web:alice",
            RecordingVisibility::Shared,
            "http://p/film.mp4",
        ))
        .expect("valid task");
        assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &shared));
    }

    #[test]
    fn a_finished_transfer_does_not_block_a_fresh_request() {
        // After a completed or failed attempt the user may legitimately want
        // another copy.
        let mut finished = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
        finished.finished = true;
        finished.state = RecordingTaskState::Completed;
        let candidate = PersistedRecordingQueue { finished: vec![finished], ..PersistedRecordingQueue::default() };
        let again = RecordingQueue::from_persisted(persisted_media(
            "b",
            "web:alice",
            RecordingVisibility::Private,
            "http://p/film.mp4",
        ))
        .expect("valid task");
        assert!(!candidate_has_duplicate_recording(&candidate, &again));
    }

    #[test]
    fn a_completed_rule_occurrence_still_blocks_a_repeat() {
        // The scheduler re-evaluates rules on every tick; without this an
        // occurrence would be re-materialized forever.
        let mut finished = persisted_rule_recording("a", Some("rule-1"), 100);
        finished.recording.provenance.occurrence_key = Some("occ-1".to_string());
        finished.finished = true;
        finished.state = RecordingTaskState::Completed;
        let candidate = PersistedRecordingQueue { finished: vec![finished], ..PersistedRecordingQueue::default() };

        let mut repeat_persisted = persisted_rule_recording("b", Some("rule-1"), 100);
        repeat_persisted.recording.provenance.occurrence_key = Some("occ-1".to_string());
        let repeat = RecordingQueue::from_persisted(repeat_persisted).expect("valid task");
        assert!(candidate_has_duplicate_recording(&candidate, &repeat));
    }

    fn persisted_rule_recording(uuid: &str, rule_id: Option<&str>, start_at: i64) -> PersistedRecordingTask {
        let mut meta = RecordingMetadata::new_live(
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
        PersistedRecordingTask {
            media_identity: String::new(),
            partition: crate::recording::recording_queue::RecordingPartition::default(),
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
            state: RecordingTaskState::Scheduled,
            kind: RecordingKind::Live,
            input_name: Some("input-a".to_string()),
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: meta,
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
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = committed_records(&downloads).await;
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { pre_roll_secs: Some(901), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::PaddingLimitExceeded)));
        assert_eq!(committed_records(&downloads).await, persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].recording.pre_roll_secs, 0);
    }

    #[tokio::test]
    async fn edit_recording_rejects_active_state_with_invalid_state_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let mut task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        task.state = RecordingTaskState::Running;
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = committed_records(&downloads).await;
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch =
            EditRecordingPatch { program_title: Some("must not persist".to_string()), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::InvalidState)));
        assert_eq!(committed_records(&downloads).await, persisted_before);
    }

    #[tokio::test]
    async fn unsupported_range_requires_confirmation_before_vod_or_series_partial_is_discarded() {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let dir = tempfile::tempdir().expect("tempdir");
            let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("recording repository"));
            let mut task =
                persisted_media("recording", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
            task.kind = kind;
            task.file_dir = dir.path().to_path_buf();
            task.file_path = dir.path().join("recording.mp4");
            task.state = RecordingTaskState::Failed;
            task.finished = true;
            task.size = 4;
            task.total_size = Some(10);
            task.error = Some(super::super::recording_transfer::RANGE_UNSUPPORTED_ERROR.to_string());
            task.recording.resume_etag = Some("\"old-etag\"".to_string());
            let partial = crate::recording::recording_worker::recording_partial_path(&task.file_path);
            std::fs::write(&partial, b"0123").expect("saved partial");
            mutate(&queue, move |candidate| {
                candidate.finished.push(task.clone());
                Ok(())
            })
            .await
            .expect("seed failed recording");
            let service = RecordingService::new(Arc::clone(&queue), test_app_config());
            let claims = shared::model::Claims {
                username: "alice".to_string(),
                iss: "tuliprox".to_string(),
                iat: 0,
                exp: 0,
                roles: shared::model::RoleSet::new(),
                permissions: Permission::RecordingManage.into(),
                pwd_version: 0,
                subject_id: Some(UserId::from("web:alice")),
                permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
            };

            assert!(matches!(service.retry_recording(&claims, "recording").await, Err(ServiceError::InvalidState)));
            assert_eq!(std::fs::read(&partial).expect("partial kept without consent"), b"0123");
            assert!(queue.finished.read().await[0].to_view(true).restart_from_beginning_required);

            assert!(service.restart_recording(&claims, "recording").await.expect("confirmed restart"));
            assert!(!partial.exists());
            assert!(queue.finished.read().await.is_empty());
            let queued = queue.queue.lock().await.front().cloned().expect("queued transfer");
            assert_eq!(queued.state, RecordingTaskState::Queued);
            assert_eq!(queued.size, 0);
            assert_eq!(queued.total_size, None);
            assert_eq!(queued.recording.resume_etag, None);
        }
    }

    fn deleting_claims() -> shared::model::Claims {
        shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingDelete.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    /// A finished entry `uuid` of `owner` on the shared film, with its file in
    /// `dir`. Both the final file and the partial are written, so a test sees
    /// exactly which one a removal touched.
    async fn finished_film(
        queue: &RecordingQueue,
        dir: &std::path::Path,
        uuid: &str,
        owner: &str,
        state: RecordingTaskState,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let mut task = persisted_media(uuid, owner, RecordingVisibility::Private, "http://provider/film.mp4");
        task.file_dir = dir.to_path_buf();
        task.file_path = dir.join("recording.mp4");
        task.state = state;
        task.finished = true;
        let final_path = task.file_path.clone();
        let partial = crate::recording::recording_worker::recording_partial_path(&final_path);
        std::fs::write(&final_path, b"recorded bytes").expect("write final file");
        std::fs::write(&partial, b"partial bytes").expect("write partial");
        let task = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task).expect("valid task"));
        mutate(queue, move |candidate| {
            candidate.finished.push(task.clone());
            Ok(())
        })
        .await
        .expect("seed recording");
        (final_path, partial)
    }

    #[tokio::test]
    async fn removing_a_completed_entry_keeps_its_file() {
        // Remove takes the entry off the list; the recording stays on disk
        // for whoever reads the directory. Deleting the file is its own action.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let (final_path, _) =
            finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Completed).await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
        assert!(final_path.exists(), "the completed file was deleted while only removing the entry");
        assert!(queue.finished.read().await.is_empty());
    }

    #[tokio::test]
    async fn removing_the_last_failed_or_cancelled_entry_removes_its_partial() {
        for state in [RecordingTaskState::Failed, RecordingTaskState::Cancelled] {
            let dir = tempfile::tempdir().expect("tempdir");
            let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
            let (final_path, partial) = finished_film(&queue, dir.path(), "recording", "web:alice", state).await;
            let service = RecordingService::new(Arc::clone(&queue), test_app_config());

            assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
            assert!(!partial.exists(), "{state:?}: a partial nothing will resume is not kept");
            assert!(final_path.exists(), "{state:?}: a final file is never removed by Remove");
        }
    }

    #[test]
    fn a_download_claiming_the_path_during_the_cleanup_keeps_its_partial() {
        // The orphan check and the unlink happen under one mutation guard. A
        // download admitted after the removal may take over the path, and
        // must find its partial intact.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let queue = Arc::new(RecordingQueue::new());
            let (final_path, partial) =
                finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Failed).await;
            std::fs::remove_file(&final_path).expect("only the partial exists");
            let mut newcomer =
                persisted_media("newcomer", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
            newcomer.file_path.clone_from(&final_path);
            newcomer.state = RecordingTaskState::Running;

            // Park the filesystem: the cleanup's unlink waits on the only
            // blocking thread until released.
            let (parked_tx, parked_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let blocker = tokio::task::spawn_blocking(move || {
                parked_tx.send(()).expect("parked");
                let _ = release_rx.recv();
            });
            parked_rx.recv().expect("blocking thread taken");

            let service = RecordingService::new(Arc::clone(&queue), test_app_config());
            let removal =
                tokio::spawn(async move { service.remove_recording_task(&deleting_claims(), "recording").await });
            while !queue.finished.read().await.is_empty() {
                tokio::task::yield_now().await;
            }
            // The newcomer is admitted and starts writing, as soon as it can.
            let claim_queue = Arc::clone(&queue);
            let claim_partial = partial.clone();
            let claim = tokio::spawn(async move {
                mutate(&claim_queue, move |candidate| {
                    candidate.active = Some(newcomer.clone());
                    Ok(())
                })
                .await
                .expect("admit newcomer");
                tokio::fs::write(&claim_partial, b"newcomer's bytes").await.expect("newcomer writes");
            });
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            release_tx.send(()).expect("release");
            blocker.await.expect("blocker");
            assert!(removal.await.expect("join").expect("remove"));
            claim.await.expect("claim");

            assert_eq!(queue.active.read().await.as_ref().map(|task| task.uuid.clone()).as_deref(), Some("newcomer"));
            assert!(partial.exists(), "the newcomer's partial must survive the cleanup of the old entry");
        });
    }

    #[tokio::test]
    async fn removing_an_entry_keeps_a_partial_another_entry_still_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let (_, partial) =
            finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Cancelled).await;
        let _ = finished_film(&queue, dir.path(), "bob-recording", "web:bob", RecordingTaskState::Failed).await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
        assert!(partial.exists(), "Bob's entry still points at this media");
        assert_eq!(queue.finished.read().await.len(), 1);
    }

    #[tokio::test]
    async fn cancelling_an_active_recording_leaves_it_worker_owned_until_the_worker_finishes() {
        // A cancel request leaves the active task to its worker: it must not
        // show as finished while the worker is still writing, and removing it
        // in that state must be refused rather than reported as done.
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let mut task = RecordingQueue::from_persisted(persisted_media(
            "recording",
            "web:alice",
            RecordingVisibility::Private,
            "http://provider/film.mp4",
        ))
        .expect("valid recording task");
        task.state = RecordingTaskState::Running;
        *downloads.active.write().await = Some(task);

        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };

        service.cancel_recording(&claims, "recording").await.expect("cancel request accepted");

        let active = downloads.active.read().await.clone().expect("the worker still owns it");
        assert_eq!(active.state, RecordingTaskState::Cancelling, "the worker commits the terminal state");
        assert!(downloads.finished.read().await.is_empty(), "nothing terminal yet");
        assert!(
            matches!(service.remove_recording_task(&claims, "recording").await, Err(ServiceError::InvalidState)),
            "removing a worker-owned recording must be refused, not silently ignored"
        );
    }

    #[tokio::test]
    async fn edit_recording_clears_epg_when_channel_changes_without_programme() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let mut task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        {
            let meta = &mut task.recording;
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
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { channel_id: Some("b".into()), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;
        assert!(result.is_ok());
        let scheduled = downloads.scheduled.read().await;
        let meta = &scheduled[0].recording;
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
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let start = chrono::Utc::now().timestamp() + 3_600;
        let mut task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, start))
            .expect("valid recording task");
        {
            let meta = &mut task.recording;
            meta.reserved_bytes = 800;
        }
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = committed_records(&downloads).await;
        let quota = tuliprox_core::model::RecordingQuotaConfig {
            default_private_bytes: Some(1_000),
            per_user_bytes: HashMap::new(),
            shared_bytes: None,
        };
        let rec_cfg = RecordingConfig {
            headers: HashMap::new(),
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
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(rec_cfg.clone()),
            }),
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
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };
        let patch = EditRecordingPatch { program_end: Some(start + 1_200), ..EditRecordingPatch::default() };

        let result = service.edit_recording(&claims, "recording", patch).await;

        assert!(matches!(result, Err(ServiceError::QuotaExceeded)), "got {result:?}");
        assert_eq!(committed_records(&downloads).await, persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].scheduled_start(), Some(start));
    }

    fn editing_claims() -> shared::model::Claims {
        shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingManage.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    /// A scheduled live recording an hour from now, for `owner`.
    fn upcoming_live(uuid: &str, owner: &str, start: i64) -> PersistedRecordingTask {
        let mut task = persisted_rule_recording(uuid, None, start);
        task.recording.owner = RecordingOwner::User(UserId::from(owner));
        task.file_path = std::path::PathBuf::from("/tmp/shared-programme.ts");
        task.filename = "shared-programme.ts".to_string();
        task.recording.relative_path = Some("shared-programme.ts".to_string());
        RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task).expect("valid task"))
    }

    async fn scheduled_queue(tasks: Vec<PersistedRecordingTask>) -> Arc<RecordingQueue> {
        let queue = Arc::new(RecordingQueue::new());
        mutate(&queue, move |candidate| {
            candidate.scheduled.clone_from(&tasks);
            Ok(())
        })
        .await
        .expect("seed");
        queue
    }

    #[tokio::test]
    async fn an_edited_window_keeps_its_padding() {
        // An edited window is scheduled with its pre- and post-roll, like a
        // new one.
        let start = chrono::Utc::now().timestamp() + 3_600;
        let queue = scheduled_queue(vec![upcoming_live("recording", "web:alice", start)]).await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        let patch = EditRecordingPatch {
            program_end: Some(start + 1_800),
            pre_roll_secs: Some(120),
            post_roll_secs: Some(300),
            ..EditRecordingPatch::default()
        };
        service.edit_recording(&editing_claims(), "recording", patch).await.expect("edit");

        let scheduled = queue.scheduled.read().await;
        let meta = &scheduled[0].recording;
        assert_eq!(meta.program_start, Some(start));
        assert_eq!(meta.scheduled_start, Some(start - 120));
        assert_eq!(meta.scheduled_end, Some(start + 1_800 + 300));
    }

    #[tokio::test]
    async fn a_vod_takes_no_window_but_can_still_be_retitled() {
        let queue = scheduled_queue(Vec::new()).await;
        let mut vod = persisted_media("film", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
        vod.state = RecordingTaskState::Queued;
        mutate(&queue, move |candidate| {
            candidate.queue.push(vod.clone());
            Ok(())
        })
        .await
        .expect("seed");
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        for patch in [
            EditRecordingPatch { program_start: Some(1), program_end: Some(2), ..EditRecordingPatch::default() },
            EditRecordingPatch { pre_roll_secs: Some(60), ..EditRecordingPatch::default() },
        ] {
            let refused = service.edit_recording(&editing_claims(), "film", patch).await;
            assert!(matches!(refused, Err(ServiceError::InvalidInterval)), "got {refused:?}");
        }
        let retitled =
            EditRecordingPatch { program_title: Some("Better title".into()), ..EditRecordingPatch::default() };
        service.edit_recording(&editing_claims(), "film", retitled).await.expect("a title edit is fine");
        let queued = queue.queue.lock().await;
        assert_eq!(queued[0].recording.program_title.as_deref(), Some("Better title"));
        assert_eq!(queued[0].recording.program_start, None, "and it stays a transfer without a window");
    }

    #[tokio::test]
    async fn an_edit_that_leaves_shared_media_moves_to_a_path_of_its_own() {
        // Alice and Bob scheduled the same programme and hold one file path.
        // Alice moving her window makes it different media; keeping the path
        // would have two captures write one file and let deleting hers remove
        // Bob's.
        let start = chrono::Utc::now().timestamp() + 3_600;
        let queue = scheduled_queue(vec![
            upcoming_live("alice-entry", "web:alice", start),
            upcoming_live("bob-entry", "web:bob", start),
        ])
        .await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        let patch = EditRecordingPatch { program_end: Some(start + 7_200), ..EditRecordingPatch::default() };
        service.edit_recording(&editing_claims(), "alice-entry", patch).await.expect("edit");

        let (_, tasks) = queue.committed_snapshot().await;
        let task = |uuid: &str| tasks.iter().find(|task| task.uuid == uuid).cloned().expect("entry");
        let (alice, bob) = (task("alice-entry"), task("bob-entry"));
        assert_ne!(alice.file_path, bob.file_path, "the edited entry no longer writes Bob's file");
        assert_eq!(bob.file_path, std::path::PathBuf::from("/tmp/shared-programme.ts"), "Bob's entry is untouched");
        let identity = |task: &RecordingTask| recording_identity_key(&task.recording, task.url.as_str());
        assert_ne!(identity(&alice), identity(&bob));
    }

    #[tokio::test]
    async fn padding_alone_does_not_split_shared_media() {
        // The programme window decides the media, its padding does not.
        let start = chrono::Utc::now().timestamp() + 3_600;
        let queue = scheduled_queue(vec![
            upcoming_live("alice-entry", "web:alice", start),
            upcoming_live("bob-entry", "web:bob", start),
        ])
        .await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        let patch = EditRecordingPatch { post_roll_secs: Some(600), ..EditRecordingPatch::default() };
        service.edit_recording(&editing_claims(), "alice-entry", patch).await.expect("edit");

        let (_, tasks) = queue.committed_snapshot().await;
        let paths: Vec<_> = tasks.iter().map(|task| task.file_path.clone()).collect();
        assert_eq!(paths[0], paths[1], "still one file for one programme");
    }

    #[tokio::test]
    async fn edit_recording_rejects_overflowing_interval_without_persisting_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100))
            .expect("valid recording task");
        downloads.scheduled.write().await.push(task);
        downloads.persist_to_disk().await.expect("persist initial queue");
        let persisted_before = committed_records(&downloads).await;
        let revision_before = downloads.revision.load(std::sync::atomic::Ordering::SeqCst);
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
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
        assert_eq!(committed_records(&downloads).await, persisted_before);
        let scheduled = downloads.scheduled.read().await;
        assert_eq!(scheduled[0].scheduled_start(), Some(100));
        assert_ne!(scheduled[0].recording.program_title.as_deref(), Some("must not persist"));
    }

    #[tokio::test]
    async fn create_recording_without_download_config_reports_disabled() {
        // A missing `video.recording` block means the server has no
        // download engine at all — the caller's source identifiers are
        // not wrong. Reporting `InvalidSource` here sent clients
        // hunting for a misconfiguration that does not exist.
        let dir = tempfile::tempdir().expect("tempdir");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository"));
        let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };

        let result = service.create_recording(&claims, &create_input()).await;

        assert!(matches!(result, Err(ServiceError::Disabled)));
    }

    /// The record set the repository actually holds, for tests that
    /// assert a rejected mutation left persistence untouched.
    async fn committed_records(queue: &RecordingQueue) -> Vec<PersistedRecordingTask> {
        let repository = queue.repository.clone().expect("queue is repository backed");
        tokio::task::spawn_blocking(move || {
            let mut guard = repository.lock().expect("repository lock");
            guard.load()
        })
        .await
        .expect("join")
        .expect("load")
        .tasks
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

    fn filename_config(template: &str, container_format: RecordingContainerFormat) -> RecordingConfig {
        let mut cfg = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
        cfg.filename_template = template.to_string();
        cfg.container_format = container_format;
        cfg.timezone = "Europe/Berlin".parse().expect("timezone parses");
        cfg
    }

    fn filename_window(input: &CreateRecordingInput) -> EffectiveRecordingWindow {
        EffectiveRecordingWindow {
            scheduled_start: input.program_start,
            scheduled_end: input.program_end,
            execution_start: input.program_start,
            remaining_duration_secs: 0,
        }
    }

    fn filename_input() -> CreateRecordingInput {
        let mut input = create_input();
        input.program_title = "Hart van Nederland".to_string();
        input.channel_name = Some("┃NLZIET┃ SBS 6 HD".to_string());
        // 2023-11-14 22:13:20 UTC = 23:13 in Berlin.
        input.program_start = 1_700_000_000;
        input.program_end = 1_700_001_800;
        input
    }

    #[test]
    fn live_filename_renders_the_configured_template() {
        let input = filename_input();
        let cfg = filename_config("{channel}_{program_title}_{start_time}", RecordingContainerFormat::Mpegts);
        assert_eq!(
            render_live_filename(&input, &cfg, &filename_window(&input), "alice"),
            "NLZIET_SBS_6_HD_Hart_van_Nederland_2023-11-14_23-13.ts"
        );
    }

    #[test]
    fn live_filename_collapses_empty_placeholders() {
        let mut input = filename_input();
        input.channel_name = None;
        let cfg = filename_config("{channel}_{program_title}_{episode}", RecordingContainerFormat::Matroska);
        assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Hart_van_Nederland.mkv");
    }

    #[test]
    fn live_filename_renders_episode_and_owner() {
        let mut input = filename_input();
        input.epg = Some(shared::model::EpgEpisodeMetadata {
            programme_id: None,
            series_id: None,
            episode_id: None,
            season: Some(3),
            episode: Some(9),
            airing: shared::model::AiringStatus::default(),
        });
        let cfg = filename_config("{owner}-{program_title}-{episode}", RecordingContainerFormat::Mp4);
        assert_eq!(
            render_live_filename(&input, &cfg, &filename_window(&input), "alice"),
            "alice-Hart_van_Nederland-S03E09.mp4"
        );
    }

    #[test]
    fn live_filename_keeps_dots_and_does_not_double_the_extension() {
        let mut input = filename_input();
        input.program_title = "Mr. Robot".to_string();
        let cfg = filename_config("{program_title}.ts", RecordingContainerFormat::Mpegts);
        assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Mr.Robot.ts");
        let cfg = filename_config("{program_title}", RecordingContainerFormat::Matroska);
        assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Mr.Robot.mkv");
    }

    #[test]
    fn live_filename_without_template_uses_the_title() {
        let input = filename_input();
        let cfg = filename_config("", RecordingContainerFormat::Mpegts);
        assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Hart van Nederland.ts");
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
        let mut queue = PersistedRecordingQueue::default();
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
        assert_eq!(task.state, RecordingTaskState::Cancelled);
        assert!(task.finished);
        assert_eq!(task.recording.reserved_bytes, 0);
    }

    #[tokio::test]
    async fn preview_conflict_collects_demand_points_from_queue_state() {
        // The server-side preview must build its own demand points from
        // the committed queue state. A queued recording on the same
        // target/input pair must show up as `others` even when the
        // caller submits no `others` payload.
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads.json");
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let mut existing = persisted_rule_recording("existing", None, 100);
        // Place a padded window that overlaps 100..200.
        existing.recording.scheduled_start = Some(100);
        existing.recording.scheduled_end = Some(200);
        let existing = RecordingQueue::from_persisted(existing).expect("valid recording task");
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
        let downloads =
            Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
        let mut other_target = persisted_rule_recording("other-target", None, 100);
        other_target.recording.source = shared::model::recording::RecordingSource::new("other-target", "9", "input-a");
        let mut other_input = persisted_rule_recording("other-input", None, 100);
        other_input.recording.source = shared::model::recording::RecordingSource::new("1", "9", "input-b");
        let other_target_task = RecordingQueue::from_persisted(other_target).expect("valid task");
        let other_input_task = RecordingQueue::from_persisted(other_input).expect("valid task");
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
        cancelled_task.state = RecordingTaskState::Cancelled;
        assert!(!is_future_rule_recording(&cancelled_task, "rule-1", 1_800_000_000));

        let mut paused_task = persisted_rule_recording("uuid-p", Some("rule-1"), 1_900_000_000);
        paused_task.state = RecordingTaskState::Paused;
        assert!(!is_future_rule_recording(&paused_task, "rule-1", 1_800_000_000));

        // Sanity: the happy path still accepts editable future tasks.
        let scheduled_task = persisted_rule_recording("uuid-s", Some("rule-1"), 1_900_000_000);
        assert!(is_future_rule_recording(&scheduled_task, "rule-1", 1_800_000_000));
    }

    #[tokio::test]
    async fn create_recording_rejects_absent_recording_config() {
        let config = tuliprox_core::model::Config::default();
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
        let downloads = Arc::new(RecordingQueue::new());
        let service = RecordingService::new(Arc::clone(&downloads), app_config);
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
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
            group: None,
            provenance: RecordingProvenance::default(),
            epg: None,
        };

        let result = service.create_recording(&claims, &input).await;

        assert!(
            matches!(result, Err(ServiceError::Disabled)),
            "absent recording config must fail closed with Disabled, got: {result:?}"
        );
    }

    /// A service rooted at `dir` with the supplied disk block, and the
    /// source and server configuration a recording needs to resolve its
    /// own target and URL.
    fn service_with_disk(
        dir: &std::path::Path,
        queue: &Arc<RecordingQueue>,
        disk: Option<tuliprox_core::model::RecordingDiskConfig>,
    ) -> RecordingService {
        let mut rec_cfg =
            RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
        rec_cfg.directory = dir.to_string_lossy().into_owned();
        rec_cfg.disk = disk;
        let config = tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(rec_cfg),
            }),
            ..tuliprox_core::model::Config::default()
        };

        let input = Arc::new(tuliprox_core::model::ConfigInput { id: 7, name: "input-a".into(), ..Default::default() });
        let target = Arc::new(tuliprox_core::model::ConfigTarget {
            id: 11,
            enabled: true,
            name: "1".to_string(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            curation: None,
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = tuliprox_core::model::SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![tuliprox_core::model::ConfigSource { inputs: vec!["input-a".into()], targets: vec![target] }],
            ..tuliprox_core::model::SourcesConfig::default()
        };

        let app_config = test_app_config();
        app_config.config.store(Arc::new(config));
        app_config.sources.store(Arc::new(sources));
        RecordingService::new(Arc::clone(queue), app_config)
    }

    fn creating_claims() -> shared::model::Claims {
        shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn claims_for(user: &str, permissions: shared::model::permission::PermissionSet) -> shared::model::Claims {
        shared::model::Claims {
            username: user.to_string(),
            subject_id: Some(UserId::from(format!("web:{user}"))),
            permissions,
            ..creating_claims()
        }
    }

    fn media_input() -> CreateMediaRecordingInput {
        CreateMediaRecordingInput {
            source: RecordingSourceInput {
                target_id: "1".to_string(),
                virtual_id: "77".to_string(),
                cluster: XtreamCluster::Video,
                input_name: "input-a".to_string(),
            },
            title: "The Film".to_string(),
            extension: "mp4".to_string(),
            visibility: RecordingVisibility::Private,
            group: None,
            series_name: None,
        }
    }

    #[tokio::test]
    async fn a_media_request_needs_the_create_permission_not_manage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(dir.path(), &queue, None);

        let manage_only = claims_for("alice", Permission::RecordingManage.into());
        let refused = service.create_media_recording_idempotent(&manage_only, &media_input(), None).await;
        assert!(matches!(refused, Err(ServiceError::Forbidden)), "got {refused:?}");
        assert!(queue.queue.lock().await.is_empty());

        let create_only = claims_for("alice", Permission::RecordingCreate.into());
        let admitted = service.create_media_recording_idempotent(&create_only, &media_input(), None).await;
        assert!(admitted.is_ok(), "got {admitted:?}");
        let queued = queue.queue.lock().await;
        assert_eq!(queued.len(), 1, "a transfer is queued, not scheduled");
        assert_eq!(queued[0].kind, RecordingKind::Vod);
    }

    #[tokio::test]
    async fn a_second_user_gets_an_own_entry_and_a_repeat_is_a_duplicate() {
        // Another user asking for the same film must neither be handed the
        // first user's entry nor be refused; the same user asking twice is
        // a duplicate.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(dir.path(), &queue, None);
        let alice = claims_for("alice", Permission::RecordingCreate.into());
        let bob = claims_for("bob", Permission::RecordingCreate.into());

        let first = service.create_media_recording_idempotent(&alice, &media_input(), None).await.expect("alice");
        let second = service.create_media_recording_idempotent(&bob, &media_input(), None).await.expect("bob");
        assert_ne!(first.uuid, second.uuid);
        assert_eq!(second.owner_id, UserId::from("web:bob"));

        let repeat = service.create_media_recording_idempotent(&alice, &media_input(), None).await;
        assert!(matches!(repeat, Err(ServiceError::Duplicate)), "got {repeat:?}");
        assert_eq!(queue.queue.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn a_replayed_media_request_is_answered_without_queueing_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(dir.path(), &queue, None);
        let alice = claims_for("alice", Permission::RecordingCreate.into());
        let request = IdempotencyRequest { key: "k1".to_string(), fingerprint: "f1".to_string() };

        let first = service
            .create_media_recording_idempotent(&alice, &media_input(), Some(request.clone()))
            .await
            .expect("first request");
        let replay = service.create_media_recording_idempotent(&alice, &media_input(), Some(request)).await;
        assert_eq!(replay.err(), Some(ServiceError::IdempotentReplay { recording_id: first.uuid }));
        assert_eq!(queue.queue.lock().await.len(), 1);
    }

    fn with_private_quota(service: &RecordingService, bytes: u64) {
        let mut config = tuliprox_core::model::Config::clone(&service.app_config.config.load());
        if let Some(recording) = config.video.as_mut().and_then(|video| video.recording.as_mut()) {
            recording.quota = Some(tuliprox_core::model::RecordingQuotaConfig {
                default_private_bytes: Some(bytes),
                ..Default::default()
            });
        }
        service.app_config.config.store(Arc::new(config));
    }

    /// Alice's request for the film, finished at `bytes`.
    async fn completed_film(service: &RecordingService, queue: &RecordingQueue, bytes: u64) {
        service
            .create_media_recording_idempotent(&creating_claims(), &media_input(), None)
            .await
            .expect("alice's request");
        mutate(queue, |candidate| {
            let mut done = candidate.queue.remove(0);
            done.state = RecordingTaskState::Completed;
            done.finished = true;
            done.size = bytes;
            done.total_size = Some(bytes);
            done.recording.measured_bytes = bytes;
            candidate.finished.push(done);
            Ok(())
        })
        .await
        .expect("complete");
    }

    #[tokio::test]
    async fn asking_for_a_finished_file_is_charged_its_size_at_admission() {
        // Attaching copies the whole file into the new entry's charge, so a
        // 16 byte quota must not let a 4096 byte file in.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(dir.path(), &queue, None);
        completed_film(&service, &queue, 4096).await;
        with_private_quota(&service, 16);

        let bob = claims_for("bob", Permission::RecordingCreate.into());
        let refused = service.create_media_recording_idempotent(&bob, &media_input(), None).await;
        assert!(matches!(refused, Err(ServiceError::QuotaExceeded)), "got {refused:?}");

        with_private_quota(&service, 8192);
        service.create_media_recording_idempotent(&bob, &media_input(), None).await.expect("fits now");
        let queued = queue.queue.lock().await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].recording.reserved_bytes, 4096, "reserved at the known size until it attaches");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_concurrent_requests_with_one_key_admit_only_one() {
        // Both requests pass the first key lookup before either commits; the
        // lookup under the mutation guard admits only one of them.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = Arc::new(service_with_disk(dir.path(), &queue, None));
        let held = queue.queue.lock().await;
        let handles: Vec<_> = [("77", "body-a"), ("78", "body-b")]
            .into_iter()
            .map(|(virtual_id, fingerprint)| {
                let service = Arc::clone(&service);
                let mut input = media_input();
                input.source.virtual_id = virtual_id.to_string();
                let request = IdempotencyRequest { key: "same-key".to_string(), fingerprint: fingerprint.to_string() };
                tokio::spawn(async move {
                    service.create_media_recording_idempotent(&creating_claims(), &input, Some(request)).await
                })
            })
            .collect();
        // Both are past their first lookup and waiting to commit.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(held);

        let mut admitted = 0;
        for handle in handles {
            match handle.await.expect("join") {
                Ok(_) => admitted += 1,
                Err(error) => assert_eq!(error, ServiceError::IdempotencyConflict),
            }
        }
        assert_eq!(admitted, 1);
        assert_eq!(queue.queue.lock().await.len(), 1);
    }

    fn disk_test_input() -> CreateRecordingInput {
        let now = chrono::Utc::now().timestamp();
        CreateRecordingInput {
            source: RecordingSourceInput {
                target_id: "1".to_string(),
                virtual_id: "42".to_string(),
                cluster: XtreamCluster::Live,
                input_name: "input-a".to_string(),
            },
            program_title: "title".to_string(),
            program_start: now,
            program_end: now + 600,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
            group: None,
            provenance: RecordingProvenance::default(),
            epg: None,
        }
    }

    #[tokio::test]
    async fn a_recording_with_no_room_on_disk_is_refused() {
        // Logical quota and physical space are different questions, and
        // admission asks both: a full disk refuses the recording up front
        // instead of letting ffmpeg fail on ENOSPC. The safety margin drives
        // headroom to zero here rather than actually filling a filesystem.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(
            dir.path(),
            &queue,
            Some(tuliprox_core::model::RecordingDiskConfig {
                high_water_percent: None,
                low_water_percent: None,
                cleanup_interval_secs: None,
                safety_bytes: Some(u64::MAX),
            }),
        );

        let result = service.create_recording(&creating_claims(), &disk_test_input()).await;

        assert!(matches!(result, Err(ServiceError::DiskFull)), "got {result:?}");
        assert!(queue.scheduled.read().await.is_empty(), "a refused admission must not leave a recording behind");
    }

    #[tokio::test]
    async fn the_same_recording_is_admitted_when_the_disk_has_room() {
        // The counterpart: without the safety margin the identical request
        // succeeds, so the refusal above is the disk rule and not the
        // fixture failing for some unrelated reason.
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let service = service_with_disk(dir.path(), &queue, None);

        let result = service.create_recording(&creating_claims(), &disk_test_input()).await;

        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(queue.scheduled.read().await.len(), 1);
    }
}
