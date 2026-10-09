use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn edit_recording_re_validates_quota_against_new_duration_atomically() {
    // Owner already has 800 reserved. New duration would push the
    // reservation to 1100 against a 1000-byte quota. Edit must
    // fail with QuotaExceeded and persist nothing.
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let start = chrono::Utc::now().timestamp() + 3_600;
    let mut task = RecordingQueue::from_persisted(persisted_rule_recording("recording", None, start))
        .expect("valid recording task");
    {
        let meta = &mut task.recording;
        meta.reserved_bytes = 800;
    }
    downloads.scheduled.write().await.push(task);
    downloads.persist_to_disk().await.expect("persist initial queue");
    let persisted_before = committed_records(&downloads).await;
    let quota = tuliprox_core::model::RecordingQuotaConfig {
        default_private_bytes: Some(1_000),
        per_user_bytes: HashMap::new(),
        shared_bytes: None,
    };
    let rec_cfg = RecordingConfig {
        headers: HashMap::new(),
        t_origin_headers: tuliprox_core::model::RecordingOriginHeaders::default(),
        organize_into_directories: false,
        episode_pattern: None,
        priority: 0,
        reserve_slots_for_users: 0,
        max_background_per_provider: 0,
        retry_backoff_initial_secs: 1,
        retry_backoff_multiplier: 1.0,
        retry_backoff_max_secs: 1,
        retry_backoff_jitter_percent: 0,
        retry_max_attempts: 1,
        enabled: true,
        container_format: RecordingContainerFormat::default(),
        directory: String::new(),
        timezone: "UTC".parse().expect("UTC must parse"),
        filename_template: String::new(),
        default_pre_roll_secs: 0,
        max_pre_roll_secs: 900,
        default_post_roll_secs: 0,
        max_post_roll_secs: 1800,
        retention: None,
        disk: None,
        quota: Some(quota),
        notifications: RecordingNotificationConfig::default(),
        fallback_bytes_per_minute: 60,
    };
    let config = tuliprox_core::model::Config {
        video: Some(tuliprox_core::model::VideoConfig {
            extensions: Vec::new(),
            web_search: None,
            recording: Some(rec_cfg.clone()),
        }),
        ..tuliprox_core::model::Config::default()
    };
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
    let patch = EditRecordingPatch { program_end: Some(start + 1_200), ..EditRecordingPatch::default() };

    let result = service.edit_recording(&claims, "recording", patch).await;

    assert!(matches!(result, Err(ServiceError::QuotaExceeded)), "got {result:?}");
    assert_eq!(committed_records(&downloads).await, persisted_before);
    let scheduled = downloads.scheduled.read().await;
    assert_eq!(scheduled[0].scheduled_start(), Some(start));
}

#[tokio::test]
async fn asking_for_a_finished_file_is_charged_its_size_at_admission() {
    // Attaching copies the whole file into the new entry's charge, so a
    // 16 byte quota must not let a 4096 byte file in.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(dir.path(), &queue, None);
    completed_film(&service, &queue, 4096).await;
    with_private_quota(&service, 16);

    let bob = claims_for("bob", Permission::RecordingCreate.into());
    let refused = service.create_media_recording_idempotent(&bob, &media_input(), None).await;
    assert!(matches!(refused, Err(ServiceError::QuotaExceeded)), "got {refused:?}");

    with_private_quota(&service, 8192);
    service.create_media_recording_idempotent(&bob, &media_input(), None).await.expect("fits now");
    let queued = queue.queue.lock().await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].recording.reserved_bytes, 4096, "reserved at the known size until it attaches");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_requests_with_one_key_admit_only_one() {
    // Both requests pass the first key lookup before either commits; the
    // lookup under the mutation guard admits only one of them.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = Arc::new(service_with_disk(dir.path(), &queue, None));
    let held = queue.queue.lock().await;
    let handles: Vec<_> = [("77", "body-a"), ("78", "body-b")]
        .into_iter()
        .map(|(virtual_id, fingerprint)| {
            let service = Arc::clone(&service);
            let mut input = media_input();
            input.source.virtual_id = virtual_id.to_string();
            let request = IdempotencyRequest { key: "same-key".to_string(), fingerprint: fingerprint.to_string() };
            tokio::spawn(async move {
                service.create_media_recording_idempotent(&creating_claims(), &input, Some(request)).await
            })
        })
        .collect();
    // Both are past their first lookup and waiting to commit.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(held);

    let mut admitted = 0;
    for handle in handles {
        match handle.await.expect("join") {
            Ok(_) => admitted += 1,
            Err(error) => assert_eq!(error, ServiceError::IdempotencyConflict),
        }
    }
    assert_eq!(admitted, 1);
    assert_eq!(queue.queue.lock().await.len(), 1);
}

#[tokio::test]
async fn the_same_recording_is_admitted_when_the_disk_has_room() {
    // The counterpart: without the safety margin the identical request
    // succeeds, so the refusal above is the disk rule and not the
    // fixture failing for some unrelated reason.
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let service = service_with_disk(dir.path(), &queue, None);

    let result = service.create_recording(&creating_claims(), &disk_test_input()).await;

    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(queue.scheduled.read().await.len(), 1);
}
