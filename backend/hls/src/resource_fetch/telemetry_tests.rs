//! Origin transfer telemetry observed through the shared retry loop, against a local origin.

use super::{
    run_hls_origin_resource_retry_loop_with_attempt_prepare, HlsOriginByteRangeExpectation, HlsOriginResourceClients,
    HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsResourceFetchKind, HlsResourceFetchSource,
    PostprocessSpan,
};
use crate::{
    origin_telemetry::{
        OriginFetchOutcome, OriginFetchPhaseError, OriginFetchRecord, OriginResourceKind, OriginTelemetryRecorder,
    },
    HlsLogIdentity, SegmentFetchPolicy,
};
use axum::http::HeaderMap;
use futures::{future::BoxFuture, FutureExt};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc::Receiver, Semaphore},
};
use tuliprox_core::{
    model::HlsOriginTelemetryConfig,
    utils::{content_coding::DecodedHttpResponse, network::DynReader},
};

type Commit<T> =
    Box<dyn FnMut(DecodedHttpResponse) -> BoxFuture<'static, Result<T, HlsOriginResourceFetchError>> + Send>;

fn recorder() -> (OriginTelemetryRecorder, Receiver<OriginFetchRecord>) {
    OriginTelemetryRecorder::with_channel(&HlsOriginTelemetryConfig {
        enabled: true,
        queue_capacity: 64,
        freshness_secs: 60,
        window_secs: 300,
    })
}

fn records(rx: &mut Receiver<OriginFetchRecord>) -> Vec<OriginFetchRecord> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn policy(telemetry: OriginTelemetryRecorder) -> SegmentFetchPolicy {
    SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, telemetry, ..SegmentFetchPolicy::default() }
}

fn target(address: SocketAddr) -> HlsOriginResourceFetchTarget {
    HlsOriginResourceFetchTarget {
        kind: HlsResourceFetchKind::Segment,
        source: HlsResourceFetchSource::Normal,
        object_id: "000007".to_string(),
        origin_url: format!("http://{address}/segment.ts?token=secret"),
        headers: HeaderMap::new(),
        byte_range_expectation: HlsOriginByteRangeExpectation::FullObject,
        media_duration_ms: Some(4_000),
    }
}

fn clients() -> HlsOriginResourceClients {
    HlsOriginResourceClients {
        session: None,
        retry_work: None,
        client: reqwest::Client::builder().no_proxy().build().expect("client"),
        no_redirect_client: reqwest::Client::builder().no_proxy().build().expect("client"),
        use_manual_redirects: false,
    }
}

async fn read_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        request.push(stream.read_u8().await.expect("read request"));
    }
}

/// Serves every connection with `respond(connection_index, stream)` after its request head was read.
async fn origin<F>(respond: F) -> SocketAddr
where
    F: Fn(usize, TcpStream) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        for index in 0.. {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let respond = Arc::clone(&respond);
            tokio::spawn(async move {
                read_request(&mut stream).await;
                respond(index, stream).await;
            });
        }
    });
    address
}

/// Reads the whole body, like the cache staging does.
fn read_all() -> Commit<usize> {
    Box::new(|mut response| {
        async move {
            let mut body = Vec::new();
            response
                .body
                .read_to_end(&mut body)
                .await
                .map_err(|error| HlsOriginResourceFetchError::cache_body(&error))?;
            Ok(body.len())
        }
        .boxed()
    })
}

async fn run<T: Send + 'static>(
    address: SocketAddr,
    policy: &SegmentFetchPolicy,
    mut commit: Commit<T>,
) -> Result<T, HlsOriginResourceFetchError> {
    run_with_prepare(address, policy, |_| async { Ok(()) }.boxed(), move |response| commit(response)).await
}

async fn run_with_prepare<T, G>(
    address: SocketAddr,
    policy: &SegmentFetchPolicy,
    prepare: impl FnMut(super::HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
    mut commit: impl FnMut(DecodedHttpResponse) -> BoxFuture<'static, Result<T, HlsOriginResourceFetchError>>,
) -> Result<T, HlsOriginResourceFetchError>
where
    T: Send + 'static,
    G: Send + 'static,
{
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target(address),
        clients(),
        policy,
        &HlsLogIdentity::for_test("session", "proxy"),
        prepare,
        |_guard| async {}.boxed(),
        move |response, _attempt, _deadline, _guard| commit(response),
    )
    .await
}

async fn ok_response(stream: &mut TcpStream, chunks: &[(&[u8], u64)], content_length: usize) {
    stream
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("head");
    for (chunk, delay_ms) in chunks {
        tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        stream.write_all(chunk).await.expect("chunk");
    }
}

#[tokio::test]
async fn slow_http_200_body_is_network_time_and_ok() {
    let address = origin(|_, mut stream| {
        async move { ok_response(&mut stream, &[(b"01234", 0), (b"56789", 200)], 10).await }.boxed()
    })
    .await;
    let (recorder, mut rx) = recorder();

    let result = run(address, &policy(recorder), read_all()).await;

    assert_eq!(result.expect("fetch"), 10);
    let records = records(&mut rx);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.outcome, OriginFetchOutcome::Ok);
    assert_eq!(record.error_phase, None);
    assert_eq!(record.resource_kind, OriginResourceKind::Segment);
    assert_eq!((record.http_status, record.bytes_read, record.content_length), (Some(200), 10, Some(10)));
    assert_eq!(record.media_duration_ms, Some(4_000));
    assert!(record.network_ms.expect("network") >= 190, "{record:?}");
    assert!(record.headers_ms.expect("headers") < 150, "{record:?}");
    assert!(record.first_byte_ms.is_some());
    assert!(record.segment_id.is_some());
    assert_eq!(record.input_name, "unknown");
    // No URL, token or credential material in the serialized record.
    let json = serde_json::to_string(record).expect("json");
    assert!(!json.contains("secret") && !json.contains("127.0.0.1"), "{json}");
}

#[tokio::test]
async fn short_body_is_truncated_in_the_body_phase() {
    let address =
        origin(|_, mut stream| async move { ok_response(&mut stream, &[(b"0123", 0)], 10).await }.boxed()).await;
    let (recorder, mut rx) = recorder();

    let result = run(address, &policy(recorder), read_all()).await;

    assert!(result.is_err());
    let records = records(&mut rx);
    assert_eq!(records.len(), 5, "one record per attempt");
    for (attempt_no, record) in records.iter().enumerate() {
        assert_eq!(record.attempt_no as usize, attempt_no);
        assert_eq!(record.outcome, OriginFetchOutcome::Truncated);
        assert_eq!(record.error_phase, Some(OriginFetchPhaseError::Body));
        assert_eq!((record.bytes_read, record.content_length), (4, Some(10)));
    }
}

#[tokio::test]
async fn dropping_the_fetch_mid_body_records_cancelled() {
    let address = origin(|_, mut stream| {
        async move {
            ok_response(&mut stream, &[(b"0123", 0)], 100).await;
            std::future::pending::<()>().await;
        }
        .boxed()
    })
    .await;
    let (recorder, mut rx) = recorder();
    let policy = policy(recorder);

    let cancelled = tokio::time::timeout(Duration::from_millis(400), run(address, &policy, read_all())).await;

    assert!(cancelled.is_err());
    let records = records(&mut rx);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, OriginFetchOutcome::Cancelled);
    assert_eq!(records[0].error_phase, Some(OriginFetchPhaseError::Body));
    assert_eq!(records[0].bytes_read, 4);
}

#[tokio::test]
async fn retry_then_success_yields_one_record_per_attempt_with_one_segment_id() {
    let address = origin(|index, mut stream| {
        async move {
            if index == 0 {
                stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("503");
            } else {
                ok_response(&mut stream, &[(b"0123456789", 0)], 10).await;
            }
        }
        .boxed()
    })
    .await;
    let (recorder, mut rx) = recorder();

    let result = run(address, &policy(recorder), read_all()).await;

    assert_eq!(result.expect("fetch"), 10);
    let records = records(&mut rx);
    assert_eq!(records.len(), 2);
    assert_eq!((records[0].attempt_no, records[1].attempt_no), (0, 1));
    assert_eq!(records[0].outcome, OriginFetchOutcome::HttpError);
    assert_eq!(records[0].http_status, Some(503));
    assert_eq!(records[0].error_phase, Some(OriginFetchPhaseError::Headers));
    assert_eq!(records[1].outcome, OriginFetchOutcome::Ok);
    assert_eq!(records[0].segment_id, records[1].segment_id);
    assert_ne!(records[0].attempt_id, records[1].attempt_id);
}

#[tokio::test]
async fn permanent_status_is_one_http_error_without_a_body() {
    let address = origin(|_, mut stream| {
        async move {
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("404");
        }
        .boxed()
    })
    .await;
    let (recorder, mut rx) = recorder();

    let result = run(address, &policy(recorder), read_all()).await;

    assert!(matches!(result, Err(HlsOriginResourceFetchError::PermanentStatus(_))));
    let records = records(&mut rx);
    assert_eq!(records.len(), 1);
    assert_eq!((records[0].outcome, records[0].http_status), (OriginFetchOutcome::HttpError, Some(404)));
    assert_eq!(records[0].bytes_read, 0);
}

#[tokio::test]
async fn provider_slot_wait_is_queue_wait_not_network_time() {
    let address =
        origin(|_, mut stream| async move { ok_response(&mut stream, &[(b"0123456789", 0)], 10).await }.boxed()).await;
    let (recorder, mut rx) = recorder();
    let slots = Arc::new(Semaphore::new(0));
    let release = Arc::clone(&slots);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        release.add_permits(1);
    });

    let result = run_with_prepare(
        address,
        &policy(recorder),
        move |_| {
            let slots = Arc::clone(&slots);
            async move { Ok(slots.acquire_owned().await.expect("slot")) }.boxed()
        },
        |response| read_all_of(response).boxed(),
    )
    .await;

    assert_eq!(result.expect("fetch"), 10);
    let records = records(&mut rx);
    assert_eq!(records.len(), 1);
    assert!(records[0].queue_wait_ms >= 240, "{:?}", records[0]);
    assert!(records[0].network_ms.expect("network") < 200, "{:?}", records[0]);
}

async fn read_all_of(mut response: DecodedHttpResponse) -> Result<usize, HlsOriginResourceFetchError> {
    let mut body = Vec::new();
    response.body.read_to_end(&mut body).await.map_err(|error| HlsOriginResourceFetchError::cache_body(&error))?;
    Ok(body.len())
}

#[tokio::test]
async fn cache_ready_is_reported_after_body_complete_with_cache_write_and_postprocess_spans() {
    let address =
        origin(|_, mut stream| async move { ok_response(&mut stream, &[(b"0123456789", 0)], 10).await }.boxed()).await;
    let (recorder, mut rx) = recorder();

    let result = run_with_prepare(
        address,
        &policy(recorder),
        |_| async { Ok(()) }.boxed(),
        |mut response| {
            async move {
                let mut body = Vec::new();
                response.body.read_to_end(&mut body).await.expect("body");
                // Finishing the staged write, then repair, before the object is servable.
                tokio::time::sleep(Duration::from_millis(80)).await;
                let span = PostprocessSpan::start();
                tokio::time::sleep(Duration::from_millis(120)).await;
                drop(span);
                Ok(body.len())
            }
            .boxed()
        },
    )
    .await;

    assert_eq!(result.expect("fetch"), 10);
    let records = records(&mut rx);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let (body_complete, cache_ready) = (record.body_complete_ms.expect("body"), record.cache_ready_ms.expect("ready"));
    assert!(cache_ready >= body_complete + 190, "{record:?}");
    assert!(record.network_ms.expect("network") <= body_complete);
    assert!(record.cache_write_ms.expect("cache write") >= 70, "{record:?}");
    assert!(record.postprocess_ms.expect("postprocess") >= 110, "{record:?}");
}

#[tokio::test]
async fn direct_stream_finishes_when_the_returned_body_is_read_or_dropped() {
    let address =
        origin(|_, mut stream| async move { ok_response(&mut stream, &[(b"0123456789", 0)], 10).await }.boxed()).await;
    let (recorder, mut rx) = recorder();
    let policy = policy(recorder);
    let hand_off: Commit<DynReader> = Box::new(|response| async move { Ok(response.body) }.boxed());

    let mut body = run(address, &policy, hand_off).await.expect("fetch");
    assert!(records(&mut rx).is_empty(), "the record waits for the stream");
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).await.expect("stream");
    let finished = records(&mut rx);
    assert_eq!(finished.len(), 1);
    assert_eq!((finished[0].outcome, finished[0].bytes_read), (OriginFetchOutcome::Ok, 10));
    assert_eq!(finished[0].cache_ready_ms, None);

    let hand_off: Commit<DynReader> = Box::new(|response| async move { Ok(response.body) }.boxed());
    let abandoned = run(address, &policy, hand_off).await.expect("fetch");
    drop(abandoned);
    let cancelled = records(&mut rx);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].outcome, OriginFetchOutcome::Cancelled);
}

#[tokio::test]
async fn disabled_recorder_changes_nothing_and_records_nothing() {
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&connections);
    let address = origin(move |_, mut stream| {
        counted.fetch_add(1, Ordering::Relaxed);
        async move { ok_response(&mut stream, &[(b"0123456789", 0)], 10).await }.boxed()
    })
    .await;
    let policy = policy(OriginTelemetryRecorder::disabled());

    let result = run(address, &policy, read_all()).await;

    assert_eq!(result.expect("fetch"), 10);
    assert_eq!(connections.load(Ordering::Relaxed), 1);
    assert!(policy.telemetry.snapshot().is_none());
    assert!(!policy.telemetry.is_enabled());
}
