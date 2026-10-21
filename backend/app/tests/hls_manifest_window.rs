//! Real process regression against the existing local Postman hls-window fixture.
//!
//! Start its simulation with a fixed port and an NDJSON log:
//! `/Users/philippmielke/.local/bin/postman simulation run postman/simulations/hls-window.sim.yaml --port 4926 --no-history --output ndjson`
//!
//! Run through `./bin/test.sh -p tuliprox --test hls_manifest_window -- --ignored`,
//! setting `TULIPROX_HLS_WINDOW_SIMULATION_ORIGIN=http://127.0.0.1:4926/origin`
//! and `TULIPROX_HLS_WINDOW_EVIDENCE_DIR` to an absolute output directory.
//! The member serves real HLS metadata; a loopback TS origin gates new bytes.
//! Only the URL-free client protocol is suitable for sharing; keep raw logs local.
use std::{
    io::Write as _,
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

fn media_url(base: &str, playlist: &str) -> String {
    let uri = playlist.lines().find(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
    reqwest::Url::parse(base).unwrap().join(uri).unwrap().to_string()
}

fn sequence(body: &str, tag: &str) -> u64 {
    body.lines().find_map(|line| line.strip_prefix(tag)).unwrap_or("0").parse().unwrap()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the real process, streaming gate and HTTP assertions in one isolated scenario."
)]
#[ignore = "requires local Postman hls-window simulation; see module documentation"]
async fn live_window_http_republishes_same_lease_while_successor_is_gated() {
    let simulation = std::env::var("TULIPROX_HLS_WINDOW_SIMULATION_ORIGIN").expect("Postman origin URL");
    let evidence =
        std::path::PathBuf::from(std::env::var("TULIPROX_HLS_WINDOW_EVIDENCE_DIR").expect("evidence directory"));
    std::fs::create_dir_all(&evidence).unwrap();
    let mut protocol = std::fs::File::create(evidence.join("client.ndjson")).unwrap();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    for mode in ["first_ready", "progressive", "optimistic"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let segment_origin = format!("http://{}", listener.local_addr().unwrap());
        let configure = |first| {
            client
                .post(format!("{simulation}/configure"))
                .json(&serde_json::json!({"firstSequence": first, "segmentOrigin": segment_origin}))
        };
        assert_eq!(configure(0).send().await.unwrap().status(), 204);
        let catalog = format!(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"window\" group-title=\"Test\",Window Channel\n{simulation}/index.m3u8\n"
        );
        let (release, released) = watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let origin = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let catalog = catalog.clone();
                        let mut released = released.clone();
                        let attempts = counted.clone();
                        children.spawn(async move {
                            let mut reader = BufReader::new(socket);
                            let mut request = String::new(); reader.read_line(&mut request).await.unwrap();
                            let path = request.split_whitespace().nth(1).unwrap().to_string();
                            loop {
                                let mut line = String::new(); reader.read_line(&mut line).await.unwrap();
                                if line == "\r\n" || line.is_empty() { break; }
                            }
                            let mut socket = reader.into_inner();
                            let body = if path == "/catalog.m3u" { catalog.as_bytes() } else { BODY };
                            let seq = path.strip_prefix('/').and_then(|path| path.strip_suffix(".ts"))
                                .and_then(|seq| seq.parse::<u64>().ok());
                            if seq.is_some_and(|seq| seq >= 6) {
                                attempts.fetch_add(1, Ordering::AcqRel);
                                while !*released.borrow_and_update() {
                                    if released.changed().await.is_err() { return; }
                                }
                            }
                            let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            if socket.write_all(headers.as_bytes()).await.is_ok() { let _ = socket.write_all(body).await; }
                        });
                    }
                    _ = children.join_next(), if !children.is_empty() => {}
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["web", "storage", "backup"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let bound = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = bound.local_addr().unwrap().port();
        drop(bound);
        let config = format!("api:\n  host: 127.0.0.1\n  port: {port}\n  web_root: '{}'\nworking_dir: '{}'\nbackup_dir: '{}'\nupdate_on_boot: true\nweb_ui:\n  enabled: false\nlog:\n  log_level: debug\nreverse_proxy:\n  rewrite_secret: 0123456789abcdef0123456789abcdef\n  hls_cache:\n    cache_path: '{}'\n    strip:\n      mode: segments\n      value: 0\n    origin_segment_timeout_ms: 10000\n    max_concurrent_segment_fetches_per_session: 6\n    initial_manifest_wait_timeout_secs: 3\n    startup:\n      mode: {mode}\n    segment_repair:\n      max_level: off\n", root.join("web").display(), root.join("storage").display(), root.join("backup").display(), root.join("cache").display());
        let sources = format!("inputs:\n  - name: window-origin\n    type: m3u\n    url: {segment_origin}/catalog.m3u\n    max_connections: 8\nsources:\n  - inputs: [window-origin]\n    targets:\n      - name: window\n        output:\n          - type: m3u\n        options:\n          share_live_streams:\n            hls: true\n");
        let users = format!("server:\n  - name: local\n    protocol: http\n    host: 127.0.0.1\n    port: '{port}'\n    timezone: UTC\n    message: Window test\nuser:\n  - target: window\n    credentials:\n      - username: viewer\n        password: test-password\n        proxy: reverse\n        server: local\n        status: Active\n        max_connections: 1\n");
        for (name, body) in [("config.yml", config), ("source.yml", sources), ("api-proxy.yml", users)] {
            std::fs::write(root.join(name), body).unwrap();
        }
        let log_file = evidence.join(format!("sut-{mode}.log"));
        let log = std::fs::File::create(&log_file).unwrap();
        let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_tuliprox"))
            .args(["-s", "-c", "config.yml", "-i", "source.yml", "-a", "api-proxy.yml"])
            .current_dir(root)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let entry = loop {
            assert!(
                process.try_wait().unwrap().is_none(),
                "SUT exited: {}",
                std::fs::read_to_string(&log_file).unwrap()
            );
            if let Ok(response) = client.get(format!("{base}/m3u?username=viewer&password=test-password")).send().await
            {
                if let Ok(body) = response.text().await {
                    if let Some(url) = body.lines().find(|line| line.starts_with("http")) {
                        break url.to_owned();
                    }
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "catalog startup timeout");
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let master = client.get(&entry).send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
        let variant = media_url(&entry, &master);
        let initial = client.get(&variant).send().await.unwrap();
        assert_eq!(initial.status(), 200);
        let previous = initial.text().await.unwrap();
        writeln!(protocol, "{}", serde_json::json!({"mode":mode,"phase":"initial","status":200,"mediaSequence":sequence(&previous,"#EXT-X-MEDIA-SEQUENCE:")})).unwrap();
        let segment = client.get(media_url(&variant, &previous)).send().await.unwrap();
        assert_eq!(segment.status(), 200);
        assert!(!segment.bytes().await.unwrap().is_empty());
        assert_eq!(configure(6).send().await.unwrap().status(), 204);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while attempts.load(Ordering::Acquire) == 0 {
            let response = client.get(&variant).send().await.unwrap();
            assert_eq!(response.status(), 200, "{mode}: waiting for origin refresh");
            assert_eq!(response.text().await.unwrap(), previous);
            writeln!(protocol, "{}", serde_json::json!({"mode":mode,"phase":"refresh","status":200,"mediaSequence":sequence(&previous,"#EXT-X-MEDIA-SEQUENCE:")})).unwrap();
            assert!(tokio::time::Instant::now() < deadline, "successor origin request was never made");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for poll in 0..20 {
            let response = client.get(&variant).send().await.unwrap();
            let status = response.status().as_u16();
            assert_eq!(status, 200, "{mode} held poll {poll}");
            assert!(response.headers().get("retry-after").is_none());
            assert_eq!(response.text().await.unwrap(), previous);
            writeln!(protocol, "{}", serde_json::json!({"mode":mode,"phase":"held","poll":poll,"status":status,"mediaSequence":sequence(&previous,"#EXT-X-MEDIA-SEQUENCE:"),"successorOriginRequests":attempts.load(Ordering::Acquire)})).unwrap();
        }
        let retained_responses = std::fs::read_to_string(&log_file)
            .unwrap()
            .lines()
            .filter(|line| line.contains("HLS live window retained:"))
            .count();
        assert!(retained_responses >= 20, "{mode}: held HTTP requests must exercise the retained-window branch");
        release.send(true).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let response = client.get(&variant).send().await.unwrap();
            assert_eq!(response.status(), 200);
            let next = response.text().await.unwrap();
            if sequence(&next, "#EXT-X-MEDIA-SEQUENCE:") > sequence(&previous, "#EXT-X-MEDIA-SEQUENCE:") {
                assert!(
                    sequence(&next, "#EXT-X-DISCONTINUITY-SEQUENCE:")
                        >= sequence(&previous, "#EXT-X-DISCONTINUITY-SEQUENCE:")
                );
                writeln!(protocol, "{}", serde_json::json!({"mode":mode,"phase":"advanced","status":200,"mediaSequence":sequence(&next,"#EXT-X-MEDIA-SEQUENCE:"),"successorOriginRequests":attempts.load(Ordering::Acquire),"retainedResponses":retained_responses})).unwrap();
                break;
            }
            writeln!(protocol, "{}", serde_json::json!({"mode":mode,"phase":"released","status":200,"mediaSequence":sequence(&next,"#EXT-X-MEDIA-SEQUENCE:")})).unwrap();
            assert!(tokio::time::Instant::now() < deadline, "successor did not publish");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        process.kill().await.unwrap();
        origin.abort();
    }
}
