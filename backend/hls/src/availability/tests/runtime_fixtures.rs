use super::*;

pub(super) async fn prepare_runtime_custom_bundle(
    fixture: &PostRefreshTerminalFixture,
    reason: HlsRuntimeCustomTailReason,
) {
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("runtime custom-tail fixture lease");
    let target_duration_ms =
        lease.last_manifest_snapshot.as_ref().expect("published runtime custom-tail manifest").target_duration_ms;
    let asset =
        snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("configured runtime custom-tail asset");
    let key = prepared_terminal_bundle_key(&asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state = fixture.ctx.hls_proxy.start_prepared_terminal_bundle(
        Arc::clone(&asset.asset),
        target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => fixture
            .ctx
            .hls_proxy
            .wait_for_prepared_terminal_bundle(key)
            .await
            .expect("runtime custom-tail bundle completion"),
        state => state,
    };
    assert!(
        matches!(state, HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.matches_key_and_shape(key)),
        "runtime custom-tail bundle must be READY: {state:?}"
    );
}

pub(super) async fn wait_for_runtime_custom_plan(fixture: &PostRefreshTerminalFixture) -> Arc<HlsTerminalTailPlan> {
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let lease = fixture
                .ctx
                .hls_proxy
                .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
                .await
                .expect("runtime custom-tail lease remains stored");
            if let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode {
                return plan;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Ok(plan) = completed {
        return plan;
    }
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await;
    let state = lease.as_ref().map_or("missing", |lease| lease.state.as_log_value());
    let playback = lease.as_ref().map_or("missing", |lease| match lease.playback_mode {
        HlsLeasePlaybackMode::Live => "live",
        HlsLeasePlaybackMode::TerminalTail(_) => "terminal-tail",
        HlsLeasePlaybackMode::TerminalUnavailable { .. } => "terminal-unavailable",
        HlsLeasePlaybackMode::Ended => "ended",
    });
    panic!(
        "runtime custom-tail owner deadline: state={state} playback={playback} owners={}",
        fixture.ctx.hls_proxy.terminal_pending().owner_count()
    );
}

pub(super) async fn commit_runtime_custom_reason(
    fixture: &PostRefreshTerminalFixture,
    reason: HlsRuntimeCustomTailReason,
    prewarm: bool,
) -> (HlsRuntimeCustomTailOutcome, Arc<HlsTerminalTailPlan>) {
    if prewarm {
        prepare_runtime_custom_bundle(fixture, reason).await;
    }
    let outcome = commit_hls_runtime_custom_tail(
        fixture.ctx.clone(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(&fixture.session),
            proxy_session_id: fixture.proxy_session_id.clone(),
            lease_id: fixture.lease_id.clone(),
            reason,
            now_ms: fixture.now_ms,
        },
    )
    .await;
    assert!(matches!(
        outcome,
        HlsRuntimeCustomTailOutcome::Committed
            | HlsRuntimeCustomTailOutcome::AlreadyCommitted
            | HlsRuntimeCustomTailOutcome::PendingOwnerRegistered
    ));
    let plan = wait_for_runtime_custom_plan(fixture).await;
    (outcome, plan)
}

pub(super) fn configured_runtime_custom_buffer(
    fixture: &PostRefreshTerminalFixture,
    reason: HlsRuntimeCustomTailReason,
) -> TransportStreamBuffer {
    let responses = fixture.ctx.app_config.custom_stream_response.load_full().expect("runtime custom responses");
    match reason {
        HlsRuntimeCustomTailReason::ChannelUnavailable => responses.channel_unavailable.as_ref(),
        HlsRuntimeCustomTailReason::LowPriorityPreempted => responses.low_priority_preempted.as_ref(),
        HlsRuntimeCustomTailReason::UserConnectionsExhausted => responses.user_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => responses.provider_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::UserAccountExpired => responses.user_account_expired.as_ref(),
        HlsRuntimeCustomTailReason::SessionOrLeaseExpired => responses.hls_session_or_lease_expired.as_ref(),
    }
    .expect("reason-specific runtime buffer")
    .clone()
}

pub(super) fn segment_bytes(plan: &HlsTerminalTailPlan, index: u16) -> Bytes {
    plan.segment_bytes(HlsTerminalSegmentPath { generation: plan.generation, index })
        .expect("committed immutable custom-tail segment")
}

pub(super) fn payload_continuity_bounds(bytes: &[u8]) -> std::collections::HashMap<u16, (u8, u8, bool)> {
    let mut payload_bounds = std::collections::HashMap::<u16, (u8, u8)>::new();
    let mut first_packet_discontinuity = std::collections::HashMap::<u16, bool>::new();
    for packet in bytes.as_chunks::<188>().0.iter().filter(|packet| packet[0] == 0x47) {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let discontinuity = matches!(adaptation_field_control, 0b10 | 0b11)
            && packet[4] > 0
            && packet.get(5).is_some_and(|flags| flags & 0x80 != 0);
        first_packet_discontinuity.entry(pid).or_insert(discontinuity);
        if !matches!(adaptation_field_control, 0b01 | 0b11) {
            continue;
        }
        let counter = packet[3] & 0x0f;
        payload_bounds.entry(pid).and_modify(|entry| entry.1 = counter).or_insert((counter, counter));
    }
    payload_bounds
        .into_iter()
        .map(|(pid, (first, last))| {
            let discontinuity = first_packet_discontinuity.get(&pid).copied().unwrap_or(false);
            (pid, (first, last, discontinuity))
        })
        .collect()
}

pub(super) fn with_internal_payload_continuity_jump(bytes: &[u8]) -> Vec<u8> {
    let mut corrupted = bytes.to_vec();
    let mut first_payload_pid = None;
    for packet in corrupted.as_chunks_mut::<188>().0.iter_mut().filter(|packet| packet[0] == 0x47) {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        if !matches!(adaptation_field_control, 0b01 | 0b11) {
            continue;
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        if pid == 0x1fff {
            continue;
        }
        match first_payload_pid {
            None => first_payload_pid = Some(pid),
            Some(first_pid) if first_pid == pid => {
                let continuity_counter = packet[3] & 0x0f;
                packet[3] = (packet[3] & 0xf0) | (continuity_counter.wrapping_add(3) & 0x0f);
                return corrupted;
            }
            Some(_) => {}
        }
    }
    panic!("terminal fixture must contain two payload packets for one PID");
}

pub(super) async fn assert_active_policy_reason_commits(name: &str, reason: HlsRuntimeCustomTailReason) {
    let fixture = post_refresh_terminal_fixture(name, true).await;
    let (_, plan) = commit_runtime_custom_reason(&fixture, reason, true).await;
    assert_eq!(plan.reason, reason);
    assert_eq!(plan.segment_duration_ms, 10_027);
    assert_eq!(
        plan.asset_identity,
        HlsRuntimeCustomTailAssetIdentity::from_asset(
            &snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("reason-specific configured asset")
        )
    );
}

pub(super) async fn wait_for_terminal_pending_owners(fixture: &PostRefreshTerminalFixture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture.ctx.hls_proxy.terminal_pending().owner_count() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "terminal-pending owner deadline: actual={} expected={expected}",
            fixture.ctx.hls_proxy.terminal_pending().owner_count()
        )
    });
}

pub(super) async fn add_active_post_refresh_lease(
    fixture: &PostRefreshTerminalFixture,
    lease_id: HlsAccessLeaseId,
    active_map: Option<HlsMapSignature>,
) {
    let lease = HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("multi-lease", &lease_id.0),
        fixture.proxy_session_id.clone(),
        "multi-lease".to_string(),
        lease_id.0.clone(),
        1,
        "stream".to_string(),
        1,
        fixture.now_ms,
        60_000,
    );
    fixture.ctx.hls_proxy.prepare_access_lease(lease).await;
    let publication = fixture
        .ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("second lease publication guard");
    let mut manifest = pressure_manifest(12_000);
    manifest.active_map = active_map;
    Arc::make_mut(&mut manifest.visible_segments)[0].uri =
        format!("/hls/shared/live/{}/{}/0.ts", fixture.proxy_session_id.0, lease_id.0).into();
    assert!(fixture
        .ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &fixture.proxy_session_id,
            publication,
            manifest,
            fixture.now_ms,
        )
        .await
        .is_committed());
    assert!(fixture
        .ctx
        .hls_proxy
        .activate_access_lease(
            &lease_id,
            &fixture.proxy_session_id,
            fixture.now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
}

pub(super) async fn advance_post_refresh_fixture_playback(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    playback_elapsed_ms: u64,
    now_ms: u64,
) {
    let lease = ctx
        .hls_proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, now_ms)
        .await
        .expect("live terminal fixture remains available");
    let identity = lease.media_identity().expect("live terminal fixture identity");
    let playback_at_ms = now_ms.saturating_sub(playback_elapsed_ms);
    for proxy_seq in [0_u64, 1] {
        let token = ctx
            .hls_proxy
            .record_access_lease_segment_request_started_if_identity_matches(
                lease_id,
                proxy_session_id,
                identity,
                proxy_seq,
                playback_at_ms,
            )
            .await
            .expect("fixture segment request starts");
        assert_eq!(
            ctx.hls_proxy
                .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                    session,
                    lease_id,
                    proxy_session_id,
                    identity,
                    token,
                    playback_at_ms,
                )
                .await,
            super::super::super::manager::HlsMediaActivityCommitOutcome::Committed
        );
    }
}

pub(super) async fn assert_multi_lease_fallback_handles_pending_and_unavailable(reverse_insertion: bool) {
    let fixture_name = if reverse_insertion { "multi-lease-fallback-reverse" } else { "multi-lease-fallback-forward" };
    let fixture = post_refresh_terminal_fixture_with_bundle_state(fixture_name, true, true, false).await;
    let incompatible_lease_id = HlsAccessLeaseId("multi-lease-incompatible".to_string());
    add_active_post_refresh_lease(
        &fixture,
        incompatible_lease_id.clone(),
        Some(HlsMapSignature { fingerprint: [0x5a; 32], container: HlsMediaContainer::FragmentedMp4 }),
    )
    .await;
    if reverse_insertion {
        let mut leases = fixture.ctx.hls_proxy.hold_access_lease_store_for_test().await;
        let primary = leases.remove_access_lease(&fixture.lease_id).expect("primary live lease remains stored");
        leases.prepare_access_lease(primary);
    }
    let asset = snapshot_terminal_media_asset(&TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec()))
        .expect("controlled terminal asset parses");
    let bundle_key = prepared_terminal_bundle_key(&asset, 12_000, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let _controlled_flight = fixture
        .ctx
        .hls_proxy
        .install_controlled_terminal_bundle_flight_for_test(bundle_key)
        .expect("controlled terminal preparation is unique");
    let evaluation_now_ms = fixture.ctx.hls_proxy.terminal_commit_now_ms();

    let aggregate =
        evaluate_owner_failure_fallback(&fixture.ctx, &fixture.session, &fixture.proxy_session_id, evaluation_now_ms)
            .await;

    assert_eq!(aggregate.total, 2);
    assert_eq!(aggregate.pending_owned, 1);
    assert_eq!(aggregate.terminal_committed, 1, "{aggregate:?}");
    assert!(aggregate.unresolved.is_empty());
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 1);
    let unavailable = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&incompatible_lease_id, &fixture.proxy_session_id, evaluation_now_ms)
        .await
        .expect("incompatible lease remains stored");
    assert!(matches!(unavailable.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
    fixture.ctx.hls_proxy.terminal_pending().cancel_session(&fixture.proxy_session_id);
}
