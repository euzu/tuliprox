//! Segment fetch outcomes feed the group-member health triggers.

use super::{fetch_context, spawn_segment_server, SegmentFetchPolicy};
use crate::{
    member_health::test_support::{deliver, RecordingMemberRuntime},
    origin_account_runtime::HlsMemberUnhealthyReason,
    HlsOriginResourceFetchError, HlsSegmentFailureObject,
};
use axum::http::StatusCode;
use std::sync::Arc;

struct Fixture {
    worker: Arc<crate::HlsSegmentWorkerPool>,
    context: super::SegmentFetchContext,
    snapshot: super::super::SegmentFetchSnapshot,
    policy: SegmentFetchPolicy,
    runtime: Arc<RecordingMemberRuntime>,
    _temp_dir: tempfile::TempDir,
}

async fn fixture(with_policy: bool) -> Fixture {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;
    let snapshot = worker.next_fetch_snapshot(&context, 20, &policy).await.expect("fetch snapshot");
    let runtime = RecordingMemberRuntime::new(with_policy.then(RecordingMemberRuntime::default_policy));
    context.session.write().await.origin_account_runtime = Some(runtime.clone());
    Fixture { worker, context, snapshot, policy, runtime, _temp_dir: temp_dir }
}

async fn apply(
    fixture: &Fixture,
    result: Result<super::super::SegmentFetchCommit, HlsOriginResourceFetchError>,
    finished_at_ms: u64,
) {
    let mut completion = {
        let mut session = fixture.context.session.write().await;
        fixture.worker.apply_segment_fetch_result(
            &mut session,
            &fixture.snapshot,
            &fixture.policy,
            result,
            finished_at_ms,
        )
    };
    deliver(completion.member_unhealthy.take());
}

#[tokio::test]
async fn permanent_origin_failures_notify_segment_failure() {
    for error in [
        HlsOriginResourceFetchError::PermanentStatus(StatusCode::NOT_FOUND),
        HlsOriginResourceFetchError::PermanentStatus(StatusCode::GONE),
    ] {
        let fixture = fixture(true).await;
        mark_rendered(&fixture).await;
        let label = format!("{error:?}");
        apply(&fixture, Err(error), 21).await;
        assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::SegmentFailure], "{label}");
    }
}

#[tokio::test]
async fn retryable_failures_do_not_notify_until_the_object_is_permanently_failed() {
    for kind in [0_u8, 1, 2] {
        let error = retryable_origin_error(kind);
        let label = format!("{error:?}");
        let first_fixture = fixture(true).await;
        apply(&first_fixture, Err(error), 21).await;
        first_fixture.runtime.assert_quiet("a still-retryable failure must not switch");

        // Once earlier temporary failures already consumed the threshold, the same error becomes permanent.
        let threshold_fixture = fixture(true).await;
        {
            let mut session = threshold_fixture.context.session.write().await;
            for at in [1, 2] {
                session.record_temporary_segment_fetch_failure(
                    at,
                    HlsSegmentFailureObject::Normal {
                        proxy_seq: threshold_fixture.snapshot.proxy_seq,
                        origin_seq: threshold_fixture.snapshot.origin_seq,
                    },
                    3,
                );
            }
        }
        apply(&threshold_fixture, Err(retryable_origin_error(kind)), 21).await;
        assert_eq!(threshold_fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::SegmentFailure], "{label}");
    }
}

fn retryable_origin_error(kind: u8) -> HlsOriginResourceFetchError {
    match kind {
        0 => HlsOriginResourceFetchError::RetryableStatus(StatusCode::BAD_GATEWAY),
        1 => HlsOriginResourceFetchError::Transport("connection reset".to_string()),
        _ => HlsOriginResourceFetchError::Timeout,
    }
}

/// A 404 for a segment a client already saw is a real delivery failure.
async fn mark_rendered(fixture: &Fixture) {
    let mut session = fixture.context.session.write().await;
    if let Some(entry) = session.segments.get_mut(&fixture.snapshot.proxy_seq) {
        entry.last_rendered_at_ms = Some(10);
    }
}

#[tokio::test]
async fn missing_unpublished_head_segment_does_not_notify() {
    let fixture = fixture(true).await;
    {
        let mut session = fixture.context.session.write().await;
        session.publishable_origin_head_proxy_seq = Some(fixture.snapshot.proxy_seq);
        session.publishable_origin_tail_proxy_seq = Some(fixture.snapshot.proxy_seq + 2);
    }
    apply(&fixture, Err(HlsOriginResourceFetchError::PermanentStatus(StatusCode::NOT_FOUND)), 21).await;
    fixture.runtime.assert_quiet("a 404 at the unpublished live head is skipped, not a failure");
}

#[tokio::test]
async fn local_and_account_failures_do_not_notify() {
    let fixture = fixture(true).await;
    let capacity = HlsOriginResourceFetchError::LocalCacheCapacity {
        required_session_bytes: 10,
        required_global_bytes: 0,
        projected_write_bytes: 10,
        revision: fixture.context.segment_cache.capacity_revision(),
    };
    apply(&fixture, Err(capacity), 21).await;
    fixture.runtime.assert_quiet("");
    let fixture = self::fixture(true).await;
    apply(
        &fixture,
        Err(HlsOriginResourceFetchError::ProviderUnavailable(crate::HlsBoundAccountAcquireErrorKind::Expired)),
        21,
    )
    .await;
    fixture.runtime.assert_quiet("account allocation is the account runtime's concern");
}

#[tokio::test]
async fn single_item_sessions_do_not_notify() {
    let fixture = fixture(false).await;
    apply(&fixture, Err(HlsOriginResourceFetchError::PermanentStatus(StatusCode::NOT_FOUND)), 21).await;
    fixture.runtime.assert_quiet("");
}

#[tokio::test]
async fn stale_generation_failures_do_not_notify() {
    let fixture = fixture(true).await;
    fixture.context.session.write().await.invalidate_queued_origin_work();
    apply(&fixture, Err(HlsOriginResourceFetchError::PermanentStatus(StatusCode::NOT_FOUND)), 21).await;
    fixture.runtime.assert_quiet("work of a replaced member says nothing about the new one");
}

#[tokio::test]
async fn ready_segment_reports_progress_to_the_runtime() {
    let fixture = fixture(true).await;
    let commit = super::super::SegmentFetchCommit { origin_digest: None, content_length: 1, generation_valid: true };
    apply(&fixture, Ok(commit), 21).await;
    assert_eq!(fixture.runtime.progress(), 1);
    fixture.runtime.assert_quiet("");
}

#[tokio::test]
async fn slow_downloads_notify_slow_delivery_once_three_segments_were_measured() {
    let fixture = fixture(true).await;
    let policy = RecordingMemberRuntime::default_policy();
    {
        // Two earlier segments took twice their media duration.
        let mut session = fixture.context.session.write().await;
        assert!(!session.member_health.segment_delivered(5_000, 4_000, 8_000, policy));
        assert!(!session.member_health.segment_delivered(6_000, 4_000, 8_000, policy));
    }
    let commit = super::super::SegmentFetchCommit { origin_digest: None, content_length: 1, generation_valid: true };
    // The snapshot started at 20 ms; the segment of 4 s media needed 8 s.
    apply(&fixture, Ok(commit), 8_020).await;
    assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::SlowDelivery]);
}

#[tokio::test]
async fn live_head_past_the_tail_after_missing_segments_does_not_panic() {
    // Every segment of the origin window is missing, so the publishable head ran past the tail.
    let fixture = fixture(true).await;
    let mut session = fixture.context.session.write().await;
    session.publishable_origin_head_proxy_seq = Some(10);
    session.publishable_origin_tail_proxy_seq = Some(3);

    super::super::recompute_unpublished_live_head(&mut session);

    assert_eq!(session.publishable_origin_head_proxy_seq, Some(10));
}
