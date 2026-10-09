use super::{
    safe_hls_access_lease_id, safe_proxy_session_id, HlsAccessLeaseId, HlsMediaActivityCommitOutcome,
    HlsMediaActivityMarker, HlsMediaLeaseIdentity, HlsProxyManager, HlsSessionHandle, HlsStartupBodyObservation,
    ProxySessionId, HLS_MEDIA_ACTIVITY_TASK_PERMITS,
};
use axum::{body::Body, http::Response};
use futures::StreamExt;
use log::debug;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Semaphore;
use tuliprox_core::utils::current_time_millis;

impl HlsMediaActivityMarker {
    /// Confirms only a polled, non-empty media body. Bytes retain their shared
    /// backing storage; no payload copy or detached per-chunk task is needed.
    pub fn confirm_media_response(&self, response: Response<Body>) -> Response<Body> {
        let (parts, body) = response.into_parts();
        let mut marker = Some(self.clone());
        let stream = body.into_data_stream().then(move |chunk| {
            let confirmation = if chunk.as_ref().is_ok_and(|bytes| !bytes.is_empty()) { marker.take() } else { None };
            async move {
                if let Some(marker) = confirmation {
                    let outcome = marker
                        .manager
                        .mark_delivered_media_for_lease(
                            &marker.session,
                            &marker.lease_id,
                            &marker.proxy_session_id,
                            marker.lease_identity,
                            current_time_millis(),
                        )
                        .await;
                    if matches!(outcome, HlsMediaActivityCommitOutcome::Committed) {
                        if let (Some(active_provider), Some(owner), Some(request_id)) = (
                            marker.active_provider.as_ref(),
                            marker.session_owner.as_deref(),
                            marker.playback_request_id,
                        ) {
                            active_provider.confirm_identified_playback_activity(owner, request_id);
                        }
                    }
                    marker.log_uncommitted_activity(outcome, "media-delivery");
                }
                chunk
            }
        });
        Response::from_parts(parts, Body::from_stream(stream))
    }
    pub fn new(
        manager: Arc<HlsProxyManager>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        lease_identity: HlsMediaLeaseIdentity,
    ) -> Self {
        Self {
            manager,
            session,
            proxy_session_id,
            lease_id,
            lease_identity,
            completed_segment: None,
            completion_scheduled: None,
            active_provider: None,
            session_owner: None,
            playback_request_id: None,
        }
    }

    pub fn with_active_provider(
        mut self,
        active_provider: Arc<tuliprox_session::ActiveProviderManager>,
        session_owner: Option<String>,
        playback_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    ) -> Self {
        self.active_provider = Some(active_provider);
        self.session_owner = session_owner;
        self.playback_request_id = playback_request_id;
        self
    }

    pub(super) async fn for_segment_request(mut self, proxy_seq: u64, requested_at_ms: u64) -> Option<Self> {
        // A terminal lease may still read the immutable READY live-tail segment
        // protected by its plan. That read marks terminal activity, but it must
        // never mutate the live playback cursor.
        if !self.lease_identity.is_live() {
            return Some(self);
        }
        self.completed_segment = self
            .manager
            .record_access_lease_segment_request_started_if_identity_matches(
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                proxy_seq,
                requested_at_ms,
            )
            .await;
        let _ = self.completed_segment.as_ref()?;
        self.completion_scheduled = Some(Arc::new(AtomicBool::new(false)));
        Some(self)
    }

    pub async fn mark_at(&self, now_ms: u64) {
        let outcome = self
            .manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &self.session,
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                now_ms,
            )
            .await;
        self.log_uncommitted_activity(outcome, "access");
    }

    pub(super) async fn mark_completion_at(&self, now_ms: u64) {
        let Some(token) = self.completed_segment else {
            self.mark_at(now_ms).await;
            return;
        };
        let outcome = self
            .manager
            .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                &self.session,
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                token,
                now_ms,
            )
            .await;
        self.log_uncommitted_activity(outcome, "live-segment-completion");
    }

    pub(super) fn spawn_mark_completion_now(&self) {
        let Some(scheduled) = self.completion_scheduled.as_ref() else {
            debug!(
                "HLS media completion ignored: lease={} proxy_session={} reason=missing-completion-token",
                safe_hls_access_lease_id(&self.lease_id),
                safe_proxy_session_id(&self.proxy_session_id)
            );
            return;
        };
        if scheduled.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            debug!(
                "HLS media completion ignored: lease={} proxy_session={} reason=completion-already-scheduled",
                safe_hls_access_lease_id(&self.lease_id),
                safe_proxy_session_id(&self.proxy_session_id)
            );
            return;
        }
        let marker = self.clone();
        spawn_bounded_media_completion(Arc::clone(&HLS_MEDIA_ACTIVITY_TASK_PERMITS), async move {
            marker.mark_completion_at(current_time_millis()).await;
        });
    }

    pub(super) fn completed_segment_marker(&self) -> Option<Self> { self.completed_segment.map(|_| self.clone()) }

    pub(super) fn record_startup_segment_request(&self, proxy_seq: u64, now_ms: u64) {
        self.manager.startup_observability().record_first_visible_segment_request(&self.lease_id, proxy_seq, now_ms);
    }

    pub(super) fn record_startup_repair_decision(&self, proxy_seq: u64, now_ms: u64) {
        self.manager.startup_observability().record_repair_decision(&self.lease_id, proxy_seq, now_ms);
    }

    pub(super) fn begin_startup_cache_response(
        &self,
        proxy_seq: u64,
        body_id: &str,
        now_ms: u64,
    ) -> Option<HlsStartupBodyObservation> {
        self.manager.startup_observability().begin_cache_response(&self.lease_id, proxy_seq, body_id, now_ms)
    }

    pub(super) fn log_uncommitted_activity(&self, outcome: HlsMediaActivityCommitOutcome, phase: &'static str) {
        let reason = match outcome {
            HlsMediaActivityCommitOutcome::Committed => return,
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity => "expired-or-playback-generation-race",
            HlsMediaActivityCommitOutcome::DeferredLockContention => "lock-contention",
        };
        debug!(
            "HLS media activity ignored: phase={phase} lease={} proxy_session={} reason={reason}",
            safe_hls_access_lease_id(&self.lease_id),
            safe_proxy_session_id(&self.proxy_session_id)
        );
    }
}

pub(super) fn spawn_bounded_media_completion(
    completion_permits: Arc<Semaphore>,
    completion: impl Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        let Ok(_permit) = completion_permits.acquire_owned().await else {
            return;
        };
        completion.await;
    });
}
