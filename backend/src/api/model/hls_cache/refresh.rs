use super::{
    begin_hls_origin_account_io, finish_hls_origin_account_io, hls_origin_headers_with_provider_session,
    is_hls_provisioning_gap_segment, is_hls_provisioning_segment,
    manifest_fetch::{
        commit_error_to_fetch_error, evaluate_manifest_origin_quality_with_mode, fetch_hls_origin_manifest_request,
        fetched_effective_manifest_host, log_hls_manifest_initial_selected, manifest_host_switch_failure_threshold,
        manifest_origin_quality_from_candidate, next_committed_origin_highwater,
        retry_hls_origin_manifest_recovery_chain, score_hls_manifest_candidate_for_selection_log,
        FetchedOriginManifest, HlsManifestAcceptanceRejectReason, HlsManifestCommitAcceptanceMode,
        HlsManifestCommitError, HlsManifestOriginQuality, HlsManifestOriginRelation, HlsManifestRejectLogReason,
        HlsManifestSequenceRelation, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest, LiveHlsOriginEntry,
        OriginManifestFetchError, RetryPolicy,
    },
    safe_hls_access_lease_id, safe_origin_log_value, safe_proxy_session_id, safe_session_key,
    sanitized_hls_origin_headers, HlsAccessLeaseChannelUnavailableReason, HlsAccessLeaseId,
    HlsFreshManifestRequiredReason, HlsManifestRenderer, HlsManifestTemporaryFailureKind,
    HlsManifestTemporaryFailureTransition, HlsMapWorkerPool, HlsOriginIoContext, HlsOriginWorkClass, HlsProxyManager,
    HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle, HlsSessionMode, MapFetchContext,
    RenderedManifestStoreOutcome, RenderedManifestStoreRejectReason, SegmentFetchContext, TransientPassthroughReason,
};
use crate::{
    model::{AppConfig, HlsManifestRecoveryBurstConfig, ReverseProxyDisabledHeaderConfig, StripConfig},
    processing::parser::hls::{
        initial_strip::initial_hls_strip_segments_for_durations,
        origin_manifest::{
            parse_manifest_timing, parse_manifest_validity, parse_origin_manifest_timeline,
            parse_origin_media_manifest, OriginManifestParseOutcome, OriginManifestTransientReason,
            ParsedOriginManifest, ParsedOriginManifestTimeline,
        },
        transient_manifest::{
            apply_transient_discontinuity_sequence, materialize_transient_provisioning_handoff_view,
            transient_discontinuity_sequence, transient_visible_discontinuity_count, TransientManifestRewriter,
            TransientRewriteOptions,
        },
    },
};
use axum::http::HeaderMap;
use log::{debug, info, warn};
use reqwest::Client;
use shared::{model::HlsStripMode, utils::sanitize_sensitive_info};
use std::sync::Arc;
use url::Url;

const COLD_START_RETRY_AFTER_SECONDS: u64 = 2;
const FIRST_FAILURE_BACKOFF_MS: u64 = 0;
const SECOND_FAILURE_BACKOFF_MS: u64 = 500;
const LATER_FAILURE_BACKOFF_MS: u64 = 1_000;

/// Debounce and singleflight state for one live HLS origin manifest.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct OriginRefreshState {
    pub last_fetch_started_at_ms: Option<u64>,
    pub last_fetch_finished_at_ms: Option<u64>,
    pub next_fetch_allowed_at_ms: u64,
    pub consecutive_failures: u32,
    pub consecutive_empty_refreshes: u32,
    pub last_success_at_ms: Option<u64>,
    pub last_error_at_ms: Option<u64>,
    pub in_flight: bool,
}

impl OriginRefreshState {
    pub fn is_due(&self, now_ms: u64) -> bool { now_ms >= self.next_fetch_allowed_at_ms && !self.in_flight }

    pub fn mark_started(&mut self, now_ms: u64) {
        self.last_fetch_started_at_ms = Some(now_ms);
        self.in_flight = true;
    }

    fn mark_success_with_timing(
        &mut self,
        fetch_started_at_ms: u64,
        fetch_finished_at_ms: u64,
        timing: HlsManifestRefreshTiming,
    ) -> u64 {
        let refresh_interval_ms = match timing.progress {
            HlsManifestProgress::Advanced | HlsManifestProgress::Rollover => {
                self.consecutive_empty_refreshes = 0;
                timing.base_interval_ms
            }
            HlsManifestProgress::Unchanged => {
                self.consecutive_empty_refreshes = self.consecutive_empty_refreshes.saturating_add(1);
                apply_empty_refresh_rampdown_ms(timing.base_interval_ms, self.consecutive_empty_refreshes)
            }
        };
        self.last_fetch_finished_at_ms = Some(fetch_finished_at_ms);
        self.last_success_at_ms = Some(fetch_finished_at_ms);
        self.last_error_at_ms = None;
        self.consecutive_failures = 0;
        self.in_flight = false;
        self.next_fetch_allowed_at_ms = fetch_started_at_ms.saturating_add(refresh_interval_ms);
        refresh_interval_ms
    }

    pub fn mark_failure(&mut self, failed_at_ms: u64) {
        self.last_fetch_finished_at_ms = Some(failed_at_ms);
        self.last_error_at_ms = Some(failed_at_ms);
        let next_retry_delay_ms = self.next_failure_backoff_ms();
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.in_flight = false;
        self.next_fetch_allowed_at_ms = failed_at_ms.saturating_add(next_retry_delay_ms);
    }

    fn next_failure_backoff_ms(&self) -> u64 {
        match self.consecutive_failures {
            0 => FIRST_FAILURE_BACKOFF_MS,
            1 => SECOND_FAILURE_BACKOFF_MS,
            _ => LATER_FAILURE_BACKOFF_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HlsManifestAcceptanceDecision {
    Accept { quality: HlsManifestOriginQuality },
    RetryCurrentTarget { quality: HlsManifestOriginQuality },
    AcceptHostSwitch { quality: HlsManifestOriginQuality },
    Reject { reason: HlsManifestAcceptanceRejectReason, quality: HlsManifestOriginQuality },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsManifestProgress {
    Advanced,
    Rollover,
    Unchanged,
}

impl HlsManifestProgress {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::Advanced => "advanced",
            Self::Rollover => "rollover",
            Self::Unchanged => "unchanged",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsManifestTimingSource {
    LastSegmentDuration,
    TargetDuration,
    Fallback,
}

impl HlsManifestTimingSource {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::LastSegmentDuration => "last_segment_duration",
            Self::TargetDuration => "target_duration",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct HlsManifestRefreshTiming {
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
    base_interval_ms: u64,
    source: HlsManifestTimingSource,
    progress: HlsManifestProgress,
}

#[derive(Clone)]
pub struct OriginRefreshRequest {
    pub app_config: Arc<AppConfig>,
    pub session: HlsSessionHandle,
    pub origin_entry: LiveHlsOriginEntry,
    pub headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub hls_proxy: Arc<HlsProxyManager>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub segment_worker_pool: Arc<HlsSegmentWorkerPool>,
    pub map_worker_pool: Arc<HlsMapWorkerPool>,
    pub origin_manifest_timeout_ms: u64,
    pub manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    pub strip: StripConfig,
    pub retry_policy: RetryPolicy,
    pub reverse_proxy_rewrite_secret: Vec<u8>,
    pub transient_resource_ttl_ms: u64,
    pub manifest_commit_requirement: HlsManifestCommitRequirement,
    pub access_lease_id: Option<HlsAccessLeaseId>,
    pub now_ms: u64,
    pub origin_io: Option<HlsOriginIoContext>,
}

/// Controls whether a canonical HLS refresh may rely on an existing committed manifest.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsManifestCommitRequirement {
    CommittedManifestAllowed,
    FreshCommitRequired { reason: HlsFreshManifestRequiredReason },
}

impl HlsManifestCommitRequirement {
    const fn fresh_reason(self) -> Option<HlsFreshManifestRequiredReason> {
        match self {
            Self::CommittedManifestAllowed => None,
            Self::FreshCommitRequired { reason } => Some(reason),
        }
    }

    const fn acceptance_mode(self) -> HlsManifestCommitAcceptanceMode {
        match self {
            Self::CommittedManifestAllowed => HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            Self::FreshCommitRequired { .. } => HlsManifestCommitAcceptanceMode::FreshBaseline,
        }
    }
}

pub async fn maybe_trigger_origin_refresh(mut request: OriginRefreshRequest) -> bool {
    let fetch_started_at_ms = request.now_ms;
    if !mark_origin_refresh_started(&mut request, fetch_started_at_ms).await {
        release_preacquired_origin_provider_handle(&request).await;
        return false;
    }

    tokio::spawn(async move {
        refresh_and_commit(request, fetch_started_at_ms).await;
    });
    true
}

pub async fn trigger_origin_refresh_sync(mut request: OriginRefreshRequest) -> bool {
    let fetch_started_at_ms = request.now_ms;
    if !mark_origin_refresh_started(&mut request, fetch_started_at_ms).await {
        release_preacquired_origin_provider_handle(&request).await;
        return false;
    }

    refresh_and_commit(request, fetch_started_at_ms).await;
    true
}

async fn mark_origin_refresh_started(request: &mut OriginRefreshRequest, fetch_started_at_ms: u64) -> bool {
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    {
        let mut session = request.session.write().await;
        if session.is_gc_marked_for_removal() {
            return false;
        }
        if session.origin_refresh.in_flight {
            metrics.record_refresh_skipped();
            let last_fetch_started_at_ms = session
                .origin_refresh
                .last_fetch_started_at_ms
                .map_or_else(|| "<none>".to_string(), |started_at_ms| started_at_ms.to_string());
            let in_flight_for_ms = session.origin_refresh.last_fetch_started_at_ms.map_or_else(
                || "<unknown>".to_string(),
                |started_at_ms| fetch_started_at_ms.saturating_sub(started_at_ms).to_string(),
            );
            debug!(
                "HLS origin manifest refresh skipped: session={} proxy_session_id={} reason=in_flight last_fetch_started_at_ms={} in_flight_for_ms={} now_ms={fetch_started_at_ms}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                last_fetch_started_at_ms,
                in_flight_for_ms
            );
            return false;
        }
        if fetch_started_at_ms < session.origin_refresh.next_fetch_allowed_at_ms
            && request.manifest_commit_requirement.fresh_reason().is_none()
        {
            metrics.record_refresh_skipped();
            let wait_ms = session.origin_refresh.next_fetch_allowed_at_ms.saturating_sub(fetch_started_at_ms);
            debug!(
                "HLS origin manifest refresh skipped: session={} proxy_session_id={} reason=debounce next_fetch_allowed_at_ms={} now_ms={fetch_started_at_ms} wait_ms={wait_ms}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                session.origin_refresh.next_fetch_allowed_at_ms
            );
            return false;
        }
        if let Some(origin_io) = request.origin_io.as_mut() {
            origin_io.started_generation = Some(session.start_origin_work());
        }
        session.origin_refresh.mark_started(fetch_started_at_ms);
        metrics.record_refresh_started();
        info!(
            "HLS origin manifest refresh started: session={} proxy_session_id={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id)
        );
    }
    true
}

#[allow(clippy::too_many_lines)]
async fn refresh_and_commit(mut request: OriginRefreshRequest, fetch_started_at_ms: u64) {
    request.headers = sanitized_hls_origin_headers(&request.headers, request.disabled_headers.as_ref());
    let provider_lease = if let Some(origin_io) = request.origin_io.as_ref() {
        let binding = request.session.read().await.origin_account_binding.clone();
        if let Some(binding) = binding {
            match begin_hls_origin_account_io(origin_io, &request.session, &binding).await {
                Ok(guard) => {
                    debug!(
                        "HLS provider session lease joined for manifest refresh: provider={}",
                        sanitize_sensitive_info(binding.account_name.as_ref())
                    );
                    Some((origin_io.clone(), guard))
                }
                Err(kind) => {
                    touch_refresh_origin_account_binding(&request, false).await;
                    let _ = finish_refresh_origin_work(&request, current_time_millis()).await;
                    finish_refresh_failure(&request, OriginManifestFetchError::ProviderUnavailable(kind)).await;
                    return;
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    let result = fetch_and_commit_manifest_with_policy(&mut request).await;
    let fetch_finished_at_ms = current_time_millis();
    let origin_work_state = finish_refresh_origin_work(&request, fetch_finished_at_ms).await;
    if let Some((origin_io, guard)) = provider_lease {
        let binding = guard.binding().clone();
        finish_hls_origin_account_io(
            &origin_io,
            &request.session,
            guard,
            origin_work_state.generation_valid && origin_work_state.refresh_reservation,
        )
        .await;
        debug!(
            "HLS provider session lease released after manifest refresh: provider={}",
            sanitize_sensitive_info(binding.account_name.as_ref())
        );
        touch_refresh_origin_account_binding(
            &request,
            origin_work_state.generation_valid && origin_work_state.refresh_reservation,
        )
        .await;
    }
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    let (
        should_wake_segment_scheduler,
        should_wake_map_scheduler,
        fresh_manifest_failure_reason,
        temporary_manifest_failure_reason,
        pending_manifest_follow_up,
    ) = {
        let mut session = request.session.write().await;
        match result {
            Ok(CommittedOriginManifest { fetched, refresh_timing, wake_segment_scheduler, wake_map_scheduler }) => {
                let pending_manifest_follow_up = Some((session.proxy_session_id.clone(), session.target_duration));
                let applied_refresh_interval_ms = session.origin_refresh.mark_success_with_timing(
                    fetch_started_at_ms,
                    fetch_finished_at_ms,
                    refresh_timing,
                );
                if let Some(reset_failures) = session.record_successful_manifest_fetch() {
                    debug!(
                        "HLS manifest temporary failure counter reset: session={} previous_failures={reset_failures}",
                        safe_session_key(&session.key)
                    );
                }
                log_manifest_refresh_timing(&session, refresh_timing, applied_refresh_interval_ms);
                metrics.record_refresh_completed();
                for _ in 1..fetched.attempts {
                    metrics.record_refresh_retried();
                }
                info!(
                    "HLS origin manifest refresh completed: session={} proxy_session_id={} final_url={} redirect_host={} status={} attempts={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    safe_origin_log_value(&fetched.final_manifest_url),
                    fetched.redirect_host.as_deref().unwrap_or("<none>"),
                    fetched.status.as_u16(),
                    fetched.attempts
                );
                if origin_work_state.generation_valid {
                    (wake_segment_scheduler, wake_map_scheduler, None, None, pending_manifest_follow_up)
                } else {
                    session.invalidate_queued_origin_work();
                    (false, false, None, None, pending_manifest_follow_up)
                }
            }
            Err(err) => {
                session.origin_refresh.mark_failure(fetch_finished_at_ms);
                metrics.record_refresh_failed();
                let temporary_manifest_failure_reason = record_temporary_manifest_fetch_failure_if_needed(
                    &mut session,
                    &request.strip,
                    &err,
                    fetch_finished_at_ms,
                );
                if manifest_hard_fetch_error(&err) {
                    session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
                    debug!(
                        "HLS manifest marked fresh-commit required after hard fetch failure: session={}",
                        safe_session_key(&session.key)
                    );
                }
                warn!(
                    "HLS origin manifest refresh completed: session={} proxy_session_id={} result=failed error={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    err.log_label()
                );
                (
                    false,
                    false,
                    request.manifest_commit_requirement.fresh_reason(),
                    temporary_manifest_failure_reason,
                    None,
                )
            }
        }
    };
    if let Some((proxy_session_id, target_duration)) = pending_manifest_follow_up {
        let shortened = request
            .hls_proxy
            .mark_pending_manifest_follow_up_for_session(&proxy_session_id, fetch_finished_at_ms, target_duration)
            .await;
        if shortened > 0 {
            debug!(
                "HLS pending manifest leases shortened after manifest commit: session={} leases={shortened}",
                safe_proxy_session_id(&proxy_session_id)
            );
        }
    }
    if let Some(reason) = fresh_manifest_failure_reason {
        mark_fresh_manifest_commit_failed_access_leases(&request, fetch_finished_at_ms, reason).await;
    }
    if let Some((failures, threshold)) = temporary_manifest_failure_reason {
        mark_manifest_temporary_failure_access_leases(&request, fetch_finished_at_ms, failures, threshold).await;
    }

    let origin_provider_session_headers = request.session.read().await.origin_provider_session_headers.clone();

    if should_wake_map_scheduler {
        request
            .map_worker_pool
            .wake_scheduler(
                MapFetchContext {
                    session: Arc::clone(&request.session),
                    segment_cache: Arc::clone(&request.segment_cache),
                    headers: request.headers.clone(),
                    origin_provider_session_headers: origin_provider_session_headers.clone(),
                    client: request.client.clone(),
                    no_redirect_client: request.no_redirect_client.clone(),
                    use_manual_redirects: request.use_manual_redirects,
                    origin_io: request
                        .origin_io
                        .clone()
                        .map(|origin_io| origin_io.with_grace(HlsOriginWorkClass::Background.allows_grace())),
                },
                fetch_finished_at_ms,
            )
            .await;
    }

    if should_wake_segment_scheduler {
        request
            .segment_worker_pool
            .wake_scheduler(
                SegmentFetchContext {
                    session: request.session,
                    segment_cache: request.segment_cache,
                    segment_repair: request.segment_repair,
                    repair_access_lease_id: None,
                    headers: request.headers,
                    origin_provider_session_headers,
                    client: request.client,
                    no_redirect_client: request.no_redirect_client,
                    use_manual_redirects: request.use_manual_redirects,
                    origin_io: request
                        .origin_io
                        .map(|origin_io| origin_io.with_grace(HlsOriginWorkClass::Background.allows_grace())),
                },
                fetch_finished_at_ms,
            )
            .await;
    }
}

async fn mark_fresh_manifest_commit_failed_access_leases(
    request: &OriginRefreshRequest,
    failed_at_ms: u64,
    reason: HlsFreshManifestRequiredReason,
) {
    let Some(origin_io) = request.origin_io.as_ref() else {
        return;
    };
    let unavailable_reason = HlsAccessLeaseChannelUnavailableReason::ManifestCommitFailed { reason };
    if let Some(access_lease_id) = request.access_lease_id.as_ref() {
        let marked = origin_io
            .app_state
            .hls_proxy
            .mark_access_lease_channel_unavailable(access_lease_id, failed_at_ms, unavailable_reason)
            .await;
        if marked {
            let proxy_session_id = request.session.read().await.proxy_session_id.clone();
            debug!(
                "HLS access lease marked channel unavailable after fresh manifest commit failed: session={} lease={} reason={reason:?}",
                safe_proxy_session_id(&proxy_session_id),
                safe_hls_access_lease_id(access_lease_id)
            );
        }
        return;
    }

    let proxy_session_id = request.session.read().await.proxy_session_id.clone();
    let marked = origin_io
        .app_state
        .hls_proxy
        .mark_access_leases_channel_unavailable_for_session(&proxy_session_id, failed_at_ms, unavailable_reason)
        .await;
    if marked > 0 {
        debug!(
            "HLS access leases marked channel unavailable after fresh manifest commit failed: session={} marked={marked} reason={reason:?}",
            safe_proxy_session_id(&proxy_session_id)
        );
    }
}

fn record_temporary_manifest_fetch_failure_if_needed(
    session: &mut super::HlsSession,
    strip: &StripConfig,
    err: &OriginManifestFetchError,
    failed_at_ms: u64,
) -> Option<(u32, u32)> {
    let kind = manifest_temporary_failure_kind(err)?;
    let threshold = manifest_host_switch_failure_threshold(session, strip);
    match session.record_temporary_manifest_fetch_failure(failed_at_ms, kind, threshold) {
        HlsManifestTemporaryFailureTransition::StillRetryable { failures, threshold } => {
            debug!(
                "HLS manifest temporary failure counted: session={} failures={} threshold={}",
                safe_session_key(&session.key),
                failures,
                threshold
            );
            None
        }
        HlsManifestTemporaryFailureTransition::BecameChannelUnavailable { failures, threshold } => {
            debug!(
                "HLS manifest temporary failure threshold reached: session={} failures={} threshold={}",
                safe_session_key(&session.key),
                failures,
                threshold
            );
            Some((failures, threshold))
        }
    }
}

fn manifest_temporary_failure_kind(err: &OriginManifestFetchError) -> Option<HlsManifestTemporaryFailureKind> {
    match err {
        OriginManifestFetchError::Timeout => Some(HlsManifestTemporaryFailureKind::Timeout),
        OriginManifestFetchError::RetryableStatus(status, _) => {
            Some(HlsManifestTemporaryFailureKind::RetryableStatus { status: *status })
        }
        OriginManifestFetchError::Request(message) if request_error_indicates_timeout(message) => {
            Some(HlsManifestTemporaryFailureKind::Timeout)
        }
        OriginManifestFetchError::ProviderUnavailable(kind) if kind.is_retryable_resource_failure() => {
            Some(HlsManifestTemporaryFailureKind::ProviderAcquire { kind: *kind })
        }
        OriginManifestFetchError::PermanentStatus(_)
        | OriginManifestFetchError::RetryExhausted
        | OriginManifestFetchError::NonRetryableStatus(_)
        | OriginManifestFetchError::Request(_)
        | OriginManifestFetchError::Redirect(_)
        | OriginManifestFetchError::ProviderUnavailable(_)
        | OriginManifestFetchError::ContentCoding(_)
        | OriginManifestFetchError::ContentDecoding { .. }
        | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
        | OriginManifestFetchError::InvalidUtf8 { .. } => None,
    }
}

fn manifest_hard_fetch_error(err: &OriginManifestFetchError) -> bool {
    match err {
        OriginManifestFetchError::PermanentStatus(_)
        | OriginManifestFetchError::NonRetryableStatus(_)
        | OriginManifestFetchError::ContentCoding(
            crate::utils::content_coding::ContentCodingError::InvalidHeader
            | crate::utils::content_coding::ContentCodingError::Unsupported(_)
            | crate::utils::content_coding::ContentCodingError::EncodedPartialContent,
        )
        | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
        | OriginManifestFetchError::InvalidUtf8 { .. } => true,
        OriginManifestFetchError::ProviderUnavailable(kind) => !kind.is_retryable_resource_failure(),
        OriginManifestFetchError::RetryableStatus(_, _)
        | OriginManifestFetchError::RetryExhausted
        | OriginManifestFetchError::Request(_)
        | OriginManifestFetchError::Redirect(_)
        | OriginManifestFetchError::Timeout
        | OriginManifestFetchError::ContentCoding(crate::utils::content_coding::ContentCodingError::PrefixRead(_))
        | OriginManifestFetchError::ContentDecoding { .. } => false,
    }
}

fn request_error_indicates_timeout(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timeout") || lower.contains("timed out")
}

async fn mark_manifest_temporary_failure_access_leases(
    request: &OriginRefreshRequest,
    failed_at_ms: u64,
    failures: u32,
    threshold: u32,
) {
    let Some(origin_io) = request.origin_io.as_ref() else {
        return;
    };
    let proxy_session_id = request.session.read().await.proxy_session_id.clone();
    let marked = origin_io
        .app_state
        .hls_proxy
        .mark_access_leases_channel_unavailable_for_session(
            &proxy_session_id,
            failed_at_ms,
            HlsAccessLeaseChannelUnavailableReason::ManifestTemporaryFailureThreshold { failures, threshold },
        )
        .await;
    if marked > 0 {
        debug!(
            "HLS access leases marked channel unavailable after temporary manifest failures: session={} marked={marked} failures={} threshold={}",
            safe_proxy_session_id(&proxy_session_id),
            failures,
            threshold
        );
    }
}

#[derive(Clone, Copy)]
struct OriginWorkFinishState {
    generation_valid: bool,
    refresh_reservation: bool,
}

async fn finish_refresh_origin_work(request: &OriginRefreshRequest, now_ms: u64) -> OriginWorkFinishState {
    let Some(started_generation) = request.origin_io.as_ref().and_then(|origin_io| origin_io.started_generation) else {
        return OriginWorkFinishState { generation_valid: true, refresh_reservation: false };
    };
    let mut session = request.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(now_ms);
    OriginWorkFinishState { generation_valid, refresh_reservation }
}

async fn finish_refresh_failure(request: &OriginRefreshRequest, err: OriginManifestFetchError) {
    release_preacquired_origin_provider_handle(request).await;
    let fetch_finished_at_ms = current_time_millis();
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    let (fresh_manifest_failure_reason, temporary_manifest_failure_reason) = {
        let mut session = request.session.write().await;
        session.origin_refresh.mark_failure(fetch_finished_at_ms);
        metrics.record_refresh_failed();
        let temporary_manifest_failure_reason =
            record_temporary_manifest_fetch_failure_if_needed(&mut session, &request.strip, &err, fetch_finished_at_ms);
        if manifest_hard_fetch_error(&err) {
            session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
            debug!(
                "HLS manifest marked fresh-commit required after hard fetch failure: session={}",
                safe_session_key(&session.key)
            );
        }
        warn!(
            "HLS origin manifest refresh completed: session={} proxy_session_id={} result=failed error={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            err.log_label()
        );
        (request.manifest_commit_requirement.fresh_reason(), temporary_manifest_failure_reason)
    };
    if let Some(reason) = fresh_manifest_failure_reason {
        mark_fresh_manifest_commit_failed_access_leases(request, fetch_finished_at_ms, reason).await;
    }
    if let Some((failures, threshold)) = temporary_manifest_failure_reason {
        mark_manifest_temporary_failure_access_leases(request, fetch_finished_at_ms, failures, threshold).await;
    }
}

async fn release_preacquired_origin_provider_handle(request: &OriginRefreshRequest) {
    let Some(origin_io) = request.origin_io.as_ref() else {
        return;
    };
    let Some(handle) = origin_io.take_preacquired_provider_handle().await else {
        return;
    };
    let binding = request.session.read().await.origin_account_binding.clone();
    if let Some(binding) = binding {
        origin_io.app_state.connection_manager.release_provider_handle(Some(handle)).await;
        debug!(
            "HLS provider handle released after manifest refresh: provider={} reason=refresh-not-started",
            sanitize_sensitive_info(binding.account_name.as_ref())
        );
    } else {
        origin_io.app_state.connection_manager.release_provider_handle(Some(handle)).await;
    }
}

async fn touch_refresh_origin_account_binding(request: &OriginRefreshRequest, reservation_refreshed: bool) {
    let mut session = request.session.write().await;
    if let Some(binding) = session.origin_account_binding.as_mut() {
        let now_ms = current_time_millis();
        binding.last_origin_io_at_ms = Some(now_ms);
        if reservation_refreshed {
            binding.last_reservation_refresh_at_ms = Some(now_ms);
        }
    }
}

struct CommittedOriginManifest {
    fetched: FetchedOriginManifest,
    refresh_timing: HlsManifestRefreshTiming,
    wake_segment_scheduler: bool,
    wake_map_scheduler: bool,
}

fn manifest_fetch_context(request: &OriginRefreshRequest) -> HlsOriginManifestFetchContext {
    HlsOriginManifestFetchContext {
        app_config: Arc::clone(&request.app_config),
        session: Arc::clone(&request.session),
        origin_entry: request.origin_entry.clone(),
        headers: hls_origin_headers_with_provider_session(&request.headers, &request.origin_provider_session_headers),
        client: request.client.clone(),
        no_redirect_client: request.no_redirect_client.clone(),
        use_manual_redirects: request.use_manual_redirects,
        origin_manifest_timeout_ms: request.origin_manifest_timeout_ms,
        manifest_recovery_burst: request.manifest_recovery_burst.clone(),
        retry_policy: request.retry_policy.clone(),
    }
}

#[allow(clippy::too_many_lines)]
async fn fetch_and_commit_manifest_with_policy(
    request: &mut OriginRefreshRequest,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let fetch_context = manifest_fetch_context(request);
    let fetched =
        fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&fetch_context)).await?;
    let acceptance_mode = request.manifest_commit_requirement.acceptance_mode();
    let selected_report =
        score_hls_manifest_candidate_for_selection_log(&fetch_context, &fetched, acceptance_mode).await;
    let commit_result = {
        let mut session = request.session.write().await;
        commit_fetched_manifest(&mut session, &fetched, request, current_time_millis())
    };

    match commit_result {
        Ok((refresh_timing, wake_segment_scheduler, wake_map_scheduler)) => {
            if let Some(report) = selected_report.as_ref() {
                log_hls_manifest_initial_selected(&fetch_context, report).await;
            }
            Ok(CommittedOriginManifest { fetched, refresh_timing, wake_segment_scheduler, wake_map_scheduler })
        }
        Err(HlsManifestCommitError::RetryCurrentTarget) => {
            let target_url = Url::parse(&fetched.resolved_request_url)
                .map_err(|err| OriginManifestFetchError::Request(err.to_string()))?;
            let last_error = match retry_hls_origin_manifest_recovery_chain(
                &fetch_context,
                target_url,
                fetched.provider_url_index,
                None,
                |fetched, acceptance_mode| commit_manifest_recovery_candidate(request, fetched, acceptance_mode),
            )
            .await
            {
                Ok(committed) => return Ok(committed),
                Err(err) => err,
            };

            let decision = {
                let mut session = request.session.write().await;
                record_pinned_host_recovery_chain_failed(&mut session, &request.strip, current_time_millis())
            };
            match decision {
                HlsManifestAcceptanceDecision::AcceptHostSwitch { .. } => {
                    let candidate_commit_result = {
                        let mut session = request.session.write().await;
                        commit_fetched_manifest_with_acceptance_mode(
                            &mut session,
                            &fetched,
                            request,
                            current_time_millis(),
                            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
                        )
                    };
                    match candidate_commit_result {
                        Ok((refresh_timing, wake_segment_scheduler, wake_map_scheduler)) => {
                            if let Some(report) = selected_report.as_ref() {
                                log_hls_manifest_initial_selected(&fetch_context, report).await;
                            }
                            Ok(CommittedOriginManifest {
                                fetched,
                                refresh_timing,
                                wake_segment_scheduler,
                                wake_map_scheduler,
                            })
                        }
                        Err(err) => Err(commit_error_to_fetch_error(&err)),
                    }
                }
                HlsManifestAcceptanceDecision::Reject { reason, .. } => {
                    debug!(
                        "HLS origin manifest host switch held: origin_entry={} reason={reason:?}",
                        safe_origin_log_value(request.origin_entry.url().as_str())
                    );
                    Err(last_error)
                }
                HlsManifestAcceptanceDecision::Accept { .. }
                | HlsManifestAcceptanceDecision::RetryCurrentTarget { .. } => Err(last_error),
            }
        }
        Err(HlsManifestCommitError::TimelineRejected { reason }) => {
            let target_url =
                Url::parse(&fetched.resolved_request_url).map_err(|_| OriginManifestFetchError::RetryExhausted)?;
            retry_hls_origin_manifest_recovery_chain(
                &fetch_context,
                target_url,
                fetched.provider_url_index,
                Some(reason),
                |fetched, acceptance_mode| commit_manifest_recovery_candidate(request, fetched, acceptance_mode),
            )
            .await
        }
    }
}

async fn commit_manifest_recovery_candidate(
    request: &OriginRefreshRequest,
    fetched: FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<CommittedOriginManifest, HlsManifestCommitError> {
    let commit_result = {
        let mut session = request.session.write().await;
        commit_fetched_manifest_with_acceptance_mode(
            &mut session,
            &fetched,
            request,
            current_time_millis(),
            acceptance_mode,
        )
    };
    match commit_result {
        Ok((refresh_timing, wake_segment_scheduler, wake_map_scheduler)) => {
            Ok(CommittedOriginManifest { fetched, refresh_timing, wake_segment_scheduler, wake_map_scheduler })
        }
        Err(err) => Err(err),
    }
}

fn commit_fetched_manifest(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
) -> Result<(HlsManifestRefreshTiming, bool, bool), HlsManifestCommitError> {
    commit_fetched_manifest_with_acceptance_mode(
        session,
        fetched,
        request,
        fetch_finished_at_ms,
        request.manifest_commit_requirement.acceptance_mode(),
    )
}

fn commit_fetched_manifest_with_acceptance_mode(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<(HlsManifestRefreshTiming, bool, bool), HlsManifestCommitError> {
    let existing_transient_reason = match &session.mode {
        HlsSessionMode::TransientPassthrough { reason } => Some(reason.clone()),
        HlsSessionMode::NormalCacheTimeline => None,
    };

    match (existing_transient_reason, parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)) {
        (None, OriginManifestParseOutcome::Normal(manifest)) => {
            let quality = evaluate_manifest_acceptance_for_commit(
                session,
                fetched,
                ParsedOriginManifestTimeline {
                    origin_manifest_sequence: manifest.origin_manifest_sequence,
                    origin_manifest_segment_cnt: manifest.origin_manifest_segment_cnt,
                },
                request,
                fetch_finished_at_ms,
                acceptance_mode,
            )?;
            mark_manifest_handoff_discontinuity_if_needed(session, &quality);
            let result = commit_normal_manifest(
                session,
                &manifest,
                fetched.redirect_host.as_deref(),
                request,
                &fetched.resolved_request_url,
                fetch_finished_at_ms,
                quality.sequence_relation,
            );
            result.map(|refresh_timing| {
                update_origin_provider_session_headers(session, fetched);
                mark_manifest_acceptance_success(session, fetched, &quality);
                (refresh_timing, true, true)
            })
        }
        (Some(reason), _) => {
            let timeline = parse_transient_manifest_timeline_for_commit(session, &fetched.body)?;
            let quality = evaluate_manifest_acceptance_for_commit(
                session,
                fetched,
                timeline,
                request,
                fetch_finished_at_ms,
                acceptance_mode,
            )?;
            mark_manifest_handoff_discontinuity_if_needed(session, &quality);
            let refresh_timing = commit_transient_manifest(
                session,
                &fetched.body,
                &fetched.final_manifest_url,
                &fetched.resolved_request_url,
                fetched.redirect_host.as_deref(),
                &request.headers,
                reason,
                &request.reverse_proxy_rewrite_secret,
                request.transient_resource_ttl_ms,
                fetch_finished_at_ms,
                timeline,
                &quality,
                &request.strip,
            );
            update_origin_provider_session_headers(session, fetched);
            mark_manifest_acceptance_success(session, fetched, &quality);
            Ok((refresh_timing, false, false))
        }
        (None, OriginManifestParseOutcome::TransientPassthrough { reason }) => {
            let timeline = parse_transient_manifest_timeline_for_commit(session, &fetched.body)?;
            let quality = evaluate_manifest_acceptance_for_commit(
                session,
                fetched,
                timeline,
                request,
                fetch_finished_at_ms,
                acceptance_mode,
            )?;
            request.segment_worker_pool.metrics().record_transient_switch();
            mark_manifest_handoff_discontinuity_if_needed(session, &quality);
            let refresh_timing = commit_transient_manifest(
                session,
                &fetched.body,
                &fetched.final_manifest_url,
                &fetched.resolved_request_url,
                fetched.redirect_host.as_deref(),
                &request.headers,
                map_transient_reason(reason),
                &request.reverse_proxy_rewrite_secret,
                request.transient_resource_ttl_ms,
                fetch_finished_at_ms,
                timeline,
                &quality,
                &request.strip,
            );
            update_origin_provider_session_headers(session, fetched);
            mark_manifest_acceptance_success(session, fetched, &quality);
            Ok((refresh_timing, false, false))
        }
    }
}

fn update_origin_provider_session_headers(session: &mut super::HlsSession, fetched: &FetchedOriginManifest) {
    if !fetched.provider_session_headers.is_empty() {
        session.origin_provider_session_headers = fetched.provider_session_headers.clone();
    }
}

fn parse_transient_manifest_timeline_for_commit(
    session: &super::HlsSession,
    body: &str,
) -> Result<ParsedOriginManifestTimeline, HlsManifestCommitError> {
    parse_origin_manifest_timeline(body).map_err(|reason| {
        warn!(
            "HLS origin manifest rejected: session={} proxy_session_id={} reason=malformed-transient-timeline error={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            safe_origin_log_value(format!("{reason:?}"))
        );
        HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::MalformedTransientTimeline }
    })
}

fn evaluate_manifest_acceptance_for_commit(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    request: &OriginRefreshRequest,
    now_ms: u64,
    mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestOriginQuality, HlsManifestCommitError> {
    let fetch_context = manifest_fetch_context(request);
    match evaluate_manifest_acceptance(session, fetched, timeline, &fetch_context, now_ms, mode) {
        HlsManifestAcceptanceDecision::Accept { quality }
        | HlsManifestAcceptanceDecision::AcceptHostSwitch { quality } => Ok(quality),
        HlsManifestAcceptanceDecision::RetryCurrentTarget { .. } => Err(HlsManifestCommitError::RetryCurrentTarget),
        HlsManifestAcceptanceDecision::Reject { reason, .. } => {
            let log_reason = HlsManifestRejectLogReason::from(reason.clone());
            warn!(
                "HLS origin manifest rejected: session={} proxy_session_id={} reason={} media_sequence={} segments={}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                log_reason.status_label(),
                timeline.origin_manifest_sequence,
                timeline.origin_manifest_segment_cnt
            );
            Err(HlsManifestCommitError::TimelineRejected { reason: log_reason })
        }
    }
}

fn evaluate_manifest_acceptance(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    fetch_context: &HlsOriginManifestFetchContext,
    now_ms: u64,
    mode: HlsManifestCommitAcceptanceMode,
) -> HlsManifestAcceptanceDecision {
    let quality = evaluate_manifest_origin_quality_with_mode(session, fetched, timeline, fetch_context, now_ms, mode);

    match quality.host_relation {
        HlsManifestOriginRelation::Initial
        | HlsManifestOriginRelation::SameRedirectHost
        | HlsManifestOriginRelation::UnknownHost => {
            if let Some(reason) = quality.reject_reason.clone() {
                return HlsManifestAcceptanceDecision::Reject { reason, quality };
            }
            HlsManifestAcceptanceDecision::Accept { quality }
        }
        HlsManifestOriginRelation::OtherRedirectHost => {
            let Some(effective_host) = quality.effective_host.clone() else {
                return HlsManifestAcceptanceDecision::Accept { quality };
            };

            upsert_host_switch_candidate(session, fetched, &quality, effective_host.clone(), now_ms);
            if matches!(mode, HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate)
                && session
                    .manifest_acceptance
                    .host_switch_candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.host == effective_host)
            {
                if let Some(reason) = quality.reject_reason.clone() {
                    return HlsManifestAcceptanceDecision::Reject { reason, quality };
                }
                return HlsManifestAcceptanceDecision::AcceptHostSwitch { quality };
            }

            HlsManifestAcceptanceDecision::RetryCurrentTarget { quality }
        }
    }
}

fn mark_manifest_handoff_discontinuity_if_needed(session: &mut super::HlsSession, quality: &HlsManifestOriginQuality) {
    if !quality.requires_handoff_discontinuity {
        return;
    }
    if matches!(quality.host_relation, HlsManifestOriginRelation::OtherRedirectHost) {
        session.mark_pending_origin_epoch_handoff_discontinuity(0);
    } else if session.pending_handoff_discontinuity_sequence.is_none() {
        session.mark_pending_handoff_discontinuity(0);
    }
}

fn upsert_host_switch_candidate(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    quality: &HlsManifestOriginQuality,
    host: String,
    now_ms: u64,
) {
    match session.manifest_acceptance.host_switch_candidate.as_mut() {
        Some(candidate) if candidate.host == host => {
            candidate.target_url.clone_from(&fetched.resolved_request_url);
            candidate.last_seen_at_ms = now_ms;
            candidate.seen_count = candidate.seen_count.saturating_add(1);
            candidate.highwater = quality.origin_highwater;
            candidate.quality_score = quality.score.rank();
        }
        _ => {
            session.manifest_acceptance.host_switch_candidate = Some(super::HlsManifestHostSwitchCandidate {
                host,
                target_url: fetched.resolved_request_url.clone(),
                first_seen_at_ms: now_ms,
                last_seen_at_ms: now_ms,
                seen_count: 1,
                highwater: quality.origin_highwater,
                quality_score: quality.score.rank(),
            });
        }
    }
}

fn record_pinned_host_recovery_chain_failed(
    session: &mut super::HlsSession,
    strip: &StripConfig,
    now_ms: u64,
) -> HlsManifestAcceptanceDecision {
    let Some(candidate) = session.manifest_acceptance.host_switch_candidate.as_mut() else {
        let quality = manifest_origin_quality_from_candidate(None);
        return HlsManifestAcceptanceDecision::Reject {
            reason: HlsManifestAcceptanceRejectReason::MissingPinnedTarget,
            quality,
        };
    };
    candidate.last_seen_at_ms = now_ms;
    let failures = session.manifest_acceptance.same_host_retry_chain_failures.saturating_add(1);
    session.manifest_acceptance.same_host_retry_chain_failures = failures;
    let threshold = manifest_host_switch_failure_threshold(session, strip);
    let quality = manifest_origin_quality_from_candidate(session.manifest_acceptance.host_switch_candidate.as_ref());
    if failures >= threshold {
        HlsManifestAcceptanceDecision::AcceptHostSwitch { quality }
    } else {
        HlsManifestAcceptanceDecision::Reject {
            reason: HlsManifestAcceptanceRejectReason::HostSwitchPending { failures, threshold },
            quality,
        }
    }
}

fn mark_manifest_acceptance_success(
    session: &mut super::HlsSession,
    fetched: &FetchedOriginManifest,
    quality: &HlsManifestOriginQuality,
) {
    if let Some(effective_host) = fetched_effective_manifest_host(fetched) {
        session.last_effective_manifest_host = Some(effective_host);
    }
    if quality.should_reset_stall_counter {
        session.manifest_acceptance.same_host_retry_chain_failures = 0;
        session.manifest_acceptance.host_switch_candidate = None;
    } else if quality.should_increment_stall_counter {
        session.manifest_acceptance.same_host_retry_chain_failures =
            session.manifest_acceptance.same_host_retry_chain_failures.saturating_add(1);
    }
    if matches!(quality.host_relation, HlsManifestOriginRelation::OtherRedirectHost) {
        session.manifest_acceptance.host_switch_candidate = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn commit_transient_manifest(
    session: &mut super::HlsSession,
    body: &str,
    final_manifest_url: &str,
    _resolved_request_url: &str,
    _redirect_host: Option<&str>,
    request_headers: &HeaderMap,
    reason: TransientPassthroughReason,
    reverse_proxy_rewrite_secret: &[u8],
    transient_resource_ttl_ms: u64,
    rendered_at_ms: u64,
    timeline: ParsedOriginManifestTimeline,
    quality: &HlsManifestOriginQuality,
    strip: &StripConfig,
) -> HlsManifestRefreshTiming {
    let was_normal = matches!(session.mode, HlsSessionMode::NormalCacheTimeline);
    let reason_log_fields = transient_reason_log_fields(&reason);
    session.mode = HlsSessionMode::TransientPassthrough { reason };
    if was_normal {
        info!(
            "HLS session switched to transient passthrough: session={} proxy_session_id={} {}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            reason_log_fields
        );
    }
    session.origin_request_headers = request_headers.clone();
    session.transient.set_resource_ttl_ms(transient_resource_ttl_ms);
    session.transient.prune_expired(rendered_at_ms);

    let previous_provisioning_manifest_body = session
        .last_rendered_manifest
        .as_ref()
        .filter(|_| session.segments.values().any(is_hls_provisioning_segment))
        .map(|rendered| rendered.body.clone());
    let provisioning_segment_duration_ms = session
        .segments
        .values()
        .find(|entry| is_hls_provisioning_segment(entry) || is_hls_provisioning_gap_segment(entry))
        .map_or(2_000, |entry| entry.duration_ms);
    let handoff_discontinuity_sequence = session.take_pending_handoff_discontinuity_sequence();
    let mut rewritten = if handoff_discontinuity_sequence.is_some() {
        TransientManifestRewriter::rewrite_with_options(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
            TransientRewriteOptions { handoff_discontinuity_sequence },
        )
    } else {
        TransientManifestRewriter::rewrite(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
        )
    };
    if handoff_discontinuity_sequence.is_some() {
        if let Some(handoff_body) = materialize_transient_provisioning_handoff_view(
            &rewritten.body,
            previous_provisioning_manifest_body.as_deref(),
            strip,
            provisioning_segment_duration_ms,
        ) {
            rewritten.body = handoff_body;
        }
        let current_discontinuity_sequence = transient_discontinuity_sequence(&rewritten.body)
            .unwrap_or(session.transient_discontinuity_sequence.unwrap_or(0));
        session.transient_discontinuity_sequence =
            Some(current_discontinuity_sequence.saturating_add(transient_visible_discontinuity_count(&rewritten.body)));
    } else if let Some(discontinuity_sequence) = session.transient_discontinuity_sequence {
        rewritten.body = apply_transient_discontinuity_sequence(&rewritten.body, discontinuity_sequence);
    }
    let manifest_validity = parse_manifest_validity(&rewritten.body);
    session.transient.upsert_resources(rewritten.resources);
    if let Some(validity) = manifest_validity {
        session.transient.replace_manifest_with_validity(rewritten.body, rendered_at_ms, validity.playlist_duration_ms);
    } else {
        session.transient.replace_manifest(rewritten.body, rendered_at_ms);
    }
    let previous_highwater = session.origin_seq_highwater;
    if let Some(highwater) = timeline.origin_highwater() {
        session.origin_seq_highwater =
            Some(next_committed_origin_highwater(session.origin_seq_highwater, highwater, quality.sequence_relation));
    }

    let timing = parse_manifest_timing(body);
    if let Some(target_duration_ms) = timing.target_duration_ms {
        if let Ok(target_duration_secs) = u32::try_from(target_duration_ms / 1_000) {
            session.target_duration = Some(target_duration_secs);
        }
    }
    let target_duration_ms =
        timing.target_duration_ms.or_else(|| session.target_duration.map(|duration| u64::from(duration) * 1_000));
    let progress =
        manifest_progress_from_highwater(previous_highwater, session.origin_seq_highwater, quality.sequence_relation);
    build_manifest_refresh_timing(timing.last_segment_duration_ms, target_duration_ms, progress)
}

fn commit_normal_manifest(
    session: &mut super::HlsSession,
    manifest: &ParsedOriginManifest,
    _redirect_host: Option<&str>,
    request: &OriginRefreshRequest,
    _resolved_request_url: &str,
    rendered_at_ms: u64,
    sequence_relation: HlsManifestSequenceRelation,
) -> Result<HlsManifestRefreshTiming, HlsManifestCommitError> {
    let previous_highwater = session.origin_seq_highwater;
    let provisioning_handoff = session.pending_handoff_discontinuity_sequence.is_some()
        && session.segments.values().any(is_hls_provisioning_segment);
    let segment_durations = manifest.segments.iter().map(|segment| segment.duration_ms).collect::<Vec<_>>();
    session.initial_prefetch_gap_segments =
        initial_hls_strip_segments_for_durations(&request.strip, &segment_durations);
    session
        .apply_origin_manifest(manifest)
        .map_err(|err| HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::from(err) })?;
    if provisioning_handoff {
        limit_publishable_normal_provisioning_handoff_tail(session, &request.strip, manifest.segments.len());
    }
    session.origin_request_headers = request.headers.clone();
    session.queue_map_fetch_candidates(rendered_at_ms);
    let backpressure = request.segment_worker_pool.classify_backpressure_for_session(session);
    let queue_report = session.queue_manifest_fetch_candidates(rendered_at_ms, backpressure.allows_prefetch());
    request.segment_worker_pool.metrics().record_prefetch_queued(queue_report.prefetch_queued);
    request.segment_worker_pool.metrics().record_prefetch_skipped(queue_report.prefetch_skipped);
    if queue_report.prefetch_queued > 0 {
        debug!(
            "HLS segment queued for prefetch: session={} proxy_session_id={} count={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_queued
        );
    }
    if queue_report.prefetch_skipped > 0 {
        debug!(
            "HLS segment queued for prefetch skipped by backpressure: session={} proxy_session_id={} count={} state={backpressure:?}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_skipped
        );
    }
    match HlsManifestRenderer::render(session, rendered_at_ms) {
        Ok(rendered) => {
            let segment_count = rendered.segment_proxy_seqs.len();
            let render_gap_segments = rendered.render_gap_segments;
            let media_sequence = rendered.first_proxy_seq;
            match session.store_rendered_manifest(rendered) {
                RenderedManifestStoreOutcome::Stored => {
                    request.segment_worker_pool.metrics().record_manifest_rendered();
                    info!(
                        "HLS manifest rendered: session={} proxy_session_id={} media_sequence={} segments={} render_gap_segments={}",
                        safe_session_key(&session.key),
                        safe_proxy_session_id(&session.proxy_session_id),
                        media_sequence,
                        segment_count,
                        render_gap_segments
                    );
                }
                RenderedManifestStoreOutcome::Rejected(
                    RenderedManifestStoreRejectReason::RegressiveMediaSequence {
                        previous_first_proxy_seq,
                        candidate_first_proxy_seq,
                    },
                ) => {
                    request.segment_worker_pool.metrics().record_manifest_render_skipped();
                    debug!(
                        "HLS manifest render rejected: session={} proxy_session_id={} reason=regressive-media-sequence previous={} candidate={}",
                        safe_session_key(&session.key),
                        safe_proxy_session_id(&session.proxy_session_id),
                        previous_first_proxy_seq,
                        candidate_first_proxy_seq
                    );
                }
            }
        }
        Err(err) => {
            request.segment_worker_pool.metrics().record_manifest_render_skipped();
            debug!(
                "HLS manifest render skipped: session={} proxy_session_id={} reason={err:?}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id)
            );
        }
    }

    let last_segment_duration_ms = manifest.segments.last().map(|segment| segment.duration_ms);
    let target_duration_ms = manifest.target_duration.map(|duration| u64::from(duration) * 1_000);
    let progress =
        manifest_progress_from_highwater(previous_highwater, session.origin_seq_highwater, sequence_relation);
    Ok(build_manifest_refresh_timing(last_segment_duration_ms, target_duration_ms, progress))
}

fn limit_publishable_normal_provisioning_handoff_tail(
    session: &mut super::HlsSession,
    strip: &StripConfig,
    origin_segment_count: usize,
) {
    let Some(gap_seq) = session
        .segments
        .iter()
        .filter_map(|(proxy_seq, entry)| is_hls_provisioning_gap_segment(entry).then_some(*proxy_seq))
        .max()
    else {
        return;
    };
    let first_origin_seq = gap_seq.saturating_add(1);
    if !session.segments.contains_key(&first_origin_seq) {
        return;
    }
    if let Some(head_seq) = visible_provisioning_handoff_head_proxy_seq(session) {
        session.publishable_origin_head_proxy_seq = Some(head_seq);
    }
    let initial_origin_segments = configured_handoff_origin_window_segments(strip, origin_segment_count).clamp(1, 3);
    let tail_seq =
        first_origin_seq.saturating_add(u64::try_from(initial_origin_segments.saturating_sub(1)).unwrap_or(0));
    if session.segments.contains_key(&tail_seq) {
        session.publishable_origin_tail_proxy_seq = Some(tail_seq);
    }
}

fn visible_provisioning_handoff_head_proxy_seq(session: &super::HlsSession) -> Option<u64> {
    session
        .last_rendered_manifest
        .as_ref()
        .and_then(|rendered| {
            rendered
                .segment_proxy_seqs
                .iter()
                .copied()
                .find(|proxy_seq| session.segments.get(proxy_seq).is_some_and(is_hls_provisioning_segment))
        })
        .or_else(|| {
            session
                .segments
                .iter()
                .filter_map(|(proxy_seq, entry)| is_hls_provisioning_segment(entry).then_some(*proxy_seq))
                .min()
        })
}

fn configured_handoff_origin_window_segments(strip: &StripConfig, origin_segment_count: usize) -> usize {
    match strip.mode {
        HlsStripMode::Segments => usize::try_from(strip.value).unwrap_or(usize::MAX),
        HlsStripMode::Seconds => 0,
    }
    .saturating_add(3)
    .min(origin_segment_count)
}

fn build_manifest_refresh_timing_base(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> HlsManifestRefreshTiming {
    let source = if last_segment_duration_ms.is_some() {
        HlsManifestTimingSource::LastSegmentDuration
    } else if target_duration_ms.is_some() {
        HlsManifestTimingSource::TargetDuration
    } else {
        HlsManifestTimingSource::Fallback
    };
    let base_interval_ms = compute_origin_refresh_interval_ms(last_segment_duration_ms, target_duration_ms);
    HlsManifestRefreshTiming {
        last_segment_duration_ms,
        target_duration_ms,
        base_interval_ms,
        source,
        progress: HlsManifestProgress::Unchanged,
    }
}

fn build_manifest_refresh_timing(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
    progress: HlsManifestProgress,
) -> HlsManifestRefreshTiming {
    let mut timing = build_manifest_refresh_timing_base(last_segment_duration_ms, target_duration_ms);
    timing.progress = progress;
    timing
}

fn manifest_progress_from_highwater(
    before: Option<u64>,
    after: Option<u64>,
    sequence_relation: HlsManifestSequenceRelation,
) -> HlsManifestProgress {
    match (sequence_relation, before, after) {
        (HlsManifestSequenceRelation::RolloverCandidate, _, _) => HlsManifestProgress::Rollover,
        (_, None, Some(_)) => HlsManifestProgress::Advanced,
        (_, Some(before), Some(after)) if after > before => HlsManifestProgress::Advanced,
        _ => HlsManifestProgress::Unchanged,
    }
}

fn apply_empty_refresh_rampdown_ms(base_interval_ms: u64, empty_refresh_count: u32) -> u64 {
    base_interval_ms.checked_shr(empty_refresh_count.min(16)).unwrap_or(0).max(1_000)
}

fn log_manifest_refresh_timing(
    session: &super::HlsSession,
    timing: HlsManifestRefreshTiming,
    refresh_interval_ms: u64,
) {
    debug!(
        "HLS manifest timing parsed: session={} target_duration={} last_segment_duration={} next_refresh_in_s={} source={} progress={} empty_refreshes={}",
        safe_session_key(&session.key),
        format_optional_millis_as_seconds(timing.target_duration_ms),
        format_optional_millis_as_seconds(timing.last_segment_duration_ms),
        format_millis_as_seconds(refresh_interval_ms),
        timing.source.as_log_value(),
        timing.progress.as_log_value(),
        session.origin_refresh.consecutive_empty_refreshes
    );
}

fn format_optional_millis_as_seconds(value_ms: Option<u64>) -> String {
    value_ms.map_or_else(|| "none".to_string(), format_millis_as_seconds)
}

fn format_millis_as_seconds(value_ms: u64) -> String {
    let seconds = value_ms / 1_000;
    let millis = value_ms % 1_000;
    format!("{seconds}.{millis:03}")
}

fn map_transient_reason(reason: OriginManifestTransientReason) -> TransientPassthroughReason {
    match reason {
        OriginManifestTransientReason::ExtXKey => TransientPassthroughReason::ExtXKey,
        OriginManifestTransientReason::UnsupportedTag { tag } => TransientPassthroughReason::UnsupportedTag { tag },
        OriginManifestTransientReason::ParserUnsupportedFeature { feature } => {
            TransientPassthroughReason::ParserUnsupportedFeature { feature }
        }
    }
}

fn transient_reason_log_fields(reason: &TransientPassthroughReason) -> String {
    match reason {
        TransientPassthroughReason::ExtXKey => "reason=ext_x_key".to_string(),
        TransientPassthroughReason::UnsupportedTag { tag } => format!("reason=unsupported_tag tag={tag}"),
        TransientPassthroughReason::ParserUnsupportedFeature { feature } => {
            format!("reason=parser_unsupported_feature feature={feature}")
        }
    }
}

pub fn compute_origin_refresh_interval_ms(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> u64 {
    last_segment_duration_ms.or(target_duration_ms).map_or(2_000, |duration_ms| duration_ms / 2).max(1_000)
}

pub fn cold_start_retry_after_seconds() -> u64 { COLD_START_RETRY_AFTER_SECONDS }

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        super::manifest_fetch::{
            classify_origin_manifest_status, fetch_hls_origin_manifest_request, hls_manifest_redirect_host,
            manifest_host_switch_failure_threshold_for_strip_segments, origin_highwater_policy_limit,
            refresh_from_live_hls_entrypoint_with_retries, resolved_hls_manifest_request_url_from_input,
            retry_after_delay_ms, retry_hls_origin_manifest_recovery_chain,
            score_hls_manifest_recovery_candidate as score_manifest_recovery_candidate, FetchedOriginManifest,
            HlsManifestCommitAcceptanceMode, HlsManifestCommitError, HlsManifestOriginQualityScore,
            HlsManifestRejectLogReason, HlsManifestSequenceRelation, HlsOriginManifestFetchContext,
            HlsOriginManifestFetchRequest, LiveHlsOriginEntry, ManifestRecoverySelectionLogPhase,
            OriginManifestFetchError, OriginManifestStatusClass, RetryPolicy,
        },
        build_manifest_refresh_timing, commit_fetched_manifest, compute_origin_refresh_interval_ms,
        fetched_effective_manifest_host, format_millis_as_seconds, format_optional_millis_as_seconds,
        manifest_fetch_context, manifest_hard_fetch_error, manifest_progress_from_highwater,
        manifest_temporary_failure_kind, mark_origin_refresh_started, record_pinned_host_recovery_chain_failed,
        request_error_indicates_timeout, transient_reason_log_fields, trigger_origin_refresh_sync,
        HlsManifestAcceptanceDecision, HlsManifestCommitRequirement, HlsManifestProgress, OriginRefreshRequest,
        OriginRefreshState,
    };
    use crate::{
        api::model::{
            maybe_trigger_origin_refresh, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
            HlsBoundAccountAcquireErrorKind, HlsFreshManifestRequiredReason, HlsMapWorkerPool, HlsOriginAccountBinding,
            HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool,
            HlsSession, HlsSessionKey, HlsSessionMode, TimelineMapError, TransientPassthroughReason,
        },
        model::{
            AppConfig, Config, ConfigProvider, HlsManifestRecoveryBurstConfig, HlsSegmentRepairConfig,
            ReverseProxyDisabledHeaderConfig, SourcesConfig, StripConfig,
        },
        utils::content_coding::{ContentCoding, ContentCodingError},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
    use shared::model::{
        ConfigPaths, ConfigProviderDto, HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripMode,
        ProviderUrlSelectionPolicy,
    };
    use std::{
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, RwLock},
    };
    use url::Url;

    async fn retry_test_manifest_recovery_chain(
        request: &OriginRefreshRequest,
        target_url: Url,
        reject_reason: HlsManifestRejectLogReason,
    ) -> Result<super::CommittedOriginManifest, OriginManifestFetchError> {
        let fetch_context = manifest_fetch_context(request);
        retry_hls_origin_manifest_recovery_chain(
            &fetch_context,
            target_url,
            None,
            Some(reject_reason),
            |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(request, fetched, acceptance_mode),
        )
        .await
    }

    fn test_session() -> Arc<RwLock<HlsSession>> {
        Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)))
    }

    fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
        Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }))
    }

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        })
    }

    #[test]
    fn origin_refresh_state_starts_only_when_due_and_not_in_flight() {
        let mut state = OriginRefreshState { next_fetch_allowed_at_ms: 100, ..OriginRefreshState::default() };
        assert!(!state.is_due(99));
        assert!(state.is_due(100));
        state.mark_started(100);
        assert!(!state.is_due(101));
    }

    #[test]
    fn origin_refresh_failure_backoff_ramps_and_success_resets_counter() {
        let mut state = OriginRefreshState::default();

        state.mark_started(1_000);
        state.mark_failure(1_100);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_error_at_ms, Some(1_100));
        assert_eq!(state.next_fetch_allowed_at_ms, 1_100);
        assert!(state.is_due(1_100));

        state.mark_started(1_200);
        state.mark_failure(1_300);
        assert_eq!(state.consecutive_failures, 2);
        assert_eq!(state.next_fetch_allowed_at_ms, 1_800);
        assert!(!state.is_due(1_799));
        assert!(state.is_due(1_800));

        state.mark_started(1_900);
        state.mark_failure(2_000);
        assert_eq!(state.consecutive_failures, 3);
        assert_eq!(state.next_fetch_allowed_at_ms, 3_000);

        state.mark_started(3_100);
        let success_timing = build_manifest_refresh_timing(Some(20_000), None, HlsManifestProgress::Advanced);
        assert_eq!(state.mark_success_with_timing(3_100, 3_200, success_timing), 10_000);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.last_error_at_ms, None);
        assert_eq!(state.next_fetch_allowed_at_ms, 13_100);

        state.mark_started(13_100);
        state.mark_failure(13_200);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.next_fetch_allowed_at_ms, 13_200);
    }

    #[test]
    fn status_classification_matches_hls_retry_policy() {
        for status in [
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
        ] {
            assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::Retryable);
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::PermanentFailure);
        }
    }

    #[test]
    fn manifest_temporary_failure_kind_counts_retryable_status_and_timeout_only() {
        assert_eq!(
            manifest_temporary_failure_kind(&OriginManifestFetchError::RetryableStatus(
                StatusCode::TOO_MANY_REQUESTS,
                None,
            )),
            Some(crate::api::model::HlsManifestTemporaryFailureKind::RetryableStatus {
                status: StatusCode::TOO_MANY_REQUESTS
            })
        );
        assert_eq!(
            manifest_temporary_failure_kind(&OriginManifestFetchError::Timeout),
            Some(crate::api::model::HlsManifestTemporaryFailureKind::Timeout)
        );
        assert_eq!(
            manifest_temporary_failure_kind(&OriginManifestFetchError::Request(
                "Request timed out and no retries left".to_string(),
            )),
            Some(crate::api::model::HlsManifestTemporaryFailureKind::Timeout)
        );
        assert_eq!(
            manifest_temporary_failure_kind(&OriginManifestFetchError::ProviderUnavailable(
                HlsBoundAccountAcquireErrorKind::WaitTimedOut,
            )),
            Some(crate::api::model::HlsManifestTemporaryFailureKind::ProviderAcquire {
                kind: HlsBoundAccountAcquireErrorKind::WaitTimedOut
            })
        );
        assert!(manifest_temporary_failure_kind(&OriginManifestFetchError::Request(
            "Request error: error sending request".to_string(),
        ))
        .is_none());
        assert!(manifest_temporary_failure_kind(&OriginManifestFetchError::ProviderUnavailable(
            HlsBoundAccountAcquireErrorKind::Expired,
        ))
        .is_none());
        assert!(manifest_temporary_failure_kind(&OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND))
            .is_none());
    }

    #[test]
    fn manifest_hard_fetch_error_matches_permanent_and_non_retryable_status_only() {
        assert!(manifest_hard_fetch_error(&OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
        assert!(manifest_hard_fetch_error(&OriginManifestFetchError::NonRetryableStatus(StatusCode::IM_A_TEAPOT)));
        assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::RetryableStatus(
            StatusCode::TOO_MANY_REQUESTS,
            None,
        )));
        assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::Timeout));
        assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::ProviderUnavailable(
            HlsBoundAccountAcquireErrorKind::WaitTimedOut,
        )));
        assert!(manifest_hard_fetch_error(&OriginManifestFetchError::ProviderUnavailable(
            HlsBoundAccountAcquireErrorKind::Expired,
        )));
    }

    #[test]
    fn manifest_content_failures_have_explicit_hard_and_temporary_classification() {
        let errors = vec![
            (
                "invalid content-coding header",
                OriginManifestFetchError::ContentCoding(ContentCodingError::InvalidHeader),
                true,
            ),
            (
                "unsupported content coding",
                OriginManifestFetchError::ContentCoding(ContentCodingError::Unsupported("unknown".to_string())),
                true,
            ),
            (
                "encoded partial content",
                OriginManifestFetchError::ContentCoding(ContentCodingError::EncodedPartialContent),
                true,
            ),
            (
                "content-coding prefix read",
                OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(io::Error::other(
                    "prefix read failed",
                ))),
                false,
            ),
            (
                "gzip decoding",
                OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Gzip },
                false,
            ),
            (
                "deflate decoding",
                OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Deflate },
                false,
            ),
            (
                "brotli decoding",
                OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Brotli },
                false,
            ),
            (
                "zstandard decoding",
                OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Zstd },
                false,
            ),
            (
                "decoded body limit",
                OriginManifestFetchError::DecodedBodyLimitExceeded { limit: 1024 },
                true,
            ),
            (
                "invalid utf-8",
                OriginManifestFetchError::InvalidUtf8 { valid_up_to: 7, error_len: Some(1) },
                true,
            ),
        ];

        for (label, error, expected_hard) in errors {
            assert_eq!(manifest_hard_fetch_error(&error), expected_hard, "unexpected hard classification: {label}");
            assert!(
                manifest_temporary_failure_kind(&error).is_none(),
                "content failure must not affect the temporary host-switch counter: {label}"
            );
        }
    }

    #[test]
    fn request_error_timeout_detection_matches_global_helper_wording() {
        assert!(request_error_indicates_timeout("Request timed out and no retries left"));
        assert!(request_error_indicates_timeout("idle timeout while trying provider://demo"));
        assert!(!request_error_indicates_timeout("Request error: error sending request"));
    }

    #[test]
    fn manifest_reject_log_reason_formats_host_switch_pending() {
        let reason = HlsManifestRejectLogReason::HostSwitchPending { failures: 2, threshold: 3 };
        assert_eq!(reason.status_label(), "host-switch-pending failures=2 threshold=3");
    }

    #[test]
    fn manifest_reject_log_reason_preserves_timeline_mapping_error() {
        assert_eq!(
            HlsManifestRejectLogReason::from(TimelineMapError::UnsupportedSegmentExtension).status_label(),
            "unsupported-segment-extension"
        );
        assert_eq!(
            HlsManifestRejectLogReason::from(TimelineMapError::ProxyMapIdOverflow).status_label(),
            "proxy-map-id-overflow"
        );
    }

    #[test]
    fn manifest_host_switch_failure_threshold_uses_half_of_visible_window_with_cap() {
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(0), 1);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(1), 2);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(2), 2);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(3), 3);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(6), 4);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(7), 5);
        assert_eq!(manifest_host_switch_failure_threshold_for_strip_segments(100), 5);
    }

    #[test]
    fn manifest_highwater_policy_limit_uses_target_duration_fallback() {
        assert_eq!(origin_highwater_policy_limit(60, None), Some(4));
        assert_eq!(origin_highwater_policy_limit(61, None), Some(5));
        assert_eq!(origin_highwater_policy_limit(60, Some(12)), Some(5));
    }

    #[test]
    fn manifest_recovery_burst_levels_map_to_candidate_counts() {
        let cases = [
            (HlsManifestRecoveryBurstLevel::Off, 1, 1, 1),
            (HlsManifestRecoveryBurstLevel::Friendly, 2, 1, 2),
            (HlsManifestRecoveryBurstLevel::Cautious, 3, 1, 3),
            (HlsManifestRecoveryBurstLevel::Balanced, 4, 1, 4),
            (HlsManifestRecoveryBurstLevel::Intense, 5, 1, 5),
            (HlsManifestRecoveryBurstLevel::Aggressive, 6, 1, 6),
            (HlsManifestRecoveryBurstLevel::Beast, 6, 2, 12),
        ];
        for (level, expected_slots, expected_lanes, expected_candidates) in cases {
            let plan = level.plan();
            assert_eq!(plan.slots, expected_slots);
            assert_eq!(plan.lanes_per_slot, expected_lanes);
            assert_eq!(plan.total_candidates(), expected_candidates);
            assert_eq!(level.total_candidates(), expected_candidates);
        }
    }

    #[test]
    fn retry_after_header_is_parsed_as_milliseconds() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(retry_after_delay_ms(&headers), Some(3_000));
    }

    #[test]
    fn resolved_hls_manifest_request_url_uses_provider_index_locally() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
            dns: None,
        }));
        let provider_entry =
            LiveHlsOriginEntry::parse_with_url_failover_provider("provider://demo/live/u/p/1.m3u8", Some(provider))
                .unwrap();

        let resolved = resolved_hls_manifest_request_url_from_input(
            &provider_entry.to_input_source(),
            Some(1),
            provider_entry.url(),
        );
        assert_eq!(resolved.as_str(), "http://provider-b.example/live/u/p/1.m3u8");
        assert!(!resolved.as_str().contains("provider://"));

        let direct_entry = LiveHlsOriginEntry::parse("http://origin.example/live/u/p/1.m3u8").unwrap();
        assert_eq!(
            resolved_hls_manifest_request_url_from_input(&direct_entry.to_input_source(), Some(1), direct_entry.url())
                .as_str(),
            "http://origin.example/live/u/p/1.m3u8"
        );
    }

    #[test]
    fn manifest_timing_log_values_are_seconds_or_none() {
        assert_eq!(format_optional_millis_as_seconds(Some(4_500)), "4.500");
        assert_eq!(format_optional_millis_as_seconds(None), "none");
        assert_eq!(format_millis_as_seconds(2_000), "2.000");
    }

    #[test]
    fn refresh_interval_uses_half_reference_duration_without_upper_clamp() {
        assert_eq!(compute_origin_refresh_interval_ms(Some(8_000), None), 4_000);
        assert_eq!(compute_origin_refresh_interval_ms(Some(500), None), 1_000);
        assert_eq!(compute_origin_refresh_interval_ms(Some(20_000), None), 10_000);
        assert_eq!(compute_origin_refresh_interval_ms(None, None), 2_000);
    }

    #[test]
    fn empty_refresh_rampdown_halves_until_one_second() {
        let mut state = OriginRefreshState::default();
        let timing = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);

        state.mark_started(0);
        assert_eq!(state.mark_success_with_timing(0, 100, timing), 3_000);
        assert_eq!(state.consecutive_empty_refreshes, 1);
        assert_eq!(state.next_fetch_allowed_at_ms, 3_000);

        state.mark_started(3_000);
        assert_eq!(state.mark_success_with_timing(3_000, 3_100, timing), 1_500);
        assert_eq!(state.consecutive_empty_refreshes, 2);
        assert_eq!(state.next_fetch_allowed_at_ms, 4_500);

        state.mark_started(4_500);
        assert_eq!(state.mark_success_with_timing(4_500, 4_600, timing), 1_000);
        assert_eq!(state.consecutive_empty_refreshes, 3);
        assert_eq!(state.next_fetch_allowed_at_ms, 5_500);

        state.mark_started(5_500);
        assert_eq!(state.mark_success_with_timing(5_500, 5_600, timing), 1_000);
        assert_eq!(state.consecutive_empty_refreshes, 4);
        assert_eq!(state.next_fetch_allowed_at_ms, 6_500);
    }

    #[test]
    fn advanced_or_rollover_refresh_resets_empty_refresh_counter() {
        let mut state = OriginRefreshState::default();
        let unchanged = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);
        let advanced = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Advanced);
        let rollover = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Rollover);

        state.mark_started(0);
        assert_eq!(state.mark_success_with_timing(0, 100, unchanged), 3_000);
        state.mark_started(3_000);
        assert_eq!(state.mark_success_with_timing(3_000, 3_100, unchanged), 1_500);
        assert_eq!(state.consecutive_empty_refreshes, 2);

        state.mark_started(4_500);
        assert_eq!(state.mark_success_with_timing(4_500, 4_600, advanced), 6_000);
        assert_eq!(state.consecutive_empty_refreshes, 0);
        assert_eq!(state.next_fetch_allowed_at_ms, 10_500);

        state.mark_started(10_500);
        assert_eq!(state.mark_success_with_timing(10_500, 10_600, unchanged), 3_000);
        assert_eq!(state.consecutive_empty_refreshes, 1);

        state.mark_started(13_500);
        assert_eq!(state.mark_success_with_timing(13_500, 13_600, rollover), 6_000);
        assert_eq!(state.consecutive_empty_refreshes, 0);
        assert_eq!(state.next_fetch_allowed_at_ms, 19_500);
    }

    #[test]
    fn failure_backoff_does_not_increment_empty_refresh_counter() {
        let mut state = OriginRefreshState::default();
        let unchanged = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);

        state.mark_started(0);
        assert_eq!(state.mark_success_with_timing(0, 100, unchanged), 3_000);
        state.mark_started(3_000);
        state.mark_failure(3_100);

        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.consecutive_empty_refreshes, 1);
    }

    #[test]
    fn manifest_progress_tracks_highwater_advancement() {
        assert_eq!(
            manifest_progress_from_highwater(None, Some(10), HlsManifestSequenceRelation::NoPreviousHighwater),
            HlsManifestProgress::Advanced
        );
        assert_eq!(
            manifest_progress_from_highwater(Some(10), Some(11), HlsManifestSequenceRelation::Next),
            HlsManifestProgress::Advanced
        );
        assert_eq!(
            manifest_progress_from_highwater(Some(10), Some(10), HlsManifestSequenceRelation::Same),
            HlsManifestProgress::Unchanged
        );
        assert_eq!(
            manifest_progress_from_highwater(Some(10), Some(9), HlsManifestSequenceRelation::Backward),
            HlsManifestProgress::Unchanged
        );
        assert_eq!(
            manifest_progress_from_highwater(Some(10), Some(1), HlsManifestSequenceRelation::RolloverCandidate),
            HlsManifestProgress::Rollover
        );
        assert_eq!(
            manifest_progress_from_highwater(None, None, HlsManifestSequenceRelation::NoOriginHighwater),
            HlsManifestProgress::Unchanged
        );
    }

    #[test]
    fn recovery_selection_log_phase_distinguishes_single_candidate_from_burst() {
        assert_eq!(
            ManifestRecoverySelectionLogPhase::from_candidate_count(1),
            ManifestRecoverySelectionLogPhase::Recovery
        );
        assert_eq!(
            ManifestRecoverySelectionLogPhase::from_candidate_count(2),
            ManifestRecoverySelectionLogPhase::Burst
        );
        assert_eq!(ManifestRecoverySelectionLogPhase::Recovery.as_log_label(), "recovery");
        assert_eq!(ManifestRecoverySelectionLogPhase::Burst.as_log_label(), "burst");
    }

    #[test]
    fn transient_reason_log_fields_include_unsupported_tag() {
        let reason = TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() };

        assert_eq!(transient_reason_log_fields(&reason), "reason=unsupported_tag tag=#EXT-X-PART");
    }

    #[test]
    fn different_host_candidate_is_not_committed_immediately() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
        let candidate = session.manifest_acceptance.host_switch_candidate.as_ref().expect("candidate is held");
        assert_eq!(candidate.host, "origin.example.com");
        assert_eq!(candidate.highwater, Some(759));
        assert!(session.transient.last_manifest_body.is_none());
    }

    #[test]
    fn fresh_commit_rebases_normal_manifest_against_stale_session_baseline() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_seq_highwater = Some(1_000);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.manifest_acceptance.same_host_retry_chain_failures = 3;
        session.manifest_acceptance.host_switch_candidate = Some(super::super::HlsManifestHostSwitchCandidate {
            host: "stale.example.com".to_string(),
            target_url: "http://stale.example.com/live/user/pass/12345.m3u8".to_string(),
            first_seen_at_ms: 1,
            last_seen_at_ms: 2,
            seen_count: 2,
            highwater: Some(1_000),
            quality_score: 1,
        });
        let mut request = test_origin_refresh_request(test_session());
        request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        };
        let fetched =
            fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg10.ts\n");

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(10));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
        assert_eq!(session.manifest_acceptance.same_host_retry_chain_failures, 0);
        assert!(session.manifest_acceptance.host_switch_candidate.is_none());
    }

    #[test]
    fn host_switch_candidate_commits_only_after_failed_chain_threshold() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
        );

        assert!(matches!(
            commit_fetched_manifest(&mut session, &fetched, &request, 100),
            Err(HlsManifestCommitError::RetryCurrentTarget)
        ));
        assert!(matches!(
            record_pinned_host_recovery_chain_failed(
                &mut session,
                &StripConfig { mode: HlsStripMode::Segments, value: 3 },
                200
            ),
            HlsManifestAcceptanceDecision::Reject { .. }
        ));
        assert!(matches!(
            record_pinned_host_recovery_chain_failed(
                &mut session,
                &StripConfig { mode: HlsStripMode::Segments, value: 3 },
                300
            ),
            HlsManifestAcceptanceDecision::Reject { .. }
        ));
        assert!(matches!(
            record_pinned_host_recovery_chain_failed(
                &mut session,
                &StripConfig { mode: HlsStripMode::Segments, value: 3 },
                400
            ),
            HlsManifestAcceptanceDecision::AcceptHostSwitch { .. }
        ));

        let result = super::commit_fetched_manifest_with_acceptance_mode(
            &mut session,
            &fetched,
            &request,
            500,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        );

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(759));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
        assert_eq!(session.manifest_acceptance.same_host_retry_chain_failures, 0);
        assert!(session.manifest_acceptance.host_switch_candidate.is_none());
    }

    #[tokio::test]
    async fn concurrent_maybe_trigger_origin_refresh_starts_singleflight_once() {
        let session = test_session();
        let entry =
            LiveHlsOriginEntry::parse("http://127.0.0.1:9/live/user/pass/12345.m3u8").expect("valid origin entry");
        let client = reqwest::Client::new();
        let no_redirect_client =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client,
            no_redirect_client,
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 1,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
            retry_policy: RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 },
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        let mut handles = Vec::new();
        for _ in 0..8 {
            let request = request.clone();
            handles.push(tokio::spawn(async move { maybe_trigger_origin_refresh(request).await }));
        }

        let started = futures::future::join_all(handles)
            .await
            .into_iter()
            .filter(|result| result.as_ref().is_ok_and(|started| *started))
            .count();
        assert_eq!(started, 1);
    }

    #[tokio::test]
    async fn fresh_manifest_commit_bypasses_refresh_debounce() {
        let session = test_session();
        session.write().await.origin_refresh.next_fetch_allowed_at_ms = 10_000;
        let mut request = test_origin_refresh_request(Arc::clone(&session));
        request.manifest_commit_requirement =
            HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };

        assert!(mark_origin_refresh_started(&mut request, 1_000).await);
        assert!(session.read().await.origin_refresh.in_flight);
    }

    #[tokio::test]
    async fn committed_manifest_refresh_still_obeys_debounce() {
        let session = test_session();
        session.write().await.origin_refresh.next_fetch_allowed_at_ms = 10_000;
        let mut request = test_origin_refresh_request(Arc::clone(&session));

        assert!(!mark_origin_refresh_started(&mut request, 1_000).await);
        assert!(!session.read().await.origin_refresh.in_flight);
    }

    #[tokio::test]
    async fn successful_manifest_commit_shortens_pending_leases_without_response_path() {
        let session = test_session();
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let hls_proxy = Arc::new(HlsProxyManager::new());
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("user", "client"),
                proxy_session_id.clone(),
                "user".to_string(),
                "session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                90_000,
            ))
            .await;
        let server = spawn_test_origin(Arc::new(|_path| {
            (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
        }))
        .await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let mut request = test_origin_refresh_request(Arc::clone(&session));
        request.hls_proxy = Arc::clone(&hls_proxy);
        request.origin_entry = entry;
        request.now_ms = now_ms;

        assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

        let lease = hls_proxy
            .access_leases()
            .write()
            .await
            .response_snapshot(&lease_id, &proxy_session_id, super::current_time_millis())
            .expect("pending lease should remain available");
        let Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms }) = lease.pending_deadline else {
            panic!("pending lease should be shortened to follow-up");
        };
        assert!(deadline_ms < now_ms.saturating_add(90_000));
        assert!(deadline_ms <= super::current_time_millis().saturating_add(10_000));
    }

    async fn refresh_session_with_origin_body(body: &'static str) -> Arc<RwLock<HlsSession>> {
        let session = test_session();
        let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), body.to_string()))).await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.transient.last_manifest_body.is_some() {
                return session;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        session
    }

    #[tokio::test]
    async fn refresh_stores_headers_after_hls_origin_policy() {
        let session = test_session();
        let server = spawn_test_origin(Arc::new(|_path| {
            (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
        }))
        .await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
        headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("blocked"));
        headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers,
            origin_provider_session_headers: HeaderMap::new(),
            disabled_headers: Some(ReverseProxyDisabledHeaderConfig {
                referer_header: false,
                x_header: true,
                cloudflare_header: true,
                custom_header: Vec::new(),
            }),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.last_rendered_manifest.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let session = session.read().await;
        assert!(!session.origin_request_headers.contains_key(header::AUTHORIZATION));
        assert!(!session.origin_request_headers.contains_key(header::COOKIE));
        assert!(!session.origin_request_headers.contains_key("proxy-authorization"));
        assert!(!session.origin_request_headers.contains_key(header::HOST));
        assert!(!session.origin_request_headers.contains_key("x-blocked"));
        assert!(!session.origin_request_headers.contains_key("cf-ray"));
        assert_eq!(session.origin_request_headers.get(header::ACCEPT_LANGUAGE).expect("language"), "de");
    }

    #[tokio::test]
    async fn accepted_manifest_commit_stores_provider_session_cookie_separately() {
        let session = test_session();
        let server = spawn_test_origin(Arc::new(|_path| {
            (
                200,
                vec![
                    ("Set-Cookie", "sid=abc; Path=/; HttpOnly".to_string()),
                    ("Set-Cookie", "pref=1; SameSite=Lax".to_string()),
                ],
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string(),
            )
        }))
        .await;
        let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("valid origin entry");
        let mut request = test_origin_refresh_request(Arc::clone(&session));
        request.origin_entry = entry;

        assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

        let session = session.read().await;
        assert!(!session.origin_request_headers.contains_key(header::COOKIE));
        assert_eq!(
            session.origin_provider_session_headers.get(header::COOKIE).expect("provider cookie"),
            "sid=abc; pref=1"
        );
    }

    #[test]
    fn origin_account_binding_change_clears_provider_session_headers() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "1"), b"secret", 100);
        session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=abc"));
        session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
            Arc::<str>::from("input"),
            Arc::<str>::from("account-a"),
            &session.proxy_session_id.clone(),
            100,
        )));
        assert!(session.origin_provider_session_headers.is_empty());

        session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=next"));
        session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
            Arc::<str>::from("input"),
            Arc::<str>::from("account-a"),
            &session.proxy_session_id.clone(),
            200,
        )));
        assert!(!session.origin_provider_session_headers.is_empty());

        session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
            Arc::<str>::from("input"),
            Arc::<str>::from("account-b"),
            &session.proxy_session_id.clone(),
            300,
        )));
        assert!(session.origin_provider_session_headers.is_empty());
    }

    #[tokio::test]
    async fn ext_x_key_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        )
        .await;
        let session = session.read().await;
        let body = session.transient.last_manifest_body.as_ref().expect("transient body");

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey }
        ));
        assert!(body.contains("/hls/shared/live/"));
        assert!(body.contains("/r/"));
        assert!(!body.contains("/hls/user/"));
        assert_eq!(session.transient.resources.len(), 2);
        assert_eq!(session.target_duration, Some(12));
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
    }

    #[test]
    fn transient_commit_accepts_plausible_same_redirect_host_rollover_and_resets_highwater() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.mark_authorized_media_access(100);
        let previous_manifest =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
        session.transient.replace_manifest(previous_manifest.clone(), 10);
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
        assert_eq!(session.origin_seq_highwater, Some(0));
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[test]
    fn transient_commit_rejects_same_host_backward_manifest_outside_rollover_window() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.mark_authorized_media_access(100);
        let previous_manifest =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
        session.transient.replace_manifest(previous_manifest.clone(), 10);
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:226\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(matches!(result, Err(HlsManifestCommitError::TimelineRejected { .. })));
        assert_eq!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
        assert_eq!(session.origin_seq_highwater, Some(758));
    }

    #[test]
    fn transient_commit_rebases_expired_session_highwater() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.target_duration = Some(12);
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.mark_authorized_media_access(1_000);
        let previous_manifest =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
        session.transient.replace_manifest(previous_manifest.clone(), 10);
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 40_000);

        assert!(result.is_ok());
        assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
        assert_eq!(session.origin_seq_highwater, Some(900));
    }

    #[test]
    fn transient_commit_accepts_monotonic_media_sequence_and_updates_highwater() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.transient.replace_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string(),
            10,
        );
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:759\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg759.ts\n#EXTINF:4.0,\nseg760.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(760));
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[test]
    fn transient_commit_with_different_redirect_host_is_held_as_candidate() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(758);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        let request = test_origin_refresh_request(test_session());
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
        assert_eq!(session.origin_seq_highwater, Some(758));
        assert!(session.transient.last_manifest_body.is_none());
        let candidate = session.manifest_acceptance.host_switch_candidate.as_ref().expect("candidate");
        assert_eq!(candidate.host, "origin.example.com");
        assert_eq!(candidate.highwater, Some(759));
    }

    #[test]
    fn fresh_commit_rebases_transient_manifest_against_stale_session_baseline() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        session.origin_seq_highwater = Some(1_000);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.manifest_acceptance.same_host_retry_chain_failures = 3;
        session.manifest_acceptance.host_switch_candidate = Some(super::super::HlsManifestHostSwitchCandidate {
            host: "stale.example.com".to_string(),
            target_url: "http://stale.example.com/live/user/pass/12345.m3u8".to_string(),
            first_seen_at_ms: 1,
            last_seen_at_ms: 2,
            seen_count: 2,
            highwater: Some(1_000),
            quality_score: 1,
        });
        let mut request = test_origin_refresh_request(test_session());
        request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        };
        let fetched = fetched_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg10.ts\n",
        );

        let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

        assert!(result.is_ok());
        assert_eq!(session.origin_seq_highwater, Some(10));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
        assert_eq!(session.manifest_acceptance.same_host_retry_chain_failures, 0);
        assert!(session.manifest_acceptance.host_switch_candidate.is_none());
        assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
    }

    #[tokio::test]
    async fn unsupported_tag_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body("#EXTM3U\n#EXT-X-PART:DURATION=1.0,URI=\"part.m4s\"\n").await;
        let session = session.read().await;

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::UnsupportedTag { .. } }
        ));
        assert!(session.transient.last_manifest_body.is_some());
    }

    #[tokio::test]
    async fn parser_unsupported_feature_manifest_commits_transient_rewrite() {
        let session = refresh_session_with_origin_body("#EXTM3U\n#EXT-X-BYTERANGE:10\n#EXTINF:4.0,\nseg.ts\n").await;
        let session = session.read().await;

        assert!(matches!(
            session.mode,
            HlsSessionMode::TransientPassthrough {
                reason: TransientPassthroughReason::ParserUnsupportedFeature { .. }
            }
        ));
        assert!(session.transient.last_manifest_body.is_some());
    }

    struct TestOriginServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        raw_requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    type TestOriginHandler = Arc<dyn Fn(String) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync>;

    impl Drop for TestOriginServer {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_origin(handler: TestOriginHandler) -> TestOriginServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = Arc::clone(&requests);
        let raw_requests = Arc::new(Mutex::new(Vec::new()));
        let raw_requests_for_task = Arc::clone(&raw_requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&requests_for_task);
                let raw_requests = Arc::clone(&raw_requests_for_task);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
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
                    let request = String::from_utf8_lossy(&buf[..used]).into_owned();
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    requests.lock().await.push(path.clone());
                    raw_requests.lock().await.push(request);
                    let (status, headers, body) = handler(path);
                    let reason = match status {
                        200 => "OK",
                        302 => "Found",
                        404 => "Not Found",
                        407 => "Proxy Authentication Required",
                        500 => "Internal Server Error",
                        _ => "Status",
                    };
                    let mut response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        response.push_str(name);
                        response.push_str(": ");
                        response.push_str(&value);
                        response.push_str("\r\n");
                    }
                    response.push_str("\r\n");
                    response.push_str(&body);
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        TestOriginServer { base_url: format!("http://{addr}"), requests, raw_requests, task }
    }

    fn request_header_value<'a>(request: &'a str, expected_name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(expected_name).then_some(value.trim())
        })
    }

    fn no_delay_policy() -> RetryPolicy { RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 } }

    fn test_origin_refresh_request(session: Arc<RwLock<HlsSession>>) -> OriginRefreshRequest {
        let entry = LiveHlsOriginEntry::parse("http://origin.example.com/live/user/pass/12345.m3u8")
            .expect("valid origin entry");
        OriginRefreshRequest {
            app_config: test_app_config(),
            session,
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        }
    }

    fn fetched_manifest(body: &str) -> FetchedOriginManifest {
        FetchedOriginManifest {
            body: body.to_string(),
            final_manifest_url: "http://origin.example.com/live/final/index.m3u8".to_string(),
            resolved_request_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
            redirect_host: Some("origin.example.com".to_string()),
            provider_url_index: None,
            provider_session_headers: HeaderMap::new(),
            status: StatusCode::OK,
            attempts: 1,
        }
    }

    #[test]
    fn provider_failover_mirror_without_redirect_uses_resolved_host_signal() {
        let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
        fetched.redirect_host = None;
        fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
        fetched.provider_url_index = Some(1);

        assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("mirror.example.com"));
    }

    #[test]
    fn provider_failover_with_redirect_uses_redirect_host_as_manifest_host_signal() {
        let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
        fetched.redirect_host = Some("redirect.example.com".to_string());
        fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
        fetched.provider_url_index = Some(1);

        assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("redirect.example.com"));
    }

    #[test]
    fn manifest_redirect_host_is_only_set_for_actual_redirect_host_switch() {
        let resolved = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("resolved url");
        let same_target = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("same url");
        let redirected = Url::parse("http://cdn.example.net/live/play/12345.m3u8").expect("redirect url");

        assert_eq!(hls_manifest_redirect_host(&resolved, &same_target), None);
        assert_eq!(hls_manifest_redirect_host(&resolved, &redirected).as_deref(), Some("cdn.example.net"));
    }

    fn host_from_base_url(base_url: &str) -> String {
        url::Url::parse(base_url).expect("base url").host_str().expect("host").to_string()
    }

    fn manifest_body() -> String { "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string() }

    #[tokio::test]
    async fn shared_initial_manifest_decoder_failure_retries_until_success() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            if hits_for_handler.fetch_add(1, Ordering::SeqCst) < 2 {
                return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
            }
            (200, Vec::new(), manifest_body())
        }))
        .await;
        let origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let context = HlsOriginManifestFetchContext {
            app_config: test_app_config(),
            session: test_session(),
            origin_entry,
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client"),
            use_manual_redirects: false,
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy: no_delay_policy(),
        };

        let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
            .await
            .expect("shared initial manifest should retry decoder failures");

        assert_eq!(fetched.body, manifest_body());
        assert_eq!(fetched.attempts, 3);
        assert_eq!(server.requests.lock().await.len(), 3);
        assert!(server
            .raw_requests
            .lock()
            .await
            .iter()
            .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    }

    #[tokio::test]
    async fn shared_initial_manifest_waits_for_next_attempt_base_delay() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            if hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
            }
            (200, Vec::new(), manifest_body())
        }))
        .await;
        let origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let context = HlsOriginManifestFetchContext {
            app_config: test_app_config(),
            session: test_session(),
            origin_entry,
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client"),
            use_manual_redirects: false,
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy: RetryPolicy { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 0 },
        };
        let started_at = std::time::Instant::now();

        let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
            .await
            .expect("second logical attempt should succeed after its base delay");

        assert_eq!(fetched.attempts, 2);
        assert_eq!(server.requests.lock().await.len(), 2);
        assert!(started_at.elapsed() >= std::time::Duration::from_millis(100));
    }

    #[tokio::test]
    async fn shared_initial_manifest_automatic_cross_origin_redirect_keeps_identity_and_scrubs_credentials() {
        let target = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
        let target_url = format!("{}/final/manifest.m3u8", target.base_url);
        let expected_target_url = target_url.clone();
        let redirect =
            spawn_test_origin(Arc::new(move |_path| (302, vec![("Location", target_url.clone())], String::new())))
                .await;
        let origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", redirect.base_url)).expect("entry url");
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer origin-secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=origin-secret"));
        let context = HlsOriginManifestFetchContext {
            app_config: test_app_config(),
            session: test_session(),
            origin_entry,
            headers,
            client: reqwest::Client::builder().no_proxy().build().expect("client"),
            no_redirect_client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client"),
            use_manual_redirects: false,
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy: no_delay_policy(),
        };

        let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
            .await
            .expect("automatic redirect should return the decoded manifest");

        assert_eq!(fetched.body, manifest_body());
        assert_eq!(fetched.attempts, 1);
        assert_eq!(fetched.final_manifest_url, expected_target_url);
        let redirect_requests = redirect.raw_requests.lock().await;
        let target_requests = target.raw_requests.lock().await;
        assert_eq!(redirect_requests.len(), 1);
        assert_eq!(target_requests.len(), 1);
        assert_eq!(request_header_value(&redirect_requests[0], "accept-encoding"), Some("identity"));
        assert_eq!(request_header_value(&target_requests[0], "accept-encoding"), Some("identity"));
        assert_eq!(request_header_value(&redirect_requests[0], "authorization"), Some("Bearer origin-secret"));
        assert_eq!(request_header_value(&redirect_requests[0], "cookie"), Some("sid=origin-secret"));
        assert!(request_header_value(&target_requests[0], "authorization").is_none());
        assert!(request_header_value(&target_requests[0], "cookie").is_none());
    }

    #[tokio::test]
    async fn shared_initial_manifest_budget_covers_status_decoder_redirect_and_success() {
        let target_hits = Arc::new(AtomicUsize::new(0));
        let target_hits_for_handler = Arc::clone(&target_hits);
        let server = spawn_test_origin(Arc::new(move |path| {
            if path == "/live/user/pass/12345.m3u8" {
                return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
            }
            if path == "/live/play/once/12345" {
                return match target_hits_for_handler.fetch_add(1, Ordering::SeqCst) {
                    0 => (500, Vec::new(), "temporary".to_string()),
                    1 => (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string()),
                    _ => (200, Vec::new(), manifest_body()),
                };
            }
            (404, Vec::new(), String::new())
        }))
        .await;
        let origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let context = HlsOriginManifestFetchContext {
            app_config: test_app_config(),
            session: test_session(),
            origin_entry,
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client"),
            use_manual_redirects: true,
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy: no_delay_policy(),
        };

        let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
            .await
            .expect("third logical attempt should succeed");

        assert_eq!(fetched.body, manifest_body());
        assert_eq!(fetched.attempts, 3);
        assert_eq!(
            *server.requests.lock().await,
            vec![
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
            ]
        );
        assert!(server
            .raw_requests
            .lock()
            .await
            .iter()
            .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    }

    #[tokio::test]
    async fn shared_initial_manifest_decoder_failures_stop_at_attempt_budget() {
        let server = spawn_test_origin(Arc::new(|_path| {
            (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string())
        }))
        .await;
        let origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let retry_policy = no_delay_policy();
        let expected_attempts = retry_policy.attempt_count();
        let context = HlsOriginManifestFetchContext {
            app_config: test_app_config(),
            session: test_session(),
            origin_entry,
            headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client"),
            use_manual_redirects: false,
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy,
        };

        let error = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
            .await
            .expect_err("decoder failures must exhaust the logical attempt budget");

        assert!(matches!(error, OriginManifestFetchError::ContentDecoding { .. }));
        assert_eq!(server.requests.lock().await.len(), expected_attempts);
        assert!(server
            .raw_requests
            .lock()
            .await
            .iter()
            .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    }

    #[tokio::test]
    async fn shared_refresh_metrics_use_successful_manifest_attempt_count() {
        let manifest_hits = Arc::new(AtomicUsize::new(0));
        let manifest_hits_for_handler = Arc::clone(&manifest_hits);
        let server = spawn_test_origin(Arc::new(move |path| {
            if path != "/live/user/pass/12345.m3u8" {
                return (404, Vec::new(), String::new());
            }
            if manifest_hits_for_handler.fetch_add(1, Ordering::SeqCst) < 2 {
                return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
            }
            (200, Vec::new(), manifest_body())
        }))
        .await;
        let session = test_session();
        let mut request = test_origin_refresh_request(session);
        request.origin_entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let metrics = Arc::clone(request.segment_worker_pool.metrics());

        assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.refresh_started, 1);
        assert_eq!(snapshot.refresh_completed, 1);
        assert_eq!(snapshot.refresh_retried, 2);
        assert_eq!(snapshot.refresh_failed, 0);
        let manifest_requests = server
            .raw_requests
            .lock()
            .await
            .iter()
            .filter(|request| request.lines().next().is_some_and(|line| line.contains(" /live/user/pass/12345.m3u8 ")))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(manifest_requests.len(), 3);
        assert!(manifest_requests
            .iter()
            .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    }

    #[tokio::test]
    async fn manifest_retry_starts_at_entrypoint_after_redirect_failure() {
        let redirect_hits = Arc::new(AtomicUsize::new(0));
        let redirect_hits_for_handler = Arc::clone(&redirect_hits);
        let server = spawn_test_origin(Arc::new(move |path| {
            if path == "/live/user/pass/12345.m3u8" {
                return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
            }
            if path == "/live/play/once/12345" {
                let hit = redirect_hits_for_handler.fetch_add(1, Ordering::SeqCst);
                if hit < 2 {
                    return (500, Vec::new(), "fail".to_string());
                }
                return (200, Vec::new(), manifest_body());
            }
            (404, Vec::new(), String::new())
        }))
        .await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
        let no_redirect_client =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");

        let fetched = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &no_redirect_client,
            true,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect("refresh eventually succeeds");

        assert_eq!(fetched.attempts, 3);
        assert_eq!(
            *server.requests.lock().await,
            vec![
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345",
                "/live/user/pass/12345.m3u8",
                "/live/play/once/12345"
            ]
        );
    }

    #[tokio::test]
    async fn retryable_407_retries_until_success() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let server = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < 2 {
                return (407, Vec::new(), "retry".to_string());
            }
            (200, Vec::new(), manifest_body())
        }))
        .await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

        let fetched = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &reqwest::Client::new(),
            false,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect("refresh eventually succeeds");

        assert_eq!(fetched.attempts, 3);
        assert_eq!(server.requests.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn permanent_404_does_not_retry() {
        let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

        let err = refresh_from_live_hls_entrypoint_with_retries(
            &entry,
            &HeaderMap::new(),
            &reqwest::Client::new(),
            &reqwest::Client::new(),
            false,
            2_000,
            &no_delay_policy(),
        )
        .await
        .expect_err("404 is permanent");

        assert!(matches!(err, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn provider_failover_status_does_not_count_as_hls_retry() {
        let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
        let second = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        let session = test_session();
        let initial_session_key = session.read().await.key.stable_value();
        let initial_proxy_session_id = session.read().await.proxy_session_id.clone();
        let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
            "provider://demo/live/user/pass/12345.m3u8",
            Some(Arc::clone(&provider)),
        )
        .expect("provider entry url");
        let segment_worker_pool = Arc::new(HlsSegmentWorkerPool::default());
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::clone(&segment_worker_pool),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.origin_seq_highwater == Some(102) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let first_manifest_requests =
            first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        let second_manifest_requests =
            second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(first_manifest_requests, 1);
        assert_eq!(second_manifest_requests, 1);
        for server in [&first, &second] {
            let manifest_requests = server
                .raw_requests
                .lock()
                .await
                .iter()
                .filter(|request| {
                    request.lines().next().is_some_and(|line| line.contains(" /live/user/pass/12345.m3u8 "))
                })
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(manifest_requests.len(), 1);
            assert_eq!(request_header_value(&manifest_requests[0], "accept-encoding"), Some("identity"));
        }
        let session = session.read().await;
        assert_eq!(session.key.stable_value(), initial_session_key);
        assert_eq!(session.proxy_session_id, initial_proxy_session_id);
        assert!(!session.key.stable_value().contains("provider://"));
        assert!(!session.key.stable_value().contains(first.base_url.as_str()));
        assert!(!session.key.stable_value().contains(second.base_url.as_str()));
        assert_eq!(session.origin_seq_highwater, Some(0));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
        assert!(session.manifest_acceptance.host_switch_candidate.is_none());
        let metrics = segment_worker_pool.metrics().snapshot();
        assert_eq!(metrics.refresh_started, 1);
        assert_eq!(metrics.refresh_completed, 1);
        assert_eq!(metrics.refresh_retried, 0);
        assert_eq!(metrics.refresh_failed, 0);
    }

    #[tokio::test]
    async fn different_host_retries_current_target_before_switching() {
        let candidate_hits = Arc::new(AtomicUsize::new(0));
        let candidate_hits_for_handler = Arc::clone(&candidate_hits);
        let candidate = spawn_test_origin(Arc::new(move |_path| {
            candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n".to_string(),
            )
        }))
        .await;
        let session = test_session();
        {
            let mut session = session.write().await;
            session.origin_seq_highwater = Some(100);
            session.last_effective_manifest_host = Some("previous.example.com".to_string());
        }
        let candidate_entry_url =
            format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
        let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.origin_seq_highwater == Some(102) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let candidate_requests =
            candidate.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(candidate_requests, 6);
        assert_eq!(session.read().await.origin_seq_highwater, Some(102));
        assert_eq!(session.read().await.manifest_acceptance.same_host_retry_chain_failures, 0);
    }

    #[tokio::test]
    async fn manifest_recovery_burst_skips_rejected_candidate() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let origin = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:50\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n50.bin\n".to_string(),
                );
            }
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
            )
        }))
        .await;
        let session = test_session();
        {
            let mut session = session.write().await;
            session.origin_seq_highwater = Some(100);
            session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        }
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
        let mut request = test_origin_refresh_request(Arc::clone(&session));
        request.origin_entry = entry;
        request.manifest_recovery_burst =
            HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

        let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
        let committed = retry_test_manifest_recovery_chain(
            &request,
            target_url,
            HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
        )
        .await
        .expect("burst should commit accepted candidate");

        assert_eq!(committed.fetched.attempts, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(session.read().await.origin_seq_highwater, Some(101));
    }

    #[test]
    fn manifest_recovery_candidate_score_prefers_same_host_next_sequence() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.mark_authorized_media_access(super::current_time_millis());
        let request = test_origin_refresh_request(test_session());
        let fetch_context = manifest_fetch_context(&request);
        let same_host_unchanged =
            fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n");
        let same_host_next =
            fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n");
        let mut other_host_next = same_host_next.clone();
        other_host_next.redirect_host = Some("other.example.com".to_string());

        assert_eq!(
            score_manifest_recovery_candidate(&session, &same_host_unchanged, &fetch_context)
                .expect("score")
                .quality
                .score,
            HlsManifestOriginQualityScore::SameHostUnchanged
        );
        assert_eq!(
            score_manifest_recovery_candidate(&session, &same_host_next, &fetch_context).expect("score").quality.score,
            HlsManifestOriginQualityScore::SameHostNextSequence
        );
        let other_host_score =
            score_manifest_recovery_candidate(&session, &other_host_next, &fetch_context).expect("score").quality;
        assert_eq!(other_host_score.score, HlsManifestOriginQualityScore::OtherHostNextSequence);
        assert!(other_host_score.requires_handoff_discontinuity);
    }

    #[tokio::test]
    async fn manifest_recovery_burst_commits_best_same_host_candidate() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_handler = Arc::clone(&hits);
        let origin = spawn_test_origin(Arc::new(move |_path| {
            let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n".to_string(),
                );
            }
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
            )
        }))
        .await;
        let session = test_session();
        {
            let mut session = session.write().await;
            session.origin_seq_highwater = Some(100);
            session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        }
        let entry =
            LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
        let mut request = test_origin_refresh_request(Arc::clone(&session));
        request.origin_entry = entry;
        request.manifest_recovery_burst =
            HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

        let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
        let committed = retry_test_manifest_recovery_chain(
            &request,
            target_url,
            HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
        )
        .await
        .expect("burst should commit best same-host candidate");

        assert_eq!(committed.fetched.attempts, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(session.read().await.origin_seq_highwater, Some(101));
    }

    #[tokio::test]
    async fn provider_failover_initial_success_commits_without_hls_host_retry_when_unpinned() {
        let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
        let second_hits = Arc::new(AtomicUsize::new(0));
        let second_hits_for_handler = Arc::clone(&second_hits);
        let second = spawn_test_origin(Arc::new(move |_path| {
            let hit = second_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit == 0 {
                return (
                    200,
                    Vec::new(),
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:102\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n102.ts\n".to_string(),
                );
            }
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n".to_string(),
            )
        }))
        .await;
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        let session = test_session();
        {
            session.write().await.origin_seq_highwater = Some(100);
        }
        let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
            "provider://demo/live/user/pass/12345.m3u8",
            Some(Arc::clone(&provider)),
        )
        .expect("provider entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if session.read().await.origin_seq_highwater == Some(102) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let first_manifest_requests =
            first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        let second_manifest_requests =
            second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
        assert_eq!(first_manifest_requests, 1);
        assert_eq!(second_manifest_requests, 1);
        assert_eq!(session.read().await.origin_seq_highwater, Some(102));
    }

    #[tokio::test]
    async fn host_switch_failure_counter_increments_once_per_full_retry_chain() {
        let candidate_hits = Arc::new(AtomicUsize::new(0));
        let candidate_hits_for_handler = Arc::clone(&candidate_hits);
        let candidate = spawn_test_origin(Arc::new(move |_path| {
            candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n900.ts\n".to_string(),
            )
        }))
        .await;
        let session = test_session();
        {
            let mut session = session.write().await;
            session.origin_seq_highwater = Some(100);
            session.last_effective_manifest_host = Some("previous.example.com".to_string());
        }
        let candidate_entry_url =
            format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
        let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
        let request = OriginRefreshRequest {
            app_config: test_app_config(),
            session: Arc::clone(&session),
            origin_entry: entry.clone(),
            headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            client: reqwest::Client::new(),
            no_redirect_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            use_manual_redirects: false,
            segment_cache: Arc::new(HlsSegmentCache::new()),
            hls_proxy: Arc::new(HlsProxyManager::new()),
            segment_repair: test_segment_repair_manager(),
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
            map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
            origin_manifest_timeout_ms: 2_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
            retry_policy: no_delay_policy(),
            reverse_proxy_rewrite_secret: b"secret".to_vec(),
            transient_resource_ttl_ms: 300_000,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            access_lease_id: None,
            disabled_headers: None,
            now_ms: 100,
            origin_io: None,
        };

        assert!(maybe_trigger_origin_refresh(request).await);
        for _ in 0..50 {
            if candidate_hits.load(Ordering::SeqCst) >= 6 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(candidate_hits.load(Ordering::SeqCst), 6);
        assert_eq!(session.read().await.origin_seq_highwater, Some(100));
        assert_eq!(session.read().await.manifest_acceptance.same_host_retry_chain_failures, 1);
    }
}
