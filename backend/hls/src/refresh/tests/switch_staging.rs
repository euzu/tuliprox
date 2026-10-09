use super::{
    assert_incompatible_switch_is_rejected_before_timeline_commit, assert_stale_switch_generation_rejects_commit,
    await_controlled_switch_segment_prefix, bind_refresh_request_to_app_state, candidate_handoff_preview,
    commit_fetched_manifest, fetch_and_commit_manifest_with_policy, fetched_manifest, host_from_base_url,
    install_published_recovery_binding, manifest_body, manifest_fetch_context, manifest_recovery_trigger,
    mark_full_burst_ready_for_switch_staging, no_delay_policy, path_has_extension, prepare_cross_host_baseline,
    publish_ready_test_manifest, retry_test_manifest_recovery_chain, score_manifest_recovery_candidate,
    spawn_controlled_switch_origin, spawn_test_origin, switch_fetched_manifest, switch_manifest_body,
    switch_test_request, test_acceptance_episode_timing, test_app_config, test_origin_refresh_request,
    test_segment_repair_manager, test_session, three_segment_manifest_body, trigger_origin_refresh_sync,
    HlsManifestAcceptanceTrigger, HlsManifestCommitAcceptanceMode, HlsManifestCommitError,
    HlsManifestCommitRequirement, HlsManifestFetchSelection, HlsManifestOriginBinding, HlsManifestOriginQualityScore,
    HlsManifestRefreshCompletionDiagnostic, HlsManifestRejectLogReason, HlsRecoveryEncryptionReadiness,
    HlsRecoveryMapWorkload, HlsRecoveryMediumReadiness, HlsRecoveryObjectReadiness, HlsRecoverySegmentWorkload,
    HlsRecoveryWorkload, LiveHlsOriginEntry, ManifestRecoverySelectionLogPhase, OriginManifestFetchError,
    OriginRefreshRequest, StaleSwitchGeneration, SWITCH_MAP_BODY, SWITCH_SEGMENT_BODY,
};
use crate::{
    HlsBoundAccountAcquireErrorKind, HlsFreshManifestRequiredReason, HlsManifestAcceptanceDirective, HlsMapWorkerPool,
    HlsOriginAccountBinding, HlsOriginIoContext, HlsProxyManager, HlsSegmentCache, HlsSegmentWorkerPool, HlsSession,
    HlsSessionKey, HlsSessionMode, MapCacheStatus, SegmentCacheStatus, TransientObjectFetchDecision,
    TransientPassthroughReason,
};
use axum::http::HeaderMap;
use shared::model::{ConfigProviderDto, HlsManifestRecoveryBurstLevel, HlsStripMode, ProviderUrlSelectionPolicy};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::AsyncReadExt;
use tuliprox_core::{
    model::{Config, ConfigProvider, HlsManifestRecoveryBurstConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_session::ConnectionKind;
use url::Url;

#[test]
fn manifest_recovery_burst_levels_map_to_candidate_counts() {
    let cases = [
        (HlsManifestRecoveryBurstLevel::Off, 1, 1),
        (HlsManifestRecoveryBurstLevel::Friendly, 2, 1),
        (HlsManifestRecoveryBurstLevel::Cautious, 3, 1),
        (HlsManifestRecoveryBurstLevel::Balanced, 4, 1),
        (HlsManifestRecoveryBurstLevel::Intense, 5, 1),
        (HlsManifestRecoveryBurstLevel::Aggressive, 6, 1),
        (HlsManifestRecoveryBurstLevel::Beast, 6, 2),
    ];
    for (level, expected_slots, expected_lanes) in cases {
        let plan = level.plan();
        let expected_candidates = expected_slots * expected_lanes;
        assert_eq!(plan.slots, expected_slots);
        assert_eq!(plan.lanes_per_slot, expected_lanes);
        assert_eq!(plan.total_candidates(), expected_candidates);
        assert_eq!(level.total_candidates(), expected_candidates);
    }
}

#[test]
fn recovery_selection_log_phase_distinguishes_single_candidate_from_burst() {
    assert_eq!(ManifestRecoverySelectionLogPhase::from_candidate_count(1), ManifestRecoverySelectionLogPhase::Recovery);
    assert_eq!(ManifestRecoverySelectionLogPhase::from_candidate_count(2), ManifestRecoverySelectionLogPhase::Burst);
    assert_eq!(ManifestRecoverySelectionLogPhase::Recovery.as_log_label(), "recovery");
    assert_eq!(ManifestRecoverySelectionLogPhase::Burst.as_log_label(), "burst");
}

#[test]
fn different_host_candidate_is_not_committed_immediately() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    let binding = HlsManifestOriginBinding::new(
        Url::parse("https://previous.example.com/live/index.m3u8?token=baseline").expect("baseline URL"),
        Some(0),
    )
    .expect("baseline binding");
    session.origin_control.manifest_origin_binding = Some(binding.clone());
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
    assert!(session.transient.last_manifest_body.is_none());
    assert_eq!(session.origin_control.manifest_origin_binding.as_ref(), Some(&binding));
}

#[test]
fn rejected_transient_candidate_preserves_committed_state_and_pending_handoff() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let mut request = test_origin_refresh_request(test_session());
    request.transient_resource_ttl_ms = 1;
    let baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n",
    );
    assert!(commit_fetched_manifest(&mut session, &baseline, &request, 100).is_ok());
    session.mark_pending_handoff_discontinuity(7);
    let baseline_body = session.transient.last_manifest_body.clone().expect("baseline manifest");
    let baseline_generation = session.transient.manifest_generation();
    let baseline_resources = session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>();
    let baseline_highwater = session.origin_seq_highwater;
    let key_format = "x".repeat(crate::manifest_limits::MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES);
    let candidate = fetched_manifest(&format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\",KEYFORMAT=\"{key_format}\"\n\
         #EXTINF:4,\nsegment.ts\n"
    ));

    let result = commit_fetched_manifest(&mut session, &candidate, &request, 102);

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation))
            if violation.kind
                == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotEncryptionDirectiveBytes
    ));
    assert!(Arc::ptr_eq(
        session.transient.last_manifest_body.as_ref().expect("baseline remains current"),
        &baseline_body
    ));
    assert_eq!(session.transient.manifest_generation(), baseline_generation);
    assert_eq!(
        session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>(),
        baseline_resources
    );
    assert_eq!(session.pending_handoff_discontinuity_sequence, Some(7));
    assert_eq!(session.origin_seq_highwater, baseline_highwater);
}

#[test]
fn refresh_completion_diagnostics_separate_logical_recovery_from_beast_candidate_requests() {
    let initial = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    let initial_diagnostic = HlsManifestRefreshCompletionDiagnostic::from_fetched(&initial);
    assert_eq!(initial_diagnostic.recovery_attempts, 0);
    assert_eq!(initial_diagnostic.candidate_requests, 1);
    assert_eq!(initial_diagnostic.selection, HlsManifestFetchSelection::Initial);

    let mut burst = initial;
    burst.attempts = 1;
    burst.candidate_requests = HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates();
    burst.selection = HlsManifestFetchSelection::Burst;
    let burst_diagnostic = HlsManifestRefreshCompletionDiagnostic::from_fetched(&burst);
    assert_eq!(burst_diagnostic.recovery_attempts, 1);
    assert_eq!(burst_diagnostic.candidate_requests, HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates());
    assert_eq!(burst_diagnostic.selection, HlsManifestFetchSelection::Burst);
}

#[tokio::test]
async fn established_recovery_burst_uses_one_fixed_provider_url_for_every_candidate() {
    let phase = Arc::new(AtomicUsize::new(0));
    let phase_for_first = Arc::clone(&phase);
    let first = spawn_test_origin(Arc::new(move |_path| {
        let media_sequence = if phase_for_first.load(Ordering::SeqCst) == 0 { 100 } else { 103 };
        (200, Vec::new(), three_segment_manifest_body(media_sequence))
    }))
    .await;
    let second = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "unexpected".to_string()))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8?token=fixed",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };

    let baseline = fetch_and_commit_manifest_with_policy(&mut request).await.expect("initial baseline commits");
    assert_eq!(baseline.fetched.provider_url_index, Some(0));
    let concrete_request_url = format!("{}/live/user/pass/12345.m3u8?token=fixed", first.base_url);
    {
        let session = session.read().await;
        let binding =
            session.origin_control.manifest_origin_binding.as_ref().expect("successful commit stores binding");
        assert_eq!(binding.request_url().as_str(), concrete_request_url);
        assert_eq!(binding.provider_url_index(), Some(0));
    }
    publish_ready_test_manifest(&session, 200).await;
    assert!(session.read().await.established_manifest_recovery_binding().is_some());

    phase.store(1, Ordering::SeqCst);
    let provider_index_before_burst = provider.get_current_index();
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let selected =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("established fixed-binding burst commits");
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let first_requests = first.requests.lock().await;
    assert_eq!(first_requests.len(), 1_usize.saturating_add(plan.total_candidates()));
    assert!(first_requests.iter().all(|path| path == "/live/user/pass/12345.m3u8?token=fixed"));
    assert!(second.requests.lock().await.is_empty());
    assert_eq!(provider.get_current_index(), provider_index_before_burst);
    assert_eq!(selected.fetched.provider_url_index, Some(0));
    assert_eq!(selected.fetched.candidate_requests, plan.total_candidates());
    assert_eq!(selected.fetched.selection, HlsManifestFetchSelection::Burst);
    assert_eq!(session.read().await.origin_seq_highwater, Some(105));
}

#[tokio::test]
async fn provisioning_handoff_without_established_origin_baseline_uses_one_initial_fetch() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let server = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ProvisioningHandoff,
    };
    assert_eq!(manifest_recovery_trigger(&request), HlsManifestAcceptanceTrigger::Critical);

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, 1);
    assert_ne!(manifest_requests, plan.total_candidates());
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert!(session.origin_control.manifest_origin_binding.is_some());
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn different_host_single_candidate_retries_do_not_prove_acceptance() {
    let candidate_hits = Arc::new(AtomicUsize::new(0));
    let candidate_hits_for_handler = Arc::clone(&candidate_hits);
    let candidate = spawn_test_origin(Arc::new(move |_path| {
        candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n"
                .to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
    }
    let candidate_entry_url =
        format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
    install_published_recovery_binding(&session, &candidate_entry_url, None).await;
    let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
    let request = OriginRefreshRequest {
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
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(trigger_origin_refresh_sync(request).await);

    let candidate_requests =
        candidate.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(candidate_requests, 6);
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(100));
    assert_eq!(session.origin_epoch, 0);
    let episode = session.origin_control.acceptance_episode.as_ref().expect("acceptance episode remains held");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.trigger(), HlsManifestAcceptanceTrigger::Observe);
    assert_eq!(episode.state, super::super::super::manifest_acceptance::HlsManifestAcceptanceState::Holding);
    assert_eq!(
        session.origin_control.path_condition,
        super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    );
}

#[tokio::test]
async fn manifest_recovery_burst_skips_rejected_candidate() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:50\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n50.bin\n".to_string(),
            );
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        session.mark_authorized_media_access(super::super::current_time_millis());
    }
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("burst should commit accepted candidate");

    assert_eq!(committed.fetched.attempts, 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(session.read().await.origin_seq_highwater, Some(101));
}

#[test]
fn manifest_recovery_candidate_score_prefers_same_host_next_sequence() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(100);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(super::super::current_time_millis());
    let request = test_origin_refresh_request(test_session());
    let fetch_context = manifest_fetch_context(&request);
    let same_host_unchanged =
        fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n");
    let same_host_next =
        fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n");
    let mut other_host_next = same_host_next.clone();
    other_host_next.redirect_host = Some("other.example.com".to_string());

    assert_eq!(
        score_manifest_recovery_candidate(&session, &same_host_unchanged, &fetch_context).expect("score").quality.score,
        HlsManifestOriginQualityScore::SameHostUnchanged
    );
    assert_eq!(
        score_manifest_recovery_candidate(&session, &same_host_next, &fetch_context).expect("score").quality.score,
        HlsManifestOriginQualityScore::SameHostNextSequence
    );
    let other_host_score =
        score_manifest_recovery_candidate(&session, &other_host_next, &fetch_context).expect("score").quality;
    assert_eq!(other_host_score.score, HlsManifestOriginQualityScore::OtherHostCandidate);
    assert!(other_host_score.requires_handoff_discontinuity);
}

#[tokio::test]
async fn manifest_recovery_burst_commits_best_same_host_candidate() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n".to_string(),
            );
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        session.mark_authorized_media_access(super::super::current_time_millis());
    }
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("burst should commit best same-host candidate");

    assert_eq!(committed.fetched.attempts, 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(session.read().await.origin_seq_highwater, Some(101));
}

#[tokio::test]
async fn host_switch_conflict_runs_one_full_plan_then_only_bounded_follow_ups() {
    let candidate_hits = Arc::new(AtomicUsize::new(0));
    let candidate_hits_for_handler = Arc::clone(&candidate_hits);
    let candidate = spawn_test_origin(Arc::new(move |_path| {
        candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n900.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
    }
    let candidate_entry_url =
        format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
    install_published_recovery_binding(&session, &candidate_entry_url, None).await;
    let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
    let request = OriginRefreshRequest {
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
        strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(trigger_origin_refresh_sync(request).await);

    assert_eq!(candidate_hits.load(Ordering::SeqCst), 6);
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(100));
    let episode = session.origin_control.acceptance_episode.as_ref().expect("held acceptance episode");
    assert_eq!(episode.full_bursts_completed, 1);
}

pub(in crate::refresh::tests) async fn assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(
    reason: HlsFreshManifestRequiredReason,
) {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n"
            .to_string();
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else if path.ends_with("/first.ts") {
            (200, Vec::new(), String::from_utf8_lossy(SWITCH_SEGMENT_BODY).into_owned())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let cache_dir = tempfile::tempdir().expect("fresh revalidation cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let recovery_request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    install_published_recovery_binding(&session, &recovery_request_url, None).await;
    let baseline_epoch = session.read().await.origin_epoch;
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired { reason };
    // A weaker observation signal must not reduce the strict revalidation
    // policy selected by the commit requirement.
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;

    assert!(trigger_origin_refresh_sync(request).await);

    let (manifest_requests, staged_segment_requests) = {
        let requests = origin.requests.lock().await;
        (
            requests.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count(),
            requests.iter().filter(|path| path.ends_with("/first.ts")).count(),
        )
    };
    assert_eq!(manifest_requests, plan.total_candidates());
    assert_eq!(staged_segment_requests, 1);

    let effective_host = host_from_base_url(&origin.base_url);
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
    let switched_head = session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .expect("fresh revalidation switched timeline head");
    assert!(switched_head.discontinuity_before);
    assert!(matches!(switched_head.status, SegmentCacheStatus::Ready { .. }));
}

#[tokio::test]
async fn expired_revalidation_cross_host_switch_runs_complete_beast_acceptance() {
    assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(
        HlsFreshManifestRequiredReason::ExpiredRevalidation,
    )
    .await;
}

#[tokio::test]
async fn hard_failure_revalidation_cross_host_switch_runs_complete_beast_acceptance() {
    assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(
        HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    )
    .await;
}

#[test]
fn hls_recovery_timing_candidate_preview_uses_actual_map_medium_not_playlist_head() {
    let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nhead40.ts\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nrecovery41.m4s\n";
    let (manifest, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    assert!(manifest.segments.first().is_some_and(|segment| segment.map_ref.is_none()));
    let actual_recovery_segment = preview.segments.get(1).expect("actual recovery medium after clear head");
    let required_map =
        actual_recovery_segment.map_ref.and_then(|map_id| preview.maps.iter().find(|map| map.proxy_map_id == map_id));

    let workload = super::super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        required_map,
        &key_resources,
        100,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::ClearSegmentFetch);
    assert_eq!(workload.map, HlsRecoveryMapWorkload::Fetch);
}

#[test]
fn hls_recovery_timing_candidate_preview_detects_generation_local_ready_aes128_key() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nrecovery40.ts\n";
    let (_, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    let key_resource = key_resources.first().cloned().expect("candidate key resource");
    session.transient.upsert_resources([key_resource.clone()]);
    let proxy_session_id = session.proxy_session_id.clone();
    let fetch = session.transient.begin_object_fetch(&proxy_session_id, &key_resource, "bin", 100, 60_000);
    let TransientObjectFetchDecision::Fetch(token) = fetch else {
        panic!("new candidate key starts one cache fill");
    };
    assert!(session.transient.mark_object_ready_if_current(
        &token,
        "application/octet-stream".to_string(),
        16,
        100,
        60_100,
    ));
    let actual_recovery_segment = preview.segments.first().expect("AES recovery medium");

    let workload = super::super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        None,
        &key_resources,
        101,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey);
    assert_eq!(
        super::super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            101
        ),
        super::super::switch_staging::HlsStagedSwitchMediaCompatibility::Compatible
    );
    assert_eq!(
        super::super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            60_101
        ),
        super::super::switch_staging::HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey,
        "the final commit revalidation must reject an expired READY key"
    );
}

#[test]
fn hls_recovery_timing_candidate_preview_requires_aes128_key_fetch_without_ready_evidence() {
    let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nrecovery40.ts\n";
    let (_, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    let actual_recovery_segment = preview.segments.first().expect("AES recovery medium");

    let workload = super::super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        None,
        &key_resources,
        101,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch);
    assert_eq!(
        super::super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            101
        ),
        super::super::switch_staging::HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey
    );
}

#[test]
fn switch_staging_rejects_map_fetch_outside_fixture_envelope_before_network() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        100,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_acceptance_episode_timing(100, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let identity = super::super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        Some("candidate.example"),
        &switch_manifest_body(true),
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    let map_fetch = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
        segment: HlsRecoveryObjectReadiness::Fetch,
        map: Some(HlsRecoveryObjectReadiness::Fetch),
        encryption: HlsRecoveryEncryptionReadiness::Clear,
    });

    assert_eq!(
        episode.bind_selected_candidate(episode.generation, identity, map_fetch),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::OutsideEnvelope
    );
}

#[tokio::test]
async fn staged_cross_host_aes_candidate_without_ready_key_evidence_never_commits() {
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nold1.ts\n#EXTINF:4.0,\nold2.ts\n#EXTINF:4.0,\nold3.ts\n";
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.php\"\n\
        #EXTINF:4.0,\nnew40.ts\n#EXTINF:4.0,\nnew41.ts\n#EXTINF:4.0,\nnew42.ts\n";

    assert_incompatible_switch_is_rejected_before_timeline_commit(
        baseline,
        candidate,
        HlsManifestRejectLogReason::SwitchEncryptionKeyNotReady,
    )
    .await;
}

#[tokio::test]
async fn staged_cross_host_aes_candidate_with_generation_local_ready_key_commits() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let now_ms = current_time_millis();
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.php\"\n\
        #EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n";
    let mut fetched = switch_fetched_manifest(&origin.base_url, false);
    fetched.body = candidate.to_string();
    let key_resource = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(mut manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(candidate, &fetched.final_manifest_url)
        else {
            panic!("AES switch manifest parses as a normal timeline");
        };
        super::super::commit::materialize_normal_key_resources(&mut manifest, b"secret", now_ms, 300_000)
            .into_iter()
            .next()
            .expect("AES switch candidate has one key resource")
    };
    {
        let mut session = session.write().await;
        session.transient.upsert_resources([key_resource.clone()]);
        let proxy_session_id = session.proxy_session_id.clone();
        let extension = key_resource.file_ext_hint.as_deref().expect("key extension");
        let fetch = session.transient.begin_object_fetch(&proxy_session_id, &key_resource, extension, now_ms, 300_000);
        let TransientObjectFetchDecision::Fetch(token) = fetch else {
            panic!("candidate key starts one cache fill");
        };
        assert!(session.transient.mark_object_ready_if_current(
            &token,
            "application/octet-stream".to_string(),
            16,
            now_ms,
            now_ms.saturating_add(300_000),
        ));
    }
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.now_ms = now_ms;
    let mut commit_task = tokio::spawn(async move {
        super::super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "encrypted cross-host switch commit").await;
    origin.release_segment_body.notify_one();
    commit_task.await.expect("encrypted switch commit task").expect("encrypted switch commit succeeds");

    let session = session.read().await;
    let switched = session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .expect("switched segment committed");
    assert!(switched.encryption.is_some());
    assert!(session
        .transient
        .ready_key_object_valid_until_ms(
            &session.proxy_session_id,
            &key_resource.id,
            key_resource.file_ext_hint.as_deref().expect("key extension"),
            current_time_millis(),
        )
        .is_some());
}

#[tokio::test]
async fn cross_host_switch_commits_only_after_complete_map_and_segment_staging() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host = host_from_base_url(&origin.base_url);
    let effective_host_id = crate::timeline::effective_origin_host_id(&effective_host);
    let (baseline_epoch, baseline_proxy_next, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            session.proxy_next_seq,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("required staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let mut commit_task = tokio::spawn(async move {
        super::super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "cross-host switch commit").await;

    assert!(!commit_task.is_finished());
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next);
        assert!(session.segments.values().all(|segment| segment.origin_key.origin_epoch == baseline_epoch));
    }
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("map metadata"), None);
    assert_eq!(
        *origin.requests.lock().await,
        vec!["/live/final/init.mp4".to_string(), "/live/final/first.ts".to_string()]
    );

    origin.release_segment_body.notify_one();
    commit_task.await.expect("switch commit task").expect("switch commit succeeds");
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));

    let (first_segment_key, first_map_key) = {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
        assert_eq!(session.origin_epoch_effective_host_id, Some(effective_host_id));
        let switched = session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
            .collect::<Vec<_>>();
        assert_eq!(switched.len(), 2);
        assert_eq!(switched[0].origin_key.host_local_sequence, 500);
        assert!(switched[0].discontinuity_before);
        assert!(matches!(
            switched[0].status,
            SegmentCacheStatus::Ready { content_length, .. }
                if content_length == u64::try_from(SWITCH_SEGMENT_BODY.len()).unwrap_or(u64::MAX)
        ));
        assert!(!matches!(switched[1].status, SegmentCacheStatus::Ready { .. }));
        let map_id = switched[0].map_ref.expect("first switched segment requires map");
        assert_eq!(switched[1].map_ref, Some(map_id));
        let map = session.maps.get(&map_id).expect("staged map committed to timeline");
        assert!(matches!(
            map.status,
            MapCacheStatus::Ready { content_length, .. }
                if content_length == u64::try_from(SWITCH_MAP_BODY.len()).unwrap_or(u64::MAX)
        ));
        (switched[0].cache_key.clone(), map.cache_key.clone())
    };
    let mut segment_file = cache.open_range(&first_segment_key, 0).await.expect("open staged segment");
    let mut segment_bytes = Vec::new();
    segment_file.read_to_end(&mut segment_bytes).await.expect("read staged segment");
    assert_eq!(segment_bytes, SWITCH_SEGMENT_BODY);
    let mut map_file = cache.open_range(&first_map_key, 0).await.expect("open staged map");
    let mut map_bytes = Vec::new();
    map_file.read_to_end(&mut map_bytes).await.expect("read staged map");
    assert_eq!(map_bytes, SWITCH_MAP_BODY);
}

#[tokio::test]
async fn caller_cancellation_after_owned_switch_staging_queues_and_collects_rollback() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host_id = crate::timeline::effective_origin_host_id(&host_from_base_url(&origin.base_url));
    let (baseline_epoch, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("required staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let (staging_complete_tx, staging_complete_rx) = tokio::sync::oneshot::channel();
    let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
    let mut staging_task = tokio::spawn(async move {
        let staged = super::super::switch_staging::stage_alternative_manifest_switch(&request, &fetched)
            .await
            .expect("switch staging succeeds");
        staging_complete_tx.send(()).expect("staging completion receiver remains");
        let _ = hold_rx.await;
        drop(staged);
    });

    await_controlled_switch_segment_prefix(&origin, &mut staging_task, "owned switch staging").await;
    origin.release_segment_body.notify_one();
    staging_complete_rx.await.expect("owned cache commits complete");
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 2));
    assert!(cache.metadata(&staged_segment_key).await.expect("staged segment metadata").is_some());
    assert!(cache.metadata(&staged_map_key).await.expect("staged map metadata").is_some());

    staging_task.abort();
    assert!(staging_task.await.expect_err("staging task is cancelled").is_cancelled());
    drop(hold_tx);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (2, 0));
    assert_eq!(session.read().await.origin_epoch, baseline_epoch);

    let report = cleanup_manager.run_garbage_collection_once(200).await.expect("rollback GC succeeds");
    assert_eq!(report.cache_object_deletions_succeeded, 2);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("rolled-back segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("rolled-back map metadata"), None);
}

#[tokio::test]
async fn stale_acceptance_generation_rejects_and_removes_completed_switch_staging() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::Acceptance).await;
}

#[tokio::test]
async fn stale_progress_generation_rejects_and_removes_completed_switch_staging() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::Progress).await;
}

#[tokio::test]
async fn switch_segment_staging_failure_holds_episode_without_timeline_mutation() {
    let manifest = switch_manifest_body(false);
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let (baseline_epoch, baseline_proxy_next, baseline_sequences) = {
        let session = session.read().await;
        (session.origin_epoch, session.proxy_next_seq, session.segments.keys().copied().collect::<Vec<_>>())
    };
    let request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");

    let result = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await;

    assert!(matches!(result, Err(OriginManifestFetchError::RetryExhausted)));
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch);
    assert_eq!(session.proxy_next_seq, baseline_proxy_next);
    assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), baseline_sequences);
    let episode = session.origin_control.acceptance_episode.as_ref().expect("failed switch episode retained");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.state, super::super::super::manifest_acceptance::HlsManifestAcceptanceState::Holding);
    drop(session);
    let requests = origin.requests.lock().await;
    assert_eq!(requests.iter().filter(|path| path_has_extension(path, "ts")).count(), 1);
}

#[tokio::test]
async fn provider_preflight_failure_real_refresh_path_has_zero_candidates() {
    let origin = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let session = test_session();
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    session.write().await.origin_account_binding = Some(HlsOriginAccountBinding::new(
        Arc::from("missing-input"),
        Arc::from("missing-account"),
        &proxy_session_id,
        0,
    ));
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/index.m3u8", origin.base_url)).expect("unused origin URL");
    request.origin_io = Some(HlsOriginIoContext {
        ctx: ctx.clone(),
        client_addr: "127.0.0.1:12345".parse().expect("test client address"),
        allow_grace: false,
        priority: 0,
        connection_kind: ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: None,
    });
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    let error = super::super::provider_preflight_manifest_error(HlsBoundAccountAcquireErrorKind::Missing);

    assert!(matches!(&error, OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Missing)));
    assert!(trigger_origin_refresh_sync(request).await);
    assert!(origin.requests.lock().await.is_empty());

    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert_eq!(
        session.origin_control.progress_phase,
        super::super::super::origin_progress::HlsOriginProgressPhase::Cold
    );
    assert_eq!(
        session.origin_control.path_condition,
        super::super::super::origin_progress::HlsOriginPathCondition::HardFetchFailure
    );
    assert!(session.origin_control.last_origin_response_at_ms.is_none());
    assert!(session.origin_control.manifest_origin_binding.is_none());
    assert!(session.last_rendered_manifest.is_none());
    assert!(session.published_live_origin_baseline.is_none());
    assert!(session.established_manifest_recovery_binding().is_none());
    drop(session);
    let metrics = metrics.snapshot();
    assert_eq!(metrics.refresh_started, 1);
    assert_eq!(metrics.refresh_completed, 0);
    assert_eq!(metrics.refresh_retried, 0);
    assert_eq!(metrics.refresh_skipped, 0);
    assert_eq!(metrics.refresh_failed, 1);
}
