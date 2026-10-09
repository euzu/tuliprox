use super::*;

/// A VOD transfer for `owner`, resolved to `url`.
#[test]
fn borrowed_media_identity_preserves_persisted_keys() {
    let url = "http://provider/film.mp4";
    let mut meta = persisted_media("key", "web:alice", RecordingVisibility::Private, url).recording;
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"Url":{"url":"http://provider/film.mp4","scheduled_start":null,"scheduled_end":null}}"#
    );
    meta.source.target_id = "target".to_string();
    meta.source.virtual_id = "channel".to_string();
    meta.program_start = Some(10);
    meta.program_end = Some(20);
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"Programme":{"target_id":"target","virtual_id":"channel","program_start":10,"program_end":20}}"#
    );
    meta.provenance.rule_id = Some("rule".to_string());
    meta.provenance.occurrence_key = Some("occurrence".to_string());
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"Occurrence":{"rule_id":"rule","occurrence_key":"occurrence"}}"#
    );
}

#[test]
fn a_repeated_vod_request_is_a_duplicate() {
    // Regression: duplicate detection only produced an identity for Live,
    // so a user who asked for the same film twice got two downloads of it
    // written side by side as `film.mp4` and `film_1.mp4`.
    let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
    let repeat = RecordingQueue::from_persisted(persisted_media(
        "b",
        "web:alice",
        RecordingVisibility::Private,
        "http://p/film.mp4",
    ))
    .expect("valid task");
    assert!(candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &repeat));
}

#[test]
fn a_different_film_is_not_a_duplicate() {
    let existing = persisted_media("a", "web:alice", RecordingVisibility::Private, "http://p/film.mp4");
    let other = RecordingQueue::from_persisted(persisted_media(
        "b",
        "web:alice",
        RecordingVisibility::Private,
        "http://p/other.mp4",
    ))
    .expect("valid task");
    assert!(!candidate_has_duplicate_recording(&queued_candidate(vec![existing]), &other));
}

#[tokio::test]
async fn edit_recording_rejects_padding_above_max_without_persisting_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let task =
        RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100)).expect("valid recording task");
    downloads.scheduled.write().await.push(task);
    downloads.persist_to_disk().await.expect("persist initial queue");
    let persisted_before = committed_records(&downloads).await;
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
    let patch = EditRecordingPatch { pre_roll_secs: Some(901), ..EditRecordingPatch::default() };

    let result = service.edit_recording(&claims, "recording", patch).await;

    assert!(matches!(result, Err(ServiceError::PaddingLimitExceeded)));
    assert_eq!(committed_records(&downloads).await, persisted_before);
    let scheduled = downloads.scheduled.read().await;
    assert_eq!(scheduled[0].recording.pre_roll_secs, 0);
}

#[tokio::test]
async fn an_edit_that_leaves_shared_media_moves_to_a_path_of_its_own() {
    // Alice and Bob scheduled the same programme and hold one file path.
    // Alice moving her window makes it different media; keeping the path
    // would have two captures write one file and let deleting hers remove
    // Bob's.
    let start = chrono::Utc::now().timestamp() + 3_600;
    let queue = scheduled_queue(vec![
        upcoming_live("alice-entry", "web:alice", start),
        upcoming_live("bob-entry", "web:bob", start),
    ])
    .await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    let patch = EditRecordingPatch { program_end: Some(start + 7_200), ..EditRecordingPatch::default() };
    service.edit_recording(&editing_claims(), "alice-entry", patch).await.expect("edit");

    let (_, tasks) = queue.committed_snapshot().await;
    let task = |uuid: &str| tasks.iter().find(|task| task.uuid == uuid).cloned().expect("entry");
    let (alice, bob) = (task("alice-entry"), task("bob-entry"));
    assert_ne!(alice.file_path, bob.file_path, "the edited entry no longer writes Bob's file");
    assert_eq!(bob.file_path, std::path::PathBuf::from("/tmp/shared-programme.ts"), "Bob's entry is untouched");
    let identity = |task: &RecordingTask| recording_identity_key(&task.recording, task.url.as_str());
    assert_ne!(identity(&alice), identity(&bob));
}

#[tokio::test]
async fn edit_recording_rejects_overflowing_interval_without_persisting_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let task =
        RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100)).expect("valid recording task");
    downloads.scheduled.write().await.push(task);
    downloads.persist_to_disk().await.expect("persist initial queue");
    let persisted_before = committed_records(&downloads).await;
    let revision_before = downloads.revision.load(std::sync::atomic::Ordering::SeqCst);
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
    let patch = EditRecordingPatch {
        program_start: Some(i64::MIN),
        program_end: Some(i64::MAX),
        program_title: Some("must not persist".to_string()),
        ..EditRecordingPatch::default()
    };

    let result = service.edit_recording(&claims, "recording", patch).await;

    assert!(matches!(result, Err(ServiceError::InvalidInterval)));
    assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), revision_before);
    assert_eq!(committed_records(&downloads).await, persisted_before);
    let scheduled = downloads.scheduled.read().await;
    assert_eq!(scheduled[0].scheduled_start(), Some(100));
    assert_ne!(scheduled[0].recording.program_title.as_deref(), Some("must not persist"));
}

#[test]
fn live_filename_renders_the_configured_template() {
    let input = filename_input();
    let cfg = filename_config("{channel}_{program_title}_{start_time}", RecordingContainerFormat::Mpegts);
    assert_eq!(
        render_live_filename(&input, &cfg, &filename_window(&input), "alice"),
        "NLZIET_SBS_6_HD_Hart_van_Nederland_2023-11-14_23-13.ts"
    );
}

#[test]
fn live_filename_collapses_empty_placeholders() {
    let mut input = filename_input();
    input.channel_name = None;
    let cfg = filename_config("{channel}_{program_title}_{episode}", RecordingContainerFormat::Matroska);
    assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Hart_van_Nederland.mkv");
}

#[test]
fn live_filename_renders_episode_and_owner() {
    let mut input = filename_input();
    input.epg = Some(shared::model::EpgEpisodeMetadata {
        programme_id: None,
        series_id: None,
        episode_id: None,
        season: Some(3),
        episode: Some(9),
        airing: shared::model::AiringStatus::default(),
    });
    let cfg = filename_config("{owner}-{program_title}-{episode}", RecordingContainerFormat::Mp4);
    assert_eq!(
        render_live_filename(&input, &cfg, &filename_window(&input), "alice"),
        "alice-Hart_van_Nederland-S03E09.mp4"
    );
}

#[test]
fn live_filename_keeps_dots_and_does_not_double_the_extension() {
    let mut input = filename_input();
    input.program_title = "Mr. Robot".to_string();
    let cfg = filename_config("{program_title}.ts", RecordingContainerFormat::Mpegts);
    assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Mr.Robot.ts");
    let cfg = filename_config("{program_title}", RecordingContainerFormat::Matroska);
    assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Mr.Robot.mkv");
}

#[test]
fn live_filename_without_template_uses_the_title() {
    let input = filename_input();
    let cfg = filename_config("", RecordingContainerFormat::Mpegts);
    assert_eq!(render_live_filename(&input, &cfg, &filename_window(&input), "alice"), "Hart van Nederland.ts");
}

#[test]
fn sanitize_filename_strips_separators_and_reserved_characters() {
    assert_eq!(sanitize_filename_component("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
}

#[test]
fn sanitize_filename_rejects_traversal_and_empty_results() {
    assert_eq!(sanitize_filename_component(""), "recording");
    assert_eq!(sanitize_filename_component("."), "recording");
    assert_eq!(sanitize_filename_component(".."), "recording");
    assert_eq!(sanitize_filename_component("   "), "recording");
    // A lone separator becomes the substitute character, which is
    // itself a perfectly valid component.
    assert_eq!(sanitize_filename_component("/"), "_");
}

#[test]
fn sanitize_filename_rejects_windows_device_names() {
    assert_eq!(sanitize_filename_component("CON"), "recording");
    assert_eq!(sanitize_filename_component("nul.ts"), "recording");
    assert_eq!(sanitize_filename_component("lpt9"), "recording");
    // Not reserved: only an exact stem match counts.
    assert_eq!(sanitize_filename_component("console"), "console");
}

#[test]
fn sanitize_filename_trims_trailing_dots_and_spaces() {
    assert_eq!(sanitize_filename_component("Show. "), "Show");
    assert_eq!(sanitize_filename_component(" .Show"), "Show");
}

#[test]
fn sanitize_filename_is_idempotent_and_bounded() {
    let long = "\u{e9}".repeat(400);
    let once = sanitize_filename_component(&long);
    assert!(once.len() <= MAX_FILENAME_COMPONENT_BYTES);
    // Truncation never splits a character.
    assert!(once.chars().all(|ch| ch == '\u{e9}'));
    assert_eq!(sanitize_filename_component(&once), once);
    for raw in ["a/b", "CON", "", "Show. ", "news\u{202e}sj.ts"] {
        let first = sanitize_filename_component(raw);
        assert_eq!(sanitize_filename_component(&first), first, "not idempotent: {raw}");
    }
}

#[test]
fn sanitized_filename_is_always_a_single_valid_component() {
    let very_long = "x".repeat(500);
    let cases = ["a/b/c", "..", "\u{0}x", "CON", "  ", "../../etc/passwd", &very_long];
    for raw in cases {
        let sanitized = sanitize_filename_component(raw);
        validate_reserved_filename(&sanitized)
            .unwrap_or_else(|err| panic!("{raw:?} sanitized to invalid component: {err}"));
    }
}

#[test]
fn validate_reserved_filename_rejects_parent_and_curdir_components() {
    assert!(validate_reserved_filename("..").is_err());
    assert!(validate_reserved_filename(".").is_err());
    assert!(validate_reserved_filename("a/..").is_err());
    assert!(validate_reserved_filename("normal.ts").is_ok());
}

#[tokio::test]
async fn a_second_user_gets_an_own_entry_and_a_repeat_is_a_duplicate() {
    // Another user asking for the same film must neither be handed the
    // first user's entry nor be refused; the same user asking twice is
    // a duplicate.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(dir.path(), &queue, None);
    let alice = claims_for("alice", Permission::RecordingCreate.into());
    let bob = claims_for("bob", Permission::RecordingCreate.into());

    let first = service.create_media_recording_idempotent(&alice, &media_input(), None).await.expect("alice");
    let second = service.create_media_recording_idempotent(&bob, &media_input(), None).await.expect("bob");
    assert_ne!(first.uuid, second.uuid);
    assert_eq!(second.owner_id, UserId::from("web:bob"));

    let repeat = service.create_media_recording_idempotent(&alice, &media_input(), None).await;
    assert!(matches!(repeat, Err(ServiceError::Duplicate)), "got {repeat:?}");
    assert_eq!(queue.queue.lock().await.len(), 2);
}

#[tokio::test]
async fn a_recording_with_no_room_on_disk_is_refused() {
    // Logical quota and physical space are different questions, and
    // admission asks both: a full disk refuses the recording up front
    // instead of letting ffmpeg fail on ENOSPC. The safety margin drives
    // headroom to zero here rather than actually filling a filesystem.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(
        dir.path(),
        &queue,
        Some(tuliprox_core::model::RecordingDiskConfig {
            high_water_percent: None,
            low_water_percent: None,
            cleanup_interval_secs: None,
            safety_bytes: Some(u64::MAX),
        }),
    );

    let result = service.create_recording(&creating_claims(), &disk_test_input()).await;

    assert!(matches!(result, Err(ServiceError::DiskFull)), "got {result:?}");
    assert!(queue.scheduled.read().await.is_empty(), "a refused admission must not leave a recording behind");
}
