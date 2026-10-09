use super::{
    mutate_optional, mutation::mutate_optional_locked, promote_from_queue, waiters::lock_unpoisoned,
    PersistedRecordingQueue, QueueMutationError, RecordingQueue, RecordingTask, RecordingTaskState,
    RECORDING_WINDOW_EXPIRED_ERR,
};
use crate::recording::{recording_transition, recording_transition::RecordingCommand};
use shared::model::{QueueRevision, RecordingKind};
use std::sync::{atomic::Ordering, Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RecordingControl {
    #[default]
    None,
    Pause,
    Cancel,
    Restart,
}

impl RecordingQueue {
    pub(super) async fn snapshot_current(&self, revision: QueueRevision) -> PersistedRecordingQueue {
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;

        PersistedRecordingQueue {
            queue: queue.iter().map(Self::to_persisted).collect(),
            scheduled: scheduled.iter().map(Self::to_persisted).collect(),
            active: active.iter().map(Self::to_persisted).collect(),
            finished: finished.iter().map(Self::to_persisted).collect(),
            revision,
        }
    }

    /// The provider input and the priority the active transfer should run at.
    ///
    /// One transfer serves every entry pointing at its file, so it runs at the
    /// strongest priority any of them asked for. Taking the active entry's own
    /// value lets a background request hold back somebody else's foreground one
    /// for the same media. Lower is stronger.
    ///
    /// Takes the mutation guard first, like `committed_snapshot`, so the two
    /// cannot interleave their inner locks.
    pub async fn active_scheduling_priority(&self, uuid: &str) -> Option<(Option<Arc<str>>, i8)> {
        let _mutation = self.mutation_guard.lock().await;
        let active = self.active.read().await;
        let active = active.iter().find(|task| task.uuid == uuid)?;
        let media = crate::recording::recording_service::recording_identity_key(&active.recording, active.url.as_str());
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let finished = self.finished.read().await;
        let strongest = queue
            .iter()
            .chain(scheduled.iter())
            .chain(finished.iter())
            .filter(|task| {
                task.uuid != active.uuid
                    && crate::recording::recording_service::recording_identity_key(&task.recording, task.url.as_str())
                        == media
            })
            .fold(active.priority, |strongest, task| strongest.min(task.priority));
        Some((active.input_name.clone(), strongest))
    }

    pub async fn committed_snapshot(&self) -> (QueueRevision, Vec<RecordingTask>) {
        let _mutation = self.mutation_guard.lock().await;
        let revision = QueueRevision(self.revision.load(Ordering::SeqCst));
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut tasks = Vec::with_capacity(queue.len() + scheduled.len() + finished.len() + active.len());
        tasks.extend(queue.iter().cloned());
        tasks.extend(scheduled.iter().cloned());
        tasks.extend(active.iter().cloned());
        tasks.extend(finished.iter().cloned());
        (revision, tasks)
    }

    pub async fn committed_partitioned_snapshot(&self) -> (Vec<RecordingTask>, Vec<RecordingTask>, Vec<RecordingTask>) {
        let _mutation = self.mutation_guard.lock().await;
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut queued = Vec::with_capacity(queue.len() + scheduled.len());
        queued.extend(queue.iter().cloned());
        queued.extend(scheduled.iter().cloned());
        (queued, active.clone(), finished.clone())
    }

    pub(super) fn finalize_missed_recording(mut task: RecordingTask) -> RecordingTask {
        task.finished = true;
        task.paused = false;
        task.state = RecordingTaskState::Failed;
        task.error = Some(RECORDING_WINDOW_EXPIRED_ERR.to_string());
        task.recording.reserved_bytes = 0;
        task
    }

    pub(super) fn recording_start_missed_window(task: &RecordingTask, now_ts: i64) -> bool {
        task.kind == RecordingKind::Live
            && task.scheduled_start().zip(task.scheduled_duration_secs()).is_some_and(|(start_at, duration_secs)| {
                shared::model::recording_math::window_elapsed(start_at, duration_secs, now_ts)
            })
    }

    /// Pause the active task. Persists the new state through the
    /// transactional boundary. The runtime-only control signal is published
    /// after the commit while the mutation guard still preserves ordering.
    /// Live captures are not pausable and are rejected here.
    pub async fn pause_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            active.state = recording_transition::transition(active.kind, active.state, RecordingCommand::Pause)
                .map_err(|_| QueueMutationError::StateNotEditable)?;
            active.paused = true;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        let worker = self.worker(uuid);
        *worker.control_signal.write().await = RecordingControl::Pause;
        worker.control_notify.notify_waiters();
        Ok(true)
    }

    /// Resume the active task. Persists the new state through the
    /// transactional boundary.
    pub async fn resume_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid && active.paused) else {
                return Ok(None);
            };
            active.state = recording_transition::transition(active.kind, active.state, RecordingCommand::Resume)
                .map_err(|_| QueueMutationError::StateNotEditable)?;
            active.paused = false;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        let worker = self.worker(uuid);
        *worker.control_signal.write().await = RecordingControl::None;
        worker.control_notify.notify_waiters();
        Ok(true)
    }

    /// Cancel the active task `uuid`, if it is still the active one.
    ///
    /// A paused task with no running worker is filed as `Cancelled` immediately.
    /// A worker still closing a paused stream is asked to cancel just like a
    /// running one: it commits `Cancelled` after releasing its file and slot.
    /// Returns whether cancellation finished immediately, or `None` when
    /// `uuid` is not active.
    pub async fn cancel_requested(&self, uuid: &str) -> Result<Option<bool>, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let worker_running = match self.existing_worker(uuid) {
            Some(worker) => *worker.running.read().await,
            None => false,
        };
        let cancelled_immediately = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.iter().find(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            let cancelled_immediately = active.paused && !worker_running;
            if cancelled_immediately {
                let Some(index) = candidate.active.iter().position(|task| task.uuid == uuid) else {
                    return Ok(None);
                };
                let mut cancelled = candidate.active.remove(index);
                cancelled.finished = true;
                cancelled.paused = false;
                cancelled.next_retry_at = None;
                cancelled.error.get_or_insert_with(|| "Cancelled by user".to_string());
                cancelled.state = RecordingTaskState::Cancelled;
                cancelled.recording.reserved_bytes = 0;
                candidate.finished.push(cancelled);
                promote_from_queue(candidate);
            } else if let Some(active) = candidate.active.iter_mut().find(|task| task.uuid == uuid) {
                // The worker still owns the file and the provider slot; it
                // commits `Cancelled` once it has let go.
                active.state = if active.paused {
                    RecordingTaskState::Cancelling
                } else {
                    recording_transition::transition(
                        active.kind,
                        active.state,
                        recording_transition::RecordingCommand::Cancel,
                    )
                    .unwrap_or(RecordingTaskState::Cancelling)
                };
                active.paused = false;
                active.error = Some("Cancelled by user".to_string());
                active.next_retry_at = None;
            }
            Ok(Some(cancelled_immediately))
        })
        .await?;

        if let Some(cancelled_immediately) = cancelled_immediately {
            let worker = if cancelled_immediately { self.existing_worker(uuid) } else { Some(self.worker(uuid)) };
            if let Some(worker) = worker {
                *worker.control_signal.write().await =
                    if cancelled_immediately { RecordingControl::None } else { RecordingControl::Cancel };
                worker.control_notify.notify_waiters();
                if cancelled_immediately {
                    lock_unpoisoned(&self.workers).remove(uuid);
                }
            }
        }
        Ok(cancelled_immediately)
    }

    /// Ask the worker to restart so it picks up a reloaded configuration.
    ///
    /// A restart never replaces a pending pause or cancel. The fallback below
    /// runs later than the caller, so it may land after a user's pause; it
    /// must not turn that pause into a requeue.
    pub fn request_worker_restart(&self) {
        fn request(control: &mut RecordingControl) -> bool {
            if *control == RecordingControl::None {
                *control = RecordingControl::Restart;
            }
            *control == RecordingControl::Restart
        }
        let workers: Vec<_> = lock_unpoisoned(&self.workers).values().cloned().collect();
        for worker in workers {
            if let Ok(mut control) = worker.control_signal.try_write() {
                if request(&mut control) {
                    worker.control_notify.notify_waiters();
                }
            } else {
                let worker = Arc::clone(&worker);
                tokio::spawn(async move {
                    let mut control = worker.control_signal.write().await;
                    if request(&mut control) {
                        worker.control_notify.notify_waiters();
                    }
                });
            }
        }
    }

    pub async fn remove_from_queue(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let queue_len = candidate.queue.len();
            candidate.queue.retain(|task| task.uuid != uuid);
            if candidate.queue.len() != queue_len {
                return Ok(Some(true));
            }
            let scheduled_len = candidate.scheduled.len();
            candidate.scheduled.retain(|task| task.uuid != uuid);
            Ok((candidate.scheduled.len() != scheduled_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let initial_len = candidate.finished.len();
            candidate.finished.retain(|task| task.uuid != uuid);
            Ok((candidate.finished.len() != initial_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let original_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            candidate.queue.retain(|task| task.uuid != uuid);
            candidate.scheduled.retain(|task| task.uuid != uuid);
            candidate.finished.retain(|task| task.uuid != uuid);
            let current_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            Ok((current_len != original_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    /// Requeue a finished VOD/Series transfer. A Live capture cannot be
    /// retried: its programme window is gone.
    pub async fn retry_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            if let Some(pos) = candidate.finished.iter().position(|task| task.uuid == uuid) {
                let mut task = candidate.finished.remove(pos);
                let Ok(next) = recording_transition::transition(task.kind, task.state, RecordingCommand::Retry) else {
                    candidate.finished.insert(pos, task);
                    return Ok(None);
                };
                task.finished = false;
                task.size = 0;
                task.paused = false;
                task.error = None;
                task.state = next;
                task.retry_attempts = 0;
                task.next_retry_at = None;
                candidate.queue.push(task);
                Ok(Some(true))
            } else {
                Ok(None)
            }
        })
        .await?
        .unwrap_or(false))
    }
}
