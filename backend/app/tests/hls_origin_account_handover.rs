//! Real-process regression: an origin account publication must reach every `WebUI` socket immediately.
use axum::{
    extract::{OriginalUri, State},
    response::IntoResponse,
    Router,
};
use futures::{SinkExt, StreamExt};
use shared::model::{ActiveUserConnectionChange, ProtocolMessage, SourcesConfigDto, StreamInfo, PROTOCOL_VERSION};
use std::{
    fmt::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Notify,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
struct Origin {
    catalog: String,
    manifests: Arc<AtomicUsize>,
    segments: Arc<AtomicUsize>,
    proceed: Arc<Notify>,
}

async fn origin(State(state): State<Origin>, OriginalUri(uri): OriginalUri) -> axum::response::Response {
    let path = uri.path();
    if path == "/__continue" {
        state.proceed.notify_one();
        return "continued".into_response();
    }
    if path.ends_with("catalog.m3u") {
        return ([("content-type", "audio/x-mpegurl")], state.catalog).into_response();
    }
    if Path::new(path).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("m3u8")) {
        state.manifests.fetch_add(1, Ordering::AcqRel);
        return ([("content-type", "application/vnd.apple.mpegurl")], "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\n100.ts\n#EXTINF:4,\n101.ts\n#EXTINF:4,\n102.ts\n#EXTINF:4,\n103.ts\n#EXTINF:4,\n104.ts\n#EXTINF:4,\n105.ts\n").into_response();
    }
    if Path::new(path).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("ts")) {
        state.segments.fetch_add(1, Ordering::AcqRel);
        return (
            [("content-type", "video/mp2t")],
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .as_slice(),
        )
            .into_response();
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

struct Fixture {
    directory: tempfile::TempDir,
    base: String,
    source: PathBuf,
    process: tokio::process::Child,
    origins: Vec<tokio::task::JoinHandle<()>>,
    b: Origin,
    proceed: Arc<Notify>,
    control: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        for origin in &self.origins {
            origin.abort();
        }
    }
}

impl Fixture {
    async fn start() -> Self {
        let a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = format!("localhost:{}", a.local_addr().unwrap().port());
        let b_addr = b.local_addr().unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["web", "storage", "backup"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let frontend = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let web = if frontend.join("index.html").exists() { frontend } else { root.join("web") };
        let config = format!("api:\n  host: 127.0.0.1\n  port: {port}\n  web_root: '{}'\nworking_dir: '{}'\nbackup_dir: '{}'\nupdate_on_boot: true\nweb_ui:\n  enabled: true\n  user_ui_enabled: false\nlog:\n  log_level: debug\nreverse_proxy:\n  rewrite_secret: 0123456789abcdef0123456789abcdef\n  stream:\n    hls_session_ttl_secs: 120\n  hls_cache:\n    cache_path: '{}'\n", web.display(), root.join("storage").display(), root.join("backup").display(), root.join("cache").display());
        let source_text = format!("inputs:\n  - name: account-a\n    type: m3u\n    url: http://{a_addr}/catalog.m3u\n    max_connections: 8\n    aliases:\n      - name: account-b\n        url: http://{b_addr}/catalog.m3u\n        max_connections: 8\nsources:\n  - inputs: [account-a]\n    targets:\n      - name: handover\n        output:\n          - type: m3u\n        options:\n          share_live_streams:\n            hls: true\n");
        let mut users = format!("server:\n  - name: local\n    protocol: http\n    host: 127.0.0.1\n    port: '{port}'\n    timezone: UTC\n    message: Handover test\nuser:\n  - target: handover\n    credentials:\n");
        for user in ["alice", "bob", "late"] {
            write!(users, "      - username: {user}\n        password: test-password\n        proxy: reverse\n        server: local\n        status: Active\n").unwrap();
        }
        let config_path = root.join("config.yml");
        let source = root.join("source.yml");
        let proxy = root.join("api-proxy.yml");
        std::fs::write(&config_path, config).unwrap();
        std::fs::write(&source, source_text).unwrap();
        std::fs::write(&proxy, users).unwrap();
        let proceed = Arc::new(Notify::new());
        let catalog = format!("#EXTM3U\n#EXTINF:-1 tvg-id=\"handover\" group-title=\"Test\",Handover Channel\nhttp://{a_addr}/live/index.m3u8\n");
        let state = |catalog: String| Origin {
            catalog,
            manifests: Arc::new(AtomicUsize::new(0)),
            segments: Arc::new(AtomicUsize::new(0)),
            proceed: proceed.clone(),
        };
        let a_state = state(catalog.clone());
        let b_state = state(catalog.replace(&a_addr, &b_addr.to_string()));
        let mut origins = Vec::new();
        for (listener, state) in [(a, a_state), (b, b_state.clone())] {
            origins.push(tokio::spawn(async move {
                axum::serve(listener, Router::new().fallback(origin).with_state(state)).await.unwrap();
            }));
        }
        let log = std::fs::File::create(root.join("sut.log")).unwrap();
        let process = tokio::process::Command::new(env!("CARGO_BIN_EXE_tuliprox"))
            .args(["-s", "-c"])
            .arg(config_path)
            .arg("-i")
            .arg(&source)
            .arg("-a")
            .arg(proxy)
            .current_dir(root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            directory,
            base: format!("http://127.0.0.1:{port}"),
            source,
            process,
            origins,
            b: b_state,
            proceed,
            control: format!("http://{a_addr}/__continue"),
        }
    }

    async fn playback(&mut self, client: &reqwest::Client, username: &str) -> String {
        let url = format!("{}/m3u?username={username}&password=test-password", self.base);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                self.process.try_wait().unwrap().is_none(),
                "SUT exited: {}",
                std::fs::read_to_string(self.directory.path().join("sut.log")).unwrap()
            );
            if let Ok(response) = client.get(&url).send().await {
                if let Ok(body) = response.text().await {
                    if let Some(url) = body.lines().find(|line| line.starts_with("http")) {
                        return url.to_string();
                    }
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "playlist unavailable: {}",
                std::fs::read_to_string(self.directory.path().join("sut.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_browser_handover(&self, viewer_clients: &[reqwest::Client], viewer_media: &[reqwest::Url]) {
        eprintln!(
            "BROWSER_READY web={} control={} fixture={}",
            self.base,
            self.control,
            self.directory.path().display()
        );
        // Keep the two manual test players running while the browser loads. Stop all
        // media requests before publishing B; the browser must then update from events alone.
        let mut playback = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = self.proceed.notified() => break,
                _ = playback.tick() => {
                    for (client, media) in viewer_clients.iter().zip(viewer_media) {
                        let response = client.get(media.clone()).send().await.unwrap();
                        assert!(response.status().is_success());
                        response.bytes().await.unwrap();
                    }
                }
            }
        }
    }
}

async fn socket(base: &str) -> Socket {
    let (mut socket, _) = connect_async(format!("{}/ws", base.replace("http://", "ws://"))).await.unwrap();
    socket.send(Message::Binary(ProtocolMessage::Version(PROTOCOL_VERSION).to_bytes().unwrap())).await.unwrap();
    assert!(
        matches!(socket.next().await.unwrap().unwrap(), Message::Binary(bytes) if bytes.as_ref() == [PROTOCOL_VERSION])
    );
    socket
}

async fn frame(socket: &mut Socket) -> ProtocolMessage {
    loop {
        let next = tokio::time::timeout(Duration::from_secs(15), socket.next())
            .await
            .expect("WebUI frame arrives without polling")
            .unwrap()
            .unwrap();
        if let Message::Binary(bytes) = next {
            return ProtocolMessage::from_bytes(bytes).unwrap();
        }
    }
}

async fn snapshot(socket: &mut Socket) -> Vec<StreamInfo> {
    socket.send(Message::Binary(ProtocolMessage::StatusRequest(String::new()).to_bytes().unwrap())).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let ProtocolMessage::StatusResponse(status) =
            tokio::time::timeout_at(deadline, frame(socket)).await.expect("full status snapshot arrives")
        {
            return status.active_user_streams;
        }
    }
}

async fn changed_viewers(socket: &mut Socket, before: &[StreamInfo]) {
    let mut seen = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while seen.len() < before.len() {
        match tokio::time::timeout_at(deadline, frame(socket)).await.expect("all viewers receive Updated frames") {
            ProtocolMessage::ActiveUserResponse(ActiveUserConnectionChange::Updated(row))
                if row.provider.as_ref() == "account-b" =>
            {
                let mut expected = before
                    .iter()
                    .find(|old| old.uid == row.uid && old.addr == row.addr)
                    .expect("same viewer row")
                    .clone();
                expected.provider = Arc::from("account-b");
                assert_eq!(row, expected, "handover changes only the displayed provider");
                seen.insert(row.uid);
            }
            ProtocolMessage::StatusResponse(_) => panic!("handover must arrive as incremental Updated frames"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn hls_origin_account_handover() {
    let mut fixture = Fixture::start().await;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
    let mut canonical = Vec::new();
    let mut viewer_clients = Vec::new();
    let mut viewer_media = Vec::new();
    for viewer in ["alice", "bob"] {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
        let mut entry = fixture.playback(&client, viewer).await;
        let mut manifest = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            let response = client.get(&entry).send().await.unwrap();
            if response.status().is_success() {
                let base = response.url().clone();
                let body = response.text().await.unwrap();
                let segments = body
                    .lines()
                    .filter(|line| !line.starts_with('#') && !line.is_empty())
                    .map(|line| base.join(line).unwrap())
                    .collect::<Vec<_>>();
                if let Some(segment) = segments.first() {
                    if body.contains("#EXT-X-STREAM-INF:") {
                        entry = segment.to_string();
                        continue;
                    }
                    let media = client.get(segment.clone()).send().await.unwrap();
                    assert!(media.status().is_success());
                    assert!(
                        !media.bytes().await.unwrap().is_empty(),
                        "viewer actually consumes media before the segment gap"
                    );
                    viewer_media.push(segment.clone());
                    manifest = Some(base);
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
        canonical.push(manifest.expect("viewer has a canonical playable manifest"));
        viewer_clients.push(client);
    }
    let mut ui_a = socket(&fixture.base).await;
    let mut ui_b = socket(&fixture.base).await;
    let mut before = snapshot(&mut ui_a).await;
    assert_eq!(
        before.len(),
        2,
        "canonical={canonical:?} SUT log: {}",
        std::fs::read_to_string(fixture.directory.path().join("sut.log")).unwrap()
    );
    assert!(before.iter().all(|row| row.provider.as_ref() == "account-a"));
    let mut before_b = snapshot(&mut ui_b).await;
    assert_eq!(before_b.len(), 2);
    let manual = std::env::var_os("TULIPROX_HANDOVER_BROWSER_CHECK").is_some();
    if manual {
        fixture.wait_for_browser_handover(&viewer_clients, &viewer_media).await;
        // Playback above may advance timestamps; capture the exact pre-publication rows.
        before = snapshot(&mut ui_a).await;
        before_b = snapshot(&mut ui_b).await;
        assert_eq!(before.len(), 2);
        assert_eq!(before_b.len(), 2);
    }
    let mut sources: SourcesConfigDto =
        serde_saphyr::from_str(&std::fs::read_to_string(&fixture.source).unwrap()).unwrap();
    sources.inputs[0].account_disabled = true;
    let config = client.get(format!("{}/api/v1/config", fixture.base)).send().await.unwrap();
    let revision =
        config.headers().get(shared::utils::HEADER_CONFIG_SOURCES_REVISION).expect("source revision").clone();
    let response = client
        .post(format!("{}/api/v1/config/sources", fixture.base))
        .header(reqwest::header::IF_MATCH, revision)
        .json(&sources)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "source reload: {}", response.text().await.unwrap());
    // Only a manifest refresh triggers the handover; neither viewer requests another segment or status.
    let response = client.get(canonical[0].clone()).send().await.unwrap();
    assert!(response.status().is_success() || response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE);
    changed_viewers(&mut ui_a, &before).await;
    changed_viewers(&mut ui_b, &before_b).await;
    eprintln!(
        "HANDOVER_EVENTS_DELIVERED replacement_manifest_requests={} replacement_segment_requests={}",
        fixture.b.manifests.load(Ordering::Acquire),
        fixture.b.segments.load(Ordering::Acquire)
    );
    let after = snapshot(&mut ui_a).await;
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|row| row.provider.as_ref() == "account-b"));
    if manual {
        eprintln!("BROWSER_SWITCHED web={} control={}", fixture.base, fixture.control);
        fixture.proceed.notified().await;
    }
    fixture.process.kill().await.unwrap();
}
