use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use std::{
    collections::HashMap,
    fmt::Write,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::watch};
use tuliprox_testkit::{
    bootstrap::{fixture_password, FixtureInputPlan, FixturePaths, IsolatedSut},
    config::{Channel, FixtureInputType, FixtureStalker, FixtureStreamOptions, PolicyContract},
    discovery::VirtualIdMap,
    hls_probe::{segment_parts, StartupMode},
    protocol::RunId,
    TestkitError,
};

pub const SUBJECT: u32 = 17;
pub const COMPETITOR: u32 = 19;

#[derive(Clone, Copy, Debug)]
pub enum Case {
    Start,
    Success,
    Exhaustion,
    PendingExpiry,
    Preemption,
    Recovery,
    RecoveryCvr,
    RecoveryError,
}

impl Case {
    pub const fn is_recovery(self) -> bool { matches!(self, Self::Recovery | Self::RecoveryCvr | Self::RecoveryError) }

    pub const fn uses_valid_ts(self) -> bool { matches!(self, Self::Preemption) || self.is_recovery() }

    pub const fn target(self) -> u32 {
        match self {
            Self::Start => 4,
            Self::Success => 1,
            // Allow the normal temporary-failure threshold to settle before
            // Pending expires. Expiry is exercised independently below.
            Self::Exhaustion => 8,
            Self::PendingExpiry | Self::Recovery | Self::RecoveryCvr | Self::RecoveryError => 2,
            // The checked-in TS assets run for just over ten seconds.
            Self::Preemption => 12,
        }
    }

    pub const fn timeout_ms(self) -> u64 {
        match self {
            Self::Start | Self::Preemption | Self::Recovery | Self::RecoveryCvr | Self::RecoveryError => 3_000,
            Self::Success | Self::Exhaustion => 5_000,
            Self::PendingExpiry => 10_000,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub marker: u32,
    pub segment: Option<u64>,
    pub attempt: usize,
    pub at: Instant,
}

pub struct Origin {
    pub case: Case,
    run: RunId,
    pub requests: Mutex<Vec<Request>>,
    pub release_retry: watch::Sender<bool>,
    /// 0: initial three units, 1: 407, 2: recovered six units, 3: healthy warmup.
    pub manifest_phase: AtomicU8,
    address: std::net::SocketAddr,
}

fn tagged_transport_stream(source: &[u8], marker: &str) -> Vec<u8> {
    // A null packet preserves valid audio/video and survives timestamp/CC rewriting.
    assert!(marker.len() < 184);
    let mut packet = [0xff; 188];
    packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    packet[4..4 + marker.len()].copy_from_slice(marker.as_bytes());
    [source, packet.as_slice()].concat()
}

pub fn preemption_segment(marker: u32, sequence: u64) -> Vec<u8> {
    tagged_transport_stream(
        include_bytes!("../../../../test/fixtures/hls/channel_unavailable.ts"),
        &format!("TPX-LIVE-{marker}-{sequence};"),
    )
}

fn short_channel_unavailable() -> &'static [u8] {
    static MEDIA: OnceLock<Vec<u8>> = OnceLock::new();
    MEDIA.get_or_init(|| {
        use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;
        let source = include_bytes!("../../../../test/fixtures/hls/channel_unavailable.ts");
        for divisor in 2..=32 {
            let end = (source.len() / 188 / divisor) * 188;
            let bytes = source[..end].to_vec();
            let inspected = TransportStreamBuffer::new(bytes.clone());
            if inspected.duration_ms().is_some_and(|duration| (500..=2_000).contains(&duration))
                && inspected.finite_hls_track_signature().is_some()
            {
                return bytes;
            }
        }
        panic!("the existing channel-unavailable fixture needs a playable prefix fitting target duration 2");
    })
}

pub fn origin_segment(case: Case, marker: u32, sequence: u64) -> Vec<u8> {
    if case.is_recovery() {
        tagged_transport_stream(short_channel_unavailable(), &format!("TPX-LIVE-{marker}-{sequence};"))
    } else {
        preemption_segment(marker, sequence)
    }
}

fn custom_response_config(case: Case, paths: &FixturePaths) -> Result<String, TestkitError> {
    if matches!(case, Case::RecoveryCvr) {
        let responses = paths.root.join("responses");
        std::fs::create_dir(&responses)?;
        std::fs::write(
            responses.join("channel_unavailable.ts"),
            tagged_transport_stream(short_channel_unavailable(), "TPX-CVS-CHANNEL_UNAVAILABLE;"),
        )?;
        return Ok(format!(
            "custom_stream_response_enabled: true\ncustom_stream_response_path: '{}'\n",
            responses.display().to_string().replace('\'', "''")
        ));
    }
    if !matches!(case, Case::Preemption) {
        return Ok("custom_stream_response_enabled: false\n".to_owned());
    }
    let responses = paths.root.join("responses");
    std::fs::create_dir(&responses)?;
    std::fs::write(
        responses.join("low_priority_preempted.ts"),
        tagged_transport_stream(
            include_bytes!("../../../../test/fixtures/hls/low_priority_preempted.ts"),
            "TPX-CVS-LOW_PRIORITY_PREEMPTED;",
        ),
    )?;
    let mut api: serde_json::Value = serde_saphyr::from_str(&std::fs::read_to_string(&paths.api_proxy_file)?).unwrap();
    for user in api["user"][0]["credentials"].as_array_mut().unwrap() {
        // Keep the occupied slots protected; only the subject has lower priority.
        user["priority"] = serde_json::json!(match user["username"].as_str().unwrap() {
            "viewer" => 10,
            "holder" => -10,
            _ => 0,
        });
    }
    std::fs::write(&paths.api_proxy_file, serde_saphyr::to_string(&api).unwrap())?;
    Ok(format!(
        "custom_stream_response_enabled: true\ncustom_stream_response_path: '{}'\n",
        responses.display().to_string().replace('\'', "''")
    ))
}

impl Origin {
    pub fn snapshot(&self) -> Vec<Request> { self.requests.lock().unwrap().clone() }

    pub fn subject_segments(&self) -> Vec<Request> {
        self.snapshot().into_iter().filter(|request| request.marker == SUBJECT && request.segment.is_some()).collect()
    }

    pub fn manifest_at(&self) -> Instant {
        self.snapshot()
            .into_iter()
            .find(|request| request.marker == SUBJECT && request.segment.is_none())
            .expect("subject manifest was requested")
            .at
    }

    fn record(&self, marker: u32, segment: Option<u64>) -> usize {
        let mut requests = self.requests.lock().unwrap();
        let attempt =
            requests.iter().filter(|request| request.marker == marker && request.segment == segment).count() + 1;
        requests.push(Request { marker, segment, attempt, at: Instant::now() });
        attempt
    }
}

async fn origin(State(state): State<Arc<Origin>>, uri: Uri) -> Response {
    let path = uri.path();
    if path == "/catalog/input.m3u" {
        let mut catalog = String::from("#EXTM3U\n");
        for marker in [SUBJECT, COMPETITOR, 101, 102, 103] {
            let resource = match marker {
                103 => "probe/103.ts".to_owned(),
                101 | 102 => format!("hold/{marker}.ts"),
                _ => format!("hls/{marker}/index.m3u8"),
            };
            writeln!(
                catalog,
                "#EXTINF:-1 tvg-id=\"test-{marker}\" group-title=\"Test\",Test {marker}\nhttp://{}/{resource}",
                state.address
            )
            .unwrap();
        }
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], catalog).into_response();
    }
    if path == "/probe/103.ts" {
        // A finite, distinct owner can verify immediate capacity release and
        // finish before the failed HLS lease is polled again.
        return ([(header::CONTENT_TYPE, "video/mp2t")], vec![0x47; 16_384]).into_response();
    }
    if path.starts_with("/hold/") {
        let stream = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Some((Ok::<_, std::io::Error>(vec![0x47; 16_384]), ()))
        });
        return Response::builder().header(header::CONTENT_TYPE, "video/mp2t").body(Body::from_stream(stream)).unwrap();
    }
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts[0] != "hls" {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }
    let marker = parts[1].parse::<u32>().unwrap();
    if parts[2] == "index.m3u8" {
        state.record(marker, None);
        let phase = state.manifest_phase.load(Ordering::Acquire);
        if state.case.is_recovery() && marker == SUBJECT && phase == 1 {
            return StatusCode::PROXY_AUTHENTICATION_REQUIRED.into_response();
        }
        let target = state.case.target();
        let head = if state.case.is_recovery() && marker == SUBJECT && phase == 2 { 6 } else { 0 };
        let mut manifest = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            100 + head
        );
        let count = if state.case.is_recovery() && marker == SUBJECT && phase != 0 { 6 } else { 3 };
        for sequence in head..head + count {
            writeln!(manifest, "#EXTINF:{target}.0,\n{sequence}.ts").unwrap();
        }
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], manifest).into_response();
    }
    let Some(segment) = parts[2].strip_suffix(".ts").and_then(|value| value.parse::<u64>().ok()) else {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    };
    let attempt = state.record(marker, Some(segment));
    if marker == SUBJECT && matches!(state.case, Case::Exhaustion) && segment != 0 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if marker == SUBJECT
        && segment == 0
        && !matches!(state.case, Case::Start | Case::Preemption)
        && !state.case.is_recovery()
    {
        if attempt == 1 {
            // Delay headers, not an already-exposed body. Every mode may retry this transfer.
            tokio::time::sleep(Duration::from_millis(state.case.timeout_ms() + 1_000)).await;
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if attempt == 2 {
            // The following 250 ms backoff has a positive minimum despite the production jitter.
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let mut released = state.release_retry.subscribe();
        while !*released.borrow_and_update() {
            if released.changed().await.is_err() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        }
        if matches!(state.case, Case::Exhaustion) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    if state.case.uses_valid_ts() {
        return ([(header::CONTENT_TYPE, "video/mp2t")], origin_segment(state.case, marker, segment)).into_response();
    }
    let (prefix, tail) = segment_parts(&state.run, marker, segment).unwrap();
    let bytes = [prefix.as_ref(), tail.as_ref()].concat();
    ([(header::CONTENT_TYPE, "video/mp2t")], bytes).into_response()
}

pub struct Fixture {
    pub origin: Arc<Origin>,
    pub run: RunId,
    pub base: String,
    entries: HashMap<String, VirtualIdMap>,
    log: PathBuf,
    holders: Vec<tokio::task::JoinHandle<()>>,
    holder_bytes: Vec<Arc<AtomicUsize>>,
    process: Option<IsolatedSut>,
    origin_task: tokio::task::JoinHandle<()>,
    _paths: FixturePaths,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.origin_task.abort();
        for holder in &self.holders {
            holder.abort();
        }
        if self.process.is_some() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let terminal = log
                .lines()
                .filter(|line| line.contains("HLS ") && (line.contains("terminal") || line.contains("bundle")))
                .rev()
                .take(16)
                .collect::<Vec<_>>();
            for line in
                terminal.into_iter().rev().chain(log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev())
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

pub fn client(identity: &str) -> reqwest::Client {
    reqwest::Client::builder().no_proxy().user_agent(identity).timeout(Duration::from_secs(35)).build().unwrap()
}

impl Fixture {
    pub async fn start(case: Case, mode: StartupMode, limit: u16) -> Result<Self, TestkitError> {
        let binary = std::env::var("TULIPROX_TESTKIT_SUT_BINARY")
            .map_err(|_| TestkitError::Configuration("set TULIPROX_TESTKIT_SUT_BINARY to the built SUT".to_owned()))?;
        let run = RunId::new(format!("hls-reservation-{mode}-{limit}-{case:?}"));
        let origin_listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = origin_listener.local_addr()?;
        let (release_retry, _) = watch::channel(false);
        let origin_state = Arc::new(Origin {
            case,
            run: run.clone(),
            requests: Mutex::new(Vec::new()),
            release_retry,
            manifest_phase: AtomicU8::new(0),
            address,
        });
        let origin_router = Router::new().fallback(origin).with_state(origin_state.clone());
        let origin_task = tokio::spawn(async move {
            axum::serve(origin_listener, origin_router).await.unwrap();
        });
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
        let sut_address = reservation.local_addr()?;
        drop(reservation);
        let base = format!("http://{sut_address}");
        let policy: PolicyContract = serde_json::from_value(serde_json::json!({
            "user_access_control": true,
            "users": {"viewer": {"max_connections": 64}, "other": {"max_connections": 64}, "holder": {"max_connections": 4}, "join": {"max_connections": 4}},
            "admission_strategies": [], "provider_max_connections": limit
        })).unwrap();
        let channels =
            HashMap::from([("subject".to_owned(), Channel { origin_marker: SUBJECT, protocol: "hls".to_owned() })]);
        let paths = FixturePaths::create(
            &run.0,
            address,
            sut_address,
            Some(&policy),
            &channels,
            &FixtureStreamOptions {
                share_live_mpeg_ts: false,
                hls_startup_mode: Some(mode),
                ..FixtureStreamOptions::default()
            },
            false,
            &FixtureInputPlan {
                input_type: FixtureInputType::M3u,
                stalker: &FixtureStalker::default(),
                failover_inputs: &[],
            },
        )?;
        let original = std::fs::read_to_string(&paths.config_file)?;
        let custom_response = custom_response_config(case, &paths)?;
        let config = format!(
            "{custom_response}custom_stream_response_error_status: {}\nconnect_timeout_secs: 1\n{original}    origin_segment_timeout_ms: {}\n    origin_manifest_timeout_ms: 3000\n    initial_manifest_wait_timeout_secs: 25\n    max_segments_prefetch: 0\n    segment_repair:\n      max_level: off\n",
            if case.is_recovery() { 502 } else { 503 }, case.timeout_ms()
        );
        std::fs::write(&paths.config_file, config)?;
        let log = paths.root.join("sut.log");
        let output = std::fs::File::create(&log)?;
        let mut command = tokio::process::Command::new(binary);
        command
            .arg("-s")
            .arg("-c")
            .arg(&paths.config_file)
            .arg("-i")
            .arg(&paths.source_file)
            .arg("-a")
            .arg(&paths.api_proxy_file)
            .current_dir(&paths.root)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        let process =
            IsolatedSut::start_command(command, &format!("{base}/healthcheck"), Duration::from_secs(30)).await?;
        let mut fixture = Self {
            origin: origin_state,
            run,
            base,
            _paths: paths,
            entries: HashMap::new(),
            log,
            holders: Vec::new(),
            holder_bytes: Vec::new(),
            process: Some(process),
            origin_task,
        };
        fixture.discover_users().await;
        fixture.start_holders(limit).await?;
        Ok(fixture)
    }

    async fn discover_users(&mut self) {
        for username in ["viewer", "other", "holder", "join"] {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Ok(response) = client("hls-reservation-discovery")
                    .get(format!("{}/m3u?username={username}&password={}", self.base, fixture_password()))
                    .send()
                    .await
                {
                    if let Ok(body) = response.text().await {
                        if let Ok(map) = VirtualIdMap::from_m3u(&body) {
                            self.entries.insert(username.to_owned(), map);
                            break;
                        }
                    }
                }
                assert!(Instant::now() < deadline, "catalog discovery exceeded deadline");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    async fn start_holders(&mut self, limit: u16) -> Result<(), TestkitError> {
        for marker in 101..100 + u32::from(limit) {
            let mut holder =
                client(&format!("hls-reservation-holder-{marker}")).get(self.entry("holder", marker)).send().await?;
            assert_eq!(holder.status(), StatusCode::OK, "holder admission");
            let first = holder.chunk().await?.unwrap();
            assert!(!first.is_empty() && first.iter().all(|byte| *byte == 0x47));
            let counter = Arc::new(AtomicUsize::new(first.len()));
            self.holder_bytes.push(Arc::clone(&counter));
            self.holders.push(tokio::spawn(async move {
                while let Ok(Some(bytes)) = holder.chunk().await {
                    assert!(bytes.iter().all(|byte| *byte == 0x47), "occupied-slot stream data must remain intact");
                    counter.fetch_add(bytes.len(), Ordering::Release);
                }
            }));
        }
        Ok(())
    }

    pub fn entry(&self, username: &str, marker: u32) -> &str { self.entries[username].playback_url(marker).unwrap() }

    pub fn log_contains(&self, needle: &str) -> bool {
        std::fs::read_to_string(&self.log).unwrap_or_default().contains(needle)
    }

    pub fn log_count(&self, needle: &str) -> usize {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().filter(|line| line.contains(needle)).count()
    }

    pub fn holder_progress(&self) -> Vec<usize> {
        self.holder_bytes.iter().map(|counter| counter.load(Ordering::Acquire)).collect()
    }

    pub fn subject_log_contains(&self, needle: &str) -> bool {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let identity = log
            .lines()
            .find(|line| line.contains("Segment '000000' attempting URL"))
            .and_then(|line| line.split_whitespace().find(|word| word.starts_with("proxy_session=")))
            .expect("subject transfer must have a diagnostic identity");
        log.lines().any(|line| line.contains(identity) && line.contains(needle))
    }

    pub fn holders_alive(&self) -> bool { self.holders.iter().all(|holder| !holder.is_finished()) }

    pub fn reserved_without_io(&self, limit: u16) -> usize {
        let active = format!("active_connections={} ", limit - 1);
        let reserved = format!("reason=reserved-capacity, current={limit}, max={limit}, foreign_reserved=1,");
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                (line.contains("reason=ReservedForOther conflict=provider-capacity")
                    && line.contains(&active)
                    && line.contains("idle_foreign_reserved_slots=1"))
                    || line.contains(&reserved)
            })
            .count()
    }

    pub async fn stop(mut self) -> Result<(), TestkitError> {
        for holder in self.holders.drain(..) {
            holder.abort();
        }
        self.process.take().unwrap().stop().await?;
        self.origin_task.abort();
        Ok(())
    }
}
