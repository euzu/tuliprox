use super::*;

#[test]
fn availability_refresh_waits_for_completion_instead_of_polling_in_flight() {
    for outcome in [HlsOriginRefreshTriggerOutcome::Started, HlsOriginRefreshTriggerOutcome::InFlight] {
        assert_eq!(
            availability_refresh_trigger_decision(outcome, true),
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::RefreshCompletion)
        );
    }
    assert_eq!(HlsAvailabilityAttemptSchedule::RefreshCompletion.wake_at_ms(100, 1, 2_100), Some(2_100));
}

#[test]
fn availability_refresh_waits_until_concrete_debounce_boundary() {
    let schedule = HlsAvailabilityAttemptSchedule::DebouncedUntil { retry_at_ms: 1_100 };
    assert_eq!(
        availability_refresh_trigger_decision(
            HlsOriginRefreshTriggerOutcome::DebouncedUntil { retry_at_ms: 1_100 },
            true,
        ),
        HlsAvailabilityRefreshTriggerDecision::Wait(schedule)
    );
    assert_eq!(schedule.wake_at_ms(100, 1, 2_100), Some(1_100));
    assert_eq!(
        HlsAvailabilityAttemptSchedule::DebouncedUntil { retry_at_ms: 3_000 }.wake_at_ms(100, 1, 2_100),
        Some(2_100)
    );
}

#[tokio::test(start_paused = true)]
async fn hard_failure_real_owner_commits_unavailable_before_exclusive_deadline() {
    let fixture = post_refresh_terminal_fixture_with_progress("real-owner-unavailable", false, false).await;
    register_real_post_refresh_owner(
        &fixture,
        super::super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
    )
    .await;
    assert_availability_owner_registered(&fixture);

    tokio::time::advance(Duration::from_millis(2_100)).await;
    assert_eq!(fixture.ctx.hls_proxy.availability_reevaluations().owner_count(), 1);
    // Playback evidence is relative to the clock as it stands now, not to the
    // fixture's base: the advance above really does move the scheduling clock.
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        7_000,
        fixture.ctx.hls_proxy.now_ms(),
    )
    .await;
    wait_for_availability_owner_completion(&fixture).await;

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingAsset, .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn multi_lease_fallback_keeps_pending_owner_and_commits_other_unavailable() {
    assert_multi_lease_fallback_handles_pending_and_unavailable(false).await;
}
