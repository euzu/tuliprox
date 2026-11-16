use super::{
    fetch_hls_origin_manifest_request, manifest_body, no_delay_policy, spawn_test_origin, test_app_config,
    test_recovery_timing_policy, test_session, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest,
    LiveHlsOriginEntry,
};
use crate::origin_telemetry::{
    OriginFetchOutcome, OriginFetchPhaseError, OriginFetchRecord, OriginResourceKind, OriginTelemetryRecorder,
};
use axum::http::HeaderMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tuliprox_core::model::{HlsManifestRecoveryBurstConfig, HlsOriginTelemetryConfig};

fn telemetry_context(
    base_url: &str,
    origin_manifest_timeout_ms: u64,
) -> (HlsOriginManifestFetchContext, tokio::sync::mpsc::Receiver<OriginFetchRecord>) {
    let (recorder, rx) = OriginTelemetryRecorder::with_channel(&HlsOriginTelemetryConfig {
        enabled: true,
        queue_capacity: 16,
        freshness_secs: 60,
        window_secs: 300,
    });
    let context = HlsOriginManifestFetchContext {
        origin_account_snapshot: None,
        attempt: None,
        app_config: test_app_config(),
        session: test_session(),
        availability_runtime: None,
        origin_entry: LiveHlsOriginEntry::parse(&format!("{base_url}/live/user/pass/12345.m3u8")).expect("entry url"),
        headers: HeaderMap::new(),
        client: reqwest::Client::builder().no_proxy().build().expect("client"),
        no_redirect_client: reqwest::Client::builder().no_proxy().build().expect("client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(origin_manifest_timeout_ms),
        acceptance_timing_seed: None,
        origin_telemetry: recorder,
    };
    (context, rx)
}

#[tokio::test]
async fn manifest_retry_yields_one_record_per_request_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        if hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
            (503, Vec::new(), "busy".to_string())
        } else {
            (200, Vec::new(), manifest_body())
        }
    }))
    .await;
    let (context, mut rx) = telemetry_context(&server.base_url, 2_000);

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("second attempt succeeds");

    assert_eq!(fetched.body, manifest_body());
    let records: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.resource_kind == OriginResourceKind::Manifest));
    assert_eq!((records[0].attempt_no, records[1].attempt_no), (0, 1));
    assert_eq!(
        (records[0].outcome, records[0].http_status, records[0].error_phase),
        (OriginFetchOutcome::HttpError, Some(503), Some(OriginFetchPhaseError::Headers))
    );
    assert_eq!((records[1].outcome, records[1].http_status), (OriginFetchOutcome::Ok, Some(200)));
    assert_eq!(records[1].bytes_read, manifest_body().len() as u64);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

async fn spawn_delayed_manifest_origin(
    body_delay: std::time::Duration,
    body: String,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind delayed origin");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(header.as_bytes()).await;
                if !body_delay.is_zero() {
                    tokio::time::sleep(body_delay).await;
                }
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), task)
}

#[tokio::test]
async fn slow_manifest_body_records_headers_before_body() {
    let body = manifest_body();
    let (base_url, task) = spawn_delayed_manifest_origin(std::time::Duration::from_millis(100), body.clone()).await;
    let (context, mut rx) = telemetry_context(&base_url, 2_000);

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("manifest fetch succeeds");

    assert_eq!(fetched.body, body);
    let records: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.outcome, OriginFetchOutcome::Ok);
    let headers_ms = record.headers_ms.expect("headers_ms");
    let body_complete_ms = record.body_complete_ms.expect("body_complete_ms");
    assert!(
        headers_ms < body_complete_ms,
        "headers_ms ({headers_ms}) must be less than body_complete_ms ({body_complete_ms})"
    );
    assert!(body_complete_ms >= 80, "body_complete_ms should reflect the 100ms delay: {body_complete_ms}");
    task.abort();
}

fn gzip(input: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(input).expect("gzip input");
    encoder.finish().expect("finish gzip")
}

async fn spawn_delayed_encoded_manifest_origin(
    body_delay: std::time::Duration,
    body: Vec<u8>,
    content_encoding: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind delayed origin");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: {content_encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                if !body_delay.is_zero() {
                    tokio::time::sleep(body_delay).await;
                }
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (format!("http://{addr}"), task)
}

#[tokio::test]
async fn gzip_manifest_delayed_body_records_first_byte_after_headers() {
    let raw_body = manifest_body();
    let compressed_body = gzip(raw_body.as_bytes());
    let (base_url, task) =
        spawn_delayed_encoded_manifest_origin(std::time::Duration::from_millis(100), compressed_body, "gzip").await;
    let (context, mut rx) = telemetry_context(&base_url, 2_000);

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("manifest fetch succeeds");

    assert_eq!(fetched.body, raw_body);
    let records: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.outcome, OriginFetchOutcome::Ok);
    let headers_ms = record.headers_ms.expect("headers_ms");
    let first_byte_ms = record.first_byte_ms.expect("first_byte_ms");
    assert!(
        first_byte_ms > headers_ms,
        "first_byte_ms ({first_byte_ms}) must be greater than headers_ms ({headers_ms})"
    );
    assert!(first_byte_ms >= 80, "first_byte_ms should reflect the 100ms delay: {first_byte_ms}");
    task.abort();
}

#[tokio::test]
async fn manifest_timeout_during_body_has_phase_body() {
    let body = manifest_body();
    // Origin delays body by 500ms, but manifest timeout is 100ms.
    let (base_url, task) = spawn_delayed_manifest_origin(std::time::Duration::from_millis(500), body).await;
    let (context, mut rx) = telemetry_context(&base_url, 100);

    let result =
        fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context)).await;
    assert!(result.is_err(), "expected timeout error");
    let records: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(records.len(), 5, "one record per attempt");
    for (attempt_no, record) in records.iter().enumerate() {
        assert_eq!(record.attempt_no as usize, attempt_no);
        assert_eq!(record.outcome, OriginFetchOutcome::Timeout);
        assert_eq!(record.error_phase, Some(OriginFetchPhaseError::Body));
        assert!(record.headers_ms.is_some(), "headers were received before body timeout");
    }
    task.abort();
}
