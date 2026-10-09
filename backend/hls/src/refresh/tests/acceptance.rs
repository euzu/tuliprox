use super::{
    apply_manifest_fetch_failure_signal, commit_fetched_manifest, critical_handoff_app_config,
    fetch_and_commit_manifest_with_policy, fetched_manifest, host_from_base_url, manifest_fetch_context,
    mark_origin_refresh_started_with_outcome, no_delay_policy, prepare_active_critical_handoff_lease,
    prepare_cross_host_baseline, publish_ready_test_manifest, retry_hls_origin_manifest_recovery_chain,
    spawn_critical_emergency_origin, spawn_test_origin, switch_test_request, test_acceptance_episode_timing,
    test_deterministic_timeline_conflict, test_manifest_origin_binding, test_origin_refresh_request,
    test_recovery_timing_policy, test_session, three_segment_manifest_body, trigger_origin_refresh_sync,
    CriticalEmergencyOriginServer, HlsManifestAcceptanceTrigger, HlsManifestCommitAcceptanceMode,
    HlsManifestCommitRequirement, HlsManifestFetchSelection, HlsManifestOriginBinding, HlsManifestRejectLogReason,
    HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason,
    HlsRecoveryEtaMs, LiveHlsOriginEntry, OriginManifestFetchError, OriginRefreshRequest, TestOriginServer,
    CRITICAL_HANDOFF_MANIFEST_BODY, CRITICAL_HANDOFF_TS_BODY,
};
use crate::{
    build_rewrite_secret_fingerprint, media_reserve::HlsLeaseReserveAvailabilityBasis,
    timeline::HLS_PROVISIONING_ORIGIN_EPOCH, CacheAccessState, GarbageCollectionPolicy, HlsAccessLease,
    HlsFreshManifestRequiredReason, HlsGarbageCollector, HlsManifestAcceptanceExhaustionReason, HlsProxyManager,
    HlsSegmentCache, HlsSession, HlsSessionKey, HlsSessionStore, OriginSegmentKey, RenderedManifest,
    RenderedManifestStoreOutcome, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
};
use axum::http::StatusCode;
use shared::model::{ConfigProviderDto, HlsManifestRecoveryBurstLevel, ProviderUrlSelectionPolicy};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tuliprox_core::model::{ConfigProvider, HlsManifestRecoveryBurstConfig};
use url::Url;

#[test]
fn deterministic_conflict_returns_generation_bound_post_refresh_action() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_control.progress_generation = 7;
    session.activity.media_readiness_generation = 11;

    let action = apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::DeterministicTimelineConflict(Box::new(test_deterministic_timeline_conflict())),
        100,
    );

    assert_eq!(
        action,
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
            origin_progress_generation: 7,
            media_readiness_generation: 11,
        }
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(100));
    assert_eq!(
        session.origin_control.path_condition,
        super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    );
    assert!(session.fresh_manifest_commit_required.is_none());
}

#[test]
fn retry_exhaustion_preserves_qualified_acceptance_conflict_but_not_all_failed_transport_state() {
    for (reason, expected) in [
        (
            HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
            super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict,
        ),
        (
            HlsManifestAcceptanceExhaustionReason::AllFailed,
            super::super::super::origin_progress::HlsOriginPathCondition::RetryableFetchFailure,
        ),
    ] {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
        session.origin_control.begin_acceptance_episode(
            100,
            burst_plan,
            HlsManifestAcceptanceTrigger::Observe,
            &test_acceptance_episode_timing(100, burst_plan),
        );
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        episode.record_full_burst();
        episode.record_exhaustion(reason);
        episode.hold_after_uncommitted_burst(None, None);
        session.origin_control.path_condition =
            super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict;

        apply_manifest_fetch_failure_signal(&mut session, &OriginManifestFetchError::RetryExhausted, 200);

        assert_eq!(session.origin_control.path_condition, expected);
    }
}

pub(in crate::refresh::tests) async fn spawn_rotating_parent_origin() -> TestOriginServer {
    let rotating_parent = Arc::new(AtomicUsize::new(0));
    let handler_counter = Arc::clone(&rotating_parent);
    spawn_test_origin(Arc::new(move |_path| {
        let parent = handler_counter.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        (
            200,
            Vec::new(),
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745190_490.ts\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745180_480.ts\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745191_491.ts\n"
            ),
        )
    }))
    .await
}

#[tokio::test]
async fn rotating_volatile_parent_proves_one_deterministic_conflict() {
    let baseline_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745180_480.ts\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745181_481.ts\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745182_482.ts\n";
    let server = spawn_rotating_parent_origin().await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&manifest_url).expect("local replay origin entry");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let plan = request.manifest_recovery_burst.level.plan();
    let mut baseline = fetched_manifest(baseline_body);
    baseline.final_manifest_url = manifest_url.clone();
    baseline.resolved_request_url = manifest_url;
    baseline.redirect_host = Some("127.0.0.1".to_string());
    {
        let mut session = session.write().await;
        commit_fetched_manifest(&mut session, &baseline, &request, 100).expect("baseline commits");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 101 };
        }
        session.render_and_store_manifest(101).expect("baseline publishes");
        assert_eq!(session.published_resource_history.generation(), 3);
    }

    let Err(first) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("one complete candidate landscape must prove the replay conflict");
    };
    let OriginManifestFetchError::DeterministicTimelineConflict(first_conflict) = first else {
        panic!("deterministic replay conflict must not collapse to retry exhaustion");
    };
    assert_eq!(first_conflict.previous_proxy_tail, Some(2));
    assert_eq!(first_conflict.existing_proxy_seq, 0);
    assert_eq!(first_conflict.candidate_position, 1);
    assert_eq!(first_conflict.candidate_origin_seq, 491);
    assert_eq!(server.requests.lock().await.len(), plan.total_candidates().saturating_add(1));
    {
        let session = session.read().await;
        let episode = session.origin_control.acceptance_episode.as_ref().expect("acceptance episode retained");
        assert_eq!(episode.full_bursts_completed, 1);
        assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
        assert!(episode.deterministic_conflict_receipt().is_some());
        assert!(matches!(
            super::super::super::manifest_acceptance::manifest_acceptance_episode_status(
                Some(episode),
                episode.generation,
                request.now_ms,
            ),
            super::super::super::manifest_acceptance::HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted {
                reason: HlsManifestAcceptanceExhaustionReason::DeterministicTimelineConflict,
                ..
            }
        ));
        assert_eq!(session.proxy_next_seq, Some(3), "replayed content receives no proxy sequence");
    }

    let request_count_before_receipt_sample = server.requests.lock().await.len();
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;
    let Err(repeated) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("an unchanged ordinary sample must remain rejected");
    };
    assert!(matches!(
        repeated,
        OriginManifestFetchError::DeterministicTimelineConflict(ref conflict)
            if conflict.as_ref() == first_conflict.as_ref()
    ));
    assert_eq!(
        server.requests.lock().await.len(),
        request_count_before_receipt_sample.saturating_add(1),
        "an unchanged receipt permits one ordinary request but no second burst"
    );

    {
        let mut session = session.write().await;
        session.published_resource_history.record(
            super::super::super::resource_identity::HlsMediaResourceIdentity::from_url(
                "http://127.0.0.1/live/user/pass/unrelated-published.ts",
                None,
            ),
            99,
        );
    }
    let request_count_before_changed_landscape = server.requests.lock().await.len();
    let Err(changed) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("changed published evidence must still reject the replay");
    };
    assert!(matches!(changed, OriginManifestFetchError::DeterministicTimelineConflict(_)));
    assert_eq!(
        server.requests.lock().await.len(),
        request_count_before_changed_landscape.saturating_add(plan.total_candidates()),
        "changed receipt evidence authorizes exactly one newly configured full burst"
    );
    let session = session.read().await;
    assert_eq!(session.proxy_next_seq, Some(3), "the replay stays blocked after reevaluation");
    assert!(!session.segments.contains_key(&3));
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_stale_guard_prevents_refresh_start() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "stale-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("registered session has recovery-pressure evidence");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    session.write().await.origin_control.progress_generation = owner_key.origin_progress_generation.saturating_add(1);

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 100).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded
    );
    let session = session.read().await;
    assert!(!session.origin_refresh.in_flight);
    assert!(session.origin_control.acceptance_episode.is_none());
}

pub(in crate::refresh::tests) fn install_stored_provisioning_manifest(session: &mut HlsSession, rendered_at_ms: u64) {
    let segment_proxy_seqs = (0_u64..3).collect::<Vec<_>>();
    for proxy_seq in &segment_proxy_seqs {
        session.segments.insert(
            *proxy_seq,
            SegmentEntry {
                origin_key: OriginSegmentKey {
                    origin_epoch: HLS_PROVISIONING_ORIGIN_EPOCH,
                    effective_host_id: 0,
                    host_local_sequence: *proxy_seq,
                    host_local_index: u32::try_from(*proxy_seq).unwrap_or(u32::MAX),
                },
                proxy_seq: *proxy_seq,
                duration_ms: 2_000,
                proxy_file_ext: "ts".to_string(),
                content_type: "video/mp2t".to_string(),
                cache_key: SegmentCacheKey::new(session.proxy_session_id.clone(), *proxy_seq, "ts"),
                discontinuity_before: false,
                program_date_time: None,
                daterange_tags_before: Vec::new(),
                origin_byte_range: None,
                map_ref: None,
                encryption: None,
                origin_fetch_ref: None,
                status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
                last_rendered_at_ms: None,
                access: Arc::new(CacheAccessState::new()),
            },
        );
    }
    session.publishable_origin_head_proxy_seq = segment_proxy_seqs.first().copied();
    session.publishable_origin_tail_proxy_seq = segment_proxy_seqs.last().copied();
    session.proxy_next_seq = Some(3);
    session.target_duration = Some(2);
    assert_eq!(
        session.store_rendered_manifest(RenderedManifest {
            body: "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n".to_string(),
            first_proxy_seq: 0,
            last_proxy_seq: 2,
            discontinuity_sequence: 0,
            target_duration_ms: 2_000,
            playlist_duration_ms: 6_000,
            valid_until_ms: rendered_at_ms.saturating_add(6_000),
            render_gap_segments: 0,
            rendered_at_ms,
            segment_proxy_seqs,
        }),
        RenderedManifestStoreOutcome::Stored
    );
    assert!(session.published_live_origin_baseline.is_none());
}

#[tokio::test]
async fn warm_retryable_initial_failure_does_not_open_acceptance_episode() {
    let server = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "retry".to_string()))).await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(10);
        session.origin_control.record_media_progress(50, 4_000);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

    assert!(trigger_origin_refresh_sync(request.clone()).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, request.retry_policy.attempt_count());
    assert!(session.read().await.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn initial_provider_failover_does_not_leak_into_established_burst() {
    let phase = Arc::new(AtomicUsize::new(0));
    let phase_for_first = Arc::clone(&phase);
    let first = spawn_test_origin(Arc::new(move |_path| {
        if phase_for_first.load(Ordering::SeqCst) == 0 {
            (200, Vec::new(), three_segment_manifest_body(100))
        } else {
            (407, Vec::new(), "rotate".to_string())
        }
    }))
    .await;
    let phase_for_second = Arc::clone(&phase);
    let second = spawn_test_origin(Arc::new(move |_path| {
        if phase_for_second.load(Ordering::SeqCst) == 0 {
            (500, Vec::new(), "unexpected".to_string())
        } else {
            (461, Vec::new(), "final-hard-failure".to_string())
        }
    }))
    .await;
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
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    fetch_and_commit_manifest_with_policy(&mut request).await.expect("initial provider URL commits baseline");
    publish_ready_test_manifest(&session, 200).await;
    let binding =
        session.read().await.established_manifest_recovery_binding().expect("published baseline has fixed binding");
    assert_eq!(binding.provider_url_index(), Some(0));
    let provider_index_before_failure = provider.get_current_index();

    phase.store(1, Ordering::SeqCst);
    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("hard initial provider cycle and fixed-target recovery must remain failed");
    };
    assert!(matches!(error, OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, _)));

    let plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    let first_requests = first.requests.lock().await;
    let second_requests = second.requests.lock().await;
    assert!(
        first_requests.len() >= 2_usize.saturating_add(plan.total_candidates()),
        "baseline, ordinary provider attempt, and at least one full fixed-target burst are required"
    );
    assert_eq!(second_requests.len(), 1, "only the ordinary provider cycle may reach URL index 1");
    assert!(first_requests.iter().all(|path| path == "/live/user/pass/12345.m3u8?token=fixed"));
    assert_eq!(second_requests[0], "/live/user/pass/12345.m3u8?token=fixed");
    assert_eq!(
        provider.get_current_index(),
        provider_index_before_failure,
        "fixed-target recovery must not rotate provider state"
    );
    assert_eq!(
        session
            .read()
            .await
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .map(HlsManifestOriginBinding::provider_url_index),
        Some(Some(0))
    );
}

#[tokio::test]
async fn expired_acceptance_deadline_still_completes_first_beast_burst_without_follow_up() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let origin = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "retry".to_string()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url))
        .expect("deadline test origin entry");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = request.origin_entry.url().clone();
    let mut context = manifest_fetch_context(&request);
    let mut expired_after_full_burst = test_recovery_timing_policy(0);
    expired_after_full_burst.evaluation_eta = HlsRecoveryEtaMs::default();
    expired_after_full_burst.commit_eta = HlsRecoveryEtaMs::default();
    expired_after_full_burst.scheduling_eta = HlsRecoveryEtaMs::default();
    context.recovery_timing_policy = expired_after_full_burst;

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    let manifest_requests =
        origin.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, plan.total_candidates());
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("completed first acceptance burst");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert_eq!(episode.full_bursts_completed, 1);
}

#[tokio::test]
async fn cold_start_hard_initial_failure_does_not_start_acceptance_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
    let metrics = Arc::clone(request.segment_worker_pool.metrics());

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, 1);
    assert_ne!(manifest_requests, 1_usize.saturating_add(plan.total_candidates()));
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
    let metrics = metrics.snapshot();
    assert_eq!(metrics.refresh_failed, 1);
    assert_eq!(metrics.refresh_completed, 0);
}

#[tokio::test]
async fn cold_provider_failover_cycle_does_not_start_acceptance_burst() {
    let first = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "rotate".to_string()))).await;
    let second = spawn_test_origin(Arc::new(|_path| (461, Vec::new(), "final-hard-failure".to_string()))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(provider),
    )
    .expect("provider entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };

    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("cold provider cycle must preserve its final hard response");
    };

    assert!(matches!(
        error,
        OriginManifestFetchError::NonRetryableStatus(status) if status.as_u16() == 461
    ));
    assert_eq!(first.requests.lock().await.len(), 1);
    assert_eq!(second.requests.lock().await.len(), 1);
    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert!(session.origin_control.manifest_origin_binding.is_none());
    assert_eq!(
        session.origin_control.progress_phase,
        super::super::super::origin_progress::HlsOriginProgressPhase::Cold
    );
}

#[tokio::test]
async fn successful_origin_commit_without_renderable_origin_window_does_not_enable_burst() {
    let manifest_hits = Arc::new(AtomicUsize::new(0));
    let manifest_hits_for_handler = Arc::clone(&manifest_hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        let media_sequence = if manifest_hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 { 100 } else { 103 };
        (200, Vec::new(), three_segment_manifest_body(media_sequence))
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        install_stored_provisioning_manifest(&mut session, 10);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };

    let baseline = fetch_and_commit_manifest_with_policy(&mut request).await.expect("ordinary origin baseline commits");
    assert_eq!(baseline.fetched.selection, HlsManifestFetchSelection::Initial);
    assert_eq!(baseline.fetched.candidate_requests, 1);
    {
        let session = session.read().await;
        assert!(session.origin_control.manifest_origin_binding.is_some());
        assert!(session.origin_control.pinned_host.is_some());
        assert_eq!(session.origin_seq_highwater, Some(102));
        assert_eq!(
            session.origin_control.progress_phase,
            super::super::super::origin_progress::HlsOriginProgressPhase::Fresh
        );
        assert_eq!(
            session.last_rendered_manifest.as_ref().expect("provisioning manifest remains stored").segment_proxy_seqs,
            vec![0, 1, 2]
        );
        assert!(session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch != HLS_PROVISIONING_ORIGIN_EPOCH)
            .all(|segment| !matches!(segment.status, SegmentCacheStatus::Ready { .. })));
        assert!(session.published_live_origin_baseline.is_none());
        assert!(session.established_manifest_recovery_binding().is_none());
        assert!(session.origin_control.acceptance_episode.is_none());
    }

    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let follow_up =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("suppressed recovery uses ordinary fetch");

    assert_eq!(follow_up.fetched.selection, HlsManifestFetchSelection::Initial);
    assert_eq!(follow_up.fetched.candidate_requests, 1);
    assert_eq!(manifest_hits.load(Ordering::SeqCst), 2);
    assert_ne!(manifest_hits.load(Ordering::SeqCst), HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates());
    let session = session.read().await;
    assert!(session.published_live_origin_baseline.is_none());
    assert!(session.established_manifest_recovery_binding().is_none());
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn material_reduced_retry_timeline_change_starts_one_new_complete_configured_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        let resource = if hit < plan.total_candidates() { "old900.ts" } else { "new900.ts" };
        (
            200,
            Vec::new(),
            format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{resource}\n"),
        )
    }))
    .await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let cache_dir = tempfile::tempdir().expect("requalification cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url =
        Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("requalification target");
    let attempts = request.retry_policy.attempt_count();
    let context = manifest_fetch_context(&request);

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    let reduced_attempts = attempts.saturating_sub(2);
    assert_eq!(hits.load(Ordering::SeqCst), plan.total_candidates().saturating_mul(2).saturating_add(reduced_attempts));
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("requalified episode");
    assert!(episode.generation.0 >= 2);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
}

#[tokio::test]
async fn material_change_in_last_reduced_slot_still_runs_new_episode_full_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let retry_policy = no_delay_policy();
    let attempts = retry_policy.attempt_count();
    let unchanged_candidate_count = plan.total_candidates().saturating_add(attempts.saturating_sub(2));
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        let resource = if hit < unchanged_candidate_count { "old900.ts" } else { "new900.ts" };
        (
            200,
            Vec::new(),
            format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{resource}\n"),
        )
    }))
    .await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let cache_dir = tempfile::tempdir().expect("last-slot requalification cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.retry_policy = retry_policy;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url))
        .expect("last-slot requalification target");
    let context = manifest_fetch_context(&request);

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        plan.total_candidates().saturating_mul(2).saturating_add(attempts.saturating_sub(1))
    );
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("last-slot requalified episode");
    assert!(episode.generation.0 >= 2);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
}

pub(in crate::refresh::tests) async fn assert_critical_handoff_evidence(
    request: &OriginRefreshRequest,
    live_lease: &HlsAccessLease,
    now_ms: u64,
) {
    let manifest = live_lease.last_manifest_snapshot.as_ref().expect("authoritative manifest snapshot");
    assert_eq!(manifest.snapshot_generation, 1);
    let ready_timeline = {
        let session = request.session.read().await;
        session.ready_timeline_snapshot(
            live_lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
            now_ms,
        )
    };
    let reserve = super::super::super::evaluate_lease_reserve(super::super::super::HlsLeaseReserveInput {
        manifest,
        cursor: &live_lease.playback_cursor,
        ready_timeline: &ready_timeline,
        now_ms,
        playback_rate_guard_milli: super::super::super::HLS_PLAYBACK_RATE_GUARD_MILLI,
        recovery_trigger_budget: super::super::super::recovery_timing::HlsRecoveryTriggerBudgetMs::from_millis(0),
        origin_path_degraded: true,
        recovery_committed: false,
    });
    assert_eq!(reserve.availability_basis, HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline);
    assert_eq!(reserve.guaranteed_media_horizon_ms, manifest.last_visible_media_end_ms);
    assert_eq!(reserve.guaranteed_reserve_ms, 0);
    assert!(reserve.cutover_required);
    let candidate_tracks = match tuliprox_mpegts::ts_inspector::inspect_mpeg_ts(
        std::io::Cursor::new(CRITICAL_HANDOFF_TS_BODY),
        tuliprox_mpegts::ts_inspector::HlsTsProbeProtection::Clear,
        tuliprox_mpegts::ts_inspector::HlsTsProbeBudget::default(),
    )
    .expect("critical handoff fixture inspection succeeds")
    {
        tuliprox_mpegts::ts_inspector::HlsTsProbeOutcome::Found(signature) => signature,
        outcome => panic!("critical handoff fixture has no MPEG-TS tracks: {outcome:?}"),
    };
    let terminal_response =
        request.app_config.custom_stream_response.load_full().expect("critical handoff terminal response");
    let terminal_asset = super::super::snapshot_terminal_media_asset(
        terminal_response.channel_unavailable.as_ref().expect("critical handoff terminal buffer"),
    )
    .expect("critical handoff terminal asset");
    assert_eq!(
        crate::critical_handoff::terminal_alternative_compatibility_for_critical_lease(
            Some(terminal_asset.as_ref()),
            live_lease,
            &candidate_tracks,
        ),
        crate::manifest_acceptance::HlsTerminalAlternativeCompatibility::LiveHandoffSafer
    );
}

pub(in crate::refresh::tests) async fn assert_critical_handoff_timeline_commit(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    origin: &CriticalEmergencyOriginServer,
    baseline_epoch: u64,
    expected_candidate_count: usize,
) {
    let (staged_cache_key, staged_content_length) = {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
        let effective_host = host_from_base_url(&origin.base_url);
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
        assert_eq!(
            session.origin_epoch_effective_host_id,
            Some(crate::timeline::effective_origin_host_id(&effective_host))
        );
        let episode = session.origin_control.acceptance_episode.as_ref().expect("completed acceptance episode");
        assert!(episode.full_burst_completed);
        assert_eq!(episode.completed_burst_candidates, expected_candidate_count);
        assert_eq!(episode.state, super::super::super::manifest_acceptance::HlsManifestAcceptanceState::Completed);
        let switched = session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
            .collect::<Vec<_>>();
        assert_eq!(switched.len(), 2);
        let staged = switched.first().expect("staged handoff segment");
        assert_eq!(staged.origin_key.host_local_sequence, 900);
        assert!(staged.discontinuity_before);
        let content_length = match &staged.status {
            SegmentCacheStatus::Ready { content_length, .. } => *content_length,
            SegmentCacheStatus::Discovered
            | SegmentCacheStatus::Queued { .. }
            | SegmentCacheStatus::Fetching { .. }
            | SegmentCacheStatus::CapacityDeferred { .. }
            | SegmentCacheStatus::FailedRetryable { .. }
            | SegmentCacheStatus::FailedPermanent { .. }
            | SegmentCacheStatus::Expired => panic!("staged handoff segment must be READY"),
        };
        assert_eq!(content_length, u64::try_from(CRITICAL_HANDOFF_TS_BODY.len()).unwrap_or(u64::MAX));
        assert!(!matches!(switched[1].status, SegmentCacheStatus::Ready { .. }));
        (staged.cache_key.clone(), content_length)
    };
    let staged_metadata = cache
        .metadata(&staged_cache_key)
        .await
        .expect("staged cache metadata reads")
        .expect("staged cache object exists");
    assert_eq!(staged_metadata.size, staged_content_length);
}

#[tokio::test]
async fn critical_single_alternative_full_acceptance_commits_verified_new_origin_epoch() {
    let origin = spawn_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("critical handoff cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let baseline_binding = HlsManifestOriginBinding::new(
        Url::parse("https://previous.example.com/live/index.m3u8?token=baseline").expect("baseline URL"),
        Some(0),
    )
    .expect("baseline binding");
    session.write().await.origin_control.manifest_origin_binding = Some(baseline_binding);
    let now_ms = super::super::current_time_millis();
    let (baseline_epoch, baseline_readiness_generation) = {
        let session = session.read().await;
        (session.origin_epoch, session.activity.media_readiness_generation)
    };
    let (hls_proxy, lease_id, live_lease) = prepare_active_critical_handoff_lease(&session, &cache, now_ms).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.app_config = critical_handoff_app_config();
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(lease_id);
    request.now_ms = now_ms;
    assert_critical_handoff_evidence(&request, &live_lease, now_ms).await;

    let plan = request.manifest_recovery_burst.level.plan();
    let target_url = request.origin_entry.url().clone();
    let fetch_context = manifest_fetch_context(&request);
    let committed = retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url.clone()),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Critical,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await
    .expect("critical emergency handoff commits");

    assert_eq!(committed.fetched.body.as_bytes(), CRITICAL_HANDOFF_MANIFEST_BODY);
    assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), plan.total_candidates());
    assert_eq!(origin.segment_requests.load(Ordering::SeqCst), 1);
    assert_critical_handoff_timeline_commit(&session, &cache, &origin, baseline_epoch, plan.total_candidates()).await;
    assert_eq!(
        session.read().await.activity.media_readiness_generation,
        baseline_readiness_generation.saturating_add(1),
        "one staged READY transaction advances media readiness exactly once"
    );
    assert_eq!(
        session
            .read()
            .await
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .map(super::super::super::manifest_origin_binding::HlsManifestOriginBinding::request_url),
        Some(&target_url)
    );
}

pub(in crate::refresh::tests) async fn assert_committed_content_anchor_is_gc_pinned_until_read_pin_drop() {
    const COMMITTED_BYTES: &[u8] = b"committed-media-bytes";

    let temp_dir = tempfile::tempdir().expect("acceptance pin cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let sessions = Arc::new(HlsSessionStore::new());
    let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let gc = HlsGarbageCollector::new(
        Arc::clone(&sessions),
        Arc::clone(&cache),
        GarbageCollectionPolicy {
            cache_duration_ms: 0,
            cache_bytes_global: 10_000,
            cache_bytes_per_session: 10_000,
            session_idle_timeout_ms: u64::MAX,
            temp_file_retention_ms: 30_000,
            failed_segment_retention_ms: 10,
        },
        build_rewrite_secret_fingerprint(b"secret"),
    );
    let manifest_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
        #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n\
        #EXTINF:4.0,\n4.ts\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            manifest_body,
            "http://origin.example/live/index.m3u8",
        )
    else {
        panic!("acceptance pin manifest must parse as a normal timeline");
    };
    let cache_key = {
        let mut session = session.write().await;
        session.proxy_next_seq = Some(1);
        session.apply_origin_manifest(&manifest).expect("acceptance pin timeline commits");
        session.segments.get(&1).expect("first committed segment").cache_key.clone()
    };
    cache.write_bytes_and_commit(&cache_key, COMMITTED_BYTES).await.expect("committed acceptance object writes");
    let access = {
        let mut session = session.write().await;
        let segment = session.segments.get_mut(&1).expect("first committed segment");
        segment.status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(COMMITTED_BYTES.len()).unwrap_or(u64::MAX),
            ready_at_ms: 0,
        };
        let expected_identity =
            crate::resource_identity::HlsMediaResourceIdentity::from_url("http://origin.example/live/1.ts", None);
        session
            .segments
            .values()
            .rev()
            .find_map(|entry| {
                let fetch_ref = entry.origin_fetch_ref.as_ref()?;
                crate::resource_identity::HlsMediaResourceIdentity::from_url(
                    &fetch_ref.resolved_origin_url,
                    fetch_ref.byte_range,
                )
                .matches(expected_identity)
                .then(|| Arc::clone(&entry.access))
            })
            .expect("content anchor object selection")
    };
    let read_pin = super::super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&access), 5);
    assert_eq!(access.active_readers(), 1);

    let pinned_report = gc.run_once(10_000).await.expect("GC runs while acceptance object is pinned");
    assert_eq!(pinned_report.segments_deleted_duration, 0);
    assert!(session.read().await.segments.contains_key(&1));
    assert!(cache.metadata(&cache_key).await.expect("pinned object metadata reads").is_some());

    drop(read_pin);
    assert_eq!(access.active_readers(), 0);
    let released_report = gc.run_once(10_001).await.expect("GC runs after acceptance pin release");
    assert_eq!(released_report.segments_deleted_duration, 1);
    assert!(!session.read().await.segments.contains_key(&1));
    assert!(cache.metadata(&cache_key).await.expect("released object metadata reads").is_none());
}

#[tokio::test]
async fn committed_content_anchor_object_survives_gc_until_acceptance_read_pin_drop() {
    assert_committed_content_anchor_is_gc_pinned_until_read_pin_drop().await;
}
