use super::{DISK_GONE_BEFORE_START, QUOTA_EXCEEDED_DURING_TRANSFER, QUOTA_GONE_BEFORE_START, SIBLING_QUOTA_EXCEEDED};
use crate::recording::recording_queue::{
    mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, RecordingQueue, RecordingTaskState,
};
use std::{path::Path, sync::Arc};
use tuliprox_core::model::AppConfig;

/// What the size check at the start of a transfer decided.
pub(super) struct SizeAdmission {
    /// Byte count the transfer may not exceed when its total is unknown.
    pub(super) byte_cap: Option<u64>,
    /// Waiting entries for the same media were refused for their own quota.
    pub(super) siblings_refused: bool,
}

/// Quota and disk admission once a transfer knows how large it is.
///
/// A VOD or series request is admitted before its size is known, so it
/// reserves nothing up front. The first response carries the size, and this
/// is the first point where the charge can be checked: against the
/// transfer's own pool, against every entry waiting to attach to the same
/// file, and against the disk.
pub(super) struct QuotaGate<'a> {
    pub(super) queue: &'a RecordingQueue,
    pub(super) app_config: &'a AppConfig,
}

impl QuotaGate<'_> {
    pub(super) async fn admit_size(
        &self,
        uuid: &str,
        total: Option<u64>,
        on_disk: u64,
    ) -> Result<SizeAdmission, String> {
        let config = self.app_config.config.load();
        let Some(recording_cfg) = config.recording() else {
            return Ok(SizeAdmission { byte_cap: None, siblings_refused: false });
        };
        let limits = crate::recording::recording_service::quota_limits_from_config(recording_cfg.quota.as_ref());
        // Measured before the mutation: it is a syscall.
        let free = crate::recording::recording_disk::free_bytes_for(Path::new(&recording_cfg.directory));
        let safety = recording_cfg.disk.as_ref().and_then(|disk| disk.safety_bytes).unwrap_or(0);

        let decided = mutate_optional(self.queue, |candidate| {
            let Some(subject) = candidate.active.iter().find(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            let pool = crate::recording::recording_quota::quota_pool_for_task(subject);
            let media = subject.media_identity.clone();
            let used = used_by_others(candidate, uuid, &pool);
            let Some(total) = total else {
                let byte_cap = crate::recording::recording_quota::limit_for_pool(&pool, &limits)
                    .map(|limit| limit.saturating_sub(used));
                return Ok(Some(Ok(SizeAdmission { byte_cap, siblings_refused: false })));
            };
            if over_limit(&pool, used, total, &limits) {
                return Ok(Some(Err(QUOTA_EXCEEDED_DURING_TRANSFER.to_string())));
            }
            if let Some(free) = free {
                let running = crate::recording::recording_disk::active_disk_reservations(
                    all_tasks(candidate).filter(|task| task.uuid != uuid),
                );
                if matches!(
                    crate::recording::recording_disk::would_fit_on_disk(
                        free,
                        safety,
                        running,
                        total.saturating_sub(on_disk)
                    ),
                    crate::recording::recording_disk::DiskAdmission::Insufficient { .. }
                ) {
                    return Ok(Some(Err(DISK_GONE_BEFORE_START.to_string())));
                }
            }
            if let Some(active) = candidate.active.iter_mut().find(|task| task.uuid == uuid) {
                active.recording.reserved_bytes = active.recording.reserved_bytes.max(total);
            }
            let siblings_refused = charge_waiting_siblings(candidate, &media, total, &limits);
            Ok(Some(Ok(SizeAdmission { byte_cap: None, siblings_refused })))
        })
        .await
        .map_err(|err| format!("Could not record the transfer size: {err}"))?;
        // The task left the active slot meanwhile; the worker finds out next.
        decided.unwrap_or(Ok(SizeAdmission { byte_cap: None, siblings_refused: false }))
    }
}

fn all_tasks(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &PersistedRecordingTask> {
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .chain(candidate.finished.iter())
}

fn used_by_others(
    candidate: &PersistedRecordingQueue,
    uuid: &str,
    pool: &crate::recording::recording_quota::QuotaPool,
) -> u64 {
    crate::recording::recording_quota::used_bytes_in_pool(all_tasks(candidate).filter(|task| task.uuid != uuid), pool)
}

fn over_limit(
    pool: &crate::recording::recording_quota::QuotaPool,
    used: u64,
    charge: u64,
    limits: &crate::recording::recording_quota::QuotaLimits,
) -> bool {
    matches!(
        crate::recording::recording_quota::would_exceed(pool, used, charge, limits),
        crate::recording::recording_quota::AdmissionOutcome::OverLimit { .. }
    )
}

/// Charge every queued entry that will attach to this file with its size,
/// and refuse the ones whose own quota cannot take it. Returns whether any
/// entry was refused.
///
/// They were admitted while the size was unknown. Attaching later copies the
/// whole file into their charge, so this is the last point where their quota
/// can still say no.
pub(super) fn charge_waiting_siblings(
    candidate: &mut PersistedRecordingQueue,
    media: &str,
    total: u64,
    limits: &crate::recording::recording_quota::QuotaLimits,
) -> bool {
    if media.is_empty() {
        return false;
    }
    let mut refused = Vec::new();
    let waiting: Vec<String> =
        candidate.queue.iter().filter(|task| task.media_identity == media).map(|task| task.uuid.clone()).collect();
    for uuid in waiting {
        let Some(sibling) = candidate.queue.iter().find(|task| task.uuid == uuid) else {
            continue;
        };
        let pool = crate::recording::recording_quota::quota_pool_for_task(sibling);
        let used = used_by_others(candidate, &uuid, &pool);
        if over_limit(&pool, used, total, limits) {
            refused.push(uuid);
        } else if let Some(sibling) = candidate.queue.iter_mut().find(|task| task.uuid == uuid) {
            sibling.recording.reserved_bytes = sibling.recording.reserved_bytes.max(total);
        }
    }
    for uuid in &refused {
        if let Some(index) = candidate.queue.iter().position(|task| &task.uuid == uuid) {
            let mut failed = candidate.queue.remove(index);
            failed.finished = true;
            failed.paused = false;
            failed.next_retry_at = None;
            failed.state = RecordingTaskState::Failed;
            failed.error = Some(SIBLING_QUOTA_EXCEEDED.to_string());
            failed.recording.reserved_bytes = 0;
            candidate.finished.push(failed);
        }
    }
    !refused.is_empty()
}

/// Re-run admission for the recording that is about to open its
/// destination, returning the reason it can no longer start.
///
/// Admission happened when the request was accepted, which can be a long
/// time before this point: the recording may have waited for provider
/// capacity, sat through a retry backoff, or been scheduled hours ahead.
/// The quota and the free space it was admitted against are not the ones
/// it is about to consume. Without this the first sign of a full disk is
/// a write failure part-way through a recording.
///
/// The subject is excluded from both sums and then added back as the
/// candidate charge, so it is not counted twice.
pub(super) async fn refused_before_start(
    download_queue: &Arc<RecordingQueue>,
    app_config: &AppConfig,
    uuid: &str,
) -> Option<&'static str> {
    let config = app_config.config.load();
    let recording_cfg = config.recording()?;
    let (_, tasks) = download_queue.committed_snapshot().await;
    let subject = tasks.iter().find(|task| task.uuid == uuid)?;
    let charge = crate::recording::recording_quota::charge_for_task(subject);
    let others = || tasks.iter().filter(|task| task.uuid != uuid);

    let limits = crate::recording::recording_service::quota_limits_from_config(recording_cfg.quota.as_ref());
    let pool = crate::recording::recording_quota::quota_pool_for_task(subject);
    let used = crate::recording::recording_quota::used_bytes_in_pool(others(), &pool);
    if matches!(
        crate::recording::recording_quota::would_exceed(&pool, used, charge, &limits),
        crate::recording::recording_quota::AdmissionOutcome::OverLimit { .. }
    ) {
        return Some(QUOTA_GONE_BEFORE_START);
    }

    // An unmeasurable root is not grounds to refuse, exactly as at admission.
    let free = crate::recording::recording_disk::free_bytes_for(Path::new(&recording_cfg.directory))?;
    let safety = recording_cfg.disk.as_ref().and_then(|disk| disk.safety_bytes).unwrap_or(0);
    let active = crate::recording::recording_disk::active_disk_reservations(others());
    if matches!(
        crate::recording::recording_disk::would_fit_on_disk(free, safety, active, charge),
        crate::recording::recording_disk::DiskAdmission::Insufficient { .. }
    ) {
        return Some(DISK_GONE_BEFORE_START);
    }
    None
}
