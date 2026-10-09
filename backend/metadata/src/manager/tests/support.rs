use super::{InputWorker, PendingTask, TaskKey};
use dashmap::DashMap;
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicUsize, Arc},
};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::BatchResultCollector;

pub(in crate::manager::tests) fn create_test_worker(
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
        bound_ctx: None,
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
