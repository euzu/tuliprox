use super::*;

#[tokio::test]
async fn removing_requeued_tasks_prunes_only_idle_workers() {
    for running in [false, true] {
        let queue = RecordingQueue::new();
        queue.queue.lock().await.push_back(task("requeued", RecordingKind::Vod, RecordingTaskState::Queued));
        let worker = queue.worker("requeued");
        *worker.running.write().await = running;
        assert!(queue.remove_from_queue("requeued").await.expect("remove"));
        assert_eq!(queue.existing_worker("requeued").is_some(), running);
        queue.request_worker_restart();
        if !running {
            assert_eq!(*worker.control_signal.read().await, RecordingControl::None);
        }
        queue.release_worker("requeued").await;
        assert!(queue.existing_worker("requeued").is_none());
    }
}

#[tokio::test]
async fn attaching_requeued_tasks_prunes_their_idle_worker_state() {
    let queue = RecordingQueue::new();
    let completed = task("completed", RecordingKind::Vod, RecordingTaskState::Completed);
    let mut waiting = task("waiting", RecordingKind::Vod, RecordingTaskState::Queued);
    waiting.url = completed.url.clone();
    queue.queue.lock().await.push_back(waiting);
    queue.finished.write().await.push(completed);
    queue.worker("waiting");
    mutate(&queue, |candidate| {
        assert!(promote_from_queue(candidate).is_none());
        Ok(())
    })
    .await
    .expect("attach existing recording");
    assert!(queue.queue.lock().await.is_empty());
    assert_eq!(queue.finished.read().await.len(), 2);
    assert!(queue.existing_worker("waiting").is_none());
}
