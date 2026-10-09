use super::*;

#[tokio::test]
async fn media_completion_scheduling_does_not_block_when_completion_capacity_is_busy() {
    let permits = Arc::new(Semaphore::new(1));
    let held = Arc::clone(&permits).acquire_owned().await.expect("initial permit");
    let completed = Arc::new(AtomicBool::new(false));
    let completed_for_task = Arc::clone(&completed);
    spawn_bounded_media_completion(Arc::clone(&permits), async move {
        completed_for_task.store(true, Ordering::Release);
    });

    tokio::task::yield_now().await;
    assert!(!completed.load(Ordering::Acquire));
    drop(held);

    for _ in 0..16 {
        if completed.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(completed.load(Ordering::Acquire));
    assert_eq!(permits.available_permits(), 1);
}
