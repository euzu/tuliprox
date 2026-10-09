use super::{
    CacheAccessState, HlsLogIdentity, HlsOriginAccountIoLeaseGuard, HlsOriginIoContext, HlsSegmentFailureObject,
    HlsSegmentFailureTransition, HlsSessionHandle, HlsTransientOriginIoGuard, HlsTransientReadGuard,
    SegmentFetchPolicy, TransientResourceKind, TransientResourceRef,
};
use log::{debug, warn};
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;

pub async fn record_successful_transient_segment_fetch(session: &HlsSessionHandle, resource: &TransientResourceRef) {
    if !transient_resource_is_media(resource.kind) {
        return;
    }
    let mut session = session.write().await;
    if !transient_resource_is_current(&session, resource, current_time_millis()) {
        return;
    }
    if let Some(reset_failures) = session.record_successful_segment_fetch() {
        if log::log_enabled!(log::Level::Debug) {
            let identity = HlsLogIdentity::from_session(&session);
            debug!(
                "HLS segment temporary failure counter reset: session={} proxy_session={} previous_failures={reset_failures}",
                identity.session(),
                identity.proxy_session()
            );
        }
    }
}

pub async fn record_temporary_transient_segment_fetch_failure(
    session: &HlsSessionHandle,
    resource: &TransientResourceRef,
    policy: &SegmentFetchPolicy,
    now_ms: u64,
) -> bool {
    if !transient_resource_affects_media_readiness(resource.kind) {
        return false;
    }
    let mut session = session.write().await;
    if !transient_resource_is_current(&session, resource, now_ms) {
        return false;
    }
    session.origin_control.path_condition =
        super::super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
    if !transient_resource_is_media(resource.kind) {
        return false;
    }
    let threshold = policy.permanent_failure_segment_threshold.max(1);
    match session.record_temporary_segment_fetch_failure(
        now_ms,
        HlsSegmentFailureObject::Transient { resource_id: resource.id.0.clone() },
        threshold,
    ) {
        HlsSegmentFailureTransition::StillRetryable { failures, threshold } => {
            if log::log_enabled!(log::Level::Debug) {
                let identity = HlsLogIdentity::from_session(&session);
                debug!(
                    "HLS segment temporary failure counted: session={} proxy_session={} object={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    resource.id.0,
                    failures,
                    threshold
                );
            }
            false
        }
        HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold } => {
            if log::log_enabled!(log::Level::Warn) {
                let identity = HlsLogIdentity::from_session(&session);
                warn!(
                    "HLS segment temporary failure threshold reached: session={} proxy_session={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    failures,
                    threshold
                );
            }
            session.invalidate_queued_origin_work();
            true
        }
    }
}

const fn transient_resource_is_media(kind: TransientResourceKind) -> bool {
    matches!(kind, TransientResourceKind::Segment | TransientResourceKind::Part)
}

pub(super) const fn transient_resource_affects_media_readiness(kind: TransientResourceKind) -> bool {
    matches!(
        kind,
        TransientResourceKind::Segment
            | TransientResourceKind::Part
            | TransientResourceKind::Key
            | TransientResourceKind::Map
    )
}

fn transient_resource_is_current(
    session: &super::super::HlsSession,
    resource: &TransientResourceRef,
    now_ms: u64,
) -> bool {
    session.transient.resource_matches_current(resource, now_ms)
}

impl HlsTransientReadGuard {
    pub(super) fn new(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for HlsTransientReadGuard {
    fn drop(&mut self) { self.access.reader_finished(); }
}

impl HlsTransientOriginIoGuard {
    pub fn new(
        session: HlsSessionHandle,
        active_origin_work_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        origin_io: HlsOriginIoContext,
        lease_guard: HlsOriginAccountIoLeaseGuard,
        started_generation: u64,
    ) -> Self {
        Self {
            session,
            active_origin_work_count,
            origin_io,
            lease_guard: Some(lease_guard),
            started_generation,
            origin_work_finished: false,
        }
    }

    pub async fn finish_clean(mut self) {
        let generation_valid = {
            let mut session = self.session.write().await;
            let valid = session.finish_origin_work(self.started_generation);
            self.origin_work_finished = true;
            valid
        };
        let refresh_reservation = if generation_valid {
            self.session
                .read()
                .await
                .should_refresh_origin_reservation(chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default())
        } else {
            false
        };
        if let Some(lease_guard) = self.lease_guard.take() {
            crate::origin::finish_hls_origin_account_io(
                &self.origin_io,
                &self.session,
                lease_guard,
                refresh_reservation,
            )
            .await;
        }
    }
}

impl Drop for HlsTransientOriginIoGuard {
    fn drop(&mut self) {
        if self.origin_work_finished {
            return;
        }
        self.origin_work_finished = true;
        self.active_origin_work_count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
