use crate::{
    api::model::{AppState, BatchResultCollector, EventMessage, ProviderHandle},
    model::MetadataUpdateConfig,
    processing::processor::{
        update_generic_stream_metadata, update_live_stream_metadata, update_series_metadata, update_vod_metadata,
        GenericProbeOutcome, SeriesProbeSettings,
    },
    repository::{
        get_input_storage_path, get_target_id_mapping_file, persist_input_live_info_batch,
        persist_input_series_info_batch, persist_input_vod_info_batch, write_playlist_batch_item_upsert,
        xtream_get_file_path, BPlusTree, BPlusTreeQuery, BPlusTreeUpdate, TargetIdMapping,
    },
    utils::{debug_if_enabled, FileReadGuard},
};
use arc_swap::ArcSwap;
use dashmap::{mapref::entry::Entry, DashMap};
use log::{debug, error, info, warn};
use parking_lot::Mutex as ParkingMutex;
use serde::{Deserialize, Serialize};
use shared::{
    create_bitset,
    error::TuliproxError,
    model::{
        InputType, LiveStreamProperties, PlaylistItemType, SeriesStreamProperties, UUIDType, VideoStreamProperties,
        XtreamCluster, XtreamPlaylistItem,
    },
    utils::generate_playlist_uuid,
};
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        Arc, OnceLock, Weak,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use shared::utils::default_probe_user_priority;

const METADATA_RETRY_STATE_FILE: &str = "metadata_retry_state.db";
const TASK_ERR_NO_CONNECTION: &str = "No connection available";
const TASK_ERR_PREEMPTED: &str = "Task preempted";
const TASK_ERR_UPDATE_IN_PROGRESS: &str = "Playlist update in progress";
// Per-task execution timeout.  ffprobe defaults to 60 s; allow extra headroom for network
// fetches, TMDB lookups, and B+Tree writes before declaring the task stuck.
const PROBE_TASK_TIMEOUT_SECS: u64 = 30;
const BLOCKING_DB_MIN_CONCURRENCY: usize = 4;
const BLOCKING_DB_MAX_CONCURRENCY: usize = 32;
const RETRY_STATE_PRUNE_INTERVAL_SECS: i64 = 300;
const RETRY_STATE_MIN_TTL_SECS: i64 = 86_400;
const RUNTIME_SETTINGS_REFRESH_INTERVAL_SECS: u64 = 60;

fn metadata_blocking_concurrency_limit() -> usize {
    let parallelism =
        std::thread::available_parallelism().map_or(BLOCKING_DB_MIN_CONCURRENCY, std::num::NonZeroUsize::get);
    parallelism.saturating_mul(2).clamp(BLOCKING_DB_MIN_CONCURRENCY, BLOCKING_DB_MAX_CONCURRENCY)
}

fn metadata_blocking_semaphore() -> &'static Semaphore {
    static BLOCKING_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    BLOCKING_SEMAPHORE.get_or_init(|| Semaphore::new(metadata_blocking_concurrency_limit()))
}

async fn spawn_blocking_limited<F, R>(task: F) -> Result<R, tokio::task::JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    // Throttle B+Tree and file-heavy blocking tasks to avoid saturating Tokio's blocking pool.
    let permit = metadata_blocking_semaphore().acquire().await.ok();
    let result = tokio::task::spawn_blocking(task).await;
    drop(permit);
    result
}

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
    tmdb_cooldown_secs: i64,
    retry_delay_secs: u64,
    metadata_retry_load_retry_delay_secs: i64,
    worker_idle_timeout_secs: u64,
    max_queue_size: usize,
    no_change_cache_ttl_secs: u64,
    probe_fairness_resolve_burst: usize,
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
        let metadata_update =
            app_state_weak.and_then(Weak::upgrade).map_or_else(MetadataUpdateConfig::default, |app_state| {
                app_state
                    .app_config
                    .config
                    .load()
                    .metadata_update
                    .as_ref()
                    .map_or_else(MetadataUpdateConfig::default, Clone::clone)
            });
        Self::from_metadata_update(&metadata_update)
    }

    fn from_metadata_update(cfg: &MetadataUpdateConfig) -> Self {
        let to_i64 = |v: u64| i64::try_from(v.max(1)).unwrap_or(i64::MAX);
        Self {
            queue_log_interval: Duration::from_secs(cfg.log.queue_interval_secs.max(1)),
            progress_log_interval: Duration::from_secs(cfg.log.progress_interval_secs.max(1)),
            max_resolve_retry_backoff_secs: cfg.resolve.max_retry_backoff_secs.max(1),
            resolve_min_retry_base_secs: cfg.resolve.min_retry_base_secs.max(1),
            max_attempts_resolve: cfg.resolve.max_attempts.max(1),
            max_attempts_probe: cfg.probe.max_attempts.max(1),
            resolve_exhaustion_reset_gap_secs: to_i64(cfg.resolve.exhaustion_reset_gap_secs),
            probe_cooldown_secs: to_i64(cfg.probe.cooldown_secs),
            tmdb_cooldown_secs: to_i64(cfg.tmdb.cooldown_secs),
            retry_delay_secs: cfg.retry_delay_secs.max(1),
            metadata_retry_load_retry_delay_secs: to_i64(cfg.probe.retry_load_retry_delay_secs),
            worker_idle_timeout_secs: cfg.worker_idle_timeout_secs.max(1),
            max_queue_size: cfg.max_queue_size.max(1),
            no_change_cache_ttl_secs: cfg.no_change_cache_ttl_secs.max(1),
            probe_fairness_resolve_burst: cfg.probe_fairness_resolve_burst.max(1),
            probe_retry_backoff_step_1_secs: cfg.probe.retry_backoff_step_1_secs.max(1),
            probe_retry_backoff_step_2_secs: cfg.probe.retry_backoff_step_2_secs.max(1),
            probe_retry_backoff_step_3_secs: cfg.probe.retry_backoff_step_3_secs.max(1),
            backoff_jitter_percent: cfg.probe.backoff_jitter_percent.min(95),
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
    fn from(id: u32) -> Self { ProviderIdType::Id(id) }
}

impl From<&str> for ProviderIdType {
    fn from(s: &str) -> Self { ProviderIdType::Text(Arc::from(s)) }
}

impl From<String> for ProviderIdType {
    fn from(s: String) -> Self { ProviderIdType::Text(Arc::from(s.as_str())) }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpdateTask {
    ResolveVod {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        source_last_modified: Option<u64>,
    },
    ResolveSeries {
        id: ProviderIdType,
        reason: ResolveReasonSet,
        delay: u16,
        source_last_modified: Option<u64>,
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
            UpdateTask::ResolveVod { id, reason, delay, .. } => {
                write!(f, "Resolve VOD {id} (Reason: {reason}, Delay: {delay}sec)")
            }
            UpdateTask::ResolveSeries { id, reason, delay, .. } => {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScopedTaskKey {
    input_name: Arc<str>,
    task_key: TaskKey,
}

impl ScopedTaskKey {
    fn new(input_name: Arc<str>, task_key: TaskKey) -> Self { Self { input_name, task_key } }
}

#[derive(Debug, Clone)]
struct RetryState {
    attempts: u8,
    next_allowed_at_ts: i64,
    cooldown_until_ts: Option<i64>,
    last_error: Option<String>,
    source_last_modified: Option<u64>,
}

impl RetryState {
    fn new() -> Self {
        Self {
            attempts: 0,
            next_allowed_at_ts: 0,
            cooldown_until_ts: None,
            last_error: None,
            source_last_modified: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDomain {
    Resolve,
    Probe,
    Tmdb,
}

#[derive(Debug, Clone, Default)]
struct TaskRetryState {
    resolve: Option<RetryState>,
    probe: Option<RetryState>,
    tmdb: Option<RetryState>,
    updated_at_ts: i64,
}

impl TaskRetryState {
    fn is_empty(&self) -> bool { self.resolve.is_none() && self.probe.is_none() && self.tmdb.is_none() }

    fn touch(&mut self, now_ts: i64) { self.updated_at_ts = now_ts.max(1); }

    fn max_domain_timestamp(&self) -> i64 {
        let domain_max = |state: &RetryState| state.next_allowed_at_ts.max(state.cooldown_until_ts.unwrap_or(0));
        self.resolve
            .as_ref()
            .map_or(0, domain_max)
            .max(self.probe.as_ref().map_or(0, domain_max))
            .max(self.tmdb.as_ref().map_or(0, domain_max))
    }

    fn is_stale(&self, now_ts: i64, ttl_secs: i64) -> bool {
        let ttl_secs = ttl_secs.max(1);
        let anchor_ts = self.updated_at_ts.max(self.max_domain_timestamp());
        now_ts >= anchor_ts.saturating_add(ttl_secs)
    }

    fn get(&self, domain: RetryDomain) -> Option<&RetryState> {
        match domain {
            RetryDomain::Resolve => self.resolve.as_ref(),
            RetryDomain::Probe => self.probe.as_ref(),
            RetryDomain::Tmdb => self.tmdb.as_ref(),
        }
    }

    fn get_mut_or_insert(&mut self, domain: RetryDomain) -> &mut RetryState {
        let slot = match domain {
            RetryDomain::Resolve => &mut self.resolve,
            RetryDomain::Probe => &mut self.probe,
            RetryDomain::Tmdb => &mut self.tmdb,
        };
        slot.get_or_insert_with(RetryState::new)
    }

    fn clear_domain(&mut self, domain: RetryDomain) {
        match domain {
            RetryDomain::Resolve => self.resolve = None,
            RetryDomain::Probe => self.probe = None,
            RetryDomain::Tmdb => self.tmdb = None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetadataRetryDbKey {
    VodId(u32),
    VodText(String),
    SeriesId(u32),
    SeriesText(String),
    LiveId(u32),
    LiveText(String),
    Stream { scope: String, id: String },
}

impl MetadataRetryDbKey {
    fn from_task_key(task_key: &TaskKey) -> Self {
        match task_key {
            TaskKey::Vod(id) => Self::VodId(*id),
            TaskKey::VodStr(id) => Self::VodText(id.as_ref().to_owned()),
            TaskKey::Series(id) => Self::SeriesId(*id),
            TaskKey::SeriesStr(id) => Self::SeriesText(id.as_ref().to_owned()),
            TaskKey::Live(id) => Self::LiveId(*id),
            TaskKey::LiveStr(id) => Self::LiveText(id.as_ref().to_owned()),
            TaskKey::Stream { scope, id } => {
                Self::Stream { scope: scope.as_ref().to_owned(), id: id.as_ref().to_owned() }
            }
        }
    }

    fn into_task_key(self) -> TaskKey {
        match self {
            Self::VodId(id) => TaskKey::Vod(id),
            Self::VodText(id) => TaskKey::VodStr(Arc::from(id)),
            Self::SeriesId(id) => TaskKey::Series(id),
            Self::SeriesText(id) => TaskKey::SeriesStr(Arc::from(id)),
            Self::LiveId(id) => TaskKey::Live(id),
            Self::LiveText(id) => TaskKey::LiveStr(Arc::from(id)),
            Self::Stream { scope, id } => TaskKey::Stream { scope: Arc::from(scope), id: Arc::from(id) },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetryStateDbValue {
    attempts: u8,
    next_allowed_at_ts: i64,
    cooldown_until_ts: Option<i64>,
    last_error: Option<String>,
    source_last_modified: Option<u64>,
}

impl RetryStateDbValue {
    fn from_retry_state(state: &RetryState) -> Self {
        Self {
            attempts: state.attempts,
            next_allowed_at_ts: state.next_allowed_at_ts,
            cooldown_until_ts: state.cooldown_until_ts,
            last_error: state.last_error.clone(),
            source_last_modified: state.source_last_modified,
        }
    }

    fn into_retry_state(self) -> Option<RetryState> {
        if self.attempts == 0
            && self.next_allowed_at_ts <= 0
            && self.cooldown_until_ts.is_none()
            && self.source_last_modified.is_none()
        {
            return None;
        }
        Some(RetryState {
            attempts: self.attempts,
            next_allowed_at_ts: self.next_allowed_at_ts,
            cooldown_until_ts: self.cooldown_until_ts,
            last_error: self.last_error,
            source_last_modified: self.source_last_modified,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataRetryDbValue {
    resolve: Option<RetryStateDbValue>,
    probe: Option<RetryStateDbValue>,
    tmdb: Option<RetryStateDbValue>,
    updated_at_ts: i64,
}

impl MetadataRetryDbValue {
    fn from_task_retry_state(state: &TaskRetryState, updated_at_ts: i64) -> Self {
        Self {
            resolve: state.resolve.as_ref().map(RetryStateDbValue::from_retry_state),
            probe: state.probe.as_ref().map(RetryStateDbValue::from_retry_state),
            tmdb: state.tmdb.as_ref().map(RetryStateDbValue::from_retry_state),
            updated_at_ts,
        }
    }

    fn into_task_retry_state(self) -> Option<TaskRetryState> {
        let mut state = TaskRetryState {
            resolve: self.resolve.and_then(RetryStateDbValue::into_retry_state),
            probe: self.probe.and_then(RetryStateDbValue::into_retry_state),
            tmdb: self.tmdb.and_then(RetryStateDbValue::into_retry_state),
            updated_at_ts: self.updated_at_ts,
        };
        if state.is_empty() {
            return None;
        }
        if state.updated_at_ts <= 0 {
            state.updated_at_ts = state.max_domain_timestamp();
        }
        Some(state)
    }
}

fn ensure_metadata_retry_db(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut tree = BPlusTree::<MetadataRetryDbKey, MetadataRetryDbValue>::new();
    tree.store(path).map(|_| ())
}

fn load_metadata_retry_states_from_disk(path: &Path) -> io::Result<HashMap<TaskKey, TaskRetryState>> {
    ensure_metadata_retry_db(path)?;

    let mut result = HashMap::new();
    let mut stale_keys: Vec<MetadataRetryDbKey> = Vec::new();
    let mut query = BPlusTreeQuery::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new(path)?;
    for (key, value) in query.iter() {
        if let Some(state) = value.clone().into_task_retry_state() {
            result.insert(key.into_task_key(), state);
        } else {
            stale_keys.push(key);
        }
    }
    drop(query);

    if !stale_keys.is_empty() {
        let delete_refs: Vec<&MetadataRetryDbKey> = stale_keys.iter().collect();
        let mut update = BPlusTreeUpdate::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new_with_backoff(path)?;
        update
            .delete_batch(&delete_refs)
            .map_err(|e| io::Error::other(format!("cleanup metadata retry tombstones failed: {e}")))?;
    }

    Ok(result)
}

fn persist_metadata_retry_state_to_disk(
    path: &Path,
    task_key: &TaskKey,
    state: Option<&TaskRetryState>,
) -> io::Result<()> {
    let db_key = MetadataRetryDbKey::from_task_key(task_key);

    ensure_metadata_retry_db(path)?;

    let mut update = BPlusTreeUpdate::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new_with_backoff(path)?;
    if let Some(retry_state) = state {
        let now_ts = chrono::Utc::now().timestamp();
        let value = MetadataRetryDbValue::from_task_retry_state(retry_state, now_ts);
        update
            .upsert_batch(&[(&db_key, &value)])
            .map_err(|e| io::Error::other(format!("persist metadata retry state failed: {e}")))?;
    } else {
        update.delete(&db_key).map_err(|e| io::Error::other(format!("delete metadata retry state failed: {e}")))?;
    }
    Ok(())
}

/// Per-input worker context. Each input has its own worker
/// that processes tasks sequentially with rate limiting.
#[derive(Clone)]
struct InputWorkerContext {
    worker_id: u64,
    sender: mpsc::Sender<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    pending_task_count: Arc<AtomicUsize>,
}

struct PendingTask {
    task: ParkingMutex<UpdateTask>,
    generation: AtomicU64,
}

impl PendingTask {
    fn new(task: UpdateTask) -> Self { Self { task: ParkingMutex::new(task), generation: AtomicU64::new(0) } }
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
    /// Synchronizes cancellation token rotation with worker context creation.
    worker_lifecycle_lock: ParkingMutex<()>,
    /// Terminal shutdown flag; once set, token rotation must not reactivate workers.
    is_shutdown_flag: AtomicBool,
    /// Global application state (weak reference to avoid cycles)
    app_state: tokio::sync::Mutex<Option<Weak<AppState>>>,
    /// Global gate:
    /// - Foreground playlist updates hold WRITE lock.
    /// - Background metadata/probe tasks hold READ lock per task.
    ///   This guarantees that no background task runs while an update is active.
    update_pause_gate: Arc<RwLock<()>>,
    /// Global cancellation token for shutdown
    cancel_token: ArcSwap<CancellationToken>,
    /// Monotonic worker generation id used to avoid removing a newly spawned worker context.
    next_worker_id: AtomicU64,
    /// Producer-visible view of active resolve cooldowns so repeated playlist refreshes
    /// can skip creating tasks that are still suppressed anyway.
    resolve_enqueue_suppressions: Arc<DashMap<ScopedTaskKey, i64>>,
    /// Producer-visible TMDB/date suppression keyed by the last seen series `last_modified`.
    tmdb_source_markers: Arc<DashMap<ScopedTaskKey, u64>>,
    /// Tracks inputs for which producer-side retry suppression state was already loaded from disk.
    enqueue_state_loaded_inputs: Arc<DashMap<Arc<str>, ()>>,
    /// Backoff for failed producer-side retry-state loads to avoid repeated blocking disk reads.
    enqueue_state_load_retry_at_ts: Arc<DashMap<Arc<str>, i64>>,
    /// Periodic prune marker for stale producer-side resolve suppression entries.
    last_resolve_enqueue_suppression_prune_at_ts: AtomicI64,
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
            worker_lifecycle_lock: ParkingMutex::new(()),
            is_shutdown_flag: AtomicBool::new(false),
            app_state: tokio::sync::Mutex::new(None),
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

    /// Requests graceful shutdown for all metadata workers and delayed requeue tasks.
    /// This operation is idempotent.
    pub fn shutdown(&self) {
        let _guard = self.worker_lifecycle_lock.lock();
        self.is_shutdown_flag.store(true, Ordering::Release);
        self.cancel_token.load_full().cancel();
        self.resolve_enqueue_suppressions.clear();
        self.tmdb_source_markers.clear();
        self.enqueue_state_loaded_inputs.clear();
        self.enqueue_state_load_retry_at_ts.clear();
    }

    /// Returns true once shutdown has been requested.
    pub fn is_shutdown(&self) -> bool { self.is_shutdown_flag.load(Ordering::Acquire) }

    /// Rotates the cancellation token for metadata workers.
    /// Existing workers are cancelled and removed so new tasks start with fresh runtime state.
    pub fn rotate_cancel_token(&self, cancel_token: CancellationToken) {
        let old_token = {
            let _guard = self.worker_lifecycle_lock.lock();
            if self.is_shutdown_flag.load(Ordering::Acquire) {
                self.workers.clear();
                cancel_token.cancel();
                return;
            }

            let old_token = self.cancel_token.swap(Arc::new(cancel_token));
            self.workers.clear();
            self.resolve_enqueue_suppressions.clear();
            self.tmdb_source_markers.clear();
            self.enqueue_state_loaded_inputs.clear();
            self.enqueue_state_load_retry_at_ts.clear();
            old_token
        };
        old_token.cancel();
    }

    /// Acquire exclusive gate for a foreground playlist update.
    /// While this guard is held, background workers wait before starting heavy metadata/probe steps.
    pub async fn acquire_update_pause_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.update_pause_gate.clone().write_owned().await
    }

    pub async fn set_app_state(&self, app_state: Weak<AppState>) {
        let mut guard = self.app_state.lock().await;
        *guard = Some(app_state);
    }

    fn scoped_task_key(input_name: &str, task: &UpdateTask) -> ScopedTaskKey {
        ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(task))
    }

    fn has_pending_task(&self, input_name: &str, task: &UpdateTask) -> bool {
        self.workers
            .get(input_name)
            .is_some_and(|ctx| !ctx.sender.is_closed() && ctx.pending_tasks.contains_key(&TaskKey::from_task(task)))
    }

    fn prune_resolve_enqueue_suppressions(&self, now_ts: i64) {
        let last_pruned_at = self.last_resolve_enqueue_suppression_prune_at_ts.load(Ordering::Relaxed);
        if last_pruned_at != 0 && now_ts.saturating_sub(last_pruned_at) < RETRY_STATE_PRUNE_INTERVAL_SECS {
            return;
        }
        self.last_resolve_enqueue_suppression_prune_at_ts.store(now_ts, Ordering::Relaxed);
        self.resolve_enqueue_suppressions
            .retain(|_, suppressed_until_ts| *suppressed_until_ts > now_ts);
    }

    fn should_skip_enqueue_cached(&self, input_name: &str, task: &UpdateTask) -> bool {
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
        if self
            .enqueue_state_load_retry_at_ts
            .get(input_name)
            .is_some_and(|retry_at_ts| now_ts < *retry_at_ts)
        {
            return;
        }

        let app_state_weak = {
            let guard = self.app_state.lock().await;
            guard.clone()
        };
        let runtime_settings = MetadataUpdateRuntimeSettings::from_app_state(app_state_weak.as_ref());
        let Some(app_state) = app_state_weak.and_then(|weak| Weak::upgrade(&weak)) else {
            return;
        };
        let retry_at_ts = now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs);

        let storage_dir = app_state.app_config.config.load().storage_dir.clone();
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
                let suppressed_until_ts = resolve_state
                    .cooldown_until_ts
                    .unwrap_or(resolve_state.next_allowed_at_ts);
                if suppressed_until_ts > now_ts {
                    let scoped_key = ScopedTaskKey::new(input_name_cloned.clone(), task_key.clone());
                    self.resolve_enqueue_suppressions.insert(scoped_key, suppressed_until_ts);
                }
            }
            if let Some(source_last_modified) = state.tmdb.as_ref().and_then(|tmdb_state| tmdb_state.source_last_modified)
            {
                let scoped_key = ScopedTaskKey::new(input_name_cloned.clone(), task_key.clone());
                self.tmdb_source_markers.insert(scoped_key, source_last_modified);
            }
        }

        self.enqueue_state_load_retry_at_ts.remove(input_name);
        self.enqueue_state_loaded_inputs.insert(input_name.clone(), ());
    }

    pub async fn should_skip_enqueue(&self, input_name: Arc<str>, task: &UpdateTask) -> bool {
        self.ensure_enqueue_state_loaded_for_input(&input_name).await;
        self.should_skip_enqueue_cached(input_name.as_ref(), task)
    }

    fn strip_tmdb_reasons_for_enqueue(&self, input_name: &str, task: UpdateTask) -> Option<UpdateTask> {
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
        let app_state_weak = {
            let guard = self.app_state.lock().await;
            guard.clone()
        };
        let runtime_settings = MetadataUpdateRuntimeSettings::from_app_state(app_state_weak.as_ref());
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
                                app_state_weak: app_state_weak.clone(),
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
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
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
    fn decrement_pending_task_count(pending_task_count: &AtomicUsize) {
        // Guard against accidental underflow in edge/error paths.
        let _ = pending_task_count.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| current.checked_sub(1));
    }

    fn merge_task_payload(existing: &mut UpdateTask, task: UpdateTask) -> bool {
        let mut changed = false;
        // Merge logic
        match (existing, task) {
            (
                UpdateTask::ResolveVod { reason: r1, delay: d1, source_last_modified: lm1, .. },
                UpdateTask::ResolveVod { reason: r2, delay: d2, source_last_modified: lm2, .. },
            ) | (
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
                    (None, some @ Some(_)) | (some @ Some(_), None) => some,
                    (None, None) => None,
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

    /// Get the number of active workers (for monitoring/debugging)
    pub fn active_worker_count(&self) -> usize { self.workers.len() }

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

struct DbHandle {
    _guard: FileReadGuard,
    query: Arc<ParkingMutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
}

struct InputWorker {
    input_name: Arc<str>,
    sender: mpsc::Sender<TaskKey>,
    receiver: mpsc::Receiver<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    pending_task_count: Arc<AtomicUsize>,
    app_state_weak: Option<Weak<AppState>>,
    update_pause_gate: Arc<RwLock<()>>,
    cancel_token: CancellationToken,
    batch_buffer: BatchResultCollector,
    db_handles: HashMap<XtreamCluster, DbHandle>,
    failed_clusters: HashSet<XtreamCluster>,
    retry_states: HashMap<TaskKey, TaskRetryState>,
    resolve_exhausted: HashMap<TaskKey, i64>,
    last_cycle_completed_at_ts: Option<i64>,
    metadata_retry_state_path: Option<PathBuf>,
    metadata_retry_loaded: bool,
    metadata_retry_load_retry_at_ts: Option<i64>,
    last_retry_state_prune_at_ts: Option<i64>,
    // Shared with detached delayed requeue tasks spawned in `schedule_requeue_at`.
    // A plain HashMap cannot be moved safely into those `'static` tasks.
    scheduled_requeues: Arc<DashMap<TaskKey, i64>>,
    // Cache of tasks that recently completed with no changes (Ok(None)).
    // Prevents repeated resolution of already-resolved items across playlist refreshes.
    // Stores the reason set so that tasks with new/different reasons are not wrongly skipped.
    recently_completed_no_change: HashMap<TaskKey, (Instant, ResolveReasonSet)>,
    resolve_enqueue_suppressions: Arc<DashMap<ScopedTaskKey, i64>>,
    tmdb_source_markers: Arc<DashMap<ScopedTaskKey, u64>>,
    dirty_retry_state_keys: HashSet<TaskKey>,
}

#[derive(Debug, Clone, Copy)]
struct ProcessTaskOutcome {
    task_changed: bool,
    tmdb_pending: bool,
    probe_pending: bool,
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
        let mut cycle_had_changes = false;
        let mut consecutive_resolve_tasks = 0_usize;

        let input_name = self.input_name.clone();
        let app_state_weak = self.app_state_weak.clone();

        let mut runtime_settings = self.runtime_settings();
        let mut last_runtime_settings_refresh_at = Instant::now();
        self.ensure_metadata_retry_state_loaded(&input_name, app_state_weak.as_ref(), &runtime_settings).await;

        // Keep one prefetched task to minimize channel waits/lock churn.
        let mut next_task: Option<(TaskKey, UpdateTask, u64)> = None;

        loop {
            if last_runtime_settings_refresh_at.elapsed() >= Duration::from_secs(RUNTIME_SETTINGS_REFRESH_INTERVAL_SECS)
            {
                runtime_settings = self.runtime_settings();
                last_runtime_settings_refresh_at = Instant::now();
            }
            let prefer_probe = consecutive_resolve_tasks >= runtime_settings.probe_fairness_resolve_burst;
            let task_data = if let Some(prefetched) = next_task.take() {
                if prefer_probe && Self::retry_domain_for_task(&prefetched.1) == RetryDomain::Resolve {
                    if let Some(probe_task) = self.take_pending_probe_task_snapshot() {
                        next_task = Some(prefetched);
                        Some(probe_task)
                    } else {
                        Some(prefetched)
                    }
                } else {
                    Some(prefetched)
                }
            } else if prefer_probe {
                if let Some(probe_task) = self.take_pending_probe_task_snapshot() {
                    Some(probe_task)
                } else {
                    self.recv_task_fast_or_wait(&runtime_settings).await
                }
            } else {
                self.recv_task_fast_or_wait(&runtime_settings).await
            };

            let Some((current_key, current_task, current_generation)) = task_data else { break };
            if self.cancel_token.is_cancelled() {
                break;
            }
            if !self.metadata_retry_loaded {
                self.ensure_metadata_retry_state_loaded(&input_name, app_state_weak.as_ref(), &runtime_settings).await;
            }
            let now_ts = chrono::Utc::now().timestamp();
            self.prune_retry_tracking_maps_if_needed(now_ts, &runtime_settings).await;

            if !queue_cycle_active {
                // First entry of a new processing cycle.
                queue_cycle_active = true;
                cycle_had_changes = false;
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
                if self.last_cycle_completed_at_ts.is_some_and(|last| {
                    now_ts.saturating_sub(last) >= runtime_settings.resolve_exhaustion_reset_gap_secs
                }) {
                    self.resolve_exhausted.clear();
                }
                debug!("Background metadata update queue has entries for input {input_name}; starting processing");
            }

            let current_retry_domain = Self::retry_domain_for_task(&current_task);
            let delay_secs = current_task.delay();
            let mut schedule_requeue_at_ts: Option<i64> = None;
            let mut remove_current_task = false;
            let mut apply_rate_limit = false;
            let mut metadata_persist_state: Option<Option<TaskRetryState>> = None;
            let mut skip_execution = false;
            let mut task_for_execution = current_task.clone();

            if Self::is_resolve_task(&task_for_execution) && self.resolve_exhausted.contains_key(&current_key) {
                debug!(
                    "[Metadata-Task] Skipping task (resolve exhausted) for input {}: {} (reset window: {}s)",
                    input_name,
                    task_for_execution,
                    runtime_settings.resolve_exhaustion_reset_gap_secs
                );
                self.scheduled_requeues.remove(&current_key);
                remove_current_task = true;
                skip_execution = true;
            }

            // Skip tasks that recently completed with no changes (already resolved in DB).
            // Only skip when the incoming reason set exactly matches the cached reason set.
            if !skip_execution
                && self.should_skip_recent_no_change_task(&current_key, &task_for_execution, &runtime_settings)
            {
                debug!(
                    "[Metadata-Task] Skipping task (recently completed with no changes) for input {input_name}: {task_for_execution}",
                );
                remove_current_task = true;
                skip_execution = true;
            }

            if !skip_execution {
                let mut clear_tmdb_state = false;
                if let Some(state_bundle) = self.retry_states.get(&current_key) {
                    if let Some(tmdb_state) = state_bundle.get(RetryDomain::Tmdb) {
                        let source_unchanged = tmdb_state.source_last_modified.is_some()
                            && tmdb_state.source_last_modified == Self::task_source_last_modified(&task_for_execution);
                        if source_unchanged && Self::task_has_tmdb_reason(&task_for_execution) {
                            if let Some(stripped_task) = Self::strip_tmdb_reasons(&task_for_execution) {
                                debug!(
                                    "[Task] TMDB/date unchanged for input {}: {}, continuing with non-TMDB reasons (last_modified={:?})",
                                    input_name,
                                    task_for_execution,
                                    tmdb_state.source_last_modified
                                );
                                task_for_execution = stripped_task;
                            } else {
                                debug!(
                                    "[Metadata-Task] Skipping task (TMDB/date unresolved and source unchanged) for input {}: {} (last_modified={:?})",
                                    input_name,
                                    task_for_execution,
                                    tmdb_state.source_last_modified
                                );
                                self.scheduled_requeues.remove(&current_key);
                                remove_current_task = true;
                                skip_execution = true;
                            }
                        } else if let Some(cooldown_until_ts) = tmdb_state.cooldown_until_ts {
                            if now_ts < cooldown_until_ts {
                                if let Some(stripped_task) = Self::strip_tmdb_reasons(&task_for_execution) {
                                    debug!(
                                        "[Task] TMDB cooldown active for input {}: {}, continuing with non-TMDB reasons (cooldown_until={}, remaining={}s)",
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        cooldown_until_ts.saturating_sub(now_ts)
                                    );
                                    task_for_execution = stripped_task;
                                } else {
                                    debug!(
                                        "[Metadata-Task] Skipping task (TMDB-only in cooldown) for input {}: {} (cooldown_until={}, remaining={}s)",
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        cooldown_until_ts.saturating_sub(now_ts)
                                    );
                                    self.scheduled_requeues.remove(&current_key);
                                    remove_current_task = true;
                                    skip_execution = true;
                                }
                            } else {
                                clear_tmdb_state = true;
                            }
                        }
                    }
                }

                let active_retry_domain = Self::retry_domain_for_task(&task_for_execution);
                let mut clear_active_retry_state = false;

                if !skip_execution {
                    if let Some(state_bundle) = self.retry_states.get(&current_key) {
                        if let Some(active_state) = state_bundle.get(active_retry_domain) {
                            if let Some(cooldown_until_ts) = active_state.cooldown_until_ts {
                                if now_ts < cooldown_until_ts {
                                    let cooldown_label = if active_retry_domain == RetryDomain::Probe {
                                        "probe"
                                    } else {
                                        "resolve"
                                    };
                                    debug!(
                                        "[Metadata-Task] Skipping task ({} cooldown) for input {}: {} (cooldown_until={}, remaining={}s)",
                                        cooldown_label,
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        cooldown_until_ts.saturating_sub(now_ts)
                                    );
                                    self.scheduled_requeues.remove(&current_key);
                                    remove_current_task = true;
                                    skip_execution = true;
                                } else {
                                    clear_active_retry_state = true;
                                }
                            }

                            if !skip_execution && active_state.next_allowed_at_ts > now_ts {
                                debug!(
                                    "[Task] Deferring task (retry backoff) for input {}: {} (next_allowed_at={}, wait={}s, attempts={})",
                                    input_name,
                                    task_for_execution,
                                    active_state.next_allowed_at_ts,
                                    active_state.next_allowed_at_ts.saturating_sub(now_ts),
                                    active_state.attempts
                                );
                                schedule_requeue_at_ts = Some(active_state.next_allowed_at_ts);
                                skip_execution = true;
                            }
                        }
                    }
                }

                if clear_active_retry_state || clear_tmdb_state {
                    let mut should_remove_retry_entry = false;
                    let mut state_after_clear: Option<TaskRetryState> = None;

                    if let Some(state_bundle) = self.retry_states.get_mut(&current_key) {
                        if clear_active_retry_state {
                            state_bundle.clear_domain(active_retry_domain);
                        }
                        if clear_tmdb_state {
                            state_bundle.clear_domain(RetryDomain::Tmdb);
                        }
                        if state_bundle.is_empty() {
                            should_remove_retry_entry = true;
                        } else {
                            state_bundle.touch(now_ts);
                            state_after_clear = Some(state_bundle.clone());
                        }
                    }

                    if should_remove_retry_entry {
                        self.retry_states.remove(&current_key);
                    }
                    if clear_active_retry_state && active_retry_domain == RetryDomain::Resolve {
                        self.clear_resolve_enqueue_suppression(&current_key);
                    }
                    if clear_tmdb_state {
                        self.clear_tmdb_source_marker(&current_key);
                    }
                    metadata_persist_state = Some(state_after_clear);
                }
            }

            if !skip_execution {
                debug!(
                    "[Task] Executing task for input {}: {} (retry_domain={:?})",
                    input_name,
                    task_for_execution,
                    Self::retry_domain_for_task(&task_for_execution)
                );
                let task_result = {
                    let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                        break;
                    };
                    Self::process_task_static(
                        &input_name,
                        app_state_weak.as_ref(),
                        &task_for_execution,
                        &mut self.batch_buffer,
                        &mut self.db_handles,
                        &mut self.failed_clusters,
                    )
                    .await
                };

                match task_result {
                    Ok(task_outcome) => {
                        if Self::is_vod_task_key(&current_key) {
                            processed_vod_count += 1;
                        } else if Self::is_series_task_key(&current_key) {
                            processed_series_count += 1;
                        }
                        let trigger_playlist_update =
                            Self::should_trigger_playlist_update_for_task(&task_for_execution, task_outcome.task_changed);
                        cycle_had_changes |= trigger_playlist_update;
                        debug!(
                            "[Metadata-Task] Task succeeded for input {input_name}: {task_for_execution} (changed={}, trigger_playlist_update={}, tmdb_pending={}, probe_pending={})",
                            task_outcome.task_changed,
                            trigger_playlist_update,
                            task_outcome.tmdb_pending,
                            task_outcome.probe_pending
                        );

                        let active_retry_domain = Self::retry_domain_for_task(&task_for_execution);
                        let task_has_tmdb_reason = Self::task_has_tmdb_reason(&task_for_execution);
                        let task_source_last_modified = Self::task_source_last_modified(&task_for_execution);
                        let mut should_remove_retry_entry = false;
                        let mut clear_tmdb_marker = false;
                        let mut state_after_success: Option<TaskRetryState> = None;

                        if let Some(state_bundle) = self.retry_states.get_mut(&current_key) {
                            state_bundle.clear_domain(active_retry_domain);
                            if task_has_tmdb_reason {
                                if task_outcome.tmdb_pending {
                                    let cooldown_until_ts = now_ts.saturating_add(runtime_settings.tmdb_cooldown_secs);
                                    let tmdb_state = state_bundle.get_mut_or_insert(RetryDomain::Tmdb);
                                    tmdb_state.attempts = 0;
                                    tmdb_state.next_allowed_at_ts = cooldown_until_ts;
                                    tmdb_state.cooldown_until_ts = Some(cooldown_until_ts);
                                    tmdb_state.last_error =
                                        Some("TMDB lookup completed without matching result".to_string());
                                    tmdb_state.source_last_modified = task_source_last_modified;
                                    debug!(
                                        "[Metadata-Task] TMDB resolve produced no match (existing retry state), entering cooldown for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        runtime_settings.tmdb_cooldown_secs
                                    );
                                } else {
                                    state_bundle.clear_domain(RetryDomain::Tmdb);
                                    clear_tmdb_marker = true;
                                }
                            }

                            if state_bundle.is_empty() {
                                should_remove_retry_entry = true;
                            } else {
                                state_bundle.touch(now_ts);
                                state_after_success = Some(state_bundle.clone());
                            }
                        } else if task_has_tmdb_reason && task_outcome.tmdb_pending {
                            let cooldown_until_ts = now_ts.saturating_add(runtime_settings.tmdb_cooldown_secs);
                            let state_bundle = TaskRetryState {
                                resolve: None,
                                probe: None,
                                tmdb: Some(RetryState {
                                    attempts: 0,
                                    next_allowed_at_ts: cooldown_until_ts,
                                    cooldown_until_ts: Some(cooldown_until_ts),
                                    last_error: Some("TMDB lookup completed without matching result".to_string()),
                                    source_last_modified: task_source_last_modified,
                                }),
                                updated_at_ts: now_ts.max(1),
                            };
                            self.retry_states.insert(current_key.clone(), state_bundle.clone());
                            state_after_success = Some(state_bundle);
                            debug!(
                                "[Metadata-Task] TMDB resolve produced no match (new retry state), entering cooldown for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                input_name,
                                task_for_execution,
                                cooldown_until_ts,
                                runtime_settings.tmdb_cooldown_secs
                            );
                        } else if task_has_tmdb_reason {
                            self.clear_tmdb_source_marker(&current_key);
                        }

                        if should_remove_retry_entry {
                            self.retry_states.remove(&current_key);
                        }
                        if clear_tmdb_marker {
                            self.clear_tmdb_source_marker(&current_key);
                        }
                        if task_outcome.tmdb_pending {
                            if let Some(source_last_modified) = task_source_last_modified {
                                self.set_tmdb_source_marker(&current_key, source_last_modified);
                            } else {
                                self.clear_tmdb_source_marker(&current_key);
                            }
                        }
                        if should_remove_retry_entry || state_after_success.is_some() {
                            metadata_persist_state = Some(state_after_success);
                        }

                        self.resolve_exhausted.remove(&current_key);
                        if active_retry_domain == RetryDomain::Resolve {
                            self.clear_resolve_enqueue_suppression(&current_key);
                        }
                        self.scheduled_requeues.remove(&current_key);

                        // Cache tasks that completed with no changes to skip redundant re-resolution.
                        if !task_outcome.task_changed
                            && Self::is_resolve_task(&task_for_execution)
                            && !task_outcome.tmdb_pending
                            && !task_outcome.probe_pending
                        {
                            let reasons = Self::task_reason(&task_for_execution);
                            debug!(
                                "[Task] Caching no-change result for input {}: {} (reasons={}, ttl={}s)",
                                input_name, task_for_execution, reasons, runtime_settings.no_change_cache_ttl_secs
                            );
                            self.recently_completed_no_change.insert(current_key.clone(), (Instant::now(), reasons));
                        } else {
                            self.recently_completed_no_change.remove(&current_key);
                        }

                        if last_progress_log_at.elapsed() >= runtime_settings.progress_log_interval {
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

                        remove_current_task = true;
                        apply_rate_limit = true;
                    }
                    Err(e) => {
                        if Self::is_permanent_not_found_error(&e.message) {
                            debug!(
                                "[Task] Task failed with permanent not-found for input {}: {} (error={})",
                                input_name, task_for_execution, e.message
                            );
                            let retry_domain = Self::retry_domain_for_task(&task_for_execution);
                            self.scheduled_requeues.remove(&current_key);
                            if retry_domain == RetryDomain::Probe {
                                let cooldown_until_ts = now_ts.saturating_add(runtime_settings.probe_cooldown_secs);
                                let state_bundle_after_update = {
                                    let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                    let state = state_bundle.get_mut_or_insert(RetryDomain::Probe);
                                    state.attempts = runtime_settings.max_attempts_probe;
                                    state.next_allowed_at_ts = cooldown_until_ts;
                                    state.cooldown_until_ts = Some(cooldown_until_ts);
                                    state.last_error = Some(e.message.clone());
                                    state_bundle.touch(now_ts);
                                    state_bundle.clone()
                                };
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                remove_current_task = true;
                                debug!(
                                    "[Metadata-Task] Probe task entering cooldown after permanent not-found for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                    input_name,
                                    task_for_execution,
                                    cooldown_until_ts,
                                    runtime_settings.probe_cooldown_secs
                                );
                            } else {
                                let cooldown_until_ts =
                                    now_ts.saturating_add(runtime_settings.resolve_exhaustion_reset_gap_secs);
                                let state_bundle_after_update = {
                                    let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                    let state = state_bundle.get_mut_or_insert(RetryDomain::Resolve);
                                    state.attempts = runtime_settings.max_attempts_resolve;
                                    state.next_allowed_at_ts = cooldown_until_ts;
                                    state.cooldown_until_ts = Some(cooldown_until_ts);
                                    state.last_error = Some(e.message.clone());
                                    state_bundle.touch(now_ts);
                                    state_bundle.clone()
                                };
                                self.set_resolve_enqueue_suppression(&current_key, cooldown_until_ts);
                                self.resolve_exhausted.insert(current_key.clone(), now_ts);
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                remove_current_task = true;
                                debug!(
                                    "[Metadata-Task] Resolve task entering cooldown after permanent not-found for input {}: {} (cooldown_until={}, cooldown_duration={}s, error={})",
                                    input_name,
                                    task_for_execution,
                                    cooldown_until_ts,
                                    runtime_settings.resolve_exhaustion_reset_gap_secs,
                                    e.message
                                );
                            }
                        } else if Self::is_transient_worker_error(&e.message) {
                            if e.message == TASK_ERR_UPDATE_IN_PROGRESS {
                                // Drop cached readers quickly so foreground writer can progress.
                                self.release_db_handles();
                            }

                            let retry_delay_secs =
                                Self::compute_retry_delay_secs(current_task.delay(), &runtime_settings);
                            let retry_delay_i64 = i64::try_from(retry_delay_secs).unwrap_or(i64::MAX);
                            schedule_requeue_at_ts = Some(now_ts.saturating_add(retry_delay_i64));
                            debug!(
                                "[Metadata-Task] Task deferred (transient error) for input {}: {} (retry_in={}s, error={})",
                                input_name,
                                task_for_execution,
                                retry_delay_secs,
                                e.message
                            );
                        } else {
                            let retry_domain = Self::retry_domain_for_task(&task_for_execution);
                            let max_attempts = if retry_domain == RetryDomain::Probe {
                                runtime_settings.max_attempts_probe
                            } else {
                                runtime_settings.max_attempts_resolve
                            };

                            let (state_after_update, state_bundle_after_update) = {
                                let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                let state_after_update = {
                                    let state = state_bundle.get_mut_or_insert(retry_domain);
                                    state.attempts = state.attempts.saturating_add(1);
                                    state.last_error = Some(e.message.clone());

                                    if state.attempts < max_attempts {
                                        let backoff_secs = if retry_domain == RetryDomain::Probe {
                                            Self::compute_probe_retry_backoff_secs(state.attempts, &runtime_settings)
                                        } else {
                                            Self::compute_resolve_retry_backoff_secs(
                                                task_for_execution.delay(),
                                                state.attempts,
                                                &runtime_settings,
                                            )
                                        };
                                        let backoff_i64 = i64::try_from(backoff_secs).unwrap_or(i64::MAX);
                                        state.next_allowed_at_ts = now_ts.saturating_add(backoff_i64);
                                        state.cooldown_until_ts = None;
                                    } else if retry_domain == RetryDomain::Probe {
                                        state.cooldown_until_ts =
                                            Some(now_ts.saturating_add(runtime_settings.probe_cooldown_secs));
                                        state.next_allowed_at_ts = state.cooldown_until_ts.unwrap_or(now_ts);
                                    }
                                    state.clone()
                                };
                                state_bundle.touch(now_ts);

                                (state_after_update, state_bundle.clone())
                            };

                            let attempts = state_after_update.attempts;

                            if attempts >= max_attempts {
                                self.scheduled_requeues.remove(&current_key);
                                if retry_domain == RetryDomain::Probe {
                                    remove_current_task = true;
                                    metadata_persist_state = Some(Some(state_bundle_after_update.clone()));
                                    let cooldown_until = state_after_update
                                        .cooldown_until_ts
                                        .map_or_else(|| "none".to_string(), |ts| ts.to_string());
                                    debug!(
                                        "Metadata-[Task] Probe task exhausted (max attempts reached) for input {}: {} (attempts={}/{}, cooldown_until={}, error={})",
                                        input_name,
                                        task_for_execution,
                                        state_after_update.attempts,
                                        max_attempts,
                                        cooldown_until,
                                        e.message
                                    );
                                } else {
                                    let cooldown_until_ts =
                                        now_ts.saturating_add(runtime_settings.resolve_exhaustion_reset_gap_secs);
                                    let state_bundle_after_update = {
                                        let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                        let state = state_bundle.get_mut_or_insert(RetryDomain::Resolve);
                                        state.attempts = max_attempts;
                                        state.next_allowed_at_ts = cooldown_until_ts;
                                        state.cooldown_until_ts = Some(cooldown_until_ts);
                                        state.last_error = Some(e.message.clone());
                                        state_bundle.touch(now_ts);
                                        state_bundle.clone()
                                    };
                                    self.set_resolve_enqueue_suppression(&current_key, cooldown_until_ts);
                                    self.resolve_exhausted.insert(current_key.clone(), now_ts);
                                    metadata_persist_state = Some(Some(state_bundle_after_update));
                                    remove_current_task = true;
                                    debug!(
                                        "[Metadata-Task] Resolve task exhausted (max attempts reached) for input {}: {} (attempts={}/{}, cooldown_duration={}s, error={})",
                                        input_name,
                                        task_for_execution,
                                        attempts,
                                        max_attempts,
                                        runtime_settings.resolve_exhaustion_reset_gap_secs,
                                        e.message
                                    );
                                }
                            } else {
                                schedule_requeue_at_ts = Some(state_after_update.next_allowed_at_ts);
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                if retry_domain == RetryDomain::Resolve {
                                    self.set_resolve_enqueue_suppression(
                                        &current_key,
                                        state_after_update.next_allowed_at_ts,
                                    );
                                }
                                debug!(
                                    "[Metadata-Task] Task failed, scheduling retry for input {}: {} (attempt={}/{}, next_allowed_at={}, backoff={}s, error={})",
                                    input_name,
                                    task_for_execution,
                                    attempts,
                                    max_attempts,
                                    state_after_update.next_allowed_at_ts,
                                    state_after_update.next_allowed_at_ts.saturating_sub(now_ts),
                                    e.message
                                );
                            }
                        }
                    }
                }
            }

            if let Some(state) = metadata_persist_state {
                self.dirty_retry_state_keys.insert(current_key.clone());
                self.persist_metadata_retry_state(&current_key, state.as_ref()).await;
            }

            // Check and flush batch
            if self.batch_buffer.should_flush() {
                self.release_db_handles();
                let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                    break;
                };
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if let Some(retry_at_ts) = schedule_requeue_at_ts {
                debug!(
                    "[Task] Scheduling requeue for input {}: {:?} (retry_at_ts={}, in={}s)",
                    input_name,
                    current_key,
                    retry_at_ts,
                    retry_at_ts.saturating_sub(chrono::Utc::now().timestamp())
                );
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

            if current_retry_domain == RetryDomain::Resolve {
                consecutive_resolve_tasks = consecutive_resolve_tasks.saturating_add(1);
            } else {
                consecutive_resolve_tasks = 0;
            }

            // Try to get the next task immediately to keep locks open.
            // Ignore phantom signals (channel key without pending map entry).
            if next_task.is_none() {
                if self.cancel_token.is_cancelled() {
                    break;
                }
                while let Ok(key) = self.receiver.try_recv() {
                    if self.cancel_token.is_cancelled() {
                        break;
                    }
                    if let Some(snapshot) = self.load_task_snapshot(key) {
                        next_task = Some(snapshot);
                        break;
                    }
                }
            }

            let channel_has_work = next_task.is_some() || !self.receiver.is_empty();
            let queue_completely_empty =
                !channel_has_work && self.pending_tasks.is_empty() && self.scheduled_requeues.is_empty();

            // Avoid O(n) queue scans per task; report queue status periodically.
            if (channel_has_work || !self.pending_tasks.is_empty())
                && last_queue_log_at.elapsed() >= runtime_settings.queue_log_interval
            {
                let queue_counts = Self::queue_resolve_counts(&self.pending_tasks);
                debug!("In queue to resolve vod: {}, series: {} (input: {input_name})", queue_counts.0, queue_counts.1);
                last_queue_log_at = Instant::now();
            }

            // If no immediate work is available, flush buffered results now even when delayed retries remain pending.
            if !channel_has_work && !self.batch_buffer.is_empty() {
                self.release_db_handles();
                let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                    break;
                };
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if queue_cycle_active && queue_completely_empty {
                self.last_cycle_completed_at_ts = Some(chrono::Utc::now().timestamp());
                queue_cycle_active = false;
                processed_vod_count = 0;
                processed_series_count = 0;
                consecutive_resolve_tasks = 0;
                if cycle_had_changes {
                    info!("All pending metadata resolves completed for input {input_name} (with changes)");
                    if let Some(app_state) = app_state_weak.as_ref().and_then(Weak::upgrade) {
                        app_state.event_manager.send_event(EventMessage::InputMetadataUpdatesCompleted(input_name.clone()));
                    }
                } else {
                    debug!("All pending metadata resolves completed for input {input_name} (no changes, skipping playlist update trigger)");
                }
                cycle_had_changes = false;
            }
        }

        // Final flush
        self.release_db_handles();
        if !self.batch_buffer.is_empty() {
            if let Some(_pause_guard) = self.wait_for_update_pause_window().await {
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }
        }

        debug!("Metadata worker stopped for input {input_name}");
    }

    async fn ensure_metadata_retry_state_loaded(
        &mut self,
        input_name: &str,
        app_state_weak: Option<&Weak<AppState>>,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) {
        if self.metadata_retry_loaded {
            return;
        }
        let now_ts = chrono::Utc::now().timestamp();
        if self.metadata_retry_load_retry_at_ts.is_some_and(|retry_at_ts| now_ts < retry_at_ts) {
            return;
        }

        let Some(app_state) = app_state_weak.and_then(Weak::upgrade) else {
            self.metadata_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
            return;
        };

        let storage_dir = app_state.app_config.config.load().storage_dir.clone();
        let Ok(storage_path) = get_input_storage_path(input_name, &storage_dir).await else {
            warn!("Could not resolve storage path for metadata retry state on input {input_name}");
            self.metadata_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
            return;
        };

        let retry_path = storage_path.join(METADATA_RETRY_STATE_FILE);
        self.metadata_retry_state_path = Some(retry_path.clone());

        let loaded = spawn_blocking_limited(move || load_metadata_retry_states_from_disk(&retry_path))
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));

        let loaded = match loaded {
            Ok(states) => states,
            Err(err) => {
                warn!("Failed to load metadata retry state for input {input_name}: {err}");
                self.metadata_retry_load_retry_at_ts =
                    Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
                return;
            }
        };

        // Intentionally do not resurrect pending tasks solely from persisted retry state.
        // The persisted state is applied once the corresponding task is naturally queued again
        // (for example by the next playlist update), because state alone does not contain the
        // full `UpdateTask` payload for all variants.
        for (key, state) in loaded {
            self.sync_resolve_enqueue_suppression_from_retry_state(&key, &state, now_ts);
            self.retry_states.insert(key, state);
        }
        self.prune_retry_tracking_maps(now_ts, runtime_settings).await;
        self.last_retry_state_prune_at_ts = Some(now_ts);
        self.metadata_retry_loaded = true;
        self.metadata_retry_load_retry_at_ts = None;
    }

    async fn persist_metadata_retry_state(&mut self, key: &TaskKey, state: Option<&TaskRetryState>) {
        let Some(path) = self.metadata_retry_state_path.clone() else {
            return;
        };
        if !self.dirty_retry_state_keys.contains(key) {
            return;
        }

        let key_for_persist = key.clone();
        let key_for_dirty = key.clone();
        let state = state.cloned();
        let input_name = self.input_name.clone();
        let persist_result =
            spawn_blocking_limited(move || persist_metadata_retry_state_to_disk(&path, &key_for_persist, state.as_ref()))
                .await;

        match persist_result {
            Ok(Ok(())) => {
                self.dirty_retry_state_keys.remove(&key_for_dirty);
            }
            Ok(Err(err)) => warn!("Failed to persist metadata retry state for input {input_name}: {err}"),
            Err(err) => warn!("Failed to persist metadata retry state for input {input_name}: {err}"),
        }
    }

    fn schedule_requeue_at(&self, key: TaskKey, retry_at_ts: i64) {
        let now_ts = chrono::Utc::now().timestamp();
        let retry_at_ts = retry_at_ts.max(now_ts);

        if self.scheduled_requeues.get(&key).is_some_and(|existing| *existing == retry_at_ts) {
            return;
        }

        self.scheduled_requeues.insert(key.clone(), retry_at_ts);

        let sender = self.sender.clone();
        let pending_tasks = Arc::clone(&self.pending_tasks);
        let pending_task_count = Arc::clone(&self.pending_task_count);
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

            let should_send = scheduled.get(&key).is_some_and(|scheduled_at| *scheduled_at == retry_at_ts);
            if !should_send {
                return;
            }
            scheduled.remove(&key);

            if !pending_tasks.contains_key(&key) {
                return;
            }

            if sender.send(key.clone()).await.is_err() {
                if pending_tasks.remove(&key).is_some() {
                    MetadataUpdateManager::decrement_pending_task_count(&pending_task_count);
                }
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
                    if self.pending_tasks.remove(current_key).is_some() {
                        MetadataUpdateManager::decrement_pending_task_count(&self.pending_task_count);
                    }
                    warn!("Failed to schedule merged task replay for input {input_name}");
                    return false;
                }
                return true;
            }
            // Task finished and is not reinserted.
            MetadataUpdateManager::decrement_pending_task_count(&self.pending_task_count);
        }
        false
    }

    async fn recv_task_fast_or_wait(
        &mut self,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> Option<(TaskKey, UpdateTask, u64)> {
        if self.cancel_token.is_cancelled() {
            return None;
        }

        // Fast path: drain immediate signals until we find a real pending task.
        loop {
            if self.cancel_token.is_cancelled() {
                return None;
            }
            match self.receiver.try_recv() {
                Ok(key) => {
                    if self.cancel_token.is_cancelled() {
                        return None;
                    }
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
                            if self.cancel_token.is_cancelled() {
                                return None;
                            }
                            if let Some(snapshot) = self.load_task_snapshot(key) {
                                return Some(snapshot);
                            }
                        }
                        Ok(None) => return None,
                        Err(_) => {
                            loop {
                                if self.cancel_token.is_cancelled() {
                                    return None;
                                }
                                match self.receiver.try_recv() {
                                    Ok(key) => {
                                        if self.cancel_token.is_cancelled() {
                                            return None;
                                        }
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

    fn take_pending_probe_task_snapshot(&self) -> Option<(TaskKey, UpdateTask, u64)> {
        for entry in self.pending_tasks.iter() {
            if self.scheduled_requeues.contains_key(entry.key()) {
                continue;
            }

            let generation = entry.generation.load(Ordering::Relaxed);
            let task = entry.task.lock().clone();
            if Self::retry_domain_for_task(&task) == RetryDomain::Probe {
                return Some((entry.key().clone(), task, generation));
            }
        }

        None
    }

    fn runtime_settings(&self) -> MetadataUpdateRuntimeSettings {
        MetadataUpdateRuntimeSettings::from_app_state(self.app_state_weak.as_ref())
    }

    async fn wait_for_update_pause_window(&mut self) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        if let Ok(guard) = self.update_pause_gate.clone().try_read_owned() {
            return Some(guard);
        }

        // Foreground writer is active or queued. Release cached handles before waiting to avoid AB-BA patterns.
        self.release_db_handles();
        tokio::select! {
            () = self.cancel_token.cancelled() => None,
            guard = self.update_pause_gate.clone().read_owned() => {
                Some(guard)
            }
        }
    }

    fn retry_state_ttl_secs(runtime_settings: &MetadataUpdateRuntimeSettings) -> i64 {
        let max_resolve_backoff = i64::try_from(runtime_settings.max_resolve_retry_backoff_secs).unwrap_or(i64::MAX);
        runtime_settings
            .tmdb_cooldown_secs
            .max(runtime_settings.probe_cooldown_secs)
            .max(runtime_settings.resolve_exhaustion_reset_gap_secs)
            .max(max_resolve_backoff)
            .saturating_mul(6)
            .max(RETRY_STATE_MIN_TTL_SECS)
    }

    fn resolve_exhausted_ttl_secs(runtime_settings: &MetadataUpdateRuntimeSettings) -> i64 {
        runtime_settings.resolve_exhaustion_reset_gap_secs.saturating_mul(6).max(RETRY_STATE_MIN_TTL_SECS)
    }

    async fn prune_retry_tracking_maps_if_needed(
        &mut self,
        now_ts: i64,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) {
        let due = self
            .last_retry_state_prune_at_ts
            .is_none_or(|last| now_ts.saturating_sub(last) >= RETRY_STATE_PRUNE_INTERVAL_SECS);
        if !due {
            return;
        }
        self.last_retry_state_prune_at_ts = Some(now_ts);
        self.prune_retry_tracking_maps(now_ts, runtime_settings).await;
    }

    async fn prune_retry_tracking_maps(&mut self, now_ts: i64, runtime_settings: &MetadataUpdateRuntimeSettings) {
        let retry_state_ttl_secs = Self::retry_state_ttl_secs(runtime_settings);
        let resolve_exhausted_ttl_secs = Self::resolve_exhausted_ttl_secs(runtime_settings);
        let stale_retry_keys: Vec<TaskKey> = self
            .retry_states
            .iter()
            .filter_map(
                |(key, state)| {
                    if state.is_stale(now_ts, retry_state_ttl_secs) {
                        Some(key.clone())
                    } else {
                        None
                    }
                },
            )
            .collect();

        for key in stale_retry_keys {
            if self.retry_states.remove(&key).is_some() {
                self.clear_resolve_enqueue_suppression(&key);
                self.clear_tmdb_source_marker(&key);
                self.dirty_retry_state_keys.insert(key.clone());
                debug_if_enabled!("Pruned stale metadata retry state for input {}: {:?}", self.input_name, key);
                self.persist_metadata_retry_state(&key, None).await;
            }
        }

        let stale_resolve_exhausted_keys: Vec<TaskKey> = self
            .resolve_exhausted
            .iter()
            .filter_map(|(key, exhausted_at_ts)| {
                if now_ts.saturating_sub(*exhausted_at_ts) >= resolve_exhausted_ttl_secs {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();
        for key in stale_resolve_exhausted_keys {
            self.resolve_exhausted.remove(&key);
            self.clear_resolve_enqueue_suppression(&key);
        }

        let no_change_ttl = Duration::from_secs(runtime_settings.no_change_cache_ttl_secs);
        self.recently_completed_no_change
            .retain(|_, (completed_at, _)| completed_at.elapsed() < no_change_ttl);
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
        let without_jitter =
            base_delay.saturating_mul(2_u64.saturating_pow(exp)).min(runtime_settings.max_resolve_retry_backoff_secs);
        Self::apply_jitter(without_jitter, runtime_settings.backoff_jitter_percent)
    }

    fn compute_retry_delay_secs(base_delay_secs: u16, runtime_settings: &MetadataUpdateRuntimeSettings) -> u64 {
        u64::from(base_delay_secs).max(runtime_settings.retry_delay_secs)
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
    fn is_vod_task_key(key: &TaskKey) -> bool { matches!(key, TaskKey::Vod(_) | TaskKey::VodStr(_)) }

    #[inline]
    fn is_series_task_key(key: &TaskKey) -> bool { matches!(key, TaskKey::Series(_) | TaskKey::SeriesStr(_)) }

    #[inline]
    fn is_probe_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ProbeLive { .. } | UpdateTask::ProbeStream { .. })
    }

    #[inline]
    fn is_probe_only_resolve_task(task: &UpdateTask) -> bool {
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
    fn retry_domain_for_task(task: &UpdateTask) -> RetryDomain {
        if Self::is_probe_task(task) || Self::is_probe_only_resolve_task(task) {
            RetryDomain::Probe
        } else {
            RetryDomain::Resolve
        }
    }

    #[inline]
    fn is_resolve_task(task: &UpdateTask) -> bool {
        matches!(task, UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. })
    }

    #[inline]
    fn task_reason(task: &UpdateTask) -> ResolveReasonSet {
        match task {
            UpdateTask::ResolveVod { reason, .. }
            | UpdateTask::ResolveSeries { reason, .. }
            | UpdateTask::ProbeLive { reason, .. }
            | UpdateTask::ProbeStream { reason, .. } => *reason,
        }
    }

    fn should_skip_recent_no_change_task(
        &mut self,
        current_key: &TaskKey,
        task_for_execution: &UpdateTask,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> bool {
        let Some((completed_at, cached_reasons)) =
            self.recently_completed_no_change.get(current_key).copied()
        else {
            return false;
        };

        let ttl = Duration::from_secs(runtime_settings.no_change_cache_ttl_secs);
        let current_reasons = Self::task_reason(task_for_execution);
        if completed_at.elapsed() < ttl && current_reasons == cached_reasons {
            self.scheduled_requeues.remove(current_key);
            return true;
        }

        self.recently_completed_no_change.remove(current_key);
        false
    }

    fn set_resolve_enqueue_suppression(&self, current_key: &TaskKey, until_ts: i64) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.resolve_enqueue_suppressions.insert(scoped_key, until_ts);
    }

    fn clear_resolve_enqueue_suppression(&self, current_key: &TaskKey) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.resolve_enqueue_suppressions.remove(&scoped_key);
    }

    fn set_tmdb_source_marker(&self, current_key: &TaskKey, source_last_modified: u64) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.tmdb_source_markers.insert(scoped_key, source_last_modified);
    }

    fn clear_tmdb_source_marker(&self, current_key: &TaskKey) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.tmdb_source_markers.remove(&scoped_key);
    }

    fn sync_resolve_enqueue_suppression_from_retry_state(&self, current_key: &TaskKey, state: &TaskRetryState, now_ts: i64) {
        let Some(resolve_state) = state.resolve.as_ref() else {
            self.clear_resolve_enqueue_suppression(current_key);
            return;
        };

        let suppressed_until_ts = resolve_state
            .cooldown_until_ts
            .unwrap_or(resolve_state.next_allowed_at_ts);
        if suppressed_until_ts > now_ts {
            self.set_resolve_enqueue_suppression(current_key, suppressed_until_ts);
        } else {
            self.clear_resolve_enqueue_suppression(current_key);
        }
    }

    #[inline]
    fn should_trigger_playlist_update_for_task(task: &UpdateTask, task_changed: bool) -> bool {
        task_changed && !Self::is_probe_task(task) && !Self::is_probe_only_resolve_task(task)
    }

    #[inline]
    fn task_has_tmdb_reason(task: &UpdateTask) -> bool {
        match task {
            UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date)
            }
            _ => false,
        }
    }

    #[inline]
    fn task_source_last_modified(task: &UpdateTask) -> Option<u64> {
        match task {
            UpdateTask::ResolveVod { source_last_modified, .. } | UpdateTask::ResolveSeries { source_last_modified, .. } => {
                *source_last_modified
            }
            _ => None,
        }
    }

    fn strip_tmdb_reasons(task: &UpdateTask) -> Option<UpdateTask> {
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

    fn is_transient_worker_error(message: &str) -> bool {
        message == TASK_ERR_UPDATE_IN_PROGRESS || message == TASK_ERR_PREEMPTED || message == TASK_ERR_NO_CONNECTION
    }

    #[inline]
    fn is_permanent_not_found_error(message: &str) -> bool {
        let normalized = message.to_ascii_lowercase();
        Self::contains_standalone_fragment(&normalized, "404")
            || Self::contains_standalone_fragment(&normalized, "not found")
    }

    #[inline]
    fn is_word_byte(byte: u8) -> bool { byte.is_ascii_alphanumeric() || byte == b'_' }

    fn contains_standalone_fragment(haystack: &str, fragment: &str) -> bool {
        if fragment.is_empty() || haystack.len() < fragment.len() {
            return false;
        }

        let bytes = haystack.as_bytes();
        let mut search_from = 0usize;

        while let Some(relative_idx) = haystack[search_from..].find(fragment) {
            let start = search_from + relative_idx;
            let end = start + fragment.len();
            let before_is_word = start > 0 && Self::is_word_byte(bytes[start - 1]);
            let after_is_word = end < bytes.len() && Self::is_word_byte(bytes[end]);

            if !before_is_word && !after_is_word {
                return true;
            }
            search_from = end;
        }

        false
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
        let cfg = app_config.config.load();
        let vod_updates = batch_buffer.take_vod_updates();
        let series_updates = batch_buffer.take_series_updates();
        let live_updates = batch_buffer.take_live_updates();

        if vod_updates.is_empty() && series_updates.is_empty() && live_updates.is_empty() {
            return;
        }

        if let Ok(storage_path) = get_input_storage_path(input_name, &cfg.storage_dir).await {
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
                match spawn_blocking_limited(move || TargetIdMapping::new(&mapping_file_clone, false)).await {
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
            match spawn_blocking_limited(move || -> Vec<XtreamPlaylistItem> {
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
            match spawn_blocking_limited(move || -> Vec<XtreamPlaylistItem> {
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
            match spawn_blocking_limited(move || -> Vec<XtreamPlaylistItem> {
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
            let cfg = app_state.app_config.config.load();
            if let Ok(storage_path) = get_input_storage_path(input_name, &cfg.storage_dir).await {
                let file_path = xtream_get_file_path(&storage_path, cluster);
                if file_path.exists() {
                    let lock = app_state.app_config.file_locks.read_lock(&file_path).await;
                    let file_path = file_path.clone();
                    let query = match spawn_blocking_limited(move || {
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
                let item = match spawn_blocking_limited(move || {
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
    ) -> Result<ProcessTaskOutcome, TuliproxError> {
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

        let probe_priority = app_state
            .app_config
            .config
            .load()
            .metadata_update
            .as_ref()
            .map_or(default_probe_user_priority(), |cfg| cfg.probe.user_priority);

        // Reserve provider capacity only for actual probe work (ffprobe paths).
        let provider_handle = if needs_probe_connection {
            let Some(handle) = app_state.active_provider.acquire_connection_for_probe(input_name, probe_priority).await else {
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
        // The hard timeout is applied only to tasks that perform network probing — Probe*
        // variants and Resolve tasks whose reason includes ResolveReason::Probe — because
        // those are the only ones that can stall on an unresponsive provider.  Pure metadata
        // resolve tasks (Info / TMDB / Date only) run without a timeout so they are not
        // subject to the probe hard limit.
        let exec_fut = async {
            if let Some(handle) = provider_handle.as_ref() {
                if let Some(token) = &handle.cancel_token {
                    tokio::select! {
                        biased;

                        () = token.cancelled() => {
                            debug_if_enabled!("Metadata update task preempted by user request for input {}", input_name);
                            Err(shared::error::info_err!("{}", TASK_ERR_PREEMPTED))
                        }

                        res = Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), Some(handle), probe_priority, collector, db_handles, failed_clusters) => {
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
                        probe_priority,
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
                    probe_priority,
                    collector,
                    db_handles,
                    failed_clusters,
                )
                .await
            }
        };
        let needs_probe_timeout = Self::is_probe_task(task)
            || (Self::is_resolve_task(task) && Self::task_reason(task).contains(ResolveReason::Probe));
        let res = if needs_probe_timeout {
            match tokio::time::timeout(Duration::from_secs(PROBE_TASK_TIMEOUT_SECS), exec_fut).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    error!(
                        "Metadata probe task timed out after {PROBE_TASK_TIMEOUT_SECS}s for input {input_name}: {task}; \
                         releasing provider handle and skipping task"
                    );
                    Err(shared::error::info_err!(
                        "Task timed out after {PROBE_TASK_TIMEOUT_SECS}s for input {input_name}: {task}"
                    ))
                }
            }
        } else {
            exec_fut.await
        };

        if provider_handle.is_some() {
            app_state.connection_manager.release_provider_handle(provider_handle).await;
        }
        match res {
            Ok((tmdb_and_date_present, probe_pending)) => {
                let task_changed = match task {
                    UpdateTask::ResolveVod { .. } => collector.vod.len() > pre_vod_updates,
                    UpdateTask::ResolveSeries { .. } => collector.series.len() > pre_series_updates,
                    UpdateTask::ProbeLive { .. } => collector.live.len() > pre_live_updates,
                    UpdateTask::ProbeStream { .. } => true,
                };
                let tmdb_pending = match task {
                    UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                        if reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date) {
                            !tmdb_and_date_present
                        } else {
                            false
                        }
                    }
                    UpdateTask::ProbeLive { .. } | UpdateTask::ProbeStream { .. } => false,
                };
                Ok(ProcessTaskOutcome { task_changed, tmdb_pending, probe_pending })
            }
            Err(e) => Err(e),
        }
    }

    fn task_needs_provider_connection(task: &UpdateTask, input_type: InputType) -> bool {
        match task {
            UpdateTask::ProbeLive { .. } => true,
            // Local library probing is fully local and must not depend on provider capacity.
            UpdateTask::ProbeStream { .. } => !matches!(input_type, InputType::Library),
            // Resolve tasks handle their own probe connection acquisition internally.
            // This avoids holding a provider connection for the entire duration of
            // info fetch + TMDB resolve + probe, reducing "provider exhausted" errors.
            UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. } => false,
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
        probe_priority: i8,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<(bool, bool), TuliproxError> {
        // The returned tuple is `(tmdb_and_date_present, probe_pending)`.
        // `tmdb_and_date_present` avoids false-positive TMDB "no match" cooldowns.
        // `probe_pending` prevents skipped/aborted probes from being cached as no-op.
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

                let tmdb_and_date_present = AtomicBool::new(false);
                let probe_pending = AtomicBool::new(false);
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
                    Some(&tmdb_and_date_present),
                    Some(&probe_pending),
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_vod(id.clone(), props);
                        Ok((
                            tmdb_and_date_present.load(Ordering::Relaxed),
                            probe_pending.load(Ordering::Relaxed),
                        ))
                    }
                    Ok(None) => Ok((
                        tmdb_and_date_present.load(Ordering::Relaxed),
                        probe_pending.load(Ordering::Relaxed),
                    )),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ResolveSeries { id, reason, .. } => {
                let fetch_info = reason.contains(ResolveReason::Info);
                let resolve_tmdb = reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date);
                let will_probe = reason.contains(ResolveReason::Probe);
                let series_probe_settings = {
                    let config = app_state.app_config.config.load();
                    SeriesProbeSettings::from_metadata_update(config.metadata_update.as_ref())
                };

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

                let tmdb_and_date_present = AtomicBool::new(false);
                let probe_pending = AtomicBool::new(false);
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
                    series_probe_settings,
                    query_opt,
                    Some(&tmdb_and_date_present),
                    Some(&probe_pending),
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_series(id.clone(), props);
                        Ok((
                            tmdb_and_date_present.load(Ordering::Relaxed),
                            probe_pending.load(Ordering::Relaxed),
                        ))
                    }
                    Ok(None) => Ok((
                        tmdb_and_date_present.load(Ordering::Relaxed),
                        probe_pending.load(Ordering::Relaxed),
                    )),
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
                        Ok((false, false))
                    }
                    Ok(None) => Ok((false, false)),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeStream { unique_id, url, item_type, .. } => {
                // Generic probe doesn't support batching yet and always takes a WRITE lock.
                // It can target any cluster, so we clear all handles to be safe.
                if !db_handles.is_empty() {
                    db_handles.clear();
                }
                let task_key = TaskKey::from_task(task);
                let probe_identifier = if unique_id.trim().is_empty() { url.as_str() } else { unique_id.as_str() };

                let outcome = update_generic_stream_metadata(
                    &app_state.app_config,
                    input.as_ref(),
                    unique_id,
                    url,
                    *item_type,
                    &app_state.active_provider,
                    active_handle,
                    probe_priority,
                )
                .await?;

                match outcome {
                    GenericProbeOutcome::Updated | GenericProbeOutcome::Noop => Ok((false, false)),
                    GenericProbeOutcome::ProbeFailed => Err(shared::error::info_err!(
                        "Probe stream task failed for key {:?} ({})",
                        task_key,
                        probe_identifier
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

    fn create_test_worker(
        input_name: &str,
        sender: mpsc::Sender<TaskKey>,
        receiver: mpsc::Receiver<TaskKey>,
        pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
        pending_task_count: Arc<AtomicUsize>,
    ) -> InputWorker {
        InputWorker {
            input_name: Arc::from(input_name),
            sender,
            receiver,
            pending_tasks,
            pending_task_count,
            app_state_weak: None,
            update_pause_gate: Arc::new(RwLock::new(())),
            cancel_token: CancellationToken::new(),
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
            resolve_enqueue_suppressions: Arc::new(DashMap::new()),
            tmdb_source_markers: Arc::new(DashMap::new()),
            dirty_retry_state_keys: HashSet::new(),
        }
    }

    #[tokio::test]
    async fn queue_task_creates_single_worker_per_input_under_concurrency() {
        let cancel_token = CancellationToken::new();
        let manager = Arc::new(MetadataUpdateManager::new(cancel_token));
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
                            source_last_modified: None,
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

        manager.shutdown();
    }

    #[test]
    fn should_skip_enqueue_respects_resolve_suppression() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_suppressed";
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 1,
            source_last_modified: None,
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
        manager
            .resolve_enqueue_suppressions
            .insert(scoped_key.clone(), chrono::Utc::now().timestamp().saturating_add(60));

        assert!(manager.should_skip_enqueue_cached(input_name, &task));

        manager
            .resolve_enqueue_suppressions
            .insert(scoped_key.clone(), chrono::Utc::now().timestamp().saturating_sub(1));

        assert!(!manager.should_skip_enqueue_cached(input_name, &task));
        assert!(!manager.resolve_enqueue_suppressions.contains_key(&scoped_key));
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_skips_unchanged_series_tmdb_only_task() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_tmdb_marker";
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Date]),
            delay: 1,
            source_last_modified: Some(777),
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
        manager.tmdb_source_markers.insert(scoped_key, 777);

        assert!(manager.strip_tmdb_reasons_for_enqueue(input_name, task).is_none());
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_keeps_non_tmdb_reasons_for_unchanged_series_source() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_tmdb_probe";
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Probe]),
            delay: 1,
            source_last_modified: Some(777),
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
        manager.tmdb_source_markers.insert(scoped_key, 777);

        let prepared = manager
            .strip_tmdb_reasons_for_enqueue(input_name, task)
            .expect("probe reason should remain");
        match prepared {
            UpdateTask::ResolveSeries { reason, source_last_modified, .. } => {
                assert_eq!(source_last_modified, Some(777));
                assert!(!reason.contains(ResolveReason::Tmdb));
                assert!(!reason.contains(ResolveReason::Date));
                assert!(reason.contains(ResolveReason::Probe));
            }
            other => panic!("unexpected task type after enqueue strip: {other:?}"),
        }
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_keeps_unknown_series_timestamp_and_tmdb_reason() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_tmdb_unknown_series";
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Date]),
            delay: 1,
            source_last_modified: None,
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));

        assert!(!manager.tmdb_source_markers.contains_key(&scoped_key));

        let prepared = manager
            .strip_tmdb_reasons_for_enqueue(input_name, task)
            .expect("unknown timestamps should not be suppressed");
        match prepared {
            UpdateTask::ResolveSeries { reason, source_last_modified, .. } => {
                assert_eq!(source_last_modified, None);
                assert!(reason.contains(ResolveReason::Tmdb));
                assert!(reason.contains(ResolveReason::Date));
            }
            other => panic!("unexpected task type after enqueue strip: {other:?}"),
        }
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_skips_unchanged_vod_tmdb_only_task() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_vod_tmdb_marker";
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Date]),
            delay: 1,
            source_last_modified: Some(777),
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
        manager.tmdb_source_markers.insert(scoped_key, 777);

        assert!(manager.strip_tmdb_reasons_for_enqueue(input_name, task).is_none());
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_keeps_non_tmdb_reasons_for_unchanged_vod_source() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_vod_tmdb_probe";
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Probe]),
            delay: 1,
            source_last_modified: Some(777),
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
        manager.tmdb_source_markers.insert(scoped_key, 777);

        let prepared = manager
            .strip_tmdb_reasons_for_enqueue(input_name, task)
            .expect("probe reason should remain");
        match prepared {
            UpdateTask::ResolveVod { reason, source_last_modified, .. } => {
                assert_eq!(source_last_modified, Some(777));
                assert!(!reason.contains(ResolveReason::Tmdb));
                assert!(!reason.contains(ResolveReason::Date));
                assert!(reason.contains(ResolveReason::Probe));
            }
            other => panic!("unexpected task type after enqueue strip: {other:?}"),
        }
    }

    #[test]
    fn strip_tmdb_reasons_for_enqueue_keeps_unknown_vod_timestamp_and_tmdb_reason() {
        let manager = MetadataUpdateManager::new(CancellationToken::new());
        let input_name = "input_tmdb_unknown_vod";
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Date]),
            delay: 1,
            source_last_modified: None,
        };
        let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));

        assert!(!manager.tmdb_source_markers.contains_key(&scoped_key));

        let prepared = manager
            .strip_tmdb_reasons_for_enqueue(input_name, task)
            .expect("unknown timestamps should not be suppressed");
        match prepared {
            UpdateTask::ResolveVod { reason, source_last_modified, .. } => {
                assert_eq!(source_last_modified, None);
                assert!(reason.contains(ResolveReason::Tmdb));
                assert!(reason.contains(ResolveReason::Date));
            }
            other => panic!("unexpected task type after enqueue strip: {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_task_merges_existing_task_and_increments_generation() {
        let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(0));

        let task_initial = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 10,
            source_last_modified: None,
        };
        let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;
        MetadataUpdateManager::submit_task(
            tx.clone(),
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            task_initial,
        )
        .await;

        let task_merge = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 2,
            source_last_modified: None,
        };
        MetadataUpdateManager::submit_task(
            tx,
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            task_merge,
        )
        .await;

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
    async fn submit_task_identical_resolve_merge_keeps_generation() {
        let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(0));
        let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;

        let initial = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb]),
            delay: 10,
            source_last_modified: None,
        };
        MetadataUpdateManager::submit_task(
            tx.clone(),
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            initial,
        )
        .await;

        let identical_merge = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb]),
            delay: 10,
            source_last_modified: None,
        };
        MetadataUpdateManager::submit_task(
            tx,
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            identical_merge,
        )
        .await;

        let first_signal = rx.try_recv().expect("first signal should be queued");
        assert_eq!(first_signal, TaskKey::Vod(42));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));

        let entry = pending_tasks.get(&TaskKey::Vod(42)).expect("pending entry should exist");
        assert_eq!(entry.generation.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn submit_task_probe_stream_merge_keeps_existing_payload_when_present() {
        let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(0));
        let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;

        let initial = UpdateTask::ProbeStream {
            probe_scope: Arc::from("scope_a"),
            unique_id: "uid_1".to_string(),
            url: "http://old.example/stream".to_string(),
            item_type: PlaylistItemType::Video,
            reason: ResolveReasonSet::from_variants(&[ResolveReason::MissingDetails]),
            delay: 10,
        };
        MetadataUpdateManager::submit_task(
            tx.clone(),
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            initial,
        )
        .await;

        let merged_in = UpdateTask::ProbeStream {
            probe_scope: Arc::from("scope_a"),
            unique_id: "uid_1".to_string(),
            url: "http://new.example/stream".to_string(),
            item_type: PlaylistItemType::LocalVideo,
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 2,
        };
        MetadataUpdateManager::submit_task(
            tx,
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            queue_size,
            merged_in,
        )
        .await;

        let first_signal = rx.try_recv().expect("first signal should be queued");
        assert_eq!(first_signal, TaskKey::Stream { scope: Arc::from("scope_a"), id: Arc::from("uid_1") });
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));

        let key = TaskKey::Stream { scope: Arc::from("scope_a"), id: Arc::from("uid_1") };
        let entry = pending_tasks.get(&key).expect("pending entry should exist");
        assert_eq!(entry.generation.load(Ordering::Relaxed), 1);

        let merged = entry.task.lock().clone();
        match merged {
            UpdateTask::ProbeStream { reason, delay, url, item_type, .. } => {
                assert!(reason.contains(ResolveReason::MissingDetails));
                assert!(reason.contains(ResolveReason::Probe));
                assert_eq!(delay, 2);
                assert_eq!(url, "http://old.example/stream");
                assert_eq!(item_type, PlaylistItemType::Video);
            }
            other => panic!("unexpected task type after merge: {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_task_with_closed_sender_does_not_report_merged_for_existing_pending_entry() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        drop(rx);

        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let key = TaskKey::Vod(42);
        let existing_task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 10,
            source_last_modified: None,
        };
        pending_tasks.insert(key.clone(), PendingTask::new(existing_task));

        let incoming_task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(42),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 2,
            source_last_modified: None,
        };

        let result = MetadataUpdateManager::submit_task(
            tx,
            pending_tasks.clone(),
            pending_task_count.clone(),
            "input_a",
            MetadataUpdateRuntimeSettings::default().max_queue_size,
            incoming_task,
        )
        .await;

        assert_eq!(result, SubmitTaskResult::ChannelClosed);
        assert!(!pending_tasks.contains_key(&key));
        assert_eq!(pending_task_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn finalize_processed_task_success_requeues_when_generation_changed() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let key = TaskKey::Vod(7);

        pending_tasks.insert(
            key.clone(),
            PendingTask::new(UpdateTask::ResolveVod {
                id: ProviderIdType::Id(7),
                reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
                delay: 0,
                source_last_modified: None,
            }),
        );
        if let Some(entry) = pending_tasks.get(&key) {
            entry.generation.store(1, Ordering::Relaxed);
        }

        let mut worker = create_test_worker("input_a", tx, rx, pending_tasks.clone(), pending_task_count);

        let requeued = worker.finalize_processed_task_success(&key, 0, "input_a").await;
        assert!(requeued);
        assert!(pending_tasks.contains_key(&key));
        assert_eq!(worker.receiver.try_recv().expect("requeued signal should be present"), key);
    }

    #[tokio::test]
    async fn finalize_processed_task_success_removes_when_unchanged() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let key = TaskKey::Vod(9);
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(9),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 0,
            source_last_modified: None,
        };

        pending_tasks.insert(key.clone(), PendingTask::new(task.clone()));

        let mut worker = create_test_worker("input_b", tx, rx, pending_tasks.clone(), pending_task_count);
        let runtime_settings = MetadataUpdateRuntimeSettings::default();

        worker.scheduled_requeues.insert(key.clone(), chrono::Utc::now().timestamp().saturating_add(30));
        worker.recently_completed_no_change.insert(
            key.clone(),
            (Instant::now(), ResolveReasonSet::from_variants(&[ResolveReason::Info])),
        );
        assert!(worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
        assert!(worker.recently_completed_no_change.contains_key(&key));
        assert!(!worker.scheduled_requeues.contains_key(&key));

        let requeued = worker.finalize_processed_task_success(&key, 0, "input_b").await;
        assert!(!requeued);
        assert!(!pending_tasks.contains_key(&key));
        assert!(matches!(worker.receiver.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
    }

    #[test]
    fn recent_no_change_skip_requires_exact_reason_match() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(0));
        let key = TaskKey::Vod(10);
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(10),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 0,
            source_last_modified: None,
        };
        let mut worker = create_test_worker("input_c", tx, rx, pending_tasks, pending_task_count);
        let runtime_settings = MetadataUpdateRuntimeSettings::default();

        worker.recently_completed_no_change.insert(
            key.clone(),
            (
                Instant::now(),
                ResolveReasonSet::from_variants(&[ResolveReason::Info, ResolveReason::Probe]),
            ),
        );
        assert!(!worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
        assert!(!worker.recently_completed_no_change.contains_key(&key));

        worker.recently_completed_no_change.insert(
            key.clone(),
            (Instant::now(), ResolveReasonSet::from_variants(&[ResolveReason::Info])),
        );
        assert!(worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
        assert!(worker.recently_completed_no_change.contains_key(&key));
    }

    #[tokio::test]
    async fn recent_no_change_skip_ttl_expiry_allows_requeue() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let key = TaskKey::Vod(11);
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(11),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 0,
            source_last_modified: None,
        };

        pending_tasks.insert(key.clone(), PendingTask::new(task.clone()));
        if let Some(entry) = pending_tasks.get(&key) {
            entry.generation.store(1, Ordering::Relaxed);
        }

        let mut worker = create_test_worker("input_d", tx, rx, pending_tasks.clone(), pending_task_count);
        let mut runtime_settings = MetadataUpdateRuntimeSettings::default();
        runtime_settings.no_change_cache_ttl_secs = 1;

        let stale_instant = Instant::now().checked_sub(Duration::from_secs(2)).unwrap_or_else(Instant::now);
        worker.recently_completed_no_change.insert(
            key.clone(),
            (stale_instant, ResolveReasonSet::from_variants(&[ResolveReason::Info])),
        );
        assert!(!worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
        assert!(!worker.recently_completed_no_change.contains_key(&key));

        let requeued = worker.finalize_processed_task_success(&key, 0, "input_d").await;
        assert!(requeued);
        assert!(pending_tasks.contains_key(&key));
        assert_eq!(worker.receiver.try_recv().expect("requeued signal should be present"), key);
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
    fn strip_tmdb_reasons_returns_none_for_tmdb_only_resolve_task() {
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(100),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Date]),
            delay: 5,
            source_last_modified: None,
        };

        assert!(InputWorker::strip_tmdb_reasons(&task).is_none());
    }

    #[test]
    fn strip_tmdb_reasons_keeps_non_tmdb_reasons() {
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(5),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Tmdb, ResolveReason::Probe, ResolveReason::Info]),
            delay: 1,
            source_last_modified: Some(123),
        };

        let stripped = InputWorker::strip_tmdb_reasons(&task).expect("task should keep non-tmdb reasons");
        match stripped {
            UpdateTask::ResolveSeries { reason, .. } => {
                assert!(!reason.contains(ResolveReason::Tmdb));
                assert!(!reason.contains(ResolveReason::Date));
                assert!(reason.contains(ResolveReason::Probe));
                assert!(reason.contains(ResolveReason::Info));
            }
            other => panic!("unexpected task type after strip: {other:?}"),
        }
    }

    #[test]
    fn metadata_retry_state_disk_roundtrip() {
        let dir = tempdir().expect("tempdir should be created");
        let path = dir.path().join("metadata_retry_state.db");
        let key = TaskKey::Stream { scope: Arc::from("input_a"), id: Arc::from("stream_1") };
        let state = TaskRetryState {
            resolve: Some(RetryState {
                attempts: 4,
                next_allowed_at_ts: 1_700_043_200,
                cooldown_until_ts: Some(1_700_043_200),
                last_error: Some("resolve exhausted".to_string()),
                source_last_modified: None,
            }),
            probe: Some(RetryState {
                attempts: 3,
                next_allowed_at_ts: 1_700_000_000,
                cooldown_until_ts: Some(1_700_086_400),
                last_error: Some("probe timeout".to_string()),
                source_last_modified: None,
            }),
            tmdb: Some(RetryState {
                attempts: 0,
                next_allowed_at_ts: 1_700_172_800,
                cooldown_until_ts: Some(1_700_172_800),
                last_error: Some("tmdb no match".to_string()),
                source_last_modified: Some(999),
            }),
            updated_at_ts: 1_700_000_000,
        };

        persist_metadata_retry_state_to_disk(&path, &key, Some(&state)).expect("state persistence should succeed");
        let loaded = load_metadata_retry_states_from_disk(&path).expect("state load should succeed");
        let loaded_state = loaded.get(&key).expect("probe key should be present");

        let loaded_resolve = loaded_state.resolve.as_ref().expect("resolve retry state should be present");
        assert_eq!(loaded_resolve.attempts, 4);
        assert_eq!(loaded_resolve.cooldown_until_ts, Some(1_700_043_200));
        assert_eq!(loaded_resolve.last_error.as_deref(), Some("resolve exhausted"));
        let loaded_probe = loaded_state.probe.as_ref().expect("probe retry state should be present");
        assert_eq!(loaded_probe.attempts, 3);
        assert_eq!(loaded_probe.cooldown_until_ts, Some(1_700_086_400));
        assert_eq!(loaded_probe.last_error.as_deref(), Some("probe timeout"));
        let loaded_tmdb = loaded_state.tmdb.as_ref().expect("tmdb retry state should be present");
        assert_eq!(loaded_tmdb.cooldown_until_ts, Some(1_700_172_800));
        assert_eq!(loaded_tmdb.last_error.as_deref(), Some("tmdb no match"));
        assert_eq!(loaded_tmdb.source_last_modified, Some(999));

        persist_metadata_retry_state_to_disk(&path, &key, None).expect("state clear should succeed");
        let cleared = load_metadata_retry_states_from_disk(&path).expect("state reload should succeed");
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

    #[test]
    fn transient_worker_errors_include_connection_unavailable() {
        assert!(InputWorker::is_transient_worker_error(TASK_ERR_UPDATE_IN_PROGRESS));
        assert!(InputWorker::is_transient_worker_error(TASK_ERR_PREEMPTED));
        assert!(InputWorker::is_transient_worker_error(TASK_ERR_NO_CONNECTION));
        assert!(!InputWorker::is_transient_worker_error("permanent error"));
    }

    #[test]
    fn permanent_not_found_error_matches_standalone_markers() {
        assert!(InputWorker::is_permanent_not_found_error("HTTP 404 Not Found"));
        assert!(InputWorker::is_permanent_not_found_error("probe failed: 404: stream unavailable"));
        assert!(InputWorker::is_permanent_not_found_error("resource not found on provider"));
    }

    #[test]
    fn permanent_not_found_error_ignores_partial_markers() {
        assert!(!InputWorker::is_permanent_not_found_error("error code 1404 while probing"));
        assert!(!InputWorker::is_permanent_not_found_error("status404unexpected"));
        assert!(!InputWorker::is_permanent_not_found_error("movie not foundry metadata mismatch"));
    }

    #[test]
    fn retry_domain_uses_probe_for_probe_only_resolve_tasks() {
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(7),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            source_last_modified: None,
        };
        assert_eq!(InputWorker::retry_domain_for_task(&task), RetryDomain::Probe);
    }

    #[test]
    fn retry_domain_keeps_resolve_for_mixed_resolve_tasks() {
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(11),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe, ResolveReason::Info]),
            delay: 0,
            source_last_modified: None,
        };
        assert_eq!(InputWorker::retry_domain_for_task(&task), RetryDomain::Resolve);
    }

    #[test]
    fn take_pending_probe_task_snapshot_skips_scheduled_requeues() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let worker = create_test_worker("input_probe_skip", tx, rx, pending_tasks.clone(), pending_task_count);

        let key = TaskKey::Live(100);
        let task = UpdateTask::ProbeLive {
            id: ProviderIdType::Id(100),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            interval: 60,
        };
        pending_tasks.insert(key.clone(), PendingTask::new(task));
        worker.scheduled_requeues.insert(key, chrono::Utc::now().timestamp().saturating_add(30));

        assert!(worker.take_pending_probe_task_snapshot().is_none());
    }

    #[test]
    fn take_pending_probe_task_snapshot_accepts_probe_only_resolve() {
        let (tx, rx) = mpsc::channel::<TaskKey>(8);
        let pending_tasks = Arc::new(DashMap::new());
        let pending_task_count = Arc::new(AtomicUsize::new(1));
        let worker = create_test_worker("input_probe_only_resolve", tx, rx, pending_tasks.clone(), pending_task_count);

        let key = TaskKey::Vod(77);
        let task = UpdateTask::ResolveVod {
            id: ProviderIdType::Id(77),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            source_last_modified: None,
        };
        pending_tasks.insert(key.clone(), PendingTask::new(task));

        let snapshot = worker.take_pending_probe_task_snapshot().expect("expected probe-domain snapshot");
        assert_eq!(snapshot.0, key);
        assert_eq!(InputWorker::retry_domain_for_task(&snapshot.1), RetryDomain::Probe);
    }

    #[test]
    fn playlist_trigger_ignores_probe_only_changes() {
        let task = UpdateTask::ProbeLive {
            id: ProviderIdType::Id(22),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Probe]),
            delay: 0,
            interval: 60,
        };
        assert!(!InputWorker::should_trigger_playlist_update_for_task(&task, true));
    }

    #[test]
    fn playlist_trigger_keeps_info_changes() {
        let task = UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(33),
            reason: ResolveReasonSet::from_variants(&[ResolveReason::Info]),
            delay: 0,
            source_last_modified: None,
        };
        assert!(InputWorker::should_trigger_playlist_update_for_task(&task, true));
        assert!(!InputWorker::should_trigger_playlist_update_for_task(&task, false));
    }
}
