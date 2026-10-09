use super::{
    IdempotencyOutcome, PersistedIdempotency, PersistedRecordingQueue, QueueMutationError, RecordingControl,
    RecordingQueue, RecordingTask,
};
use shared::model::QueueRevision;
use std::{collections::VecDeque, sync::atomic::Ordering};

/// Lock ordering for the queue mutation boundary:
///
/// 1. `mutation_guard` (`Mutex`) — outermost for persisted mutations and ordered control publication.
/// 2. `queue` (`Mutex`) — always taken before persisted state locks.
/// 3. `scheduled` / `active` / `finished` (`RwLock`, write) — taken after the queue.
/// 4. `control_signal` (`RwLock`) and `control_notify` (`Notify`) — taken after the
///    queue locks; they signal runtime state, not persisted state.
/// 5. `revision` (`AtomicU64`) — no lock; swapped atomically with the
///    commit step.
///
/// The queue mutation boundary (`mutate`) holds the queue locks only while
/// building the candidate snapshot. It does **not** hold any queue lock
/// while running the user closure or while persisting. Persisting holds
/// the recording repository mutex only.
///
/// Rule repository mutations are acquired strictly after the queue boundary
/// has committed; never inside it.
///
/// Apply a single transactional queue mutation. The closure receives an
/// owned `PersistedRecordingQueue` candidate cloned from the current state
/// and returns either a value or a [`QueueMutationError`]. On success the
/// candidate is persisted atomically, then swapped into the in-memory state,
/// then the `QueueRevision` is incremented. On any failure — closure error
/// or persist error — the in-memory state, the repository, and the
/// revision are all unchanged.
pub async fn mutate<F, R>(this: &RecordingQueue, op: F) -> Result<R, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<R, QueueMutationError>,
{
    match mutate_optional(this, |candidate| op(candidate).map(Some)).await? {
        Some(result) => Ok(result),
        None => Err(QueueMutationError::MutationSkipped),
    }
}

/// [`mutate`], recording an accepted idempotency key in the same commit.
///
/// The key has to land with the recording it describes. Writing it
/// afterwards would leave a window in which the recording exists and the
/// record that suppresses its retry does not.
pub async fn mutate_with_idempotency<F, R>(
    this: &RecordingQueue,
    idempotency: PersistedIdempotency,
    op: F,
) -> Result<R, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<R, QueueMutationError>,
{
    let _mutation = this.mutation_guard.lock().await;
    // Checked again under the guard: the caller's lookup ran without it, so
    // another request with the same key may have committed in between.
    let outcome = this
        .lookup_idempotency(&idempotency.principal, &idempotency.key, &idempotency.request_fingerprint)
        .await
        .map_err(QueueMutationError::from_io)?;
    match outcome {
        IdempotencyOutcome::Fresh => {}
        IdempotencyOutcome::Replay { recording_id } => {
            return Err(QueueMutationError::IdempotentReplay { recording_id });
        }
        IdempotencyOutcome::Conflict => return Err(QueueMutationError::IdempotencyConflict),
    }
    match mutate_optional_locked_with(this, Some(idempotency), |candidate| op(candidate).map(Some)).await? {
        Some(result) => Ok(result),
        None => Err(QueueMutationError::MutationSkipped),
    }
}

/// [`mutate`], then `after` with its result, before the mutation guard is
/// released.
///
/// For filesystem work that must not race the next mutation: once the guard
/// is released a new admission or a worker start can claim the path the
/// committed state just gave up.
pub async fn mutate_then<F, R, A, Fut>(this: &RecordingQueue, op: F, after: A) -> Result<R, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<R, QueueMutationError>,
    A: FnOnce(&R) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _mutation = this.mutation_guard.lock().await;
    let result = mutate_optional_locked(this, |candidate| op(candidate).map(Some))
        .await?
        .ok_or(QueueMutationError::MutationSkipped)?;
    after(&result).await;
    Ok(result)
}

/// `prepare`, then [`mutate`] with its result, under one mutation guard.
///
/// For filesystem work that has to happen before a mutation commits, without
/// blocking a runtime thread inside the mutation closure and without another
/// mutation slipping in between. `prepare` reads the committed in-memory
/// state, which cannot change while the guard is held.
pub async fn mutate_prepared<P, Fut, T, F, R>(this: &RecordingQueue, prepare: P, op: F) -> Result<R, QueueMutationError>
where
    P: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, QueueMutationError>>,
    F: FnOnce(&mut PersistedRecordingQueue, T) -> Result<R, QueueMutationError>,
{
    let _mutation = this.mutation_guard.lock().await;
    let prepared = prepare().await?;
    mutate_optional_locked(this, |candidate| op(candidate, prepared).map(Some))
        .await?
        .ok_or(QueueMutationError::MutationSkipped)
}

pub async fn mutate_optional<F, R>(this: &RecordingQueue, op: F) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
{
    let _mutation = this.mutation_guard.lock().await;
    mutate_optional_locked(this, op).await
}

pub(super) async fn mutate_optional_locked<F, R>(this: &RecordingQueue, op: F) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
{
    mutate_optional_locked_with(this, None, op).await
}

async fn mutate_optional_locked_with<F, R>(
    this: &RecordingQueue,
    idempotency: Option<PersistedIdempotency>,
    op: F,
) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
{
    let next_revision = this.revision.load(Ordering::SeqCst).saturating_add(1);

    // 2. Build candidate under the queue locks.
    let mut candidate = this.snapshot_current(QueueRevision(next_revision)).await;

    // 3. Apply the mutation to a candidate snapshot. The closure can do
    //    arbitrary validation and refer back to the candidate's prior
    //    state.
    let Some(result) = op(&mut candidate)? else {
        return Ok(None);
    };

    let records = candidate.to_records();

    let PersistedRecordingQueue {
        queue: candidate_queue,
        scheduled: candidate_scheduled,
        active: candidate_active,
        finished: candidate_finished,
        revision: _,
    } = candidate;

    // 4. Validate every persisted entry into its in-memory form before
    // swapping. A single corrupt entry must abort the commit so the
    // persisted file and the in-memory state stay identical. Without this,
    // a bad URL would be silently dropped from memory while the file still
    // listed it, desyncing the two.
    let mut queue: VecDeque<RecordingTask> = VecDeque::with_capacity(candidate_queue.len());
    for p in candidate_queue {
        queue.push_back(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted queue entry invalid: {e}")))?,
        );
    }
    let mut scheduled: Vec<RecordingTask> = Vec::with_capacity(candidate_scheduled.len());
    for p in candidate_scheduled {
        scheduled.push(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted scheduled entry invalid: {e}")))?,
        );
    }
    let active = candidate_active
        .into_iter()
        .map(|task| {
            RecordingQueue::from_persisted(task)
                .map_err(|e| QueueMutationError::new(format!("persisted active entry invalid: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut finished: Vec<RecordingTask> = Vec::with_capacity(candidate_finished.len());
    for p in candidate_finished {
        finished.push(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted finished entry invalid: {e}")))?,
        );
    }

    // 5. Persist only after the complete candidate has been validated.
    this.persist_records(next_revision, records, idempotency).await?;

    // 6. Commit. Swap the validated in-memory state from the persisted
    // candidate.
    let mut queue_lock = this.queue.lock().await;
    let mut scheduled_lock = this.scheduled.write().await;
    let mut active_lock = this.active.write().await;
    let mut finished_lock = this.finished.write().await;
    *queue_lock = queue;
    *scheduled_lock = scheduled;
    *active_lock = active;
    *finished_lock = finished;
    this.prune_idle_workers(&queue_lock, &scheduled_lock, &active_lock);
    this.revision.store(next_revision, Ordering::SeqCst);
    this.queue_changed.notify_one();
    Ok(Some(result))
}

impl RecordingQueue {
    pub async fn mutate_optional_and_clear_control<F, R>(
        &self,
        uuid: &str,
        expected: RecordingControl,
        op: F,
    ) -> Result<Option<R>, QueueMutationError>
    where
        F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
    {
        let _mutation = self.mutation_guard.lock().await;
        let worker = self.existing_worker(uuid);
        let result = mutate_optional_locked(self, op).await?;
        if result.is_some() {
            let Some(worker) = worker else { return Ok(result) };
            let mut control = worker.control_signal.write().await;
            if *control == expected {
                *control = RecordingControl::None;
            }
        }
        Ok(result)
    }
}
