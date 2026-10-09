use super::{
    fetch_segment_key_dependency_into_cache, select_key_dependency, HlsSegmentFetchWorkload, HlsSegmentFile,
    HlsSegmentWorkerPool, ReadySegmentKeyFetchSnapshot, SegmentCacheStatus, SegmentDemandFetchOutcome,
    SegmentFetchContext, SegmentFetchPolicy, SegmentFetchPriority, SegmentKeyBindingSnapshot, SegmentKeyDependency,
    SegmentKeyDependencySelection, SegmentKeyFetchDependency,
};
use log::{debug, warn};
use std::sync::Arc;
use tokio::{sync::OwnedSemaphorePermit, time::timeout};
use tuliprox_core::utils::current_time_millis;

impl HlsSegmentWorkerPool {
    pub async fn demand_fetch_and_wait(
        self: &Arc<Self>,
        context: SegmentFetchContext,
        segment_file: &HlsSegmentFile,
        now_ms: u64,
    ) -> SegmentDemandFetchOutcome {
        let (notifier, workload) = {
            let mut session = context.session.write().await;
            if session.is_gc_marked_for_removal() {
                return SegmentDemandFetchOutcome::NotFound;
            }
            let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
                return SegmentDemandFetchOutcome::NotFound;
            };
            if entry.proxy_file_ext != segment_file.extension {
                return SegmentDemandFetchOutcome::NotFound;
            }
            if let SegmentCacheStatus::FailedRetryable { failed_at_ms, retry_after_ms } = entry.status {
                if now_ms < failed_at_ms.saturating_add(retry_after_ms) {
                    return SegmentDemandFetchOutcome::TimedOut;
                }
                if let Some(entry) = session.segments.get_mut(&segment_file.proxy_seq) {
                    entry.status = SegmentCacheStatus::Discovered;
                }
            }
            let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
                return SegmentDemandFetchOutcome::NotFound;
            };
            let workload = HlsSegmentFetchWorkload::from_encrypted(entry.encryption.is_some());
            let notifier = match entry.status {
                SegmentCacheStatus::Ready { .. } => return SegmentDemandFetchOutcome::Ready,
                SegmentCacheStatus::Fetching { .. } | SegmentCacheStatus::CapacityDeferred { .. } => {
                    session.segment_fetch_notifiers.entry(segment_file.proxy_seq).or_default().clone()
                }
                SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. } => {
                    if entry.origin_fetch_ref.is_none() {
                        return SegmentDemandFetchOutcome::Unavailable;
                    }
                    let backpressure = self.classify_backpressure_for_session(&session);
                    if !backpressure.allows_new_demand_fetch() {
                        if log::log_enabled!(log::Level::Warn) {
                            let identity = super::super::HlsLogIdentity::from_session(&session);
                            warn!(
                                "HLS segment demand fetch skipped by backpressure: session={} proxy_session={} source=normal resource={:06} state={backpressure:?}",
                                identity.session(),
                                identity.proxy_session(),
                                segment_file.proxy_seq
                            );
                        }
                        return SegmentDemandFetchOutcome::Unavailable;
                    }
                    session.queue_segment_fetch_candidate(segment_file.proxy_seq, SegmentFetchPriority::Demand, now_ms);
                    self.metrics.record_demand_fetch_started();
                    if log::log_enabled!(log::Level::Debug) {
                        let identity = super::super::HlsLogIdentity::from_session(&session);
                        debug!(
                            "HLS segment demand fetch started: session={} proxy_session={} source=normal resource={:06}",
                            identity.session(),
                            identity.proxy_session(),
                            segment_file.proxy_seq
                        );
                    }
                    session.segment_fetch_notifiers.entry(segment_file.proxy_seq).or_default().clone()
                }
                SegmentCacheStatus::FailedPermanent { .. } | SegmentCacheStatus::Expired => {
                    return SegmentDemandFetchOutcome::Unavailable;
                }
                SegmentCacheStatus::FailedRetryable { .. } => return SegmentDemandFetchOutcome::TimedOut,
            };
            (notifier, workload)
        };

        self.wake_scheduler(context.clone(), now_ms).await;

        let wait_timeout = self.runtime.load().policy.demand_wait_timeout_for(workload);
        if timeout(wait_timeout, notifier.notified()).await.is_err() {
            return SegmentDemandFetchOutcome::TimedOut;
        }

        let session = context.session.read().await;
        if session.is_gc_marked_for_removal() {
            return SegmentDemandFetchOutcome::NotFound;
        }
        match session.segments.get(&segment_file.proxy_seq).map(|entry| &entry.status) {
            Some(SegmentCacheStatus::Ready { .. }) => SegmentDemandFetchOutcome::Ready,
            Some(SegmentCacheStatus::Queued { .. } | SegmentCacheStatus::Fetching { .. }) => {
                SegmentDemandFetchOutcome::QueuedOrFetching
            }
            Some(SegmentCacheStatus::CapacityDeferred { .. } | SegmentCacheStatus::FailedRetryable { .. }) => {
                SegmentDemandFetchOutcome::TimedOut
            }
            Some(
                SegmentCacheStatus::Discovered
                | SegmentCacheStatus::Expired
                | SegmentCacheStatus::FailedPermanent { .. },
            ) => SegmentDemandFetchOutcome::Unavailable,
            None => SegmentDemandFetchOutcome::NotFound,
        }
    }

    pub(super) async fn next_ready_segment_key_fetch_snapshot(
        &self,
        context: &SegmentFetchContext,
        now_ms: u64,
        policy: &SegmentFetchPolicy,
    ) -> Option<ReadySegmentKeyFetchSnapshot> {
        let (proxy_session_id, gc_marked_for_removal) = {
            let session = context.session.read().await;
            (session.proxy_session_id.clone(), session.is_gc_marked_for_removal())
        };
        if gc_marked_for_removal
            || !self.access_leases.write().await.has_usable_access_lease_for_session(&proxy_session_id, now_ms)
        {
            return None;
        }
        let mut session = context.session.write().await;
        if session.is_gc_marked_for_removal() || session.active_segment_fetches >= policy.max_session_segment_fetches {
            return None;
        }
        let rendered_proxy_seqs = session.last_rendered_manifest.as_ref()?.segment_proxy_seqs.clone();
        for proxy_seq in rendered_proxy_seqs {
            let Some((cache_key, encryption)) = session.segments.get(&proxy_seq).and_then(|segment| {
                matches!(segment.status, SegmentCacheStatus::Ready { .. })
                    .then(|| (segment.cache_key.clone(), segment.encryption.clone()))
            }) else {
                continue;
            };
            let Some(encryption) = encryption else {
                continue;
            };
            let SegmentKeyDependencySelection::Ready(Some(SegmentKeyDependency::Fetch(dependency))) =
                select_key_dependency(&mut session, &proxy_session_id, &encryption, now_ms)
            else {
                continue;
            };
            session.active_segment_fetches = session.active_segment_fetches.saturating_add(1);
            return Some(ReadySegmentKeyFetchSnapshot {
                binding: SegmentKeyBindingSnapshot {
                    proxy_seq,
                    cache_key,
                    origin_work_generation: session.activity.origin_work_generation,
                },
                dependency: *dependency,
            });
        }
        None
    }

    pub(super) async fn fetch_ready_segment_key(
        self: Arc<Self>,
        context: SegmentFetchContext,
        snapshot: ReadySegmentKeyFetchSnapshot,
        policy: SegmentFetchPolicy,
        permit: OwnedSemaphorePermit,
    ) {
        let SegmentKeyFetchDependency { token, resource, resource_file } = snapshot.dependency;
        let result = fetch_segment_key_dependency_into_cache(
            &context,
            &snapshot.binding,
            &policy,
            token,
            resource,
            resource_file,
        )
        .await;
        let proxy_session_id = {
            let mut session = context.session.write().await;
            session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
            session.proxy_session_id.clone()
        };
        if result.is_ok() {
            if let Some(coordinator) = self.availability_reevaluations.as_ref() {
                coordinator.notify_session_evidence_changed(&proxy_session_id);
            }
        }
        drop(permit);
        if result.is_ok() {
            self.schedule_wake(context, current_time_millis());
        }
    }
}
