#[allow(dead_code, reason = "share the existing playable TS byte builder with the origin-restart fixture")]
#[path = "origin_restart_fixture.rs"]
mod origin_media;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tuliprox_testkit::{
    bootstrap::{fixture_password, FixtureInputPlan, FixturePaths, IsolatedSut},
    config::{Channel, FixtureInputType, FixtureStalker, FixtureStreamOptions},
    discovery::VirtualIdMap,
    hls_probe::StartupMode,
    TestkitError,
};

pub fn bytes(sequence: u64) -> Vec<u8> { origin_media::bytes(0, sequence) }
pub fn client(identity: &str) -> reqwest::Client {
    reqwest::Client::builder().no_proxy().user_agent(identity).timeout(Duration::from_secs(20)).build().unwrap()
}

pub struct Origin {
    pub head: AtomicU64,
    pub window_count: AtomicU64,
    pub media_requests: AtomicUsize,
    requests: Mutex<Vec<String>>,
    address: std::net::SocketAddr,
}

async fn serve_origin(State(state): State<Arc<Origin>>, uri: Uri) -> Response {
    let head = state.head.load(Ordering::Acquire);
    let count = state.window_count.load(Ordering::Acquire);
    state.requests.lock().unwrap().push(format!("head={head} count={count} {uri}"));
    if uri.path() == "/catalog/input.m3u" {
        return format!(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"test-17\" group-title=\"Test\",Test 17\nhttp://{}/hls/17/index.m3u8\n",
            state.address
        )
        .into_response();
    }
    if uri.path().ends_with("index.m3u8") {
        let mut body = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
        for sequence in head..head + count {
            writeln!(body, "#EXTINF:2.0,\n/hls/17/{sequence}.ts").unwrap();
        }
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], body).into_response();
    }
    let Some(sequence) = uri
        .path()
        .rsplit('/')
        .next()
        .and_then(|file| file.strip_suffix(".ts"))
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    state.media_requests.fetch_add(1, Ordering::AcqRel);
    Response::builder().header(header::CONTENT_TYPE, "video/mp2t").body(Body::from(bytes(sequence))).unwrap()
}

pub struct Fixture {
    pub origin: Arc<Origin>,
    pub entry: String,
    pub log: PathBuf,
    paths: FixturePaths,
    process: Option<IsolatedSut>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        if self.process.is_some() {
            if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
                let _ = std::fs::create_dir_all(&directory);
                let _ = std::fs::copy(&self.log, PathBuf::from(directory).join("last-failure.log"));
            }
        }
    }
}

impl Fixture {
    pub async fn start(mode: StartupMode) -> Result<Self, TestkitError> { Self::start_configured(mode, 3, 6, 0).await }

    pub async fn start_strip_two(mode: StartupMode) -> Result<Self, TestkitError> {
        Self::start_configured(mode, 2, 5, 6).await
    }

    async fn start_configured(
        mode: StartupMode,
        strip: u64,
        count: u64,
        prefetch: usize,
    ) -> Result<Self, TestkitError> {
        let binary = std::env::var("TULIPROX_TESTKIT_SUT_BINARY")
            .map_err(|_| TestkitError::Configuration("set TULIPROX_TESTKIT_SUT_BINARY to the built proxy".into()))?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = Arc::new(Origin {
            head: AtomicU64::new(0),
            window_count: AtomicU64::new(count),
            media_requests: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            address,
        });
        let app = Router::new().fallback(serve_origin).with_state(Arc::clone(&origin));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
        let sut_address = reservation.local_addr()?;
        drop(reservation);
        let base = format!("http://{sut_address}");
        let paths = FixturePaths::create(
            &format!("strip-entry-{mode}"),
            address,
            sut_address,
            None,
            &HashMap::from([("strip".into(), Channel { origin_marker: 17, protocol: "hls".into() })]),
            &FixtureStreamOptions {
                hls_startup_mode: Some(mode),
                hls_startup_strip_segments: Some(strip),
                ..Default::default()
            },
            false,
            &FixtureInputPlan { input_type: FixtureInputType::M3u, stalker: &FixtureStalker::default() },
        )?;
        let config = std::fs::read_to_string(&paths.config_file)?;
        std::fs::write(&paths.config_file, format!("config_hot_reload: true\n{config}    origin_manifest_timeout_ms: 1000\n    initial_manifest_wait_timeout_secs: 8\n    max_segments_prefetch: {prefetch}\n    segment_repair:\n      max_level: off\n"))?;
        let log = paths.root.join("sut.log");
        let output = std::fs::File::create(&log)?;
        let mut command = tokio::process::Command::new(binary);
        command
            .arg("-s")
            .arg("-p")
            .arg(paths.root.canonicalize()?)
            .arg("-c")
            .arg(paths.config_file.canonicalize()?)
            .arg("-i")
            .arg(paths.source_file.canonicalize()?)
            .arg("-a")
            .arg(paths.api_proxy_file.canonicalize()?)
            .current_dir(&paths.root)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        let process =
            IsolatedSut::start_command(command, &format!("{base}/healthcheck"), Duration::from_secs(30)).await?;
        let player = client("strip-discovery");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let entry = loop {
            let attempt = async {
                let body = player
                    .get(format!("{base}/m3u?username=testkit&password={}", fixture_password()))
                    .send()
                    .await?
                    .text()
                    .await?;
                VirtualIdMap::from_m3u(&body)
            }
            .await;
            if let Ok(map) = attempt {
                if let Ok(entry) = map.playback_url(17) {
                    break entry.to_owned();
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "catalog discovery deadline");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        Ok(Self { origin, entry, log, paths, process: Some(process), task })
    }

    pub fn logs(&self) -> String { std::fs::read_to_string(&self.log).unwrap_or_default() }

    pub async fn reload(&self, old: u64, new: u64) -> Result<(), TestkitError> {
        let before = self.logs().matches("Loaded config file").count();
        let config = std::fs::read_to_string(&self.paths.config_file)?;
        let from = format!("value: {old}");
        assert!(config.contains(&from), "existing strip value");
        std::fs::write(&self.paths.config_file, config.replacen(&from, &format!("value: {new}"), 1))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while self.logs().matches("Loaded config file").count() <= before {
            assert!(tokio::time::Instant::now() < deadline, "real config watcher reload");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    pub async fn stop(mut self, label: &str, observations: &[String]) -> Result<(), TestkitError> {
        if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
            std::fs::create_dir_all(&directory)?;
            let directory = PathBuf::from(directory);
            std::fs::copy(&self.log, directory.join(format!("{label}.log")))?;
            std::fs::write(
                directory.join(format!("{label}-origin.txt")),
                self.origin.requests.lock().unwrap().join("\n"),
            )?;
            std::fs::write(directory.join(format!("{label}-client.txt")), observations.join("\n"))?;
        }
        self.process.take().ok_or_else(|| TestkitError::Protocol("missing isolated process".into()))?.stop().await
    }
}
