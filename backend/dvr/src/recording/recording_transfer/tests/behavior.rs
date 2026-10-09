use super::{
    acquire_result_after_wait, acquire_result_for_control, app_config_with_listener, bare_app_config,
    continue_after_pause, download_file, finalize_http_transfer, finish_active_and_promote, http_transfer_path,
    read_request, recording_deadline_instant, requeue_active_download_for_capacity_wait, scheduled_task,
    serve_range_fixture, slot_queue, start_recording_scheduler, wait_for_provider_slot, DownloadExecutionResult,
    ProviderAcquireResult, RecordingNotificationPlan,
};
#[cfg(unix)]
use super::{counting_ffmpeg, ensure_recording_worker_running};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::{RecordingControl, RecordingQueue, RecordingWaitOutcome},
};
use shared::model::{NoopSink, RecordingKind, RecordingTaskState};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::{Notify, RwLock};
use tuliprox_core::model::RecordingConfig;

#[tokio::test]
async fn vod_and_series_resume_at_the_saved_byte_offset() {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let dir = TempDir::new().expect("recording directory");
        let (url, server) = serve_range_fixture(false);
        let mut task = scheduled_task(kind, chrono::Utc::now().timestamp(), 900);
        task.file_dir = dir.path().to_path_buf();
        task.file_path = dir.path().join("recording.mp4");
        task.url = url;
        task.total_size = Some(10);
        let partial = http_transfer_path(&task);
        tokio::fs::write(&partial, b"0123").await.expect("saved partial");
        let active = Arc::new(RwLock::new(vec![task.clone()]));
        let result = download_file::<NoopSink>(
            active,
            task.clone(),
            &reqwest::Client::new(),
            None,
            Arc::new(RwLock::new(RecordingControl::None)),
            Arc::new(Notify::new()),
            None,
            None,
        )
        .await;
        assert!(matches!(result, DownloadExecutionResult::Completed));
        assert_eq!(tokio::fs::read(&task.file_path).await.expect("completed recording"), b"0123456789");
        assert!(!partial.exists());
        let request = server.join().expect("fixture server").to_ascii_lowercase();
        assert!(request.contains("range: bytes=4-\r\n"), "missing resume Range: {request}");
        assert!(request.contains("accept-encoding: identity\r\n"), "missing identity encoding: {request}");
    }
}

#[tokio::test]
async fn resume_during_pause_shutdown_keeps_a_worker_for_vod_and_series() {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let queue = RecordingQueue::new();
        *queue.active.write().await = vec![scheduled_task(kind, 0, 900)];
        *queue.worker("task").running.write().await = true;

        assert!(queue.pause_active("task").await.expect("pause"));
        assert!(queue.resume_active("task").await.expect("resume before old worker exits"));
        assert!(continue_after_pause(&queue, "task").await);
        assert!(queue.workers_running().await);
        assert_eq!(queue.active.read().await.first().map(|task| task.state), Some(RecordingTaskState::Running));

        assert!(queue.pause_active("task").await.expect("second pause"));
        assert!(!continue_after_pause(&queue, "task").await);
        assert!(queue.workers_running().await, "the claim lasts until worker exit");
        queue.release_worker("task").await;
        assert!(!queue.workers_running().await);
        assert!(queue.resume_active("task").await.expect("resume after old worker exits"));
        assert!(!queue.active.read().await.first().is_some_and(|task| task.paused));
    }
}

#[tokio::test]
async fn a_restart_requeue_consumes_the_restart_signal() {
    // A restart that survives its own requeue makes every new worker
    // preempt itself on start, spinning until the live window closes.
    let queue = RecordingQueue::new();
    *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
    *queue.worker("task").control_signal.write().await = RecordingControl::Restart;

    let requeued = requeue_active_download_for_capacity_wait(
        &queue,
        "task",
        "Reloading download service configuration",
        false,
        Some(RecordingControl::Restart),
    )
    .await
    .expect("requeue");

    assert!(requeued);
    assert_eq!(*queue.worker("task").control_signal.read().await, RecordingControl::None);
}

#[test]
fn a_slot_wait_maps_onto_the_acquisition_outcome() {
    // A wait that ends without an outcome ran into the live window's end;
    // being signalled means trying to acquire again.
    assert!(matches!(acquire_result_after_wait(None), Some(ProviderAcquireResult::WindowClosed)));
    assert!(acquire_result_after_wait(Some(RecordingWaitOutcome::Signalled)).is_none());
    assert!(matches!(
        acquire_result_after_wait(Some(RecordingWaitOutcome::Paused)),
        Some(ProviderAcquireResult::Paused)
    ));
    assert!(matches!(
        acquire_result_after_wait(Some(RecordingWaitOutcome::Cancelled)),
        Some(ProviderAcquireResult::Cancelled)
    ));
    assert!(matches!(
        acquire_result_after_wait(Some(RecordingWaitOutcome::Restarted)),
        Some(ProviderAcquireResult::Preempted)
    ));
    assert!(acquire_result_for_control(RecordingControl::None).is_none());
    assert!(matches!(acquire_result_for_control(RecordingControl::Cancel), Some(ProviderAcquireResult::Cancelled)));
    assert!(matches!(acquire_result_for_control(RecordingControl::Pause), Some(ProviderAcquireResult::Paused)));
    assert!(matches!(acquire_result_for_control(RecordingControl::Restart), Some(ProviderAcquireResult::Preempted)));
}

#[test]
fn a_live_window_that_has_already_closed_stops_immediately() {
    let now = chrono::Utc::now().timestamp();
    let long_over = scheduled_task(RecordingKind::Live, now - 7_200, 900);
    let deadline = recording_deadline_instant(&long_over).expect("still has a deadline");
    assert!(deadline <= tokio::time::Instant::now(), "a closed window must not keep recording");
}

#[test]
fn only_live_captures_are_bounded_by_a_window() {
    // A film does not stop being downloadable because a clock ran out.
    let now = chrono::Utc::now().timestamp();
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        assert!(recording_deadline_instant(&scheduled_task(kind, now, 900)).is_none());
    }
}

#[test]
fn resumable_kinds_stage_through_a_partial_and_live_writes_in_place() {
    // The strategy split: an HTTP transfer can be interrupted and resumed,
    // so it stages. ffmpeg owns its output file for the whole capture and
    // has nothing to resume into.
    let now = chrono::Utc::now().timestamp();
    let live = scheduled_task(RecordingKind::Live, now, 900);
    assert_eq!(http_transfer_path(&live), live.file_path, "a live capture writes straight to its file");

    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let task = scheduled_task(kind, now, 900);
        let staged = http_transfer_path(&task);
        assert_ne!(staged, task.file_path);
        assert!(
            staged.to_string_lossy().ends_with(".partial"),
            "a resumable transfer stages beside its final file, got {}",
            staged.display()
        );
    }
}

#[tokio::test]
async fn stopping_live_capture_clears_cancel_before_starting_a_due_recording() -> Result<(), Box<dyn std::error::Error>>
{
    for terminal in [RecordingTaskState::Completed, RecordingTaskState::Failed] {
        for has_next in [false, true] {
            let dir = TempDir::new()?;
            let queue = RecordingQueue::new_persistent(dir.path(), dir.path())?;
            let now = chrono::Utc::now().timestamp();
            let mut active = scheduled_task(RecordingKind::Live, now - 60, 900);
            active.uuid = "immediate".to_string();
            let mut scheduled = scheduled_task(RecordingKind::Live, now, 900);
            scheduled.uuid = "scheduled".to_string();
            scheduled.state = RecordingTaskState::Scheduled;
            let active = RecordingQueue::to_persisted(&active);
            let scheduled = RecordingQueue::to_persisted(&scheduled);
            crate::recording::recording_queue::mutate(&queue, |candidate| {
                candidate.active = vec![active];
                if has_next {
                    candidate.scheduled.push(scheduled);
                }
                Ok(())
            })
            .await?;
            assert_eq!(queue.promote_due_scheduled(now).await, usize::from(has_next));
            assert_eq!(queue.cancel_requested("immediate").await?, Some(false));
            assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::Cancel);

            let committed = finish_active_and_promote(&queue, "immediate", None, |task| {
                task.state = terminal;
                task.finished = true;
                task.error =
                    (terminal == RecordingTaskState::Failed).then(|| "Stopped recording has no data".to_string());
                task.recording.reserved_bytes = 0;
                RecordingNotificationPlan::empty()
            })
            .await?;
            assert!(committed.is_some());
            assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::None);
            assert_eq!(
                queue.active.read().await.first().map(|task| task.uuid.as_str()),
                has_next.then_some("scheduled")
            );
            let finished = queue.finished.read().await;
            assert_eq!(finished.len(), 1);
            assert_eq!(finished[0].state, terminal);
            assert!(finished[0].to_view(true).is_terminal());
            drop(finished);

            let restored = RecordingQueue::new_persistent(dir.path(), dir.path())?;
            restored.load_from_disk().await?;
            assert_eq!(restored.finished.read().await[0].state, terminal);
        }
    }
    Ok(())
}

#[tokio::test]
async fn finalizing_publishes_the_staged_bytes_and_clears_the_partial() {
    let dir = TempDir::new().expect("tempdir");
    let partial = dir.path().join("film.mp4.partial");
    let final_path = dir.path().join("film.mp4");
    std::fs::write(&partial, b"complete recording").expect("write");

    finalize_http_transfer(&final_path, &partial).await.expect("finalize");

    assert!(final_path.exists());
    assert!(!partial.exists(), "the partial is cleared once published");
}

#[tokio::test]
async fn finalizing_twice_succeeds_instead_of_failing_a_complete_recording() {
    // A crash between the link and the partial's removal leaves both files.
    // The resumed transfer sees a complete file, finalizes again, and used
    // to get AlreadyExists back -- reporting a recording whose bytes are
    // entirely on disk as failed, on every retry.
    let dir = TempDir::new().expect("tempdir");
    let partial = dir.path().join("film.mp4.partial");
    let final_path = dir.path().join("film.mp4");
    std::fs::write(&partial, b"complete recording").expect("write");
    std::fs::hard_link(&partial, &final_path).expect("simulate the interrupted finalize");

    finalize_http_transfer(&final_path, &partial).await.expect("finalizing again must succeed");

    assert!(final_path.exists());
    assert!(!partial.exists(), "the second attempt finishes the job the first did not");
}

#[tokio::test]
async fn a_different_file_at_the_final_path_is_not_published_over() {
    // Path reservation should make this unreachable. If it ever happens the
    // staged bytes were never verified against what is already there, so
    // the transfer must not claim success.
    let dir = TempDir::new().expect("tempdir");
    let partial = dir.path().join("film.mp4.partial");
    let final_path = dir.path().join("film.mp4");
    std::fs::write(&partial, b"the recording we just made").expect("write");
    std::fs::write(&final_path, b"something else entirely, of another size").expect("write");

    let error = finalize_http_transfer(&final_path, &partial).await.expect_err("must refuse");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(partial.exists(), "the staged bytes are kept for inspection");
}

#[tokio::test]
async fn a_transfer_written_straight_to_its_final_path_needs_no_finalization() {
    let dir = TempDir::new().expect("tempdir");
    let final_path = dir.path().join("stream.ts");
    std::fs::write(&final_path, b"live capture").expect("write");

    finalize_http_transfer(&final_path, &final_path).await.expect("finalize");

    assert!(final_path.exists());
}

#[tokio::test(start_paused = true)]
async fn a_transfer_waits_for_a_slot_however_long_it_takes() {
    // The counterpart, and the reason the bound is not applied to
    // everything: a file on a server is still there in an hour.
    let dir = TempDir::new().expect("tempdir");
    let queue = slot_queue(&dir);
    let control = RwLock::new(RecordingControl::None);
    let notify = Notify::new();

    let waiters = Arc::clone(&queue.slot_waiters);
    let freed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_mins(60)).await;
        let queued = waiters.snapshots();
        let waiter = queued.first().expect("someone is waiting an hour later");
        waiters.signal_waiter(waiter.id)
    });

    let outcome = wait_for_provider_slot(&queue, &Arc::from("provider"), 0, &control, &notify, None).await;

    assert!(freed.await.expect("signaller"), "the waiter was still there to signal");
    assert!(matches!(outcome, Some(RecordingWaitOutcome::Signalled)), "it took the slot when one appeared");
}

#[cfg(unix)]
#[tokio::test]
async fn scheduler_starts_a_due_live_capture_while_another_capture_is_running() -> Result<(), Box<dyn std::error::Error>>
{
    for has_data in [false, true] {
        let dir = TempDir::new()?;
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
        let now = chrono::Utc::now().timestamp();
        let mut immediate = scheduled_task(RecordingKind::Live, now - 60, 900);
        immediate.uuid = "immediate".to_string();
        immediate.state = RecordingTaskState::Queued;
        immediate.input_name = Some(Arc::from("provider"));
        immediate.recording.source.virtual_id = "42".to_string();
        immediate.file_dir = dir.path().to_path_buf();
        immediate.file_path = dir.path().join("immediate.ts");
        let mut scheduled = immediate.clone();
        scheduled.uuid = "scheduled".to_string();
        scheduled.state = RecordingTaskState::Scheduled;
        scheduled.file_path = dir.path().join("scheduled.ts");
        scheduled.recording.program_start = Some(now);
        scheduled.recording.scheduled_start = Some(now);
        let immediate = RecordingQueue::to_persisted(&immediate);
        let scheduled = RecordingQueue::to_persisted(&scheduled);
        crate::recording::recording_queue::mutate(&queue, |candidate| {
            candidate.queue.push(immediate);
            candidate.scheduled.push(scheduled);
            Ok(())
        })
        .await?;
        let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
        let write_initial = if has_data { "printf recorded > \"$output\"" } else { ":" };
        std::fs::write(&script, format!(
            "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\ncase \"$output\" in\n*immediate.ts.partial) {write_initial}; touch \"$output.ready\"; read -r control ;;\n*) printf recorded > \"$output\" ;;\nesac\nexit 0\n"
        ))?;
        let stub = StubCapacity::with_room();
        let capacity: Arc<dyn RecordingCapacityPort> = stub.clone();
        let config = RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
        ensure_recording_worker_running(&app_config_with_listener(), &config, &queue, &NoopSink, &capacity, &script)
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("immediate.ts.partial.ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        start_recording_scheduler(
            Arc::new(app_config_with_listener()),
            config.clone(),
            &queue,
            NoopSink,
            capacity.clone(),
            cancel.clone(),
            script.clone(),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !queue.finished.read().await.iter().any(|task| task.uuid == "scheduled") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert!(queue
            .active
            .read()
            .await
            .iter()
            .any(|task| task.uuid == "immediate" && task.state == RecordingTaskState::Running));
        assert!(queue.finished.read().await.iter().all(|task| task.uuid != "immediate"));
        queue.cancel_requested("immediate").await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while queue.finished.read().await.len() != 2 || queue.workers_running().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        cancel.cancel();
        assert_eq!(stub.acquire_count(), 2);
        let finished = queue.finished.read().await;
        let stopped = finished.iter().find(|task| task.uuid == "immediate").ok_or("missing stopped task")?;
        let next = finished.iter().find(|task| task.uuid == "scheduled").ok_or("missing scheduled task")?;
        assert_eq!(stopped.state, if has_data { RecordingTaskState::Completed } else { RecordingTaskState::Failed });
        assert_eq!(next.state, RecordingTaskState::Completed, "{:?}", next.error);
        assert_eq!(tokio::fs::read(&next.file_path).await?, b"recorded");
        assert!(stopped.to_view(true).is_terminal());
        assert!(next.to_view(true).is_terminal());
        assert!(queue.active.read().await.is_empty());
        assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::None);
        assert_eq!(stub.release_count(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn scheduler_starts_a_queued_transfer_after_worker_exit() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let mut transfer = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 300);
    transfer.state = RecordingTaskState::Queued;
    transfer.input_name = Some(Arc::from("provider"));
    let persisted = RecordingQueue::to_persisted(&transfer);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await
    .expect("seed queue");

    let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::full();
    let cancel = tokio_util::sync::CancellationToken::new();
    start_recording_scheduler(
        Arc::new(bare_app_config()),
        RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        NoopSink,
        capacity,
        cancel.clone(),
        Path::new(crate::recording::recording_worker::FFMPEG_BINARY).to_path_buf(),
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if queue
                .active
                .read()
                .await
                .first()
                .is_some_and(|task| task.state == RecordingTaskState::WaitingForCapacity)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scheduler did not restart the stranded transfer");
    cancel.cancel();
}

#[tokio::test]
async fn serial_transfers_start_the_next_worker_without_waiting_for_a_tick() -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;
    let dir = TempDir::new()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let app = Arc::new(app_config_with_listener());
    let mut config = (*app.config.load_full()).clone();
    config.api.port = listener.local_addr()?.port();
    app.config.store(Arc::new(config));
    let queue = Arc::new(RecordingQueue::new());
    for index in 0..2 {
        let mut task = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 300);
        task.uuid = format!("transfer-{index}");
        task.state = RecordingTaskState::Queued;
        task.recording.source.virtual_id = (42 + index).to_string();
        task.file_dir = dir.path().to_path_buf();
        task.file_path = dir.path().join(format!("transfer-{index}.mp4"));
        queue.queue.lock().await.push_back(task);
    }
    let (first_seen, first_received) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let (second_seen, second_received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        for (seen, gate) in [(first_seen, Some(released)), (second_seen, None)] {
            let (mut socket, _) = listener.accept().await?;
            read_request(&mut socket).await?;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n").await?;
            let _ = seen.send(());
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            socket.write_all(b"0123456789").await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room();
    start_recording_scheduler(
        app,
        RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
        &queue,
        NoopSink,
        capacity,
        cancellation.clone(),
        PathBuf::from("unused"),
    );
    let result = async {
        tokio::time::timeout(Duration::from_secs(5), first_received).await??;
        assert_eq!(queue.active.read().await.len(), 1);
        assert_eq!(queue.queue.lock().await.len(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).map_err(|()| "fixture stopped")?;
        tokio::time::timeout(Duration::from_millis(500), second_received).await??;
        tokio::time::timeout(Duration::from_secs(5), async {
            while queue.finished.read().await.len() != 2 || queue.workers_running().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert!(queue.finished.read().await.iter().all(|task| task.state == RecordingTaskState::Completed));
        server.await??;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    cancellation.cancel();
    result
}
