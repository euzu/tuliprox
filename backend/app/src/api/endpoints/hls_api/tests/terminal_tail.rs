use super::{
    access_lease_id_from_variant_uri, cache_test_m3u_hls_item, configure_default_test_server,
    create_active_hls_user_session, enable_channel_unavailable_custom_response, enable_hls_cache, get_response,
    get_status, media_uri_count, normal_manifest, path_has_extension, prepare_pending_test_hls_access_lease,
    proxy_session_id_from_variant_uri, regression_origin_manifest, response_body, single_variant_master_playlist,
    spawn_test_binary_origin, spawn_test_encrypted_hls_origin, store_test_sources_with_target, terminal_test_asset,
    test_app_state_with_hls_proxy_and_inputs, test_app_state_with_inputs, test_fingerprint, test_hls_access_context,
    test_hls_entry_stream_context, test_m3u_hls_item, test_m3u_hls_share_target, try_test_hls_cached_manifest_response,
    TestBinaryOriginResponse, TestSegmentOrigin, AES_TEST_KEY_BYTES, AES_TEST_MANIFEST, AES_TEST_PLAINTEXT_SEGMENT,
};
use crate::{
    api::model::{
        build_proxy_session_id, commit_terminal_tail_if_lease_reserve_requires_cutover, prepare_terminal_base_evidence,
        prepared_terminal_bundle_key, snapshot_terminal_media_asset, trigger_origin_refresh_sync, AppState,
        ConnectionKind, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsAccessLease, HlsAccessLeaseId,
        HlsAccessLeaseState, HlsAccessLeaseTiming, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
        HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome, HlsManifestAcceptanceExhaustionReason,
        HlsManifestAcceptanceTrigger, HlsManifestCommitRequirement, HlsObservedRecoveryLatency, HlsOperationTimeoutMs,
        HlsOriginPathCondition, HlsPlaybackFamilyKey, HlsPreparedTerminalBundleState, HlsProxyManager,
        HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsSessionHandle, HlsSessionKey,
        HlsTerminalMediaAsset, HlsTerminalMediaPreparationState, HlsTerminalResolution, HlsTerminalSegmentPath,
        HlsTerminalTailGeneration, HlsTransitionMarginMs, LiveHlsOriginEntry, OriginRefreshRequest, ProxySessionId,
        RetryPolicy, SegmentCacheStatus, TransportStreamBuffer, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    model::{ConfigInput, HlsCacheConfig, ProxyUserCredentials, StripConfig},
};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::model::{
    HlsCacheConfigDto, HlsManifestRecoveryBurstConfigDto, HlsManifestRecoveryBurstLevel, HlsStripConfigDto,
    HlsStripMode, InputType, UserConnectionPermission,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

pub(in crate::api::endpoints::hls_api::tests) async fn terminal_generation_for_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) -> HlsTerminalTailGeneration {
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, super::super::current_time_millis())
        .await
        .expect("terminal lease remains stored");
    let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode else {
        panic!("lease remains terminal");
    };
    plan.generation
}

pub(in crate::api::endpoints::hls_api::tests) const RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;

pub(in crate::api::endpoints::hls_api::tests) struct RecoveryBeforeCutoverFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp_dir: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) origin_phase: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) refresh: OriginRefreshRequest,
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_initial_recovery_window(
    fixture: &RecoveryBeforeCutoverFixture,
) {
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::initial(Duration::from_secs(10)),
    )
    .await
    .expect("READY initial manifest");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains("/000000.ts"));
    assert!(body.contains("/000002.ts"));
    assert!(!body.contains("/000003.ts"));
    let now_ms = super::super::current_time_millis();
    assert!(fixture
        .app_state
        .hls
        .proxy
        .activate_access_lease(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
        )
        .await
        .is_activated());
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .expect("active stripped lease");
    let snapshot = lease.last_manifest_snapshot.as_ref().expect("lease manifest snapshot");
    assert_eq!(snapshot.visible_segments.len(), 3);
    assert_eq!(snapshot.last_proxy_seq, 2);
    let evidence =
        prepare_terminal_base_evidence(&fixture.session, fixture.app_state.hls.proxy.segment_cache(), snapshot, now_ms)
            .await;
    assert_eq!(evidence.track_signature(), Some(terminal_test_asset().track_signature().clone()));
    evidence.release();
    extend_ready_segment_as_sparse_file(&fixture.app_state, &fixture.session, 2, RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES)
        .await;
    let uri = format!("/hls/shared/live/{}/{}/000002.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_eq!(segment.headers()[header::CONTENT_LENGTH], RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES.to_string());
    assert_eq!(
        u64::try_from(response_body(segment).await.len()).unwrap_or(u64::MAX),
        RECOVERY_LOGICAL_LARGE_SEGMENT_BYTES
    );
}

pub(in crate::api::endpoints::hls_api::tests) async fn recovery_before_cutover_fixture() -> RecoveryBeforeCutoverFixture
{
    let temp_dir = tempfile::tempdir().expect("recovery cache tempdir");
    let unchanged_manifest = Arc::<[u8]>::from(regression_origin_manifest(100, 6));
    let progressed_manifest = Arc::<[u8]>::from(regression_origin_manifest(101, 6));
    let segment = Arc::<[u8]>::from(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
            .as_slice(),
    );
    let origin_phase = Arc::new(AtomicUsize::new(0));
    let phase = Arc::clone(&origin_phase);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        if path_has_extension(path, "m3u8") {
            return match phase.load(Ordering::SeqCst) {
                0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&unchanged_manifest)),
                1 => TestBinaryOriginResponse::new(
                    StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                    Arc::<[u8]>::from(&b"retry"[..]),
                ),
                _ => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&progressed_manifest)),
            };
        }
        TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&segment))
    }))
    .await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("recovery-regression-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state =
        test_app_state_with_hls_proxy_and_inputs(test_beast_hls_proxy(temp_dir.path()), vec![Arc::new(input)]);
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 1_000)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("recovery-before-cutover".to_string());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
    let refresh =
        regression_origin_refresh_request(&app_state, Arc::clone(&session), &manifest_url, Some(lease_id.clone()));
    let fixture = RecoveryBeforeCutoverFixture {
        _temp_dir: temp_dir,
        origin,
        origin_phase,
        app_state,
        session,
        proxy_session_id,
        lease_id,
        refresh,
    };
    assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    wait_for_ready_timeline(&fixture.session, 6).await;
    assert_initial_recovery_window(&fixture).await;
    fixture
}

pub(in crate::api::endpoints::hls_api::tests) async fn run_recovery_outage(
    fixture: &mut RecoveryBeforeCutoverFixture,
) -> (u64, Option<u64>) {
    let progress_generation = fixture.session.read().await.origin_control.progress_generation;
    for _ in 0..2 {
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    }
    assert_eq!(fixture.session.read().await.origin_control.progress_generation, progress_generation);
    fixture.origin_phase.store(1, Ordering::SeqCst);
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    assert_eq!(fixture.app_state.hls.proxy.manifest_recovery_burst().level.plan(), plan);
    fixture.refresh.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let requests_before = fixture.origin.manifest_request_count();
    let mut last_episode_generation = None;
    for _ in 0..3 {
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
        let lease = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                super::super::current_time_millis(),
            )
            .await
            .expect("407 exhaustion cannot remove the lease");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        let session = fixture.session.read().await;
        let episode = session.origin_control.acceptance_episode.as_ref().expect("bounded acceptance evidence");
        assert!(episode.full_burst_completed);
        assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
        last_episode_generation = Some(episode.generation.0);
    }
    assert!(
        fixture.origin.manifest_request_count().saturating_sub(requests_before)
            >= plan.total_candidates().saturating_mul(3)
    );
    let uri = format!("/hls/shared/live/{}/{}/000003.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let cached = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(cached.status(), StatusCode::OK);
    assert!(!response_body(cached).await.is_empty());
    (progress_generation, last_episode_generation)
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_recovery_after_outage(
    fixture: &mut RecoveryBeforeCutoverFixture,
    progress_generation: u64,
    last_episode_generation: Option<u64>,
) {
    fixture.origin_phase.store(2, Ordering::SeqCst);
    fixture.refresh.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    wait_for_ready_timeline(&fixture.session, 7).await;
    {
        let session = fixture.session.read().await;
        assert_eq!(session.origin_seq_highwater, Some(106));
        assert_eq!(session.proxy_next_seq, Some(7));
        assert!(session.origin_control.progress_generation > progress_generation);
        assert!(session.origin_control.acceptance_episode.is_none());
        assert!(last_episode_generation
            .is_some_and(|generation| session.origin_control.acceptance_generation.0 > generation));
    }
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("recovered normal manifest");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("recovered manifest utf8");
    assert!(body.contains("/000006.ts"));
    assert!(!body.contains("/terminal/"));
    assert!(!body.contains("#EXT-X-ENDLIST"));
    let uri = format!("/hls/shared/live/{}/{}/000006.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert!(!response_body(segment).await.is_empty());
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("recovered lease remains stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
}

pub(in crate::api::endpoints::hls_api::tests) struct StaleOriginServers {
    pub(in crate::api::endpoints::hls_api::tests) pinned: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) alternative: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) pinned_phase: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) alternative_phase: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) burst_candidates: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) pinned_candidates: Arc<AtomicUsize>,
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_stale_origin_servers() -> StaleOriginServers {
    let pinned_manifest = Arc::<[u8]>::from(regression_origin_manifest(100, 6));
    let alternative_manifest = Arc::<[u8]>::from(regression_origin_manifest(200, 6));
    let continued_manifest = Arc::<[u8]>::from(regression_origin_manifest(201, 6));
    let segment = Arc::<[u8]>::from(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
            .as_slice(),
    );
    let alternative_phase = Arc::new(AtomicUsize::new(0));
    let alternative_phase_for_handler = Arc::clone(&alternative_phase);
    let alternative_segment = Arc::clone(&segment);
    let alternative = spawn_test_binary_origin(Arc::new(move |path| {
        if path_has_extension(path, "m3u8") {
            let body = if alternative_phase_for_handler.load(Ordering::SeqCst) == 0 {
                Arc::clone(&alternative_manifest)
            } else {
                Arc::clone(&continued_manifest)
            };
            return TestBinaryOriginResponse::new(StatusCode::OK, body);
        }
        TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&alternative_segment))
    }))
    .await;
    let alternative_url =
        format!("{}/live/user/pass/12345.m3u8", alternative.base_url.replacen("127.0.0.1", "localhost", 1));
    let pinned_phase = Arc::new(AtomicUsize::new(0));
    let burst_candidates = Arc::new(AtomicUsize::new(0));
    let pinned_candidates = Arc::new(AtomicUsize::new(0));
    let handler_phase = Arc::clone(&pinned_phase);
    let handler_burst_candidates = Arc::clone(&burst_candidates);
    let handler_pinned_candidates = Arc::clone(&pinned_candidates);
    let handler_pinned_manifest = Arc::clone(&pinned_manifest);
    let pinned_segment = Arc::clone(&segment);
    let pinned = spawn_test_binary_origin(Arc::new(move |path| {
        if !path_has_extension(path, "m3u8") {
            return TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&pinned_segment));
        }
        match handler_phase.load(Ordering::SeqCst) {
            0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&handler_pinned_manifest)),
            1 => {
                let index = handler_burst_candidates.fetch_add(1, Ordering::SeqCst);
                if index.is_multiple_of(2) {
                    handler_pinned_candidates.fetch_add(1, Ordering::SeqCst);
                    TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&handler_pinned_manifest))
                } else {
                    TestBinaryOriginResponse::redirect(alternative_url.clone())
                }
            }
            _ => TestBinaryOriginResponse::redirect(alternative_url.clone()),
        }
    }))
    .await;
    StaleOriginServers { pinned, alternative, pinned_phase, alternative_phase, burst_candidates, pinned_candidates }
}

pub(in crate::api::endpoints::hls_api::tests) struct StaleOriginFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp_dir: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) servers: StaleOriginServers,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) refresh: OriginRefreshRequest,
    pub(in crate::api::endpoints::hls_api::tests) initial_visible_tail: u64,
    pub(in crate::api::endpoints::hls_api::tests) initial_progress_generation: u64,
    pub(in crate::api::endpoints::hls_api::tests) initial_origin_epoch: u64,
    pub(in crate::api::endpoints::hls_api::tests) initial_progress_at_ms: Option<u64>,
}

pub(in crate::api::endpoints::hls_api::tests) async fn stale_origin_fixture() -> StaleOriginFixture {
    let temp_dir = tempfile::tempdir().expect("stale-origin cache tempdir");
    let servers = spawn_stale_origin_servers().await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("reachable-stale-origin-input"),
        input_type: InputType::M3u,
        url: servers.pinned.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state =
        test_app_state_with_hls_proxy_and_inputs(test_beast_hls_proxy(temp_dir.path()), vec![Arc::new(input)]);
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", servers.pinned.base_url);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 1_000)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("reachable-stale-origin".to_string());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
    let refresh =
        regression_origin_refresh_request(&app_state, Arc::clone(&session), &manifest_url, Some(lease_id.clone()));
    assert!(trigger_origin_refresh_sync(refresh.clone()).await);
    wait_for_ready_timeline(&session, 6).await;
    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::initial(Duration::from_secs(10)),
    )
    .await
    .expect("READY pinned-origin manifest");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert_eq!(media_uri_count(&body), 3);
    let now_ms = super::super::current_time_millis();
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
        )
        .await
        .is_activated());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("activated pinned-origin lease");
    let initial_visible_tail =
        lease.last_manifest_snapshot.as_ref().expect("initial lease manifest snapshot").last_proxy_seq;
    let (initial_progress_generation, initial_origin_epoch, initial_progress_at_ms) = {
        let session = session.read().await;
        assert_eq!(session.origin_seq_highwater, Some(105));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
        (
            session.origin_control.progress_generation,
            session.origin_epoch,
            session.origin_control.last_media_progress_at_ms,
        )
    };
    StaleOriginFixture {
        _temp_dir: temp_dir,
        servers,
        app_state,
        session,
        proxy_session_id,
        lease_id,
        refresh,
        initial_visible_tail,
        initial_progress_generation,
        initial_origin_epoch,
        initial_progress_at_ms,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn observe_stale_origin(
    fixture: &mut StaleOriginFixture,
) -> HlsManifestAcceptanceDirective {
    let requests_before = fixture.servers.pinned.manifest_request_count();
    for _ in 0..2 {
        fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
        assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    }
    {
        let session = fixture.session.read().await;
        assert_eq!(session.origin_seq_highwater, Some(105));
        assert_eq!(session.origin_control.progress_generation, fixture.initial_progress_generation);
        assert_eq!(session.origin_control.last_media_progress_at_ms, fixture.initial_progress_at_ms);
        assert_eq!(session.origin_refresh.consecutive_failures, 0);
    }
    assert_eq!(fixture.servers.pinned.manifest_request_count().saturating_sub(requests_before), 2);
    assert_eq!(fixture.servers.alternative.manifest_request_count(), 0);
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("reachable stale origin keeps the committed live manifest");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("stale live manifest utf8");
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(!body.contains("/terminal/"));
    assert!(!body.contains("#EXT-X-ENDLIST"));
    {
        let mut session = fixture.session.write().await;
        for segment in session.segments.values_mut().filter(|segment| segment.proxy_seq > fixture.initial_visible_tail)
        {
            segment.duration_ms = 10_000;
        }
        session.origin_control.last_media_progress_at_ms = Some(0);
        session.advance_media_readiness_generation();
    }
    let directive = match crate::api::model::hls_manifest_acceptance_directive_for_session(
        &fixture.app_state.hls_ctx(),
        &fixture.session,
        &fixture.proxy_session_id,
    )
    .await
    {
        HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
        other => panic!("stale progress evidence must evaluate: {other:?}"),
    };
    assert!(directive.trigger.recovery_required());
    assert_eq!(fixture.session.read().await.origin_control.path_condition, HlsOriginPathCondition::PublicationLate);
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("stale progress evidence keeps the lease stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    directive
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_stale_origin_handoff(
    fixture: &mut StaleOriginFixture,
    directive: HlsManifestAcceptanceDirective,
) -> u64 {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    assert_eq!(fixture.app_state.hls.proxy.manifest_recovery_burst().level.plan(), plan);
    fixture.servers.pinned_phase.store(1, Ordering::SeqCst);
    let requests_before = fixture.servers.pinned.manifest_request_count();
    fixture.refresh.acceptance_directive = directive;
    fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    wait_for_ready_timeline(&fixture.session, 12).await;
    assert_eq!(
        fixture.servers.pinned.manifest_request_count().saturating_sub(requests_before),
        plan.total_candidates()
    );
    assert_eq!(fixture.servers.burst_candidates.load(Ordering::SeqCst), plan.total_candidates());
    assert!(fixture.servers.pinned_candidates.load(Ordering::SeqCst) > 0);
    assert!(fixture.servers.alternative.manifest_request_count() >= 2);
    let progress_generation = {
        let session = fixture.session.read().await;
        assert_eq!(session.origin_seq_highwater, Some(205));
        assert_eq!(session.proxy_next_seq, Some(12));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("localhost"));
        assert_eq!(session.origin_epoch, fixture.initial_origin_epoch.saturating_add(1));
        assert!(session.origin_control.progress_generation > fixture.initial_progress_generation);
        session.origin_control.progress_generation
    };
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("cross-host recovery manifest");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("recovered manifest utf8");
    assert!(body.contains("/000006.ts"));
    assert!(body.contains("#EXT-X-DISCONTINUITY"));
    assert!(!body.contains("/terminal/"));
    assert!(!body.contains("#EXT-X-ENDLIST"));
    let uri = format!("/hls/shared/live/{}/{}/000006.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert!(!response_body(segment).await.is_empty());
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("recovered lease remains stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    progress_generation
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_stale_origin_continuation(
    fixture: &mut StaleOriginFixture,
    progress_generation: u64,
) {
    fixture.servers.alternative_phase.store(1, Ordering::SeqCst);
    fixture.servers.pinned_phase.store(2, Ordering::SeqCst);
    fixture.refresh.acceptance_directive = HlsManifestAcceptanceDirective::none();
    fixture.refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert!(trigger_origin_refresh_sync(fixture.refresh.clone()).await);
    wait_for_ready_timeline(&fixture.session, 13).await;
    {
        let session = fixture.session.read().await;
        assert_eq!(session.origin_seq_highwater, Some(206));
        assert_eq!(session.proxy_next_seq, Some(13));
        assert_eq!(session.origin_epoch, fixture.initial_origin_epoch.saturating_add(1));
        assert!(session.origin_control.progress_generation > progress_generation);
    }
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("continued alternative-origin timeline");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("continued manifest utf8");
    assert!(body.contains("/000012.ts"));
    assert!(!body.contains("/terminal/"));
    assert!(!body.contains("#EXT-X-ENDLIST"));
    let uri = format!("/hls/shared/live/{}/{}/000012.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let segment = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert!(!response_body(segment).await.is_empty());
}

pub(in crate::api::endpoints::hls_api::tests) struct PreparedTerminalCutoverFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp_dir: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) request_url: String,
    pub(in crate::api::endpoints::hls_api::tests) base_manifest: HlsLeaseManifestSnapshot,
    pub(in crate::api::endpoints::hls_api::tests) asset_buffer: TransportStreamBuffer,
    pub(in crate::api::endpoints::hls_api::tests) asset: Arc<HlsTerminalMediaAsset>,
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_terminal_cutover_bundle(
    app_state: &Arc<AppState>,
    base_manifest: &HlsLeaseManifestSnapshot,
) -> (TransportStreamBuffer, Arc<HlsTerminalMediaAsset>) {
    let asset_buffer = app_state
        .app_config
        .custom_stream_response
        .load_full()
        .as_ref()
        .and_then(|responses| responses.channel_unavailable.as_ref())
        .cloned()
        .expect("configured terminal renderer");
    let asset = snapshot_terminal_media_asset(&asset_buffer).expect("terminal asset snapshot");
    let key = prepared_terminal_bundle_key(&asset, base_manifest.target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state = app_state.hls.proxy.start_prepared_terminal_bundle(
        Arc::clone(&asset),
        base_manifest.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => app_state
            .hls
            .proxy
            .wait_for_prepared_terminal_bundle(key)
            .await
            .expect("prepared terminal bundle completion"),
        state => state,
    };
    assert!(matches!(
        state,
        HlsPreparedTerminalBundleState::Ready { ref bundle }
            if bundle.key == key
                && bundle.segments.len() == usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
    ));
    assert_eq!(asset_buffer.finite_hls_render_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    (asset_buffer, asset)
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepared_terminal_cutover_fixture(
) -> PreparedTerminalCutoverFixture {
    let temp_dir = tempfile::tempdir().expect("terminal cutover tempdir");
    let origin = spawn_test_encrypted_hls_origin(
        AES_TEST_MANIFEST,
        Arc::from(AES_TEST_KEY_BYTES),
        Arc::from(AES_TEST_PLAINTEXT_SEGMENT),
    )
    .await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("terminal-regression-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state =
        test_app_state_with_hls_proxy_and_inputs(test_beast_hls_proxy(temp_dir.path()), vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    enable_channel_unavailable_custom_response(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/channel/index.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let lease_id = HlsAccessLeaseId("prepared-terminal-cutover".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &lease_id).await;
    let response = super::super::try_hls_cache_canonical_manifest_response(
        &app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: &request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(&request_url),
            input: &input,
            origin_source,
        },
        HeaderMap::new(),
        None,
        "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("AES shared session cold start");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("AES manifest utf8");
    assert!(body.contains("#EXT-X-KEY:METHOD=AES-128"));
    let session = app_state.hls.proxy.sessions().get_by_key(&session_key).await.expect("AES shared session");
    wait_for_ready_timeline(&session, 6).await;
    let now_ms = super::super::current_time_millis();
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 },
        )
        .await
        .is_activated());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("live AES lease");
    let base_manifest = lease.last_manifest_snapshot.as_ref().expect("frozen AES lease manifest").clone();
    assert_eq!(base_manifest.target_duration_ms, 12_000);
    assert_eq!(base_manifest.visible_segments.len(), 3);
    assert!(base_manifest.active_encryption.is_some());
    let (asset_buffer, asset) = prepare_terminal_cutover_bundle(&app_state, &base_manifest).await;
    for segment in base_manifest.visible_segments.iter() {
        let response = get_response(Arc::clone(&app_state), &segment.uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response_body(response).await.is_empty());
    }
    PreparedTerminalCutoverFixture {
        _temp_dir: temp_dir,
        origin,
        app_state,
        session,
        proxy_session_id,
        lease_id,
        request_url,
        base_manifest,
        asset_buffer,
        asset,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn apply_terminal_cutover_pressure(
    fixture: &PreparedTerminalCutoverFixture,
) {
    let mut session = fixture.session.write().await;
    session.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
    let transition_buffer_seq = fixture.base_manifest.last_proxy_seq.saturating_add(1);
    let commit_guard_seq = transition_buffer_seq.saturating_add(1);
    for segment in
        session.segments.values_mut().filter(|segment| segment.proxy_seq > fixture.base_manifest.last_proxy_seq)
    {
        if segment.proxy_seq == transition_buffer_seq {
            segment.duration_ms = 1_000;
        } else if segment.proxy_seq == commit_guard_seq {
            segment.duration_ms = 2_800;
        } else {
            segment.status = SegmentCacheStatus::Expired;
        }
    }
    session.advance_media_readiness_generation();
}

pub(in crate::api::endpoints::hls_api::tests) async fn terminal_cutover_acceptance_directive(
    fixture: &PreparedTerminalCutoverFixture,
) -> HlsManifestAcceptanceDirective {
    apply_terminal_cutover_pressure(fixture).await;
    let directive = match crate::api::model::hls_manifest_acceptance_directive_for_session(
        &fixture.app_state.hls_ctx(),
        &fixture.session,
        &fixture.proxy_session_id,
    )
    .await
    {
        HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
        other => panic!("deterministic recovery-pressure snapshot must evaluate: {other:?}"),
    };
    assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::RecoveryRequired);
    let bundle_key = prepared_terminal_bundle_key(
        &fixture.asset,
        fixture.base_manifest.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let timing = directive.timing_seed.expect("prepared acceptance timing seed");
    assert_eq!(timing.required_terminal_media_key, Some(bundle_key));
    assert_eq!(timing.terminal_media_preparation, HlsTerminalMediaPreparationState::Ready { key: bundle_key });
    directive
}

pub(in crate::api::endpoints::hls_api::tests) async fn exhaust_terminal_cutover_recovery(
    fixture: &PreparedTerminalCutoverFixture,
    directive: HlsManifestAcceptanceDirective,
) -> HlsAccessLease {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let requests_before = fixture.origin.manifest_request_count();
    let mut refresh = regression_origin_refresh_request(
        &fixture.app_state,
        Arc::clone(&fixture.session),
        &fixture.request_url,
        Some(fixture.lease_id.clone()),
    );
    refresh.acceptance_directive = directive;
    refresh.now_ms = fixture.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert!(trigger_origin_refresh_sync(refresh).await);
    assert!(fixture.origin.manifest_request_count().saturating_sub(requests_before) >= plan.total_candidates());
    apply_terminal_cutover_pressure(fixture).await;
    let bundle_key = prepared_terminal_bundle_key(
        &fixture.asset,
        fixture.base_manifest.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    {
        let session = fixture.session.read().await;
        let episode = session.origin_control.acceptance_episode.as_ref().expect("exhausted acceptance episode");
        assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
        assert!(episode.full_burst_completed);
        assert_eq!(episode.timing().required_terminal_media_key, Some(bundle_key));
        assert_eq!(
            episode.timing().terminal_media_preparation,
            HlsTerminalMediaPreparationState::Ready { key: bundle_key }
        );
        assert!(episode.exhaustion_reason().is_some());
    }
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("pressured live lease");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(
        lease.playback_cursor.highest_contiguous_completed_proxy_seq,
        Some(fixture.base_manifest.last_proxy_seq)
    );
    {
        let session = fixture.session.read().await;
        let ready_after_tail = session
            .segments
            .values()
            .filter(|segment| segment.proxy_seq > fixture.base_manifest.last_proxy_seq)
            .filter_map(|segment| {
                matches!(segment.status, SegmentCacheStatus::Ready { .. })
                    .then_some((segment.proxy_seq, segment.duration_ms))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ready_after_tail,
            vec![
                (fixture.base_manifest.last_proxy_seq.saturating_add(1), 1_000),
                (fixture.base_manifest.last_proxy_seq.saturating_add(2), 2_800),
            ]
        );
        assert!(session.origin_control.path_condition.is_degraded());
    }
    lease
}

pub(in crate::api::endpoints::hls_api::tests) async fn commit_prepared_terminal_cutover(
    fixture: &PreparedTerminalCutoverFixture,
    pressured_lease: &HlsAccessLease,
) -> (u64, u64) {
    let cutover_now_ms = pressured_lease
        .playback_cursor
        .first_segment_completed_at_ms
        .expect("measured lease playback start")
        .saturating_add(fixture.base_manifest.playlist_duration_ms);
    let first = commit_terminal_tail_if_lease_reserve_requires_cutover(
        &fixture.app_state.hls_ctx(),
        &fixture.session,
        &fixture.proxy_session_id,
        pressured_lease,
        cutover_now_ms,
    )
    .await;
    assert_eq!(first, HlsTerminalResolution::Committed);
    let terminal_lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease remains stored");
    let second = commit_terminal_tail_if_lease_reserve_requires_cutover(
        &fixture.app_state.hls_ctx(),
        &fixture.session,
        &fixture.proxy_session_id,
        &terminal_lease,
        cutover_now_ms.saturating_add(1),
    )
    .await;
    assert_eq!(second, HlsTerminalResolution::Committed);
    let HlsLeasePlaybackMode::TerminalTail(plan) = terminal_lease.playback_mode else {
        panic!("prepared terminal tail must commit");
    };
    assert_eq!(plan.segment_count, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    (plan.generation.0, cutover_now_ms)
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_prepared_terminal_cutover_manifest(
    fixture: &PreparedTerminalCutoverFixture,
    generation: u64,
) {
    let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", fixture.proxy_session_id.0, fixture.lease_id.0);
    let response = get_response(Arc::clone(&fixture.app_state), &manifest_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest utf8");
    let live_tail_path = format!(
        "/{}/{}/{:06}.ts",
        fixture.proxy_session_id.0, fixture.lease_id.0, fixture.base_manifest.last_proxy_seq
    );
    let terminal_prefix = format!("/{}/{}/terminal/{generation}/", fixture.proxy_session_id.0, fixture.lease_id.0);
    assert!(body.contains(&live_tail_path));
    assert_eq!(body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(body.find(&live_tail_path) < body.find("#EXT-X-DISCONTINUITY\n"));
    let key_reset = body.find("#EXT-X-KEY:METHOD=NONE\n").expect("AES-to-clear key reset");
    let discontinuity = body.find("#EXT-X-DISCONTINUITY\n").expect("terminal discontinuity");
    assert!(key_reset < discontinuity);
    assert_eq!(body.matches(&terminal_prefix).count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    for index in 0..HLS_TERMINAL_TAIL_SEGMENT_COUNT {
        assert!(body.contains(&format!("{terminal_prefix}{index}.ts")));
    }
    let duration_ms = fixture.asset.duration_ms();
    let extinf = format!("#EXTINF:{}.{:03},", duration_ms / 1_000, duration_ms % 1_000);
    assert_eq!(body.matches(&extinf).count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    let renders_before = fixture.asset_buffer.finite_hls_render_count();
    let zero_uri =
        format!("/hls/shared/live/{}/{}/terminal/{generation}/0.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let one_uri =
        format!("/hls/shared/live/{}/{}/terminal/{generation}/1.ts", fixture.proxy_session_id.0, fixture.lease_id.0);
    let zero = response_body(get_response(Arc::clone(&fixture.app_state), &zero_uri, None).await).await;
    let one = response_body(get_response(Arc::clone(&fixture.app_state), &one_uri, None).await).await;
    assert_ne!(zero, one);
    assert_eq!(fixture.asset_buffer.finite_hls_render_count(), renders_before);
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_terminal_cutover_sticky_after_recovery(
    fixture: &PreparedTerminalCutoverFixture,
    generation: u64,
    cutover_now_ms: u64,
) {
    {
        let mut session = fixture.session.write().await;
        let recovered_manifest =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:83\n#EXT-X-TARGETDURATION:12\n#EXTINF:12.0,\n83.ts\n");
        session.apply_origin_manifest(&recovered_manifest).expect("later shared-session recovery");
        session.origin_control.record_media_progress(cutover_now_ms.saturating_add(1), 12_000);
    }
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("sticky terminal lease");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalTail(ref plan) if plan.generation.0 == generation
    ));
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_other_live_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId("other-live-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            60_000,
        ))
        .await;
    lease_id
}

pub(in crate::api::endpoints::hls_api::tests) fn assert_terminal_plan_unchanged(
    playback_mode: &HlsLeasePlaybackMode,
    expected_generation: HlsTerminalTailGeneration,
    terminal_path: HlsTerminalSegmentPath,
    expected_bytes: &bytes::Bytes,
) {
    let HlsLeasePlaybackMode::TerminalTail(plan) = playback_mode else {
        panic!("shared lease operation cannot reactivate the terminal lease");
    };
    assert_eq!(plan.generation, expected_generation);
    assert_eq!(plan.segment_bytes(terminal_path).as_ref(), Some(expected_bytes));
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_conflicted_standalone_fallback(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) {
    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(app_state, proxy_session_id, lease_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone manifest utf8");
    assert!(body.contains("#EXT-X-ENDLIST"));
    assert!(
        !body.contains("/hls/shared/live/"),
        "standalone response must not expose a normal segment without a readiness path"
    );
    assert_eq!(
        session.read().await.origin_control.path_condition,
        HlsOriginPathCondition::AcceptanceConflict,
        "lease-local fallback must not relax deterministic conflict evidence"
    );
}

pub(in crate::api::endpoints::hls_api::tests) fn test_beast_hls_proxy(
    cache_path: &std::path::Path,
) -> Arc<HlsProxyManager> {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto {
        cache_path: Some(cache_path.to_string_lossy().into_owned()),
        strip: HlsStripConfigDto { mode: HlsStripMode::Segments, value: 3 },
        max_segments_prefetch: 6,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto { level: HlsManifestRecoveryBurstLevel::Beast },
        ..HlsCacheConfigDto::default()
    });
    Arc::new(HlsProxyManager::from_hls_cache_config(Some(&config)))
}

pub(in crate::api::endpoints::hls_api::tests) struct PublicationLateFixture {
    pub(in crate::api::endpoints::hls_api::tests) origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) origin_phase: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) access_lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) media_playlist_uri: String,
}

pub(in crate::api::endpoints::hls_api::tests) async fn publication_late_fixture() -> PublicationLateFixture {
    let initial_manifest = Arc::<[u8]>::from(regression_origin_manifest(123, 6));
    let progressed_manifest = Arc::<[u8]>::from(regression_origin_manifest(124, 6));
    let segment = Arc::<[u8]>::from(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
            .as_slice(),
    );
    let origin_phase = Arc::new(AtomicUsize::new(0));
    let origin_phase_for_handler = Arc::clone(&origin_phase);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        if path_has_extension(path, "m3u8") {
            return match origin_phase_for_handler.load(Ordering::SeqCst) {
                0 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&initial_manifest)),
                1 => TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&progressed_manifest)),
                _ => TestBinaryOriginResponse::new(
                    StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                    Arc::<[u8]>::from(&b"retry"[..]),
                ),
            };
        }
        TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&segment))
    }))
    .await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("publication-late-request-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    enable_channel_unavailable_custom_response(&app_state);
    configure_default_test_server(&app_state);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let origin_manifest_url = format!("{}/channel/index.m3u8", origin.base_url);
    cache_test_m3u_hls_item(&app_state, &target, test_m3u_hls_item(&input, 12345, "channel-a", &origin_manifest_url))
        .await;
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let entry_path = super::super::build_virtual_hls_entry_path(&target, &input, &user, 12345);
    let entry_response = super::super::handle_hls_stream_request(
        &test_fingerprint(),
        &app_state,
        &user,
        &target,
        None,
        None,
        &origin_manifest_url,
        None,
        test_hls_entry_stream_context(12345, "channel-a", None),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &entry_path,
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();
    let (_, media_playlist_uri) = single_variant_master_playlist(entry_response).await;
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());
    let initial_media_response = get_response(Arc::clone(&app_state), &media_playlist_uri, None).await;
    assert_eq!(initial_media_response.status(), StatusCode::OK);
    let initial_media_body =
        String::from_utf8(response_body(initial_media_response).await.to_vec()).expect("initial media utf8");
    let last_segment_uri = initial_media_body
        .lines()
        .rfind(|line| line.starts_with("/hls/shared/live/") && path_has_extension(line, "ts"))
        .expect("initial manifest segment");
    assert_eq!(get_status(Arc::clone(&app_state), last_segment_uri).await, StatusCode::OK);
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id)
        .await
        .expect("shared publication-late session");
    PublicationLateFixture {
        origin,
        origin_phase,
        app_state,
        session,
        proxy_session_id,
        access_lease_id,
        media_playlist_uri,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn refresh_publication_late_fixture(
    fixture: &PublicationLateFixture,
) -> HlsAccessLease {
    let progress_generation_before = fixture.session.read().await.origin_control.progress_generation;
    {
        let mut session = fixture.session.write().await;
        session.origin_control.last_media_progress_at_ms = Some(0);
        session.origin_refresh.next_fetch_allowed_at_ms = 0;
    }
    let requests_before = fixture.origin.manifest_request_count();
    fixture.origin_phase.store(1, Ordering::SeqCst);
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("refreshed media utf8");
    assert!(!body.contains("#EXT-X-ENDLIST"));
    assert!(!body.contains("/terminal/"));
    assert!(fixture.origin.manifest_request_count() > requests_before);
    {
        let session = fixture.session.read().await;
        assert!(session.origin_control.progress_generation > progress_generation_before);
        assert_eq!(session.origin_seq_highwater, Some(129));
    }
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("publication-late lease remains stored");
    assert_eq!(lease.state, HlsAccessLeaseState::Activated);
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(fixture.app_state.hls.proxy.terminal_pending().owner_count(), 0);
    lease
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_publication_late_terminal_pressure(
    fixture: &PublicationLateFixture,
    base_manifest: &HlsLeaseManifestSnapshot,
) {
    let terminal_response = fixture.app_state.app_config.custom_stream_response.load_full();
    let terminal_asset = snapshot_terminal_media_asset(
        terminal_response
            .as_ref()
            .and_then(|response| response.channel_unavailable.as_ref())
            .expect("configured terminal asset"),
    )
    .expect("compatible terminal asset");
    let bundle_key = prepared_terminal_bundle_key(
        &terminal_asset,
        base_manifest.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = fixture.app_state.hls.proxy.start_prepared_terminal_bundle(
        terminal_asset,
        base_manifest.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => fixture
            .app_state
            .hls
            .proxy
            .wait_for_prepared_terminal_bundle(bundle_key)
            .await
            .expect("terminal bundle completion"),
        state => state,
    };
    assert!(matches!(state, HlsPreparedTerminalBundleState::Ready { .. }));
    let mut session = fixture.session.write().await;
    for segment in session.segments.values_mut().filter(|segment| segment.proxy_seq > base_manifest.last_proxy_seq) {
        segment.duration_ms = 1;
    }
    session.origin_control.last_media_progress_at_ms = Some(0);
    session.origin_refresh.next_fetch_allowed_at_ms = 0;
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_publication_late_terminal_result(
    fixture: &PublicationLateFixture,
) {
    let requests_before = fixture.origin.manifest_request_count();
    fixture.origin_phase.store(2, Ordering::SeqCst);
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease remains stored");
    let recovery_plan = fixture.app_state.hls.proxy.manifest_recovery_burst().level.plan();
    assert!(
        fixture.origin.manifest_request_count().saturating_sub(requests_before) >= recovery_plan.total_candidates()
    );
    match lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) => {
            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest utf8");
            assert!(body.contains("#EXT-X-ENDLIST"));
            assert!(body.contains("/terminal/"));
        }
        HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        HlsLeasePlaybackMode::Live => {
            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(response_body(response).await.to_vec()).expect("live manifest utf8");
            assert!(body.starts_with("#EXTM3U"));
            assert!(!body.contains("#EXT-X-ENDLIST"));
        }
        HlsLeasePlaybackMode::Ended => {
            panic!("active hard-failed lease must not become ended without terminal publication")
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn regression_origin_refresh_request(
    app_state: &Arc<AppState>,
    session: HlsSessionHandle,
    manifest_url: &str,
    access_lease_id: Option<HlsAccessLeaseId>,
) -> OriginRefreshRequest {
    OriginRefreshRequest {
        app_config: Arc::clone(&app_state.app_config),
        session,
        origin_entry: LiveHlsOriginEntry::parse(manifest_url).expect("regression origin entry"),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: app_state.http_clients.no_redirect.load().as_ref().clone(),
        use_manual_redirects: false,
        segment_cache: Arc::clone(app_state.hls.proxy.segment_cache()),
        hls_proxy: Arc::clone(&app_state.hls.proxy),
        segment_repair: Arc::clone(app_state.hls.proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls.proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls.proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls.proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: app_state.hls.proxy.manifest_recovery_burst(),
        strip: app_state.hls.proxy.strip(),
        retry_policy: RetryPolicy { delays_ms: [0; 5], jitter_max_ms: 0 },
        reverse_proxy_rewrite_secret: app_state.get_encrypt_secret().to_vec(),
        transient_resource_ttl_ms: app_state.hls.proxy.transient_resource_ttl_ms(),
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id,
        disabled_headers: None,
        now_ms: super::super::current_time_millis(),
        origin_io: None,
        post_refresh_runtime: None,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn wait_for_ready_timeline(
    session: &HlsSessionHandle,
    expected_ready: usize,
) {
    let wait = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ready = session
                .read()
                .await
                .segments
                .values()
                .filter(|segment| matches!(segment.status, SegmentCacheStatus::Ready { .. }))
                .count();
            if ready >= expected_ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if wait.is_err() {
        let session = session.read().await;
        let statuses =
            session.segments.values().map(|segment| (segment.proxy_seq, segment.status.clone())).collect::<Vec<_>>();
        panic!("READY timeline deadline: expected={expected_ready} statuses={statuses:?}");
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn extend_ready_segment_as_sparse_file(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_seq: u64,
    logical_size: u64,
) {
    let cache_key = session.read().await.segments.get(&proxy_seq).expect("mapped sparse segment").cache_key.clone();
    let metadata = app_state
        .hls
        .proxy
        .segment_cache()
        .metadata(&cache_key)
        .await
        .expect("sparse cache metadata read")
        .expect("READY sparse cache object");
    let file = tokio::fs::OpenOptions::new().write(true).open(&metadata.path).await.expect("sparse cache object opens");
    file.set_len(logical_size).await.expect("sparse cache object extends");
    let mut session = session.write().await;
    let segment = session.segments.get_mut(&proxy_seq).expect("sparse segment remains mapped");
    segment.status =
        SegmentCacheStatus::Ready { content_length: logical_size, ready_at_ms: super::super::current_time_millis() };
    session.advance_media_readiness_generation();
    session.render_and_store_manifest(super::super::current_time_millis()).expect("sparse timeline renders");
}

pub(in crate::api::endpoints::hls_api::tests) async fn publish_test_manifest_and_exhaust_configured_acceptance(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    snapshot: HlsLeaseManifestSnapshot,
    now_ms: u64,
) {
    let target_duration_ms = snapshot.target_duration_ms;
    let publication_guard = app_state
        .hls
        .proxy
        .prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms)
        .await
        .expect("live lease accepts publication preparation");
    assert!(app_state
        .hls
        .proxy
        .commit_access_lease_manifest_publication(lease_id, proxy_session_id, publication_guard, snapshot, now_ms,)
        .await
        .is_committed());

    let terminal_response = app_state.app_config.custom_stream_response.load_full();
    let terminal_asset = terminal_response
        .as_ref()
        .and_then(|responses| responses.channel_unavailable.as_ref())
        .and_then(|buffer| snapshot_terminal_media_asset(buffer).ok())
        .expect("configured terminal test asset");
    let terminal_key =
        prepared_terminal_bundle_key(&terminal_asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state = app_state.hls.proxy.start_prepared_terminal_bundle(
        terminal_asset,
        target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => app_state
            .hls
            .proxy
            .wait_for_prepared_terminal_bundle(terminal_key)
            .await
            .expect("terminal bundle completion"),
        state => state,
    };
    assert!(matches!(
        state,
        HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.key == terminal_key
    ));

    let session =
        app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.expect("warm shared session");
    let mut session = session.write().await;
    session.origin_control.record_media_progress(now_ms, target_duration_ms);
    let burst_plan = app_state.hls.proxy.manifest_recovery_burst().level.plan();
    let operation_timeout = HlsOperationTimeoutMs::from_millis(app_state.hls.proxy.origin_manifest_timeout_ms());
    let expected_eta = HlsRecoveryEtaMs::from_millis(app_state.hls.proxy.origin_manifest_timeout_ms());
    let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms: now_ms,
        burst_plan,
        target_duration_ms,
        transition_margin: HlsTransitionMarginMs::from_millis(target_duration_ms),
        workload: HlsRecoveryWorkload::clear_fetch(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: Some(terminal_key),
        terminal_media_preparation: HlsTerminalMediaPreparationState::Ready { key: terminal_key },
        policy: HlsRecoveryTimingPolicy::new(operation_timeout, operation_timeout, expected_eta, expected_eta),
    });
    session.origin_control.begin_acceptance_episode(
        now_ms,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &timing,
    );
    session.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
    episode.hold_after_uncommitted_burst(None, None);
}
