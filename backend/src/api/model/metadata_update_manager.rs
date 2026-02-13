use crate::api::model::ProviderHandle;
use crate::api::model::{AppState, EventMessage};
use crate::processing::processor::{update_generic_stream_metadata, update_live_stream_metadata, update_series_metadata, update_vod_metadata};
use crate::utils::debug_if_enabled;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use log::{debug, error, info, warn};
use shared::create_bitset;
use shared::error::TuliproxError;
use shared::model::{InputType, LiveStreamProperties, PlaylistItemType, SeriesStreamProperties, UUIDType, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem};
use shared::utils::generate_playlist_uuid;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::api::model::BatchResultCollector;
use crate::repository::{get_input_storage_path, get_target_id_mapping_file, write_playlist_batch_item_upsert, xtream_get_file_path, BPlusTreeQuery};
use crate::repository::{persist_input_live_info_batch, persist_input_series_info_batch, persist_input_vod_info_batch, TargetIdMapping};
use crate::utils::FileReadGuard;
use std::cmp::min;
use std::collections::{HashMap, HashSet};

const QUEUE_LOG_INTERVAL: Duration = Duration::from_secs(30);
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(15);
const MAX_RETRY_BACKOFF_SECS: u64 = 60;

const MAX_QUEUE_SIZE: usize = 100_000;
const TASK_ERR_NO_CONNECTION: &str = "No connection available";
const TASK_ERR_PREEMPTED: &str = "Task preempted";

create_bitset!(u8, ResolveReason, Info, Tmdb, Date, Probe, MissingDetails);

/// `PlaylistItemIdType` ID can be either a String (M3U) or u32 (Xtream/TargetDB)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderIdType {
    Text(Arc<str>),
    Id(u32),
}

impl std::fmt::Display for ProviderIdType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderIdType::Text(s) => write!(f, "{s}"),
            ProviderIdType::Id(id) => write!(f, "{id}"),
        }
    }
}

impl From<u32> for ProviderIdType {
    fn from(id: u32) -> Self {
        ProviderIdType::Id(id)
    }
}

impl From<&str> for ProviderIdType {
    fn from(s: &str) -> Self {
        ProviderIdType::Text(Arc::from(s))
    }
}

impl From<String> for ProviderIdType {
    fn from(s: String) -> Self {
        ProviderIdType::Text(Arc::from(s.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpdateTask {
    ResolveVod { id: ProviderIdType, reason: ResolveReasonSet, delay: u16 },
    ResolveSeries { id: ProviderIdType, reason: ResolveReasonSet, delay: u16 },
    ProbeLive { id: ProviderIdType, reason: ResolveReasonSet, delay: u16, interval: u64 },
    // Generic probe for M3U/Library/etc.
    ProbeStream { probe_scope: Arc<str>, unique_id: String, url: String, item_type: PlaylistItemType, reason: ResolveReasonSet, delay: u16 },
}

impl UpdateTask {
    pub fn delay(&self) -> u16 {
        match self {
            UpdateTask::ResolveVod { delay, .. } |
            UpdateTask::ResolveSeries { delay, .. } |
            UpdateTask::ProbeLive { delay, .. } |
            UpdateTask::ProbeStream { delay, .. } => *delay,
        }
    }
}

impl std::fmt::Display for UpdateTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateTask::ResolveVod { id, reason, delay } => write!(f, "Resolve VOD {id} (Reason: {reason}, Delay: {delay}sec)"),
            UpdateTask::ResolveSeries { id, reason, delay } => write!(f, "Resolve Series {id} (Reason: {reason}, Delay: {delay}sec)"),
            UpdateTask::ProbeLive { id, reason, delay, interval } => write!(f, "Probe Live {id} (Reason: {reason}, Delay: {delay}sec, Interval: {interval}secs )"),
            UpdateTask::ProbeStream { probe_scope, unique_id, reason, delay, .. } => write!(f, "Probe Stream {probe_scope}/{unique_id} (Reason: {reason}, Delay: {delay}sec)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskKey {
    Vod(u32),
    VodStr(Arc<str>),
    Series(u32),
    SeriesStr(Arc<str>),
    Live(u32),
    LiveStr(Arc<str>),
    Stream { scope: Arc<str>, id: Arc<str> },
}

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

/// Per-input worker context. Each input has its own worker
/// that processes tasks sequentially with rate limiting.
#[derive(Clone)]
struct InputWorkerContext {
    sender: mpsc::Sender<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
}

struct PendingTask {
    task: Mutex<UpdateTask>,
    generation: AtomicU64,
}

impl PendingTask {
    fn new(task: UpdateTask) -> Self {
        Self {
            task: Mutex::new(task),
            generation: AtomicU64::new(0),
        }
    }
}

/// Manager for background metadata resolution tasks.
///
/// Architecture: Per-Input Worker Pattern
/// - Each input gets its own dedicated worker (tokio task)
/// - Tasks for the SAME input are processed sequentially with rate limiting (defined per task)
/// - Tasks for DIFFERENT inputs run in parallel
/// - Workers are spawned on-demand when first task arrives for an input
/// - Workers terminate when their channel is empty and no more senders exist
pub struct MetadataUpdateManager {
    /// Per-input worker senders. Worker is spawned when entry is created.
    workers: DashMap<Arc<str>, InputWorkerContext>,
    /// Global application state (weak reference to avoid cycles)
    app_state: tokio::sync::Mutex<Option<Weak<AppState>>>,
    /// Global cancellation token for shutdown
    cancel_token: CancellationToken,
}

impl Default for MetadataUpdateManager {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

impl MetadataUpdateManager {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            workers: DashMap::new(),
            app_state: tokio::sync::Mutex::new(None),
            cancel_token,
        }
    }

    pub async fn set_app_state(&self, app_state: Weak<AppState>) {
        let mut guard = self.app_state.lock().await;
        *guard = Some(app_state);
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
    pub async fn queue_task(&self, input_name: Arc<str>, task: UpdateTask) {
        // Read app state once and reuse for worker creation when needed.
        let app_state_weak = {
            let guard = self.app_state.lock().await;
            guard.clone()
        };

        // Atomically ensure there is exactly one worker context per input.
        let mut worker_to_spawn: Option<InputWorker> = None;
        let ctx = match self.workers.entry(input_name.clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let (tx, rx) = mpsc::channel::<TaskKey>(256);
                let pending_tasks = Arc::new(DashMap::new());

                let ctx = InputWorkerContext {
                    sender: tx.clone(),
                    pending_tasks: pending_tasks.clone(),
                };
                entry.insert(ctx.clone());

                worker_to_spawn = Some(InputWorker {
                    input_name: input_name.clone(),
                    sender: tx,
                    receiver: rx,
                    pending_tasks,
                    app_state_weak,
                    cancel_token: self.cancel_token.clone(),
                    batch_buffer: BatchResultCollector::new(),
                    db_handles: HashMap::new(),
                    failed_clusters: HashSet::new(),
                });

                ctx
            }
        };

        if let Some(worker) = worker_to_spawn {
            let workers_ref = self.workers.clone();
            let input_name_for_cleanup = input_name.clone();
            tokio::spawn(async move {
                worker.run().await;
                // Cleanup: remove self from workers map when done
                workers_ref.remove(&input_name_for_cleanup);
            });
        }

        Self::submit_task(ctx.sender.clone(), ctx.pending_tasks.clone(), &input_name, task).await;
    }

    async fn submit_task(
        sender: mpsc::Sender<TaskKey>,
        pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
        input_name: &str,
        task: UpdateTask,
    ) {
        let key = TaskKey::from_task(&task);

        if let Some(entry) = pending_tasks.get(&key) {
            let mut existing = entry.task.lock().await;
            let mut merged = false;
            // Merge logic
            match (&mut *existing, task) {
                (UpdateTask::ResolveVod { reason: r1, delay: d1, .. }, UpdateTask::ResolveVod { reason: r2, delay: d2, .. })
                | (UpdateTask::ResolveSeries { reason: r1, delay: d1, .. }, UpdateTask::ResolveSeries { reason: r2, delay: d2, .. })
                | (UpdateTask::ProbeStream { reason: r1, delay: d1, .. }, UpdateTask::ProbeStream { reason: r2, delay: d2, .. }) => {
                    *r1 |= r2;
                    *d1 = min(*d1, d2);
                    merged = true;
                }
                (UpdateTask::ProbeLive { reason: r1, delay: d1, interval: i1, .. }, UpdateTask::ProbeLive { reason: r2, delay: d2, interval: i2, .. }) => {
                    *r1 |= r2;
                    *d1 = min(*d1, d2);
                    *i1 = min(*i1, i2);
                    merged = true;
                }
                _ => {} // Mismatched types, should not happen due to TaskKey
            }

            if merged {
                entry.generation.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }

        if pending_tasks.len() >= MAX_QUEUE_SIZE {
            warn!("Metadata queue full for input {input_name}, dropping task");
            return;
        }

        pending_tasks.insert(key.clone(), PendingTask::new(task));
        if sender.send(key.clone()).await.is_err() {
            pending_tasks.remove(&key);
            warn!("Failed to send task signal for input {input_name}");
        }
    }

    /// Queue a task using the legacy API (for backward compatibility).
    /// Uses default delay of 50ms.
    pub async fn queue_task_legacy(&self, input_name: Arc<str>, task: UpdateTask) {
        self.queue_task(input_name, task).await;
    }

    /// Get the number of active workers (for monitoring/debugging)
    pub fn active_worker_count(&self) -> usize {
        self.workers.len()
    }
}

struct DbHandle {
    _guard: FileReadGuard,
    query: BPlusTreeQuery<u32, XtreamPlaylistItem>,
}

struct InputWorker {
    input_name: Arc<str>,
    sender: mpsc::Sender<TaskKey>,
    receiver: mpsc::Receiver<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    app_state_weak: Option<Weak<AppState>>,
    cancel_token: CancellationToken,
    batch_buffer: BatchResultCollector,
    db_handles: HashMap<XtreamCluster, DbHandle>,
    failed_clusters: HashSet<XtreamCluster>,
}

impl InputWorker {
    #[allow(clippy::too_many_lines)]
    async fn run(mut self) {
        debug!("Metadata worker started for input {}", &self.input_name);

        let mut processed_vod_count: usize = 0;
        let mut processed_series_count: usize = 0;
        let mut retry_attempts: HashMap<TaskKey, u8> = HashMap::new();
        let mut last_queue_log_at = Instant::now();
        let mut last_progress_log_at = Instant::now();
        let mut queue_cycle_active = false;

        let input_name = self.input_name.clone();
        let app_state_weak = self.app_state_weak.clone();

        // Keep one prefetched task to minimize channel waits/lock churn.
        let mut next_task: Option<(TaskKey, UpdateTask, u64)> = None;

        loop {
            let task_data = if let Some(t) = next_task.take() {
                Some(t)
            } else {
                self.recv_task_fast_or_wait().await
            };

            let Some((current_key, current_task, current_generation)) = task_data else { break };

            if !queue_cycle_active {
                // First entry of a new processing cycle.
                queue_cycle_active = true;
                processed_vod_count = 0;
                processed_series_count = 0;
                last_progress_log_at = Instant::now();
                // Emit queue logs promptly for the new cycle.
                last_queue_log_at = Instant::now()
                    .checked_sub(QUEUE_LOG_INTERVAL + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
                debug!("Background metadata update queue has entries for input {input_name}; starting processing");
            }

            let delay_secs = current_task.delay();

            let mut requeue_current = false;
            let mut retry_delay_secs = 0_u64;

            match Self::process_task_static(
                &input_name,
                app_state_weak.as_ref(),
                &current_task,
                &mut self.batch_buffer,
                &mut self.db_handles,
                &mut self.failed_clusters,
            )
                .await
            {
                Ok(task_changed) => {
                    if Self::is_vod_task_key(&current_key) {
                        processed_vod_count += 1;
                    } else if Self::is_series_task_key(&current_key) {
                        processed_series_count += 1;
                    }
                    debug!("Processed metadata task for input {input_name}: {current_task} (changed={task_changed})");
                    retry_attempts.remove(&current_key);
                    if last_progress_log_at.elapsed() >= PROGRESS_LOG_INTERVAL {
                        // current_key is removed from pending_tasks later in this loop iteration;
                        // subtract it here so "remaining" reflects the post-success queue size.
                        let (mut remaining_vod, mut remaining_series) =
                            Self::queue_resolve_counts(&self.pending_tasks);
                        if Self::is_vod_task_key(&current_key) {
                            remaining_vod = remaining_vod.saturating_sub(1);
                        } else if Self::is_series_task_key(&current_key) {
                            remaining_series = remaining_series.saturating_sub(1);
                        }

                        let total_vod = processed_vod_count.saturating_add(remaining_vod);
                        let total_series = processed_series_count.saturating_add(remaining_series);
                        let resolved_total = processed_vod_count.saturating_add(processed_series_count);
                        let total_resolve = total_vod.saturating_add(total_series);

                        info!("Background metadata update: {resolved_total} / {total_resolve} resolved for input {input_name} (vod: {processed_vod_count}/{total_vod}, series: {processed_series_count}/{total_series})");
                        last_progress_log_at = Instant::now();
                    }
                }
                Err(e) if e.message == TASK_ERR_NO_CONNECTION => {
                    requeue_current = true;

                    let entry = retry_attempts.entry(current_key.clone()).or_insert(0);
                    *entry = entry.saturating_add(1);
                    let attempts = *entry;

                    // If this is the only queued signal, use exponential backoff.
                    // Otherwise, push to the back immediately to improve throughput/fairness.
                    if self.receiver.is_empty() {
                        retry_delay_secs = Self::compute_retry_backoff_secs(current_task.delay(), attempts);
                    }

                    debug_if_enabled!("No provider connection for task {} on input {}, requeueing (attempt={}, retry_delay={}s, queue_len={})",
                        current_task,
                        &input_name,
                        attempts,
                        retry_delay_secs,
                        self.receiver.len()
                    );
                }
                Err(e) => {
                    retry_attempts.remove(&current_key);
                    if e.message != TASK_ERR_PREEMPTED {
                        error!("Task {current_task} failed for input {input_name}: {e}");
                    }
                }
            }

            // Check and flush batch
            if self.batch_buffer.should_flush() {
                self.release_db_handles();
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if requeue_current {
                if retry_delay_secs > 0
                    && Self::sleep_or_cancel(&self.cancel_token, Duration::from_secs(retry_delay_secs)).await
                {
                    break;
                }

                if self.sender.send(current_key.clone()).await.is_err() {
                    // Channel closed, drop the pending task key to avoid leaks.
                    self.pending_tasks.remove(&current_key);
                    retry_attempts.remove(&current_key);
                    warn!("Failed to requeue task {current_task} for input {input_name}");
                }
            } else {
                self
                    .finalize_processed_task_success(&current_key, current_generation, &input_name)
                    .await;

                // Rate limiting
                if delay_secs > 0
                    && Self::sleep_or_cancel(&self.cancel_token, Duration::from_secs(u64::from(delay_secs))).await
                {
                    break;
                }
            }

            // Try to get the next task immediately to keep locks open
            if next_task.is_none() {
                match self.receiver.try_recv() {
                    Ok(key) => {
                        next_task = self.load_task_snapshot(key).await;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                }
            }

            let queue_has_work =
                next_task.is_some() || !self.receiver.is_empty() || !self.pending_tasks.is_empty();

            if queue_has_work {
                // Avoid O(n) queue scans per task; report queue status periodically.
                if last_queue_log_at.elapsed() >= QUEUE_LOG_INTERVAL {
                    let queue_counts = Self::queue_resolve_counts(&self.pending_tasks);
                    debug!("In queue to resolve vod: {}, series: {} (input: {input_name})", queue_counts.0, queue_counts.1);
                    last_queue_log_at = Instant::now();
                }
            } else {
                // Queue is drained: flush remaining batched updates immediately.
                if !self.batch_buffer.is_empty() {
                    self.release_db_handles();
                    Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
                }

                if queue_cycle_active {
                    info!("All pending metadata resolves completed for input {input_name}");
                    if let Some(app_state) = app_state_weak.as_ref().and_then(Weak::upgrade) {
                        app_state
                            .event_manager
                            .send_event(EventMessage::InputMetadataUpdatesCompleted(input_name.clone()));
                    }
                    queue_cycle_active = false;
                    processed_vod_count = 0;
                    processed_series_count = 0;
                }
            }
        }

        // Final flush
        self.release_db_handles();
        Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;

        debug!("Metadata worker stopped for input {input_name}");
    }

    async fn finalize_processed_task_success(
        &mut self,
        current_key: &TaskKey,
        current_generation: u64,
        input_name: &str,
    ) -> bool {
        // Atomically remove processed key and reinsert it if it changed while in-flight.
        if let Some((_k, removed_task)) = self.pending_tasks.remove(current_key) {
            let latest_generation = removed_task.generation.load(Ordering::Relaxed);
            if latest_generation != current_generation {
                self.pending_tasks.insert(current_key.clone(), removed_task);
                if self.sender.send(current_key.clone()).await.is_err() {
                    self.pending_tasks.remove(current_key);
                    warn!("Failed to schedule merged task replay for input {input_name}");
                    return false;
                }
                return true;
            }
        }
        false
    }

    async fn recv_task_fast_or_wait(&mut self) -> Option<(TaskKey, UpdateTask, u64)> {
        match self.receiver.try_recv() {
            Ok(key) => {
                return self.load_task_snapshot(key).await;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return None;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        }

        // When idle, release read handles to avoid writer starvation.
        self.release_db_handles();

        tokio::select! {
            biased;
            () = self.cancel_token.cancelled() => None,
            res = self.receiver.recv() => {
                match res {
                    Some(key) => self.load_task_snapshot(key).await,
                    None => None,
                }
            }
        }
    }

    async fn load_task_snapshot(&self, key: TaskKey) -> Option<(TaskKey, UpdateTask, u64)> {
        let entry = self.pending_tasks.get(&key)?;
        let generation = entry.generation.load(Ordering::Relaxed);
        let task = entry.task.lock().await.clone();
        Some((key, task, generation))
    }

    fn release_db_handles(&mut self) {
        if !self.db_handles.is_empty() {
            self.db_handles.clear();
        }
        if !self.failed_clusters.is_empty() {
            self.failed_clusters.clear();
        }
    }

    fn compute_retry_backoff_secs(base_delay_secs: u16, attempts: u8) -> u64 {
        let base_delay = u64::from(base_delay_secs.max(1));
        let exp = u32::from(attempts.saturating_sub(1).min(6));
        base_delay
            .saturating_mul(2_u64.saturating_pow(exp))
            .min(MAX_RETRY_BACKOFF_SECS)
    }

    async fn sleep_or_cancel(cancel_token: &CancellationToken, duration: Duration) -> bool {
        tokio::select! {
            () = cancel_token.cancelled() => true,
            () = tokio::time::sleep(duration) => false,
        }
    }

    fn queue_resolve_counts(pending_tasks: &DashMap<TaskKey, PendingTask>) -> (usize, usize) {
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
    fn is_vod_task_key(key: &TaskKey) -> bool {
        matches!(key, TaskKey::Vod(_) | TaskKey::VodStr(_))
    }

    #[inline]
    fn is_series_task_key(key: &TaskKey) -> bool {
        matches!(key, TaskKey::Series(_) | TaskKey::SeriesStr(_))
    }

    // Changed to static method
    async fn flush_batch_static(
        input_name: &str,
        app_state_weak: Option<&Weak<AppState>>,
        batch_buffer: &mut BatchResultCollector,
    ) {
        if batch_buffer.is_empty() { return; }

        let Some(app_state) = app_state_weak.and_then(Weak::upgrade) else { return };
        let app_config = &app_state.app_config;
        let working_dir = &app_config.config.load().working_dir;
        let vod_updates = batch_buffer.take_vod_updates();
        let series_updates = batch_buffer.take_series_updates();
        let live_updates = batch_buffer.take_live_updates();

        if vod_updates.is_empty() && series_updates.is_empty() && live_updates.is_empty() {
            return;
        }

        if let Ok(storage_path) = get_input_storage_path(input_name, working_dir).await {
            if !vod_updates.is_empty() {
                let mut updates: Vec<(u32, VideoStreamProperties)> = Vec::with_capacity(vod_updates.len());
                for (id, props) in &vod_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_vod_info_batch(app_config, &storage_path, XtreamCluster::Video, input_name, updates).await {
                        error!("Failed to flush VOD batch for input {input_name}: {e}");
                    }
                }
            }

            if !series_updates.is_empty() {
                let mut updates: Vec<(u32, SeriesStreamProperties)> = Vec::with_capacity(series_updates.len());
                for (id, props) in &series_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_series_info_batch(app_config, &storage_path, XtreamCluster::Series, input_name, updates).await {
                        error!("Failed to flush Series batch for input {input_name}: {e}");
                    }
                }
            }

            if !live_updates.is_empty() {
                let mut updates: Vec<(u32, LiveStreamProperties)> = Vec::with_capacity(live_updates.len());
                for (id, props) in &live_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_live_info_batch(app_config, &storage_path, XtreamCluster::Live, input_name, updates).await {
                        error!("Failed to flush Live batch for input {input_name}: {e}");
                    }
                }
            }
        }

        let cascade_batch = BatchResultCollector {
            vod: vod_updates,
            series: series_updates,
            live: live_updates,
        };

        Self::cascade_updates(
            &app_state,
            &app_config.config.load(),
            input_name,
            &cascade_batch,
        ).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn cascade_updates(
        app_state: &Arc<AppState>,
        config: &crate::model::Config,
        input_name: &str,
        batch: &BatchResultCollector,
    ) {
        if batch.is_empty() {
            return;
        }

        // Find targets affected by this input.
        let targets = {
            let sources = app_state.app_config.sources.load();
            let mut affected_targets = Vec::new();

            for source in &sources.sources {
                if source.inputs.iter().any(|i_name| i_name.as_ref() == input_name) {
                    for t_def in &source.targets {
                        affected_targets.push(t_def.clone());
                    }
                }
            }
            affected_targets
        };

        if targets.is_empty() { return; }

        for target in targets {
            let target_name = &target.name;
            let Some(target_path) = crate::repository::get_target_storage_path(config, target_name) else { continue; };
            let Some(storage_path) = crate::repository::xtream_get_storage_path(config, target_name) else { continue; };
            let mapping_file = get_target_id_mapping_file(&target_path);

            // Read mapping under lock and release lock immediately after opening.
            let mapping = {
                let _lock = app_state.app_config.file_locks.read_lock(&mapping_file).await;
                match TargetIdMapping::new(&mapping_file, false) {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Failed to open ID mapping for target {target_name}: {e}");
                        continue;
                    }
                }
            };

            let mut provider_virtual_ids: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut uuid_virtual_ids: HashMap<UUIDType, Option<u32>> = HashMap::new();

            let vod_virtual_updates = Self::collect_vod_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_vod_cascade_updates(
                app_state,
                &target,
                &storage_path,
                vod_virtual_updates,
            ).await;

            let series_virtual_updates = Self::collect_series_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_series_cascade_updates(
                app_state,
                &target,
                &storage_path,
                series_virtual_updates,
            ).await;

            let live_virtual_updates = Self::collect_live_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_live_cascade_updates(
                app_state,
                &target,
                &storage_path,
                live_virtual_updates,
            ).await;
        }
    }

    fn get_cached_uuid_virtual_id(
        mapping: &TargetIdMapping,
        cache: &mut HashMap<UUIDType, Option<u32>>,
        uuid: UUIDType,
    ) -> Option<u32> {
        if let Some(cached) = cache.get(&uuid) {
            return *cached;
        }
        let resolved = mapping.get_virtual_id_by_uuid(&uuid);
        cache.insert(uuid, resolved);
        resolved
    }

    fn collect_vod_virtual_updates<'a>(
        mapping: &TargetIdMapping,
        input_name: &str,
        batch: &'a BatchResultCollector,
        provider_virtual_ids: &mut HashMap<u32, Vec<u32>>,
        uuid_virtual_ids: &mut HashMap<UUIDType, Option<u32>>,
    ) -> HashMap<u32, &'a VideoStreamProperties> {
        let mut virtual_updates: HashMap<u32, &'a VideoStreamProperties> = HashMap::new();
        if batch.vod.is_empty() {
            return virtual_updates;
        }

        for (pid, props) in &batch.vod {
            match pid {
                ProviderIdType::Id(provider_id) => {
                    let virtual_ids = provider_virtual_ids
                        .entry(*provider_id)
                        .or_insert_with(|| mapping.find_virtual_ids(*provider_id));
                    for virtual_id in virtual_ids {
                        virtual_updates.insert(*virtual_id, props);
                    }
                }
                ProviderIdType::Text(provider_id_text) => {
                    let uuid = generate_playlist_uuid(
                        input_name,
                        provider_id_text,
                        PlaylistItemType::Video,
                        props.direct_source.as_ref(),
                    );
                    if let Some(virtual_id) =
                        Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid)
                    {
                        virtual_updates.insert(virtual_id, props);
                    }
                }
            }
        }

        virtual_updates
    }

    fn collect_series_virtual_updates<'a>(
        mapping: &TargetIdMapping,
        input_name: &str,
        batch: &'a BatchResultCollector,
        provider_virtual_ids: &mut HashMap<u32, Vec<u32>>,
        uuid_virtual_ids: &mut HashMap<UUIDType, Option<u32>>,
    ) -> HashMap<u32, &'a SeriesStreamProperties> {
        let mut virtual_updates: HashMap<u32, &'a SeriesStreamProperties> = HashMap::new();
        if batch.series.is_empty() {
            return virtual_updates;
        }

        for (pid, props) in &batch.series {
            match pid {
                ProviderIdType::Id(provider_id) => {
                    let virtual_ids = provider_virtual_ids
                        .entry(*provider_id)
                        .or_insert_with(|| mapping.find_virtual_ids(*provider_id));
                    for virtual_id in virtual_ids {
                        virtual_updates.insert(*virtual_id, props);
                    }
                }
                ProviderIdType::Text(provider_id_text) => {
                    let uuid = generate_playlist_uuid(
                        input_name,
                        provider_id_text,
                        PlaylistItemType::Series,
                        "",
                    );
                    if let Some(virtual_id) =
                        Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid)
                    {
                        virtual_updates.insert(virtual_id, props);
                    }
                }
            }
        }

        virtual_updates
    }

    fn collect_live_virtual_updates<'a>(
        mapping: &TargetIdMapping,
        input_name: &str,
        batch: &'a BatchResultCollector,
        provider_virtual_ids: &mut HashMap<u32, Vec<u32>>,
        uuid_virtual_ids: &mut HashMap<UUIDType, Option<u32>>,
    ) -> HashMap<u32, &'a LiveStreamProperties> {
        let mut virtual_updates: HashMap<u32, &'a LiveStreamProperties> = HashMap::new();
        if batch.live.is_empty() {
            return virtual_updates;
        }

        for (pid, props) in &batch.live {
            match pid {
                ProviderIdType::Id(provider_id) => {
                    let virtual_ids = provider_virtual_ids
                        .entry(*provider_id)
                        .or_insert_with(|| mapping.find_virtual_ids(*provider_id));
                    for virtual_id in virtual_ids {
                        virtual_updates.insert(*virtual_id, props);
                    }
                }
                ProviderIdType::Text(provider_id_text) => {
                    let uuid = generate_playlist_uuid(
                        input_name,
                        provider_id_text,
                        PlaylistItemType::Live,
                        props.direct_source.as_ref(),
                    );
                    if let Some(virtual_id) =
                        Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid)
                    {
                        virtual_updates.insert(virtual_id, props);
                    }
                }
            }
        }

        virtual_updates
    }

    async fn apply_vod_cascade_updates(
        app_state: &Arc<AppState>,
        target: &crate::model::ConfigTarget,
        storage_path: &std::path::Path,
        virtual_updates: HashMap<u32, &VideoStreamProperties>,
    ) {
        if virtual_updates.is_empty() {
            return;
        }

        let target_name = target.name.as_str();
        let xtream_path = xtream_get_file_path(storage_path, XtreamCluster::Video);
        let updates = {
            let _lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let mut updates = Vec::with_capacity(virtual_updates.len());
            if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                for (virtual_id, props) in &virtual_updates {
                    if let Ok(Some(mut item)) = query.query_zero_copy(virtual_id) {
                        item.additional_properties =
                            Some(shared::model::StreamProperties::Video(Box::new((*props).clone())));
                        updates.push(item);
                    }
                }
            }
            updates
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) = write_playlist_batch_item_upsert(
            &app_state.app_config,
            target_name,
            XtreamCluster::Video,
            &updates,
        ).await {
            error!("Failed to cascade VOD updates to target {target_name}: {e}");
            return;
        }

        if target.use_memory_cache {
            Self::update_memory_cache(app_state, target_name, XtreamCluster::Video, updates).await;
        }
    }

    async fn apply_series_cascade_updates(
        app_state: &Arc<AppState>,
        target: &crate::model::ConfigTarget,
        storage_path: &std::path::Path,
        virtual_updates: HashMap<u32, &SeriesStreamProperties>,
    ) {
        if virtual_updates.is_empty() {
            return;
        }

        let target_name = target.name.as_str();
        let xtream_path = xtream_get_file_path(storage_path, XtreamCluster::Series);
        let updates = {
            let _lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let mut updates = Vec::with_capacity(virtual_updates.len());
            if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                for (virtual_id, props) in &virtual_updates {
                    if let Ok(Some(mut item)) = query.query_zero_copy(virtual_id) {
                        item.additional_properties =
                            Some(shared::model::StreamProperties::Series(Box::new((*props).clone())));
                        updates.push(item);
                    }
                }
            }
            updates
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) = write_playlist_batch_item_upsert(
            &app_state.app_config,
            target_name,
            XtreamCluster::Series,
            &updates,
        ).await {
            error!("Failed to cascade Series updates to target {target_name}: {e}");
            return;
        }

        if target.use_memory_cache {
            Self::update_memory_cache(app_state, target_name, XtreamCluster::Series, updates).await;
        }
    }

    async fn apply_live_cascade_updates(
        app_state: &Arc<AppState>,
        target: &crate::model::ConfigTarget,
        storage_path: &std::path::Path,
        virtual_updates: HashMap<u32, &LiveStreamProperties>,
    ) {
        if virtual_updates.is_empty() {
            return;
        }

        let target_name = target.name.as_str();
        let xtream_path = xtream_get_file_path(storage_path, XtreamCluster::Live);
        let updates = {
            let _lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let mut updates = Vec::with_capacity(virtual_updates.len());
            if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                for (virtual_id, props) in &virtual_updates {
                    if let Ok(Some(mut item)) = query.query_zero_copy(virtual_id) {
                        item.additional_properties =
                            Some(shared::model::StreamProperties::Live(Box::new((*props).clone())));
                        updates.push(item);
                    }
                }
            }
            updates
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) = write_playlist_batch_item_upsert(
            &app_state.app_config,
            target_name,
            XtreamCluster::Live,
            &updates,
        ).await {
            error!("Failed to cascade Live updates to target {target_name}: {e}");
            return;
        }

        if target.use_memory_cache {
            Self::update_memory_cache(app_state, target_name, XtreamCluster::Live, updates).await;
        }
    }

    async fn update_memory_cache(
        app_state: &Arc<AppState>,
        target_name: &str,
        cluster: XtreamCluster,
        updates: Vec<XtreamPlaylistItem>,
    ) {
        let mut playlists = app_state.playlists.data.write().await;
        if let Some(playlist) = playlists.get_mut(target_name) {
            if let Some(xtream_storage) = &mut playlist.xtream {
                let storage = match cluster {
                    XtreamCluster::Live => &mut xtream_storage.live,
                    XtreamCluster::Video => &mut xtream_storage.vod,
                    XtreamCluster::Series => &mut xtream_storage.series,
                };
                for item in updates {
                    storage.insert(item.virtual_id, item);
                }
            }
        }
    }
    async fn get_or_open_query<'a>(
        input_name: &str,
        app_state: &Arc<AppState>,
        cluster: XtreamCluster,
        db_handles: &'a mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Option<&'a mut BPlusTreeQuery<u32, XtreamPlaylistItem>> {
        if failed_clusters.contains(&cluster) {
            return None;
        }

        if let std::collections::hash_map::Entry::Vacant(e) = db_handles.entry(cluster) {
            let working_dir = &app_state.app_config.config.load().working_dir;
            if let Ok(storage_path) = get_input_storage_path(input_name, working_dir).await {
                let file_path = xtream_get_file_path(&storage_path, cluster);
                if file_path.exists() {
                    let lock = app_state.app_config.file_locks.read_lock(&file_path).await;
                    if let Ok(query) = BPlusTreeQuery::try_new(&file_path) {
                        e.insert(DbHandle { _guard: lock, query });
                    } else {
                        failed_clusters.insert(cluster);
                    }
                } else {
                    // File doesn't exist, technically a failure to open but acceptable.
                    // We don't mark as failure to allow creation if it appears, but for read logic
                    // we could cache the non-existence if we wanted.
                    // For now, let's strictly follow the instruction about "failure cache" for *errors* or corruption.
                }
            }
        }

        db_handles.get_mut(&cluster).map(|h| &mut h.query)
    }

    // Helper for get_item_name with caching
    async fn get_item_name_static(
        input_name: &str,
        app_state: &Arc<AppState>,
        task: &UpdateTask,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Option<String> {
        let (id, cluster) = match task {
            UpdateTask::ResolveVod { id, .. } => (id, XtreamCluster::Video),
            UpdateTask::ResolveSeries { id, .. } => (id, XtreamCluster::Series),
            UpdateTask::ProbeLive { id, .. } => (id, XtreamCluster::Live),
            UpdateTask::ProbeStream { .. } => return None,
        };

        if let ProviderIdType::Id(vid) = id {
            if let Some(query) = Self::get_or_open_query(input_name, app_state, cluster, db_handles, failed_clusters).await {
                if let Ok(Some(item)) = query.query_zero_copy(vid) {
                    return Some(if item.title.is_empty() { item.name.to_string() } else { item.title.to_string() });
                }
            }
        }
        None
    }

    async fn process_task_static(
        input_name: &Arc<str>,
        app_state_weak: Option<&Weak<AppState>>,
        task: &UpdateTask,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<bool, TuliproxError> {
        let app_state = app_state_weak
            .and_then(Weak::upgrade)
            .ok_or_else(|| shared::error::info_err!("AppState not available"))?;

        let Some(input_base) = app_state.app_config.get_input_by_name(input_name) else {
            return Err(shared::error::info_err!("Input {} not found", input_name));
        };

        if !input_base.enabled {
            return Err(shared::error::info_err!("Input {} is disabled", input_name));
        }

        let needs_probe_connection =
            Self::task_needs_provider_connection(task, input_base.input_type);

        // Reserve provider capacity only for actual probe work (ffprobe paths).
        let provider_handle = if needs_probe_connection {
            let Some(handle) = app_state.active_provider.acquire_connection_for_probe(input_name).await else {
                debug_if_enabled!("No provider connection available for background task {}, skipping...", task);
                return Err(shared::error::info_err!("{}", TASK_ERR_NO_CONNECTION));
            };
            Some(handle)
        } else {
            None
        };

        let item_title = Self::get_item_name_static(input_name, &app_state, task, db_handles, failed_clusters).await;

        let config_to_use = provider_handle
            .as_ref()
            .and_then(|handle| handle.allocation.get_provider_config());
        let name_display = item_title.as_deref().map_or(String::new(), |n| format!(" \"{n}\""));

        debug!("Processing task for {input_name}: {task}{name_display}");

        let pre_vod_updates = collector.vod.len();
        let pre_series_updates = collector.series.len();
        let pre_live_updates = collector.live.len();

        // Determine input to use (may be alias)
        let input_to_use = config_to_use
            .filter(|alloc| alloc.name != input_base.name)
            .and_then(|alloc| {
                input_base.aliases.as_ref()?
                    .iter()
                    .find(|a| a.enabled && a.name == alloc.name)
            })
            .map(|alias_def| {
                let mut temp_input = (*input_base).clone();
                temp_input.url.clone_from(&alias_def.url);
                temp_input.username.clone_from(&alias_def.username);
                temp_input.password.clone_from(&alias_def.password);
                Arc::new(temp_input)
            })
            .unwrap_or(input_base);

        let client = if app_state.should_use_manual_redirects() {
            app_state.http_client_no_redirect.load()
        } else {
            app_state.http_client.load()
        };

        // Execute task; probe tasks get a reserved provider handle, resolve tasks don't.
        let res = if let Some(handle) = provider_handle.as_ref() {
            if let Some(token) = &handle.cancel_token {
                tokio::select! {
                    biased;

                    () = token.cancelled() => {
                        debug_if_enabled!("Metadata update task preempted by user request for input {}", input_name);
                        Err(shared::error::info_err!("{}", TASK_ERR_PREEMPTED))
                    }

                    res = Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), Some(handle), collector, db_handles, failed_clusters) => {
                        res
                    }
                }
            } else {
                Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), Some(handle), collector, db_handles, failed_clusters).await
            }
        } else {
            Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), None, collector, db_handles, failed_clusters).await
        };

        if provider_handle.is_some() {
            app_state.connection_manager.release_provider_handle(provider_handle).await;
        }
        match res {
            Ok(()) => {
                let task_changed = match task {
                    UpdateTask::ResolveVod { .. } => collector.vod.len() > pre_vod_updates,
                    UpdateTask::ResolveSeries { .. } => collector.series.len() > pre_series_updates,
                    UpdateTask::ProbeLive { .. } => collector.live.len() > pre_live_updates,
                    UpdateTask::ProbeStream { .. } => true,
                };
                Ok(task_changed)
            }
            Err(e) => Err(e),
        }
    }

    fn task_needs_provider_connection(task: &UpdateTask, input_type: InputType) -> bool {
        match task {
            UpdateTask::ProbeLive { .. } => true,
            // Local library probing is fully local and must not depend on provider capacity.
            UpdateTask::ProbeStream { .. } => !matches!(input_type, InputType::Library),
            UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. } => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_task_inner_static(
        app_state: &Arc<AppState>,
        client: &reqwest::Client,
        input: &Arc<crate::model::ConfigInput>,
        task: &UpdateTask,
        item_title: Option<&str>,
        active_handle: Option<&ProviderHandle>,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<(), TuliproxError> {
        match task {
            UpdateTask::ResolveVod { id, reason, .. } => {
                let fetch_info = reason.contains(ResolveReason::Info);

                let query_opt = Self::get_or_open_query(&input.name, app_state, XtreamCluster::Video, db_handles, failed_clusters).await;

                match update_vod_metadata(
                    &app_state.app_config,
                    client,
                    input,
                    id.clone(),
                    active_handle,
                    &app_state.active_provider,
                    item_title,
                    false, // Batch collect
                    fetch_info,
                    query_opt,
                ).await {
                    Ok(Some(props)) => {
                        collector.add_vod(id.clone(), props);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ResolveSeries { id, reason, .. } => {
                let fetch_info = reason.contains(ResolveReason::Info);

                // Get handle for Series
                let query_opt = Self::get_or_open_query(&input.name, app_state, XtreamCluster::Series, db_handles, failed_clusters).await;

                match update_series_metadata(
                    &app_state.app_config,
                    client,
                    input,
                    id.clone(),
                    &app_state.active_provider,
                    active_handle,
                    item_title,
                    false, // Batch collect
                    fetch_info,
                    reason.contains(ResolveReason::Probe),
                    query_opt,
                ).await {
                    Ok(Some(props)) => {
                        collector.add_series(id.clone(), props);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeLive { id, .. } => {
                let query_opt = Self::get_or_open_query(&input.name, app_state, XtreamCluster::Live, db_handles, failed_clusters).await;

                match update_live_stream_metadata(
                    &app_state.app_config,
                    input,
                    id.clone(),
                    false,
                    query_opt,
                ).await {
                    Ok(Some(props)) => {
                        collector.add_live(id.clone(), props);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeStream { unique_id, url, item_type, .. } => {
                // Generic probe doesn't support batching yet
                update_generic_stream_metadata(
                    &app_state.app_config,
                    input.as_ref(),
                    unique_id,
                    url,
                    *item_type,
                    &app_state.active_provider,
                    active_handle,
                ).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn queue_task_creates_single_worker_per_input_under_concurrency() {
        let cancel_token = CancellationToken::new();
        let manager = Arc::new(MetadataUpdateManager::new(cancel_token.clone()));
        let input_name: Arc<str> = Arc::from("race_input");

        let mut joins = Vec::new();
        for id in 0..32u32 {
            let manager_cloned = manager.clone();
            let input_cloned = input_name.clone();
            joins.push(tokio::spawn(async move {
                manager_cloned
                    .queue_task(
                        input_cloned,
                        UpdateTask::ResolveVod {
                            id: ProviderIdType::Id(id),
                            reason: ResolveReasonSet::default(),
                            delay: 0,
                        },
                    )
                    .await;
            }));
        }

        for join in joins {
            join.await.expect("queue task spawn should complete");
        }

        // Allow spawned worker startup to settle.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(manager.active_worker_count(), 1);

        cancel_token.cancel();
    }

    #[tokio::test]
    async fn submit_task_merges_existing_task_and_increments_generation() {
        let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());

        let task_initial = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 10,
        };
        MetadataUpdateManager::submit_task(tx.clone(), pending_tasks.clone(), "input_a", task_initial).await;

        let task_merge = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 2,
        };
        MetadataUpdateManager::submit_task(tx, pending_tasks.clone(), "input_a", task_merge).await;

        let first_signal = rx.try_recv().expect("first signal should be queued");
        assert_eq!(first_signal, TaskKey::Vod(42));
        assert!(matches!(
            rx.try_recv(),
            Err(
                tokio::sync::mpsc::error::TryRecvError::Empty
                    | tokio::sync::mpsc::error::TryRecvError::Disconnected
            )
        ));

        let entry = pending_tasks
            .get(&TaskKey::Vod(42))
            .expect("pending entry should exist");
        assert_eq!(entry.generation.load(Ordering::Relaxed), 1);

        let merged = entry.task.lock().await.clone();
        match merged {
            UpdateTask::ResolveVod { reason, delay, .. } => {
                assert!(reason.contains(ResolveReason::Info));
                assert!(reason.contains(ResolveReason::Probe));
                assert_eq!(delay, 2);
            }
            other => panic!("unexpected task type after merge: {other:?}"),
        }
    }

    #[tokio::test]
    async fn finalize_processed_task_success_requeues_when_generation_changed() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let key = TaskKey::Vod(7);

        pending_tasks.insert(
            key.clone(),
            PendingTask::new(UpdateTask::ResolveVod {
                id: ProviderIdType::Id(7),
                reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
                delay: 0,
            }),
        );
        if let Some(entry) = pending_tasks.get(&key) {
            entry.generation.store(1, Ordering::Relaxed);
        }

        let mut worker = InputWorker {
            input_name: Arc::from("input_a"),
            sender: tx,
            receiver: rx,
            pending_tasks: pending_tasks.clone(),
            app_state_weak: None,
            cancel_token: CancellationToken::new(),
            batch_buffer: BatchResultCollector::new(),
            db_handles: HashMap::new(),
            failed_clusters: HashSet::new(),
        };

        let requeued = worker
            .finalize_processed_task_success(&key, 0, "input_a")
            .await;
        assert!(requeued);
        assert!(pending_tasks.contains_key(&key));
        assert_eq!(
            worker
                .receiver
                .try_recv()
                .expect("requeued signal should be present"),
            key
        );
    }

    #[tokio::test]
    async fn finalize_processed_task_success_removes_when_unchanged() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let key = TaskKey::Vod(9);

        pending_tasks.insert(
            key.clone(),
            PendingTask::new(UpdateTask::ResolveVod {
                id: ProviderIdType::Id(9),
                reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
                delay: 0,
            }),
        );

        let mut worker = InputWorker {
            input_name: Arc::from("input_b"),
            sender: tx,
            receiver: rx,
            pending_tasks: pending_tasks.clone(),
            app_state_weak: None,
            cancel_token: CancellationToken::new(),
            batch_buffer: BatchResultCollector::new(),
            db_handles: HashMap::new(),
            failed_clusters: HashSet::new(),
        };

        let requeued = worker
            .finalize_processed_task_success(&key, 0, "input_b")
            .await;
        assert!(!requeued);
        assert!(!pending_tasks.contains_key(&key));
        assert!(matches!(
            worker.receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn task_needs_provider_connection_skips_library_probe_stream() {
        let task = UpdateTask::ProbeStream {
            probe_scope: Arc::from("input_a"),
            unique_id: "u1".to_string(),
            url: "file:///movie.mkv".to_string(),
            item_type: PlaylistItemType::LocalVideo,
            reason: ResolveReasonSet::from_variants(&[ResolveReason::MissingDetails]),
            delay: 0,
        };

        assert!(!InputWorker::task_needs_provider_connection(
            &task,
            InputType::Library
        ));
    }

    #[test]
    fn task_needs_provider_connection_keeps_non_library_probe_stream() {
        let task = UpdateTask::ProbeStream {
            probe_scope: Arc::from("input_a"),
            unique_id: "u1".to_string(),
            url: "http://example.com/stream.m3u8".to_string(),
            item_type: PlaylistItemType::Video,
            reason: ResolveReasonSet::from_variants(&[ResolveReason::MissingDetails]),
            delay: 0,
        };

        assert!(InputWorker::task_needs_provider_connection(
            &task,
            InputType::M3u
        ));
        assert!(InputWorker::task_needs_provider_connection(
            &task,
            InputType::Xtream
        ));
    }

    #[test]
    fn task_needs_provider_connection_keeps_live_probe() {
        let task = UpdateTask::ProbeLive {
            id: ProviderIdType::Id(1),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            interval: 60,
        };

        assert!(InputWorker::task_needs_provider_connection(
            &task,
            InputType::Library
        ));
    }
}
