use super::{
    super::{
        get_response,
        terminal_tail::{
            commit_prepared_terminal_cutover, exhaust_terminal_cutover_recovery, prepared_terminal_cutover_fixture,
            terminal_cutover_acceptance_directive, PreparedTerminalCutoverFixture,
        },
        AppState,
    },
    prepare_revisions, response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response,
};
use axum::http::StatusCode;
use shared::model::{HlsCacheConfigDto, HlsStartupMode, HlsStripMode};
use std::{fmt::Write as _, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{HlsCacheConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_hls::api::{
    HlsAccessLeaseId, HlsAccessLeaseState, HlsCachedManifestOptions, HlsLeasePlaybackMode, HlsOriginAccountBinding,
    HlsProxyManager, HlsSessionHandle, HlsSessionKey, SegmentCacheStatus,
};

const MODES: [HlsStartupMode; 4] =
    [HlsStartupMode::Conservative, HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic];

fn manifest(first: u64, short: Option<u64>) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for sequence in first..first + 4 {
        let duration_ms = if Some(sequence) == short { 5_000 } else { 6_000 };
        writeln!(
            body,
            "#EXTINF:{}.{:03},\nhttp://origin.invalid/{sequence}.ts",
            duration_ms / 1_000,
            duration_ms % 1_000
        )
        .unwrap();
    }
    super::super::normal_manifest(&body)
}

fn ready(state: &mut tuliprox_hls::api::HlsSession) {
    let sequences = state.segments.keys().copied().collect::<Vec<_>>();
    if state.startup_mode() == HlsStartupMode::Conservative {
        for seq in sequences {
            let segment = state.segments.get_mut(&seq).unwrap();
            if !matches!(segment.status, SegmentCacheStatus::Ready { .. }) {
                segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: current_time_millis() };
            }
        }
    } else {
        prepare_revisions(state, &sequences);
    }
}

async fn cached_live_response(
    app: &Arc<AppState>,
    session: &HlsSessionHandle,
    lease_id: &HlsAccessLeaseId,
    strip: &StripConfig,
) -> axum::response::Response {
    try_test_hls_cached_manifest_response(
        app,
        session,
        lease_id,
        HlsAccessLeaseState::Activated,
        strip,
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .unwrap()
}

// Reentry must publish the complete sliding window without renewing media activity.
async fn assert_reentry_window_publication(first: u64, rebase: u64, proxy_start: u64, short: Option<u64>) {
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            max_segments_prefetch: 0,
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 3;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(Arc::clone(&manager));
        let base = current_time_millis();
        manager.follow_tokio_clock_for_test(base);
        let session =
            manager.get_or_create_session(HlsSessionKey::new(1, "publication-handoff"), b"secret", base).await;
        let proxy = session.read().await.proxy_session_id.clone();
        {
            let mut state = session.write().await;
            state.proxy_next_seq = Some(proxy_start);
            state.apply_origin_manifest(&manifest(first, short)).unwrap();
            ready(&mut state);
            state.activity.last_authorized_media_at_ms = Some(base);
            state.origin_account_binding =
                Some(HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &proxy, base));
            state.render_and_store_manifest(base).unwrap();
        }
        let lease_id = HlsAccessLeaseId(super::super::support::grant_hls_proxy_lease(&app, &proxy.0).await);
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        for _ in 0..2 {
            let response = cached_live_response(&app, &session, &lease_id, &strip).await;
            assert_eq!(response.status(), StatusCode::OK, "{mode:?}: baseline");
        }
        let mut lease = manager.access_lease_response_snapshot(&lease_id, &proxy, base).await.unwrap();
        lease.active_until_ms = Some(base + 120_000);
        lease.valid_until_ms = base + 180_000;
        manager.prepare_access_lease(lease).await;
        let epoch = session.read().await.origin_epoch;
        let cached = session.read().await.segments.clone();
        tokio::time::advance(Duration::from_secs(22)).await;
        let now = manager.now_ms();
        {
            let mut state = session.write().await;
            assert_eq!(state.account_binding_protection(now), tuliprox_hls::api::HlsAccountBindingProtection::Expired);
            let owner = tuliprox_hls::api::build_hls_origin_session_owner(&proxy);
            state.restart_admitted_origin_account(HlsOriginAccountBinding::rebound(
                Arc::from("input"),
                Arc::from("account"),
                owner,
                2,
                now,
            ));
            let host = state.origin_epoch_effective_host_id.unwrap();
            state.apply_origin_rebase_manifest(&manifest(rebase, short), host, false).unwrap();
            ready(&mut state);
            state.render_and_store_manifest(now).unwrap();
        }
        for (offset, expected_first) in [(0, rebase), (1, rebase + 1)] {
            if offset != 0 {
                tokio::time::advance(Duration::from_secs(4)).await;
                let mut state = session.write().await;
                state.apply_origin_manifest(&manifest(expected_first, short)).unwrap();
                ready(&mut state);
                state.render_and_store_manifest(manager.now_ms()).unwrap();
            }
            for _ in 0..2 {
                let response = cached_live_response(&app, &session, &lease_id, &strip).await;
                assert_eq!(response.status(), StatusCode::OK, "{mode:?}: window={expected_first}");
                let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
                assert!(!body.contains("#EXT-X-ENDLIST"));
                let current =
                    manager.access_lease_response_snapshot(&lease_id, &proxy, manager.now_ms()).await.unwrap();
                let snapshot = current.last_manifest_snapshot.unwrap();
                let start = proxy_start + expected_first - first;
                assert_eq!(snapshot.visible_proxy_seqs().collect::<Vec<_>>(), (start..start + 4).collect::<Vec<_>>());
                assert_eq!(current.playback_mode, HlsLeasePlaybackMode::Live);
            }
        }
        let state = session.read().await;
        assert_eq!(state.origin_epoch, epoch, "{mode:?}: keep the continuous origin epoch");
        assert_eq!(state.activity.last_authorized_media_at_ms, Some(base), "polling is not media activity");
        for seq in proxy_start + rebase - first..proxy_start + 4 {
            assert_eq!(state.segments[&seq].cache_key, cached[&seq].cache_key);
            assert_eq!(state.segments[&seq].status, cached[&seq].status);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn reentry_preserves_two_cached_segments_in_all_modes() {
    assert_reentry_window_publication(100, 102, 10, None).await;
}

#[tokio::test(start_paused = true)]
async fn reentry_preserves_three_cached_segments_with_short_tail_in_all_modes() {
    assert_reentry_window_publication(100, 101, 20, Some(104)).await;
}

async fn terminal_fixture() -> (PreparedTerminalCutoverFixture, HlsAccessLeaseState) {
    let fixture = prepared_terminal_cutover_fixture().await;
    let directive = terminal_cutover_acceptance_directive(&fixture).await;
    let pressured = exhaust_terminal_cutover_recovery(&fixture, directive).await;
    assert_eq!(pressured.playback_mode, HlsLeasePlaybackMode::Live);
    let state = pressured.state;
    assert_eq!(state, HlsAccessLeaseState::Activated);
    assert!(
        fixture
            .app_state
            .hls
            .proxy
            .prepare_access_lease_manifest_publication(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                current_time_millis(),
            )
            .await
            .is_some(),
        "live publication must be eligible before the terminal commit"
    );
    commit_prepared_terminal_cutover(&fixture, &pressured).await;
    assert!(
        fixture
            .app_state
            .hls
            .proxy
            .prepare_access_lease_manifest_publication(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                current_time_millis(),
            )
            .await
            .is_none(),
        "a committed terminal must close ordinary live publication"
    );
    (fixture, state)
}

#[tokio::test]
async fn terminal_tail_committed_before_http_request_remains_playable() {
    let (fixture, _) = terminal_fixture().await;
    let uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", fixture.proxy_session_id.0, fixture.lease_id.0);
    let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    assert!(body.contains("/terminal/") && body.ends_with("#EXT-X-ENDLIST\n"));
    let terminal_uri = body.lines().find(|line| line.contains("/terminal/") && !line.starts_with('#')).unwrap();
    let segment = get_response(Arc::clone(&fixture.app_state), terminal_uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert!(!response_body(segment).await.is_empty());
}

#[tokio::test]
async fn terminal_commit_before_publication_preparation_preserves_terminal_response() {
    // A request reads Live before the terminal commit and enters cached publication afterward.
    let (fixture, checked_live_state) = terminal_fixture().await;
    let terminal = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .unwrap();
    assert!(matches!(terminal.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)));
    let direct = crate::api::endpoints::hls_api::hls_terminal_playback_response(
        &fixture.app_state,
        &terminal,
        &fixture.proxy_session_id,
        &fixture.lease_id,
    )
    .unwrap();
    assert_eq!(direct.status(), StatusCode::OK);
    let expected_body = response_body(direct).await;
    let response = try_test_hls_cached_manifest_response(
        &fixture.app_state,
        &fixture.session,
        &fixture.lease_id,
        checked_live_state,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .unwrap();
    let status = response.status();
    let body = response_body(response).await;
    assert_eq!(status, StatusCode::OK, "a committed terminal reply must survive the lost live-publication guard");
    assert_eq!(body, expected_body);
}
