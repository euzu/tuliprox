#[cfg(unix)]
use super::ConcurrentLiveFixture;
use super::{app_config_with_listener, counting_ffmpeg, ensure_recording_worker_running, scheduled_task};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::RecordingQueue,
};
use shared::model::{NoopSink, RecordingKind, RecordingTaskState};
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::{path::Path, sync::Arc, time::Duration};
use tuliprox_core::model::RecordingConfig;

#[cfg(unix)]
#[tokio::test]
async fn live_workers_respect_capacity_and_start_waiting_recordings_when_a_slot_is_released(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = ConcurrentLiveFixture::new(4, 2).await?;
    fixture.start().await?;
    let running = fixture.wait_for_running(2).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.queue.slot_waiters.snapshots().len() != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(fixture.capacity.in_use.load(Ordering::SeqCst), 2);
    fixture.queue.cancel_requested(&running[0]).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let now_running = fixture.wait_for_running(2).await?;
            if now_running.iter().any(|uuid| !running.contains(uuid)) {
                break Ok::<_, tokio::time::error::Elapsed>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 2);
    assert!(fixture
        .queue
        .active
        .read()
        .await
        .iter()
        .any(|task| task.uuid == running[1] && task.state == RecordingTaskState::Running));
    fixture.stop_all(4).await?;
    assert!(fixture.queue.finished.read().await.iter().all(|task| task.to_view(true).is_terminal()));
    assert!(fixture.queue.slot_waiters.snapshots().is_empty());
    Ok(())
}

#[tokio::test]
async fn live_timeout_keeps_the_transfer_cause_without_claiming_the_window_expired(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    std::fs::write(&script, "#!/bin/sh\necho 'Error opening input files: Operation timed out' >&2\nexit 1\n")?;
    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &NoopSink,
        &capacity,
        &script,
    )
    .await?;
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(task) = queue.finished.read().await.first().cloned() {
                break task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(settled.state, RecordingTaskState::Failed);
    let error = settled.error.as_deref().ok_or("missing transfer cause")?;
    assert!(error.contains("Error opening input files: Operation timed out"), "{error}");
    assert!(!error.contains("window has moved on"), "{error}");
    assert_eq!(settled.retry_attempts, 1);
    assert_eq!(settled.recording.reserved_bytes, 0);
    assert_eq!(stub.acquire_count(), 1);
    assert_eq!(stub.release_count(), 1);
    assert!(queue.active.read().await.is_empty());
    assert_eq!(queue.finished.read().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn idle_scheduler_does_not_snapshot_recording_history() -> Result<(), Box<dyn std::error::Error>> {
    let queue = Arc::new(RecordingQueue::new());
    let history = queue.finished.write().await;
    let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room();
    let config = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
    tokio::time::timeout(Duration::from_millis(200), async {
        queue.promote_due_scheduled_now().await;
        ensure_recording_worker_running(
            &app_config_with_listener(),
            &config,
            &queue,
            &NoopSink,
            &capacity,
            Path::new("unused"),
        )
        .await
    })
    .await??;
    drop(history);
    Ok(())
}

#[tokio::test]
async fn two_clips_of_different_media_never_receive_each_others_progress() {
    let queue = RecordingQueue::new();
    let mut clip_1 = scheduled_task(RecordingKind::Live, 1000, 1000);
    clip_1.uuid = "clip-1".to_string();
    clip_1.state = RecordingTaskState::Running;
    clip_1.media_identity = Some("media-1".to_string());
    clip_1.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1000,
        end_ts: 2000,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    let mut clip_2 = scheduled_task(RecordingKind::Live, 1000, 1000);
    clip_2.uuid = "clip-2".to_string();
    clip_2.state = RecordingTaskState::Running;
    clip_2.media_identity = Some("media-2".to_string());
    clip_2.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1000,
        end_ts: 2000,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    queue.active.write().await.push(clip_1);
    queue.active.write().await.push(clip_2);

    // Call update_active_download_for_worker with worker_uuid "worker-x"
    let updated = super::super::update_active_download_for_worker(&queue.active, "worker-x", |task| {
        task.size = 12345;
        true
    })
    .await;
    assert!(!updated, "unknown worker uuid must not update any clip");

    // Neither clip received progress
    let active = queue.active.read().await;
    let c1 = active.iter().find(|t| t.uuid == "clip-1").unwrap();
    assert_eq!(c1.size, 0);
    let c2 = active.iter().find(|t| t.uuid == "clip-2").unwrap();
    assert_eq!(c2.size, 0);
    drop(active);

    // Updating specifically clip-2 updates ONLY clip-2, not clip-1
    let updated2 = super::super::update_active_download_for_worker(&queue.active, "clip-2", |task| {
        task.size = 9999;
        true
    })
    .await;
    assert!(updated2);

    let active = queue.active.read().await;
    let c1 = active.iter().find(|t| t.uuid == "clip-1").unwrap();
    assert_eq!(c1.size, 0);
    let c2 = active.iter().find(|t| t.uuid == "clip-2").unwrap();
    assert_eq!(c2.size, 9999);
}

#[derive(Clone, Default)]
struct FinishedEventsSink(Arc<std::sync::Mutex<Vec<(String, shared::model::RecordingFinishedState)>>>);

impl shared::model::EventSink for FinishedEventsSink {
    fn emit(&self, event: shared::model::EventMessage) {
        if let shared::model::EventMessage::RecordingFinished { id, state, .. } = event {
            self.0.lock().unwrap().push((id, state));
        }
    }
}

#[tokio::test]
async fn pre_start_failure_window_closed_emits_recording_finished_failed_event(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp() - 200, 100);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    let stub = StubCapacity::full();
    let capacity: Arc<dyn RecordingCapacityPort> = stub;
    let events = FinishedEventsSink::default();
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &events,
        &capacity,
        &script,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while queue.finished.read().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let emitted = events.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1, "expected exactly one RecordingFinished event, got {emitted:?}");
    assert_eq!(emitted[0].1, shared::model::RecordingFinishedState::Failed);
    Ok(())
}

#[tokio::test]
async fn pre_start_failure_maintenance_emits_recording_finished_failed_event() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    let stub = StubCapacity::with_room();
    stub.set_maintenance("provider", true);
    let capacity: Arc<dyn RecordingCapacityPort> = stub;
    let events = FinishedEventsSink::default();
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &events,
        &capacity,
        &script,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while queue.finished.read().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let emitted = events.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1, "expected exactly one RecordingFinished event, got {emitted:?}");
    assert_eq!(emitted[0].1, shared::model::RecordingFinishedState::Failed);
    Ok(())
}

#[tokio::test]
async fn pre_start_failure_refused_before_start_emits_recording_finished_failed_event(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    capture.recording.reserved_bytes = 1_000_000;
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = stub;
    let events = FinishedEventsSink::default();
    let app_config = app_config_with_listener();
    let mut config = (*app_config.config.load_full()).clone();
    let rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        enabled: true,
        directory: Some(dir.path().display().to_string()),
        quota: Some(shared::model::RecordingQuotaConfigDto {
            default_private: Some(shared::model::ByteSize::new("10")),
            per_user: std::collections::HashMap::new(),
            shared: None,
        }),
        ..Default::default()
    });
    config.video = Some(tuliprox_core::model::VideoConfig {
        extensions: Vec::new(),
        web_search: None,
        recording: Some(rec_cfg.clone()),
    });
    app_config.config.store(Arc::new(config));
    ensure_recording_worker_running(&app_config, &rec_cfg, &queue, &events, &capacity, &script).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while queue.finished.read().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let emitted = events.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1, "expected exactly one RecordingFinished event, got {emitted:?}");
    assert_eq!(emitted[0].1, shared::model::RecordingFinishedState::Failed);
    Ok(())
}

#[tokio::test]
async fn execution_failure_emits_recording_finished_failed_event_exactly_once() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    std::fs::write(&script, "#!/bin/sh\nexit 1\n")?;
    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = stub;
    let events = FinishedEventsSink::default();
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &events,
        &capacity,
        &script,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while queue.finished.read().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let emitted = events.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1, "expected exactly one RecordingFinished event, got {emitted:?}");
    assert_eq!(emitted[0].1, shared::model::RecordingFinishedState::Failed);
    Ok(())
}

#[tokio::test]
async fn normal_completion_emits_recording_finished_completed_event_exactly_once(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
    let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir = dir.path().to_path_buf();
    capture.file_path = dir.path().join("capture.ts");
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await?;
    let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
    std::fs::write(
        &script,
        "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf recorded > \"$output\"\nexit 0\n",
    )?;
    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = stub;
    let events = FinishedEventsSink::default();
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &events,
        &capacity,
        &script,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while queue.finished.read().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let emitted = events.0.lock().unwrap().clone();
    assert_eq!(emitted.len(), 1, "expected exactly one RecordingFinished event, got {emitted:?}");
    assert_eq!(emitted[0].1, shared::model::RecordingFinishedState::Completed);
    Ok(())
}
