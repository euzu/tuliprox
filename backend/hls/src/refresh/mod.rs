//! Refreshing a session's origin manifest.
//!
//! This module owns the flow: decide whether a refresh may start, take the work
//! generation and the provider handle, fetch, commit, and release. Each step of
//! that flow lives in its own private submodule -
//!
//! - [`failure`] - classifying a failed fetch and choosing the retry backoff
//! - [`timing`] - when the next refresh is due, and the timing diagnostics
//! - [`switch_staging`] - staging, verifying and committing a switch to a
//!   different origin manifest, including the critical-handoff path
//! - [`commit`] - turning an accepted manifest into committed session state
//!
//! so that what stays here is the order the steps run in and the state that
//! crosses them.

use super::{
    availability_reevaluation::HlsRecoveryPressureGuardAccess,
    begin_hls_origin_account_io,
    critical_handoff::{critical_handoff_terminal_response_is_current, retry_critical_handoff_state_access},
    deterministic_conflict::HlsDeterministicTimelineConflict,
    finish_hls_origin_account_io,
    hls_ctx::WeakHlsCtx,
    hls_manifest_recovery_log_fields, hls_origin_headers_with_provider_session, hls_origin_log_value,
    hls_recovery_timing_policy,
    manifest_acceptance::{HlsManifestAcceptanceState, HlsManifestAcceptanceTrigger},
    manifest_fetch::{
        deterministic_conflict_receipt_is_current, deterministic_conflict_receipt_matches,
        deterministic_timeline_conflict_from_rejection, fetch_hls_origin_manifest_request,
        log_hls_manifest_initial_selected, retry_hls_origin_manifest_recovery_chain,
        score_hls_manifest_candidate_for_selection_log, FetchedOriginManifest, HlsManifestCommitAcceptanceMode,
        HlsManifestCommitError, HlsManifestFetchSelection, HlsManifestRecoveryUnavailableReason,
        HlsManifestRejectLogReason, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest, LiveHlsOriginEntry,
        OriginManifestFetchError, RetryPolicy,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    prepared_terminal_bundle::{HlsPreparedTerminalBundleKey, HlsPreparedTerminalBundleState},
    safe_proxy_session_id, safe_session_key, sanitized_hls_origin_headers,
    terminal_tail::{snapshot_terminal_media_asset, terminal_media_asset_identity, HLS_TERMINAL_TAIL_SEGMENT_COUNT},
    HlsAccessLeaseId, HlsFreshManifestRequiredReason, HlsLogIdentity, HlsManifestAcceptanceDirective, HlsMapWorkerPool,
    HlsOriginIoContext, HlsOriginWorkClass, HlsProxyManager, HlsRecoveryTriggerDiagnostic, HlsRecoveryTriggerSource,
    HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle, MapFetchContext,
    SegmentFetchContext,
};
use axum::http::HeaderMap;
use log::{debug, error, info, warn};
use reqwest::Client;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, HlsManifestRecoveryBurstConfig, ReverseProxyDisabledHeaderConfig, StripConfig};

mod commit;
mod failure;
mod switch_staging;
mod timing;

pub use self::timing::cold_start_retry_after_seconds;
use self::{
    commit::{
        commit_fetched_manifest, commit_fetched_manifest_with_acceptance_mode,
        ensure_refresh_origin_work_generation_is_current, refresh_origin_work_generation_is_current,
        HlsManifestCommitProgressEvidence,
    },
    failure::{apply_manifest_fetch_failure_signal, manifest_hard_fetch_error, refresh_failure_backoff_schedule},
    switch_staging::{
        critical_handoff_snapshot_is_current, prepare_critical_handoff, remove_uncommitted_staged_switch_files,
        stage_alternative_manifest_switch, switch_staging_error, verify_staged_content_anchor,
        verify_staged_emergency_handoff, HlsStagedSwitchCommit,
    },
    timing::{
        apply_empty_refresh_rampdown_ms, log_manifest_refresh_timing, HlsManifestProgress, HlsManifestRefreshTiming,
    },
};

const HLS_RECOVERY_PRESSURE_START_CAS_ATTEMPTS: usize = 3;

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

    fn mark_invalidated(&mut self, finished_at_ms: u64) {
        self.last_fetch_finished_at_ms = Some(finished_at_ms);
        self.in_flight = false;
    }

    fn next_failure_backoff_ms(&self) -> u64 {
        let [first, second, later] = refresh_failure_backoff_schedule();
        match self.consecutive_failures {
            0 => first,
            1 => second,
            _ => later,
        }
    }
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
    pub fresh_manifest_requirement_generation: Option<u64>,
    pub acceptance_directive: HlsManifestAcceptanceDirective,
    pub access_lease_id: Option<HlsAccessLeaseId>,
    pub now_ms: u64,
    pub origin_io: Option<HlsOriginIoContext>,
    pub post_refresh_runtime: Option<HlsPostRefreshRuntime>,
}

#[derive(Clone)]
pub struct HlsPostRefreshRuntime {
    pub ctx: WeakHlsCtx,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsPostRefreshAvailabilityReason {
    DeterministicTimelineConflict,
    HardManifestFailure,
}

impl HlsPostRefreshAvailabilityReason {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::DeterministicTimelineConflict => "deterministic_timeline_conflict",
            Self::HardManifestFailure => "hard_manifest_failure",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HlsPostRefreshAvailabilityAction {
    None,
    Reevaluate {
        reason: HlsPostRefreshAvailabilityReason,
        origin_progress_generation: u64,
        media_readiness_generation: u64,
    },
}

impl HlsPostRefreshAvailabilityAction {
    pub const fn reason(&self) -> Option<HlsPostRefreshAvailabilityReason> {
        match self {
            Self::None => None,
            Self::Reevaluate { reason, .. } => Some(*reason),
        }
    }
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
            Self::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::ColdStart | HlsFreshManifestRequiredReason::ProvisioningHandoff,
            } => HlsManifestCommitAcceptanceMode::FreshBaseline,
            Self::FreshCommitRequired {
                reason:
                    HlsFreshManifestRequiredReason::ExpiredRevalidation
                    | HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
            } => HlsManifestCommitAcceptanceMode::FreshPinnedRevalidation,
        }
    }

    const fn strict_handoff_trigger(self) -> HlsManifestAcceptanceTrigger {
        match self {
            Self::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ProvisioningHandoff } => {
                HlsManifestAcceptanceTrigger::Critical
            }
            Self::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart } => {
                HlsManifestAcceptanceTrigger::None
            }
            Self::FreshCommitRequired {
                reason:
                    HlsFreshManifestRequiredReason::ExpiredRevalidation
                    | HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
            } => HlsManifestAcceptanceTrigger::RecoveryRequired,
            Self::CommittedManifestAllowed => HlsManifestAcceptanceTrigger::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginRefreshTriggerOutcome {
    Started,
    SessionUnavailable,
    InFlight,
    DebouncedUntil { retry_at_ms: u64 },
    RecoveryPressureSuperseded,
    RecoveryPressureStateContention,
}

/// `maybe_trigger_origin_refresh_with_outcome` reduced to "did it start?".
///
/// Only this module's tests care about the boolean; production call sites read
/// the full outcome, so this stays out of the build and out of the public API.
#[cfg(test)]
async fn maybe_trigger_origin_refresh(request: OriginRefreshRequest) -> bool {
    matches!(maybe_trigger_origin_refresh_with_outcome(request).await, HlsOriginRefreshTriggerOutcome::Started)
}

pub async fn maybe_trigger_origin_refresh_with_outcome(
    mut request: OriginRefreshRequest,
) -> HlsOriginRefreshTriggerOutcome {
    let fetch_started_at_ms = request.now_ms;
    let start_outcome = mark_origin_refresh_started_with_outcome(&mut request, fetch_started_at_ms).await;
    if start_outcome != HlsOriginRefreshTriggerOutcome::Started {
        release_preacquired_origin_provider_handle(&request).await;
        return start_outcome;
    }

    tokio::spawn(async move {
        Box::pin(refresh_and_commit(request, fetch_started_at_ms)).await;
    });
    HlsOriginRefreshTriggerOutcome::Started
}

pub async fn trigger_origin_refresh_sync(mut request: OriginRefreshRequest) -> bool {
    let fetch_started_at_ms = request.now_ms;
    if !mark_origin_refresh_started(&mut request, fetch_started_at_ms).await {
        release_preacquired_origin_provider_handle(&request).await;
        return false;
    }

    let refresh = tokio::spawn(async move {
        Box::pin(refresh_and_commit(request, fetch_started_at_ms)).await;
    });
    if let Err(error) = refresh.await {
        error!("HLS owned origin refresh task failed: cancelled={} panic={}", error.is_cancelled(), error.is_panic());
    }
    true
}

async fn mark_origin_refresh_started(request: &mut OriginRefreshRequest, fetch_started_at_ms: u64) -> bool {
    matches!(
        mark_origin_refresh_started_with_outcome(request, fetch_started_at_ms).await,
        HlsOriginRefreshTriggerOutcome::Started
    )
}

async fn mark_origin_refresh_started_with_outcome(
    request: &mut OriginRefreshRequest,
    fetch_started_at_ms: u64,
) -> HlsOriginRefreshTriggerOutcome {
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    if let Some(guard) = request.acceptance_directive.recovery_pressure_guard.clone() {
        let hls_proxy = Arc::clone(&request.hls_proxy);
        let session_handle = Arc::clone(&request.session);
        for attempt in 0..HLS_RECOVERY_PRESSURE_START_CAS_ATTEMPTS {
            match hls_proxy.with_current_recovery_pressure_session(&session_handle, &guard, |session| {
                mark_origin_refresh_started_in_session(request, session, fetch_started_at_ms, &metrics)
            }) {
                HlsRecoveryPressureGuardAccess::Acquired(outcome) => return outcome,
                HlsRecoveryPressureGuardAccess::Superseded => {
                    return HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded;
                }
                HlsRecoveryPressureGuardAccess::LockBusy => {
                    if attempt.saturating_add(1) < HLS_RECOVERY_PRESSURE_START_CAS_ATTEMPTS {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        return HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention;
    }
    let session_handle = Arc::clone(&request.session);
    let mut session = session_handle.write().await;
    mark_origin_refresh_started_in_session(request, &mut session, fetch_started_at_ms, &metrics)
}

fn mark_origin_refresh_started_in_session(
    request: &mut OriginRefreshRequest,
    session: &mut super::HlsSession,
    fetch_started_at_ms: u64,
    metrics: &super::HlsCacheMetrics,
) -> HlsOriginRefreshTriggerOutcome {
    if session.is_gc_marked_for_removal() {
        return HlsOriginRefreshTriggerOutcome::SessionUnavailable;
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
            "HLS origin manifest refresh skipped: session={} proxy_session={} reason=in_flight last_fetch_started_at_ms={} in_flight_for_ms={} now_ms={fetch_started_at_ms}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            last_fetch_started_at_ms,
            in_flight_for_ms
        );
        return HlsOriginRefreshTriggerOutcome::InFlight;
    }
    if fetch_started_at_ms < session.origin_refresh.next_fetch_allowed_at_ms
        && request.manifest_commit_requirement.fresh_reason().is_none()
    {
        metrics.record_refresh_skipped();
        let wait_ms = session.origin_refresh.next_fetch_allowed_at_ms.saturating_sub(fetch_started_at_ms);
        debug!(
            "HLS origin manifest refresh skipped: session={} proxy_session={} reason=debounce next_fetch_allowed_at_ms={} now_ms={fetch_started_at_ms} wait_ms={wait_ms}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            session.origin_refresh.next_fetch_allowed_at_ms
        );
        return HlsOriginRefreshTriggerOutcome::DebouncedUntil {
            retry_at_ms: session.origin_refresh.next_fetch_allowed_at_ms,
        };
    }
    if let Some(origin_io) = request.origin_io.as_mut() {
        origin_io.started_generation = Some(session.start_origin_work());
    }
    request.fresh_manifest_requirement_generation = request
        .manifest_commit_requirement
        .fresh_reason()
        .and_then(|reason| session.fresh_manifest_commit_requirement_generation(reason));
    session.origin_refresh.mark_started(fetch_started_at_ms);
    metrics.record_refresh_started();
    info!(
        "HLS origin manifest refresh started: session={} proxy_session={}",
        safe_session_key(&session.key),
        safe_proxy_session_id(&session.proxy_session_id)
    );
    HlsOriginRefreshTriggerOutcome::Started
}

#[allow(clippy::too_many_lines)]
async fn refresh_and_commit(mut request: OriginRefreshRequest, fetch_started_at_ms: u64) {
    request.headers = sanitized_hls_origin_headers(&request.headers, request.disabled_headers.as_ref());
    let mut provider_acquire_failure = None;
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
                    release_preacquired_origin_provider_handle(&request).await;
                    provider_acquire_failure = Some(kind);
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    let result = if let Some(kind) = provider_acquire_failure {
        Err(provider_preflight_manifest_error(kind))
    } else {
        Box::pin(fetch_and_commit_manifest_with_policy(&mut request)).await
    };
    if result.is_ok() {
        if let Some(lease_id) = request.access_lease_id.as_ref() {
            request.hls_proxy.startup_observability().record_origin_manifest_commit(lease_id, current_time_millis());
        }
    }
    let early_prepared_terminal_target_duration_ms =
        result.as_ref().ok().and_then(|committed| committed.progress_evidence.refresh_timing().target_duration_ms);
    if let Some(target_duration_ms) = early_prepared_terminal_target_duration_ms {
        start_refresh_terminal_bundle_preparation(&request, target_duration_ms);
    }
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
        pending_manifest_follow_up,
        prepared_terminal_target_duration_ms,
        post_refresh_action,
        committed_evidence_session,
    ) = {
        let mut session = request.session.write().await;
        match result {
            Ok(CommittedOriginManifest { fetched, progress_evidence, wake_segment_scheduler, wake_map_scheduler }) => {
                let pending_manifest_follow_up = Some((session.proxy_session_id.clone(), session.target_duration));
                let prepared_terminal_target_duration_ms = if early_prepared_terminal_target_duration_ms.is_none() {
                    session
                        .origin_control
                        .target_duration_snapshot_ms
                        .or_else(|| session.target_duration.map(|seconds| u64::from(seconds).saturating_mul(1_000)))
                } else {
                    None
                };
                let (refresh_timing, applied_refresh_interval_ms) = record_committed_manifest_success(
                    &mut session,
                    progress_evidence,
                    fetch_started_at_ms,
                    fetch_finished_at_ms,
                );
                log_manifest_refresh_timing(&session, refresh_timing, applied_refresh_interval_ms);
                metrics.record_refresh_completed();
                for _ in 1..fetched.attempts {
                    metrics.record_refresh_retried();
                }
                let completion = HlsManifestRefreshCompletionDiagnostic::from_fetched(&fetched);
                info!(
                    "HLS origin manifest refresh completed: session={} proxy_session={} final_url={} redirect_host={} status={} recovery_attempts={} candidate_requests={} selection={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    hls_origin_log_value(&fetched.final_manifest_url),
                    fetched
                        .redirect_host
                        .as_deref()
                        .map_or_else(|| "none".to_string(), hls_origin_log_value),
                    fetched.status.as_u16(),
                    completion.recovery_attempts,
                    completion.candidate_requests,
                    completion.selection.as_log_value()
                );
                if origin_work_state.generation_valid {
                    (
                        wake_segment_scheduler,
                        wake_map_scheduler,
                        pending_manifest_follow_up,
                        prepared_terminal_target_duration_ms,
                        HlsPostRefreshAvailabilityAction::None,
                        Some(session.proxy_session_id.clone()),
                    )
                } else {
                    (
                        false,
                        false,
                        pending_manifest_follow_up,
                        prepared_terminal_target_duration_ms,
                        HlsPostRefreshAvailabilityAction::None,
                        Some(session.proxy_session_id.clone()),
                    )
                }
            }
            Err(OriginManifestFetchError::RecoveryUnavailable {
                reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            }) => {
                record_superseded_manifest_recovery_completion(&mut session, metrics.as_ref(), fetch_finished_at_ms);
                (false, false, None, None, HlsPostRefreshAvailabilityAction::None, None)
            }
            Err(err)
                if !origin_work_state.generation_valid
                    || !refresh_origin_work_generation_is_current(&session, &request) =>
            {
                session.origin_refresh.mark_invalidated(fetch_finished_at_ms);
                metrics.record_refresh_skipped();
                debug!(
                    "HLS origin manifest refresh completion discarded: session={} proxy_session={} reason=origin-work-generation-invalidated error={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    err.log_label()
                );
                (false, false, None, None, HlsPostRefreshAvailabilityAction::None, None)
            }
            Err(err) => {
                session.origin_refresh.mark_failure(fetch_finished_at_ms);
                metrics.record_refresh_failed();
                let post_refresh_action = apply_manifest_fetch_failure_signal(&mut session, &err, fetch_finished_at_ms);
                warn!(
                    "HLS origin manifest refresh completed: session={} proxy_session={} result=failed error={}",
                    safe_session_key(&session.key),
                    safe_proxy_session_id(&session.proxy_session_id),
                    err.log_label()
                );
                (false, false, None, None, post_refresh_action, None)
            }
        }
    };
    if let Some(proxy_session_id) = committed_evidence_session.as_ref() {
        request.hls_proxy.notify_session_evidence_changed(proxy_session_id);
    }
    if post_refresh_action != HlsPostRefreshAvailabilityAction::None {
        schedule_post_refresh_availability_reevaluation(&request, post_refresh_action).await;
    }
    if let Some(target_duration_ms) = prepared_terminal_target_duration_ms {
        start_refresh_terminal_bundle_preparation(&request, target_duration_ms);
    }
    if let Some((proxy_session_id, target_duration)) = pending_manifest_follow_up {
        let shortened = request
            .hls_proxy
            .mark_pending_manifest_follow_up_for_session(&proxy_session_id, fetch_finished_at_ms, target_duration)
            .await;
        if shortened > 0 {
            debug!(
                "HLS pending manifest leases shortened after manifest commit: proxy_session={} leases={shortened}",
                safe_proxy_session_id(&proxy_session_id)
            );
        }
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

fn record_superseded_manifest_recovery_completion(
    session: &mut super::HlsSession,
    metrics: &super::HlsCacheMetrics,
    finished_at_ms: u64,
) {
    session.origin_refresh.mark_invalidated(finished_at_ms);
    metrics.record_refresh_skipped();
    debug!(
        "HLS origin manifest recovery discarded: session={} proxy_session={} reason=binding-superseded",
        safe_session_key(&session.key),
        safe_proxy_session_id(&session.proxy_session_id)
    );
}

const fn provider_preflight_manifest_error(kind: super::HlsBoundAccountAcquireErrorKind) -> OriginManifestFetchError {
    OriginManifestFetchError::ProviderUnavailable(kind)
}

async fn schedule_post_refresh_availability_reevaluation(
    request: &OriginRefreshRequest,
    action: HlsPostRefreshAvailabilityAction,
) {
    let HlsPostRefreshAvailabilityAction::Reevaluate { reason, .. } = action else {
        return;
    };
    let identity = {
        let session = request.session.read().await;
        (safe_session_key(&session.key), safe_proxy_session_id(&session.proxy_session_id))
    };
    let ctx = request.post_refresh_runtime.as_ref().and_then(|runtime| runtime.ctx.upgrade());
    let registration = if let Some(ctx) = ctx.as_ref() {
        super::availability::register_post_refresh_availability_reevaluation(
            ctx.clone(),
            Arc::clone(&request.session),
            request.clone(),
            action.clone(),
        )
        .await
    } else {
        super::HlsAvailabilityReevaluationRegistration::RuntimeUnavailable
    };
    if matches!(
        registration,
        super::HlsAvailabilityReevaluationRegistration::CapacityExceeded
            | super::HlsAvailabilityReevaluationRegistration::RuntimeUnavailable
    ) {
        if let Some(ctx) = ctx {
            let fallback = super::post_refresh_availability::commit_post_refresh_terminal_fallback(
                ctx,
                Arc::clone(&request.session),
                action,
                registration,
            )
            .await;
            error!(
                "HLS post-refresh availability owner fallback: session={} proxy_session={} reason={} outcome={}",
                identity.0,
                identity.1,
                reason.as_log_value(),
                fallback.as_label()
            );
        }
    }
    let registration_label = match registration {
        super::HlsAvailabilityReevaluationRegistration::Scheduled => "scheduled",
        super::HlsAvailabilityReevaluationRegistration::AlreadyOwned => "already_owned",
        super::HlsAvailabilityReevaluationRegistration::Superseded => "superseded",
        super::HlsAvailabilityReevaluationRegistration::CapacityExceeded => "capacity_exceeded",
        super::HlsAvailabilityReevaluationRegistration::RuntimeUnavailable => "runtime_unavailable",
    };
    info!(
        "HLS post-refresh availability reevaluation: session={} proxy_session={} reason={} registration={registration_label}",
        identity.0,
        identity.1,
        reason.as_log_value()
    );
}

fn record_committed_manifest_success(
    session: &mut super::HlsSession,
    progress_evidence: HlsManifestCommitProgressEvidence,
    fetch_started_at_ms: u64,
    fetch_finished_at_ms: u64,
) -> (HlsManifestRefreshTiming, u64) {
    let refresh_timing = progress_evidence.success_bookkeeping_timing();
    let applied_refresh_interval_ms =
        session.origin_refresh.mark_success_with_timing(fetch_started_at_ms, fetch_finished_at_ms, refresh_timing);
    if progress_evidence.is_media_progress() {
        // The recovery runner completes its exact episode generation after the
        // atomic manifest commit. Preserve any replacement episode opened before
        // this bookkeeping phase instead of consuming unrelated evidence.
        if let Some(episode) = session
            .origin_control
            .acceptance_episode
            .take_if(|episode| episode.state == HlsManifestAcceptanceState::Completed)
        {
            session.origin_control.recovery_samples.record(fetch_finished_at_ms.saturating_sub(episode.started_at_ms));
        }
    } else {
        session.origin_control.record_origin_response(fetch_finished_at_ms);
    }
    (refresh_timing, applied_refresh_interval_ms)
}

fn start_refresh_terminal_bundle_preparation(request: &OriginRefreshRequest, target_duration_ms: u64) {
    let terminal_response = request.app_config.custom_stream_response.load_full();
    let Some(buffer) = terminal_response.as_ref().and_then(|response| response.channel_unavailable.as_ref()) else {
        return;
    };
    let Some(asset_identity) = terminal_media_asset_identity(buffer) else {
        return;
    };
    let key = HlsPreparedTerminalBundleKey {
        asset: asset_identity,
        target_duration_ms,
        segment_count: HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    };
    if request.hls_proxy.prepared_terminal_bundle_state(key).is_some() {
        return;
    }
    let Ok(asset) = snapshot_terminal_media_asset(buffer) else {
        return;
    };
    match request.hls_proxy.start_prepared_terminal_bundle(asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT) {
        HlsPreparedTerminalBundleState::Ready { .. } | HlsPreparedTerminalBundleState::Preparing { .. } => {}
        HlsPreparedTerminalBundleState::Failed { reason, .. } => {
            debug!("HLS terminal bundle preparation unavailable: state=failed reason={reason:?}");
        }
        HlsPreparedTerminalBundleState::Incompatible { reason, .. } => {
            debug!("HLS terminal bundle preparation unavailable: state=incompatible reason={reason:?}");
        }
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

async fn release_preacquired_origin_provider_handle(request: &OriginRefreshRequest) {
    let Some(origin_io) = request.origin_io.as_ref() else {
        return;
    };
    let Some(handle) = origin_io.take_preacquired_provider_handle().await else {
        return;
    };
    let binding = request.session.read().await.origin_account_binding.clone();
    if let Some(binding) = binding {
        origin_io.ctx.connection_manager.release_provider_handle(Some(handle)).await;
        debug!(
            "HLS provider handle released after manifest refresh: provider={} reason=refresh-not-started",
            sanitize_sensitive_info(binding.account_name.as_ref())
        );
    } else {
        origin_io.ctx.connection_manager.release_provider_handle(Some(handle)).await;
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
    progress_evidence: HlsManifestCommitProgressEvidence,
    wake_segment_scheduler: bool,
    wake_map_scheduler: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct HlsManifestRefreshCompletionDiagnostic {
    recovery_attempts: usize,
    candidate_requests: usize,
    selection: HlsManifestFetchSelection,
}

impl HlsManifestRefreshCompletionDiagnostic {
    const fn from_fetched(fetched: &FetchedOriginManifest) -> Self {
        let recovery_attempts = match fetched.selection {
            HlsManifestFetchSelection::Initial => 0,
            HlsManifestFetchSelection::Recovery | HlsManifestFetchSelection::Burst => fetched.attempts,
        };
        Self { recovery_attempts, candidate_requests: fetched.candidate_requests, selection: fetched.selection }
    }
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
        recovery_timing_policy: hls_recovery_timing_policy(&request.hls_proxy, request.origin_manifest_timeout_ms),
        acceptance_timing_seed: request.acceptance_directive.timing_seed,
    }
}

async fn fetch_and_commit_manifest_with_policy(
    request: &mut OriginRefreshRequest,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let (established_recovery_binding, prefetch_trigger) = {
        let session = request.session.read().await;
        if !refresh_origin_work_generation_is_current(&session, request) {
            return Err(OriginManifestFetchError::RetryExhausted);
        }
        let trigger = if request.manifest_commit_requirement == HlsManifestCommitRequirement::CommittedManifestAllowed
            && deterministic_conflict_receipt_is_current(&session)
        {
            // An unchanged deterministic receipt permits one ordinary sample so
            // fresh origin evidence can invalidate it. Availability pressure must
            // not turn that sample back into an identical configured full burst.
            HlsManifestAcceptanceTrigger::None
        } else {
            manifest_recovery_trigger(request)
        };
        (session.established_manifest_recovery_binding(), trigger)
    };
    let fetch_context = manifest_fetch_context(request);
    let mut recovery_suppression_logged = false;
    if prefetch_trigger.starts_episode() {
        if let Some(binding) = established_recovery_binding.clone() {
            return Box::pin(recover_manifest_for_request(
                &fetch_context,
                request,
                HlsManifestRecoveryPath {
                    binding,
                    reject_reason: None,
                    deterministic_conflict: None,
                    trigger: prefetch_trigger,
                    diagnostic: recovery_prefetch_diagnostic(request),
                },
            ))
            .await;
        }
        log_manifest_recovery_burst_suppressed(request, prefetch_trigger).await;
        recovery_suppression_logged = true;
    }
    let fetched =
        match fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&fetch_context))
            .await
        {
            Ok(fetched) => fetched,
            Err(initial_error) if manifest_hard_fetch_error(&initial_error) => {
                if let Some(binding) = established_recovery_binding.clone() {
                    return Box::pin(recover_manifest_for_request(
                        &fetch_context,
                        request,
                        HlsManifestRecoveryPath {
                            binding,
                            reject_reason: None,
                            deterministic_conflict: None,
                            trigger: HlsManifestAcceptanceTrigger::Observe,
                            diagnostic: HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::HardFetchFailure),
                        },
                    ))
                    .await;
                }
                if !recovery_suppression_logged {
                    log_manifest_recovery_burst_suppressed(request, HlsManifestAcceptanceTrigger::Observe).await;
                }
                return Err(initial_error);
            }
            Err(initial_error) => return Err(initial_error),
        };
    Box::pin(commit_initial_fetched_manifest(
        request,
        &fetch_context,
        fetched,
        established_recovery_binding,
        recovery_suppression_logged,
    ))
    .await
}

async fn log_manifest_recovery_burst_suppressed(request: &OriginRefreshRequest, trigger: HlsManifestAcceptanceTrigger) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let identity = {
        let session = request.session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    debug!(
        "HLS manifest recovery burst suppressed: session={} proxy_session={} reason=session-not-established trigger={}",
        identity.session(),
        identity.proxy_session(),
        trigger.as_log_value()
    );
}

fn manifest_recovery_trigger(request: &OriginRefreshRequest) -> HlsManifestAcceptanceTrigger {
    match (request.acceptance_directive.trigger, request.manifest_commit_requirement.strict_handoff_trigger()) {
        (HlsManifestAcceptanceTrigger::Critical, _) | (_, HlsManifestAcceptanceTrigger::Critical) => {
            HlsManifestAcceptanceTrigger::Critical
        }
        (HlsManifestAcceptanceTrigger::RecoveryRequired, _) | (_, HlsManifestAcceptanceTrigger::RecoveryRequired) => {
            HlsManifestAcceptanceTrigger::RecoveryRequired
        }
        (HlsManifestAcceptanceTrigger::Observe, _) | (_, HlsManifestAcceptanceTrigger::Observe) => {
            HlsManifestAcceptanceTrigger::Observe
        }
        (HlsManifestAcceptanceTrigger::None, HlsManifestAcceptanceTrigger::None) => HlsManifestAcceptanceTrigger::None,
    }
}

struct HlsManifestRecoveryPath {
    binding: HlsManifestOriginBinding,
    reject_reason: Option<HlsManifestRejectLogReason>,
    deterministic_conflict: Option<HlsDeterministicTimelineConflict>,
    trigger: HlsManifestAcceptanceTrigger,
    diagnostic: HlsRecoveryTriggerDiagnostic,
}

fn recovery_prefetch_diagnostic(request: &OriginRefreshRequest) -> HlsRecoveryTriggerDiagnostic {
    match request.manifest_commit_requirement {
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ProvisioningHandoff,
        } => HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::ProvisioningHandoff),
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
        } => HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::HardFetchFailure),
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation | HlsFreshManifestRequiredReason::ColdStart,
        }
        | HlsManifestCommitRequirement::CommittedManifestAllowed => request
            .acceptance_directive
            .recovery_diagnostic
            .clone()
            .unwrap_or_else(|| HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::Other)),
    }
}

async fn recover_manifest_for_request(
    fetch_context: &HlsOriginManifestFetchContext,
    request: &OriginRefreshRequest,
    path: HlsManifestRecoveryPath,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let binding_is_current = {
        let session = request.session.read().await;
        session.established_manifest_recovery_binding().as_ref() == Some(&path.binding)
    };
    if !binding_is_current {
        return Err(OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        });
    }
    if log::log_enabled!(log::Level::Debug) {
        let identity = {
            let session = request.session.read().await;
            HlsLogIdentity::from_session(&session)
        };
        if let Some(fields) = hls_manifest_recovery_log_fields(&identity, path.trigger, &path.diagnostic) {
            debug!("HLS manifest recovery started: {fields}");
        }
    }
    Box::pin(retry_hls_origin_manifest_recovery_chain(
        fetch_context,
        path.binding,
        path.reject_reason,
        path.deterministic_conflict,
        path.trigger,
        request.manifest_commit_requirement.acceptance_mode(),
        |fetched, acceptance_mode| Box::pin(commit_manifest_recovery_candidate(request, fetched, acceptance_mode)),
    ))
    .await
}

async fn commit_initial_fetched_manifest(
    request: &OriginRefreshRequest,
    fetch_context: &HlsOriginManifestFetchContext,
    fetched: FetchedOriginManifest,
    established_recovery_binding: Option<HlsManifestOriginBinding>,
    recovery_suppression_logged: bool,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let acceptance_mode = request.manifest_commit_requirement.acceptance_mode();
    let selected_report =
        score_hls_manifest_candidate_for_selection_log(fetch_context, &fetched, acceptance_mode).await;
    let commit_result = {
        let mut session = request.session.write().await;
        commit_fetched_manifest(&mut session, &fetched, request, current_time_millis())
    };
    cancel_superseded_terminal_work_after_media_progress(request, &commit_result).await;
    if commit_result.is_err() {
        let session = request.session.read().await;
        if !refresh_origin_work_generation_is_current(&session, request) {
            return Err(OriginManifestFetchError::RetryExhausted);
        }
    }

    match commit_result {
        Ok((progress_evidence, wake_segment_scheduler, wake_map_scheduler)) => {
            if let Some(report) = selected_report.as_ref() {
                log_hls_manifest_initial_selected(fetch_context, report).await;
            }
            Ok(CommittedOriginManifest { fetched, progress_evidence, wake_segment_scheduler, wake_map_scheduler })
        }
        Err(HlsManifestCommitError::RetryCurrentTarget) => {
            let diagnostic = {
                let session = request.session.read().await;
                HlsRecoveryTriggerDiagnostic::other_redirect_host(
                    session.origin_control.pinned_host.clone().or_else(|| session.last_effective_manifest_host.clone()),
                    selected_report.as_ref().and_then(|report| report.quality.effective_host.clone()),
                )
            };
            let Some(binding) = established_recovery_binding else {
                if !recovery_suppression_logged {
                    log_manifest_recovery_burst_suppressed(request, HlsManifestAcceptanceTrigger::Observe).await;
                }
                return Err(OriginManifestFetchError::RecoveryUnavailable {
                    reason: HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse,
                });
            };
            Box::pin(recover_manifest_for_request(
                fetch_context,
                request,
                HlsManifestRecoveryPath {
                    binding,
                    reject_reason: None,
                    deterministic_conflict: None,
                    trigger: HlsManifestAcceptanceTrigger::Observe,
                    diagnostic,
                },
            ))
            .await
        }
        Err(HlsManifestCommitError::TimelineRejected { reason }) => {
            let deterministic_conflict = deterministic_timeline_conflict_from_rejection(&fetched, &reason);
            if let Some(conflict) = deterministic_conflict.as_ref() {
                let session = request.session.read().await;
                if deterministic_conflict_receipt_matches(&session, conflict) {
                    return Err(OriginManifestFetchError::DeterministicTimelineConflict(Box::new(conflict.clone())));
                }
            }
            let Some(binding) = established_recovery_binding else {
                if !recovery_suppression_logged {
                    log_manifest_recovery_burst_suppressed(request, HlsManifestAcceptanceTrigger::Observe).await;
                }
                return Err(OriginManifestFetchError::RecoveryUnavailable {
                    reason: HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse,
                });
            };
            Box::pin(recover_manifest_for_request(
                fetch_context,
                request,
                HlsManifestRecoveryPath {
                    binding,
                    reject_reason: Some(reason),
                    deterministic_conflict,
                    trigger: HlsManifestAcceptanceTrigger::Observe,
                    diagnostic: HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::TimelineRejection),
                },
            ))
            .await
        }
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation)) => {
            Err(OriginManifestFetchError::LocalRepresentationLimit(violation))
        }
        Err(HlsManifestCommitError::MalformedTransientRepresentation) => {
            Err(OriginManifestFetchError::MalformedTransientRepresentation)
        }
        Err(HlsManifestCommitError::CommitGenerationExhausted) => {
            Err(OriginManifestFetchError::CommitGenerationExhausted)
        }
    }
}

/// Retries only the final atomic state access. The already staged media and its
/// cleanup guards remain owned by the surrounding singleflight, and every
/// acquired attempt repeats the complete frozen-state validation before commit.
async fn commit_verified_critical_handoff(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    staged: &HlsStagedSwitchCommit,
) -> Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    retry_critical_handoff_state_access(staged.generation.acceptance_generation, || {
        request.hls_proxy.with_critical_handoff_state(&request.session, |leases, session| {
            // Both locks are held: revalidation and commit now share one fresh lease-time view.
            let commit_now_ms = current_time_millis();
            let Some(preparation) = staged.critical_handoff.as_ref() else {
                return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
            };
            if preparation.generation != staged.generation
                || preparation.base_evidence.track_base() != Some(&preparation.snapshot.base)
                || !critical_handoff_terminal_response_is_current(
                    &request.app_config,
                    preparation.terminal_response.as_ref(),
                )
                || !critical_handoff_snapshot_is_current(
                    leases,
                    session,
                    &staged.generation,
                    &preparation.snapshot,
                    commit_now_ms,
                )
            {
                return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
            }
            commit_fetched_manifest_with_acceptance_mode(
                session,
                fetched,
                request,
                commit_now_ms,
                acceptance_mode,
                Some(staged),
            )
        })
    })
    .await
}

async fn commit_manifest_recovery_candidate(
    request: &OriginRefreshRequest,
    fetched: FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<CommittedOriginManifest, HlsManifestCommitError> {
    {
        let session = request.session.read().await;
        ensure_refresh_origin_work_generation_is_current(&session, request)?;
    }
    let critical_handoff =
        if matches!(acceptance_mode, HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate) {
            Some(Box::pin(prepare_critical_handoff(request)).await?)
        } else {
            None
        };
    let mut staged_switch = if matches!(
        acceptance_mode,
        HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
            | HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate
            | HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate
    ) {
        Some(Box::pin(stage_alternative_manifest_switch(request, &fetched)).await?)
    } else {
        None
    };
    if let (Some(staged), Some(preparation)) = (staged_switch.as_mut(), critical_handoff) {
        staged.critical_handoff = Some(preparation);
    }
    if let Some(staged) = staged_switch.as_mut() {
        let verification = match acceptance_mode {
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate => {
                Box::pin(verify_staged_content_anchor(request, &fetched, &*staged)).await
            }
            HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate => {
                Box::pin(verify_staged_emergency_handoff(request, &fetched, &*staged)).await
            }
            HlsManifestCommitAcceptanceMode::StrictPinnedHost
            | HlsManifestCommitAcceptanceMode::FreshPinnedRevalidation
            | HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
            | HlsManifestCommitAcceptanceMode::FreshBaseline => Ok(()),
        };
        if let Err(error) = verification {
            if let Some(staged) = staged_switch.as_mut() {
                remove_uncommitted_staged_switch_files(request, staged).await;
            }
            return Err(error);
        }
    }
    let has_critical_handoff = staged_switch.as_ref().is_some_and(|staged| staged.critical_handoff.is_some());
    let commit_result = if has_critical_handoff {
        let staged = staged_switch
            .as_ref()
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        commit_verified_critical_handoff(request, &fetched, acceptance_mode, staged).await
    } else {
        let mut session = request.session.write().await;
        let commit_now_ms = current_time_millis();
        commit_fetched_manifest_with_acceptance_mode(
            &mut session,
            &fetched,
            request,
            commit_now_ms,
            acceptance_mode,
            staged_switch.as_ref(),
        )
    };
    cancel_superseded_terminal_work_after_media_progress(request, &commit_result).await;
    match commit_result {
        Ok((progress_evidence, wake_segment_scheduler, wake_map_scheduler)) => {
            if let Some(staged) = staged_switch.as_mut() {
                staged.disarm_cleanup();
            }
            Ok(CommittedOriginManifest { fetched, progress_evidence, wake_segment_scheduler, wake_map_scheduler })
        }
        Err(err) => {
            if let Some(staged_switch) = staged_switch.as_mut() {
                remove_uncommitted_staged_switch_files(request, staged_switch).await;
            }
            Err(err)
        }
    }
}

async fn cancel_superseded_terminal_work_after_media_progress(
    request: &OriginRefreshRequest,
    commit_result: &Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError>,
) {
    if !commit_result.as_ref().is_ok_and(|(evidence, _, _)| evidence.is_media_progress()) {
        return;
    }
    let proxy_session_id = request.session.read().await.proxy_session_id.clone();
    request.hls_proxy.cancel_superseded_terminal_work_for_session(&proxy_session_id);
}

use tuliprox_core::utils::current_time_millis;

#[cfg(test)]
mod tests;
