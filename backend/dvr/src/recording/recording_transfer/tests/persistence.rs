use super::{
    app_config_with_listener, ensure_recording_worker_running, recording_deadline_instant, scheduled_task, vod_entry,
};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::RecordingQueue,
};
use shared::model::{NoopSink, RecordingKind, RecordingTaskState};
use std::{path::Path, sync::Arc, time::Duration};
use tuliprox_core::model::RecordingConfig;

#[tokio::test]
async fn starting_workers_commits_entries_attached_to_an_already_completed_file(
) -> Result<(), Box<dyn std::error::Error>> {
    let queue = Arc::new(RecordingQueue::new());
    let mut done = vod_entry("done", "alice", RecordingTaskState::Completed);
    done.finished = true;
    done.size = 123;
    done.recording.measured_bytes = 123;
    let waiting = vod_entry("waiting", "bob", RecordingTaskState::Queued);
    crate::recording::recording_queue::mutate(&queue, |candidate| {
        candidate.finished.push(done);
        candidate.queue.push(waiting);
        Ok(())
    })
    .await?;
    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = stub.clone();
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        &NoopSink,
        &capacity,
        Path::new("unused-encoder"),
    )
    .await?;
    assert!(queue.queue.lock().await.is_empty());
    assert!(queue.active.read().await.is_empty());
    assert_eq!(queue.finished.read().await.len(), 2);
    let finished = queue.finished.read().await;
    let attached = finished.iter().find(|task| task.uuid == "waiting").ok_or("missing attached entry")?;
    assert_eq!(attached.state, RecordingTaskState::Completed);
    assert_eq!(attached.size, 123);
    assert_eq!(stub.acquire_count(), 0);
    Ok(())
}

#[test]
fn a_live_window_is_measured_in_wall_clock_not_in_time_spent_recording() {
    // A capture that was preempted or waiting for capacity does not get
    // that time back: the broadcast ran regardless. The deadline is
    // anchored to the programme, so an interruption cannot push a capture
    // past the window and into the next programme.
    let now = chrono::Utc::now().timestamp();
    let started_ten_minutes_ago = scheduled_task(RecordingKind::Live, now - 600, 900);
    let remaining = recording_deadline_instant(&started_ten_minutes_ago)
        .expect("a scheduled live capture has a deadline")
        .saturating_duration_since(tokio::time::Instant::now())
        .as_secs();
    // 900s window, 600s already elapsed: about 300 left, never a fresh 900.
    assert!((295..=305).contains(&remaining), "expected roughly 300s left, got {remaining}");
}

#[tokio::test]
async fn blocked_serial_queue_does_not_clone_partitions_on_each_commit() -> Result<(), Box<dyn std::error::Error>> {
    let queue = RecordingQueue::new();
    queue.active.write().await.push(scheduled_task(RecordingKind::Vod, 0, 300));
    for index in 0..49 {
        let mut task = scheduled_task(RecordingKind::Vod, 0, 300);
        task.uuid = format!("waiting-{index}");
        task.recording.source.virtual_id = index.to_string();
        task.state = RecordingTaskState::Queued;
        queue.queue.lock().await.push_back(task);
    }
    for index in 0..100 {
        let mut done = scheduled_task(RecordingKind::Vod, 0, 300);
        done.uuid = format!("completed-{index}");
        done.recording.source.virtual_id = (100 + index).to_string();
        done.state = RecordingTaskState::Completed;
        queue.finished.write().await.push(done);
    }
    for size in 0..3 {
        crate::recording::recording_queue::mutate(&queue, |candidate| {
            candidate.active[0].size = size;
            Ok(())
        })
        .await?;
        // Promotion has no reason to read this partition; a full snapshot does.
        let scheduled = queue.scheduled.write().await;
        assert!(
            !tokio::time::timeout(Duration::from_millis(200), super::super::promote_ready_downloads(&queue)).await??
        );
        drop(scheduled);
    }
    assert_eq!(queue.queue.lock().await.len(), 49);
    Ok(())
}
