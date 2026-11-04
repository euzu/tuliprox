use super::{
    manifest_live_window::{origin, prepare_revisions},
    response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response,
};
use crate::api::{
    endpoints::hls_api::manifest::{HlsManifestPublicationBarrier, HLS_MANIFEST_PUBLICATION_BARRIER},
    model::AppState,
};
use axum::{http::StatusCode, response::Response};
use shared::model::{HlsCacheConfigDto, HlsStartupMode, HlsStripMode, ReverseProxyConfigDto};
use std::{cell::RefCell, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{HlsCacheConfig, ReverseProxyConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_hls::api::{
    HlsAccessLeaseId, HlsAccessLeaseState, HlsAccessLeaseTiming, HlsCachedManifestOptions, HlsProxyManager,
    HlsSessionHandle, HlsSessionKey, ProxySessionId,
};

const MODES: [HlsStartupMode; 3] =
    [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic];

struct Fixture {
    _directory: tempfile::TempDir,
    dto: HlsCacheConfigDto,
    app: Arc<AppState>,
    manager: Arc<HlsProxyManager>,
    session: HlsSessionHandle,
    proxy: ProxySessionId,
    id: HlsAccessLeaseId,
}

impl Fixture {
    async fn persist_media(&self) {
        super::create_active_hls_user_session(&self.app).await;
        let mut state = self.session.write().await;
        for seq in 0..6 {
            let bytes = format!("strip-entry-media-{seq}");
            let key = state.segments[&seq].cache_key.clone();
            let metadata = self.manager.segment_cache().write_bytes_and_commit(&key, bytes.as_bytes()).await.unwrap();
            state.segments.get_mut(&seq).unwrap().status = tuliprox_hls::api::SegmentCacheStatus::Ready {
                content_length: metadata.size,
                ready_at_ms: self.manager.now_ms(),
            };
            if let Some(startup) = state.startup.as_mut() {
                let kind = startup.revisions[&seq].revision().key.kind;
                let revision = startup.store.create(self.proxy.clone(), seq, kind).unwrap();
                let metadata = self
                    .manager
                    .segment_cache()
                    .write_bytes_and_commit(&revision.revision().key, bytes.as_bytes())
                    .await
                    .unwrap();
                revision.revision().complete(metadata);
                startup.revisions.insert(seq, revision);
            }
        }
    }

    async fn lease(&self) -> tuliprox_hls::api::HlsAccessLease {
        self.manager.access_lease_response_snapshot(&self.id, &self.proxy, self.manager.now_ms()).await.unwrap()
    }

    async fn fetch_snapshot_media(&self, snapshot: &tuliprox_hls::api::HlsLeaseManifestSnapshot) {
        for segment in snapshot.visible_segments.iter() {
            let response = super::get_response(self.app.clone(), &segment.uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response_body(response).await.as_ref(),
                format!("strip-entry-media-{}", segment.proxy_seq).as_bytes()
            );
        }
    }

    async fn new(mode: HlsStartupMode, strip: u64) -> Self {
        Self::with_policy(mode, 6, HlsStripMode::Segments, strip).await
    }

    async fn with_policy(mode: HlsStartupMode, count: usize, strip_mode: HlsStripMode, strip: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            max_segments_prefetch: 0,
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = strip;
        dto.strip.mode = strip_mode;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let base = current_time_millis();
        manager.follow_tokio_clock_for_test(base);
        let session = manager.get_or_create_session(HlsSessionKey::new(1, "strip-entry"), b"secret", base).await;
        let proxy = session.read().await.proxy_session_id.clone();
        {
            let mut state = session.write().await;
            if count > 0 {
                state.apply_origin_manifest(&origin(0, count, &[4, 4, 4, 4, 3, 7])).unwrap();
            }
            if (1..3).contains(&count) {
                state.qualify_short_entry_window_for_test();
            }
            if mode == HlsStartupMode::Conservative {
                for entry in state.segments.values_mut() {
                    entry.status =
                        tuliprox_hls::api::SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: base };
                }
            } else {
                prepare_revisions(&mut state, &(0..u64::try_from(count).unwrap()).collect::<Vec<_>>());
            }
            state.live_window_strip = manager.strip();
            if count > 0 {
                state.render_and_store_manifest(base).unwrap();
                state.render_and_store_manifest(base).unwrap();
            }
        }
        let id = HlsAccessLeaseId("strip-viewer".into());
        super::prepare_pending_test_hls_access_lease(&app, &proxy, &id).await;
        Self { _directory: directory, dto, app, manager, session, proxy, id }
    }

    async fn request(&self, state: HlsAccessLeaseState, strip: u64, wait: Duration) -> Option<Response> {
        try_test_hls_cached_manifest_response(
            &self.app,
            &self.session,
            &self.id,
            state,
            &StripConfig { mode: HlsStripMode::Segments, value: strip },
            None,
            HlsCachedManifestOptions::initial(wait),
        )
        .await
    }

    async fn reload(&self, value: u64) {
        let mut dto = self.dto.clone();
        dto.strip.value = value;
        let rp = ReverseProxyConfigDto { hls_cache: Some(dto), ..Default::default() };
        let mut config = self.app.app_config.config.load().as_ref().clone();
        config.reverse_proxy = Some(ReverseProxyConfig::from(&rp));
        self.app.app_config.config.store(Arc::new(config));
        self.manager.update_config(&self.app.app_config).await;
    }

    async fn activate_then_idle(&self) {
        let now = self.manager.now_ms();
        assert!(self
            .manager
            .activate_access_lease(
                &self.id,
                &self.proxy,
                now,
                HlsAccessLeaseTiming { active_window_ms: 1_000, valid_window_ms: 60_000 }
            )
            .await
            .is_activated());
        tokio::time::advance(Duration::from_millis(1_001)).await;
        let snapshot = self.manager.access_leases().write().await.session_snapshot(&self.proxy, self.manager.now_ms());
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(
            self.manager
                .access_lease_response_snapshot(&self.id, &self.proxy, self.manager.now_ms())
                .await
                .unwrap()
                .state,
            HlsAccessLeaseState::Idle
        );
    }

    async fn make_transient(&self, semantics: Option<&str>) -> String {
        let mut body =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:0\n{}", semantics.unwrap_or(""));
        for (seq, duration) in [4, 4, 4, 4, 3, 7].into_iter().enumerate() {
            use std::fmt::Write as _;
            writeln!(
                body,
                "#EXTINF:{duration},\n/hls/shared/live/{}/{}/r/seg{seq}.ts",
                self.proxy.0,
                tuliprox_hls::api::HLS_ACCESS_LEASE_ID_PLACEHOLDER
            )
            .unwrap();
        }
        let mut state = self.session.write().await;
        state.mode = tuliprox_hls::api::HlsSessionMode::TransientPassthrough {
            reason: tuliprox_hls::api::TransientPassthroughReason::ExtXKey,
        };
        state.transient.replace_manifest_with_semantics(body.clone(), self.manager.now_ms(), None);
        state.mark_authorized_media_access(self.manager.now_ms());
        body
    }

    async fn paused_request(
        self: &Arc<Self>,
        state: HlsAccessLeaseState,
    ) -> (Arc<HlsManifestPublicationBarrier>, tokio::task::JoinHandle<Option<Response>>) {
        let barrier = Arc::new(HlsManifestPublicationBarrier::default());
        let scoped = barrier.clone();
        let fixture = self.clone();
        let task = tokio::spawn(async move {
            HLS_MANIFEST_PUBLICATION_BARRIER
                .scope(RefCell::new(Some(scoped)), fixture.request(state, 99, Duration::from_millis(500)))
                .await
        });
        barrier.prepared.notified().await;
        (barrier, task)
    }
}

#[tokio::test(start_paused = true)]
async fn provisioning_handoff_applies_the_live_entry_policy_once() {
    for mode in MODES {
        let fixture = Fixture::with_policy(mode, 0, HlsStripMode::Segments, 3).await;
        super::owner_handoff::enable_hls_provisioning_custom_response(&fixture.app);
        let response = super::super::hls_shared_provisioning_timeline_manifest_response(
            &fixture.app,
            None,
            &fixture.session,
            &fixture.id,
            HlsAccessLeaseState::Pending,
            &fixture.manager.strip(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        let artificial = fixture.lease().await;
        let artificial_seqs =
            artificial.last_manifest_snapshot.as_ref().unwrap().visible_proxy_seqs().collect::<Vec<_>>();
        assert_eq!(
            body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count(),
            artificial_seqs.len()
        );
        assert!(!artificial.has_current_entry_manifest());
        assert!(fixture
            .manager
            .activate_access_lease(
                &fixture.id,
                &fixture.proxy,
                fixture.manager.now_ms(),
                HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 }
            )
            .await
            .is_activated());
        let first = {
            let mut state = fixture.session.write().await;
            state.apply_origin_manifest(&origin(100, 6, &[4])).unwrap();
            let seqs = state
                .segments
                .iter()
                .filter(|(_, entry)| !tuliprox_hls::api::is_hls_provisioning_segment(entry))
                .map(|(seq, _)| *seq)
                .collect::<Vec<_>>();
            prepare_revisions(&mut state, &seqs);
            state.render_and_store_manifest(fixture.manager.now_ms()).unwrap();
            *seqs.first().unwrap()
        };
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap()).await,
            [first, first + 1, first + 2]
        );
        let admitted = fixture.lease().await;
        assert!(admitted.has_current_entry_manifest());
        assert_eq!(admitted.entry_generation(), artificial.entry_generation());
        assert_eq!(admitted.issued_at_ms, artificial.issued_at_ms);
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap()).await,
            (first..first + 6).collect::<Vec<_>>()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn activated_leases_keep_their_windows_and_bound_segment_bytes() {
    for mode in MODES {
        let mut fixture = Fixture::new(mode, 3).await;
        fixture.persist_media().await;
        fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap();
        let a_entry = fixture.lease().await.last_manifest_snapshot.unwrap();
        fixture.fetch_snapshot_media(&a_entry).await;
        assert_eq!(fixture.lease().await.state, HlsAccessLeaseState::Activated);
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap()).await,
            [0, 1, 2, 3, 4, 5]
        );
        let a = fixture.lease().await;
        let shared = fixture.session.read().await.last_rendered_manifest.clone().unwrap().body;
        fixture.reload(1).await;
        fixture.id = HlsAccessLeaseId("second-strip-viewer".into());
        super::prepare_pending_test_hls_access_lease(&fixture.app, &fixture.proxy, &fixture.id).await;
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap()).await,
            [2, 3, 4]
        );
        let b_entry = fixture.lease().await.last_manifest_snapshot.unwrap();
        fixture.fetch_snapshot_media(&b_entry).await;
        assert_eq!(fixture.lease().await.state, HlsAccessLeaseState::Activated);
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap()).await,
            [2, 3, 4, 5]
        );
        let b_active = fixture.lease().await.last_manifest_snapshot.unwrap();
        fixture.activate_then_idle().await;
        let idle = fixture.lease().await;
        assert_eq!(idle.entry_generation(), Some(2));
        fixture.fetch_snapshot_media(&b_active).await;
        let still_idle = fixture.lease().await;
        assert_eq!(still_idle.state, HlsAccessLeaseState::Idle, "{mode}: old media cannot activate a new entry");
        assert!(!still_idle.has_current_entry_manifest());
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap()).await,
            [2, 3, 4]
        );
        fixture.fetch_snapshot_media(&fixture.lease().await.last_manifest_snapshot.unwrap()).await;
        assert_eq!(fixture.lease().await.state, HlsAccessLeaseState::Activated);
        assert_eq!(
            fixture
                .manager
                .access_lease_response_snapshot(&a.lease_id, &fixture.proxy, fixture.manager.now_ms())
                .await
                .unwrap()
                .last_manifest_snapshot,
            a.last_manifest_snapshot
        );
        fixture.fetch_snapshot_media(a.last_manifest_snapshot.as_ref().unwrap()).await;
        assert_eq!(fixture.session.read().await.last_rendered_manifest.as_ref().unwrap().body, shared);
    }
}

#[tokio::test(start_paused = true)]
async fn regressive_entry_deadline_is_bounded_without_affecting_other_leases() {
    for mode in MODES {
        let fixture = Arc::new(Fixture::new(mode, 0).await);
        fixture.request(HlsAccessLeaseState::Pending, 0, Duration::ZERO).await.unwrap();
        fixture.activate_then_idle().await;
        fixture.reload(3).await;
        let old = fixture.lease().await;
        let worker = fixture.clone();
        let task =
            tokio::spawn(async move { worker.request(HlsAccessLeaseState::Idle, 3, Duration::from_millis(100)).await });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::advance(Duration::from_millis(101)).await;
        let started = tokio::time::Instant::now();
        let response = fixture.request(HlsAccessLeaseState::Idle, 3, Duration::from_secs(10)).await;
        assert!(response.is_none_or(|response| response.status() == StatusCode::SERVICE_UNAVAILABLE));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "{mode}: an aborted waiter must not reset the entry budget"
        );
        assert_eq!(fixture.lease().await.last_manifest_snapshot, old.last_manifest_snapshot);
        assert_eq!(fixture.lease().await.entry_generation(), old.entry_generation());
    }
}

async fn sequences(response: Response) -> Vec<u64> {
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(response_body(response).await.to_vec())
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.rsplit('/').next().unwrap().split('.').next().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn idle_entry_cannot_bypass_strip_with_a_retained_active_body() {
    for mode in MODES {
        let fixture = Fixture::new(mode, 0).await;
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 0, Duration::ZERO).await.unwrap()).await,
            vec![3, 4, 5]
        );
        fixture.activate_then_idle().await;
        fixture.reload(3).await;
        let previous = fixture
            .manager
            .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
            .await
            .unwrap()
            .last_manifest_snapshot;
        let response = fixture.request(HlsAccessLeaseState::Idle, 3, Duration::ZERO).await;
        assert!(
            response.is_none_or(|response| response.status() != StatusCode::OK),
            "{mode:?}: a regressive strip entry must wait, never reuse the active body"
        );
        assert_eq!(
            fixture
                .manager
                .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                .await
                .unwrap()
                .last_manifest_snapshot,
            previous
        );
    }
}

#[tokio::test(start_paused = true)]
async fn entry_policy_reload_is_revalidated_at_publication() {
    let fixture = Arc::new(Fixture::new(HlsStartupMode::Conservative, 1).await);
    let barrier = Arc::new(HlsManifestPublicationBarrier::default());
    let scoped = barrier.clone();
    let request = fixture.clone();
    let task = tokio::spawn(async move {
        HLS_MANIFEST_PUBLICATION_BARRIER
            .scope(
                RefCell::new(Some(scoped)),
                request.request(HlsAccessLeaseState::Pending, 1, Duration::from_millis(100)),
            )
            .await
    });
    barrier.prepared.notified().await;
    fixture.reload(2).await;
    barrier.resume.notify_one();
    let response = task.await.unwrap().expect("bounded request should replan to the current policy");
    assert_eq!(sequences(response).await, vec![0, 1, 2, 3], "the old policy candidate must never be committed");
}

#[tokio::test(start_paused = true)]
async fn repeated_pending_and_idle_polls_do_not_strip_cumulatively() {
    let policies = [
        (HlsStripMode::Segments, 0),
        (HlsStripMode::Segments, 1),
        (HlsStripMode::Segments, 2),
        (HlsStripMode::Segments, 3),
        (HlsStripMode::Segments, u64::MAX),
        (HlsStripMode::Seconds, 7),
        (HlsStripMode::Seconds, 8),
    ];
    let cases = [
        (1, vec![vec![0]; 7]),
        (2, vec![vec![0, 1]; 7]),
        (3, vec![vec![0, 1, 2]; 7]),
        (
            4,
            vec![
                vec![1, 2, 3],
                vec![0, 1, 2],
                vec![0, 1, 2],
                vec![0, 1, 2],
                vec![0, 1, 2],
                vec![0, 1, 2],
                vec![0, 1, 2],
            ],
        ),
        (
            6,
            vec![
                vec![3, 4, 5],
                vec![2, 3, 4],
                vec![1, 2, 3],
                vec![0, 1, 2],
                vec![0, 1, 2],
                vec![2, 3, 4],
                vec![1, 2, 3],
            ],
        ),
        (
            12,
            vec![
                vec![9, 10, 11],
                vec![8, 9, 10],
                vec![7, 8, 9],
                vec![6, 7, 8],
                vec![0, 1, 2],
                vec![8, 9, 10],
                vec![7, 8, 9],
            ],
        ),
    ];
    for mode in MODES {
        for (count, expected) in &cases {
            for ((strip_mode, configured), expected) in policies.iter().zip(expected) {
                let fixture = Fixture::with_policy(mode, *count, *strip_mode, *configured).await;
                let shared = fixture.session.read().await.last_rendered_manifest.as_ref().unwrap().body.clone();
                for entry in 1..=2 {
                    for _ in 0..2 {
                        // Both hints are stale: the stored lease and manager policy are authoritative.
                        let response =
                            fixture.request(HlsAccessLeaseState::Activated, u64::MAX, Duration::ZERO).await.unwrap();
                        assert_eq!(
                            &sequences(response).await,
                            expected,
                            "{mode:?}: N={count} {strip_mode:?}={configured}, entry={entry}"
                        );
                        let lease = fixture
                            .manager
                            .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                            .await
                            .unwrap();
                        assert_eq!(lease.entry_generation(), Some(entry));
                        assert_eq!(
                            &lease.last_manifest_snapshot.unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
                            expected
                        );
                    }
                    if entry == 1 {
                        fixture.activate_then_idle().await;
                    }
                }
                assert_eq!(
                    fixture.session.read().await.last_rendered_manifest.as_ref().unwrap().body,
                    shared,
                    "the lease view must not shorten the shared source"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn regressive_entry_waits_for_a_policy_compliant_window() {
    for mode in MODES {
        let fixture = Arc::new(Fixture::new(mode, 0).await);
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 0, Duration::ZERO).await.unwrap()).await,
            vec![3, 4, 5]
        );
        fixture.activate_then_idle().await;
        fixture.reload(3).await;
        let request = fixture.clone();
        let task = tokio::spawn(async move {
            request.request(HlsAccessLeaseState::Activated, 0, Duration::from_millis(500)).await
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "the old active body cannot satisfy the new policy");
        {
            let mut state = fixture.session.write().await;
            state.apply_origin_manifest(&origin(3, 6, &[4, 4, 4, 4, 3, 7])).unwrap();
            prepare_revisions(&mut state, &[3, 4, 5, 6, 7, 8]);
        }
        let response = task.await.unwrap().unwrap();
        assert_eq!(sequences(response).await, vec![3, 4, 5]);
        let lease = fixture
            .manager
            .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
            .await
            .unwrap();
        assert_eq!(lease.entry_generation(), Some(2));
        assert!(lease.has_current_entry_manifest());
    }
}

#[tokio::test(start_paused = true)]
async fn selected_entry_readiness_does_not_fall_back_to_an_unstripped_tail() {
    for mode in MODES {
        let fixture = Fixture::new(mode, 3).await;
        {
            let mut state = fixture.session.write().await;
            for seq in [0, 1, 2] {
                state.segments.get_mut(&seq).unwrap().status = tuliprox_hls::api::SegmentCacheStatus::Discovered;
                state.startup.as_mut().unwrap().revisions.remove(&seq);
            }
        }
        let response = fixture.request(HlsAccessLeaseState::Pending, 0, Duration::ZERO).await;
        assert!(response.is_none_or(|response| response.status() != StatusCode::OK));
        let mut queued = fixture.session.read().await.segment_prefetch_queue.proxy_seqs();
        queued.sort_unstable();
        assert_eq!(queued, vec![0, 1, 2]);
        assert!(fixture
            .manager
            .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
            .await
            .unwrap()
            .last_manifest_snapshot
            .is_none());
        prepare_revisions(&mut *fixture.session.write().await, &[0, 1, 2]);
        assert_eq!(
            sequences(fixture.request(HlsAccessLeaseState::Pending, 0, Duration::ZERO).await.unwrap()).await,
            vec![0, 1, 2]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn entry_uses_current_cache_policy_in_every_manifest_path() {
    let policies = [
        (HlsStripMode::Segments, 0, 6),
        (HlsStripMode::Segments, 1, 5),
        (HlsStripMode::Segments, 2, 4),
        (HlsStripMode::Segments, 3, 3),
        (HlsStripMode::Segments, u64::MAX, 3),
        (HlsStripMode::Seconds, 7, 5),
        (HlsStripMode::Seconds, 8, 4),
    ];
    for transient in [false, true] {
        for (mode, value, visible) in policies {
            let fixture = Fixture::with_policy(HlsStartupMode::Conservative, 6, mode, value).await;
            let source = if transient {
                fixture.make_transient(None).await
            } else {
                fixture.session.read().await.last_rendered_manifest.as_ref().unwrap().body.clone()
            };
            let expected = source
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .take(visible)
                .map(|line| line.replace(tuliprox_hls::api::HLS_ACCESS_LEASE_ID_PLACEHOLDER, &fixture.id.0))
                .collect::<Vec<_>>();
            for entry in 1..=2 {
                for _ in 0..2 {
                    let response = fixture.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
                    let actual =
                        body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect::<Vec<_>>();
                    assert_eq!(actual, expected, "transient={transient} {mode:?}={value} entry={entry}");
                    let lease = fixture
                        .manager
                        .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                        .await
                        .unwrap();
                    assert_eq!(lease.entry_generation(), Some(entry));
                    assert_eq!(lease.last_manifest_snapshot.unwrap().visible_segments.len(), visible);
                }
                if entry == 1 {
                    fixture.activate_then_idle().await;
                }
            }
            let unchanged = if transient {
                fixture.session.read().await.transient.last_manifest_body.clone().unwrap().to_string()
            } else {
                fixture.session.read().await.last_rendered_manifest.as_ref().unwrap().body.clone()
            };
            assert_eq!(unchanged, source);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn entry_preserves_full_manifest_semantics_in_real_app_responses() {
    let archive = Fixture::new(HlsStartupMode::Conservative, u64::MAX).await;
    {
        let mut state = archive.session.write().await;
        state.key.archive_reference = Some(123);
        state.origin_source.archive_reference = Some(123);
    }
    for entry in 1..=2 {
        assert_eq!(
            sequences(archive.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap()).await,
            [0, 1, 2, 3, 4, 5]
        );
        assert_eq!(archive.lease().await.entry_generation(), Some(entry));
        if entry == 1 {
            archive.activate_then_idle().await;
        }
    }
    for tag in ["#EXT-X-PLAYLIST-TYPE:EVENT\n", "#EXT-X-PLAYLIST-TYPE:VOD\n", "#EXT-X-ENDLIST\n"] {
        let fixture = Fixture::new(HlsStartupMode::Conservative, u64::MAX).await;
        let source = fixture.make_transient(Some(tag)).await;
        for entry in 1..=2 {
            let response = fixture.request(HlsAccessLeaseState::Activated, 0, Duration::ZERO).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert_eq!(body, source.replace(tuliprox_hls::api::HLS_ACCESS_LEASE_ID_PLACEHOLDER, &fixture.id.0));
            let lease = fixture
                .manager
                .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                .await
                .unwrap();
            assert_eq!(lease.entry_generation(), Some(entry));
            if entry == 1 {
                fixture.activate_then_idle().await;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn entry_policy_reload_covers_pending_and_idle_in_all_cache_paths() {
    for mode in [
        HlsStartupMode::Conservative,
        HlsStartupMode::FirstReady,
        HlsStartupMode::Progressive,
        HlsStartupMode::Optimistic,
    ] {
        for idle in [false, true] {
            let fixture = Arc::new(Fixture::new(mode, if idle { 3 } else { 1 }).await);
            if idle {
                assert_eq!(
                    sequences(fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap()).await,
                    vec![0, 1, 2]
                );
                fixture.activate_then_idle().await;
                fixture.reload(1).await;
            }
            let (barrier, task) = fixture.paused_request(HlsAccessLeaseState::Activated).await;
            fixture.reload(2).await;
            barrier.resume.notify_one();
            let response = task.await.unwrap().unwrap();
            assert_eq!(
                sequences(response).await,
                if mode == HlsStartupMode::Conservative { vec![0, 1, 2, 3] } else { vec![1, 2, 3] }
            );
        }
    }
    for idle in [false, true] {
        let fixture = Arc::new(Fixture::new(HlsStartupMode::Conservative, 1).await);
        fixture.make_transient(None).await;
        if idle {
            fixture.request(HlsAccessLeaseState::Pending, 1, Duration::ZERO).await.unwrap();
            fixture.activate_then_idle().await;
        }
        let (barrier, task) = fixture.paused_request(HlsAccessLeaseState::Activated).await;
        fixture.reload(2).await;
        barrier.resume.notify_one();
        let response = task.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let lease = fixture
            .manager
            .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
            .await
            .unwrap();
        assert_eq!(lease.last_manifest_snapshot.unwrap().visible_segments.len(), 4);
    }
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the deterministic mutation and its publication assertion together for each race."
)]
async fn entry_publication_fences_activation_idle_terminal_and_source_races() {
    for mode in MODES {
        for change in ["activation", "idle", "origin", "session", "terminal", "readiness", "lease", "source"] {
            let fixture = Arc::new(Fixture::new(mode, 3).await);
            fixture.request(HlsAccessLeaseState::Pending, 3, Duration::ZERO).await.unwrap();
            let before = fixture
                .manager
                .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                .await
                .unwrap();
            if change == "idle" {
                assert!(fixture
                    .manager
                    .activate_access_lease(
                        &fixture.id,
                        &fixture.proxy,
                        fixture.manager.now_ms(),
                        HlsAccessLeaseTiming { active_window_ms: 1_000, valid_window_ms: 60_000 }
                    )
                    .await
                    .is_activated());
            }
            let (barrier, task) = fixture.paused_request(HlsAccessLeaseState::Pending).await;
            match change {
                "activation" => assert!(fixture
                    .manager
                    .activate_access_lease(
                        &fixture.id,
                        &fixture.proxy,
                        fixture.manager.now_ms(),
                        HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 }
                    )
                    .await
                    .is_activated()),
                "idle" => {
                    tokio::time::advance(Duration::from_millis(1_001)).await;
                    assert_eq!(
                        fixture
                            .manager
                            .access_leases()
                            .write()
                            .await
                            .session_snapshot(&fixture.proxy, fixture.manager.now_ms())
                            .idle_releases
                            .len(),
                        1
                    );
                }
                "origin" => {
                    let guard = fixture
                        .manager
                        .prepare_access_lease_manifest_publication(
                            &fixture.id,
                            &fixture.proxy,
                            fixture.manager.now_ms(),
                        )
                        .await
                        .unwrap();
                    let mut state = fixture.session.write().await;
                    state.origin_epoch += 1;
                    state.activity.origin_work_generation += 1;
                    drop(state);
                    assert!(!fixture
                        .manager
                        .commit_access_lease_manifest_publication(
                            &fixture.id,
                            &fixture.proxy,
                            guard,
                            before.last_manifest_snapshot.clone().unwrap(),
                            fixture.manager.now_ms()
                        )
                        .await
                        .is_committed());
                }
                "session" => {
                    let key = fixture.session.read().await.key.clone();
                    fixture.manager.sessions().remove_session(&key, &fixture.proxy).await.unwrap();
                    let replacement =
                        fixture.manager.get_or_create_session(key, b"secret", fixture.manager.now_ms()).await;
                    assert!(!Arc::ptr_eq(&replacement, &fixture.session));
                }
                "terminal" => {
                    let mut lease = before.clone();
                    lease.playback_mode = tuliprox_hls::api::HlsLeasePlaybackMode::Ended;
                    fixture.manager.prepare_access_lease(lease).await;
                }
                "readiness" => {
                    let mut state = fixture.session.write().await;
                    state.startup.as_mut().unwrap().revisions.remove(&0);
                    state.segments.get_mut(&0).unwrap().status = tuliprox_hls::api::SegmentCacheStatus::Discovered;
                    state.activity.media_readiness_generation += 1;
                }
                "lease" => {
                    let mut lease = before.clone();
                    lease.issued_at_ms += 1;
                    fixture.manager.prepare_access_lease(lease).await;
                }
                "source" => {
                    let mut state = fixture.session.write().await;
                    state.apply_origin_manifest(&origin(1, 6, &[4, 4, 4, 4, 3, 7])).unwrap();
                    prepare_revisions(&mut state, &[6]);
                }
                _ => unreachable!(),
            }
            barrier.resume.notify_one();
            let response = task.await.unwrap();
            if matches!(change, "activation" | "idle" | "source" | "origin") {
                let expected = match change {
                    "activation" => vec![0, 1, 2, 3, 4, 5],
                    "idle" | "origin" => vec![0, 1, 2],
                    _ => vec![1, 2, 3],
                };
                assert_eq!(sequences(response.unwrap()).await, expected, "{mode:?}: {change}");
            } else {
                assert!(
                    response.is_none_or(|response| response.status() != StatusCode::OK),
                    "{mode:?}: {change} cannot publish the stale candidate"
                );
                let current = fixture
                    .manager
                    .access_lease_response_snapshot(&fixture.id, &fixture.proxy, fixture.manager.now_ms())
                    .await
                    .unwrap();
                assert_eq!(
                    current.last_manifest_snapshot, before.last_manifest_snapshot,
                    "{mode:?}: {change} mutated a published snapshot"
                );
            }
        }
    }
}
