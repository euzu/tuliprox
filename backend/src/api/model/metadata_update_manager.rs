use crate::api::model::{AppState, EventMessage};
use log::{debug, error, info, warn};
use shared::error::TuliproxError;
use shared::utils::{sanitize_sensitive_info, generate_playlist_uuid};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use shared::create_bit_set;
use crate::utils::debug_if_enabled;
use crate::processing::processor::{update_vod_metadata, update_series_metadata, update_live_stream_metadata, update_generic_stream_metadata};
use shared::model::{LiveStreamProperties, PlaylistItemType, SeriesStreamProperties, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem};
use crate::api::model::ProviderHandle;

use crate::repository::{get_input_storage_path, xtream_get_file_path, BPlusTreeQuery, get_target_id_mapping_file, write_playlist_batch_item_upsert};
use crate::repository::{persist_input_vod_info_batch, persist_input_series_info_batch, persist_input_live_info_batch,  TargetIdMapping};
use crate::api::model::BatchResultCollector;
use std::collections::{HashMap, HashSet};
use crate::utils::FileReadGuard;
use std::cmp::min;

const MAX_QUEUE_SIZE: usize = 100_000;
const TASK_ERR_NO_CONNECTION: &str = "No connection available";
const TASK_ERR_PREEMPTED: &str = "Task preempted";

create_bit_set!(u32, ResolveReason, Info, Tmdb, Date, Probe, MissingDetails);

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
    ProbeStream { unique_id: String, url: String, item_type: PlaylistItemType, reason: ResolveReasonSet, delay: u16 },
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
            UpdateTask::ProbeStream { unique_id, reason, delay, .. } => write!(f, "Probe Stream {unique_id} (Reason: {reason}, Delay: {delay}sec)"),
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
    Stream(String), 
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
            UpdateTask::ProbeStream { unique_id, url, .. } => {
                if unique_id.trim().is_empty() {
                    TaskKey::Stream(url.clone())
                } else {
                    TaskKey::Stream(unique_id.clone())
                }
            },
        }
    }
}

/// Per-input worker context. Each input has its own worker
/// that processes tasks sequentially with rate limiting.
#[derive(Clone)]
struct InputWorkerContext {
    sender: mpsc::Sender<TaskKey>,
    pending_tasks: Arc<DashMap<TaskKey, Mutex<UpdateTask>>>,
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
        pending_tasks: Arc<DashMap<TaskKey, Mutex<UpdateTask>>>,
        input_name: &str,
        task: UpdateTask
    ) {
        let key = TaskKey::from_task(&task);

        if let Some(entry) = pending_tasks.get(&key) {
            let mut existing = entry.lock().await;
            // Merge logic
            match (&mut *existing, task) {
                (UpdateTask::ResolveVod { reason: r1, delay: d1, .. }, UpdateTask::ResolveVod { reason: r2, delay: d2, .. })
                | (UpdateTask::ResolveSeries { reason: r1, delay: d1, .. }, UpdateTask::ResolveSeries { reason: r2, delay: d2, .. })
                | (UpdateTask::ProbeStream { reason: r1, delay: d1, .. }, UpdateTask::ProbeStream { reason: r2, delay: d2, .. }) => {
                    *r1 = *r1 | r2;
                    *d1 = min(*d1, d2);
                }
                (UpdateTask::ProbeLive { reason: r1, delay: d1, interval: i1, .. }, UpdateTask::ProbeLive { reason: r2, delay: d2, interval: i2, .. }) => {
                    *r1 = *r1 | r2;
                    *d1 = min(*d1, d2);
                    *i1 = min(*i1, i2);
                }
                _ => {} // Mismatched types, should not happen due to TaskKey
            }
            return;
        }

        if pending_tasks.len() >= MAX_QUEUE_SIZE {
            warn!("Metadata queue full for input {}, dropping task", sanitize_sensitive_info(input_name));
            return;
        }

        pending_tasks.insert(key.clone(), Mutex::new(task));
        if sender.send(key.clone()).await.is_err() {
            pending_tasks.remove(&key);
            warn!("Failed to send task signal for input {}", sanitize_sensitive_info(input_name));
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
    pending_tasks: Arc<DashMap<TaskKey, Mutex<UpdateTask>>>,
    app_state_weak: Option<Weak<AppState>>,
    cancel_token: CancellationToken,
    batch_buffer: BatchResultCollector,
    db_handles: HashMap<XtreamCluster, DbHandle>,
    failed_clusters: HashSet<XtreamCluster>,
}

impl InputWorker {
    #[allow(clippy::too_many_lines)]
    async fn run(mut self) {
        debug!("Metadata worker started for input {}", sanitize_sensitive_info(&self.input_name));
        
        let mut processed_count: usize = 0;
        let mut retry_attempts: HashMap<TaskKey, u8> = HashMap::new();
        let mut last_queue_counts: (usize, usize) = (usize::MAX, usize::MAX);
        let mut last_queue_log_at = Instant::now();

        let input_name = self.input_name.clone();
        let app_state_weak = self.app_state_weak.clone();

        // Buffer for the "next" task if we picked it up via try_recv
        let mut next_task: Option<(TaskKey, UpdateTask)> = None;

        loop {
            // Determine the task to process
            let task_data = if let Some(t) = next_task.take() {
                Some(t)
            } else {
                // If we have to wait, we MUST release all DB locks/handles to avoid blocking writers
                self.db_handles.clear();
                self.failed_clusters.clear();

                tokio::select! {
                    biased;
                    
                    () = self.cancel_token.cancelled() => {
                        debug!("Metadata worker cancelled for input {}", sanitize_sensitive_info(&input_name));
                        break;
                    }
                    
                    res = self.receiver.recv() => {
                        match res {
                            Some(key) => {
                                // Retrieve task snapshot
                                if let Some(entry) = self.pending_tasks.get(&key) {
                                    let task = entry.lock().await.clone();
                                    // Optimization: Remove only AFTER processing? 
                                    // Requirement: "When a worker finishes a task, it must remove the ID"
                                    // So we keep it in map, but we need to pass the KEY to the next block to remove it later.
                                    Some((key, task))
                                } else {
                                    None // Task cancelled or processed??
                                }
                            },
                            None => break, // Channel closed
                        }
                    }
                }
            };
            
            let Some((current_key, current_task)) = task_data else { continue };
            
            if self.db_handles.is_empty() {
                 info!("Starting background metadata updates for input {}", sanitize_sensitive_info(&input_name));
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
                Ok(()) => {
                    processed_count += 1;
                    retry_attempts.remove(&current_key);
                    if processed_count.is_multiple_of(10) {
                        info!(
                            "Background metadata update: {} resolved for input {}",
                            processed_count,
                            sanitize_sensitive_info(&input_name)
                        );
                    }
                }
                Err(e) if e.message == TASK_ERR_NO_CONNECTION => {
                    requeue_current = true;

                    let entry = retry_attempts.entry(current_key.clone()).or_insert(0);
                    *entry = entry.saturating_add(1);
                    let attempts = *entry;

                    // If there are other tasks, just move this task to the back of the queue.
                    // If this is the only task, use exponential backoff based on resolve_delay.
                    if self.receiver.is_empty() {
                        let base_delay = u64::from(current_task.delay().max(1));
                        let exp = u32::from(attempts.saturating_sub(1).min(5));
                        retry_delay_secs = (base_delay.saturating_mul(2_u64.saturating_pow(exp))).min(60);
                    }

                    debug_if_enabled!(
                        "No provider connection for task {} on input {}, requeueing (attempt={}, retry_delay={}s, queue_len={})",
                        current_task,
                        sanitize_sensitive_info(&input_name),
                        attempts,
                        retry_delay_secs,
                        self.receiver.len()
                    );
                }
                Err(e) => {
                    retry_attempts.remove(&current_key);
                    if e.message != TASK_ERR_PREEMPTED {
                        error!(
                            "Task {} failed for input {}: {}",
                            current_task,
                            sanitize_sensitive_info(&input_name),
                            e
                        );
                    }
                }
            }

            // Check and flush batch
            if self.batch_buffer.should_flush() {
                // Release locks before flushing (writing)
                self.db_handles.clear();
                self.failed_clusters.clear();
                Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
            }

            if requeue_current {
                if retry_delay_secs > 0 {
                    tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                }

                if self.sender.send(current_key.clone()).await.is_err() {
                    // Channel closed, drop the pending task key to avoid leaks.
                    self.pending_tasks.remove(&current_key);
                    retry_attempts.remove(&current_key);
                    warn!(
                        "Failed to requeue task {} for input {}",
                        current_task,
                        sanitize_sensitive_info(&input_name)
                    );
                }
            } else {
                // Rate limiting
                if delay_secs > 0 {
                    tokio::time::sleep(Duration::from_secs(u64::from(delay_secs))).await;
                }

                // Cleanup from map (allows new tasks for this ID to be queued)
                self.pending_tasks.remove(&current_key);
            }

            // Try to get the next task immediately to keep locks open
            if let Ok(key) = self.receiver.try_recv() {
                 if let Some(entry) = self.pending_tasks.get(&key) {
                    next_task = Some((key, entry.lock().await.clone()));
                 }
            }

            let queue_counts = Self::queue_resolve_counts(&self.pending_tasks);
            if queue_counts != last_queue_counts || last_queue_log_at.elapsed() >= Duration::from_secs(30) {
                info!("In queue to resolve vod: {}, series: {} (input: {input_name})", queue_counts.0, queue_counts.1);
                last_queue_counts = queue_counts;
                last_queue_log_at = Instant::now();
            }
        }

        // Final flush
        self.db_handles.clear();
        self.failed_clusters.clear();
        Self::flush_batch_static(&input_name, app_state_weak.as_ref(), &mut self.batch_buffer).await;
        
        // Log completion
        if processed_count > 0 {
             info!("Metadata updates completed for input {}. Total processed: {}", 
                   sanitize_sensitive_info(&input_name), processed_count);
             if let Some(app_state) = app_state_weak.as_ref().and_then(Weak::upgrade) {
                app_state.event_manager.send_event(EventMessage::InputMetadataUpdatesCompleted(input_name.clone()));
            }
        }
        
        debug!("Metadata worker stopped for input {}", sanitize_sensitive_info(&input_name));
    }

    fn queue_resolve_counts(pending_tasks: &DashMap<TaskKey, Mutex<UpdateTask>>) -> (usize, usize) {
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

    // Changed to static method
    async fn flush_batch_static(
        input_name: &str,
        app_state_weak: Option<&Weak<AppState>>,
        batch_buffer: &mut BatchResultCollector
    ) {
        if batch_buffer.is_empty() { return; }

        let Some(app_state) = app_state_weak.and_then(Weak::upgrade) else { return };
        let app_config = &app_state.app_config;
        let working_dir = &app_config.config.load().working_dir;

        if let Ok(storage_path) = get_input_storage_path(input_name, working_dir).await {
            let vod_updates = batch_buffer.take_vod_updates();
            if !vod_updates.is_empty() {
                let updates: Vec<(u32, VideoStreamProperties)> = vod_updates.into_iter()
                    .filter_map(|(id, props)| if let ProviderIdType::Id(vid) = id { Some((vid, props)) } else { None })
                    .collect();
                
                if !updates.is_empty() {
                    if let Err(e) = persist_input_vod_info_batch(app_config, &storage_path, XtreamCluster::Video, input_name, updates).await {
                         error!("Failed to flush VOD batch for input {}: {}", sanitize_sensitive_info(input_name), e);
                    }
                }
            }
            
            let series_updates = batch_buffer.take_series_updates();
            if !series_updates.is_empty() {
                let updates: Vec<(u32, SeriesStreamProperties)> = series_updates.into_iter()
                    .filter_map(|(id, props)| if let ProviderIdType::Id(vid) = id { Some((vid, props)) } else { None })
                    .collect();

                if !updates.is_empty() {
                    if let Err(e) = persist_input_series_info_batch(app_config, &storage_path, XtreamCluster::Series, input_name, updates).await {
                        error!("Failed to flush Series batch for input {}: {}", sanitize_sensitive_info(input_name), e);
                    }
                }
            }

            let live_updates = batch_buffer.take_live_updates();
            if !live_updates.is_empty() {
                let updates: Vec<(u32, LiveStreamProperties)> = live_updates.into_iter()
                    .filter_map(|(id, props)| if let ProviderIdType::Id(vid) = id { Some((vid, props)) } else { None })
                    .collect();

                if !updates.is_empty() {
                    if let Err(e) = persist_input_live_info_batch(app_config, &storage_path, XtreamCluster::Live, input_name, updates).await {
                         error!("Failed to flush Live batch for input {}: {}", sanitize_sensitive_info(input_name), e);
                    }
                }
            }
        }

        Self::cascade_updates(
            &app_state,
            &app_config.config.load(),
            input_name,
            batch_buffer
        ).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn cascade_updates(
        app_state: &Arc<AppState>,
        config: &crate::model::Config,
        input_name: &str,
        batch: &BatchResultCollector,
    ) {
        // Find inputs that use this input_name (including aliases)
        let targets = {
            let sources = app_state.app_config.sources.load();
            let mut affected_targets = Vec::new(); // (Target, InputConfig)

            for source in &sources.sources {
               for t_def in &source.targets {
                   // Check if this source uses the input
                   if source.inputs.iter().any(|i_name| i_name.as_ref() == input_name) {
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

            // Read Mapping to find Virtual IDs
            let lock = app_state.app_config.file_locks.read_lock(&mapping_file).await;
            let mapping = match TargetIdMapping::new(&mapping_file, false) {
                Ok(m) => m,
                Err(e) => {
                    error!("Failed to open ID mapping for target {target_name}: {e}");
                    continue;
                }
            };

            // Process VOD
            if !batch.vod.is_empty() {
                let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Video);
                let lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
                
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    let mut updates = Vec::with_capacity(batch.vod.len());
                    for (pid, props) in &batch.vod {
                        let vids = match pid {
                            ProviderIdType::Id(vid) => mapping.find_virtual_ids(*vid),
                            ProviderIdType::Text(sid) => {
                                let url = props.direct_source.as_ref();
                                let uuid = generate_playlist_uuid(input_name, sid, PlaylistItemType::Video, url);
                                mapping.get_virtual_id_by_uuid(&uuid).into_iter().collect()
                            }
                        };
                        
                        for virtual_id in vids {
                            if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                                item.additional_properties = Some(shared::model::StreamProperties::Video(Box::new(props.clone())));
                                updates.push(item);
                            }
                        }
                    }
                    if !updates.is_empty() {
                        // Release read lock before write
                        drop(lock_read);
                        if let Err(e) = write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Video, &updates).await {
                            error!("Failed to cascade VOD updates to target {target_name}: {e}");
                        }
                        // Cache Update
                        if target.use_memory_cache {
                            Self::update_memory_cache(app_state, target_name, XtreamCluster::Video, updates).await;
                        }
                    }
                }
            }

            // Process Series
            if !batch.series.is_empty() {
                let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Series);
                let lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
                
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    let mut updates = Vec::with_capacity(batch.series.len());
                    for (pid, props) in &batch.series {
                        let vids = match pid {
                            ProviderIdType::Id(vid) => mapping.find_virtual_ids(*vid),
                            ProviderIdType::Text(sid) => {
                                // Series usually don't have a direct URL in properties, relying on ID
                                let uuid = generate_playlist_uuid(input_name, sid, PlaylistItemType::Series, "");
                                mapping.get_virtual_id_by_uuid(&uuid).into_iter().collect()
                            }
                        };

                        for virtual_id in vids {
                            if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                                item.additional_properties = Some(shared::model::StreamProperties::Series(Box::new(props.clone())));
                                updates.push(item);
                            }
                        }
                    }
                    if !updates.is_empty() {
                        drop(lock_read);
                        if let Err(e) = write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Series, &updates).await {
                            error!("Failed to cascade Series updates to target {target_name}: {e}");
                        }
                         if target.use_memory_cache {
                            Self::update_memory_cache(app_state, target_name, XtreamCluster::Series, updates).await;
                        }
                    }
                }
            }
            
            // Process Live (if needed)
             if !batch.live.is_empty() {
                let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
                let lock_read = app_state.app_config.file_locks.read_lock(&xtream_path).await;
                
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    let mut updates = Vec::with_capacity(batch.live.len());
                    for (pid, props) in &batch.live {
                        let vids = match pid {
                            ProviderIdType::Id(vid) => mapping.find_virtual_ids(*vid),
                            ProviderIdType::Text(sid) => {
                                let url = props.direct_source.as_ref();
                                let uuid = generate_playlist_uuid(input_name, sid, PlaylistItemType::Live, url);
                                mapping.get_virtual_id_by_uuid(&uuid).into_iter().collect()
                            }
                        };

                        for virtual_id in vids {
                            if let Ok(Some(mut item)) = query.query_zero_copy(&virtual_id) {
                                item.additional_properties = Some(shared::model::StreamProperties::Live(Box::new(props.clone())));
                                updates.push(item);
                            }
                        }
                    }
                    if !updates.is_empty() {
                        drop(lock_read);
                        if let Err(e) = write_playlist_batch_item_upsert(&app_state.app_config, target_name, XtreamCluster::Live, &updates).await {
                            error!("Failed to cascade Live updates to target {target_name}: {e}");
                        }
                         if target.use_memory_cache {
                            Self::update_memory_cache(app_state, target_name, XtreamCluster::Live, updates).await;
                        }
                    }
                }
            }

            drop(lock); 
        }
    }

    async fn update_memory_cache(
        app_state: &Arc<AppState>,
        target_name: &str,
        cluster: XtreamCluster,
        updates: Vec<XtreamPlaylistItem>
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
        failed_clusters: &mut HashSet<XtreamCluster>
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
        failed_clusters: &mut HashSet<XtreamCluster>
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
        failed_clusters: &mut HashSet<XtreamCluster>
    ) -> Result<(), TuliproxError> {
        let app_state = app_state_weak
            .and_then(Weak::upgrade)
            .ok_or_else(|| shared::error::info_err!("AppState not available"))?;

        let Some(input_base) = app_state.app_config.get_input_by_name(input_name) else {
            return Err(shared::error::info_err!("Input {} not found", input_name));
        };
        
        if !input_base.enabled { 
            return Err(shared::error::info_err!("Input {} is disabled", input_name)); 
        }

        // Attempt to acquire connection with low priority
        let Some(handle) = app_state.active_provider.acquire_connection_for_probe(input_name).await else {
            debug_if_enabled!("No provider connection available for background task {}, skipping...", task);
            return Err(shared::error::info_err!("{}", TASK_ERR_NO_CONNECTION));
        };
        
        let item_title = Self::get_item_name_static(input_name, &app_state, task, db_handles, failed_clusters).await;
        
        let config_to_use = handle.allocation.get_provider_config();
        let name_display = item_title.as_deref().map_or(String::new(), |n| format!(" \"{n}\""));

        debug_if_enabled!("Processing task for {}: {}{}", 
            sanitize_sensitive_info(input_name), task, name_display);

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

        // Execute the task with preemption handling
        if let Some(token) = &handle.cancel_token {
            tokio::select! {
                biased;
                
                () = token.cancelled() => {
                    debug_if_enabled!("Metadata update task preempted by user request for input {}", 
                                      sanitize_sensitive_info(input_name));
                    app_state.connection_manager.release_provider_handle(Some(handle)).await;
                    Err(shared::error::info_err!("{}", TASK_ERR_PREEMPTED))
                }
                
                res = Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), Some(&handle), collector, db_handles, failed_clusters) => {
                    app_state.connection_manager.release_provider_handle(Some(handle)).await;
                    res
                }
            }
        } else {
            let res = Self::execute_task_inner_static(&app_state, &client, &input_to_use, task, item_title.as_deref(), Some(&handle), collector, db_handles, failed_clusters).await;
            app_state.connection_manager.release_provider_handle(Some(handle)).await;
            res
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
        failed_clusters: &mut HashSet<XtreamCluster>
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
                    },
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
                    },
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeLive { id, .. } => {
                let query_opt = Self::get_or_open_query(&input.name, app_state, XtreamCluster::Live, db_handles, failed_clusters).await;
            
                match update_live_stream_metadata(
                    &app_state.app_config, 
                    client, 
                    input, 
                    id.clone(), 
                    false,
                    query_opt
                ).await {
                     Ok(Some(props)) => {
                        collector.add_live(id.clone(), props);
                        Ok(())
                    },
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
}
