use super::*;
use crate::short_manifest::HlsTargetDurationQualificationResult as Outcome;
use shared::model::{HlsCacheConfigDto, HlsManifestRecoveryBurstLevel, Secs};
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tuliprox_core::utils::current_time_millis;

fn playlist(head: usize, duration: Option<u32>) -> String {
    let target = duration.map_or_else(String::new, |duration| format!("#EXT-X-TARGETDURATION:{duration}\n"));
    let mut body = format!("#EXTM3U\n{target}#EXT-X-MEDIA-SEQUENCE:{head}\n");
    for offset in 0..6 {
        let extinf = if offset == 0 { "0.640" } else { "1.920" };
        write!(body, "#EXTINF:{extinf},\n/segment-{}.ts\n", head + offset).unwrap();
    }
    body
}

fn request(server: &TestOriginServer, level: HlsManifestRecoveryBurstLevel) -> OriginRefreshRequest {
    let session = Arc::new(tokio::sync::RwLock::new(crate::HlsSession::new(
        crate::HlsSessionKey::new(1, format!("duration-test-{}", server.base_url).as_str()),
        b"secret",
        current_time_millis(),
    )));
    let mut request = test_origin_refresh_request(session);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).unwrap();
    request.manifest_recovery_burst.level = level;
    request.hls_proxy.update_min_origin_target_duration_secs(Secs::new(4));
    let mut config = request.app_config.config.load_full().as_ref().clone();
    config.reverse_proxy =
        Some(tuliprox_core::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto {
            hls_cache: Some(request.hls_proxy.keep_cache_path(HlsCacheConfigDto {
                min_origin_target_duration_secs: Secs::new(4),
                ..Default::default()
            })),
            ..Default::default()
        }));
    request.app_config.config.store(Arc::new(config));
    request.now_ms = current_time_millis();
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: crate::HlsFreshManifestRequiredReason::ColdStart };
    request
}

async fn result(request: &OriginRefreshRequest) -> (Secs, Option<Outcome>, bool) {
    let session = request.session.read().await;
    let check = session.origin_control.target_duration_qualification.as_ref().unwrap();
    (check.minimum, check.result, check.warned)
}

#[tokio::test]
async fn default_zero_disables_duration_search_for_short_target_duration() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, Some(2))))).await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Beast);
    request.hls_proxy = Arc::new(crate::HlsProxyManager::new());
    request.app_config = test_app_config();
    assert_eq!(request.hls_proxy.min_origin_target_duration_secs(), Secs::new(0));
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 1);
    assert_eq!(result(&request).await, (Secs::new(0), Some(Outcome::SuitableOriginSelected), false));
    assert_eq!(request.session.read().await.target_duration, Some(2));
}

#[tokio::test]
async fn strict_threshold_uses_target_duration_not_extinf_and_preserves_fallback() {
    for duration in [2, 3, 4, 6] {
        let server = spawn_test_origin(Arc::new(move |_| (200, Vec::new(), playlist(100, Some(duration))))).await;
        let request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        let short = duration < 4;
        assert_eq!(server.requests.lock().await.len(), if short { 3 } else { 1 }, "TD={duration}");
        assert_eq!(
            result(&request).await,
            (
                Secs::new(4),
                Some(if short { Outcome::NoSuitableOrigin } else { Outcome::SuitableOriginSelected }),
                short
            )
        );
        let session = request.session.read().await;
        assert_eq!(session.origin_refresh.consecutive_failures, 0);
        assert_eq!(session.segments.len(), 6);
        assert!(matches!(session.mode, crate::HlsSessionMode::NormalCacheTimeline));
        assert!(!session.allows_short_origin_windows());
    }
}

#[tokio::test]
async fn suitable_six_segment_candidate_is_selected_before_first_publication() {
    super::short_manifest::init_qualification_logger();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let server = spawn_test_origin(Arc::new(move |_| {
        let call = c.fetch_add(1, Ordering::SeqCst);
        (200, Vec::new(), playlist(if call == 0 { 100 } else { 200 }, Some(if call == 0 { 2 } else { 6 })))
    }))
    .await;
    let request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert_eq!(session.target_duration, Some(6));
    assert!(session.segments.values().all(|entry| entry.origin_key.host_local_sequence >= 200));
    assert_eq!(session.origin_manifest_commits(), 1);
    assert_eq!(server.requests.lock().await.len(), 3);
    let proxy_session = crate::safe_proxy_session_id(&session.proxy_session_id);
    drop(session);
    assert_eq!(result(&request).await.1, Some(Outcome::SuitableOriginSelected));
    let completion = super::short_manifest::refresh_completion_message(&proxy_session).unwrap();
    assert!(completion.contains("recovery_attempts=1 candidate_requests=3 selection=burst"), "{completion}");
    assert_eq!(request.segment_worker_pool.metrics().snapshot().refresh_retried, 0);
}

#[tokio::test]
async fn duration_only_search_can_fall_back_to_original_after_transport_failures() {
    super::short_manifest::init_qualification_logger();
    for (level, recovery_status) in [
        (HlsManifestRecoveryBurstLevel::Off, 500),
        (HlsManifestRecoveryBurstLevel::Friendly, 500),
        (HlsManifestRecoveryBurstLevel::Off, 404),
        (HlsManifestRecoveryBurstLevel::Friendly, 404),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let server = spawn_test_origin(Arc::new(move |_| {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                (200, Vec::new(), playlist(100, Some(2)))
            } else {
                (recovery_status, Vec::new(), String::new())
            }
        }))
        .await;
        let request = request(&server, level);
        let metrics = Arc::clone(request.segment_worker_pool.metrics());
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        let session = request.session.read().await;
        if recovery_status == 404 {
            assert_eq!(session.origin_manifest_commits(), 0, "hard qualification failure must not fall back");
            assert_eq!(session.origin_control.path_condition, crate::HlsOriginPathCondition::HardFetchFailure);
            assert_eq!(metrics.snapshot().refresh_failed, 1);
            continue;
        }
        assert_eq!(session.origin_manifest_commits(), 1);
        assert_eq!(session.origin_refresh.consecutive_failures, 0);
        assert!(session.origin_refresh.last_error_at_ms.is_none());
        assert!(matches!(session.mode, crate::HlsSessionMode::NormalCacheTimeline));
        let proxy_session = crate::safe_proxy_session_id(&session.proxy_session_id);
        drop(session);
        assert_eq!(result(&request).await.1, Some(Outcome::NoSuitableOrigin));
        let attempts = request.retry_policy.attempt_count().saturating_sub(1);
        let expected_requests = 1 + level.total_candidates() + attempts - 1;
        assert_eq!(server.requests.lock().await.len(), expected_requests);
        let completion = super::short_manifest::refresh_completion_message(&proxy_session).unwrap();
        let selection = if attempts == 1 && level.total_candidates() > 1 { "burst" } else { "recovery" };
        assert!(
            completion.contains(&format!(
                "recovery_attempts={attempts} candidate_requests={expected_requests} selection={selection}"
            )),
            "{completion}"
        );
        let metrics = metrics.snapshot();
        assert_eq!(metrics.refresh_completed, 1);
        assert_eq!(metrics.refresh_failed, 0);
        assert_eq!(metrics.refresh_retried, u64::try_from(attempts - 1).unwrap());
    }
}

#[tokio::test]
async fn all_leases_share_one_search_and_normal_polls_keep_receipt_until_revalidation() {
    super::short_manifest::init_qualification_logger();
    let head = Arc::new(AtomicUsize::new(100));
    let h = Arc::clone(&head);
    let server =
        spawn_test_origin(Arc::new(move |_| (200, Vec::new(), playlist(h.load(Ordering::SeqCst), Some(2))))).await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
    let mut tasks = tokio::task::JoinSet::new();
    for lease in 0..20 {
        let mut request = request.clone();
        request.access_lease_id = Some(crate::HlsAccessLeaseId(format!("duration-lease-{lease}")));
        tasks.spawn(trigger_origin_refresh_sync(request));
    }
    let mut started = 0;
    while let Some(result) = tasks.join_next().await {
        started += usize::from(result.unwrap());
    }
    assert_eq!(started, 1);
    assert_eq!(server.requests.lock().await.len(), 3);
    let generation = request.session.read().await.origin_control.acceptance_generation;
    request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
    for cycle in 1..=10 {
        head.store(100 + cycle, Ordering::SeqCst);
        request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
        request.now_ms = current_time_millis();
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        assert_eq!(request.session.read().await.origin_control.acceptance_generation, generation);
    }
    assert_eq!(server.requests.lock().await.len(), 13);
    let proxy_session = crate::safe_proxy_session_id(&request.session.read().await.proxy_session_id);
    let messages = super::short_manifest::qualification_messages(&proxy_session);
    assert_eq!(messages.iter().filter(|line| line.starts_with("HLS origin target duration below minimum:")).count(), 1);
    assert!(messages.iter().any(|line| line.contains("result=no_suitable_origin")));
    assert!(messages.iter().all(|line| !line.contains("/live/user/pass/") && !line.contains(".m3u8")));
    publish_ready_test_manifest(&request.session, request.now_ms).await;
    assert!(request.session.read().await.established_manifest_recovery_binding().is_some());
    request
        .session
        .write()
        .await
        .require_fresh_manifest_commit(crate::HlsFreshManifestRequiredReason::ExpiredRevalidation);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: crate::HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    head.store(120, Ordering::SeqCst);
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 15, "existing lifecycle burst satisfies duration search");
    let messages = super::short_manifest::qualification_messages(&proxy_session);
    assert_eq!(messages.iter().filter(|line| line.starts_with("HLS origin target duration below minimum:")).count(), 2);
    assert!(request.session.read().await.origin_control.acceptance_generation.0 > generation.0);
}

#[tokio::test]
async fn configured_off_and_beast_keep_their_exact_first_burst_sizes() {
    for level in [HlsManifestRecoveryBurstLevel::Off, HlsManifestRecoveryBurstLevel::Beast] {
        let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, Some(2))))).await;
        let request = request(&server, level);
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        assert_eq!(server.requests.lock().await.len(), 1 + level.total_candidates());
    }
}

#[tokio::test]
async fn policy_snapshot_survives_config_update_until_next_real_qualification() {
    let head = Arc::new(AtomicUsize::new(100));
    let h = Arc::clone(&head);
    let server =
        spawn_test_origin(Arc::new(move |_| (200, Vec::new(), playlist(h.load(Ordering::SeqCst), Some(4))))).await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
    // Freeze the same policy snapshot that singleflight takes before doing I/O.
    super::super::short_manifest::prepare_target_duration_qualification(&request, &mut *request.session.write().await);
    request.hls_proxy.update_min_origin_target_duration_secs(Secs::new(6));
    assert_eq!(request.hls_proxy.min_origin_target_duration_secs(), Secs::new(6));
    assert_eq!(result(&request).await.0, Secs::new(4));
    let mut config = request.app_config.config.load_full().as_ref().clone();
    let hls =
        tuliprox_core::model::HlsCacheConfig::from(&request.hls_proxy.keep_cache_path(HlsCacheConfigDto {
            min_origin_target_duration_secs: Secs::new(6),
            ..Default::default()
        }));
    config.reverse_proxy = Some(tuliprox_core::model::ReverseProxyConfig {
        hls_cache: Some(hls),
        ..config.reverse_proxy.unwrap_or_else(|| {
            tuliprox_core::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto::default())
        })
    });
    request.app_config.config.store(Arc::new(config));
    request.hls_proxy.update_config(&request.app_config).await;
    assert_eq!(request.hls_proxy.min_origin_target_duration_secs(), Secs::new(6));
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(result(&request).await.0, Secs::new(4));
    assert_eq!(server.requests.lock().await.len(), 1);
    publish_ready_test_manifest(&request.session, request.now_ms).await;
    assert!(request.session.read().await.established_manifest_recovery_binding().is_some());
    request
        .session
        .write()
        .await
        .require_fresh_manifest_commit(crate::HlsFreshManifestRequiredReason::ExpiredRevalidation);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: crate::HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    head.store(120, Ordering::SeqCst);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(result(&request).await, (Secs::new(6), Some(Outcome::NoSuitableOrigin), true));
    assert_eq!(server.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn missing_invalid_and_full_manifest_durations_keep_existing_handling() {
    for body in [
        playlist(100, None),
        playlist(100, Some(0)),
        playlist(100, Some(2)).replace("DURATION:2", "DURATION:invalid"),
        format!("{}#EXT-X-ENDLIST\n", playlist(100, Some(2))),
    ] {
        let server = spawn_test_origin(Arc::new(move |_| (200, Vec::new(), body.clone()))).await;
        let request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        assert_eq!(server.requests.lock().await.len(), 1);
        assert!(!result(&request).await.2);
    }
}

#[tokio::test]
async fn archive_playback_does_not_start_duration_qualification() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, Some(2))))).await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Beast);
    request.session = Arc::new(tokio::sync::RwLock::new(crate::HlsSession::new(
        crate::HlsSessionKey::new(1, "duration-archive").with_archive_reference(1_784_898_000),
        b"secret",
        current_time_millis(),
    )));
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 1);
    assert!(request.session.read().await.origin_control.target_duration_qualification.is_none());
}

fn anchored_playlist(head: usize, duration: u32) -> String {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:{duration}\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
    for offset in 0..6 {
        write!(
            body,
            "#EXT-X-PROGRAM-DATE-TIME:2026-10-20T12:00:{:02}.000Z\n#EXTINF:1.920,\n/segment-{}.ts\n",
            offset * 2,
            head + offset
        )
        .unwrap();
    }
    body
}

#[tokio::test]
async fn preferred_host_staging_failure_reuses_same_burst_for_valid_pinned_fallback() {
    let alternative = spawn_test_origin(Arc::new(|path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), anchored_playlist(101, 6))
        } else {
            (503, Vec::new(), String::new())
        }
    }))
    .await;
    let alt_url = format!("{}/live/user/pass/12345.m3u8", alternative.base_url.replace("127.0.0.1", "localhost"));
    let recovering = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&recovering);
    let server = spawn_test_origin(Arc::new(move |_| {
        let call = flag.load(Ordering::SeqCst);
        if call > 0 && flag.fetch_add(1, Ordering::SeqCst) == 1 {
            (302, vec![("Location", alt_url.clone())], String::new())
        } else {
            (200, Vec::new(), anchored_playlist(if call == 0 { 100 } else { 101 }, 2))
        }
    }))
    .await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    recovering.store(1, Ordering::SeqCst);
    publish_ready_test_manifest(&request.session, request.now_ms).await;
    assert!(request.session.read().await.established_manifest_recovery_binding().is_some());
    request
        .session
        .write()
        .await
        .require_fresh_manifest_commit(crate::HlsFreshManifestRequiredReason::ExpiredRevalidation);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: crate::HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert_eq!(session.target_duration, Some(2));
    assert_eq!(session.origin_refresh.consecutive_failures, 0);
    assert_eq!(server.requests.lock().await.len(), 5, "one startup burst and one revalidation burst");
    assert!(
        alternative.requests.lock().await.iter().any(|path| path_has_extension(path, "ts")),
        "preferred host was actually staged"
    );
    assert_eq!(session.origin_control.pinned_host.as_deref(), Some("127.0.0.1"));
    drop(session);
    assert_eq!(result(&request).await.1, Some(Outcome::NoSuitableOrigin));
}

#[tokio::test]
async fn existing_transient_mode_does_not_gain_a_duration_search_on_normal_looking_body() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, Some(2))))).await;
    let request = request(&server, HlsManifestRecoveryBurstLevel::Beast);
    request.session.write().await.mode = crate::HlsSessionMode::TransientPassthrough {
        reason: crate::TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() },
    };
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 1);
    assert!(request.session.read().await.origin_control.target_duration_qualification.is_none());
}

#[tokio::test]
async fn existing_recovery_after_a_completed_check_qualifies_duration_with_its_own_burst() {
    let head = Arc::new(AtomicUsize::new(100));
    let h = Arc::clone(&head);
    let server = spawn_test_origin(Arc::new(move |_| {
        let head = h.load(Ordering::SeqCst);
        (
            200,
            Vec::new(),
            playlist(head, Some(if head == 100 { 4 } else { 2 })).replace("#EXTINF:0.640", "#EXTINF:1.920"),
        )
    }))
    .await;
    let mut request = request(&server, HlsManifestRecoveryBurstLevel::Friendly);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    publish_ready_test_manifest(&request.session, request.now_ms).await;
    request.session.write().await.mark_authorized_media_access(request.now_ms);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;
    request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    request.now_ms = current_time_millis();
    head.store(101, Ordering::SeqCst);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 3, "the existing burst performs the duration check");
    assert_eq!(result(&request).await, (Secs::new(4), Some(Outcome::NoSuitableOrigin), true));
    {
        let session = request.session.read().await;
        let check = session.origin_control.target_duration_qualification.as_ref().unwrap();
        assert_eq!(check.basis, crate::short_manifest::HlsOriginQualificationBasis::capture(&session));
        assert!(session.origin_is_qualified(), "{:?}", session.origin_control.qualification);
    }
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::None;
    request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    request.now_ms = current_time_millis();
    head.store(102, Ordering::SeqCst);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(server.requests.lock().await.len(), 4, "the next poll does not spend a second burst");
}
