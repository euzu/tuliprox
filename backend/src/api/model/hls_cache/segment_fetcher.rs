#![allow(clippy::large_futures)]

use super::{
    begin_hls_origin_account_io_bounded, build_hls_origin_resource_headers, classify_hls_backpressure,
    finish_hls_origin_account_io, hls_object_body_deadline, run_hls_origin_resource_retry_loop_with_attempt_prepare,
    CachedSegmentMetadata, HlsAccessLeaseChannelUnavailableReason, HlsAccessLeaseId, HlsAccessLeaseStore,
    HlsBackpressureState, HlsBoundAccountAcquireErrorKind, HlsCacheMetrics, HlsOriginAccountIoLeaseGuard,
    HlsOriginByteRangeExpectation, HlsOriginIoContext, HlsOriginResourceBodyDeadline, HlsOriginResourceClients,
    HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsRepairRenderedObjectId, HlsResourceFetchKind,
    HlsResourceFetchSource, HlsSegmentCache, HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSegmentFile,
    HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource, HlsSessionHandle,
    OriginSegmentFetchRef, SegmentCacheKey, SegmentCacheStatus, SegmentFetchPriority,
};
use crate::{
    model::HlsCacheConfig, processing::parser::hls::origin_manifest::ParsedByteRange,
    utils::content_coding::DecodedHttpResponse,
};
use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use futures::FutureExt;
use log::{debug, warn};
use reqwest::Client;
use shared::model::{HlsSegmentRepairMode, HlsStripMode};
use std::{fmt, sync::Arc, time::Duration};
use tokio::{
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
    time::timeout,
};

const DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES: usize = 64;
const DEFAULT_MAX_SESSION_SEGMENT_FETCHES: usize = 2;
const DEFAULT_MAX_PREFETCH_QUEUE_DEPTH: usize = 6;
const DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS: u64 = 10_000;

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
            permanent_failure_segment_threshold: permanent_failure_segment_threshold_from_config(config),
            ..Self::default()
        }
    }

    pub fn demand_wait_timeout(&self) -> Duration {
        let attempts = u64::try_from(self.retry_delays_ms.len()).unwrap_or(u64::MAX);
        let per_attempt_budget =
            self.origin_segment_timeout_ms.saturating_add(self.effective_repair_postprocess_timeout_ms);
        let retry_delay_budget = self.retry_delays_ms.iter().copied().fold(0_u64, u64::saturating_add);
        let jitter_budget = attempts.saturating_mul(self.retry_jitter_max_ms);
        Duration::from_millis(
            attempts
                .saturating_mul(per_attempt_budget)
                .saturating_add(retry_delay_budget)
                .saturating_add(jitter_budget)
                .saturating_add(1_000),
        )
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

fn permanent_failure_segment_threshold_from_config(config: &HlsCacheConfig) -> u32 {
    let configured_strip_segments = match config.strip.mode {
        HlsStripMode::Segments => u32::try_from(config.strip.value).unwrap_or(u32::MAX.saturating_sub(3)),
        HlsStripMode::Seconds => 0,
    };
    3_u32.saturating_add(configured_strip_segments)
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

#[derive(Clone)]
struct SegmentFetchSnapshot {
    proxy_seq: u64,
    proxy_seq_log: String,
    cache_key: SegmentCacheKey,
    fetch_ref: OriginSegmentFetchRef,
    proxy_file_ext: String,
    origin_seq: u64,
    complete_object: bool,
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
            .field("cache_key", &self.cache_key)
            .field("fetch_ref", &self.fetch_ref)
            .field("proxy_file_ext", &self.proxy_file_ext)
            .field("origin_seq", &self.origin_seq)
            .field("complete_object", &self.complete_object)
            .finish()
    }
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
        Self {
            runtime: ArcSwap::from_pointee(SegmentWorkerRuntime::new(policy, global_semaphore)),
            access_leases,
            metrics,
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
        let wait_timeout = self.runtime.load().policy.demand_wait_timeout();
        let notifier = {
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
            match entry.status {
                SegmentCacheStatus::Ready { .. } => return SegmentDemandFetchOutcome::Ready,
                SegmentCacheStatus::Fetching { .. } => {
                    session.segment_fetch_notifiers.entry(segment_file.proxy_seq).or_default().clone()
                }
                SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. } => {
                    if entry.origin_fetch_ref.is_none() {
                        return SegmentDemandFetchOutcome::Unavailable;
                    }
                    let backpressure = self.classify_backpressure_for_session(&session);
                    if !backpressure.allows_new_demand_fetch() {
                        warn!(
                            "HLS segment demand fetch skipped by backpressure: session={} source=normal resource={:06} state={backpressure:?}",
                            super::safe_proxy_session_id(&session.proxy_session_id),
                            segment_file.proxy_seq
                        );
                        return SegmentDemandFetchOutcome::Unavailable;
                    }
                    session.queue_segment_fetch_candidate(segment_file.proxy_seq, SegmentFetchPriority::Demand, now_ms);
                    self.metrics.record_demand_fetch_started();
                    debug!(
                        "HLS segment demand fetch started: session={} source=normal resource={:06}",
                        super::safe_proxy_session_id(&session.proxy_session_id),
                        segment_file.proxy_seq
                    );
                    session.segment_fetch_notifiers.entry(segment_file.proxy_seq).or_default().clone()
                }
                SegmentCacheStatus::FailedRetryable { .. } => {
                    return SegmentDemandFetchOutcome::TimedOut;
                }
                SegmentCacheStatus::FailedPermanent { .. } | SegmentCacheStatus::Expired => {
                    return SegmentDemandFetchOutcome::Unavailable;
                }
            }
        };

        self.wake_scheduler(context.clone(), now_ms).await;

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
            Some(
                SegmentCacheStatus::Discovered
                | SegmentCacheStatus::Expired
                | SegmentCacheStatus::FailedPermanent { .. },
            ) => SegmentDemandFetchOutcome::Unavailable,
            Some(SegmentCacheStatus::FailedRetryable { .. }) => SegmentDemandFetchOutcome::TimedOut,
            None => SegmentDemandFetchOutcome::NotFound,
        }
    }

    pub async fn wake_scheduler(self: &Arc<Self>, context: SegmentFetchContext, now_ms: u64) {
        loop {
            let runtime = self.runtime.load_full();
            let Ok(permit) = Arc::clone(&runtime.global_semaphore).try_acquire_owned() else {
                return;
            };
            let Some(snapshot) = self.next_fetch_snapshot(&context, now_ms, &runtime.policy).await else {
                drop(permit);
                return;
            };

            let worker = Arc::clone(self);
            let task_context = context.clone();
            tokio::spawn(async move {
                worker.fetch_one_segment(task_context, snapshot, runtime.policy.clone(), permit).await;
            });
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
            let Some(entry) = session.segments.get_mut(&proxy_seq) else {
                continue;
            };
            if !matches!(entry.status, SegmentCacheStatus::Queued { .. }) {
                continue;
            }
            if priority != SegmentFetchPriority::Demand && !has_usable_access_lease {
                entry.status = SegmentCacheStatus::Discovered;
                continue;
            }
            let Some(fetch_ref) = entry.origin_fetch_ref.clone() else {
                continue;
            };
            if !fetch_ref.is_valid_at(now_ms) {
                continue;
            }

            let cache_key = entry.cache_key.clone();
            let proxy_file_ext = entry.proxy_file_ext.clone();
            let origin_seq = entry.origin_key.origin_seq;
            let complete_object = entry.origin_byte_range.is_none();
            entry.status = SegmentCacheStatus::Fetching { priority, started_at_ms: now_ms };
            session.active_segment_fetches = session.active_segment_fetches.saturating_add(1);
            let proxy_seq_log = format!("{proxy_seq:06}");
            debug!(
                "HLS segment fetch started: session={} source=normal resource={} priority={priority:?}",
                super::safe_proxy_session_id(&session.proxy_session_id),
                proxy_seq_log
            );
            return Some(SegmentFetchSnapshot {
                proxy_seq,
                proxy_seq_log,
                cache_key,
                fetch_ref,
                proxy_file_ext,
                origin_seq,
                complete_object,
            });
        }

        None
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_one_segment(
        self: Arc<Self>,
        context: SegmentFetchContext,
        snapshot: SegmentFetchSnapshot,
        policy: SegmentFetchPolicy,
        permit: OwnedSemaphorePermit,
    ) {
        let result = fetch_segment_into_cache(&context, &snapshot, &policy).await;
        let finished_at_ms = current_time_millis();
        let generation_valid = result.as_ref().map_or(true, |commit| commit.generation_valid);
        let fetch_succeeded = result.is_ok();
        let (notifier, response_flag_reason) = {
            let mut session = context.session.write().await;
            session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
            let mut response_flag_reason = None;
            match result {
                Ok(commit) => {
                    let content_length = commit.content_length;
                    if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                        entry.status = SegmentCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
                    }
                    let reset_failures = session.record_successful_segment_fetch();
                    self.metrics.record_segment_cached();
                    debug!(
                        "HLS segment cached: session={} source=normal resource={} content_length={content_length}",
                        super::safe_proxy_session_id(&session.proxy_session_id),
                        snapshot.proxy_seq_log
                    );
                    if let Some(reset_failures) = reset_failures {
                        debug!(
                            "HLS segment temporary failure counter reset: session={} previous_failures={reset_failures}",
                            super::safe_proxy_session_id(&session.proxy_session_id)
                        );
                    }
                }
                Err(err) => {
                    let mut invalidate_queued_origin_work = false;
                    let status = if err.retryable_failure() {
                        let threshold =
                            session.segment_temporary_failure_threshold(policy.permanent_failure_segment_threshold);
                        match session.record_temporary_segment_fetch_failure(
                            finished_at_ms,
                            HlsSegmentFailureObject::Normal {
                                proxy_seq: snapshot.proxy_seq,
                                origin_seq: snapshot.origin_seq,
                            },
                            threshold,
                        ) {
                            HlsSegmentFailureTransition::StillRetryable { failures, threshold } => {
                                debug!(
                                    "HLS segment temporary failure counted: session={} object={} failures={} threshold={}",
                                    super::safe_proxy_session_id(&session.proxy_session_id),
                                    snapshot.proxy_seq_log,
                                    failures,
                                    threshold
                                );
                                SegmentCacheStatus::FailedRetryable {
                                    failed_at_ms: finished_at_ms,
                                    retry_after_ms: 1_000,
                                }
                            }
                            HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold } => {
                                warn!(
                                    "HLS segment temporary failure threshold reached: session={} failures={} threshold={}",
                                    super::safe_proxy_session_id(&session.proxy_session_id),
                                    failures,
                                    threshold
                                );
                                invalidate_queued_origin_work = true;
                                response_flag_reason =
                                    Some(HlsAccessLeaseChannelUnavailableReason::SegmentTemporaryFailureThreshold {
                                        failures,
                                        threshold,
                                    });
                                SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None }
                            }
                        }
                    } else {
                        response_flag_reason = Some(HlsAccessLeaseChannelUnavailableReason::SegmentPermanentFailure {
                            status: err.permanent_status(),
                        });
                        SegmentCacheStatus::FailedPermanent {
                            failed_at_ms: finished_at_ms,
                            status: err.permanent_status(),
                        }
                    };
                    if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                        entry.status = status;
                    }
                    if invalidate_queued_origin_work {
                        session.invalidate_queued_origin_work();
                        if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                            entry.status =
                                SegmentCacheStatus::FailedPermanent { failed_at_ms: finished_at_ms, status: None };
                        }
                    }
                }
            }
            if fetch_succeeded && generation_valid {
                let _ = session.render_and_store_manifest(finished_at_ms);
            }
            (session.segment_fetch_notifiers.remove(&snapshot.proxy_seq), response_flag_reason)
        };
        if let Some(reason) = response_flag_reason {
            let marked = self.mark_channel_unavailable_for_session(&context.session, finished_at_ms, reason).await;
            if marked > 0 {
                debug!(
                    "HLS access leases marked channel unavailable: session={} marked={marked}",
                    super::safe_proxy_session_id(&context.session.read().await.proxy_session_id)
                );
            }
        }
        if let Some(notifier) = notifier {
            notifier.notify_waiters();
        }
        drop(permit);
        if generation_valid {
            self.schedule_wake(context, finished_at_ms);
        }
    }

    fn schedule_wake(self: &Arc<Self>, context: SegmentFetchContext, now_ms: u64) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            worker.wake_scheduler(context, now_ms).await;
        });
    }

    async fn mark_channel_unavailable_for_session(
        &self,
        session: &HlsSessionHandle,
        now_ms: u64,
        reason: HlsAccessLeaseChannelUnavailableReason,
    ) -> usize {
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        self.access_leases.write().await.mark_channel_unavailable_for_session(&proxy_session_id, now_ms, reason)
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
    fetch_segment_with_retries_into_cache(context, snapshot, policy).await
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
            let _ = finish_segment_origin_work(&context, started_generation).await;
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
                let _ = finish_segment_origin_work(&context, started_generation).await;
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
    let session_log_id = context.session.read().await.proxy_session_id.0.clone();
    let context = context.clone();
    let snapshot = snapshot.clone();
    let policy_for_prepare = policy.clone();
    let prepare_context = context.clone();
    let cleanup_context = context.clone();
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &session_log_id,
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
    let proxy_session_id = context.session.read().await.proxy_session_id.clone();
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Normal,
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
        encrypted: false,
        custom_response: false,
    };
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
    use super::{build_segment_origin_headers, SegmentFetchContext, SegmentFetchPolicy, SegmentFetchPriority};
    use crate::{
        api::model::{
            HlsAccessLease, HlsAccessLeaseId, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSegmentFile,
            HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, ProxySessionId,
            SegmentCacheStatus,
        },
        model::HlsSegmentRepairConfig,
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    };
    use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZstdEncoder};
    use axum::http::{header, HeaderMap, HeaderValue};
    use shared::model::HlsSegmentRepairMode;
    use std::{
        collections::VecDeque,
        fmt::Write as _,
        io::Cursor,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::Mutex,
    };

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    #[test]
    fn demand_wait_timeout_uses_fetch_attempt_and_postprocess_budget() {
        let policy = SegmentFetchPolicy {
            origin_segment_timeout_ms: 10_000,
            effective_repair_postprocess_timeout_ms: 2_000,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            ..SegmentFetchPolicy::default()
        };

        assert_eq!(policy.demand_wait_timeout(), Duration::from_millis(63_100));
    }

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
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
            Some(crate::processing::parser::hls::origin_manifest::ParsedByteRange { offset: 10, length: 5 }),
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
        max_active: Arc<AtomicUsize>,
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
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let task_active = Arc::clone(&active);
        let task_max = Arc::clone(&max_active);
        let task_requests = Arc::clone(&requests);
        let task_responses = Arc::clone(&responses);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let active = Arc::clone(&task_active);
                let max_active = Arc::clone(&task_max);
                let requests = Arc::clone(&task_requests);
                let responses = Arc::clone(&task_responses);
                tokio::spawn(async move {
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
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    let response = responses.lock().await.pop_front().unwrap_or(TestOriginResponse {
                        status: 500,
                        headers: Vec::new(),
                        body: Vec::new(),
                    });
                    active.fetch_sub(1, Ordering::SeqCst);
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

        TestSegmentServer { base_url: format!("http://{addr}"), max_active, requests, task }
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
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_active = Arc::clone(&active);
        let task_max = Arc::clone(&max_active);
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let active = Arc::clone(&task_active);
                let max_active = Arc::clone(&task_max);
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
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
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

        TestSegmentServer { base_url: format!("http://{addr}"), max_active, requests, task }
    }

    async fn spawn_sequence_status_server(statuses: Vec<u16>) -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let statuses = Arc::new(Mutex::new(VecDeque::from(statuses)));
        let task_active = Arc::clone(&active);
        let task_max = Arc::clone(&max_active);
        let task_requests = Arc::clone(&requests);
        let task_statuses = Arc::clone(&statuses);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let active = Arc::clone(&task_active);
                let max_active = Arc::clone(&task_max);
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
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
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

        TestSegmentServer { base_url: format!("http://{addr}"), max_active, requests, task }
    }

    async fn spawn_redirect_retry_server() -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let max_active = Arc::new(AtomicUsize::new(0));
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

        TestSegmentServer { base_url: format!("http://{addr}"), max_active, requests, task }
    }

    async fn spawn_range_segment_server() -> TestSegmentServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let max_active = Arc::new(AtomicUsize::new(0));
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

        TestSegmentServer { base_url: format!("http://{addr}"), max_active, requests, task }
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

    async fn clear_scheduled_prefetch(context: &SegmentFetchContext, policy: &SegmentFetchPolicy) {
        let mut session = context.session.write().await;
        session.segment_prefetch_queue = crate::api::model::SegmentPrefetchQueue::new(policy.max_prefetch_queue_depth);
        for segment in session.segments.values_mut() {
            if !matches!(segment.status, SegmentCacheStatus::Ready { .. }) {
                segment.status = SegmentCacheStatus::Discovered;
            }
        }
    }

    async fn committed_segment(
        context: &SegmentFetchContext,
        proxy_seq: u64,
    ) -> (crate::api::model::SegmentCacheKey, Vec<u8>) {
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
        assert!(!context.segment_cache.has_active_temp_files().await);
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
        assert!(!context.segment_cache.has_active_temp_files().await);
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
        assert!(!context.segment_cache.has_active_temp_files().await);
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
        tokio::time::sleep(Duration::from_millis(20)).await;

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
        let server = spawn_segment_server(80).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 4,
            max_session_segment_fetches: 1,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;

        worker.wake_scheduler(context.clone(), 20).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(server.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn global_limit_is_respected() {
        let server = spawn_segment_server(80).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            max_global_segment_fetches: 1,
            max_session_segment_fetches: 3,
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;

        worker.wake_scheduler(context.clone(), 20).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(server.max_active.load(Ordering::SeqCst), 1);
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
            session.segment_prefetch_queue = crate::api::model::SegmentPrefetchQueue::new(6);
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
