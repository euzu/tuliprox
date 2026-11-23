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
    meta.source.input_name = "input-a".to_string();
    meta.source.provider_id = Some("stream-1".to_string());
    meta.source.cluster = XtreamCluster::Live;
    meta.program_start = Some(10);
    meta.program_end = Some(20);
    meta.pre_roll_secs = 2;
    meta.post_roll_secs = 3;
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"LiveChannel":{"input_name":"input-a","provider_stream_id":"stream-1","padded_start":8,"padded_end":23}}"#
    );
    meta.source.is_group = true;
    meta.source.provider_id = None;
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"LiveGroup":{"target_id":"target","group_vid":"channel","padded_start":8,"padded_end":23}}"#
    );
    meta.source.is_group = false;
    meta.source.cluster = XtreamCluster::Video;
    meta.source.provider_id = Some("vod-42".to_string());
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"Media":{"input_name":"input-a","cluster":"Video","provider_stream_id":"vod-42"}}"#
    );
    // Rule recordings follow the medium rules for file identity:
    meta.provenance.rule_id = Some("rule".to_string());
    meta.provenance.occurrence_key = Some("occurrence".to_string());
    assert_eq!(
        recording_identity_key(&meta, url),
        r#"{"Media":{"input_name":"input-a","cluster":"Video","provider_stream_id":"vod-42"}}"#
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

#[tokio::test]
async fn two_users_same_programme_same_padding_share_one_materialization() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));
    let start = chrono::Utc::now().timestamp() + 3_600;

    let mut task_a = persisted_rule_recording("alice-task", None, start);
    task_a.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
    task_a.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("provider-stream-1".to_string()));
    task_a.recording.pre_roll_secs = 60;
    task_a.recording.post_roll_secs = 120;
    task_a.file_path = std::path::PathBuf::from("/tmp/shared.ts");
    task_a.filename = "shared.ts".to_string();
    task_a.recording.relative_path = Some("shared.ts".to_string());

    let mut task_b = persisted_rule_recording("bob-task", None, start);
    task_b.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    task_b.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("provider-stream-1".to_string()));
    task_b.recording.pre_roll_secs = 60;
    task_b.recording.post_roll_secs = 120;
    task_b.file_path = std::path::PathBuf::from("/tmp/shared.ts");
    task_b.filename = "shared.ts".to_string();
    task_b.recording.relative_path = Some("shared.ts".to_string());

    let task_a = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_a).expect("task a"));
    let task_b = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_b).expect("task b"));
    assert_eq!(task_a.media_identity, task_b.media_identity);

    mutate(&queue, |candidate| {
        candidate.scheduled.push(task_a);
        candidate.scheduled.push(task_b);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 1);
    assert_eq!(mats[0].reference_count, 2);
}

#[tokio::test]
async fn different_padding_produces_two_materializations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));
    let start = chrono::Utc::now().timestamp() + 3_600;

    let mut task_a = persisted_rule_recording("alice-task", None, start);
    task_a.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
    task_a.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("provider-stream-1".to_string()));
    task_a.recording.pre_roll_secs = 60;
    task_a.recording.post_roll_secs = 120;

    let mut task_b = persisted_rule_recording("bob-task", None, start);
    task_b.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    task_b.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("provider-stream-1".to_string()));
    task_b.recording.pre_roll_secs = 120;
    task_b.recording.post_roll_secs = 120;

    let task_a = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_a).expect("task a"));
    let task_b = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_b).expect("task b"));
    assert_ne!(task_a.media_identity, task_b.media_identity);

    mutate(&queue, |candidate| {
        candidate.scheduled.push(task_a);
        candidate.scheduled.push(task_b);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 2);
    assert_eq!(mats[0].reference_count, 1);
    assert_eq!(mats[1].reference_count, 1);
}

#[tokio::test]
async fn same_vod_episode_from_two_targets_and_different_times_shares_materialization() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));

    let mut task_target1 = persisted_media("t1", "web:alice", RecordingVisibility::Private, "http://provider/ep1.mkv");
    task_target1.recording.source = RecordingSource::new("target-alpha", "42", "input-vod")
        .with_cluster(XtreamCluster::Series)
        .with_provider_id(Some("vod-stream-99".to_string()));
    task_target1.recording.scheduled_start = Some(1_700_000_000);

    let mut task_target2 = persisted_media("t2", "web:bob", RecordingVisibility::Private, "http://provider/ep1.mkv");
    task_target2.recording.source = RecordingSource::new("target-beta", "108", "input-vod")
        .with_cluster(XtreamCluster::Series)
        .with_provider_id(Some("vod-stream-99".to_string()));
    task_target2.recording.scheduled_start = Some(1_700_050_000);

    let t1 = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_target1).expect("task 1"));
    let t2 = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_target2).expect("task 2"));
    assert_eq!(t1.media_identity, t2.media_identity);

    mutate(&queue, |candidate| {
        candidate.queue.push(t1);
        candidate.queue.push(t2);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 1);
    assert_eq!(mats[0].reference_count, 2);
}

#[tokio::test]
async fn group_channel_identity_is_target_bound() {
    // Group channel recordings: identity is target_id + group_vid + padded window.
    // Within the same target, two users requesting the group channel share one materialization.
    // Across different targets, the group identity is target-bound (different target_id),
    // producing distinct materializations even with identical group vid and window.
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));
    let start = chrono::Utc::now().timestamp() + 3_600;

    let mut task_t1_a = persisted_rule_recording("t1-a", None, start);
    task_t1_a.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
    task_t1_a.recording.source = RecordingSource::new("target-1", "grp-100", "input-a").with_is_group(true);
    task_t1_a.file_path = std::path::PathBuf::from("/tmp/t1-shared.ts");
    task_t1_a.filename = "t1-shared.ts".to_string();
    task_t1_a.recording.relative_path = Some("t1-shared.ts".to_string());

    let mut task_t1_b = persisted_rule_recording("t1-b", None, start);
    task_t1_b.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    task_t1_b.recording.source = RecordingSource::new("target-1", "grp-100", "input-a").with_is_group(true);
    task_t1_b.file_path = std::path::PathBuf::from("/tmp/t1-shared.ts");
    task_t1_b.filename = "t1-shared.ts".to_string();
    task_t1_b.recording.relative_path = Some("t1-shared.ts".to_string());

    let mut task_t2_c = persisted_rule_recording("t2-c", None, start);
    task_t2_c.recording.owner = RecordingOwner::User(UserId::from("web:charlie"));
    task_t2_c.recording.source = RecordingSource::new("target-2", "grp-100", "input-a").with_is_group(true);
    task_t2_c.file_path = std::path::PathBuf::from("/tmp/t2.ts");
    task_t2_c.filename = "t2.ts".to_string();
    task_t2_c.recording.relative_path = Some("t2.ts".to_string());

    let t1_a = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_t1_a).expect("t1 a"));
    let t1_b = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_t1_b).expect("t1 b"));
    let t2_c = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_t2_c).expect("t2 c"));

    assert_eq!(t1_a.media_identity, t1_b.media_identity, "same target + group vid share identity");
    assert_ne!(t1_a.media_identity, t2_c.media_identity, "group identity is bound to target");

    mutate(&queue, |candidate| {
        candidate.scheduled.push(t1_a);
        candidate.scheduled.push(t1_b);
        candidate.scheduled.push(t2_c);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 2);
    let target1_mat = mats.iter().find(|m| m.media.source.target_id == "target-1").expect("target 1");
    let target2_mat = mats.iter().find(|m| m.media.source.target_id == "target-2").expect("target 2");
    assert_eq!(target1_mat.reference_count, 2);
    assert_eq!(target2_mat.reference_count, 1);
}

#[tokio::test]
async fn rule_occurrence_and_manual_request_share_file_and_duplicate_occurrence_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));
    let start = chrono::Utc::now().timestamp() + 3_600;

    let mut manual = persisted_rule_recording("manual-task", None, start);
    manual.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
    manual.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("stream-300".to_string()));
    manual.recording.pre_roll_secs = 60;
    manual.recording.post_roll_secs = 60;
    manual.file_path = std::path::PathBuf::from("/tmp/manual.ts");
    manual.filename = "manual.ts".to_string();
    manual.recording.relative_path = Some("manual.ts".to_string());

    let mut rule_occ = persisted_rule_recording("rule-task-1", Some("rule-1"), start);
    rule_occ.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    rule_occ.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("stream-300".to_string()));
    rule_occ.recording.pre_roll_secs = 60;
    rule_occ.recording.post_roll_secs = 60;
    rule_occ.recording.provenance.occurrence_key = Some("occurrence-2026-11-20".to_string());
    rule_occ.file_path = std::path::PathBuf::from("/tmp/manual.ts");
    rule_occ.filename = "manual.ts".to_string();
    rule_occ.recording.relative_path = Some("manual.ts".to_string());

    let manual_task = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(manual).expect("manual"));
    let rule_task = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(rule_occ).expect("rule"));

    assert_eq!(manual_task.media_identity, rule_task.media_identity);

    mutate(&queue, |candidate| {
        candidate.scheduled.push(manual_task);
        candidate.scheduled.push(rule_task);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 1);
    assert_eq!(mats[0].reference_count, 2);

    let repeat_rule = persisted_rule_recording("rule-task-2", Some("rule-1"), start);
    let mut repeat_rule = RecordingQueue::from_persisted(repeat_rule).expect("valid");
    repeat_rule.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    repeat_rule.recording.provenance.occurrence_key = Some("occurrence-2026-11-20".to_string());

    let candidate_tasks = committed_records(&queue).await;
    let candidate = PersistedRecordingQueue { scheduled: candidate_tasks, ..Default::default() };
    assert!(candidate_has_duplicate_recording(&candidate, &repeat_rule));
}

#[tokio::test]
async fn edit_split_on_shared_file_and_sole_entry_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let queue = Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open repo"));
    let start = chrono::Utc::now().timestamp() + 3_600;

    let mut task_alice = persisted_rule_recording("alice-entry", None, start);
    task_alice.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
    task_alice.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("stream-1".to_string()));
    task_alice.file_path = std::path::PathBuf::from("/tmp/shared-show.ts");
    task_alice.filename = "shared-show.ts".to_string();
    task_alice.recording.relative_path = Some("shared-show.ts".to_string());

    let mut task_bob = persisted_rule_recording("bob-entry", None, start);
    task_bob.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    task_bob.recording.source =
        RecordingSource::new("target-1", "1", "input-a").with_provider_id(Some("stream-1".to_string()));
    task_bob.file_path = std::path::PathBuf::from("/tmp/shared-show.ts");
    task_bob.filename = "shared-show.ts".to_string();
    task_bob.recording.relative_path = Some("shared-show.ts".to_string());

    let mut task_dave = persisted_rule_recording("dave-entry", None, start);
    task_dave.recording.owner = RecordingOwner::User(UserId::from("web:dave"));
    task_dave.recording.source =
        RecordingSource::new("target-1", "2", "input-a").with_provider_id(Some("stream-2".to_string()));
    task_dave.file_path = std::path::PathBuf::from("/tmp/dave-show.ts");
    task_dave.filename = "dave-show.ts".to_string();
    task_dave.recording.relative_path = Some("dave-show.ts".to_string());

    let t_alice = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_alice).expect("alice"));
    let t_bob = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_bob).expect("bob"));
    let t_dave = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task_dave).expect("dave"));

    mutate(&queue, |candidate| {
        candidate.scheduled.push(t_alice);
        candidate.scheduled.push(t_bob);
        candidate.scheduled.push(t_dave);
        Ok(())
    })
    .await
    .expect("mutate");

    let mats = committed_materializations(&queue).await;
    assert_eq!(mats.len(), 2);
    let shared_mat = mats.iter().find(|m| m.reference_count == 2).expect("shared");
    assert_eq!(shared_mat.filename, "shared-show.ts");

    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    let patch_alice = EditRecordingPatch { program_end: Some(start + 7_200), ..EditRecordingPatch::default() };
    service.edit_recording(&editing_claims(), "alice-entry", patch_alice).await.expect("edit alice");

    let mats_after = committed_materializations(&queue).await;
    assert_eq!(mats_after.len(), 3);
    let bob_mat = mats_after.iter().find(|m| m.filename == "shared-show.ts").expect("bob mat");
    assert_eq!(bob_mat.reference_count, 1, "Bob is now sole holder of original materialization");

    let (_, tasks) = queue.committed_snapshot().await;
    let get_task = |uuid: &str| tasks.iter().find(|t| t.uuid == uuid).cloned().expect("task");
    let (alice_now, bob_now) = (get_task("alice-entry"), get_task("bob-entry"));
    assert_eq!(bob_now.file_path, std::path::PathBuf::from("/tmp/shared-show.ts"), "Bob's file is untouched");
    assert_ne!(alice_now.file_path, bob_now.file_path, "Alice moved to her own file path");

    let patch_dave = EditRecordingPatch { pre_roll_secs: Some(30), ..EditRecordingPatch::default() };
    service.edit_recording(&dave_claims(), "dave-entry", patch_dave).await.expect("edit dave");

    let (_, tasks_after_dave) = queue.committed_snapshot().await;
    let dave_now = tasks_after_dave.iter().find(|t| t.uuid == "dave-entry").expect("dave task");
    assert_eq!(dave_now.file_path, std::path::PathBuf::from("/tmp/dave-show.ts"), "Sole entry is edited in place");
}

#[tokio::test]
async fn edit_of_clip_detaches_cleanly_and_clears_clip_metadata() {
    let start = chrono::Utc::now().timestamp() + 3_600;
    let bob = upcoming_live("bob-entry", "web:bob", start);
    let mut alice = upcoming_live("alice-entry", "web:alice", start);
    // Alice is a clip on Bob's recording
    alice.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: start + 60,
        end_ts: start + 300,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    let queue = scheduled_queue(vec![bob, alice]).await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    // Alice edits her window so she leaves the shared media
    let patch_alice = EditRecordingPatch { program_end: Some(start + 7_200), ..EditRecordingPatch::default() };
    service.edit_recording(&editing_claims(), "alice-entry", patch_alice).await.expect("edit alice");

    let (_, tasks) = queue.committed_snapshot().await;
    let alice_now = tasks.iter().find(|t| t.uuid == "alice-entry").expect("alice entry");
    assert!(alice_now.clip.is_none(), "detached clip must have its clip metadata cleared");
    let bob_now = tasks.iter().find(|t| t.uuid == "bob-entry").expect("bob entry");
    assert_ne!(alice_now.file_path, bob_now.file_path, "Alice now has her own file path");
}
