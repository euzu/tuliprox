use super::{
    assert_stale_switch_generation_rejects_commit, bind_refresh_request_to_app_state, classify_manifest_fetch_failure,
    commit_fetched_manifest, fetch_and_commit_manifest_with_policy, fetched_manifest, host_from_base_url,
    install_published_recovery_binding, manifest_fetch_context, mark_origin_refresh_started, no_delay_policy,
    path_has_extension, prepare_cross_host_baseline, publish_ready_test_manifest, refresh_and_commit,
    request_header_value, resolved_hls_manifest_request_url_from_input, retry_test_manifest_recovery_chain,
    spawn_test_origin, switch_manifest_body, switch_test_request, test_app_config, test_origin_refresh_request,
    test_segment_repair_manager, test_session, three_segment_manifest_body, trigger_origin_refresh_sync,
    HlsManifestAcceptanceTrigger, HlsManifestCommitRequirement, HlsManifestFetchFailureKind,
    HlsManifestFetchFailureSignal, HlsManifestRecoveryUnavailableReason, HlsManifestRejectLogReason,
    LiveHlsOriginEntry, OriginManifestFetchError, OriginRefreshRequest, StaleSwitchGeneration,
};
use crate::{
    refresh::maybe_trigger_origin_refresh, HlsFreshManifestRequiredReason, HlsManifestAcceptanceDirective,
    HlsMapWorkerPool, HlsProxyManager, HlsSegmentCache, HlsSegmentWorkerPool, HlsSession, HlsSessionKey,
    SegmentCacheStatus,
};
use axum::http::{header, HeaderMap};
use sha2::Digest;
use shared::model::{ConfigProviderDto, HlsManifestRecoveryBurstLevel, HlsStripMode, ProviderUrlSelectionPolicy};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{io::AsyncReadExt, sync::RwLock};
use tuliprox_core::model::{Config, ConfigProvider, HlsManifestRecoveryBurstConfig, StripConfig};
use url::Url;

#[test]
fn resolved_hls_manifest_request_url_uses_provider_index_locally() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));
    let provider_entry =
        LiveHlsOriginEntry::parse_with_url_failover_provider("provider://demo/live/u/p/1.m3u8", Some(provider))
            .unwrap();

    let resolved =
        resolved_hls_manifest_request_url_from_input(&provider_entry.to_input_source(), Some(1), provider_entry.url());
    assert_eq!(resolved.as_str(), "http://provider-b.example/live/u/p/1.m3u8");
    assert!(!resolved.as_str().contains("provider://"));

    let direct_entry = LiveHlsOriginEntry::parse("http://origin.example/live/u/p/1.m3u8").unwrap();
    assert_eq!(
        resolved_hls_manifest_request_url_from_input(&direct_entry.to_input_source(), Some(1), direct_entry.url())
            .as_str(),
        "http://origin.example/live/u/p/1.m3u8"
    );
}

#[test]
fn fresh_revalidation_rebases_normal_manifest_on_the_pinned_host() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(1_000);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    let mut request = test_origin_refresh_request(test_session());
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    let fetched =
        fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg10.ts\n");

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(10));
    assert_eq!(session.origin_epoch, 1);
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
}

#[tokio::test]
async fn accepted_manifest_commit_stores_provider_session_cookie_separately() {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(|_path| {
        (
            200,
            vec![
                ("Set-Cookie", "sid=abc; Path=/; HttpOnly".to_string()),
                ("Set-Cookie", "pref=1; SameSite=Lax".to_string()),
            ],
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string(),
        )
    }))
    .await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;

    assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

    let session = session.read().await;
    assert!(!session.origin_request_headers.contains_key(header::COOKIE));
    assert_eq!(
        session.origin_provider_session_headers.get(header::COOKIE).expect("provider cookie"),
        "sid=abc; pref=1"
    );
}

#[tokio::test]
async fn provider_failover_status_does_not_count_as_hls_retry() {
    let first = spawn_test_origin(Arc::new(|_path| (503, Vec::new(), "rotate".to_string()))).await;
    let second = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let initial_session_key = session.read().await.key.stable_value();
    let initial_proxy_session_id = session.read().await.proxy_session_id.clone();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let segment_worker_pool = Arc::new(HlsSegmentWorkerPool::default());
    let request = OriginRefreshRequest {
        work_owner: None,
        attempt: None,
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::clone(&segment_worker_pool),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        refresh_identity: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.origin_seq_highwater == Some(102) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let first_manifest_requests =
        first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    let second_manifest_requests =
        second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(first_manifest_requests, 1);
    assert_eq!(second_manifest_requests, 1);
    for server in [&first, &second] {
        let manifest_requests = server
            .raw_requests
            .lock()
            .await
            .iter()
            .filter(|request| request.lines().next().is_some_and(|line| line.contains(" /live/user/pass/12345.m3u8 ")))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(manifest_requests.len(), 1);
        assert_eq!(request_header_value(&manifest_requests[0], "accept-encoding"), Some("identity"));
    }
    let session = session.read().await;
    assert_eq!(session.key.stable_value(), initial_session_key);
    assert_eq!(session.proxy_session_id, initial_proxy_session_id);
    assert!(!session.key.stable_value().contains("provider://"));
    assert!(!session.key.stable_value().contains(first.base_url.as_str()));
    assert!(!session.key.stable_value().contains(second.base_url.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(102));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
    let metrics = segment_worker_pool.metrics().snapshot();
    assert_eq!(metrics.refresh_started, 1);
    assert_eq!(metrics.refresh_completed, 1);
    assert_eq!(metrics.refresh_retried, 0);
    assert_eq!(metrics.refresh_failed, 0);
}

pub(in crate::refresh::tests) fn assert_binding_superseded_error(error: &OriginManifestFetchError) {
    assert!(matches!(
        error,
        OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        }
    ));
    assert_eq!(error.log_label(), "recovery_binding_superseded");
    assert_eq!(error.to_string(), "origin manifest recovery unavailable: manifest origin binding superseded");
    assert_eq!(
        classify_manifest_fetch_failure(error),
        HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::Superseded)
    );
}

#[tokio::test]
async fn recovery_binding_supersession_is_discarded_without_failure_signal() {
    let origin = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let session = Arc::new(RwLock::with_max_readers(
        HlsSession::new(HlsSessionKey::new(1, "binding-supersession"), b"secret", 0),
        1,
    ));
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    let baseline_binding_url = format!("{}/live/a/index.m3u8", origin.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&baseline_binding_url).expect("binding A entry URL");
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;

    fetch_and_commit_manifest_with_policy(&mut request).await.expect("binding A baseline commits");
    publish_ready_test_manifest(&session, 200).await;
    let baseline_binding =
        session.read().await.established_manifest_recovery_binding().expect("published baseline has binding A");
    let requests_before_supersession = origin.requests.lock().await.len();

    let replacement_binding_url = format!("{}/live/b/index.m3u8", origin.base_url);
    let mut binding_b_manifest = fetched_manifest(&three_segment_manifest_body(103));
    binding_b_manifest.resolved_request_url.clone_from(&replacement_binding_url);
    binding_b_manifest.final_manifest_url.clone_from(&replacement_binding_url);
    binding_b_manifest.redirect_host = Some(host_from_base_url(&origin.base_url));
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    assert!(mark_origin_refresh_started(&mut request, 300).await);
    let verification_request = request.clone();
    let replacement_request = request.clone();
    let read_blocker = session.read().await;
    let refresh = tokio::spawn(refresh_and_commit(request, 300));
    tokio::task::yield_now().await;
    let replacement_session = Arc::clone(&session);
    let replacement = tokio::spawn(async move {
        let mut session = replacement_session.write().await;
        commit_fetched_manifest(&mut session, &binding_b_manifest, &replacement_request, 325)
            .expect("newer binding B manifest commits");
        session.origin_control.record_origin_response(275);
        session.origin_control.path_condition =
            super::super::super::origin_progress::HlsOriginPathCondition::PublicationLate;
        session.origin_refresh.consecutive_failures = 3;
        session.origin_refresh.last_error_at_ms = Some(250);
    });
    tokio::task::yield_now().await;
    drop(read_blocker);
    replacement.await.expect("replacement binding task joins");
    refresh.await.expect("superseded refresh task joins");

    let metrics_after = metrics.snapshot();
    let replacement_binding = {
        let session = session.read().await;
        assert_eq!(session.origin_control.last_origin_response_at_ms, Some(275));
        assert_eq!(
            session.origin_control.path_condition,
            super::super::super::origin_progress::HlsOriginPathCondition::PublicationLate
        );
        assert_eq!(session.origin_refresh.consecutive_failures, 3);
        assert_eq!(session.origin_refresh.last_error_at_ms, Some(250));
        assert!(!session.origin_refresh.in_flight);
        assert!(session.origin_control.acceptance_episode.is_none());
        session.origin_control.manifest_origin_binding.clone().expect("newer binding B remains installed")
    };
    assert_ne!(baseline_binding, replacement_binding);
    assert_eq!(metrics_after.refresh_started, 1);
    assert_eq!(metrics_after.refresh_completed, 0);
    assert_eq!(metrics_after.refresh_retried, 0);
    assert_eq!(metrics_after.refresh_failed, 0);
    assert_eq!(metrics_after.refresh_skipped, 1);
    assert_eq!(ctx.hls_proxy.availability_reevaluations().owner_count(), 0);
    assert_eq!(origin.requests.lock().await.len(), requests_before_supersession);

    let Err(error) = super::super::recover_manifest_for_request(
        &manifest_fetch_context(&verification_request, None),
        &verification_request,
        super::super::HlsManifestRecoveryPath {
            binding: baseline_binding,
            reject_reason: None,
            deterministic_conflict: None,
            trigger: HlsManifestAcceptanceTrigger::RecoveryRequired,
            diagnostic: super::super::HlsRecoveryTriggerDiagnostic::new(super::super::HlsRecoveryTriggerSource::Other),
        },
    )
    .await
    else {
        panic!("superseded recovery binding must be discarded");
    };
    assert_binding_superseded_error(&error);
    assert_eq!(origin.requests.lock().await.len(), requests_before_supersession);
}

#[tokio::test]
async fn provider_failover_initial_success_commits_without_hls_host_retry_when_unpinned() {
    let first = spawn_test_origin(Arc::new(|_path| (503, Vec::new(), "rotate".to_string()))).await;
    let second_hits = Arc::new(AtomicUsize::new(0));
    let second_hits_for_handler = Arc::clone(&second_hits);
    let second = spawn_test_origin(Arc::new(move |_path| {
        let hit = second_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (200, Vec::new(), three_segment_manifest_body(100));
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n"
                .to_string(),
        )
    }))
    .await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    {
        session.write().await.origin_seq_highwater = Some(100);
    }
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let request = OriginRefreshRequest {
        work_owner: None,
        attempt: None,
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        refresh_identity: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.origin_seq_highwater == Some(102) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let first_manifest_requests =
        first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    let second_manifest_requests =
        second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(first_manifest_requests, 1);
    assert_eq!(second_manifest_requests, 1);
    assert_eq!(session.read().await.origin_seq_highwater, Some(102));
}

#[tokio::test]
async fn fresh_pinned_revalidation_rejects_reset_without_ready_media_evidence() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let origin = spawn_test_origin(Arc::new(|path| {
        if path_has_extension(&path, "m3u8") {
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n10.ts\n#EXTINF:4.0,\n11.ts\n"
                    .to_string(),
            )
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let effective_host = host_from_base_url(&origin.base_url);
    let baseline_body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1000\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\n1000.ts\n#EXTINF:4.0,\n1001.ts\n";
    let baseline_manifest_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(baseline_body, &baseline_manifest_url)
    else {
        panic!("same-host stale baseline parses");
    };
    let session = test_session();
    let baseline_epoch = {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(&baseline, crate::timeline::effective_origin_host_id(&effective_host))
            .expect("same-host stale baseline commits");
        session.last_effective_manifest_host = Some(effective_host.clone());
        session.origin_control.pinned_host = Some(effective_host.clone());
        session.origin_control.origin_epoch = session.origin_epoch;
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
        session.origin_epoch
    };
    install_published_recovery_binding(&session, &baseline_manifest_url, None).await;
    let cache_dir = tempfile::tempdir().expect("fresh pinned revalidation cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    };
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        origin.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, plan.total_candidates() + no_delay_policy().attempt_count() - 1);
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch);
    assert_eq!(session.origin_seq_highwater, Some(1001));
    assert_eq!(session.proxy_next_seq, Some(2));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
    assert_eq!(
        session.fresh_manifest_commit_required,
        Some(HlsFreshManifestRequiredReason::PreviousHardManifestFailure)
    );
    assert!(session.segments.values().all(|entry| entry.origin_key.origin_epoch == baseline_epoch));
}

/// Marks the baseline segment ready with `bytes` and records it as the content anchor.
pub(in crate::refresh::tests) async fn prepare_ready_content_anchor(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    bytes: &[u8],
) {
    let key = {
        let mut session = session.write().await;
        let entry = {
            let segment = session.segments.values_mut().next().expect("baseline segment");
            segment.status = SegmentCacheStatus::Ready {
                content_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                ready_at_ms: 1,
            };
            segment.clone()
        };
        let generation = session.activity.origin_work_generation;
        session.origin_content.record(&entry, generation, sha2::Sha256::digest(bytes).into());
        entry.cache_key
    };
    cache.write_bytes_and_commit(&key, bytes).await.expect("committed anchor bytes");
}

pub(in crate::refresh::tests) async fn assert_cross_host_replay_does_not_commit(candidate_bytes: &'static str) {
    const COMMITTED_BYTES: &[u8] = b"committed-media-bytes";
    let manifest = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nold500.ts\n".to_string();
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else if path.ends_with("/old500.ts") {
            (200, Vec::new(), candidate_bytes.to_string())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let cache_dir = tempfile::tempdir().expect("content anchor cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    prepare_ready_content_anchor(&session, &cache, COMMITTED_BYTES).await;
    let baseline_epoch = session.read().await.origin_epoch;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };
    let target_url =
        Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("content anchor target");

    let result = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await;

    let requests = origin.requests.lock().await.clone();
    let session = session.read().await;
    assert!(result.is_err(), "replay-only cross-host candidate committed: requests={requests:?}");
    assert_eq!(session.origin_epoch, baseline_epoch);
}

#[tokio::test]
async fn same_cross_host_sequence_path_and_equal_bytes_without_forward_progress_never_commits() {
    assert_cross_host_replay_does_not_commit("committed-media-bytes").await;
}

#[tokio::test]
async fn same_cross_host_sequence_and_path_with_different_bytes_never_commits_as_anchor() {
    assert_cross_host_replay_does_not_commit("different-media-bytes").await;
}

#[tokio::test]
async fn pinned_origin_recovery_during_staging_rejects_alternative_commit() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::PinnedHostRecovered).await;
}

#[tokio::test]
async fn recovery_required_orchestrator_accepts_pinned_progress_without_another_full_plan() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let entry_hits = Arc::new(AtomicUsize::new(0));
    let entry_hits_for_handler = Arc::clone(&entry_hits);
    let pinned_hits = Arc::new(AtomicUsize::new(0));
    let pinned_hits_for_handler = Arc::clone(&pinned_hits);
    let origin_port = Arc::new(AtomicUsize::new(0));
    let origin_port_for_handler = Arc::clone(&origin_port);
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            let hit = entry_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < plan.total_candidates() {
                return (200, Vec::new(), switch_manifest_body(false));
            }
            return (
                302,
                vec![(
                    "Location",
                    format!("http://127.0.0.1:{}/pinned/index.m3u8", origin_port_for_handler.load(Ordering::SeqCst)),
                )],
                String::new(),
            );
        }
        if path == "/pinned/index.m3u8" {
            pinned_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\npinned101.ts\n".to_string(),
            );
        }
        (404, Vec::new(), String::new())
    }))
    .await;
    let parsed_origin_url = Url::parse(&origin.base_url).expect("origin base URL");
    origin_port.store(usize::from(parsed_origin_url.port().expect("test origin port")), Ordering::SeqCst);

    let session = test_session();
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\npinned100.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            baseline,
            "http://127.0.0.1/pinned/index.m3u8",
        )
    else {
        panic!("pinned baseline parses");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(&baseline, crate::timeline::effective_origin_host_id("127.0.0.1"))
            .expect("pinned baseline commits");
        session.last_effective_manifest_host = Some("127.0.0.1".to_string());
        session.origin_control.pinned_host = Some("127.0.0.1".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }

    let temp_dir = tempfile::tempdir().expect("orchestrator cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    let alternative_entry =
        format!("{}/live/user/pass/12345.m3u8", origin.base_url).replacen("127.0.0.1", "localhost", 1);
    request.origin_entry = LiveHlsOriginEntry::parse(&alternative_entry).expect("alternative entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = Url::parse(&alternative_entry).expect("alternative target URL");

    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("pinned follow-up commits");

    assert_eq!(committed.fetched.redirect_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(entry_hits.load(Ordering::SeqCst), plan.total_candidates().saturating_add(1));
    assert_eq!(pinned_hits.load(Ordering::SeqCst), 1);
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(101));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
    let episode = session.origin_control.acceptance_episode.as_ref().expect("completed acceptance episode");
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.state, super::super::super::manifest_acceptance::HlsManifestAcceptanceState::Completed);
}

/// With mirror recovery on, a pinned 503 moves the fetch to the next profile URL.
#[tokio::test]
async fn pinned_recovery_moves_to_alternative_provider_url_on_qualifying_failure() {
    let first = spawn_test_origin(Arc::new(|_path| (503, Vec::new(), "unavailable".to_string()))).await;
    let second = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;

    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "cdn".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));

    let session = test_session();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://cdn/live/user/pass/12345.m3u8?token=fixed",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url")
    .with_profile_recovery_failover(true);

    let binding = crate::manifest_origin_binding::HlsManifestOriginBinding::new(
        Url::parse(&format!("{}/live/user/pass/12345.m3u8?token=fixed", first.base_url)).unwrap(),
        Some(0),
    )
    .unwrap();

    let attempt = Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        crate::HlsManifestCommitRequirement::CommittedManifestAllowed,
        true,
        None,
    ));
    assert!(attempt.begin_logical_attempt());
    assert_eq!(attempt.logical_attempts(), 1);

    let mut context = manifest_fetch_context(&test_origin_refresh_request(Arc::clone(&session)), None);
    context.origin_entry = entry;
    context.attempt = Some(Arc::clone(&attempt));

    // 1. PL returns 503: with profile_recovery_failover enabled, it must move to EU, and logical attempts stay 1.
    let recovered = crate::manifest_fetch::fetch_hls_origin_manifest_recovery_once(&context, &binding)
        .await
        .expect("pinned session with PL 503 must recover on EU");
    assert_eq!(recovered.provider_url_index, Some(1));
    assert_eq!(attempt.logical_attempts(), 1);
    assert_eq!(recovered.body, three_segment_manifest_body(100));

    // 2. Disabled flag: does not move to EU, fails with 503.
    context.origin_entry = context.origin_entry.clone().with_profile_recovery_failover(false);
    let disabled_result = crate::manifest_fetch::fetch_hls_origin_manifest_recovery_once(&context, &binding).await;
    assert!(disabled_result.is_err());
    assert!(matches!(
        disabled_result.err().unwrap(),
        OriginManifestFetchError::RetryableStatus(axum::http::StatusCode::SERVICE_UNAVAILABLE, _)
    ));

    // 3. 403 Forbidden on PL: stays closed, does NOT move to EU.
    let forbidden_origin = spawn_test_origin(Arc::new(|_path| (403, Vec::new(), "forbidden".to_string()))).await;
    let forbidden_binding = crate::manifest_origin_binding::HlsManifestOriginBinding::new(
        Url::parse(&format!("{}/live/user/pass/12345.m3u8?token=fixed", forbidden_origin.base_url)).unwrap(),
        Some(0),
    )
    .unwrap();
    context.origin_entry = context.origin_entry.clone().with_profile_recovery_failover(true);
    let forbidden_result =
        crate::manifest_fetch::fetch_hls_origin_manifest_recovery_once(&context, &forbidden_binding).await;
    assert!(forbidden_result.is_err());
    assert!(matches!(
        forbidden_result.err().unwrap(),
        OriginManifestFetchError::PermanentStatus(axum::http::StatusCode::FORBIDDEN)
    ));
}

/// Runs the forward content anchor scenario with optional optimistic fast start.
#[allow(clippy::too_many_lines)]
async fn assert_forward_content_anchor_after_published_prefix(
    anchor_bytes: &'static str,
    should_commit: bool,
    fast_start: bool,
) {
    let mode = fast_start.then_some(shared::model::HlsStartupMode::Optimistic);
    assert_forward_content_anchor_with_mode(anchor_bytes, should_commit, mode).await;
}

/// Asserts that a refresh trimming the published prefix commits only when the
/// overlap segment matches `anchor_bytes`, for the given startup mode.
#[allow(clippy::too_many_lines)]
async fn assert_forward_content_anchor_with_mode(
    anchor_bytes: &'static str,
    should_commit: bool,
    startup_mode: Option<shared::model::HlsStartupMode>,
) {
    assert_forward_content_anchor_with_revision_pin(anchor_bytes, should_commit, startup_mode, false).await;
}

/// Like [`assert_forward_content_anchor_with_mode`]. When `keep_revision_pin`
/// is set, the completed startup revisions keep their active-file pins, as
/// production revisions do while a guard serves them.
#[allow(clippy::too_many_lines)]
async fn assert_forward_content_anchor_with_revision_pin(
    anchor_bytes: &'static str,
    should_commit: bool,
    startup_mode: Option<shared::model::HlsStartupMode>,
    keep_revision_pin: bool,
) {
    const COMMITTED: &[u8] = b"committed-anchor-bytes";
    const NOVEL: &str = "different-new-segment-bytes";
    super::short_manifest::init_qualification_logger();
    let manifest = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4,\nold500.ts\n#EXTINF:4,\nold501.ts\n#EXTINF:4,\nold502.ts\n#EXTINF:4,\nnew503.ts\n";
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.to_string())
        } else if path.ends_with("/old502.ts") {
            (200, Vec::new(), anchor_bytes.to_string())
        } else {
            (200, Vec::new(), NOVEL.to_string())
        }
    }))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let cache = Arc::new(HlsSegmentCache::with_cache_path(directory.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let baseline = manifest.replace("#EXTINF:4,\nnew503.ts\n", "");
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            &baseline,
            "http://previous.example.com/live/user/pass/12345.m3u8",
        )
    else {
        panic!("baseline parses");
    };
    session
        .write()
        .await
        .apply_origin_manifest_for_host(&baseline, crate::timeline::effective_origin_host_id("previous.example.com"))
        .unwrap();
    publish_ready_test_manifest(&session, 1).await;
    let (anchor_key, anchor_proxy_seq) = {
        let mut session = session.write().await;
        let entry = {
            let segment = session.segments.values_mut().next_back().expect("baseline segment");
            segment.status = SegmentCacheStatus::Ready {
                content_length: u64::try_from(COMMITTED.len()).unwrap_or(u64::MAX),
                ready_at_ms: 1,
            };
            segment.clone()
        };
        let generation = session.activity.origin_work_generation;
        session.origin_content.record(&entry, generation, sha2::Sha256::digest(COMMITTED).into());
        (entry.cache_key, entry.proxy_seq)
    };
    cache.write_bytes_and_commit(&anchor_key, COMMITTED).await.expect("committed anchor bytes");
    if let Some(mode) = startup_mode {
        let config = tuliprox_core::model::HlsStartupConfig { mode, ..Default::default() };
        let store = Arc::new(crate::SegmentRevisionStore::default());
        let mut revisions = std::collections::BTreeMap::new();
        let keys = session
            .read()
            .await
            .segments
            .values()
            .map(|entry| (entry.proxy_seq, entry.cache_key.clone()))
            .collect::<Vec<_>>();
        for (seq, key) in keys {
            cache
                .write_bytes_and_commit(&key, if seq == anchor_proxy_seq { COMMITTED } else { NOVEL.as_bytes() })
                .await
                .unwrap();
            let revision =
                store.create(key.proxy_session_id().clone(), seq, crate::SegmentRevisionKind::Processed).unwrap();
            *revision.revision().file_pin.lock().unwrap() =
                Some(cache.pin_revision_file(&cache.object_path(&revision.revision().key)).unwrap());
            let metadata = cache
                .publish_processed_revision(
                    &revision.revision().key,
                    &cache.object_path(&key),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                )
                .await
                .unwrap();
            revision.revision().complete(metadata);
            if !keep_revision_pin {
                revision.revision().file_pin.lock().unwrap().take();
            }
            revision.revision().bind_origin(&*session.read().await);
            revisions.insert(seq, revision);
        }
        let worker = Arc::new(crate::HlsSegmentWorkerPool::default());
        session.write().await.startup = Some(crate::HlsSessionStartup {
            config: config.clone(),
            worker: Arc::downgrade(&worker),
            access_leases: Arc::new(RwLock::new(crate::HlsAccessLeaseStore::default())),
            revisions,
            policy_fixed: true,
            store,
            budget: crate::ProgressiveBudgetManager::new(config),
        });
    }
    let baseline_revision_bytes = cache.scan_revision_disk_bytes().await.unwrap();
    let baseline_epoch = session.read().await.origin_epoch;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };
    let result = retry_test_manifest_recovery_chain(
        &request,
        Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).unwrap(),
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await;
    assert_eq!(result.is_ok(), should_commit, "recovery error: {:?}", result.as_ref().err());
    let state = session.read().await;
    if should_commit {
        assert!(state.origin_epoch > baseline_epoch);
        assert_eq!(state.origin_seq_highwater, Some(503));
        let key = state.segments.values().next_back().unwrap().cache_key.clone();
        if let Some(mode) = startup_mode {
            let revision = state
                .startup
                .as_ref()
                .unwrap()
                .revisions
                .get(&key.proxy_seq())
                .expect("ready handoff segment has an immutable startup revision");
            assert!(revision.revision().is_complete());
            assert!(
                revision.revision().origin_binding_is_current(&state),
                "handoff revision must be bound after commit"
            );
            let expected_kind = if mode.streams_raw_revisions() {
                crate::SegmentRevisionKind::Raw
            } else {
                crate::SegmentRevisionKind::Processed
            };
            assert_eq!(
                revision.revision().key.kind,
                expected_kind,
                "startup revision kind must follow mode.streams_raw_revisions()"
            );
        }
        drop(state);
        let mut file = cache.open_range(&key, 0).await.unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).await.unwrap();
        assert_eq!(body, NOVEL, "anchor bytes must never replace the first new segment");
    } else {
        assert_eq!(state.origin_epoch, baseline_epoch);
        drop(state);
        assert_eq!(
            cache.scan_revision_disk_bytes().await.unwrap(),
            baseline_revision_bytes,
            "rejected handoff must remove its unpublished revision files"
        );
    }
}

/// Unchanged overlap bytes after the trimmed prefix verify the anchor.
#[tokio::test]
async fn forward_content_anchor_verifies_overlap_after_trimming_published_prefix() {
    assert_forward_content_anchor_after_published_prefix("committed-anchor-bytes", true, false).await;
}

/// Changed overlap bytes after the trimmed prefix reject the anchor.
#[tokio::test]
async fn forward_content_anchor_rejects_changed_overlap_after_trimming_published_prefix() {
    assert_forward_content_anchor_after_published_prefix("changed-anchor-bytes", false, false).await;
}

/// An accepted optimistic handoff registers its revision as ready.
#[tokio::test]
async fn forward_content_anchor_registers_ready_optimistic_handoff_revision() {
    assert_forward_content_anchor_after_published_prefix("committed-anchor-bytes", true, true).await;
}

/// An accepted conservative handoff registers its revision as ready.
#[tokio::test]
async fn forward_content_anchor_registers_ready_conservative_handoff_revision() {
    assert_forward_content_anchor_with_mode(
        "committed-anchor-bytes",
        true,
        Some(shared::model::HlsStartupMode::Conservative),
    )
    .await;
}

/// A rejected optimistic handoff removes its unpublished revision.
#[tokio::test]
async fn forward_content_anchor_cleans_rejected_optimistic_handoff_revision() {
    assert_forward_content_anchor_after_published_prefix("changed-anchor-bytes", false, true).await;
}

/// A completed optimistic startup revision keeps its own active-file pin while
/// a guard serves it. The content anchor probe must not pin that path again.
#[tokio::test]
async fn forward_content_anchor_accepts_already_pinned_optimistic_reference() {
    assert_forward_content_anchor_with_revision_pin(
        "committed-anchor-bytes",
        true,
        Some(shared::model::HlsStartupMode::Optimistic),
        true,
    )
    .await;
}

/// A completed conservative startup revision keeps its own active-file pin
/// while a guard serves it. The content anchor probe must not pin it again.
#[tokio::test]
async fn forward_content_anchor_accepts_already_pinned_conservative_reference() {
    assert_forward_content_anchor_with_revision_pin(
        "committed-anchor-bytes",
        true,
        Some(shared::model::HlsStartupMode::Conservative),
        true,
    )
    .await;
}

/// A pinned reference with different origin bytes is still rejected.
#[tokio::test]
async fn forward_content_anchor_rejects_already_pinned_changed_reference() {
    assert_forward_content_anchor_with_revision_pin(
        "changed-anchor-bytes",
        false,
        Some(shared::model::HlsStartupMode::Optimistic),
        true,
    )
    .await;
}
