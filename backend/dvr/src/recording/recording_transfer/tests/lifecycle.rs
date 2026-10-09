use super::{
    app_config_with_listener, counting_ffmpeg, ensure_recording_worker_running, scheduled_task, ConcurrentLiveFixture,
};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::RecordingQueue,
};
use shared::model::{NoopSink, RecordingKind, RecordingTaskState};
use std::{
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
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
