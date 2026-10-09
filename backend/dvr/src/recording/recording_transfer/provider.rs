use super::{mark_recording_metadata_notification, RecordingNotificationPlan};
use crate::recording::{
    recording_notification::LifecycleEvent,
    recording_queue::{
        mutate_optional, QueueMutationError, RecordingControl, RecordingQueue, RecordingTaskState, RecordingWaitOutcome,
    },
};
use std::sync::Arc;
use tokio::{
    sync::{Notify, RwLock},
    time,
    time::Instant,
};
use tuliprox_core::model::RecordingConfig;

pub(super) type ProviderCapacities = Vec<(Arc<str>, usize, usize)>;

pub(super) enum ProviderAcquireResult {
    Acquired(Option<tuliprox_core::model::ProviderHandle>),
    Paused,
    Cancelled,
    Preempted,
    /// A live capture waited for capacity until its broadcast window closed.
    WindowClosed,
}

pub(super) fn background_download_should_wait(
    priority: i8,
    capacities: &[(Arc<str>, usize, usize)],
    download_cfg: &RecordingConfig,
) -> bool {
    if priority <= 0 || capacities.is_empty() {
        return false;
    }

    let background_limit = usize::from(download_cfg.max_background_per_provider);
    let reserve_slots = usize::from(download_cfg.reserve_slots_for_users);

    let blocked_by_background_limit =
        background_limit > 0 && capacities.iter().all(|(_, current, _)| *current >= background_limit);
    let blocked_by_reserved_slots = reserve_slots > 0
        && capacities.iter().all(|(_, current, max)| *max > 0 && current.saturating_add(reserve_slots) >= *max);

    blocked_by_background_limit || blocked_by_reserved_slots
}

pub(super) fn capacities_have_free_slot(capacities: &[(Arc<str>, usize, usize)]) -> bool {
    capacities.iter().any(|(_, current, max)| *max == 0 || current < max)
}

/// Wait for a provider slot, giving up when a live window closes.
///
/// Waiting past `scheduled_end` cannot produce the recording that was asked
/// for: the programme has finished. Only live work has a deadline; a transfer
/// waits as long as it takes.
pub(super) async fn wait_for_provider_slot(
    download_queue: &RecordingQueue,
    input_name: &Arc<str>,
    priority: i8,
    control_signal: &RwLock<RecordingControl>,
    control_notify: &Notify,
    deadline: Option<Instant>,
) -> Option<RecordingWaitOutcome> {
    let waiting =
        download_queue.slot_waiters.wait(Some(Arc::clone(input_name)), priority, control_signal, control_notify);
    match deadline {
        Some(deadline) => time::timeout_at(deadline, waiting).await.ok(),
        None => Some(waiting.await),
    }
}

/// What a control signal observed while acquiring a slot means; `None` keeps
/// trying.
pub(super) fn acquire_result_for_control(control: RecordingControl) -> Option<ProviderAcquireResult> {
    match control {
        RecordingControl::Cancel => Some(ProviderAcquireResult::Cancelled),
        RecordingControl::Pause => Some(ProviderAcquireResult::Paused),
        RecordingControl::Restart => Some(ProviderAcquireResult::Preempted),
        RecordingControl::None => None,
    }
}

/// What the end of a provider-slot wait means; `None` (signalled) tries to
/// acquire again. A wait without an outcome ran into the live window's end.
pub(super) fn acquire_result_after_wait(outcome: Option<RecordingWaitOutcome>) -> Option<ProviderAcquireResult> {
    match outcome {
        None => Some(ProviderAcquireResult::WindowClosed),
        Some(RecordingWaitOutcome::Signalled) => None,
        Some(RecordingWaitOutcome::Paused) => Some(ProviderAcquireResult::Paused),
        Some(RecordingWaitOutcome::Cancelled) => Some(ProviderAcquireResult::Cancelled),
        Some(RecordingWaitOutcome::Restarted) => Some(ProviderAcquireResult::Preempted),
    }
}

pub(super) async fn commit_acquired_download(
    download_queue: &RecordingQueue,
    uuid: &str,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        active.state = RecordingTaskState::Running;
        active.error = None;
        active.paused = false;
        active.finished = false;
        let notification = mark_recording_metadata_notification(&mut active.recording, LifecycleEvent::Started, None);
        Ok(Some(notification))
    })
    .await
}
