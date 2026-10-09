use super::{create_test_worker, MetadataUpdateRuntimeSettings, PendingTask, TaskKey};
use dashmap::DashMap;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tuliprox_core::model::{ProviderIdType, ResolveReason, UpdateTask};

#[tokio::test]
async fn recent_no_change_skip_ttl_expiry_allows_requeue() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let key = TaskKey::Vod(11);
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(11),
        reason: ResolveReason::Info.into(),
        delay: 0,
        source_last_modified: None,
    };

    pending_tasks.insert(key.clone(), PendingTask::new(task.clone()));
    if let Some(entry) = pending_tasks.get(&key) {
        entry.generation.store(1, Ordering::Relaxed);
    }

    let mut worker = create_test_worker("input_d", tx, rx, pending_tasks.clone(), pending_task_count);
    let runtime_settings =
        MetadataUpdateRuntimeSettings { no_change_cache_ttl_secs: 1, ..MetadataUpdateRuntimeSettings::default() };

    let stale_instant = Instant::now().checked_sub(Duration::from_secs(2)).unwrap_or_else(Instant::now);
    worker.recently_completed_no_change.insert(key.clone(), (stale_instant, ResolveReason::Info.into()));
    assert!(!worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
    assert!(!worker.recently_completed_no_change.contains_key(&key));

    let requeued = worker.finalize_processed_task_success(&key, 0, "input_d").await;
    assert!(requeued);
    assert!(pending_tasks.contains_key(&key));
    assert_eq!(worker.receiver.try_recv().expect("requeued signal should be present"), key);
}
