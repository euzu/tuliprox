//! Provider admission across manifest acceptance and media startup.
use super::{
    create_active_hls_user_session, create_bound_hls_test_session, get_response,
    manifest_entry_freshness::{
        canonical_response_for_lease, enable_hls_cache_with_startup_mode, origin_playlist_with_duration,
    },
    path_has_extension, response_body, spawn_test_binary_origin, test_app_state_with_inputs, test_fingerprint,
    HlsAccessLeaseId, HlsAccessLeaseState, TestBinaryOriginResponse,
};
use axum::http::StatusCode;
use shared::model::{HlsStartupMode, InputType};
use std::{sync::Arc, time::Duration};
use tuliprox_core::model::PlaybackKind;
use tuliprox_hls::api::{
    HlsAccountBindingProtection, HlsAccountOverlapTiming, HlsOriginStartProtection, ProxySessionId,
};
use tuliprox_session::{ConnectionKind, PlaybackLeaseRef};

fn provider(url: &str, limit: u16) -> crate::model::ConfigInput {
    crate::model::ConfigInput {
        id: 1,
        name: Arc::from("startup-provider"),
        input_type: InputType::Xtream,
        url: url.into(),
        username: Some("user".into()),
        password: Some("pass".into()),
        max_connections: limit,
        enabled: true,
        ..Default::default()
    }
}

async fn master(app: &Arc<crate::api::model::AppState>, input: &crate::model::ConfigInput, url: &str) -> String {
    let mut user = crate::model::ProxyUserCredentials::default();
    user.username = "hls-user".into();
    user.password = "hls-pass".into();
    let mut target = super::test_m3u_hls_share_target();
    target.name = "default".into();
    super::store_test_sources_with_target(app, input.clone(), target.clone());
    super::cache_test_m3u_hls_item(app, &target, super::test_m3u_hls_item(input, 12345, "12345", url)).await;
    super::configure_default_test_server(app);
    let response = super::super::create_hls_cache_entry_master_playlist_response(
        app,
        &test_fingerprint(),
        &user,
        super::super::build_hls_origin_source(input, "12345"),
        12345,
        None,
        None,
        None,
        url,
        input,
        shared::model::UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    super::single_variant_uri(response).await
}

#[tokio::test]
async fn master_only_uses_existing_entry_timeout_without_origin_or_provider_admission() {
    let origin = spawn_test_binary_origin(Arc::new(|_| panic!("master must never contact origin"))).await;
    let input = provider(&origin.base_url, 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
    let uri = master(&app, &input, &format!("{}/index.m3u8", origin.base_url)).await;
    let proxy = ProxySessionId(super::proxy_session_id_from_variant_uri(&uri).to_string());
    let id = HlsAccessLeaseId(super::access_lease_id_from_variant_uri(&uri).to_string());
    let lease =
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis()).await.unwrap();
    assert_eq!(lease.state, HlsAccessLeaseState::Pending);
    assert_eq!(lease.valid_until_ms - lease.issued_at_ms, super::super::hls_pending_bootstrap_window_ms(&app));
    assert_eq!(app.hls.proxy.initial_manifest_wait_timeout_secs(), 90);
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 0);
    assert_eq!(origin.manifest_request_count(), 0);
    assert_eq!(origin.segment_request_count(), 0);
    assert_eq!(
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, lease.valid_until_ms).await.unwrap().state,
        HlsAccessLeaseState::Expired
    );
}

#[tokio::test]
async fn timely_session_start_outlives_original_entry_deadline() {
    let (release, gate) = tokio::sync::watch::channel(false);
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&entered);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        let mut response = TestBinaryOriginResponse::new(
            StatusCode::OK,
            if path_has_extension(path, "m3u8") {
                Arc::from(origin_playlist_with_duration(100, 10))
            } else {
                Arc::from(&[0x47; 8192][..])
            },
        );
        if path_has_extension(path, "m3u8") {
            response.gate = Some(gate.clone());
            observed.notify_one();
        }
        response
    }))
    .await;
    let input = provider(&origin.base_url, 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
    let uri = master(&app, &input, &format!("{}/index.m3u8", origin.base_url)).await;
    let proxy = ProxySessionId(super::proxy_session_id_from_variant_uri(&uri).to_string());
    let id = HlsAccessLeaseId(super::access_lease_id_from_variant_uri(&uri).to_string());
    let mut lease =
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis()).await.unwrap();
    // Model a master fetched near the end of its unchanged 90-second entry window.
    let deadline = super::super::current_time_millis() + 300;
    lease.issued_at_ms = deadline - super::super::hls_pending_bootstrap_window_ms(&app);
    lease.valid_until_ms = deadline;
    lease.pending_deadline =
        Some(tuliprox_hls::api::HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms: deadline });
    app.hls.proxy.prepare_access_lease(lease).await;
    let request = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { get_response(app, &uri, None).await })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await.unwrap();
    let remaining = deadline.saturating_sub(super::super::current_time_millis());
    tokio::time::sleep(Duration::from_millis(remaining + 20)).await;
    let processing =
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis()).await.unwrap();
    assert_eq!(processing.state, HlsAccessLeaseState::Pending);
    assert!(processing.valid_until_ms > deadline);
    release.send(true).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), request).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(origin.segment_request_count() > 0);
}

#[tokio::test]
async fn manifest_segment_handoff_preserves_admission_at_limits_one_and_three() {
    for limit in [1, 3] {
        let origin = spawn_test_binary_origin(Arc::new(|path| {
            TestBinaryOriginResponse::new(
                StatusCode::OK,
                if path_has_extension(path, "m3u8") {
                    Arc::from(origin_playlist_with_duration(100, 10))
                } else {
                    Arc::from(&[0x47; 8192][..])
                },
            )
        }))
        .await;
        let input = provider(&origin.base_url, limit);
        assert_protected_handoff(input, format!("{}/live/user/pass/12345.m3u8", origin.base_url), false).await;
        assert!(origin.segment_request_count() > 0);
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the synchronized handoff, contention and delivery assertions together."
)]
async fn assert_protected_handoff(input: crate::model::ConfigInput, url: String, cancel_first: bool) {
    let limit = input.max_connections;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
    create_active_hls_user_session(&app).await;
    let mut occupied = Vec::new();
    for index in 1..limit {
        occupied.push(
            app.active_provider
                .acquire_exact_connection_with_lease_for_session(
                    &input.name,
                    &test_fingerprint().addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(PlaybackLeaseRef::new(&format!("foreign-{index}"), PlaybackKind::LiveTs)),
                )
                .expect("other configured slots are occupied"),
        );
    }
    let barrier = app.hls.proxy.segment_worker_pool().pause_next_origin_prepare_for_test();
    let request = {
        let app = Arc::clone(&app);
        let input = input.clone();
        let url = url.clone();
        tokio::spawn(async move { canonical_response_for_lease(&app, &input, &url, "protected-start").await })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified()).await.expect("manifest committed");
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    assert!(binding.startup_protection.is_some());
    assert_eq!(session.read().await.activity.last_delivered_media_at_ms, None, "reservation is not delivery");
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);
    assert_eq!(app.active_provider.get_provider_connections_count(), usize::from(limit - 1));
    assert!(app.active_provider.is_provider_reserved_for_other_session(&input.name, Some("competitor")));
    let foreign = app.active_provider.acquire_exact_connection_with_lease_for_session(
        &input.name,
        &test_fingerprint().addr,
        false,
        0,
        ConnectionKind::Normal,
        Some(PlaybackLeaseRef::new("competitor", PlaybackKind::LiveHls)),
    );
    assert!(foreign.is_none(), "limit={limit}: accepted start retains its slot");
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), usize::from(limit));
    assert_eq!(
        app.active_provider.provider_lease_usage(&input.name).active,
        0,
        "manifest acceptance is not media confirmation"
    );
    for (id, state) in [("pending-join", HlsAccessLeaseState::Pending), ("idle-join", HlsAccessLeaseState::Idle)] {
        let id = HlsAccessLeaseId(id.into());
        let proxy = session.read().await.proxy_session_id.clone();
        super::prepare_pending_test_hls_access_lease(&app, &proxy, &id).await;
        let mut lease = app
            .hls
            .proxy
            .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
            .await
            .unwrap();
        lease.state = state;
        if state == HlsAccessLeaseState::Idle {
            lease.pending_deadline = None;
        }
        app.hls.proxy.prepare_access_lease(lease).await;
    }
    let request = if cancel_first {
        request.abort();
        let app = Arc::clone(&app);
        let input = input.clone();
        let url = url.clone();
        tokio::spawn(async move { canonical_response_for_lease(&app, &input, &url, "pending-join").await })
    } else {
        request
    };
    barrier.prepare.notify_one();
    barrier.prepared.notified().await;
    barrier.proceed.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(5), request).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    for id in ["pending-join", "idle-join"] {
        let joined = canonical_response_for_lease(&app, &input, &url, id).await;
        assert_eq!(joined.status(), StatusCode::OK);
        assert_eq!(
            session.read().await.origin_account_binding.as_ref().unwrap().startup_protection,
            binding.startup_protection
        );
        assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), usize::from(limit));
    }
    let uri = body.lines().find(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
    let media = get_response(Arc::clone(&app), uri, None).await;
    assert_eq!(media.status(), StatusCode::OK);
    assert!(!response_body(media).await.is_empty());
    assert_eq!(
        app.active_provider.provider_lease_usage(&input.name).active,
        1,
        "real media confirms the reserved start"
    );
    for handle in occupied {
        app.active_provider.release_handle(&handle);
    }
}

#[tokio::test]
async fn cancelling_one_waiter_preserves_the_shared_start_for_another_client() {
    let origin = spawn_test_binary_origin(Arc::new(|path| {
        TestBinaryOriginResponse::new(
            StatusCode::OK,
            if path_has_extension(path, "m3u8") {
                Arc::from(origin_playlist_with_duration(100, 10))
            } else {
                Arc::from(&[0x47; 8192][..])
            },
        )
    }))
    .await;
    assert_protected_handoff(provider(&origin.base_url, 1), format!("{}/index.m3u8", origin.base_url), true).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the failure/status matrix and same-lease/fresh-master checks together."
)]
async fn exhausted_segment_admission_finishes_immediately_with_typed_cvr_or_status_and_no_repoll_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for (video, asset, status) in [
        (false, false, StatusCode::BAD_GATEWAY),
        (false, false, StatusCode::UNPROCESSABLE_ENTITY),
        (false, false, StatusCode::SERVICE_UNAVAILABLE),
        (true, false, StatusCode::SERVICE_UNAVAILABLE),
        (true, true, StatusCode::OK),
    ] {
        let manifest_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&manifest_hits);
        let origin = spawn_test_binary_origin(Arc::new(move |path| {
            TestBinaryOriginResponse::new(
                StatusCode::OK,
                if path_has_extension(path, "m3u8") {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Arc::from(origin_playlist_with_duration(100, 10))
                } else {
                    Arc::from(&[0x47; 8192][..])
                },
            )
        }))
        .await;
        let input = provider(&origin.base_url, 1);
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
        create_active_hls_user_session(&app).await;
        super::store_test_sources_with_target(&app, input.clone(), super::test_hls_share_target(true));
        let config = app.app_config.config.load();
        app.app_config.config.store(Arc::new(crate::model::Config {
            custom_stream_response_enabled: video,
            custom_stream_response_error_status: if video || status == StatusCode::BAD_GATEWAY {
                shared::model::ConfigDto::default().custom_stream_response_error_status
            } else {
                status.as_u16()
            },
            ..config.as_ref().clone()
        }));
        if asset || !video {
            app.app_config.custom_stream_response.store(Some(Arc::new(crate::model::CustomStreamResponse {
                provider_connections_exhausted: Some(super::test_custom_video_buffer()),
                channel_unavailable: None,
                user_connections_exhausted: None,
                low_priority_preempted: None,
                user_account_expired: None,
                panel_api_provisioning: None,
                hls_session_or_lease_expired: None,
                maintenance: None,
                panel_api_provisioning_hls_segments: Vec::new(),
            })));
        }
        let barrier = app.hls.proxy.segment_worker_pool().pause_next_origin_prepare_for_test();
        let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let request = {
            let app = Arc::clone(&app);
            let input = input.clone();
            let url = url.clone();
            tokio::spawn(async move { canonical_response_for_lease(&app, &input, &url, "failed-admission").await })
        };
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified()).await.unwrap();
        let key = super::super::build_hls_origin_source(&input, "12345").session_key();
        let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
        let binding = session.read().await.origin_account_binding.clone().unwrap();
        // Simulate losing admission after acceptance, independently of the fixed
        // handoff test: all segment URLs remain healthy at the real HTTP origin.
        app.active_provider.clear_identified_provider_reservation(
            &binding.session_owner,
            &input.name,
            binding.provider_binding_tag,
        );
        app.active_provider.refresh_provider_reservation(&input.name, "foreign-playback", 60);
        assert!(app.active_provider.confirm_playback_activity("foreign-playback").is_some());
        barrier.prepare.notify_one();
        barrier.prepared.notified().await;
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        barrier.proceed.notify_one();
        let response = tokio::time::timeout(Duration::from_secs(4), request)
            .await
            .expect("failure wakes request without GC")
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(response.status(), status, "video={video} asset={asset}");
        if asset {
            use tower::ServiceExt;
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert!(body.contains("#EXT-X-ENDLIST"));
            let router =
                crate::api::endpoints::custom_video_stream_api::cvs_api_register().with_state(Arc::clone(&app));
            for uri in body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
                let url = url::Url::parse(uri).unwrap();
                let start = url.path().rfind("/cvs/").unwrap();
                let mut request =
                    axum::http::Request::builder().uri(&url.path()[start..]).body(axum::body::Body::empty()).unwrap();
                request.extensions_mut().insert(axum::extract::ConnectInfo(test_fingerprint().addr));
                let media = router.clone().oneshot(request).await.unwrap();
                assert_eq!(media.status(), StatusCode::OK);
                assert!(!response_body(media).await.is_empty());
            }
        }
        let proxy = session.read().await.proxy_session_id.clone();
        let lease = app
            .hls
            .proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId("failed-admission".into()),
                &proxy,
                super::super::current_time_millis(),
            )
            .await
            .unwrap();
        let attempt = lease.manifest_start_attempt.unwrap();
        assert!(attempt.failed());
        assert_eq!(
            attempt.provider_failure(),
            Some(tuliprox_hls::api::HlsBoundAccountAcquireErrorKind::ReservedForOther)
        );
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert_eq!(session.read().await.active_segment_fetches, 0);
        assert!(app.active_provider.binding_tag_for_owner(&binding.session_owner).is_none());
        assert_eq!(origin.segment_request_count(), 0);
        let budget = attempt.logical_attempts();
        let requests = manifest_hits.load(Ordering::SeqCst);
        for _ in 0..2 {
            let repeated = canonical_response_for_lease(&app, &input, &url, "failed-admission").await;
            assert_eq!(repeated.status(), status);
            assert_eq!(attempt.logical_attempts(), budget);
            assert_eq!(manifest_hits.load(Ordering::SeqCst), requests);
        }
        app.active_provider.clear_provider_reservation("foreign-playback");
        tokio::time::resume();
        let fresh = master(&app, &input, &url).await;
        let response = get_response(Arc::clone(&app), &fresh, None).await;
        assert_eq!(response.status(), StatusCode::OK, "fresh master restarts after released capacity");
        assert!(origin.segment_request_count() > 0);
    }
}

#[tokio::test(start_paused = true)]
async fn start_windows_are_frozen_and_only_media_renews_protection() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, 1_000).await;
    {
        let mut state = session.write().await;
        state.target_duration = Some(10);
        state.origin_account_binding.as_mut().unwrap().startup_protection = Some(HlsOriginStartProtection {
            anchored_at_ms: 1_000,
            timing: HlsAccountOverlapTiming::from_target_duration_secs(Some(10)),
        });
        assert_eq!(
            state.account_binding_protection(11_000),
            HlsAccountBindingProtection::HardActive { until_ms: 11_000 }
        );
        assert_eq!(
            state.account_binding_protection(11_001),
            HlsAccountBindingProtection::SoftActive { reclaim_until_ms: 31_000 }
        );
        assert_eq!(
            state.account_binding_protection(31_000),
            HlsAccountBindingProtection::SoftActive { reclaim_until_ms: 31_000 }
        );
        assert_eq!(state.account_binding_protection(31_001), HlsAccountBindingProtection::Expired);
        assert_eq!(state.account_reservation_remaining_ms(1_000), Some(31_000));
        state.target_duration = Some(20);
    }
    for now in [2_000, 8_000, 20_000] {
        super::super::mark_successful_canonical_manifest_activity(&app, &session, now).await;
    }
    assert_eq!(session.read().await.account_reservation_remaining_ms(20_000), Some(12_000));
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);
    let newcomer = ProxySessionId("newcomer".into());
    assert!(super::super::find_hls_account_overlap_candidate(&app, &input, &newcomer, 11_000).await.is_none());
    assert!(super::super::find_hls_account_overlap_candidate(&app, &input, &newcomer, 30_000).await.is_some());
    let generation = session.write().await.start_origin_work();
    assert!(super::super::find_hls_account_overlap_candidate(&app, &input, &newcomer, 30_000).await.is_none());
    session.write().await.finish_origin_work(generation);
    session.write().await.mark_authorized_media_access(25_000);
    assert_eq!(
        session.read().await.account_binding_protection(40_000),
        HlsAccountBindingProtection::HardActive { until_ms: 45_000 }
    );
    assert_eq!(session.read().await.account_reservation_remaining_ms(25_000), Some(61_000));
}

#[tokio::test]
async fn generic_preparation_failures_do_not_select_the_provider_capacity_video() {
    use tuliprox_hls::api::{HlsBoundAccountAcquireErrorKind as Kind, HlsOriginAccountSnapshot};
    for kind in [Kind::Unavailable, Kind::WaitTimedOut, Kind::AcquireTimedOut, Kind::Exhausted, Kind::ReservedForOther]
    {
        let input = provider("http://origin.invalid", 1);
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
        super::store_test_sources_with_target(&app, input.clone(), super::test_hls_share_target(true));
        let config = app.app_config.config.load();
        app.app_config
            .config
            .store(Arc::new(crate::model::Config { custom_stream_response_enabled: true, ..config.as_ref().clone() }));
        super::owner_handoff::enable_provider_exhausted_custom_response(&app);
        let now = super::super::current_time_millis();
        let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
        let proxy = session.read().await.proxy_session_id.clone();
        let id = HlsAccessLeaseId("preparation-failed".into());
        super::prepare_pending_test_hls_access_lease(&app, &proxy, &id).await;
        let snapshot = HlsOriginAccountSnapshot::from_session(&*session.read().await);
        app.hls.proxy.fail_access_lease_start_admission(&app.hls_ctx(), &session, &id, kind, now, &snapshot).await;
        let response = canonical_response_for_lease(&app, &input, "http://origin.invalid/index.m3u8", &id.0).await;
        let capacity = matches!(kind, Kind::Exhausted | Kind::ReservedForOther);
        assert_eq!(kind.is_capacity_denial(), capacity);
        assert_eq!(
            response.status(),
            if capacity { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE },
            "{kind:?}: only the provider-capacity asset is configured"
        );
        if !capacity {
            let existing = app.app_config.custom_stream_response.load().as_ref().unwrap().as_ref().clone();
            app.app_config.custom_stream_response.store(Some(Arc::new(crate::model::CustomStreamResponse {
                channel_unavailable: Some(super::test_custom_video_buffer()),
                ..existing
            })));
            let channel = canonical_response_for_lease(&app, &input, "http://origin.invalid/index.m3u8", &id.0).await;
            assert_eq!(channel.status(), StatusCode::OK, "generic failure uses the channel asset when present");
            assert!(String::from_utf8(response_body(channel).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
        }
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep running provisioning, repeated lease access and terminal resolution in one scenario."
)]
async fn pending_and_idle_provisioning_stay_retryable_until_the_job_finishes() {
    use shared::model::{PanelApiConfigDto, PanelApiQueryParamDto};
    for (lease_state, asset) in [
        (HlsAccessLeaseState::Pending, true),
        (HlsAccessLeaseState::Idle, true),
        (HlsAccessLeaseState::Pending, false),
        (HlsAccessLeaseState::Idle, false),
    ] {
        let (release, gate) = tokio::sync::watch::channel(false);
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed = Arc::clone(&entered);
        let origin = spawn_test_binary_origin(Arc::new(move |path| {
            if path.starts_with("/panel") {
                let mut response =
                    TestBinaryOriginResponse::new(StatusCode::BAD_REQUEST, Arc::from(&b"unavailable"[..]));
                response.gate = Some(gate.clone());
                observed.notify_one();
                response
            } else {
                TestBinaryOriginResponse::new(StatusCode::OK, Arc::from(origin_playlist_with_duration(100, 10)))
            }
        }))
        .await;
        let mut input = provider(&origin.base_url, 1);
        let mut panel = PanelApiConfigDto {
            enabled: true,
            url: format!("{}/panel", origin.base_url),
            api_key: Some(Arc::from("local-test")),
            ..Default::default()
        };
        let api_key = PanelApiQueryParamDto { key: Arc::from("api_key"), value: Arc::from("auto") };
        panel.query_parameter.client_info = vec![
            api_key.clone(),
            PanelApiQueryParamDto { key: Arc::from("username"), value: Arc::from("auto") },
            PanelApiQueryParamDto { key: Arc::from("password"), value: Arc::from("auto") },
        ];
        panel.query_parameter.client_new =
            vec![api_key, PanelApiQueryParamDto { key: Arc::from("type"), value: Arc::from("m3u") }];
        input.panel_api = Some(crate::model::PanelApiConfig::from(&panel));
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache_with_startup_mode(&app, HlsStartupMode::Conservative).await;
        create_active_hls_user_session(&app).await;
        super::store_test_sources_with_target(&app, input.clone(), super::test_hls_share_target(true));
        let mut cache = app.hls.proxy.keep_cache_path(shared::model::HlsCacheConfigDto::default());
        cache.strip.value = 0;
        let config = app.app_config.config.load();
        app.app_config.config.store(Arc::new(crate::model::Config {
            custom_stream_response_enabled: true,
            reverse_proxy: Some(crate::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto {
                hls_cache: Some(cache),
                stream: Some(shared::model::StreamConfigDto { grace_period_hold_stream: false, ..Default::default() }),
                ..Default::default()
            })),
            ..config.as_ref().clone()
        }));
        app.hls.proxy.update_config(&app.app_config).await;
        if asset {
            super::owner_handoff::enable_hls_provisioning_custom_response(&app);
        }
        app.active_provider.refresh_provider_reservation(&input.name, "provisioning-foreign", 60);
        app.active_provider.confirm_playback_activity("provisioning-foreign").unwrap();
        let session =
            super::create_unbound_hls_test_session(&app, &input, "12345", super::super::current_time_millis()).await;
        let proxy = session.read().await.proxy_session_id.clone();
        let id = HlsAccessLeaseId("provisioning-start".into());
        super::prepare_pending_test_hls_access_lease(&app, &proxy, &id).await;
        if lease_state == HlsAccessLeaseState::Idle {
            let mut lease = app
                .hls
                .proxy
                .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
                .await
                .unwrap();
            lease.state = lease_state;
            lease.pending_deadline = None;
            app.hls.proxy.prepare_access_lease(lease).await;
        }
        let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        for _ in 0..2 {
            let response = canonical_response_for_lease(&app, &input, &url, &id.0).await;
            assert_eq!(
                response.status(),
                if asset { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE },
                "{lease_state:?} asset={asset}"
            );
            if asset {
                let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
                assert!(!body.contains("#EXT-X-ENDLIST"), "provisioning remains a live timeline");
                assert!(body.contains("/hls/shared/live/"));
            }
            let lease = app
                .hls
                .proxy
                .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
                .await
                .unwrap();
            assert!(
                lease.manifest_start_attempt.as_ref().is_none_or(|attempt| !attempt.failed()),
                "running provisioning must not fail {lease_state:?}"
            );
        }
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("real panel request is in progress");
        release.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if app.hls.provisioning.consumer_status(&input.name, 12345, super::super::current_time_millis())
                    == Some(crate::api::model::HlsProvisioningStatus::ProviderExhausted)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed panel job resolves provisioning");
        let response = canonical_response_for_lease(&app, &input, &url, &id.0).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let lease = app
            .hls
            .proxy
            .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
            .await
            .unwrap();
        assert!(lease.manifest_start_attempt.unwrap().failed(), "only the final capacity result fails the lease");
        app.active_provider.clear_provider_reservation("provisioning-foreign");
    }
}

#[tokio::test(start_paused = true)]
async fn unadmitted_binding_replacement_and_late_polling_preserve_the_start_anchor() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, 1_000).await;
    let start = HlsOriginStartProtection {
        anchored_at_ms: 1_000,
        timing: HlsAccountOverlapTiming::from_target_duration_secs(Some(10)),
    };
    {
        let mut state = session.write().await;
        state.origin_account_binding.as_mut().unwrap().startup_protection = Some(start);
        let mut replacement = state.origin_account_binding.clone().unwrap();
        replacement.generation += 1;
        replacement.pinned_at_ms = 40_000;
        replacement.startup_protection = None;
        state.replace_origin_account_binding(Some(replacement));
        state.target_duration = Some(60);
    }
    super::super::mark_successful_canonical_manifest_activity(&app, &session, 41_000).await;
    let state = session.read().await;
    assert_eq!(state.origin_account_binding.as_ref().unwrap().startup_protection, Some(start));
    assert_eq!(state.account_binding_protection(41_000), HlsAccountBindingProtection::Expired);
    assert_eq!(state.account_reservation_remaining_ms(41_000), Some(0));
    assert_eq!(state.activity.last_authorized_media_at_ms, None);
}

#[tokio::test(start_paused = true)]
async fn provider_start_is_one_unconfirmed_slot_with_fixed_expiry_and_generation_safe_cleanup() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let owner = "hls-cache:start";
    let handle = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .unwrap();
    let tag = app.active_provider.reserve_hls_start(owner, &handle, Duration::from_secs(31)).unwrap();
    assert_eq!(tag, handle.binding_tag.unwrap());
    assert!(
        !app.active_provider.is_provider_reserved_for_other_session(&input.name, Some("foreign")),
        "allocation and reserve count once"
    );
    app.active_provider.release_handle(&handle);
    assert!(app.active_provider.is_provider_reserved_for_other_session(&input.name, Some("foreign")));
    tokio::time::advance(Duration::from_secs(20)).await;
    let joined = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .unwrap();
    assert_eq!(joined.binding_tag, Some(tag));
    app.active_provider.refresh_provider_reservation(&input.name, owner, 200);
    app.active_provider.reserve_hls_start(owner, &joined, Duration::from_secs(200));
    app.active_provider.release_handle(&joined);
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(!app.active_provider.is_provider_reserved_for_other_session(&input.name, Some("foreign")));
    let successor = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .unwrap();
    let successor_tag = app.active_provider.reserve_hls_start(owner, &successor, Duration::from_secs(31)).unwrap();
    assert_ne!(successor_tag, tag);
    app.active_provider.clear_identified_provider_reservation(owner, &input.name, Some(tag));
    assert_eq!(app.active_provider.binding_tag_for_owner(owner), Some(successor_tag));
    app.active_provider.release_handle(&successor);
}

#[tokio::test]
async fn regular_readmission_of_an_expired_start_clears_the_anchor_after_allocation() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let now = super::super::current_time_millis();
    let past = now - 40_000;
    let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, past).await;
    let old_generation = {
        let mut state = session.write().await;
        state.origin_account_binding.as_mut().unwrap().startup_protection = Some(HlsOriginStartProtection {
            anchored_at_ms: past,
            timing: HlsAccountOverlapTiming::from_target_duration_secs(Some(10)),
        });
        state.origin_account_binding.as_ref().unwrap().generation
    };
    let proxy = session.read().await.proxy_session_id.clone();
    super::prepare_pending_test_hls_access_lease(&app, &proxy, &HlsAccessLeaseId("readmitted".into())).await;
    let url = "http://origin.invalid/live/user/pass/12345.m3u8";
    let prepared = super::super::prepare_hls_origin_runtime(
        &app,
        &session,
        &input,
        url,
        url,
        &proxy,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        now,
    )
    .await
    .unwrap();
    {
        let state = session.read().await;
        let binding = state.origin_account_binding.as_ref().unwrap();
        assert!(binding.generation > old_generation);
        assert!(binding.startup_protection.is_none(), "the next accepted manifest may anchor the new start");
        assert!(state.activity.last_delivered_media_at_ms.is_none(), "admission does not require fake media");
    }
    assert!(prepared
        .preacquired_origin_account_handle
        .as_ref()
        .and_then(tuliprox_session::ManagedProviderHandle::handle)
        .is_some());
    assert_eq!(
        app.active_provider.get_provider_connections_count(),
        1,
        "a real allocation protects the new manifest fetch"
    );
    app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
}

#[tokio::test(start_paused = true)]
async fn admitted_slow_manifest_can_grant_start_after_ordinary_starting_lease_expired() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let owner = "hls-cache:slow-manifest";
    let handle = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .unwrap();
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(app.active_provider.binding_tag_for_owner(owner).is_none());
    assert_eq!(app.active_provider.get_provider_connections_count(), 1);
    let tag = app.active_provider.reserve_hls_start(owner, &handle, Duration::from_secs(31)).unwrap();
    assert_ne!(Some(tag), handle.binding_tag);
    app.active_provider.release_handle(&handle);
    assert!(app.active_provider.is_provider_reserved_for_other_session(&input.name, Some("foreign")));
    app.active_provider.clear_identified_provider_reservation(owner, &input.name, handle.binding_tag);
    assert_eq!(app.active_provider.binding_tag_for_owner(owner), Some(tag));
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_start_can_be_taken_over_in_soft_window_but_never_during_origin_work() {
    let input = provider("http://origin.invalid", 1);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let old = create_bound_hls_test_session(&app, &input, "old", &input.name, 1_000).await;
    let owner = old.read().await.origin_account_binding.as_ref().unwrap().session_owner.clone();
    let handle = app
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &input.name,
            &test_fingerprint().addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(&owner, PlaybackKind::LiveHls)),
        )
        .unwrap();
    let tag = app.active_provider.reserve_hls_start(&owner, &handle, Duration::from_secs(31)).unwrap();
    app.active_provider.release_handle(&handle);
    {
        let mut state = old.write().await;
        state.target_duration = Some(10);
        let binding = state.origin_account_binding.as_mut().unwrap();
        binding.provider_binding_tag = Some(tag);
        binding.startup_protection = Some(HlsOriginStartProtection {
            anchored_at_ms: 1_000,
            timing: HlsAccountOverlapTiming::from_target_duration_secs(Some(10)),
        });
    }
    let new = super::create_unbound_hls_test_session(&app, &input, "new", 21_000).await;
    let proxy = new.read().await.proxy_session_id.clone();
    let generation = old.write().await.start_origin_work();
    let url = "http://origin.invalid/live/user/pass/new.m3u8";
    let blocked = super::super::prepare_hls_origin_runtime(
        &app,
        &new,
        &input,
        url,
        url,
        &proxy,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        21_000,
    )
    .await;
    assert!(blocked.is_err());
    assert_eq!(app.active_provider.binding_tag_for_owner(&owner), Some(tag));
    old.write().await.finish_origin_work(generation);
    let prepared = super::super::prepare_hls_origin_runtime(
        &app,
        &new,
        &input,
        url,
        url,
        &proxy,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        21_000,
    )
    .await
    .expect("soft startup is eligible for existing overlap policy");
    assert!(matches!(
        new.read().await.origin_account_binding.as_ref().unwrap().binding_mode,
        tuliprox_hls::api::HlsOriginAccountBindingMode::Speculative { .. }
    ));
    assert_eq!(old.read().await.activity.last_delivered_media_at_ms, None);
    app.connection_manager.release_managed_provider_handle(prepared.preacquired_origin_account_handle);
}

#[tokio::test(start_paused = true)]
async fn terminal_start_cleanup_is_immediate_and_cannot_release_a_successor_binding() {
    use tuliprox_hls::api::{HlsBoundAccountAcquireErrorKind, HlsOriginAccountBinding, HlsOriginAccountSnapshot};
    for stale in [false, true] {
        let input = provider("http://origin.invalid", 1);
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let now = super::super::current_time_millis();
        let session = create_bound_hls_test_session(&app, &input, "12345", &input.name, now).await;
        let proxy = session.read().await.proxy_session_id.clone();
        let id = HlsAccessLeaseId("terminal-start".into());
        super::prepare_pending_test_hls_access_lease(&app, &proxy, &id).await;
        let owner = session.read().await.origin_account_binding.as_ref().unwrap().session_owner.clone();
        let acquire = || {
            app.active_provider
                .acquire_exact_connection_with_lease_for_session(
                    &input.name,
                    &test_fingerprint().addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(PlaybackLeaseRef::new(&owner, PlaybackKind::LiveHls)),
                )
                .unwrap()
        };
        let first = acquire();
        let old_tag = app.active_provider.reserve_hls_start(&owner, &first, Duration::from_secs(31)).unwrap();
        app.active_provider.release_handle(&first);
        session.write().await.origin_account_binding.as_mut().unwrap().provider_binding_tag = Some(old_tag);
        let expected = HlsOriginAccountSnapshot::from_session(&*session.read().await);
        let successor = if stale {
            app.active_provider.clear_identified_provider_reservation(&owner, &input.name, Some(old_tag));
            let next = acquire();
            let tag = app.active_provider.reserve_hls_start(&owner, &next, Duration::from_secs(31)).unwrap();
            app.active_provider.release_handle(&next);
            let mut binding =
                HlsOriginAccountBinding::rebound(input.name.clone(), input.name.clone(), owner.clone(), 1, now);
            binding.provider_binding_tag = Some(tag);
            session.write().await.handoff_origin_account(binding);
            Some(tag)
        } else {
            None
        };
        let clock = tokio::time::Instant::now();
        app.hls
            .proxy
            .fail_access_lease_start_admission(
                &app.hls_ctx(),
                &session,
                &id,
                HlsBoundAccountAcquireErrorKind::ReservedForOther,
                now,
                &expected,
            )
            .await;
        assert_eq!(clock.elapsed(), Duration::ZERO, "completion needs no lease or GC time advance");
        assert_eq!(app.active_provider.binding_tag_for_owner(&owner), successor);
        assert_eq!(
            session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        let lease = app.hls.proxy.access_lease_response_snapshot(&id, &proxy, now).await.unwrap();
        assert!(lease.manifest_start_attempt.unwrap().failed());
        if stale {
            assert!(session.read().await.origin_account_binding.as_ref().unwrap().is_active());
        }
    }
}

#[tokio::test]
async fn idle_reentry_capacity_failure_preserves_previous_publication_and_closes_without_gc() {
    assert_idle_reentry_admission(1, true, HlsStartupMode::Conservative, false).await;
}

#[tokio::test]
async fn admitted_idle_reentry_protects_the_manifest_segment_handoff_at_limits_one_and_three() {
    for limit in [1, 3] {
        assert_idle_reentry_admission(limit, false, HlsStartupMode::Conservative, false).await;
    }
}

#[tokio::test]
async fn idle_reentry_with_evicted_media_defers_fallback_until_refresh_finishes() {
    for mode in [
        HlsStartupMode::Conservative,
        HlsStartupMode::FirstReady,
        HlsStartupMode::Progressive,
        HlsStartupMode::Optimistic,
    ] {
        assert_idle_reentry_admission(1, false, mode, true).await;
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Exercise a complete played session, idle readmission and its competing owner."
)]
async fn assert_idle_reentry_admission(limit: u16, exhaust: bool, mode: HlsStartupMode, debounce: bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    let head = Arc::new(AtomicU64::new(100));
    let current = Arc::clone(&head);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        TestBinaryOriginResponse::new(
            StatusCode::OK,
            if path_has_extension(path, "m3u8") {
                Arc::from(origin_playlist_with_duration(current.load(Ordering::Acquire), 10))
            } else {
                Arc::from(&[0x47; 8192][..])
            },
        )
    }))
    .await;
    let input = provider(&origin.base_url, limit);
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, mode).await;
    create_active_hls_user_session(&app).await;
    let mut occupied = Vec::new();
    for index in 1..limit {
        occupied.push(
            app.active_provider
                .acquire_exact_connection_with_lease_for_session(
                    &input.name,
                    &test_fingerprint().addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(PlaybackLeaseRef::new(&format!("idle-foreign-{index}"), PlaybackKind::LiveTs)),
                )
                .unwrap(),
        );
    }
    let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let response = canonical_response_for_lease(&app, &input, &url, "warm").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    let uri = body.lines().find(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
    let media = get_response(Arc::clone(&app), uri, None).await;
    assert_eq!(media.status(), StatusCode::OK);
    assert!(!response_body(media).await.is_empty());
    let session = app
        .hls
        .proxy
        .sessions()
        .get_by_key(&super::super::build_hls_origin_source(&input, "12345").session_key())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = session.read().await;
            if state.active_segment_fetches == 0 && state.segment_prefetch_queue.is_empty() {
                break;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("warm preparation finishes before idle reentry");
    let proxy = session.read().await.proxy_session_id.clone();
    let id = HlsAccessLeaseId("warm".into());
    // Body EOF schedules completion bookkeeping independently of the reader.
    // Snapshot idle state only after that prior playback has actually completed.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let lease = app
                .hls
                .proxy
                .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
                .await
                .unwrap();
            if lease.playback_cursor.first_segment_completed_at_ms.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("warm media completion is committed before idle snapshot");
    let mut before =
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis()).await.unwrap();
    before.state = HlsAccessLeaseState::Idle;
    before.active_until_ms = None;
    before.pending_deadline = None;
    app.hls.proxy.prepare_access_lease(before.clone()).await;
    {
        let mut state = session.write().await;
        let past = super::super::current_time_millis() - 100_000;
        let activity = if debounce { super::super::current_time_millis() } else { past };
        state.activity.last_authorized_media_at_ms = Some(activity);
        state.activity.last_delivered_media_at_ms = Some(activity);
        state.origin_refresh.last_success_at_ms = Some(past);
        state.origin_refresh.consecutive_failures = u32::from(debounce);
        state.origin_refresh.next_fetch_allowed_at_ms = if debounce { activity + 60_000 } else { 0 };
        state.origin_control.last_origin_response_at_ms = Some(past);
        state.origin_control.last_media_progress_at_ms = Some(past);
        if let Some(manifest) = state.last_rendered_manifest.as_mut() {
            manifest.rendered_at_ms = past;
            manifest.valid_until_ms = past;
        }
        for segment in state.segments.values_mut() {
            segment.status = tuliprox_hls::api::SegmentCacheStatus::Expired;
        }
        if debounce && mode == HlsStartupMode::Conservative {
            state.segments.clear();
        }
        let binding = state.origin_account_binding.as_mut().unwrap();
        binding.pinned_at_ms = activity;
        binding.startup_protection.as_mut().unwrap().anchored_at_ms = activity;
    }
    head.store(106, Ordering::Release);
    let config = app.app_config.config.load();
    app.app_config.config.store(Arc::new(crate::model::Config {
        custom_stream_response_enabled: false,
        custom_stream_response_error_status: 422,
        ..config.as_ref().clone()
    }));
    if debounce {
        let requests = origin.manifest_request_count();
        let response =
            tokio::time::timeout(Duration::from_secs(1), canonical_response_for_lease(&app, &input, &url, "warm"))
                .await
                .expect("debounced idle reentry must stay retryable without waiting for lease expiry");
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{mode:?}: old cache exhaustion is not terminal"
        );
        assert!(response.headers().contains_key(axum::http::header::RETRY_AFTER));
        assert_eq!(origin.manifest_request_count(), requests, "the existing refresh backoff remains effective");
        let waiting = app
            .hls
            .proxy
            .access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis())
            .await
            .unwrap();
        assert!(!waiting.manifest_start_attempt.as_ref().is_some_and(|attempt| attempt.failed()));
        assert_eq!(waiting.last_manifest_snapshot, before.last_manifest_snapshot);
        assert_eq!(waiting.playback_cursor, before.playback_cursor);
        session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    }
    let barrier = app.hls.proxy.segment_worker_pool().pause_next_origin_prepare_for_test();
    let request = {
        let app = Arc::clone(&app);
        let input = input.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let response = canonical_response_for_lease(&app, &input, &url, "warm").await;
                if !debounce || response.status() != StatusCode::SERVICE_UNAVAILABLE {
                    return response;
                }
                assert!(tokio::time::Instant::now() < deadline, "{mode:?}: idle start must recover on the same lease");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .expect("idle reentry commits advancing manifest");
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    if exhaust {
        app.active_provider.clear_identified_provider_reservation(
            &binding.session_owner,
            &input.name,
            binding.provider_binding_tag,
        );
        app.active_provider.refresh_provider_reservation(&input.name, "foreign-playback", 60);
        app.active_provider.confirm_playback_activity("foreign-playback").unwrap();
    } else if !debounce {
        let state = session.read().await;
        let start = binding.startup_protection.expect("readmitted start has accepted its first manifest");
        assert!(start.anchored_at_ms > state.activity.last_delivered_media_at_ms.unwrap());
        assert!(matches!(
            state.account_binding_protection(start.anchored_at_ms),
            HlsAccountBindingProtection::HardActive { .. }
        ));
        assert!(state.account_reservation_remaining_ms(start.anchored_at_ms).unwrap() > 30_000);
        assert_eq!(app.active_provider.get_provider_connections_count(), usize::from(limit - 1));
        assert!(
            app.active_provider
                .acquire_exact_connection_with_lease_for_session(
                    &input.name,
                    &test_fingerprint().addr,
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some(PlaybackLeaseRef::new("idle-competitor", PlaybackKind::LiveHls)),
                )
                .is_none(),
            "idle readmission retains its slot at limit {limit}"
        );
    }
    barrier.prepare.notify_one();
    barrier.prepared.notified().await;
    if exhaust {
        tokio::time::pause();
    }
    barrier.proceed.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(4), request)
        .await
        .expect("idle failure wakes without expiry")
        .unwrap();
    if !exhaust {
        assert_eq!(response.status(), StatusCode::OK, "{mode:?}: admitted idle start must deliver current media");
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        if debounce {
            let seq = super::manifest_media_sequence(&body);
            assert!(session.read().await.segments[&seq].origin_key.host_local_sequence >= 106);
        }
        let repeated = canonical_response_for_lease(&app, &input, &url, "warm").await;
        assert_eq!(repeated.status(), StatusCode::OK);
        assert_eq!(
            session.read().await.origin_account_binding.as_ref().unwrap().startup_protection,
            binding.startup_protection,
            "polling retains the new admitted start anchor"
        );
        let uri = body.lines().find(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
        let media = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(media.status(), StatusCode::OK);
        assert!(!response_body(media).await.is_empty());
        for handle in occupied {
            app.active_provider.release_handle(&handle);
        }
        return;
    }
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let after =
        app.hls.proxy.access_lease_response_snapshot(&id, &proxy, super::super::current_time_millis()).await.unwrap();
    assert_eq!(after.last_manifest_snapshot, before.last_manifest_snapshot);
    assert_eq!(after.playback_cursor, before.playback_cursor);
    assert!(after.manifest_start_attempt.unwrap().failed());
    assert!(app.active_provider.binding_tag_for_owner(&binding.session_owner).is_none());
    tokio::time::resume();
}
