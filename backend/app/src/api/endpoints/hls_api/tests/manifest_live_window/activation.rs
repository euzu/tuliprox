use super::{
    origin, prepare_revisions, response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response,
};
use crate::api::{
    endpoints::hls_api::manifest::{HlsManifestPublicationBarrier, HLS_MANIFEST_PUBLICATION_BARRIER},
    model::AppState,
};
use axum::{
    http::{header, StatusCode},
    response::Response,
};
use shared::model::{HlsCacheConfigDto, HlsStartupMode, HlsStripMode};
use std::{cell::RefCell, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{HlsCacheConfig, StripConfig},
    utils::current_time_millis,
};
use tuliprox_hls::api::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsAccessLeaseTiming, HlsCachedManifestOptions,
    HlsOriginAccountBinding, HlsProxyManager, HlsSessionHandle, HlsSessionKey, ProxySessionId,
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
    pending: HlsAccessLease,
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
        dto.strip.value = 3;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let base = current_time_millis();
        manager.follow_tokio_clock_for_test(base);
        let session =
            manager.get_or_create_session(HlsSessionKey::new(1, "activation-publication"), b"secret", base).await;
        let proxy = session.read().await.proxy_session_id.clone();
        {
            let mut state = session.write().await;
            state.apply_origin_manifest(&origin(100, 4, &[6])).unwrap();
            prepare_revisions(&mut state, &[0, 1, 2, 3]);
            state.origin_account_binding =
                Some(HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &proxy, base));
            state.render_and_store_manifest(base).unwrap();
        }
        let lease_id = HlsAccessLeaseId("activation-publication".into());
        let response = Self::request(&app, &session, &lease_id).await;
        assert_eq!(response.status(), StatusCode::OK);
        let pending = manager.access_lease_response_snapshot(&lease_id, &proxy, manager.now_ms()).await.unwrap();
        assert_eq!(pending.state, HlsAccessLeaseState::Pending);
        assert_eq!(
            pending.last_manifest_snapshot.as_ref().unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        tokio::time::advance(Duration::from_millis(70)).await;
        Self { _directory: directory, app, manager, session, proxy, lease_id, pending }
    }

    async fn request(app: &Arc<AppState>, session: &HlsSessionHandle, lease_id: &HlsAccessLeaseId) -> Response {
        try_test_hls_cached_manifest_response(
            app,
            session,
            lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 3 },
            None,
            HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .unwrap()
    }

    async fn paused_request(&self) -> (Arc<HlsManifestPublicationBarrier>, tokio::task::JoinHandle<Response>) {
        let barrier = Arc::new(HlsManifestPublicationBarrier::default());
        let scoped = Arc::clone(&barrier);
        let app = Arc::clone(&self.app);
        let session = Arc::clone(&self.session);
        let lease_id = self.lease_id.clone();
        let task = tokio::spawn(async move {
            HLS_MANIFEST_PUBLICATION_BARRIER
                .scope(RefCell::new(Some(scoped)), Self::request(&app, &session, &lease_id))
                .await
        });
        barrier.prepared.notified().await;
        (barrier, task)
    }

    async fn activate(&self) {
        assert!(self
            .manager
            .activate_access_lease(
                &self.lease_id,
                &self.proxy,
                self.manager.now_ms(),
                HlsAccessLeaseTiming { active_window_ms: 12_000, valid_window_ms: 300_000 }
            )
            .await
            .is_activated());
    }

    async fn lease(&self) -> HlsAccessLease {
        self.manager.access_lease_response_snapshot(&self.lease_id, &self.proxy, self.manager.now_ms()).await.unwrap()
    }
}

#[tokio::test(start_paused = true)]
async fn activation_during_publication_replans_the_same_zero_wait_request() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        let (epoch, work_generation, media_anchor) = {
            let state = fixture.session.read().await;
            (state.origin_epoch, state.activity.origin_work_generation, state.activity.last_authorized_media_at_ms)
        };
        let (barrier, request) = fixture.paused_request().await;
        fixture.activate().await;
        barrier.resume.notify_one();
        let response = request.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{mode}");
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        assert!(!String::from_utf8(response_body(response).await.to_vec()).unwrap().contains("#EXT-X-ENDLIST"));
        let current = fixture.lease().await;
        assert_eq!(current.state, HlsAccessLeaseState::Activated);
        let published = current.last_manifest_snapshot.unwrap();
        assert_eq!(published.visible_proxy_seqs().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(
            published.snapshot_generation,
            fixture.pending.last_manifest_snapshot.as_ref().unwrap().snapshot_generation + 1
        );
        let state = fixture.session.read().await;
        assert_eq!(
            (state.origin_epoch, state.activity.origin_work_generation, state.activity.last_authorized_media_at_ms),
            (epoch, work_generation, media_anchor)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn activation_does_not_retry_a_superseded_origin_plan() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        let (barrier, request) = fixture.paused_request().await;
        fixture.activate().await;
        fixture.session.write().await.apply_origin_manifest(&origin(101, 4, &[6])).unwrap();
        barrier.resume.notify_one();
        assert_eq!(request.await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.lease().await.last_manifest_snapshot, fixture.pending.last_manifest_snapshot);
    }
}

#[tokio::test(start_paused = true)]
async fn activation_retry_cannot_cross_an_admission_generation_change() {
    for mode in MODES {
        let fixture = Fixture::new(mode).await;
        let (barrier, request) = fixture.paused_request().await;
        fixture.activate().await;
        let mut changed = fixture.lease().await;
        changed.admission_generation += 1;
        fixture.manager.prepare_access_lease(changed).await;
        barrier.resume.notify_one();
        assert_eq!(request.await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.lease().await.last_manifest_snapshot, fixture.pending.last_manifest_snapshot);
    }
}
