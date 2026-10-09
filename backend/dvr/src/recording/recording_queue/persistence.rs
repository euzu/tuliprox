use super::{
    poisoned_repository, waiters::lock_unpoisoned, PersistedError, PersistedIdempotency, PersistedRecordingQueue,
    PersistedRecordingTask, QueueMutationError, RecordingPartition, RecordingQueue, RecordingSlotWaitQueue,
    RecordingTask, RecordingTaskState, RECORDING_INTERRUPTED_ERR,
};
use chrono::Utc;
use log::error;
use shared::model::{QueueRevision, RecordingKind};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
};
use tokio::sync::{Mutex, Notify, RwLock};
use tuliprox_repository::recording_repository::RecordingRepository;

impl PersistedRecordingQueue {
    /// Flatten the four partitions into the repository's record set, tagging
    /// each task with the partition it must be restored into.
    pub(super) fn to_records(&self) -> Vec<PersistedRecordingTask> {
        let mut records =
            Vec::with_capacity(self.queue.len() + self.scheduled.len() + self.finished.len() + self.active.len());
        let partitions: [(&[PersistedRecordingTask], RecordingPartition); 4] = [
            (&self.queue, RecordingPartition::Queued),
            (&self.scheduled, RecordingPartition::Scheduled),
            (&self.active, RecordingPartition::Active),
            (&self.finished, RecordingPartition::Finished),
        ];
        for (tasks, partition) in partitions {
            for task in tasks {
                let mut task = task.clone();
                task.partition = partition;
                records.push(task);
            }
        }
        records
    }

    /// Rebuild the four partitions from a repository snapshot.
    pub(super) fn from_records(records: Vec<PersistedRecordingTask>, revision: QueueRevision) -> Self {
        let mut restored = Self { revision, ..Self::default() };
        for task in records {
            match task.partition {
                RecordingPartition::Scheduled => restored.scheduled.push(task),
                RecordingPartition::Finished => restored.finished.push(task),
                RecordingPartition::Active => restored.active.push(task),
                RecordingPartition::Queued => restored.queue.push(task),
            }
        }
        restored
    }
}

impl RecordingQueue {
    pub fn new() -> Self { Self::new_with_repository(None) }

    /// Opens the recoverable store and builds a queue backed by it.
    ///
    /// `recovery_root` may point at a different filesystem than `storage_dir`;
    /// that is what lets the history outlive the loss of the database volume.
    pub fn new_persistent(storage_dir: &Path, recovery_root: &Path) -> std::io::Result<Self> {
        let (repository, _) = RecordingRepository::open(storage_dir, recovery_root)?;
        Ok(Self::new_with_repository(Some(Arc::new(StdMutex::new(repository)))))
    }

    pub fn new_with_repository(repository: Option<Arc<StdMutex<RecordingRepository>>>) -> Self {
        Self {
            queue: Arc::from(Mutex::new(VecDeque::new())),
            scheduled: Arc::from(RwLock::new(Vec::new())),
            active: Arc::from(RwLock::new(Vec::new())),
            finished: Arc::from(RwLock::new(Vec::new())),
            workers: StdMutex::new(HashMap::new()),
            capacity_guard: Mutex::new(()),
            queue_changed: Notify::new(),
            repository,
            slot_waiters: Arc::new(RecordingSlotWaitQueue::new()),
            revision: Arc::new(AtomicU64::new(0)),
            mutation_guard: Arc::new(Mutex::new(())),
        }
    }

    pub fn to_persisted(task: &RecordingTask) -> PersistedRecordingTask {
        PersistedRecordingTask {
            media_identity: crate::recording::recording_service::recording_identity_key(
                &task.recording,
                task.url.as_str(),
            ),
            // Overwritten from the owning partition when the candidate is flattened.
            partition: RecordingPartition::default(),
            uuid: task.uuid.clone(),
            kind: task.kind,
            file_dir: task.file_dir.clone(),
            file_path: task.file_path.clone(),
            filename: task.filename.clone(),
            url: task.url.to_string(),
            finished: task.finished,
            size: task.size,
            total_size: task.total_size,
            paused: task.paused,
            error: task.error.clone(),
            state: task.state,
            input_name: task.input_name.as_ref().map(std::string::ToString::to_string),
            priority: task.priority,
            retry_attempts: task.retry_attempts,
            next_retry_at: task.next_retry_at,
            recording: task.recording.clone(),
        }
    }

    pub fn from_persisted(task: PersistedRecordingTask) -> Result<RecordingTask, PersistedError> {
        let url = reqwest::Url::parse(&task.url).map_err(|e| PersistedError::InvalidUrl(e.to_string()))?;
        Ok(RecordingTask {
            uuid: task.uuid,
            kind: task.kind,
            file_dir: task.file_dir,
            file_path: task.file_path,
            filename: task.filename,
            url,
            finished: task.finished,
            size: task.size,
            total_size: task.total_size,
            paused: task.paused,
            error: task.error,
            state: task.state,
            input_name: task.input_name.map(|s| Arc::from(s.as_str())),
            priority: task.priority,
            retry_attempts: task.retry_attempts,
            next_retry_at: task.next_retry_at,
            recording: task.recording,
        })
    }

    pub async fn persist_to_disk(&self) -> std::io::Result<()> {
        // `snapshot_current` takes the partition locks in mutation order; the
        // guard is what keeps that order from meeting a reader that takes them
        // differently.
        let _mutation = self.mutation_guard.lock().await;
        let revision = self.revision.load(Ordering::SeqCst);
        let records = self.snapshot_current(QueueRevision(revision)).await.to_records();
        let result = self.commit_records(revision, records, None).await;
        // Callers discard the result; log here so persistence failures are never silent
        if let Err(err) = &result {
            error!("Failed to persist recording queue: {err}");
        }
        result
    }

    /// Commit a record set to the repository, off the async executor.
    ///
    /// The repository fsyncs both its journal and its B+Tree, so it must not
    /// run on a runtime worker thread.
    pub(super) async fn commit_records(
        &self,
        revision: u64,
        records: Vec<PersistedRecordingTask>,
        idempotency: Option<PersistedIdempotency>,
    ) -> std::io::Result<()> {
        let Some(repository) = self.repository.clone() else {
            return Ok(());
        };
        let now = chrono::Utc::now().timestamp();
        tokio::task::spawn_blocking(move || {
            let mut guard = repository.lock().map_err(|_| poisoned_repository())?;
            guard.commit_with_idempotency(revision, &records, idempotency, now)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub(super) async fn persist_records(
        &self,
        revision: u64,
        records: Vec<PersistedRecordingTask>,
        idempotency: Option<PersistedIdempotency>,
    ) -> Result<(), QueueMutationError> {
        self.commit_records(revision, records, idempotency).await.map_err(QueueMutationError::from_io)
    }

    pub async fn load_from_disk(&self) -> std::io::Result<()> {
        let Some(repository) = self.repository.clone() else {
            return Ok(());
        };
        let snapshot = tokio::task::spawn_blocking(move || {
            let mut guard = repository.lock().map_err(|_| poisoned_repository())?;
            guard.load()
        })
        .await
        .map_err(std::io::Error::other)??;

        let persisted = PersistedRecordingQueue::from_records(snapshot.tasks, QueueRevision(snapshot.queue_revision));

        let invalid = |err: PersistedError| std::io::Error::new(std::io::ErrorKind::InvalidData, err);
        let mut queue = VecDeque::with_capacity(persisted.queue.len());
        for task in persisted.queue {
            queue.push_back(Self::recover_loaded_task(Self::from_persisted(task).map_err(invalid)?));
        }
        let now_ts = Utc::now().timestamp();
        let mut scheduled = Vec::with_capacity(persisted.scheduled.len());
        let mut missed_scheduled = Vec::new();
        for task in persisted.scheduled {
            let task = Self::recover_loaded_task(Self::from_persisted(task).map_err(invalid)?);
            if Self::recording_start_missed_window(&task, now_ts) {
                missed_scheduled.push(task);
            } else {
                scheduled.push(task);
            }
        }
        let active = persisted
            .active
            .into_iter()
            .map(|task| Self::from_persisted(task).map(Self::recover_loaded_task).map_err(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        let mut finished = Vec::with_capacity(persisted.finished.len());
        for task in persisted.finished {
            finished.push(Self::from_persisted(task).map_err(invalid)?);
        }
        finished.extend(missed_scheduled.into_iter().map(Self::finalize_missed_recording));

        *self.queue.lock().await = queue;
        *self.scheduled.write().await = scheduled;
        *self.finished.write().await = finished;
        self.revision.store(persisted.revision.0, Ordering::SeqCst);
        let mut paused = Vec::new();
        for task in active {
            if task.paused || task.state == RecordingTaskState::Paused {
                paused.push(task);
            } else if !task.finished && task.state != RecordingTaskState::Cancelled {
                self.queue.lock().await.push_front(task);
            } else {
                self.finished.write().await.push(task);
            }
        }
        *self.active.write().await = paused;
        lock_unpoisoned(&self.workers).clear();
        Ok(())
    }

    pub(super) fn recover_loaded_task(mut task: RecordingTask) -> RecordingTask {
        if task.paused || task.state == RecordingTaskState::Paused {
            task.paused = true;
            task.finished = false;
            task.state = RecordingTaskState::Paused;
            return task;
        }
        // The worker that was going to acknowledge the cancellation died with
        // the process. Nothing will ever finish it, and it is not `finished`,
        // so the requeue below would restart work the user cancelled.
        if task.state == RecordingTaskState::Cancelling {
            task.paused = false;
            task.finished = true;
            task.state = RecordingTaskState::Cancelled;
            task.error.get_or_insert_with(|| "Cancelled by user".to_string());
            task.next_retry_at = None;
            task.recording.reserved_bytes = 0;
            return task;
        }
        if task.state == RecordingTaskState::Scheduled {
            task.paused = false;
            task.finished = false;
            return task;
        }
        // A live capture cannot be picked up again: whatever was broadcast while
        // the process was down is gone. Requeueing would restart it and write a
        // recording silently missing everything up to now.
        if task.kind == RecordingKind::Live
            && matches!(task.state, RecordingTaskState::Running | RecordingTaskState::WaitingForCapacity)
        {
            task.paused = false;
            task.finished = true;
            task.state = RecordingTaskState::Failed;
            task.error = Some(RECORDING_INTERRUPTED_ERR.to_string());
            task.next_retry_at = None;
            task.recording.reserved_bytes = 0;
            return task;
        }
        if !task.finished {
            task.paused = false;
            task.state = RecordingTaskState::Queued;
            task.error = None;
            task.retry_attempts = 0;
            task.next_retry_at = None;
        }
        task
    }
}
