use super::*;

#[tokio::test]
async fn recovering_multiple_active_recordings_preserves_every_task_and_its_recovery_policy(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path())?;
    let mut tasks = Vec::new();
    for index in 0..2 {
        tasks.push(RecordingQueue::to_persisted(&task(
            &format!("live-{index}"),
            RecordingKind::Live,
            RecordingTaskState::Running,
        )));
        let mut paused = task(&format!("paused-{index}"), RecordingKind::Vod, RecordingTaskState::Paused);
        paused.paused = true;
        tasks.push(RecordingQueue::to_persisted(&paused));
        tasks.push(RecordingQueue::to_persisted(&task(
            &format!("vod-{index}"),
            RecordingKind::Vod,
            RecordingTaskState::Running,
        )));
    }
    mutate(&queue, |candidate| {
        candidate.active = tasks;
        Ok(())
    })
    .await?;
    let restored = RecordingQueue::new_persistent(dir.path(), dir.path())?;
    restored.load_from_disk().await?;
    assert_eq!(restored.active.read().await.len(), 2);
    assert!(restored.active.read().await.iter().all(|task| task.state == RecordingTaskState::Paused));
    assert_eq!(restored.queue.lock().await.len(), 2);
    assert!(restored.queue.lock().await.iter().all(|task| task.state == RecordingTaskState::Queued));
    assert_eq!(restored.finished.read().await.len(), 2);
    assert!(restored
        .finished
        .read()
        .await
        .iter()
        .all(|task| task.state == RecordingTaskState::Failed && task.kind == RecordingKind::Live));
    let (_, tasks) = restored.committed_snapshot().await;
    assert_eq!(tasks.len(), 6);
    assert!(!restored.workers_running().await);
    Ok(())
}

#[test]
fn an_interrupted_live_capture_fails_instead_of_restarting() {
    // The broadcast that played while the process was down is gone. A
    // resumed live capture would write a recording with a hole at the
    // front and report it as complete.
    for state in [RecordingTaskState::Running, RecordingTaskState::WaitingForCapacity] {
        let interrupted = RecordingTask { state, ..task("live", RecordingKind::Live, state) };
        let recovered = RecordingQueue::recover_loaded_task(interrupted);
        assert_eq!(recovered.state, RecordingTaskState::Failed, "live in {}", state.label());
        assert!(recovered.finished);
        assert!(recovered.error.is_some(), "the operator is told why");
    }
}

#[test]
fn an_interrupted_transfer_is_still_resumed() {
    // The counterpart: a file on a server is still there after a restart,
    // so VOD and series pick up where they left off.
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let interrupted = task("film", kind, RecordingTaskState::Running);
        let recovered = RecordingQueue::recover_loaded_task(interrupted);
        assert_eq!(recovered.state, RecordingTaskState::Queued, "{kind} must resume");
        assert!(!recovered.finished);
    }
}

#[test]
fn a_future_live_recording_survives_a_restart_still_scheduled() {
    let scheduled = task("live", RecordingKind::Live, RecordingTaskState::Scheduled);
    let recovered = RecordingQueue::recover_loaded_task(scheduled);
    assert_eq!(recovered.state, RecordingTaskState::Scheduled, "a window that has not opened is untouched");
    assert!(!recovered.finished);
}

#[test]
fn recover_loaded_task_requeues_waiting_states() {
    let waiting_for_capacity = RecordingTask {
        size: 77,
        total_size: Some(99),
        error: Some("old error".to_string()),
        state: RecordingTaskState::WaitingForCapacity,
        ..task("capacity", RecordingKind::Vod, RecordingTaskState::WaitingForCapacity)
    };
    let retry_waiting = RecordingTask { state: RecordingTaskState::RetryWaiting, ..waiting_for_capacity.clone() };

    let restored_waiting_for_capacity = RecordingQueue::recover_loaded_task(waiting_for_capacity);
    let restored_retry_waiting = RecordingQueue::recover_loaded_task(retry_waiting);

    assert_eq!(restored_waiting_for_capacity.state, RecordingTaskState::Queued);
    assert!(!restored_waiting_for_capacity.paused);
    assert!(restored_waiting_for_capacity.error.is_none());

    assert_eq!(restored_retry_waiting.state, RecordingTaskState::Queued);
    assert!(!restored_retry_waiting.paused);
    assert!(restored_retry_waiting.error.is_none());
}

#[test]
fn recover_loaded_task_clears_pending_retry_timestamp() {
    let retry_waiting = RecordingTask {
        size: 12,
        total_size: Some(20),
        error: Some("retrying".to_string()),
        state: RecordingTaskState::RetryWaiting,
        retry_attempts: 2,
        next_retry_at: Some(1_700_000_000),
        ..task("retry", RecordingKind::Vod, RecordingTaskState::RetryWaiting)
    };

    let restored = RecordingQueue::recover_loaded_task(retry_waiting);
    assert_eq!(restored.state, RecordingTaskState::Queued);
    assert_eq!(restored.retry_attempts, 0);
    assert!(restored.next_retry_at.is_none());
}

#[tokio::test]
async fn retry_finished_clears_retry_metadata() {
    let queue = RecordingQueue::new();
    queue.finished.write().await.push(RecordingTask {
        finished: true,
        error: Some("Retry limit reached".to_string()),
        state: RecordingTaskState::Failed,
        retry_attempts: 5,
        next_retry_at: Some(1_700_000_000),
        ..task("done", RecordingKind::Vod, RecordingTaskState::Failed)
    });

    assert!(queue.retry_finished("done").await.expect("retry finished"));
    let queued = queue.queue.lock().await.front().cloned().expect("queued download");
    assert_eq!(queued.state, RecordingTaskState::Queued);
    assert_eq!(queued.retry_attempts, 0);
    assert!(queued.next_retry_at.is_none());
    assert!(queued.error.is_none());
}

#[tokio::test]
async fn retry_finished_rejects_recordings() {
    let queue = RecordingQueue::new();
    queue.finished.write().await.push(RecordingTask {
        finished: true,
        error: Some("Cancelled by user".to_string()),
        state: RecordingTaskState::Cancelled,
        recording: live_meta("web:alice", 1_700_000_000, 300),
        ..task("recording", RecordingKind::Live, RecordingTaskState::Cancelled)
    });

    assert!(!queue.retry_finished("recording").await.expect("reject recording retry"));
    assert!(queue.queue.lock().await.is_empty());
    assert_eq!(queue.finished.read().await.len(), 1);
}

#[tokio::test]
async fn request_worker_restart_sets_restart_control_and_notifies_waiters() {
    let queue = RecordingQueue::new();
    let worker = queue.worker("task");
    let waiter_queue = Arc::clone(&queue.slot_waiters);
    let control_signal = Arc::clone(&worker.control_signal);
    let control_notify = Arc::clone(&worker.control_notify);

    let waiter =
        tokio::spawn(async move { waiter_queue.wait(None, 0, control_signal.as_ref(), control_notify.as_ref()).await });

    tokio::task::yield_now().await;
    queue.request_worker_restart();

    assert_eq!(
        timeout(Duration::from_millis(100), waiter).await.expect("waiter finished").expect("join ok"),
        RecordingWaitOutcome::Restarted
    );
    assert_eq!(*worker.control_signal.read().await, RecordingControl::Restart);
}

#[tokio::test]
async fn a_restart_never_replaces_a_pending_pause_or_cancel() {
    // A configuration reload must not turn a user's pause into a requeue
    // or swallow a cancel, whichever of the two writes lands first.
    for pending in [RecordingControl::Pause, RecordingControl::Cancel] {
        let queue = RecordingQueue::new();
        let worker = queue.worker("task");
        *worker.control_signal.write().await = pending;
        queue.request_worker_restart();
        assert_eq!(*worker.control_signal.read().await, pending);

        // The deferred path, taken while another writer holds the lock.
        let held = worker.control_signal.write().await;
        queue.request_worker_restart();
        drop(held);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(*worker.control_signal.read().await, pending);
    }
}

#[tokio::test]
async fn retry_finished_after_failed_state_commits_one_revision() {
    let queue = RecordingQueue::new();
    let finished = RecordingTask {
        finished: true,
        state: RecordingTaskState::Failed,
        ..task("done", RecordingKind::Vod, RecordingTaskState::Failed)
    };
    queue.finished.write().await.push(finished);

    assert!(queue.retry_finished("done").await.expect("retry commit"));

    assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
    assert!(queue.finished.read().await.is_empty());
    assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("done"));
}
