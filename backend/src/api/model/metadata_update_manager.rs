use crate::api::model::ProviderHandle;
use crate::api::model::{AppState, EventMessage};
use crate::processing::processor::{
    update_generic_stream_metadata, update_live_stream_metadata, update_series_metadata, update_vod_metadata,
    GenericProbeOutcome,
};
use crate::utils::debug_if_enabled;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use log::{debug, error, info, warn};
use parking_lot::Mutex as ParkingMutex;
use serde::{Deserialize, Serialize};
use shared::create_bitset;
use shared::error::TuliproxError;
use shared::model::{
    InputType, LiveStreamProperties, PlaylistItemType, SeriesStreamProperties, UUIDType, VideoStreamProperties,
    XtreamCluster, XtreamPlaylistItem,
};
use shared::utils::generate_playlist_uuid;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::api::model::BatchResultCollector;
use crate::repository::{
    get_input_storage_path, get_target_id_mapping_file, write_playlist_batch_item_upsert, xtream_get_file_path,
    BPlusTree, BPlusTreeQuery, BPlusTreeUpdate,
};
use crate::repository::{
    persist_input_live_info_batch, persist_input_series_info_batch, persist_input_vod_info_batch, TargetIdMapping,
};
use crate::utils::FileReadGuard;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use crate::model::MetadataUpdateConfig;

const PROBE_RETRY_STATE_FILE: &str = "probe_retry_state.db";
const TASK_ERR_NO_CONNECTION: &str = "No connection available";
const TASK_ERR_PREEMPTED: &str = "Task preempted";
const TASK_ERR_UPDATE_IN_PROGRESS: &str = "Playlist update in progress";

#[derive(Debug, Clone)]
struct MetadataUpdateRuntimeSettings {
    queue_log_interval: Duration,
    progress_log_interval: Duration,
    max_resolve_retry_backoff_secs: u64,
    resolve_min_retry_base_secs: u64,
    max_attempts_resolve: u8,
    max_attempts_probe: u8,
    resolve_exhaustion_reset_gap_secs: i64,
    probe_cooldown_secs: i64,
    t_retry_delay_secs: u64,
    probe_retry_load_retry_delay_secs: i64,
    worker_idle_timeout_secs: u64,
    max_queue_size: usize,
    probe_retry_backoff_step_1_secs: u64,
    probe_retry_backoff_step_2_secs: u64,
    probe_retry_backoff_step_3_secs: u64,
    backoff_jitter_percent: u8,
}

impl Default for MetadataUpdateRuntimeSettings {
    fn default() -> Self {
        let defaults = MetadataUpdateConfig::default();
        Self::from_metadata_update(&defaults)
    }
}

impl MetadataUpdateRuntimeSettings {
    fn from_app_state(app_state_weak: Option<&Weak<AppState>>) -> Self {
        let metadata_update = app_state_weak.and_then(Weak::upgrade).map_or_else(
            MetadataUpdateConfig::default,
            |app_state| {
                app_state
                    .app_config
                    .config
                    .load()
                    .metadata_update
                    .as_ref()
                    .map_or_else(MetadataUpdateConfig::default, Clone::clone)
            },
        );
        Self::from_metadata_update(&metadata_update)
    }

    fn from_metadata_update(cfg: &MetadataUpdateConfig) -> Self {
        let to_i64 = |v: u64| i64::try_from(v.max(1)).unwrap_or(i64::MAX);
        Self {
            queue_log_interval: Duration::from_secs(cfg.queue_log_interval_secs.max(1)),
            progress_log_interval: Duration::from_secs(cfg.progress_log_interval_secs.max(1)),
            max_resolve_retry_backoff_secs: cfg.max_resolve_retry_backoff_secs.max(1),
            resolve_min_retry_base_secs: cfg.resolve_min_retry_base_secs.max(1),
            max_attempts_resolve: cfg.max_attempts_resolve.max(1),
            max_attempts_probe: cfg.max_attempts_probe.max(1),
            resolve_exhaustion_reset_gap_secs: to_i64(cfg.resolve_exhaustion_reset_gap_secs),
            probe_cooldown_secs: to_i64(cfg.probe_cooldown_secs),
            t_retry_delay_secs: cfg.t_retry_delay_secs.max(1),
            probe_retry_load_retry_delay_secs: to_i64(cfg.probe_retry_load_retry_delay_secs),
            worker_idle_timeout_secs: cfg.worker_idle_timeout_secs.max(1),
            max_queue_size: cfg.max_queue_size.max(1),
            probe_retry_backoff_step_1_secs: cfg.probe_retry_backoff_step_1_secs.max(1),
            probe_retry_backoff_step_2_secs: cfg.probe_retry_backoff_step_2_secs.max(1),
            probe_retry_backoff_step_3_secs: cfg.probe_retry_backoff_step_3_secs.max(1),
            backoff_jitter_percent: cfg.backoff_jitter_percent.min(95),
        }
    }
}

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
    ResolveVod {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
    },
    ResolveSeries {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
    },
    ProbeLive {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        interval: u64,
    },
    // Generic probe for M3U/Library/etc.
    ProbeStream {
        probe_scope: Arc<str>,
        unique_id: String,
        url: String,
        item_type: PlaylistItemType,
        reason: ResolveReasonSet,
        delay: u16,
    },
}

impl UpdateTask {
    pub fn delay(&self) -> u16 {
        match self {
            UpdateTask::ResolveVod { delay, .. }
            | UpdateTask::ResolveSeries { delay, .. }
            | UpdateTask::ProbeLive { delay, .. }
            | UpdateTask::ProbeStream { delay, .. } => *delay,
        }
    }
}

impl std::fmt::Display for UpdateTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateTask::ResolveVod { id, reason, delay } => {
                write!(f, "Resolve VOD {id} (Reason: {reason}, Delay: {delay}sec)")
            }
            UpdateTask::ResolveSeries { id, reason, delay } => {
                write!(f, "Resolve Series {id} (Reason: {reason}, Delay: {delay}sec)")
            }
            UpdateTask::ProbeLive { id, reason, delay, interval } => {
                write!(f, "Probe Live {id} (Reason: {reason}, Delay: {delay}sec, Interval: {interval}secs )")
            }
            UpdateTask::ProbeStream { probe_scope, unique_id, reason, delay, .. } => {
                write!(f, "Probe Stream {probe_scope}/{unique_id} (Reason: {reason}, Delay: {delay}sec)")
            }
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

#[derive(Debug, Clone)]
struct RetryState {
    attempts: u8,
    next_allowed_at_ts: i64,
    cooldown_until_ts: Option<i64>,
    last_error: Option<String>,
}

impl RetryState {
    fn new() -> Self {
        Self {
            attempts: 0,
            next_allowed_at_ts: 0,
            cooldown_until_ts: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum ProbeRetryDbKey {
    LiveId(u32),
    LiveText(String),
    Stream { scope: String, id: String },
}

impl ProbeRetryDbKey {
    fn from_task_key(task_key: &TaskKey) -> Option<Self> {
        match task_key {
            TaskKey::Live(id) => Some(Self::LiveId(*id)),
            TaskKey::LiveStr(id) => Some(Self::LiveText(id.as_ref().to_owned())),
            TaskKey::Stream { scope, id } => Some(Self::Stream {
                scope: scope.as_ref().to_owned(),
                id: id.as_ref().to_owned(),
            }),
            _ => None,
        }
    }

    fn into_task_key(self) -> TaskKey {
        match self {
            Self::LiveId(id) => TaskKey::Live(id),
            Self::LiveText(id) => TaskKey::LiveStr(Arc::from(id)),
            Self::Stream { scope, id } => TaskKey::Stream {
                scope: Arc::from(scope),
                id: Arc::from(id),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeRetryDbValue {
    attempts: u8,
    next_allowed_at_ts: i64,
    cooldown_until_ts: Option<i64>,
    last_error: Option<String>,
    updated_at_ts: i64,
}

impl ProbeRetryDbValue {
    fn from_retry_state(state: &RetryState, updated_at_ts: i64) -> Self {
        Self {
            attempts: state.attempts,
            next_allowed_at_ts: state.next_allowed_at_ts,
            cooldown_until_ts: state.cooldown_until_ts,
            last_error: state.last_error.clone(),
            updated_at_ts,
        }
    }

    fn cleared(updated_at_ts: i64) -> Self {
        Self {
            attempts: 0,
            next_allowed_at_ts: 0,
            cooldown_until_ts: None,
            last_error: None,
            updated_at_ts,
        }
    }

    fn into_retry_state(self) -> Option<RetryState> {
        if self.attempts == 0 && self.next_allowed_at_ts <= 0 && self.cooldown_until_ts.is_none() {
            return None;
        }
        Some(RetryState {
            attempts: self.attempts,
            next_allowed_at_ts: self.next_allowed_at_ts,
            cooldown_until_ts: self.cooldown_until_ts,
            last_error: self.last_error,
        })
    }
}

fn ensure_probe_retry_db(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut tree = BPlusTree::<ProbeRetryDbKey, ProbeRetryDbValue>::new();
    tree.store(path).map(|_| ())
}

fn load_probe_retry_states_from_disk(path: &Path) -> io::Result<HashMap<TaskKey, RetryState>> {
    ensure_probe_retry_db(path)?;

    let mut result = HashMap::new();
    let mut query = BPlusTreeQuery::<ProbeRetryDbKey, ProbeRetryDbValue>::try_new(path)?;
    for (key, value) in query.iter() {
        if let Some(state) = value.into_retry_state() {
            result.insert(key.into_task_key(), state);
        }
    }

    Ok(result)
}

fn persist_probe_retry_state_to_disk(path: &Path, task_key: &TaskKey, state: Option<&RetryState>) -> io::Result<()> {
    let Some(db_key) = ProbeRetryDbKey::from_task_key(task_key) else {
        return Ok(());
    };

    ensure_probe_retry_db(path)?;

    let now_ts = chrono::Utc::now().timestamp();
    let value = match state {
        Some(s) => ProbeRetryDbValue::from_retry_state(s, now_ts),
        None => ProbeRetryDbValue::cleared(now_ts),
    };

    let mut update = BPlusTreeUpdate::<ProbeRetryDbKey, ProbeRetryDbValue>::try_new_with_backoff(path)?;
    update
        .upsert_batch(&[(&db_key, &value)])
        .map_err(|e| io::Error::other(format!("persist probe retry state failed: {e}")))?;
    Ok(())
}

/// Per-input worker context. Each input has its own worker
/// that processes tasks sequentially with rate limiting.
#[derive(Clone)]
struct InputWorkerContext {
    worker_id: u64,
    sender: mpsc::Sender<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
}

struct PendingTask {
    task: ParkingMutex<UpdateTask>,
    generation: AtomicU64,
}

impl PendingTask {
    fn new(task: UpdateTask) -> Self {
        Self {
            task: ParkingMutex::new(task),
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
/// - Workers terminate after an idle timeout and are respawned on demand
pub struct MetadataUpdateManager {
    /// Per-input worker senders. Worker is spawned when entry is created.
    workers: DashMap<Arc<str>, InputWorkerContext>,
    /// Global application state (weak reference to avoid cycles)
    app_state: tokio::sync::Mutex<Option<Weak<AppState>>>,
    /// Global gate:
    /// - Foreground playlist updates hold WRITE lock.
    /// - Background metadata/probe tasks hold READ lock per task.
    ///   This guarantees that no background task runs while an update is active.
    update_pause_gate: Arc<RwLock<()>>,
    /// Global cancellation token for shutdown
    cancel_token: CancellationToken,
    /// Monotonic worker generation id used to avoid removing a newly spawned worker context.
    next_worker_id: AtomicU64,
}

impl Default for MetadataUpdateManager {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitTaskResult {
    QueuedOrMerged,
    QueueFull,
    ChannelClosed,
}

impl MetadataUpdateManager {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            workers: DashMap::new(),
            app_state: tokio::sync::Mutex::new(None),
            update_pause_gate: Arc::new(RwLock::new(())),
            cancel_token,
            next_worker_id: AtomicU64::new(1),
        }
    }

    /// Acquire exclusive gate for a foreground playlist update.
    /// While this guard is held, background metadata/probe tasks are paused.
    pub async fn acquire_update_pause_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.update_pause_gate.clone().write_owned().await
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
        let runtime_settings = MetadataUpdateRuntimeSettings::from_app_state(app_state_weak.as_ref());
        let max_queue_size = runtime_settings.max_queue_size;

        let task_to_queue = task;
        for attempt in 0..2 {
            // Atomically ensure there is exactly one worker context per input.
            let mut worker_to_spawn: Option<(u64, InputWorker)> = None;
            let ctx = match self.workers.entry(input_name.clone()) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    let (tx, rx) = mpsc::channel::<TaskKey>(max_queue_size);
                    let pending_tasks = Arc::new(DashMap::new());
                    let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);

                    let ctx = InputWorkerContext {
                        worker_id,
                        sender: tx.clone(),
                        pending_tasks: pending_tasks.clone(),
                    };
                    entry.insert(ctx.clone());

                    worker_to_spawn = Some((
                        worker_id,
                        InputWorker {
                            input_name: input_name.clone(),
                            sender: tx,
                            receiver: rx,
                            pending_tasks,
                            app_state_weak: app_state_weak.clone(),
                            update_pause_gate: Arc::clone(&self.update_pause_gate),
                            cancel_token: self.cancel_token.clone(),
                            batch_buffer: BatchResultCollector::new(),
                            db_handles: HashMap::new(),
                            failed_clusters: HashSet::new(),
                            retry_states: HashMap::new(),
                            resolve_exhausted: HashMap::new(),
                            last_cycle_completed_at_ts: None,
                            probe_retry_state_path: None,
                            probe_retry_loaded: false,
                            probe_retry_load_retry_at_ts: None,
                            scheduled_requeues: Arc::new(DashMap::new()),
                        },
                    ));

                    ctx
                }
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

            match Self::submit_task(
                ctx.sender.clone(),
                ctx.pending_tasks.clone(),
                &input_name,
                max_queue_size,
                task_to_queue.clone(),
            )
                .await
            {
                SubmitTaskResult::QueuedOrMerged => return,
                SubmitTaskResult::QueueFull => {
                    warn!("Metadata queue full for input {input_name}, dropping task");
                    return;
                }
                SubmitTaskResult::ChannelClosed => {
                    debug_if_enabled!(
                        "Detected closed metadata worker channel for input {}, recreating worker context (attempt {})",
                        input_name,
                        attempt + 1
                    );
                    Self::remove_worker_context_if_id(&self.workers, &input_name, ctx.worker_id);
                }
            }
        }

        warn!("Failed to queue metadata task for input {input_name} after worker recovery attempts: {task_to_queue}");
    }

    fn remove_worker_context_if_id(
        workers: &DashMap<Arc<str>, InputWorkerContext>,
        input_name: &Arc<str>,
        worker_id: u64,
    ) {
        if let Entry::Occupied(entry) = workers.entry(input_name.clone()) {
            if entry.get().worker_id == worker_id {
                entry.remove();
            }
        }
    }

    async fn submit_task(
        sender: mpsc::Sender<TaskKey>,
        pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
        input_name: &str,
        max_queue_size: usize,
        task: UpdateTask,
    ) -> SubmitTaskResult {
        let key = TaskKey::from_task(&task);

        if let Some(entry) = pending_tasks.get(&key) {
            let mut existing = entry.task.lock();
            let mut merged = false;
            // Merge logic
            match (&mut *existing, task) {
                (
                    UpdateTask::ResolveVod { reason: r1, delay: d1, .. },
                    UpdateTask::ResolveVod { reason: r2, delay: d2, .. },
                )
                | (
                    UpdateTask::ResolveSeries { reason: r1, delay: d1, .. },
                    UpdateTask::ResolveSeries { reason: r2, delay: d2, .. },
                )
                | (
                    UpdateTask::ProbeStream { reason: r1, delay: d1, .. },
                    UpdateTask::ProbeStream { reason: r2, delay: d2, .. },
                ) => {
                    *r1 |= r2;
                    *d1 = min(*d1, d2);
                    merged = true;
                }
                (
                    UpdateTask::ProbeLive { reason: r1, delay: d1, interval: i1, .. },
                    UpdateTask::ProbeLive { reason: r2, delay: d2, interval: i2, .. },
                ) => {
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
            return SubmitTaskResult::QueuedOrMerged;
        }

        if pending_tasks.len() >= max_queue_size {
            return SubmitTaskResult::QueueFull;
        }

        pending_tasks.insert(key.clone(), PendingTask::new(task));
        if sender.send(key.clone()).await.is_err() {
            pending_tasks.remove(&key);
            warn!("Failed to send task signal for input {input_name}");
            return SubmitTaskResult::ChannelClosed;
        }
        SubmitTaskResult::QueuedOrMerged
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
    query: Arc<ParkingMutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
}

struct InputWorker {
    input_name: Arc<str>,
    sender: mpsc::Sender<TaskKey>,
    receiver: mpsc::Receiver<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    app_state_weak: Option<Weak<AppState>>,
    update_pause_gate: Arc<RwLock<()>>,
    cancel_token: CancellationToken,
    batch_buffer: BatchResultCollector,
    db_handles: HashMap<XtreamCluster, DbHandle>,
    failed_clusters: HashSet<XtreamCluster>,
    retry_states: HashMap<TaskKey, RetryState>,
    resolve_exhausted: HashMap<TaskKey, i64>,
    last_cycle_completed_at_ts: Option<i64>,
    probe_retry_state_path: Option<PathBuf>,
    probe_retry_loaded: bool,
    probe_retry_load_retry_at_ts: Option<i64>,
    // Shared with detached delayed requeue tasks spawned in `schedule_requeue_at`.
    // A plain HashMap cannot be moved safely into those `'static` tasks.
    scheduled_requeues: Arc<DashMap<TaskKey, i64>>,
}

impl InputWorker {
    #[allow(clippy::too_many_lines)]
    async fn run(mut self) {
        debug!("Metadata worker started for input {}", &self.input_name);

        let mut processed_vod_count: usize = 0;
        let mut processed_series_count: usize = 0;
        let mut last_queue_log_at = Instant::now();
        let mut last_progress_log_at = Instant::now();
        let mut queue_cycle_active = false;

        let input_name = self.input_name.clone();
        let app_state_weak = self.app_state_weak.clone();

        let startup_runtime_settings = self.runtime_settings();
        self.ensure_probe_retry_state_loaded(&input_name, app_state_weak.as_ref(), &startup_runtime_settings)
            .await;

        // Keep one prefetched task to minimize channel waits/lock churn.
        let mut next_task: Option<(TaskKey, UpdateTask, u64)> = None;

        loop {
            let task_data = if let Some(t) = next_task.take() {
                Some(t)
            } else {
                let wait_runtime_settings = self.runtime_settings();
                self.recv_task_fast_or_wait(&wait_runtime_settings).await
            };

            let Some((current_key, current_task, current_generation)) = task_data else { break };
            let runtime_settings = self.runtime_settings();
            if !self.probe_retry_loaded {
                self.ensure_probe_retry_state_loaded(&input_name, app_state_weak.as_ref(), &runtime_settings)
                    .await;
            }
            let now_ts = chrono::Utc::now().timestamp();

            if !queue_cycle_active {
                // First entry of a new processing cycle.
                queue_cycle_active = true;
                processed_vod_count = 0;
                processed_series_count = 0;
                last_progress_log_at = Instant::now();
                // Emit queue logs promptly for the new cycle.
                last_queue_log_at = Instant::now()
                    .checked_sub(runtime_settings.queue_log_interval + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
                if let Some(app_state) = app_state_weak.as_ref().and_then(Weak::upgrade) {
                    app_state.event_manager.send_event(EventMessage::InputMetadataUpdatesStarted(input_name.clone()));
                }
                if self
                    .last_cycle_completed_at_ts
                    .is_some_and(|last| now_ts.saturating_sub(last) >= runtime_settings.resolve_exhaustion_reset_gap_secs)
                {
                    self.resolve_exhausted.clear();
                }
                debug!("Background metadata update queue has entries for input {input_name}; starting processing");
            }

            let delay_secs = current_task.delay();
            let mut schedule_requeue_at_ts: Option<i64> = None;
            let mut remove_current_task = false;
            let mut apply_rate_limit = false;
            let mut probe_persist_state: Option<Option<RetryState>> = None;
            let mut skip_execution = false;

            if Self::is_resolve_task(&current_task) && self.resolve_exhausted.contains_key(&current_key) {
                debug_if_enabled!(
                    "Skipping exhausted resolve task for input {}: {} (reset window: {}s)",
                    input_name,
                    current_task,
                    runtime_settings.resolve_exhaustion_reset_gap_secs
                );
                self.scheduled_requeues.remove(&current_key);
                remove_current_task = true;
                skip_execution = true;
            }

            if !skip_execution {
                let mut clear_probe_state = false;
                if let Some(state) = self.retry_states.get(&current_key) {
                    if let Some(cooldown_until_ts) = state.cooldown_until_ts {
                        if now_ts < cooldown_until_ts {
                            debug_if_enabled!(
                                "Skipping probe task in cooldown for input {}: {} (cooldown_until={})",
                                input_name,
                                current_task,
                                cooldown_until_ts
                            );
                            self.scheduled_requeues.remove(&current_key);
                            remove_current_task = true;
                            skip_execution = true;
                        } else if Self::is_probe_task(&current_task) {
                            clear_probe_state = true;
                        }
                    }

                    if !skip_execution && state.next_allowed_at_ts > now_ts {
                        schedule_requeue_at_ts = Some(state.next_allowed_at_ts);
                        skip_execution = true;
                    }
                }

                if clear_probe_state {
                    self.retry_states.remove(&current_key);
                    probe_persist_state = Some(None);
                }
            }

            if !skip_execution {
                // Hold READ gate while executing one background task.
                // Foreground updates acquire WRITE gate and therefore pause this path.
                let task_result = {
                    let _task_gate_guard = if let Ok(guard) = self.update_pause_gate.clone().try_read_owned() {
                        guard
                    } else {
                        // Foreground update contention detected.
                        // Drop cached file read handles before blocking on the gate to avoid AB-BA deadlocks.
                        self.release_db_handles();
                        self.update_pause_gate.clone().read_owned().await
                    };
                    Self::process_task_static(
                        &input_name,
                        app_state_weak.as_ref(),
                        &current_task,
                        &mut self.batch_buffer,
                        &mut self.db_handles,
                        &mut self.failed_clusters,
                    )
                    .await
                };

                match task_result {
                    Ok(task_changed) => {
                        if Self::is_vod_task_key(&current_key) {
                            processed_vod_count += 1;
                        } else if Self::is_series_task_key(&current_key) {
                            processed_series_count += 1;
                        }
                        debug!("Processed metadata task for input {input_name}: {current_task} (changed={task_changed})");
                        self.retry_states.remove(&current_key);
                        self.resolve_exhausted.remove(&current_key);
                        self.scheduled_requeues.remove(&current_key);
                        if Self::is_probe_task(&current_task) {
                            probe_persist_state = Some(None);
                        }

                        if last_progress_log_at.elapsed() >= runtime_settings.progress_log_interval {
                            // current_key is removed from pending_tasks later in this loop iteration;
                            // subtract it here so "remaining" reflects the post-success queue size.
                            let (mut remaining_vod, mut remaining_series) = Self::queue_resolve_counts(&self.pending_tasks);
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

                        remove_current_task = true;
                        apply_rate_limit = true;
                    }
                    Err(e) => {
                        if Self::is_transient_worker_error(&e.message) {
                            if e.message == TASK_ERR_UPDATE_IN_PROGRESS {
                                // Drop cached readers quickly so foreground writer can progress.
                                self.release_db_handles();
                            }

                            let retry_delay_secs =
                                Self::compute_t_retry_delay_secs(current_task.delay(), &runtime_settings);
                            let retry_delay_i64 = i64::try_from(retry_delay_secs).unwrap_or(i64::MAX);
                            schedule_requeue_at_ts = Some(now_ts.saturating_add(retry_delay_i64));
                            debug_if_enabled!(
                                "Transient task deferral for input {}: {} (retry_in={}s, err={})",
                                input_name,
                                current_task,
                                retry_delay_secs,
                                e.message
                            );
                        } else {
                            let is_probe_task = Self::is_probe_task(&current_task);
                            let max_attempts = if is_probe_task {
                                runtime_settings.max_attempts_probe
                            } else {
                                runtime_settings.max_attempts_resolve
                            };

                            let state_after_update = {
                                let state = self
                                    .retry_states
                                    .entry(current_key.clone())
                                    .or_insert_with(RetryState::new);
                                state.attempts = state.attempts.saturating_add(1);
                                state.last_error = Some(e.message.clone());

                                if state.attempts < max_attempts {
                                    let backoff_secs = if is_probe_task {
                                        Self::compute_probe_retry_backoff_secs(state.attempts, &runtime_settings)
                                    } else {
                                        Self::compute_resolve_retry_backoff_secs(
                                            current_task.delay(),
                                            state.attempts,
                                            &runtime_settings,
                                        )
                                    };
                                    let backoff_i64 = i64::try_from(backoff_secs).unwrap_or(i64::MAX);
                                    state.next_allowed_at_ts = now_ts.saturating_add(backoff_i64);
                                    state.cooldown_until_ts = None;
                                } else if is_probe_task {
                                    state.cooldown_until_ts =
                                        Some(now_ts.saturating_add(runtime_settings.probe_cooldown_secs));
                                    state.next_allowed_at_ts = state.cooldown_until_ts.unwrap_or(now_ts);
                                }

                                state.clone()
                            };

                            let attempts = state_after_update.attempts;

                            if attempts >= max_attempts {
                                self.scheduled_requeues.remove(&current_key);
                                if is_probe_task {
                                    remove_current_task = true;
                                    probe_persist_state = Some(Some(state_after_update.clone()));
                                    debug_if_enabled!(
                                        "Probe task exhausted for input {}: {} (attempts={}, cooldown_until={:?})",
                                        input_name,
                                        current_task,
                                        state_after_update.attempts,
                                        state_after_update.cooldown_until_ts
                                    );
                                } else {
                                    self.resolve_exhausted.insert(current_key.clone(), now_ts);
                                    self.retry_states.remove(&current_key);
                                    remove_current_task = true;
                                    debug_if_enabled!(
                                        "Resolve task exhausted for input {}: {} (attempts={})",
                                        input_name,
                                        current_task,
                                        attempts
                                    );
                                }
                            } else {
                                schedule_requeue_at_ts = Some(state_after_update.next_allowed_at_ts);
                                if is_probe_task {
                                    probe_persist_state = Some(Some(state_after_update.clone()));
                                }
                                debug_if_enabled!(
                                    "Task failed for input {}, scheduling retry: {} (attempt={}, next_allowed_at={}, err={})",
                                    input_name,
                                    current_task,
                                    attempts,
                                    state_after_update.next_allowed_at_ts,
                                    e.message
                                );
                            }
                        }
                    }
                }
            }

            if let Some(state) = probe_persist_state {
                self.persist_probe_retry_state(&current_key, state.as_ref()).await;
            }

            // Check and flush batch
            if self.batch_buffer.should_flush() {
                self.release_db_handles();
                let _gate_guard = self.update_pause_gate.clone().read_owned().await;
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if let Some(retry_at_ts) = schedule_requeue_at_ts {
                self.schedule_requeue_at(current_key.clone(), retry_at_ts);
            }

            if remove_current_task {
                self.finalize_processed_task_success(&current_key, current_generation, &input_name).await;
            }

            if apply_rate_limit {
                // Rate limiting
                if delay_secs > 0
                    && Self::sleep_or_cancel(&self.cancel_token, Duration::from_secs(u64::from(delay_secs))).await
                {
                    break;
                }
            }

            // Try to get the next task immediately to keep locks open.
            // Ignore phantom signals (channel key without pending map entry).
            if next_task.is_none() {
                while let Ok(key) = self.receiver.try_recv() {
                    if let Some(snapshot) = self.load_task_snapshot(key) {
                        next_task = Some(snapshot);
                        break;
                    }
                }
            }

            let channel_has_work = next_task.is_some() || !self.receiver.is_empty();
            let queue_completely_empty = !channel_has_work && self.pending_tasks.is_empty();

            // Avoid O(n) queue scans per task; report queue status periodically.
            if (channel_has_work || !self.pending_tasks.is_empty())
                && last_queue_log_at.elapsed() >= runtime_settings.queue_log_interval
            {
                let queue_counts = Self::queue_resolve_counts(&self.pending_tasks);
                debug!(
                    "In queue to resolve vod: {}, series: {} (input: {input_name})",
                    queue_counts.0, queue_counts.1
                );
                last_queue_log_at = Instant::now();
            }

            // If no immediate work is available, flush buffered results now even when delayed retries remain pending.
            if !channel_has_work && !self.batch_buffer.is_empty() {
                self.release_db_handles();
                let _gate_guard = self.update_pause_gate.clone().read_owned().await;
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if queue_cycle_active && queue_completely_empty {
                info!("All pending metadata resolves completed for input {input_name}");
                if let Some(app_state) = app_state_weak.as_ref().and_then(Weak::upgrade) {
                    app_state
                        .event_manager
                        .send_event(EventMessage::InputMetadataUpdatesCompleted(input_name.clone()));
                }
                self.last_cycle_completed_at_ts = Some(chrono::Utc::now().timestamp());
                queue_cycle_active = false;
                processed_vod_count = 0;
                processed_series_count = 0;
            }
        }

        // Final flush
        self.release_db_handles();
        if !self.batch_buffer.is_empty() {
            let _gate_guard = self.update_pause_gate.clone().read_owned().await;
            Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
        }

        debug!("Metadata worker stopped for input {input_name}");
    }

    async fn ensure_probe_retry_state_loaded(
        &mut self,
        input_name: &str,
        app_state_weak: Option<&Weak<AppState>>,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) {
        if self.probe_retry_loaded {
            return;
        }
        let now_ts = chrono::Utc::now().timestamp();
        if self
            .probe_retry_load_retry_at_ts
            .is_some_and(|retry_at_ts| now_ts < retry_at_ts)
        {
            return;
        }

        let Some(app_state) = app_state_weak.and_then(Weak::upgrade) else {
            self.probe_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.probe_retry_load_retry_delay_secs));
            return;
        };

        let working_dir = app_state.app_config.config.load().working_dir.clone();
        let Ok(storage_path) = get_input_storage_path(input_name, &working_dir).await else {
            warn!("Could not resolve storage path for probe retry state on input {input_name}");
            self.probe_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.probe_retry_load_retry_delay_secs));
            return;
        };

        let retry_path = storage_path.join(PROBE_RETRY_STATE_FILE);
        self.probe_retry_state_path = Some(retry_path.clone());

        let loaded = match tokio::task::spawn_blocking(move || load_probe_retry_states_from_disk(&retry_path)).await {
            Ok(Ok(states)) => states,
            Ok(Err(err)) => {
                warn!("Failed to load probe retry state for input {input_name}: {err}");
                self.probe_retry_load_retry_at_ts =
                    Some(now_ts.saturating_add(runtime_settings.probe_retry_load_retry_delay_secs));
                return;
            }
            Err(err) => {
                warn!("Failed to load probe retry state for input {input_name}: {err}");
                self.probe_retry_load_retry_at_ts =
                    Some(now_ts.saturating_add(runtime_settings.probe_retry_load_retry_delay_secs));
                return;
            }
        };

        // Intentionally do not resurrect pending probe tasks solely from persisted retry state.
        // The persisted state is applied once the corresponding task is naturally queued again
        // (for example by the next playlist update), because state alone does not contain the
        // full `UpdateTask` payload for all variants.
        for (key, state) in loaded {
            self.retry_states.insert(key, state);
        }
        self.probe_retry_loaded = true;
        self.probe_retry_load_retry_at_ts = None;
    }

    async fn persist_probe_retry_state(&self, key: &TaskKey, state: Option<&RetryState>) {
        let Some(path) = self.probe_retry_state_path.clone() else {
            return;
        };

        let key = key.clone();
        let state = state.cloned();
        let input_name = self.input_name.clone();
        let persist_result = tokio::task::spawn_blocking(move || {
            persist_probe_retry_state_to_disk(&path, &key, state.as_ref())
        })
        .await;

        match persist_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!("Failed to persist probe retry state for input {input_name}: {err}"),
            Err(err) => warn!("Failed to persist probe retry state for input {input_name}: {err}"),
        }
    }

    fn schedule_requeue_at(&self, key: TaskKey, retry_at_ts: i64) {
        let now_ts = chrono::Utc::now().timestamp();
        let retry_at_ts = retry_at_ts.max(now_ts);

        if self
            .scheduled_requeues
            .get(&key)
            .is_some_and(|existing| *existing == retry_at_ts)
        {
            return;
        }

        self.scheduled_requeues.insert(key.clone(), retry_at_ts);

        let sender = self.sender.clone();
        let pending_tasks = Arc::clone(&self.pending_tasks);
        let scheduled = Arc::clone(&self.scheduled_requeues);
        let cancel_token = self.cancel_token.clone();
        let input_name = self.input_name.clone();

        tokio::spawn(async move {
            let delay_secs = retry_at_ts.saturating_sub(chrono::Utc::now().timestamp());
            if delay_secs > 0 {
                let delay = Duration::from_secs(u64::try_from(delay_secs).unwrap_or(u64::MAX));
                tokio::select! {
                    () = cancel_token.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
            }

            let should_send = scheduled
                .get(&key)
                .is_some_and(|scheduled_at| *scheduled_at == retry_at_ts);
            if !should_send {
                return;
            }
            scheduled.remove(&key);

            if !pending_tasks.contains_key(&key) {
                return;
            }

            if sender.send(key.clone()).await.is_err() {
                pending_tasks.remove(&key);
                warn!("Failed to schedule delayed retry task for input {input_name}");
            }
        });
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

    async fn recv_task_fast_or_wait(
        &mut self,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> Option<(TaskKey, UpdateTask, u64)> {
        // Fast path: drain immediate signals until we find a real pending task.
        loop {
            match self.receiver.try_recv() {
                Ok(key) => {
                    if let Some(snapshot) = self.load_task_snapshot(key) {
                        return Some(snapshot);
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    break;
                }
            }
        }

        // When idle, release read handles to avoid writer starvation.
        self.release_db_handles();

        loop {
            tokio::select! {
                biased;
                () = self.cancel_token.cancelled() => return None,
                res = tokio::time::timeout(Duration::from_secs(runtime_settings.worker_idle_timeout_secs), self.receiver.recv()) => {
                    match res {
                        Ok(Some(key)) => {
                            if let Some(snapshot) = self.load_task_snapshot(key) {
                                return Some(snapshot);
                            }
                        }
                        Ok(None) => return None,
                        Err(_) => {
                            loop {
                                match self.receiver.try_recv() {
                                    Ok(key) => {
                                        if let Some(snapshot) = self.load_task_snapshot(key) {
                                            return Some(snapshot);
                                        }
                                    }
                                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
                                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                                }
                            }
                            if self.pending_tasks.is_empty()
                                && self.receiver.is_empty()
                                && self.scheduled_requeues.is_empty()
                            {
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }

    fn load_task_snapshot(&self, key: TaskKey) -> Option<(TaskKey, UpdateTask, u64)> {
        let entry = self.pending_tasks.get(&key)?;
        let generation = entry.generation.load(Ordering::Relaxed);
        let task = entry.task.lock().clone();
        Some((key, task, generation))
    }

    fn runtime_settings(&self) -> MetadataUpdateRuntimeSettings {
        MetadataUpdateRuntimeSettings::from_app_state(self.app_state_weak.as_ref())
    }

    fn release_db_handles(&mut self) {
        if !self.db_handles.is_empty() {
            self.db_handles.clear();
        }
        if !self.failed_clusters.is_empty() {
            self.failed_clusters.clear();
        }
    }

    fn compute_resolve_retry_backoff_secs(
        base_delay_secs: u16,
        attempts: u8,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> u64 {
        let base_delay = u64::from(base_delay_secs).max(runtime_settings.resolve_min_retry_base_secs);
        let exp = u32::from(attempts.saturating_sub(1).min(6));
        let without_jitter = base_delay
            .saturating_mul(2_u64.saturating_pow(exp))
            .min(runtime_settings.max_resolve_retry_backoff_secs);
        Self::apply_jitter(without_jitter, runtime_settings.backoff_jitter_percent)
    }

    fn compute_t_retry_delay_secs(base_delay_secs: u16, runtime_settings: &MetadataUpdateRuntimeSettings) -> u64 {
        u64::from(base_delay_secs).max(runtime_settings.t_retry_delay_secs)
    }

    fn compute_probe_retry_backoff_secs(attempts: u8, runtime_settings: &MetadataUpdateRuntimeSettings) -> u64 {
        let base_secs = match attempts {
            1 => runtime_settings.probe_retry_backoff_step_1_secs,
            2 => runtime_settings.probe_retry_backoff_step_2_secs,
            _ => runtime_settings.probe_retry_backoff_step_3_secs,
        };
        Self::apply_jitter(base_secs, runtime_settings.backoff_jitter_percent)
    }

    fn apply_jitter(base_secs: u64, jitter_percent: u8) -> u64 {
        let jitter_percent = i64::from(jitter_percent);
        let jitter_percent = fastrand::i64(-jitter_percent..=jitter_percent);
        let base_i64 = i64::try_from(base_secs).unwrap_or(i64::MAX);
        let jitter_delta = base_i64.saturating_mul(jitter_percent).saturating_div(100);
        let jittered = base_i64.saturating_add(jitter_delta);
        u64::try_from(jittered.max(1)).unwrap_or(1)
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

    #[inline]
    fn is_probe_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ProbeLive { .. } | UpdateTask::ProbeStream { .. })
    }

    #[inline]
    fn is_resolve_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. })
    }

    fn is_transient_worker_error(message: &str) -> bool {
        message == TASK_ERR_UPDATE_IN_PROGRESS || message == TASK_ERR_PREEMPTED
    }

    // Changed to static method
    async fn flush_batch_static(
        input_name: &str,
        app_state_weak: Option<&Weak<AppState>>,
        batch_buffer: &mut BatchResultCollector,
    ) {
        if batch_buffer.is_empty() {
            return;
        }

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
                    if let Err(e) = persist_input_vod_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Video,
                        input_name,
                        updates,
                    )
                    .await
                    {
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
                    if let Err(e) = persist_input_series_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Series,
                        input_name,
                        updates,
                    )
                    .await
                    {
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
                    if let Err(e) = persist_input_live_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Live,
                        input_name,
                        updates,
                    )
                    .await
                    {
                        error!("Failed to flush Live batch for input {input_name}: {e}");
                    }
                }
            }
        }

        let cascade_batch = BatchResultCollector { vod: vod_updates, series: series_updates, live: live_updates };

        Self::cascade_updates(&app_state, &app_config.config.load(), input_name, &cascade_batch).await;
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

        if targets.is_empty() {
            return;
        }

        for target in targets {
            let target_name = &target.name;
            let Some(target_path) = crate::repository::get_target_storage_path(config, target_name) else {
                continue;
            };
            let Some(storage_path) = crate::repository::xtream_get_storage_path(config, target_name) else {
                continue;
            };
            let mapping_file = get_target_id_mapping_file(&target_path);

            let mapping = {
                // Scope read lock strictly to mapping load.
                let _file_lock = app_state.app_config.file_locks.read_lock(&mapping_file).await;
                let mapping_file_clone = mapping_file.clone();
                match tokio::task::spawn_blocking(move || {
                    TargetIdMapping::new(&mapping_file_clone, false)
                })
                .await
                {
                    Ok(Ok(mapping)) => mapping,
                    Ok(Err(e)) => {
                        error!("Failed to open ID mapping for target {target_name}: {e}");
                        continue;
                    }
                    Err(err) => {
                        error!("Failed to open ID mapping for target {target_name}: {err}");
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
            Self::apply_vod_cascade_updates(app_state, &target, &storage_path, vod_virtual_updates).await;

            let series_virtual_updates = Self::collect_series_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_series_cascade_updates(app_state, &target, &storage_path, series_virtual_updates).await;

            let live_virtual_updates = Self::collect_live_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_live_cascade_updates(app_state, &target, &storage_path, live_virtual_updates).await;
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
                    if let Some(virtual_id) = Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid) {
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
                    let uuid = generate_playlist_uuid(input_name, provider_id_text, PlaylistItemType::Series, "");
                    if let Some(virtual_id) = Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid) {
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
                    if let Some(virtual_id) = Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid) {
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
        let updates_input: Vec<(u32, VideoStreamProperties)> =
            virtual_updates.into_iter().map(|(vid, props)| (vid, props.clone())).collect();

        let updates = {
            // Scope read lock to read-only query phase so write phase can acquire lock.
            let _file_lock = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path_clone = xtream_path.clone();
            match tokio::task::spawn_blocking(move || -> Vec<XtreamPlaylistItem> {
                let mut updates = Vec::with_capacity(updates_input.len());
                let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone) else {
                    return updates;
                };
                for (virtual_id, props) in updates_input {
                    if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                        item.additional_properties = Some(shared::model::StreamProperties::Video(Box::new(props)));
                        updates.push(item);
                    }
                }
                updates
            })
            .await
            {
                Ok(updates) => updates,
                Err(err) => {
                    error!("Failed to read VOD updates from disk for {target_name}: {err}");
                    Vec::new()
                }
            }
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) =
            write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Video, &updates).await
        {
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
        let updates_input: Vec<(u32, SeriesStreamProperties)> =
            virtual_updates.into_iter().map(|(vid, props)| (vid, props.clone())).collect();

        let updates = {
            // Scope read lock to read-only query phase so write phase can acquire lock.
            let _file_lock = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path_clone = xtream_path.clone();
            match tokio::task::spawn_blocking(move || -> Vec<XtreamPlaylistItem> {
                let mut updates = Vec::with_capacity(updates_input.len());
                let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone) else {
                    return updates;
                };
                for (virtual_id, props) in updates_input {
                    if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                        item.additional_properties = Some(shared::model::StreamProperties::Series(Box::new(props)));
                        updates.push(item);
                    }
                }
                updates
            })
            .await
            {
                Ok(updates) => updates,
                Err(err) => {
                    error!("Failed to read Series updates from disk for {target_name}: {err}");
                    Vec::new()
                }
            }
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) =
            write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Series, &updates).await
        {
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
        let updates_input: Vec<(u32, LiveStreamProperties)> =
            virtual_updates.into_iter().map(|(vid, props)| (vid, props.clone())).collect();

        let updates = {
            // Scope read lock to read-only query phase so write phase can acquire lock.
            let _file_lock = app_state.app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path_clone = xtream_path.clone();
            match tokio::task::spawn_blocking(move || -> Vec<XtreamPlaylistItem> {
                let mut updates = Vec::with_capacity(updates_input.len());
                let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone) else {
                    return updates;
                };
                for (virtual_id, props) in updates_input {
                    if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                        item.additional_properties = Some(shared::model::StreamProperties::Live(Box::new(props)));
                        updates.push(item);
                    }
                }
                updates
            })
            .await
            {
                Ok(updates) => updates,
                Err(err) => {
                    error!("Failed to read Live updates from disk for {target_name}: {err}");
                    Vec::new()
                }
            }
        };

        if updates.is_empty() {
            return;
        }

        if let Err(e) =
            write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Live, &updates).await
        {
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
    async fn get_or_open_query(
        input_name: &str,
        app_state: &Arc<AppState>,
        cluster: XtreamCluster,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Option<Arc<ParkingMutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>> {
        if failed_clusters.contains(&cluster) {
            return None;
        }

        if let std::collections::hash_map::Entry::Vacant(entry) = db_handles.entry(cluster) {
            let working_dir = &app_state.app_config.config.load().working_dir;
            if let Ok(storage_path) = get_input_storage_path(input_name, working_dir).await {
                let file_path = xtream_get_file_path(&storage_path, cluster);
                if file_path.exists() {
                    let lock = app_state.app_config.file_locks.read_lock(&file_path).await;
                    let file_path = file_path.clone();
                    let query = match tokio::task::spawn_blocking(move || {
                        BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&file_path)
                    })
                    .await
                    {
                        Ok(Ok(query)) => Some(query),
                        Ok(Err(err)) => {
                            error!("Failed to open BPlusTreeQuery for {cluster}: {err}");
                            None
                        }
                        Err(err) => {
                            error!("Failed to open BPlusTreeQuery for {cluster}: {err}");
                            None
                        }
                    };

                    if let Some(query) = query {
                        entry.insert(DbHandle { _guard: lock, query: Arc::new(ParkingMutex::new(query)) });
                    } else {
                        failed_clusters.insert(cluster);
                    }
                } else {
                    // File doesn't exist; do not mark as failure to allow future creation.
                }
            }
        }

        db_handles.get(&cluster).map(|h| Arc::clone(&h.query))
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
            let stream_id = *vid;
            if let Some(query) =
                Self::get_or_open_query(input_name, app_state, cluster, db_handles, failed_clusters).await
            {
                let query = Arc::clone(&query);
                let item = match tokio::task::spawn_blocking(move || {
                    let mut guard = query.lock();
                    guard.query_zero_copy(&stream_id).ok().flatten()
                })
                .await
                {
                    Ok(item) => item,
                    Err(err) => {
                        error!("Failed to query item name for {stream_id}: {err}");
                        None
                    }
                };

                if let Some(item) = item {
                    return Some(if item.title.is_empty() { item.name.to_string() } else { item.title.to_string() });
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    async fn process_task_static(
        input_name: &Arc<str>,
        app_state_weak: Option<&Weak<AppState>>,
        task: &UpdateTask,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<bool, TuliproxError> {
        let app_state =
            app_state_weak.and_then(Weak::upgrade).ok_or_else(|| shared::error::info_err!("AppState not available"))?;

        let Some(input_base) = app_state.app_config.get_input_by_name(input_name) else {
            return Err(shared::error::info_err!("Input {} not found", input_name));
        };

        if !input_base.enabled {
            return Err(shared::error::info_err!("Input {} is disabled", input_name));
        }

        // Background metadata/probe tasks are low-priority.
        // Never run them while a foreground playlist update is active.
        if let Some(guard) = app_state.update_guard.try_playlist() {
            drop(guard);
        } else {
            return Err(shared::error::info_err!("{}", TASK_ERR_UPDATE_IN_PROGRESS));
        }

        let needs_probe_connection = Self::task_needs_provider_connection(task, input_base.input_type);

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

        let config_to_use = provider_handle.as_ref().and_then(|handle| handle.allocation.get_provider_config());
        let name_display = item_title.as_deref().map_or(String::new(), |n| format!(" \"{n}\""));

        debug!("Processing task for {input_name}: {task}{name_display}");

        let pre_vod_updates = collector.vod.len();
        let pre_series_updates = collector.series.len();
        let pre_live_updates = collector.live.len();

        // Determine input to use (may be alias)
        let input_to_use = config_to_use
            .filter(|alloc| alloc.name != input_base.name)
            .and_then(|alloc| input_base.aliases.as_ref()?.iter().find(|a| a.enabled && a.name == alloc.name))
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
                Self::execute_task_inner_static(
                    &app_state,
                    &client,
                    &input_to_use,
                    task,
                    item_title.as_deref(),
                    Some(handle),
                    collector,
                    db_handles,
                    failed_clusters,
                )
                .await
            }
        } else {
            Self::execute_task_inner_static(
                &app_state,
                &client,
                &input_to_use,
                task,
                item_title.as_deref(),
                None,
                collector,
                db_handles,
                failed_clusters,
            )
            .await
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
            UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                reason.contains(ResolveReason::Probe)
            }
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
                let resolve_tmdb =
                    fetch_info || reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date);
                let will_probe = reason.contains(ResolveReason::Probe);

                // If we are going to probe, release the cached handle to avoid holding a READ lock
                // for along time (blocks writers) and also to avoid potential deadlocks if
                // the probe function itself tries to acquire a WRITE lock later.
                if will_probe {
                    db_handles.remove(&XtreamCluster::Video);
                }

                let query_opt = if will_probe {
                    None
                } else {
                    Self::get_or_open_query(&input.name, app_state, XtreamCluster::Video, db_handles, failed_clusters)
                        .await
                };

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
                    resolve_tmdb,
                    will_probe,
                    query_opt,
                )
                .await
                {
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
                let resolve_tmdb =
                    fetch_info || reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date);
                let will_probe = reason.contains(ResolveReason::Probe);

                if will_probe {
                    db_handles.remove(&XtreamCluster::Series);
                }

                // Get handle for Series
                let query_opt = if will_probe {
                    None
                } else {
                    Self::get_or_open_query(&input.name, app_state, XtreamCluster::Series, db_handles, failed_clusters)
                        .await
                };

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
                    resolve_tmdb,
                    will_probe,
                    query_opt,
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_series(id.clone(), props);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeLive { id, .. } => {
                // ProbeLive always probes, so we must never use a cached handle here.
                db_handles.remove(&XtreamCluster::Live);

                match update_live_stream_metadata(
                    &app_state.app_config,
                    input,
                    id.clone(),
                    false,
                    None,
                    active_handle,
                    &app_state.active_provider,
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_live(id.clone(), props);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeStream { unique_id, url, item_type, .. } => {
                // Generic probe doesn't support batching yet and always takes a WRITE lock.
                // It can target any cluster, so we clear all handles to be safe.
                if !db_handles.is_empty() {
                    db_handles.clear();
                }

                let outcome = update_generic_stream_metadata(
                    &app_state.app_config,
                    input.as_ref(),
                    unique_id,
                    url,
                    *item_type,
                    &app_state.active_provider,
                    active_handle,
                )
                .await?;

                match outcome {
                    GenericProbeOutcome::Updated | GenericProbeOutcome::Noop => Ok(()),
                    GenericProbeOutcome::ProbeFailed => Err(shared::error::info_err!(
                        "Probe stream task failed for {}",
                        unique_id
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
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
        let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;
        MetadataUpdateManager::submit_task(tx.clone(), pending_tasks.clone(), "input_a", queue_size, task_initial).await;

        let task_merge = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 2,
        };
        MetadataUpdateManager::submit_task(tx, pending_tasks.clone(), "input_a", queue_size, task_merge).await;

        let first_signal = rx.try_recv().expect("first signal should be queued");
        assert_eq!(first_signal, TaskKey::Vod(42));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));

        let entry = pending_tasks.get(&TaskKey::Vod(42)).expect("pending entry should exist");
        assert_eq!(entry.generation.load(Ordering::Relaxed), 1);

        let merged = entry.task.lock().clone();
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
            update_pause_gate: Arc::new(RwLock::new(())),
            cancel_token: CancellationToken::new(),
            batch_buffer: BatchResultCollector::new(),
            db_handles: HashMap::new(),
            failed_clusters: HashSet::new(),
            retry_states: HashMap::new(),
            resolve_exhausted: HashMap::new(),
            last_cycle_completed_at_ts: None,
            probe_retry_state_path: None,
            probe_retry_loaded: false,
            probe_retry_load_retry_at_ts: None,
            scheduled_requeues: Arc::new(DashMap::new()),
        };

        let requeued = worker.finalize_processed_task_success(&key, 0, "input_a").await;
        assert!(requeued);
        assert!(pending_tasks.contains_key(&key));
        assert_eq!(worker.receiver.try_recv().expect("requeued signal should be present"), key);
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
            update_pause_gate: Arc::new(RwLock::new(())),
            cancel_token: CancellationToken::new(),
            batch_buffer: BatchResultCollector::new(),
            db_handles: HashMap::new(),
            failed_clusters: HashSet::new(),
            retry_states: HashMap::new(),
            resolve_exhausted: HashMap::new(),
            last_cycle_completed_at_ts: None,
            probe_retry_state_path: None,
            probe_retry_loaded: false,
            probe_retry_load_retry_at_ts: None,
            scheduled_requeues: Arc::new(DashMap::new()),
        };

        let requeued = worker.finalize_processed_task_success(&key, 0, "input_b").await;
        assert!(!requeued);
        assert!(!pending_tasks.contains_key(&key));
        assert!(matches!(worker.receiver.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
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

        assert!(!InputWorker::task_needs_provider_connection(&task, InputType::Library));
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

        assert!(InputWorker::task_needs_provider_connection(&task, InputType::M3u));
        assert!(InputWorker::task_needs_provider_connection(&task, InputType::Xtream));
    }

    #[test]
    fn task_needs_provider_connection_keeps_live_probe() {
        let task = UpdateTask::ProbeLive {
            id: ProviderIdType::Id(1),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            interval: 60,
        };

        assert!(InputWorker::task_needs_provider_connection(&task, InputType::Library));
    }

    #[test]
    fn probe_retry_state_disk_roundtrip() {
        let dir = tempdir().expect("tempdir should be created");
        let path = dir.path().join("probe_retry_state.db");
        let key = TaskKey::Stream {
            scope: Arc::from("input_a"),
            id: Arc::from("stream_1"),
        };
        let state = RetryState {
            attempts: 3,
            next_allowed_at_ts: 1_700_000_000,
            cooldown_until_ts: Some(1_700_086_400),
            last_error: Some("probe timeout".to_string()),
        };

        persist_probe_retry_state_to_disk(&path, &key, Some(&state))
            .expect("state persistence should succeed");
        let loaded = load_probe_retry_states_from_disk(&path).expect("state load should succeed");
        let loaded_state = loaded.get(&key).expect("probe key should be present");

        assert_eq!(loaded_state.attempts, 3);
        assert_eq!(loaded_state.cooldown_until_ts, Some(1_700_086_400));
        assert_eq!(loaded_state.last_error.as_deref(), Some("probe timeout"));

        persist_probe_retry_state_to_disk(&path, &key, None).expect("state clear should succeed");
        let cleared = load_probe_retry_states_from_disk(&path).expect("state reload should succeed");
        assert!(!cleared.contains_key(&key));
    }

    #[test]
    fn probe_backoff_steps_follow_expected_windows() {
        let runtime_settings = MetadataUpdateRuntimeSettings::default();
        let first = InputWorker::compute_probe_retry_backoff_secs(1, &runtime_settings);
        let second = InputWorker::compute_probe_retry_backoff_secs(2, &runtime_settings);
        let third = InputWorker::compute_probe_retry_backoff_secs(3, &runtime_settings);

        assert!((480..=720).contains(&first), "expected ~10m with jitter, got {first}");
        assert!((1_440..=2_160).contains(&second), "expected ~30m with jitter, got {second}");
        assert!((2_880..=4_320).contains(&third), "expected ~60m with jitter, got {third}");
    }
}
