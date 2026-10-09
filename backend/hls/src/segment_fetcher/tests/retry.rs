use super::{
    clear_scheduled_prefetch, encode_test_body, fetch_context, normal_manifest, spawn_segment_server,
    spawn_sequence_response_server, temp_cache_files, HlsSegmentFetchWorkload, SegmentFetchContext, SegmentFetchPolicy,
    TestContentEncoding, TestOriginResponse, TestSegmentServer,
};
use crate::{HlsSegmentFile, HlsSessionKey, SegmentCacheStatus};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

#[test]
fn missing_head_is_order_independent_and_preserves_unrelated_degradation() -> std::io::Result<()> {
    for order in [[1_u64, 0], [0, 1]] {
        let mut session = crate::HlsSession::new(HlsSessionKey::new(1, "startup"), b"secret", 0);
        session.apply_origin_manifest(&normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4,\n0.ts\n#EXTINF:4,\n1.ts\n#EXTINF:4,\n2.ts\n#EXTINF:4,\n3.ts\n#EXTINF:4,\n4.ts\n"
        )).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        let head = session.publishable_origin_head_proxy_seq.ok_or_else(|| std::io::Error::other("head"))?;
        for seq in head.saturating_add(2)..=head.saturating_add(4) {
            let entry = session.segments.get_mut(&seq).ok_or_else(|| std::io::Error::other("suffix"))?;
            entry.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 0 };
        }
        session.origin_control.path_condition = crate::HlsOriginPathCondition::HardFetchFailure;
        for offset in order {
            let entry =
                session.segments.get_mut(&(head + offset)).ok_or_else(|| std::io::Error::other("head entry"))?;
            entry.status = SegmentCacheStatus::FailedPermanent {
                failed_at_ms: 1,
                status: Some(axum::http::StatusCode::NOT_FOUND),
            };
            super::super::recompute_unpublished_live_head(&mut session);
        }
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(head + 2));
        assert_eq!(session.origin_control.path_condition, crate::HlsOriginPathCondition::HardFetchFailure);
    }
    Ok(())
}

#[test]
fn segment_fetch_budget_accounts_for_serialized_key_and_media_retry_chains() {
    let policy = SegmentFetchPolicy {
        origin_segment_timeout_ms: 10_000,
        effective_repair_postprocess_timeout_ms: 2_000,
        retry_delays_ms: [0, 100, 250, 500, 750],
        retry_jitter_max_ms: 100,
        ..SegmentFetchPolicy::default()
    };

    assert_eq!(policy.demand_wait_timeout_for(HlsSegmentFetchWorkload::Clear), Duration::from_millis(63_100));
    assert_eq!(
        policy.demand_wait_timeout_for(HlsSegmentFetchWorkload::EncryptedWithKey),
        Duration::from_millis(125_200)
    );
    assert_eq!(policy.demand_wait_timeout(), Duration::from_millis(125_200));
    assert_eq!(policy.origin_object_wait_timeout(), Duration::from_millis(63_100));
}

#[test]
fn hls_recovery_timing_expected_object_eta_is_not_the_retry_chain_timeout() {
    let policy = SegmentFetchPolicy {
        origin_segment_timeout_ms: 10_000,
        effective_repair_postprocess_timeout_ms: 2_000,
        retry_delays_ms: [0, 100, 250, 500, 750],
        retry_jitter_max_ms: 100,
        ..SegmentFetchPolicy::default()
    };

    assert_eq!(policy.recovery_object_eta_ms(), 13_000);
    assert!(policy.recovery_object_eta_ms() < policy.workload_budget_ms(HlsSegmentFetchWorkload::Clear));
    assert!(policy.recovery_object_eta_ms() < policy.workload_budget_ms(HlsSegmentFetchWorkload::EncryptedWithKey));

    let long_hard_timeout = SegmentFetchPolicy {
        origin_segment_timeout_ms: 120_000,
        effective_repair_postprocess_timeout_ms: 30_000,
        ..policy
    };
    assert_eq!(long_hard_timeout.recovery_object_eta_ms(), 13_000);
    assert!(
        long_hard_timeout.recovery_object_eta_ms()
            < long_hard_timeout.workload_budget_ms(HlsSegmentFetchWorkload::Clear)
    );
}

pub(in crate::segment_fetcher::tests) async fn wait_for_concurrent_capacity_deferral(
    context: &SegmentFetchContext,
    server: &TestSegmentServer,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let both_deferred = {
                let session = context.session.read().await;
                [2_u64, 3].into_iter().all(|proxy_seq| {
                    matches!(
                        session.segments.get(&proxy_seq).map(|segment| &segment.status),
                        Some(SegmentCacheStatus::CapacityDeferred { .. })
                    )
                })
            };
            if both_deferred && server.requests.lock().await.len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both capacity failures become revision-bound deferrals");
}

pub(in crate::segment_fetcher::tests) async fn assert_segment_request_counts(
    server: &TestSegmentServer,
    expected: usize,
    failure_context: &str,
) {
    let requests = server.requests.lock().await;
    for proxy_seq in [2_u64, 3] {
        assert_eq!(
            requests.iter().filter(|request| request.starts_with(&format!("GET /{proxy_seq}.ts "))).count(),
            expected,
            "{failure_context}: segment {proxy_seq}"
        );
    }
}

pub(in crate::segment_fetcher::tests) async fn assert_concurrent_capacity_recovery_ready(
    context: &SegmentFetchContext,
) {
    let session = context.session.read().await;
    for proxy_seq in [2_u64, 3, 4] {
        assert!(matches!(
            session.segments.get(&proxy_seq).map(|segment| &segment.status),
            Some(SegmentCacheStatus::Ready { .. })
        ));
    }
    let rendered = session.last_rendered_manifest.as_ref().expect("recovered timeline renders");
    assert_eq!(rendered.first_proxy_seq, 2);
    assert_eq!(rendered.last_proxy_seq, 4);
    assert!(rendered.body.contains("000002.ts"));
    assert!(rendered.body.contains("000003.ts"));
}

#[tokio::test]
async fn concurrent_capacity_deferred_segments_retry_once_after_real_cache_release() {
    let response = TestOriginResponse { status: 200, headers: Vec::new(), body: b"12345".to_vec() };
    let server = spawn_sequence_response_server(vec![response; 4]).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;
    {
        let mut session = context.session.write().await;
        session
            .apply_origin_manifest(&normal_manifest(&format!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{0}/1.ts\n#EXTINF:4.0,\n{0}/2.ts\n#EXTINF:4.0,\n{0}/3.ts\n#EXTINF:4.0,\n{0}/4.ts\n",
                server.base_url
            )))
            .expect("four-segment manifest maps");
    }
    clear_scheduled_prefetch(&context, &policy).await;
    let (released_key, tail_key) = {
        let session = context.session.read().await;
        (
            session.segments.get(&1).expect("released segment").cache_key.clone(),
            session.segments.get(&4).expect("ready tail segment").cache_key.clone(),
        )
    };
    context
        .segment_cache
        .write_bytes_and_commit(&released_key, b"12345678901")
        .await
        .expect("released fixture commits");
    context.segment_cache.write_bytes_and_commit(&tail_key, b"t").await.expect("tail fixture commits");
    {
        let mut session = context.session.write().await;
        session.segments.get_mut(&1).expect("released segment").status =
            SegmentCacheStatus::Ready { content_length: 11, ready_at_ms: 10 };
        session.segments.get_mut(&4).expect("ready tail segment").status =
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
    }
    context.segment_cache.update_cache_limits(100, 12);

    let first_worker = Arc::clone(&worker);
    let first_context = context.clone();
    let first = tokio::spawn(async move {
        first_worker
            .demand_fetch_and_wait(first_context, &HlsSegmentFile { proxy_seq: 2, extension: "ts".to_string() }, 20)
            .await
    });
    let second_worker = Arc::clone(&worker);
    let second_context = context.clone();
    let second = tokio::spawn(async move {
        second_worker
            .demand_fetch_and_wait(second_context, &HlsSegmentFile { proxy_seq: 3, extension: "ts".to_string() }, 20)
            .await
    });

    wait_for_concurrent_capacity_deferral(&context, &server).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_segment_request_counts(&server, 1, "initial capacity deferral").await;

    for _ in 0..3 {
        context.segment_cache.notify_capacity_protection_changed();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }
    assert_segment_request_counts(&server, 1, "insufficient protection revision").await;
    {
        let session = context.session.read().await;
        assert!([2_u64, 3].into_iter().all(|proxy_seq| {
            session.segments.get(&proxy_seq).is_some_and(|segment| segment.status.awaits_capacity_recovery())
        }));
    }

    {
        let mut session = context.session.write().await;
        session.segments.get_mut(&1).expect("completed segment").status = SegmentCacheStatus::Expired;
        session.publishable_origin_head_proxy_seq = Some(2);
        session.advance_media_readiness_generation();
    }
    context.segment_cache.delete(&released_key).await.expect("completed cache object is released");

    let (first, second) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(first, second) })
        .await
        .expect("both deferred fetches resume after the accounting revision");
    assert_eq!(first.expect("first demand task joins"), super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(second.expect("second demand task joins"), super::super::SegmentDemandFetchOutcome::Ready);
    assert_segment_request_counts(&server, 2, "real capacity release").await;
    assert_concurrent_capacity_recovery_ready(&context).await;
}

#[tokio::test]
async fn failed_retryable_segment_has_a_bounded_demand_requeue_contract() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;
    context.session.write().await.segments.get_mut(&segment_file.proxy_seq).expect("segment").status =
        SegmentCacheStatus::FailedRetryable { failed_at_ms: 20, retry_after_ms: 1_000 };

    let early = worker.demand_fetch_and_wait(context.clone(), &segment_file, 1_019).await;
    assert_eq!(early, super::super::SegmentDemandFetchOutcome::TimedOut);
    assert!(server.requests.lock().await.is_empty());

    let retried = worker.demand_fetch_and_wait(context.clone(), &segment_file, 1_020).await;
    assert_eq!(retried, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.len(), 1);
}

pub(in crate::segment_fetcher::tests) async fn spawn_sequence_status_server(statuses: Vec<u16>) -> TestSegmentServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let statuses = Arc::new(Mutex::new(VecDeque::from(statuses)));
    let task_requests = Arc::clone(&requests);
    let task_statuses = Arc::clone(&statuses);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let statuses = Arc::clone(&task_statuses);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                requests.lock().await.push(request.clone());
                let status = statuses.lock().await.pop_front().unwrap_or(200);
                let reason = if status == 200 { "OK" } else { "Error" };
                let body = if status == 200 { "segment-body" } else { "" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
}

pub(in crate::segment_fetcher::tests) async fn spawn_redirect_retry_server() -> TestSegmentServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let request_count = Arc::new(AtomicUsize::new(0));
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let request_count = Arc::clone(&request_count);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                requests.lock().await.push(path.to_string());
                let count = request_count.fetch_add(1, Ordering::SeqCst);
                let response = if path == "/1.ts" && count == 0 {
                    "HTTP/1.1 302 Found\r\nLocation: /redirected.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else if path == "/redirected.ts" {
                    "HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsegment-body".to_string()
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
}

#[tokio::test]
async fn segment_decoded_object_limit_is_authoritative_and_non_retryable() {
    let identity_bytes = vec![b'x'; 512];
    let encoded = encode_test_body(&identity_bytes, TestContentEncoding::Gzip).await;
    assert!(encoded.len() < 64, "fixture must be smaller on the wire than the decoded limit");
    let server = spawn_sequence_response_server(vec![TestOriginResponse::encoded("gzip", encoded)]).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    context.segment_cache.update_cache_limits(64, 64);
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Unavailable);
    assert_eq!(server.requests.lock().await.len(), 1);
    let cache_key = context.session.read().await.segments.get(&1).expect("segment").cache_key.clone();
    assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
    assert!(!context.segment_cache.has_active_temp_files());
    assert!(temp_cache_files(temp_dir.path()).is_empty());
}

#[tokio::test]
async fn retryable_407_retries_segment_fetch_until_success() {
    let server = spawn_sequence_status_server(vec![407, 200]).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn permanent_404_does_not_retry_segment_fetch() {
    let server = spawn_sequence_status_server(vec![404, 200]).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Unavailable);
    assert_eq!(server.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn segment_retry_starts_again_at_fetch_ref_after_redirect_failure() {
    let server = spawn_redirect_retry_server().await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.as_slice(), ["/1.ts", "/redirected.ts", "/1.ts"]);
}
