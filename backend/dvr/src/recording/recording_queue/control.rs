use super::{
    mutate_optional, mutation::mutate_optional_locked, promote_from_queue, waiters::lock_unpoisoned,
    PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingQueue, RecordingTask,
    RecordingTaskState, RecordingVodEntry, RECORDING_WINDOW_EXPIRED_ERR,
};
use crate::recording::{recording_transition, recording_transition::RecordingCommand};
use shared::model::{QueueRevision, RecordingKind, RecordingVisibility, UserId};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
};

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CancelledEnclosingAction {
    pub files_to_compact: Vec<PathBuf>,
    pub files_to_delete: Vec<PathBuf>,
}

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
            next_vod_id: self.vod_id_counter.load(Ordering::SeqCst),
        }
    }

    /// Completed recordings this API user may see, projected for the Xtream VOD
    /// listing. Only the finished partition can hold them, so no other task is cloned.
    ///
    /// `subject_id`, `target_name` and `target_id` describe the caller's target. A
    /// recording is projected only when the caller could possibly see it: they own it,
    /// or it is shared and was recorded from their target (by name or id). The full
    /// visibility rule still runs afterwards; this only avoids cloning metadata the
    /// rule would reject anyway.
    ///
    /// The mutation guard is deliberately not taken: a mutation publishes the
    /// finished partition under its write lock only after its records are persisted,
    /// so a read lock already observes committed state. Taking the guard would make
    /// every catalogue request wait for a recording's disk write.
    pub async fn committed_recording_vod_entries(
        &self,
        subject_id: &UserId,
        target_name: &str,
        target_id: u16,
    ) -> Vec<RecordingVodEntry> {
        let target_id = target_id.to_string();
        let finished = self.finished.read().await;
        finished
            .iter()
            .filter(|task| vod_entry_may_be_visible(task, subject_id, target_name, &target_id))
            .filter_map(recording_vod_entry)
            .collect()
    }

    /// The completed recording published under `vod_id`, projected for playback.
    ///
    /// Same partition and the same read-lock invariant as
    /// [`Self::committed_recording_vod_entries`]; only the projected task is built.
    pub async fn committed_recording_vod_entry(&self, vod_id: u32) -> Option<RecordingVodEntry> {
        let finished = self.finished.read().await;
        finished.iter().find(|task| task.vod_id == vod_id).and_then(recording_vod_entry)
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

    pub async fn committed_tasks_for_file(&self, file_path: &Path) -> Vec<RecordingTask> {
        let _mutation = self.mutation_guard.lock().await;
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut tasks = Vec::new();
        for t in queue.iter().chain(scheduled.iter()).chain(active.iter()).chain(finished.iter()) {
            if t.file_path == file_path {
                tasks.push(t.clone());
            }
        }
        tasks
    }

    pub async fn find_enclosing_candidate(&self, new_task: &PersistedRecordingTask) -> Option<PersistedRecordingTask> {
        let _mutation = self.mutation_guard.lock().await;
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;

        let candidates = scheduled
            .iter()
            .chain(queue.iter())
            .chain(active.iter())
            .chain(finished.iter().filter(|t| t.state == RecordingTaskState::Completed));

        crate::recording::recording_service::find_enclosing_in_candidates(candidates, new_task).map(Self::to_persisted)
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
        let mut worker_cancelled = false;
        let cancelled_immediately = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
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
                worker_cancelled = true;
                Ok(Some(true))
            } else {
                let active_media = active.media_identity.clone();
                let now_ts = chrono::Utc::now().timestamp();
                let remaining_attached_end = latest_attached_clip_end(&candidate.scheduled, &active_media);

                if let Some(latest_end) = remaining_attached_end {
                    active.recording.scheduled_end = Some(latest_end.max(now_ts));
                    active.cancelled_by_owner = true;
                    active.recording.reserved_bytes = 0;
                    return Ok(Some(false));
                }

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
                active.recording.reserved_bytes = 0;
                worker_cancelled = true;
                Ok(Some(false))
            }
        })
        .await?;

        if let Some(cancelled_immediately) = cancelled_immediately {
            if worker_cancelled {
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
        }
        Ok(cancelled_immediately)
    }

    /// Update stop time for an active enclosing recording marked cancelled by owner
    /// whose attached clips have changed. If no attached clips remain or they end in the
    /// past, moves stop time to now and sets state to Cancelling, returning the uuid of
    /// the worker that should be cancelled.
    pub fn update_cancelled_enclosing_stop_time(
        candidate: &mut PersistedRecordingQueue,
        media_identity: &str,
    ) -> Option<String> {
        if media_identity.is_empty() {
            return None;
        }
        let active =
            candidate.active.iter_mut().find(|t| t.cancelled_by_owner && t.media_identity == media_identity)?;
        let now_ts = chrono::Utc::now().timestamp();
        let remaining_latest = latest_attached_clip_end(&candidate.scheduled, media_identity);

        if let Some(latest) = remaining_latest {
            active.recording.scheduled_end = Some(latest.max(now_ts));
            None
        } else {
            active.recording.scheduled_end = Some(now_ts);
            active.state = RecordingTaskState::Cancelling;
            Some(active.uuid.clone())
        }
    }

    /// Remove any finished enclosing recording marked `cancelled_by_owner`
    /// whose attached clips are all settled (no clips in scheduled or active).
    /// If another non-clip task still references the file, neither compacts nor deletes;
    /// else if remaining clips exist on the file, schedules compaction;
    /// if none remain, schedules deletion.
    pub fn cleanup_settled_cancelled_enclosings(candidate: &mut PersistedRecordingQueue) -> CancelledEnclosingAction {
        let mut action = CancelledEnclosingAction::default();
        let mut i = 0;
        while i < candidate.finished.len() {
            let task = &candidate.finished[i];
            if task.cancelled_by_owner && task.clip.is_none() {
                let file_path = task.file_path.clone();
                let media = task.media_identity.clone();
                let references_file = |t: &PersistedRecordingTask| {
                    t.file_path == file_path || (!media.is_empty() && t.media_identity == media)
                };
                let has_unsettled_clips = candidate
                    .scheduled
                    .iter()
                    .chain(candidate.active.iter())
                    .any(|t| t.clip.is_some() && references_file(t));

                if !has_unsettled_clips {
                    candidate.finished.remove(i);
                    let remaining_tasks = || {
                        candidate
                            .queue
                            .iter()
                            .chain(candidate.scheduled.iter())
                            .chain(candidate.active.iter())
                            .chain(candidate.finished.iter())
                    };

                    let has_remaining_non_clip =
                        remaining_tasks().any(|t| t.clip.is_none() && references_file(t) && task_retains_media(t));
                    if !has_remaining_non_clip {
                        let has_remaining_clips = remaining_tasks().any(|t| t.clip.is_some() && references_file(t));
                        if has_remaining_clips {
                            if !action.files_to_compact.contains(&file_path) {
                                action.files_to_compact.push(file_path);
                            }
                        } else if !action.files_to_delete.contains(&file_path) {
                            action.files_to_delete.push(file_path);
                        }
                    }
                    continue;
                }
            }
            i += 1;
        }
        action
    }

    /// Stop a running worker early with a cancel control signal.
    pub async fn stop_worker_early(&self, uuid: &str) {
        if let Some(worker) = self.existing_worker(uuid) {
            *worker.control_signal.write().await = RecordingControl::Cancel;
            worker.control_notify.notify_waiters();
        }
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
                task.recording.provider_started = false;
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

/// The Xtream VOD projection of one finished task, if it is a completed recording with a file.
pub(super) fn recording_vod_entry(task: &RecordingTask) -> Option<RecordingVodEntry> {
    let eligible =
        task.vod_id != 0 && task.state == RecordingTaskState::Completed && task.recording.relative_path.is_some();
    eligible.then(|| {
        let title = task
            .recording
            .program_title
            .as_deref()
            .map(str::trim)
            .filter(|trimmed| !trimmed.is_empty())
            .map_or_else(|| task.filename.clone(), ToString::to_string);
        RecordingVodEntry {
            uuid: task.uuid.clone(),
            vod_id: task.vod_id,
            filename: task.filename.clone(),
            title,
            duration_secs: recorded_duration_secs(task),
            added: task.recording.completed_at,
            recording: task.recording.clone(),
        }
    })
}

/// Whether the caller could see `task` at all, judged from fields only.
///
/// Mirrors the owner/target part of the API-user visibility rule: the owner always
/// sees their recording, and a shared recording is bound to the target it was
/// recorded from, matched by name or by id.
fn vod_entry_may_be_visible(task: &RecordingTask, subject_id: &UserId, target_name: &str, target_id: &str) -> bool {
    if task.recording.owner_id() == subject_id {
        return true;
    }
    task.recording.visibility == RecordingVisibility::Shared
        && (task.recording.source.target_id == target_name || task.recording.source.target_id == target_id)
}

/// Length of the recorded window in seconds.
///
/// A clip covers its own interval; otherwise the padded programme window is what the
/// worker actually recorded, and the unpadded programme window is the fallback. A
/// media transfer without a programme window has no length.
pub(super) fn recorded_duration_secs(task: &RecordingTask) -> Option<u64> {
    if let Some(clip) = task.clip.as_ref() {
        return u64::try_from(clip.end_ts.saturating_sub(clip.start_ts)).ok().filter(|secs| *secs > 0);
    }
    let (start, end) = match (task.recording.scheduled_start, task.recording.scheduled_end) {
        (Some(start), Some(end)) => (start, end),
        _ => (task.recording.program_start?, task.recording.program_end?),
    };
    u64::try_from(end.saturating_sub(start)).ok().filter(|secs| *secs > 0)
}

/// Returns whether a recording task retains on-disk media (not terminal, or completed).
pub fn task_retains_media(task: &PersistedRecordingTask) -> bool {
    !task.state.is_terminal() || task.state == RecordingTaskState::Completed
}

fn latest_attached_clip_end(scheduled: &[PersistedRecordingTask], media_identity: &str) -> Option<i64> {
    if media_identity.is_empty() {
        return None;
    }
    scheduled
        .iter()
        .filter(|t| !t.media_identity.is_empty() && t.media_identity == media_identity)
        .filter_map(|t| t.clip.as_ref().map(|c| c.end_ts))
        .max()
}
