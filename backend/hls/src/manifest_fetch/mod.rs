#[cfg(any(test, feature = "test-support"))]
use self::http::fetch_origin_manifest_once;
use super::{
    hls_origin_log_value,
    manifest_origin_binding::HlsManifestOriginBinding,
    recovery_timing::{HlsAcceptanceEpisodeTimingSeed, HlsRecoveryTimingPolicy},
    HlsSessionHandle, TimelineMapError,
};
#[cfg(any(test, feature = "test-support"))]
use tokio::time::timeout;
#[cfg(any(test, feature = "test-support"))]
use tuliprox_core::utils::content_coding::ContentCodingError;

mod episode;
mod error;
mod fingerprint;
mod http;
mod quality;
mod recovery;
mod selection_log;

#[cfg(any(test, feature = "test-support"))]
pub use self::quality::score_hls_manifest_recovery_candidate;
#[cfg(any(test, feature = "test-support"))]
use self::selection_log::log_origin_refresh_retry_scheduled;
use self::{
    error::is_hls_retryable_initial_manifest_fetch_error,
    http::{
        fetch_hls_origin_manifest_recovery_direct_target, origin_manifest_fetch_error_from_io_error,
        response_to_fetched_manifest, ManifestRecoveryAttemptLogContext,
    },
    selection_log::{log_manifest_initial_attempt, log_manifest_retry_scheduled, ManifestRetryLogKind},
};
#[cfg(any(test, feature = "test-support"))]
pub use self::{
    error::{classify_origin_manifest_status, OriginManifestStatusClass},
    http::{hls_manifest_redirect_host, retry_after_delay_ms},
    quality::origin_highwater_policy_limit,
    selection_log::ManifestRecoverySelectionLogPhase,
};
pub use self::{
    error::{
        HlsManifestAcceptanceRejectReason, HlsManifestCommitError, HlsManifestRecoveryUnavailableReason,
        HlsManifestRejectLogReason, OriginManifestFetchError,
    },
    fingerprint::{
        deterministic_conflict_receipt_is_current, deterministic_conflict_receipt_matches,
        deterministic_timeline_conflict_from_rejection,
    },
    http::resolved_hls_manifest_request_url_from_input,
    quality::{
        evaluate_manifest_origin_quality_with_mode, next_committed_origin_highwater,
        score_hls_manifest_candidate_for_selection_log,
    },
    recovery::retry_hls_origin_manifest_recovery_chain,
    selection_log::log_hls_manifest_initial_selected,
};
use axum::http::{HeaderMap, StatusCode};
use log::debug;
use reqwest::Client;
use shared::model::InputFetchMethod;
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{AppConfig, ConfigProvider, HlsManifestRecoveryBurstConfig, InputSource},
    utils::{
        content_coding::OutboundContentCodingPolicy,
        request::{
            send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
            send_input_with_retry_and_provider_policy_with_options_result, RequestFetchOptions,
        },
    },
};
use url::Url;

const DEFAULT_HLS_TARGET_DURATION_SECS: u32 = 15;

use super::MAX_MANUAL_REDIRECTS;
const DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS: u64 = 300;
const HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT: usize = 64;
pub const MAX_HLS_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

/// Origin manifest entrypoint snapshot for live HLS refreshes.
#[derive(Clone)]
pub struct LiveHlsOriginEntry {
    url: Url,
    url_failover_provider: Option<Arc<ConfigProvider>>,
}

impl LiveHlsOriginEntry {
    pub fn parse(url: &str) -> Option<Self> { Self::parse_with_url_failover_provider(url, None) }

    pub fn parse_with_url_failover_provider(
        url: &str,
        url_failover_provider: Option<Arc<ConfigProvider>>,
    ) -> Option<Self> {
        Url::parse(url).ok().map(|url| Self { url, url_failover_provider })
    }

    pub fn url(&self) -> &Url { &self.url }

    pub fn url_failover_provider(&self) -> Option<&Arc<ConfigProvider>> { self.url_failover_provider.as_ref() }

    pub fn to_input_source(&self) -> InputSource {
        InputSource {
            name: Arc::<str>::from("hls-origin"),
            url: self.url.to_string(),
            // In this HLS context, InputSource.provider is the URL-failover provider from source.yml,
            // not a runtime origin-account provider.
            provider: self.url_failover_provider.clone(),
            username: None,
            password: None,
            method: InputFetchMethod::GET,
            headers: HashMap::new(),
        }
    }
}

impl fmt::Debug for LiveHlsOriginEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveHlsOriginEntry")
            .field("scheme", &self.url.scheme())
            .field("host", &self.url.host_str().unwrap_or("<missing>"))
            .field("path", &"<redacted>")
            .field("url_failover_provider", &self.url_failover_provider.as_ref().map(|provider| provider.name.as_ref()))
            .finish()
    }
}

/// Fixed retry policy for HLS origin manifest refreshes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RetryPolicy {
    pub delays_ms: [u64; 5],
    pub jitter_max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self { Self { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 100 } }
}

impl RetryPolicy {
    pub fn delay_for_attempt_ms(&self, attempt_index: usize, jitter_ms: u64) -> Option<u64> {
        self.delays_ms.get(attempt_index).map(|base| base.saturating_add(jitter_ms.min(self.jitter_max_ms)))
    }

    pub fn attempt_count(&self) -> usize { self.delays_ms.len() }

    /// Samples a uniform jitter in `0..=jitter_max_ms` (0 when jitter is disabled).
    pub fn sample_jitter_ms(&self) -> u64 {
        if self.jitter_max_ms == 0 {
            0
        } else {
            fastrand::u64(0..=self.jitter_max_ms)
        }
    }
}

impl From<HlsManifestAcceptanceRejectReason> for HlsManifestRejectLogReason {
    fn from(reason: HlsManifestAcceptanceRejectReason) -> Self {
        match reason {
            HlsManifestAcceptanceRejectReason::MissingOriginHighwater => Self::MissingOriginHighwater,
            HlsManifestAcceptanceRejectReason::ForwardTooFar { previous, origin, window } => {
                Self::ForwardTooFar { previous, origin, window }
            }
            HlsManifestAcceptanceRejectReason::BackwardOutsideRollover { previous, origin, window } => {
                Self::BackwardOutsideRollover { previous, origin, window }
            }
        }
    }
}

impl From<TimelineMapError> for HlsManifestRejectLogReason {
    fn from(err: TimelineMapError) -> Self {
        match err {
            TimelineMapError::UnsupportedSegmentExtension => Self::UnsupportedSegmentExtension,
            TimelineMapError::UnsupportedMapExtension => Self::UnsupportedMapExtension,
            TimelineMapError::ProxySequenceOverflow => Self::ProxySequenceOverflow,
            TimelineMapError::ProxyMapIdOverflow => Self::ProxyMapIdOverflow,
            TimelineMapError::MissingKeyResource => Self::MissingKeyResource,
            TimelineMapError::PublishedResourceReplay {
                previous_proxy_tail,
                existing_proxy_seq,
                candidate_position,
                candidate_origin_seq,
                resource_key,
                decision,
            } => Self::PublishedResourceReplay {
                previous_proxy_tail,
                existing_proxy_seq,
                candidate_position,
                candidate_origin_seq,
                resource_key,
                decision,
            },
            TimelineMapError::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq } => {
                Self::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestCommitAcceptanceMode {
    StrictPinnedHost,
    FreshPinnedRevalidation,
    AllowHeldHostSwitchCandidate,
    AllowVerifiedContentAnchorHostSwitchCandidate,
    AllowVerifiedEmergencyHostSwitchCandidate,
    FreshBaseline,
}

#[derive(Clone, Eq, PartialEq)]
pub struct FetchedOriginManifest {
    pub body: String,
    pub final_manifest_url: String,
    pub resolved_request_url: String,
    pub redirect_host: Option<String>,
    pub provider_url_index: Option<usize>,
    pub provider_session_headers: HeaderMap,
    pub status: StatusCode,
    /// Logical initial attempts or recovery generations. Existing refresh
    /// retry metrics intentionally retain this semantic.
    pub attempts: usize,
    pub candidate_requests: usize,
    pub selection: HlsManifestFetchSelection,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsManifestFetchSelection {
    Initial,
    Recovery,
    Burst,
}

impl HlsManifestFetchSelection {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Recovery => "recovery",
            Self::Burst => "burst",
        }
    }
}

impl fmt::Debug for FetchedOriginManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchedOriginManifest")
            .field("body_len", &self.body.len())
            .field("final_manifest_url", &"<redacted>")
            .field("resolved_request_url", &"<redacted>")
            .field("redirect_host", &self.redirect_host.as_ref().map(|_| "<redacted>"))
            .field("provider_url_index", &self.provider_url_index)
            .field("provider_session_headers_len", &self.provider_session_headers.len())
            .field("status", &self.status)
            .field("attempts", &self.attempts)
            .field("candidate_requests", &self.candidate_requests)
            .field("selection", &self.selection)
            .finish()
    }
}

impl FetchedOriginManifest {
    pub fn with_attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts;
        if self.selection == HlsManifestFetchSelection::Initial {
            self.candidate_requests = attempts;
        }
        self
    }

    fn with_recovery_diagnostics(
        mut self,
        recovery_attempts: usize,
        candidate_requests: usize,
        selection: HlsManifestFetchSelection,
    ) -> Self {
        self.attempts = recovery_attempts;
        self.candidate_requests = candidate_requests;
        self.selection = selection;
        self
    }
}

#[derive(Clone)]
pub struct HlsOriginManifestFetchContext {
    pub app_config: Arc<AppConfig>,
    pub session: HlsSessionHandle,
    pub origin_entry: LiveHlsOriginEntry,
    pub headers: HeaderMap,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub origin_manifest_timeout_ms: u64,
    pub manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    pub retry_policy: RetryPolicy,
    /// Expected-latency policy. Hard operation timeouts remain separate and do
    /// not become playback trigger or cutover estimates.
    pub recovery_timing_policy: HlsRecoveryTimingPolicy,
    /// Lease-specific pressure evidence captured before refresh scheduling.
    /// Each started generation freezes its own timing from this seed.
    pub acceptance_timing_seed: Option<HlsAcceptanceEpisodeTimingSeed>,
}

enum HlsOriginManifestFetchMode<'a> {
    InitialGlobalPolicy,
    RecoveryDirectTarget {
        binding: &'a HlsManifestOriginBinding,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    },
}

pub struct HlsOriginManifestFetchRequest<'a> {
    context: &'a HlsOriginManifestFetchContext,
    mode: HlsOriginManifestFetchMode<'a>,
}

impl<'a> HlsOriginManifestFetchRequest<'a> {
    pub const fn initial_global_policy(context: &'a HlsOriginManifestFetchContext) -> Self {
        Self { context, mode: HlsOriginManifestFetchMode::InitialGlobalPolicy }
    }

    const fn recovery_direct_target(
        context: &'a HlsOriginManifestFetchContext,
        binding: &'a HlsManifestOriginBinding,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    ) -> Self {
        Self { context, mode: HlsOriginManifestFetchMode::RecoveryDirectTarget { binding, reason, log_context } }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestRecoveryCandidateScoreReport {
    pub media_sequence: u64,
    pub quality: HlsManifestOriginQuality,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsManifestOriginQualityScore {
    Rejected,
    OtherHostCandidate,
    SameHostUnchanged,
    SameHostRolloverCandidate,
    SameHostRebase,
    SameHostPlausibleForward,
    SameHostNextSequence,
}

impl HlsManifestOriginQualityScore {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::OtherHostCandidate => "other-host-candidate",
            Self::SameHostUnchanged => "same-host-unchanged",
            Self::SameHostRolloverCandidate => "same-host-rollover-candidate",
            Self::SameHostRebase => "same-host-rebase",
            Self::SameHostPlausibleForward => "same-host-plausible-forward",
            Self::SameHostNextSequence => "same-host-next-sequence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestOriginRelation {
    Initial,
    SameRedirectHost,
    OtherRedirectHost,
    UnknownHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestSequenceRelation {
    NoPreviousHighwater,
    NoOriginHighwater,
    Rebase,
    Same,
    Next,
    PlausibleForward,
    ForwardTooFar,
    RolloverCandidate,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestContinuityMode {
    StrictContinuity,
    RebaseAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestOriginQuality {
    pub score: HlsManifestOriginQualityScore,
    pub host_relation: HlsManifestOriginRelation,
    pub sequence_relation: HlsManifestSequenceRelation,
    pub effective_host: Option<String>,
    pub origin_highwater: Option<u64>,
    pub previous_highwater: Option<u64>,
    pub allowed_forward_window: Option<u64>,
    pub requires_handoff_discontinuity: bool,
    pub reject_reason: Option<HlsManifestAcceptanceRejectReason>,
}

pub async fn fetch_hls_origin_manifest_request(
    request: HlsOriginManifestFetchRequest<'_>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    match request.mode {
        HlsOriginManifestFetchMode::InitialGlobalPolicy => {
            fetch_hls_origin_manifest_initial_global_policy(request.context).await
        }
        HlsOriginManifestFetchMode::RecoveryDirectTarget { binding, reason, log_context } => {
            fetch_hls_origin_manifest_recovery_direct_target(request.context, binding, reason, log_context).await
        }
    }
}

async fn fetch_hls_origin_manifest_initial_global_policy(
    context: &HlsOriginManifestFetchContext,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    log_manifest_initial_attempt(context).await;
    let input_source = context.origin_entry.to_input_source();
    let account_binding = {
        let session = context.session.read().await;
        if session.origin_account_binding.is_some() {
            "present"
        } else {
            "absent"
        }
    };
    debug!(
        "HLS origin manifest request started: account_binding={account_binding} request_url={}",
        hls_origin_log_value(input_source.url.as_str())
    );
    let fetch_options = RequestFetchOptions::with_attempt_idle_timeout(Duration::from_millis(
        context.origin_manifest_timeout_ms.max(1),
    ))
    .with_content_coding(OutboundContentCodingPolicy::Identity)
    .without_resource_retries();
    let attempts = context.retry_policy.attempt_count();

    // This HLS loop owns the logical manifest-attempt budget. Each iteration may traverse one bounded provider URL
    // failover cycle and redirect chain, but the generic resource-retry counter is not nested beneath it.
    for attempt_index in 0..attempts {
        let response_result = if context.use_manual_redirects {
            send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
                &context.app_config,
                &context.no_redirect_client,
                &input_source,
                Some(&context.headers),
                context.origin_entry.url(),
                MAX_MANUAL_REDIRECTS,
                fetch_options,
            )
            .await
        } else {
            send_input_with_retry_and_provider_policy_with_options_result(
                &context.app_config,
                &context.client,
                &input_source,
                Some(&context.headers),
                context.origin_entry.url(),
                fetch_options,
            )
            .await
        };
        let fetch_result = match response_result {
            Ok(response_result) => {
                let provider_url_index = response_result.provider_url_index;
                let resolved_request_url = resolved_hls_manifest_request_url_from_input(
                    &input_source,
                    provider_url_index,
                    context.origin_entry.url(),
                );
                response_to_fetched_manifest(
                    response_result.response,
                    provider_url_index,
                    resolved_request_url,
                    context.origin_manifest_timeout_ms,
                )
                .await
            }
            Err(err) => Err(origin_manifest_fetch_error_from_io_error(&err)),
        };
        match fetch_result {
            Ok(fetched) => return Ok(fetched.with_attempts(attempt_index + 1)),
            Err(err) if is_hls_retryable_initial_manifest_fetch_error(&err) && attempt_index + 1 < attempts => {
                let retry_after_ms = match &err {
                    OriginManifestFetchError::RetryableStatus(_, retry_after_ms) => *retry_after_ms,
                    _ => None,
                };
                let jitter_ms = if retry_after_ms.is_some() { 0 } else { context.retry_policy.sample_jitter_ms() };
                let delay_ms = next_retry_delay_ms(&context.retry_policy, attempt_index, retry_after_ms, jitter_ms);
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::InitialFetch,
                    attempt_index,
                    attempts,
                    delay_ms,
                    None,
                    Some(&err),
                )
                .await;
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(OriginManifestFetchError::RetryExhausted)
}

fn request_hls_session_idle_timeout_secs_from_config(app_config: &AppConfig) -> u64 {
    app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.hls_cache.as_ref())
        .map_or(DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS, |hls_cache| hls_cache.session_idle_timeout)
        .max(1)
}

pub fn fetched_effective_manifest_host(fetched: &FetchedOriginManifest) -> Option<String> {
    if fetched.redirect_host.is_some() {
        return fetched.redirect_host.clone();
    }
    Url::parse(&fetched.resolved_request_url).ok().and_then(|url| url.host_str().map(str::to_string))
}

fn next_retry_delay_ms(
    retry_policy: &RetryPolicy,
    attempt_index: usize,
    retry_after_ms: Option<u64>,
    jitter_ms: u64,
) -> u64 {
    retry_after_ms
        .unwrap_or_else(|| retry_policy.delay_for_attempt_ms(attempt_index + 1, jitter_ms).unwrap_or_default())
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(any(test, feature = "test-support"))]
pub async fn refresh_from_live_hls_entrypoint_with_retries(
    origin_entry: &LiveHlsOriginEntry,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    origin_manifest_timeout_ms: u64,
    retry_policy: &RetryPolicy,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut retry_after_delay_ms = None;
    let attempts = retry_policy.attempt_count();

    for attempt_index in 0..attempts {
        let delay_ms = retry_after_delay_ms.take().unwrap_or_else(|| {
            let jitter = retry_policy.sample_jitter_ms();
            retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        });
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetch_result = timeout(
            Duration::from_millis(origin_manifest_timeout_ms.max(1)),
            fetch_origin_manifest_once(
                origin_entry.url(),
                headers,
                client,
                no_redirect_client,
                use_manual_redirects,
                None,
                origin_manifest_timeout_ms,
            ),
        )
        .await
        .map_err(|_| OriginManifestFetchError::Timeout);

        match fetch_result {
            Ok(Ok(fetched)) => return Ok(fetched.with_attempts(attempt_index + 1)),
            Ok(Err(OriginManifestFetchError::PermanentStatus(status))) => {
                return Err(OriginManifestFetchError::PermanentStatus(status));
            }
            Ok(Err(OriginManifestFetchError::NonRetryableStatus(status))) => {
                return Err(OriginManifestFetchError::NonRetryableStatus(status));
            }
            Ok(Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, retry_after_ms, 0),
                    format!("status={}", status.as_u16()),
                );
                retry_after_delay_ms = retry_after_ms;
            }
            Ok(Err(OriginManifestFetchError::Request(err))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Request(err));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    "error=request",
                );
            }
            Ok(Err(
                err @ (OriginManifestFetchError::ContentDecoding { .. }
                | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_))
                | OriginManifestFetchError::Redirect(_)
                | OriginManifestFetchError::Timeout),
            )) => {
                if attempt_index + 1 == attempts {
                    return Err(err);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    format!("error={}", err.log_label()),
                );
            }
            Err(OriginManifestFetchError::Timeout) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Timeout);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    "error=timeout",
                );
            }
            Ok(Err(
                err @ (OriginManifestFetchError::RetryExhausted
                | OriginManifestFetchError::RecoveryUnavailable { .. }
                | OriginManifestFetchError::DeterministicTimelineConflict(_)
                | OriginManifestFetchError::ProviderUnavailable(_)
                | OriginManifestFetchError::ContentCoding(_)
                | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
                | OriginManifestFetchError::InvalidUtf8 { .. }),
            ))
            | Err(err) => return Err(err),
        }
    }

    Err(OriginManifestFetchError::RetryExhausted)
}

#[cfg(test)]
mod tests;
