use super::{live_media_fixture, HlsMediaActivityCommitOutcome};

#[tokio::test]
async fn contiguous_live_completion_wakes_capacity_protection_waiters() {
    let (manager, session, proxy_session_id, lease_id, live_identity) = live_media_fixture("capacity-release").await;
    let revision = manager.segment_cache.capacity_revision();
    let mut capacity_wait = Box::pin(manager.segment_cache.wait_for_capacity_change(&revision));
    assert!(matches!(futures::poll!(capacity_wait.as_mut()), std::task::Poll::Pending));
    let token = manager
        .record_access_lease_segment_request_started_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            live_identity,
            40,
            2_000,
        )
        .await
        .expect("current live request token");

    let outcome = manager
        .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
            &session,
            &lease_id,
            &proxy_session_id,
            live_identity,
            token,
            2_100,
        )
        .await;

    assert_eq!(outcome, HlsMediaActivityCommitOutcome::Committed);
    assert!(matches!(futures::poll!(capacity_wait.as_mut()), std::task::Poll::Ready(())));
}
