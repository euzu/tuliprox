use super::*;

#[tokio::test]
async fn mutate_persists_and_increments_revision_on_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().to_path_buf();
    let queue = RecordingQueue::new_persistent(&state_dir, &state_dir).expect("open recording repository");
    assert_eq!(queue.revision.load(Ordering::SeqCst), 0);

    // Insert one recording so a candidate is non-empty.
    let task = make_test_recording_task("rec-1", dir.path().join("a.ts"));
    queue.queue.lock().await.push_back(task);

    let result: Result<(), QueueMutationError> = mutate(&queue, |_candidate| Ok(())).await;
    assert!(result.is_ok(), "mutate should succeed: {result:?}");
    // The first committed mutation publishes revision 1, both in
    // memory and in the repository.
    assert_eq!(queue.revision.load(Ordering::SeqCst), 1, "counter must store the new value");
    let (revision, uuids) = committed(&queue).await;
    assert_eq!(revision, 1, "repository carries the candidate's revision");
    assert_eq!(uuids, vec!["rec-1".to_string()]);
}

#[tokio::test]
async fn mutate_invalid_candidate_keeps_existing_repository_memory_and_revision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().to_path_buf();
    let queue = RecordingQueue::new_persistent(&state_dir, &state_dir).expect("open recording repository");
    let initial = make_test_recording_task("rec-1", dir.path().join("a.ts"));
    let persisted = RecordingQueue::to_persisted(&initial);
    mutate(&queue, |candidate| {
        candidate.queue.push(persisted);
        Ok(())
    })
    .await
    .expect("initial commit");
    let committed_before = committed(&queue).await;

    let result: Result<(), QueueMutationError> = mutate(&queue, |candidate| {
        if let Some(download) = candidate.queue.first_mut() {
            download.url = "not a url".to_string();
        }
        Ok(())
    })
    .await;

    assert!(result.is_err());
    assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
    // A candidate that cannot be converted back is rejected before the
    // repository is touched, so the committed set is byte-identical.
    assert_eq!(committed(&queue).await, committed_before);
    assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("rec-1"));
}

#[tokio::test]
async fn concurrent_writers_serialize_and_increment_each_revision() {
    let queue = Arc::new(RecordingQueue::new());
    queue.queue.lock().await.push_back(make_test_recording_task("remove", PathBuf::from("/tmp/remove.ts")));
    let finished = RecordingTask {
        finished: true,
        state: RecordingTaskState::Failed,
        ..task("retry", RecordingKind::Vod, RecordingTaskState::Failed)
    };
    queue.finished.write().await.push(finished);

    let remove_queue = Arc::clone(&queue);
    let retry_queue = Arc::clone(&queue);
    let (removed, retried) = tokio::join!(async move { remove_queue.remove_from_queue("remove").await }, async move {
        retry_queue.retry_finished("retry").await
    },);

    assert!(removed.expect("remove commit"));
    assert!(retried.expect("retry commit"));
    assert_eq!(queue.revision.load(Ordering::SeqCst), 2);
    assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("retry"));
    assert!(queue.finished.read().await.is_empty());
}

#[tokio::test]
async fn same_value_control_is_published_after_worker_commit_and_clear() {
    let queue = Arc::new(RecordingQueue::new());
    let worker = queue.worker("task-a");
    *queue.active.write().await = vec![make_test_recording_task("task-a", PathBuf::from("/tmp/task-a.ts"))];
    queue.queue.lock().await.push_back(make_test_recording_task("task-b", PathBuf::from("/tmp/task-b.ts")));
    let mut control_lock = worker.control_signal.write().await;
    *control_lock = RecordingControl::Cancel;
    let worker_queue = Arc::clone(&queue);
    let worker_commit = tokio::spawn(async move {
        worker_queue
            .mutate_optional_and_clear_control("task-a", RecordingControl::Cancel, |candidate| {
                let Some(mut active) = candidate.active.pop() else {
                    return Ok(None);
                };
                active.finished = true;
                candidate.finished.push(active);
                if !candidate.queue.is_empty() {
                    candidate.active = vec![candidate.queue.remove(0)];
                }
                Ok(Some(true))
            })
            .await
    });

    for _ in 0..100 {
        if queue.revision.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
    let api_queue = Arc::clone(&queue);
    let newer_cancel = tokio::spawn(async move { api_queue.cancel_requested("task-b").await });
    tokio::task::yield_now().await;
    assert!(!newer_cancel.is_finished());

    drop(control_lock);

    assert!(worker_commit.await.expect("worker task").expect("worker commit").is_some());
    assert_eq!(newer_cancel.await.expect("cancel task").expect("cancel commit"), Some(false));
    assert_eq!(*queue.worker("task-b").control_signal.read().await, RecordingControl::Cancel);
}
