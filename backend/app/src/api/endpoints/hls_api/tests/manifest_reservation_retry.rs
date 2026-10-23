use super::{
    create_active_hls_user_session, create_bound_hls_test_session, enable_channel_unavailable_custom_response,
    manifest_entry_freshness::{
        canonical_response_for_lease, enable_hls_cache_with_startup_mode, origin_playlist_with_duration,
    },
    path_has_extension, prepare_pending_test_hls_access_lease, response_body, spawn_test_binary_origin,
    test_app_state_with_inputs, try_test_hls_cached_manifest_response, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsCachedManifestOptions, HlsSessionKey, StripConfig, TestBinaryOriginResponse,
};
use axum::http::StatusCode;
use shared::model::{HlsStartupMode, HlsStripMode, InputType};
use std::{sync::Arc, time::Duration};
use tuliprox_hls::{
    api::{
        evaluate_lease_reserve, HlsAccessLease, HlsFreshManifestRequiredReason, HlsLeasePlaybackMode,
        HlsLeaseReserveInput, HlsManifestCommitRequirement, HlsRecoveryTriggerBudgetMs, HlsSessionHandle,
    },
    HlsManifestAttemptContext, HlsOriginPathCondition,
};

const MODES: [HlsStartupMode; 4] =
    [HlsStartupMode::Conservative, HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic];

async fn assert_playback_reserve_preserved(
    app: &Arc<crate::api::model::AppState>,
    session: &HlsSessionHandle,
    before: &HlsAccessLease,
) {
    let now_ms = super::super::current_time_millis();
    let current =
        app.hls.proxy.access_lease_response_snapshot(&before.lease_id, &before.proxy_session_id, now_ms).await.unwrap();
    assert_eq!(current.state, HlsAccessLeaseState::Activated);
    assert_eq!(current.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(current.last_manifest_snapshot, before.last_manifest_snapshot);
    assert_eq!(current.playback_cursor, before.playback_cursor);
    let snapshot = current.last_manifest_snapshot.as_ref().unwrap();
    let timeline = session.read().await.ready_timeline_snapshot(
        current.playback_cursor.ready_timeline_start_proxy_seq(snapshot.first_proxy_seq),
        now_ms,
    );
    let reserve_at = |lease: &HlsAccessLease| {
        evaluate_lease_reserve(HlsLeaseReserveInput {
            manifest: lease.last_manifest_snapshot.as_ref().unwrap(),
            cursor: &lease.playback_cursor,
            ready_timeline: &timeline,
            now_ms,
            recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(0),
            origin_path_degraded: true,
            recovery_committed: false,
        })
    };
    assert_eq!(reserve_at(&current), reserve_at(before), "pending clients must not change the active reserve");
    assert!(reserve_at(&current).playback_position_known);
    assert!(reserve_at(&current).guaranteed_reserve_ms > 0);
}

/// Waits until background segment fetches of the session no longer hold its origin account I/O slot.
///
/// A fetch of the old generation that is still running keeps the old slot occupied after the explicitly held guard is
/// released, so the retry of the pending clients would keep being denied for as long as that fetch takes.
async fn wait_for_origin_io_idle(session: &HlsSessionHandle) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let idle = session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .is_none_or(|lease| !lease.is_active_or_acquiring());
            if idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background origin I/O settles");
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "Keep the real active playback, two pending clients and I/O gate together.")]
async fn pending_clients_share_reservation_retry_without_consuming_active_playback_reserve_all_modes() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    for mode in MODES {
        let manifest_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&manifest_hits);
        let manifest_head = Arc::new(AtomicU64::new(100));
        let head = Arc::clone(&manifest_head);
        let origin = spawn_test_binary_origin(Arc::new(move |path| {
            let body = if path_has_extension(path, "m3u8") {
                hits.fetch_add(1, Ordering::SeqCst);
                Arc::from(origin_playlist_with_duration(head.load(Ordering::SeqCst), 12))
            } else {
                Arc::from(&[0x47; 8192][..])
            };
            TestBinaryOriginResponse::new(StatusCode::OK, body)
        }))
        .await;
        let input = crate::model::ConfigInput {
            id: 1,
            name: Arc::from("reserved-provider"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".into()),
            password: Some("pass".into()),
            max_connections: 1,
            enabled: true,
            ..Default::default()
        };
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache_with_startup_mode(&app, mode).await;
        create_active_hls_user_session(&app).await;
        let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let response = canonical_response_for_lease(&app, &input, &url, "warm").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        let media_uri = body.lines().find(|line| !line.starts_with('#') && !line.trim().is_empty()).unwrap();
        let media = super::get_response(Arc::clone(&app), media_uri, None).await;
        assert_eq!(media.status(), StatusCode::OK);
        assert!(!response_body(media).await.is_empty());
        let key = super::super::build_hls_origin_source(&input, "12345").session_key();
        let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
        let proxy = session.read().await.proxy_session_id.clone();
        let active = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let lease = app
                    .hls
                    .proxy
                    .access_lease_response_snapshot(
                        &HlsAccessLeaseId("warm".into()),
                        &proxy,
                        super::super::current_time_millis(),
                    )
                    .await
                    .unwrap();
                if lease.playback_cursor.highest_contiguous_completed_proxy_seq.is_some() {
                    break lease;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_playback_reserve_preserved(&app, &session, &active).await;
        let io = tuliprox_hls::api::HlsOriginIoContext {
            ctx: app.hls_ctx(),
            client_addr: "127.0.0.1:1234".parse().unwrap(),
            allow_grace: false,
            priority: 0,
            connection_kind: tuliprox_session::ConnectionKind::Normal,
            reservation_ttl_secs: 60,
            preacquired_provider_handle: None,
            started_generation: None,
            prepared_account: None,
            playback_kind: tuliprox_core::model::PlaybackKind::LiveHls,
            account_runtime: None,
        };
        let old_binding = session.read().await.origin_account_binding.clone().unwrap();
        // The held guard must be the only old-generation I/O: a background fetch that joined the same slot would
        // outlive the release and keep denying the pending clients.
        let old_guard = loop {
            wait_for_origin_io_idle(&session).await;
            let guard = tuliprox_hls::api::begin_hls_origin_account_io_bounded(
                &io,
                &session,
                &old_binding,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            let sole_holder = session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .is_some_and(|lease| lease.active_io_count.load(Ordering::Acquire) == 1);
            if sole_holder {
                break guard;
            }
            drop(guard);
        };
        session.write().await.origin_account_binding = Some(tuliprox_hls::api::HlsOriginAccountBinding::rebound(
            old_binding.input_name,
            old_binding.account_name,
            old_binding.session_owner,
            old_binding.generation + 1,
            super::super::current_time_millis(),
        ));
        for id in ["pending-one", "pending-two"] {
            prepare_pending_test_hls_access_lease(&app, &proxy, &HlsAccessLeaseId(id.into())).await;
        }
        {
            let mut state = session.write().await;
            state.origin_refresh.last_success_at_ms = Some(super::super::current_time_millis().saturating_sub(120_000));
            state.origin_refresh.next_fetch_allowed_at_ms = 0;
        }
        let before_hits = manifest_hits.load(Ordering::SeqCst);
        let gate = async {
            let attempt = loop {
                let mut attempts = Vec::new();
                for id in ["pending-one", "pending-two"] {
                    if let Some(lease) = app
                        .hls
                        .proxy
                        .access_lease_response_snapshot(
                            &HlsAccessLeaseId(id.into()),
                            &proxy,
                            super::super::current_time_millis(),
                        )
                        .await
                    {
                        assert_eq!(lease.state, HlsAccessLeaseState::Pending);
                        assert!(lease.last_manifest_snapshot.is_none());
                        if let Some(attempt) = lease.manifest_start_attempt {
                            attempts.push(attempt);
                        }
                    }
                }
                if attempts.len() == 2 && attempts[0].logical_attempts() > 0 {
                    assert!(Arc::ptr_eq(&attempts[0], &attempts[1]), "distinct pending leases share one budget");
                    break Arc::clone(&attempts[0]);
                }
                tokio::task::yield_now().await;
            };
            assert!(!attempt.is_closed());
            assert_eq!(manifest_hits.load(Ordering::SeqCst), before_hits, "no HTTP through the blocked binding");
            {
                let state = session.read().await;
                assert!(state.origin_refresh.in_flight);
                assert!(Arc::ptr_eq(state.origin_refresh.attempt.as_ref().unwrap(), &attempt));
            }
            assert_eq!(
                session.read().await.origin_control.path_condition,
                HlsOriginPathCondition::RetryableFetchFailure
            );
            assert_playback_reserve_preserved(&app, &session, &active).await;
            // Only a real advancing origin response may requalify new entry;
            // the local denial itself must not create media progress.
            manifest_head.store(101, Ordering::SeqCst);
            // The shared retry keeps denying (and counting) on its real-time backoff while the old binding is
            // held, so the number of denials depends on how long this gate takes on a loaded machine.
            let denials = attempt.logical_attempts();
            tuliprox_hls::api::finish_hls_origin_account_io(&io, &session, old_guard, false).await;
            (attempt, denials)
        };
        let outcomes = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                canonical_response_for_lease(&app, &input, &url, "pending-one"),
                canonical_response_for_lease(&app, &input, &url, "pending-two"),
                gate,
            )
        })
        .await;
        if outcomes.is_err() {
            for id in ["pending-one", "pending-two"] {
                let lease = app
                    .hls
                    .proxy
                    .access_lease_response_snapshot(
                        &HlsAccessLeaseId(id.into()),
                        &proxy,
                        super::super::current_time_millis(),
                    )
                    .await;
                eprintln!(
                    "{mode:?} {id}: {:?}",
                    lease.map(|lease| (
                        lease.state,
                        lease.last_manifest_snapshot.is_some(),
                        lease.manifest_start_attempt.map(|a| (a.logical_attempts(), a.is_closed(), a.failed())),
                    ))
                );
            }
            let state = session.read().await;
            eprintln!(
                "{mode:?} refresh: in_flight={} path={:?} attempt={:?}",
                state.origin_refresh.in_flight,
                state.origin_control.path_condition,
                state.origin_refresh.attempt.as_ref().map(|a| (a.logical_attempts(), a.is_closed(), a.failed()))
            );
        }
        let (first, second, (attempt, denials)) =
            outcomes.expect("both pending clients finish through the shared retry");
        for response in [first, second] {
            assert_eq!(response.status(), StatusCode::OK, "{mode:?}");
            assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
        }
        assert_eq!(attempt.logical_attempts(), denials + 1, "the denials while blocked plus one shared fetch");
        assert!(attempt.is_closed() && !attempt.failed());
        assert_eq!(manifest_hits.load(Ordering::SeqCst), before_hits + 1);
        assert_playback_reserve_preserved(&app, &session, &active).await;
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keeps both real reservation gates and the first successful media request in one scenario."
)]
async fn transient_real_provider_reservation_recovers_same_request_all_modes() {
    for mode in MODES {
        for io_conflict in [false, true] {
            let origin = spawn_test_binary_origin(Arc::new(|path| {
                let body: Arc<[u8]> = if path_has_extension(path, "m3u8") {
                    Arc::from(origin_playlist_with_duration(100, 2))
                } else {
                    Arc::from(&[0x47; 8192][..])
                };
                TestBinaryOriginResponse::new(StatusCode::OK, body)
            }))
            .await;
            let input = crate::model::ConfigInput {
                id: 1,
                name: Arc::from("reserved-provider"),
                input_type: InputType::Xtream,
                url: origin.base_url.clone(),
                username: Some("user".into()),
                password: Some("pass".into()),
                max_connections: 1,
                enabled: true,
                ..Default::default()
            };
            let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
            enable_hls_cache_with_startup_mode(&app, mode).await;
            create_active_hls_user_session(&app).await;
            let session =
                create_bound_hls_test_session(&app, &input, "12345", &input.name, super::super::current_time_millis())
                    .await;
            let origin_io = tuliprox_hls::api::HlsOriginIoContext {
                ctx: app.hls_ctx(),
                client_addr: "127.0.0.1:1234".parse().unwrap(),
                allow_grace: false,
                priority: 0,
                connection_kind: tuliprox_session::ConnectionKind::Normal,
                reservation_ttl_secs: 60,
                preacquired_provider_handle: None,
                started_generation: None,
                prepared_account: None,
                playback_kind: tuliprox_core::model::PlaybackKind::LiveHls,
                account_runtime: None,
            };
            let old_guard = if io_conflict {
                let binding = session.read().await.origin_account_binding.clone().unwrap();
                let guard = tuliprox_hls::api::begin_hls_origin_account_io_bounded(
                    &origin_io,
                    &session,
                    &binding,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                let current = session.read().await.origin_account_binding.clone().unwrap();
                let replacement = tuliprox_hls::api::HlsOriginAccountBinding::rebound(
                    Arc::clone(&current.input_name),
                    Arc::clone(&current.account_name),
                    current.session_owner.clone(),
                    current.generation + 1,
                    super::super::current_time_millis(),
                );
                session.write().await.origin_account_binding = Some(replacement);
                Some(guard)
            } else {
                app.active_provider.refresh_provider_reservation(&input.name, "foreign-playback", 60);
                app.active_provider.confirm_playback_activity("foreign-playback");
                let binding = session.read().await.origin_account_binding.clone().unwrap();
                assert!(app
                    .active_provider
                    .is_provider_reserved_for_other_session(&input.name, Some(&binding.session_owner)));
                None
            };
            let old_lease = session.read().await.origin_account_io_lease.clone();
            let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
            let response = canonical_response_for_lease(&app, &input, &url, "reserved-start");
            tokio::pin!(response);
            let release = async {
                // Observe the real denial before freeing the competing reservation.
                loop {
                    if session.read().await.origin_control.path_condition
                        == HlsOriginPathCondition::RetryableFetchFailure
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                if let Some(guard) = old_guard {
                    assert!(session
                        .read()
                        .await
                        .origin_account_io_lease
                        .as_ref()
                        .zip(old_lease.as_ref())
                        .is_some_and(|(current, old)| Arc::ptr_eq(current, old)));
                    tuliprox_hls::api::finish_hls_origin_account_io(&origin_io, &session, guard, false).await;
                } else {
                    app.active_provider.clear_provider_reservation("foreign-playback");
                }
            };
            let (response, ()) =
                tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(response, release) })
                    .await
                    .expect("bounded manifest request");
            assert_eq!(response.status(), StatusCode::OK, "{mode:?}");
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert!(!body.contains("#EXT-X-ENDLIST"), "must publish live playback on the original lease");
            assert!(body.contains("reserved-start"));
            let media_uri = body.lines().find(|line| !line.starts_with('#') && !line.trim().is_empty()).unwrap();
            let media = super::get_response(Arc::clone(&app), media_uri, None).await;
            assert_eq!(media.status(), StatusCode::OK, "{mode:?} io_conflict={io_conflict}");
            assert!(!response_body(media).await.is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn failed_start_before_and_after_publication_have_identical_fallback() {
    for mode in MODES {
        for video in [false, true] {
            for fail_before in [false, true] {
                let app = super::test_app_state();
                enable_hls_cache_with_startup_mode(&app, mode).await;
                if video {
                    enable_channel_unavailable_custom_response(&app);
                }
                let now = super::super::current_time_millis();
                let session = app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", now).await;
                let proxy = session.read().await.proxy_session_id.clone();
                let lease_id = HlsAccessLeaseId("failed-start".into());
                prepare_pending_test_hls_access_lease(&app, &proxy, &lease_id).await;
                let attempt = Arc::new(HlsManifestAttemptContext::new(
                    HlsManifestCommitRequirement::FreshCommitRequired {
                        reason: HlsFreshManifestRequiredReason::ColdStart,
                    },
                    false,
                    None,
                ));
                let mut lease = app.hls.proxy.access_lease_response_snapshot(&lease_id, &proxy, now).await.unwrap();
                lease.manifest_start_attempt = Some(Arc::clone(&attempt));
                app.hls.proxy.prepare_access_lease(lease).await;
                if fail_before {
                    attempt.finish(true);
                } else {
                    session.write().await.origin_refresh.in_flight = true;
                }
                let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
                let response = try_test_hls_cached_manifest_response(
                    &app,
                    &session,
                    &lease_id,
                    HlsAccessLeaseState::Pending,
                    &strip,
                    None,
                    HlsCachedManifestOptions::initial(Duration::from_secs(2)),
                );
                // With uncontended fixture locks, the first Pending must be the
                // evidence timer after guard preparation, not a cooperative yield.
                let response = tokio::task::unconstrained(response);
                tokio::pin!(response);
                if !fail_before {
                    assert!(futures::poll!(&mut response).is_pending(), "guard prepared, waiting for evidence");
                    attempt.finish(true);
                }
                let response = match response.await {
                    Some(response) => response,
                    None => {
                        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy, &lease_id).await
                    }
                };
                assert_eq!(
                    response.status(),
                    if video { StatusCode::OK } else { StatusCode::NOT_FOUND },
                    "{mode:?} video={video} fail_before={fail_before}"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
#[allow(
    clippy::too_many_lines,
    reason = "Checks exhaustion, repeated access and a fresh master against the same real reservation."
)]
async fn persistent_reservation_closes_once_and_new_master_can_restart_all_modes() {
    for mode in MODES {
        for video in [false, true] {
            let origin = spawn_test_binary_origin(Arc::new(|path| {
                TestBinaryOriginResponse::new(
                    StatusCode::OK,
                    if path_has_extension(path, "m3u8") {
                        Arc::from(origin_playlist_with_duration(100, 2))
                    } else {
                        Arc::from(&[0x47; 8192][..])
                    },
                )
            }))
            .await;
            let input = crate::model::ConfigInput {
                id: 1,
                name: Arc::from("reserved-provider"),
                input_type: InputType::Xtream,
                url: origin.base_url.clone(),
                username: Some("user".into()),
                password: Some("pass".into()),
                max_connections: 1,
                enabled: true,
                ..Default::default()
            };
            let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
            enable_hls_cache_with_startup_mode(&app, mode).await;
            if video {
                enable_channel_unavailable_custom_response(&app);
            }
            create_active_hls_user_session(&app).await;
            let session =
                create_bound_hls_test_session(&app, &input, "12345", &input.name, super::super::current_time_millis())
                    .await;
            let proxy = session.read().await.proxy_session_id.clone();
            app.active_provider.refresh_provider_reservation(&input.name, "foreign-playback", 60);
            assert!(app.active_provider.confirm_playback_activity("foreign-playback").is_some());
            let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
            let started = tokio::time::Instant::now();
            let response = canonical_response_for_lease(&app, &input, &url, "failed-start").await;
            assert!(started.elapsed() < Duration::from_secs(3), "{mode:?} video={video}");
            assert_eq!(response.status(), if video { StatusCode::OK } else { StatusCode::NOT_FOUND });
            let lease_id = HlsAccessLeaseId("failed-start".into());
            let lease = app
                .hls
                .proxy
                .access_lease_response_snapshot(&lease_id, &proxy, super::super::current_time_millis())
                .await
                .unwrap();
            let attempt = lease.manifest_start_attempt.unwrap();
            assert!(attempt.failed());
            assert_eq!(attempt.logical_attempts(), 5);
            assert_eq!(origin.manifest_request_count(), 0);
            assert_eq!(origin.segment_request_count(), 0);
            app.active_provider.clear_provider_reservation("foreign-playback");
            let repeated = canonical_response_for_lease(&app, &input, &url, "failed-start").await;
            assert_eq!(repeated.status(), if video { StatusCode::OK } else { StatusCode::NOT_FOUND });
            assert_eq!(attempt.logical_attempts(), 5);
            assert_eq!(
                origin.manifest_request_count(),
                0,
                "availability and repeated access must not restart the failed attempt"
            );
            let mut user = crate::model::ProxyUserCredentials::default();
            user.username = "hls-user".into();
            user.password = "hls-pass".into();
            let mut target = super::test_m3u_hls_share_target();
            target.name = "default".into();
            super::store_test_sources_with_target(&app, input.clone(), target.clone());
            super::cache_test_m3u_hls_item(&app, &target, super::test_m3u_hls_item(&input, 12345, "12345", &url)).await;
            super::configure_default_test_server(&app);
            let master = super::super::create_hls_cache_entry_master_playlist_response(
                &app,
                &super::test_fingerprint(),
                &user,
                super::super::build_hls_origin_source(&input, "12345"),
                12345,
                None,
                None,
                None,
                &url,
                &input,
                shared::model::UserConnectionPermission::Allowed,
                Some(tuliprox_session::ConnectionKind::Normal),
                None,
            )
            .await;
            let variant = super::single_variant_uri(master).await;
            assert_ne!(super::access_lease_id_from_variant_uri(&variant), "failed-start");
            // Only the denial budget and response deadline use virtual time. Local HTTP
            // and segment streaming run with the normal I/O clock.
            tokio::time::resume();
            let response = super::get_response(Arc::clone(&app), &variant, None).await;
            assert_eq!(response.status(), StatusCode::OK, "new master {mode:?} video={video}");
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert!(!body.contains("#EXT-X-ENDLIST"));
            assert!(origin.manifest_request_count() > 0);
            tokio::time::pause();
        }
    }
}
