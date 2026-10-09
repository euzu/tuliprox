use super::*;

#[tokio::test]
async fn removing_the_last_failed_or_cancelled_entry_removes_its_partial() {
    for state in [RecordingTaskState::Failed, RecordingTaskState::Cancelled] {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let (final_path, partial) = finished_film(&queue, dir.path(), "recording", "web:alice", state).await;
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());

        assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
        assert!(!partial.exists(), "{state:?}: a partial nothing will resume is not kept");
        assert!(final_path.exists(), "{state:?}: a final file is never removed by Remove");
    }
}

#[test]
fn a_download_claiming_the_path_during_the_cleanup_keeps_its_partial() {
    // The orphan check and the unlink happen under one mutation guard. A
    // download admitted after the removal may take over the path, and
    // must find its partial intact.
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().expect("runtime");
    runtime.block_on(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new());
        let (final_path, partial) =
            finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Failed).await;
        std::fs::remove_file(&final_path).expect("only the partial exists");
        let mut newcomer =
            persisted_media("newcomer", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
        newcomer.file_path.clone_from(&final_path);
        newcomer.state = RecordingTaskState::Running;

        // Park the filesystem: the cleanup's unlink waits on the only
        // blocking thread until released.
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            parked_tx.send(()).expect("parked");
            let _ = release_rx.recv();
        });
        parked_rx.recv().expect("blocking thread taken");

        let service = RecordingService::new(Arc::clone(&queue), test_app_config());
        let removal = tokio::spawn(async move { service.remove_recording_task(&deleting_claims(), "recording").await });
        while !queue.finished.read().await.is_empty() {
            tokio::task::yield_now().await;
        }
        // The newcomer is admitted and starts writing, as soon as it can.
        let claim_queue = Arc::clone(&queue);
        let claim_partial = partial.clone();
        let claim = tokio::spawn(async move {
            mutate(&claim_queue, move |candidate| {
                candidate.active = vec![newcomer.clone()];
                Ok(())
            })
            .await
            .expect("admit newcomer");
            tokio::fs::write(&claim_partial, b"newcomer's bytes").await.expect("newcomer writes");
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        release_tx.send(()).expect("release");
        blocker.await.expect("blocker");
        assert!(removal.await.expect("join").expect("remove"));
        claim.await.expect("claim");

        assert_eq!(queue.active.read().await.first().map(|task| task.uuid.clone()).as_deref(), Some("newcomer"));
        assert!(partial.exists(), "the newcomer's partial must survive the cleanup of the old entry");
    });
}

#[tokio::test]
async fn cancelling_an_active_recording_leaves_it_worker_owned_until_the_worker_finishes() {
    // A cancel request leaves the active task to its worker: it must not
    // show as finished while the worker is still writing, and removing it
    // in that state must be refused rather than reported as done.
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let mut task = RecordingQueue::from_persisted(persisted_media(
        "recording",
        "web:alice",
        RecordingVisibility::Private,
        "http://provider/film.mp4",
    ))
    .expect("valid recording task");
    task.state = RecordingTaskState::Running;
    *downloads.active.write().await = vec![task];

    let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
    let claims = shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    };

    service.cancel_recording(&claims, "recording").await.expect("cancel request accepted");

    let active = downloads.active.read().await.first().cloned().expect("the worker still owns it");
    assert_eq!(active.state, RecordingTaskState::Cancelling, "the worker commits the terminal state");
    assert!(downloads.finished.read().await.is_empty(), "nothing terminal yet");
    assert!(
        matches!(service.remove_recording_task(&claims, "recording").await, Err(ServiceError::InvalidState)),
        "removing a worker-owned recording must be refused, not silently ignored"
    );
}

#[test]
fn sanitize_filename_collapses_runs_and_drops_control_chars() {
    assert_eq!(sanitize_filename_component("a///b"), "a_b");
    assert_eq!(sanitize_filename_component("a\u{7}\u{1}b"), "a_b");
}

#[test]
fn sanitize_filename_drops_invisible_formatting() {
    // A right-to-left override renders the name differently from the
    // bytes on disk; it must leave no trace at all.
    assert_eq!(sanitize_filename_component("news\u{202e}sj.ts"), "newssj.ts");
    assert_eq!(sanitize_filename_component("a\u{200b}b"), "ab");
}

#[test]
fn cancel_future_rule_recordings_moves_only_matching_future_tasks() {
    let now = 1_700_000_000;
    let mut queue = PersistedRecordingQueue::default();
    queue.scheduled.push(persisted_rule_recording("future-match", Some("rule-1"), now + 60));
    queue.scheduled.push(persisted_rule_recording("past-match", Some("rule-1"), now - 60));
    queue.queue.push(persisted_rule_recording("other-rule", Some("rule-2"), now + 60));

    let cancelled = cancel_future_rule_recordings_in_candidate(&mut queue, "rule-1", now);

    assert_eq!(cancelled.len(), 1);
    assert_eq!(queue.scheduled.len(), 1);
    assert_eq!(queue.queue.len(), 1);
    assert_eq!(queue.finished.len(), 1);
    let task = &queue.finished[0];
    assert_eq!(task.uuid, "future-match");
    assert_eq!(task.state, RecordingTaskState::Cancelled);
    assert!(task.finished);
    assert_eq!(task.recording.reserved_bytes, 0);
}
