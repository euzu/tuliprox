#![allow(clippy::large_futures)]

use super::{
    begin_hls_origin_account_io, classify_hls_backpressure, classify_hls_resource_status, finish_hls_origin_account_io,
    force_identity_without_range, hls_object_body_deadline, log_hls_resource_attempt_started,
    log_hls_resource_attempt_succeeded, log_hls_resource_fetch_failed, log_hls_resource_retry_scheduled,
    log_hls_resource_timeout, scrub_hls_origin_headers, CachedSegmentMetadata, HlsAccessLeaseId, HlsAccessLeaseStore,
    HlsBackpressureState, HlsCacheMetrics, HlsOriginAccountIoLeaseGuard, HlsOriginIoContext,
    HlsRepairRenderedObjectId, HlsResourceFetchAttempt, HlsResourceFetchKind, HlsResourceFetchLogContext,
    HlsResourceFetchLogStatus, HlsResourceStatusClass, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager,
    HlsSegmentRepairObjectContext, HlsSegmentRepairSource, HlsSessionHandle, OriginSegmentFetchRef, SegmentCacheKey,
    SegmentCacheStatus, SegmentFetchPriority,
};
use crate::{
    model::{HlsCacheConfig, HlsSegmentRepairMode},
    processing::parser::hls::origin_manifest::ParsedByteRange,
};
use arc_swap::ArcSwap;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use futures::TryStreamExt;
use log::{debug, warn};
use reqwest::Client;
use shared::utils::sanitize_sensitive_info;
use std::{fmt, io, sync::Arc, time::{Duration, Instant}};
use tokio::{
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
    time::timeout,
};
use tokio_util::io::StreamReader;
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;
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
            ..Self::default()
        }
    }

    pub fn demand_wait_timeout(&self) -> Duration {
        Duration::from_millis(
            self.retry_delays_ms
                .len()
                .try_into()
                .map_or(u64::MAX, |attempts: u64| {
                    attempts.saturating_mul(
                        self.origin_segment_timeout_ms.saturating_add(self.effective_repair_postprocess_timeout_ms),
                    )
                })
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

#[derive(Debug)]
enum SegmentFetchError {
    PermanentStatus(StatusCode),
    RetryableStatus(StatusCode),
    RetryExhausted,
    NonRetryableStatus(StatusCode),
    Request(String),
    Redirect,
    Timeout,
    InvalidOriginUrl,
    InvalidByteRange,
    UnexpectedByteRangeStatus,
    Cache,
    ProviderUnavailable,
}

impl SegmentFetchError {
    fn retryable_failure(&self) -> bool {
        matches!(
            self,
            Self::RetryableStatus(_)
                | Self::RetryExhausted
                | Self::Request(_)
                | Self::Redirect
                | Self::Timeout
                | Self::Cache
                | Self::ProviderUnavailable
        )
    }

    fn permanent_status(&self) -> Option<StatusCode> {
        match self {
            Self::PermanentStatus(status) | Self::NonRetryableStatus(status) => Some(*status),
            _ => None,
        }
    }
}

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
                            "HLS segment demand fetch started skipped by backpressure: proxy_seq={} state={backpressure:?}",
                            segment_file.proxy_seq
                        );
                        return SegmentDemandFetchOutcome::Unavailable;
                    }
                    session.queue_segment_fetch_candidate(segment_file.proxy_seq, SegmentFetchPriority::Demand, now_ms);
                    self.metrics.record_demand_fetch_started();
                    debug!("HLS segment demand fetch started: proxy_seq={}", segment_file.proxy_seq);
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
            debug!("HLS segment fetch started: proxy_seq={proxy_seq} priority={priority:?}");
            return Some(SegmentFetchSnapshot {
                proxy_seq,
                proxy_seq_log: proxy_seq.to_string(),
                cache_key,
                fetch_ref,
                proxy_file_ext,
                origin_seq,
                complete_object,
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
        let generation_valid = result.as_ref().map_or(true, |commit| commit.generation_valid);
        let fetch_succeeded = result.is_ok();
        let notifier = {
            let mut session = context.session.write().await;
            session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
            if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                match result {
                    Ok(commit) => {
                        let content_length = commit.content_length;
                        entry.status = SegmentCacheStatus::Ready { content_length, ready_at_ms: finished_at_ms };
                        self.metrics.record_segment_cached();
                        debug!("HLS segment cached: proxy_seq={} content_length={content_length}", snapshot.proxy_seq);
                    }
                    Err(err) => {
                        if err.retryable_failure() {
                            entry.status = SegmentCacheStatus::FailedRetryable {
                                failed_at_ms: finished_at_ms,
                                retry_after_ms: 1_000,
                            };
                        } else {
                            entry.status = SegmentCacheStatus::FailedPermanent {
                                failed_at_ms: finished_at_ms,
                                status: err.permanent_status(),
                            };
                        }
                    }
                }
            }
            if fetch_succeeded && generation_valid {
                let _ = session.render_and_store_manifest(finished_at_ms);
            }
            session.segment_fetch_notifiers.remove(&snapshot.proxy_seq)
        };
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
}

impl Default for HlsSegmentWorkerPool {
    fn default() -> Self { Self::new(SegmentFetchPolicy::default()) }
}

async fn fetch_segment_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<SegmentFetchCommit, SegmentFetchError> {
    let started_generation = start_segment_origin_work(context).await;
    let binding =
        if context.origin_io.is_some() { context.session.read().await.origin_account_binding.clone() } else { None };
    let provider_lease = if let (Some(origin_io), Some(binding)) = (context.origin_io.as_ref(), binding.as_ref()) {
        if binding.is_detached() {
            let _ = finish_segment_origin_work(context, started_generation).await;
            touch_segment_origin_account_binding(context, false).await;
            return Err(SegmentFetchError::ProviderUnavailable);
        }
        let Ok(guard) = begin_hls_origin_account_io(origin_io, &context.session, binding).await else {
            let _ = finish_segment_origin_work(context, started_generation).await;
            touch_segment_origin_account_binding(context, false).await;
            return Err(SegmentFetchError::ProviderUnavailable);
        };
        Some((origin_io.clone(), guard))
    } else {
        None
    };

    let metadata = match fetch_segment_with_retries_into_cache(context, snapshot, policy).await {
        Ok(metadata) => metadata,
        Err(err) => {
            finish_segment_origin_io(context, started_generation, provider_lease).await;
            return Err(err);
        }
    };
    let origin_work = finish_segment_origin_io(context, started_generation, provider_lease).await;
    Ok(SegmentFetchCommit { content_length: metadata.size, generation_valid: origin_work.generation_valid })
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
) -> Result<CachedSegmentMetadata, SegmentFetchError> {
    let attempts = policy.retry_delays_ms.len();
    for attempt_index in 0..attempts {
        let attempt = HlsResourceFetchAttempt { attempt_index, attempts };
        let delay_ms = policy.retry_delay_ms(attempt_index);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        log_hls_resource_attempt_started(segment_retry_log_context(snapshot), attempt);
        let attempt_started_at = Instant::now();
        let fetch_result = fetch_segment_attempt_into_cache(context, snapshot, policy).await;

        match fetch_result {
            Ok(metadata) => {
                log_hls_resource_attempt_succeeded(segment_retry_log_context(snapshot), attempt_started_at.elapsed());
                return Ok(metadata);
            }
            Err(SegmentFetchError::PermanentStatus(status)) => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                );
                return Err(SegmentFetchError::PermanentStatus(status));
            }
            Err(SegmentFetchError::NonRetryableStatus(status)) => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                );
                return Err(SegmentFetchError::NonRetryableStatus(status));
            }
            Err(SegmentFetchError::RetryableStatus(status)) if attempt_index + 1 == attempts => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                );
                return Err(SegmentFetchError::RetryableStatus(status));
            }
            Err(SegmentFetchError::RetryableStatus(status)) => {
                log_hls_resource_retry_scheduled(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(SegmentFetchError::Request(_err)) if attempt_index + 1 == attempts => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::TransportError,
                );
                return Err(SegmentFetchError::Request(_err));
            }
            Err(SegmentFetchError::Request(_err)) => {
                log_hls_resource_retry_scheduled(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::TransportError,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default(),
                );
            }
            Err(err @ SegmentFetchError::Redirect) => {
                if attempt_index + 1 == attempts {
                    log_hls_resource_fetch_failed(
                        segment_retry_log_context(snapshot),
                        attempt,
                        HlsResourceFetchLogStatus::RedirectError,
                    );
                    return Err(err);
                }
                log_hls_resource_retry_scheduled(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::RedirectError,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(err @ SegmentFetchError::Timeout) => {
                let session_id = context.session.read().await.proxy_session_id.0.clone();
                log_hls_resource_timeout(
                    &session_id,
                    segment_retry_log_context(snapshot),
                    attempt,
                    hls_object_body_deadline(policy.origin_segment_timeout_ms).as_millis(),
                );
                if attempt_index + 1 == attempts {
                    log_hls_resource_fetch_failed(
                        segment_retry_log_context(snapshot),
                        attempt,
                        HlsResourceFetchLogStatus::Timeout,
                    );
                    return Err(err);
                }
                log_hls_resource_retry_scheduled(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::Timeout,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(err @ (SegmentFetchError::InvalidOriginUrl | SegmentFetchError::InvalidByteRange)) => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::TransportError,
                );
                return Err(err);
            }
            Err(SegmentFetchError::ProviderUnavailable) => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::ProviderUnavailable,
                );
                return Err(SegmentFetchError::ProviderUnavailable);
            }
            Err(err @ SegmentFetchError::Cache) => {
                if attempt_index + 1 == attempts {
                    log_hls_resource_fetch_failed(
                        segment_retry_log_context(snapshot),
                        attempt,
                        HlsResourceFetchLogStatus::CacheCommitError,
                    );
                    return Err(err);
                }
                log_hls_resource_retry_scheduled(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::CacheCommitError,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default(),
                );
            }
            Err(SegmentFetchError::UnexpectedByteRangeStatus) => {
                log_hls_resource_fetch_failed(
                    segment_retry_log_context(snapshot),
                    attempt,
                    HlsResourceFetchLogStatus::TransportError,
                );
                return Err(SegmentFetchError::UnexpectedByteRangeStatus);
            }
            Err(SegmentFetchError::RetryExhausted) => {}
        }
    }

    Err(SegmentFetchError::RetryExhausted)
}

fn segment_retry_log_context(snapshot: &SegmentFetchSnapshot) -> HlsResourceFetchLogContext<'_> {
    HlsResourceFetchLogContext {
        kind: HlsResourceFetchKind::Segment,
        object_id: &snapshot.proxy_seq_log,
        origin_url: Some(&snapshot.fetch_ref.resolved_origin_url),
    }
}

async fn fetch_segment_attempt_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<CachedSegmentMetadata, SegmentFetchError> {
    let response = fetch_segment_once(context, &snapshot.fetch_ref).await?;
    if snapshot.fetch_ref.byte_range.is_some() && response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(SegmentFetchError::UnexpectedByteRangeStatus);
    }
    if snapshot.fetch_ref.byte_range.is_none() && response.status() == StatusCode::PARTIAL_CONTENT {
        return Err(SegmentFetchError::UnexpectedByteRangeStatus);
    }
    let deadline = hls_object_body_deadline(policy.origin_segment_timeout_ms);
    let stream_reader = StreamReader::new(response.bytes_stream().map_err(io::Error::other));
    let proxy_session_id = context.session.read().await.proxy_session_id.clone();
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Normal,
        proxy_session_id,
        hls_access_lease_id: context.repair_access_lease_id.clone(),
        rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: snapshot.proxy_seq },
        resource_id: format!("{:06}", snapshot.proxy_seq),
        file_ext: snapshot.proxy_file_ext.clone(),
        normalized_origin_uri: snapshot.fetch_ref.resolved_origin_url.clone(),
        media_sequence: Some(snapshot.origin_seq),
        discontinuity_sequence: None,
        complete_object: snapshot.complete_object,
        encrypted: false,
        custom_response: false,
    };
    context
        .segment_repair
        .commit_origin_response(&context.segment_cache, &snapshot.cache_key, stream_reader, deadline, repair_context)
        .await
        .map_err(|err| {
            if err.kind() == io::ErrorKind::TimedOut {
                SegmentFetchError::Timeout
            } else {
                SegmentFetchError::Cache
            }
        })
}

async fn fetch_segment_once(
    context: &SegmentFetchContext,
    fetch_ref: &OriginSegmentFetchRef,
) -> Result<reqwest::Response, SegmentFetchError> {
    let url = Url::parse(&fetch_ref.resolved_origin_url).map_err(|_| SegmentFetchError::InvalidOriginUrl)?;
    let headers = build_segment_origin_headers(&context.headers, fetch_ref.byte_range)?;
    if context.use_manual_redirects {
        fetch_segment_with_manual_redirects(&url, headers, &context.no_redirect_client).await
    } else {
        let response =
            context.client.get(url).headers(headers).send().await.map_err(|err| {
                SegmentFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
            })?;
        classify_segment_response(response)
    }
}

async fn fetch_segment_with_manual_redirects(
    entry_url: &Url,
    headers: HeaderMap,
    client: &Client,
) -> Result<reqwest::Response, SegmentFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers;
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let response =
            client.get(current_url.clone()).headers(current_headers.clone()).send().await.map_err(|err| {
                SegmentFetchError::Request(sanitize_sensitive_info(err.to_string().as_str()).to_string())
            })?;
        if !response.status().is_redirection() {
            return classify_segment_response(response);
        }
        if remaining_redirects == 0 {
            return Err(SegmentFetchError::Redirect);
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(SegmentFetchError::Redirect)?;
        let next_url =
            response_url.join(location).or_else(|_| Url::parse(location)).map_err(|_| SegmentFetchError::Redirect)?;
        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

fn classify_segment_response(response: reqwest::Response) -> Result<reqwest::Response, SegmentFetchError> {
    let status = response.status();
    match classify_hls_resource_status(status) {
        HlsResourceStatusClass::Success => Ok(response),
        HlsResourceStatusClass::Retryable => Err(SegmentFetchError::RetryableStatus(status)),
        HlsResourceStatusClass::Permanent => Err(SegmentFetchError::PermanentStatus(status)),
        HlsResourceStatusClass::NonRetryable => Err(SegmentFetchError::NonRetryableStatus(status)),
    }
}

fn build_segment_origin_headers(
    source_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, SegmentFetchError> {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    force_identity_without_range(&mut headers);
    if let Some(byte_range) = byte_range {
        let end = byte_range
            .offset
            .checked_add(byte_range.length)
            .and_then(|end_exclusive| end_exclusive.checked_sub(1))
            .ok_or(SegmentFetchError::InvalidByteRange)?;
        let range_value = format!("bytes={}-{}", byte_range.offset, end);
        let range_value = HeaderValue::from_str(&range_value).map_err(|_| SegmentFetchError::InvalidByteRange)?;
        headers.insert(header::RANGE, range_value);
    }
    Ok(headers)
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    scrub_hls_origin_headers(headers, None);
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
        model::{HlsSegmentRepairConfig, HlsSegmentRepairMode},
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    };
    use axum::http::{header, HeaderMap, HeaderValue};
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
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

        assert_eq!(policy.demand_wait_timeout(), Duration::from_secs(61));
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
        let headers = build_segment_origin_headers(&headers, None).expect("headers should build");

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
            Some(crate::processing::parser::hls::origin_manifest::ParsedByteRange { offset: 10, length: 5 }),
        )
        .expect("headers should build");

        assert_eq!(headers.get(header::RANGE).expect("range"), "bytes=10-14");
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
