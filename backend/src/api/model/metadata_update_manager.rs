use crate::api::model::{AppState, EventMessage};
use log::{debug, error, info};
use shared::error::TuliproxError;
use shared::utils::{sanitize_sensitive_info};
use std::collections::{HashMap, VecDeque, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use crate::utils::{debug_if_enabled};
use crate::processing::processor::xtream_vod::update_vod_metadata;
use crate::processing::processor::xtream_series::update_series_metadata;
use crate::processing::processor::xtream::update_live_stream_metadata;
use crate::processing::processor::stream_probe::update_generic_stream_metadata;
use shared::model::{PlaylistItemType, XtreamCluster, XtreamPlaylistItem};
use crate::api::model::ProviderHandle;
use crate::repository::{get_input_storage_path, xtream_get_file_path, BPlusTreeQuery};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpdateTask {
    ResolveVod { id: u32, reason: String },
    ResolveSeries { id: u32, reason: String },
    ProbeLive { id: u32, reason: String },
    // Generic probe for M3U/Library/etc.
    ProbeStream { unique_id: String, url: String, item_type: PlaylistItemType, reason: String },
}

impl UpdateTask {
    // Returns a tuple to identify if tasks target the same item (ignoring reason)
    fn get_key(&self) -> (u32, String) {
        match self {
            UpdateTask::ResolveVod { id, .. } => (*id, "vod".to_string()),
            UpdateTask::ResolveSeries { id, .. } => (*id, "series".to_string()),
            UpdateTask::ProbeLive { id, .. } => (*id, "live".to_string()),
            UpdateTask::ProbeStream { unique_id, .. } => (0, unique_id.clone()),
        }
    }
    
    fn update_reason(&mut self, new_reason: &str) {
        let current_reason = match self {
            UpdateTask::ResolveVod { reason, .. } => reason,
            UpdateTask::ResolveSeries { reason, .. } => reason,
            UpdateTask::ProbeLive { reason, .. } => reason,
            UpdateTask::ProbeStream { reason, .. } => reason,
        };
        
        let mut parts: HashSet<String> = current_reason.split(',').map(String::from).collect();
        for p in new_reason.split(',') {
            parts.insert(p.to_string());
        }
        let mut sorted: Vec<String> = parts.into_iter().collect();
        sorted.sort();
        *current_reason = sorted.join(",");
    }
}

impl std::fmt::Display for UpdateTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateTask::ResolveVod { id, reason } => write!(f, "Resolve VOD {} (Reason: {})", id, reason),
            UpdateTask::ResolveSeries { id, reason } => write!(f, "Resolve Series {} (Reason: {})", id, reason),
            UpdateTask::ProbeLive { id, reason } => write!(f, "Probe Live {} (Reason: {})", id, reason),
            UpdateTask::ProbeStream { unique_id, reason, .. } => write!(f, "Probe Stream {} (Reason: {})", unique_id, reason),
        }
    }
}

struct QueueState {
    tasks: VecDeque<UpdateTask>,
    // Mapping from (ID, Type) -> Index in tasks VecDeque
    // This allows O(1) lookup to find existing task for merging
    task_map: HashMap<(u32, String), usize>, 
    processed_count: usize, // Track successful updates for batch logging
    total_in_batch: usize, // Snapshot of total items when batch starts
}

pub struct MetadataUpdateManager {
    queues: Mutex<HashMap<Arc<str>, QueueState>>, // Key: Input Name
    notify: Notify,
    // Use std::sync::Mutex for simple Option swapping to allow sync access without await
    app_state: std::sync::Mutex<Option<Weak<AppState>>>,
}

impl Default for MetadataUpdateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataUpdateManager {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            app_state: std::sync::Mutex::new(None),
        }
    }

    pub fn set_app_state(&self, app_state: Weak<AppState>) {
        let mut guard = self.app_state.lock().unwrap();
        *guard = Some(app_state);
    }

    pub async fn queue_task(&self, input_name: Arc<str>, task: UpdateTask) {
        let mut queues = self.queues.lock().await;
        let state = queues.entry(input_name).or_insert_with(|| QueueState {
            tasks: VecDeque::new(),
            task_map: HashMap::new(),
            processed_count: 0,
            total_in_batch: 0,
        });

        let key = task.get_key();
        
        if let Some(&index) = state.task_map.get(&key) {
             // Task exists, merge reasons
             if index < state.tasks.len() {
                 let existing_task = &mut state.tasks[index];
                 // If the keys match (ID and Type), merge reasons
                 if existing_task.get_key() == key {
                     let new_reason = match &task {
                         UpdateTask::ResolveVod { reason, .. } => reason.clone(),
                         UpdateTask::ResolveSeries { reason, .. } => reason.clone(),
                         UpdateTask::ProbeLive { reason, .. } => reason.clone(),
                         UpdateTask::ProbeStream { reason, .. } => reason.clone(),
                     };
                     existing_task.update_reason(&new_reason);
                     // debug_if_enabled!("Merged task reasons: {}", existing_task);
                     return;
                 }
             }
        }
        
        state.tasks.push_back(task);
        // Map points to the index of the newly added task
        state.task_map.insert(key, state.tasks.len() - 1);
        
        if state.total_in_batch == 0 {
            state.total_in_batch = 1; 
        } else {
            state.total_in_batch += 1;
        }
        self.notify.notify_one();
    }

    pub async fn start_workers(self: Arc<Self>, cancel_token: CancellationToken) {
        tokio::spawn(async move {
            debug!("Metadata update worker started");
            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => break,
                    () = self.notify.notified() => {
                        // Process available tasks
                        self.process_queues().await;
                    }
                }
            }
            debug!("Metadata update worker stopped");
        });
    }

    async fn process_queues(&self) {
        let app_state_arc = {
            let guard = self.app_state.lock().unwrap();
            match guard.as_ref().and_then(Weak::upgrade) {
                Some(s) => s,
                None => return, // AppState gone
            }
        };

        let mut inputs_to_process = Vec::new();
        {
            let queues = self.queues.lock().await;
            for (name, state) in queues.iter() {
                if !state.tasks.is_empty() {
                    inputs_to_process.push(name.clone());
                }
            }
        }

        for input_name in inputs_to_process {
            self.process_input_queue(&app_state_arc, input_name).await;
        }
    }

    async fn get_item_name(app_state: &Arc<AppState>, input_name: &str, task: &UpdateTask) -> Option<String> {
        let (id, cluster) = match task {
            UpdateTask::ResolveVod { id, .. } => (*id, XtreamCluster::Video),
            UpdateTask::ResolveSeries { id, .. } => (*id, XtreamCluster::Series),
            UpdateTask::ProbeLive { id, .. } => (*id, XtreamCluster::Live),
            _ => return None,
        };

        let working_dir = &app_state.app_config.config.load().working_dir;
        if let Ok(storage_path) = get_input_storage_path(input_name, working_dir).await {
            let file_path = xtream_get_file_path(&storage_path, cluster);
            if file_path.exists() {
                let _lock = app_state.app_config.file_locks.read_lock(&file_path).await;
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&file_path) {
                    if let Ok(Some(item)) = query.query_zero_copy(&id) {
                        return Some(if item.title.is_empty() { item.name.to_string() } else { item.title.to_string() });
                    }
                }
            }
        }
        None
    }

    async fn process_input_queue(&self, app_state: &Arc<AppState>, input_name: Arc<str>) {
        let dummy_addr = "127.0.0.1:0".parse().unwrap();
        // Priority 0 = Background
        let prio = 0; 
        
        {
            let queues = self.queues.lock().await;
            if let Some(state) = queues.get(&input_name) {
                if state.processed_count == 0 && !state.tasks.is_empty() {
                    info!("Starting background metadata updates for input {}: {} items queued", 
                        sanitize_sensitive_info(&input_name), state.tasks.len());
                }
            }
        }
        
        loop {
             let (task, current_progress, total) = {
                let mut queues = self.queues.lock().await;
                let Some(state) = queues.get_mut(&input_name) else { break; };
                
                // If we are starting a fresh batch (or just queued items), update total snapshot
                if state.processed_count == 0 && state.total_in_batch < state.tasks.len() {
                    state.total_in_batch = state.tasks.len();
                }
                
                let task = state.tasks.pop_front();
                
                // Rebuild map because we pushed front or popped, indices invalid
                state.task_map.clear();
                for (i, t) in state.tasks.iter().enumerate() {
                    state.task_map.insert(t.get_key(), i);
                }
                
                (task, state.processed_count, state.total_in_batch)
            };

            let Some(task) = task else { 
                // Queue drained. Trigger "Bundled" update event.
                info!("Metadata updates completed for input {}. Total processed: {}", 
                     sanitize_sensitive_info(&input_name), current_progress);
                
                // Reset counters
                {
                    let mut queues = self.queues.lock().await;
                    if let Some(state) = queues.get_mut(&input_name) {
                        state.processed_count = 0;
                        state.total_in_batch = 0;
                    }
                }
                
                app_state.event_manager.send_event(EventMessage::InputMetadataUpdatesCompleted(input_name));
                break; 
            };

            // Attempt to acquire connection with low priority
            // IMPORTANT: use acquire_connection_with_grace_override with `false` to strictly enforce max_connections limits for background tasks.
            if let Some(handle) = app_state.active_provider.acquire_connection_with_grace_override(&input_name, &dummy_addr, false, prio).await {
                
                // Identify the specific config used (e.g. alias vs main input) to use correct credentials
                let config_to_use = handle.allocation.get_provider_config();
                
                let item_title = Self::get_item_name(app_state, &input_name, &task).await;
                let name_display = item_title.as_deref().map_or("".to_string(), |n| format!(" \"{n}\""));

                debug_if_enabled!("Processing task {}/{} for {}: {}{}", 
                    current_progress + 1, total, sanitize_sensitive_info(&input_name), task, name_display);

                // Execute Task - PASS THE HANDLE so inner logic doesn't try to acquire again (deadlock prevention)
                // Wrap in select to handle preemption cancellation immediately
                let execution = self.execute_task(app_state, &input_name, &task, config_to_use, Some(&handle), item_title);
                let result = if let Some(token) = &handle.cancel_token {
                    tokio::select! {
                        res = execution => res,
                        _ = token.cancelled() => {
                            debug_if_enabled!("Metadata update task preempted by user request for input {}", sanitize_sensitive_info(&input_name));
                            // Put task back at front using CLONE to satisfy borrow checker
                            let mut queues = self.queues.lock().await;
                            if let Some(state) = queues.get_mut(&input_name) {
                                state.tasks.push_front(task.clone());
                            }
                             Err(shared::error::info_err!("Task preempted"))
                        }
                    }
                } else {
                    execution.await
                };
                
                app_state.connection_manager.release_provider_handle(Some(handle)).await;
                
                // Handle result and logging
                let mut queues = self.queues.lock().await;
                if let Some(state) = queues.get_mut(&input_name) {
                    match result {
                        Ok(()) => {
                            state.processed_count += 1;
                            
                            // Log info every 10 items
                            if state.processed_count % 10 == 0 {
                                info!(
                                    "Background Metadata Update: {}/{} resolved for input {}", 
                                    state.processed_count, 
                                    state.total_in_batch,
                                    sanitize_sensitive_info(&input_name)
                                );
                            }
                        }
                        Err(e) => {
                            // Only log error if it wasn't a preemption (preemption already logged and requeued)
                            if e.message != "Task preempted" {
                                error!("Task {:?} failed for input {}: {}", task, sanitize_sensitive_info(&input_name), e);
                            } else {
                                 // Break the loop so we release the connection properly and don't tight loop
                                 break;
                            }
                        }
                    }
                }
                
                // Small delay to yield
                tokio::time::sleep(Duration::from_millis(50)).await;
                
            } else {
                // No connection available, put task back and backoff for this input
                debug_if_enabled!("No provider connection available for background task {}, deferring...", task);
                let mut queues = self.queues.lock().await;
                if let Some(state) = queues.get_mut(&input_name) {
                     state.tasks.push_front(task);
                     // Rebuild map because we pushed front
                     state.task_map.clear();
                     for (i, t) in state.tasks.iter().enumerate() {
                        state.task_map.insert(t.get_key(), i);
                     }
                }
                break; // Stop processing this input for now
            }
        }
    }
    
    async fn execute_task(
        &self, 
        app_state: &Arc<AppState>, 
        input_name: &str, 
        task: &UpdateTask,
        allocated_config: Option<Arc<crate::api::model::ProviderConfig>>,
        active_handle: Option<&ProviderHandle>,
        item_title: Option<String>,
    ) -> Result<(), TuliproxError> {
        let Some(input_base) = app_state.app_config.get_input_by_name(&input_name.into()) else {
             return Err(shared::error::info_err!("Input {input_name} not found"));
        };
        
        let input_to_use = if let Some(alloc) = allocated_config {
             if alloc.name != input_base.name {
                 if let Some(aliases) = &input_base.aliases {
                     if let Some(alias_def) = aliases.iter().find(|a| a.name == alloc.name) {
                         let mut temp_input = (*input_base).clone();
                         temp_input.url = alias_def.url.clone();
                         temp_input.username = alias_def.username.clone();
                         temp_input.password = alias_def.password.clone();
                         temp_input.name = input_base.name.clone(); 
                         Arc::new(temp_input)
                     } else { input_base }
                 } else { input_base }
             } else { input_base }
        } else {
            input_base
        };
        
        let client = app_state.http_client.load();

        match task {
            UpdateTask::ResolveVod { id, reason } => {
                let fetch_info = reason.contains("info");
                 update_vod_metadata(
                    &app_state.app_config, 
                    &client, 
                    &input_to_use, 
                    *id, 
                    active_handle, 
                    &app_state.active_provider,
                    item_title.as_deref(),
                    true, // Background tasks always save immediately
                    fetch_info,
                 ).await.map(|_| ())
            }
             UpdateTask::ResolveSeries { id, reason } => {
                 let fetch_info = reason.contains("info");
                 update_series_metadata(
                    &app_state.app_config, 
                    &client, 
                    &input_to_use, 
                    *id, 
                    &app_state.active_provider, 
                    active_handle,
                    item_title.as_deref(),
                    true, // Background tasks always save immediately
                    fetch_info
                 ).await.map(|_| ())
            }
             UpdateTask::ProbeLive { id, .. } => {
                 update_live_stream_metadata(&app_state.app_config, &client, &input_to_use, *id).await
            }
            UpdateTask::ProbeStream { unique_id, url, item_type, .. } => {
                update_generic_stream_metadata(
                    &app_state.app_config, 
                    &client, 
                    &input_to_use, 
                    unique_id, 
                    url, 
                    *item_type, 
                    &app_state.active_provider,
                    active_handle
                ).await
            }
        }
    }
}