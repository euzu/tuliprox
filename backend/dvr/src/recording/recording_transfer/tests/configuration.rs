use super::{
    app_config_with_listener, ensure_recording_worker_running, scheduled_task, serve_range_fixture,
    ConcurrentLiveFixture,
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_starts_respect_the_configured_background_limit() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = ConcurrentLiveFixture::with_background_limit(3, 3, 1).await?;
    fixture.start().await?;
    let running = fixture.wait_for_running(1).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.queue.slot_waiters.snapshots().len() != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 1);
    fixture.queue.cancel_requested(&running[0]).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let now_running = fixture.wait_for_running(1).await?;
            if now_running != running {
                break Ok::<_, tokio::time::error::Elapsed>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 1);
    fixture.stop_all(3).await?;
    Ok(())
}

#[tokio::test]
async fn vod_and_series_transfers_reach_the_local_listener_past_a_configured_proxy() {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (fixture_url, server) = serve_range_fixture(true);
        let app_config = app_config_with_listener();
        let mut config = (*app_config.config.load_full()).clone();
        config.api.port = fixture_url.port().expect("fixture port");
        // Nothing listens here: a transfer routed through the proxy fails.
        config.proxy = Some(tuliprox_core::model::ProxyConfig {
            url: "http://127.0.0.1:9".to_string(),
            username: None,
            password: None,
        });
        app_config.config.store(Arc::new(config));

        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let mut transfer = scheduled_task(kind, chrono::Utc::now().timestamp(), 300);
        transfer.state = RecordingTaskState::Queued;
        transfer.input_name = Some(Arc::from("provider"));
        transfer.recording.source.virtual_id = "42".to_string();
        transfer.file_dir = dir.path().to_path_buf();
        transfer.file_path = dir.path().join("transfer.mp4");
        let persisted = RecordingQueue::to_persisted(&transfer);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(persisted.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room() as Arc<dyn RecordingCapacityPort>;
        ensure_recording_worker_running(
            &app_config,
            &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
            &queue,
            &NoopSink,
            &capacity,
            Path::new(crate::recording::recording_worker::FFMPEG_BINARY),
        )
        .await
        .expect("worker started");

        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(done) = queue.finished.read().await.first().cloned() {
                    break done;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("transfer settled");
        assert_eq!(settled.state, RecordingTaskState::Completed, "{kind:?}: {:?}", settled.error);
        let request = server.join().expect("fixture server");
        assert!(request.starts_with("GET /api/v1/playlist/recording/"), "{request}");
    }
}
