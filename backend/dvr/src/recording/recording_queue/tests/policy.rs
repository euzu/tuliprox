use super::*;

#[tokio::test]
async fn promotion_precheck_matches_promotion_for_mixed_queues() {
    fn fixture(uuid: &str, media: usize, kind: RecordingKind, state: RecordingTaskState) -> RecordingTask {
        let mut entry = task(&format!("media-{media}"), kind, state);
        entry.uuid = uuid.to_string();
        entry.recording = media_meta("web:alice");
        entry.recording.source.virtual_id = media.to_string();
        match media % 3 {
            1 => {
                entry.recording.program_start = Some(10);
                entry.recording.program_end = Some(20);
            }
            2 => {
                entry.recording.provenance.rule_id = Some("rule".to_string());
                entry.recording.provenance.occurrence_key = Some(media.to_string());
            }
            _ => {}
        }
        entry.paused = state == RecordingTaskState::Paused;
        entry.finished =
            matches!(state, RecordingTaskState::Completed | RecordingTaskState::Failed | RecordingTaskState::Cancelled);
        entry
    }
    let kinds = [RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series];
    let active_states = [
        RecordingTaskState::Running,
        RecordingTaskState::Paused,
        RecordingTaskState::WaitingForCapacity,
        RecordingTaskState::RetryWaiting,
    ];
    let finished_states = [RecordingTaskState::Completed, RecordingTaskState::Failed, RecordingTaskState::Cancelled];
    let mut random = fastrand::Rng::with_seed(0x7265_636f_7264);
    let (mut promotable, mut blocked, mut attachment_only) = (0, 0, 0);
    for case in 0..512 {
        let queue = RecordingQueue::new();
        for index in 0..random.usize(0..12) {
            queue.queue.lock().await.push_back(fixture(
                &format!("queued-{index}"),
                random.usize(0..6),
                kinds[random.usize(0..kinds.len())],
                RecordingTaskState::Queued,
            ));
        }
        for index in 0..random.usize(0..4) {
            queue.active.write().await.push(fixture(
                &format!("active-{index}"),
                random.usize(0..6),
                kinds[random.usize(0..kinds.len())],
                active_states[random.usize(0..active_states.len())],
            ));
        }
        for index in 0..random.usize(0..6) {
            queue.finished.write().await.push(fixture(
                &format!("finished-{index}"),
                random.usize(0..6),
                kinds[random.usize(0..kinds.len())],
                finished_states[random.usize(0..finished_states.len())],
            ));
        }
        let precheck = queue.has_promotable_queued().await;
        let mut snapshot = queue.snapshot_current(QueueRevision(0)).await;
        let queued_before = snapshot.queue.len();
        let promoted = promote_from_queue(&mut snapshot).is_some();
        let attached = snapshot.queue.len() < queued_before;
        assert_eq!(precheck, promoted || attached, "case {case}: {snapshot:?}");
        if precheck {
            promotable += 1;
        } else {
            blocked += 1;
        }
        if !promoted && attached {
            attachment_only += 1;
        }
    }
    assert!(promotable > 0 && blocked > 0 && attachment_only > 0);
}

#[test]
fn bulk_transfers_stay_serial_while_live_tasks_promote_in_parallel() {
    let mut candidate = PersistedRecordingQueue::default();
    for index in 0..50 {
        let mut transfer =
            identified(&format!("transfer-{index}"), &format!("media-{index}"), RecordingTaskState::Queued);
        transfer.kind = if index % 2 == 0 { RecordingKind::Vod } else { RecordingKind::Series };
        transfer.input_name = None;
        candidate.queue.push(transfer);
    }
    for index in 0..2 {
        let mut live = identified(&format!("live-{index}"), &format!("live-media-{index}"), RecordingTaskState::Queued);
        live.kind = RecordingKind::Live;
        candidate.queue.push(live);
    }
    while promote_from_queue(&mut candidate).is_some() {}
    assert_eq!(candidate.active.len(), 3);
    assert_eq!(candidate.active.iter().filter(|task| task.kind != RecordingKind::Live).count(), 1);
    assert_eq!(candidate.queue.len(), 49);
    candidate.active.retain(|task| task.kind == RecordingKind::Live);
    assert_eq!(promote_from_queue(&mut candidate).map(|(uuid, _)| uuid).as_deref(), Some("transfer-1"));
}

#[test]
fn the_dto_offers_exactly_what_the_transition_graph_permits() {
    // The frontend renders its buttons straight from this set, so a
    // mismatch here is a button that errors when pressed.
    for kind in [RecordingKind::Live, RecordingKind::Vod, RecordingKind::Series] {
        for state in [
            RecordingTaskState::Scheduled,
            RecordingTaskState::Queued,
            RecordingTaskState::WaitingForCapacity,
            RecordingTaskState::Running,
            RecordingTaskState::Paused,
            RecordingTaskState::RetryWaiting,
            RecordingTaskState::Completed,
            RecordingTaskState::Failed,
            RecordingTaskState::Cancelled,
        ] {
            let dto = task("t", kind, state).to_view(true);
            assert_eq!(
                dto.allowed_actions,
                recording_transition::allowed_actions(kind, state),
                "{kind} in {} disagrees with the graph",
                state.label()
            );
        }
    }
}

#[tokio::test]
async fn a_transfer_runs_at_the_strongest_priority_anyone_attached_asked_for() {
    // Alice's background request is producing the file; Bob wants the same
    // media urgently. One transfer serves both, so it must not sit in the
    // background queue while Bob waits for bytes already being fetched.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
    let mut background = task("alice", RecordingKind::Vod, RecordingTaskState::Running);
    background.priority = 5;
    let mut foreground = task("bob", RecordingKind::Vod, RecordingTaskState::Queued);
    foreground.priority = -3;
    // Same media: a VOD identity keys on the url, so both entries name one.
    foreground.url = background.url.clone();
    let (background, foreground) =
        (RecordingQueue::to_persisted(&background), RecordingQueue::to_persisted(&foreground));
    assert_eq!(background.media_identity, foreground.media_identity, "fixture must share one media");
    mutate(&queue, move |candidate| {
        candidate.active = vec![background.clone()];
        candidate.queue.push(foreground.clone());
        Ok(())
    })
    .await
    .expect("seed");

    let (_input, priority) = queue.active_scheduling_priority("alice").await.expect("an active transfer");
    assert_eq!(priority, -3, "the transfer inherits Bob's urgency");
}

#[tokio::test]
async fn an_unrelated_urgent_recording_does_not_lift_this_one() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
    let mut background = task("alice", RecordingKind::Vod, RecordingTaskState::Running);
    background.priority = 5;
    let mut other = task("bob", RecordingKind::Vod, RecordingTaskState::Queued);
    other.priority = -3;
    let (background, other) = (RecordingQueue::to_persisted(&background), RecordingQueue::to_persisted(&other));
    assert_ne!(background.media_identity, other.media_identity, "fixture must be different media");
    mutate(&queue, move |candidate| {
        candidate.active = vec![background.clone()];
        candidate.queue.push(other.clone());
        Ok(())
    })
    .await
    .expect("seed");

    let (_input, priority) = queue.active_scheduling_priority("alice").await.expect("an active transfer");
    assert_eq!(priority, 5, "priority is shared only with entries on the same file");
}

#[test]
fn a_queued_entry_attaches_to_a_file_another_entry_finished() {
    let candidate = PersistedRecordingQueue {
        finished: vec![identified("a", "film-42", RecordingTaskState::Completed)],
        ..PersistedRecordingQueue::default()
    };
    let queued = identified("b", "film-42", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::AttachTo(0));
}

#[test]
fn a_failed_sibling_is_not_attached_to() {
    // There is no file to adopt, so the entry has to run.
    let candidate = PersistedRecordingQueue {
        finished: vec![identified("a", "film-42", RecordingTaskState::Failed)],
        ..PersistedRecordingQueue::default()
    };
    let queued = identified("b", "film-42", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::Execute);
}

#[test]
fn an_unidentified_entry_never_matches_another() {
    // An empty identity means the identity could not be resolved. Treating
    // two of them as the same recording would merge two different files.
    let candidate = PersistedRecordingQueue {
        finished: vec![identified("a", "", RecordingTaskState::Completed)],
        active: vec![identified("c", "", RecordingTaskState::Running)],
        ..PersistedRecordingQueue::default()
    };
    let queued = identified("b", "", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::Execute);
}

#[test]
fn different_media_does_not_attach() {
    let candidate = PersistedRecordingQueue {
        finished: vec![identified("a", "film-42", RecordingTaskState::Completed)],
        ..PersistedRecordingQueue::default()
    };
    let queued = identified("b", "film-99", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::Execute);
}

#[test]
fn the_entry_behind_a_finished_transfer_attaches_instead_of_downloading_again() {
    // The live path: Alice's transfer completes and Bob is next in the
    // queue for the same film. Taking the queue head blindly would start a
    // second transfer over the file Alice just produced.
    let mut candidate = PersistedRecordingQueue {
        finished: vec![identified("alice", "film-42", RecordingTaskState::Completed)],
        queue: vec![identified("bob", "film-42", RecordingTaskState::Queued)],
        ..PersistedRecordingQueue::default()
    };
    let promoted = promote_from_queue(&mut candidate);
    assert!(promoted.is_none(), "nothing needs to run");
    assert!(candidate.active.is_empty());
    assert!(candidate.queue.is_empty(), "bob left the queue");
    let bob = candidate.finished.iter().find(|task| task.uuid == "bob").expect("bob is filed");
    assert_eq!(bob.state, RecordingTaskState::Completed);
}

#[test]
fn an_entry_for_other_media_behind_a_finished_transfer_still_runs() {
    let mut candidate = PersistedRecordingQueue {
        finished: vec![identified("alice", "film-42", RecordingTaskState::Completed)],
        queue: vec![identified("bob", "film-99", RecordingTaskState::Queued)],
        ..PersistedRecordingQueue::default()
    };
    let promoted = promote_from_queue(&mut candidate);
    assert_eq!(promoted.map(|(uuid, _)| uuid), Some("bob".to_string()));
    assert_eq!(candidate.active.first().map(|task| task.uuid.as_str()), Some("bob"));
}

#[test]
fn attachable_entries_are_skipped_to_reach_one_that_must_run() {
    let mut candidate = PersistedRecordingQueue {
        finished: vec![identified("alice", "film-42", RecordingTaskState::Completed)],
        queue: vec![
            identified("bob", "film-42", RecordingTaskState::Queued),
            identified("carol", "film-42", RecordingTaskState::Queued),
            identified("dave", "film-99", RecordingTaskState::Queued),
        ],
        ..PersistedRecordingQueue::default()
    };
    let promoted = promote_from_queue(&mut candidate);
    assert_eq!(promoted.map(|(uuid, _)| uuid), Some("dave".to_string()));
    assert!(candidate.queue.is_empty(), "every entry was dispatched");
    // Bob and Carol were filed against Alice's file without running.
    assert_eq!(candidate.finished.len(), 3);
}

#[tokio::test]
async fn a_live_window_that_has_not_opened_acquires_nothing() {
    // Promotion is what puts a capture in front of the provider. A window
    // that starts in an hour must not reach it, or the DVR holds a slot for
    // an hour of nothing and records the wrong programme.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
    let now = 1_700_000_000;
    let mut upcoming = task("live", RecordingKind::Live, RecordingTaskState::Scheduled);
    upcoming.recording.scheduled_start = Some(now + 3_600);
    upcoming.recording.scheduled_end = Some(now + 7_200);
    let upcoming = RecordingQueue::to_persisted(&upcoming);
    mutate(&queue, move |candidate| {
        candidate.scheduled.push(upcoming.clone());
        Ok(())
    })
    .await
    .expect("seed");

    assert_eq!(queue.promote_due_scheduled(now).await, 0, "an hour early is not due");
    assert!(queue.queue.lock().await.is_empty(), "and nothing is queued for a provider");

    // At the padded start it becomes work.
    assert_eq!(queue.promote_due_scheduled(now + 3_600).await, 1, "due at its padded start");
    assert_eq!(queue.queue.lock().await.len(), 1);
}

#[tokio::test]
async fn promote_due_scheduled_moves_only_ready_recordings_to_queue() {
    let queue = RecordingQueue::new();
    let due = RecordingTask {
        size: 123,
        total_size: Some(999),
        error: Some("old error".to_string()),
        state: RecordingTaskState::Scheduled,
        recording: live_meta("web:alice", 100, 60),
        ..task("due", RecordingKind::Live, RecordingTaskState::Scheduled)
    };
    let future = RecordingTask {
        state: RecordingTaskState::Scheduled,
        recording: live_meta("web:alice", 200, 60),
        ..task("future", RecordingKind::Live, RecordingTaskState::Scheduled)
    };

    queue.scheduled.write().await.extend([due, future]);
    let revision = queue.revision.load(Ordering::SeqCst);

    let promoted = queue.promote_due_scheduled(150).await;

    assert_eq!(promoted, 1);
    assert_eq!(queue.revision.load(Ordering::SeqCst), revision + 1);
    let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
    assert_eq!(queued_items.len(), 1);
    assert_eq!(queued_items[0].uuid, "due");
    assert_eq!(queued_items[0].state, RecordingTaskState::Queued);
    assert_eq!(queued_items[0].size, 0);
    assert!(queued_items[0].error.is_none());
    let scheduled_items = queue.scheduled.read().await.clone();
    assert_eq!(scheduled_items.len(), 1);
    assert_eq!(scheduled_items[0].uuid, "future");
}

#[tokio::test]
async fn promote_due_scheduled_marks_expired_recordings_failed() {
    let queue = RecordingQueue::new();
    let expired = RecordingTask {
        state: RecordingTaskState::Scheduled,
        recording: live_meta("web:alice", 100, 60),
        ..task("expired", RecordingKind::Live, RecordingTaskState::Scheduled)
    };

    queue.scheduled.write().await.push(expired);
    let promoted = queue.promote_due_scheduled(200).await;

    assert_eq!(promoted, 1);
    assert!(queue.queue.lock().await.is_empty());
    let finished = queue.finished.read().await.clone();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].uuid, "expired");
    assert_eq!(finished[0].state, RecordingTaskState::Failed);
    assert!(finished[0].finished);
    assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));
}

#[test]
fn recording_uuid_differs_for_same_url_with_different_start_times() {
    let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp".to_string()),
        ..Default::default()
    });

    let first = RecordingTask::new(
        RecordingKind::Live,
        "https://example.com/live/1",
        "recording_1.ts",
        &cfg,
        None,
        0,
        live_meta("web:alice", 1_700_000_000, 5_400),
    )
    .expect("first recording");
    let second = RecordingTask::new(
        RecordingKind::Live,
        "https://example.com/live/1",
        "recording_2.ts",
        &cfg,
        None,
        0,
        live_meta("web:alice", 1_700_005_400, 5_400),
    )
    .expect("second recording");

    assert_ne!(first.uuid, second.uuid);
}

#[test]
fn organized_series_episodes_share_the_series_folder_in_their_group() {
    let episode = |title: &str| {
        let mut meta = media_meta("web:alice");
        meta.program_title = Some(title.to_string());
        meta.series_name = Some("The Show".to_string());
        meta.group = Some("Crime".to_string());
        meta
    };
    let first = organized_task(RecordingKind::Series, "The Show S01E01.mkv", episode("Pilot"));
    let second = organized_task(RecordingKind::Series, "The Show S02E05.mkv", episode("Finale"));
    assert_eq!(first.recording.relative_path.as_deref(), Some("Crime/The Show/The_Show_S01E01.mkv"));
    assert_eq!(second.recording.relative_path.as_deref(), Some("Crime/The Show/The_Show_S02E05.mkv"));
}

#[tokio::test]
async fn promote_due_scheduled_places_due_recordings_ahead_of_existing_queue_items() {
    let queue = RecordingQueue::new();
    queue.queue.lock().await.push_back(task("existing", RecordingKind::Vod, RecordingTaskState::Queued));
    queue.scheduled.write().await.extend([
        RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            ..task("due-first", RecordingKind::Live, RecordingTaskState::Scheduled)
        },
        RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 110, 60),
            ..task("due-second", RecordingKind::Live, RecordingTaskState::Scheduled)
        },
    ]);

    let promoted = queue.promote_due_scheduled(150).await;

    assert_eq!(promoted, 2);
    let queued = queue.queue.lock().await.iter().map(|download| download.uuid.clone()).collect::<Vec<_>>();
    assert_eq!(queued, vec!["due-first", "due-second", "existing"]);
}

#[tokio::test]
async fn download_slot_wait_queue_signals_matching_waiter_by_id() {
    let queue = Arc::new(RecordingSlotWaitQueue::new());
    let control_signal = Arc::new(RwLock::new(RecordingControl::None));
    let control_notify = Arc::new(Notify::new());

    let queue_for_a = Arc::clone(&queue);
    let control_signal_for_a = Arc::clone(&control_signal);
    let control_notify_for_a = Arc::clone(&control_notify);
    let waiter_a = tokio::spawn(async move {
        queue_for_a
            .wait(Some(Arc::from("input-a")), 1, control_signal_for_a.as_ref(), control_notify_for_a.as_ref())
            .await
    });

    let queue_for_b = Arc::clone(&queue);
    let control_signal_for_b = Arc::clone(&control_signal);
    let control_notify_for_b = Arc::clone(&control_notify);
    let waiter_b = tokio::spawn(async move {
        queue_for_b
            .wait(Some(Arc::from("input-b")), 0, control_signal_for_b.as_ref(), control_notify_for_b.as_ref())
            .await
    });

    let waiter_b_id = loop {
        let snapshots = queue.snapshots();
        if snapshots.len() == 2 {
            break snapshots
                .into_iter()
                .find(|waiter| waiter.input_name.as_deref() == Some("input-b"))
                .map(|waiter| waiter.id)
                .expect("waiter id for input-b");
        }
        tokio::task::yield_now().await;
    };

    assert!(queue.signal_waiter(waiter_b_id));
    assert_eq!(
        timeout(Duration::from_millis(100), waiter_b).await.expect("waiter_b finished").expect("join ok"),
        RecordingWaitOutcome::Signalled
    );

    *control_signal.write().await = RecordingControl::Cancel;
    control_notify.notify_waiters();
    assert_eq!(
        timeout(Duration::from_millis(100), waiter_a).await.expect("waiter_a finished").expect("join ok"),
        RecordingWaitOutcome::Cancelled
    );
}

#[tokio::test]
async fn a_second_request_on_an_accepted_key_is_refused_and_leaves_the_key_alone() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository");
    let record = |fingerprint: &str, recording_id: &str| PersistedIdempotency {
        principal: "web:alice".to_string(),
        key: "same-key".to_string(),
        request_fingerprint: fingerprint.to_string(),
        recording_id: recording_id.to_string(),
        accepted_at: Utc::now().timestamp(),
    };

    let first = RecordingQueue::to_persisted(&task("first", RecordingKind::Vod, RecordingTaskState::Queued));
    mutate_with_idempotency(&queue, record("first-body", "first"), move |candidate| {
        candidate.queue.push(first.clone());
        Ok(())
    })
    .await
    .expect("first request");

    let second = RecordingQueue::to_persisted(&task("second", RecordingKind::Vod, RecordingTaskState::Queued));
    let refused = mutate_with_idempotency(&queue, record("different-body", "second"), move |candidate| {
        candidate.queue.push(second.clone());
        Ok(())
    })
    .await;
    assert!(matches!(refused, Err(QueueMutationError::IdempotencyConflict)), "got {refused:?}");
    assert_eq!(queue.queue.lock().await.len(), 1, "the second request admitted nothing");
    assert_eq!(
        queue.lookup_idempotency("web:alice", "same-key", "first-body").await.expect("lookup"),
        IdempotencyOutcome::Replay { recording_id: "first".to_string() },
        "the accepted key still answers for the first request"
    );

    let replay = RecordingQueue::to_persisted(&task("again", RecordingKind::Vod, RecordingTaskState::Queued));
    let replayed = mutate_with_idempotency(&queue, record("first-body", "again"), move |candidate| {
        candidate.queue.push(replay.clone());
        Ok(())
    })
    .await;
    assert!(
        matches!(&replayed, Err(QueueMutationError::IdempotentReplay { recording_id }) if recording_id == "first"),
        "got {replayed:?}"
    );
}

#[tokio::test]
async fn wait_observes_preexisting_control_before_selecting() {
    let queue = RecordingQueue::new();
    let worker = queue.worker("task");
    *worker.control_signal.write().await = RecordingControl::Pause;

    let outcome =
        queue.slot_waiters.wait(None, 0, worker.control_signal.as_ref(), worker.control_notify.as_ref()).await;

    assert_eq!(outcome, RecordingWaitOutcome::Paused);
    assert!(queue.slot_waiters.snapshots().is_empty());
}

#[tokio::test]
async fn control_uuid_mismatch_is_noop_and_preserves_next_task() {
    let queue = RecordingQueue::new();
    let worker = queue.worker("active");
    *queue.active.write().await = vec![make_test_recording_task("active", PathBuf::from("/tmp/active.ts"))];
    queue.queue.lock().await.push_back(make_test_recording_task("next", PathBuf::from("/tmp/next.ts")));

    assert!(!queue.pause_active("next").await.expect("uuid mismatch"));

    assert_eq!(queue.revision.load(Ordering::SeqCst), 0);
    assert_eq!(queue.active.read().await.first().map(|download| download.uuid.as_str()), Some("active"));
    assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("next"));
    assert_eq!(*worker.control_signal.read().await, RecordingControl::None);
}
