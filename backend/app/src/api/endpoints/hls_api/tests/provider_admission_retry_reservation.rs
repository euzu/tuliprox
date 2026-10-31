//! Retry-operation capacity rights: real provider transitions with controlled time.
use super::{single_hls_provider_input, test_app_state_with_inputs, test_fingerprint};
use std::{sync::Arc, time::Duration};
use tuliprox_core::model::{PlaybackKind, ProviderHandle};
use tuliprox_session::{ConnectionKind, PlaybackLeaseRef};

#[tokio::test]
async fn timeout_backoff_and_followup_preparation_keep_admission_at_limits_one_and_three() {
    for limit in [1, 3] {
        for success in [true, false] {
            assert_retry_http_lifecycle(limit, success, false).await;
        }
    }
}

#[tokio::test]
async fn cancelling_one_waiter_during_backoff_preserves_shared_retry_work() {
    assert_retry_http_lifecycle(1, true, true).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the timeout, two synchronization phases, result and cleanup in one end-to-end scenario."
)]
async fn assert_retry_http_lifecycle(limit: u16, success: bool, cancel_waiter: bool) {
    use super::{
        create_active_hls_user_session, create_unbound_hls_test_session, get_response,
        manifest_entry_freshness::{canonical_response_for_lease, enable_hls_cache_with_startup_mode},
        path_has_extension, response_body, spawn_test_binary_origin, TestBinaryOriginResponse,
    };
    use axum::http::StatusCode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tuliprox_hls::api::{HlsAccessLeaseId, SegmentCacheStatus};
    let attempts = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&attempts);
    let (_release, gate) = tokio::sync::watch::channel(false);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        if path_has_extension(path.split('?').next().unwrap(), "m3u8") {
            return TestBinaryOriginResponse::new(
                StatusCode::OK,
                Arc::from(&b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:2,\n3.ts\n"[..]),
            );
        }
        let selected = path.split('?').next().unwrap().ends_with("/3.ts");
        let first = selected && hits.fetch_add(1, Ordering::SeqCst) == 0;
        let mut response = TestBinaryOriginResponse::new(
            if selected {
                if success {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            } else {
                StatusCode::NOT_FOUND
            },
            Arc::from(&[0x47; 8192][..]),
        );
        if first {
            response.gate = Some(gate.clone());
        }
        response
    }))
    .await;
    let mut input = single_hls_provider_input("retry-provider");
    input.input_type = shared::model::InputType::M3u;
    input.url = origin.base_url.clone();
    input.max_connections = limit;
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache_with_startup_mode(&app, shared::model::HlsStartupMode::FirstReady).await;
    let mut config = app.app_config.config.load().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().strip.value = 0;
    config.custom_stream_response_enabled = false;
    config.custom_stream_response_error_status = 502;
    app.app_config.config.store(Arc::new(config));
    app.hls.proxy.update_config(&app.app_config).await;
    create_active_hls_user_session(&app).await;
    let mut policy = app.hls.proxy.segment_fetch_policy();
    let mut timeouts = policy.timeouts.config();
    timeouts.segment = shared::model::HlsSegmentTimeout::Fixed(shared::model::Millis::new(10_000));
    timeouts.manifest_ms = 10_000;
    policy.timeouts.update(timeouts);
    policy.retry_delays_ms = [0, 100, 100, 100, 100];
    policy.retry_jitter_max_ms = 0;
    app.hls.proxy.segment_worker_pool().update_config(policy, Arc::new(tokio::sync::Semaphore::new(24)));
    let mut occupied = Vec::new();
    for index in 1..limit {
        occupied.push(acquire(&app, &input, &format!("other-{index}")).unwrap());
    }
    let session = create_unbound_hls_test_session(&app, &input, "12345", super::super::current_time_millis()).await;
    let retry = session.write().await.pause_next_origin_retry_for_test();
    // Precreating the session lets the test install barriers. Model the timely
    // Created-entry processing phase before exercising the Reused handler helper.
    let proxy = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("warm".into());
    super::prepare_pending_test_hls_access_lease(&app, &proxy, &lease_id).await;
    super::super::touch_initial_manifest_access_lease_window(
        &app,
        &lease_id,
        &proxy,
        tuliprox_hls::api::HlsAccessLeaseState::Pending,
        Duration::from_secs(app.hls.proxy.initial_manifest_wait_timeout_secs()),
        super::super::current_time_millis(),
    )
    .await;
    let url = format!("{}/index.m3u8", input.url);
    let mut request = {
        let app = Arc::clone(&app);
        let input = input.clone();
        let url = url.clone();
        tokio::spawn(async move { canonical_response_for_lease(&app, &input, &url, "warm").await })
    };
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = session.read().await;
            if state.origin_account_binding.as_ref().is_some_and(|binding| binding.startup_protection.is_some())
                && state.active_segment_fetches == 1
                && state.segments.len() == 1
                && matches!(state.segments[&0].status, SegmentCacheStatus::Fetching { .. })
            {
                break;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await;
    if settled.is_err() {
        let state = session.read().await;
        panic!(
            "hanging segment setup: active={} segments={:?}",
            state.active_segment_fetches,
            state.segments.iter().map(|(seq, entry)| (*seq, format!("{:?}", entry.status))).collect::<Vec<_>>()
        );
    }
    tokio::time::timeout(Duration::from_secs(2), retry.transfer_started.notified()).await.unwrap();
    let binding = session.read().await.origin_account_binding.clone().unwrap();
    let generation = session.read().await.activity.origin_work_generation;
    assert!(acquire(&app, &input, "competitor").is_none(), "active transfer owns the last slot");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(15), retry.retry_ready.notified())
        .await
        .expect("first timeout reaches backoff");
    if cancel_waiter {
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let app = Arc::clone(&app);
        let input = input.clone();
        let url = url.clone();
        request = tokio::spawn(async move { canonical_response_for_lease(&app, &input, &url, "survivor").await });
    }
    for phase in [0, 1] {
        super::super::detach_unprotected_hls_origin_account_bindings(
            &app,
            binding.startup_protection.unwrap().anchored_at_ms + 10_000,
        )
        .await;
        let state = session.read().await;
        assert_eq!(state.activity.origin_work_generation, generation);
        assert_eq!(state.origin_account_binding.as_ref().unwrap().generation, binding.generation);
        assert!(state.activity.active_origin_work_count.load(Ordering::Acquire) > 0);
        assert_eq!(app.active_provider.binding_tag_for_owner(&binding.session_owner), binding.provider_binding_tag);
        assert_eq!(app.active_provider.get_provider_connections_count(), usize::from(limit - 1));
        assert!(acquire(&app, &input, "competitor").is_none(), "phase {phase} retains the logical slot");
        drop(state);
        if phase == 0 {
            retry.resume_backoff.notify_one();
            tokio::time::timeout(Duration::from_secs(2), retry.prepare_ready.notified()).await.unwrap();
        }
    }
    retry.resume_prepare.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(8), request).await.unwrap().unwrap();
    let proxy = session.read().await.proxy_session_id.clone();
    if success {
        assert_eq!(response.status(), StatusCode::OK, "retry finishes before manifest publication");
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        assert_eq!(session.read().await.activity.origin_work_generation, generation);
        assert!(
            session.read().await.activity.last_delivered_media_at_ms.is_none(),
            "origin success is not media delivery"
        );
        let uri = body.lines().find(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
        let media = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(media.status(), StatusCode::OK);
        assert_eq!(response_body(media).await.as_ref(), &[0x47; 8192]);
    } else {
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
        assert!(lease.manifest_start_attempt.as_ref().unwrap().failed());
        assert_eq!(lease.manifest_start_attempt.as_ref().unwrap().provider_failure(), None);
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert_eq!(session.read().await.active_segment_fetches, 0);
        assert!(app.active_provider.binding_tag_for_owner(&binding.session_owner).is_none());
        let competitor = acquire(&app, &input, "competitor").expect("terminal failure releases capacity immediately");
        let seen = origin.segment_request_count();
        let repeated = canonical_response_for_lease(&app, &input, &url, "warm").await;
        assert_eq!(repeated.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(origin.segment_request_count(), seen);
        app.active_provider.release_handle(&competitor);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), if success { 2 } else { 5 });
    for handle in occupied {
        app.active_provider.release_handle(&handle);
    }
}

fn acquire(
    app: &Arc<crate::api::model::AppState>,
    input: &crate::model::ConfigInput,
    owner: &str,
) -> Option<ProviderHandle> {
    app.active_provider.acquire_exact_connection_with_lease_for_session(
        &input.name,
        &test_fingerprint().addr,
        false,
        0,
        ConnectionKind::Normal,
        Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
    )
}

#[tokio::test(start_paused = true)]
async fn work_right_keeps_the_last_slot_after_the_seven_second_start_expires() {
    for limit in [1, 3] {
        let mut input = single_hls_provider_input("retry-provider");
        input.max_connections = limit;
        let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let mut occupied = Vec::new();
        for index in 1..limit {
            occupied.push(acquire(&app, &input, &format!("other-{index}")).unwrap());
        }
        let owner = "hls-cache:retry";
        let handle = acquire(&app, &input, owner).unwrap();
        let tag = app.active_provider.reserve_hls_start(owner, &handle, Duration::from_secs(7)).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let work = app.active_provider.reserve_hls_origin_work(owner, &input.name, tag, deadline).unwrap();
        assert!(acquire(&app, &input, "competitor").is_none());
        app.active_provider.release_handle(&handle);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(app.active_provider.binding_tag_for_owner(owner), Some(tag));
        assert_eq!(app.active_provider.get_provider_connections_count(), usize::from(limit - 1));
        assert!(work.is_current());
        assert!(acquire(&app, &input, "competitor").is_none(), "backoff keeps the last logical slot");
        // Foreign generic opening leases have expired; their live allocations
        // still count. Only this operation retains a reservation in the table.
        assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 1);
        assert_eq!(app.active_provider.get_provider_connections_count() + 1, usize::from(limit));
        assert_eq!(app.active_provider.provider_lease_usage(&input.name).active, 0, "work is not media");
        drop(work);
        let competitor = acquire(&app, &input, "competitor").expect("no lease/GC delay after operation completion");
        app.active_provider.release_handle(&competitor);
        for handle in occupied {
            app.active_provider.release_handle(&handle);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn parallel_rights_expire_independently_and_old_cleanup_preserves_a_successor() {
    let input = single_hls_provider_input("retry-provider");
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let owner = "hls-cache:parallel-retry";
    let handle = acquire(&app, &input, owner).unwrap();
    let tag = app.active_provider.reserve_hls_start(owner, &handle, Duration::from_secs(7)).unwrap();
    let first = app
        .active_provider
        .reserve_hls_origin_work(owner, &input.name, tag, tokio::time::Instant::now() + Duration::from_secs(20))
        .unwrap();
    let second = app
        .active_provider
        .reserve_hls_origin_work(owner, &input.name, tag, tokio::time::Instant::now() + Duration::from_secs(30))
        .unwrap();
    app.active_provider.release_handle(&handle);
    tokio::time::advance(Duration::from_secs(10)).await;
    drop(first);
    assert!(second.is_current());
    assert!(acquire(&app, &input, "competitor").is_none());
    tokio::time::advance(Duration::from_secs(21)).await;
    assert!(app.active_provider.binding_tag_for_owner(owner).is_none(), "expiry needs no worker completion");
    let successor = acquire(&app, &input, owner).unwrap();
    let next = app.active_provider.reserve_hls_start(owner, &successor, Duration::from_secs(7)).unwrap();
    assert_ne!(tag, next);
    drop(second);
    assert_eq!(app.active_provider.binding_tag_for_owner(owner), Some(next));
    app.active_provider.release_handle(&successor);
}

#[tokio::test(start_paused = true)]
async fn work_rights_require_own_admission_and_cannot_chain_expired_normal_rights() {
    let input = single_hls_provider_input("retry-provider");
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let owner = "hls-cache:bounded-retry";
    let handle = acquire(&app, &input, owner).unwrap();
    let tag = app.active_provider.reserve_hls_start(owner, &handle, Duration::from_secs(7)).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    assert!(app.active_provider.reserve_hls_origin_work("foreign", &input.name, tag, deadline).is_none());
    let first = app.active_provider.reserve_hls_origin_work(owner, &input.name, tag, deadline).unwrap();
    app.active_provider.release_handle(&handle);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(app.active_provider.reserve_hls_origin_work(owner, &input.name, tag, deadline).is_none());
    let reused = acquire(&app, &input, owner).unwrap();
    assert!(
        app.active_provider.reserve_hls_origin_work(owner, &input.name, tag, deadline).is_none(),
        "an allocation backed solely by another operation cannot manufacture a new deadline"
    );
    app.active_provider.release_handle(&reused);
    drop(first);
    app.active_provider.clear_identified_provider_reservation(owner, &input.name, Some(tag));
    let foreign = acquire(&app, &input, "foreign").unwrap();
    assert!(app.active_provider.reserve_hls_origin_work(owner, &input.name, tag, deadline).is_none());
    assert_eq!(app.active_provider.get_provider_connections_count(), 1);
    app.active_provider.release_handle(&foreign);
}
