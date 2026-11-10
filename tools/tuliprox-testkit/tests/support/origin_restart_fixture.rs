use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
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

pub const MAX_MANIFEST_FAILURE_BACKOFF: Duration = Duration::from_secs(1);
pub const DIAGNOSTIC_AUTH_PATH: &str = "synthetic-auth-path";
pub const DIAGNOSTIC_QUERY: &str = "opaque=synthetic-opaque";
pub const DIAGNOSTIC_TOKEN: &str = "token=synthetic-token";

#[derive(Clone, Copy, Debug)]
pub enum ProviderCase {
    Failover,
    Authentication,
    Status,
    Connection,
    Timeout,
    Redirect,
}

#[derive(Clone, Copy)]
enum RestartScenario {
    Single,
    Double,
    RedirectRollover,
}

pub struct Origin {
    pub phase: AtomicU8,
    pub unchanged_first: bool,
    replay_only: bool,
    pub reentry_manifests: AtomicUsize,
    pub requests: Mutex<Vec<String>>,
    diagnostic_markers: bool,
    provider: Option<ProviderCase>,
    address: std::net::SocketAddr,
    scenario: RestartScenario,
    started: tokio::time::Instant,
    pauses: [Mutex<Option<tokio::time::Instant>>; 2],
    protocol: Mutex<Vec<serde_json::Value>>,
}

impl Origin {
    pub fn protocol(&self) -> Vec<serde_json::Value> { self.protocol.lock().unwrap().clone() }

    pub fn pause_observed(&self, index: usize) -> Option<tokio::time::Instant> { *self.pauses[index].lock().unwrap() }
}

fn source_media() -> &'static [u8] {
    static MEDIA: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    MEDIA.get_or_init(|| {
        let source = include_bytes!("../../../../test/fixtures/hls/channel_unavailable.ts");
        for divisor in 2..=32 {
            let end = source.len() / divisor / 188 * 188;
            let bytes = source[..end].to_vec();
            let inspected = tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer::new(bytes.clone());
            if inspected.duration_ms().is_some_and(|duration| (500..=2_000).contains(&duration))
                && inspected.finite_hls_track_signature().is_some()
            {
                return bytes;
            }
        }
        panic!("synthetic media requires a playable prefix fitting target duration 2");
    })
}

pub fn bytes(generation: u8, sequence: u64) -> Vec<u8> {
    let mut packet = [0xff; 188];
    packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let marker = format!("TPX-RESTART-{generation}-{sequence};");
    packet[4..4 + marker.len()].copy_from_slice(marker.as_bytes());
    [source_media(), packet.as_slice()].concat()
}

async fn origin(State(state): State<Arc<Origin>>, headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path();
    let phase = state.phase.load(Ordering::Acquire);
    state.requests.lock().unwrap().push(format!("phase={phase} {uri}"));
    if path == "/catalog/input.m3u" {
        let manifest = if state.diagnostic_markers {
            format!("/hls/17/{DIAGNOSTIC_AUTH_PATH}/index.m3u8?{DIAGNOSTIC_TOKEN}&{DIAGNOSTIC_QUERY}")
        } else {
            "/hls/17/index.m3u8".to_string()
        };
        let authority = if state.diagnostic_markers {
            format!("synthetic-user:synthetic-password@{}", state.address)
        } else {
            state.address.to_string()
        };
        let origin =
            if state.provider.is_some() { "provider://fixture".to_string() } else { format!("http://{authority}") };
        return format!("#EXTM3U\n#EXTINF:-1 tvg-id=\"test-17\" group-title=\"Test\",Test 17\n{origin}{manifest}\n")
            .into_response();
    }
    if matches!(state.scenario, RestartScenario::RedirectRollover) {
        return redirect_rollover_response(&state, &headers, &uri, phase);
    }
    if state.diagnostic_markers
        && (!path.contains(DIAGNOSTIC_AUTH_PATH)
            || !uri.query().is_some_and(|query| query.contains(DIAGNOSTIC_QUERY) && query.contains(DIAGNOSTIC_TOKEN)))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if matches!(state.scenario, RestartScenario::Double) {
        return double_restart_response(&state, &uri, phase);
    }
    if path.starts_with("/unavailable/") {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if path.starts_with("/authentication/") {
        return StatusCode::PROXY_AUTHENTICATION_REQUIRED.into_response();
    }
    if path.starts_with("/timeout/") {
        tokio::time::sleep(Duration::from_secs(10)).await;
        return StatusCode::GATEWAY_TIMEOUT.into_response();
    }
    if path.starts_with("/redirect/") {
        // A redirect target without a manifest suffix must still inherit HLS privacy.
        return (
            StatusCode::FOUND,
            [(
                header::LOCATION,
                format!("/redirect/{DIAGNOSTIC_AUTH_PATH}/opaque-target?{DIAGNOSTIC_TOKEN}&{DIAGNOSTIC_QUERY}"),
            )],
        )
            .into_response();
    }
    if path.ends_with("/index.m3u8") {
        if phase == 1 {
            return StatusCode::PROXY_AUTHENTICATION_REQUIRED.into_response();
        }
        let unchanged = phase == 2
            && state.unchanged_first
            && (state.reentry_manifests.fetch_add(1, Ordering::AcqRel) == 0 || state.replay_only);
        let new = phase >= 2 && !unchanged;
        let namespace = if new { "fedcba9876543210" } else { "0123456789abcdef" };
        let head = u64::from(phase == 3);
        let count = if new { 4 } else { 5 };
        let mut manifest =
            format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
        for sequence in head..head + count {
            if state.diagnostic_markers {
                writeln!(manifest, "#EXTINF:2.0,\n/hls/17/{DIAGNOSTIC_AUTH_PATH}/{namespace}/{sequence}.ts?{DIAGNOSTIC_TOKEN}&{DIAGNOSTIC_QUERY}").unwrap();
            } else {
                writeln!(manifest, "#EXTINF:2.0,\n/hls/17/{namespace}/{sequence}.ts?synthetic=1").unwrap();
            }
        }
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], manifest).into_response();
    }
    let Some(sequence) =
        path.rsplit('/').next().and_then(|file| file.strip_suffix(".ts")).and_then(|number| number.parse::<u64>().ok())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let generation = u8::from(path.contains("fedcba9876543210"));
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from(bytes(generation, sequence)))
        .unwrap()
}

fn double_restart_response(state: &Origin, uri: &Uri, phase: u8) -> Response {
    let path = uri.path();
    let response = if path == "/hls/17/index.m3u8" {
        if phase == 1 || phase == 4 {
            let index = usize::from(phase == 4);
            state.pauses[index].lock().unwrap().get_or_insert_with(tokio::time::Instant::now);
            StatusCode::PROXY_AUTHENTICATION_REQUIRED.into_response()
        } else {
            let namespace = match phase {
                0 => "a",
                2 | 3 => "b",
                5 | 6 => "c",
                _ => panic!("invalid double restart phase"),
            };
            let head = u64::from(phase == 3 || phase == 6);
            let mut body =
                format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
            for sequence in head..head + 6 {
                writeln!(body, "#EXTINF:2.0,\n/hls/17/{namespace}/{sequence}.ts?synthetic=1").unwrap();
            }
            ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], body).into_response()
        }
    } else {
        let occurrence = path.strip_prefix("/hls/17/").and_then(|tail| tail.split_once('/')).and_then(|(ns, file)| {
            let generation = match ns {
                "a" => 0,
                "b" => 1,
                "c" => 2,
                _ => return None,
            };
            file.strip_suffix(".ts").and_then(|seq| seq.parse::<u64>().ok()).map(|seq| (generation, seq))
        });
        if let Some((generation, sequence)) =
            occurrence.filter(|(_, seq)| *seq <= 6 && uri.query() == Some("synthetic=1"))
        {
            Response::builder()
                .header(header::CONTENT_TYPE, "video/mp2t")
                .body(Body::from(bytes(generation, sequence)))
                .unwrap()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    };
    state.protocol.lock().unwrap().push(serde_json::json!({
        "elapsed_ms": state.started.elapsed().as_millis(), "phase": phase,
        "uri": uri.to_string(), "status": response.status().as_u16(),
        "origin_media_sequence": (path == "/hls/17/index.m3u8" && response.status().is_success())
            .then_some(u64::from(phase == 3 || phase == 6)),
    }));
    response
}

fn redirect_rollover_response(state: &Origin, headers: &HeaderMap, uri: &Uri, phase: u8) -> Response {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("missing");
    let response = if uri.path() == "/hls/17/index.m3u8" {
        let target = if phase == 0 { "127.0.0.1" } else { "localhost" };
        (
            StatusCode::FOUND,
            [(header::LOCATION, format!("http://{target}:{}/rollover/index.m3u8", state.address.port()))],
        )
            .into_response()
    } else if uri.path() == "/rollover/index.m3u8" {
        let head = match phase {
            0 => 432,
            1 => 434,
            _ => 436,
        };
        let mut body = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
        for sequence in head..head + 6 {
            writeln!(body, "#EXTINF:8.0,\n/media/{sequence}.ts?synthetic=1").unwrap();
        }
        ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], body).into_response()
    } else if let Some(seq) = uri
        .path()
        .strip_prefix("/media/")
        .and_then(|file| file.strip_suffix(".ts"))
        .and_then(|seq| seq.parse::<u64>().ok())
        .filter(|seq| (432..444).contains(seq))
    {
        ([(header::CONTENT_TYPE, "video/mp2t")], Body::from(bytes(0, seq))).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    };
    state.protocol.lock().unwrap().push(serde_json::json!({"elapsed_ms": state.started.elapsed().as_millis(), "phase": phase, "host": host, "uri": uri.to_string(), "status": response.status().as_u16()}));
    response
}

fn configure_provider(
    paths: &FixturePaths,
    address: std::net::SocketAddr,
    sanitize: bool,
    case: ProviderCase,
) -> Result<(), TestkitError> {
    let config = std::fs::read_to_string(&paths.config_file)?
        .replace("  log_level: debug", &format!("  log_level: debug\n  sanitize_sensitive_info: {sanitize}"))
        .replace("origin_manifest_timeout_ms: 3000", "origin_manifest_timeout_ms: 500")
        .replace("initial_manifest_wait_timeout_secs: 15", "initial_manifest_wait_timeout_secs: 8");
    std::fs::write(&paths.config_file, config)?;
    let urls = match case {
        ProviderCase::Failover => format!("      - http://{address}/unavailable\n      - http://{address}"),
        ProviderCase::Authentication => format!("      - http://{address}/authentication"),
        ProviderCase::Status => format!("      - http://{address}/unavailable"),
        ProviderCase::Timeout => format!("      - http://{address}/timeout"),
        ProviderCase::Redirect => format!("      - http://{address}/redirect"),
        ProviderCase::Connection => {
            let reserved = std::net::TcpListener::bind("127.0.0.1:0")?;
            let refused = reserved.local_addr()?;
            drop(reserved);
            format!("      - http://{address}/unavailable\n      - http://{refused}")
        }
    };
    let source = std::fs::read_to_string(&paths.source_file)?.replacen(
        "    type: m3u",
        "    type: m3u\n    playback_provider: fixture",
        1,
    );
    std::fs::write(&paths.source_file, format!("provider:\n  - name: fixture\n    urls:\n{urls}\n{source}"))?;
    Ok(())
}

pub struct Fixture {
    pub origin: Arc<Origin>,
    pub entry: String,
    pub log: PathBuf,
    process: Option<IsolatedSut>,
    task: tokio::task::JoinHandle<()>,
    paths: FixturePaths,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        if self.process.is_some() {
            if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
                let _ = std::fs::create_dir_all(&directory);
                let directory = PathBuf::from(directory);
                let _ = std::fs::copy(&self.log, directory.join("last-failure.log"));
                let _ = std::fs::write(
                    directory.join("last-failure-origin-requests.txt"),
                    self.origin.requests.lock().unwrap().join("\n"),
                );
                if let Ok(protocol) = serde_json::to_vec_pretty(&*self.origin.protocol.lock().unwrap()) {
                    let _ = std::fs::write(directory.join("last-failure-origin.json"), protocol);
                }
            }
            for line in std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let safe = line
                    .split_whitespace()
                    .map(|word| if word.contains('/') || word.contains('\\') { "<location>" } else { word })
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!("SUT diagnostic: {safe}");
            }
        }
    }
}

impl Fixture {
    pub async fn start(mode: StartupMode, unchanged_first: bool) -> Result<Self, TestkitError> {
        Self::start_with_replay(mode, unchanged_first, false, None, RestartScenario::Single).await
    }

    pub async fn start_replay(mode: StartupMode) -> Result<Self, TestkitError> {
        Self::start_with_replay(mode, true, true, None, RestartScenario::Single).await
    }

    pub async fn start_provider(sanitize: bool, case: ProviderCase) -> Result<Self, TestkitError> {
        Self::start_with_replay(StartupMode::FirstReady, false, false, Some((sanitize, case)), RestartScenario::Single)
            .await
    }

    pub async fn start_double_restart(mode: StartupMode) -> Result<Self, TestkitError> {
        Self::start_with_replay(mode, false, false, None, RestartScenario::Double).await
    }

    pub async fn start_redirect_rollover() -> Result<Self, TestkitError> {
        Self::start_with_replay(StartupMode::Conservative, false, false, None, RestartScenario::RedirectRollover).await
    }

    async fn start_with_replay(
        mode: StartupMode,
        unchanged_first: bool,
        replay_only: bool,
        provider: Option<(bool, ProviderCase)>,
        scenario: RestartScenario,
    ) -> Result<Self, TestkitError> {
        let binary = std::env::var("TULIPROX_TESTKIT_SUT_BINARY")
            .map_err(|_| TestkitError::Configuration("set TULIPROX_TESTKIT_SUT_BINARY to the built proxy".into()))?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Origin {
            phase: AtomicU8::new(0),
            unchanged_first,
            replay_only,
            reentry_manifests: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            diagnostic_markers: provider.is_some() || std::env::var_os("TULIPROX_TESTKIT_DIAGNOSTIC_MARKERS").is_some(),
            provider: provider.map(|(_, case)| case),
            address,
            scenario,
            started: tokio::time::Instant::now(),
            pauses: [Mutex::new(None), Mutex::new(None)],
            protocol: Mutex::new(Vec::new()),
        });
        let app = Router::new().fallback(origin).with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
        let sut_address = reservation.local_addr()?;
        drop(reservation);
        let base = format!("http://{sut_address}");
        let paths = FixturePaths::create(
            &format!("origin-restart-{mode}-{unchanged_first}"),
            address,
            sut_address,
            None,
            &HashMap::from([("restart".into(), Channel { origin_marker: 17, protocol: "hls".into() })]),
            &FixtureStreamOptions {
                hls_startup_mode: Some(mode),
                hls_startup_strip_segments: Some(0),
                ..Default::default()
            },
            false,
            &FixtureInputPlan {
                input_type: FixtureInputType::M3u,
                stalker: &FixtureStalker::default(),
                failover_inputs: &[],
            },
        )?;
        let responses = paths.root.join("responses");
        std::fs::create_dir(&responses)?;
        std::fs::write(responses.join("channel_unavailable.ts"), bytes(9, 0))?;
        let original = std::fs::read_to_string(&paths.config_file)?;
        std::fs::write(&paths.config_file, format!("custom_stream_response_enabled: true\ncustom_stream_response_path: '{}'\ncustom_stream_response_error_status: 502\nconnect_timeout_secs: 1\n{original}    origin_manifest_timeout_ms: 3000\n    initial_manifest_wait_timeout_secs: 15\n    max_segments_prefetch: 0\n    segment_repair:\n      max_level: off\n", responses.display().to_string().replace('\'', "''")))?;
        if let Some((sanitize, case)) = provider {
            configure_provider(&paths, address, sanitize, case)?;
        }
        let log = paths.root.join("sut.log");
        let output = std::fs::File::create(&log)?;
        let mut command = tokio::process::Command::new(binary);
        command
            .env("TULIPROX_HLS_REFRESH_BACKOFF_MS", "0,500,1000")
            .arg("-s")
            .arg("-c")
            .arg(&paths.config_file)
            .arg("-i")
            .arg(&paths.source_file)
            .arg("-a")
            .arg(&paths.api_proxy_file)
            .current_dir(&paths.root)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        let process =
            IsolatedSut::start_command(command, &format!("{base}/healthcheck"), Duration::from_secs(30)).await?;
        let client = client("origin-restart-discovery");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let entry = loop {
            let attempt = async {
                let playlist = client
                    .get(format!("{base}/m3u?username=testkit&password={}", fixture_password()))
                    .send()
                    .await?
                    .text()
                    .await?;
                VirtualIdMap::from_m3u(&playlist)
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
        Ok(Self { origin: state, entry, log, process: Some(process), task, paths })
    }

    pub fn log_contains(&self, value: &str) -> bool {
        std::fs::read_to_string(&self.log).unwrap_or_default().contains(value)
    }

    pub async fn stop_with_observations(self, label: &str, observations: &[String]) -> Result<(), TestkitError> {
        if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory)?;
            std::fs::write(directory.join(format!("{label}-client.txt")), observations.join("\n"))?;
            std::fs::write(
                directory.join(format!("{label}-origin.json")),
                serde_json::to_vec_pretty(&*self.origin.protocol.lock().unwrap()).map_err(std::io::Error::other)?,
            )?;
        }
        self.stop(label).await
    }

    pub async fn stop(mut self, label: &str) -> Result<(), TestkitError> {
        if let Some(directory) = std::env::var_os("TULIPROX_TESTKIT_ARTIFACT_DIR") {
            std::fs::create_dir_all(&directory)?;
            let directory = PathBuf::from(directory);
            std::fs::copy(&self.log, directory.join(format!("{label}.log")))?;
            std::fs::write(
                directory.join(format!("{label}-origin-requests.txt")),
                self.origin.requests.lock().unwrap().join("\n"),
            )?;
        }
        self.process.take().ok_or_else(|| TestkitError::Protocol("missing isolated process".into()))?.stop().await?;
        assert!(self.paths.root.is_dir());
        Ok(())
    }
}

pub fn client(identity: &str) -> reqwest::Client {
    reqwest::Client::builder().no_proxy().user_agent(identity).timeout(Duration::from_secs(20)).build().unwrap()
}
