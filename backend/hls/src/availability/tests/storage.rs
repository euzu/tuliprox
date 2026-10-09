use super::*;

#[tokio::test(start_paused = true)]
async fn persistent_conflict_real_owner_commits_before_exclusive_deadline() {
    let fixture = post_refresh_terminal_fixture_with_progress("real-owner-terminal", true, false).await;
    register_real_post_refresh_owner(
        &fixture,
        super::super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
    )
    .await;
    assert_availability_owner_registered(&fixture);

    tokio::time::advance(Duration::from_millis(2_100)).await;
    assert_eq!(
        fixture.ctx.hls_proxy.availability_reevaluations().owner_count(),
        1,
        "the real owner must survive its rapid evaluation budget while reserve remains"
    );
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        20_100,
        fixture.now_ms,
    )
    .await;
    wait_for_availability_owner_completion(&fixture).await;

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal lease remains stored");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)));
}
