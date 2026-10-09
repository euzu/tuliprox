use super::*;

#[tokio::test]
async fn post_refresh_runtime_failure_leaves_no_unowned_live_lease() {
    assert_post_refresh_registration_failure_leaves_no_unowned_live_lease(
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable,
        "post-refresh-runtime-failure",
    )
    .await;
}

#[tokio::test]
async fn different_late_reason_cannot_replace_committed_plan() {
    let fixture = post_refresh_terminal_fixture("runtime-late-reason", true).await;
    let (_, first) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::UserConnectionsExhausted, true).await;

    let (outcome, replay) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::SessionOrLeaseExpired, false).await;

    assert_eq!(outcome, HlsRuntimeCustomTailOutcome::AlreadyCommitted);
    assert_eq!(replay.reason, HlsRuntimeCustomTailReason::UserConnectionsExhausted);
    assert_eq!(replay.asset_identity, first.asset_identity);
}

#[tokio::test]
async fn same_reason_singleflight_has_one_media_finalizer() {
    let fixture = post_refresh_terminal_fixture("runtime-same-reason-singleflight", true).await;
    let reason = HlsRuntimeCustomTailReason::LowPriorityPreempted;
    let buffer = configured_runtime_custom_buffer(&fixture, reason);
    let asset = snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("singleflight asset");
    let target_duration_ms = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .and_then(|lease| lease.last_manifest_snapshot.map(|manifest| manifest.target_duration_ms))
        .expect("published target duration");
    let key = prepared_terminal_bundle_key(&asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let publisher = fixture
        .ctx
        .hls_proxy
        .install_controlled_terminal_bundle_flight_for_test(key)
        .expect("controlled singleflight preparation");
    let request = || HlsRuntimeCustomTailRequest {
        session: Arc::clone(&fixture.session),
        proxy_session_id: fixture.proxy_session_id.clone(),
        lease_id: fixture.lease_id.clone(),
        reason,
        now_ms: fixture.now_ms,
    };

    let (first, second) = tokio::join!(
        commit_hls_runtime_custom_tail(fixture.ctx.clone(), request()),
        commit_hls_runtime_custom_tail(fixture.ctx.clone(), request()),
    );
    assert_eq!(first, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    assert_eq!(second, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 1);
    let bundle = build_prepared_terminal_bundle(&asset.asset, key).expect("single relative bundle");
    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle });
    let plan = wait_for_runtime_custom_plan(&fixture).await;

    assert_eq!(plan.reason, reason);
    assert_eq!(buffer.finite_hls_render_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    assert_eq!(buffer.finite_hls_finalize_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
}

#[tokio::test(start_paused = true)]
async fn in_flight_post_refresh_owner_does_not_poll_refresh_gate() {
    let fixture = post_refresh_terminal_fixture_with_progress("in-flight-refresh-wait", true, false).await;
    fixture.session.write().await.origin_refresh.mark_started(fixture.now_ms);
    assert_post_refresh_owner_checks_refresh_gate_once(&fixture).await;
}

#[tokio::test(start_paused = true)]
async fn debounced_post_refresh_owner_does_not_poll_refresh_gate() {
    let fixture = post_refresh_terminal_fixture_with_progress("debounced-refresh-wait", true, false).await;
    fixture.session.write().await.origin_refresh.next_fetch_allowed_at_ms = fixture.now_ms.saturating_add(10_000);
    assert_post_refresh_owner_checks_refresh_gate_once(&fixture).await;
}

#[tokio::test(start_paused = true)]
async fn multi_lease_fallback_handles_reverse_insertion_without_early_return() {
    assert_multi_lease_fallback_handles_pending_and_unavailable(true).await;
}
