use super::*;
use crate::{
    short_manifest::HlsOriginQualification, HlsAccessLeaseId, HlsFreshManifestRequiredReason, HlsManifestRenderer,
    SegmentCacheStatus,
};
use shared::model::HlsManifestRecoveryBurstLevel;
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tuliprox_core::utils::current_time_millis;

fn playlist(head: u64, count: usize) -> String {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
    for offset in 0..count {
        writeln!(body, "#EXTINF:2.5,\nsegment-{}.ts", head + u64::try_from(offset).unwrap()).unwrap();
    }
    body
}

fn request_for_origin(server: &TestOriginServer) -> OriginRefreshRequest {
    let mut request = test_origin_refresh_request(test_session());
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).unwrap();
    request.manifest_recovery_burst.level = HlsManifestRecoveryBurstLevel::Friendly;
    request.now_ms = current_time_millis();
    request
}

async fn configure_first_ready(request: &OriginRefreshRequest) {
    let config = tuliprox_core::model::HlsStartupConfig {
        mode: shared::model::HlsStartupMode::FirstReady,
        ..Default::default()
    };
    request.session.write().await.startup = Some(crate::HlsSessionStartup {
        config: config.clone(),
        first_data_timeout_ms: 10_000,
        worker: Arc::downgrade(&request.segment_worker_pool),
        access_leases: Arc::new(tokio::sync::RwLock::new(crate::HlsAccessLeaseStore::default())),
        revisions: std::collections::BTreeMap::new(),
        policy_fixed: true,
        store: Arc::new(crate::SegmentRevisionStore::default()),
        budget: crate::ProgressiveBudgetManager::new(config),
    });
}

fn publish_first_ready_head(session: &mut crate::HlsSession, now_ms: u64) {
    let head = session.publishable_origin_head_proxy_seq.unwrap();
    let startup = session.startup.as_mut().unwrap();
    let revision =
        startup.store.create(session.proxy_session_id.clone(), head, crate::SegmentRevisionKind::Processed).unwrap();
    assert!(revision
        .revision()
        .complete(crate::CachedSegmentMetadata { path: "first-ready-test.ts".into(), size: 128 }));
    startup.revisions.insert(head, revision);
    session.segments.get_mut(&head).unwrap().status =
        SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: now_ms };
    session.render_and_store_manifest(now_ms).unwrap();
}

#[tokio::test]
async fn short_startup_one_segment_waits_for_complete_burst() { startup_with_short_window(1).await; }

#[tokio::test]
async fn short_startup_two_segments_waits_for_complete_burst() { startup_with_short_window(2).await; }

async fn startup_with_short_window(count: usize) {
    let server = spawn_test_origin(Arc::new(move |_| (200, Vec::new(), playlist(100, count)))).await;
    let mut request = request_for_origin(&server);
    request.strip.value = 10;
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let mut session = request.session.write().await;
    assert!(session.allows_short_origin_windows());
    assert_eq!(server.requests.lock().await.len(), 1 + request.manifest_recovery_burst.level.plan().total_candidates());
    assert_eq!(session.origin_control.qualification.generation(), Some(session.origin_control.acceptance_generation));
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: request.now_ms };
    }
    let rendered = session.render_and_store_manifest(request.now_ms).unwrap();
    assert_eq!(rendered.segment_proxy_seqs.len(), count);
    assert_eq!(rendered.playlist_duration_ms, u64::try_from(count).unwrap() * 2_500);
    assert_eq!(session.initial_prefetch_gap_segments, 0);
    assert!(!rendered.body.contains("#EXT-X-ENDLIST"));
}

#[tokio::test]
async fn short_qualification_survives_ten_refreshes_and_larger_intermediate_windows() {
    for short_count in [1, 2] {
        let head = Arc::new(AtomicUsize::new(100));
        let count = Arc::new(AtomicUsize::new(short_count));
        let (origin_head, origin_count) = (Arc::clone(&head), Arc::clone(&count));
        let server = spawn_test_origin(Arc::new(move |_| {
            (
                200,
                Vec::new(),
                playlist(
                    u64::try_from(origin_head.load(Ordering::SeqCst)).unwrap(),
                    origin_count.load(Ordering::SeqCst),
                ),
            )
        }))
        .await;
        let mut request = request_for_origin(&server);
        assert!(trigger_origin_refresh_sync(request.clone()).await);
        let generation = request.session.read().await.origin_control.acceptance_generation;
        let startup_requests = server.requests.lock().await.len();
        for cycle in 1..=10 {
            head.store(100 + cycle * 4, Ordering::SeqCst);
            count.store(if cycle % 2 == 1 { short_count + 2 } else { short_count }, Ordering::SeqCst);
            {
                let mut session = request.session.write().await;
                session.origin_refresh.next_fetch_allowed_at_ms = 0;
            }
            request.now_ms = current_time_millis();
            request.access_lease_id = Some(HlsAccessLeaseId(format!("new-lease-{cycle}")));
            request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
            };
            assert!(trigger_origin_refresh_sync(request.clone()).await);
            let mut session = request.session.write().await;
            assert_eq!(session.origin_control.acceptance_generation, generation);
            assert!(session.allows_short_origin_windows());
            for segment in session.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: request.now_ms };
            }
            let rendered = session.render_and_store_manifest(request.now_ms).unwrap();
            assert!(rendered.segment_proxy_seqs.len() >= count.load(Ordering::SeqCst));
            assert_eq!(server.requests.lock().await.len(), startup_requests + cycle);
        }
    }
}

async fn concurrent_refreshes(request: &OriginRefreshRequest, leases: usize) -> usize {
    let mut tasks = tokio::task::JoinSet::new();
    for lease in 0..leases {
        let mut request = request.clone();
        request.access_lease_id = Some(HlsAccessLeaseId(format!("lease-{lease}")));
        tasks.spawn(trigger_origin_refresh_sync(request));
    }
    let mut started = 0;
    while let Some(result) = tasks.join_next().await {
        started += usize::from(result.unwrap());
    }
    started
}

#[tokio::test]
async fn twenty_leases_share_startup_revalidation_and_normal_freshness_requests() {
    let mut request_counts = Vec::new();
    for leases in [1, 20] {
        let head = Arc::new(AtomicUsize::new(100));
        let origin_head = Arc::clone(&head);
        let server = spawn_test_origin(Arc::new(move |_| {
            (200, Vec::new(), playlist(u64::try_from(origin_head.load(Ordering::SeqCst)).unwrap(), 1))
        }))
        .await;
        let mut request = request_for_origin(&server);
        request.manifest_commit_requirement =
            HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
        assert_eq!(concurrent_refreshes(&request, leases).await, 1);
        let initial_count = server.requests.lock().await.len();
        let first_generation = request.session.read().await.origin_control.acceptance_generation;
        {
            let mut session = request.session.write().await;
            session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::ExpiredRevalidation);
        }
        head.store(101, Ordering::SeqCst);
        request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        };
        request.now_ms = current_time_millis();
        assert_eq!(concurrent_refreshes(&request, leases).await, 1);
        let session = request.session.read().await;
        assert!(session.allows_short_origin_windows(), "{:?} {:?}", session.origin_control, session.origin_refresh);
        assert!(session.origin_control.acceptance_generation.0 > first_generation.0);
        assert!(session.fresh_manifest_commit_required.is_none());
        drop(session);
        head.store(102, Ordering::SeqCst);
        request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
        request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
        };
        request.now_ms = current_time_millis();
        assert_eq!(concurrent_refreshes(&request, leases).await, 1);
        request_counts.push((initial_count, server.requests.lock().await.len()));
    }
    assert_eq!(request_counts[0], request_counts[1]);
}

#[tokio::test]
async fn short_failed_qualification_preserves_full_burst_receipt_and_backoff_for_new_leases() {
    let calls = Arc::new(AtomicUsize::new(0));
    let origin_calls = Arc::clone(&calls);
    let server = spawn_test_origin(Arc::new(move |_| {
        if origin_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            (200, Vec::new(), playlist(100, 1))
        } else {
            (500, Vec::new(), String::new())
        }
    }))
    .await;
    let mut request = request_for_origin(&server);
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let generation = request.session.read().await.origin_control.acceptance_generation;
    assert!(matches!(
        request.session.read().await.origin_control.qualification,
        HlsOriginQualification::RetryPending { .. }
    ));
    // The existing failure schedule allows an immediate first retry, then backs off.
    request.now_ms = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    let before_retry = calls.load(Ordering::SeqCst);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before_retry + 1,
        "failed qualification retries one sample, never a new full burst"
    );
    let failed_state = request.session.read().await.origin_refresh.clone();
    request.now_ms = failed_state.next_fetch_allowed_at_ms - 1;
    assert_eq!(concurrent_refreshes(&request, 20).await, 0);
    let session = request.session.read().await;
    assert_eq!(session.origin_refresh, failed_state);
    assert_eq!(session.origin_control.acceptance_generation, generation);
    assert!(!session.allows_short_origin_windows());
    assert!(session.segments.is_empty());
}

#[tokio::test]
async fn later_regular_candidate_wins_before_short_startup_is_published() {
    let calls = Arc::new(AtomicUsize::new(0));
    let origin_calls = Arc::clone(&calls);
    let server = spawn_test_origin(Arc::new(move |_| {
        let count = if origin_calls.fetch_add(1, Ordering::SeqCst) < 2 { 1 } else { 3 };
        (200, Vec::new(), playlist(100, count))
    }))
    .await;
    let request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert_eq!(session.segments.len(), 3);
    assert!(!session.allows_short_origin_windows());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn unqualified_short_window_stays_unrenderable_and_unchanged_refresh_does_not_advance_media() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let short = fetched_manifest(&playlist(100, 1));
    let mut session = session.write().await;
    commit_fetched_manifest(&mut session, &short, &request, 100).unwrap();
    assert!(!session.allows_short_origin_windows());
    assert!(HlsManifestRenderer::render(&session, 100).is_err());
    let progress = session.origin_control.progress_generation;
    commit_fetched_manifest(&mut session, &short, &request, 200).unwrap();
    assert_eq!(session.origin_control.progress_generation, progress);
    assert_eq!(session.origin_control.last_media_progress_at_ms, Some(100));
}

#[tokio::test]
async fn short_qualification_rejects_stale_epoch_binding_and_acceptance_generation_before_commit() {
    use crate::short_manifest::{HlsOriginQualification, HlsOriginQualificationBasis};
    for change in 0..3 {
        let handle = test_session();
        let request = test_origin_refresh_request(Arc::clone(&handle));
        let plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
        let mut session = handle.write().await;
        let generation = session.origin_control.begin_acceptance_episode(
            100,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &test_acceptance_episode_timing(100, plan),
        );
        let target = test_manifest_origin_binding(url::Url::parse("https://origin.example/live.m3u8").unwrap());
        session.origin_control.qualification = HlsOriginQualification::Checking {
            basis: HlsOriginQualificationBasis::capture(&session),
            generation,
            target: target.clone(),
        };
        session.origin_control.acceptance_episode.as_mut().unwrap().record_full_burst();
        match change {
            0 => session.origin_epoch += 1,
            1 => session.origin_control.manifest_origin_binding = Some(target),
            2 => session.origin_control.acceptance_generation.0 += 1,
            _ => unreachable!(),
        }
        let result = commit_fetched_manifest(&mut session, &fetched_manifest(&playlist(100, 1)), &request, 200);
        assert!(matches!(
            result,
            Err(HlsManifestCommitError::TimelineRejected {
                reason: HlsManifestRejectLogReason::StagedSwitchInvalidated,
            })
        ));
        assert!(!session.allows_short_origin_windows());
        assert!(session.segments.is_empty());
        assert_eq!(session.origin_manifest_commits(), 0);
    }
}

struct QualificationLogger(std::sync::Mutex<Vec<String>>);

impl log::Log for QualificationLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool { metadata.level() == log::Level::Warn }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            if message.starts_with("HLS short origin window qualified:") {
                self.0.lock().unwrap().push(message);
            }
        }
    }
    fn flush(&self) {}
}

#[tokio::test]
async fn short_window_warning_occurs_once_per_qualification_across_new_leases() {
    static LOGGER: QualificationLogger = QualificationLogger(std::sync::Mutex::new(Vec::new()));
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    let head = Arc::new(AtomicUsize::new(100));
    let origin_head = Arc::clone(&head);
    let server = spawn_test_origin(Arc::new(move |_| {
        (200, Vec::new(), playlist(u64::try_from(origin_head.load(Ordering::SeqCst)).unwrap(), 1))
    }))
    .await;
    let mut request = request_for_origin(&server);
    request.session = Arc::new(tokio::sync::RwLock::new(crate::HlsSession::new(
        crate::HlsSessionKey::new(1, "warning-once"),
        b"secret",
        current_time_millis(),
    )));
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    for lease in 0..10 {
        request.access_lease_id = Some(HlsAccessLeaseId(format!("warning-lease-{lease}")));
        request.now_ms = current_time_millis();
        request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
        assert!(trigger_origin_refresh_sync(request.clone()).await);
    }
    let proxy_session = crate::safe_proxy_session_id(&request.session.read().await.proxy_session_id);
    let warnings = || {
        LOGGER
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.contains(&format!("proxy_session={proxy_session}")))
            .cloned()
            .collect::<Vec<_>>()
    };
    let first = warnings();
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("RFC 8216 section 6.2.2"));
    assert!(first[0].contains(&format!("host={}", crate::hls_origin_log_value("127.0.0.1"))));
    assert!(first[0].contains("segments=1 qualification=1 burst=2"));
    head.store(101, Ordering::SeqCst);
    request.session.write().await.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::ExpiredRevalidation);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request).await);
    assert_eq!(warnings().len(), 2);
}

#[tokio::test]
async fn short_permission_does_not_release_empty_replayed_or_failed_media() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, 1)))).await;
    let request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let mut session = request.session.write().await;
    assert!(session.allows_short_origin_windows());
    let proxy_seq = session.publishable_origin_head_proxy_seq.unwrap();
    session.segments.get_mut(&proxy_seq).unwrap().status = SegmentCacheStatus::FailedPermanent {
        failed_at_ms: request.now_ms,
        status: Some(axum::http::StatusCode::NOT_FOUND),
    };
    assert!(HlsManifestRenderer::render(&session, request.now_ms).is_err());
    session.segments.get_mut(&proxy_seq).unwrap().status =
        SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: request.now_ms };
    session.render_and_store_manifest(request.now_ms).unwrap();
    let commits = session.origin_manifest_commits();
    let mut replay = fetched_manifest(&playlist(101, 1).replace("segment-101.ts", "segment-100.ts"));
    replay.final_manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    replay.resolved_request_url.clone_from(&replay.final_manifest_url);
    replay.redirect_host = None;
    assert!(matches!(
        commit_fetched_manifest(&mut session, &replay, &request, request.now_ms + 1),
        Err(HlsManifestCommitError::TimelineRejected {
            reason: HlsManifestRejectLogReason::PublishedResourceReplay { .. }
        })
    ));
    assert!(session.allows_short_origin_windows());
    assert_eq!(session.origin_manifest_commits(), commits);
    let mut empty = fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:102\n");
    empty.final_manifest_url = replay.final_manifest_url;
    empty.resolved_request_url = replay.resolved_request_url;
    empty.redirect_host = None;
    assert!(commit_fetched_manifest(&mut session, &empty, &request, request.now_ms + 2).is_err());
    assert_eq!(session.origin_manifest_commits(), commits);
}

#[tokio::test]
async fn expired_manifest_requires_new_qualification_without_an_explicit_fresh_requirement_flag() {
    let head = Arc::new(AtomicUsize::new(100));
    let origin_head = Arc::clone(&head);
    let server = spawn_test_origin(Arc::new(move |_| {
        (200, Vec::new(), playlist(u64::try_from(origin_head.load(Ordering::SeqCst)).unwrap(), 1))
    }))
    .await;
    let mut request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let generation = request.session.read().await.origin_control.acceptance_generation;
    {
        let mut session = request.session.write().await;
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: request.now_ms };
        }
        session.render_and_store_manifest(request.now_ms).unwrap();
        assert!(session.last_normal_manifest_commit_identity().is_some());
        session.last_rendered_manifest = None;
    }
    assert!(request.session.read().await.fresh_manifest_commit_required.is_none());
    head.store(101, Ordering::SeqCst);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert!(session.origin_control.acceptance_generation.0 > generation.0);
    assert!(session.allows_short_origin_windows());
    assert_eq!(server.requests.lock().await.len(), 6);
}

#[tokio::test]
async fn recovered_origin_requalifies_after_expired_episode_under_shared_backoff() {
    let phase = Arc::new(AtomicUsize::new(0));
    let origin_phase = Arc::clone(&phase);
    let server = spawn_test_origin(Arc::new(move |_| match origin_phase.load(Ordering::SeqCst) {
        0 => (200, Vec::new(), playlist(100, 1)),
        1 => (500, Vec::new(), String::new()),
        _ => (200, Vec::new(), playlist(101, 1)),
    }))
    .await;
    let mut request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    {
        let mut session = request.session.write().await;
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 128, ready_at_ms: request.now_ms };
        }
        session.render_and_store_manifest(request.now_ms).unwrap();
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::ExpiredRevalidation);
    }
    phase.store(1, Ordering::SeqCst);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let failed_generation = {
        let mut session = request.session.write().await;
        let previous = session.origin_control.acceptance_episode.take().unwrap();
        let mut expired = crate::manifest_acceptance::HlsManifestAcceptanceEpisode::new(
            previous.generation,
            1,
            previous.burst_plan,
            previous.trigger(),
            &test_acceptance_episode_timing(1, previous.burst_plan),
        );
        expired.record_full_burst();
        expired.state = previous.state;
        expired.observed_landscape = previous.observed_landscape;
        assert!(expired.timing().acceptance_deadline.as_millis_since_epoch() < current_time_millis());
        session.origin_control.acceptance_episode = Some(expired);
        assert_eq!(session.origin_seq_highwater, Some(100));
        session.origin_control.acceptance_generation
    };
    let requests_before_retry = server.requests.lock().await.len();
    request.now_ms = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    assert_eq!(server.requests.lock().await.len(), requests_before_retry + 1);
    assert_eq!(request.session.read().await.origin_control.acceptance_generation, failed_generation);
    let retry_at_ms = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    request.now_ms = retry_at_ms - 1;
    phase.store(2, Ordering::SeqCst);
    assert_eq!(concurrent_refreshes(&request, 20).await, 0);
    assert_eq!(server.requests.lock().await.len(), requests_before_retry + 1);
    request.now_ms = retry_at_ms;
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    let session = request.session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(101));
    assert_eq!(session.origin_control.acceptance_generation.0, failed_generation.0 + 1);
    assert!(session.allows_short_origin_windows());
    assert_eq!(session.origin_refresh.consecutive_failures, 0);
    assert!(session.fresh_manifest_commit_required.is_none());
    assert_eq!(server.requests.lock().await.len(), requests_before_retry + 4);
}

#[tokio::test]
async fn new_leases_reuse_origin_qualification_before_first_publication() {
    let server = spawn_test_origin(Arc::new(|_| (200, Vec::new(), playlist(100, 1)))).await;
    let mut request = request_for_origin(&server);
    configure_first_ready(&request).await;
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let generation = {
        let session = request.session.read().await;
        assert!(session.origin_is_qualified());
        assert!(session.last_rendered_manifest.is_none());
        assert!(session.last_normal_manifest_commit_identity().is_none());
        assert!(session.segments.values().all(|segment| !matches!(segment.status, SegmentCacheStatus::Ready { .. })));
        session.origin_control.acceptance_generation
    };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    assert_eq!(request.session.read().await.origin_control.acceptance_generation, generation);
    assert_eq!(server.requests.lock().await.len(), 4);
}

#[tokio::test]
async fn first_ready_revalidation_is_shared_until_its_publication_then_can_expire_again() {
    let head = Arc::new(AtomicUsize::new(100));
    let origin_head = Arc::clone(&head);
    let server = spawn_test_origin(Arc::new(move |_| {
        (200, Vec::new(), playlist(u64::try_from(origin_head.load(Ordering::SeqCst)).unwrap(), 1))
    }))
    .await;
    let mut request = request_for_origin(&server);
    configure_first_ready(&request).await;
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let previous_commit = {
        let mut session = request.session.write().await;
        publish_first_ready_head(&mut session, request.now_ms);
        session.last_rendered_manifest.as_mut().unwrap().valid_until_ms = 0;
        assert!(!crate::manifest_commit::has_servable_committed_manifest(&session, request.now_ms));
        session.last_normal_manifest_commit_identity().unwrap()
    };
    head.store(101, Ordering::SeqCst);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    {
        let session = request.session.read().await;
        assert_eq!(session.startup_mode(), shared::model::HlsStartupMode::FirstReady);
        assert_eq!(session.origin_control.acceptance_generation.0, 2);
        assert_eq!(session.origin_seq_highwater, Some(101));
        assert!(session.origin_is_qualified());
        assert_eq!(session.last_normal_manifest_commit_identity(), Some(previous_commit));
        assert!(!crate::manifest_commit::has_servable_committed_manifest(&session, request.now_ms));
    }
    assert_eq!(server.requests.lock().await.len(), 5);
    request.now_ms = current_time_millis();
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    {
        let mut session = request.session.write().await;
        assert_eq!(session.origin_control.acceptance_generation.0, 2);
        assert!(session.origin_is_qualified());
        assert_eq!(session.last_normal_manifest_commit_identity(), Some(previous_commit));
        assert_eq!(server.requests.lock().await.len(), 6);
        publish_first_ready_head(&mut session, request.now_ms);
        assert!(session.last_normal_manifest_commit_identity().unwrap() > previous_commit);
        assert!(crate::manifest_commit::has_servable_committed_manifest(&session, request.now_ms));
        session.last_rendered_manifest.as_mut().unwrap().valid_until_ms = 0;
    }
    head.store(102, Ordering::SeqCst);
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert_eq!(session.origin_control.acceptance_generation.0, 3);
    assert_eq!(session.origin_seq_highwater, Some(102));
    assert!(session.origin_is_qualified());
    assert_eq!(server.requests.lock().await.len(), 8);
}

#[tokio::test]
async fn genuine_lifecycle_revalidation_replaces_failed_startup_once_and_shares_its_failure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let origin_calls = Arc::clone(&calls);
    let server = spawn_test_origin(Arc::new(move |_| {
        if origin_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            (200, Vec::new(), playlist(100, 1))
        } else {
            (500, Vec::new(), String::new())
        }
    }))
    .await;
    let mut request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let generation = request.session.read().await.origin_control.acceptance_generation;
    request
        .session
        .write()
        .await
        .require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    };
    request.now_ms = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    let revalidated = request.session.read().await.origin_control.acceptance_generation;
    assert!(revalidated.0 > generation.0);
    let requests_after_revalidation = calls.load(Ordering::SeqCst);
    request.now_ms = request.session.read().await.origin_refresh.next_fetch_allowed_at_ms;
    assert_eq!(concurrent_refreshes(&request, 20).await, 1);
    assert_eq!(calls.load(Ordering::SeqCst), requests_after_revalidation + 1);
    assert_eq!(request.session.read().await.origin_control.acceptance_generation, revalidated);
}

#[tokio::test]
async fn segment_count_change_preserves_regular_qualification_without_starting_a_new_burst() {
    let count = Arc::new(AtomicUsize::new(3));
    let origin_count = Arc::clone(&count);
    let server = spawn_test_origin(Arc::new(move |_| {
        let count = origin_count.load(Ordering::SeqCst);
        (200, Vec::new(), playlist(if count == 3 { 100 } else { 104 }, count))
    }))
    .await;
    let mut request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let generation = request.session.read().await.origin_control.acceptance_generation;
    assert!(request.session.read().await.origin_is_qualified());
    assert!(!request.session.read().await.allows_short_origin_windows());
    count.store(1, Ordering::SeqCst);
    request.session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::StaleEntryWindow };
    request.now_ms = current_time_millis();
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let session = request.session.read().await;
    assert_eq!(session.origin_control.acceptance_generation, generation);
    assert!(session.origin_is_qualified());
    assert!(!session.allows_short_origin_windows());
    assert_eq!(server.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn short_startup_preserves_first_response_provider_cookie_for_burst_and_media_fetches() {
    let calls = Arc::new(AtomicUsize::new(0));
    let origin_calls = Arc::clone(&calls);
    let server = spawn_test_origin(Arc::new(move |_| {
        let headers = if origin_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![("Set-Cookie", "sid=short-origin; Path=/; HttpOnly".to_string())]
        } else {
            Vec::new()
        };
        (200, headers, playlist(100, 1))
    }))
    .await;
    let request = request_for_origin(&server);
    assert!(trigger_origin_refresh_sync(request.clone()).await);
    let raw_requests = server.raw_requests.lock().await;
    assert_eq!(raw_requests.len(), 3);
    assert!(raw_requests[1..]
        .iter()
        .all(|request| request_header_value(request, "cookie") == Some("sid=short-origin")));
    let session = request.session.read().await;
    assert!(session.allows_short_origin_windows());
    assert_eq!(session.origin_provider_session_headers.get(axum::http::header::COOKIE).unwrap(), "sid=short-origin");
    assert!(!session.origin_request_headers.contains_key(axum::http::header::COOKIE));
}
