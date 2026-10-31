use super::{
    manifest_entry_freshness::open_manifest_for_new_lease,
    origin_account_rebind::{enable_isolated_rebind_cache, resume_origin, resume_origin_media},
    *,
};
use shared::model::{HlsStartupConfigDto, HlsStartupMode};
use std::sync::atomic::AtomicU64;
use tuliprox_core::utils::current_time_millis;

const PAUSE_MS: u64 = 130_000;
const WAIT: Duration = Duration::from_secs(5);

async fn settle_origin(session: &HlsSessionHandle) {
    tokio::time::timeout(WAIT, async {
        loop {
            let s = session.read().await;
            if !s.origin_refresh.in_flight
                && s.active_segment_fetches == 0
                && s.activity.active_origin_work_count.load(Ordering::Acquire) == 0
            {
                break;
            }
            drop(s);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("origin work settles before playback is stopped");
}

/// Model wall-clock absence without erasing actual media delivery, or relying on
/// Tokio's clock (account preparation and HTTP serving also read `SystemTime`).
async fn pause_playback(app: &Arc<AppState>, session: &HlsSessionHandle, lease: &str) {
    settle_origin(session).await;
    assert!(app
        .hls
        .proxy
        .segment_worker_pool()
        .access_leases()
        .write()
        .await
        .remove_access_lease(&HlsAccessLeaseId(lease.to_string()))
        .is_some());
    app.active_users.release_session_streams_and_counted_reservation("hls-user", "hls-session-token").await;
    let now = current_time_millis();
    let proxy_id = {
        let mut s = session.write().await;
        let age = |time: &mut Option<u64>| *time = time.map(|time| time.saturating_sub(PAUSE_MS));
        age(&mut s.activity.last_authorized_manifest_at_ms);
        age(&mut s.activity.last_authorized_media_at_ms);
        age(&mut s.activity.last_delivered_media_at_ms);
        s.last_client_access_at_ms = s.last_client_access_at_ms.saturating_sub(PAUSE_MS);
        let binding = s.origin_account_binding.as_mut().unwrap();
        binding.pinned_at_ms = binding.pinned_at_ms.saturating_sub(PAUSE_MS);
        age(&mut binding.startup_media_deadline_ms);
        if let Some(start) = binding.startup_protection.as_mut() {
            start.anchored_at_ms = start.anchored_at_ms.saturating_sub(PAUSE_MS);
        }
        age(&mut binding.last_origin_io_at_ms);
        age(&mut binding.last_reservation_refresh_at_ms);
        age(&mut s.origin_refresh.last_fetch_started_at_ms);
        age(&mut s.origin_refresh.last_fetch_finished_at_ms);
        age(&mut s.origin_refresh.last_success_at_ms);
        age(&mut s.origin_refresh.last_error_at_ms);
        s.origin_refresh.next_fetch_allowed_at_ms = s.origin_refresh.next_fetch_allowed_at_ms.saturating_sub(PAUSE_MS);
        if let Some(rendered) = s.last_rendered_manifest.as_mut() {
            rendered.rendered_at_ms = rendered.rendered_at_ms.saturating_sub(PAUSE_MS);
            rendered.valid_until_ms = rendered.valid_until_ms.saturating_sub(PAUSE_MS);
        }
        assert!(s.activity.last_delivered_media_at_ms.is_some_and(|last| now - last >= PAUSE_MS));
        assert!(!s.can_expire_idle_session(now, 300_000), "retained session is inside its 300-second timeout");
        s.proxy_session_id.clone()
    };
    app.hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app.active_users,
            &app.active_provider,
            session,
            &proxy_id,
            now,
        )
        .await;
    super::super::detach_unprotected_hls_origin_account_bindings(app, now).await;
    assert!(session.read().await.origin_account_binding.as_ref().unwrap().is_detached());
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "pause releases provider slots");
    for account in ["account-a", "account-b"] {
        assert_eq!(
            app.active_provider.provider_lease_usage(&Arc::from(account)).total(),
            0,
            "no logical slot remains for {account}"
        );
    }
}

fn segment_uris(manifest: &str) -> Vec<&str> {
    assert!(manifest.starts_with("#EXTM3U"), "live playlist: {manifest}");
    assert!(!manifest.contains("/cvs/hls/"), "replacement video must never satisfy resume: {manifest}");
    assert!(!manifest.contains("#EXT-X-ENDLIST"), "resume remains live");
    let uris = manifest.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect::<Vec<_>>();
    assert!(!uris.is_empty(), "live media playlist must contain media");
    uris
}

async fn deliver_origin_segment(app: &Arc<AppState>, session: &HlsSessionHandle, manifest: &str, head: u64) {
    let uri = segment_uris(manifest)[0];
    let seq = uri.rsplit('/').next().unwrap().trim_end_matches(".ts").parse::<u64>().unwrap();
    let origin_url = {
        let s = session.read().await;
        let segment = &s.segments[&seq];
        assert!(
            (head..head + 6).contains(&segment.origin_key.host_local_sequence),
            "current origin window: expected {head}..{}; got {} at proxy {seq}; manifest={manifest}",
            head + 6,
            segment.origin_key.host_local_sequence
        );
        segment.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.clone()
    };
    let path = reqwest::Url::parse(&origin_url).unwrap().path().to_string();
    let response = get_response(Arc::clone(app), uri.strip_prefix("/iptv").unwrap_or(uri), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert_eq!(body.as_ref(), resume_origin_media(&path), "client receives the exact current origin object");
    let s = session.read().await;
    assert!(s.activity.last_authorized_media_at_ms.is_some());
    assert!(s.activity.last_delivered_media_at_ms.is_some(), "fully consume the Tuliprox response before pausing");
    eprintln!(
        "delivered origin={path} bytes={} authorized={:?} delivered={:?}",
        body.len(),
        s.activity.last_authorized_media_at_ms,
        s.activity.last_delivered_media_at_ms
    );
}

#[expect(clippy::too_many_lines, reason = "Keep the ordered pause/resume scenario and its assertions together.")]
async fn same_account_resume(mode: HlsStartupMode, resumes: usize) {
    let head = Arc::new(AtomicU64::new(1113));
    let origin = resume_origin(Arc::clone(&head)).await;
    let cache = tempfile::tempdir().unwrap();
    let mut input = overlap_provider_input();
    input.name = Arc::from("account-a");
    input.priority = 0;
    input.url.clone_from(&origin.base_url);
    let b = &mut input.aliases.as_mut().unwrap()[0];
    b.name = Arc::from("account-b");
    b.priority = 10;
    b.url.clone_from(&origin.base_url);
    let app = test_app_state_with_hls_proxy_and_inputs(
        Arc::new(HlsProxyManager::with_cache_settings(cache.path(), 300)),
        vec![Arc::new(input.clone())],
    );
    let available_b = HlsOriginAccountBinding::new(
        Arc::clone(&input.name),
        Arc::from("account-b"),
        &ProxySessionId("availability-check".into()),
        current_time_millis(),
    );
    assert_eq!(
        tuliprox_hls::api::hls_origin_account_status(&app.hls_ctx(), &available_b),
        tuliprox_hls::api::HlsOriginAccountStatus::Known
    );
    enable_isolated_rebind_cache(&app, cache.path());
    let mut config = app.app_config.config.load_full().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().startup =
        (&HlsStartupConfigDto { mode, ..HlsStartupConfigDto::default() }).into();
    app.app_config.config.store(Arc::new(config));
    enable_channel_unavailable_custom_response(&app);
    app.hls.proxy.update_config(&app.app_config).await;
    assert_eq!(app.hls.proxy.session_idle_timeout_ms(), 300_000);
    create_active_hls_user_session(&app).await;
    let url = format!("{}/live/root-user/root-pass/12345.m3u8", origin.base_url);
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let mut lease = "resume-initial".to_string();
    let first = open_manifest_for_new_lease(&app, &input, &url, &lease).await;
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    deliver_origin_segment(&app, &session, &first, 1113).await;
    settle_origin(&session).await;
    let proxy_id = session.read().await.proxy_session_id.clone();

    for cycle in 0..resumes {
        let old = session.read().await.origin_account_binding.clone().unwrap();
        assert_eq!(old.account_name.as_ref(), "account-a");
        pause_playback(&app, &session, &lease).await;
        let current_head = 1126 + u64::try_from(cycle).unwrap() * 13;
        head.store(current_head, Ordering::Release);
        lease = format!("resume-{cycle}");
        create_active_hls_user_session(&app).await;
        let boundary = origin.requests.lock().await.len();
        let started_at = current_time_millis();
        let barrier = app.hls.proxy.segment_worker_pool().pause_next_origin_prepare_for_test();
        let (task_app, task_input, task_url, task_lease) =
            (Arc::clone(&app), input.clone(), url.clone(), lease.clone());
        let opening =
            tokio::spawn(
                async move { open_manifest_for_new_lease(&task_app, &task_input, &task_url, &task_lease).await },
            );
        tokio::time::timeout(WAIT, barrier.entered.notified()).await.expect("manifest commit reaches scheduler");
        let retained = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
        assert!(Arc::ptr_eq(&session, &retained), "same shared session allocation");
        let (rebound, scheduled, delivered_before_prepare) = {
            let s = session.read().await;
            assert_eq!(s.key, key);
            assert_eq!(s.proxy_session_id, proxy_id);
            assert_eq!(s.target_duration, Some(11));
            let rebound = s.origin_account_binding.clone().unwrap();
            assert_eq!(rebound.account_name, old.account_name, "B is available; A is actually rebound");
            assert!(rebound.is_active());
            assert!(rebound.generation > old.generation);
            assert!(rebound.pinned_at_ms >= started_at);
            let mut scheduled = s
                .segment_prefetch_queue
                .proxy_seqs()
                .into_iter()
                .map(|seq| {
                    let entry = &s.segments[&seq];
                    assert!((current_head..current_head + 6).contains(&entry.origin_key.host_local_sequence));
                    (seq, entry.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.clone())
                })
                .collect::<Vec<_>>();
            scheduled.sort();
            assert!(!scheduled.is_empty(), "capture jobs immediately after commit, before preparation");
            eprintln!("{mode:?} cycle={cycle} shared={key:?} proxy={proxy_id:?} account={} binding={}->{} pinned={} scheduled={scheduled:?}", rebound.account_name, old.generation, rebound.generation, rebound.pinned_at_ms);
            (rebound, scheduled, s.activity.last_delivered_media_at_ms)
        };
        barrier.prepare.notify_one();
        tokio::time::timeout(WAIT, barrier.prepared.notified()).await.expect("resource preparation completes");
        eprintln!(
            "requests through prepare={:?}",
            origin.requests.lock().await[boundary..]
                .iter()
                .map(|request| request.lines().next().unwrap())
                .collect::<Vec<_>>()
        );
        {
            let s = session.read().await;
            eprintln!(
                "after prepare: binding={:?} queue={:?} states={:?}",
                s.origin_account_binding,
                s.segment_prefetch_queue.proxy_seqs(),
                scheduled.iter().map(|(seq, _)| (*seq, &s.segments[seq].status)).collect::<Vec<_>>()
            );
            assert!(
                scheduled.iter().all(|(seq, _)| matches!(
                    s.segments[seq].status,
                    SegmentCacheStatus::Queued { .. }
                        | SegmentCacheStatus::Fetching { .. }
                        | SegmentCacheStatus::Ready { .. }
                )),
                "manifest segment jobs were lost during resource preparation"
            );
            let current = s.origin_account_binding.as_ref().unwrap();
            assert_eq!(current.generation, rebound.generation, "fresh binding stays stable into media work");
            assert_eq!(current.pinned_at_ms, rebound.pinned_at_ms);
            assert_eq!(
                s.activity.last_delivered_media_at_ms, delivered_before_prepare,
                "manifest fetch is not media delivery"
            );
        }
        barrier.proceed.notify_one();
        // No demand fetch or test-side requeue: every original job must progress autonomously.
        tokio::time::timeout(WAIT, async {
            loop {
                let s = session.read().await;
                if scheduled.iter().all(|(seq, _)| matches!(s.segments[seq].status, SegmentCacheStatus::Ready { .. })) {
                    break;
                }
                drop(s);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all originally scheduled resources reach Ready");
        let manifest = tokio::time::timeout(WAIT, opening).await.unwrap().unwrap();
        let requests = origin.requests.lock().await[boundary..]
            .iter()
            .map(|request| request.lines().next().unwrap().to_string())
            .collect::<Vec<_>>();
        eprintln!("{mode:?} cycle={cycle} origin requests={requests:?}");
        for (_, url) in &scheduled {
            let path = reqwest::Url::parse(url).unwrap().path().to_string();
            assert!(
                requests.iter().any(|line| line == &format!("GET {path} HTTP/1.1")),
                "scheduled origin request: {path}"
            );
        }
        assert!(requests.iter().all(|line| !line.contains("account-user")), "B remains unused");
        deliver_origin_segment(&app, &session, &manifest, current_head).await;
        assert_eq!(session.read().await.origin_account_binding.as_ref().unwrap().generation, rebound.generation);
    }
    pause_playback(&app, &session, &lease).await;
    assert!(origin.requests.lock().await.iter().all(|request| !request.contains("account-user")));
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "no provider slot leak after repeated pauses");
}

#[tokio::test]
async fn hls_same_account_resume_keeps_scheduled_segments() {
    same_account_resume(HlsStartupMode::Optimistic, 2).await;
}

#[tokio::test]
async fn hls_same_account_resume_keeps_scheduled_segments_conservative() {
    same_account_resume(HlsStartupMode::Conservative, 2).await;
}
