use super::{
    clear_scheduled_prefetch, committed_segment, encode_test_body, encrypted_fetch_context, fetch_context,
    spawn_segment_server, spawn_sequence_response_server, temp_cache_files, SegmentFetchPolicy, TestContentEncoding,
    TestOriginResponse,
};
use crate::SegmentCacheStatus;

#[tokio::test]
async fn demand_fetch_writes_cache_and_sets_ready_after_commit() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    let session = context.session.read().await;
    assert!(matches!(session.segments.get(&1).expect("segment").status, SegmentCacheStatus::Ready { .. }));
}

#[tokio::test]
async fn encrypted_segment_fetch_commits_exact_key_before_media_becomes_ready() {
    let server = spawn_sequence_response_server(vec![
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"0123456789abcdef".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media".to_vec() },
    ])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, context, segment_file) = encrypted_fetch_context(&server, &temp_dir, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    let session = context.session.read().await;
    let first = session.segments.get(&1).expect("encrypted segment");
    assert!(matches!(first.status, SegmentCacheStatus::Ready { .. }));
    let encryption = first.encryption.as_ref().expect("segment keeps key dependency");
    assert!(session
        .transient
        .ready_key_object_valid_until_ms(
            &session.proxy_session_id,
            &encryption.resource_id,
            &encryption.resource_extension,
            20,
        )
        .is_some());
    assert!(session.ready_timeline_snapshot(1, 20).units[0].required_key_ready);
    assert!(session.activity.media_readiness_generation >= 2);
    drop(session);
    let requests = server.requests.lock().await;
    assert!(requests[0].starts_with("GET /key.key "));
    assert!(requests[1].starts_with("GET /1.ts "));
}

#[tokio::test]
async fn segment_decoder_failure_retries_then_commits_and_cleans_temp_file() {
    let identity_bytes = b"identity-media-after-retry".to_vec();
    let valid = encode_test_body(&identity_bytes, TestContentEncoding::Gzip).await;
    let mut corrupt = valid.clone();
    corrupt.truncate(corrupt.len() / 2);
    let server = spawn_sequence_response_server(vec![
        TestOriginResponse::encoded("gzip", corrupt),
        TestOriginResponse::encoded("gzip", valid),
    ])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(committed_segment(&context, 1).await.1, identity_bytes);
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.to_ascii_lowercase().contains("accept-encoding: identity")));
    drop(requests);
    assert!(!context.segment_cache.has_active_temp_files());
    assert!(temp_cache_files(temp_dir.path()).is_empty());
}
