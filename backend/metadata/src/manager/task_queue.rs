use super::{
    load_metadata_retry_states_from_disk, spawn_blocking_limited, InputWorker, InputWorkerContext,
    MetadataUpdateManager, MetadataUpdateRuntimeSettings, RetryDomain, TaskKey, METADATA_RETRY_STATE_FILE,
    RETRY_STATE_MIN_TTL_SECS,
};
use crate::ctx::BoundMetadataUpdateCtx;
use arc_swap::ArcSwap;
use dashmap::{mapref::entry::Entry, DashMap};
use log::{debug, error, warn};
use parking_lot::Mutex as ParkingMutex;
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{BatchResultCollector, ProviderIdType, ResolveReason, ResolveReasonSet, UpdateTask},
    utils::debug_if_enabled,
};
use tuliprox_repository::get_input_storage_path;

impl TaskKey {
    pub fn from_task(task: &UpdateTask) -> Self {
        match task {
            UpdateTask::ResolveVod { id, .. } => match id {
                ProviderIdType::Id(val) => TaskKey::Vod(*val),
                ProviderIdType::Text(val) => TaskKey::VodStr(val.clone()),
            },
            UpdateTask::ResolveSeries { id, .. } => match id {
                ProviderIdType::Id(val) => TaskKey::Series(*val),
                ProviderIdType::Text(val) => TaskKey::SeriesStr(val.clone()),
            },
            UpdateTask::ProbeLive { id, .. } => match id {
                ProviderIdType::Id(val) => TaskKey::Live(*val),
                ProviderIdType::Text(val) => TaskKey::LiveStr(val.clone()),
            },
            UpdateTask::ProbeStream { probe_scope, unique_id, url, .. } => {
                if unique_id.trim().is_empty() {
                    TaskKey::Stream { scope: probe_scope.clone(), id: Arc::from(url.as_str()) }
                } else {
                    TaskKey::Stream { scope: probe_scope.clone(), id: Arc::from(unique_id.as_str()) }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ScopedTaskKey {
    pub(super) input_name: Arc<str>,
    pub(super) task_key: TaskKey,
}

impl ScopedTaskKey {
    pub(super) fn new(input_name: Arc<str>, task_key: TaskKey) -> Self { Self { input_name, task_key } }
}

pub(super) struct PendingTask {
    pub(super) task: ParkingMutex<UpdateTask>,
    pub(super) generation: AtomicU64,
}

impl PendingTask {
    pub(super) fn new(task: UpdateTask) -> Self {
        Self { task: ParkingMutex::new(task), generation: AtomicU64::new(0) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmitTaskResult {
    QueuedOrMerged,
    QueueFull,
    ChannelClosed,
}

impl MetadataUpdateManager {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            workers: DashMap::new(),
            worker_lifecycle_lock: ParkingMutex::new(()),
            is_shutdown_flag: AtomicBool::new(false),
            ctx: tokio::sync::Mutex::new(None),
            update_pause_gate: Arc::new(RwLock::new(())),
            cancel_token: ArcSwap::from_pointee(cancel_token),
            next_worker_id: AtomicU64::new(1),
            resolve_enqueue_suppressions: Arc::new(DashMap::new()),
            tmdb_source_markers: Arc::new(DashMap::new()),
            enqueue_state_loaded_inputs: Arc::new(DashMap::new()),
            enqueue_state_load_retry_at_ts: Arc::new(DashMap::new()),
            last_resolve_enqueue_suppression_prune_at_ts: AtomicI64::new(0),
        }
    }

    /// Bind the handles the worker reads. Called once, after the server's root
    /// state is built - the worker itself is constructed before it.
    pub async fn set_ctx(&self, ctx: BoundMetadataUpdateCtx) {
        let mut guard = self.ctx.lock().await;
        *guard = Some(ctx);
    }

    fn scoped_task_key(input_name: &str, task: &UpdateTask) -> ScopedTaskKey {
        ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(task))
    }

    fn has_pending_task(&self, input_name: &str, task: &UpdateTask) -> bool {
        self.workers
            .get(input_name)
            .is_some_and(|ctx| !ctx.sender.is_closed() && ctx.pending_tasks.contains_key(&TaskKey::from_task(task)))
    }

    pub(super) fn should_skip_enqueue_cached(&self, input_name: &str, task: &UpdateTask) -> bool {
        let now_ts = chrono::Utc::now().timestamp();
        self.prune_resolve_enqueue_suppressions(now_ts);

        if self.is_redundant_with_pending_task(input_name, task) {
            return true;
        }

        if InputWorker::retry_domain_for_task(task) != RetryDomain::Resolve {
            return false;
        }

        let scoped_key = Self::scoped_task_key(input_name, task);
        if let Some(entry) = self.resolve_enqueue_suppressions.get(&scoped_key) {
            let suppressed_until_ts = *entry;
            drop(entry);
            if now_ts < suppressed_until_ts {
                if self.has_pending_task(input_name, task) {
                    return false;
                }
                debug!(
                    "[Task] Skipping enqueue (resolve suppression) for input {}: {} (until_ts={}, remaining={}s)",
                    input_name,
                    task,
                    suppressed_until_ts,
                    suppressed_until_ts.saturating_sub(now_ts)
                );
                return true;
            }
            self.resolve_enqueue_suppressions.remove(&scoped_key);
        }

        false
    }

    async fn ensure_enqueue_state_loaded_for_input(&self, input_name: &Arc<str>) {
        if self.enqueue_state_loaded_inputs.contains_key(input_name) {
            return;
        }
        let now_ts = chrono::Utc::now().timestamp();
        if self.enqueue_state_load_retry_at_ts.get(input_name).is_some_and(|retry_at_ts| now_ts < *retry_at_ts) {
            return;
        }

        let bound_ctx = {
            let guard = self.ctx.lock().await;
            guard.clone()
        };
        let runtime_settings = MetadataUpdateRuntimeSettings::from_ctx(bound_ctx.as_ref());
        let Some(ctx) = bound_ctx else {
            return;
        };
        let retry_at_ts = now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs);

        let storage_dir = ctx.app_config.config.load().storage_dir.clone();
        let Ok(storage_path) = get_input_storage_path(input_name, &storage_dir).await else {
            self.enqueue_state_load_retry_at_ts.insert(input_name.clone(), retry_at_ts);
            return;
        };
        let retry_path = storage_path.join(METADATA_RETRY_STATE_FILE);
        let input_name_cloned = input_name.clone();
        let loaded = spawn_blocking_limited(move || load_metadata_retry_states_from_disk(&retry_path))
            .await
            .ok()
            .and_then(Result::ok);
        let Some(states) = loaded else {
            self.enqueue_state_load_retry_at_ts.insert(input_name.clone(), retry_at_ts);
            return;
        };

        for (task_key, state) in states {
            if let Some(resolve_state) = state.resolve.as_ref() {
                let suppressed_until_ts = resolve_state.cooldown_until_ts.unwrap_or(resolve_state.next_allowed_at_ts);
                if suppressed_until_ts > now_ts {
                    let scoped_key = ScopedTaskKey::new(input_name_cloned.clone(), task_key.clone());
                    self.resolve_enqueue_suppressions.insert(scoped_key, suppressed_until_ts);
                }
            }
            if let Some(source_last_modified) =
                state.tmdb.as_ref().and_then(|tmdb_state| tmdb_state.source_last_modified)
            {
                let scoped_key = ScopedTaskKey::new(input_name_cloned.clone(), task_key.clone());
                self.tmdb_source_markers.insert(scoped_key, source_last_modified);
            }
        }

        self.enqueue_state_load_retry_at_ts.remove(input_name);
        self.enqueue_state_loaded_inputs.insert(input_name.clone(), ());
    }

    pub(super) fn strip_tmdb_reasons_for_enqueue(&self, input_name: &str, task: UpdateTask) -> Option<UpdateTask> {
        let scoped_key = Self::scoped_task_key(input_name, &task);
        let current_last_modified = InputWorker::task_source_last_modified(&task);
        let previous_last_modified = self.tmdb_source_markers.get(&scoped_key).map(|entry| *entry);
        if (current_last_modified.is_some() || previous_last_modified.is_some())
            && current_last_modified == previous_last_modified
            && InputWorker::task_has_tmdb_reason(&task)
        {
            InputWorker::strip_tmdb_reasons(&task)
        } else {
            Some(task)
        }
    }

    async fn prepare_task_for_enqueue(&self, input_name: Arc<str>, task: UpdateTask) -> Option<UpdateTask> {
        self.ensure_enqueue_state_loaded_for_input(&input_name).await;
        let prepared_task = self.strip_tmdb_reasons_for_enqueue(input_name.as_ref(), task)?;
        if self.should_skip_enqueue_cached(input_name.as_ref(), &prepared_task) {
            None
        } else {
            Some(prepared_task)
        }
    }

    /// Spawn a background task to queue the update.
    /// This is a fire-and-forget method that returns immediately.
    pub fn queue_task_background(self: &Arc<Self>, input_name: Arc<str>, task: UpdateTask) {
        let this = self.clone();
        tokio::spawn(async move {
            this.queue_task(input_name, task).await;
        });
    }

    /// Queue a task for background processing.
    ///
    /// If a worker exists for the input, the task is sent to it.
    /// If no worker exists, a new one is spawned.
    ///
    /// # Arguments
    /// * `input_name` - The input this task belongs to
    /// * `task` - The task to process
    #[allow(clippy::too_many_lines)]
    pub async fn queue_task(&self, input_name: Arc<str>, task: UpdateTask) {
        debug!("[Task] Queuing task for input {input_name}: {task}");
        let Some(task_to_queue) = self.prepare_task_for_enqueue(input_name.clone(), task).await else {
            return;
        };

        // Read app state once and reuse for worker creation when needed.
        let bound_ctx = {
            let guard = self.ctx.lock().await;
            guard.clone()
        };
        let runtime_settings = MetadataUpdateRuntimeSettings::from_ctx(bound_ctx.as_ref());
        let max_queue_size = runtime_settings.max_queue_size;

        let mut channel_closed_attempt: u32 = 0;
        loop {
            // Atomically ensure there is exactly one worker context per input.
            let mut worker_to_spawn: Option<(u64, InputWorker)> = None;
            let (cancel_token, ctx) = {
                let _guard = self.worker_lifecycle_lock.lock();
                let cancel_token = self.cancel_token.load_full();
                if cancel_token.is_cancelled() {
                    debug_if_enabled!(
                        "Aborting metadata enqueue loop for input {input_name} because cancellation was requested: {task_to_queue}"
                    );
                    return;
                }

                let ctx = match self.workers.entry(input_name.clone()) {
                    Entry::Occupied(entry) => entry.get().clone(),
                    Entry::Vacant(entry) => {
                        let (tx, rx) = mpsc::channel::<TaskKey>(max_queue_size);
                        let pending_tasks = Arc::new(DashMap::new());
                        let pending_task_count = Arc::new(AtomicUsize::new(0));
                        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);

                        let ctx = InputWorkerContext {
                            worker_id,
                            sender: tx.clone(),
                            pending_tasks: pending_tasks.clone(),
                            pending_task_count: pending_task_count.clone(),
                        };
                        entry.insert(ctx.clone());

                        worker_to_spawn = Some((
                            worker_id,
                            InputWorker {
                                input_name: input_name.clone(),
                                sender: tx,
                                receiver: rx,
                                pending_tasks,
                                pending_task_count,
                                bound_ctx: bound_ctx.clone(),
                                update_pause_gate: Arc::clone(&self.update_pause_gate),
                                cancel_token: (*cancel_token).clone(),
                                batch_buffer: BatchResultCollector::new(),
                                db_handles: HashMap::new(),
                                failed_clusters: HashSet::new(),
                                retry_states: HashMap::new(),
                                resolve_exhausted: HashMap::new(),
                                last_cycle_completed_at_ts: None,
                                metadata_retry_state_path: None,
                                metadata_retry_loaded: false,
                                metadata_retry_load_retry_at_ts: None,
                                last_retry_state_prune_at_ts: None,
                                scheduled_requeues: Arc::new(DashMap::new()),
                                recently_completed_no_change: HashMap::new(),
                                resolve_enqueue_suppressions: Arc::clone(&self.resolve_enqueue_suppressions),
                                tmdb_source_markers: Arc::clone(&self.tmdb_source_markers),
                                dirty_retry_state_keys: HashSet::new(),
                            },
                        ));

                        ctx
                    }
                };

                (cancel_token, ctx)
            };

            if let Some((worker_id, worker)) = worker_to_spawn {
                let workers_ref = self.workers.clone();
                let input_name_for_cleanup = input_name.clone();
                tokio::spawn(async move {
                    worker.run().await;

                    // Cleanup only if this exact worker context is still active.
                    if let Entry::Occupied(entry) = workers_ref.entry(input_name_for_cleanup.clone()) {
                        if entry.get().worker_id == worker_id {
                            entry.remove();
                        }
                    }
                });
            }

            let sender_still_current = {
                let _guard = self.worker_lifecycle_lock.lock();
                let token_matches = Arc::ptr_eq(&self.cancel_token.load_full(), &cancel_token);
                let worker_matches =
                    self.workers.get(&input_name).is_some_and(|current| current.worker_id == ctx.worker_id);
                token_matches && worker_matches
            };
            if !sender_still_current {
                continue;
            }

            match Self::submit_task(
                ctx.sender.clone(),
                ctx.pending_tasks.clone(),
                ctx.pending_task_count.clone(),
                &input_name,
                max_queue_size,
                task_to_queue.clone(),
            )
            .await
            {
                SubmitTaskResult::QueuedOrMerged => return,
                SubmitTaskResult::QueueFull => {
                    error!(
                        "Metadata queue full for input {input_name} (max_queue_size={max_queue_size}), \
                         dropping task: {task_to_queue}; consider increasing max_queue_size or reducing \
                         probe frequency"
                    );
                    return;
                }
                SubmitTaskResult::ChannelClosed => {
                    channel_closed_attempt = channel_closed_attempt.saturating_add(1);
                    if channel_closed_attempt.is_multiple_of(10) {
                        warn!(
                            "Metadata enqueue channel still closed for input {input_name} after {channel_closed_attempt} retries; continuing recovery"
                        );
                    }
                    debug_if_enabled!(
                        "Detected closed metadata worker channel for input {}, recreating worker context (attempt {})",
                        input_name,
                        channel_closed_attempt
                    );
                    Self::remove_worker_context_if_id(&self.workers, &input_name, ctx.worker_id);
                    let exp = channel_closed_attempt.saturating_sub(1).min(6);
                    let factor = 1_u64.checked_shl(exp).unwrap_or(u64::MAX);
                    let backoff_ms = 25_u64.saturating_mul(factor).min(2_000);
                    let backoff = Duration::from_millis(backoff_ms);
                    tokio::select! {
                        () = cancel_token.cancelled() => {
                            warn!(
                                "Aborting metadata task enqueue for input {input_name} because cancellation was requested: {task_to_queue}"
                            );
                            return;
                        }
                        () = tokio::time::sleep(backoff) => {}
                    }
                }
            }
        }
    }

    pub(super) async fn submit_task(
        sender: mpsc::Sender<TaskKey>,
        pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
        pending_task_count: Arc<AtomicUsize>,
        input_name: &str,
        max_queue_size: usize,
        task: UpdateTask,
    ) -> SubmitTaskResult {
        let key = TaskKey::from_task(&task);
        let task_to_submit = task;

        if let Some(entry) = pending_tasks.get(&key) {
            if sender.is_closed() {
                drop(entry);
                if pending_tasks.remove(&key).is_some() {
                    Self::decrement_pending_task_count(&pending_task_count);
                }
            } else {
                let mut existing = entry.task.lock();
                let before = format!("{existing}");
                if Self::merge_task_payload(&mut existing, task_to_submit) {
                    entry.generation.fetch_add(1, Ordering::Relaxed);
                    debug!("[Task] Merged task for input {input_name}: before={before}, after={existing}");
                } else {
                    debug!("[Task] Task already pending for input {input_name} (no merge needed): {existing}");
                }
                return SubmitTaskResult::QueuedOrMerged;
            }
        }

        // Lock-free admission with CAS: reserve one queue slot only if capacity allows.
        if pending_task_count
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                if current < max_queue_size {
                    Some(current + 1)
                } else {
                    None
                }
            })
            .is_err()
        {
            return SubmitTaskResult::QueueFull;
        }

        loop {
            match pending_tasks.entry(key.clone()) {
                Entry::Occupied(entry) => {
                    // Another producer inserted this key after our fast-path `get`.
                    if sender.is_closed() {
                        entry.remove();
                        Self::decrement_pending_task_count(&pending_task_count);
                        continue;
                    }

                    // Release reserved capacity and merge into the existing task.
                    Self::decrement_pending_task_count(&pending_task_count);
                    let mut existing = entry.get().task.lock();
                    if Self::merge_task_payload(&mut existing, task_to_submit) {
                        entry.get().generation.fetch_add(1, Ordering::Relaxed);
                    }
                    return SubmitTaskResult::QueuedOrMerged;
                }
                Entry::Vacant(entry) => {
                    entry.insert(PendingTask::new(task_to_submit));
                    break;
                }
            }
        }

        if sender.send(key.clone()).await.is_err() {
            if pending_tasks.remove(&key).is_some() {
                Self::decrement_pending_task_count(&pending_task_count);
            }
            warn!("Failed to send task signal for input {input_name}");
            return SubmitTaskResult::ChannelClosed;
        }
        SubmitTaskResult::QueuedOrMerged
    }

    #[inline]
    pub(super) fn decrement_pending_task_count(pending_task_count: &AtomicUsize) {
        // Guard against accidental underflow in edge/error paths.
        let _ = pending_task_count.try_update(Ordering::AcqRel, Ordering::Relaxed, |current| current.checked_sub(1));
    }

    pub(super) fn merge_task_payload(existing: &mut UpdateTask, task: UpdateTask) -> bool {
        let mut changed = false;
        // Merge logic
        match (existing, task) {
            (
                UpdateTask::ResolveVod { reason: r1, delay: d1, source_last_modified: lm1, .. },
                UpdateTask::ResolveVod { reason: r2, delay: d2, source_last_modified: lm2, .. },
            )
            | (
                UpdateTask::ResolveSeries { reason: r1, delay: d1, source_last_modified: lm1, .. },
                UpdateTask::ResolveSeries { reason: r2, delay: d2, source_last_modified: lm2, .. },
            ) => {
                let previous_reason = *r1;
                let previous_delay = *d1;
                let previous_last_modified = *lm1;
                *r1 |= r2;
                *d1 = min(*d1, d2);
                *lm1 = match (*lm1, lm2) {
                    (Some(left), Some(right)) => Some(left.max(right)),
                    _ => None,
                };
                changed = *r1 != previous_reason || *d1 != previous_delay || *lm1 != previous_last_modified;
            }
            (
                UpdateTask::ProbeStream { reason: r1, delay: d1, url: url1, item_type: item_type1, .. },
                UpdateTask::ProbeStream { reason: r2, delay: d2, url: url2, item_type: item_type2, .. },
            ) => {
                let previous_reason = *r1;
                let previous_delay = *d1;
                let previous_url = url1.clone();
                let previous_item_type = *item_type1;
                *r1 |= r2;
                *d1 = min(*d1, d2);
                // Keep the existing payload by default; only fill it from the incoming
                // task when the destination payload is empty.
                if url1.is_empty() && !url2.is_empty() {
                    *url1 = url2;
                    *item_type1 = item_type2;
                }
                changed = *r1 != previous_reason
                    || *d1 != previous_delay
                    || *url1 != previous_url
                    || *item_type1 != previous_item_type;
            }
            (
                UpdateTask::ProbeLive { reason: r1, delay: d1, interval: i1, .. },
                UpdateTask::ProbeLive { reason: r2, delay: d2, interval: i2, .. },
            ) => {
                let previous_reason = *r1;
                let previous_delay = *d1;
                let previous_interval = *i1;
                *r1 |= r2;
                *d1 = min(*d1, d2);
                *i1 = min(*i1, i2);
                changed = *r1 != previous_reason || *d1 != previous_delay || *i1 != previous_interval;
            }
            _ => {} // Mismatched types, should not happen due to TaskKey
        }

        changed
    }

    /// Queue a task using the legacy API (for backward compatibility).
    /// Uses default delay of 50ms.
    pub async fn queue_task_legacy(&self, input_name: Arc<str>, task: UpdateTask) {
        self.queue_task(input_name, task).await;
    }

    /// Returns `true` when an equivalent task is already pending for this input and
    /// submitting `task` would not change the queued payload after merge semantics.
    pub fn is_redundant_with_pending_task(&self, input_name: &str, task: &UpdateTask) -> bool {
        let Some(ctx) = self.workers.get(input_name) else {
            return false;
        };

        // If the worker channel is already closed, let normal enqueue recovery run.
        if ctx.sender.is_closed() {
            return false;
        }

        let key = TaskKey::from_task(task);
        let Some(entry) = ctx.pending_tasks.get(&key) else {
            return false;
        };

        let mut merged = entry.task.lock().clone();
        !Self::merge_task_payload(&mut merged, task.clone())
    }
}

impl InputWorker {
    pub(super) fn runtime_settings(&self) -> MetadataUpdateRuntimeSettings {
        MetadataUpdateRuntimeSettings::from_ctx(self.bound_ctx.as_ref())
    }

    pub(super) fn resolve_exhausted_ttl_secs(runtime_settings: &MetadataUpdateRuntimeSettings) -> i64 {
        runtime_settings.resolve_exhaustion_reset_gap_secs.saturating_mul(6).max(RETRY_STATE_MIN_TTL_SECS)
    }

    pub(super) fn queue_resolve_counts(pending_tasks: &DashMap<TaskKey, PendingTask>) -> (usize, usize) {
        let mut vod_count = 0_usize;
        let mut series_count = 0_usize;

        for entry in pending_tasks {
            match entry.key() {
                TaskKey::Vod(_) | TaskKey::VodStr(_) => vod_count += 1,
                TaskKey::Series(_) | TaskKey::SeriesStr(_) => series_count += 1,
                _ => {}
            }
        }

        (vod_count, series_count)
    }

    #[inline]
    pub(super) fn is_vod_task_key(key: &TaskKey) -> bool { matches!(key, TaskKey::Vod(_) | TaskKey::VodStr(_)) }

    #[inline]
    pub(super) fn is_series_task_key(key: &TaskKey) -> bool {
        matches!(key, TaskKey::Series(_) | TaskKey::SeriesStr(_))
    }

    #[inline]
    pub(super) fn is_probe_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ProbeLive { .. } | UpdateTask::ProbeStream { .. })
    }

    #[inline]
    pub(super) fn is_probe_only_resolve_task(task: &UpdateTask) -> bool {
        match task {
            UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                reason.contains(ResolveReason::Probe)
                    && !reason.contains(ResolveReason::Info)
                    && !reason.contains(ResolveReason::Tmdb)
                    && !reason.contains(ResolveReason::Date)
            }
            _ => false,
        }
    }

    #[inline]
    pub(super) fn is_resolve_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. })
    }

    #[inline]
    pub(super) fn task_reason(task: &UpdateTask) -> ResolveReasonSet {
        match task {
            UpdateTask::ResolveVod { reason, .. }
            | UpdateTask::ResolveSeries { reason, .. }
            | UpdateTask::ProbeLive { reason, .. }
            | UpdateTask::ProbeStream { reason, .. } => *reason,
        }
    }

    #[inline]
    pub(super) fn task_has_tmdb_reason(task: &UpdateTask) -> bool {
        match task {
            UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date)
            }
            _ => false,
        }
    }

    #[inline]
    pub(super) fn task_source_last_modified(task: &UpdateTask) -> Option<u64> {
        match task {
            UpdateTask::ResolveVod { source_last_modified, .. }
            | UpdateTask::ResolveSeries { source_last_modified, .. } => *source_last_modified,
            _ => None,
        }
    }

    pub(super) fn strip_tmdb_reasons(task: &UpdateTask) -> Option<UpdateTask> {
        match task {
            UpdateTask::ResolveVod { id, reason, delay, source_last_modified } => {
                let mut next_reason = *reason;
                next_reason.unset(ResolveReason::Tmdb);
                next_reason.unset(ResolveReason::Date);
                if next_reason.is_empty() {
                    None
                } else {
                    Some(UpdateTask::ResolveVod {
                        id: id.clone(),
                        reason: next_reason,
                        delay: *delay,
                        source_last_modified: *source_last_modified,
                    })
                }
            }
            UpdateTask::ResolveSeries { id, reason, delay, source_last_modified } => {
                let mut next_reason = *reason;
                next_reason.unset(ResolveReason::Tmdb);
                next_reason.unset(ResolveReason::Date);
                if next_reason.is_empty() {
                    None
                } else {
                    Some(UpdateTask::ResolveSeries {
                        id: id.clone(),
                        reason: next_reason,
                        delay: *delay,
                        source_last_modified: *source_last_modified,
                    })
                }
            }
            _ => Some(task.clone()),
        }
    }
}

/// The pipeline's view of this manager.
///
/// `tuliprox-processing` needs three operations here and nothing else; the trait
/// lives there so the pipeline does not have to name this type. The impl lives
/// here because the type does.
impl tuliprox_processing::metadata_sink::MetadataUpdateSink for MetadataUpdateManager {
    async fn acquire_update_pause_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        MetadataUpdateManager::acquire_update_pause_guard(self).await
    }

    async fn prepare_enqueue_state(&self, input_name: Arc<str>) {
        self.ensure_enqueue_state_loaded_for_input(&input_name).await;
    }

    fn should_skip_enqueue(&self, input_name: &str, task: &UpdateTask) -> bool {
        self.should_skip_enqueue_cached(input_name, task)
    }

    fn queue_task_background(self: Arc<Self>, input_name: Arc<str>, task: UpdateTask) {
        MetadataUpdateManager::queue_task_background(&self, input_name, task);
    }
}
