use super::*;

#[test]
fn source_input_rejects_empty_virtual_id() {
    let input = source("target", "", "i");
    assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
}

#[test]
fn source_input_rejects_empty_input_name() {
    let input = source("target", "1", "");
    assert!(matches!(input.validate(), Err(ServiceError::InvalidSource)));
}

#[test]
fn source_input_accepts_non_empty_identifiers() {
    let input = source("target", "1", "input-a");
    assert!(input.validate().is_ok());
}

#[test]
fn create_recording_input_rejects_zero_or_negative_interval() {
    let mut input = create_input();
    input.program_end = input.program_start;
    assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
    input.program_end = input.program_start - 1;
    assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
}

#[test]
fn create_recording_input_accepts_valid_interval() {
    let input = create_input();
    assert!(input.validate().is_ok());
}

#[test]
fn create_recording_input_rejects_overflowing_interval() {
    let mut input = create_input();
    input.program_start = i64::MIN;
    input.program_end = i64::MAX;

    assert!(matches!(input.validate(), Err(ServiceError::InvalidInterval)));
}

#[test]
fn effective_window_rejects_exact_or_past_end_boundary() {
    assert!(matches!(effective_recording_window(1_000, 2_000, 100, 200, 2_200), Err(ServiceError::InvalidInterval)));
    assert!(matches!(effective_recording_window(1_000, 2_000, 100, 200, 2_201), Err(ServiceError::InvalidInterval)));
}

#[tokio::test]
async fn edit_recording_rejects_active_state_with_invalid_state_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let mut task =
        RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100)).expect("valid recording task");
    task.state = RecordingTaskState::Running;
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
    let patch =
        EditRecordingPatch { program_title: Some("must not persist".to_string()), ..EditRecordingPatch::default() };

    let result = service.edit_recording(&claims, "recording", patch).await;

    assert!(matches!(result, Err(ServiceError::InvalidState)));
    assert_eq!(committed_records(&downloads).await, persisted_before);
}

#[tokio::test]
async fn removing_a_completed_entry_keeps_its_file() {
    // Remove takes the entry off the list; the recording stays on disk
    // for whoever reads the directory. Deleting the file is its own action.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let (final_path, _) =
        finished_film(&queue, dir.path(), "recording", "web:alice", RecordingTaskState::Completed).await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    assert!(service.remove_recording_task(&deleting_claims(), "recording").await.expect("remove"));
    assert!(final_path.exists(), "the completed file was deleted while only removing the entry");
    assert!(queue.finished.read().await.is_empty());
}

#[tokio::test]
async fn an_edited_window_keeps_its_padding() {
    // An edited window is scheduled with its pre- and post-roll, like a
    // new one.
    let start = chrono::Utc::now().timestamp() + 3_600;
    let queue = scheduled_queue(vec![upcoming_live("recording", "web:alice", start)]).await;
    let service = RecordingService::new(Arc::clone(&queue), test_app_config());

    let patch = EditRecordingPatch {
        program_end: Some(start + 1_800),
        pre_roll_secs: Some(120),
        post_roll_secs: Some(300),
        ..EditRecordingPatch::default()
    };
    service.edit_recording(&editing_claims(), "recording", patch).await.expect("edit");

    let scheduled = queue.scheduled.read().await;
    let meta = &scheduled[0].recording;
    assert_eq!(meta.program_start, Some(start));
    assert_eq!(meta.scheduled_start, Some(start - 120));
    assert_eq!(meta.scheduled_end, Some(start + 1_800 + 300));
}

#[tokio::test]
async fn create_recording_without_download_config_reports_disabled() {
    // A missing `video.recording` block means the server has no
    // download engine at all — the caller's source identifiers are
    // not wrong. Reporting `InvalidSource` here sent clients
    // hunting for a misconfiguration that does not exist.
    let dir = tempfile::tempdir().expect("tempdir");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository"));
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

    let result = service.create_recording(&claims, &create_input()).await;

    assert!(matches!(result, Err(ServiceError::Disabled)));
}

#[tokio::test]
async fn preview_conflict_collects_demand_points_from_queue_state() {
    // The server-side preview must build its own demand points from
    // the committed queue state. A queued recording on the same
    // target/input pair must show up as `others` even when the
    // caller submits no `others` payload.
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let mut existing = persisted_rule_recording("existing", None, 100);
    // Place a padded window that overlaps 100..200.
    existing.recording.scheduled_start = Some(100);
    existing.recording.scheduled_end = Some(200);
    let existing = RecordingQueue::from_persisted(existing).expect("valid recording task");
    downloads.queue.lock().await.push_back(existing);
    let points = collect_demand_points_for_provider(&downloads, "1", "input-a").await;
    assert_eq!(points.len(), 1, "queue entry must surface as a demand point");
    assert_eq!(points[0].padded_start, 100);
    assert_eq!(points[0].padded_end, 200);
}

#[test]
fn is_future_rule_recording_rejects_non_editable_states() {
    // Old implementation passed `cancel_targets_task(false, true)`
    // literally — that always returned `true`, so the rule cancel
    // path would happily tear down a task whose state was already
    // terminal. Both terminal and non-editable-but-active states
    // must be skipped now.
    let mut cancelled_task = persisted_rule_recording("uuid-c", Some("rule-1"), 1_900_000_000);
    cancelled_task.state = RecordingTaskState::Cancelled;
    assert!(!is_future_rule_recording(&cancelled_task, "rule-1", 1_800_000_000));

    let mut paused_task = persisted_rule_recording("uuid-p", Some("rule-1"), 1_900_000_000);
    paused_task.state = RecordingTaskState::Paused;
    assert!(!is_future_rule_recording(&paused_task, "rule-1", 1_800_000_000));

    // Sanity: the happy path still accepts editable future tasks.
    let scheduled_task = persisted_rule_recording("uuid-s", Some("rule-1"), 1_900_000_000);
    assert!(is_future_rule_recording(&scheduled_task, "rule-1", 1_800_000_000));
}

#[tokio::test]
async fn create_recording_rejects_absent_recording_config() {
    let config = tuliprox_core::model::Config::default();
    let app_config = Arc::new(AppConfig {
        config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
        sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
        hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
        api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
        file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
        paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
    });
    let downloads = Arc::new(RecordingQueue::new());
    let service = RecordingService::new(Arc::clone(&downloads), app_config);
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
    let input = CreateRecordingInput {
        source: RecordingSourceInput {
            target_id: "1".to_string(),
            virtual_id: "1".to_string(),
            cluster: XtreamCluster::Live,
            input_name: "input-a".to_string(),
        },
        program_title: "title".to_string(),
        program_start: 0,
        program_end: 60,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        visibility: RecordingVisibility::Private,
        channel_id: None,
        channel_name: None,
        group: None,
        provenance: RecordingProvenance::default(),
        epg: None,
    };

    let result = service.create_recording(&claims, &input).await;

    assert!(
        matches!(result, Err(ServiceError::Disabled)),
        "absent recording config must fail closed with Disabled, got: {result:?}"
    );
}
