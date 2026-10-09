use super::{
    encrypted_fetch_context, fetch_context, spawn_segment_server, spawn_sequence_response_server,
    take_queued_segment_fetch_candidate, SegmentFetchPolicy, SegmentFetchPriority, TestOriginResponse,
};
use crate::{HlsSegmentFile, SegmentCacheStatus, TransientResourceKind};
use std::time::Duration;
use tuliprox_core::utils::current_time_millis;

#[tokio::test]
async fn popped_candidate_with_expired_fetch_binding_returns_to_discovered() {
    let server = spawn_sequence_response_server(vec![TestOriginResponse {
        status: 200,
        headers: Vec::new(),
        body: b"unused".to_vec(),
    }])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (_, context, _) = fetch_context(&server, &temp_dir, &policy).await;
    let mut session = context.session.write().await;
    let entry = session.segments.get_mut(&1).expect("segment");
    entry.status = SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
    entry.origin_fetch_ref.as_mut().expect("fetch binding").valid_until_ms = Some(9);

    assert!(take_queued_segment_fetch_candidate(&mut session, 1, SegmentFetchPriority::Prefetch, 10, true,).is_none());
    assert!(matches!(session.segments[&1].status, SegmentCacheStatus::Discovered));
}

#[tokio::test]
async fn expired_aes_key_is_refetched_before_a_second_segment_becomes_ready() {
    let server = spawn_sequence_response_server(vec![
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"0123456789abcdef".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media-a".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"fedcba9876543210".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media-b".to_vec() },
    ])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
    {
        let mut session = context.session.write().await;
        session.transient.set_resource_ttl_ms(1);
    }

    let first_segment = HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() };
    let first_outcome = worker.demand_fetch_and_wait(context.clone(), &first_segment, current_time_millis()).await;
    assert_eq!(first_outcome, super::super::SegmentDemandFetchOutcome::Ready);

    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let session = context.session.read().await;
        let key = session
            .transient
            .resources
            .values()
            .find(|resource| resource.kind == TransientResourceKind::Key)
            .cloned()
            .expect("encrypted fixture key resource");
        assert!(session
            .transient
            .ready_key_object_valid_until_ms(
                &session.proxy_session_id,
                &key.id,
                key.file_ext_hint.as_deref().expect("key extension"),
                current_time_millis(),
            )
            .is_none());
    }

    let second_segment = HlsSegmentFile { proxy_seq: 2, extension: "ts".to_string() };
    let second_outcome = worker.demand_fetch_and_wait(context.clone(), &second_segment, current_time_millis()).await;
    assert_eq!(second_outcome, super::super::SegmentDemandFetchOutcome::Ready);

    {
        let session = context.session.read().await;
        assert!(matches!(
            session.segments.get(&2).map(|segment| &segment.status),
            Some(SegmentCacheStatus::Ready { .. })
        ));
    }

    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 4, "requests={requests:?}");
    assert!(requests[0].starts_with("GET /key.key "));
    assert!(requests[1].starts_with("GET /1.ts "));
    assert!(requests[2].starts_with("GET /key.key "));
    assert!(requests[3].starts_with("GET /2.ts "));
}

#[tokio::test]
async fn demand_fetch_is_blocked_for_gc_marked_session() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    context.session.write().await.mark_for_gc_removal();

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::NotFound);
    assert!(server.requests.lock().await.is_empty());
}
