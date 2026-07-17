use super::{
    extract_hls_provider_session_header_map, log_hls_origin_content_coding, safe_origin_log_value, safe_session_key,
    HlsAccountBindingProtection, HlsBoundAccountAcquireErrorKind, HlsOriginContentCodingObjectKind,
    HlsOriginContentCodingSource, HlsSessionHandle, HlsSessionMode, TimelineMapError,
};
use crate::{
    model::{
        resolve_provider_scheme_url_with_provider_index, AppConfig, ConfigProvider, HlsManifestRecoveryBurstConfig,
        InputSource, StripConfig,
    },
    processing::parser::hls::origin_manifest::{
        parse_manifest_timing, parse_origin_manifest_timeline, parse_origin_media_manifest, OriginManifestParseOutcome,
        ParsedOriginManifestTimeline,
    },
    utils::{
        content_coding::{
            apply_outbound_content_coding_policy, content_decoding_error_from_io, decode_response_to_identity,
            is_http_body_transport_error, read_utf8_limited, ContentBodyReadError, ContentCoding,
            ContentCodingDetection, ContentCodingError, OutboundContentCodingPolicy,
        },
        request::{
            send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
            send_input_with_retry_and_provider_policy_with_options_result, RequestFetchOptions,
        },
    },
};
use axum::http::{header, HeaderMap, StatusCode};
use log::{debug, warn};
use reqwest::Client;
use shared::{
    model::{HlsManifestRecoveryBurstLevel, HlsManifestRecoveryBurstPlan, HlsStripMode, InputFetchMethod},
    utils::sanitize_sensitive_info,
};
use std::{collections::HashMap, fmt, future::Future, io, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::timeout};
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;
const DEFAULT_HLS_TARGET_DURATION_SECS: u32 = 15;
const HLS_MANIFEST_HOST_SWITCH_BASE_WINDOW_SEGMENTS: u32 = 3;
const HLS_MANIFEST_HOST_SWITCH_MAX_FAILURE_THRESHOLD: u32 = 5;
const DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS: u64 = 300;
pub(crate) const MAX_HLS_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

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

    pub(crate) fn attempt_count(&self) -> usize { self.delays_ms.len() }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OriginManifestStatusClass {
    Success,
    Retryable,
    PermanentFailure,
    NonRetryableFailure,
}

pub fn classify_origin_manifest_status(status: StatusCode) -> OriginManifestStatusClass {
    if status.is_success() {
        return OriginManifestStatusClass::Success;
    }
    if status.is_server_error()
        || matches!(
            status,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return OriginManifestStatusClass::Retryable;
    }
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE
    ) {
        return OriginManifestStatusClass::PermanentFailure;
    }
    OriginManifestStatusClass::NonRetryableFailure
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OriginManifestFetchError {
    #[error("origin manifest returned permanent status {0}")]
    PermanentStatus(StatusCode),
    #[error("origin manifest returned retryable status {0}, retry_after_ms={1:?}")]
    RetryableStatus(StatusCode, Option<u64>),
    #[error("origin manifest retry attempts exhausted")]
    RetryExhausted,
    #[error("origin manifest returned non-retryable status {0}")]
    NonRetryableStatus(StatusCode),
    #[error("origin manifest request failed: {0}")]
    Request(String),
    #[error("origin manifest redirect failed: {0}")]
    Redirect(String),
    #[error("origin manifest request timed out")]
    Timeout,
    #[error("origin provider unavailable: {0:?}")]
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
    #[error("origin manifest content coding failed: {0}")]
    ContentCoding(ContentCodingError),
    #[error("origin manifest decoding failed: coding={coding:?}")]
    ContentDecoding { coding: ContentCoding },
    #[error("decoded origin manifest exceeds configured limit {limit}")]
    DecodedBodyLimitExceeded { limit: usize },
    #[error("decoded origin manifest is not valid UTF-8: valid_up_to={valid_up_to} error_len={error_len:?}")]
    InvalidUtf8 { valid_up_to: usize, error_len: Option<usize> },
}

impl OriginManifestFetchError {
    /// Returns a fixed/numeric diagnostic label without origin-controlled strings.
    pub(crate) fn log_label(&self) -> String {
        match self {
            Self::PermanentStatus(status) => format!("permanent_status status={}", status.as_u16()),
            Self::RetryableStatus(status, _) => format!("retryable_status status={}", status.as_u16()),
            Self::RetryExhausted => "retry_exhausted".to_string(),
            Self::NonRetryableStatus(status) => format!("non_retryable_status status={}", status.as_u16()),
            Self::Request(_) => "request".to_string(),
            Self::Redirect(_) => "redirect".to_string(),
            Self::Timeout => "timeout".to_string(),
            Self::ProviderUnavailable(kind) => format!("provider_unavailable kind={}", kind.as_log_label()),
            Self::ContentCoding(error) => match error {
                ContentCodingError::InvalidHeader => "content_coding class=invalid_header".to_string(),
                ContentCodingError::Unsupported(_) => "content_coding class=unsupported".to_string(),
                ContentCodingError::EncodedPartialContent => "content_coding class=encoded_partial_content".to_string(),
                ContentCodingError::PrefixRead(error) => content_decoding_error_from_io(error).map_or_else(
                    || "content_coding class=prefix_read".to_string(),
                    |error| format!("content_decoding coding={}", error.coding.as_http_token()),
                ),
            },
            Self::ContentDecoding { coding, .. } => {
                format!("content_decoding coding={}", coding.as_http_token())
            }
            Self::DecodedBodyLimitExceeded { limit } => format!("decoded_body_limit limit={limit}"),
            Self::InvalidUtf8 { valid_up_to, error_len } => {
                format!("invalid_utf8 valid_up_to={valid_up_to} error_len={error_len:?}")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum HlsManifestCommitError {
    TimelineRejected { reason: HlsManifestRejectLogReason },
    RetryCurrentTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HlsManifestAcceptanceRejectReason {
    MissingPinnedTarget,
    HostSwitchPending { failures: u32, threshold: u32 },
    MissingOriginHighwater,
    ForwardTooFar { previous: u64, origin: u64, window: Option<u64> },
    BackwardOutsideRollover { previous: u64, origin: u64, window: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HlsManifestRejectLogReason {
    MissingPinnedTarget,
    HostSwitchPending { failures: u32, threshold: u32 },
    MissingOriginHighwater,
    ForwardTooFar { previous: u64, origin: u64, window: Option<u64> },
    BackwardOutsideRollover { previous: u64, origin: u64, window: Option<u64> },
    PinnedHostRecoveryRejected,
    UnsupportedSegmentExtension,
    UnsupportedMapExtension,
    ProxySequenceOverflow,
    ProxyMapIdOverflow,
    MalformedTransientTimeline,
}

impl HlsManifestRejectLogReason {
    pub(crate) fn status_label(&self) -> String {
        match self {
            Self::MissingPinnedTarget => "missing-pinned-target".to_string(),
            Self::HostSwitchPending { failures, threshold } => {
                format!("host-switch-pending failures={failures} threshold={threshold}")
            }
            Self::MissingOriginHighwater => "missing-origin-highwater".to_string(),
            Self::ForwardTooFar { previous, origin, window } => {
                format!(
                    "forward-too-far previous={previous} origin={origin} window={}",
                    format_optional_highwater(*window)
                )
            }
            Self::BackwardOutsideRollover { previous, origin, window } => {
                format!(
                    "backward-outside-rollover previous={previous} origin={origin} window={}",
                    format_optional_highwater(*window)
                )
            }
            Self::PinnedHostRecoveryRejected => "pinned-host-recovery-rejected".to_string(),
            Self::UnsupportedSegmentExtension => "unsupported-segment-extension".to_string(),
            Self::UnsupportedMapExtension => "unsupported-map-extension".to_string(),
            Self::ProxySequenceOverflow => "proxy-sequence-overflow".to_string(),
            Self::ProxyMapIdOverflow => "proxy-map-id-overflow".to_string(),
            Self::MalformedTransientTimeline => "malformed-transient-timeline".to_string(),
        }
    }
}

impl From<HlsManifestAcceptanceRejectReason> for HlsManifestRejectLogReason {
    fn from(reason: HlsManifestAcceptanceRejectReason) -> Self {
        match reason {
            HlsManifestAcceptanceRejectReason::MissingPinnedTarget => Self::MissingPinnedTarget,
            HlsManifestAcceptanceRejectReason::HostSwitchPending { failures, threshold } => {
                Self::HostSwitchPending { failures, threshold }
            }
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsManifestCommitAcceptanceMode {
    StrictPinnedHost,
    AllowHeldHostSwitchCandidate,
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
    pub attempts: usize,
}

impl fmt::Debug for FetchedOriginManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchedOriginManifest")
            .field("body_len", &self.body.len())
            .field("final_manifest_url", &"<redacted>")
            .field("resolved_request_url", &"<redacted>")
            .field("redirect_host", &self.redirect_host)
            .field("provider_url_index", &self.provider_url_index)
            .field("provider_session_headers_len", &self.provider_session_headers.len())
            .field("status", &self.status)
            .field("attempts", &self.attempts)
            .finish()
    }
}

impl FetchedOriginManifest {
    pub(crate) fn with_attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts;
        self
    }
}

#[derive(Clone)]
pub(crate) struct HlsOriginManifestFetchContext {
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
}

enum HlsOriginManifestFetchMode<'a> {
    InitialGlobalPolicy,
    RecoveryDirectTarget {
        target_url: &'a Url,
        provider_url_index: Option<usize>,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    },
}

pub(crate) struct HlsOriginManifestFetchRequest<'a> {
    context: &'a HlsOriginManifestFetchContext,
    mode: HlsOriginManifestFetchMode<'a>,
}

impl<'a> HlsOriginManifestFetchRequest<'a> {
    pub(crate) const fn initial_global_policy(context: &'a HlsOriginManifestFetchContext) -> Self {
        Self { context, mode: HlsOriginManifestFetchMode::InitialGlobalPolicy }
    }

    const fn recovery_direct_target(
        context: &'a HlsOriginManifestFetchContext,
        target_url: &'a Url,
        provider_url_index: Option<usize>,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    ) -> Self {
        Self {
            context,
            mode: HlsOriginManifestFetchMode::RecoveryDirectTarget {
                target_url,
                provider_url_index,
                reason,
                log_context,
            },
        }
    }
}

enum HlsManifestRecoveryAttemptError<T> {
    Fetch(OriginManifestFetchError),
    Rejected(HlsManifestRejectLogReason),
    Committed(T),
}

#[derive(Debug)]
struct HlsManifestRecoveryCandidate {
    candidate_index: usize,
    fetched: FetchedOriginManifest,
    report: HlsManifestRecoveryCandidateScoreReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsManifestRecoveryCandidateScoreReport {
    pub(crate) media_sequence: u64,
    pub(crate) quality: HlsManifestOriginQuality,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum HlsManifestOriginQualityScore {
    Rejected,
    OtherHostUnchanged,
    SameHostUnchanged,
    OtherHostRolloverCandidate,
    SameHostRolloverCandidate,
    OtherHostRebaseCandidate,
    OtherHostPlausibleForward,
    OtherHostNextSequence,
    SameHostRebase,
    SameHostPlausibleForward,
    SameHostNextSequence,
}

impl HlsManifestOriginQualityScore {
    pub(crate) const fn rank(self) -> u16 {
        match self {
            Self::Rejected => 0,
            Self::OtherHostUnchanged => 10,
            Self::SameHostUnchanged => 20,
            Self::OtherHostRolloverCandidate => 35,
            Self::SameHostRolloverCandidate => 50,
            Self::OtherHostRebaseCandidate => 60,
            Self::OtherHostPlausibleForward => 65,
            Self::OtherHostNextSequence => 75,
            Self::SameHostRebase => 85,
            Self::SameHostPlausibleForward => 90,
            Self::SameHostNextSequence => 100,
        }
    }

    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::OtherHostUnchanged => "other-host-unchanged",
            Self::SameHostUnchanged => "same-host-unchanged",
            Self::OtherHostRolloverCandidate => "other-host-rollover-candidate",
            Self::SameHostRolloverCandidate => "same-host-rollover-candidate",
            Self::OtherHostRebaseCandidate => "other-host-rebase-candidate",
            Self::OtherHostPlausibleForward => "other-host-plausible-forward",
            Self::OtherHostNextSequence => "other-host-next-sequence",
            Self::SameHostRebase => "same-host-rebase",
            Self::SameHostPlausibleForward => "same-host-plausible-forward",
            Self::SameHostNextSequence => "same-host-next-sequence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsManifestOriginRelation {
    Initial,
    SameRedirectHost,
    OtherRedirectHost,
    UnknownHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsManifestSequenceRelation {
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
pub(crate) enum HlsManifestContinuityMode {
    StrictContinuity,
    RebaseAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsManifestOriginQuality {
    pub(crate) score: HlsManifestOriginQualityScore,
    pub(crate) continuity_mode: HlsManifestContinuityMode,
    pub(crate) host_relation: HlsManifestOriginRelation,
    pub(crate) sequence_relation: HlsManifestSequenceRelation,
    pub(crate) effective_host: Option<String>,
    pub(crate) origin_highwater: Option<u64>,
    pub(crate) previous_highwater: Option<u64>,
    pub(crate) allowed_forward_window: Option<u64>,
    pub(crate) should_increment_stall_counter: bool,
    pub(crate) should_reset_stall_counter: bool,
    pub(crate) requires_handoff_discontinuity: bool,
    pub(crate) reject_reason: Option<HlsManifestAcceptanceRejectReason>,
}

pub(crate) async fn fetch_hls_origin_manifest_request(
    request: HlsOriginManifestFetchRequest<'_>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    match request.mode {
        HlsOriginManifestFetchMode::InitialGlobalPolicy => {
            fetch_hls_origin_manifest_initial_global_policy(request.context).await
        }
        HlsOriginManifestFetchMode::RecoveryDirectTarget { target_url, provider_url_index, reason, log_context } => {
            fetch_hls_origin_manifest_recovery_direct_target(
                request.context,
                target_url,
                provider_url_index,
                reason,
                log_context,
            )
            .await
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
        "HLS origin manifest request started: account_binding={account_binding} origin_entry={}",
        safe_origin_log_value(input_source.url.as_str())
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
                let jitter_ms = if retry_after_ms.is_some() || context.retry_policy.jitter_max_ms == 0 {
                    0
                } else {
                    fastrand::u64(0..=context.retry_policy.jitter_max_ms)
                };
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

pub(crate) async fn score_hls_manifest_candidate_for_selection_log(
    context: &HlsOriginManifestFetchContext,
    fetched: &FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Option<HlsManifestRecoveryCandidateScoreReport> {
    let session = context.session.read().await;
    let timeline = parse_manifest_timeline_for_recovery_scoring(&session, fetched).ok()?;
    Some(HlsManifestRecoveryCandidateScoreReport {
        media_sequence: timeline.origin_manifest_sequence,
        quality: evaluate_manifest_origin_quality_with_mode(
            &session,
            fetched,
            timeline,
            context,
            current_time_millis(),
            acceptance_mode,
        ),
    })
}

pub(crate) async fn retry_hls_origin_manifest_recovery_chain<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    target_url: Url,
    provider_url_index: Option<usize>,
    mut reject_reason: Option<HlsManifestRejectLogReason>,
    mut commit: C,
) -> Result<T, OriginManifestFetchError>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let attempts = context.retry_policy.attempt_count();
    let mut last_error = OriginManifestFetchError::RetryExhausted;

    for attempt_index in 0..attempts {
        let delay_ms = {
            let jitter = if context.retry_policy.jitter_max_ms == 0 {
                0
            } else {
                fastrand::u64(0..=context.retry_policy.jitter_max_ms)
            };
            context.retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        };
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let attempt_plan = HlsManifestRecoveryAttemptPlan {
            target_url: &target_url,
            provider_url_index,
            attempt_index,
            attempts,
            reject_reason: reject_reason.as_ref(),
            acceptance_mode: HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        };
        match fetch_and_commit_manifest_recovery_attempt(context, attempt_plan, &mut commit).await {
            HlsManifestRecoveryAttemptError::Committed(committed) => return Ok(committed),
            HlsManifestRecoveryAttemptError::Rejected(reason) if attempt_index + 1 < attempts => {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempts,
                    next_retry_delay_ms(&context.retry_policy, attempt_index, None, 0),
                    Some(&reason),
                    None,
                )
                .await;
                reject_reason = Some(reason);
                last_error = OriginManifestFetchError::RetryExhausted;
            }
            HlsManifestRecoveryAttemptError::Rejected(_reason) => {
                return Err(OriginManifestFetchError::RetryExhausted);
            }
            HlsManifestRecoveryAttemptError::Fetch(err)
                if is_hls_retryable_manifest_reject_fetch_error(&err) && attempt_index + 1 < attempts =>
            {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempts,
                    next_retry_delay_ms(&context.retry_policy, attempt_index, None, 0),
                    None,
                    Some(&err),
                )
                .await;
                last_error = err;
            }
            HlsManifestRecoveryAttemptError::Fetch(err) => return Err(err),
        }
    }

    Err(last_error)
}

struct HlsManifestRecoveryAttemptPlan<'a> {
    target_url: &'a Url,
    provider_url_index: Option<usize>,
    attempt_index: usize,
    attempts: usize,
    reject_reason: Option<&'a HlsManifestRejectLogReason>,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
}

async fn fetch_and_commit_manifest_recovery_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    commit: &mut C,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let burst_plan = recovery_burst_plan(context, plan.attempt_index);
    let candidates = burst_plan.total_candidates();
    if candidates == 1 {
        let fetched = match fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::recovery_direct_target(
            context,
            plan.target_url,
            plan.provider_url_index,
            plan.reject_reason,
            ManifestRecoveryAttemptLogContext::single(plan.attempt_index, plan.attempts),
        ))
        .await
        {
            Ok(fetched) => fetched,
            Err(err) => return HlsManifestRecoveryAttemptError::Fetch(err),
        };
        let report = score_manifest_recovery_candidate_with_logging(context, 0, candidates, &fetched).await.ok();
        let committed = commit(fetched.with_attempts(plan.attempt_index + 1), plan.acceptance_mode).await;
        match (committed, report.as_ref()) {
            (Ok(committed), Some(report)) => {
                log_manifest_recovery_selected(context, 0, candidates, report).await;
                HlsManifestRecoveryAttemptError::Committed(committed)
            }
            (Ok(committed), None) => HlsManifestRecoveryAttemptError::Committed(committed),
            (Err(err), _) => HlsManifestRecoveryAttemptError::Rejected(commit_error_to_retry_reason(&err)),
        }
    } else {
        fetch_and_commit_manifest_recovery_burst_attempt(context, plan, burst_plan, commit).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_and_commit_manifest_recovery_burst_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    commit: &mut C,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let (mut fetched_candidates, last_fetch_error, mut last_reject_reason) = fetch_manifest_recovery_burst_candidates(
        context,
        plan.target_url,
        plan.provider_url_index,
        plan.attempt_index,
        plan.attempts,
        plan.reject_reason,
        burst_plan,
    )
    .await;
    let candidates = burst_plan.total_candidates();

    fetched_candidates.sort_by(|left, right| {
        right
            .report
            .quality
            .score
            .rank()
            .cmp(&left.report.quality.score.rank())
            .then_with(|| right.report.quality.origin_highwater.cmp(&left.report.quality.origin_highwater))
            .then_with(|| left.candidate_index.cmp(&right.candidate_index))
    });
    for candidate in fetched_candidates {
        let HlsManifestRecoveryCandidate { candidate_index, fetched, report } = candidate;
        match commit(fetched.with_attempts(plan.attempt_index + 1), plan.acceptance_mode).await {
            Ok(committed) => {
                log_manifest_recovery_selected(context, candidate_index, candidates, &report).await;
                return HlsManifestRecoveryAttemptError::Committed(committed);
            }
            Err(err) => {
                let reason = commit_error_to_retry_reason(&err);
                log_manifest_recovery_candidate_rejected(
                    context,
                    candidate_index,
                    candidates,
                    report.quality.effective_host.as_deref(),
                    report.quality.origin_highwater,
                    &reason,
                )
                .await;
                last_reject_reason = Some(reason);
            }
        }
    }

    if let Some(reason) = last_reject_reason {
        return HlsManifestRecoveryAttemptError::Rejected(reason);
    }
    HlsManifestRecoveryAttemptError::Fetch(last_fetch_error.unwrap_or(OriginManifestFetchError::RetryExhausted))
}

async fn fetch_manifest_recovery_burst_candidates(
    context: &HlsOriginManifestFetchContext,
    target_url: &Url,
    provider_url_index: Option<usize>,
    attempt_index: usize,
    attempts: usize,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> (Vec<HlsManifestRecoveryCandidate>, Option<OriginManifestFetchError>, Option<HlsManifestRejectLogReason>) {
    let mut tasks = JoinSet::new();
    let candidates = burst_plan.total_candidates();
    for candidate_index in 0..candidates {
        let context = context.clone();
        let target_url = target_url.clone();
        let reject_reason = reject_reason.cloned();
        tasks.spawn(async move {
            let stagger_ms =
                u64::try_from(burst_plan.slot_for_candidate(candidate_index)).unwrap_or_default().saturating_mul(100);
            if stagger_ms > 0 {
                tokio::time::sleep(Duration::from_millis(stagger_ms)).await;
            }
            let request = HlsOriginManifestFetchRequest::recovery_direct_target(
                &context,
                &target_url,
                provider_url_index,
                reject_reason.as_ref(),
                ManifestRecoveryAttemptLogContext { attempt_index, attempts, candidate_index, candidates },
            );
            let result = fetch_hls_origin_manifest_request(request).await;
            (candidate_index, result)
        });
    }

    let mut last_fetch_error = None;
    let mut last_reject_reason = None;
    let mut fetched_candidates = Vec::new();
    while let Some(join_result) = tasks.join_next().await {
        let Ok((candidate_index, result)) = join_result else {
            last_fetch_error = Some(OriginManifestFetchError::Request("manifest recovery task failed".to_string()));
            continue;
        };
        match result {
            Ok(fetched) => {
                match score_manifest_recovery_candidate_with_logging(context, candidate_index, candidates, &fetched)
                    .await
                {
                    Ok(report) => {
                        fetched_candidates.push(HlsManifestRecoveryCandidate { candidate_index, fetched, report });
                    }
                    Err(reason) => last_reject_reason = Some(reason),
                }
            }
            Err(err) => {
                last_fetch_error = Some(err);
            }
        }
    }
    (fetched_candidates, last_fetch_error, last_reject_reason)
}

async fn score_manifest_recovery_candidate_with_logging(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    fetched: &FetchedOriginManifest,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let score_result = {
        let session = context.session.read().await;
        score_hls_manifest_recovery_candidate(&session, fetched, context)
    };
    match score_result {
        Ok(report) => {
            log_manifest_recovery_candidate_scored(context, candidate_index, candidates, &report).await;
            Ok(report)
        }
        Err(reason) => {
            log_manifest_recovery_candidate_rejected(
                context,
                candidate_index,
                candidates,
                fetched_effective_manifest_host(fetched).as_deref(),
                None,
                &reason,
            )
            .await;
            Err(reason)
        }
    }
}

pub(crate) fn score_hls_manifest_recovery_candidate(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
    context: &HlsOriginManifestFetchContext,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let timeline = parse_manifest_timeline_for_recovery_scoring(session, fetched)?;
    let media_sequence = timeline.origin_manifest_sequence;
    Ok(HlsManifestRecoveryCandidateScoreReport {
        media_sequence,
        quality: evaluate_manifest_origin_quality(session, fetched, timeline, context, current_time_millis()),
    })
}

fn parse_manifest_timeline_for_recovery_scoring(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
) -> Result<ParsedOriginManifestTimeline, HlsManifestRejectLogReason> {
    if matches!(session.mode, HlsSessionMode::TransientPassthrough { .. }) {
        return parse_origin_manifest_timeline(&fetched.body)
            .map_err(|_| HlsManifestRejectLogReason::MalformedTransientTimeline);
    }
    match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) => Ok(ParsedOriginManifestTimeline {
            origin_manifest_sequence: manifest.origin_manifest_sequence,
            origin_manifest_segment_cnt: manifest.origin_manifest_segment_cnt,
        }),
        OriginManifestParseOutcome::TransientPassthrough { .. } => parse_origin_manifest_timeline(&fetched.body)
            .map_err(|_| HlsManifestRejectLogReason::MalformedTransientTimeline),
    }
}

pub(crate) fn evaluate_manifest_origin_quality(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    context: &HlsOriginManifestFetchContext,
    now_ms: u64,
) -> HlsManifestOriginQuality {
    evaluate_manifest_origin_quality_with_mode(
        session,
        fetched,
        timeline,
        context,
        now_ms,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
    )
}

pub(crate) fn evaluate_manifest_origin_quality_with_mode(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    context: &HlsOriginManifestFetchContext,
    now_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> HlsManifestOriginQuality {
    let effective_host = fetched_effective_manifest_host(fetched);
    let fresh_baseline = matches!(acceptance_mode, HlsManifestCommitAcceptanceMode::FreshBaseline);
    let host_relation = if fresh_baseline {
        if effective_host.is_some() {
            HlsManifestOriginRelation::Initial
        } else {
            HlsManifestOriginRelation::UnknownHost
        }
    } else {
        match (session.last_effective_manifest_host.as_deref(), effective_host.as_deref()) {
            (None, _) => HlsManifestOriginRelation::Initial,
            (_, None) => HlsManifestOriginRelation::UnknownHost,
            (Some(pinned), Some(effective)) if pinned == effective => HlsManifestOriginRelation::SameRedirectHost,
            (Some(_), Some(_)) => HlsManifestOriginRelation::OtherRedirectHost,
        }
    };
    let origin_highwater = timeline.origin_highwater();
    let previous_highwater = if fresh_baseline { None } else { session.origin_seq_highwater };
    let continuity_mode = if fresh_baseline {
        HlsManifestContinuityMode::RebaseAllowed
    } else {
        manifest_continuity_mode(session, now_ms)
    };
    let allowed_forward_window = allowed_manifest_forward_window(session, context, Some(&fetched.body));
    let sequence_relation = classify_manifest_sequence_relation(
        previous_highwater,
        origin_highwater,
        allowed_forward_window,
        continuity_mode,
    );
    let reject_reason =
        manifest_quality_reject_reason(sequence_relation, previous_highwater, origin_highwater, allowed_forward_window);
    let score = manifest_origin_quality_score(host_relation, sequence_relation, reject_reason.is_some());
    let should_reset_stall_counter = matches!(
        sequence_relation,
        HlsManifestSequenceRelation::NoPreviousHighwater
            | HlsManifestSequenceRelation::Rebase
            | HlsManifestSequenceRelation::Next
            | HlsManifestSequenceRelation::PlausibleForward
    );
    let should_increment_stall_counter = matches!(
        sequence_relation,
        HlsManifestSequenceRelation::NoOriginHighwater
            | HlsManifestSequenceRelation::Same
            | HlsManifestSequenceRelation::ForwardTooFar
            | HlsManifestSequenceRelation::Backward
    );

    HlsManifestOriginQuality {
        score,
        continuity_mode,
        host_relation,
        sequence_relation,
        effective_host,
        origin_highwater,
        previous_highwater,
        allowed_forward_window,
        should_increment_stall_counter,
        should_reset_stall_counter,
        requires_handoff_discontinuity: matches!(
            (host_relation, sequence_relation),
            (HlsManifestOriginRelation::OtherRedirectHost, _) | (_, HlsManifestSequenceRelation::RolloverCandidate)
        ),
        reject_reason,
    }
}

fn classify_manifest_sequence_relation(
    previous_highwater: Option<u64>,
    origin_highwater: Option<u64>,
    allowed_forward_window: Option<u64>,
    continuity_mode: HlsManifestContinuityMode,
) -> HlsManifestSequenceRelation {
    if matches!(continuity_mode, HlsManifestContinuityMode::RebaseAllowed) && origin_highwater.is_some() {
        return HlsManifestSequenceRelation::Rebase;
    }
    let Some(previous_highwater) = previous_highwater else {
        return HlsManifestSequenceRelation::NoPreviousHighwater;
    };
    let Some(origin_highwater) = origin_highwater else {
        return HlsManifestSequenceRelation::NoOriginHighwater;
    };
    if origin_highwater == previous_highwater {
        return HlsManifestSequenceRelation::Same;
    }
    if previous_highwater.checked_add(1) == Some(origin_highwater) {
        return HlsManifestSequenceRelation::Next;
    }
    if origin_highwater > previous_highwater {
        return if manifest_highwater_delta_within_window(
            origin_highwater.saturating_sub(previous_highwater),
            allowed_forward_window,
        ) {
            HlsManifestSequenceRelation::PlausibleForward
        } else {
            HlsManifestSequenceRelation::ForwardTooFar
        };
    }
    if origin_highwater_is_within_limit(origin_highwater, allowed_forward_window) {
        HlsManifestSequenceRelation::RolloverCandidate
    } else {
        HlsManifestSequenceRelation::Backward
    }
}

fn manifest_continuity_mode(session: &super::HlsSession, now_ms: u64) -> HlsManifestContinuityMode {
    if session.origin_seq_highwater.is_none() {
        return HlsManifestContinuityMode::RebaseAllowed;
    }
    match session.account_binding_protection(now_ms) {
        HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired => {
            HlsManifestContinuityMode::RebaseAllowed
        }
        HlsAccountBindingProtection::HardActive { .. } | HlsAccountBindingProtection::SoftActive { .. } => {
            HlsManifestContinuityMode::StrictContinuity
        }
    }
}

fn manifest_highwater_delta_within_window(delta: u64, allowed_forward_window: Option<u64>) -> bool {
    allowed_forward_window.is_none_or(|window| delta <= window.max(1))
}

fn manifest_quality_reject_reason(
    sequence_relation: HlsManifestSequenceRelation,
    previous_highwater: Option<u64>,
    origin_highwater: Option<u64>,
    allowed_forward_window: Option<u64>,
) -> Option<HlsManifestAcceptanceRejectReason> {
    match sequence_relation {
        HlsManifestSequenceRelation::NoOriginHighwater => {
            Some(HlsManifestAcceptanceRejectReason::MissingOriginHighwater)
        }
        HlsManifestSequenceRelation::ForwardTooFar => Some(HlsManifestAcceptanceRejectReason::ForwardTooFar {
            previous: previous_highwater.unwrap_or_default(),
            origin: origin_highwater.unwrap_or_default(),
            window: allowed_forward_window,
        }),
        HlsManifestSequenceRelation::Backward => Some(HlsManifestAcceptanceRejectReason::BackwardOutsideRollover {
            previous: previous_highwater.unwrap_or_default(),
            origin: origin_highwater.unwrap_or_default(),
            window: allowed_forward_window,
        }),
        HlsManifestSequenceRelation::NoPreviousHighwater
        | HlsManifestSequenceRelation::Rebase
        | HlsManifestSequenceRelation::Same
        | HlsManifestSequenceRelation::Next
        | HlsManifestSequenceRelation::PlausibleForward
        | HlsManifestSequenceRelation::RolloverCandidate => None,
    }
}

fn manifest_origin_quality_score(
    host_relation: HlsManifestOriginRelation,
    sequence_relation: HlsManifestSequenceRelation,
    rejected: bool,
) -> HlsManifestOriginQualityScore {
    if rejected {
        return HlsManifestOriginQualityScore::Rejected;
    }
    let same_host =
        matches!(host_relation, HlsManifestOriginRelation::Initial | HlsManifestOriginRelation::SameRedirectHost);
    match (same_host, sequence_relation) {
        (true, HlsManifestSequenceRelation::Next) => HlsManifestOriginQualityScore::SameHostNextSequence,
        (true, HlsManifestSequenceRelation::Rebase) => HlsManifestOriginQualityScore::SameHostRebase,
        (true, HlsManifestSequenceRelation::NoPreviousHighwater | HlsManifestSequenceRelation::PlausibleForward) => {
            HlsManifestOriginQualityScore::SameHostPlausibleForward
        }
        (true, HlsManifestSequenceRelation::RolloverCandidate) => {
            HlsManifestOriginQualityScore::SameHostRolloverCandidate
        }
        (true, HlsManifestSequenceRelation::Same) => HlsManifestOriginQualityScore::SameHostUnchanged,
        (false, HlsManifestSequenceRelation::Next) => HlsManifestOriginQualityScore::OtherHostNextSequence,
        (false, HlsManifestSequenceRelation::Rebase) => HlsManifestOriginQualityScore::OtherHostRebaseCandidate,
        (false, HlsManifestSequenceRelation::NoPreviousHighwater | HlsManifestSequenceRelation::PlausibleForward) => {
            HlsManifestOriginQualityScore::OtherHostPlausibleForward
        }
        (false, HlsManifestSequenceRelation::RolloverCandidate) => {
            HlsManifestOriginQualityScore::OtherHostRolloverCandidate
        }
        (false, HlsManifestSequenceRelation::Same) => HlsManifestOriginQualityScore::OtherHostUnchanged,
        (
            _,
            HlsManifestSequenceRelation::NoOriginHighwater
            | HlsManifestSequenceRelation::ForwardTooFar
            | HlsManifestSequenceRelation::Backward,
        ) => HlsManifestOriginQualityScore::Rejected,
    }
}

pub(crate) async fn log_hls_manifest_initial_selected(
    context: &HlsOriginManifestFetchContext,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' initial selected: host={} media-sequence={} highwater={} score={}",
        session_label,
        report.quality.effective_host.as_deref().unwrap_or("none"),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

async fn log_manifest_recovery_candidate_scored(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' candidate {} of {} scored: host={} media-sequence={} highwater={} score={}",
        session_label,
        candidate_index + 1,
        candidates,
        report.quality.effective_host.as_deref().unwrap_or("none"),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

async fn log_manifest_recovery_candidate_rejected(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    host: Option<&str>,
    highwater: Option<u64>,
    reason: &HlsManifestRejectLogReason,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' candidate {} of {} rejected: host={} highwater={} reason={}",
        session_label,
        candidate_index + 1,
        candidates,
        host.unwrap_or("none"),
        format_optional_highwater(highwater),
        reason.status_label()
    );
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ManifestRecoverySelectionLogPhase {
    Recovery,
    Burst,
}

impl ManifestRecoverySelectionLogPhase {
    pub(crate) const fn from_candidate_count(candidates: usize) -> Self {
        if candidates > 1 {
            Self::Burst
        } else {
            Self::Recovery
        }
    }

    pub(crate) const fn as_log_label(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::Burst => "burst",
        }
    }
}

async fn log_manifest_initial_attempt(context: &HlsOriginManifestFetchContext) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let input_source = context.origin_entry.to_input_source();
    debug!(
        "Manifest '{}' attempting URL attempt initial: {} reason=origin-refresh",
        session_label,
        safe_origin_log_value(input_source.url.as_str())
    );
}

async fn log_manifest_recovery_selected(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let phase = ManifestRecoverySelectionLogPhase::from_candidate_count(candidates);
    debug!(
        "Manifest '{}' {} selected candidate {} of {}: host={} media-sequence={} highwater={} score={}",
        session_label,
        phase.as_log_label(),
        candidate_index + 1,
        candidates,
        report.quality.effective_host.as_deref().unwrap_or("none"),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

pub(crate) fn format_optional_highwater(highwater: Option<u64>) -> String {
    highwater.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn recovery_burst_plan(context: &HlsOriginManifestFetchContext, attempt_index: usize) -> HlsManifestRecoveryBurstPlan {
    if attempt_index == 0 {
        context.manifest_recovery_burst.level.plan()
    } else {
        HlsManifestRecoveryBurstLevel::Off.plan()
    }
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

pub(crate) fn allowed_manifest_forward_window(
    session: &super::HlsSession,
    context: &HlsOriginManifestFetchContext,
    body: Option<&str>,
) -> Option<u64> {
    let timing = body.map(parse_manifest_timing);
    let target_duration_secs = timing
        .and_then(|timing| timing.target_duration_ms.and_then(|duration_ms| u32::try_from(duration_ms / 1_000).ok()));
    origin_highwater_policy_limit(
        request_hls_session_idle_timeout_secs_from_config(&context.app_config),
        target_duration_secs.or(session.target_duration),
    )
}

pub(crate) fn origin_highwater_policy_limit(
    session_idle_timeout_secs: u64,
    target_duration_secs: Option<u32>,
) -> Option<u64> {
    let target_duration_secs = u64::from(target_duration_secs.unwrap_or(DEFAULT_HLS_TARGET_DURATION_SECS));
    if target_duration_secs == 0 {
        return None;
    }
    Some(session_idle_timeout_secs.div_ceil(target_duration_secs))
}

fn origin_highwater_is_within_limit(origin_highwater: u64, origin_highwater_limit: Option<u64>) -> bool {
    origin_highwater_limit.is_some_and(|limit| origin_highwater <= limit)
}

pub(crate) fn manifest_origin_quality_from_candidate(
    candidate: Option<&super::HlsManifestHostSwitchCandidate>,
) -> HlsManifestOriginQuality {
    let (effective_host, origin_highwater, score) =
        candidate.map_or((None, None, HlsManifestOriginQualityScore::Rejected), |candidate| {
            (
                Some(candidate.host.clone()),
                candidate.highwater,
                manifest_origin_quality_score_from_rank(candidate.quality_score),
            )
        });
    HlsManifestOriginQuality {
        score,
        continuity_mode: HlsManifestContinuityMode::StrictContinuity,
        host_relation: HlsManifestOriginRelation::OtherRedirectHost,
        sequence_relation: HlsManifestSequenceRelation::NoOriginHighwater,
        effective_host,
        origin_highwater,
        previous_highwater: None,
        allowed_forward_window: None,
        should_increment_stall_counter: true,
        should_reset_stall_counter: false,
        requires_handoff_discontinuity: false,
        reject_reason: None,
    }
}

fn manifest_origin_quality_score_from_rank(rank: u16) -> HlsManifestOriginQualityScore {
    match rank {
        100 => HlsManifestOriginQualityScore::SameHostNextSequence,
        90 => HlsManifestOriginQualityScore::SameHostPlausibleForward,
        85 => HlsManifestOriginQualityScore::SameHostRebase,
        75 => HlsManifestOriginQualityScore::OtherHostNextSequence,
        65 => HlsManifestOriginQualityScore::OtherHostPlausibleForward,
        60 => HlsManifestOriginQualityScore::OtherHostRebaseCandidate,
        50 => HlsManifestOriginQualityScore::SameHostRolloverCandidate,
        35 => HlsManifestOriginQualityScore::OtherHostRolloverCandidate,
        20 => HlsManifestOriginQualityScore::SameHostUnchanged,
        10 => HlsManifestOriginQualityScore::OtherHostUnchanged,
        _ => HlsManifestOriginQualityScore::Rejected,
    }
}

pub(crate) fn manifest_host_switch_failure_threshold(session: &super::HlsSession, strip: &StripConfig) -> u32 {
    let effective_strip_segments = match strip.mode {
        HlsStripMode::Segments => {
            u32::try_from(strip.value).unwrap_or(u32::MAX.saturating_sub(HLS_MANIFEST_HOST_SWITCH_BASE_WINDOW_SEGMENTS))
        }
        HlsStripMode::Seconds => u32::try_from(session.initial_prefetch_gap_segments)
            .unwrap_or(u32::MAX.saturating_sub(HLS_MANIFEST_HOST_SWITCH_BASE_WINDOW_SEGMENTS)),
    };
    manifest_host_switch_failure_threshold_for_strip_segments(effective_strip_segments)
}

pub(crate) fn manifest_host_switch_failure_threshold_for_strip_segments(effective_strip_segments: u32) -> u32 {
    HLS_MANIFEST_HOST_SWITCH_BASE_WINDOW_SEGMENTS
        .saturating_add(effective_strip_segments)
        .saturating_div(2)
        .clamp(1, HLS_MANIFEST_HOST_SWITCH_MAX_FAILURE_THRESHOLD)
}

pub(crate) fn fetched_effective_manifest_host(fetched: &FetchedOriginManifest) -> Option<String> {
    if fetched.redirect_host.is_some() {
        return fetched.redirect_host.clone();
    }
    Url::parse(&fetched.resolved_request_url).ok().and_then(|url| url.host_str().map(str::to_string))
}

fn is_hls_retryable_initial_manifest_fetch_error(err: &OriginManifestFetchError) -> bool {
    matches!(
        err,
        OriginManifestFetchError::RetryableStatus(_, _)
            | OriginManifestFetchError::Request(_)
            | OriginManifestFetchError::Redirect(_)
            | OriginManifestFetchError::Timeout
            | OriginManifestFetchError::ContentDecoding { .. }
            | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_))
    )
}

fn is_hls_retryable_manifest_reject_fetch_error(err: &OriginManifestFetchError) -> bool {
    match err {
        OriginManifestFetchError::RetryableStatus(_, _)
        | OriginManifestFetchError::Request(_)
        | OriginManifestFetchError::Redirect(_)
        | OriginManifestFetchError::Timeout
        | OriginManifestFetchError::RetryExhausted
        | OriginManifestFetchError::ContentDecoding { .. }
        | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_)) => true,
        OriginManifestFetchError::PermanentStatus(_)
        | OriginManifestFetchError::NonRetryableStatus(_)
        | OriginManifestFetchError::ProviderUnavailable(_)
        | OriginManifestFetchError::ContentCoding(_)
        | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
        | OriginManifestFetchError::InvalidUtf8 { .. } => false,
    }
}

pub(crate) fn commit_error_to_fetch_error(err: &HlsManifestCommitError) -> OriginManifestFetchError {
    match err {
        HlsManifestCommitError::TimelineRejected { .. } | HlsManifestCommitError::RetryCurrentTarget => {
            OriginManifestFetchError::RetryExhausted
        }
    }
}

fn commit_error_to_retry_reason(err: &HlsManifestCommitError) -> HlsManifestRejectLogReason {
    match err {
        HlsManifestCommitError::TimelineRejected { reason } => reason.clone(),
        HlsManifestCommitError::RetryCurrentTarget => HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    }
}

pub(crate) fn next_committed_origin_highwater(
    current_highwater: Option<u64>,
    origin_highwater: u64,
    sequence_relation: HlsManifestSequenceRelation,
) -> u64 {
    match sequence_relation {
        HlsManifestSequenceRelation::NoPreviousHighwater
        | HlsManifestSequenceRelation::Rebase
        | HlsManifestSequenceRelation::Next
        | HlsManifestSequenceRelation::PlausibleForward
        | HlsManifestSequenceRelation::RolloverCandidate => origin_highwater,
        HlsManifestSequenceRelation::NoOriginHighwater
        | HlsManifestSequenceRelation::Same
        | HlsManifestSequenceRelation::ForwardTooFar
        | HlsManifestSequenceRelation::Backward => {
            current_highwater.map_or(origin_highwater, |current| current.max(origin_highwater))
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestRetryLogKind {
    InitialFetch,
    PinnedHostRecovery,
}

async fn log_manifest_retry_scheduled(
    context: &HlsOriginManifestFetchContext,
    retry_kind: ManifestRetryLogKind,
    attempt_index: usize,
    attempts: usize,
    delay_ms: u64,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    fetch_error: Option<&OriginManifestFetchError>,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let status = manifest_retry_status_label(retry_kind, reject_reason, fetch_error);
    warn!(
        "Manifest '{}' retry scheduled: status {} attempt {} of {} next_delay_ms={delay_ms}",
        session_label,
        status,
        attempt_index + 1,
        attempts
    );
}

fn manifest_retry_status_label(
    retry_kind: ManifestRetryLogKind,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    fetch_error: Option<&OriginManifestFetchError>,
) -> String {
    match (retry_kind, reject_reason, fetch_error) {
        (_, Some(reason), Some(err)) => format!("{} error={}", reason.status_label(), err.log_label()),
        (_, Some(reason), None) => reason.status_label(),
        (ManifestRetryLogKind::InitialFetch, None, Some(err)) => {
            format!("initial-fetch error={}", err.log_label())
        }
        (ManifestRetryLogKind::InitialFetch, None, None) => "initial-fetch".to_string(),
        (ManifestRetryLogKind::PinnedHostRecovery, None, Some(err)) => {
            format!("pinned-host-recovery error={}", err.log_label())
        }
        (ManifestRetryLogKind::PinnedHostRecovery, None, None) => "pinned-host-recovery".to_string(),
    }
}

async fn fetch_origin_manifest_once(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    provider_url_index: Option<usize>,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    if use_manual_redirects {
        fetch_origin_manifest_with_manual_redirects(
            entry_url,
            headers,
            no_redirect_client,
            provider_url_index,
            origin_manifest_timeout_ms,
        )
        .await
    } else {
        let mut request_headers = headers.clone();
        apply_outbound_content_coding_policy(&mut request_headers, OutboundContentCodingPolicy::Identity);
        let response = client
            .get(entry_url.clone())
            .headers(request_headers)
            .send()
            .await
            .map_err(|err| origin_manifest_fetch_error_from_reqwest_error(&err))?;
        response_to_fetched_manifest(response, provider_url_index, entry_url.clone(), origin_manifest_timeout_ms).await
    }
}

async fn fetch_origin_manifest_with_manual_redirects(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    provider_url_index: Option<usize>,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers.clone();
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let mut request_headers = current_headers.clone();
        apply_outbound_content_coding_policy(&mut request_headers, OutboundContentCodingPolicy::Identity);
        let response = client
            .get(current_url.clone())
            .headers(request_headers)
            .send()
            .await
            .map_err(|err| origin_manifest_fetch_error_from_reqwest_error(&err))?;
        if !response.status().is_redirection() {
            return response_to_fetched_manifest(
                response,
                provider_url_index,
                entry_url.clone(),
                origin_manifest_timeout_ms,
            )
            .await;
        }
        if remaining_redirects == 0 {
            return Err(OriginManifestFetchError::Redirect("too many redirects".to_string()));
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| OriginManifestFetchError::Redirect("redirect missing location".to_string()))?;
        let next_url = response_url
            .join(location)
            .or_else(|_| Url::parse(location))
            .map_err(|_| OriginManifestFetchError::Redirect("redirect location invalid".to_string()))?;

        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

async fn fetch_hls_origin_manifest_recovery_direct_target(
    context: &HlsOriginManifestFetchContext,
    target_url: &Url,
    provider_url_index: Option<usize>,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    log_context: ManifestRecoveryAttemptLogContext,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let reason =
        reject_reason.map_or_else(|| "pinned-host-recovery".to_string(), HlsManifestRejectLogReason::status_label);
    if log_context.candidates > 1 {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {} candidate {} of {}: {} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            log_context.candidate_index + 1,
            log_context.candidates,
            safe_origin_log_value(target_url.as_str()),
            reason
        );
    } else {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {}: {} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            safe_origin_log_value(target_url.as_str()),
            reason
        );
    }
    timeout(
        Duration::from_millis(context.origin_manifest_timeout_ms.max(1)),
        fetch_origin_manifest_once(
            target_url,
            &context.headers,
            &context.client,
            &context.no_redirect_client,
            context.use_manual_redirects,
            provider_url_index,
            context.origin_manifest_timeout_ms,
        ),
    )
    .await
    .map_err(|_| OriginManifestFetchError::Timeout)?
}

#[derive(Debug, Clone, Copy)]
struct ManifestRecoveryAttemptLogContext {
    attempt_index: usize,
    attempts: usize,
    candidate_index: usize,
    candidates: usize,
}

impl ManifestRecoveryAttemptLogContext {
    const fn single(attempt_index: usize, attempts: usize) -> Self {
        Self { attempt_index, attempts, candidate_index: 0, candidates: 1 }
    }
}

async fn response_to_fetched_manifest(
    response: reqwest::Response,
    provider_url_index: Option<usize>,
    resolved_request_url: Url,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let status = response.status();
    debug!(
        "HLS origin manifest response received: request_target={} final_target={} status={}",
        safe_origin_log_value(resolved_request_url.as_str()),
        safe_origin_log_value(response.url().as_str()),
        status.as_u16()
    );
    match classify_origin_manifest_status(status) {
        OriginManifestStatusClass::Success => {
            let (decoded, body) = read_origin_manifest_body(response, origin_manifest_timeout_ms).await?;
            let redirect_host = hls_manifest_redirect_host(&resolved_request_url, &decoded.final_url);
            let provider_session_headers = extract_hls_provider_session_header_map(&decoded.headers);
            Ok(FetchedOriginManifest {
                body,
                final_manifest_url: decoded.final_url.to_string(),
                resolved_request_url: resolved_request_url.to_string(),
                redirect_host,
                provider_url_index,
                provider_session_headers,
                status: decoded.status,
                attempts: 1,
            })
        }
        OriginManifestStatusClass::Retryable => {
            Err(OriginManifestFetchError::RetryableStatus(status, retry_after_delay_ms(response.headers())))
        }
        OriginManifestStatusClass::PermanentFailure => Err(OriginManifestFetchError::PermanentStatus(status)),
        OriginManifestStatusClass::NonRetryableFailure => Err(OriginManifestFetchError::NonRetryableStatus(status)),
    }
}

async fn read_origin_manifest_body(
    response: reqwest::Response,
    origin_manifest_timeout_ms: u64,
) -> Result<(crate::utils::content_coding::DecodedHttpResponse, String), OriginManifestFetchError> {
    let read = async move {
        let mut decoded =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOrKnownHlsManifestMagic)
                .await
                .map_err(origin_manifest_content_coding_error)?;
        if let Some(observation) = decoded.content_coding_observation() {
            log_hls_origin_content_coding(
                observation,
                HlsOriginContentCodingObjectKind::Manifest,
                false,
                HlsOriginContentCodingSource::Shared,
            );
        }
        let body = read_utf8_limited(&mut decoded.body, MAX_HLS_MANIFEST_BYTES)
            .await
            .map_err(origin_manifest_body_read_error)?;
        Ok((decoded, body))
    };
    timeout(Duration::from_millis(origin_manifest_timeout_ms.max(1)), read)
        .await
        .map_err(|_| OriginManifestFetchError::Timeout)?
}

fn origin_manifest_content_coding_error(error: ContentCodingError) -> OriginManifestFetchError {
    match error {
        ContentCodingError::PrefixRead(io_error) => {
            if let Some(decoding_error) = content_decoding_error_from_io(&io_error) {
                OriginManifestFetchError::ContentDecoding { coding: decoding_error.coding }
            } else if io_error.kind() == io::ErrorKind::TimedOut {
                OriginManifestFetchError::Timeout
            } else if is_http_body_transport_error(&io_error) {
                origin_manifest_fetch_error_from_io_error(&io_error)
            } else {
                OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(io_error))
            }
        }
        error => OriginManifestFetchError::ContentCoding(error),
    }
}

fn origin_manifest_body_read_error(error: ContentBodyReadError) -> OriginManifestFetchError {
    match error {
        ContentBodyReadError::LimitExceeded { limit } => OriginManifestFetchError::DecodedBodyLimitExceeded { limit },
        ContentBodyReadError::InvalidUtf8 { valid_up_to, error_len } => {
            OriginManifestFetchError::InvalidUtf8 { valid_up_to, error_len }
        }
        ContentBodyReadError::Io(io_error) => {
            if let Some(decoding_error) = content_decoding_error_from_io(&io_error) {
                OriginManifestFetchError::ContentDecoding { coding: decoding_error.coding }
            } else {
                origin_manifest_fetch_error_from_io_error(&io_error)
            }
        }
    }
}

pub(crate) fn hls_manifest_redirect_host(resolved_request_url: &Url, final_url: &Url) -> Option<String> {
    let final_host = final_url.host_str()?;
    (resolved_request_url.host_str() != Some(final_host)).then(|| final_host.to_string())
}

pub(crate) fn resolved_hls_manifest_request_url_from_input(
    input_source: &InputSource,
    provider_url_index: Option<usize>,
    fallback_url: &Url,
) -> Url {
    let fallback = || Url::parse(input_source.url.as_str()).unwrap_or_else(|_| fallback_url.clone());
    let (Some(provider), Some(provider_url_index)) = (input_source.get_provider(), provider_url_index) else {
        return fallback();
    };
    match resolve_provider_scheme_url_with_provider_index(
        input_source.url.as_str(),
        Some(Arc::clone(provider)),
        provider_url_index,
    ) {
        Ok((_provider, resolved_url)) => Url::parse(resolved_url.as_ref()).unwrap_or_else(|err| {
            debug!(
                "HLS provider URL resolution returned invalid URL: error={} origin={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                safe_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }),
        Err(err) => {
            debug!(
                "HLS provider URL resolution failed: error={} origin={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                safe_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }
    }
}

fn origin_manifest_fetch_error_from_request_error(err: &impl ToString) -> OriginManifestFetchError {
    let message = sanitize_sensitive_info(err.to_string().as_str()).to_string();
    let Some(status) = request_failed_status_from_message(&message) else {
        return OriginManifestFetchError::Request(message);
    };
    match classify_origin_manifest_status(status) {
        OriginManifestStatusClass::Success => OriginManifestFetchError::Request(message),
        OriginManifestStatusClass::Retryable => OriginManifestFetchError::RetryableStatus(status, None),
        OriginManifestStatusClass::PermanentFailure => OriginManifestFetchError::PermanentStatus(status),
        OriginManifestStatusClass::NonRetryableFailure => OriginManifestFetchError::NonRetryableStatus(status),
    }
}

fn origin_manifest_fetch_error_from_reqwest_error(err: &reqwest::Error) -> OriginManifestFetchError {
    if err.is_timeout() {
        OriginManifestFetchError::Timeout
    } else {
        origin_manifest_fetch_error_from_request_error(err)
    }
}

fn origin_manifest_fetch_error_from_io_error(err: &io::Error) -> OriginManifestFetchError {
    if err.kind() == io::ErrorKind::TimedOut {
        OriginManifestFetchError::Timeout
    } else {
        origin_manifest_fetch_error_from_request_error(err)
    }
}

fn request_failed_status_from_message(message: &str) -> Option<StatusCode> {
    let marker = "Request failed (";
    let status_start = message.find(marker)?.checked_add(marker.len())?;
    let status_text = message.get(status_start..)?.split(')').next()?;
    let status_code = status_text.split_whitespace().next()?.parse::<u16>().ok()?;
    StatusCode::from_u16(status_code).ok()
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    super::scrub_hls_origin_headers(headers, None);
}

pub fn retry_after_delay_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn refresh_from_live_hls_entrypoint_with_retries(
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
            let jitter =
                if retry_policy.jitter_max_ms == 0 { 0 } else { fastrand::u64(0..=retry_policy.jitter_max_ms) };
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
            Ok(Err(OriginManifestFetchError::RetryExhausted)) => return Err(OriginManifestFetchError::RetryExhausted),
            Ok(Err(OriginManifestFetchError::ProviderUnavailable(kind))) => {
                return Err(OriginManifestFetchError::ProviderUnavailable(kind));
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
                err @ (OriginManifestFetchError::ContentCoding(_)
                | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
                | OriginManifestFetchError::InvalidUtf8 { .. }),
            ))
            | Err(err) => return Err(err),
        }
    }

    Err(OriginManifestFetchError::RetryExhausted)
}

#[cfg(test)]
fn log_origin_refresh_retry_scheduled(
    origin_entry: &LiveHlsOriginEntry,
    attempt_index: usize,
    delay_ms: u64,
    detail: impl AsRef<str>,
) {
    warn!(
        "HLS origin manifest refresh retry scheduled: origin_entry={} attempt={} {} delay_ms={delay_ms}",
        safe_origin_log_value(origin_entry.url().as_str()),
        attempt_index + 1,
        detail.as_ref()
    );
}

#[cfg(test)]
mod tests {
    use super::{
        fetch_origin_manifest_once, origin_manifest_content_coding_error, origin_manifest_fetch_error_from_io_error,
        origin_manifest_fetch_error_from_request_error, request_failed_status_from_message, ManifestRetryLogKind,
        OriginManifestFetchError, RetryPolicy, MAX_HLS_MANIFEST_BYTES,
    };
    use crate::utils::content_coding::{ContentCoding, ContentCodingError};
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use flate2::{
        write::{DeflateEncoder, GzEncoder},
        Compression,
    };
    use reqwest::{redirect::Policy, Client};
    use std::{io, io::Write, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };
    use url::Url;

    const TEST_MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nsegment.ts\n";

    struct TestOriginResponse {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        body_delay: Duration,
    }

    impl TestOriginResponse {
        fn ok(body: Vec<u8>) -> Self {
            Self { status: "200 OK", headers: Vec::new(), body, body_delay: Duration::ZERO }
        }

        fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
            self.headers.push((name, value.into()));
            self
        }
    }

    async fn spawn_test_origin(response: TestOriginResponse) -> (Url, oneshot::Receiver<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind test origin");
        let address = listener.local_addr().expect("test origin address");
        let (request_sender, request_receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.expect("read test request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let _ = request_sender.send(String::from_utf8_lossy(&request).into_owned());

            let mut response_head = format!(
                "HTTP/1.1 {}\r\nConnection: close\r\nContent-Length: {}\r\n",
                response.status,
                response.body.len()
            );
            for (name, value) in response.headers {
                response_head.push_str(name);
                response_head.push_str(": ");
                response_head.push_str(&value);
                response_head.push_str("\r\n");
            }
            response_head.push_str("\r\n");
            socket.write_all(response_head.as_bytes()).await.expect("write test response headers");
            if !response.body_delay.is_zero() {
                tokio::time::sleep(response.body_delay).await;
            }
            let _ = socket.write_all(&response.body).await;
        });
        (Url::parse(&format!("http://{address}/manifest.m3u8")).expect("test origin URL"), request_receiver)
    }

    fn gzip(input: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).expect("gzip input");
        encoder.finish().expect("finish gzip")
    }

    fn raw_deflate(input: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).expect("raw-deflate input");
        encoder.finish().expect("finish raw-deflate")
    }

    fn brotli(input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
            encoder.write_all(input).expect("brotli input");
        }
        output
    }

    async fn zstd(input: &[u8]) -> Vec<u8> {
        let (writer, mut reader) = tokio::io::duplex(64 * 1024);
        let input = input.to_vec();
        let encoder_task = tokio::spawn(async move {
            let mut encoder = async_compression::tokio::write::ZstdEncoder::new(writer);
            encoder.write_all(&input).await.expect("zstd input");
            encoder.shutdown().await.expect("finish zstd");
        });
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.expect("read zstd output");
        encoder_task.await.expect("join zstd encoder");
        output
    }

    fn captured_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().skip(1).find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn test_clients() -> (Client, Client) {
        let client = Client::builder().build().expect("test client");
        let no_redirect_client = Client::builder().redirect(Policy::none()).build().expect("no-redirect client");
        (client, no_redirect_client)
    }

    #[test]
    fn request_failed_status_is_extracted_from_global_provider_policy_error() {
        assert_eq!(
            request_failed_status_from_message(
                "Request failed (407 Proxy Authentication Required): provider://demo/live/u/p/1.m3u8",
            ),
            Some(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
        );
    }

    #[test]
    fn initial_manifest_retry_delay_uses_next_slot_bounded_jitter_and_retry_after_override() {
        let retry_policy = RetryPolicy { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 25 };

        assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, None, 17), 117);
        assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, None, 100), 125);
        assert_eq!(super::next_retry_delay_ms(&retry_policy, 1, None, 10), 260);
        assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, Some(3_000), 25), 3_000);
    }

    #[test]
    fn initial_manifest_retry_status_is_not_labeled_as_recovery() {
        let error = OriginManifestFetchError::Timeout;

        let initial = super::manifest_retry_status_label(ManifestRetryLogKind::InitialFetch, None, Some(&error));
        let recovery = super::manifest_retry_status_label(ManifestRetryLogKind::PinnedHostRecovery, None, Some(&error));

        assert!(initial.starts_with("initial-fetch error="));
        assert!(!initial.contains("pinned-host-recovery"));
        assert!(recovery.starts_with("pinned-host-recovery error="));
    }

    #[test]
    fn manifest_content_coding_log_labels_never_include_origin_controlled_details() {
        let unsupported =
            OriginManifestFetchError::ContentCoding(ContentCodingError::Unsupported("signed-token-secret".to_string()));
        assert_eq!(unsupported.log_label(), "content_coding class=unsupported");
        assert!(!unsupported.log_label().contains("signed-token-secret"));

        let decoding = OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Brotli };
        assert_eq!(decoding.log_label(), "content_decoding coding=br");
    }

    #[test]
    fn request_failed_407_maps_to_retryable_manifest_status() {
        let err = origin_manifest_fetch_error_from_request_error(
            &"Request failed (407 Proxy Authentication Required): provider://demo/live/u/p/1.m3u8",
        );
        assert!(matches!(
            err,
            OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, None)
        ));
    }

    #[test]
    fn request_failed_retryable_statuses_map_to_retryable_manifest_status() {
        for (message, expected) in [
            ("Request failed (429 Too Many Requests): http://example.test/live.m3u8", StatusCode::TOO_MANY_REQUESTS),
            (
                "Request failed (500 Internal Server Error): http://example.test/live.m3u8",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let err = origin_manifest_fetch_error_from_request_error(&message);
            assert!(matches!(err, OriginManifestFetchError::RetryableStatus(status, None) if status == expected));
        }
    }

    #[test]
    fn request_failed_404_maps_to_permanent_manifest_status() {
        let err = origin_manifest_fetch_error_from_request_error(
            &"Request failed (404 Not Found): http://example.test/live.m3u8",
        );
        assert!(matches!(err, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
    }

    #[test]
    fn transport_error_without_http_status_stays_request_error() {
        let err = origin_manifest_fetch_error_from_request_error(&"error sending request for url");
        assert!(
            matches!(err, OriginManifestFetchError::Request(message) if message == "error sending request for url")
        );
    }

    #[test]
    fn io_timeout_maps_to_structured_manifest_timeout() {
        let error = io::Error::new(io::ErrorKind::TimedOut, "origin body timed out");
        assert!(matches!(origin_manifest_fetch_error_from_io_error(&error), OriginManifestFetchError::Timeout));
        let prefix_timeout =
            ContentCodingError::PrefixRead(io::Error::new(io::ErrorKind::TimedOut, "origin prefix timed out"));
        assert!(matches!(origin_manifest_content_coding_error(prefix_timeout), OriginManifestFetchError::Timeout));
    }

    #[tokio::test]
    async fn shared_manifest_decodes_supported_origin_content_codings_and_keeps_provider_cookie() {
        let encoded_bodies = [
            ("gzip", gzip(TEST_MANIFEST)),
            ("deflate", raw_deflate(TEST_MANIFEST)),
            ("br", brotli(TEST_MANIFEST)),
            ("zstd", zstd(TEST_MANIFEST).await),
        ];

        for (coding, encoded_body) in encoded_bodies {
            let response = TestOriginResponse::ok(encoded_body)
                .with_header("Content-Encoding", coding)
                .with_header("Set-Cookie", format!("sid={coding}; Path=/"));
            let (entry_url, request_receiver) = spawn_test_origin(response).await;
            let (client, no_redirect_client) = test_clients();
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));

            let fetched =
                fetch_origin_manifest_once(&entry_url, &headers, &client, &no_redirect_client, false, None, 1_000)
                    .await
                    .unwrap_or_else(|error| panic!("decode {coding} manifest: {error:?}"));

            assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST, "coding={coding}");
            assert_eq!(
                fetched
                    .provider_session_headers
                    .get(header::COOKIE)
                    .expect("provider cookie")
                    .to_str()
                    .expect("provider cookie text"),
                format!("sid={coding}"),
                "coding={coding}"
            );
            let request = request_receiver.await.expect("captured request");
            assert_eq!(captured_header(&request, "accept-encoding"), Some("identity"), "coding={coding}");
        }
    }

    #[tokio::test]
    async fn shared_manifest_magic_sniffs_gzip_only_in_manifest_mode() {
        let (entry_url, _) = spawn_test_origin(TestOriginResponse::ok(gzip(TEST_MANIFEST))).await;
        let (client, no_redirect_client) = test_clients();

        let fetched =
            fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 1_000)
                .await
                .expect("decode magic-sniffed gzip manifest");

        assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST);
    }

    #[tokio::test]
    async fn direct_manual_redirect_reapplies_identity_after_cross_origin_scrubbing() {
        let (target_url, target_request_receiver) =
            spawn_test_origin(TestOriginResponse::ok(TEST_MANIFEST.to_vec())).await;
        let redirect_response = TestOriginResponse {
            status: "302 Found",
            headers: vec![("Location", target_url.to_string())],
            body: Vec::new(),
            body_delay: Duration::ZERO,
        };
        let (entry_url, redirect_request_receiver) = spawn_test_origin(redirect_response).await;
        let (client, no_redirect_client) = test_clients();
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));

        let fetched = fetch_origin_manifest_once(&entry_url, &headers, &client, &no_redirect_client, true, None, 1_000)
            .await
            .expect("follow manual manifest redirect");
        assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST);

        let redirect_request = redirect_request_receiver.await.expect("captured redirect request");
        let target_request = target_request_receiver.await.expect("captured target request");
        assert_eq!(captured_header(&redirect_request, "accept-encoding"), Some("identity"));
        assert_eq!(captured_header(&target_request, "accept-encoding"), Some("identity"));
        assert!(captured_header(&target_request, "authorization").is_none());
        assert!(captured_header(&target_request, "cookie").is_none());
    }

    #[tokio::test]
    async fn shared_manifest_limit_applies_after_decompression() {
        let decoded_body = vec![b'x'; MAX_HLS_MANIFEST_BYTES + 1];
        let response = TestOriginResponse::ok(gzip(&decoded_body)).with_header("Content-Encoding", "gzip");
        let (entry_url, _) = spawn_test_origin(response).await;
        let (client, no_redirect_client) = test_clients();

        let error =
            fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 2_000)
                .await
                .expect_err("decoded manifest above limit must fail");

        assert!(matches!(
            error,
            OriginManifestFetchError::DecodedBodyLimitExceeded { limit } if limit == MAX_HLS_MANIFEST_BYTES
        ));
    }

    #[tokio::test]
    async fn shared_manifest_deadline_includes_magic_prefix_read() {
        let response = TestOriginResponse {
            body_delay: Duration::from_millis(100),
            ..TestOriginResponse::ok(TEST_MANIFEST.to_vec())
        };
        let (entry_url, _) = spawn_test_origin(response).await;
        let (client, no_redirect_client) = test_clients();

        let error =
            fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 10)
                .await
                .expect_err("prefix read must honor manifest deadline");

        assert!(matches!(error, OriginManifestFetchError::Timeout));
    }

    #[tokio::test]
    async fn shared_manifest_encoded_body_timeout_stays_structured_timeout() {
        let response = TestOriginResponse {
            body_delay: Duration::from_millis(100),
            ..TestOriginResponse::ok(gzip(TEST_MANIFEST)).with_header("Content-Encoding", "gzip")
        };
        let (entry_url, _) = spawn_test_origin(response).await;
        let (client, no_redirect_client) = test_clients();

        let error =
            fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 10)
                .await
                .expect_err("encoded body read must honor manifest deadline");

        assert!(matches!(error, OriginManifestFetchError::Timeout));
    }

    #[tokio::test]
    async fn shared_manifest_distinguishes_invalid_utf8_from_decoder_failure() {
        let (invalid_utf8_url, _) = spawn_test_origin(TestOriginResponse::ok(vec![0xff])).await;
        let corrupt_gzip = TestOriginResponse::ok(vec![0x1f, 0x8b, 0x08, 0x00]).with_header("Content-Encoding", "gzip");
        let (corrupt_gzip_url, _) = spawn_test_origin(corrupt_gzip).await;
        let (client, no_redirect_client) = test_clients();

        let invalid_utf8 = fetch_origin_manifest_once(
            &invalid_utf8_url,
            &HeaderMap::new(),
            &client,
            &no_redirect_client,
            false,
            None,
            1_000,
        )
        .await
        .expect_err("invalid UTF-8 must fail");
        let decoder_failure = fetch_origin_manifest_once(
            &corrupt_gzip_url,
            &HeaderMap::new(),
            &client,
            &no_redirect_client,
            false,
            None,
            1_000,
        )
        .await
        .expect_err("corrupt gzip must fail");

        assert!(matches!(invalid_utf8, OriginManifestFetchError::InvalidUtf8 { .. }));
        assert!(matches!(decoder_failure, OriginManifestFetchError::ContentDecoding { .. }));
    }

    #[tokio::test]
    async fn shared_manifest_rejects_encoded_partial_content() {
        let response = TestOriginResponse {
            status: "206 Partial Content",
            ..TestOriginResponse::ok(gzip(TEST_MANIFEST)).with_header("Content-Encoding", "gzip")
        };
        let (entry_url, _) = spawn_test_origin(response).await;
        let (client, no_redirect_client) = test_clients();

        let error =
            fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 1_000)
                .await
                .expect_err("encoded partial manifest must fail");

        assert!(matches!(error, OriginManifestFetchError::ContentCoding(ContentCodingError::EncodedPartialContent)));
    }
}
