use super::*;

#[tokio::test]
async fn persisted_queue_round_trips_and_requeues_running_downloads() {
    let state_file = temp_state_file("download_state");
    let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    let queued = RecordingTask {
        size: 10,
        total_size: Some(100),
        ..task("queued", RecordingKind::Vod, RecordingTaskState::Queued)
    };
    let active = RecordingTask {
        size: 20,
        total_size: Some(200),
        ..task("active", RecordingKind::Vod, RecordingTaskState::Running)
    };
    let paused = RecordingTask {
        size: 30,
        total_size: Some(300),
        paused: true,
        state: RecordingTaskState::Paused,
        ..task("paused", RecordingKind::Vod, RecordingTaskState::Paused)
    };

    queue.queue.lock().await.push_back(queued);
    *queue.active.write().await = vec![active];
    queue.finished.write().await.push(paused.clone());
    queue.persist_to_disk().await.expect("persist state");

    let restored = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    restored.load_from_disk().await.expect("load state");

    assert_eq!(restored.queue.lock().await.len(), 2);
    let restored_active = restored.active.read().await.clone();
    assert!(restored_active.is_empty());
    let restored_finished = restored.finished.read().await.clone();
    assert_eq!(restored_finished.len(), 1);
    assert_eq!(restored_finished[0].uuid, paused.uuid);

    let queued_items = restored.queue.lock().await.iter().map(|d| d.uuid.clone()).collect::<Vec<_>>();
    assert!(queued_items.iter().any(|id| id == "queued"));
    assert!(queued_items.iter().any(|id| id == "active"));

    let _ = std::fs::remove_dir_all(state_file);
}

#[tokio::test]
async fn persisted_scheduled_recordings_round_trip_without_becoming_active() {
    let state_file = temp_state_file("record_state");
    let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    let future_start = Utc::now().timestamp().saturating_add(3_600);
    let scheduled = RecordingTask {
        url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
        state: RecordingTaskState::Scheduled,
        recording: live_meta("web:alice", future_start, 5_400),
        ..task("recording", RecordingKind::Live, RecordingTaskState::Scheduled)
    };

    queue.scheduled.write().await.push(scheduled.clone());
    queue.persist_to_disk().await.expect("persist state");

    let restored = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    restored.load_from_disk().await.expect("load state");

    assert!(restored.active.read().await.is_empty());
    assert_eq!(restored.queue.lock().await.len(), 0);
    let restored_scheduled = restored.scheduled.read().await.clone();
    assert_eq!(restored_scheduled.len(), 1);
    assert_eq!(restored_scheduled[0].uuid, scheduled.uuid);
    assert_eq!(restored_scheduled[0].state, RecordingTaskState::Scheduled);
    assert_eq!(restored_scheduled[0].scheduled_start(), Some(future_start));
    assert_eq!(restored_scheduled[0].scheduled_end(), Some(future_start + 5_400));
    assert_eq!(restored_scheduled[0].scheduled_duration_secs(), Some(5_400));
    assert_eq!(restored_scheduled[0].kind, RecordingKind::Live);

    let _ = std::fs::remove_dir_all(state_file);
}

#[tokio::test]
async fn load_from_disk_moves_expired_scheduled_recordings_to_finished() {
    let state_file = temp_state_file("expired_record_state");
    let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    let expired = RecordingTask {
        state: RecordingTaskState::Scheduled,
        recording: live_meta("web:alice", 100, 60),
        ..task("expired", RecordingKind::Live, RecordingTaskState::Scheduled)
    };

    queue.scheduled.write().await.push(expired);
    queue.persist_to_disk().await.expect("persist state");

    let restored = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
    restored.load_from_disk().await.expect("load state");

    assert!(restored.scheduled.read().await.is_empty());
    let finished = restored.finished.read().await.clone();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].uuid, "expired");
    assert_eq!(finished[0].state, RecordingTaskState::Failed);
    assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));

    let _ = std::fs::remove_dir_all(state_file);
}

#[test]
fn download_uuid_differs_for_same_url_with_different_filenames() {
    let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp".to_string()),
        ..Default::default()
    });

    let first = RecordingTask::new(
        RecordingKind::Vod,
        "https://example.com/video.mp4",
        "first.mp4",
        &cfg,
        None,
        0,
        media_meta("web:alice"),
    )
    .expect("first download");
    let second = RecordingTask::new(
        RecordingKind::Vod,
        "https://example.com/video.mp4",
        "second.mp4",
        &cfg,
        None,
        0,
        media_meta("web:alice"),
    )
    .expect("second download");

    assert_ne!(first.uuid, second.uuid);
}

#[test]
fn a_name_taken_on_disk_is_numbered_from_the_original_stem() {
    // Collision suffixes number the original stem: the third copy is
    // `film_2.mp4`, not `film_1_2.mp4`.
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("film.mp4"), b"").expect("first copy");
    std::fs::write(dir.path().join("film_1.mp4"), b"").expect("second copy");
    let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some(dir.path().to_string_lossy().into_owned()),
        ..Default::default()
    });

    let task = RecordingTask::new(
        RecordingKind::Vod,
        "https://example.com/film.mp4",
        "film.mp4",
        &cfg,
        None,
        0,
        media_meta("web:alice"),
    )
    .expect("a free name");

    assert_eq!(task.filename, "film_2.mp4");
}

#[test]
fn download_new_omits_trailing_dot_when_filename_has_no_extension() {
    let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp".to_string()),
        ..Default::default()
    });

    let task = RecordingTask::new(
        RecordingKind::Vod,
        "https://example.com/live",
        "title with trailing dot.",
        &cfg,
        None,
        0,
        media_meta("web:alice"),
    )
    .expect("download");

    assert_eq!(task.filename, "title_with_trailing_dot");
    assert!(!task.filename.ends_with('.'));
}

#[test]
fn task_filename_keeps_extension_dots_and_letters_of_every_script() {
    let mut meta = live_meta("web:alice", 1_700_000_000, 1_800);
    meta.group = Some("Новости".to_string());
    let task = organized_task(RecordingKind::Live, "Mr. Robot — Çalıkuşu 🎬.ts", meta);
    assert_eq!(task.recording.relative_path.as_deref(), Some("Новости/Mr._Robot_Çalıkuşu.ts"));
}

#[tokio::test]
async fn a_commit_whose_checkpoint_fails_is_kept_in_memory_and_on_disk() {
    // The checkpoint runs after the batch is durable. The queue keeps the
    // committed record, and the next commit does not delete it again.
    let dir = tempfile::TempDir::new().expect("tempdir");
    drop(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    force_a_failing_checkpoint(dir.path());
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("reopen repository");

    let committed = RecordingQueue::to_persisted(&task("kept", RecordingKind::Vod, RecordingTaskState::Completed));
    mutate(&queue, move |candidate| {
        candidate.finished.push(committed.clone());
        Ok(())
    })
    .await
    .expect("the commit succeeds although its checkpoint does not");
    assert_eq!(queue.finished.read().await.len(), 1);

    // A second commit diffs against that state rather than deleting it.
    let another = RecordingQueue::to_persisted(&task("next", RecordingKind::Vod, RecordingTaskState::Completed));
    mutate(&queue, move |candidate| {
        candidate.finished.push(another.clone());
        Ok(())
    })
    .await
    .expect("second commit");
    drop(queue);

    let reopened = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("reopen repository");
    reopened.load_from_disk().await.expect("load");
    let mut uuids: Vec<String> = reopened.finished.read().await.iter().map(|task| task.uuid.clone()).collect();
    uuids.sort();
    assert_eq!(uuids, ["kept", "next"]);
}

#[test]
fn from_persisted_rejects_invalid_url() {
    let mut p = RecordingQueue::to_persisted(&task("bad-url", RecordingKind::Vod, RecordingTaskState::Queued));
    p.url = "not a url at all".to_string();
    let result = RecordingQueue::from_persisted(p);
    assert!(result.is_err(), "invalid url must surface as an error");
    assert!(matches!(result.unwrap_err(), PersistedError::InvalidUrl(_)), "must surface the parse error");
}

#[tokio::test]
async fn mutate_keeps_state_when_closure_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().to_path_buf();
    let queue = RecordingQueue::new_persistent(&state_dir, &state_dir).expect("open recording repository");
    let original_revision = queue.revision.load(Ordering::SeqCst);
    let original_len = queue.queue.lock().await.len();

    let result: Result<(), QueueMutationError> =
        mutate(&queue, |_candidate| Err(QueueMutationError::new("validation failed"))).await;
    assert!(result.is_err(), "closure error should propagate");
    assert_eq!(queue.queue.lock().await.len(), original_len, "queue must stay unchanged");
    assert_eq!(queue.revision.load(Ordering::SeqCst), original_revision);
    let (revision, uuids) = committed(&queue).await;
    assert_eq!(revision, 0, "nothing may be committed on closure error");
    assert_eq!(uuids.len(), 0);
}

#[tokio::test]
async fn committed_snapshot_waits_for_mutation_boundary() {
    let queue = std::sync::Arc::new(RecordingQueue::new());
    let task = make_test_recording_task("rec-1", PathBuf::from("/tmp/rec-1.ts"));
    queue.queue.lock().await.push_back(task);

    let mutation_guard = queue.mutation_guard.lock().await;
    let snapshot_queue = std::sync::Arc::clone(&queue);
    let snapshot = tokio::spawn(async move { snapshot_queue.committed_snapshot().await });

    tokio::task::yield_now().await;
    assert!(!snapshot.is_finished(), "snapshot must wait for the mutation boundary");

    drop(mutation_guard);
    let Ok((revision, tasks)) = snapshot.await else {
        unreachable!("snapshot task failed");
    };
    assert_eq!(revision, QueueRevision(0));
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks.first().map(|task| task.uuid.as_str()), Some("rec-1"));
}

#[tokio::test]
async fn committed_partitioned_snapshot_waits_for_mutation_boundary() {
    let queue = Arc::new(RecordingQueue::new());
    let mutation_guard = queue.mutation_guard.lock().await;
    let snapshot_queue = Arc::clone(&queue);
    let snapshot = tokio::spawn(async move { snapshot_queue.committed_partitioned_snapshot().await });

    tokio::task::yield_now().await;
    assert!(!snapshot.is_finished());

    drop(mutation_guard);
    let Ok((queued, active, finished)) = snapshot.await else {
        unreachable!("snapshot task failed");
    };
    assert!(queued.is_empty());
    assert!(active.is_empty());
    assert!(finished.is_empty());
}

#[tokio::test]
async fn control_signal_is_ordered_inside_mutation_guard() {
    let queue = Arc::new(RecordingQueue::new());
    let worker = queue.worker("active");
    *queue.active.write().await = vec![make_test_transfer_task("active", PathBuf::from("/tmp/active.ts"))];
    let control_lock = worker.control_signal.write().await;
    let pause_queue = Arc::clone(&queue);
    let pause = tokio::spawn(async move { pause_queue.pause_active("active").await });

    for _ in 0..100 {
        if queue.revision.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
    let snapshot_queue = Arc::clone(&queue);
    let snapshot = tokio::spawn(async move { snapshot_queue.committed_snapshot().await });
    tokio::task::yield_now().await;
    assert!(!snapshot.is_finished(), "mutation guard must remain held until control publication");

    drop(control_lock);
    assert!(pause.await.is_ok_and(|result| result.is_ok()));
    assert!(snapshot.await.is_ok());
    assert_eq!(*worker.control_signal.read().await, RecordingControl::Pause);
}

#[tokio::test]
async fn mutate_swap_restores_in_memory_state_from_persisted_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().to_path_buf();
    let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");

    let persisted = RecordingQueue::to_persisted(&make_test_recording_task("rec-1", dir.path().join("a.ts")));
    mutate(&queue, |candidate| {
        candidate.queue.push(persisted);
        Ok(())
    })
    .await
    .expect("first mutate");

    let len_after_commit = queue.queue.lock().await.len();
    assert_eq!(len_after_commit, 1, "committed task must be in memory");

    // Now mutate again to remove that task. The candidate should be
    // re-built from the just-committed state, not from the empty
    // pre-mutate in-memory state.
    mutate(&queue, |candidate| {
        candidate.queue.retain(|d| d.uuid != "rec-1");
        Ok(())
    })
    .await
    .expect("second mutate");

    assert!(queue.queue.lock().await.is_empty(), "remove must propagate to in-memory state");
    assert_eq!(queue.revision.load(Ordering::SeqCst), 2);
}

// --- load_from_disk rejection paths ---

#[tokio::test]
async fn load_from_disk_rejects_invalid_url_in_persisted_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().to_path_buf();
    let mut persisted = RecordingQueue::to_persisted(&task("rec-1", RecordingKind::Vod, RecordingTaskState::Queued));
    persisted.url = "not a url".to_string();

    let queue = RecordingQueue::new_persistent(&state_dir, &state_dir).expect("open recording repository");
    queue.commit_records(1, vec![persisted], None).await.expect("commit unparseable record");

    let result = queue.load_from_disk().await;

    assert!(result.is_err(), "invalid url must surface as an error");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    // Rejection happens before assignment, so memory stays empty and the
    // repository is left for an operator to inspect.
    assert!(queue.queue.lock().await.is_empty());
    assert!(queue.scheduled.read().await.is_empty());
    assert!(queue.finished.read().await.is_empty());
    assert!(queue.active.read().await.is_empty());
}

#[test]
fn reserve_recording_relative_path_returns_numbered_stem_when_no_collision() {
    let mut candidate = PersistedRecordingQueue::default();
    let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
    assert_eq!(reserved, "pilot_1", "numbered suffix is the first candidate");
}

#[test]
fn reserve_recording_relative_path_skips_existing_paths() {
    let mut candidate = PersistedRecordingQueue::default();
    let occupied = RecordingMetadata { relative_path: Some("pilot_1".to_string()), ..media_meta("web:alice") };
    candidate.queue.push(persisted_recording("other", occupied));
    let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
    assert_eq!(reserved, "pilot_2");
}

#[test]
fn reserve_recording_relative_path_does_not_double_bump_for_self() {
    let mut candidate = PersistedRecordingQueue::default();
    let existing = RecordingMetadata { relative_path: Some("pilot_3".to_string()), ..media_meta("web:alice") };
    candidate.queue.push(persisted_recording("rec-1", existing));
    let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
    assert_eq!(reserved, "pilot_3", "must not bump because of self");
}
