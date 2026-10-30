use super::{
    enable_channel_unavailable_custom_response, get_response, map_hls_map, map_ready_segment, response_body,
    terminal_tail::{
        assert_prepared_terminal_cutover_manifest, assert_terminal_cutover_sticky_after_recovery,
        commit_prepared_terminal_cutover, exhaust_terminal_cutover_recovery, prepared_terminal_cutover_fixture,
        publish_test_manifest_and_exhaust_configured_acceptance, terminal_cutover_acceptance_directive,
    },
    terminal_test_asset, terminal_test_plan_shape, terminalize_existing_test_lease, test_app_state_with_hls_proxy,
    test_hls_access_context,
};
use crate::api::model::{
    HlsAccessLeaseId, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
    HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMapSignature, HlsMediaContainer, HlsProxyManager,
    ProxySessionId,
};
use axum::http::{header, StatusCode};
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

async fn within_real_second<F: std::future::Future>(future: F) -> F::Output {
    tokio::pin!(future);
    let started = std::time::Instant::now();
    loop {
        let polled = futures::poll!(future.as_mut());
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "real-time safety deadline");
        if let std::task::Poll::Ready(value) = polled {
            return value;
        }
        // Keep the paused runtime runnable: a response after commit must not
        // require Tokio to auto-advance to an outstanding origin timeout.
        tokio::task::yield_now().await;
    }
}

struct DelayedTerminalRefresh {
    fixture: super::terminal_tail::PublicationLateFixture,
    release: tokio::sync::watch::Sender<bool>,
    blocked: tokio::sync::watch::Receiver<bool>,
}

type PendingRefresh = futures::future::Shared<futures::future::BoxFuture<'static, bool>>;

impl DelayedTerminalRefresh {
    async fn new(mode: shared::model::HlsStartupMode) -> Self {
        let _ = env_logger::Builder::from_env(env_logger::Env::default()).is_test(true).try_init();
        let playlist = |first: u64| {
            let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
            for seq in first..first + 6 {
                writeln!(body, "#EXTINF:10.027,\n{seq}.ts").unwrap();
            }
            Arc::<[u8]>::from(body.into_bytes())
        };
        let (initial, progressed) = (playlist(100), playlist(101));
        let phase = Arc::new(AtomicUsize::new(0));
        let handler_phase = Arc::clone(&phase);
        let attempts = AtomicUsize::new(0);
        let (release, gate) = tokio::sync::watch::channel(false);
        let (entered, blocked) = tokio::sync::watch::channel(false);
        let origin = super::spawn_test_binary_origin(Arc::new(move |path| {
            if !super::path_has_extension(path, "m3u8") {
                return super::TestBinaryOriginResponse::new(
                    StatusCode::OK,
                    Arc::from(
                        include_bytes!(concat!(
                            env!("CARGO_MANIFEST_DIR"),
                            "/../../test/fixtures/hls/channel_unavailable.ts"
                        ))
                        .as_slice(),
                    ),
                );
            }
            if handler_phase.load(Ordering::SeqCst) == 0 {
                return super::TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&initial));
            }
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return super::TestBinaryOriginResponse::new(StatusCode::BAD_GATEWAY, Arc::from(&b"retry"[..]));
            }
            entered.send_replace(true);
            let status = if handler_phase.load(Ordering::SeqCst) == 2 { StatusCode::NOT_FOUND } else { StatusCode::OK };
            let mut response = super::TestBinaryOriginResponse::new(status, Arc::clone(&progressed));
            response.gate = Some(gate.clone());
            response
        }))
        .await;
        let mut config = shared::model::HlsCacheConfigDto {
            origin_manifest_timeout_ms: shared::model::Millis::new(120_000),
            ..Default::default()
        };
        config.startup.mode = mode;
        let fixture = super::terminal_tail::publication_fixture_with_origin(config, origin, phase).await;
        super::terminal_tail::wait_for_ready_timeline(&fixture.session, 3).await;
        let lease = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                fixture.app_state.hls.proxy.now_ms(),
            )
            .await
            .unwrap();
        let snapshot = lease.last_manifest_snapshot.as_ref().unwrap();
        let preparation_observer = fixture.session.read().await.subscribe_terminal_commits();
        super::terminal_tail::prepare_terminal_cutover_bundle(&fixture.app_state, snapshot).await;
        assert!(!preparation_observer.has_changed().unwrap(), "bundle preparation must not publish a commit signal");
        drop(preparation_observer);
        let response = get_response(Arc::clone(&fixture.app_state), &snapshot.visible_segments[0].uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response_body(response).await.is_empty());
        let base = fixture.app_state.hls.proxy.now_ms();
        tokio::time::pause();
        fixture.app_state.hls.proxy.follow_tokio_clock_for_test(base);
        {
            let mut session = fixture.session.write().await;
            session.origin_refresh.next_fetch_allowed_at_ms = 0;
            session.origin_control.last_media_progress_at_ms = Some(base.saturating_sub(24_000));
        }
        fixture.origin_phase.store(1, Ordering::SeqCst);
        Self { fixture, release, blocked }
    }

    async fn reserve(&self) -> u64 {
        let manager = &self.fixture.app_state.hls.proxy;
        let now = manager.now_ms();
        let lease = manager
            .access_lease_response_snapshot(&self.fixture.access_lease_id, &self.fixture.proxy_session_id, now)
            .await
            .unwrap();
        let manifest = lease.last_manifest_snapshot.as_ref().unwrap();
        let session = self.fixture.session.read().await;
        let reserve = tuliprox_hls::api::evaluate_lease_reserve(tuliprox_hls::api::HlsLeaseReserveInput {
            manifest,
            cursor: &lease.playback_cursor,
            ready_timeline: &session.ready_timeline_snapshot(
                lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
                now,
            ),
            now_ms: now,
            recovery_trigger_budget: tuliprox_hls::api::HlsRecoveryTriggerBudgetMs::from_millis(0),
            origin_path_degraded: session.origin_control.path_condition.is_degraded(),
            recovery_committed: false,
        });
        assert!(reserve.playback_position_known);
        reserve.guaranteed_reserve_ms
    }

    async fn waiting_request(&mut self) -> tokio::task::JoinHandle<axum::response::Response> {
        let app = Arc::clone(&self.fixture.app_state);
        let uri = self.fixture.media_playlist_uri.clone();
        let request = tokio::spawn(async move { get_response(app, &uri, None).await });
        self.wait_for_blocked_origin().await;
        assert!(!request.is_finished(), "request must wait with positive reserve");
        request
    }

    async fn wait_for_blocked_origin(&mut self) {
        within_real_second(async {
            loop {
                if self.fixture.session.read().await.origin_control.path_condition.is_degraded() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Drive only the pre-commit retry stagger, allowing the native socket
        // to reach its explicit gate before advancing to media exhaustion.
        for _ in 0..20 {
            if *self.blocked.borrow() {
                break;
            }
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_millis(5) {
                tokio::task::yield_now().await;
            }
        }
        within_real_second(self.blocked.wait_for(|entered| *entered)).await.unwrap();
        assert!(self.fixture.session.read().await.origin_refresh.in_flight);
        assert!(self.reserve().await > 0);
    }

    fn refresh_request(&self) -> tuliprox_hls::api::OriginRefreshRequest {
        let fixture = &self.fixture;
        let mut request = super::terminal_tail::regression_origin_refresh_request(
            &fixture.app_state,
            Arc::clone(&fixture.session),
            &format!("{}/channel/index.m3u8", fixture.origin.base_url),
            Some(fixture.access_lease_id.clone()),
        );
        request.now_ms = fixture.app_state.hls.proxy.now_ms();
        request.post_refresh_runtime =
            Some(tuliprox_hls::api::HlsPostRefreshRuntime { ctx: fixture.app_state.hls_ctx().downgrade() });
        request.origin_io = Some(super::provider_admission::test_hls_origin_io_context(&fixture.app_state));
        request
    }

    async fn pending_refresh(&mut self) -> PendingRefresh {
        use futures::FutureExt as _;
        let mut refresh = tuliprox_hls::api::trigger_origin_refresh_sync(self.refresh_request()).boxed().shared();
        assert!(futures::poll!(&mut refresh).is_pending());
        self.wait_for_blocked_origin().await;
        refresh
    }

    async fn lease(&self) -> tuliprox_hls::api::HlsAccessLease {
        let fixture = &self.fixture;
        fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                fixture.app_state.hls.proxy.now_ms(),
            )
            .await
            .unwrap()
    }

    async fn consume_reserve(&self) -> u64 {
        let mut commits = self.fixture.session.read().await.subscribe_terminal_commits();
        tokio::time::advance(std::time::Duration::from_millis(self.reserve().await)).await;
        within_real_second(commits.changed()).await.unwrap();
        assert!(matches!(
            self.lease().await.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. }
        ));
        assert!(!*self.release.borrow());
        self.assert_origin_stopped().await;
        self.fixture.app_state.hls.proxy.now_ms()
    }

    async fn assert_origin_stopped(&self) {
        within_real_second(async {
            loop {
                let state = self.fixture.session.read().await;
                if !state.origin_refresh.in_flight
                    && state.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire) == 0
                    && state.origin_account_io_lease.as_ref().is_none_or(|io| !io.is_active_or_acquiring())
                {
                    break;
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn finish_refresh(&self, refresh: PendingRefresh) {
        self.release.send_replace(true);
        assert!(within_real_second(refresh).await);
        let state = self.fixture.session.read().await;
        assert!(!state.origin_refresh.in_flight);
        assert_eq!(state.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(state.origin_account_io_lease.as_ref().is_none_or(|lease| !lease.is_active_or_acquiring()));
    }
}

#[tokio::test]
async fn terminal_notification_releases_conservative_manifest_before_origin_refresh_finishes() {
    delayed_terminal_response_before_refresh(shared::model::HlsStartupMode::Conservative).await;
}

#[derive(Clone, Copy)]
enum CommitSignalRace {
    Unsubscribed,
    Subscribed,
    Checked,
}

async fn terminal_notification_race(race: CommitSignalRace) {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Optimistic).await;
    let refresh = controlled.pending_refresh().await;
    let expected = controlled.lease().await;
    let fixture = &controlled.fixture;
    let mut commits = None;
    let mut checked = None;
    if !matches!(race, CommitSignalRace::Unsubscribed) {
        commits = Some(fixture.session.read().await.subscribe_terminal_commits());
    }
    if matches!(race, CommitSignalRace::Checked) {
        checked = Some(
            super::super::current_hls_refresh_lease(
                &fixture.app_state,
                &fixture.session,
                &fixture.proxy_session_id,
                &fixture.access_lease_id,
                &expected,
            )
            .await
            .unwrap(),
        );
    }
    let commit_at = controlled.consume_reserve().await;
    let commits = match commits {
        Some(commits) => commits,
        None => fixture.session.read().await.subscribe_terminal_commits(),
    };
    let checked = match checked {
        Some(lease) => lease,
        None => super::super::current_hls_refresh_lease(
            &fixture.app_state,
            &fixture.session,
            &fixture.proxy_session_id,
            &fixture.access_lease_id,
            &expected,
        )
        .await
        .unwrap(),
    };
    let response = within_real_second(super::super::await_hls_terminal_or_refresh(
        &fixture.app_state,
        &fixture.session,
        &expected,
        checked,
        commits,
        refresh.clone(),
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(String::from_utf8(response_body(response).await.to_vec()).unwrap().ends_with("#EXT-X-ENDLIST\n"));
    assert_eq!(fixture.app_state.hls.proxy.now_ms(), commit_at);
    controlled.assert_origin_stopped().await;
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    controlled.finish_refresh(refresh).await;
}

#[tokio::test]
async fn terminal_notification_commit_before_subscription_is_not_lost() {
    terminal_notification_race(CommitSignalRace::Unsubscribed).await;
}

#[tokio::test]
async fn terminal_notification_commit_between_subscription_and_check_is_not_lost() {
    terminal_notification_race(CommitSignalRace::Subscribed).await;
}

#[tokio::test]
async fn terminal_notification_commit_between_check_and_wait_is_not_lost() {
    terminal_notification_race(CommitSignalRace::Checked).await;
}

#[tokio::test]
async fn terminal_notification_wakes_all_waiters_without_another_refresh_or_generation() {
    use futures::FutureExt as _;
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Progressive).await;
    let refresh = controlled.pending_refresh().await;
    let expected = controlled.lease().await;
    let fixture = &controlled.fixture;
    let starts = fixture.app_state.hls.proxy.metrics().snapshot().refresh_started;
    let mut waiters = Vec::new();
    for _ in 0..3 {
        let commits = fixture.session.read().await.subscribe_terminal_commits();
        let lease = super::super::current_hls_refresh_lease(
            &fixture.app_state,
            &fixture.session,
            &fixture.proxy_session_id,
            &fixture.access_lease_id,
            &expected,
        )
        .await
        .unwrap();
        let mut waiter = super::super::await_hls_terminal_or_refresh(
            &fixture.app_state,
            &fixture.session,
            &expected,
            lease,
            commits,
            refresh.clone(),
        )
        .boxed();
        assert!(futures::poll!(&mut waiter).is_pending());
        waiters.push(waiter);
    }
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 3);
    let commit_at = controlled.consume_reserve().await;
    let responses = within_real_second(futures::future::join_all(waiters)).await;
    let mut bodies = Vec::new();
    for response in responses {
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response_body(response).await);
    }
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[1], bodies[2]);
    assert_eq!(fixture.app_state.hls.proxy.now_ms(), commit_at);
    assert_eq!(fixture.app_state.hls.proxy.metrics().snapshot().refresh_started, starts);
    assert_eq!(controlled.lease().await.admission_generation, expected.admission_generation + 1);
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    controlled.finish_refresh(refresh).await;
}

#[tokio::test]
async fn terminal_notification_client_abort_drops_observer_without_aborting_origin() {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Optimistic).await;
    let request = controlled.waiting_request().await;
    let fixture = &controlled.fixture;
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 1);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    assert!(fixture.session.read().await.origin_refresh.in_flight);
    controlled.release.send_replace(true);
    within_real_second(async {
        while fixture.session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(
        fixture.session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn terminal_notification_successful_refresh_before_exhaustion_keeps_manifest_live() {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::FirstReady).await;
    let waiting = controlled.waiting_request().await;
    controlled.release.send_replace(true);
    let response = within_real_second(waiting).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
    assert_eq!(controlled.lease().await.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(controlled.reserve().await > 0);
    assert_eq!(controlled.fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
}

#[derive(Clone, Copy)]
enum RefreshWaitReplacement {
    Session,
    Lease,
    Admission,
}

#[tokio::test]
async fn terminal_notification_singleflight_followers_return_live_without_new_origin_starts() {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Progressive).await;
    let owner = controlled.waiting_request().await;
    let fixture = &controlled.fixture;
    let starts = fixture.app_state.hls.proxy.metrics().snapshot().refresh_started;
    for _ in 0..4 {
        let response =
            within_real_second(get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
        assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 1);
    }
    assert_eq!(fixture.app_state.hls.proxy.metrics().snapshot().refresh_started, starts);
    assert!(!owner.is_finished());
    controlled.release.send_replace(true);
    assert_eq!(within_real_second(owner).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn terminal_notification_refresh_failure_keeps_positive_reserve_live_and_releases_work() {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::FirstReady).await;
    controlled.fixture.origin_phase.store(2, std::sync::atomic::Ordering::SeqCst);
    let waiting = controlled.waiting_request().await;
    controlled.release.send_replace(true);
    let response = within_real_second(waiting).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
    assert!(controlled.reserve().await > 0);
    assert_eq!(controlled.lease().await.playback_mode, HlsLeasePlaybackMode::Live);
    let session = controlled.fixture.session.read().await;
    assert!(!session.origin_refresh.in_flight);
    assert!(session.origin_control.path_condition.is_degraded());
    assert_eq!(session.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire), 0);
    assert_eq!(session.terminal_commit_observer_count_for_test(), 0);
}

#[tokio::test]
async fn terminal_notification_unavailable_releases_waiter_before_origin_finishes() {
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Optimistic).await;
    let waiting = controlled.waiting_request().await;
    controlled.fixture.app_state.app_config.custom_stream_response.store(None);
    let mut config = controlled.fixture.app_state.app_config.config.load_full().as_ref().clone();
    config.custom_stream_response_enabled = false;
    config.custom_stream_response_error_status = 502;
    controlled.fixture.app_state.app_config.config.store(Arc::new(config));
    let commit_at = controlled.consume_reserve().await;
    let response = within_real_second(waiting).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response_body(response).await.is_empty());
    assert!(matches!(controlled.lease().await.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
    assert_eq!(controlled.fixture.app_state.hls.proxy.now_ms(), commit_at);
    controlled.assert_origin_stopped().await;
    assert_eq!(controlled.fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    let replay =
        get_response(Arc::clone(&controlled.fixture.app_state), &controlled.fixture.media_playlist_uri, None).await;
    assert_eq!(replay.status(), StatusCode::BAD_GATEWAY, "the committed error status must survive polling");
    controlled.release.send_replace(true);
    within_real_second(async {
        while controlled.fixture.session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn terminal_notification_replacement(replacement: RefreshWaitReplacement) {
    use futures::FutureExt as _;
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Optimistic).await;
    let refresh = controlled.pending_refresh().await;
    let expected = controlled.lease().await;
    let fixture = &controlled.fixture;
    let commits = fixture.session.read().await.subscribe_terminal_commits();
    let checked = super::super::current_hls_refresh_lease(
        &fixture.app_state,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.access_lease_id,
        &expected,
    )
    .await
    .unwrap();
    let mut waiting = super::super::await_hls_terminal_or_refresh(
        &fixture.app_state,
        &fixture.session,
        &expected,
        checked,
        commits,
        refresh.clone(),
    )
    .boxed();
    assert!(futures::poll!(&mut waiting).is_pending());
    controlled.consume_reserve().await;
    match replacement {
        RefreshWaitReplacement::Session => {
            let key = fixture.session.read().await.key.clone();
            fixture.app_state.hls.proxy.sessions().remove_session(&key, &fixture.proxy_session_id).await.unwrap();
            let replacement = fixture
                .app_state
                .hls
                .proxy
                .get_or_create_session(
                    key,
                    &fixture.app_state.get_encrypt_secret(),
                    fixture.app_state.hls.proxy.now_ms(),
                )
                .await;
            assert!(!Arc::ptr_eq(&replacement, &fixture.session));
        }
        RefreshWaitReplacement::Lease | RefreshWaitReplacement::Admission => {
            let mut leases = fixture.app_state.hls.proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&fixture.access_lease_id).unwrap();
            match replacement {
                RefreshWaitReplacement::Lease => lease.issued_at_ms += 1,
                RefreshWaitReplacement::Admission => lease.admission_generation += 1,
                RefreshWaitReplacement::Session => unreachable!(),
            }
            leases.prepare_access_lease(lease);
        }
    }
    let response = within_real_second(waiting).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response_body(response).await.is_empty());
    controlled.assert_origin_stopped().await;
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    controlled.finish_refresh(refresh).await;
}

#[tokio::test]
async fn terminal_notification_revalidates_session_incarnation_before_responding() {
    terminal_notification_replacement(RefreshWaitReplacement::Session).await;
}

#[tokio::test]
async fn terminal_notification_revalidates_lease_incarnation_before_responding() {
    terminal_notification_replacement(RefreshWaitReplacement::Lease).await;
}

#[tokio::test]
async fn terminal_notification_revalidates_admission_before_responding() {
    terminal_notification_replacement(RefreshWaitReplacement::Admission).await;
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "One controlled two-lease notification and recovery scenario.")]
async fn terminal_notification_for_another_lease_keeps_own_request_waiting_for_recovery() {
    use futures::FutureExt as _;
    let mut controlled = DelayedTerminalRefresh::new(shared::model::HlsStartupMode::Optimistic).await;
    let fixture = &controlled.fixture;
    let other = HlsAccessLeaseId("foreign-terminal-observer".to_owned());
    let snapshot = within_real_second(async {
        super::prepare_pending_test_hls_access_lease(&fixture.app_state, &fixture.proxy_session_id, &other).await;
        let original = controlled.lease().await;
        {
            let mut leases = fixture.app_state.hls.proxy.access_leases().write().await;
            let mut lease = leases.remove_access_lease(&other).unwrap();
            lease.username = original.username;
            lease.user_session_token = original.user_session_token;
            leases.prepare_access_lease(lease);
        }
        let manifest = super::try_test_hls_cached_manifest_response(
            &fixture.app_state,
            &fixture.session,
            &other,
            tuliprox_hls::api::HlsAccessLeaseState::Pending,
            &fixture.app_state.hls.proxy.strip(),
            None,
            tuliprox_hls::api::HlsCachedManifestOptions::initial(std::time::Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(manifest.status(), StatusCode::OK);
        response_body(manifest).await;
        let manager = &fixture.app_state.hls.proxy;
        assert!(manager
            .activate_access_lease(
                &other,
                &fixture.proxy_session_id,
                manager.now_ms(),
                tuliprox_hls::api::HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 180_000 }
            )
            .await
            .is_activated());
        let snapshot = manager
            .access_lease_response_snapshot(&other, &fixture.proxy_session_id, manager.now_ms())
            .await
            .unwrap()
            .last_manifest_snapshot
            .unwrap();
        let segment =
            get_response(Arc::clone(&fixture.app_state), snapshot.visible_segments.last().unwrap().uri.as_ref(), None)
                .await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert!(!response_body(segment).await.is_empty());
        snapshot
    })
    .await;
    let refresh = controlled.pending_refresh().await;
    let expected = controlled.lease().await;
    let fixture = &controlled.fixture;
    let manager = &fixture.app_state.hls.proxy;
    let notifications = fixture.session.read().await.subscribe_terminal_commits();
    let checked = super::super::current_hls_refresh_lease(
        &fixture.app_state,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.access_lease_id,
        &expected,
    )
    .await
    .unwrap();
    let mut request = super::super::await_hls_terminal_or_refresh(
        &fixture.app_state,
        &fixture.session,
        &expected,
        checked,
        notifications,
        refresh.clone(),
    )
    .boxed();
    assert!(futures::poll!(&mut request).is_pending());
    let other_lease =
        manager.access_lease_response_snapshot(&other, &fixture.proxy_session_id, manager.now_ms()).await.unwrap();
    let exhaustion = other_lease.playback_cursor.first_segment_started_at_ms.unwrap()
        + snapshot.visible_segments.last().unwrap().duration_ms;
    let mut commits = fixture.session.read().await.subscribe_terminal_commits();
    tokio::time::advance(std::time::Duration::from_millis(exhaustion.saturating_sub(manager.now_ms()))).await;
    within_real_second(commits.changed()).await.unwrap();
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&other, &fixture.proxy_session_id, manager.now_ms())
            .await
            .unwrap()
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
    assert_eq!(controlled.lease().await.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(controlled.reserve().await > 0);
    assert!(futures::poll!(&mut request).is_pending(), "a foreign commit is only a wake-up signal");
    drop(commits);
    controlled.release.send_replace(true);
    assert!(within_real_second(request).await.is_none());
    controlled.finish_refresh(refresh).await;
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
}

#[tokio::test]
async fn terminal_notification_releases_first_ready_manifest_before_origin_refresh_finishes() {
    delayed_terminal_response_before_refresh(shared::model::HlsStartupMode::FirstReady).await;
}

#[tokio::test]
async fn terminal_notification_releases_progressive_manifest_before_origin_refresh_finishes() {
    delayed_terminal_response_before_refresh(shared::model::HlsStartupMode::Progressive).await;
}

#[tokio::test]
async fn terminal_notification_releases_optimistic_manifest_before_origin_refresh_finishes() {
    delayed_terminal_response_before_refresh(shared::model::HlsStartupMode::Optimistic).await;
}

async fn delayed_terminal_response_before_refresh(mode: shared::model::HlsStartupMode) {
    let mut controlled = DelayedTerminalRefresh::new(mode).await;
    let waiting = controlled.waiting_request().await;
    let fixture = &controlled.fixture;
    let manager = &fixture.app_state.hls.proxy;
    let previous = manager
        .access_lease_response_snapshot(&fixture.access_lease_id, &fixture.proxy_session_id, manager.now_ms())
        .await
        .unwrap()
        .last_manifest_snapshot
        .unwrap();
    let work = Arc::clone(&fixture.session.read().await.activity.active_origin_work_count);
    let progress_before = fixture.session.read().await.origin_control.progress_generation;
    let success_before = fixture.session.read().await.origin_refresh.last_success_at_ms;
    assert!(work.load(std::sync::atomic::Ordering::Acquire) > 0);
    let io = fixture.session.read().await.origin_account_io_lease.clone().unwrap();
    assert!(io.is_active_or_acquiring());
    let starts = manager.metrics().snapshot().refresh_started;
    let mut commits = fixture.session.read().await.subscribe_terminal_commits();
    let reserve = controlled.reserve().await;
    assert!(reserve > 0);
    assert!(!waiting.is_finished());
    let safety_started = std::time::Instant::now();
    tokio::time::advance(std::time::Duration::from_millis(reserve)).await;
    within_real_second(commits.changed()).await.unwrap();
    let commit_at = manager.now_ms();
    assert_eq!(controlled.reserve().await, 0);
    let response_started = std::time::Instant::now();
    let response = within_real_second(waiting).await.unwrap();
    let response_delay_us = response_started.elapsed().as_micros();
    assert!(
        safety_started.elapsed() < std::time::Duration::from_secs(1),
        "owner commit and HTTP response share one real second"
    );
    assert_eq!(manager.now_ms(), commit_at, "no virtual time after terminal commit");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    controlled.assert_origin_stopped().await;
    assert!(!*controlled.release.borrow());
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    let last_live = previous.visible_segments.last().unwrap().uri.as_ref();
    let media = body.lines().filter(|line| !line.starts_with('#') && !line.is_empty()).collect::<Vec<_>>();
    let first_cvr = media.iter().position(|uri| uri.contains("/terminal/")).unwrap();
    assert_eq!(media[first_cvr - 1], last_live);
    assert!(body.contains("#EXT-X-DISCONTINUITY\n"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    let cvr = get_response(Arc::clone(&fixture.app_state), media[first_cvr], None).await;
    assert_eq!(cvr.status(), StatusCode::OK);
    assert!(!response_body(cvr).await.is_empty());
    let plan = super::terminal_tail::terminal_generation_for_lease(
        &fixture.app_state,
        &fixture.proxy_session_id,
        &fixture.access_lease_id,
    )
    .await;
    eprintln!("terminal_refresh_proof mode={mode:?} reserve_before_ms={reserve} commit_at_ms={commit_at} response_at_ms={} response_delay_us={response_delay_us} origin_in_flight={} origin_gate_released={} last_live_preserved={} cvr_status=200 generation={}",
        manager.now_ms(), fixture.session.read().await.origin_refresh.in_flight, *controlled.release.borrow(), media[first_cvr - 1] == last_live, plan.0);
    drop(commits);
    assert_eq!(fixture.session.read().await.terminal_commit_observer_count_for_test(), 0);
    assert!(
        !manager.has_usable_access_lease_for_session(&fixture.proxy_session_id, manager.now_ms()).await,
        "terminal-only viewers must not authorize new origin work"
    );
    controlled.release.send_replace(true);
    within_real_second(async {
        while fixture.session.read().await.origin_refresh.in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(work.load(std::sync::atomic::Ordering::Acquire), 0);
    assert!(!io.is_active_or_acquiring());
    assert_eq!(fixture.session.read().await.origin_refresh.last_success_at_ms, success_before);
    assert_eq!(fixture.session.read().await.origin_control.progress_generation, progress_before);
    assert_eq!(manager.metrics().snapshot().refresh_started, starts);
    assert_eq!(
        super::terminal_tail::terminal_generation_for_lease(
            &fixture.app_state,
            &fixture.proxy_session_id,
            &fixture.access_lease_id
        )
        .await,
        plan
    );
    let replay = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response_body(replay).await, body.as_bytes());
}

#[tokio::test]
async fn terminal_lease_manifest_is_inline_immutable_endlist_on_canonical_path() {
    const LIVE_TAIL_BYTES: &[u8] = b"original-live-tail";
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", LIVE_TAIL_BYTES).await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let cursor_before = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(lease_id.clone()),
            &ProxySessionId(proxy_session_id.clone()),
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease before media read")
        .playback_cursor;
    let (generation, segment_count) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/manifest.m3u8");
    let mut reloaded_api_proxy =
        app_state.app_config.api_proxy.load_full().as_deref().cloned().expect("test API proxy config");
    reloaded_api_proxy.server[0].path = Some("reloaded".to_string());
    app_state.app_config.api_proxy.store(Some(Arc::new(reloaded_api_proxy)));

    let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest is utf8");
    let repeated = get_response(Arc::clone(&app_state), &manifest_uri, None).await;
    assert_eq!(repeated.status(), StatusCode::OK);
    assert!(!repeated.headers().contains_key(header::LOCATION));
    assert_eq!(response_body(repeated).await, body.as_bytes());
    let live_tail = format!("/{proxy_session_id}/{lease_id}/000123.ts");
    let terminal_prefix = format!("/{proxy_session_id}/{lease_id}/terminal/{generation}/");
    assert!(body.contains(&live_tail));
    assert!(body.contains("/iptv/hls/shared/live/"));
    assert!(!body.contains("/reloaded/hls/shared/live/"));
    assert_eq!(body.matches(&terminal_prefix).count(), usize::from(segment_count));
    assert_eq!(body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    assert!(body.find(&live_tail) < body.find("#EXT-X-DISCONTINUITY\n"));
    assert!(body.find("#EXT-X-DISCONTINUITY\n") < body.find(&terminal_prefix));

    let live_tail_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/000123.ts");
    let live_tail_response = get_response(Arc::clone(&app_state), &live_tail_uri, Some("bytes=0-")).await;
    assert_eq!(live_tail_response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(live_tail_response.headers()[header::CONTENT_LENGTH], LIVE_TAIL_BYTES.len().to_string());
    assert_eq!(
        live_tail_response.headers()[header::CONTENT_RANGE],
        format!("bytes 0-{}/{}", LIVE_TAIL_BYTES.len() - 1, LIVE_TAIL_BYTES.len())
    );
    assert_eq!(response_body(live_tail_response).await, bytes::Bytes::from_static(LIVE_TAIL_BYTES));
    let cursor_after = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(lease_id),
            &ProxySessionId(proxy_session_id),
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease after media read")
        .playback_cursor;
    assert_eq!(cursor_after, cursor_before);
}

#[tokio::test]
async fn commits_prepared_terminal_tail_once_after_complete_reserve_consumption() {
    let fixture = prepared_terminal_cutover_fixture().await;
    let directive = terminal_cutover_acceptance_directive(&fixture).await;
    let pressured_lease = exhaust_terminal_cutover_recovery(&fixture, directive).await;
    let (generation, cutover_now_ms) = commit_prepared_terminal_cutover(&fixture, &pressured_lease).await;
    assert_prepared_terminal_cutover_manifest(&fixture, generation).await;
    assert_terminal_cutover_sticky_after_recovery(&fixture, generation, cutover_now_ms).await;
}

#[tokio::test]
async fn warm_fmp4_at_exhaustion_preserves_the_explicit_incompatible_media_error() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    enable_channel_unavailable_custom_response(&app_state);
    let proxy_session_id = ProxySessionId(map_hls_map(&app_state, b"fmp4-init", true).await);
    let lease_id = HlsAccessLeaseId(format!("test-access-lease-{}", proxy_session_id.0));
    let now_ms = super::super::current_time_millis();
    let (base_proxy_seq, duration_ms) = {
        let session = app_state
            .hls
            .proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("fMP4 shared session");
        let session = session.read().await;
        let entry = session.segments.values().next().expect("fMP4 media entry");
        (entry.proxy_seq, entry.duration_ms)
    };
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq: base_proxy_seq,
        last_proxy_seq: base_proxy_seq,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: base_proxy_seq,
            duration_ms,
            uri: format!("/iptv/hls/shared/live/{}/{}/{base_proxy_seq:06}.m4s", proxy_session_id.0, lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: terminal_test_asset().duration_ms().saturating_add(1_000),
        playlist_duration_ms: duration_ms,
        last_visible_media_end_ms: duration_ms,
        active_map: Some(HlsMapSignature { fingerprint: [7; 32], container: HlsMediaContainer::FragmentedMp4 }),
        active_encryption: None,
        container: HlsMediaContainer::FragmentedMp4,
    };
    publish_test_manifest_and_exhaust_configured_acceptance(&app_state, &proxy_session_id, &lease_id, snapshot, now_ms)
        .await;
    let manager = &app_state.hls.proxy;
    let session = manager.sessions().get_by_proxy_session_id(&proxy_session_id).await.unwrap();
    let identity = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .unwrap()
        .media_identity()
        .unwrap();
    let started_at = now_ms - 120_000;
    let token = manager
        .record_access_lease_segment_request_started_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            identity,
            base_proxy_seq,
            started_at,
        )
        .await
        .unwrap();
    let _ = manager
        .mark_delivered_media_for_lease(&session, &lease_id, &proxy_session_id, identity, Some(token), started_at)
        .await;
    let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, lease_id.0);

    let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response_body(response).await.is_empty());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .unwrap();
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}

#[tokio::test]
async fn delayed_live_resource_completion_revalidates_after_terminal_cutover_without_a_sleep() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"ready-before-cutover").await;
    let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
    let proxy_session_key = ProxySessionId(proxy_session_id.clone());
    let access_context = test_hls_access_context(proxy_session_key.clone(), lease_id.clone());
    let live_identity = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_key, super::super::current_time_millis())
        .await
        .and_then(|lease| lease.media_identity())
        .expect("live lease identity");
    let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
    let app_state_for_completion = Arc::clone(&app_state);
    let access_context_for_completion = access_context.clone();
    let completion = tokio::spawn(async move {
        let _ = release_receiver.await;
        super::super::hls_live_lease_identity_is_current(
            &app_state_for_completion,
            &access_context_for_completion,
            live_identity,
        )
        .await
    });

    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id.0, 123).await;
    release_sender.send(()).expect("release delayed completion after cutover");

    assert!(!completion.await.expect("controlled completion task"));
}
