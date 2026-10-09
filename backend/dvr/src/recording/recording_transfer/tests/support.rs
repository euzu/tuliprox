#[cfg(unix)]
use super::{ensure_recording_worker_running, start_recording_scheduler};
#[cfg(unix)]
use crate::recording::recording_capacity::RecordingCapacityPort;
use crate::recording::recording_queue::{PersistedRecordingTask, RecordingPartition, RecordingQueue, RecordingTask};
#[cfg(unix)]
use shared::model::NoopSink;
use shared::model::{
    RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingTaskState, RecordingVisibility, UserId,
};
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
#[cfg(unix)]
use tokio::sync::Notify;
#[cfg(unix)]
use tuliprox_core::model::RecordingConfig;

/// A task whose Live window runs from `program_start` for `duration_secs`.
pub(in crate::recording::recording_transfer::tests) fn scheduled_task(
    kind: RecordingKind,
    program_start: i64,
    duration_secs: i64,
) -> RecordingTask {
    let meta = RecordingMetadata::new_live(
        RecordingOwner::User(UserId::from("web:alice")),
        RecordingVisibility::Private,
        RecordingSource::new("t1", "v1", "in1"),
        program_start,
        program_start + duration_secs,
        0,
        0,
    );
    RecordingQueue::from_persisted(PersistedRecordingTask {
        media_identity: String::new(),
        partition: RecordingPartition::default(),
        uuid: "task".to_string(),
        kind,
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from("/tmp/capture.ts"),
        filename: "capture.ts".to_string(),
        url: "https://example.com/stream".to_string(),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state: RecordingTaskState::Running,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
    })
    .expect("valid fixture")
}

pub(in crate::recording::recording_transfer::tests) fn serve_range_fixture(
    ignore_range: bool,
) -> (reqwest::Url, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let url = reqwest::Url::parse(&format!("http://{}/download", listener.local_addr().expect("server address")))
        .expect("fixture URL");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept download request");
        socket.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).expect("read download request");
            assert!(read > 0, "request ended before headers");
            request.extend_from_slice(&chunk[..read]);
        }
        let request = String::from_utf8(request).expect("request headers");
        let (status, headers, body) = if ignore_range {
            ("200 OK", "", b"0123456789".as_slice())
        } else {
            ("206 Partial Content", "Content-Range: bytes 4-9/10\r\n", b"456789".as_slice())
        };
        let response =
            format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        socket.write_all(response.as_bytes()).expect("write response headers");
        socket.write_all(body).expect("write response body");
        request
    });
    (url, server)
}

/// One VOD on one media for `owner`, in the given partition and state.
pub(in crate::recording::recording_transfer::tests) fn vod_entry(
    uuid: &str,
    owner: &str,
    state: RecordingTaskState,
) -> PersistedRecordingTask {
    let mut task = scheduled_task(RecordingKind::Vod, 0, 0);
    task.uuid = uuid.to_string();
    task.state = state;
    task.recording = RecordingMetadata::new_media(
        RecordingOwner::User(UserId::from(owner)),
        RecordingVisibility::Private,
        RecordingSource::new("t1", "v1", "in1"),
        "Film".to_string(),
    );
    let mut persisted = RecordingQueue::to_persisted(&task);
    persisted.media_identity = "film".to_string();
    persisted
}

/// A queue with nothing in it; the wait queue is what these exercise.
pub(in crate::recording::recording_transfer::tests) fn slot_queue(dir: &TempDir) -> RecordingQueue {
    RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository")
}

pub(in crate::recording::recording_transfer::tests) fn bare_app_config() -> tuliprox_core::model::AppConfig {
    tuliprox_core::model::AppConfig {
        config: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::Config::default())),
        sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
        hdhomerun: Arc::new(arc_swap::ArcSwapOption::default()),
        api_proxy: Arc::new(arc_swap::ArcSwapOption::default()),
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
        custom_stream_response: Arc::new(arc_swap::ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
    }
}

/// Configured local API listener with no public playback server.
pub(in crate::recording::recording_transfer::tests) fn app_config_with_listener() -> tuliprox_core::model::AppConfig {
    let app_config = bare_app_config();
    let mut config = (*app_config.config.load_full()).clone();
    config.api.host = "127.0.0.1".to_string();
    config.api.port = 8901;
    app_config.config.store(Arc::new(config));
    app_config
}

/// An executable standing in for ffmpeg that appends a line to `log`
/// every time it is run, then writes its output argument and exits
/// cleanly. The log is what lets a test say "once" rather than "at
/// least once".
pub(in crate::recording::recording_transfer::tests) fn counting_ffmpeg(dir: &Path, log: &Path) -> PathBuf {
    let script_path = dir.join("fake-ffmpeg");
    std::fs::write(
        &script_path,
        format!(
            "#!/bin/sh\necho run >> \"{}\"\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf 'recorded' > \"$output\"\nexit 0\n",
            log.display()
        ),
    )
    .expect("write fake ffmpeg");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod");
    }
    script_path
}

pub(in crate::recording::recording_transfer::tests) fn spawn_count(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().filter(|line| !line.is_empty()).count())
}

#[cfg(unix)]
pub(in crate::recording::recording_transfer::tests) struct LimitedCapacity {
    pub(in crate::recording::recording_transfer::tests) limit: usize,
    pub(in crate::recording::recording_transfer::tests) in_use: AtomicUsize,
    pub(in crate::recording::recording_transfer::tests) peak: AtomicUsize,
    pub(in crate::recording::recording_transfer::tests) releases: AtomicUsize,
    pub(in crate::recording::recording_transfer::tests) notify: Arc<Notify>,
}

#[cfg(unix)]
impl RecordingCapacityPort for LimitedCapacity {
    fn capacities_for_input<'a>(
        &'a self,
        input: &'a Arc<str>,
    ) -> futures::future::BoxFuture<'a, Vec<crate::recording::recording_capacity::ProviderCapacity>> {
        Box::pin(async move {
            let in_use = self.in_use.load(Ordering::SeqCst);
            tokio::task::yield_now().await;
            vec![(input.clone(), in_use, self.limit)]
        })
    }

    fn acquire<'a>(
        &'a self,
        _input: &'a Arc<str>,
        _priority: i8,
    ) -> futures::future::BoxFuture<'a, Option<tuliprox_core::model::ProviderHandle>> {
        Box::pin(async move {
            let previous = self
                .in_use
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    (self.limit == 0 || used < self.limit).then_some(used + 1)
                })
                .ok()?;
            self.peak.fetch_max(previous + 1, Ordering::SeqCst);
            Some(tuliprox_core::model::ProviderHandle::new(
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                1,
                tuliprox_core::model::ProviderAllocation::Exhausted,
                Some(tokio_util::sync::CancellationToken::new()),
            ))
        })
    }

    fn release(&self, handle: Option<tuliprox_core::model::ProviderHandle>) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            if handle.is_some() {
                self.in_use.fetch_sub(1, Ordering::SeqCst);
                self.releases.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_one();
            }
        })
    }

    fn capacity_changed(&self) -> Arc<Notify> { self.notify.clone() }
}

#[cfg(unix)]
pub(in crate::recording::recording_transfer::tests) struct ConcurrentLiveFixture {
    pub(in crate::recording::recording_transfer::tests) dir: TempDir,
    pub(in crate::recording::recording_transfer::tests) queue: Arc<RecordingQueue>,
    pub(in crate::recording::recording_transfer::tests) script: PathBuf,
    pub(in crate::recording::recording_transfer::tests) capacity: Arc<LimitedCapacity>,
    pub(in crate::recording::recording_transfer::tests) app: Arc<tuliprox_core::model::AppConfig>,
    pub(in crate::recording::recording_transfer::tests) config: RecordingConfig,
    pub(in crate::recording::recording_transfer::tests) cancel: tokio_util::sync::CancellationToken,
}

#[cfg(unix)]
impl ConcurrentLiveFixture {
    pub(in crate::recording::recording_transfer::tests) async fn new(
        count: u32,
        limit: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_background_limit(count, limit, 0).await
    }

    pub(in crate::recording::recording_transfer::tests) async fn with_background_limit(
        count: u32,
        limit: usize,
        background_limit: u8,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
        let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
        std::fs::write(&script, "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf recorded > \"$output\"\ntouch \"$output.ready\"\nread -r control\nexit 0\n")?;
        let now = chrono::Utc::now().timestamp();
        let tasks = (0..count)
            .map(|index| {
                let mut task = scheduled_task(RecordingKind::Live, now, 300);
                task.uuid = format!("live-{index}");
                task.state = RecordingTaskState::Queued;
                task.priority = 5;
                task.input_name = Some(Arc::from("provider"));
                task.recording.source.virtual_id = (42 + index).to_string();
                task.recording.source.input_name = "provider".to_string();
                task.file_dir = dir.path().to_path_buf();
                task.file_path = dir.path().join(format!("live-{index}.ts"));
                RecordingQueue::to_persisted(&task)
            })
            .collect::<Vec<_>>();
        crate::recording::recording_queue::mutate(&queue, |candidate| {
            candidate.queue.extend(tasks);
            Ok(())
        })
        .await?;
        let fixture = Self {
            dir,
            queue,
            script,
            capacity: Arc::new(LimitedCapacity {
                limit,
                in_use: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                notify: Arc::new(Notify::new()),
            }),
            app: Arc::new(app_config_with_listener()),
            config: RecordingConfig::from(&shared::model::RecordingConfigDto {
                enabled: true,
                max_background_per_provider: background_limit,
                ..Default::default()
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        start_recording_scheduler(
            fixture.app.clone(),
            fixture.config.clone(),
            &fixture.queue,
            NoopSink,
            fixture.capacity.clone(),
            fixture.cancel.clone(),
            fixture.script.clone(),
        );
        Ok(fixture)
    }

    pub(in crate::recording::recording_transfer::tests) async fn start(&self) -> Result<(), String> {
        let capacity: Arc<dyn RecordingCapacityPort> = self.capacity.clone();
        ensure_recording_worker_running(&self.app, &self.config, &self.queue, &NoopSink, &capacity, &self.script).await
    }

    pub(in crate::recording::recording_transfer::tests) async fn wait_for_running(
        &self,
        count: usize,
    ) -> Result<Vec<String>, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = self
                    .queue
                    .active
                    .read()
                    .await
                    .iter()
                    .filter(|task| {
                        task.state == RecordingTaskState::Running
                            && self.dir.path().join(format!("{}.ts.partial.ready", task.uuid)).exists()
                    })
                    .map(|task| task.uuid.clone())
                    .collect::<Vec<_>>();
                if ready.len() == count {
                    break ready;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    pub(in crate::recording::recording_transfer::tests) async fn stop_all(
        &self,
        count: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let active = self.queue.active.read().await.iter().map(|task| task.uuid.clone()).collect::<Vec<_>>();
        for uuid in active {
            self.queue.cancel_requested(&uuid).await?;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.queue.finished.read().await.len() != count || self.queue.workers_running().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(self.capacity.in_use.load(Ordering::SeqCst), 0);
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ConcurrentLiveFixture {
    fn drop(&mut self) { self.cancel.cancel(); }
}

pub(in crate::recording::recording_transfer::tests) async fn read_request(
    socket: &mut tokio::net::TcpStream,
) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    let mut byte = [0; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete request headers"));
        }
        request.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&request).to_ascii_lowercase())
}
