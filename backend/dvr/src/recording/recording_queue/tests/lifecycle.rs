use super::*;

#[tokio::test]
async fn pause_and_resume_keep_active_download_resumable() {
    let queue = RecordingQueue::new();
    let active = RecordingTask {
        size: 42,
        total_size: Some(100),
        state: RecordingTaskState::Running,
        ..task("id", RecordingKind::Vod, RecordingTaskState::Running)
    };

    *queue.active.write().await = vec![active];
    queue.pause_active("id").await.expect("pause active");

    let paused = queue.active.read().await.first().cloned().expect("active download");
    assert_eq!(paused.state, RecordingTaskState::Paused);
    assert!(paused.paused);
    assert!(!paused.finished);

    queue.resume_active("id").await.expect("resume active");

    let resumed = queue.active.read().await.first().cloned().expect("active download");
    assert_eq!(resumed.state, RecordingTaskState::Running);
    assert!(!resumed.paused);
    assert!(!resumed.finished);
}

#[tokio::test]
async fn cancel_leaves_a_running_task_cancelling_until_the_worker_lets_go() {
    let queue = RecordingQueue::new();
    let worker = queue.worker("id");
    let active = task("id", RecordingKind::Vod, RecordingTaskState::Running);

    *queue.active.write().await = vec![active];
    assert_eq!(queue.cancel_requested("id").await.expect("cancel active"), Some(false));

    let cancelled = queue.active.read().await.first().cloned().expect("active download");
    assert_eq!(cancelled.state, RecordingTaskState::Cancelling);
    assert_eq!(*worker.control_signal.read().await, RecordingControl::Cancel);
    assert!(!cancelled.finished);
    assert_eq!(cancelled.error.as_deref(), Some("Cancelled by user"));
    assert!(queue.finished.read().await.is_empty());
}

#[tokio::test]
async fn cancelling_a_paused_task_releases_idle_worker_state() {
    for running in [None, Some(false), Some(true)] {
        let queue = RecordingQueue::new();
        let mut paused = task("paused", RecordingKind::Vod, RecordingTaskState::Paused);
        paused.paused = true;
        queue.active.write().await.push(paused);
        if let Some(running) = running {
            *queue.worker("paused").running.write().await = running;
        }
        let immediate = running != Some(true);
        assert_eq!(queue.cancel_requested("paused").await.expect("cancel paused"), Some(immediate));
        if immediate {
            assert!(queue.active.read().await.is_empty());
            assert_eq!(queue.finished.read().await[0].state, RecordingTaskState::Cancelled);
        } else {
            assert!(queue.finished.read().await.is_empty(), "the stream has not closed yet");
            assert_eq!(queue.active.read().await[0].state, RecordingTaskState::Cancelling);
            assert_eq!(*queue.worker("paused").control_signal.read().await, RecordingControl::Cancel);
            mutate(&queue, |candidate| {
                let mut task = candidate.active.remove(0);
                task.state = RecordingTaskState::Cancelled;
                task.finished = true;
                candidate.finished.push(task);
                Ok(())
            })
            .await
            .expect("worker closes and commits");
        }
        let retained = queue.workers.lock().expect("worker map").contains_key("paused");
        assert_eq!(retained, running == Some(true), "only an exiting worker retains its state");
        queue.release_worker("paused").await;
        assert!(!queue.workers.lock().expect("worker map").contains_key("paused"));
    }
}

#[tokio::test]
async fn worker_claims_do_not_create_state_for_missing_or_paused_tasks() {
    let queue = RecordingQueue::new();
    assert!(queue.claim_worker("missing").await.is_none());
    let mut paused = task("paused", RecordingKind::Vod, RecordingTaskState::Paused);
    paused.paused = true;
    queue.active.write().await.push(paused);
    assert!(queue.claim_worker("paused").await.is_none());
    assert!(queue.workers.lock().expect("worker map").is_empty());
    assert!(queue.resume_active("paused").await.expect("resume"));
    assert!(queue.claim_worker("paused").await.is_some());
    assert!(queue.claim_worker("paused").await.is_none(), "a worker can be claimed only once");
}

#[tokio::test]
async fn pause_active_routes_through_mutate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().to_path_buf();
    let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    let task = make_test_transfer_task("rec-1", dir.path().join("a.ts"));
    {
        let mut active = queue.active.write().await;
        *active = vec![task];
    }
    let prior_revision = queue.revision.load(Ordering::SeqCst);
    queue.pause_active("rec-1").await.expect("pause_active");
    let active = queue.active.read().await.first().cloned().expect("active");
    assert!(active.paused);
    assert_eq!(active.state, RecordingTaskState::Paused);
    assert!(active.next_retry_at.is_none());
    assert_eq!(queue.revision.load(Ordering::SeqCst), prior_revision + 1);
}
