use super::{
    classify_hls_backpressure, fetch_segment_into_cache, segment_fetch_attempt_matches, select_segment_key_dependency,
    HlsAccessLeaseStore, HlsBackpressureState, HlsCacheCapacityRevision, HlsCacheMetrics, HlsSegmentEncryption,
    HlsSegmentFetchWorkload, HlsSegmentWorkerPool, HlsSessionHandle, OriginSegmentFetchRef, OriginSegmentKey,
    ReadySegmentKeyFetchSnapshot, SegmentCacheKey, SegmentCacheStatus, SegmentFetchContext, SegmentFetchPolicy,
    SegmentFetchPriority, SegmentKeyDependency, SegmentKeyDependencySelection, TransientObjectFetchToken,
    TransientResourceFile, TransientResourceRef, DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS,
    DEFAULT_REPAIR_POSTPROCESS_TIMEOUT_MS, SEGMENT_FETCH_SCHEDULING_MARGIN_MS,
};
use arc_swap::ArcSwap;
use log::{debug, warn};
use shared::model::HlsSegmentRepairMode;
use std::{fmt, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tuliprox_core::{model::HlsCacheConfig, utils::current_time_millis};

impl HlsSegmentFetchWorkload {
    pub(super) const fn from_encrypted(encrypted: bool) -> Self {
        if encrypted {
            Self::EncryptedWithKey
        } else {
            Self::Clear
        }
    }
}

impl SegmentFetchPolicy {
    pub fn from_config(config: &HlsCacheConfig) -> Self {
        let postprocess_enabled = (config.segment_repair.max_level != HlsSegmentRepairMode::Off
            && config.segment_repair.apply_to_first_segments > 0)
            || config.segment_repair.corrupt_segment_watchdog.mode.is_enabled();
        Self {
            max_global_segment_fetches: config.max_concurrent_segment_fetches_global.max(1),
            max_session_segment_fetches: config.max_concurrent_segment_fetches_per_session.max(1),
            max_prefetch_queue_depth: config.max_segments_prefetch,
            origin_segment_timeout_ms: config.origin_segment_timeout_ms.get().max(1),
            effective_repair_postprocess_timeout_ms: if postprocess_enabled {
                config.segment_repair.postprocess_timeout_ms.get().max(100)
            } else {
                0
            },
            // Object retry classification is deliberately independent from initial-strip policy.
            permanent_failure_segment_threshold: Self::default().permanent_failure_segment_threshold,
            ..Self::default()
        }
    }

    /// Expected latency of one successful media-object operation.
    ///
    /// This deliberately excludes the complete retry chain. Callers may use it
    /// for recovery ETA, while `workload_budget_ms` remains the hard wait bound.
    /// Configured protection timeouts above the conservative fallback are not
    /// predictions of successful-object latency.
    pub fn recovery_object_eta_ms(&self) -> u64 {
        self.origin_segment_timeout_ms
            .min(DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS)
            .saturating_add(self.effective_repair_postprocess_timeout_ms.min(DEFAULT_REPAIR_POSTPROCESS_TIMEOUT_MS))
            .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS)
    }
}

#[derive(Clone)]
pub(super) struct SegmentFetchSnapshot {
    pub(super) proxy_seq: u64,
    pub(super) proxy_seq_log: String,
    pub(super) origin_key: OriginSegmentKey,
    pub(super) cache_key: SegmentCacheKey,
    pub(super) fetch_ref: OriginSegmentFetchRef,
    pub(super) encryption: Option<HlsSegmentEncryption>,
    pub(super) priority: SegmentFetchPriority,
    pub(super) started_at_ms: u64,
    pub(super) proxy_file_ext: String,
    pub(super) origin_seq: u64,
    pub(super) complete_object: bool,
    pub(super) key_dependency: Option<SegmentKeyDependency>,
    pub(super) origin_work_generation: u64,
}

enum SegmentWorkerTask {
    ReadyKey(ReadySegmentKeyFetchSnapshot),
    Segment(SegmentFetchSnapshot),
}

pub(super) struct QueuedSegmentFetchCandidate {
    origin_key: OriginSegmentKey,
    cache_key: SegmentCacheKey,
    fetch_ref: OriginSegmentFetchRef,
    encryption: Option<HlsSegmentEncryption>,
    proxy_file_ext: String,
    complete_object: bool,
}

#[derive(Clone)]
pub(super) struct SegmentKeyFetchDependency {
    pub(super) token: TransientObjectFetchToken,
    pub(super) resource: TransientResourceRef,
    pub(super) resource_file: TransientResourceFile,
}

pub(super) struct ScheduledSegmentRetry {
    pub(super) wake: SegmentRetryWake,
    pub(super) priority: SegmentFetchPriority,
}

pub(super) enum SegmentRetryWake {
    CapacityRevision { revision: HlsCacheCapacityRevision, projected_write_bytes: u64 },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CapacityRetryAdmission {
    Ready,
    BindingExpired,
    LocalIoFailure,
}

impl fmt::Debug for SegmentFetchSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentFetchSnapshot")
            .field("proxy_seq", &self.proxy_seq)
            .field("proxy_seq_log", &self.proxy_seq_log)
            .field("origin_key", &self.origin_key)
            .field("cache_key", &self.cache_key)
            .field("fetch_ref", &self.fetch_ref)
            .field("priority", &self.priority)
            .field("started_at_ms", &self.started_at_ms)
            .field("proxy_file_ext", &self.proxy_file_ext)
            .field("origin_seq", &self.origin_seq)
            .field("complete_object", &self.complete_object)
            .field("encrypted", &self.encryption.is_some())
            .field(
                "key_dependency",
                &self.key_dependency.as_ref().map(|dependency| match dependency {
                    SegmentKeyDependency::Fetch { .. } => "fetch",
                    SegmentKeyDependency::Wait { .. } => "wait",
                }),
            )
            .field("origin_work_generation", &self.origin_work_generation)
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct SegmentWorkerRuntime {
    pub(super) global_semaphore: Arc<Semaphore>,
    pub(super) policy: SegmentFetchPolicy,
}

impl SegmentWorkerRuntime {
    pub(super) fn new(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self {
        Self { global_semaphore, policy }
    }
}

impl HlsSegmentWorkerPool {
    pub fn new(policy: SegmentFetchPolicy) -> Self {
        let global_semaphore = Arc::new(Semaphore::new(policy.max_global_segment_fetches));
        Self::with_global_semaphore(policy, global_semaphore)
    }

    pub fn with_global_semaphore(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self {
        Self::with_global_semaphore_and_metrics(
            policy,
            global_semaphore,
            Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
            Arc::new(HlsCacheMetrics::default()),
        )
    }

    pub fn with_global_semaphore_and_metrics(
        policy: SegmentFetchPolicy,
        global_semaphore: Arc<Semaphore>,
        access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
        metrics: Arc<HlsCacheMetrics>,
    ) -> Self {
        Self::with_global_semaphore_metrics_and_availability(policy, global_semaphore, access_leases, metrics, None)
    }

    pub fn with_global_semaphore_metrics_and_availability(
        policy: SegmentFetchPolicy,
        global_semaphore: Arc<Semaphore>,
        access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
        metrics: Arc<HlsCacheMetrics>,
        availability_reevaluations: Option<
            Arc<super::super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>,
        >,
    ) -> Self {
        Self {
            runtime: ArcSwap::from_pointee(SegmentWorkerRuntime::new(policy, global_semaphore)),
            access_leases,
            metrics,
            availability_reevaluations,
        }
    }

    pub fn update_config(&self, policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) {
        self.runtime.store(Arc::new(SegmentWorkerRuntime::new(policy, global_semaphore)));
    }

    pub fn policy(&self) -> SegmentFetchPolicy { self.runtime.load().policy.clone() }

    pub fn access_leases(&self) -> &Arc<RwLock<HlsAccessLeaseStore>> { &self.access_leases }

    pub fn metrics(&self) -> &Arc<HlsCacheMetrics> { &self.metrics }

    pub async fn classify_backpressure(&self, session: &HlsSessionHandle) -> HlsBackpressureState {
        let session = session.read().await;
        self.classify_backpressure_for_session(&session)
    }

    pub fn classify_backpressure_for_session(&self, session: &super::super::HlsSession) -> HlsBackpressureState {
        let runtime = self.runtime.load();
        classify_hls_backpressure(
            session,
            runtime.global_semaphore.available_permits(),
            runtime.policy.max_session_segment_fetches,
        )
    }

    pub async fn wake_scheduler(self: &Arc<Self>, context: SegmentFetchContext, now_ms: u64) {
        loop {
            let runtime = self.runtime.load_full();
            let Ok(permit) = Arc::clone(&runtime.global_semaphore).try_acquire_owned() else {
                return;
            };
            let task = if let Some(snapshot) =
                self.next_ready_segment_key_fetch_snapshot(&context, now_ms, &runtime.policy).await
            {
                Some(SegmentWorkerTask::ReadyKey(snapshot))
            } else {
                self.next_fetch_snapshot(&context, now_ms, &runtime.policy).await.map(SegmentWorkerTask::Segment)
            };
            let Some(task) = task else {
                drop(permit);
                return;
            };

            let worker = Arc::clone(self);
            let task_context = context.clone();
            tokio::spawn(async move {
                match task {
                    SegmentWorkerTask::ReadyKey(snapshot) => {
                        worker.fetch_ready_segment_key(task_context, snapshot, runtime.policy.clone(), permit).await;
                    }
                    SegmentWorkerTask::Segment(snapshot) => {
                        worker.fetch_one_segment(task_context, snapshot, runtime.policy.clone(), permit).await;
                    }
                }
            });
        }
    }

    pub(super) async fn next_fetch_snapshot(
        &self,
        context: &SegmentFetchContext,
        now_ms: u64,
        policy: &SegmentFetchPolicy,
    ) -> Option<SegmentFetchSnapshot> {
        let (proxy_session_id, gc_marked_for_removal) = {
            let session = context.session.read().await;
            (session.proxy_session_id.clone(), session.is_gc_marked_for_removal())
        };
        if gc_marked_for_removal {
            return None;
        }
        let has_usable_access_lease =
            self.access_leases.write().await.has_usable_access_lease_for_session(&proxy_session_id, now_ms);
        let mut session = context.session.write().await;
        if session.is_gc_marked_for_removal() {
            return None;
        }
        let initial_barrier = session.startup.as_ref().is_some_and(|startup| {
            let Some(head) = session.publishable_origin_head_proxy_seq else {
                return true;
            };
            let mode = startup.config.mode;
            startup.revisions.get(&head).is_none_or(|revision| {
                mode != shared::model::HlsStartupMode::Conservative && !revision.revision().is_startup_head_ready(mode)
            })
        });
        let fetch_limit = if initial_barrier { 1 } else { policy.max_session_segment_fetches };
        if session.active_segment_fetches >= fetch_limit {
            return None;
        }

        while let Some((proxy_seq, priority)) = session.segment_prefetch_queue.pop_next() {
            let origin_work_generation = session.activity.origin_work_generation;
            let Some(candidate) =
                take_queued_segment_fetch_candidate(&mut session, proxy_seq, priority, now_ms, has_usable_access_lease)
            else {
                continue;
            };
            let key_dependency = match select_segment_key_dependency(
                &mut session,
                &proxy_session_id,
                proxy_seq,
                candidate.encryption.as_ref(),
                now_ms,
            ) {
                SegmentKeyDependencySelection::Ready(dependency) => dependency,
                SegmentKeyDependencySelection::Unavailable => continue,
            };
            if let Some(entry) = session.segments.get_mut(&proxy_seq) {
                entry.status = SegmentCacheStatus::Fetching { priority, started_at_ms: now_ms };
            } else {
                continue;
            }
            session.active_segment_fetches = session.active_segment_fetches.saturating_add(1);
            let proxy_seq_log = format!("{proxy_seq:06}");
            if log::log_enabled!(log::Level::Debug) {
                let identity = super::super::HlsLogIdentity::from_session(&session);
                debug!(
                    "HLS segment fetch started: session={} proxy_session={} source=normal resource={} priority={priority:?}",
                    identity.session(),
                    identity.proxy_session(),
                    proxy_seq_log
                );
            }
            return Some(SegmentFetchSnapshot {
                proxy_seq,
                proxy_seq_log,
                origin_key: candidate.origin_key,
                cache_key: candidate.cache_key,
                fetch_ref: candidate.fetch_ref,
                encryption: candidate.encryption,
                priority,
                started_at_ms: now_ms,
                proxy_file_ext: candidate.proxy_file_ext,
                origin_seq: candidate.origin_key.host_local_sequence,
                complete_object: candidate.complete_object,
                key_dependency,
                origin_work_generation,
            });
        }

        None
    }

    async fn fetch_one_segment(
        self: Arc<Self>,
        context: SegmentFetchContext,
        snapshot: SegmentFetchSnapshot,
        policy: SegmentFetchPolicy,
        permit: OwnedSemaphorePermit,
    ) {
        let result = fetch_segment_into_cache(&context, &snapshot, &policy).await;
        let finished_at_ms = current_time_millis();
        let completion = {
            let mut session = context.session.write().await;
            self.apply_segment_fetch_result(&mut session, &snapshot, &policy, result, finished_at_ms)
        };
        let mut notifier = completion.notifier;
        if completion.stale_commit_cleanup_reserved {
            if let Err(err) = context.segment_cache.delete(&snapshot.cache_key).await {
                if log::log_enabled!(log::Level::Warn) {
                    let identity = {
                        let session = context.session.read().await;
                        super::super::HlsLogIdentity::from_session(&session)
                    };
                    warn!(
                        "HLS stale segment cache cleanup failed: session={} proxy_session={} resource={} error={err}",
                        identity.session(),
                        identity.proxy_session(),
                        snapshot.proxy_seq_log
                    );
                }
            }
            let mut session = context.session.write().await;
            if let Some(entry) = session
                .segments
                .get_mut(&snapshot.proxy_seq)
                .filter(|entry| segment_fetch_attempt_matches(entry, &snapshot))
            {
                entry.status = SegmentCacheStatus::Discovered;
            }
            notifier = session.segment_fetch_notifiers.remove(&snapshot.proxy_seq);
        }
        if let Some(notifier) = notifier {
            notifier.notify_waiters();
        }
        if let (Some(coordinator), Some(proxy_session_id)) =
            (self.availability_reevaluations.as_ref(), completion.evidence_changed_for.as_ref())
        {
            coordinator.notify_session_evidence_changed(proxy_session_id);
        }
        drop(permit);
        if let Some(retry) = completion.scheduled_retry {
            self.schedule_segment_retry(context, snapshot, retry);
            return;
        }
        if completion.generation_valid {
            self.schedule_wake(context, finished_at_ms);
        }
    }

    pub(super) fn schedule_wake(self: &Arc<Self>, context: SegmentFetchContext, now_ms: u64) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            worker.wake_scheduler(context, now_ms).await;
        });
    }

    pub(super) fn schedule_segment_retry(
        self: &Arc<Self>,
        context: SegmentFetchContext,
        snapshot: SegmentFetchSnapshot,
        retry: ScheduledSegmentRetry,
    ) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let capacity_admission = match &retry.wake {
                SegmentRetryWake::CapacityRevision { revision, projected_write_bytes } => {
                    wait_for_capacity_retry_admission(&context, &snapshot, revision.clone(), *projected_write_bytes)
                        .await
                }
            };
            let now_ms = current_time_millis();
            let (requeued, abandoned_notifier) = {
                let mut session = context.session.write().await;
                let retry_state_matches = session.segments.get(&snapshot.proxy_seq).is_some_and(|entry| {
                    matches!(
                        (&entry.status, &retry.wake),
                        (SegmentCacheStatus::CapacityDeferred { .. }, SegmentRetryWake::CapacityRevision { .. })
                    )
                });
                if retry_state_matches {
                    let binding_current = session.segments.get(&snapshot.proxy_seq).is_some_and(|entry| {
                        entry.origin_key == snapshot.origin_key
                            && entry.cache_key == snapshot.cache_key
                            && entry.origin_fetch_ref.as_ref() == Some(&snapshot.fetch_ref)
                            && entry.encryption == snapshot.encryption
                            && session.activity.origin_work_generation == snapshot.origin_work_generation
                    });
                    if !binding_current
                        || session.is_gc_marked_for_removal()
                        || capacity_admission != CapacityRetryAdmission::Ready
                    {
                        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                            entry.status = SegmentCacheStatus::Discovered;
                        }
                        (false, session.segment_fetch_notifiers.remove(&snapshot.proxy_seq))
                    } else {
                        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                            entry.status = SegmentCacheStatus::Discovered;
                        }
                        (session.queue_segment_fetch_candidate(snapshot.proxy_seq, retry.priority, now_ms), None)
                    }
                } else {
                    (false, None)
                }
            };
            if let Some(notifier) = abandoned_notifier {
                notifier.notify_waiters();
            }
            if requeued {
                worker.wake_scheduler(context, now_ms).await;
            }
        });
    }
}

async fn wait_for_capacity_retry_admission(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    mut revision: HlsCacheCapacityRevision,
    projected_write_bytes: u64,
) -> CapacityRetryAdmission {
    loop {
        context.segment_cache.wait_for_capacity_change(&revision).await;
        let binding_current = {
            let session = context.session.read().await;
            !session.is_gc_marked_for_removal()
                && session.activity.origin_work_generation == snapshot.origin_work_generation
                && session.segments.get(&snapshot.proxy_seq).is_some_and(|entry| {
                    entry.origin_key == snapshot.origin_key
                        && entry.cache_key == snapshot.cache_key
                        && entry.origin_fetch_ref.as_ref() == Some(&snapshot.fetch_ref)
                        && entry.encryption == snapshot.encryption
                        && entry.status.awaits_capacity_recovery()
                })
        };
        if !binding_current {
            return CapacityRetryAdmission::BindingExpired;
        }
        match context.segment_cache.ensure_projected_write_capacity(&snapshot.cache_key, projected_write_bytes).await {
            Ok(()) => return CapacityRetryAdmission::Ready,
            Err(error) => {
                let Some(capacity) = super::super::cache::hls_cache_capacity_from_io(&error) else {
                    return CapacityRetryAdmission::LocalIoFailure;
                };
                revision = capacity.revision().clone();
            }
        }
    }
}

pub(super) fn take_queued_segment_fetch_candidate(
    session: &mut super::super::HlsSession,
    proxy_seq: u64,
    priority: SegmentFetchPriority,
    now_ms: u64,
    has_usable_access_lease: bool,
) -> Option<QueuedSegmentFetchCandidate> {
    let entry = session.segments.get_mut(&proxy_seq)?;
    if !matches!(entry.status, SegmentCacheStatus::Queued { .. }) {
        return None;
    }
    if priority != SegmentFetchPriority::Demand && !has_usable_access_lease {
        entry.status = SegmentCacheStatus::Discovered;
        return None;
    }
    let Some(fetch_ref) = entry.origin_fetch_ref.clone() else {
        entry.status = SegmentCacheStatus::Discovered;
        return None;
    };
    if !fetch_ref.is_valid_at(now_ms) {
        entry.status = SegmentCacheStatus::Discovered;
        return None;
    }
    Some(QueuedSegmentFetchCandidate {
        origin_key: entry.origin_key,
        cache_key: entry.cache_key.clone(),
        fetch_ref,
        encryption: entry.encryption.clone(),
        proxy_file_ext: entry.proxy_file_ext.clone(),
        complete_object: entry.origin_byte_range.is_none(),
    })
}

pub(super) fn mark_segment_discovered(session: &mut super::super::HlsSession, proxy_seq: u64) {
    if let Some(entry) = session.segments.get_mut(&proxy_seq) {
        entry.status = SegmentCacheStatus::Discovered;
    }
}

impl Default for HlsSegmentWorkerPool {
    fn default() -> Self { Self::new(SegmentFetchPolicy::default()) }
}
