use super::{
    assert_incompatible_switch_is_rejected_before_timeline_commit, commit_ready_baseline_snapshot,
    critical_handoff_app_config, manifest_fetch_context, prepare_active_critical_handoff_lease,
    prepare_cross_host_baseline, retry_hls_origin_manifest_recovery_chain, spawn_critical_emergency_origin,
    spawn_critical_emergency_origin_with_control, switch_test_request, test_acceptance_episode_timing,
    test_manifest_origin_binding, test_session, CriticalEmergencyOriginServer, HlsManifestAcceptanceTrigger,
    HlsManifestCommitAcceptanceMode, HlsManifestCommitError, HlsManifestRejectLogReason,
    CRITICAL_HANDOFF_MANIFEST_BODY, CRITICAL_HANDOFF_TS_BODY,
};
use crate::{
    build_rewrite_secret_fingerprint, terminal_tail::HlsLeasePlaybackMode, GarbageCollectionPolicy, HlsAccessLease,
    HlsAccessLeaseId, HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsGarbageCollector, HlsLeaseManifestSnapshot,
    HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache, HlsSession, HlsSessionKey, HlsSessionStore,
    SegmentCacheStatus,
};
use shared::model::HlsManifestRecoveryBurstLevel;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;

pub(in crate::refresh::tests) async fn spawn_controlled_critical_emergency_origin() -> CriticalEmergencyOriginServer {
    spawn_critical_emergency_origin_with_control(true).await
}

pub(in crate::refresh::tests) fn active_critical_test_lease(
    lease_id: &str,
    proxy_session_id: &super::super::super::ProxySessionId,
    issued_at_ms: u64,
    now_ms: u64,
    manifest: HlsLeaseManifestSnapshot,
) -> HlsAccessLease {
    let mut lease = HlsAccessLease::pending(
        HlsAccessLeaseId(lease_id.to_string()),
        HlsPlaybackFamilyKey::new(lease_id, lease_id),
        proxy_session_id.clone(),
        lease_id.to_string(),
        format!("{lease_id}-session"),
        1,
        "12345".to_string(),
        12345,
        issued_at_ms,
        120_000,
    );
    lease.state = super::super::super::HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(now_ms.saturating_add(120_000));
    lease.valid_until_ms = now_ms.saturating_add(120_000);
    lease.playback_mode = HlsLeasePlaybackMode::Live;
    lease.admission_generation = 1;
    lease.last_manifest_snapshot = Some(manifest);
    lease
}

pub(in crate::refresh::tests) async fn prepare_three_segment_critical_timeline(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> HlsLeaseManifestSnapshot {
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nold500.ts\n#EXTINF:4.0,\nold501.ts\n#EXTINF:4.0,\nold502.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            body,
            "http://previous.example.com/live/user/pass/12345.m3u8",
        )
    else {
        panic!("three-segment baseline parses");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(
                &manifest,
                crate::timeline::effective_origin_host_id("previous.example.com"),
            )
            .expect("three-segment baseline commits");
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }
    commit_ready_baseline_snapshot(session, cache, now_ms).await
}

pub(in crate::refresh::tests) fn critical_staging_generation(
    session: &mut HlsSession,
    now_ms: u64,
) -> super::super::switch_staging::HlsSwitchStagingGeneration {
    let burst_plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    session.origin_control.begin_acceptance_episode(
        now_ms,
        burst_plan,
        HlsManifestAcceptanceTrigger::Critical,
        &test_acceptance_episode_timing(now_ms, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("critical acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let identity = super::super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        Some("candidate.example.com"),
        "test-candidate",
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    assert_eq!(
        episode.bind_selected_candidate(
            episode.generation,
            identity,
            super::super::super::recovery_timing::HlsRecoveryWorkload::clear_fetch(),
        ),
        super::super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    super::super::switch_staging::switch_staging_generation(session).expect("critical staging generation")
}

#[tokio::test]
async fn critical_handoff_lock_contention_exhaustion_is_typed_and_bounded() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_access = Arc::clone(&attempts);

    let result: Result<(), HlsManifestCommitError> = super::super::retry_critical_handoff_state_access(
        crate::manifest_acceptance::HlsManifestAcceptanceGeneration(8),
        || {
            attempts_for_access.fetch_add(1, Ordering::SeqCst);
            async { crate::HlsCriticalHandoffStateAccess::LockBusy }
        },
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), crate::critical_handoff::HLS_CRITICAL_HANDOFF_COMMIT_RETRIES);
    let Err(HlsManifestCommitError::TimelineRejected { reason }) = result else {
        panic!("exhausted lock contention must return a typed timeline rejection");
    };
    assert_eq!(reason, HlsManifestRejectLogReason::CriticalHandoffLockContentionExhausted);
    assert_eq!(reason.status_label(), "critical-handoff-lock-contention-exhausted");
    assert_ne!(reason, HlsManifestRejectLogReason::SwitchResourceUnavailable);
    assert_ne!(reason, HlsManifestRejectLogReason::StagedSwitchInvalidated);
    assert_ne!(reason, HlsManifestRejectLogReason::PinnedHostRecoveryRejected);
}

#[tokio::test]
async fn critical_handoff_lock_busy_retry_retains_staged_state_until_acquired() {
    struct StagedRetentionProbe {
        identity: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for StagedRetentionProbe {
        fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let staged = StagedRetentionProbe { identity: 41, drops: Arc::clone(&drops) };
    let staged_address = std::ptr::from_ref(&staged);
    let attempts_for_access = Arc::clone(&attempts);

    let committed = super::super::retry_critical_handoff_state_access(
        crate::manifest_acceptance::HlsManifestAcceptanceGeneration(9),
        || {
            let attempt = attempts_for_access.fetch_add(1, Ordering::SeqCst);
            let staged = &staged;
            let drops_for_attempt = Arc::clone(&drops);
            async move {
                assert_eq!(std::ptr::from_ref(staged), staged_address);
                assert_eq!(drops_for_attempt.load(Ordering::SeqCst), 0);
                if attempt == 0 {
                    crate::HlsCriticalHandoffStateAccess::LockBusy
                } else {
                    crate::HlsCriticalHandoffStateAccess::Acquired(Ok(staged.identity))
                }
            }
        },
    )
    .await
    .expect("the retained staged state commits after transient contention");

    assert_eq!(committed, 41);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(staged);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn critical_handoff_selects_endangered_lease_base_instead_of_session_tail() {
    let temp_dir = tempfile::tempdir().expect("critical selection cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut first_window = complete.clone();
    first_window.snapshot_generation = 1;
    first_window.visible_segments = Arc::from([complete.visible_segments[0].clone()]);
    first_window.last_proxy_seq = first_window.first_proxy_seq;
    first_window.playlist_duration_ms = 4_000;
    first_window.last_visible_media_end_ms = 4_000;
    let mut endangered_window = complete.clone();
    endangered_window.snapshot_generation = 2;
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let leases = vec![
        active_critical_test_lease("more-reserve", &proxy_session_id, now_ms, now_ms, first_window),
        active_critical_test_lease(
            "endangered",
            &proxy_session_id,
            now_ms.saturating_add(1),
            now_ms,
            endangered_window,
        ),
    ];
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (selected, snapshot) =
        super::super::switch_staging::select_critical_handoff_lease(&session, &leases, &generation, now_ms)
            .expect("one lease is cutover critical");

    assert_eq!(selected.lease_id, HlsAccessLeaseId("endangered".to_string()));
    assert_eq!(snapshot.base.proxy_seq, complete.visible_segments[1].proxy_seq);
    assert_ne!(snapshot.base.proxy_seq, complete.visible_segments[2].proxy_seq);
}

#[tokio::test]
async fn critical_handoff_prioritizes_earliest_safe_cutover_deadline() {
    let temp_dir = tempfile::tempdir().expect("critical deadline cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut wider_margin = complete.clone();
    wider_margin.snapshot_generation = 3;
    wider_margin.target_duration_ms = 4_000;
    let mut exhausted_short_margin = complete.clone();
    exhausted_short_margin.snapshot_generation = 4;
    exhausted_short_margin.target_duration_ms = 1_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let mut wider_margin_lease =
        active_critical_test_lease("wider-margin", &proxy_session_id, now_ms, now_ms, wider_margin);
    let playback_at_ms = now_ms.saturating_sub(3_000);
    let token = wider_margin_lease.playback_cursor.record_request_started(complete.last_proxy_seq, playback_at_ms);
    let _ = wider_margin_lease.playback_cursor.record_request_completed(token, playback_at_ms);
    let leases = vec![
        wider_margin_lease,
        active_critical_test_lease(
            "exhausted-short-margin",
            &proxy_session_id,
            now_ms.saturating_add(1),
            now_ms,
            exhausted_short_margin,
        ),
    ];
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (selected, snapshot) =
        super::super::switch_staging::select_critical_handoff_lease(&session, &leases, &generation, now_ms)
            .expect("critical leases have a deterministic deadline order");

    assert_eq!(selected.lease_id, HlsAccessLeaseId("wider-margin".to_string()));
    assert_eq!(snapshot.base.proxy_seq, complete.last_proxy_seq);
}

#[tokio::test]
async fn critical_handoff_manifest_supersession_invalidates_frozen_snapshot() {
    let temp_dir = tempfile::tempdir().expect("critical supersession cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut endangered_window = complete.clone();
    endangered_window.snapshot_generation = 7;
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let mut lease = active_critical_test_lease("endangered", &proxy_session_id, now_ms, now_ms, endangered_window);
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (_, frozen) =
        super::super::switch_staging::select_critical_handoff_lease(&session, &[lease.clone()], &generation, now_ms)
            .expect("initial critical snapshot");
    let mut leases = HlsAccessLeaseStore::default();
    leases.prepare_access_lease(lease.clone());
    assert!(super::super::switch_staging::critical_handoff_snapshot_is_current(
        &mut leases,
        &session,
        &generation,
        &frozen,
        now_ms,
    ));
    if let Some(manifest) = lease.last_manifest_snapshot.as_mut() {
        manifest.snapshot_generation = manifest.snapshot_generation.saturating_add(1);
    }
    leases.prepare_access_lease(lease);

    assert!(!super::super::switch_staging::critical_handoff_snapshot_is_current(
        &mut leases,
        &session,
        &generation,
        &frozen,
        now_ms,
    ));
}

pub(in crate::refresh::tests) async fn install_active_critical_test_lease(
    hls_proxy: &HlsProxyManager,
    lease_id: &str,
    proxy_session_id: &super::super::super::ProxySessionId,
    manifest: HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new(lease_id.0.as_str(), lease_id.0.as_str()),
            proxy_session_id.clone(),
            lease_id.0.clone(),
            format!("{}-session", lease_id.0),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            120_000,
        ))
        .await;
    assert!(hls_proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 120_000 },
        )
        .await
        .is_activated());
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, proxy_session_id, now_ms)
        .await
        .expect("critical test lease publication guard");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(&lease_id, proxy_session_id, publication_guard, manifest, now_ms,)
        .await
        .is_committed());
    lease_id
}

#[test]
fn critical_handoff_terminal_response_revision_must_remain_current() {
    let app_config = critical_handoff_app_config();
    let frozen = app_config.custom_stream_response.load_full();
    assert!(super::super::critical_handoff_terminal_response_is_current(&app_config, frozen.as_ref()));
    let replacement = frozen.as_ref().map(|response| Arc::new(response.as_ref().clone()));
    app_config.custom_stream_response.store(replacement);

    assert!(!super::super::critical_handoff_terminal_response_is_current(&app_config, frozen.as_ref()));
}

#[test]
fn critical_handoff_and_terminal_tail_share_ts_inspector_signature() {
    let terminal_buffer = TransportStreamBuffer::new(CRITICAL_HANDOFF_TS_BODY.to_vec());
    let terminal_asset = super::super::snapshot_terminal_media_asset(&terminal_buffer).expect("terminal asset");
    let critical_signature = match tuliprox_mpegts::ts_inspector::inspect_mpeg_ts(
        std::io::Cursor::new(CRITICAL_HANDOFF_TS_BODY),
        tuliprox_mpegts::ts_inspector::HlsTsProbeProtection::Clear,
        tuliprox_mpegts::ts_inspector::HlsTsProbeBudget::default(),
    )
    .expect("critical handoff fixture inspection succeeds")
    {
        tuliprox_mpegts::ts_inspector::HlsTsProbeOutcome::Found(signature) => signature,
        outcome => panic!("critical handoff fixture has no MPEG-TS tracks: {outcome:?}"),
    };

    assert_eq!(critical_signature, terminal_asset.track_signature().clone());
}

#[tokio::test]
async fn critical_handoff_uses_endangered_lease_base_when_session_tail_is_incompatible() {
    let origin = spawn_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("lease-specific critical handoff cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    let now_ms = super::super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut noncritical_window = complete.clone();
    noncritical_window.visible_segments = Arc::from([complete.visible_segments[0].clone()]);
    noncritical_window.last_proxy_seq = noncritical_window.first_proxy_seq;
    noncritical_window.playlist_duration_ms = 4_000;
    noncritical_window.last_visible_media_end_ms = 4_000;
    let mut endangered_window = complete.clone();
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let (tail_key, lease_base_key, baseline_epoch) = {
        let session = session.read().await;
        (
            session.segments.get(&complete.last_proxy_seq).expect("session-wide tail").cache_key.clone(),
            session.segments.get(&endangered_window.last_proxy_seq).expect("endangered lease base").cache_key.clone(),
            session.origin_epoch,
        )
    };
    cache.delete(&tail_key).await.expect("replace session-wide tail fixture");
    let invalid_tail = vec![0_u8; 188 * 2];
    cache.write_bytes_and_commit(&tail_key, &invalid_tail).await.expect("incompatible session-wide tail commits");
    {
        let mut session = session.write().await;
        let tail = session.segments.get_mut(&complete.last_proxy_seq).expect("session-wide tail entry");
        tail.status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(invalid_tail.len()).unwrap_or(u64::MAX),
            ready_at_ms: now_ms,
        };
    }
    assert!(super::super::switch_staging::inspect_cache_object_tracks(&cache, &tail_key).await.signature().is_none());
    assert!(super::super::switch_staging::inspect_cache_object_tracks(&cache, &lease_base_key)
        .await
        .signature()
        .is_some());

    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(cache.cache_path(), 300));
    let _noncritical_lease = install_active_critical_test_lease(
        &hls_proxy,
        "noncritical-window",
        &proxy_session_id,
        noncritical_window,
        now_ms,
    )
    .await;
    let endangered_lease = install_active_critical_test_lease(
        &hls_proxy,
        "endangered-window",
        &proxy_session_id,
        endangered_window,
        now_ms,
    )
    .await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.hls_proxy = hls_proxy;
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(endangered_lease);
    request.now_ms = now_ms;
    let plan = request.manifest_recovery_burst.level.plan();
    let target_url = request.origin_entry.url().clone();
    let fetch_context = manifest_fetch_context(&request);

    let committed = retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Critical,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await
    .expect("lease-specific compatible base permits critical handoff");

    assert_eq!(committed.fetched.body.as_bytes(), CRITICAL_HANDOFF_MANIFEST_BODY);
    assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), plan.total_candidates());
    assert_eq!(origin.segment_requests.load(Ordering::SeqCst), 1);
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
    assert!(session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .is_some_and(|segment| segment.discontinuity_before));
}

#[tokio::test]
async fn critical_handoff_manifest_supersession_during_candidate_io_rolls_back_staging() {
    let origin = spawn_controlled_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("critical supersession cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let now_ms = super::super::current_time_millis();
    let (hls_proxy, lease_id, live_lease) = prepare_active_critical_handoff_lease(&session, &cache, now_ms).await;
    let proxy_session_id = live_lease.proxy_session_id.clone();
    let base_proxy_seq = live_lease.last_manifest_snapshot.as_ref().expect("critical manifest snapshot").last_proxy_seq;
    let (baseline_epoch, baseline_proxy_next_seq, baseline_readiness_generation, baseline_segments, base_access) = {
        let session = session.read().await;
        (
            session.origin_epoch,
            session.proxy_next_seq,
            session.activity.media_readiness_generation,
            session
                .segments
                .iter()
                .map(|(proxy_seq, segment)| (*proxy_seq, segment.cache_key.clone()))
                .collect::<Vec<_>>(),
            Arc::clone(&session.segments.get(&base_proxy_seq).expect("critical lease base segment").access),
        )
    };
    let candidate_cache_key = super::super::super::SegmentCacheKey::new(
        proxy_session_id.clone(),
        baseline_proxy_next_seq.expect("cross-host preview sequence"),
        "ts",
    );
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.app_config = critical_handoff_app_config();
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(lease_id.clone());
    request.now_ms = now_ms;
    let request = Arc::new(request);
    let task_request = Arc::clone(&request);
    let target_url = task_request.origin_entry.url().clone();
    let commit_task = tokio::spawn(async move {
        let fetch_context = manifest_fetch_context(&task_request);
        retry_hls_origin_manifest_recovery_chain(
            &fetch_context,
            test_manifest_origin_binding(target_url),
            Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
            None,
            HlsManifestAcceptanceTrigger::Critical,
            HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            |fetched, acceptance_mode| {
                super::super::commit_manifest_recovery_candidate(&task_request, fetched, acceptance_mode)
            },
        )
        .await
    });

    origin.segment_prefix_written.notified().await;
    assert!(base_access.active_readers() > 0, "lease base is pinned before candidate media completes");
    let publication_now_ms = super::super::current_time_millis();
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, publication_now_ms)
        .await
        .expect("superseding manifest publication guard");
    let superseding_snapshot = live_lease.last_manifest_snapshot.clone().expect("superseding snapshot");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &proxy_session_id,
            publication_guard,
            superseding_snapshot,
            publication_now_ms,
        )
        .await
        .is_committed());
    origin.release_segment_body.notify_one();

    let result = commit_task.await.expect("critical handoff task joins");
    assert!(result.is_err(), "superseded lease snapshot cannot commit");
    assert_eq!(base_access.active_readers(), 0, "rollback releases the frozen base pin");
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next_seq);
        assert_eq!(session.activity.media_readiness_generation, baseline_readiness_generation);
        assert_eq!(
            session
                .segments
                .iter()
                .map(|(proxy_seq, segment)| (*proxy_seq, segment.cache_key.clone()))
                .collect::<Vec<_>>(),
            baseline_segments
        );
    }
    assert!(
        cache.metadata(&candidate_cache_key).await.expect("candidate cache metadata reads").is_none(),
        "final CAS rejection removes the staged candidate object"
    );
}

#[tokio::test]
async fn critical_handoff_base_evidence_survives_gc_until_preparation_drop() {
    let temp_dir = tempfile::tempdir().expect("critical evidence GC tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let sessions = Arc::new(HlsSessionStore::new());
    let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    prepare_cross_host_baseline(&session).await;
    let manifest = commit_ready_baseline_snapshot(&session, &cache, 0).await;
    let base_proxy_seq = manifest.last_proxy_seq;
    let (base_key, base_access) = {
        let session = session.read().await;
        let base = session.segments.get(&base_proxy_seq).expect("lease-specific terminal base");
        (base.cache_key.clone(), Arc::clone(&base.access))
    };
    let gc = HlsGarbageCollector::new(
        Arc::clone(&sessions),
        Arc::clone(&cache),
        GarbageCollectionPolicy {
            cache_duration_ms: 0,
            cache_bytes_global: 10_000_000,
            cache_bytes_per_session: 10_000_000,
            session_idle_timeout_ms: u64::MAX,
            temp_file_retention_ms: 30_000,
            failed_segment_retention_ms: 10,
        },
        build_rewrite_secret_fingerprint(b"secret"),
    );

    let evidence = super::super::super::prepare_terminal_base_evidence(&session, &cache, &manifest, 5).await;
    assert_eq!(evidence.track_base().map(|base| base.proxy_seq), Some(base_proxy_seq));
    assert!(evidence.track_signature().is_some());
    assert!(base_access.active_readers() > 0);
    let pinned_report = gc.run_once(10_000).await.expect("GC runs while critical base evidence is pinned");
    assert_eq!(pinned_report.segments_deleted_duration, 0);
    assert!(cache.metadata(&base_key).await.expect("pinned base metadata reads").is_some());

    evidence.release();
    assert_eq!(base_access.active_readers(), 0);
    let released_report = gc.run_once(10_001).await.expect("GC runs after critical base evidence release");
    assert!(released_report.segments_deleted_duration > 0);
    assert!(cache.metadata(&base_key).await.expect("released base metadata reads").is_none());
}

#[tokio::test]
async fn mapped_fmp4_tail_cannot_handoff_to_mapless_ts_timeline() {
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n\
        #EXTINF:4.0,\nold1.m4s\n#EXTINF:4.0,\nold2.m4s\n#EXTINF:4.0,\nold3.m4s\n";
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nnew40.ts\n#EXTINF:4.0,\nnew41.ts\n#EXTINF:4.0,\nnew42.ts\n";

    assert_incompatible_switch_is_rejected_before_timeline_commit(
        baseline,
        candidate,
        HlsManifestRejectLogReason::SwitchMapResetUnsupported,
    )
    .await;
}
