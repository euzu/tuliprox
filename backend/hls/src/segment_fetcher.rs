#![allow(clippy::large_futures)]

use super::{
    begin_hls_origin_account_io_bounded, build_hls_origin_resource_headers, classify_hls_backpressure,
    fetch_hls_transient_origin_response_with_attempt_prepare, finish_hls_origin_account_io, hls_object_body_deadline,
    run_hls_origin_resource_retry_loop_with_attempt_prepare, CachedSegmentMetadata, HlsAccessLeaseId,
    HlsAccessLeaseStore, HlsBackpressureState, HlsBoundAccountAcquireErrorKind, HlsCacheCapacityRevision,
    HlsCacheMetrics, HlsCacheObjectKey, HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation,
    HlsOriginIoContext, HlsOriginResourceBodyDeadline, HlsOriginResourceClients, HlsOriginResourceFetchError,
    HlsOriginResourceFetchTarget, HlsRepairRenderedObjectId, HlsResourceFetchKind, HlsResourceFetchSource,
    HlsSegmentCache, HlsSegmentEncryption, HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSegmentFile,
    HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource, HlsSessionHandle,
    HlsTransientObjectFetchFinalizer, HlsTransientOriginFetchRequest, OriginSegmentFetchRef, OriginSegmentKey,
    ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPriority, StagedCacheObject,
    TransientObjectFetchDecision, TransientObjectFetchToken, TransientObjectUnavailableState,
    TransientPassthroughState, TransientResourceFile, TransientResourceId, TransientResourceKind, TransientResourceRef,
};
use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use futures::FutureExt;
use log::{debug, warn};
use reqwest::Client;
use shared::model::HlsSegmentRepairMode;
use std::{fmt, sync::Arc, time::Duration};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, RwLock, Semaphore},
    time::{timeout, timeout_at, Instant},
};
use tuliprox_core::{model::HlsCacheConfig, utils::content_coding::DecodedHttpResponse};
use tuliprox_parser::hls::origin_manifest::ParsedByteRange;

const DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES: usize = 64;
const DEFAULT_MAX_SESSION_SEGMENT_FETCHES: usize = 2;
const DEFAULT_MAX_PREFETCH_QUEUE_DEPTH: usize = 6;
const DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REPAIR_POSTPROCESS_TIMEOUT_MS: u64 = 2_000;
const SEGMENT_FETCH_SCHEDULING_MARGIN_MS: u64 = 1_000;

/// Origin-object work which must complete before one media segment is usable.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSegmentFetchWorkload {
    Clear,
    EncryptedWithKey,
}

impl HlsSegmentFetchWorkload {
    const fn serialized_origin_objects(self) -> u64 {
        match self {
            Self::Clear => 1,
            Self::EncryptedWithKey => 2,
        }
    }

    const fn from_encrypted(encrypted: bool) -> Self {
        if encrypted {
            Self::EncryptedWithKey
        } else {
            Self::Clear
        }
    }
}

/// Runtime policy for bounded live HLS segment origin fetches.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SegmentFetchPolicy {
    pub max_global_segment_fetches: usize,
    pub max_session_segment_fetches: usize,
    pub max_prefetch_queue_depth: usize,
    pub origin_segment_timeout_ms: u64,
    pub effective_repair_postprocess_timeout_ms: u64,
    pub retry_delays_ms: [u64; 5],
    pub retry_jitter_max_ms: u64,
    pub permanent_failure_segment_threshold: u32,
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
            origin_segment_timeout_ms: config.origin_segment_timeout_ms.max(1),
            effective_repair_postprocess_timeout_ms: if postprocess_enabled {
                config.segment_repair.postprocess_timeout_ms.max(100)
            } else {
                0
            },
            // Object retry classification is deliberately independent from initial-strip policy.
            permanent_failure_segment_threshold: Self::default().permanent_failure_segment_threshold,
            ..Self::default()
        }
    }

    fn origin_object_retry_chain_budget_ms(&self) -> u64 {
        let attempts = u64::try_from(self.retry_delays_ms.len()).unwrap_or(u64::MAX);
        let per_attempt_budget =
            self.origin_segment_timeout_ms.saturating_add(self.effective_repair_postprocess_timeout_ms);
        let retry_delay_budget = self.retry_delays_ms.iter().copied().fold(0_u64, u64::saturating_add);
        let jitter_budget = attempts.saturating_mul(self.retry_jitter_max_ms);
        attempts.saturating_mul(per_attempt_budget).saturating_add(retry_delay_budget).saturating_add(jitter_budget)
    }

    pub fn workload_budget_ms(&self, workload: HlsSegmentFetchWorkload) -> u64 {
        self.origin_object_retry_chain_budget_ms()
            .saturating_mul(workload.serialized_origin_objects())
            .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS)
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

    /// Conservative public wait bound for a segment whose encryption state is not known at the call site.
    pub fn demand_wait_timeout(&self) -> Duration {
        self.demand_wait_timeout_for(HlsSegmentFetchWorkload::EncryptedWithKey)
    }

    pub fn demand_wait_timeout_for(&self, workload: HlsSegmentFetchWorkload) -> Duration {
        Duration::from_millis(self.workload_budget_ms(workload))
    }

    /// Wait bound for one transient origin object such as a key or MAP.
    pub fn origin_object_wait_timeout(&self) -> Duration {
        self.demand_wait_timeout_for(HlsSegmentFetchWorkload::Clear)
    }

    pub fn retry_delay_ms(&self, attempt_index: usize) -> u64 {
        let base_delay_ms = self.retry_delays_ms[attempt_index];
        if self.retry_jitter_max_ms == 0 {
            return base_delay_ms;
        }
        let jitter_ms = fastrand::u64(0..=self.retry_jitter_max_ms);
        if fastrand::bool() {
            base_delay_ms.saturating_sub(jitter_ms)
        } else {
            base_delay_ms.saturating_add(jitter_ms)
        }
    }
}

impl Default for SegmentFetchPolicy {
    fn default() -> Self {
        Self {
            max_global_segment_fetches: DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES,
            max_session_segment_fetches: DEFAULT_MAX_SESSION_SEGMENT_FETCHES,
            max_prefetch_queue_depth: DEFAULT_MAX_PREFETCH_QUEUE_DEPTH,
            origin_segment_timeout_ms: DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS,
            effective_repair_postprocess_timeout_ms: 0,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            permanent_failure_segment_threshold: 3,
        }
    }
}

/// Shared context required to schedule a segment fetch without holding session locks.
#[derive(Clone)]
pub struct SegmentFetchContext {
    pub session: HlsSessionHandle,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub repair_access_lease_id: Option<HlsAccessLeaseId>,
    pub headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub origin_io: Option<HlsOriginIoContext>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SegmentDemandFetchOutcome {
    Ready,
    QueuedOrFetching,
    NotFound,
    Unavailable,
    TimedOut,
}

/// Resource class staged before a cross-host manifest handoff is allowed to mutate the timeline.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSwitchResourceKind {
    Segment,
    Map,
}

/// Fetches one candidate-bound switch resource into an uncommitted cache object.
///
/// The caller owns the returned object and must either commit it to the previewed cache key or remove it. This helper
/// shares the normal origin-resource Range/content-coding/retry implementation and performs no session mutation.
pub async fn stage_hls_switch_resource<K>(
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    cache_key: K,
    origin_url: String,
    byte_range: Option<ParsedByteRange>,
    kind: HlsSwitchResourceKind,
) -> Result<StagedCacheObject, HlsOriginResourceFetchError>
where
    K: HlsCacheObjectKey + Clone + Send + Sync + 'static,
{
    let headers = build_segment_origin_headers(&context.headers, &context.origin_provider_session_headers, byte_range)?;
    let resource_kind = match kind {
        HlsSwitchResourceKind::Segment => HlsResourceFetchKind::Segment,
        HlsSwitchResourceKind::Map => HlsResourceFetchKind::Map,
    };
    let target = HlsOriginResourceFetchTarget {
        kind: resource_kind,
        source: HlsResourceFetchSource::Normal,
        object_id: match kind {
            HlsSwitchResourceKind::Segment => "switch-segment".to_string(),
            HlsSwitchResourceKind::Map => "switch-map".to_string(),
        },
        origin_url,
        headers,
        byte_range_expectation: if byte_range.is_some() {
            HlsOriginByteRangeExpectation::PartialContent
        } else {
            HlsOriginByteRangeExpectation::FullObject
        },
    };
    let clients = HlsOriginResourceClients {
        client: context.client.clone(),
        no_redirect_client: context.no_redirect_client.clone(),
        use_manual_redirects: context.use_manual_redirects,
    };
    let log_identity = {
        let session = context.session.read().await;
        super::HlsLogIdentity::from_session(&session)
    };
    let cache = Arc::clone(&context.segment_cache);
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &log_identity,
        |_attempt| async { Ok(()) }.boxed(),
        |()| async {}.boxed(),
        move |response, _attempt, body_deadline, ()| {
            let cache = Arc::clone(&cache);
            let cache_key = cache_key.clone();
            async move {
                let staged = cache
                    .stage_temp_with_deadline(&cache_key, response.body, body_deadline.deadline())
                    .await
                    .map_err(|err| HlsOriginResourceFetchError::cache_body(&err))?;
                if staged.size == 0 {
                    if let Err(err) = cache.remove_staged(staged).await {
                        warn!("HLS empty staged switch resource cleanup failed: error={err}");
                    }
                    return Err(HlsOriginResourceFetchError::cache_body(&std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "empty switch resource",
                    )));
                }
                Ok(staged)
            }
            .boxed()
        },
    )
    .await
}

#[derive(Clone)]
struct SegmentFetchSnapshot {
    proxy_seq: u64,
    proxy_seq_log: String,
    origin_key: OriginSegmentKey,
    cache_key: SegmentCacheKey,
    fetch_ref: OriginSegmentFetchRef,
    encryption: Option<HlsSegmentEncryption>,
    priority: SegmentFetchPriority,
    started_at_ms: u64,
    proxy_file_ext: String,
    origin_seq: u64,
    complete_object: bool,
    key_dependency: Option<SegmentKeyDependency>,
    origin_work_generation: u64,
}

#[derive(Clone)]
struct SegmentKeyBindingSnapshot {
    proxy_seq: u64,
    cache_key: SegmentCacheKey,
    origin_work_generation: u64,
}

impl From<&SegmentFetchSnapshot> for SegmentKeyBindingSnapshot {
    fn from(snapshot: &SegmentFetchSnapshot) -> Self {
        Self {
            proxy_seq: snapshot.proxy_seq,
            cache_key: snapshot.cache_key.clone(),
            origin_work_generation: snapshot.origin_work_generation,
        }
    }
}

struct ReadySegmentKeyFetchSnapshot {
    binding: SegmentKeyBindingSnapshot,
    dependency: SegmentKeyFetchDependency,
}

enum SegmentWorkerTask {
    ReadyKey(ReadySegmentKeyFetchSnapshot),
    Segment(SegmentFetchSnapshot),
}

struct QueuedSegmentFetchCandidate {
    origin_key: OriginSegmentKey,
    cache_key: SegmentCacheKey,
    fetch_ref: OriginSegmentFetchRef,
    encryption: Option<HlsSegmentEncryption>,
    proxy_file_ext: String,
    complete_object: bool,
}

#[derive(Clone)]
enum SegmentKeyDependency {
    Fetch(Box<SegmentKeyFetchDependency>),
    Wait { notifier: Arc<Notify>, resource_id: TransientResourceId, resource_extension: String },
}

#[derive(Clone)]
struct SegmentKeyFetchDependency {
    token: TransientObjectFetchToken,
    resource: TransientResourceRef,
    resource_file: TransientResourceFile,
}

enum SegmentKeyDependencySelection {
    Ready(Option<SegmentKeyDependency>),
    Unavailable,
}

struct SegmentFetchCompletion {
    generation_valid: bool,
    stale_commit_cleanup_reserved: bool,
    notifier: Option<Arc<Notify>>,
    scheduled_retry: Option<ScheduledSegmentRetry>,
    evidence_changed_for: Option<ProxySessionId>,
}

struct ScheduledSegmentRetry {
    wake: SegmentRetryWake,
    priority: SegmentFetchPriority,
}

enum SegmentRetryWake {
    CapacityRevision { revision: HlsCacheCapacityRevision, projected_write_bytes: u64 },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CapacityRetryAdmission {
    Ready,
    BindingExpired,
    LocalIoFailure,
}

struct SegmentFetchCommit {
    content_length: u64,
    generation_valid: bool,
}

#[derive(Clone, Copy)]
struct SegmentOriginWorkFinish {
    generation_valid: bool,
    refresh_reservation: bool,
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

fn segment_fetch_attempt_matches(entry: &SegmentEntry, snapshot: &SegmentFetchSnapshot) -> bool {
    entry.origin_key == snapshot.origin_key
        && entry.cache_key == snapshot.cache_key
        && matches!(
            entry.status,
            SegmentCacheStatus::Fetching { priority, started_at_ms }
                if priority == snapshot.priority && started_at_ms == snapshot.started_at_ms
        )
}

fn segment_fetch_binding_matches(entry: &SegmentEntry, snapshot: &SegmentFetchSnapshot) -> bool {
    segment_fetch_attempt_matches(entry, snapshot)
        && entry.origin_fetch_ref.as_ref() == Some(&snapshot.fetch_ref)
        && entry.encryption == snapshot.encryption
}

type SegmentFetchError = HlsOriginResourceFetchError;

#[derive(Clone)]
struct SegmentWorkerRuntime {
    global_semaphore: Arc<Semaphore>,
    policy: SegmentFetchPolicy,
}

impl SegmentWorkerRuntime {
    fn new(policy: SegmentFetchPolicy, global_semaphore: Arc<Semaphore>) -> Self { Self { global_semaphore, policy } }
}

/// Bounded scheduler for live HLS segment demand fetches and prefetches.
pub struct HlsSegmentWorkerPool {
    runtime: ArcSwap<SegmentWorkerRuntime>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    metrics: Arc<HlsCacheMetrics>,
    availability_reevaluations: Option<Arc<super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>>,
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
            Arc<super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>,
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

    pub fn classify_backpressure_for_session(&self, session: &super::HlsSession) -> HlsBackpressureState {
        let runtime = self.runtime.load();
        classify_hls_backpressure(
            session,
            runtime.global_semaphore.available_permits(),
            runtime.policy.max_session_segment_fetches,
        )
    }

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
                            let identity = super::HlsLogIdentity::from_session(&session);
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
                        let identity = super::HlsLogIdentity::from_session(&session);
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

    async fn next_ready_segment_key_fetch_snapshot(
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

    async fn fetch_ready_segment_key(
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

    async fn next_fetch_snapshot(
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
        if session.active_segment_fetches >= policy.max_session_segment_fetches {
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
                let identity = super::HlsLogIdentity::from_session(&session);
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
                        super::HlsLogIdentity::from_session(&session)
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

    fn apply_segment_fetch_result(
        &self,
        session: &mut super::HlsSession,
        snapshot: &SegmentFetchSnapshot,
        policy: &SegmentFetchPolicy,
        result: Result<SegmentFetchCommit, SegmentFetchError>,
        finished_at_ms: u64,
    ) -> SegmentFetchCompletion {
        session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
        let fetch_succeeded = result.is_ok();
        let attempt_matches = session
            .segments
            .get(&snapshot.proxy_seq)
            .is_some_and(|entry| segment_fetch_attempt_matches(entry, snapshot));
        let binding_matches = session
            .segments
            .get(&snapshot.proxy_seq)
            .is_some_and(|entry| segment_fetch_binding_matches(entry, snapshot));
        let generation_valid = binding_matches
            && session.activity.origin_work_generation == snapshot.origin_work_generation
            && result.as_ref().map_or(true, |commit| commit.generation_valid);
        // A successful origin/repair path has already atomically installed its physical object. Keep this fetch's
        // entry in `Fetching` until stale cleanup finishes so no rebound attempt can install newer bytes at the same
        // stable cache path and then lose them to this worker's cleanup.
        let stale_commit_cleanup_reserved = fetch_succeeded && !generation_valid && attempt_matches;
        let capacity_retry = if generation_valid {
            result.as_ref().err().and_then(|error| {
                error.capacity_revision().cloned().zip(error.projected_write_bytes()).map(
                    |(revision, projected_write_bytes)| ScheduledSegmentRetry {
                        wake: SegmentRetryWake::CapacityRevision { revision, projected_write_bytes },
                        priority: snapshot.priority,
                    },
                )
            })
        } else {
            None
        };
        if generation_valid {
            match result {
                Ok(commit) => self.commit_successful_segment_fetch(session, snapshot, &commit, finished_at_ms),
                Err(err) => commit_failed_segment_fetch(session, snapshot, policy, &err, finished_at_ms),
            }
        } else if !stale_commit_cleanup_reserved {
            if let Some(entry) = session
                .segments
                .get_mut(&snapshot.proxy_seq)
                .filter(|entry| segment_fetch_attempt_matches(entry, snapshot))
            {
                entry.status = SegmentCacheStatus::Discovered;
            }
        }
        let scheduled_retry = capacity_retry;
        if generation_valid {
            if let Err(err) = session.render_and_store_manifest(finished_at_ms) {
                if log::log_enabled!(log::Level::Debug) {
                    let identity = super::HlsLogIdentity::from_session(session);
                    debug!(
                        "HLS manifest render deferred after segment state change: session={} proxy_session={} resource={} error={err:?}",
                        identity.session(),
                        identity.proxy_session(),
                        snapshot.proxy_seq_log
                    );
                }
            }
        }
        let notifier = (!stale_commit_cleanup_reserved && scheduled_retry.is_none())
            .then(|| session.segment_fetch_notifiers.remove(&snapshot.proxy_seq))
            .flatten();
        let evidence_changed_for = (generation_valid && fetch_succeeded).then(|| session.proxy_session_id.clone());
        SegmentFetchCompletion {
            generation_valid,
            stale_commit_cleanup_reserved,
            notifier,
            scheduled_retry,
            evidence_changed_for,
        }
    }

    fn commit_successful_segment_fetch(
        &self,
        session: &mut super::HlsSession,
        snapshot: &SegmentFetchSnapshot,
        commit: &SegmentFetchCommit,
        finished_at_ms: u64,
    ) {
        let content_length = commit.content_length;
        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
            entry.status = SegmentCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
            session.advance_media_readiness_generation();
        }
        let reset_failures = session.record_successful_segment_fetch();
        self.metrics.record_segment_cached();
        if log::log_enabled!(log::Level::Debug) {
            let identity = super::HlsLogIdentity::from_session(session);
            debug!(
                "HLS segment cached: session={} proxy_session={} source=normal resource={} content_length={content_length}",
                identity.session(),
                identity.proxy_session(),
                snapshot.proxy_seq_log
            );
            if let Some(reset_failures) = reset_failures {
                debug!(
                    "HLS segment temporary failure counter reset: session={} proxy_session={} previous_failures={reset_failures}",
                    identity.session(),
                    identity.proxy_session()
                );
            }
        }
    }

    fn schedule_wake(self: &Arc<Self>, context: SegmentFetchContext, now_ms: u64) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            worker.wake_scheduler(context, now_ms).await;
        });
    }

    fn schedule_segment_retry(
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
                let Some(capacity) = super::cache::hls_cache_capacity_from_io(&error) else {
                    return CapacityRetryAdmission::LocalIoFailure;
                };
                revision = capacity.revision().clone();
            }
        }
    }
}

fn take_queued_segment_fetch_candidate(
    session: &mut super::HlsSession,
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

fn select_segment_key_dependency(
    session: &mut super::HlsSession,
    proxy_session_id: &ProxySessionId,
    proxy_seq: u64,
    encryption: Option<&HlsSegmentEncryption>,
    now_ms: u64,
) -> SegmentKeyDependencySelection {
    let Some(encryption) = encryption else {
        return SegmentKeyDependencySelection::Ready(None);
    };
    let selection = select_key_dependency(session, proxy_session_id, encryption, now_ms);
    if matches!(&selection, SegmentKeyDependencySelection::Unavailable) {
        mark_segment_discovered(session, proxy_seq);
    }
    selection
}

fn select_key_dependency(
    session: &mut super::HlsSession,
    proxy_session_id: &ProxySessionId,
    encryption: &HlsSegmentEncryption,
    now_ms: u64,
) -> SegmentKeyDependencySelection {
    let Some(resource) = session.transient.resources.get(&encryption.resource_id).cloned() else {
        return SegmentKeyDependencySelection::Unavailable;
    };
    if resource.kind != TransientResourceKind::Key
        || resource.file_ext_hint.as_deref() != Some(encryption.resource_extension.as_str())
        || !resource.is_valid_at(now_ms)
    {
        return SegmentKeyDependencySelection::Unavailable;
    }
    let resource_file = TransientResourceFile {
        resource_id: encryption.resource_id.clone(),
        extension: encryption.resource_extension.clone(),
    };
    let cache_duration_ms = session.transient.resource_ttl_ms;
    let decision = session.transient.begin_object_fetch(
        proxy_session_id,
        &resource,
        &encryption.resource_extension,
        now_ms,
        cache_duration_ms,
    );
    let dependency = match decision {
        TransientObjectFetchDecision::Ready => None,
        TransientObjectFetchDecision::Fetch(token) => {
            Some(SegmentKeyDependency::Fetch(Box::new(SegmentKeyFetchDependency {
                token: *token,
                resource,
                resource_file,
            })))
        }
        TransientObjectFetchDecision::Wait(notifier) => Some(SegmentKeyDependency::Wait {
            notifier,
            resource_id: encryption.resource_id.clone(),
            resource_extension: encryption.resource_extension.clone(),
        }),
    };
    SegmentKeyDependencySelection::Ready(dependency)
}

fn mark_segment_discovered(session: &mut super::HlsSession, proxy_seq: u64) {
    if let Some(entry) = session.segments.get_mut(&proxy_seq) {
        entry.status = SegmentCacheStatus::Discovered;
    }
}

fn commit_failed_segment_fetch(
    session: &mut super::HlsSession,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
    error: &SegmentFetchError,
    finished_at_ms: u64,
) {
    if !error.is_local_cache_capacity() {
        session.origin_control.path_condition = super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
    }
    let (status, invalidate_queued_origin_work) =
        failed_segment_status(session, snapshot, policy, error, finished_at_ms);
    if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
        entry.status = status;
    }
    if invalidate_queued_origin_work {
        session.invalidate_queued_origin_work();
        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
            entry.status = SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None };
        }
    }
}

fn failed_segment_status(
    session: &mut super::HlsSession,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
    error: &SegmentFetchError,
    finished_at_ms: u64,
) -> (SegmentCacheStatus, bool) {
    if error.is_local_cache_capacity() {
        return (
            SegmentCacheStatus::CapacityDeferred { priority: snapshot.priority, deferred_at_ms: finished_at_ms },
            false,
        );
    }
    if !error.retryable_failure() {
        return (
            SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: error.permanent_status() },
            false,
        );
    }
    let threshold = policy.permanent_failure_segment_threshold.max(1);
    let transition = session.record_temporary_segment_fetch_failure(
        finished_at_ms,
        HlsSegmentFailureObject::Normal { proxy_seq: snapshot.proxy_seq, origin_seq: snapshot.origin_seq },
        threshold,
    );
    match transition {
        HlsSegmentFailureTransition::StillRetryable { failures, threshold } => {
            if log::log_enabled!(log::Level::Debug) {
                let identity = super::HlsLogIdentity::from_session(session);
                debug!(
                    "HLS segment temporary failure counted: session={} proxy_session={} object={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    snapshot.proxy_seq_log,
                    failures,
                    threshold
                );
            }
            (SegmentCacheStatus::FailedRetryable { failed_at_ms: finished_at_ms, retry_after_ms: 1_000 }, false)
        }
        HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold } => {
            if log::log_enabled!(log::Level::Warn) {
                let identity = super::HlsLogIdentity::from_session(session);
                warn!(
                    "HLS segment temporary failure threshold reached: session={} proxy_session={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    failures,
                    threshold
                );
            }
            (SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None }, true)
        }
    }
}

impl Default for HlsSegmentWorkerPool {
    fn default() -> Self { Self::new(SegmentFetchPolicy::default()) }
}

async fn fetch_segment_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<SegmentFetchCommit, SegmentFetchError> {
    ensure_segment_key_dependency_ready(context, snapshot, policy).await?;
    fetch_segment_with_retries_into_cache(context, snapshot, policy).await
}

async fn ensure_segment_key_dependency_ready(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<(), SegmentFetchError> {
    let Some(dependency) = snapshot.key_dependency.clone() else {
        return Ok(());
    };
    match dependency {
        SegmentKeyDependency::Wait { notifier, resource_id, resource_extension } => {
            wait_for_segment_key_dependency(
                context,
                notifier,
                resource_id,
                resource_extension,
                policy.origin_object_wait_timeout(),
            )
            .await
        }
        SegmentKeyDependency::Fetch(dependency) => {
            let SegmentKeyFetchDependency { token, resource, resource_file } = *dependency;
            let binding = SegmentKeyBindingSnapshot::from(snapshot);
            fetch_segment_key_dependency_into_cache(context, &binding, policy, token, resource, resource_file).await
        }
    }
}

async fn wait_for_segment_key_dependency(
    context: &SegmentFetchContext,
    notifier: Arc<Notify>,
    resource_id: TransientResourceId,
    resource_extension: String,
    wait_timeout: Duration,
) -> Result<(), SegmentFetchError> {
    let deadline = Instant::now() + wait_timeout;
    loop {
        let wake = notifier.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let should_wait = {
            let now_ms = current_time_millis();
            let session = context.session.read().await;
            if session
                .transient
                .ready_key_object_valid_until_ms(&session.proxy_session_id, &resource_id, &resource_extension, now_ms)
                .is_some()
            {
                return Ok(());
            }
            let key = TransientPassthroughState::transient_object_key(
                &session.proxy_session_id,
                &resource_id,
                resource_extension.clone(),
            );
            matches!(
                session.transient.object_unavailable_state(&key, now_ms),
                TransientObjectUnavailableState::Fetching
            )
        };
        if !should_wait {
            return Err(SegmentFetchError::Timeout);
        }
        timeout_at(deadline, wake).await.map_err(|_| SegmentFetchError::Timeout)?;
    }
}

async fn fetch_segment_key_dependency_into_cache(
    context: &SegmentFetchContext,
    binding: &SegmentKeyBindingSnapshot,
    policy: &SegmentFetchPolicy,
    token: TransientObjectFetchToken,
    resource: TransientResourceRef,
    resource_file: TransientResourceFile,
) -> Result<(), SegmentFetchError> {
    let mut fetch_finalizer = HlsTransientObjectFetchFinalizer::new(
        context.session.clone(),
        Arc::clone(&context.segment_cache),
        token.clone(),
        1_000,
    );
    let (staged, origin_work) =
        stage_segment_key_dependency(context, policy, &token, &resource, &resource_file).await?;
    let generation_valid = origin_work.generation_valid && {
        let session = context.session.read().await;
        segment_key_dependency_generation_matches(
            &session,
            binding,
            &token,
            &resource,
            &resource_file,
            current_time_millis(),
        )
    };
    if !generation_valid {
        if let Err(err) = context.segment_cache.remove_staged(staged).await {
            warn!("HLS stale staged AES key cleanup failed: error={err}");
        }
        context.session.write().await.fail_transient_object_retryable_if_current(&token, current_time_millis(), 1_000);
        return Err(SegmentFetchError::Timeout);
    }
    let metadata = match context.segment_cache.commit_staged(token.cache_key(), staged).await {
        Ok(metadata) => metadata,
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                &token,
                current_time_millis(),
                1_000,
            );
            return Err(SegmentFetchError::cache_commit(&err));
        }
    };
    let ready_at_ms = current_time_millis();
    super::transient_fetcher::delete_superseded_transient_object(&context.segment_cache, &token).await;
    let mut session = context.session.write().await;
    if !segment_key_dependency_generation_matches(&session, binding, &token, &resource, &resource_file, ready_at_ms) {
        session.fail_transient_object_retryable_if_current(&token, ready_at_ms, 1_000);
        return Err(SegmentFetchError::Timeout);
    }
    let expires_at_ms = ready_at_ms.saturating_add(session.transient.resource_ttl_ms).max(resource.expires_at_ms);
    if !session.commit_transient_object_ready_if_current(
        TransientResourceKind::Key,
        &token,
        resource.content_type_hint.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
        metadata.size,
        ready_at_ms,
        expires_at_ms,
    ) {
        return Err(SegmentFetchError::Timeout);
    }
    fetch_finalizer.complete();
    Ok(())
}

async fn stage_segment_key_dependency(
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    token: &TransientObjectFetchToken,
    resource: &TransientResourceRef,
    resource_file: &TransientResourceFile,
) -> Result<(StagedCacheObject, SegmentOriginWorkFinish), SegmentFetchError> {
    let request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: resource.resolved_origin_uri.clone(),
        origin_headers: context.headers.clone(),
        origin_provider_session_headers: context.origin_provider_session_headers.clone(),
        range_header: None,
        resource_file: resource_file.clone(),
        resource_kind: TransientResourceKind::Key,
        clients: HlsOriginResourceClients {
            client: context.client.clone(),
            no_redirect_client: context.no_redirect_client.clone(),
            use_manual_redirects: context.use_manual_redirects,
        },
        policy: policy.clone(),
        log_identity: {
            let session = context.session.read().await;
            super::HlsLogIdentity::from_session(&session)
        },
    };
    let prepare_context = context.clone();
    let prepare_policy = policy.clone();
    let response = fetch_hls_transient_origin_response_with_attempt_prepare(request, move |_attempt| {
        let context = prepare_context.clone();
        let policy = prepare_policy.clone();
        async move { prepare_segment_origin_attempt(context, policy).await }.boxed()
    })
    .await;
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                token,
                current_time_millis(),
                1_000,
            );
            return Err(err);
        }
    };
    let staged = context
        .segment_cache
        .stage_temp_with_deadline(token.cache_key(), response.decoded.body, response.body_deadline.deadline())
        .await;
    let origin_work = finish_segment_origin_attempt(context.clone(), response.guard).await;
    let staged = match staged {
        Ok(staged) if staged.size == 16 => staged,
        Ok(staged) => {
            if let Err(err) = context.segment_cache.remove_staged(staged).await {
                warn!("HLS invalid staged AES key cleanup failed: error={err}");
            }
            context.session.write().await.fail_transient_object_permanent_if_current(
                token,
                current_time_millis(),
                Some(axum::http::StatusCode::BAD_GATEWAY),
            );
            return Err(SegmentFetchError::NonRetryableStatus(axum::http::StatusCode::BAD_GATEWAY));
        }
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                token,
                current_time_millis(),
                1_000,
            );
            return Err(SegmentFetchError::cache_body(&err));
        }
    };
    Ok((staged, origin_work))
}

fn segment_key_dependency_generation_matches(
    session: &super::HlsSession,
    binding: &SegmentKeyBindingSnapshot,
    token: &TransientObjectFetchToken,
    resource: &TransientResourceRef,
    resource_file: &TransientResourceFile,
    now_ms: u64,
) -> bool {
    if session.activity.origin_work_generation != binding.origin_work_generation
        || !session.transient.object_fetch_token_matches(token)
    {
        return false;
    }
    let segment_matches = session.segments.get(&binding.proxy_seq).is_some_and(|segment| {
        segment.cache_key == binding.cache_key
            && segment.encryption.as_ref().is_some_and(|encryption| {
                encryption.resource_id == resource_file.resource_id
                    && encryption.resource_extension == resource_file.extension
            })
    });
    let resource_matches = session.transient.resources.get(&resource_file.resource_id).is_some_and(|current| {
        current.kind == TransientResourceKind::Key
            && current.resolved_origin_uri == resource.resolved_origin_uri
            && current.file_ext_hint.as_deref() == Some(resource_file.extension.as_str())
            && current.is_valid_at(now_ms)
    });
    segment_matches && resource_matches
}

struct SegmentOriginAttemptGuard {
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
}

async fn prepare_segment_origin_attempt(
    context: SegmentFetchContext,
    policy: SegmentFetchPolicy,
) -> Result<SegmentOriginAttemptGuard, SegmentFetchError> {
    let started_generation = start_segment_origin_work(&context).await;
    let binding =
        if context.origin_io.is_some() { context.session.read().await.origin_account_binding.clone() } else { None };
    let provider_lease = if let (Some(origin_io), Some(binding)) = (context.origin_io.as_ref(), binding.as_ref()) {
        if binding.is_detached() {
            finish_segment_origin_work(&context, started_generation).await;
            touch_segment_origin_account_binding(&context, false).await;
            return Err(SegmentFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Detached));
        }
        let guard = match begin_hls_origin_account_io_bounded(
            origin_io,
            &context.session,
            binding,
            hls_object_body_deadline(policy.origin_segment_timeout_ms),
        )
        .await
        {
            Ok(guard) => guard,
            Err(err) => {
                finish_segment_origin_work(&context, started_generation).await;
                touch_segment_origin_account_binding(&context, false).await;
                return Err(SegmentFetchError::ProviderUnavailable(err));
            }
        };
        Some((origin_io.clone(), guard))
    } else {
        None
    };
    Ok(SegmentOriginAttemptGuard { started_generation, provider_lease })
}

async fn finish_segment_origin_attempt(
    context: SegmentFetchContext,
    guard: SegmentOriginAttemptGuard,
) -> SegmentOriginWorkFinish {
    finish_segment_origin_io(&context, guard.started_generation, guard.provider_lease).await
}

async fn finish_segment_origin_io(
    context: &SegmentFetchContext,
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
) -> SegmentOriginWorkFinish {
    let origin_work = finish_segment_origin_work(context, started_generation).await;
    if let Some((origin_io, guard)) = provider_lease {
        finish_hls_origin_account_io(
            &origin_io,
            &context.session,
            guard,
            origin_work.generation_valid && origin_work.refresh_reservation,
        )
        .await;
        touch_segment_origin_account_binding(context, origin_work.generation_valid && origin_work.refresh_reservation)
            .await;
    }
    origin_work
}

async fn start_segment_origin_work(context: &SegmentFetchContext) -> Option<u64> {
    context.origin_io.as_ref()?;
    let mut session = context.session.write().await;
    Some(session.start_origin_work())
}

async fn finish_segment_origin_work(
    context: &SegmentFetchContext,
    started_generation: Option<u64>,
) -> SegmentOriginWorkFinish {
    let Some(started_generation) = started_generation else {
        return SegmentOriginWorkFinish { generation_valid: true, refresh_reservation: false };
    };
    let mut session = context.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(current_time_millis());
    SegmentOriginWorkFinish { generation_valid, refresh_reservation }
}

async fn touch_segment_origin_account_binding(context: &SegmentFetchContext, reservation_refreshed: bool) {
    let mut session = context.session.write().await;
    if let Some(binding) = session.origin_account_binding.as_mut() {
        let now_ms = current_time_millis();
        binding.last_origin_io_at_ms = Some(now_ms);
        if reservation_refreshed {
            binding.last_reservation_refresh_at_ms = Some(now_ms);
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn fetch_segment_with_retries_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<SegmentFetchCommit, SegmentFetchError> {
    let headers = build_segment_origin_headers(
        &context.headers,
        &context.origin_provider_session_headers,
        snapshot.fetch_ref.byte_range,
    )?;
    let target = HlsOriginResourceFetchTarget {
        kind: HlsResourceFetchKind::Segment,
        source: HlsResourceFetchSource::Normal,
        object_id: snapshot.proxy_seq_log.clone(),
        origin_url: snapshot.fetch_ref.resolved_origin_url.clone(),
        headers,
        byte_range_expectation: if snapshot.fetch_ref.byte_range.is_some() {
            HlsOriginByteRangeExpectation::PartialContent
        } else {
            HlsOriginByteRangeExpectation::FullObject
        },
    };
    let clients = HlsOriginResourceClients {
        client: context.client.clone(),
        no_redirect_client: context.no_redirect_client.clone(),
        use_manual_redirects: context.use_manual_redirects,
    };
    let log_identity = {
        let session = context.session.read().await;
        super::HlsLogIdentity::from_session(&session)
    };
    let context = context.clone();
    let snapshot = snapshot.clone();
    let policy_for_prepare = policy.clone();
    let prepare_context = context.clone();
    let cleanup_context = context.clone();
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &log_identity,
        move |_attempt| {
            let context = prepare_context.clone();
            let policy = policy_for_prepare.clone();
            async move { prepare_segment_origin_attempt(context, policy).await }.boxed()
        },
        move |guard| {
            let context = cleanup_context.clone();
            async move {
                finish_segment_origin_attempt(context, guard).await;
            }
            .boxed()
        },
        move |response, _attempt, body_deadline, guard| {
            let context = context.clone();
            let snapshot = snapshot.clone();
            async move {
                let commit_result =
                    commit_segment_response_into_cache(&context, &snapshot, response, body_deadline).await;
                let origin_work = finish_segment_origin_attempt(context, guard).await;
                commit_result.map(|metadata| SegmentFetchCommit {
                    content_length: metadata.size,
                    generation_valid: origin_work.generation_valid,
                })
            }
            .boxed()
        },
    )
    .await
}

async fn commit_segment_response_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    response: DecodedHttpResponse,
    body_deadline: HlsOriginResourceBodyDeadline,
) -> Result<CachedSegmentMetadata, SegmentFetchError> {
    let (proxy_session_id, log_identity) = {
        let session = context.session.read().await;
        (session.proxy_session_id.clone(), super::HlsLogIdentity::from_session(&session))
    };
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Normal,
        log_identity,
        proxy_session_id,
        hls_access_lease_id: context.repair_access_lease_id.clone(),
        rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: snapshot.proxy_seq },
        resource_id: format!("{:06}", snapshot.proxy_seq),
        file_ext: snapshot.proxy_file_ext.clone(),
        // Segment repair uses the concrete fetch URL for diagnostics/postprocess metadata only.
        origin_fetch_uri_for_diagnostics: snapshot.fetch_ref.resolved_origin_url.clone(),
        media_sequence: Some(snapshot.origin_seq),
        discontinuity_sequence: None,
        complete_object: snapshot.complete_object,
        encrypted: snapshot.encryption.is_some(),
        custom_response: false,
    };
    if let Some(content_length) = reliable_decoded_content_length(&response) {
        context
            .segment_cache
            .ensure_projected_write_capacity(&snapshot.cache_key, content_length)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?;
    }
    context
        .segment_repair
        .commit_origin_response(
            &context.segment_cache,
            &snapshot.cache_key,
            response.body,
            body_deadline.deadline(),
            repair_context,
        )
        .await
        .map_err(|err| SegmentFetchError::cache_body(&err))
}

fn reliable_decoded_content_length(response: &DecodedHttpResponse) -> Option<u64> {
    if response.was_content_decoded() {
        return None;
    }
    response.headers.get(axum::http::header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

fn build_segment_origin_headers(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, SegmentFetchError> {
    build_hls_origin_resource_headers(source_headers, provider_session_headers, byte_range)
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        build_segment_origin_headers, commit_failed_segment_fetch, take_queued_segment_fetch_candidate,
        wait_for_segment_key_dependency, HlsSegmentFetchWorkload, SegmentFetchContext, SegmentFetchPolicy,
        SegmentFetchPriority,
    };
    use crate::{
        build_rewrite_secret_fingerprint, GarbageCollectionPolicy, HlsAccessLease, HlsAccessLeaseId,
        HlsGarbageCollector, HlsOriginResourceFetchError, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSegmentFile,
        HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, ProxySessionId,
        RenderedManifest, SegmentCacheStatus, TimelineMapError, TransientObjectFetchDecision,
        TransientObjectFetchToken, TransientResourceId, TransientResourceKind, TransientResourceRef,
    };
    use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZstdEncoder};
    use axum::http::{header, HeaderMap, HeaderValue};
    use futures::poll;
    use shared::model::{HlsCacheConfigDto, HlsSegmentRepairMode, HlsStripMode};
    use std::{
        collections::VecDeque,
        io::Cursor,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::Poll,
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::{oneshot, Mutex, Notify},
    };
    use tuliprox_core::model::{HlsCacheConfig, HlsSegmentRepairConfig};
    use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    #[test]
    fn segment_fetch_budget_accounts_for_serialized_key_and_media_retry_chains() {
        let policy = SegmentFetchPolicy {
            origin_segment_timeout_ms: 10_000,
            effective_repair_postprocess_timeout_ms: 2_000,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            ..SegmentFetchPolicy::default()
        };

        assert_eq!(policy.demand_wait_timeout_for(HlsSegmentFetchWorkload::Clear), Duration::from_millis(63_100));
        assert_eq!(
            policy.demand_wait_timeout_for(HlsSegmentFetchWorkload::EncryptedWithKey),
            Duration::from_millis(125_200)
        );
        assert_eq!(policy.demand_wait_timeout(), Duration::from_millis(125_200));
        assert_eq!(policy.origin_object_wait_timeout(), Duration::from_millis(63_100));
    }

    #[test]
    fn hls_recovery_timing_expected_object_eta_is_not_the_retry_chain_timeout() {
        let policy = SegmentFetchPolicy {
            origin_segment_timeout_ms: 10_000,
            effective_repair_postprocess_timeout_ms: 2_000,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            ..SegmentFetchPolicy::default()
        };

        assert_eq!(policy.recovery_object_eta_ms(), 13_000);
        assert!(policy.recovery_object_eta_ms() < policy.workload_budget_ms(HlsSegmentFetchWorkload::Clear));
        assert!(policy.recovery_object_eta_ms() < policy.workload_budget_ms(HlsSegmentFetchWorkload::EncryptedWithKey));

        let long_hard_timeout = SegmentFetchPolicy {
            origin_segment_timeout_ms: 120_000,
            effective_repair_postprocess_timeout_ms: 30_000,
            ..policy
        };
        assert_eq!(long_hard_timeout.recovery_object_eta_ms(), 13_000);
        assert!(
            long_hard_timeout.recovery_object_eta_ms()
                < long_hard_timeout.workload_budget_ms(HlsSegmentFetchWorkload::Clear)
        );
    }

    #[tokio::test]
    async fn popped_candidate_without_usable_fetch_binding_returns_to_discovered() {
        let server = spawn_sequence_response_server(vec![TestOriginResponse {
            status: 200,
            headers: Vec::new(),
            body: b"unused".to_vec(),
        }])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy::default();
        let (_, context, _) = fetch_context(&server, &temp_dir, &policy).await;
        let mut session = context.session.write().await;
        let entry = session.segments.get_mut(&1).expect("segment");
        entry.status = SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
        entry.origin_fetch_ref = None;

        assert!(
            take_queued_segment_fetch_candidate(&mut session, 1, SegmentFetchPriority::Prefetch, 10, true,).is_none()
        );
        assert!(matches!(session.segments[&1].status, SegmentCacheStatus::Discovered));
    }

    #[tokio::test]
    async fn popped_candidate_with_expired_fetch_binding_returns_to_discovered() {
        let server = spawn_sequence_response_server(vec![TestOriginResponse {
            status: 200,
            headers: Vec::new(),
            body: b"unused".to_vec(),
        }])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy::default();
        let (_, context, _) = fetch_context(&server, &temp_dir, &policy).await;
        let mut session = context.session.write().await;
        let entry = session.segments.get_mut(&1).expect("segment");
        entry.status = SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
        entry.origin_fetch_ref.as_mut().expect("fetch binding").valid_until_ms = Some(9);

        assert!(
            take_queued_segment_fetch_candidate(&mut session, 1, SegmentFetchPriority::Prefetch, 10, true,).is_none()
        );
        assert!(matches!(session.segments[&1].status, SegmentCacheStatus::Discovered));
    }

    #[test]
    fn object_failure_threshold_is_independent_from_initial_strip() {
        let mut without_strip = HlsCacheConfigDto::default();
        without_strip.strip.mode = HlsStripMode::Segments;
        without_strip.strip.value = 0;
        let mut with_large_strip = without_strip.clone();
        with_large_strip.strip.mode = HlsStripMode::Seconds;
        with_large_strip.strip.value = u64::MAX;

        let without_strip = SegmentFetchPolicy::from_config(&HlsCacheConfig::from(&without_strip));
        let with_large_strip = SegmentFetchPolicy::from_config(&HlsCacheConfig::from(&with_large_strip));

        assert_eq!(
            without_strip.permanent_failure_segment_threshold,
            with_large_strip.permanent_failure_segment_threshold,
        );
        assert_eq!(
            without_strip.permanent_failure_segment_threshold,
            SegmentFetchPolicy::default().permanent_failure_segment_threshold
        );
    }

    #[tokio::test]
    async fn local_capacity_failure_does_not_change_origin_progress_or_failure_counter() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;
        let snapshot = worker.next_fetch_snapshot(&context, 20, &policy).await.expect("fetch snapshot");
        let error = HlsOriginResourceFetchError::LocalCacheCapacity {
            required_session_bytes: 10,
            required_global_bytes: 0,
            projected_write_bytes: 10,
            revision: context.segment_cache.capacity_revision(),
        };
        let mut session = context.session.write().await;
        let original_path_condition = session.origin_control.path_condition;

        commit_failed_segment_fetch(&mut session, &snapshot, &policy, &error, 21);

        assert_eq!(session.origin_control.path_condition, original_path_condition);
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
        assert!(matches!(
            session.segments.get(&snapshot.proxy_seq).map(|segment| &segment.status),
            Some(SegmentCacheStatus::CapacityDeferred { .. })
        ));
    }

    #[tokio::test]
    async fn capacity_deferred_segment_requeues_only_after_capacity_revision_changes() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;
        {
            let mut session = context.session.write().await;
            for proxy_seq in [2_u64, 3] {
                session.segments.get_mut(&proxy_seq).expect("ready tail segment").status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 19 };
            }
            assert!(session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Demand, 20));
        }
        let snapshot = worker.next_fetch_snapshot(&context, 20, &policy).await.expect("fetch snapshot");
        let notifier = {
            let mut session = context.session.write().await;
            session.segment_fetch_notifiers.entry(snapshot.proxy_seq).or_default().clone()
        };
        let completion = {
            let revision = context.segment_cache.capacity_revision();
            let error = HlsOriginResourceFetchError::LocalCacheCapacity {
                required_session_bytes: 1,
                required_global_bytes: 0,
                projected_write_bytes: 1,
                revision,
            };
            let mut session = context.session.write().await;
            worker.apply_segment_fetch_result(&mut session, &snapshot, &policy, Err(error), 21)
        };
        let retry = completion.scheduled_retry.expect("capacity deferral owns a revision retry");
        assert!(completion.notifier.is_none(), "deferred work retains its existing demand notifier");
        worker.schedule_segment_retry(context.clone(), snapshot, retry);
        let first_demand = worker.demand_fetch_and_wait(context.clone(), &segment_file, 22);
        let repeated_demand = worker.demand_fetch_and_wait(context.clone(), &segment_file, 23);
        tokio::pin!(first_demand, repeated_demand);
        assert!(matches!(futures::poll!(first_demand.as_mut()), std::task::Poll::Pending));
        assert!(matches!(futures::poll!(repeated_demand.as_mut()), std::task::Poll::Pending));
        assert!(server.requests.lock().await.is_empty(), "capacity wait does not poll the origin");

        let completed = notifier.notified();
        tokio::pin!(completed);
        completed.as_mut().enable();
        // The revision may change before the spawned waiter first polls. Its
        // token comparison must make that race lossless.
        context.segment_cache.notify_capacity_protection_changed();
        let ((), first_demand_outcome, repeated_demand_outcome) =
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(completed.as_mut(), first_demand.as_mut(), repeated_demand.as_mut())
            })
            .await
            .expect("revision wake completes one requeued fetch");

        assert_eq!(first_demand_outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(repeated_demand_outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.len(), 1);
        let session = context.session.read().await;
        assert!(matches!(
            session.segments.get(&1).map(|segment| &segment.status),
            Some(SegmentCacheStatus::Ready { .. })
        ));
        let rendered = session.last_rendered_manifest.as_ref().expect("recovered timeline renders a manifest");
        assert_eq!(rendered.first_proxy_seq, 1);
        assert_eq!(rendered.last_proxy_seq, 3);
        assert!(rendered.body.contains("000001.ts"));
    }

    async fn wait_for_concurrent_capacity_deferral(context: &SegmentFetchContext, server: &TestSegmentServer) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let both_deferred = {
                    let session = context.session.read().await;
                    [2_u64, 3].into_iter().all(|proxy_seq| {
                        matches!(
                            session.segments.get(&proxy_seq).map(|segment| &segment.status),
                            Some(SegmentCacheStatus::CapacityDeferred { .. })
                        )
                    })
                };
                if both_deferred && server.requests.lock().await.len() >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both capacity failures become revision-bound deferrals");
    }

    async fn assert_segment_request_counts(server: &TestSegmentServer, expected: usize, failure_context: &str) {
        let requests = server.requests.lock().await;
        for proxy_seq in [2_u64, 3] {
            assert_eq!(
                requests.iter().filter(|request| request.starts_with(&format!("GET /{proxy_seq}.ts "))).count(),
                expected,
                "{failure_context}: segment {proxy_seq}"
            );
        }
    }

    async fn assert_concurrent_capacity_recovery_ready(context: &SegmentFetchContext) {
        let session = context.session.read().await;
        for proxy_seq in [2_u64, 3, 4] {
            assert!(matches!(
                session.segments.get(&proxy_seq).map(|segment| &segment.status),
                Some(SegmentCacheStatus::Ready { .. })
            ));
        }
        let rendered = session.last_rendered_manifest.as_ref().expect("recovered timeline renders");
        assert_eq!(rendered.first_proxy_seq, 2);
        assert_eq!(rendered.last_proxy_seq, 4);
        assert!(rendered.body.contains("000002.ts"));
        assert!(rendered.body.contains("000003.ts"));
    }

    #[tokio::test]
    async fn concurrent_capacity_deferred_segments_retry_once_after_real_cache_release() {
        let response = TestOriginResponse { status: 200, headers: Vec::new(), body: b"12345".to_vec() };
        let server = spawn_sequence_response_server(vec![response; 4]).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
        let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;
        {
            let mut session = context.session.write().await;
            session
                .apply_origin_manifest(&normal_manifest(&format!(
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{0}/1.ts\n#EXTINF:4.0,\n{0}/2.ts\n#EXTINF:4.0,\n{0}/3.ts\n#EXTINF:4.0,\n{0}/4.ts\n",
                    server.base_url
                )))
                .expect("four-segment manifest maps");
        }
        clear_scheduled_prefetch(&context, &policy).await;
        let (released_key, tail_key) = {
            let session = context.session.read().await;
            (
                session.segments.get(&1).expect("released segment").cache_key.clone(),
                session.segments.get(&4).expect("ready tail segment").cache_key.clone(),
            )
        };
        context
            .segment_cache
            .write_bytes_and_commit(&released_key, b"12345678901")
            .await
            .expect("released fixture commits");
        context.segment_cache.write_bytes_and_commit(&tail_key, b"t").await.expect("tail fixture commits");
        {
            let mut session = context.session.write().await;
            session.segments.get_mut(&1).expect("released segment").status =
                SegmentCacheStatus::Ready { content_length: 11, ready_at_ms: 10 };
            session.segments.get_mut(&4).expect("ready tail segment").status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
        }
        context.segment_cache.update_cache_limits(100, 12);

        let first_worker = Arc::clone(&worker);
        let first_context = context.clone();
        let first = tokio::spawn(async move {
            first_worker
                .demand_fetch_and_wait(first_context, &HlsSegmentFile { proxy_seq: 2, extension: "ts".to_string() }, 20)
                .await
        });
        let second_worker = Arc::clone(&worker);
        let second_context = context.clone();
        let second = tokio::spawn(async move {
            second_worker
                .demand_fetch_and_wait(
                    second_context,
                    &HlsSegmentFile { proxy_seq: 3, extension: "ts".to_string() },
                    20,
                )
                .await
        });

        wait_for_concurrent_capacity_deferral(&context, &server).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_segment_request_counts(&server, 1, "initial capacity deferral").await;

        for _ in 0..3 {
            context.segment_cache.notify_capacity_protection_changed();
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
        }
        assert_segment_request_counts(&server, 1, "insufficient protection revision").await;
        {
            let session = context.session.read().await;
            assert!([2_u64, 3].into_iter().all(|proxy_seq| {
                session.segments.get(&proxy_seq).is_some_and(|segment| segment.status.awaits_capacity_recovery())
            }));
        }

        {
            let mut session = context.session.write().await;
            session.segments.get_mut(&1).expect("completed segment").status = SegmentCacheStatus::Expired;
            session.publishable_origin_head_proxy_seq = Some(2);
            session.advance_media_readiness_generation();
        }
        context.segment_cache.delete(&released_key).await.expect("completed cache object is released");

        let (first, second) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(first, second) })
            .await
            .expect("both deferred fetches resume after the accounting revision");
        assert_eq!(first.expect("first demand task joins"), super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(second.expect("second demand task joins"), super::SegmentDemandFetchOutcome::Ready);
        assert_segment_request_counts(&server, 2, "real capacity release").await;
        assert_concurrent_capacity_recovery_ready(&context).await;
    }

    #[tokio::test]
    async fn failed_retryable_segment_has_a_bounded_demand_requeue_contract() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;
        context.session.write().await.segments.get_mut(&segment_file.proxy_seq).expect("segment").status =
            SegmentCacheStatus::FailedRetryable { failed_at_ms: 20, retry_after_ms: 1_000 };

        let early = worker.demand_fetch_and_wait(context.clone(), &segment_file, 1_019).await;
        assert_eq!(early, super::SegmentDemandFetchOutcome::TimedOut);
        assert!(server.requests.lock().await.is_empty());

        let retried = worker.demand_fetch_and_wait(context.clone(), &segment_file, 1_020).await;
        assert_eq!(retried, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    fn normal_manifest(body: &str) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, BASE_URL) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
        Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }))
    }

    #[test]
    fn segment_origin_headers_remove_client_range_and_force_identity() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
        let headers = build_segment_origin_headers(&headers, &HeaderMap::new(), None).expect("headers should build");

        assert!(!headers.contains_key(header::RANGE));
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key(header::HOST));
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
    }

    #[test]
    fn segment_origin_headers_apply_byterange() {
        let headers = build_segment_origin_headers(
            &HeaderMap::new(),
            &HeaderMap::new(),
            Some(tuliprox_parser::hls::origin_manifest::ParsedByteRange { offset: 10, length: 5 }),
        )
        .expect("headers should build");

        assert_eq!(headers.get(header::RANGE).expect("range"), "bytes=10-14");
    }

    #[tokio::test]
    async fn segment_fetch_snapshot_uses_concrete_final_segment_fetch_url() {
        let manifest = match parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nmedia/seg001.ts\n",
            "https://cdn.example.net/live/redirected/playlist.m3u8",
        ) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        };
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.configure_segment_prefetch_queue(SegmentFetchPolicy::default().max_prefetch_queue_depth);
            session.apply_origin_manifest(&manifest).expect("manifest maps");
            session.queue_manifest_prefetch_candidates(10);
        }

        let worker = Arc::new(HlsSegmentWorkerPool::new(SegmentFetchPolicy::default()));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let context = SegmentFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
            segment_repair: test_segment_repair_manager(),
            repair_access_lease_id: None,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
            origin_io: None,
        };

        let snapshot =
            worker.next_fetch_snapshot(&context, 11, &SegmentFetchPolicy::default()).await.expect("segment snapshot");

        assert_eq!(snapshot.fetch_ref.resolved_origin_url, "https://cdn.example.net/live/redirected/media/seg001.ts");
        assert_eq!(snapshot.fetch_ref.byte_range, None);
        assert_eq!(snapshot.origin_seq, 10);
    }

    struct TestSegmentServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestSegmentServer {
        fn drop(&mut self) { self.task.abort(); }
    }

    #[derive(Clone)]
    struct TestOriginResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestOriginResponse {
        fn encoded(content_encoding: &str, body: Vec<u8>) -> Self {
            Self { status: 200, headers: vec![("Content-Encoding".to_string(), content_encoding.to_string())], body }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum TestContentEncoding {
        Gzip,
        RawDeflate,
        Brotli,
        Zstd,
    }

    impl TestContentEncoding {
        const fn header_value(self) -> &'static str {
            match self {
                Self::Gzip => "gzip",
                Self::RawDeflate => "deflate",
                Self::Brotli => "br",
                Self::Zstd => "zstd",
            }
        }
    }

    async fn encode_test_body(body: &[u8], encoding: TestContentEncoding) -> Vec<u8> {
        let reader = BufReader::new(Cursor::new(body.to_vec()));
        let mut encoded = Vec::new();
        match encoding {
            TestContentEncoding::Gzip => GzipEncoder::new(reader).read_to_end(&mut encoded).await,
            TestContentEncoding::RawDeflate => DeflateEncoder::new(reader).read_to_end(&mut encoded).await,
            TestContentEncoding::Brotli => BrotliEncoder::new(reader).read_to_end(&mut encoded).await,
            TestContentEncoding::Zstd => ZstdEncoder::new(reader).read_to_end(&mut encoded).await,
        }
        .expect("test body encodes");
        encoded
    }

    async fn spawn_sequence_response_server(responses: Vec<TestOriginResponse>) -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let task_requests = Arc::clone(&requests);
        let task_responses = Arc::clone(&responses);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let responses = Arc::clone(&task_responses);
                tokio::spawn(async move {
                    use std::fmt::Write as _;

                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 2048];
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                    }
                    requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                    let response = responses.lock().await.pop_front().unwrap_or(TestOriginResponse {
                        status: 500,
                        headers: Vec::new(),
                        body: Vec::new(),
                    });
                    let reason = if response.status == 200 { "OK" } else { "Status" };
                    let mut head =
                        format!("HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n", response.status, response.body.len());
                    for (name, value) in response.headers {
                        let _ = write!(head, "{name}: {value}\r\n");
                    }
                    head.push_str("Connection: close\r\n\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&response.body).await;
                });
            }
        });

        TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
    }

    fn temp_cache_files(root: &Path) -> Vec<PathBuf> {
        fn visit(path: &Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, found);
                } else if path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.contains(".tmp.")) {
                    found.push(path);
                }
            }
        }

        let mut found = Vec::new();
        visit(root, &mut found);
        found
    }

    async fn spawn_segment_server(delay_ms: u64) -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 2048];
                    let mut used = 0_usize;
                    loop {
                        let Ok(read) = socket.read(&mut buf[used..]).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        used += read;
                        if used >= 4 && buf[..used].windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if used == buf.len() {
                            return;
                        }
                    }
                    let request = String::from_utf8_lossy(&buf[..used]).to_string();
                    requests.lock().await.push(request.clone());
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                    let body = format!("body:{path}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
    }

    async fn spawn_controlled_segment_server() -> (TestSegmentServer, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 2048];
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            task_requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
            let _ = request_seen_tx.send(());
            if release_rx.await.is_err() {
                return;
            }
            let body = b"controlled-segment";
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
        });
        (TestSegmentServer { base_url: format!("http://{addr}"), requests, task }, request_seen_rx, release_tx)
    }

    async fn spawn_sequence_status_server(statuses: Vec<u16>) -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let statuses = Arc::new(Mutex::new(VecDeque::from(statuses)));
        let task_requests = Arc::clone(&requests);
        let task_statuses = Arc::clone(&statuses);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let statuses = Arc::clone(&task_statuses);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    requests.lock().await.push(request.clone());
                    let status = statuses.lock().await.pop_front().unwrap_or(200);
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let body = if status == 200 { "segment-body" } else { "" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
    }

    async fn spawn_redirect_retry_server() -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let request_count = Arc::new(AtomicUsize::new(0));
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let request_count = Arc::clone(&request_count);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                    requests.lock().await.push(path.to_string());
                    let count = request_count.fetch_add(1, Ordering::SeqCst);
                    let response = if path == "/1.ts" && count == 0 {
                        "HTTP/1.1 302 Found\r\nLocation: /redirected.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if path == "/redirected.ts" {
                        "HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsegment-body".to_string()
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
    }

    async fn spawn_range_segment_server() -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0_u8; 2048];
            let Ok(read) = socket.read(&mut buf).await else {
                return;
            };
            if read == 0 {
                return;
            }
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            task_requests.lock().await.push(request.clone());
            let body = if request.to_ascii_lowercase().contains("range: bytes=10-14") { "seg!!" } else { "bad" };
            let status = if body == "seg!!" { "206 Partial Content" } else { "200 OK" };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes 10-14/20\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
    }

    async fn grant_usable_worker_access_lease(worker: &HlsSegmentWorkerPool, proxy_session_id: &ProxySessionId) {
        worker.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
            HlsAccessLeaseId("worker-lease".to_string()),
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            10,
            15_000,
        ));
    }

    async fn fetch_context_with_access_lease(
        server: &TestSegmentServer,
        temp_dir: &tempfile::TempDir,
        policy: &SegmentFetchPolicy,
        grant_access_lease: bool,
    ) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
            session.proxy_next_seq = Some(1);
            session
                .apply_origin_manifest(&normal_manifest(&format!(
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{}/1.ts\n#EXTINF:4.0,\n{}/2.ts\n#EXTINF:4.0,\n{}/3.ts\n",
                    server.base_url, server.base_url, server.base_url
                )))
                .expect("manifest maps");
            session.queue_manifest_prefetch_candidates(10);
        }
        let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
        if grant_access_lease {
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
        }
        let context = SegmentFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
            segment_repair: test_segment_repair_manager(),
            repair_access_lease_id: None,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: true,
            origin_io: None,
        };
        (worker, context, HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() })
    }

    async fn fetch_context(
        server: &TestSegmentServer,
        temp_dir: &tempfile::TempDir,
        policy: &SegmentFetchPolicy,
    ) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
        fetch_context_with_access_lease(server, temp_dir, policy, true).await
    }

    async fn encrypted_fetch_context(
        server: &TestSegmentServer,
        temp_dir: &tempfile::TempDir,
        policy: &SegmentFetchPolicy,
    ) -> (Arc<HlsSegmentWorkerPool>, SegmentFetchContext, HlsSegmentFile) {
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        let key_uri = format!("{}/key.key", server.base_url);
        let key_resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            &key_uri,
            b"secret",
            0,
            u64::MAX,
            Some("key".to_string()),
        );
        let mut manifest = normal_manifest(&format!(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n\
             #EXTINF:4.0,\n{}/1.ts\n#EXTINF:4.0,\n{}/2.ts\n#EXTINF:4.0,\n{}/3.ts\n",
            server.base_url, server.base_url, server.base_url
        ));
        for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
            encryption.proxy_resource_id = Some(key_resource.id.0.clone());
            encryption.proxy_resource_extension = Some("key".to_string());
        }
        {
            let mut session = session.write().await;
            session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
            session.proxy_next_seq = Some(1);
            session.transient.upsert_resources([key_resource]);
            session.apply_origin_manifest(&manifest).expect("encrypted manifest maps");
        }
        let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
        let context = SegmentFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
            segment_repair: test_segment_repair_manager(),
            repair_access_lease_id: None,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::new(),
            use_manual_redirects: false,
            origin_io: None,
        };
        (worker, context, HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() })
    }

    async fn shared_key_fetch_and_wait(
        context: &SegmentFetchContext,
        now_ms: u64,
    ) -> (TransientObjectFetchToken, Arc<Notify>, TransientResourceId, String) {
        let mut session = context.session.write().await;
        let resource = session
            .transient
            .resources
            .values()
            .find(|resource| resource.kind == TransientResourceKind::Key)
            .cloned()
            .expect("encrypted fixture key resource");
        let extension = resource.file_ext_hint.clone().expect("key resource extension");
        let proxy_session_id = session.proxy_session_id.clone();
        let cache_duration_ms = session.transient.resource_ttl_ms;
        let fetch_token = match session.transient.begin_object_fetch(
            &proxy_session_id,
            &resource,
            &extension,
            now_ms,
            cache_duration_ms,
        ) {
            TransientObjectFetchDecision::Fetch(token) => token,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                panic!("first shared-key decision must own the fetch")
            }
        };
        let notifier = match session.transient.begin_object_fetch(
            &proxy_session_id,
            &resource,
            &extension,
            now_ms,
            cache_duration_ms,
        ) {
            TransientObjectFetchDecision::Wait(notifier) => notifier,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Fetch(_) => {
                panic!("second shared-key decision must wait for the existing fetch")
            }
        };
        (*fetch_token, notifier, resource.id, extension)
    }

    async fn commit_test_key_ready(context: &SegmentFetchContext, token: &TransientObjectFetchToken, now_ms: u64) {
        assert!(context.session.write().await.commit_transient_object_ready_if_current(
            TransientResourceKind::Key,
            token,
            "application/octet-stream".to_string(),
            16,
            now_ms,
            u64::MAX,
        ));
    }

    async fn clear_scheduled_prefetch(context: &SegmentFetchContext, policy: &SegmentFetchPolicy) {
        let mut session = context.session.write().await;
        session.segment_prefetch_queue = crate::SegmentPrefetchQueue::new(policy.max_prefetch_queue_depth);
        for segment in session.segments.values_mut() {
            if !matches!(segment.status, SegmentCacheStatus::Ready { .. }) {
                segment.status = SegmentCacheStatus::Discovered;
            }
        }
    }

    async fn assert_only_one_prefetch_is_active_before_controlled_release(policy: SegmentFetchPolicy) {
        let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;

        worker.wake_scheduler(context.clone(), 20).await;
        tokio::time::timeout(Duration::from_secs(10), request_seen)
            .await
            .expect("origin request starts before test deadline")
            .expect("controlled origin observes the request");

        let (active_proxy_seq, completion_notifier) = {
            let mut session = context.session.write().await;
            let fetching_proxy_seqs = session
                .segments
                .iter()
                .filter_map(|(proxy_seq, segment)| {
                    matches!(segment.status, SegmentCacheStatus::Fetching { .. }).then_some(*proxy_seq)
                })
                .collect::<Vec<_>>();
            assert_eq!(session.active_segment_fetches, 1);
            assert_eq!(fetching_proxy_seqs.len(), 1);
            let active_proxy_seq = fetching_proxy_seqs[0];
            let completion_notifier = session.segment_fetch_notifiers.entry(active_proxy_seq).or_default().clone();
            session.invalidate_queued_origin_work();
            (active_proxy_seq, completion_notifier)
        };
        assert_eq!(server.requests.lock().await.len(), 1);

        let completion = completion_notifier.notified();
        tokio::pin!(completion);
        completion.as_mut().enable();
        release_response.send(()).expect("controlled origin response releases");
        tokio::time::timeout(Duration::from_secs(10), completion.as_mut())
            .await
            .expect("active prefetch completes before test deadline");

        let session = context.session.read().await;
        assert_eq!(session.active_segment_fetches, 0);
        assert!(matches!(
            session.segments.get(&active_proxy_seq).map(|segment| &segment.status),
            Some(SegmentCacheStatus::Discovered)
        ));
    }

    async fn committed_segment(context: &SegmentFetchContext, proxy_seq: u64) -> (crate::SegmentCacheKey, Vec<u8>) {
        let cache_key = context.session.read().await.segments.get(&proxy_seq).expect("segment").cache_key.clone();
        let metadata = context
            .segment_cache
            .metadata(&cache_key)
            .await
            .expect("cache metadata reads")
            .expect("cache object exists");
        let bytes = tokio::fs::read(metadata.path).await.expect("cache object reads");
        (cache_key, bytes)
    }

    #[tokio::test]
    async fn demand_fetch_writes_cache_and_sets_ready_after_commit() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        let session = context.session.read().await;
        assert!(matches!(session.segments.get(&1).expect("segment").status, SegmentCacheStatus::Ready { .. }));
    }

    #[tokio::test]
    async fn projected_capacity_reclamation_commits_one_origin_download() {
        let server = spawn_sequence_response_server(vec![TestOriginResponse {
            status: 200,
            headers: Vec::new(),
            body: b"abcdefghij".to_vec(),
        }])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let store = Arc::new(HlsSessionStore::new());
        let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
            session.proxy_next_seq = Some(1);
            session
                .apply_origin_manifest(&normal_manifest(&format!(
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{0}/1.ts\n#EXTINF:4.0,\n{0}/2.ts\n#EXTINF:4.0,\n{0}/3.ts\n#EXTINF:4.0,\n{0}/4.ts\n#EXTINF:4.0,\n{0}/5.ts\n#EXTINF:4.0,\n{0}/6.ts\n",
                    server.base_url
                )))
                .expect("manifest maps");
        }
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let gc = Arc::new(HlsGarbageCollector::new(
            Arc::clone(&store),
            Arc::clone(&cache),
            GarbageCollectionPolicy::default(),
            build_rewrite_secret_fingerprint(b"secret"),
        ));
        cache.install_capacity_reclaimer(&gc);
        let old_keys = {
            let session = session.read().await;
            assert!(session.segments.contains_key(&6), "mapped proxy sequences: {:?}", session.segments.keys());
            [1_u64, 2].map(|proxy_seq| session.segments.get(&proxy_seq).expect("old segment").cache_key.clone())
        };
        for key in &old_keys {
            cache.write_bytes_and_commit(key, b"0123456789").await.expect("old segment commits");
        }
        {
            let mut session = session.write().await;
            for proxy_seq in [1_u64, 2] {
                session.segments.get_mut(&proxy_seq).expect("old segment").status =
                    SegmentCacheStatus::Ready { content_length: 10, ready_at_ms: proxy_seq };
            }
        }
        cache.update_cache_limits(100, 25);
        let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
        let context = SegmentFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::clone(&cache),
            segment_repair: test_segment_repair_manager(),
            repair_access_lease_id: None,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: true,
            origin_io: None,
        };
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker
            .demand_fetch_and_wait(context.clone(), &HlsSegmentFile { proxy_seq: 6, extension: "ts".to_string() }, 20)
            .await;

        let target_status = format!("{:?}", session.read().await.segments.get(&6).map(|segment| &segment.status));
        assert_eq!(
            outcome,
            super::SegmentDemandFetchOutcome::Ready,
            "requests={} target_status={target_status}",
            server.requests.lock().await.len(),
        );
        assert_eq!(server.requests.lock().await.len(), 1, "staged body is not downloaded again");
        assert_eq!(committed_segment(&context, 6).await.1, b"abcdefghij");
        let session = session.read().await;
        assert!(!session.segments.contains_key(&1));
        assert!(session.segments.contains_key(&2));
        drop(session);
        let usage = cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
        assert_eq!(usage.session_bytes, 20);
        assert!(!cache.has_active_temp_files());
    }

    #[tokio::test]
    async fn encrypted_segment_fetch_commits_exact_key_before_media_becomes_ready() {
        let server = spawn_sequence_response_server(vec![
            TestOriginResponse { status: 200, headers: Vec::new(), body: b"0123456789abcdef".to_vec() },
            TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media".to_vec() },
        ])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy =
            SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
        let (worker, context, segment_file) = encrypted_fetch_context(&server, &temp_dir, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        let session = context.session.read().await;
        let first = session.segments.get(&1).expect("encrypted segment");
        assert!(matches!(first.status, SegmentCacheStatus::Ready { .. }));
        let encryption = first.encryption.as_ref().expect("segment keeps key dependency");
        assert!(session
            .transient
            .ready_key_object_valid_until_ms(
                &session.proxy_session_id,
                &encryption.resource_id,
                &encryption.resource_extension,
                20,
            )
            .is_some());
        assert!(session.ready_timeline_snapshot(1, 20).units[0].required_key_ready);
        assert!(session.activity.media_readiness_generation >= 2);
        drop(session);
        let requests = server.requests.lock().await;
        assert!(requests[0].starts_with("GET /key.key "));
        assert!(requests[1].starts_with("GET /1.ts "));
    }

    #[tokio::test]
    async fn ready_encrypted_segments_schedule_one_key_only_fetch() {
        let server = spawn_sequence_response_server(vec![TestOriginResponse {
            status: 200,
            headers: Vec::new(),
            body: b"0123456789abcdef".to_vec(),
        }])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy =
            SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
        let (worker, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
        let proxy_session_id = {
            let mut session = context.session.write().await;
            for segment in session.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 32, ready_at_ms: 10 };
            }
            session.last_rendered_manifest = Some(RenderedManifest {
                body: "#EXTM3U\n".to_string(),
                first_proxy_seq: 1,
                last_proxy_seq: 3,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                playlist_duration_ms: 12_000,
                valid_until_ms: u64::MAX,
                render_gap_segments: 0,
                rendered_at_ms: 10,
                segment_proxy_seqs: vec![1, 2, 3],
            });
            session.proxy_session_id.clone()
        };
        grant_usable_worker_access_lease(&worker, &proxy_session_id).await;

        tokio::join!(worker.wake_scheduler(context.clone(), 20), worker.wake_scheduler(context.clone(), 20));

        let pending_key = {
            let mut session = context.session.write().await;
            let resource = session
                .transient
                .resources
                .values()
                .find(|resource| resource.kind == TransientResourceKind::Key)
                .cloned()
                .expect("encrypted fixture key resource");
            let extension = resource.file_ext_hint.clone().expect("key extension");
            let proxy_session_id = session.proxy_session_id.clone();
            let cache_duration_ms = session.transient.resource_ttl_ms;
            match session.transient.begin_object_fetch(&proxy_session_id, &resource, &extension, 20, cache_duration_ms)
            {
                TransientObjectFetchDecision::Ready => None,
                TransientObjectFetchDecision::Wait(notifier) => Some((notifier, resource.id, extension)),
                TransientObjectFetchDecision::Fetch(_) => {
                    panic!("rendered READY media must already have scheduled its key-only fetch")
                }
            }
        };
        if let Some((notifier, resource_id, extension)) = pending_key {
            tokio::time::timeout(
                Duration::from_secs(10),
                wait_for_segment_key_dependency(&context, notifier, resource_id, extension, Duration::from_secs(10)),
            )
            .await
            .expect("key-only readiness completes before test deadline")
            .expect("key-only readiness succeeds");
        }

        let session = context.session.read().await;
        assert_eq!(session.active_segment_fetches, 0);
        assert!(session.ready_timeline_snapshot(1, 20).units.iter().all(|unit| unit.required_key_ready));
        assert!(session.segments.values().all(|segment| matches!(segment.status, SegmentCacheStatus::Ready { .. })));
        drop(session);
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /key.key "));
    }

    #[tokio::test]
    async fn completed_shared_key_before_wait_registration_is_observed_without_timeout() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy::default();
        let (_, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
        let (fetch_token, notifier, resource_id, extension) = shared_key_fetch_and_wait(&context, 20).await;
        commit_test_key_ready(&context, &fetch_token, 21).await;

        let result =
            wait_for_segment_key_dependency(&context, notifier, resource_id, extension, Duration::from_millis(1)).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn registered_shared_key_waiter_observes_controlled_ready_transition() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy::default();
        let (_, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
        let (fetch_token, notifier, resource_id, extension) = shared_key_fetch_and_wait(&context, 20).await;
        let mut wait = Box::pin(wait_for_segment_key_dependency(
            &context,
            notifier,
            resource_id,
            extension,
            Duration::from_secs(1),
        ));
        assert!(matches!(poll!(wait.as_mut()), Poll::Pending));

        commit_test_key_ready(&context, &fetch_token, 21).await;

        assert!(matches!(poll!(wait.as_mut()), Poll::Ready(Ok(()))));
    }

    #[tokio::test]
    async fn invalid_aes_key_size_blocks_segment_fetch_and_ready_reserve() {
        let server = spawn_sequence_response_server(vec![TestOriginResponse {
            status: 200,
            headers: Vec::new(),
            body: vec![b'k'; 15],
        }])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy =
            SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
        let (worker, context, segment_file) = encrypted_fetch_context(&server, &temp_dir, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Unavailable);
        let session = context.session.read().await;
        let first = session.segments.get(&1).expect("encrypted segment");
        assert!(matches!(first.status, SegmentCacheStatus::FailedPermanent { .. }));
        assert!(!session.ready_timeline_snapshot(1, 20).units[0].required_key_ready);
        assert_eq!(session.activity.media_readiness_generation, 0);
        drop(session);
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /key.key "));
    }

    #[tokio::test]
    async fn invalidated_generation_discards_late_segment_completion_without_sleep() {
        let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;
        let fetch_context = context.clone();
        let fetch = tokio::spawn(async move { worker.demand_fetch_and_wait(fetch_context, &segment_file, 20).await });

        request_seen.await.expect("origin request starts");
        context.session.write().await.invalidate_queued_origin_work();
        release_response.send(()).expect("origin response released");

        assert_eq!(fetch.await.expect("fetch task joins"), super::SegmentDemandFetchOutcome::Unavailable);
        let cache_key = {
            let session = context.session.read().await;
            assert_eq!(session.active_segment_fetches, 0);
            let segment = session.segments.get(&1).expect("segment remains mapped");
            assert!(matches!(segment.status, SegmentCacheStatus::Discovered));
            segment.cache_key.clone()
        };
        assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
    }

    #[tokio::test]
    async fn same_origin_sequence_resource_conflict_preserves_in_flight_binding() {
        let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;
        let fetch_context = context.clone();
        let fetch = tokio::spawn(async move { worker.demand_fetch_and_wait(fetch_context, &segment_file, 20).await });

        request_seen.await.expect("origin request starts");
        let key_uri = format!("{}/rebound.key", server.base_url);
        let key_resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            &key_uri,
            b"secret",
            0,
            u64::MAX,
            Some("key".to_string()),
        );
        let mut rebound_manifest = normal_manifest(&format!(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n\
             #EXTINF:4.0,\n{}/rebound.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4.0,\n{}/2.ts\n\
             #EXTINF:4.0,\n{}/3.ts\n",
            server.base_url, server.base_url, server.base_url
        ));
        for encryption in rebound_manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
            encryption.proxy_resource_id = Some(key_resource.id.0.clone());
            encryption.proxy_resource_extension = Some("key".to_string());
        }
        let (origin_work_generation, cache_key) = {
            let mut session = context.session.write().await;
            let origin_work_generation = session.activity.origin_work_generation;
            session.transient.upsert_resources([key_resource]);
            assert!(matches!(
                session.apply_origin_manifest(&rebound_manifest),
                Err(TimelineMapError::OriginSequenceResourceConflict { candidate_origin_seq: 1, .. })
            ));
            assert_eq!(session.activity.origin_work_generation, origin_work_generation);
            let segment = session.segments.get(&1).expect("original proxy segment remains mapped");
            assert!(matches!(segment.status, SegmentCacheStatus::Fetching { .. }));
            assert_eq!(
                segment.origin_fetch_ref.as_ref().expect("original fetch ref").resolved_origin_url,
                format!("{}/1.ts", server.base_url)
            );
            assert!(segment.encryption.is_none());
            (origin_work_generation, segment.cache_key.clone())
        };

        release_response.send(()).expect("origin response released");

        assert_eq!(fetch.await.expect("fetch task joins"), super::SegmentDemandFetchOutcome::Ready);
        let session = context.session.read().await;
        assert_eq!(session.activity.origin_work_generation, origin_work_generation);
        assert_eq!(session.active_segment_fetches, 0);
        let segment = session.segments.get(&1).expect("original segment remains mapped");
        assert!(matches!(segment.status, SegmentCacheStatus::Ready { .. }));
        assert_eq!(
            segment.origin_fetch_ref.as_ref().expect("original fetch ref remains authoritative").resolved_origin_url,
            format!("{}/1.ts", server.base_url)
        );
        assert!(segment.encryption.is_none());
        drop(session);
        let (committed_key, bytes) = committed_segment(&context, 1).await;
        assert_eq!(committed_key, cache_key);
        assert_eq!(bytes, b"controlled-segment");
        assert!(temp_cache_files(temp_dir.path()).is_empty());
    }

    #[tokio::test]
    async fn segment_declared_codings_decode_to_identity_before_cache_and_preserve_opaque_bytes() {
        let identity_bytes = b"\x00\xffopaque-hls-ciphertext\x1f\x8bmedia".to_vec();

        for encoding in [
            TestContentEncoding::Gzip,
            TestContentEncoding::RawDeflate,
            TestContentEncoding::Brotli,
            TestContentEncoding::Zstd,
        ] {
            let encoded = encode_test_body(&identity_bytes, encoding).await;
            let server =
                spawn_sequence_response_server(vec![TestOriginResponse::encoded(encoding.header_value(), encoded)])
                    .await;
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let policy = SegmentFetchPolicy {
                retry_delays_ms: [0, 0, 0, 0, 0],
                retry_jitter_max_ms: 0,
                ..SegmentFetchPolicy::default()
            };
            let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
            clear_scheduled_prefetch(&context, &policy).await;

            let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

            assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready, "failed for {encoding:?}");
            let (cache_key, cached) = committed_segment(&context, 1).await;
            assert_eq!(cached, identity_bytes, "cache retained HTTP coding for {encoding:?}");
            let mut range = context.segment_cache.open_range(&cache_key, 5).await.expect("cache range opens");
            let mut ranged = Vec::new();
            range.read_to_end(&mut ranged).await.expect("cache range reads");
            assert_eq!(ranged, identity_bytes[5..], "range did not address Identity bytes for {encoding:?}");
            let requests = server.requests.lock().await;
            assert_eq!(requests.len(), 1, "unexpected retries for {encoding:?}");
            assert!(
                requests[0].to_ascii_lowercase().contains("accept-encoding: identity"),
                "request did not enforce identity for {encoding:?}"
            );
        }
    }

    #[tokio::test]
    async fn segment_decoder_failure_retries_then_commits_and_cleans_temp_file() {
        let identity_bytes = b"identity-media-after-retry".to_vec();
        let valid = encode_test_body(&identity_bytes, TestContentEncoding::Gzip).await;
        let mut corrupt = valid.clone();
        corrupt.truncate(corrupt.len() / 2);
        let server = spawn_sequence_response_server(vec![
            TestOriginResponse::encoded("gzip", corrupt),
            TestOriginResponse::encoded("gzip", valid),
        ])
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(committed_segment(&context, 1).await.1, identity_bytes);
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.to_ascii_lowercase().contains("accept-encoding: identity")));
        drop(requests);
        assert!(!context.segment_cache.has_active_temp_files());
        assert!(temp_cache_files(temp_dir.path()).is_empty());
    }

    #[tokio::test]
    async fn segment_decoder_failure_exhausts_attempt_budget_without_cache_or_temp_file() {
        let valid = encode_test_body(b"never committed", TestContentEncoding::Gzip).await;
        let mut corrupt = valid;
        corrupt.truncate(corrupt.len() / 2);
        let server = spawn_sequence_response_server(
            (0..5).map(|_| TestOriginResponse::encoded("gzip", corrupt.clone())).collect(),
        )
        .await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::TimedOut);
        assert_eq!(server.requests.lock().await.len(), 5);
        let cache_key = context.session.read().await.segments.get(&1).expect("segment").cache_key.clone();
        assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
        assert!(!context.segment_cache.has_active_temp_files());
        assert!(temp_cache_files(temp_dir.path()).is_empty());
    }

    #[tokio::test]
    async fn segment_decoded_object_limit_is_authoritative_and_non_retryable() {
        let identity_bytes = vec![b'x'; 512];
        let encoded = encode_test_body(&identity_bytes, TestContentEncoding::Gzip).await;
        assert!(encoded.len() < 64, "fixture must be smaller on the wire than the decoded limit");
        let server = spawn_sequence_response_server(vec![TestOriginResponse::encoded("gzip", encoded)]).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        context.segment_cache.update_cache_limits(64, 64);
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Unavailable);
        assert_eq!(server.requests.lock().await.len(), 1);
        let cache_key = context.session.read().await.segments.get(&1).expect("segment").cache_key.clone();
        assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
        assert!(!context.segment_cache.has_active_temp_files());
        assert!(temp_cache_files(temp_dir.path()).is_empty());
    }

    #[tokio::test]
    async fn demand_fetch_is_blocked_for_gc_marked_session() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        context.session.write().await.mark_for_gc_removal();

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::NotFound);
        assert!(server.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn background_segment_fetch_without_usable_access_lease_resets_queue_without_origin_request() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, _) = fetch_context_with_access_lease(&server, &temp_dir, &policy, false).await;

        worker.wake_scheduler(context.clone(), 20).await;

        assert!(server.requests.lock().await.is_empty());
        let session = context.session.read().await;
        assert_eq!(session.active_segment_fetches, 0);
        assert!(session.segment_prefetch_queue.is_empty());
        assert!(session.segments.values().all(|segment| matches!(segment.status, SegmentCacheStatus::Discovered)));
    }

    #[tokio::test]
    async fn demand_fetch_starts_without_worker_usable_access_lease() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context_with_access_lease(&server, &temp_dir, &policy, false).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn demand_fetch_returns_unavailable_when_fetch_slots_are_saturated() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 1,
            max_session_segment_fetches: 1,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;
        context.session.write().await.active_segment_fetches = 1;

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Unavailable);
        assert!(server.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn ready_cache_hit_is_allowed_when_fetch_slots_are_saturated() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 1,
            max_session_segment_fetches: 1,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        {
            let mut session = context.session.write().await;
            session.active_segment_fetches = 1;
            session.segments.get_mut(&1).expect("segment").status =
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 20 };
        }

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        assert!(server.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn origin_byterange_segment_fetch_uses_http_range_and_stores_logical_segment() {
        let server = spawn_range_segment_server().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            origin_segment_timeout_ms: 1_000,
            ..SegmentFetchPolicy::default()
        };
        let store = HlsSessionStore::new();
        let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        {
            let mut session = session.write().await;
            session
                .apply_origin_manifest(&normal_manifest(&format!(
                    "#EXTM3U\n#EXT-X-BYTERANGE:5@10\n#EXTINF:4.0,\n{}/big.m4s\n",
                    server.base_url
                )))
                .expect("manifest maps");
        }
        let context = SegmentFetchContext {
            session: Arc::clone(&session),
            segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
            segment_repair: test_segment_repair_manager(),
            repair_access_lease_id: None,
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: true,
            origin_io: None,
        };
        let worker = Arc::new(HlsSegmentWorkerPool::new(policy));
        let segment_file = HlsSegmentFile { proxy_seq: 0, extension: "m4s".to_string() };

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].to_ascii_lowercase().contains("range: bytes=10-14"));
        let session = context.session.read().await;
        let segment = session.segments.get(&0).expect("segment");
        assert!(matches!(segment.status, SegmentCacheStatus::Ready { content_length: 5, .. }));
        assert!(context.segment_cache.metadata(&segment.cache_key).await.expect("metadata").is_some());
    }

    #[tokio::test]
    async fn one_proxy_sequence_has_at_most_one_active_origin_fetch() {
        let server = spawn_segment_server(80).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 4,
            max_session_segment_fetches: 4,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let first = {
            let worker = Arc::clone(&worker);
            let context = context.clone();
            let segment_file = segment_file.clone();
            tokio::spawn(async move { worker.demand_fetch_and_wait(context, &segment_file, 20).await })
        };
        let second = {
            let worker = Arc::clone(&worker);
            let context = context.clone();
            let segment_file = segment_file.clone();
            tokio::spawn(async move { worker.demand_fetch_and_wait(context, &segment_file, 21).await })
        };

        assert_eq!(first.await.expect("task"), super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(second.await.expect("task"), super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn retryable_407_retries_segment_fetch_until_success() {
        let server = spawn_sequence_status_server(vec![407, 200]).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn permanent_404_does_not_retry_segment_fetch() {
        let server = spawn_sequence_status_server(vec![404, 200]).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Unavailable);
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn segment_retry_starts_again_at_fetch_ref_after_redirect_failure() {
        let server = spawn_redirect_retry_server().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        assert_eq!(server.requests.lock().await.as_slice(), ["/1.ts", "/redirected.ts", "/1.ts"]);
    }

    #[tokio::test]
    async fn session_limit_is_respected() {
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 4,
            max_session_segment_fetches: 1,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };

        assert_only_one_prefetch_is_active_before_controlled_release(policy).await;
    }

    #[tokio::test]
    async fn global_limit_is_respected() {
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 1,
            max_session_segment_fetches: 3,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };

        assert_only_one_prefetch_is_active_before_controlled_release(policy).await;
    }

    #[tokio::test]
    async fn demand_priority_runs_before_prefetch() {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 1,
            max_session_segment_fetches: 1,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        {
            let mut session = context.session.write().await;
            session.segment_prefetch_queue = crate::SegmentPrefetchQueue::new(6);
            session.segments.get_mut(&1).expect("segment").status = SegmentCacheStatus::Discovered;
            session.segments.get_mut(&2).expect("segment").status = SegmentCacheStatus::Discovered;
            session.queue_segment_fetch_candidate(2, SegmentFetchPriority::Prefetch, 10);
        }

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::SegmentDemandFetchOutcome::Ready);
        let requests = server.requests.lock().await;
        let first_request = requests.first().expect("request should be made");
        assert!(first_request.starts_with("GET /1.ts "));
    }
}
