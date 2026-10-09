use super::*;

pub(super) fn post_refresh_owner_request(fixture: &PostRefreshTerminalFixture) -> OriginRefreshRequest {
    let origin_entry =
        super::super::super::manifest_fetch::LiveHlsOriginEntry::parse("http://127.0.0.1:9/live/user/pass/12345.m3u8")
            .expect("test origin entry parses");
    OriginRefreshRequest {
        app_config: Arc::clone(&fixture.ctx.app_config),
        session: Arc::clone(&fixture.session),
        origin_entry,
        headers: axum::http::HeaderMap::new(),
        origin_provider_session_headers: axum::http::HeaderMap::new(),
        disabled_headers: None,
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::clone(fixture.ctx.hls_proxy.segment_cache()),
        hls_proxy: Arc::clone(&fixture.ctx.hls_proxy),
        segment_repair: Arc::clone(fixture.ctx.hls_proxy.segment_repair()),
        segment_worker_pool: Arc::clone(fixture.ctx.hls_proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(fixture.ctx.hls_proxy.map_worker_pool()),
        origin_manifest_timeout_ms: fixture.ctx.hls_proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: fixture.ctx.hls_proxy.manifest_recovery_burst(),
        strip: fixture.ctx.hls_proxy.strip(),
        retry_policy: super::super::super::manifest_fetch::RetryPolicy { delays_ms: [0; 5], jitter_max_ms: 0 },
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement:
            super::super::super::refresh::HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        now_ms: fixture.now_ms,
        origin_io: None,
        post_refresh_runtime: Some(super::super::super::refresh::HlsPostRefreshRuntime {
            ctx: fixture.ctx.downgrade(),
        }),
    }
}

pub(super) async fn register_real_post_refresh_owner(
    fixture: &PostRefreshTerminalFixture,
    reason: super::super::super::refresh::HlsPostRefreshAvailabilityReason,
) {
    let (origin_progress_generation, media_readiness_generation) = {
        let session = fixture.session.read().await;
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert_eq!(
        register_post_refresh_availability_reevaluation(
            fixture.ctx.clone(),
            Arc::clone(&fixture.session),
            post_refresh_owner_request(fixture),
            HlsPostRefreshAvailabilityAction::Reevaluate {
                reason,
                origin_progress_generation,
                media_readiness_generation,
            },
        )
        .await,
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
}

pub(super) fn assert_availability_owner_registered(fixture: &PostRefreshTerminalFixture) {
    assert_eq!(fixture.ctx.hls_proxy.availability_reevaluations().owner_count(), 1);
}

/// How long the owner gets to finish before the wait gives up.
///
/// These tests run on a paused clock, so this is logical time and costs
/// nothing when the owner completes normally.
const AVAILABILITY_OWNER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of observer revisions to accept before concluding the owner is
/// cycling without converging.
const AVAILABILITY_OWNER_MAX_REVISIONS: usize = 1_024;

pub(super) async fn wait_for_availability_owner_completion(fixture: &PostRefreshTerminalFixture) {
    let coordinator = fixture.ctx.hls_proxy.availability_reevaluations();
    // Bounded twice over, because the two ways this can fail to converge
    // need different guards. A parked owner never wakes the observer, and
    // on a paused clock the runtime is idle, so the timeout fires. An owner
    // that keeps re-arming a successor wakes the observer forever and keeps
    // the runtime busy, so the clock never advances and only the revision
    // count catches it. Either way the test fails with a diagnostic instead
    // of hanging the suite.
    let wait = async {
        let mut revisions = 0usize;
        while let Some(mut observer) = coordinator.observe_owner(&fixture.proxy_session_id) {
            if matches!(
                observer.changed().await,
                crate::availability_reevaluation::HlsAvailabilityReevaluationObservation::OwnerFinished
            ) {
                // The owner is gone; re-check the map and leave the loop.
                continue;
            }
            revisions += 1;
            assert!(
                revisions <= AVAILABILITY_OWNER_MAX_REVISIONS,
                "availability owner for {:?} produced {revisions} revisions without completing",
                fixture.proxy_session_id
            );
        }
    };
    assert!(
        tokio::time::timeout(AVAILABILITY_OWNER_COMPLETION_TIMEOUT, wait).await.is_ok(),
        "availability owner for {:?} did not complete within {AVAILABILITY_OWNER_COMPLETION_TIMEOUT:?}; \
         {} owner(s) still registered",
        fixture.proxy_session_id,
        coordinator.owner_count()
    );
    assert_eq!(coordinator.owner_count(), 0);
}

pub(super) async fn assert_post_refresh_owner_checks_refresh_gate_once(fixture: &PostRefreshTerminalFixture) {
    let refresh_skipped_before = fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped;
    register_real_post_refresh_owner(
        fixture,
        super::super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
    )
    .await;
    for _ in 0..256 {
        if fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped == refresh_skipped_before.saturating_add(1) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped,
        refresh_skipped_before.saturating_add(1),
        "the owner must observe the unavailable refresh gate once"
    );

    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped,
        refresh_skipped_before.saturating_add(1),
        "unchanged refresh evidence must not produce repeated gate attempts"
    );

    fixture.ctx.hls_proxy.availability_reevaluations().cancel_session(&fixture.proxy_session_id);
    wait_for_availability_owner_completion(fixture).await;
}

pub(super) async fn post_refresh_terminal_fixture(name: &str, terminal_asset: bool) -> PostRefreshTerminalFixture {
    post_refresh_terminal_fixture_with_progress(name, terminal_asset, true).await
}

pub(super) async fn post_refresh_terminal_fixture_with_progress(
    name: &str,
    terminal_asset: bool,
    complete_playback: bool,
) -> PostRefreshTerminalFixture {
    post_refresh_terminal_fixture_with_bundle_state(name, terminal_asset, complete_playback, true).await
}

pub(super) fn post_refresh_origin_manifest(terminal_asset: bool) -> (u64, ParsedOriginManifest) {
    let (target_duration_ms, body) = if terminal_asset {
        (
            12_000,
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:12\n\
             #EXTINF:12.0,\n0.ts\n#EXTINF:12.0,\n1.ts\n#EXTINF:12.0,\n2.ts\n",
        )
    } else {
        (
            8_000,
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:8\n\
             #EXTINF:4.0,\n0.ts\n#EXTINF:8.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        )
    };
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(body, "http://origin.example/live/index.m3u8")
    else {
        panic!("post-refresh terminal fixture parses");
    };
    (target_duration_ms, manifest)
}

pub(super) async fn prepare_post_refresh_terminal_base(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    manifest: &ParsedOriginManifest,
    terminal_asset: bool,
    prepare_terminal_bundle: bool,
    target_duration_ms: u64,
    now_ms: u64,
) {
    let terminal_base_cache_key = {
        let mut session = session.write().await;
        session.apply_origin_manifest(manifest).expect("fixture timeline applies");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: now_ms };
        }
        session.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
        session.origin_control.last_media_progress_at_ms = Some(now_ms);
        session.origin_control.target_duration_snapshot_ms = Some(target_duration_ms);
        session.segments.get(&0).expect("fixture terminal-base segment").cache_key.clone()
    };
    if !terminal_asset {
        return;
    }
    ctx.hls_proxy
        .segment_cache()
        .write_bytes_and_commit(&terminal_base_cache_key, TERMINAL_ASSET_BYTES)
        .await
        .expect("terminal-compatible READY base bytes commit");
    session.write().await.segments.get_mut(&0).expect("fixture terminal-base segment").status =
        SegmentCacheStatus::Ready {
            content_length: u64::try_from(TERMINAL_ASSET_BYTES.len()).unwrap_or(u64::MAX),
            ready_at_ms: now_ms,
        };
    if prepare_terminal_bundle {
        let asset = snapshot_terminal_media_asset(&TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec()))
            .expect("fixture terminal asset parses");
        let key = prepared_terminal_bundle_key(&asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let state = ctx.hls_proxy.start_prepared_terminal_bundle(
            Arc::clone(&asset),
            target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => {
                ctx.hls_proxy.wait_for_prepared_terminal_bundle(key).await
            }
            HlsPreparedTerminalBundleState::Ready { .. }
            | HlsPreparedTerminalBundleState::Failed { .. }
            | HlsPreparedTerminalBundleState::Incompatible { .. } => Some(state),
        };
        assert!(
            matches!(state, Some(HlsPreparedTerminalBundleState::Ready { .. })),
            "terminal preparation must be ready: {state:?}"
        );
    }
}

pub(super) async fn publish_post_refresh_terminal_lease(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    name: &str,
    terminal_asset: bool,
    target_duration_ms: u64,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(format!("{name}-lease"));
    ctx.hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new(name, name),
            proxy_session_id.clone(),
            name.to_string(),
            "token".to_string(),
            1,
            "stream".to_string(),
            1,
            now_ms,
            60_000,
        ))
        .await;
    let publication = ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, proxy_session_id, now_ms)
        .await
        .expect("terminal fixture publication guard");
    let mut manifest_snapshot = pressure_manifest(target_duration_ms);
    if terminal_asset {
        Arc::make_mut(&mut manifest_snapshot.visible_segments)[0].duration_ms = target_duration_ms;
        manifest_snapshot.playlist_duration_ms = target_duration_ms;
        manifest_snapshot.last_visible_media_end_ms = target_duration_ms;
    }
    Arc::make_mut(&mut manifest_snapshot.visible_segments)[0].uri =
        format!("/hls/shared/live/{}/{}/0.ts", proxy_session_id.0, lease_id.0).into();
    assert!(ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(&lease_id, proxy_session_id, publication, manifest_snapshot, now_ms,)
        .await
        .is_committed());
    assert!(ctx
        .hls_proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    assert!(
        ctx.hls_proxy.access_lease_response_snapshot(&lease_id, proxy_session_id, now_ms).await.is_some(),
        "activated terminal fixture lease remains available"
    );
    lease_id
}

pub(super) async fn post_refresh_terminal_fixture_with_bundle_state(
    name: &str,
    terminal_asset: bool,
    complete_playback: bool,
    prepare_terminal_bundle: bool,
) -> PostRefreshTerminalFixture {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: true, ..Config::default() });
    let ctx = &hls_ctx;
    if terminal_asset {
        ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));
    }
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    // These tests drive time with `tokio::time::advance`. Anchor the scheduling
    // clock to tokio's so the owner's deadlines move with it; otherwise it
    // sleeps towards a wall-clock deadline the paused clock never reaches.
    ctx.hls_proxy.follow_tokio_clock_for_test(now_ms);
    let (session, _) =
        ctx.hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, name), b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let (target_duration_ms, manifest) = post_refresh_origin_manifest(terminal_asset);
    prepare_post_refresh_terminal_base(
        ctx,
        &session,
        &manifest,
        terminal_asset,
        prepare_terminal_bundle,
        target_duration_ms,
        now_ms,
    )
    .await;
    let lease_id =
        publish_post_refresh_terminal_lease(ctx, &proxy_session_id, name, terminal_asset, target_duration_ms, now_ms)
            .await;
    if complete_playback {
        advance_post_refresh_fixture_playback(
            ctx,
            &session,
            &proxy_session_id,
            &lease_id,
            if terminal_asset { 22_000 } else { 7_000 },
            now_ms,
        )
        .await;
    }
    PostRefreshTerminalFixture { ctx: ctx.clone(), session, proxy_session_id, lease_id, now_ms }
}
