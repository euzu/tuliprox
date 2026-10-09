use super::{
    super::{
        prepare_terminal_base_evidence, prepared_terminal_bundle_key, snapshot_terminal_media_asset, AppState, Arc,
        CustomStreamResponse, Duration, HlsAccessLeaseId, HlsLeasePlaybackMode, HlsPreparedTerminalBundleState,
        HlsProxyManager, HlsTerminalTailPlan, ProxySessionId, StatusCode, TransportStreamBuffer,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    get_response, grant_hls_proxy_lease, map_ready_segment_without_lease, publish_ready_test_manifest_for_lease,
    response_body, test_app_state_with_hls_proxy,
};

pub(in crate::api::endpoints::hls_api::tests) fn enable_runtime_policy_custom_responses(app_state: &Arc<AppState>) {
    app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: None,
        user_connections_exhausted: Some(TransportStreamBuffer::new(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../test/fixtures/hls/user_connections_exhausted.ts"
            ))
            .to_vec(),
        )),
        provider_connections_exhausted: Some(TransportStreamBuffer::new(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../test/fixtures/hls/provider_connections_exhausted.ts"
            ))
            .to_vec(),
        )),
        low_priority_preempted: Some(TransportStreamBuffer::new(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/low_priority_preempted.ts"))
                .to_vec(),
        )),
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_user_exhausted_terminal_bundle(
    app_state: &Arc<AppState>,
    target_duration_ms: u64,
) {
    let response = app_state.app_config.custom_stream_response.load_full().expect("runtime custom responses");
    let asset = response
        .user_connections_exhausted
        .as_ref()
        .and_then(|buffer| snapshot_terminal_media_asset(buffer).ok())
        .expect("valid user-exhausted terminal asset");
    let key = prepared_terminal_bundle_key(&asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state =
        app_state.hls.proxy.start_prepared_terminal_bundle(asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => app_state
            .hls
            .proxy
            .wait_for_prepared_terminal_bundle(key)
            .await
            .expect("user-exhausted terminal bundle completion"),
        state => state,
    };
    assert!(matches!(
        state,
        HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.key == key
    ));
}

pub(in crate::api::endpoints::hls_api::tests) struct RuntimePolicyEndpointFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp_dir: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) manifest_uri: String,
    pub(in crate::api::endpoints::hls_api::tests) live_segment_uri: String,
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_runtime_policy_base_timing(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    phase: &str,
) {
    let session =
        app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.expect("runtime policy session");
    let manifest = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, super::super::super::current_time_millis())
        .await
        .and_then(|lease| lease.last_manifest_snapshot)
        .expect("runtime policy manifest");
    let evidence = prepare_terminal_base_evidence(
        &session,
        app_state.hls.proxy.segment_cache(),
        &manifest,
        super::super::super::current_time_millis(),
    )
    .await;
    assert!(
        evidence.timing().is_some(),
        "runtime policy base timing {phase}: {}",
        evidence.track_evidence_reason_code()
    );
    evidence.release();
}

pub(in crate::api::endpoints::hls_api::tests) async fn serve_and_wait_runtime_policy_base_segment(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    live_segment_uri: &str,
) {
    let initial_segment = get_response(Arc::clone(app_state), live_segment_uri, None).await;
    assert_eq!(initial_segment.status(), StatusCode::OK);
    assert!(!response_body(initial_segment).await.is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let completed = app_state
                .hls
                .proxy
                .access_lease_response_snapshot(lease_id, proxy_session_id, super::super::super::current_time_millis())
                .await
                .and_then(|lease| lease.playback_cursor.highest_contiguous_completed_proxy_seq);
            if completed == Some(123) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial live segment completion");
}

pub(in crate::api::endpoints::hls_api::tests) async fn runtime_policy_endpoint_fixture(
    publish_manifest: bool,
) -> RuntimePolicyEndpointFixture {
    const TARGET_DURATION_MS: u64 = 12_000;

    let temp_dir = tempfile::tempdir().expect("runtime policy cache tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    enable_runtime_policy_custom_responses(&app_state);
    let live_bytes =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
    let proxy_session_id = ProxySessionId(map_ready_segment_without_lease(&app_state, 123, "ts", live_bytes).await);
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id)
        .await
        .expect("runtime policy session");
    session.write().await.segments.get_mut(&123).expect("runtime policy base segment").duration_ms = TARGET_DURATION_MS;
    let lease_id = HlsAccessLeaseId(grant_hls_proxy_lease(&app_state, &proxy_session_id.0).await);
    if publish_manifest {
        publish_ready_test_manifest_for_lease(&app_state, &proxy_session_id, &lease_id, TARGET_DURATION_MS).await;
        let target_duration_ms = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, super::super::super::current_time_millis())
            .await
            .and_then(|lease| lease.last_manifest_snapshot)
            .map(|manifest| manifest.target_duration_ms)
            .expect("published runtime policy target duration");
        prepare_user_exhausted_terminal_bundle(&app_state, target_duration_ms).await;
    }
    let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, lease_id.0);
    let live_segment_uri = format!("/hls/shared/live/{}/{}/000123.ts", proxy_session_id.0, lease_id.0);
    if publish_manifest {
        assert_runtime_policy_base_timing(&app_state, &proxy_session_id, &lease_id, "before serve").await;
    }
    serve_and_wait_runtime_policy_base_segment(&app_state, &proxy_session_id, &lease_id, &live_segment_uri).await;
    if publish_manifest {
        assert_runtime_policy_base_timing(&app_state, &proxy_session_id, &lease_id, "after serve").await;
    }

    RuntimePolicyEndpointFixture {
        _temp_dir: temp_dir,
        app_state,
        proxy_session_id,
        lease_id,
        manifest_uri,
        live_segment_uri,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn wait_for_runtime_policy_terminal_plan(
    fixture: &RuntimePolicyEndpointFixture,
) -> Arc<HlsTerminalTailPlan> {
    let plan = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(HlsLeasePlaybackMode::TerminalTail(plan)) = fixture
                .app_state
                .hls
                .proxy
                .access_lease_response_snapshot(
                    &fixture.lease_id,
                    &fixture.proxy_session_id,
                    super::super::super::current_time_millis(),
                )
                .await
                .map(|lease| lease.playback_mode)
            {
                return plan;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Ok(plan) = plan {
        return plan;
    }
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::super::current_time_millis(),
        )
        .await;
    let state = lease.as_ref().map_or("missing", |lease| lease.state.as_log_value());
    let playback = lease.as_ref().map_or("missing", |lease| match lease.playback_mode {
        HlsLeasePlaybackMode::Live => "live",
        HlsLeasePlaybackMode::TerminalTail(_) => "terminal-tail",
        HlsLeasePlaybackMode::TerminalUnavailable { .. } => "terminal-unavailable",
        HlsLeasePlaybackMode::Ended => "ended",
    });
    panic!(
        "runtime policy terminal owner deadline: state={state} playback={playback} owners={}",
        fixture.app_state.hls.proxy.terminal_pending().owner_count()
    );
}
