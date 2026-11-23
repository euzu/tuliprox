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
            mutate, mutate_with_idempotency, task_retains_media, IdempotencyOutcome, PersistedIdempotency,
            PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingQueue, RecordingTask,
            RecordingTaskState,
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
        .with_cluster(input.source.cluster)
        .with_provider_id(input.source.provider_id.clone())
        .with_is_group(input.source.is_group);
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
        let mut input = input.clone();
        if input.source.is_group {
            input.source.provider_id = None;
        } else if input.source.provider_id.is_none() {
            input.source.provider_id = source_resolution::resolve_live_provider_id(
                &self.app_config,
                &input.source.target_id,
                &input.source.input_name,
                &input.source.virtual_id,
            )
            .await;
        }
        let duration_secs = window.remaining_duration_secs;
        let (recording, reserved_bytes) =
            Self::build_live_recording(&owner_id, &claims.username, &input, group, recording_cfg, &window, &url)?;
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
        .with_cluster(input.source.cluster)
        .with_provider_id(input.source.provider_id.clone())
        .with_is_group(input.source.is_group);
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
    #[allow(clippy::too_many_lines)]
    pub(super) async fn admit(
        &self,
        recording: &RecordingTask,
        reserved_bytes: u64,
        owner_id: &UserId,
        idempotency: Option<&IdempotencyRequest>,
        recording_cfg: &tuliprox_core::model::RecordingConfig,
    ) -> Result<(), ServiceError> {
        let mut persisted = RecordingQueue::to_persisted(recording);
        let live_quotas = self.app_config.live_api_user_recording_quotas();
        let quota_limits = quota_limits_from_config(recording_cfg.quota.as_ref(), live_quotas);
        let max_concurrent = self.app_config.live_api_user_max_concurrent_recordings_for(owner_id);
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

        let is_live_ts = persisted.kind == RecordingKind::Live
            && (std::path::Path::new(&persisted.filename)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
                || std::path::Path::new(&persisted.file_path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("ts")));

        let enclosing_candidate =
            if is_live_ts { self.recordings.find_enclosing_candidate(&persisted).await } else { None };

        let (precomputed_offsets, precomputed_header) = if let Some(ref a) = enclosing_candidate {
            if a.state == RecordingTaskState::Completed {
                let start_ts = persisted.recording.scheduled_start.unwrap_or(0);
                let end_ts = persisted.recording.scheduled_end.unwrap_or(0);
                let offsets =
                    crate::recording::recording_worker::compute_clip_offsets(&a.file_path, start_ts, end_ts).await.ok();
                let header = crate::recording::recording_worker::read_clip_header(&a.file_path).await.ok();
                (offsets, header)
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let admit = |candidate: &mut PersistedRecordingQueue| -> Result<(), QueueMutationError> {
            let enclosing = find_enclosing_materialization(candidate, &persisted).cloned();
            if let Some(ref a) = enclosing {
                persisted.media_identity.clone_from(&a.media_identity);
                persisted.file_dir.clone_from(&a.file_dir);
                persisted.file_path.clone_from(&a.file_path);
                persisted.filename.clone_from(&a.filename);
                persisted.recording.relative_path.clone_from(&a.recording.relative_path);
                let start_ts = persisted.recording.scheduled_start.unwrap_or(0);
                let end_ts = persisted.recording.scheduled_end.unwrap_or(0);

                let completed_offsets = if a.state == RecordingTaskState::Completed
                    && enclosing_candidate.as_ref().is_some_and(|orig| orig.uuid == a.uuid)
                {
                    precomputed_offsets
                } else {
                    None
                };

                if let Some((start_offset, end_offset)) = completed_offsets {
                    crate::recording::complete_clip(
                        &mut persisted,
                        start_offset,
                        end_offset,
                        precomputed_header,
                        chrono::Utc::now().timestamp(),
                    );
                } else {
                    persisted.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
                        start_ts,
                        end_ts,
                        start_offset: None,
                        end_offset: None,
                        header_bytes: None,
                    });
                    persisted.recording.reserved_bytes = reserved_bytes;
                }
            } else {
                reserve_recording_relative_path(candidate, &mut persisted)?;
            }

            if candidate_has_duplicate_recording(candidate, recording) {
                return Err(QueueMutationError::Duplicate);
            }
            // Media another entry finished, or is fetching with a known size,
            // is charged at that size now: attaching copies the whole file
            // into this entry's charge, so admitting it at zero would let it
            // past the quota.
            let charge = if persisted.clip.is_some() {
                if persisted.recording.measured_bytes > 0 {
                    persisted.recording.measured_bytes
                } else {
                    persisted.recording.reserved_bytes
                }
            } else {
                reserved_bytes.max(known_media_size(candidate, &persisted))
            };
            persisted.recording.reserved_bytes = charge;
            let pool = recording_quota::quota_pool_for_task(&persisted);
            let used = used_bytes_for_pool(candidate, &pool);
            if matches!(
                recording_quota::would_exceed(&pool, used, charge, &quota_limits),
                AdmissionOutcome::OverLimit { .. }
            ) {
                return Err(QueueMutationError::QuotaExceeded);
            }
            if let Some(max_concurrent) = max_concurrent {
                check_concurrency_limit(
                    candidate,
                    &persisted,
                    owner_id,
                    max_concurrent,
                    chrono::Utc::now().timestamp(),
                )?;
            }
            // Quota is per-owner and logical; this is the physical
            // question, and one can pass while the other fails.
            if persisted.clip.is_none() {
                if let Some(free_bytes) = free_bytes {
                    let active = recording_disk::active_disk_reservations(candidate_tasks(candidate));
                    if matches!(
                        recording_disk::would_fit_on_disk(free_bytes, disk_safety_bytes, active, reserved_bytes),
                        recording_disk::DiskAdmission::Insufficient { .. }
                    ) {
                        return Err(QueueMutationError::DiskFull);
                    }
                }
            }
            if persisted.state == RecordingTaskState::Completed {
                candidate.finished.push(persisted.clone());
            } else if recording.kind.is_scheduled() {
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

pub trait EnclosingCandidate {
    fn kind(&self) -> RecordingKind;
    fn scheduled_start(&self) -> Option<i64>;
    fn scheduled_end(&self) -> Option<i64>;
    fn filename(&self) -> &str;
    fn file_path(&self) -> &std::path::Path;
    fn state(&self) -> RecordingTaskState;
    fn clip(&self) -> Option<&tuliprox_repository::recording_repository::RecordingClip>;
    fn retry_attempts(&self) -> u32;
    fn recording(&self) -> &shared::model::RecordingMetadata;
    fn url(&self) -> &str;
}

impl EnclosingCandidate for PersistedRecordingTask {
    fn kind(&self) -> RecordingKind { self.kind }
    fn scheduled_start(&self) -> Option<i64> { self.recording.scheduled_start }
    fn scheduled_end(&self) -> Option<i64> { self.recording.scheduled_end }
    fn filename(&self) -> &str { &self.filename }
    fn file_path(&self) -> &std::path::Path { &self.file_path }
    fn state(&self) -> RecordingTaskState { self.state }
    fn clip(&self) -> Option<&tuliprox_repository::recording_repository::RecordingClip> { self.clip.as_ref() }
    fn retry_attempts(&self) -> u32 { self.retry_attempts.into() }
    fn recording(&self) -> &shared::model::RecordingMetadata { &self.recording }
    fn url(&self) -> &str { self.url.as_str() }
}

impl EnclosingCandidate for crate::recording::recording_queue::RecordingTask {
    fn kind(&self) -> RecordingKind { self.kind }
    fn scheduled_start(&self) -> Option<i64> { self.recording.scheduled_start }
    fn scheduled_end(&self) -> Option<i64> { self.recording.scheduled_end }
    fn filename(&self) -> &str { &self.filename }
    fn file_path(&self) -> &std::path::Path { &self.file_path }
    fn state(&self) -> RecordingTaskState { self.state }
    fn clip(&self) -> Option<&tuliprox_repository::recording_repository::RecordingClip> { self.clip.as_ref() }
    fn retry_attempts(&self) -> u32 { self.retry_attempts.into() }
    fn recording(&self) -> &shared::model::RecordingMetadata { &self.recording }
    fn url(&self) -> &str { self.url.as_str() }
}

pub fn find_enclosing_in_candidates<'a, T: EnclosingCandidate>(
    candidates: impl IntoIterator<Item = &'a T>,
    new_task: &PersistedRecordingTask,
) -> Option<&'a T> {
    if new_task.kind != RecordingKind::Live {
        return None;
    }
    let (Some(b_start), Some(b_end)) = (new_task.recording.scheduled_start, new_task.recording.scheduled_end) else {
        return None;
    };
    if b_start >= b_end {
        return None;
    }

    let is_mpegts = |filename: &str, file_path: &std::path::Path| -> bool {
        std::path::Path::new(filename).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
            || file_path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
    };

    if !is_mpegts(&new_task.filename, &new_task.file_path) {
        return None;
    }

    let mut best: Option<(&'a T, i64)> = None;

    for task in candidates {
        if task.state() == RecordingTaskState::Failed || task.state() == RecordingTaskState::Cancelled {
            continue;
        }
        if task.clip().is_some() {
            continue;
        }
        if task.retry_attempts() > 0 {
            continue;
        }
        if !is_mpegts(task.filename(), task.file_path()) {
            continue;
        }
        if !crate::recording::recording_service::identity::same_live_source(
            task.recording(),
            task.url(),
            &new_task.recording,
            &new_task.url,
        ) {
            continue;
        }
        let (Some(a_start), Some(a_end)) = (task.scheduled_start(), task.scheduled_end()) else {
            continue;
        };
        if a_start <= b_start && a_end >= b_end {
            if a_start == b_start && a_end == b_end {
                continue;
            }
            let duration = a_end - a_start;
            if best.is_none_or(|(_, best_dur)| duration < best_dur) {
                best = Some((task, duration));
            }
        }
    }

    best.map(|(task, _)| task)
}

pub fn find_enclosing_materialization<'a>(
    candidate: &'a PersistedRecordingQueue,
    new_task: &PersistedRecordingTask,
) -> Option<&'a PersistedRecordingTask> {
    find_enclosing_in_candidates(
        candidate
            .scheduled
            .iter()
            .chain(candidate.queue.iter())
            .chain(candidate.active.iter())
            .chain(candidate.finished.iter().filter(|t| t.state == RecordingTaskState::Completed)),
        new_task,
    )
}

fn check_concurrency_limit(
    candidate: &PersistedRecordingQueue,
    new_task: &PersistedRecordingTask,
    owner_id: &UserId,
    max_concurrent: u32,
    now_ts: i64,
) -> Result<(), QueueMutationError> {
    if new_task.clip.is_some() || will_attach_to_existing_materialization(candidate, new_task) {
        return Ok(());
    }
    let overlapping_count = candidate_tasks(candidate)
        .filter(|existing| {
            !existing.state.is_terminal()
                && existing.recording.owner_id() == owner_id
                && windows_overlap(
                    existing.recording.scheduled_start,
                    existing.recording.scheduled_end,
                    new_task.recording.scheduled_start,
                    new_task.recording.scheduled_end,
                    now_ts,
                )
                && entry_needs_own_work(candidate, existing)
        })
        .count();

    let proposed = u32::try_from(overlapping_count).unwrap_or(u32::MAX).saturating_add(1);
    if proposed > max_concurrent {
        return Err(QueueMutationError::ConcurrencyExceeded);
    }
    Ok(())
}

pub(super) fn matching_existing_materialization<'a>(
    candidate: &'a PersistedRecordingQueue,
    uuid: &str,
    media_identity: &str,
) -> Option<&'a PersistedRecordingTask> {
    if media_identity.is_empty() {
        return None;
    }
    candidate_tasks(candidate).find(|other| {
        other.uuid != uuid
            && other.clip.is_none()
            && other.recording.deleting_previous_state.is_none()
            && !other.media_identity.is_empty()
            && other.media_identity == media_identity
            && task_retains_media(other)
    })
}

fn will_attach_to_existing_materialization(candidate: &PersistedRecordingQueue, task: &PersistedRecordingTask) -> bool {
    if task.clip.is_some() {
        return true;
    }
    matching_existing_materialization(candidate, &task.uuid, &task.media_identity).is_some()
}

fn entry_needs_own_work(candidate: &PersistedRecordingQueue, task: &PersistedRecordingTask) -> bool {
    if task.clip.is_some() {
        return false;
    }
    if task.media_identity.is_empty() {
        return true;
    }
    !candidate_tasks(candidate).any(|other| {
        other.uuid != task.uuid
            && other.media_identity == task.media_identity
            && (other.recording.owner_id() != task.recording.owner_id() || other.state == RecordingTaskState::Completed)
            && task_retains_media(other)
    })
}

pub(crate) fn windows_overlap(
    start1: Option<i64>,
    end1: Option<i64>,
    start2: Option<i64>,
    end2: Option<i64>,
    now_ts: i64,
) -> bool {
    match ((start1, end1), (start2, end2)) {
        ((Some(s1), Some(e1)), (Some(s2), Some(e2))) => {
            crate::recording::recording_conflict::intervals_overlap(s1, e1, s2, e2)
        }
        ((Some(s), Some(e)), _) | (_, (Some(s), Some(e))) => s <= now_ts && now_ts < e,
        _ => true,
    }
}
