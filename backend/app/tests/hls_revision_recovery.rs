//! Real Tuliprox process + a streaming origin. No application HTTP client is stubbed.
use std::{
    fmt::Write as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::watch,
};

const BODY: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

// A configured Postman member is shared between the process scenarios.
static SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    directory: tempfile::TempDir,
    base: String,
    process: tokio::process::Child,
    origin: tokio::task::JoinHandle<()>,
    release: watch::Sender<bool>,
    release_retry: watch::Sender<bool>,
    attempts: Arc<AtomicUsize>,
    _scenario: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        self.origin.abort();
    }
}

impl Fixture {
    #[allow(clippy::too_many_lines, reason = "Keeps the isolated process and streaming origin fixture together.")]
    async fn start() -> Self {
        let scenario = SCENARIO.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_base = format!("http://{}", listener.local_addr().unwrap());
        let manifest_base =
            std::env::var("TULIPROX_HLS_RECOVERY_MANIFEST_BASE").unwrap_or_else(|_| origin_base.clone());
        if manifest_base != origin_base {
            let response = reqwest::Client::new()
                .post(format!("{manifest_base}/configure"))
                .json(&serde_json::json!({"segmentOrigin": origin_base}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 204);
        }
        let catalog = format!("#EXTM3U\n#EXTINF:-1 tvg-id=\"recovery\" group-title=\"Test\",Recovery Channel\n{manifest_base}/index.m3u8\n");
        let mut manifest = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n".to_string();
        for seq in 100..106 {
            writeln!(manifest, "#EXTINF:4,\n{origin_base}/{seq}.ts").unwrap();
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let (release, released) = watch::channel(false);
        let (release_retry, retry_released) = watch::channel(false);
        let counted = attempts.clone();
        let origin = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let catalog = catalog.clone(); let manifest = manifest.clone();
                        let attempts = counted.clone(); let mut released = released.clone();
                        let mut retry_released = retry_released.clone();
                        children.spawn(async move {
                            let mut reader = BufReader::new(socket);
                            let mut request = String::new();
                            reader.read_line(&mut request).await.unwrap();
                            let path = request.split_whitespace().nth(1).unwrap().to_owned();
                            loop {
                                let mut line = String::new(); reader.read_line(&mut line).await.unwrap();
                                if line == "\r\n" || line.is_empty() { break; }
                            }
                            let mut socket = reader.into_inner();
                            if path == "/catalog.m3u" || path == "/index.m3u8" {
                                let body = if path == "/index.m3u8" { manifest } else { catalog };
                                let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/vnd.apple.mpegurl\r\nConnection: close\r\n\r\n", body.len());
                                socket.write_all(headers.as_bytes()).await.unwrap();
                                socket.write_all(body.as_bytes()).await.unwrap();
                                return;
                            }
                            // The accelerated entry is 103,104,105. Inject the
                            // failure into its first visible successor.
                            let failed_attempt = path == "/104.ts" && attempts.fetch_add(1, Ordering::AcqRel) == 0;
                            let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n", BODY.len());
                            socket.write_all(headers.as_bytes()).await.unwrap();
                            if failed_attempt {
                                socket.write_all(b"stale-prefix").await.unwrap();
                                // Hold an incomplete 200 open beyond the configured body idle timeout.
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                return;
                            }
                            if path == "/104.ts" {
                                while !*retry_released.borrow_and_update() {
                                    if retry_released.changed().await.is_err() { return; }
                                }
                            }
                            socket.write_all(&BODY[..188]).await.unwrap();
                            while !*released.borrow_and_update() {
                                if released.changed().await.is_err() { return; }
                            }
                            let _ = socket.write_all(&BODY[188..]).await;
                        });
                    }
                    _ = children.join_next(), if !children.is_empty() => {}
                }
            }
        });
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["web", "storage", "backup"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let config = format!("api:\n  host: 127.0.0.1\n  port: {port}\n  web_root: '{}'\nworking_dir: '{}'\nbackup_dir: '{}'\nupdate_on_boot: true\nweb_ui:\n  enabled: false\nlog:\n  log_level: debug\nreverse_proxy:\n  rewrite_secret: 0123456789abcdef0123456789abcdef\n  stream:\n    hls_session_ttl_secs: 120\n  hls_cache:\n    cache_path: '{}'\n    origin_segment_timeout_ms: 300\n    max_concurrent_segment_fetches_per_session: 3\n    initial_manifest_wait_timeout_secs: 3\n    startup:\n      mode: optimistic\n    segment_repair:\n      max_level: off\n", root.join("web").display(), root.join("storage").display(), root.join("backup").display(), root.join("cache").display());
        let sources = format!("inputs:\n  - name: recovery-origin\n    type: m3u\n    url: {origin_base}/catalog.m3u\n    max_connections: 8\nsources:\n  - inputs: [recovery-origin]\n    targets:\n      - name: recovery\n        output:\n          - type: m3u\n        options:\n          share_live_streams:\n            hls: true\n");
        let users = format!("server:\n  - name: local\n    protocol: http\n    host: 127.0.0.1\n    port: '{port}'\n    timezone: UTC\n    message: Recovery test\nuser:\n  - target: recovery\n    credentials:\n      - username: viewer\n        password: test-password\n        proxy: reverse\n        server: local\n        status: Active\n        max_connections: 1\n");
        for (name, body) in [("config.yml", config), ("source.yml", sources), ("api-proxy.yml", users)] {
            std::fs::write(root.join(name), body).unwrap();
        }
        let log = std::fs::File::create(root.join("sut.log")).unwrap();
        let process = tokio::process::Command::new(env!("CARGO_BIN_EXE_tuliprox"))
            .args(["-s", "-c", "config.yml", "-i", "source.yml", "-a", "api-proxy.yml"])
            .current_dir(root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            directory,
            base: format!("http://127.0.0.1:{port}"),
            process,
            origin,
            release,
            release_retry,
            attempts,
            _scenario: scenario,
        }
    }

    fn log(&self) -> String { std::fs::read_to_string(self.directory.path().join("sut.log")).unwrap() }

    async fn playback(&mut self, client: &reqwest::Client) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(self.process.try_wait().unwrap().is_none(), "SUT exited: {}", self.log());
            if let Ok(response) =
                client.get(format!("{}/m3u?username=viewer&password=test-password", self.base)).send().await
            {
                if let Ok(body) = response.text().await {
                    if let Some(url) = body.lines().find(|line| line.starts_with("http")) {
                        return url.to_owned();
                    }
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "catalog unavailable: {}", self.log());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn resolve_media(base: &str, body: &str, index: usize) -> String {
    let uri = body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).nth(index).expect("media URI");
    reqwest::Url::parse(base).unwrap().join(uri).unwrap().to_string()
}

#[tokio::test]
async fn incomplete_origin_200_recovers_same_segment_uri_without_a_playlist_refresh() {
    let mut fixture = Fixture::start().await;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let entry = fixture.playback(&client).await;
    let master = client.get(&entry).send().await.unwrap();
    assert!(master.status().is_success(), "entry: {}", fixture.log());
    let master_body = master.text().await.unwrap();
    let variant = resolve_media(&entry, &master_body, 0);
    let playlist = client.get(&variant).send().await.unwrap();
    assert_eq!(playlist.status(), 200, "startup: {}", fixture.log());
    let playlist = playlist.text().await.unwrap();
    assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:3\n"), "entry follows the current live edge");
    let segment = resolve_media(&variant, &playlist, 1);
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 1);
    // Exercise unexposed successor recovery after consuming this lease's one
    // progressive request. A successor selected as the first GET may now pipe
    // its prefix and therefore cannot silently switch bytes after a body drop.
    let head = client.get(resolve_media(&variant, &playlist, 0)).send().await.unwrap();
    assert_eq!(head.status(), 200, "head must stream before EOF: {}", fixture.log());
    fixture.release.send_replace(true);
    assert_eq!(head.bytes().await.unwrap().as_ref(), BODY);
    let waiting = client.get(&segment).send().await.unwrap();
    assert_eq!(waiting.status(), 503, "retry must be temporary: {}", fixture.log());
    assert!(waiting.headers().contains_key("retry-after"));
    // Make the pending response independent of the origin retry's timing.
    fixture.release_retry.send_replace(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = client.get(&segment).send().await.unwrap();
        if response.status() == 200 {
            assert_eq!(response.bytes().await.unwrap().as_ref(), BODY);
            break;
        }
        assert_eq!(response.status(), 503, "same URI must not become 404 during recovery: {}", fixture.log());
        assert!(tokio::time::Instant::now() < deadline, "retry never recovered: {}", fixture.log());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(fixture.attempts.load(Ordering::Acquire) >= 2);
    let refreshed = client.get(&variant).send().await.unwrap();
    assert_eq!(refreshed.status(), 200, "recovered binding must not conflict: {}", fixture.log());
    assert!(refreshed.text().await.unwrap().contains(reqwest::Url::parse(&segment).unwrap().path()));
    for _ in 0..3 {
        let replay = client.get(&segment).send().await.unwrap();
        assert_eq!(replay.status(), 200);
        assert_eq!(replay.bytes().await.unwrap().as_ref(), BODY);
    }
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 2, "repaired GETs reuse the committed revision");
    assert!(fixture.log().contains("outcome=replacement_bound"));
    // Retain run evidence only when requested; test configuration and credentials are synthetic.
    if let Ok(destination) = std::env::var("TULIPROX_HLS_RECOVERY_LOG") {
        std::fs::copy(fixture.directory.path().join("sut.log"), PathBuf::from(destination)).unwrap();
    }
    fixture.process.kill().await.unwrap();
}

#[tokio::test]
async fn first_requested_successor_streams_before_origin_eof() {
    let mut fixture = Fixture::start().await;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let entry = fixture.playback(&client).await;
    let master = client.get(&entry).send().await.unwrap().text().await.unwrap();
    let variant = resolve_media(&entry, &master, 0);
    let playlist = client.get(&variant).send().await.unwrap();
    assert_eq!(playlist.status(), 200, "startup: {}", fixture.log());
    let playlist = playlist.text().await.unwrap();
    // Skip both the playlist head and the deliberately failing first successor.
    let segment = resolve_media(&variant, &playlist, 2);
    let mut response = tokio::time::timeout(Duration::from_secs(2), client.get(&segment).send())
        .await
        .expect("first successor GET must not wait for origin EOF")
        .unwrap();
    assert_eq!(response.status(), 200, "successor: {}", fixture.log());
    assert!(!response.headers().contains_key("content-length"));
    let prefix = response.chunk().await.unwrap().expect("progressive prefix");
    assert_eq!(prefix.as_ref(), &BODY[..188]);
    assert!(!*fixture.release.borrow(), "origin EOF is still held");
    fixture.release.send_replace(true);
    fixture.release_retry.send_replace(true);
    let mut bytes = prefix.to_vec();
    while let Some(chunk) = response.chunk().await.unwrap() {
        bytes.extend_from_slice(&chunk);
    }
    assert_eq!(bytes, BODY);
    let replay = client.get(&segment).send().await.unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.bytes().await.unwrap().as_ref(), BODY);
    if let Ok(destination) = std::env::var("TULIPROX_HLS_FIRST_REQUEST_LOG") {
        std::fs::copy(fixture.directory.path().join("sut.log"), PathBuf::from(destination)).unwrap();
    }
    fixture.process.kill().await.unwrap();
}

#[tokio::test]
async fn exposed_failure_allows_a_new_player_with_user_connection_limit_one() {
    let mut fixture = Fixture::start().await;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let entry = fixture.playback(&client).await;
    let master = client.get(&entry).send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
    let variant = resolve_media(&entry, &master, 0);
    let playlist = client.get(&variant).send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
    let segment = resolve_media(&variant, &playlist, 1);
    // The first full GET deliberately selects the successor, exercising the
    // integrated piping policy and exposing this attempt's prefix.
    let exposed = client.get(&segment).send().await.unwrap();
    assert_eq!(exposed.status(), 200, "first successor GET must pipe: {}", fixture.log());
    fixture.release.send_replace(true);
    assert!(exposed.bytes().await.is_err(), "exposed truncated body must not become successful EOF");
    let ended = client.get(&segment).send().await.unwrap();
    assert_eq!(ended.status(), 404, "old lease must end: {}", fixture.log());
    fixture.release_retry.send_replace(true);
    let new_player =
        reqwest::Client::builder().user_agent("new-reentry-player").timeout(Duration::from_secs(10)).build().unwrap();
    let master = new_player.get(&entry).send().await.unwrap();
    assert_eq!(master.status(), 200, "counted user reservation blocked reentry: {}", fixture.log());
    let body = master.text().await.unwrap();
    assert!(body.contains("#EXT-X-STREAM-INF"), "new entry must reach live playback: {body}");
    let new_variant = resolve_media(&entry, &body, 0);
    assert_ne!(new_variant, variant);
    let previous_session = reqwest::Url::parse(&variant).unwrap().path_segments().unwrap().nth(3).unwrap().to_owned();
    let current_session =
        reqwest::Url::parse(&new_variant).unwrap().path_segments().unwrap().nth(3).unwrap().to_owned();
    assert_eq!(previous_session, current_session, "the shared origin session stays alive");
    // Reentry releases the user slot before the origin retry finishes. The
    // pending lease may still wait for that retry's publishable media.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let playlist = loop {
        let response = new_player.get(&new_variant).send().await.unwrap();
        if response.status() == 200 {
            break response;
        }
        // Before its first publication, a lease can use the unavailable-channel
        // fallback (404 without a configured custom stream) or a temporary 503.
        assert!(
            matches!(response.status().as_u16(), 404 | 503),
            "new live playlist rejected with {}: {}",
            response.status(),
            fixture.log()
        );
        assert!(tokio::time::Instant::now() < deadline, "new live playlist never recovered: {}", fixture.log());
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let new_segment = resolve_media(&new_variant, &playlist.text().await.unwrap(), 1);
    let repaired = new_player.get(new_segment).send().await.unwrap();
    assert_eq!(repaired.status(), 200, "new player cannot use the successful revision: {}", fixture.log());
    assert_eq!(repaired.bytes().await.unwrap().as_ref(), BODY);
    assert_eq!(client.get(&segment).send().await.unwrap().status(), 404);
    fixture.process.kill().await.unwrap();
}
