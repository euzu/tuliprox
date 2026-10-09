use super::{
    app_config_with_listener, counting_ffmpeg, download_file, ensure_recording_worker_running, http_transfer_path,
    read_request, scheduled_task, serve_range_fixture, spawn_count, DownloadExecutionResult, DISK_GONE_BEFORE_START,
};
use crate::recording::{
    recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
    recording_queue::{RecordingControl, RecordingQueue},
};
use shared::model::{NoopSink, RecordingKind, RecordingTaskState};
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::sync::{Notify, RwLock};
use tuliprox_core::model::RecordingConfig;

#[tokio::test]
async fn ignored_range_preserves_vod_and_series_partials() {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let dir = TempDir::new().expect("recording directory");
        let (url, server) = serve_range_fixture(true);
        let mut task = scheduled_task(kind, chrono::Utc::now().timestamp(), 900);
        task.file_dir = dir.path().to_path_buf();
        task.file_path = dir.path().join("recording.mp4");
        task.url = url;
        task.total_size = Some(10);
        let partial = http_transfer_path(&task);
        tokio::fs::write(&partial, b"0123").await.expect("saved partial");
        let result = download_file::<NoopSink>(
            Arc::new(RwLock::new(vec![task.clone()])),
            task.clone(),
            &reqwest::Client::new(),
            None,
            Arc::new(RwLock::new(RecordingControl::None)),
            Arc::new(Notify::new()),
            None,
            None,
        )
        .await;
        assert!(
            matches!(result, DownloadExecutionResult::Failed(error) if error == super::super::RANGE_UNSUPPORTED_ERROR)
        );
        assert_eq!(tokio::fs::read(&partial).await.expect("partial preserved"), b"0123");
        assert!(!task.file_path.exists());
        assert!(server.join().expect("fixture server").to_ascii_lowercase().contains("range: bytes=4-\r\n"));
    }
}

#[tokio::test]
async fn recording_redirects_keep_the_proxy_and_resume_headers() -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_url = reqwest::Url::parse(&format!("http://{}/capture", listener.local_addr()?))?;
    let local_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let request = read_request(&mut socket).await?;
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://recording-origin.invalid/start\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        Ok::<_, std::io::Error>(request)
    });
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let proxy_url = format!("http://{}", proxy.local_addr()?);
    let proxy_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /finish\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 302 Found\r\nLocation: http://recording-cdn.invalid/end\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 6\r\nConnection: close\r\n\r\n456789",
        ] {
            let (mut socket, _) = proxy.accept().await?;
            let request = read_request(&mut socket).await?;
            socket.write_all(response.as_bytes()).await?;
            requests.push(request);
        }
        Ok::<_, std::io::Error>(requests)
    });
    let config = app_config_with_listener();
    let mut updated = (*config.config.load_full()).clone();
    updated.proxy = Some(tuliprox_core::model::ProxyConfig { url: proxy_url, username: None, password: None });
    config.config.store(Arc::new(updated));
    let mut sensitive = reqwest::header::HeaderMap::new();
    sensitive
        .insert(reqwest::header::AUTHORIZATION, reqwest::header::HeaderValue::from_static("Bearer listener-secret"));
    sensitive.insert(reqwest::header::COOKIE, reqwest::header::HeaderValue::from_static("session=listener-secret"));
    let local = tuliprox_core::utils::request::create_client(&config)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(sensitive)
        .build()?;
    let recording_headers = std::collections::HashMap::from([
        ("Authorization".to_string(), "Bearer recording-secret".to_string()),
        ("Cookie".to_string(), "session=recording-secret".to_string()),
        ("X-Recording-Test".to_string(), "custom".to_string()),
    ]);
    let upstream = tuliprox_core::utils::request::create_client(&config)
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(tuliprox_core::utils::request::get_request_headers(Some(&recording_headers), None, None, None))
        .build()?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        super::super::send_download_request(
            &local,
            Some(&upstream),
            &local_url,
            4,
            &RwLock::new(RecordingControl::None),
            &Notify::new(),
        ),
    )
    .await?
    .map_err(|result| format!("request failed: {result:?}"))?;
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await?.as_ref(), b"456789");
    let local_request = local_task.await??;
    let upstream_requests = proxy_task.await??;
    assert!(local_request.starts_with("get /capture "));
    assert!(upstream_requests[0].starts_with("get http://recording-origin.invalid/start "));
    assert!(upstream_requests[1].starts_with("get http://recording-origin.invalid/finish "));
    assert!(local_request.contains("authorization: bearer listener-secret"), "{local_request}");
    assert!(local_request.contains("cookie: session=listener-secret"), "{local_request}");
    assert!(!local_request.contains("recording-secret"), "{local_request}");
    assert!(upstream_requests[2].starts_with("get http://recording-cdn.invalid/end "));
    for (index, request) in upstream_requests.iter().enumerate() {
        assert!(!request.contains("listener-secret"), "{request}");
        if index < 2 {
            assert!(request.contains("authorization: bearer recording-secret"), "{request}");
            assert!(request.contains("cookie: session=recording-secret"), "{request}");
        } else {
            assert!(!request.contains("recording-secret"), "{request}");
        }
        assert!(request.contains("x-recording-test: custom"), "{request}");
    }
    for request in std::iter::once(&local_request).chain(&upstream_requests) {
        assert!(request.contains("range: bytes=4-"), "{request}");
        assert!(request.contains("accept-encoding: identity"), "{request}");
        assert!(request.contains(&shared::model::RECORDING_STREAM_USER_AGENT.to_ascii_lowercase()), "{request}");
    }
    Ok(())
}

#[tokio::test]
async fn recording_redirects_without_upstream_client_strip_listener_credentials(
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_url = reqwest::Url::parse(&format!("http://{}/capture", listener.local_addr()?))?;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        upstream.local_addr()?
    );
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (listener, response) in [
            (listener, redirect),
            (upstream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()),
        ] {
            let (mut socket, _) = listener.accept().await?;
            let request = read_request(&mut socket).await?;
            socket.write_all(response.as_bytes()).await?;
            requests.push(request);
        }
        Ok::<_, std::io::Error>(requests)
    });
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, reqwest::header::HeaderValue::from_static("Bearer listener-secret"));
    headers.insert(reqwest::header::COOKIE, reqwest::header::HeaderValue::from_static("session=listener-secret"));
    headers.insert("x-recording-test", reqwest::header::HeaderValue::from_static("custom"));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(headers)
        .build()?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        super::super::send_download_request(
            &client,
            None,
            &local_url,
            0,
            &RwLock::new(RecordingControl::None),
            &Notify::new(),
        ),
    )
    .await?
    .map_err(|result| format!("request failed: {result:?}"))?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let requests = server.await??;
    assert!(requests[0].contains("authorization: bearer listener-secret"), "{}", requests[0]);
    assert!(requests[0].contains("cookie: session=listener-secret"), "{}", requests[0]);
    assert!(!requests[1].contains("listener-secret"), "{}", requests[1]);
    assert!(requests[1].contains("x-recording-test: custom"), "{}", requests[1]);
    Ok(())
}

#[tokio::test]
async fn a_recording_whose_disk_filled_while_it_waited_never_opens_a_destination() {
    // Admission happened when the request was accepted; this recording
    // then waited. By the time it reaches the front of the queue the
    // space it was admitted against is gone. Without the recheck the
    // first sign of that is a write failure part-way through.
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
    capture.recording.source.virtual_id = "42".to_string();
    capture.recording.reserved_bytes = 1_024;
    capture.file_dir.clone_from(&recordings_dir);
    capture.file_path = recordings_dir.join("capture.ts");
    let persisted = RecordingQueue::to_persisted(&capture);
    crate::recording::recording_queue::mutate(&queue, move |candidate| {
        candidate.queue.push(persisted.clone());
        Ok(())
    })
    .await
    .expect("seed");

    // The safety margin stands in for a disk that filled up: it drives
    // headroom to zero without needing a real full filesystem.
    let app_config = app_config_with_listener();
    let mut rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
    rec_cfg.directory = recordings_dir.to_string_lossy().into_owned();
    rec_cfg.disk = Some(tuliprox_core::model::RecordingDiskConfig {
        high_water_percent: None,
        low_water_percent: None,
        cleanup_interval_secs: None,
        safety_bytes: Some(u64::MAX),
    });
    let mut config = tuliprox_core::model::Config::clone(&app_config.config.load());
    config.video = Some(tuliprox_core::model::VideoConfig {
        extensions: Vec::new(),
        web_search: None,
        recording: Some(rec_cfg.clone()),
    });
    app_config.config.store(Arc::new(config));

    let stub = StubCapacity::with_room();
    let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
    ensure_recording_worker_running(&app_config, &rec_cfg, &queue, &NoopSink, &capacity, &script)
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
    let Ok(settled) = settled else {
        panic!(
            "never settled: active={:?} spawns={}",
            queue.active.read().await.first().map(|active| (active.state, active.error.clone())),
            spawn_count(&log),
        );
    };

    assert_eq!(settled.state, RecordingTaskState::Failed, "{:?}", settled.error);
    assert_eq!(settled.error.as_deref(), Some(DISK_GONE_BEFORE_START), "and it says which resource ran out");
    assert_eq!(spawn_count(&log), 0, "the encoder was never started");
    assert!(!capture.file_path.exists(), "and no destination was opened");
    assert_eq!(stub.release_count(), 1, "the provider slot it had was given back");
    assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
}
