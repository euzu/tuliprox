use super::{
    poisoned_repository, waiters::lock_unpoisoned, IdempotencyOutcome, RecordingControl, RecordingQueue, RecordingTask,
    RecordingTaskState,
};
use chrono::Utc;
use shared::model::RecordingKind;
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};
use tokio::sync::{Notify, RwLock};

/// Runtime state belonging to one recording. Control signals never cross task boundaries.
///
/// Queue mutations and worker release take `mutation_guard` first. The worker-map
/// mutex is held only for lookup/removal, never across an await. The `running`
/// lock is released before acquiring queue/active locks or entering a mutation.
/// Control publication follows persisted mutations under `mutation_guard`.
/// Idle-map pruning uses `try_read` under partition locks and retains contested
/// entries for a later worker release.
#[derive(Default)]
pub struct RecordingWorkerState {
    pub control_signal: Arc<RwLock<RecordingControl>>,
    pub control_notify: Arc<Notify>,
    pub running: Arc<RwLock<bool>>,
}

impl RecordingQueue {
    pub fn worker(&self, uuid: &str) -> Arc<RecordingWorkerState> {
        let mut workers = lock_unpoisoned(&self.workers);
        if let Some(worker) = workers.get(uuid) {
            return Arc::clone(worker);
        }
        workers.entry(uuid.to_owned()).or_default().clone()
    }

    pub fn existing_worker(&self, uuid: &str) -> Option<Arc<RecordingWorkerState>> {
        lock_unpoisoned(&self.workers).get(uuid).cloned()
    }

    pub(super) fn prune_idle_workers(
        &self,
        queued: &VecDeque<RecordingTask>,
        scheduled: &[RecordingTask],
        active: &[RecordingTask],
    ) {
        lock_unpoisoned(&self.workers).retain(|uuid, worker| {
            queued.iter().chain(scheduled).chain(active).any(|task| task.uuid == *uuid)
                || !worker.running.try_read().is_ok_and(|running| !*running)
        });
    }

    /// Check promotion eligibility without cloning persisted partitions.
    pub(crate) async fn has_promotable_queued(&self) -> bool {
        use crate::recording::recording_service::recording_identity;
        let queued = self.queue.lock().await;
        if queued.is_empty() {
            return false;
        }
        let active = self.active.read().await;
        let transfer_running = active.iter().any(|task| task.kind != RecordingKind::Live);
        let active_media: HashSet<_> =
            active.iter().map(|task| recording_identity(&task.recording, task.url.as_str())).collect();
        let mut waiting_media = HashSet::new();
        for task in queued.iter() {
            let identity = recording_identity(&task.recording, task.url.as_str());
            if active_media.contains(&identity) {
                continue;
            }
            if task.kind == RecordingKind::Live || !transfer_running {
                return true;
            }
            waiting_media.insert(identity);
        }
        if waiting_media.is_empty() {
            return false;
        }
        self.finished.read().await.iter().any(|task| {
            task.state == RecordingTaskState::Completed
                && waiting_media.contains(&recording_identity(&task.recording, task.url.as_str()))
        })
    }

    pub(crate) async fn claim_worker(&self, uuid: &str) -> Option<Arc<RecordingWorkerState>> {
        let _mutation = self.mutation_guard.lock().await;
        if !self.active.read().await.iter().any(|task| task.uuid == uuid && !task.paused && !task.finished) {
            return None;
        }
        let worker = self.worker(uuid);
        let mut running = worker.running.write().await;
        if *running {
            return None;
        }
        *running = true;
        drop(running);
        Some(worker)
    }

    pub async fn release_worker(&self, uuid: &str) {
        let _mutation = self.mutation_guard.lock().await;
        if let Some(worker) = self.existing_worker(uuid) {
            *worker.running.write().await = false;
            let queued = self.queue.lock().await.iter().any(|task| task.uuid == uuid);
            let active = self.active.read().await.iter().any(|task| task.uuid == uuid);
            if !queued && !active {
                lock_unpoisoned(&self.workers).remove(uuid);
            }
        }
        // A resume/requeue may have raced the previous worker's final iteration.
        self.queue_changed.notify_one();
    }

    pub async fn workers_running(&self) -> bool {
        let workers: Vec<_> = lock_unpoisoned(&self.workers).values().cloned().collect();
        for worker in workers {
            if *worker.running.read().await {
                return true;
            }
        }
        false
    }

    /// Load the canonical recording state from the repository.
    ///
    /// An empty repository is a fresh install. A record that cannot be
    /// converted back to its in-memory form is an error the caller must
    /// propagate: the repository is never reset or rebuilt from memory.
    /// The recovery contract for an operator tool.
    ///
    /// Read-only: it reports, and never opens the B+Tree for mutation.
    /// `None` when the queue is not repository backed.
    pub async fn recovery_health(&self) -> Option<tuliprox_repository::recording_repository::RecoveryHealth> {
        let repository = self.repository.clone()?;
        tokio::task::spawn_blocking(move || repository.lock().ok().map(|guard| guard.health())).await.ok().flatten()
    }

    /// Whether a request carrying this idempotency key has already been
    /// accepted. Without a repository there is nothing to remember, so every
    /// request is fresh.
    pub async fn lookup_idempotency(
        &self,
        principal: &str,
        key: &str,
        request_fingerprint: &str,
    ) -> std::io::Result<IdempotencyOutcome> {
        let Some(repository) = self.repository.clone() else {
            return Ok(IdempotencyOutcome::Fresh);
        };
        let (principal, key, fingerprint) = (principal.to_owned(), key.to_owned(), request_fingerprint.to_owned());
        let now = Utc::now().timestamp();
        tokio::task::spawn_blocking(move || {
            let mut guard = repository.lock().map_err(|_| poisoned_repository())?;
            guard.lookup_idempotency(&principal, &key, &fingerprint, now)
        })
        .await
        .map_err(std::io::Error::other)?
    }
}
