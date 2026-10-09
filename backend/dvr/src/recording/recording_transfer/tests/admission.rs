#[cfg(unix)]
use super::ConcurrentLiveFixture;
use super::{
    app_config_with_listener, bare_app_config, counting_ffmpeg, download_file, ensure_recording_worker_running,
    finish_active_and_promote, requeue_active_download_for_capacity_wait, scheduled_task, serve_range_fixture,
    slot_queue, spawn_count, vod_entry, wait_for_provider_slot, DownloadExecutionResult, QuotaGate,
    RecordingNotificationPlan, DOWNLOAD_PREEMPTED_REASON, LIVE_CAPACITY_WINDOW_CLOSED, QUOTA_EXCEEDED_DURING_TRANSFER,
};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::{PersistedRecordingTask, RecordingControl, RecordingQueue, RecordingWaitOutcome},
};
use shared::model::{
    NoopSink, RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingTaskState,
    RecordingVisibility, UserId,
};
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::{path::Path, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::sync::{Notify, RwLock};
use tuliprox_core::model::RecordingConfig;

#[tokio::test]
async fn a_preemption_requeue_leaves_a_pending_control_untouched() {
    let queue = RecordingQueue::new();
    *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
    *queue.worker("task").control_signal.write().await = RecordingControl::Pause;

    requeue_active_download_for_capacity_wait(&queue, "task", DOWNLOAD_PREEMPTED_REASON, false, None)
        .await
        .expect("requeue");

    assert_eq!(*queue.worker("task").control_signal.read().await, RecordingControl::Pause);
}

/// Recording enabled under `dir` with a private quota of `quota` bytes.
pub(in crate::recording::recording_transfer::tests) fn app_config_with_quota(
    dir: &Path,
    quota: u64,
) -> tuliprox_core::model::AppConfig {
    let app_config = bare_app_config();
    let mut rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
    rec_cfg.directory = dir.to_string_lossy().into_owned();
    rec_cfg.quota =
        Some(tuliprox_core::model::RecordingQuotaConfig { default_private_bytes: Some(quota), ..Default::default() });
    app_config.config.store(Arc::new(tuliprox_core::model::Config {
        video: Some(tuliprox_core::model::VideoConfig {
            extensions: Vec::new(),
            web_search: None,
            recording: Some(rec_cfg),
        }),
        ..tuliprox_core::model::Config::default()
    }));
    app_config
}

pub(in crate::recording::recording_transfer::tests) async fn queue_with(
    dir: &TempDir,
    active: PersistedRecordingTask,
    queued: Vec<PersistedRecordingTask>,
) -> RecordingQueue {
    let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository");
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.active = vec![active.clone()];
        candidate.queue.clone_from(&queued);
        Ok(())
    })
    .await
    .expect("seed");
    queue
}

#[tokio::test]
async fn a_transfer_larger_than_the_quota_left_is_refused_once_its_size_is_known() {
    // Admitted at zero because the size was unknown; the first response
    // says 4096 bytes against a 16 byte quota.
    let dir = TempDir::new().expect("tempdir");
    let queue = queue_with(&dir, vod_entry("alice", "web:alice", RecordingTaskState::Running), Vec::new()).await;
    let app_config = app_config_with_quota(dir.path(), 16);
    let gate = QuotaGate { queue: &queue, app_config: &app_config };

    let refused = gate.admit_size("alice", Some(4096), 0).await.err();
    assert_eq!(refused.as_deref(), Some(QUOTA_EXCEEDED_DURING_TRANSFER));

    let admitted = gate.admit_size("alice", Some(16), 0).await.expect("fits");
    assert_eq!(admitted.byte_cap, None);
    let reserved = queue.active.read().await.first().map(|active| active.recording.reserved_bytes);
    assert_eq!(reserved, Some(16), "the known size is now reserved");
}

#[tokio::test]
async fn a_download_over_the_quota_stops_before_writing_its_file() {
    let dir = TempDir::new().expect("tempdir");
    let (url, server) = serve_range_fixture(true);
    let mut task = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 900);
    task.file_dir = dir.path().to_path_buf();
    task.file_path = dir.path().join("film.mp4");
    task.url = url;
    task.recording = RecordingMetadata::new_media(
        RecordingOwner::User(UserId::from("web:alice")),
        RecordingVisibility::Private,
        RecordingSource::new("t1", "v1", "in1"),
        "Film".to_string(),
    );
    let queue = queue_with(&dir, RecordingQueue::to_persisted(&task), Vec::new()).await;
    let app_config = app_config_with_quota(dir.path(), 4);

    let result = download_file::<NoopSink>(
        Arc::clone(&queue.active),
        task.clone(),
        &reqwest::Client::new(),
        None,
        Arc::new(RwLock::new(RecordingControl::None)),
        Arc::new(Notify::new()),
        None,
        Some(QuotaGate { queue: &queue, app_config: &app_config }),
    )
    .await;
    let _ = server.join();

    assert!(
        matches!(&result, DownloadExecutionResult::Failed(reason) if reason == QUOTA_EXCEEDED_DURING_TRANSFER),
        "a 10 byte file against a 4 byte quota"
    );
    assert!(!task.file_path.exists(), "nothing was published");
}

#[tokio::test]
async fn a_file_of_unknown_size_is_checked_against_waiting_quotas_when_it_completes() {
    // Without a Content-Length nothing can be charged when the transfer
    // starts. Alice has room; Bob, waiting to attach, has 16 bytes. The
    // finished size is the first moment his quota can be asked.
    let dir = TempDir::new().expect("tempdir");
    let queue = queue_with(
        &dir,
        vod_entry("alice", "web:alice", RecordingTaskState::Running),
        vec![vod_entry("bob", "web:bob", RecordingTaskState::Queued)],
    )
    .await;
    let app_config = app_config_with_quota(dir.path(), 1 << 20);
    let gate = QuotaGate { queue: &queue, app_config: &app_config };
    let started = gate.admit_size("alice", None, 0).await.expect("alice starts");
    assert!(!started.siblings_refused, "nothing is known about the size yet");

    let limits = crate::recording::recording_quota::QuotaLimits {
        default_private_bytes: Some(16),
        per_user_bytes: [(UserId::from("web:alice"), 1 << 20)].into_iter().collect(),
        shared_bytes: None,
    };
    finish_active_and_promote(&queue, "alice", Some(&limits), |done| {
        done.finished = true;
        done.state = RecordingTaskState::Completed;
        done.size = 4096;
        done.recording.measured_bytes = 4096;
        done.recording.reserved_bytes = 0;
        RecordingNotificationPlan::empty()
    })
    .await
    .expect("commit");

    let finished = queue.finished.read().await.clone();
    let state_of = |uuid: &str| finished.iter().find(|task| task.uuid == uuid).map(|task| task.state);
    assert_eq!(state_of("alice"), Some(RecordingTaskState::Completed));
    assert_eq!(state_of("bob"), Some(RecordingTaskState::Failed), "not attached past his quota");
    assert!(queue.queue.lock().await.is_empty());
}

#[tokio::test]
async fn a_transfer_of_unknown_size_is_capped_at_the_quota_left() {
    let dir = TempDir::new().expect("tempdir");
    let queue = queue_with(&dir, vod_entry("alice", "web:alice", RecordingTaskState::Running), Vec::new()).await;
    let app_config = app_config_with_quota(dir.path(), 1000);
    let gate = QuotaGate { queue: &queue, app_config: &app_config };

    let admitted = gate.admit_size("alice", None, 0).await.expect("admitted");
    assert_eq!(admitted.byte_cap, Some(1000));
}

#[tokio::test]
async fn an_entry_waiting_to_attach_is_refused_for_its_own_quota() {
    // Bob queued behind Alice's transfer while its size was unknown.
    // Attaching later would charge him the whole file, so his quota has
    // to be asked now.
    let dir = TempDir::new().expect("tempdir");
    let queue = queue_with(
        &dir,
        vod_entry("alice", "web:alice", RecordingTaskState::Running),
        vec![vod_entry("bob", "web:bob", RecordingTaskState::Queued)],
    )
    .await;
    let app_config = app_config_with_quota(dir.path(), 4096);
    // Bob already holds something else that leaves him less than the file.
    crate::recording::recording_queue::mutate(&queue, |candidate| {
        let mut other = vod_entry("bob-other", "web:bob", RecordingTaskState::Completed);
        other.media_identity = "other".to_string();
        other.recording.measured_bytes = 1000;
        other.finished = true;
        candidate.finished.push(other);
        Ok(())
    })
    .await
    .expect("seed");
    let gate = QuotaGate { queue: &queue, app_config: &app_config };

    let admitted = gate.admit_size("alice", Some(4096), 0).await.expect("alice fits");
    assert!(admitted.siblings_refused);
    assert!(queue.queue.lock().await.is_empty(), "bob no longer waits");
    let bob = queue.finished.read().await.iter().find(|task| task.uuid == "bob").cloned().expect("bob is filed");
    assert_eq!(bob.state, RecordingTaskState::Failed);
    assert_eq!(bob.recording.reserved_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn a_live_capture_stops_waiting_for_capacity_when_its_window_closes() {
    // Waiting past the padded end cannot produce the recording that was
    // asked for -- the programme has finished. Before this the wait had no
    // bound at all, so a live capture with no free slot sat in the queue
    // through its own broadcast and out the other side.
    let dir = TempDir::new().expect("tempdir");
    let queue = slot_queue(&dir);
    let control = RwLock::new(RecordingControl::None);
    let notify = Notify::new();
    let window_ends = tokio::time::Instant::now() + Duration::from_mins(30);

    let outcome = wait_for_provider_slot(&queue, &Arc::from("provider"), 0, &control, &notify, Some(window_ends)).await;

    assert!(outcome.is_none(), "the wait ends when the programme does");
    assert!(queue.slot_waiters.snapshots().is_empty(), "expired waits must deregister");
    assert!(tokio::time::Instant::now() >= window_ends, "and only then");
}

#[tokio::test(start_paused = true)]
async fn a_capacity_wait_still_answers_a_cancellation_before_its_deadline() {
    // The deadline must not swallow the control signals: a user cancelling
    // a live capture that is waiting for capacity should not have to wait
    // out the programme.
    let dir = TempDir::new().expect("tempdir");
    let queue = slot_queue(&dir);
    let control = RwLock::new(RecordingControl::None);
    let notify = Notify::new();
    let window_ends = tokio::time::Instant::now() + Duration::from_mins(30);

    let provider: Arc<str> = Arc::from("provider");
    let waiting = wait_for_provider_slot(&queue, &provider, 0, &control, &notify, Some(window_ends));
    let cancelling = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        *control.write().await = RecordingControl::Cancel;
        notify.notify_waiters();
    };
    let (outcome, ()) = tokio::join!(waiting, cancelling);

    assert!(matches!(outcome, Some(RecordingWaitOutcome::Cancelled)), "the cancel is answered, not the deadline");
    assert!(tokio::time::Instant::now() < window_ends, "and long before the window closes");
}

#[cfg(unix)]
#[tokio::test]
async fn independent_live_workers_fill_provider_capacity_and_stop_only_the_selected_recording(
) -> Result<(), Box<dyn std::error::Error>> {
    for limit in [0, 4] {
        let fixture = ConcurrentLiveFixture::new(4, limit).await?;
        let (first, second) = tokio::join!(fixture.start(), fixture.start());
        first?;
        second?;
        fixture.wait_for_running(4).await?;
        assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 4);
        fixture.queue.cancel_requested("live-2").await?;
        let remaining = fixture.wait_for_running(3).await?;
        assert!(!remaining.iter().any(|uuid| uuid == "live-2"));
        assert!(fixture
            .queue
            .active
            .read()
            .await
            .iter()
            .filter(|task| task.uuid != "live-2")
            .all(|task| task.state == RecordingTaskState::Running));
        fixture.stop_all(4).await?;
        assert_eq!(
            fixture.capacity.releases.load(Ordering::SeqCst),
            4,
            "one allocation per recording despite concurrent starts"
        );
        assert!(fixture.queue.finished.read().await.iter().all(|task| task.state == RecordingTaskState::Completed));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_live_capture_that_never_gets_capacity_fails_at_its_window_without_recording() {
    // The whole path, driven against a provider that is permanently full:
    // the worker asks for a slot, is refused, waits, and gives up when the
    // programme it was going to record has finished. Until the capacity
    // port existed this could only be produced by a real provider actually
    // being busy for the length of a broadcast.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let now = chrono::Utc::now().timestamp();
    let mut capture = scheduled_task(RecordingKind::Live, now, 1_800);
    capture.uuid = "live".to_string();
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    let capture = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(capture.clone());
        Ok(())
    })
    .await
    .expect("seed");

    let stub = StubCapacity::full();
    let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
    let app_config = bare_app_config();
    ensure_recording_worker_running(
        &app_config,
        &RecordingConfig::from(&shared::model::RecordingConfigDto {
            enabled: true,
            ..shared::model::RecordingConfigDto::default()
        }),
        &queue,
        &NoopSink,
        &capacity,
        Path::new(crate::recording::recording_worker::FFMPEG_BINARY),
    )
    .await
    .expect("worker started");

    // Sleeping rather than spinning: with a paused clock the runtime only
    // advances time when nothing is runnable, so a busy wait would stop the
    // window from ever closing.
    tokio::time::sleep(Duration::from_mins(31)).await;

    let finished = queue.finished.read().await;
    let settled = finished.first().expect("the capture reached a terminal state");
    assert_eq!(settled.state, RecordingTaskState::Failed, "no capacity ever came");
    assert_eq!(settled.error.as_deref(), Some(LIVE_CAPACITY_WINDOW_CLOSED), "and it says why");
    assert_eq!(settled.size, 0, "nothing was ever recorded");
    assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
    assert_eq!(
        stub.acquire_count(),
        1,
        "it asked for a slot once and then waited, rather than spinning on the provider"
    );
    assert_eq!(stub.release_count(), 0, "and it never held one to give back");
}

#[tokio::test]
async fn a_live_capture_asks_for_capacity_once_runs_the_encoder_once_and_completes_once() {
    // The other half of the window contract. A real clock here on
    // purpose: the worker spawns an actual child process, and pausing
    // time would let unrelated timers fire between the spawn and the
    // exit while the process itself still ran in wall-clock.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let recordings_dir = dir.path().join("recordings");
    std::fs::create_dir_all(&recordings_dir).expect("create recording dir");
    let log = dir.path().join("spawns.log");
    let script = counting_ffmpeg(dir.path(), &log);

    let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
    let now = chrono::Utc::now().timestamp();
    let mut capture = scheduled_task(RecordingKind::Live, now, 1_800);
    capture.uuid = "live".to_string();
    capture.state = RecordingTaskState::Queued;
    capture.input_name = Some(Arc::from("provider"));
    // The execution URL is derived from the source, so the virtual id has
    // to be a real one rather than the placeholder the other fixtures use.
    capture.recording.source.virtual_id = "42".to_string();
    capture.file_dir.clone_from(&recordings_dir);
    capture.file_path = recordings_dir.join("capture.ts");
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await
    .expect("seed");

    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
    ensure_recording_worker_running(
        &app_config_with_listener(),
        &RecordingConfig::from(&shared::model::RecordingConfigDto {
            enabled: true,
            ..shared::model::RecordingConfigDto::default()
        }),
        &queue,
        &NoopSink,
        &capacity,
        &script,
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
    .await;
    // A bare timeout here says nothing about why. Where the capture got
    // stuck is the whole diagnosis, so report it.
    let Ok(settled) = settled else {
        panic!(
            "never settled: active={:?} queued={} spawns={} acquires={}",
            queue.active.read().await.first().map(|active| (active.uuid.clone(), active.state, active.error.clone())),
            queue.queue.lock().await.len(),
            spawn_count(&log),
            stub.acquire_count(),
        );
    };

    assert_eq!(settled.state, RecordingTaskState::Completed, "{:?}", settled.error);
    assert_eq!(spawn_count(&log), 1, "the encoder ran once, not once per retry or per promotion pass");
    assert_eq!(stub.acquire_count(), 1, "and one slot was asked for");
    assert_eq!(stub.release_count(), 1, "and given back exactly once");
    assert_eq!(queue.finished.read().await.len(), 1, "the recording was committed once");
    assert_eq!(
        tokio::fs::read(&capture.file_path).await.expect("the recording is where playback will look"),
        b"recorded"
    );
    assert!(!crate::recording_partial_path(&capture.file_path).exists(), "and nothing was left staged");
    assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
}
