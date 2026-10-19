use super::{
    super::origin_refresh_request::{hls_origin_refresh_request, HlsOriginRefreshOptions},
    manifest_entry_freshness::open_manifest_for_new_lease,
    origin_account_rebind::{
        enable_isolated_rebind_cache, reduced_retry_origin, resume_origin_media, ReducedRetryOriginControl,
    },
    *,
};
use shared::model::{HlsManifestRecoveryBurstLevel, HlsStartupConfigDto, HlsStartupMode};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    trigger_origin_refresh_sync, HlsManifestCommitRequirement, HlsOriginAccountSnapshot, HlsPreparedOriginAccount,
};

const WAIT: Duration = Duration::from_secs(15);

async fn ready(app: &Arc<AppState>, session: &HlsSessionHandle, sequences: &[u64]) {
    tokio::time::timeout(WAIT, async {
        loop {
            let s = session.read().await;
            if sequences.iter().all(|seq| {
                s.segments.get(seq).is_some_and(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
            }) && s.active_segment_fetches == 0
                && !s.origin_refresh.in_flight
                && s.activity.active_origin_work_count.load(Ordering::Acquire) == 0
            {
                break;
            }
            drop(s);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("autonomous segment work and singleflight complete");
    assert!(app.active_provider.get_provider_connections_count() == 0);
}

async fn deliver(app: &Arc<AppState>, session: &HlsSessionHandle, manifest: &str, expected: &[u64]) {
    assert!(!manifest.contains("/cvs/hls/"));
    assert!(!manifest.contains("#EXT-X-ENDLIST"));
    for sequence in expected {
        let (proxy_seq, url) = {
            let s = session.read().await;
            let (seq, entry) =
                s.segments.iter().find(|(_, entry)| entry.origin_key.host_local_sequence == *sequence).unwrap();
            (*seq, entry.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.clone())
        };
        let uri = manifest
            .lines()
            .find(|line| line.ends_with(&format!("/{proxy_seq:06}.ts")))
            .expect("segment URI is published by Tuliprox");
        let response = get_response(Arc::clone(app), uri.strip_prefix("/iptv").unwrap_or(uri), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response_body(response).await;
        let path = reqwest::Url::parse(&url).unwrap().path().to_string();
        assert_eq!(bytes.as_ref(), resume_origin_media(&path));
        eprintln!("app delivered resource={path} bytes={}", bytes.len());
    }
}

async fn repaired_reduced_retry(mode: HlsStartupMode) {
    let control = Arc::new(ReducedRetryOriginControl::default());
    let origin = reduced_retry_origin(Arc::clone(&control)).await;
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
    let hls = config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap();
    hls.startup = (&HlsStartupConfigDto { mode, ..Default::default() }).into();
    hls.manifest_recovery_burst.level = HlsManifestRecoveryBurstLevel::Beast;
    app.app_config.config.store(Arc::new(config));
    enable_channel_unavailable_custom_response(&app);
    app.hls.proxy.update_config(&app.app_config).await;
    create_active_hls_user_session(&app).await;
    let url = format!("{}/live/root-user/root-pass/12345.m3u8", origin.base_url);
    let lease = "reduced-retry-baseline";
    let _initial = open_manifest_for_new_lease(&app, &input, &url, lease).await;
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    ready(&app, &session, &[0, 1, 2, 3, 4, 5]).await;
    let response = try_test_hls_cached_manifest_response(
        &app,
        &session,
        &HlsAccessLeaseId(lease.to_string()),
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .expect("complete baseline is published to the playback lease");
    assert_eq!(response.status(), StatusCode::OK);
    let first = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    deliver(&app, &session, &first, &[1000]).await;
    tokio::time::timeout(WAIT, async {
        let proxy_id = session.read().await.proxy_session_id.clone();
        loop {
            let snapshot = app
                .hls
                .proxy
                .access_lease_response_snapshot(&HlsAccessLeaseId(lease.to_string()), &proxy_id, current_time_millis())
                .await
                .unwrap();
            if snapshot.playback_cursor.highest_contiguous_completed_proxy_seq == Some(0) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("baseline body completion is recorded before recovery pressure is evaluated");
    let (binding, baseline, commits, proxy_id) = {
        let s = session.read().await;
        assert!(s.activity.last_authorized_media_at_ms.is_some());
        assert!(s.activity.last_delivered_media_at_ms.is_some());
        assert!(s.established_manifest_recovery_binding().is_some());
        (
            s.origin_account_binding.clone().unwrap(),
            s.origin_to_proxy.clone(),
            s.origin_manifest_commits(),
            s.proxy_session_id.clone(),
        )
    };
    assert_eq!(binding.account_name.as_ref(), "account-a");
    eprintln!(
        "{mode:?} baseline account={} binding_generation={} pinned_at_ms={} proxy_session={}",
        binding.account_name, binding.generation, binding.pinned_at_ms, proxy_id.0
    );
    session.write().await.origin_control.path_condition = tuliprox_hls::HlsOriginPathCondition::HardFetchFailure;
    let directive =
        match tuliprox_hls::api::hls_manifest_acceptance_directive_for_session(&app.hls_ctx(), &session, &proxy_id)
            .await
        {
            tuliprox_hls::api::HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
            other => panic!("recovery pressure must evaluate: {other:?}"),
        };
    eprintln!("app baseline directive={directive:?}");
    assert!(directive.recovery_pressure_guard.is_some(), "directive has current lease/session evidence");
    assert_eq!(directive.trigger, tuliprox_hls::HlsManifestAcceptanceTrigger::RecoveryRequired);
    let mut io = super::provider_admission::test_hls_origin_io_context(&app);
    io.account_runtime = session.read().await.origin_account_runtime.clone();
    let snapshot = HlsOriginAccountSnapshot::from_session(&*session.read().await);
    let mut request = hls_origin_refresh_request(
        &app,
        &session,
        HlsPreparedOriginAccount { entry: LiveHlsOriginEntry::parse(&url).unwrap(), snapshot },
        HlsOriginRefreshOptions {
            headers: HeaderMap::new(),
            origin_io: io,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            acceptance_directive: directive,
            access_lease_id: Some(HlsAccessLeaseId(lease.to_string())),
            rewrite_secret: app.get_encrypt_secret().to_vec(),
            now_ms: current_time_millis(),
        },
    );
    request.now_ms = session.read().await.origin_refresh.next_fetch_allowed_at_ms.max(current_time_millis());
    control.recovering.store(true, Ordering::Release);
    let refresh = tokio::spawn(trigger_origin_refresh_sync(request));
    tokio::time::timeout(WAIT, control.retry_started.notified()).await.expect("reduced HTTP attempt 13 starts");
    let episode_before = {
        let s = session.read().await;
        assert_eq!(s.origin_to_proxy, baseline);
        assert_eq!(s.origin_manifest_commits(), commits, "first actual conflict commit did not mutate the timeline");
        let episode = s.origin_control.acceptance_episode.clone().unwrap();
        assert_eq!(episode.full_bursts_completed, 1);
        assert!(episode.exhaustion_reason().is_some());
        assert!(episode.deterministic_conflict_receipt().is_none());
        eprintln!(
            "{mode:?} before response 13: generation={} full_bursts={} reason={:?} timing={:?}",
            episode.generation.0,
            episode.full_bursts_completed,
            episode.exhaustion_reason(),
            episode.timing()
        );
        episode
    };
    let scheduler = app.hls.proxy.segment_worker_pool().pause_next_origin_prepare_for_test();
    control.release_retry.notify_one();
    tokio::time::timeout(WAIT, scheduler.entered.notified())
        .await
        .expect("repaired reduced retry commits and schedules media");
    let scheduled = {
        let s = session.read().await;
        let mut resources = s
            .segment_prefetch_queue
            .proxy_seqs()
            .iter()
            .map(|seq| {
                let entry = &s.segments[seq];
                (
                    *seq,
                    entry.origin_key.host_local_sequence,
                    entry.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.clone(),
                )
            })
            .collect::<Vec<_>>();
        resources.sort();
        assert_eq!(resources.iter().map(|(_, seq, _)| *seq).collect::<Vec<_>>(), vec![1006, 1007, 1008]);
        assert_eq!(s.origin_manifest_commits(), commits + 1);
        assert_eq!(s.origin_account_binding.as_ref().unwrap().generation, binding.generation);
        assert_eq!(s.origin_control.acceptance_generation, episode_before.generation);
        assert!(
            s.origin_control.acceptance_episode.is_none(),
            "successful refresh bookkeeping consumes the completed episode"
        );
        eprintln!("{mode:?} committed generation={} scheduled={resources:?}", episode_before.generation.0);
        resources
    };
    scheduler.prepare.notify_one();
    tokio::time::timeout(WAIT, scheduler.prepared.notified()).await.unwrap();
    scheduler.proceed.notify_one();
    let seqs = scheduled.iter().map(|(seq, _, _)| *seq).collect::<Vec<_>>();
    ready(&app, &session, &seqs).await;
    tokio::time::timeout(WAIT, refresh).await.unwrap().unwrap();
    assert_eq!(control.recovery_requests.load(Ordering::Acquire), 13);
    {
        let requests = origin.requests.lock().await;
        for (_, _, url) in &scheduled {
            let path = reqwest::Url::parse(url).unwrap().path().to_string();
            let count = requests.iter().filter(|request| request.starts_with(&format!("GET {path} "))).count();
            assert!(count > 0, "queued resource was fetched before any client demand");
            eprintln!("{mode:?} autonomous origin GET {path} requests={count} status=Ready");
        }
        assert!(requests.iter().all(|request| !request.contains("account-user")), "B remains unused");
    }
    let response = try_test_hls_cached_manifest_response(
        &app,
        &session,
        &HlsAccessLeaseId(lease.to_string()),
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .expect("repaired live manifest is published");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    deliver(&app, &session, &body, &[1006, 1007, 1008]).await;
    let s = session.read().await;
    assert_eq!(s.origin_account_binding.as_ref().unwrap().generation, binding.generation);
    assert_eq!(s.origin_account_binding.as_ref().unwrap().account_name, binding.account_name);
    assert_eq!(s.origin_account_binding.as_ref().unwrap().pinned_at_ms, binding.pinned_at_ms);
    assert_eq!(s.origin_account_binding.as_ref().unwrap().provider_binding_tag, binding.provider_binding_tag);
    assert_eq!(control.recovery_requests.load(Ordering::Acquire), 13);
    eprintln!(
        "{mode:?} complete: recovery_requests=13 account={} binding_generation={} origin_work=0 singleflight=false",
        binding.account_name, binding.generation
    );
    assert_eq!(s.proxy_session_id, proxy_id);
    assert!(!s.origin_refresh.in_flight);
    assert_eq!(s.active_segment_fetches, 0);
    assert_eq!(s.activity.active_origin_work_count.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn hls_reduced_retry_delivers_repaired_origin_media_optimistic() {
    repaired_reduced_retry(HlsStartupMode::Optimistic).await;
}

#[tokio::test]
async fn hls_reduced_retry_delivers_repaired_origin_media_conservative() {
    repaired_reduced_retry(HlsStartupMode::Conservative).await;
}
