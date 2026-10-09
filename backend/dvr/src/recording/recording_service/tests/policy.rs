use super::*;

#[test]
fn a_live_window_must_have_time_left_in_it() {
    // A window whose padded end is not after its padded start cannot
    // produce a recording, and admitting one means a scheduled capture that
    // can only ever fail.
    let now = 1_700_000_000;
    // Zero-length and inverted programmes.
    assert!(matches!(effective_recording_window(now + 100, now + 100, 0, 0, now), Err(ServiceError::InvalidInterval)));
    assert!(matches!(effective_recording_window(now + 200, now + 100, 0, 0, now), Err(ServiceError::InvalidInterval)));
    // A programme that finished before it was asked for.
    assert!(matches!(
        effective_recording_window(now - 7_200, now - 3_600, 0, 0, now),
        Err(ServiceError::InvalidInterval)
    ));
}

#[test]
fn a_live_window_already_underway_records_only_what_is_left() {
    // Joining late is legal; it just cannot rewind. The padded bounds stay
    // as planned so the stop time is still the programme's.
    let now = 1_700_000_000;
    let window = effective_recording_window(now - 600, now + 600, 60, 120, now).expect("still running");

    assert_eq!(window.scheduled_start, now - 660, "the padded start is history, not moved to now");
    assert_eq!(window.scheduled_end, now + 720);
    assert_eq!(window.execution_start, now, "but recording starts now");
    assert_eq!(window.remaining_duration_secs, 720, "and runs to the padded end");
}

#[test]
fn another_user_asking_for_the_same_film_is_not_refused() {
    // Until one physical file can carry several library entries, treating
    // this as a duplicate would tell the second user "already recording"
    // and leave them with nothing.
    let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
    let other_user = RecordingQueue::from_persisted(persisted_media(
        "b",
        "web:bob",
        RecordingVisibility::Private,
        "http://p/film.mp4",
    ))
    .expect("valid task");
    assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &other_user));
}

#[test]
fn a_shared_copy_does_not_collide_with_a_private_one() {
    // The two charge different quota pools, so they are two recordings.
    let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
    let shared = RecordingQueue::from_persisted(persisted_media(
        "b",
        "web:alice",
        RecordingVisibility::Shared,
        "http://p/film.mp4",
    ))
    .expect("valid task");
    assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &shared));
}

#[test]
fn a_finished_transfer_does_not_block_a_fresh_request() {
    // After a completed or failed attempt the user may legitimately want
    // another copy.
    let mut finished = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
    finished.finished = true;
    finished.state = RecordingTaskState::Completed;
    let candidate = PersistedRecordingQueue { finished: vec![finished], ..PersistedRecordingQueue::default() };
    let again = RecordingQueue::from_persisted(persisted_media(
        "b",
        "web:alice",
        RecordingVisibility::Private,
        "http://p/film.mp4",
    ))
    .expect("valid task");
    assert!(!candidate_has_duplicate_recording(&candidate, &again));
}

#[test]
fn a_completed_rule_occurrence_still_blocks_a_repeat() {
    // The scheduler re-evaluates rules on every tick; without this an
    // occurrence would be re-materialized forever.
    let mut finished = persisted_rule_recording("a", Some("rule-1"), 100);
    finished.recording.provenance.occurrence_key = Some("occ-1".to_string());
    finished.finished = true;
    finished.state = RecordingTaskState::Completed;
    let candidate = PersistedRecordingQueue { finished: vec![finished], ..PersistedRecordingQueue::default() };

    let mut repeat_persisted = persisted_rule_recording("b", Some("rule-1"), 100);
    repeat_persisted.recording.provenance.occurrence_key = Some("occ-1".to_string());
    let repeat = RecordingQueue::from_persisted(repeat_persisted).expect("valid task");
    assert!(candidate_has_duplicate_recording(&candidate, &repeat));
}

#[test]
fn effective_window_applies_padding_and_remaining_duration() {
    let window = effective_recording_window(1_000, 2_000, 100, 200, 1_500).expect("valid effective window");

    assert_eq!(window.scheduled_start, 900);
    assert_eq!(window.scheduled_end, 2_200);
    assert_eq!(window.execution_start, 1_500);
    assert_eq!(window.remaining_duration_secs, 700);
}

#[test]
fn effective_window_is_panic_free_at_integer_boundaries() {
    let window = effective_recording_window(i64::MIN + 1, i64::MAX - 1, 10, 10, 0).expect("saturated effective window");

    assert_eq!(window.scheduled_start, i64::MIN);
    assert_eq!(window.scheduled_end, i64::MAX);
    assert_eq!(window.execution_start, 0);
    assert_eq!(window.remaining_duration_secs, i64::MAX as u64);
}

#[tokio::test]
async fn removing_an_entry_keeps_a_partial_another_entry_still_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let (_, partial) = finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Cancelled).await;
    let _ = finished_film(&queue, dir.path(), "bob-recording", "web:bob", RecordingTaskState::Failed).await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
    assert!(partial.exists(), "Bob's entry still points at this media");
    assert_eq!(queue.finished.read().await.len(), 1);
}

#[tokio::test]
async fn a_vod_takes_no_window_but_can_still_be_retitled() {
    let queue = scheduled_queue(Vec::new()).await;
    let mut vod = persisted_media("film", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
    vod.state = RecordingTaskState::Queued;
    mutate(&queue, move |candidate| {
        candidate.queue.push(vod.clone());
        Ok(())
    })
    .await
    .expect("seed");
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    for patch in [
        EditRecordingPatch { program_start: Some(1), program_end: Some(2), ..EditRecordingPatch::default() },
        EditRecordingPatch { pre_roll_secs: Some(60), ..EditRecordingPatch::default() },
    ] {
        let refused = service.edit_recording(&editing_claims(), "film", patch).await;
        assert!(matches!(refused, Err(ServiceError::InvalidInterval)), "got {refused:?}");
    }
    let retitled = EditRecordingPatch { program_title: Some("Better title".into()), ..EditRecordingPatch::default() };
    service.edit_recording(&editing_claims(), "film", retitled).await.expect("a title edit is fine");
    let queued = queue.queue.lock().await;
    assert_eq!(queued[0].recording.program_title.as_deref(), Some("Better title"));
    assert_eq!(queued[0].recording.program_start, None, "and it stays a transfer without a window");
}

#[tokio::test]
async fn padding_alone_does_not_split_shared_media() {
    // The programme window decides the media, its padding does not.
    let start = chrono::Utc::now().timestamp() + 3_600;
    let queue = scheduled_queue(vec![
        upcoming_live("alice-entry", "web:alice", start),
        upcoming_live("bob-entry", "web:bob", start),
    ])
    .await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    let patch = EditRecordingPatch { post_roll_secs: Some(600), ..EditRecordingPatch::default() };
    service.edit_recording(&editing_claims(), "alice-entry", patch).await.expect("edit");

    let (_, tasks) = queue.committed_snapshot().await;
    let paths: Vec<_> = tasks.iter().map(|task| task.file_path.clone()).collect();
    assert_eq!(paths[0], paths[1], "still one file for one programme");
}

#[test]
fn service_error_code_is_stable_string() {
    assert_eq!(ServiceError::UnknownOwner.code(), "recording_unknown_owner");
    assert_eq!(ServiceError::InvalidSource.code(), "recording_invalid_source");
    assert_eq!(ServiceError::Forbidden.code(), "recording_forbidden");
    assert_eq!(ServiceError::SharedCreationNotAdministrator.code(), "recording_shared_not_administrator");
    assert_eq!(ServiceError::InvalidState.code(), "recording_invalid_state");
    assert_eq!(ServiceError::InvalidInterval.code(), "recording_invalid_interval");
    assert_eq!(ServiceError::UnknownRecording.code(), "recording_unknown");
    assert_eq!(ServiceError::PersistenceFailed.code(), "recording_persistence_failed");
    assert_eq!(ServiceError::ProvenanceImmutable.code(), "recording_provenance_immutable");
    assert_eq!(ServiceError::Disabled.code(), "recording_disabled");
}

#[test]
fn provenance_cleared_does_not_masquerade_as_invalid_state() {
    assert_eq!(map_edit_validation_error(&EditError::ProvenanceCleared), ServiceError::ProvenanceImmutable);
}

#[tokio::test]
async fn a_media_request_needs_the_create_permission_not_manage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(dir.path(), &queue, None);

    let manage_only = claims_for("alice", Permission::RecordingManage.into());
    let refused = service.create_media_recording_idempotent(&manage_only, &media_input(), None).await;
    assert!(matches!(refused, Err(ServiceError::Forbidden)), "got {refused:?}");
    assert!(queue.queue.lock().await.is_empty());

    let create_only = claims_for("alice", Permission::RecordingCreate.into());
    let admitted = service.create_media_recording_idempotent(&create_only, &media_input(), None).await;
    assert!(admitted.is_ok(), "got {admitted:?}");
    let queued = queue.queue.lock().await;
    assert_eq!(queued.len(), 1, "a transfer is queued, not scheduled");
    assert_eq!(queued[0].kind, RecordingKind::Vod);
}

#[tokio::test]
async fn a_replayed_media_request_is_answered_without_queueing_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(dir.path(), &queue, None);
    let alice = claims_for("alice", Permission::RecordingCreate.into());
    let request = IdempotencyRequest { key: "k1".to_string(), fingerprint: "f1".to_string() };

    let first = service
        .create_media_recording_idempotent(&alice, &media_input(), Some(request.clone()))
        .await
        .expect("first request");
    let replay = service.create_media_recording_idempotent(&alice, &media_input(), Some(request)).await;
    assert_eq!(replay.err(), Some(ServiceError::IdempotentReplay { recording_id: first.uuid }));
    assert_eq!(queue.queue.lock().await.len(), 1);
}
