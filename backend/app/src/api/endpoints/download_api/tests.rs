use super::{
    active_download_snapshot, active_download_snapshot_for_worker, broadcast_download_queue_update,
    broadcast_required_worker_mutation, broadcast_worker_mutation, cancel_active_and_promote, cancel_download,
    commit_acquired_download, finish_active_and_promote, mark_recording_notification, parse_content_range_total,
    pause_download, preemption_reason_for, recording_deadline_reached, recording_execution_download,
    refresh_recording_progress, requeue_active_download_for_capacity_wait, requeue_active_download_for_retry,
    resume_download, retryable_transport_error_message, rollback_last_recording_marker, set_active_download_state,
    should_exit_worker_after_preempt, DownloadActionRequest, DOWNLOAD_PREEMPTED_REASON, RECORDING_PREEMPTED_REASON,
};
use crate::{
    api::model::{
        recording_notification::LifecycleEvent, ActiveProviderManager, ActiveUserManager, AppState, CancelTokens,
        ConnectionManager, DownloadControl, DownloadKind, DownloadQueue, DownloadState, EventManager, EventMessage,
        FileDownload, MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, UpdateGuard,
    },
    model::{
        ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, MediaToolCapabilities, MessageContent,
        ProcessTargets, SourcesConfig,
    },
    repository::GeoIp,
    utils::FileLockManager,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::response::IntoResponse;
use reqwest::header::{HeaderMap, HeaderValue};
use shared::{
    model::{
        ConfigPaths, InputFetchMethod, InputType, RecordingMetadata, RecordingOwner, RecordingSource,
        RecordingVisibility, UserId,
    },
    utils::Internable,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, RwLock};

fn make_download(
    kind: DownloadKind,
    state: DownloadState,
    start_at: Option<i64>,
    duration_secs: Option<u64>,
) -> FileDownload {
    FileDownload {
        uuid: "id".to_string(),
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from("/tmp/file.ts"),
        filename: "file.ts".to_string(),
        url: reqwest::Url::parse("https://example.com/file.ts").expect("valid url"),
        finished: false,
        size: 128,
        total_size: Some(1024),
        paused: false,
        error: Some("transient".to_string()),
        state,
        start_at,
        duration_secs,
        kind,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: None,
    }
}

fn attach_recording(download: &mut FileDownload, owner: RecordingOwner, visibility: RecordingVisibility) {
    let mut metadata =
        RecordingMetadata::new(owner, visibility, RecordingSource::new("1", "42", "input-a"), 1_000, 1_060, 0, 0);
    metadata.program_title = Some("Programme".to_string());
    metadata.channel_name = Some("Channel".to_string());
    metadata.relative_path = Some("Channel/Programme.ts".to_string());
    download.recording = Some(metadata);
}

#[test]
fn recording_notification_marker_is_at_most_once() {
    let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
    attach_recording(&mut download, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);

    let first = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);
    let duplicate = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);

    assert!(matches!(first.message, Some(MessageContent::RecordingLifecycle(_))));
    assert!(duplicate.message.is_none());
    assert_eq!(download.recording.as_ref().map_or(0, |metadata| metadata.notification_markers.len()), 1);
}

#[test]
fn private_user_recording_notification_is_suppressed() {
    let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
    attach_recording(&mut download, RecordingOwner::User(UserId::from("web:alice")), RecordingVisibility::Private);

    let message = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);

    assert!(message.message.is_none());
    assert_eq!(download.recording.as_ref().map_or(0, |metadata| metadata.notification_markers.len()), 0);
}

#[test]
fn rollback_last_recording_marker_removes_most_recent_matching_kind() {
    let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
    attach_recording(&mut download, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);

    // Two distinct marker kinds end up in the same metadata after a
    // successful Completed followed by a Failed on the same task — this
    // mirrors what would happen in production across two persist rounds.
    let first = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);
    let second = mark_recording_notification(&mut download, LifecycleEvent::Failed, Some("ffmpeg exited".to_string()));
    assert!(first.marker_kind.is_some());
    assert!(second.marker_kind.is_some());

    let kind = second.marker_kind.unwrap();
    rollback_last_recording_marker(&mut download, &kind);

    let markers = &download.recording.as_ref().unwrap().notification_markers;
    assert_eq!(markers.len(), 1, "only the Completed marker should remain");
    assert!(matches!(markers[0].kind, shared::model::recording::NotificationMarkerKind::Completed));
}

#[test]
fn recording_deadline_uses_start_plus_duration() {
    let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));
    let normal = make_download(DownloadKind::Download, DownloadState::Downloading, Some(1_000), Some(60));

    assert!(!recording_deadline_reached(&recording, 1_059));
    assert!(recording_deadline_reached(&recording, 1_060));
    assert!(!recording_deadline_reached(&normal, 1_060));
}

#[test]
fn recording_execution_requires_metadata_source() {
    let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));

    let result = recording_execution_download(&create_test_app_config(), &recording);

    assert_eq!(result.as_ref().err().map(String::as_str), Some("Recording source metadata missing"));
}

#[test]
fn recording_execution_uses_fresh_token_without_mutating_persisted_descriptor() {
    let mut recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));
    recording.url = reqwest::Url::parse(
        "tuliprox-recording://source?target_name=stable-target&input_name=provider_1&virtual_id=42&cluster=live",
    )
    .expect("valid descriptor");
    attach_recording(&mut recording, RecordingOwner::LegacyAdmin, RecordingVisibility::Private);
    let source = recording.recording.as_mut().and_then(|metadata| metadata.source.as_mut()).expect("recording source");
    source.target_id = "stable-target".to_string();
    source.virtual_id = "42".to_string();
    source.input_name = "provider_1".to_string();
    let persisted_before = DownloadQueue::to_persisted(&recording);
    let app_config = create_test_app_config();

    let execution = recording_execution_download(&app_config, &recording).expect("execution download");
    let token = execution
        .url
        .path_segments()
        .and_then(|segments| segments.collect::<Vec<_>>().get(4).copied())
        .expect("route token");

    assert!(crate::auth::verify_access_token(
        token,
        &app_config.access_token_secret,
        crate::auth::scope::INTERNAL_PLAYER
    ));
    assert_eq!(recording.url.as_str(), persisted_before.url);
    assert_eq!(DownloadQueue::to_persisted(&recording).url, persisted_before.url);
    assert_ne!(execution.url, recording.url);
}

#[tokio::test]
async fn retry_requeues_active_download_at_front_in_one_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads_state.json");
    let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
    let queued = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
    let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);

    queue.queue.lock().await.push_back(queued);
    *queue.active.write().await = Some(active);

    requeue_active_download_for_retry(&queue, "id", false).await.expect("requeue retry");

    assert!(queue.active.read().await.is_none());
    let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
    assert_eq!(queued_items.len(), 2);
    assert_eq!(queued_items[0].state, DownloadState::Queued);
    assert_eq!(queued_items[0].size, 128);
    assert!(queued_items[0].error.is_none());
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
    let persisted: crate::api::model::PersistedDownloadQueue =
        serde_json::from_slice(&std::fs::read(state_file).expect("read state")).expect("parse state");
    assert_eq!(persisted.revision, shared::model::QueueRevision(1));
}

#[tokio::test]
async fn preempted_active_download_requeues_to_capacity_wait_with_partial_progress() {
    let queue = DownloadQueue::new();
    let mut active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
    active.size = 512;
    active.total_size = Some(2048);
    *queue.active.write().await = Some(active);

    requeue_active_download_for_capacity_wait(&queue, "id", DOWNLOAD_PREEMPTED_REASON, false, None)
        .await
        .expect("requeue capacity wait");

    assert!(queue.active.read().await.is_none());
    let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
    assert_eq!(queued_items.len(), 1);
    assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
    assert_eq!(queued_items[0].size, 512);
    assert_eq!(queued_items[0].total_size, Some(2048));
    assert_eq!(queued_items[0].error.as_deref(), Some(DOWNLOAD_PREEMPTED_REASON));
}

#[tokio::test]
async fn terminal_transition_finishes_active_and_promotes_next_in_one_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = DownloadQueue::new_with_state_file(Some(dir.path().join("downloads_state.json")));
    *queue.active.write().await = Some(make_download(DownloadKind::Download, DownloadState::Downloading, None, None));
    let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
    next.uuid = "next".to_string();
    queue.queue.lock().await.push_back(next);

    finish_active_and_promote(&queue, "id", |finished| {
        finished.finished = true;
        finished.state = DownloadState::Completed;
        super::RecordingNotificationPlan::empty()
    })
    .await
    .expect("terminal commit");

    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(queue.finished.read().await.len(), 1);
    assert_eq!(queue.active.read().await.as_ref().map(|active| active.uuid.as_str()), Some("next"));
}

#[tokio::test]
async fn worker_mutation_failure_keeps_memory_and_revision_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocking_dir = dir.path().join("state");
    std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
    let queue = DownloadQueue::new_with_state_file(Some(blocking_dir));
    *queue.active.write().await = Some(make_download(DownloadKind::Download, DownloadState::Downloading, None, None));

    let result = requeue_active_download_for_retry(&queue, "id", false).await;

    assert!(result.is_err());
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(queue.queue.lock().await.is_empty());
    assert_eq!(queue.active.read().await.as_ref().map(|active| active.state.clone()), Some(DownloadState::Downloading));
}

#[tokio::test]
async fn preempted_active_recording_requeues_with_recording_specific_policy_message() {
    let queue = DownloadQueue::new();
    let mut active = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(600));
    active.size = 512;
    *queue.active.write().await = Some(active);

    requeue_active_download_for_capacity_wait(&queue, "id", RECORDING_PREEMPTED_REASON, false, None)
        .await
        .expect("requeue recording");

    let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
    assert_eq!(queued_items.len(), 1);
    assert_eq!(queued_items[0].kind, DownloadKind::Recording);
    assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
    assert_eq!(queued_items[0].error.as_deref(), Some(RECORDING_PREEMPTED_REASON));
}

#[test]
fn preemption_reason_is_explicit_for_recordings_and_downloads() {
    let download = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
    let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));

    assert_eq!(preemption_reason_for(&download), DOWNLOAD_PREEMPTED_REASON);
    assert_eq!(preemption_reason_for(&recording), RECORDING_PREEMPTED_REASON);
}

#[test]
fn only_restart_exits_worker_after_preempt() {
    assert!(!should_exit_worker_after_preempt(DownloadControl::None));
    assert!(!should_exit_worker_after_preempt(DownloadControl::Pause));
    assert!(!should_exit_worker_after_preempt(DownloadControl::Cancel));
    assert!(should_exit_worker_after_preempt(DownloadControl::Restart));
}

#[tokio::test]
async fn set_active_download_state_updates_snapshot_state() {
    let queue = DownloadQueue::new();
    let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
    *queue.active.write().await = Some(active);

    let changed =
        set_active_download_state(&queue, "id", DownloadState::WaitingForCapacity, Some("waiting".to_string()), false)
            .await;

    assert!(changed.expect("set active state"));
    let active = queue.active.read().await.clone().expect("active download");
    assert_eq!(active.state, DownloadState::WaitingForCapacity);
    assert_eq!(active.error.as_deref(), Some("waiting"));
    assert!(!active.paused);
}

#[tokio::test]
async fn acquisition_without_provider_handle_commits_downloading_state() {
    let queue = DownloadQueue::new();
    *queue.active.write().await = Some(make_download(DownloadKind::Download, DownloadState::Queued, None, None));

    let notification = commit_acquired_download(&queue, "id").await.expect("acquired commit");

    assert!(notification.is_some());
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(queue.active.read().await.as_ref().map(|active| active.state.clone()), Some(DownloadState::Downloading));
}

#[tokio::test]
async fn acquired_transition_rejects_switched_active_task() {
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let queue = DownloadQueue::new();
    let mut switched = make_download(DownloadKind::Recording, DownloadState::Queued, None, None);
    switched.uuid = "task-b".to_string();
    attach_recording(&mut switched, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);
    *queue.active.write().await = Some(switched);

    let transition = commit_acquired_download(&queue, "task-a").await.map(|notification| notification.is_some());
    let result =
        broadcast_required_worker_mutation(&event_manager, &queue, transition, "acquired downloading state").await;

    assert!(result.is_err());
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(queue.active.read().await.as_ref().map(|active| active.uuid.as_str()), Some("task-b"));
    assert_eq!(
        queue
            .active
            .read()
            .await
            .as_ref()
            .and_then(|active| active.recording.as_ref())
            .map_or(0, |recording| recording.notification_markers.len()),
        0
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn post_acquire_snapshot_rejects_switched_active_task() {
    let queue = DownloadQueue::new();
    let mut switched = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
    switched.uuid = "task-b".to_string();
    *queue.active.write().await = Some(switched);

    assert!(active_download_snapshot_for_worker(&queue.active, "task-a").await.is_none());
}

#[tokio::test]
async fn stale_worker_progress_does_not_update_switched_active_task() {
    let queue = DownloadQueue::new();
    let mut switched = make_download(DownloadKind::Recording, DownloadState::Downloading, None, None);
    switched.uuid = "task-b".to_string();
    switched.size = 10;
    *queue.active.write().await = Some(switched);
    let dir = tempfile::tempdir().expect("tempdir");
    let progress_path = dir.path().join("task-a.ts.part");
    std::fs::write(&progress_path, [0_u8; 20]).expect("progress file");
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();

    refresh_recording_progress(&queue.active, "task-a", &progress_path, &event_manager).await;

    assert_eq!(queue.active.read().await.as_ref().map(|active| active.size), Some(10));
    assert!(events.try_recv().is_err());
}

#[test]
fn compute_download_retry_backoff_uses_multiplier_and_cap() {
    let download_cfg = crate::model::VideoDownloadConfig {
        headers: std::collections::HashMap::new(),
        directory: "/tmp".to_string(),
        organize_into_directories: false,
        episode_pattern: None,
        download_priority: 0,
        recording_priority: 0,
        reserve_slots_for_users: 0,
        max_background_per_provider: 0,
        retry_backoff_initial_secs: 3,
        retry_backoff_multiplier: 3.0,
        retry_backoff_max_secs: 30,
        retry_backoff_jitter_percent: 0,
        retry_max_attempts: 5,
        recording: None,
    };

    assert_eq!(super::compute_download_retry_backoff_secs(1, &download_cfg), 3);
    assert_eq!(super::compute_download_retry_backoff_secs(2, &download_cfg), 9);
    assert_eq!(super::compute_download_retry_backoff_secs(3, &download_cfg), 27);
    assert_eq!(super::compute_download_retry_backoff_secs(8, &download_cfg), 30);
}

#[test]
fn background_download_waits_when_all_candidates_hit_background_limit() {
    let download_cfg = crate::model::VideoDownloadConfig {
        headers: std::collections::HashMap::new(),
        directory: "/tmp".to_string(),
        organize_into_directories: false,
        episode_pattern: None,
        download_priority: 0,
        recording_priority: 0,
        reserve_slots_for_users: 0,
        max_background_per_provider: 2,
        retry_backoff_initial_secs: 3,
        retry_backoff_multiplier: 3.0,
        retry_backoff_max_secs: 30,
        retry_backoff_jitter_percent: 0,
        retry_max_attempts: 5,
        recording: None,
    };

    let capacities = vec![(Arc::<str>::from("a"), 2, 5), (Arc::<str>::from("b"), 3, 5)];
    assert!(super::background_download_should_wait(1, &capacities, &download_cfg));
    assert!(!super::background_download_should_wait(0, &capacities, &download_cfg));
}

#[test]
fn background_download_waits_when_reserved_user_slots_would_be_consumed() {
    let download_cfg = crate::model::VideoDownloadConfig {
        headers: std::collections::HashMap::new(),
        directory: "/tmp".to_string(),
        organize_into_directories: false,
        episode_pattern: None,
        download_priority: 0,
        recording_priority: 0,
        reserve_slots_for_users: 1,
        max_background_per_provider: 0,
        retry_backoff_initial_secs: 3,
        retry_backoff_multiplier: 3.0,
        retry_backoff_max_secs: 30,
        retry_backoff_jitter_percent: 0,
        retry_max_attempts: 5,
        recording: None,
    };

    let blocked = vec![(Arc::<str>::from("a"), 4, 5), (Arc::<str>::from("b"), 4, 5)];
    let allowed = vec![(Arc::<str>::from("a"), 3, 5), (Arc::<str>::from("b"), 4, 6)];
    assert!(super::background_download_should_wait(1, &blocked, &download_cfg));
    assert!(!super::background_download_should_wait(1, &allowed, &download_cfg));
}

#[test]
fn retryable_transport_error_message_detects_common_transient_failures() {
    assert!(retryable_transport_error_message("dns lookup failed"));
    assert!(retryable_transport_error_message("connection reset by peer"));
    assert!(retryable_transport_error_message("operation timed out"));
    assert!(!retryable_transport_error_message("invalid URL"));
}

#[tokio::test]
async fn active_download_snapshot_releases_read_lock_before_followup_write() {
    let active = Arc::new(RwLock::new(Some(FileDownload {
        uuid: "id".to_string(),
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from("/tmp/file.bin"),
        filename: "deadlock-test.bin".to_string(),
        url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state: DownloadState::Downloading,
        start_at: None,
        duration_secs: None,
        kind: DownloadKind::Download,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: None,
    })));
    let snapshot = active_download_snapshot(&active).await;
    assert!(snapshot.is_some());

    let write_result = tokio::time::timeout(Duration::from_millis(100), active.write()).await;
    assert!(write_result.is_ok(), "write lock should not be blocked by snapshot helper");
}

#[test]
fn parse_content_range_total_extracts_full_size() {
    let mut headers = HeaderMap::new();
    headers.insert("content-range", HeaderValue::from_static("bytes 512-1023/4096"));

    assert_eq!(parse_content_range_total(&headers), Some(4096));
}

fn create_test_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-1.example".to_string(),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: None,
        ..ConfigInput::default()
    });
    let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::from(Some(Arc::new(ApiProxyConfig {
            server: vec![ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "player.example".to_string(),
                port: None,
                timezone: "UTC".to_string(),
                message: String::new(),
                path: None,
            }],
            ..ApiProxyConfig::default()
        })))),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
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
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

fn create_test_app_state_with_downloads(downloads: Arc<DownloadQueue>) -> Arc<AppState> {
    let app_cfg = Arc::new(create_test_app_config());
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));

    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
    let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

    Arc::new(AppState {
        forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        })),
        app_config: app_cfg,
        http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        downloads,
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
        hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
        stalker_resolve_coordinator: Arc::default(),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        update_guard: UpdateGuard::new(),
        metadata_manager,
        identity_registry: Arc::new(tuliprox_repository::identity_registry::IdentityRegistry::empty(
            std::path::PathBuf::new(),
        )),
        login_throttle: Arc::new(crate::auth::LoginThrottle::new()),
        token_revocations: Arc::new(tuliprox_repository::token_revocations::TokenRevocations::empty(
            std::path::PathBuf::new(),
        )),
        manual_update_sender,
    })
}

fn create_test_app_state() -> Arc<AppState> { create_test_app_state_with_downloads(Arc::new(DownloadQueue::new())) }

#[tokio::test]
async fn pause_persist_failure_returns_error_without_event_or_memory_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocking_dir = dir.path().join("state");
    std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
    let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
    *downloads.active.write().await =
        Some(make_download(DownloadKind::Download, DownloadState::Downloading, None, None));
    let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
    let mut events = app_state.event_manager.get_event_channel();

    let response = pause_download(
        axum::extract::State(app_state),
        axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
    assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    let active = downloads.active.read().await;
    assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Downloading));
    assert_eq!(active.as_ref().map(|download| download.paused), Some(false));
}

#[tokio::test]
async fn resume_persist_failure_returns_error_without_event_or_memory_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocking_dir = dir.path().join("state");
    std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
    let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
    let mut paused = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
    paused.paused = true;
    *downloads.active.write().await = Some(paused);
    let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
    let mut events = app_state.event_manager.get_event_channel();

    let response = resume_download(
        axum::extract::State(app_state),
        axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
    assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    let active = downloads.active.read().await;
    assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Paused));
    assert_eq!(active.as_ref().map(|download| download.paused), Some(true));
}

#[tokio::test]
async fn paused_cancel_persist_failure_returns_error_without_event_or_memory_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocking_dir = dir.path().join("state");
    std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
    let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
    let mut paused = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
    paused.paused = true;
    *downloads.active.write().await = Some(paused);
    let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
    let mut events = app_state.event_manager.get_event_channel();

    let response = cancel_download(
        axum::extract::State(app_state),
        axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
    assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(downloads.finished.read().await.is_empty());
    let active = downloads.active.read().await;
    assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Paused));
    assert_eq!(active.as_ref().map(|download| download.paused), Some(true));
}

#[tokio::test]
async fn cancel_normalizes_active_and_promotes_next_in_one_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = DownloadQueue::new_with_state_file(Some(dir.path().join("downloads_state.json")));
    let mut active = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
    active.paused = true;
    active.next_retry_at = Some(42);
    active.error = None;
    *queue.active.write().await = Some(active);
    let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
    next.uuid = "next".to_string();
    queue.queue.lock().await.push_back(next);

    let committed = cancel_active_and_promote(&queue, "id").await.expect("cancel commit");

    assert!(committed);
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
    let finished = queue.finished.read().await;
    let cancelled = finished.first().expect("cancelled task");
    assert!(cancelled.finished);
    assert!(!cancelled.paused);
    assert_eq!(cancelled.state, DownloadState::Cancelled);
    assert_eq!(cancelled.error.as_deref(), Some("Cancelled by user"));
    assert!(cancelled.next_retry_at.is_none());
    assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("next"));
}

#[tokio::test]
async fn cancel_uuid_mismatch_does_not_finish_or_promote_next_task() {
    let queue = DownloadQueue::new();
    *queue.active.write().await = Some(make_download(DownloadKind::Download, DownloadState::Paused, None, None));
    let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
    next.uuid = "next".to_string();
    queue.queue.lock().await.push_back(next);

    let committed = cancel_active_and_promote(&queue, "next").await.expect("cancel no-op");

    assert!(!committed);
    assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("id"));
    assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("next"));
    assert!(queue.finished.read().await.is_empty());
}

#[tokio::test]
async fn worker_noop_mutation_does_not_broadcast() {
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let queue = DownloadQueue::new();

    let changed = broadcast_worker_mutation(&event_manager, &queue, Ok(false), "test no-op mutation").await;

    assert!(!changed.expect("no-op result"));
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn worker_commit_error_is_propagated_without_clearing_control() {
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let queue = DownloadQueue::new();
    *queue.control_signal.write().await = DownloadControl::Cancel;

    let result = broadcast_worker_mutation(
        &event_manager,
        &queue,
        Err(crate::api::model::QueueMutationError::DiskFull),
        "terminal transition",
    )
    .await;

    assert!(result.is_err());
    assert_eq!(*queue.control_signal.read().await, DownloadControl::Cancel);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn required_worker_noop_is_an_error_without_broadcast() {
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let queue = DownloadQueue::new();

    let result = broadcast_required_worker_mutation(&event_manager, &queue, Ok(false), "terminal transition").await;

    assert!(result.is_err());
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn pause_and_resume_handlers_return_without_hanging() {
    let app_state = create_test_app_state();
    let active = FileDownload {
        uuid: "handler-id".to_string(),
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from("/tmp/handler-file.bin"),
        filename: "handler-file.bin".to_string(),
        url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
        finished: false,
        size: 32,
        total_size: Some(64),
        paused: false,
        error: None,
        state: DownloadState::Downloading,
        start_at: None,
        duration_secs: None,
        kind: DownloadKind::Download,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: None,
    };
    *app_state.downloads.active.write().await = Some(active);

    let pause_response = tokio::time::timeout(
        Duration::from_millis(100),
        pause_download(
            axum::extract::State(Arc::clone(&app_state)),
            axum::extract::Json(DownloadActionRequest { uuid: "handler-id".to_string() }),
        ),
    )
    .await;
    assert!(pause_response.is_ok(), "pause handler should return promptly");

    let resume_response = tokio::time::timeout(
        Duration::from_millis(100),
        resume_download(
            axum::extract::State(app_state),
            axum::extract::Json(DownloadActionRequest { uuid: "handler-id".to_string() }),
        ),
    )
    .await;
    assert!(resume_response.is_ok(), "resume handler should return promptly");

    let _ = pause_response.expect("pause response").into_response();
    let _ = resume_response.expect("resume response").into_response();
}

#[tokio::test]
async fn queue_update_notifies_recording_subscribers() {
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let queue = DownloadQueue::new();

    broadcast_download_queue_update(&event_manager, &queue).await;

    let mut recording_changed = false;
    while let Ok(event) = events.try_recv() {
        if event == EventMessage::RecordingChanged {
            recording_changed = true;
        }
    }
    assert!(recording_changed);
}
