use super::{
    error::map_edit_validation_error, window::padding_bounds, RecordingService, RecordingSourceInput, ServiceError,
};
use crate::{
    recording::recording_queue::{
        PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingTask, RecordingTaskState,
    },
    recording_edit,
    recording_edit::EditError,
    recording_quota,
    recording_quota::{QuotaLimits, QuotaPool},
};
use shared::model::{RecordingKind, UserId};
use std::{collections::HashMap, sync::Arc};
use tuliprox_auth::{authorize, RecordingAction, RecordingDecision, RecordingSubject, TerminalState};

impl RecordingService {
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
pub(super) fn authorize_task_in_candidate(
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

pub(super) fn candidate_tasks(
    candidate: &PersistedRecordingQueue,
) -> impl Iterator<Item = &PersistedRecordingTask> + '_ {
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
pub(super) fn used_bytes_for_pool(candidate: &PersistedRecordingQueue, pool: &QuotaPool) -> u64 {
    recording_quota::used_bytes_in_pool(candidate_tasks(candidate), pool)
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

pub(super) async fn collect_demand_points_for_provider(
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
