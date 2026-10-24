use super::{
    origin, prepare_revisions, response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response,
};
use crate::api::model::AppState;
use axum::http::StatusCode;
use shared::model::{HlsCacheConfigDto, HlsStartupMode, HlsStripMode};
use std::{sync::Arc, time::Duration};
use tuliprox_core::{
    model::{HlsCacheConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_hls::api::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsCachedManifestOptions,
    HlsLeaseManifestPublicationOutcome as Outcome, HlsLeaseManifestPublicationRejectReason as Reject,
    HlsLeaseManifestSnapshot, HlsProxyManager, HlsSessionHandle, HlsSessionKey, ProxySessionId,
};

const MODES: [HlsStartupMode; 3] =
    [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic];

struct Fixture {
    _directory: tempfile::TempDir,
    app: Arc<AppState>,
    manager: Arc<HlsProxyManager>,
    session: HlsSessionHandle,
    proxy: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    previous: HlsLeaseManifestSnapshot,
    body: bytes::Bytes,
}

impl Fixture {
    async fn new(mode: HlsStartupMode) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            max_segments_prefetch: 0,
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 0;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let session =
            manager.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
        let proxy = session.read().await.proxy_session_id.clone();
        {
            let mut state = session.write().await;
            state.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            prepare_revisions(&mut state, &[3, 4, 5]);
            // Real revision files make GC and media requests exercise the same
            // immutable objects that this lease publishes.
            for seq in [3, 4, 5] {
                let old = state.startup.as_mut().unwrap().revisions.remove(&seq).unwrap();
                let revision =
                    state.startup.as_ref().unwrap().store.create(proxy.clone(), seq, old.revision().key.kind).unwrap();
                let metadata = manager
                    .segment_cache()
                    .write_bytes_and_commit(&revision.revision().key, format!("bound-{seq}").as_bytes())
                    .await
                    .unwrap();
                revision.revision().complete(metadata);
                state.startup.as_mut().unwrap().revisions.insert(seq, revision);
            }
            state.render_and_store_manifest(current_time_millis()).unwrap();
        }
        let lease_id = HlsAccessLeaseId(super::super::support::grant_hls_proxy_lease(&app, &proxy.0).await);
        let response = try_test_hls_cached_manifest_response(
            &app,
            &session,
            &lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 0 },
            None,
            HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body(response).await;
        let mut lease = manager.access_lease_response_snapshot(&lease_id, &proxy, current_time_millis()).await.unwrap();
        lease.active_until_ms = Some(current_time_millis() + 120_000);
        let previous = lease.last_manifest_snapshot.as_ref().unwrap().clone();
        manager.prepare_access_lease(lease).await;
        Self { _directory: directory, app, manager, session, proxy, lease_id, previous, body }
    }

    async fn advance(&self) { self.session.write().await.apply_origin_manifest(&origin(6, 6, &[4])).unwrap(); }

    async fn request(&self, options: HlsCachedManifestOptions) -> axum::response::Response {
        try_test_hls_cached_manifest_response(
            &self.app,
            &self.session,
            &self.lease_id,
            HlsAccessLeaseState::Activated,
            &StripConfig { mode: HlsStripMode::Segments, value: 0 },
            None,
            options,
        )
        .await
        .unwrap()
    }

    async fn lease(&self) -> HlsAccessLease {
        self.manager.access_lease_response_snapshot(&self.lease_id, &self.proxy, current_time_millis()).await.unwrap()
    }

    async fn republish_at(&self, now_ms: u64, snapshot: HlsLeaseManifestSnapshot) -> Outcome {
        let guard =
            self.manager.prepare_access_lease_manifest_publication(&self.lease_id, &self.proxy, now_ms).await.unwrap();
        self.manager
            .republish_access_lease_live_window(
                &self.session,
                &self.lease_id,
                &self.proxy,
                guard,
                snapshot,
                HlsCachedManifestOptions::initial(Duration::ZERO),
                now_ms,
                now_ms,
            )
            .await
    }
}

#[tokio::test]
async fn live_window_retains_twenty_zero_wait_polls_until_the_successor_gate_opens() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        fixture.advance().await;
        let (release, gate) = tokio::sync::oneshot::channel();
        let session = fixture.session.clone();
        let successor = tokio::spawn(async move {
            gate.await.unwrap();
            prepare_revisions(&mut *session.write().await, &[6, 7, 8, 9, 10, 11]);
        });
        for _ in 0..20 {
            let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
            assert_eq!(response.status(), StatusCode::OK, "{mode}");
            assert!(!response.headers().contains_key(axum::http::header::RETRY_AFTER));
            assert_eq!(response_body(response).await, fixture.body);
            let published = fixture.lease().await.last_manifest_snapshot.unwrap();
            assert_eq!(published.delivered_at_ms, fixture.previous.delivered_at_ms);
            assert_eq!(published.source_commit_identity, fixture.previous.source_commit_identity);
            assert_eq!(published.last_visible_media_end_ms, fixture.previous.last_visible_media_end_ms);
            assert_eq!(published.startup_revisions, fixture.previous.startup_revisions);
            assert!(published.snapshot_generation > fixture.previous.snapshot_generation);
        }
        assert!(!successor.is_finished());
        assert!(!fixture.session.read().await.segment_prefetch_queue.is_empty(), "successor work stays queued");
        release.send(()).unwrap();
        successor.await.unwrap();
        let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response_body(response).await, fixture.body);
        let next = fixture.lease().await.last_manifest_snapshot.unwrap();
        assert_eq!(next.first_proxy_seq, 6);
        assert!(next.first_proxy_seq > fixture.previous.first_proxy_seq);
        assert!(next.discontinuity_sequence >= fixture.previous.discontinuity_sequence);
    }
}

#[tokio::test]
async fn live_window_retained_deadline_is_inclusive_and_never_slides() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        fixture.advance().await;
        let deadline = fixture.previous.delivered_at_ms + fixture.previous.playlist_duration_ms;
        assert!(fixture.republish_at(deadline, fixture.previous.clone()).await.is_committed(), "{mode}");
        let current = fixture.lease().await.last_manifest_snapshot.unwrap();
        assert_eq!(current.delivered_at_ms, fixture.previous.delivered_at_ms);
        let result = fixture.republish_at(deadline + 1, current).await;
        assert_eq!(result, Outcome::Rejected(Reject::ManifestGenerationUnavailable));
    }
}

#[tokio::test]
async fn live_window_failed_successor_does_not_poison_the_bound_predecessor() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        fixture.advance().await;
        {
            let mut state = fixture.session.write().await;
            prepare_revisions(&mut state, &[7, 8, 9, 10, 11]);
            let revision = state
                .startup
                .as_ref()
                .unwrap()
                .store
                .create(fixture.proxy.clone(), 6, tuliprox_hls::SegmentRevisionKind::Raw)
                .unwrap();
            revision.revision().fail();
            state.startup.as_mut().unwrap().revisions.insert(6, revision);
        }
        let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, fixture.body);
    }
}

#[tokio::test]
async fn live_window_retained_publication_revalidates_concurrent_state_changes() {
    for change in ["newer", "terminal", "lease", "admission", "session", "origin", "missing_revision", "fresh_commit"] {
        let fixture = Fixture::new(HlsStartupMode::Optimistic).await;
        fixture.advance().await;
        let now = current_time_millis();
        let guard = fixture
            .manager
            .prepare_access_lease_manifest_publication(&fixture.lease_id, &fixture.proxy, now)
            .await
            .unwrap();
        match change {
            "newer" => {
                prepare_revisions(&mut *fixture.session.write().await, &[6, 7, 8, 9, 10, 11]);
                assert_eq!(
                    fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await.status(),
                    StatusCode::OK
                );
            }
            "terminal" => {
                let mut lease = fixture.lease().await;
                lease.playback_mode = tuliprox_hls::api::HlsLeasePlaybackMode::TerminalUnavailable {
                    decision_generation: 1,
                    reason: tuliprox_hls::HlsTerminalTailCompatibility::MissingAsset,
                };
                fixture.manager.prepare_access_lease(lease).await;
            }
            "lease" | "admission" => {
                let mut lease = fixture.lease().await;
                if change == "lease" {
                    lease.issued_at_ms += 1;
                } else {
                    lease.admission_generation += 1;
                }
                fixture.manager.prepare_access_lease(lease).await;
            }
            "session" => {
                let key = fixture.session.read().await.key.clone();
                fixture.manager.sessions().remove_session(&key, &fixture.proxy).await.unwrap();
                fixture.manager.get_or_create_session(key, b"secret", now).await;
            }
            "origin" => fixture.session.write().await.activity.origin_work_generation += 1,
            "missing_revision" => {
                fixture.manager.access_leases().write().await.prune_revision_bindings(&fixture.proxy, 4);
            }
            "fresh_commit" => {
                fixture.session.write().await.fresh_manifest_commit_required =
                    Some(tuliprox_hls::api::HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
            }
            _ => unreachable!(),
        }
        let before = fixture.lease().await;
        let result = fixture
            .manager
            .republish_access_lease_live_window(
                &fixture.session,
                &fixture.lease_id,
                &fixture.proxy,
                guard,
                fixture.previous.clone(),
                HlsCachedManifestOptions::initial(Duration::ZERO),
                now,
                now,
            )
            .await;
        assert!(!result.is_committed(), "{change}: {result:?}");
        if change == "newer" {
            assert_eq!(result, Outcome::Rejected(Reject::SnapshotChanged));
        }
        assert_eq!(fixture.lease().await.last_manifest_snapshot, before.last_manifest_snapshot, "{change}");
    }
}

#[tokio::test]
async fn live_window_retained_fallback_preserves_each_leases_body_and_freshness_requirements() {
    let fixture = Fixture::new(HlsStartupMode::FirstReady).await;
    let second_id = HlsAccessLeaseId("another-live-lease".into());
    let response = try_test_hls_cached_manifest_response(
        &fixture.app,
        &fixture.session,
        &second_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .unwrap();
    let second_body = response_body(response).await;
    let mut second = fixture
        .manager
        .access_lease_response_snapshot(&second_id, &fixture.proxy, current_time_millis())
        .await
        .unwrap();
    second.state = HlsAccessLeaseState::Activated;
    second.active_until_ms = Some(current_time_millis() + 60_000);
    fixture.manager.prepare_access_lease(second).await;
    fixture.advance().await;
    assert_eq!(
        response_body(fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await).await,
        fixture.body
    );
    let response = try_test_hls_cached_manifest_response(
        &fixture.app,
        &fixture.session,
        &second_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await, second_body);
    assert_ne!(second_body, fixture.body);
    let response = fixture
        .request(
            HlsCachedManifestOptions::initial(Duration::ZERO)
                .requiring_newer_manifest(fixture.previous.delivered_at_ms),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn live_window_gc_keeps_bound_bytes_during_fallback_and_releases_them_after_retention() {
    let mut fixture = Fixture::new(HlsStartupMode::FirstReady).await;
    fixture.advance().await;
    let now = current_time_millis();
    let gc = fixture.manager.garbage_collector();
    let mut policy = gc.policy().as_ref().clone();
    policy.cache_duration_ms = 0;
    gc.update_config(policy, gc.rewrite_secret_fingerprint());
    let mut paths = Vec::new();
    for _ in 0..3 {
        assert_eq!(fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await.status(), StatusCode::OK);
        gc.run_once(now + 1).await.unwrap();
        for segment in fixture.previous.visible_segments.iter() {
            let response = super::super::support::get_response(fixture.app.clone(), &segment.uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response_body(response).await.as_ref(), format!("bound-{}", segment.proxy_seq).as_bytes());
            let revision = fixture
                .manager
                .access_leases()
                .read()
                .await
                .published_segment_revision(&fixture.lease_id, segment.proxy_seq)
                .unwrap();
            if let tuliprox_hls::SegmentRevisionState::Complete(metadata) = revision.revision().state() {
                paths.push(metadata.path);
            }
        }
    }
    prepare_revisions(&mut *fixture.session.write().await, &[6, 7, 8, 9, 10, 11]);
    assert_eq!(fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await.status(), StatusCode::OK);
    fixture.previous.startup_revisions = None; // Release the test's own historical pins.
    gc.run_once(now + 60_000).await.unwrap();
    gc.run_once(now + 60_001).await.unwrap();
    assert!(paths.iter().all(|path| !path.exists()), "obsolete bound revision files must become reclaimable");
}

#[tokio::test]
async fn live_window_retained_fallback_does_not_require_complete_successors_in_progressive_modes() {
    for mode in [HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let fixture = Fixture::new(mode).await;
        fixture.advance().await;
        {
            let mut state = fixture.session.write().await;
            prepare_revisions(&mut state, &[6]);
            for seq in 7..=11 {
                let revision = state
                    .startup
                    .as_ref()
                    .unwrap()
                    .store
                    .create(fixture.proxy.clone(), seq, tuliprox_hls::SegmentRevisionKind::Raw)
                    .unwrap();
                if mode == HlsStartupMode::Progressive {
                    revision.revision().prefix_available.store(1, std::sync::atomic::Ordering::Release);
                }
                state.startup.as_mut().unwrap().revisions.insert(seq, revision);
            }
        }
        let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response_body(response).await, fixture.body);
        assert_eq!(fixture.lease().await.last_manifest_snapshot.unwrap().first_proxy_seq, 6);
        assert!(!fixture.session.read().await.startup.as_ref().unwrap().revisions[&7].revision().is_complete());
    }
}

#[tokio::test]
async fn live_window_unusable_bound_predecessor_and_missing_snapshot_keep_the_error_path() {
    for unusable in [false, true] {
        let fixture = Fixture::new(HlsStartupMode::Optimistic).await;
        if unusable {
            // Publish an optimistic pending successor, then fail that exact
            // bound revision before the next origin window arrives.
            {
                let mut state = fixture.session.write().await;
                let revision = state
                    .startup
                    .as_ref()
                    .unwrap()
                    .store
                    .create(fixture.proxy.clone(), 5, tuliprox_hls::SegmentRevisionKind::Raw)
                    .unwrap();
                state.startup.as_mut().unwrap().revisions.insert(5, revision);
            }
            // A complete binding cannot silently be replaced: use a new lease
            // to publish this pending tail under the normal publication rules.
            let id = HlsAccessLeaseId("pending-predecessor".into());
            let response = try_test_hls_cached_manifest_response(
                &fixture.app,
                &fixture.session,
                &id,
                HlsAccessLeaseState::Pending,
                &StripConfig { mode: HlsStripMode::Segments, value: 0 },
                None,
                HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut lease = fixture
                .manager
                .access_lease_response_snapshot(&id, &fixture.proxy, current_time_millis())
                .await
                .unwrap();
            lease.state = HlsAccessLeaseState::Activated;
            lease.active_until_ms = Some(current_time_millis() + 60_000);
            fixture.manager.prepare_access_lease(lease).await;
            fixture.session.read().await.startup.as_ref().unwrap().revisions[&5].revision().fail();
            fixture.advance().await;
            let response = try_test_hls_cached_manifest_response(
                &fixture.app,
                &fixture.session,
                &id,
                HlsAccessLeaseState::Activated,
                &StripConfig { mode: HlsStripMode::Segments, value: 0 },
                None,
                HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        } else {
            let mut lease = fixture.lease().await;
            lease.last_manifest_snapshot = None;
            fixture.manager.prepare_access_lease(lease).await;
            fixture.advance().await;
            assert_eq!(
                fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
    }
}

#[tokio::test]
async fn live_window_retained_response_dispatches_successor_work_before_returning() {
    let fixture = Fixture::new(HlsStartupMode::FirstReady).await;
    fixture.advance().await;
    let gate = fixture.manager.segment_worker_pool().pause_next_origin_prepare_for_test();
    let response = super::super::super::try_hls_cached_manifest_response(
        &fixture.app,
        Some(&super::super::test_fingerprint()),
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        HlsCachedManifestOptions::initial(Duration::ZERO),
        super::super::super::HlsRuntimeBandwidthLearningContext::Disabled,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await, fixture.body);
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified()).await.expect("successor scheduler was woken");
    assert!(!fixture.session.read().await.segment_prefetch_queue.is_empty());
    fixture.manager.access_leases().write().await.remove_access_lease(&fixture.lease_id);
    gate.prepare.notify_one();
    tokio::time::timeout(Duration::from_secs(1), gate.prepared.notified()).await.unwrap();
    gate.proceed.notify_one();
}

#[tokio::test]
async fn live_window_retained_response_rejects_expired_missing_bindings_and_inactive_leases() {
    for reason in ["expired", "bindings", "idle", "pending", "origin_generation"] {
        let fixture = Fixture::new(HlsStartupMode::Progressive).await;
        fixture.advance().await;
        match reason {
            "expired" => {
                let mut lease = fixture.lease().await;
                let previous = lease.last_manifest_snapshot.as_mut().unwrap();
                previous.delivered_at_ms = current_time_millis() - previous.playlist_duration_ms - 1;
                fixture.manager.prepare_access_lease(lease).await;
            }
            "bindings" => fixture.manager.access_leases().write().await.prune_revision_bindings(&fixture.proxy, 4),
            "idle" | "pending" => {
                let mut lease = fixture.lease().await;
                lease.state = if reason == "idle" { HlsAccessLeaseState::Idle } else { HlsAccessLeaseState::Pending };
                fixture.manager.prepare_access_lease(lease).await;
            }
            "origin_generation" => fixture.session.write().await.activity.origin_work_generation += 1,
            _ => unreachable!(),
        }
        let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{reason}");
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "1");
        assert!(response_body(response).await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_window_concurrent_retained_polls_reselect_without_overwriting_newer_snapshots() {
    let fixture = Arc::new(Fixture::new(HlsStartupMode::Optimistic).await);
    fixture.advance().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let fixture = fixture.clone();
        let barrier = barrier.clone();
        requests.spawn(async move {
            barrier.wait().await;
            let response = fixture.request(HlsCachedManifestOptions::initial(Duration::ZERO)).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response_body(response).await, fixture.body);
        });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }
    let current = fixture.lease().await.last_manifest_snapshot.unwrap();
    assert_eq!(current.snapshot_generation, fixture.previous.snapshot_generation + 20);
    assert_eq!(current.delivered_at_ms, fixture.previous.delivered_at_ms);
}

#[tokio::test]
async fn live_window_snapshot_derivation_failure_cannot_select_the_old_body() {
    let fixture = Fixture::new(HlsStartupMode::FirstReady).await;
    fixture.advance().await;
    prepare_revisions(&mut *fixture.session.write().await, &[6, 7, 8, 9, 10, 11]);
    // All segments render, but materialization exceeds the existing snapshot
    // URI bound. This is a representation error, not a render readiness error.
    let prefix = format!("/{}", "x".repeat(400_000));
    let response = try_test_hls_cached_manifest_response(
        &fixture.app,
        &fixture.session,
        &fixture.lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        Some(&prefix),
        HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.lease().await.last_manifest_snapshot.unwrap(), fixture.previous);
}

#[tokio::test]
async fn live_window_retained_deadline_is_rechecked_after_lock_acquisition() {
    let fixture = Fixture::new(HlsStartupMode::FirstReady).await;
    fixture.advance().await;
    let mut lease = fixture.lease().await;
    let previous = lease.last_manifest_snapshot.as_mut().unwrap();
    previous.delivered_at_ms = current_time_millis() - previous.playlist_duration_ms - 1;
    let previous = previous.clone();
    let stale_request_time = previous.delivered_at_ms + previous.playlist_duration_ms;
    fixture.manager.prepare_access_lease(lease).await;
    // The request's captured timestamp was still on the inclusive boundary;
    // the publication transaction must use its current clock as well.
    assert_eq!(
        fixture.republish_at(stale_request_time, previous).await,
        Outcome::Rejected(Reject::ManifestGenerationUnavailable),
    );
}
