use super::{
    conflicts::{candidate_tasks, used_bytes_for_pool},
    control::{authorize_create_recording, map_queue_error},
    deletion::known_media_size,
    error::map_edit_validation_error,
    filenames::{render_live_filename, reserve_recording_relative_path},
    identity::candidate_has_duplicate_recording,
    quota_limits_from_config, source_resolution,
    window::{effective_recording_window, padding_bounds},
    CreateMediaRecordingInput, CreateRecordingInput, EffectiveRecordingWindow, RecordingCtx, RecordingService,
    RecordingSourceInput, RecordingTaskView, ServiceError,
};
use crate::{
    recording::{
        recording_disk,
        recording_queue::{
            mutate, mutate_with_idempotency, IdempotencyOutcome, PersistedIdempotency, PersistedRecordingQueue,
            QueueMutationError, RecordingQueue, RecordingTask, RecordingTaskState,
        },
    },
    recording_edit,
    recording_edit::EditError,
    recording_quota,
    recording_quota::AdmissionOutcome,
};
use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingSource},
    EventSink, RecordingKind, UserId,
};
use std::sync::Arc;
use tuliprox_core::model::AppConfig;

/// An `Idempotency-Key` and a digest of the body it arrived with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdempotencyRequest {
    pub key: String,
    pub fingerprint: String,
}

impl RecordingService {
    /// Construct from the queue and app config.
    pub fn new(recordings: Arc<RecordingQueue>, app_config: Arc<AppConfig>) -> Self { Self { recordings, app_config } }

    /// Convenience constructor from the DVR's context.
    pub fn from_ctx<E: EventSink + Clone + 'static>(ctx: &RecordingCtx<E>) -> Self {
        Self::new(ctx.recordings.clone(), ctx.app_config.clone())
    }

    pub(super) fn subject_id(claims: &shared::model::Claims) -> Result<UserId, ServiceError> {
        claims.subject_id.clone().ok_or(ServiceError::UnknownOwner)
    }

    pub(super) fn recording_url(&self, source: &RecordingSourceInput) -> Option<String> {
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
    pub(super) fn build_live_recording(
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
    pub(super) async fn live_group(
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
    pub(super) async fn check_idempotency(
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

    pub(super) fn enabled_config(&self) -> Result<Arc<tuliprox_core::model::Config>, ServiceError> {
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
    pub(super) async fn admit(
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
}
