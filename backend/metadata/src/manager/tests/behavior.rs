use super::{
    create_test_worker, InputWorker, MetadataUpdateManager, MetadataUpdateRuntimeSettings, PendingTask, RetryDomain,
    ScopedTaskKey, SubmitTaskResult, TaskKey, TASK_ERR_NO_CONNECTION, TASK_ERR_PREEMPTED, TASK_ERR_UPDATE_IN_PROGRESS,
};
use dashmap::DashMap;
use shared::{
    model::{
        InputType, LiveStreamProperties, PlaylistItemType, SeriesStreamProperties, StreamProperties,
        VideoStreamProperties, VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::generate_provider_playlist_uuid,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{BatchResultCollector, ProviderIdType, ResolveReason, ResolveReasonSet, UpdateTask};
use tuliprox_repository::TargetIdMapping;

#[tokio::test]
async fn queue_task_creates_single_worker_per_input_under_concurrency() {
    let cancel_token = CancellationToken::new();
    let manager = Arc::new(MetadataUpdateManager::new(cancel_token));
    let input_name: Arc<str> = Arc::from("race_input");

    let mut joins = Vec::new();
    for id in 0..32u32 {
        let manager_cloned = manager.clone();
        let input_cloned = input_name.clone();
        joins.push(tokio::spawn(async move {
            manager_cloned
                .queue_task(
                    input_cloned,
                    UpdateTask::ResolveVod {
                        id: ProviderIdType::Id(id),
                        reason: ResolveReasonSet::default(),
                        delay: 0,
                        source_last_modified: None,
                    },
                )
                .await;
        }));
    }

    for join in joins {
        join.await.expect("queue task spawn should complete");
    }

    // Allow spawned worker startup to settle.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(manager.active_worker_count(), 1);

    manager.shutdown();
}

#[test]
fn should_skip_enqueue_respects_resolve_suppression() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_suppressed";
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Info.into(),
        delay: 1,
        source_last_modified: None,
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
    manager.resolve_enqueue_suppressions.insert(scoped_key.clone(), chrono::Utc::now().timestamp().saturating_add(60));

    assert!(manager.should_skip_enqueue_cached(input_name, &task));

    manager.resolve_enqueue_suppressions.insert(scoped_key.clone(), chrono::Utc::now().timestamp().saturating_sub(1));

    assert!(!manager.should_skip_enqueue_cached(input_name, &task));
    assert!(!manager.resolve_enqueue_suppressions.contains_key(&scoped_key));
}

#[test]
fn strip_tmdb_reasons_for_enqueue_skips_unchanged_series_tmdb_only_task() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_tmdb_marker";
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Date,
        delay: 1,
        source_last_modified: Some(777),
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
    manager.tmdb_source_markers.insert(scoped_key, 777);

    assert!(manager.strip_tmdb_reasons_for_enqueue(input_name, task).is_none());
}

#[test]
fn strip_tmdb_reasons_for_enqueue_keeps_non_tmdb_reasons_for_unchanged_series_source() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_tmdb_probe";
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Probe,
        delay: 1,
        source_last_modified: Some(777),
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
    manager.tmdb_source_markers.insert(scoped_key, 777);

    let prepared = manager.strip_tmdb_reasons_for_enqueue(input_name, task).expect("probe reason should remain");
    match prepared {
        UpdateTask::ResolveSeries { reason, source_last_modified, .. } => {
            assert_eq!(source_last_modified, Some(777));
            assert!(!reason.contains(ResolveReason::Tmdb));
            assert!(!reason.contains(ResolveReason::Date));
            assert!(reason.contains(ResolveReason::Probe));
        }
        other => panic!("unexpected task type after enqueue strip: {other:?}"),
    }
}

#[test]
fn strip_tmdb_reasons_for_enqueue_keeps_unknown_series_timestamp_and_tmdb_reason() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_tmdb_unknown_series";
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Date,
        delay: 1,
        source_last_modified: None,
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));

    assert!(!manager.tmdb_source_markers.contains_key(&scoped_key));

    let prepared =
        manager.strip_tmdb_reasons_for_enqueue(input_name, task).expect("unknown timestamps should not be suppressed");
    match prepared {
        UpdateTask::ResolveSeries { reason, source_last_modified, .. } => {
            assert_eq!(source_last_modified, None);
            assert!(reason.contains(ResolveReason::Tmdb));
            assert!(reason.contains(ResolveReason::Date));
        }
        other => panic!("unexpected task type after enqueue strip: {other:?}"),
    }
}

#[test]
fn strip_tmdb_reasons_for_enqueue_skips_unchanged_vod_tmdb_only_task() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_vod_tmdb_marker";
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Date,
        delay: 1,
        source_last_modified: Some(777),
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
    manager.tmdb_source_markers.insert(scoped_key, 777);

    assert!(manager.strip_tmdb_reasons_for_enqueue(input_name, task).is_none());
}

#[test]
fn strip_tmdb_reasons_for_enqueue_keeps_non_tmdb_reasons_for_unchanged_vod_source() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_vod_tmdb_probe";
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Probe,
        delay: 1,
        source_last_modified: Some(777),
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));
    manager.tmdb_source_markers.insert(scoped_key, 777);

    let prepared = manager.strip_tmdb_reasons_for_enqueue(input_name, task).expect("probe reason should remain");
    match prepared {
        UpdateTask::ResolveVod { reason, source_last_modified, .. } => {
            assert_eq!(source_last_modified, Some(777));
            assert!(!reason.contains(ResolveReason::Tmdb));
            assert!(!reason.contains(ResolveReason::Date));
            assert!(reason.contains(ResolveReason::Probe));
        }
        other => panic!("unexpected task type after enqueue strip: {other:?}"),
    }
}

#[test]
fn strip_tmdb_reasons_for_enqueue_keeps_unknown_vod_timestamp_and_tmdb_reason() {
    let manager = MetadataUpdateManager::new(CancellationToken::new());
    let input_name = "input_tmdb_unknown_vod";
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb | ResolveReason::Date,
        delay: 1,
        source_last_modified: None,
    };
    let scoped_key = ScopedTaskKey::new(Arc::from(input_name), TaskKey::from_task(&task));

    assert!(!manager.tmdb_source_markers.contains_key(&scoped_key));

    let prepared =
        manager.strip_tmdb_reasons_for_enqueue(input_name, task).expect("unknown timestamps should not be suppressed");
    match prepared {
        UpdateTask::ResolveVod { reason, source_last_modified, .. } => {
            assert_eq!(source_last_modified, None);
            assert!(reason.contains(ResolveReason::Tmdb));
            assert!(reason.contains(ResolveReason::Date));
        }
        other => panic!("unexpected task type after enqueue strip: {other:?}"),
    }
}

#[tokio::test]
async fn submit_task_merges_existing_task_and_increments_generation() {
    let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(0));

    let task_initial = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Info.into(),
        delay: 10,
        source_last_modified: None,
    };
    let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;
    MetadataUpdateManager::submit_task(
        tx.clone(),
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        task_initial,
    )
    .await;

    let task_merge = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Probe.into(),
        delay: 2,
        source_last_modified: None,
    };
    MetadataUpdateManager::submit_task(
        tx,
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        task_merge,
    )
    .await;

    let first_signal = rx.try_recv().expect("first signal should be queued");
    assert_eq!(first_signal, TaskKey::Vod(42));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));

    let entry = pending_tasks.get(&TaskKey::Vod(42)).expect("pending entry should exist");
    assert_eq!(entry.generation.load(Ordering::Relaxed), 1);

    let merged = entry.task.lock().clone();
    match merged {
        UpdateTask::ResolveVod { reason, delay, .. } => {
            assert!(reason.contains(ResolveReason::Info));
            assert!(reason.contains(ResolveReason::Probe));
            assert_eq!(delay, 2);
        }
        other => panic!("unexpected task type after merge: {other:?}"),
    }
}

#[tokio::test]
async fn submit_task_identical_resolve_merge_keeps_generation() {
    let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(0));
    let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;

    let initial = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb.into(),
        delay: 10,
        source_last_modified: None,
    };
    MetadataUpdateManager::submit_task(
        tx.clone(),
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        initial,
    )
    .await;

    let identical_merge = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Tmdb.into(),
        delay: 10,
        source_last_modified: None,
    };
    MetadataUpdateManager::submit_task(
        tx,
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        identical_merge,
    )
    .await;

    let first_signal = rx.try_recv().expect("first signal should be queued");
    assert_eq!(first_signal, TaskKey::Vod(42));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));

    let entry = pending_tasks.get(&TaskKey::Vod(42)).expect("pending entry should exist");
    assert_eq!(entry.generation.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn submit_task_probe_stream_merge_keeps_existing_payload_when_present() {
    let (tx, mut rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(0));
    let queue_size = MetadataUpdateRuntimeSettings::default().max_queue_size;

    let initial = UpdateTask::ProbeStream {
        probe_scope: Arc::from("scope_a"),
        unique_id: "uid_1".to_string(),
        url: "http://old.example/stream".to_string(),
        item_type: PlaylistItemType::Video,
        reason: ResolveReason::MissingDetails.into(),
        delay: 10,
    };
    MetadataUpdateManager::submit_task(
        tx.clone(),
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        initial,
    )
    .await;

    let merged_in = UpdateTask::ProbeStream {
        probe_scope: Arc::from("scope_a"),
        unique_id: "uid_1".to_string(),
        url: "http://new.example/stream".to_string(),
        item_type: PlaylistItemType::LocalVideo,
        reason: ResolveReason::Probe.into(),
        delay: 2,
    };
    MetadataUpdateManager::submit_task(
        tx,
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        queue_size,
        merged_in,
    )
    .await;

    let first_signal = rx.try_recv().expect("first signal should be queued");
    assert_eq!(first_signal, TaskKey::Stream { scope: Arc::from("scope_a"), id: Arc::from("uid_1") });
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));

    let key = TaskKey::Stream { scope: Arc::from("scope_a"), id: Arc::from("uid_1") };
    let entry = pending_tasks.get(&key).expect("pending entry should exist");
    assert_eq!(entry.generation.load(Ordering::Relaxed), 1);

    let merged = entry.task.lock().clone();
    match merged {
        UpdateTask::ProbeStream { reason, delay, url, item_type, .. } => {
            assert!(reason.contains(ResolveReason::MissingDetails));
            assert!(reason.contains(ResolveReason::Probe));
            assert_eq!(delay, 2);
            assert_eq!(url, "http://old.example/stream");
            assert_eq!(item_type, PlaylistItemType::Video);
        }
        other => panic!("unexpected task type after merge: {other:?}"),
    }
}

#[tokio::test]
async fn submit_task_with_closed_sender_does_not_report_merged_for_existing_pending_entry() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    drop(rx);

    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let key = TaskKey::Vod(42);
    let existing_task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Info.into(),
        delay: 10,
        source_last_modified: None,
    };
    pending_tasks.insert(key.clone(), PendingTask::new(existing_task));

    let incoming_task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(42),
        reason: ResolveReason::Probe.into(),
        delay: 2,
        source_last_modified: None,
    };

    let result = MetadataUpdateManager::submit_task(
        tx,
        pending_tasks.clone(),
        pending_task_count.clone(),
        "input_a",
        MetadataUpdateRuntimeSettings::default().max_queue_size,
        incoming_task,
    )
    .await;

    assert_eq!(result, SubmitTaskResult::ChannelClosed);
    assert!(!pending_tasks.contains_key(&key));
    assert_eq!(pending_task_count.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn finalize_processed_task_success_requeues_when_generation_changed() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let key = TaskKey::Vod(7);

    pending_tasks.insert(
        key.clone(),
        PendingTask::new(UpdateTask::ResolveVod {
            id: ProviderIdType::Id(7),
            reason: ResolveReason::Info.into(),
            delay: 0,
            source_last_modified: None,
        }),
    );
    if let Some(entry) = pending_tasks.get(&key) {
        entry.generation.store(1, Ordering::Relaxed);
    }

    let mut worker = create_test_worker("input_a", tx, rx, pending_tasks.clone(), pending_task_count);

    let requeued = worker.finalize_processed_task_success(&key, 0, "input_a").await;
    assert!(requeued);
    assert!(pending_tasks.contains_key(&key));
    assert_eq!(worker.receiver.try_recv().expect("requeued signal should be present"), key);
}

#[tokio::test]
async fn finalize_processed_task_success_removes_when_unchanged() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let key = TaskKey::Vod(9);
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(9),
        reason: ResolveReason::Info.into(),
        delay: 0,
        source_last_modified: None,
    };

    pending_tasks.insert(key.clone(), PendingTask::new(task.clone()));

    let mut worker = create_test_worker("input_b", tx, rx, pending_tasks.clone(), pending_task_count);
    let runtime_settings = MetadataUpdateRuntimeSettings::default();

    worker.scheduled_requeues.insert(key.clone(), chrono::Utc::now().timestamp().saturating_add(30));
    worker.recently_completed_no_change.insert(key.clone(), (Instant::now(), ResolveReason::Info.into()));
    assert!(worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
    assert!(worker.recently_completed_no_change.contains_key(&key));
    assert!(!worker.scheduled_requeues.contains_key(&key));

    let requeued = worker.finalize_processed_task_success(&key, 0, "input_b").await;
    assert!(!requeued);
    assert!(!pending_tasks.contains_key(&key));
    assert!(matches!(worker.receiver.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
}

#[test]
fn recent_no_change_skip_requires_exact_reason_match() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(0));
    let key = TaskKey::Vod(10);
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(10),
        reason: ResolveReason::Info.into(),
        delay: 0,
        source_last_modified: None,
    };
    let mut worker = create_test_worker("input_c", tx, rx, pending_tasks, pending_task_count);
    let runtime_settings = MetadataUpdateRuntimeSettings::default();

    worker
        .recently_completed_no_change
        .insert(key.clone(), (Instant::now(), ResolveReason::Info | ResolveReason::Probe));
    assert!(!worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
    assert!(!worker.recently_completed_no_change.contains_key(&key));

    worker.recently_completed_no_change.insert(key.clone(), (Instant::now(), ResolveReason::Info.into()));
    assert!(worker.should_skip_recent_no_change_task(&key, &task, &runtime_settings));
    assert!(worker.recently_completed_no_change.contains_key(&key));
}

#[test]
fn task_needs_provider_connection_skips_library_probe_stream() {
    let task = UpdateTask::ProbeStream {
        probe_scope: Arc::from("input_a"),
        unique_id: "u1".to_string(),
        url: "file:///movie.mkv".to_string(),
        item_type: PlaylistItemType::LocalVideo,
        reason: ResolveReason::MissingDetails.into(),
        delay: 0,
    };

    assert!(!InputWorker::task_needs_provider_connection(&task, InputType::Library));
}

#[test]
fn task_needs_provider_connection_skips_media_server_probe_stream() {
    let task = UpdateTask::ProbeStream {
        probe_scope: Arc::from("input_remote"),
        unique_id: "u1".to_string(),
        url: "http://example.com/stream.mkv".to_string(),
        item_type: PlaylistItemType::Video,
        reason: ResolveReason::MissingDetails.into(),
        delay: 0,
    };

    for input_type in [InputType::Emby, InputType::Jellyfin, InputType::Plex] {
        assert!(!InputWorker::task_needs_provider_connection(&task, input_type));
    }
    assert!(InputWorker::task_needs_provider_connection(&task, InputType::M3u));
}

#[test]
fn task_needs_provider_connection_keeps_non_library_probe_stream() {
    let task = UpdateTask::ProbeStream {
        probe_scope: Arc::from("input_a"),
        unique_id: "u1".to_string(),
        url: "http://example.com/stream.m3u8".to_string(),
        item_type: PlaylistItemType::Video,
        reason: ResolveReason::MissingDetails.into(),
        delay: 0,
    };

    assert!(InputWorker::task_needs_provider_connection(&task, InputType::M3u));
    assert!(InputWorker::task_needs_provider_connection(&task, InputType::Xtream));
}

#[test]
fn task_needs_provider_connection_keeps_live_probe() {
    let task = UpdateTask::ProbeLive {
        id: ProviderIdType::Id(1),
        reason: ResolveReason::Probe.into(),
        delay: 0,
        interval: 60,
    };

    assert!(InputWorker::task_needs_provider_connection(&task, InputType::Library));
}

#[test]
fn collect_series_virtual_updates_resolves_text_ids_via_series_info_uuid() {
    assert_collect_resolves_text_id!(
        collect_series_virtual_updates,
        PlaylistItemType::SeriesInfo,
        "input_series",
        "series-text-id",
        add_series,
        SeriesStreamProperties::default()
    );
}

#[test]
fn collect_vod_virtual_updates_resolves_text_ids_via_video_uuid() {
    assert_collect_resolves_text_id!(
        collect_vod_virtual_updates,
        PlaylistItemType::Video,
        "input_vod",
        "vod-text-id",
        add_vod,
        VideoStreamProperties::default()
    );
}

#[test]
fn collect_live_virtual_updates_resolves_text_ids_via_live_uuid() {
    assert_collect_resolves_text_id!(
        collect_live_virtual_updates,
        PlaylistItemType::Live,
        "input_live",
        "live-text-id",
        add_live,
        LiveStreamProperties::default()
    );
}

#[test]
fn generic_probe_uses_pending_vod_batch_as_update_base() {
    let mut batch = BatchResultCollector::new();
    let pending_props =
        VideoStreamProperties { container_extension: Arc::from("mkv"), ..VideoStreamProperties::default() };
    batch.add_vod(ProviderIdType::Id(42), pending_props);

    let mut item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(7),
        provider_id: 42,
        name: Arc::from("Movie"),
        logo: Arc::from(""),
        logo_small: Arc::from(""),
        group: Arc::from(""),
        title: Arc::from(""),
        parent_code: Arc::from(""),
        rec: Arc::from(""),
        url: Arc::from(""),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: None,
        item_type: PlaylistItemType::Video,
        category_id: 0,
        input_name: Arc::from("input"),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: Arc::from("42"),
        upstream_user_agent: None,
    };

    InputWorker::apply_pending_generic_probe_base(&batch, XtreamCluster::Video, 42, &mut item);

    let Some(StreamProperties::Video(props)) = item.additional_properties else {
        panic!("expected pending video properties");
    };
    assert_eq!(props.container_extension.as_ref(), "mkv");
}

#[test]
fn strip_tmdb_reasons_returns_none_for_tmdb_only_resolve_task() {
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(100),
        reason: ResolveReason::Tmdb | ResolveReason::Date,
        delay: 5,
        source_last_modified: None,
    };

    assert!(InputWorker::strip_tmdb_reasons(&task).is_none());
}

#[test]
fn strip_tmdb_reasons_keeps_non_tmdb_reasons() {
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(5),
        reason: ResolveReason::Tmdb | ResolveReason::Probe | ResolveReason::Info,
        delay: 1,
        source_last_modified: Some(123),
    };

    let stripped = InputWorker::strip_tmdb_reasons(&task).expect("task should keep non-tmdb reasons");
    match stripped {
        UpdateTask::ResolveSeries { reason, .. } => {
            assert!(!reason.contains(ResolveReason::Tmdb));
            assert!(!reason.contains(ResolveReason::Date));
            assert!(reason.contains(ResolveReason::Probe));
            assert!(reason.contains(ResolveReason::Info));
        }
        other => panic!("unexpected task type after strip: {other:?}"),
    }
}

#[test]
fn merge_task_payload_resolve_vod_unknown_last_modified_wins() {
    let mut existing = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(7),
        reason: ResolveReason::Info.into(),
        delay: 5,
        source_last_modified: Some(123),
    };
    let changed = MetadataUpdateManager::merge_task_payload(
        &mut existing,
        UpdateTask::ResolveVod {
            id: ProviderIdType::Id(7),
            reason: ResolveReason::Probe.into(),
            delay: 3,
            source_last_modified: None,
        },
    );

    assert!(changed);
    match existing {
        UpdateTask::ResolveVod { reason, delay, source_last_modified, .. } => {
            assert!(reason.contains(ResolveReason::Info));
            assert!(reason.contains(ResolveReason::Probe));
            assert_eq!(delay, 3);
            assert_eq!(source_last_modified, None);
        }
        other => panic!("unexpected merged task type: {other:?}"),
    }
}

#[test]
fn merge_task_payload_resolve_series_unknown_last_modified_wins() {
    let mut existing = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(9),
        reason: ResolveReason::Info.into(),
        delay: 2,
        source_last_modified: None,
    };
    let changed = MetadataUpdateManager::merge_task_payload(
        &mut existing,
        UpdateTask::ResolveSeries {
            id: ProviderIdType::Id(9),
            reason: ResolveReason::Tmdb.into(),
            delay: 2,
            source_last_modified: Some(456),
        },
    );

    assert!(changed);
    match existing {
        UpdateTask::ResolveSeries { reason, delay, source_last_modified, .. } => {
            assert!(reason.contains(ResolveReason::Info));
            assert!(reason.contains(ResolveReason::Tmdb));
            assert_eq!(delay, 2);
            assert_eq!(source_last_modified, None);
        }
        other => panic!("unexpected merged task type: {other:?}"),
    }
}

#[test]
fn transient_worker_errors_include_connection_unavailable() {
    assert!(InputWorker::is_transient_worker_error(TASK_ERR_UPDATE_IN_PROGRESS));
    assert!(InputWorker::is_transient_worker_error(TASK_ERR_PREEMPTED));
    assert!(InputWorker::is_transient_worker_error(TASK_ERR_NO_CONNECTION));
    assert!(!InputWorker::is_transient_worker_error("permanent error"));
}

#[test]
fn permanent_not_found_error_matches_standalone_markers() {
    assert!(InputWorker::is_permanent_not_found_error("HTTP 404 Not Found"));
    assert!(InputWorker::is_permanent_not_found_error("probe failed: 404: stream unavailable"));
    assert!(InputWorker::is_permanent_not_found_error("resource not found on provider"));
}

#[test]
fn permanent_not_found_error_ignores_partial_markers() {
    assert!(!InputWorker::is_permanent_not_found_error("error code 1404 while probing"));
    assert!(!InputWorker::is_permanent_not_found_error("status404unexpected"));
    assert!(!InputWorker::is_permanent_not_found_error("movie not foundry metadata mismatch"));
}

#[test]
fn take_pending_probe_task_snapshot_skips_scheduled_requeues() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let worker = create_test_worker("input_probe_skip", tx, rx, pending_tasks.clone(), pending_task_count);

    let key = TaskKey::Live(100);
    let task = UpdateTask::ProbeLive {
        id: ProviderIdType::Id(100),
        reason: ResolveReason::Probe.into(),
        delay: 0,
        interval: 60,
    };
    pending_tasks.insert(key.clone(), PendingTask::new(task));
    worker.scheduled_requeues.insert(key, chrono::Utc::now().timestamp().saturating_add(30));

    assert!(worker.take_pending_probe_task_snapshot().is_none());
}

#[test]
fn take_pending_probe_task_snapshot_accepts_probe_only_resolve() {
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(1));
    let worker = create_test_worker("input_probe_only_resolve", tx, rx, pending_tasks.clone(), pending_task_count);

    let key = TaskKey::Vod(77);
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(77),
        reason: ResolveReason::Probe.into(),
        delay: 0,
        source_last_modified: None,
    };
    pending_tasks.insert(key.clone(), PendingTask::new(task));

    let snapshot = worker.take_pending_probe_task_snapshot().expect("expected probe-domain snapshot");
    assert_eq!(snapshot.0, key);
    assert_eq!(InputWorker::retry_domain_for_task(&snapshot.1), RetryDomain::Probe);
}

#[test]
fn playlist_trigger_ignores_probe_only_changes() {
    let task = UpdateTask::ProbeLive {
        id: ProviderIdType::Id(22),
        reason: ResolveReason::Probe.into(),
        delay: 0,
        interval: 60,
    };
    assert!(!InputWorker::should_trigger_playlist_update_for_task(&task, true));
}

#[test]
fn playlist_trigger_keeps_info_changes() {
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(33),
        reason: ResolveReason::Info.into(),
        delay: 0,
        source_last_modified: None,
    };
    assert!(InputWorker::should_trigger_playlist_update_for_task(&task, true));
    assert!(!InputWorker::should_trigger_playlist_update_for_task(&task, false));
}
