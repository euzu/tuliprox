use super::{
    deterministic_conflict::{
        HlsDeterministicConflictFingerprint, HlsDeterministicConflictSegmentFingerprint,
        HlsDeterministicTimelineConflict,
    },
    extract_hls_provider_session_header_map, log_hls_origin_content_coding,
    manifest_acceptance::{
        classify_host_local_sequence, classify_reduced_retry_landscape, evaluate_manifest_acceptance,
        held_alternative_after_burst, manifest_acceptance_landscape, HlsAlternativeOriginCohort,
        HlsCandidateHostRelation, HlsCommittedContentAnchorEvidence, HlsCrossHostAcceptanceEvidence,
        HlsEmergencyAcceptanceEvidence, HlsEmergencyLiveHandoffCompatibility, HlsHostLocalSequenceRelation,
        HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceGeneration, HlsManifestAcceptanceInput,
        HlsManifestAcceptanceLandscape, HlsManifestAcceptanceState, HlsManifestAcceptanceTrigger,
        HlsManifestCandidateObservation, HlsManifestCommitKind, HlsManifestCommitPlan, HlsManifestSegmentFingerprint,
        HlsDeterministicConflictReceipt, HlsManifestRecoveryCandidateIdentity, HlsManifestTimelineFingerprint,
        HlsRecoveryWorkloadBindingUpdate,
        HlsReducedRetryLandscapeChange, HlsResourceTimelineEvidence, HlsSwitchSegmentReadiness,
        HlsTerminalAlternativeCompatibility,
        HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH, HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT,
        HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    recovery_timing::{
        HlsAcceptanceDeadlineMs, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput,
        HlsAcceptanceEpisodeTimingSeed, HlsRecoveryTimingPolicy, HlsRecoveryWorkloadEnvelope,
        HlsTerminalMediaPreparationState, HlsTransitionMarginMs,
    },
    resource_identity::{HlsMediaResourceIdentity, HlsMediaResourceSemanticKey},
    timeline::HlsResourceReplayDecision,
    hls_origin_log_value, safe_session_key, HlsAccountBindingProtection, HlsBoundAccountAcquireErrorKind,
    HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource, HlsSessionHandle, HlsSessionMode, TimelineMapError,
};
use crate::{
    model::{
        resolve_provider_scheme_url_with_provider_index, AppConfig, ConfigProvider, HlsManifestRecoveryBurstConfig,
        InputSource,
    },
    processing::parser::hls::origin_manifest::{
        parse_manifest_timing, parse_origin_manifest_timeline, parse_origin_media_manifest, OriginManifestParseOutcome,
        ParsedOriginManifest, ParsedOriginManifestTimeline,
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
use sha2::{Digest, Sha256};
use shared::{
    model::{HlsManifestRecoveryBurstLevel, HlsManifestRecoveryBurstPlan, InputFetchMethod},
    utils::sanitize_sensitive_info,
};
use std::{
    collections::HashMap,
    fmt::{self, Write as _},
    future::Future,
    io,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::timeout};
use url::Url;

const DEFAULT_HLS_TARGET_DURATION_SECS: u32 = 15;
const HLS_MANIFEST_HOST_SWITCH_BASE_WINDOW_SEGMENTS: u32 = 3;

use super::MAX_MANUAL_REDIRECTS;
const HLS_MANIFEST_HOST_SWITCH_MAX_FAILURE_THRESHOLD: u32 = 5;
const DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS: u64 = 300;
const HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT: usize = 64;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsManifestRecoveryUnavailableReason {
    NoEstablishedBindingAfterResponse,
    BindingSuperseded,
}

impl fmt::Display for HlsManifestRecoveryUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoEstablishedBindingAfterResponse => "no established binding after origin response",
            Self::BindingSuperseded => "manifest origin binding superseded",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OriginManifestFetchError {
    #[error("origin manifest returned permanent status {0}")]
    PermanentStatus(StatusCode),
    #[error("origin manifest returned retryable status {0}, retry_after_ms={1:?}")]
    RetryableStatus(StatusCode, Option<u64>),
    #[error("origin manifest retry attempts exhausted")]
    RetryExhausted,
    #[error("origin manifest recovery unavailable: {reason}")]
    RecoveryUnavailable { reason: HlsManifestRecoveryUnavailableReason },
    #[error("origin manifest has a deterministic timeline conflict")]
    DeterministicTimelineConflict(Box<HlsDeterministicTimelineConflict>),
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
            Self::RecoveryUnavailable { reason } => match reason {
                HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse => {
                    "recovery_unavailable_after_response".to_string()
                }
                HlsManifestRecoveryUnavailableReason::BindingSuperseded => {
                    "recovery_binding_superseded".to_string()
                }
            },
            Self::DeterministicTimelineConflict(conflict) => format!(
                "deterministic_timeline_conflict previous_proxy_tail={} existing_proxy_seq={} candidate_position={} candidate_origin_seq={} repeated_resource={} decision={}",
                format_optional_highwater(conflict.previous_proxy_tail),
                conflict.existing_proxy_seq,
                conflict.candidate_position,
                conflict.candidate_origin_seq,
                format_resource_identity_token(conflict.diagnostic_resource_token()),
                conflict.decision.as_log_value()
            ),
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

fn format_resource_identity_token(resource_identity: [u8; 8]) -> String {
    let mut token = String::with_capacity(resource_identity.len().saturating_mul(2));
    for byte in resource_identity {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[derive(Debug)]
pub(crate) enum HlsManifestCommitError {
    TimelineRejected { reason: HlsManifestRejectLogReason },
    RetryCurrentTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HlsManifestAcceptanceRejectReason {
    MissingOriginHighwater,
    ForwardTooFar { previous: u64, origin: u64, window: Option<u64> },
    BackwardOutsideRollover { previous: u64, origin: u64, window: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HlsManifestRejectLogReason {
    MissingOriginHighwater,
    ForwardTooFar { previous: u64, origin: u64, window: Option<u64> },
    BackwardOutsideRollover { previous: u64, origin: u64, window: Option<u64> },
    PinnedHostRecoveryRejected,
    UnsupportedSegmentExtension,
    UnsupportedMapExtension,
    ProxySequenceOverflow,
    ProxyMapIdOverflow,
    MissingKeyResource,
    PublishedResourceReplay {
        previous_proxy_tail: Option<u64>,
        existing_proxy_seq: u64,
        candidate_position: usize,
        candidate_origin_seq: u64,
        resource_key: HlsMediaResourceSemanticKey,
        decision: HlsResourceReplayDecision,
    },
    OriginSequenceResourceConflict { existing_proxy_seq: u64, candidate_origin_seq: u64 },
    SwitchResourceUnavailable,
    SwitchEncryptionKeyNotReady,
    SwitchMapResetUnsupported,
    CriticalHandoffLockContentionExhausted,
    StagedSwitchInvalidated,
    MalformedTransientTimeline,
}

impl HlsManifestRejectLogReason {
    pub(crate) fn status_label(&self) -> String {
        match self {
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
            Self::MissingKeyResource => "missing-key-resource".to_string(),
            Self::PublishedResourceReplay {
                previous_proxy_tail,
                existing_proxy_seq,
                candidate_position,
                candidate_origin_seq,
                resource_key,
                decision,
            } => {
                let resource_identity = format_resource_identity_token(resource_key.diagnostic_token());
                format!(
                    "published-resource-replay previous_proxy_tail={} existing_proxy_seq={existing_proxy_seq} candidate_position={candidate_position} candidate_origin_seq={candidate_origin_seq} repeated_resource={resource_identity} decision={}",
                    format_optional_highwater(*previous_proxy_tail),
                    decision.as_log_value()
                )
            }
            Self::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq } => {
                format!(
                    "origin-sequence-resource-conflict existing_proxy_seq={existing_proxy_seq} candidate_origin_seq={candidate_origin_seq}"
                )
            }
            Self::SwitchResourceUnavailable => "switch-resource-unavailable".to_string(),
            Self::SwitchEncryptionKeyNotReady => "switch-encryption-key-not-ready".to_string(),
            Self::SwitchMapResetUnsupported => "switch-map-reset-unsupported".to_string(),
            Self::CriticalHandoffLockContentionExhausted => "critical-handoff-lock-contention-exhausted".to_string(),
            Self::StagedSwitchInvalidated => "staged-switch-invalidated".to_string(),
            Self::MalformedTransientTimeline => "malformed-transient-timeline".to_string(),
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
            } => {
                Self::PublishedResourceReplay {
                    previous_proxy_tail,
                    existing_proxy_seq,
                    candidate_position,
                    candidate_origin_seq,
                    resource_key,
                    decision,
                }
            }
            TimelineMapError::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq } => {
                Self::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsManifestCommitAcceptanceMode {
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
    pub(crate) candidate_requests: usize,
    pub(crate) selection: HlsManifestFetchSelection,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum HlsManifestFetchSelection {
    Initial,
    Recovery,
    Burst,
}

impl HlsManifestFetchSelection {
    pub(crate) const fn as_log_value(self) -> &'static str {
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
    pub(crate) fn with_attempts(mut self, attempts: usize) -> Self {
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
        binding: &'a HlsManifestOriginBinding,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    ) -> Self {
        Self {
            context,
            mode: HlsOriginManifestFetchMode::RecoveryDirectTarget { binding, reason, log_context },
        }
    }
}

enum HlsManifestRecoveryAttemptError<T> {
    Fetch(OriginManifestFetchError),
    Rejected(HlsManifestRejectLogReason),
    Requalified,
    Committed(T),
}

#[derive(Debug)]
struct HlsManifestRecoveryCandidate {
    candidate_index: usize,
    fetch_elapsed_ms: u64,
    fetched: FetchedOriginManifest,
    report: HlsManifestRecoveryCandidateScoreReport,
}

struct HlsManifestRecoveryBurstCollection {
    fetched_candidates: Vec<HlsManifestRecoveryCandidate>,
    completed_candidates: usize,
    last_fetch_error: Option<OriginManifestFetchError>,
    last_reject_reason: Option<HlsManifestRejectLogReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsManifestRecoveryCandidateScoreReport {
    pub(crate) media_sequence: u64,
    pub(crate) quality: HlsManifestOriginQuality,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum HlsManifestOriginQualityScore {
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
    pub(crate) host_relation: HlsManifestOriginRelation,
    pub(crate) sequence_relation: HlsManifestSequenceRelation,
    pub(crate) effective_host: Option<String>,
    pub(crate) origin_highwater: Option<u64>,
    pub(crate) previous_highwater: Option<u64>,
    pub(crate) allowed_forward_window: Option<u64>,
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
    binding: HlsManifestOriginBinding,
    mut reject_reason: Option<HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<HlsDeterministicTimelineConflict>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    mut commit: C,
) -> Result<T, OriginManifestFetchError>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    if !trigger.starts_episode() {
        return Err(OriginManifestFetchError::RetryExhausted);
    }
    let attempts = context.retry_policy.attempt_count();
    let mut last_error = OriginManifestFetchError::RetryExhausted;
    let mut next_attempt_is_full_plan = true;
    let mut requalifications = 0_u8;
    let mut attempt_index = 0_usize;
    let mut attempt_limit = attempts;
    let mut completed_candidate_requests = 0_usize;
    while attempt_index < attempt_limit {
        // A materially changed landscape is requalified immediately. Once
        // authorized, that new generation must still receive its complete
        // configured burst rather than losing candidates to a retry delay.
        let delay_ms = if attempt_index > 0 && next_attempt_is_full_plan {
            0
        } else {
            let jitter = if context.retry_policy.jitter_max_ms == 0 {
                0
            } else {
                fastrand::u64(0..=context.retry_policy.jitter_max_ms)
            };
            context.retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        };
        let acceptance_deadline = current_acceptance_deadline(context).await;
        if !acceptance_attempt_may_start(
            next_attempt_is_full_plan,
            current_time_millis(),
            delay_ms,
            acceptance_deadline,
        ) {
            return Err(last_error);
        }
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let attempt_plan = HlsManifestRecoveryAttemptPlan {
            binding: &binding,
            attempt_index,
            attempts: attempt_limit,
            reject_reason: reject_reason.as_ref(),
            initial_deterministic_conflict: initial_deterministic_conflict.as_ref(),
            acceptance_mode,
            trigger,
            current_burst_is_full_plan: next_attempt_is_full_plan,
            may_requalify: requalifications < HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH,
            acceptance_deadline,
            completed_candidate_requests,
        };
        let candidate_requests_in_attempt = recovery_burst_plan(context, next_attempt_is_full_plan).total_candidates();
        next_attempt_is_full_plan = false;
        match fetch_and_commit_manifest_recovery_attempt(context, attempt_plan, &mut commit).await {
            HlsManifestRecoveryAttemptError::Committed(committed) => return Ok(committed),
            HlsManifestRecoveryAttemptError::Requalified => {
                requalifications = requalifications.saturating_add(1);
                next_attempt_is_full_plan = true;
                last_error = OriginManifestFetchError::RetryExhausted;
                attempt_limit = attempt_limit_for_started_requalification(attempt_limit, attempt_index);
            }
            HlsManifestRecoveryAttemptError::Rejected(reason) if attempt_index.saturating_add(1) < attempt_limit => {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempt_limit,
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
                if is_hls_retryable_manifest_reject_fetch_error(&err)
                    && attempt_index.saturating_add(1) < attempt_limit =>
            {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempt_limit,
                    next_retry_delay_ms(&context.retry_policy, attempt_index, None, 0),
                    None,
                    Some(&err),
                )
                .await;
                last_error = err;
            }
            HlsManifestRecoveryAttemptError::Fetch(err) => return Err(err),
        }
        completed_candidate_requests =
            completed_candidate_requests.saturating_add(candidate_requests_in_attempt);
        attempt_index = attempt_index.saturating_add(1);
    }

    Err(last_error)
}

fn acceptance_attempt_may_start(
    current_burst_is_full_plan: bool,
    now_ms: u64,
    delay_ms: u64,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
) -> bool {
    // Every new episode owns one unconditional configured burst. Budget
    // enforcement bounds reduced retries and whether a requalification may
    // begin, but never abandons a generation after it has begun.
    current_burst_is_full_plan || now_ms.saturating_add(delay_ms) < acceptance_deadline.as_millis_since_epoch()
}

async fn current_acceptance_deadline(context: &HlsOriginManifestFetchContext) -> HlsAcceptanceDeadlineMs {
    context.session.read().await.origin_control.acceptance_episode.as_ref().map_or_else(
        || HlsAcceptanceDeadlineMs::from_millis_since_epoch(u64::MAX),
        |episode| episode.timing().acceptance_deadline,
    )
}

fn attempt_limit_for_started_requalification(attempt_limit: usize, attempt_index: usize) -> usize {
    // A requalification normally consumes the next configured retry slot. If
    // the landscape changed in the last slot, reserve exactly one additional
    // slot for the newly started generation's mandatory configured burst.
    attempt_limit.max(attempt_index.saturating_add(2))
}

struct HlsManifestRecoveryAttemptPlan<'a> {
    binding: &'a HlsManifestOriginBinding,
    attempt_index: usize,
    attempts: usize,
    reject_reason: Option<&'a HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<&'a HlsDeterministicTimelineConflict>,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    trigger: HlsManifestAcceptanceTrigger,
    current_burst_is_full_plan: bool,
    may_requalify: bool,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
    completed_candidate_requests: usize,
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
    let burst_plan = recovery_burst_plan(context, plan.current_burst_is_full_plan);
    fetch_and_commit_manifest_recovery_burst_attempt(context, plan, burst_plan, commit).await
}

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
    let episode_generation = begin_manifest_acceptance_episode(context, &plan, burst_plan).await;
    let HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        last_fetch_error,
        mut last_reject_reason,
    } = fetch_manifest_recovery_burst_candidates(context, &plan, burst_plan).await;
    record_full_manifest_acceptance_burst(
        context,
        episode_generation,
        plan.current_burst_is_full_plan,
        completed_candidates,
    )
    .await;
    let evaluation =
        match evaluate_manifest_recovery_burst(context, &plan, burst_plan, episode_generation, &fetched_candidates)
    .await
    {
        HlsManifestRecoveryBurstEvaluationOutcome::Continue(evaluation) => *evaluation,
        HlsManifestRecoveryBurstEvaluationOutcome::DeterministicConflict(conflict) => {
            return HlsManifestRecoveryAttemptError::Fetch(
                OriginManifestFetchError::DeterministicTimelineConflict(conflict),
            );
        }
        HlsManifestRecoveryBurstEvaluationOutcome::Requalified => {
            return HlsManifestRecoveryAttemptError::Requalified;
        }
        HlsManifestRecoveryBurstEvaluationOutcome::Exhausted => {
            return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RetryExhausted);
        }
    };
    match commit_selected_manifest_recovery_candidate(
        context,
        &plan,
        burst_plan,
        episode_generation,
        evaluation.acceptance_plan,
        fetched_candidates,
        completed_candidates,
        commit,
    )
    .await
    {
        HlsSelectedManifestRecoveryCommit::Committed(committed) => {
            return HlsManifestRecoveryAttemptError::Committed(committed);
        }
        HlsSelectedManifestRecoveryCommit::Rejected(reason) => last_reject_reason = Some(reason),
        HlsSelectedManifestRecoveryCommit::NotSelected => {}
    }

    if plan.current_burst_is_full_plan {
        record_manifest_acceptance_exhaustion(
            context,
            episode_generation,
            manifest_acceptance_exhaustion_reason(&evaluation.observations),
        )
        .await;
    }
    hold_uncommitted_manifest_acceptance_episode(
        context,
        episode_generation,
        evaluation.held_alternative,
        evaluation.next_retry_at_ms,
    )
    .await;
    if let Some(reason) = last_reject_reason {
        return HlsManifestRecoveryAttemptError::Rejected(reason);
    }
    HlsManifestRecoveryAttemptError::Fetch(last_fetch_error.unwrap_or(OriginManifestFetchError::RetryExhausted))
}

struct HlsManifestRecoveryBurstEvaluation {
    observations: Vec<HlsManifestCandidateObservation>,
    acceptance_plan: HlsManifestCommitPlan,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
}

enum HlsManifestRecoveryBurstEvaluationOutcome {
    Continue(Box<HlsManifestRecoveryBurstEvaluation>),
    DeterministicConflict(Box<HlsDeterministicTimelineConflict>),
    Requalified,
    Exhausted,
}

const fn manifest_acceptance_state_for_plan(plan: &HlsManifestCommitPlan) -> HlsManifestAcceptanceState {
    match plan {
        HlsManifestCommitPlan::Commit { .. } => HlsManifestAcceptanceState::Committing,
        HlsManifestCommitPlan::StageAlternative { .. } => HlsManifestAcceptanceState::StagingSwitchSegment,
        HlsManifestCommitPlan::HoldAlternative | HlsManifestCommitPlan::RejectAll => {
            HlsManifestAcceptanceState::Holding
        }
    }
}

async fn evaluate_manifest_recovery_burst(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    fetched_candidates: &[HlsManifestRecoveryCandidate],
) -> HlsManifestRecoveryBurstEvaluationOutcome {
    // Candidate order is scheduler order only. In particular, a numerically
    // larger highwater from a different effective host is never a sort key.
    // Pressure is the immutable pre-episode snapshot; `Recovering` itself is
    // deliberately not reserve evidence.
    let resource_evidence = committed_resource_evidence(context).await;
    let candidate_evaluations = fetched_candidates
        .iter()
        .map(|candidate| {
            let observation = manifest_candidate_observation(
                candidate,
                burst_plan,
                &resource_evidence.ready_identities,
                &resource_evidence.published_identities,
            );
            let deterministic_conflict = deterministic_timeline_conflict_for_candidate(candidate, &resource_evidence);
            (observation, deterministic_conflict)
        })
        .collect::<Vec<_>>();
    let observations = candidate_evaluations
        .iter()
        .map(|(observation, _)| observation.clone())
        .collect::<Vec<_>>();
    if plan.current_burst_is_full_plan {
        record_manifest_acceptance_landscape(context, episode_generation, manifest_acceptance_landscape(&observations))
            .await;
        if let Some(conflict) = deterministic_conflict_proven_by_full_burst(
            plan.initial_deterministic_conflict,
            &candidate_evaluations,
            fetched_candidates.len(),
            burst_plan.total_candidates(),
        ) {
            record_deterministic_conflict_receipt(
                context,
                episode_generation,
                conflict.clone(),
                &resource_evidence,
            )
            .await;
            return HlsManifestRecoveryBurstEvaluationOutcome::DeterministicConflict(Box::new(conflict));
        }
    }
    let episode_snapshot = manifest_acceptance_episode_snapshot(context, episode_generation).await;
    let reduced_landscape_changed = !plan.current_burst_is_full_plan
        && episode_snapshot
            .as_ref()
            .and_then(|episode| episode.observed_landscape.as_ref())
            .map(|landscape| classify_reduced_retry_landscape(landscape, &observations))
            .is_some_and(HlsReducedRetryLandscapeChange::requires_full_requalification);
    if reduced_landscape_changed {
        if plan.may_requalify
            && begin_requalified_manifest_acceptance_episode(
                context,
                episode_generation,
                plan.trigger,
                plan.acceptance_deadline,
            )
            .await
        {
            return HlsManifestRecoveryBurstEvaluationOutcome::Requalified;
        }
        // A changed landscape may never fall through into reduced-burst
        // cross-host acceptance when its full requalification budget is gone.
        let next_retry_at_ms = current_time_millis().saturating_add(next_retry_delay_ms(
            &context.retry_policy,
            plan.attempt_index,
            None,
            0,
        ));
        hold_uncommitted_manifest_acceptance_episode(
            context,
            episode_generation,
            episode_snapshot.and_then(|episode| episode.held_alternative),
            next_retry_at_ms,
        )
        .await;
        return HlsManifestRecoveryBurstEvaluationOutcome::Exhausted;
    }
    let acceptance_plan = episode_snapshot.as_ref().map_or(HlsManifestCommitPlan::RejectAll, |episode| {
        evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
            full_burst_completed: episode.full_burst_completed,
            current_burst_is_full_plan: plan.current_burst_is_full_plan,
            trigger: episode.trigger,
            previous_alternative: episode.held_alternative.as_ref(),
            observations: &observations,
        })
    });
    let held_alternative = episode_snapshot.as_ref().and_then(|episode| {
        held_alternative_after_burst(&observations, episode.held_alternative.as_ref(), plan.current_burst_is_full_plan)
    });
    let next_retry_at_ms =
        current_time_millis().saturating_add(next_retry_delay_ms(&context.retry_policy, plan.attempt_index, None, 0));
    let next_state = manifest_acceptance_state_for_plan(&acceptance_plan);
    update_manifest_acceptance_episode_state(context, episode_generation, next_state).await;
    HlsManifestRecoveryBurstEvaluationOutcome::Continue(Box::new(HlsManifestRecoveryBurstEvaluation {
        observations,
        acceptance_plan,
        held_alternative,
        next_retry_at_ms,
    }))
}

enum HlsSelectedManifestRecoveryCommit<T> {
    Committed(T),
    Rejected(HlsManifestRejectLogReason),
    NotSelected,
}

#[allow(clippy::too_many_arguments)]
async fn commit_selected_manifest_recovery_candidate<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    acceptance_plan: HlsManifestCommitPlan,
    fetched_candidates: Vec<HlsManifestRecoveryCandidate>,
    completed_candidates: usize,
    commit: &mut C,
) -> HlsSelectedManifestRecoveryCommit<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let Some((selected_candidate_index, selected_commit_kind)) = selected_manifest_candidate(acceptance_plan) else {
        return HlsSelectedManifestRecoveryCommit::NotSelected;
    };
    let Some(candidate) =
        fetched_candidates.into_iter().find(|candidate| candidate.candidate_index == selected_candidate_index)
    else {
        return HlsSelectedManifestRecoveryCommit::NotSelected;
    };
    let HlsManifestRecoveryCandidate { candidate_index, fetched, report, .. } = candidate;
    let candidate_identity = HlsManifestRecoveryCandidateIdentity::from_candidate(
        candidate_index,
        report.quality.effective_host.as_deref(),
        &fetched.body,
    );
    if !select_manifest_recovery_candidate(context, episode_generation, candidate_identity).await {
        return HlsSelectedManifestRecoveryCommit::Rejected(HlsManifestRejectLogReason::StagedSwitchInvalidated);
    }
    let acceptance_mode = selected_commit_acceptance_mode(selected_commit_kind, plan.acceptance_mode);
    let selection = if burst_plan.total_candidates() > 1 {
        HlsManifestFetchSelection::Burst
    } else {
        HlsManifestFetchSelection::Recovery
    };
    let candidate_requests = plan.completed_candidate_requests.saturating_add(completed_candidates);
    match commit(
        fetched.with_recovery_diagnostics(plan.attempt_index + 1, candidate_requests, selection),
        acceptance_mode,
    )
    .await
    {
        Ok(committed) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            log_manifest_recovery_selected(context, candidate_index, burst_plan.total_candidates(), &report).await;
            HlsSelectedManifestRecoveryCommit::Committed(committed)
        }
        Err(err) => {
            let reason = commit_error_to_retry_reason(&err);
            log_manifest_recovery_candidate_rejected(
                context,
                candidate_index,
                burst_plan.total_candidates(),
                report.quality.effective_host.as_deref(),
                report.quality.origin_highwater,
                &reason,
            )
            .await;
            HlsSelectedManifestRecoveryCommit::Rejected(reason)
        }
    }
}

async fn select_manifest_recovery_candidate(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    candidate_identity: HlsManifestRecoveryCandidateIdentity,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return false;
    };
    episode.select_candidate(generation, candidate_identity) == HlsRecoveryWorkloadBindingUpdate::Applied
}

const fn selected_commit_acceptance_mode(
    commit_kind: HlsManifestCommitKind,
    fallback: HlsManifestCommitAcceptanceMode,
) -> HlsManifestCommitAcceptanceMode {
    match commit_kind {
        HlsManifestCommitKind::AnchoredAlternative | HlsManifestCommitKind::AlternativeAsNewEpoch => {
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
        }
        HlsManifestCommitKind::ContentVerifiedAlternative => {
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate
        }
        HlsManifestCommitKind::EmergencyAlternativeAsNewEpoch => {
            HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate
        }
        HlsManifestCommitKind::Pinned => fallback,
    }
}

fn selected_manifest_candidate(plan: HlsManifestCommitPlan) -> Option<(usize, HlsManifestCommitKind)> {
    match plan {
        HlsManifestCommitPlan::Commit { candidate_index, kind }
        | HlsManifestCommitPlan::StageAlternative { candidate_index, kind } => Some((candidate_index, kind)),
        HlsManifestCommitPlan::HoldAlternative | HlsManifestCommitPlan::RejectAll => None,
    }
}

async fn begin_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> Option<HlsManifestAcceptanceGeneration> {
    let mut session = context.session.write().await;
    if plan.attempt_index != 0 {
        let episode = session.origin_control.acceptance_episode.as_mut()?;
        if episode.trigger() != plan.trigger || episode.state == HlsManifestAcceptanceState::Completed {
            return None;
        }
        episode.state = HlsManifestAcceptanceState::Collecting;
        return Some(episode.generation);
    }
    let started_at_ms = current_time_millis();
    let timing = acceptance_episode_timing(context, &session, started_at_ms, burst_plan);
    let generation = session.origin_control.begin_acceptance_episode(started_at_ms, burst_plan, plan.trigger, &timing);
    if let Some(episode) = session.origin_control.acceptance_episode.as_mut() {
        episode.state = HlsManifestAcceptanceState::Collecting;
        debug!(
            "HLS manifest acceptance full burst started: generation={} candidates={} max_stagger_ms={} binding_scheme={} binding_host={} provider_url_index={}",
            generation.0,
            episode.required_candidates(),
            episode.burst_max_stagger_ms(),
            plan.binding.request_url().scheme(),
            plan.binding
                .request_url()
                .host_str()
                .map_or_else(|| "none".to_string(), hls_origin_log_value),
            plan.binding
                .provider_url_index()
                .map_or_else(|| "none".to_string(), |index| index.to_string())
        );
    }
    Some(generation)
}

fn acceptance_episode_timing(
    context: &HlsOriginManifestFetchContext,
    session: &super::HlsSession,
    started_at_ms: u64,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    let fallback_target_duration_ms = session
        .origin_control
        .target_duration_snapshot_ms
        .or_else(|| session.target_duration.map(|seconds| u64::from(seconds).saturating_mul(1_000)))
        .unwrap_or(15_000);
    let seed = context.acceptance_timing_seed.unwrap_or(HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: fallback_target_duration_ms,
        transition_margin: HlsTransitionMarginMs::from_millis(fallback_target_duration_ms),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    });
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: seed.target_duration_ms,
        transition_margin: seed.transition_margin,
        workload: seed.workload,
        observed_latency: session.origin_control.recovery_samples.latency_snapshot(),
        required_terminal_media_key: seed.required_terminal_media_key,
        terminal_media_preparation: seed.terminal_media_preparation,
        policy: context.recovery_timing_policy,
    })
}

#[derive(Debug, Clone)]
struct HlsManifestAcceptanceEpisodeSnapshot {
    trigger: HlsManifestAcceptanceTrigger,
    full_burst_completed: bool,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    observed_landscape: Option<HlsManifestAcceptanceLandscape>,
}

async fn manifest_acceptance_episode_snapshot(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
) -> Option<HlsManifestAcceptanceEpisodeSnapshot> {
    let generation = generation?;
    let session = context.session.read().await;
    session.origin_control.acceptance_episode.as_ref().filter(|episode| episode.generation == generation).map(
        |episode| HlsManifestAcceptanceEpisodeSnapshot {
            trigger: episode.trigger(),
            full_burst_completed: episode.full_burst_completed,
            held_alternative: episode.held_alternative.clone(),
            observed_landscape: episode.observed_landscape.clone(),
        },
    )
}

async fn record_manifest_acceptance_landscape(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    landscape: HlsManifestAcceptanceLandscape,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.observed_landscape = Some(landscape);
    }
}

async fn begin_requalified_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let mut session = context.session.write().await;
    let now_ms = current_time_millis();
    if !acceptance_attempt_may_start(false, now_ms, 0, acceptance_deadline) {
        return false;
    }
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return false;
    };
    if episode.generation != generation
        || !episode.full_burst_completed
        || episode.state == HlsManifestAcceptanceState::Completed
    {
        return false;
    }
    episode.state = HlsManifestAcceptanceState::Holding;
    let configured_plan = context.manifest_recovery_burst.level.plan();
    let timing = acceptance_episode_timing(context, &session, now_ms, configured_plan);
    let next_generation = session.origin_control.begin_acceptance_episode(now_ms, configured_plan, trigger, &timing);
    debug!(
        "HLS manifest acceptance landscape changed: previous_generation={} next_generation={} candidates={} decision=full-requalification",
        generation.0,
        next_generation.0,
        configured_plan.total_candidates()
    );
    true
}

async fn record_full_manifest_acceptance_burst(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    current_burst_is_full_plan: bool,
    completed_candidates: usize,
) {
    if !current_burst_is_full_plan {
        return;
    }
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_full_burst_candidates(completed_candidates);
    }
}

async fn update_manifest_acceptance_episode_state(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    state: HlsManifestAcceptanceState,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.state = state;
    }
}

async fn hold_uncommitted_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.hold_after_uncommitted_burst(held_alternative, Some(next_retry_at_ms));
        session.origin_control.path_condition = super::origin_progress::HlsOriginPathCondition::AcceptanceConflict;
    }
}

async fn complete_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.complete();
    }
}

fn manifest_acceptance_exhaustion_reason(
    observations: &[HlsManifestCandidateObservation],
) -> HlsManifestAcceptanceExhaustionReason {
    if observations.is_empty() {
        return HlsManifestAcceptanceExhaustionReason::AllFailed;
    }
    if observations.iter().all(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::PinnedHost
            && matches!(
                candidate.local_sequence_relation,
                Some(HlsHostLocalSequenceRelation::Same | HlsHostLocalSequenceRelation::Backward)
            )
    }) {
        HlsManifestAcceptanceExhaustionReason::NoProgress
    } else {
        HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate
    }
}

async fn record_manifest_acceptance_exhaustion(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    reason: HlsManifestAcceptanceExhaustionReason,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_exhaustion(reason);
    }
}

fn manifest_candidate_observation(
    candidate: &HlsManifestRecoveryCandidate,
    burst_plan: HlsManifestRecoveryBurstPlan,
    committed_resource_identities: &[HlsMediaResourceIdentity],
    published_resource_identities: &[HlsMediaResourceIdentity],
) -> HlsManifestCandidateObservation {
    let host_relation = match candidate.report.quality.host_relation {
        HlsManifestOriginRelation::SameRedirectHost => HlsCandidateHostRelation::PinnedHost,
        HlsManifestOriginRelation::OtherRedirectHost => HlsCandidateHostRelation::OtherHost,
        HlsManifestOriginRelation::Initial => HlsCandidateHostRelation::InitialBaseline,
        HlsManifestOriginRelation::UnknownHost => HlsCandidateHostRelation::Unknown,
    };
    let (timeline_fingerprint, has_switch_segment, emergency_evidence) =
        build_manifest_timeline_fingerprint(&candidate.fetched.body, &candidate.fetched.final_manifest_url);
    let resource_timeline_evidence =
        candidate_resource_timeline_evidence(&timeline_fingerprint, published_resource_identities);
    let committed_content_anchor = timeline_fingerprint
        .segment_samples
        .first()
        .and_then(|segment| segment.normalized_resource_identity)
        .filter(|identity| committed_resource_identities.iter().any(|committed| committed.matches(*identity)))
        .filter(|_| {
            emergency_evidence.live_handoff == HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification
        })
        .map_or(HlsCommittedContentAnchorEvidence::Unavailable, |_| {
            HlsCommittedContentAnchorEvidence::RequiresStagedByteVerification
        });
    HlsManifestCandidateObservation {
        candidate_index: candidate.candidate_index,
        candidate_slot: burst_plan.slot_for_candidate(candidate.candidate_index),
        effective_host: candidate.report.quality.effective_host.clone(),
        host_relation,
        host_local_media_sequence: candidate.report.media_sequence,
        host_local_highwater: candidate.report.quality.origin_highwater,
        local_sequence_relation: matches!(
            host_relation,
            HlsCandidateHostRelation::PinnedHost | HlsCandidateHostRelation::InitialBaseline
        )
        .then(|| {
            classify_host_local_sequence(
                candidate.report.quality.previous_highwater,
                candidate.report.quality.origin_highwater,
                candidate.report.quality.allowed_forward_window.unwrap_or(1),
                candidate.report.quality.sequence_relation == HlsManifestSequenceRelation::Rebase,
            )
        })
        .flatten(),
        resource_timeline_evidence,
        timeline_fingerprint,
        manifest_fetch_elapsed_ms: candidate.fetch_elapsed_ms,
        switch_segment_readiness: if has_switch_segment {
            HlsSwitchSegmentReadiness::RequiresStaging
        } else {
            HlsSwitchSegmentReadiness::Unavailable
        },
        committed_content_anchor,
        emergency_evidence,
        evidence: HlsCrossHostAcceptanceEvidence::Insufficient,
    }
}

struct HlsCommittedResourceEvidence {
    ready_identities: Vec<HlsMediaResourceIdentity>,
    published_identities: Vec<HlsMediaResourceIdentity>,
    published_entries: Vec<(HlsMediaResourceIdentity, u64)>,
    previous_proxy_tail: Option<u64>,
    origin_progress_generation: u64,
    published_resource_history_generation: u64,
    pinned_host_generation: u64,
}

async fn committed_resource_evidence(context: &HlsOriginManifestFetchContext) -> HlsCommittedResourceEvidence {
    let session = context.session.read().await;
    let ready_identities = session
        .segments
        .values()
        .rev()
        .filter(|entry| entry.origin_key.origin_epoch == session.origin_epoch)
        .filter(|entry| matches!(&entry.status, super::SegmentCacheStatus::Ready { .. }))
        .filter_map(|entry| {
            let fetch_ref = entry.origin_fetch_ref.as_ref()?;
            Some(HlsMediaResourceIdentity::from_url(&fetch_ref.resolved_origin_url, fetch_ref.byte_range))
        })
        .take(HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT)
        .collect::<Vec<_>>();
    let published_entries = session.published_resource_history.recent_entries(usize::MAX).collect::<Vec<_>>();
    let published_identities = published_entries.iter().map(|(identity, _)| *identity).collect();
    HlsCommittedResourceEvidence {
        ready_identities,
        published_identities,
        published_entries,
        previous_proxy_tail: session.proxy_next_seq.and_then(|next| next.checked_sub(1)),
        origin_progress_generation: session.origin_control.progress_generation,
        published_resource_history_generation: session.published_resource_history.generation(),
        pinned_host_generation: session.origin_epoch,
    }
}

fn candidate_resource_timeline_evidence(
    fingerprint: &HlsManifestTimelineFingerprint,
    published: &[HlsMediaResourceIdentity],
) -> HlsResourceTimelineEvidence {
    let mut saw_published = false;
    let mut saw_new = false;
    for identity in fingerprint
        .segment_samples
        .iter()
        .filter_map(|segment| segment.normalized_resource_identity)
    {
        let was_published = published.iter().any(|existing| existing.matches(identity));
        if was_published && saw_new {
            return HlsResourceTimelineEvidence::ContradictoryOrder;
        }
        saw_published |= was_published;
        saw_new |= !was_published;
    }
    if saw_published && !saw_new {
        HlsResourceTimelineEvidence::ReplayOnly
    } else {
        HlsResourceTimelineEvidence::Eligible
    }
}

fn deterministic_timeline_conflict_for_candidate(
    candidate: &HlsManifestRecoveryCandidate,
    evidence: &HlsCommittedResourceEvidence,
) -> Option<HlsDeterministicTimelineConflict> {
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&candidate.fetched.body, &candidate.fetched.final_manifest_url)
    else {
        return None;
    };
    let candidate_fingerprint = deterministic_conflict_fingerprint(
        &manifest,
        &candidate.fetched.body,
        &candidate.fetched.final_manifest_url,
    );
    let mut saw_new = false;
    for (candidate_position, segment) in manifest.segments.iter().enumerate() {
        let identity = HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range);
        let resource_key = identity.semantic_key();
        let published = evidence
            .published_entries
            .iter()
            .find(|(published, _)| published.semantic_key() == resource_key);
        if let Some((_, existing_proxy_seq)) = published {
            if saw_new {
                return Some(HlsDeterministicTimelineConflict {
                    previous_proxy_tail: evidence.previous_proxy_tail,
                    existing_proxy_seq: *existing_proxy_seq,
                    candidate_position,
                    candidate_origin_seq: segment.origin_seq,
                    resource_key,
                    decision: HlsResourceReplayDecision::RejectContradictoryOrder,
                    candidate_fingerprint,
                });
            }
        } else {
            saw_new = true;
        }
    }
    None
}

fn deterministic_conflict_proven_by_full_burst(
    initial: Option<&HlsDeterministicTimelineConflict>,
    evaluations: &[(HlsManifestCandidateObservation, Option<HlsDeterministicTimelineConflict>)],
    fetched_candidates: usize,
    required_candidates: usize,
) -> Option<HlsDeterministicTimelineConflict> {
    if fetched_candidates != required_candidates || evaluations.len() != required_candidates {
        return None;
    }
    let first = evaluations.first()?.1.as_ref()?;
    if initial.is_some_and(|initial| initial != first)
        || evaluations.iter().any(|(_, conflict)| conflict.as_ref() != Some(first))
    {
        return None;
    }
    Some(first.clone())
}

async fn record_deterministic_conflict_receipt(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    conflict: HlsDeterministicTimelineConflict,
    evidence: &HlsCommittedResourceEvidence,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let receipt = HlsDeterministicConflictReceipt {
        conflict,
        origin_progress_generation: evidence.origin_progress_generation,
        published_resource_history_generation: evidence.published_resource_history_generation,
        pinned_host_generation: evidence.pinned_host_generation,
    };
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_deterministic_conflict(receipt);
    }
}

pub(crate) fn deterministic_conflict_receipt_matches(
    session: &super::HlsSession,
    conflict: &HlsDeterministicTimelineConflict,
) -> bool {
    session
        .origin_control
        .acceptance_episode
        .as_ref()
        .and_then(|episode| episode.deterministic_conflict_receipt())
        .is_some_and(|receipt| {
            receipt.conflict == *conflict && deterministic_conflict_receipt_is_current_for_session(receipt, session)
        })
}

pub(crate) fn deterministic_conflict_receipt_is_current(session: &super::HlsSession) -> bool {
    session
        .origin_control
        .acceptance_episode
        .as_ref()
        .and_then(|episode| episode.deterministic_conflict_receipt())
        .is_some_and(|receipt| deterministic_conflict_receipt_is_current_for_session(receipt, session))
}

fn deterministic_conflict_receipt_is_current_for_session(
    receipt: &HlsDeterministicConflictReceipt,
    session: &super::HlsSession,
) -> bool {
    receipt.origin_progress_generation == session.origin_control.progress_generation
        && receipt.published_resource_history_generation == session.published_resource_history.generation()
        && receipt.pinned_host_generation == session.origin_epoch
}

fn build_manifest_timeline_fingerprint(
    body: &str,
    final_manifest_url: &str,
) -> (HlsManifestTimelineFingerprint, bool, HlsEmergencyAcceptanceEvidence) {
    match parse_origin_media_manifest(body, final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) => {
            let has_switch_segment = !manifest.segments.is_empty();
            let emergency_evidence = emergency_manifest_evidence(&manifest);
            (fingerprint_parsed_manifest(&manifest, body, final_manifest_url), has_switch_segment, emergency_evidence)
        }
        OriginManifestParseOutcome::TransientPassthrough { .. } => {
            let (fingerprint, _has_media_uri) = fingerprint_transient_manifest(body, final_manifest_url);
            // Cross-host transient manifests cannot use the normal typed timeline/MAP staging contract. They remain
            // valid on the pinned host, but an alternative host is not acceptance-ready until a dedicated typed
            // transient receipt exists.
            (fingerprint, false, HlsEmergencyAcceptanceEvidence::INCOMPATIBLE)
        }
    }
}

pub(crate) fn deterministic_timeline_conflict_from_rejection(
    fetched: &FetchedOriginManifest,
    reason: &HlsManifestRejectLogReason,
) -> Option<HlsDeterministicTimelineConflict> {
    let HlsManifestRejectLogReason::PublishedResourceReplay {
        previous_proxy_tail,
        existing_proxy_seq,
        candidate_position,
        candidate_origin_seq,
        resource_key: expected_resource_key,
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
    } = reason
    else {
        return None;
    };
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)
    else {
        return None;
    };
    let segment = manifest.segments.get(*candidate_position)?;
    let resource_key =
        HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range).semantic_key();
    if resource_key != *expected_resource_key {
        return None;
    }
    let candidate_fingerprint =
        deterministic_conflict_fingerprint(&manifest, &fetched.body, &fetched.final_manifest_url);
    Some(HlsDeterministicTimelineConflict {
        previous_proxy_tail: *previous_proxy_tail,
        existing_proxy_seq: *existing_proxy_seq,
        candidate_position: *candidate_position,
        candidate_origin_seq: *candidate_origin_seq,
        resource_key,
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
        candidate_fingerprint,
    })
}

fn emergency_manifest_evidence(manifest: &ParsedOriginManifest) -> HlsEmergencyAcceptanceEvidence {
    let clear_mpeg_ts_without_map = manifest.maps.is_empty()
        && !manifest.segments.is_empty()
        && manifest.segments.iter().all(|segment| segment.encryption.is_none())
        && manifest.segments.iter().all(|segment| is_mpeg_ts_resource(&segment.resolved_origin_url));
    if clear_mpeg_ts_without_map {
        HlsEmergencyAcceptanceEvidence {
            live_handoff: HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification,
            terminal_alternative: HlsTerminalAlternativeCompatibility::RequiresStagedComparison,
        }
    } else {
        HlsEmergencyAcceptanceEvidence::INCOMPATIBLE
    }
}

fn is_mpeg_ts_resource(resource: &str) -> bool {
    Url::parse(resource).ok().map_or_else(
        || {
            resource
                .split(['?', '#'])
                .next()
                .and_then(|path| path.rsplit_once('.'))
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("ts"))
        },
        |url| url.path().rsplit_once('.').is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("ts")),
    )
}

fn fingerprint_parsed_manifest(
    manifest: &ParsedOriginManifest,
    body: &str,
    final_manifest_url: &str,
) -> HlsManifestTimelineFingerprint {
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::with_capacity(manifest.segments.len().min(HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT));
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for segment in &manifest.segments {
        duration_hasher.update(segment.duration_ms.to_be_bytes());
        discontinuity_hasher.update([u8::from(segment.discontinuity_before)]);
        let resource_identity =
            HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range);
        resource_hasher.update(resource_identity.exact_path_hash());
        update_container_signature(&mut container_hasher, &segment.resolved_origin_url);
        let program_date_time_ms = parse_program_date_time_ms(segment.program_date_time.as_deref());
        first_program_date_time_ms = first_program_date_time_ms.or(program_date_time_ms);
        if program_date_time_ms.is_some() {
            last_program_date_time_ms = program_date_time_ms;
        }
        if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
            segment_samples.push(HlsManifestSegmentFingerprint {
                duration_ms: segment.duration_ms,
                discontinuity_before: segment.discontinuity_before,
                program_date_time_ms,
                normalized_resource_identity: Some(resource_identity),
            });
        }
    }
    if !manifest.maps.is_empty() {
        container_hasher.update(b"map");
    }
    HlsManifestTimelineFingerprint {
        segment_count: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
        first_program_date_time_ms,
        last_program_date_time_ms,
        duration_pattern_hash: duration_hasher.finalize().into(),
        discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
        normalized_resource_pattern_hash: (!manifest.segments.is_empty()).then(|| resource_hasher.finalize().into()),
        map_and_encryption_hash: map_and_encryption_hash(body, &manifest.maps, final_manifest_url),
        container_signature_hash: container_hasher.finalize().into(),
        segment_samples,
    }
}

fn deterministic_conflict_fingerprint(
    manifest: &ParsedOriginManifest,
    body: &str,
    final_manifest_url: &str,
) -> HlsDeterministicConflictFingerprint {
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::with_capacity(manifest.segments.len().min(HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT));
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for segment in &manifest.segments {
        duration_hasher.update(segment.duration_ms.to_be_bytes());
        discontinuity_hasher.update([u8::from(segment.discontinuity_before)]);
        let resource_key =
            HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range).semantic_key();
        resource_hasher.update(resource_key.bytes());
        update_container_signature(&mut container_hasher, &segment.resolved_origin_url);
        let program_date_time_ms = parse_program_date_time_ms(segment.program_date_time.as_deref());
        first_program_date_time_ms = first_program_date_time_ms.or(program_date_time_ms);
        if program_date_time_ms.is_some() {
            last_program_date_time_ms = program_date_time_ms;
        }
        if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
            segment_samples.push(HlsDeterministicConflictSegmentFingerprint {
                duration_ms: segment.duration_ms,
                discontinuity_before: segment.discontinuity_before,
                program_date_time_ms,
                resource_key: Some(resource_key),
            });
        }
    }
    if !manifest.maps.is_empty() {
        container_hasher.update(b"map");
    }
    HlsDeterministicConflictFingerprint {
        segment_count: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
        first_program_date_time_ms,
        last_program_date_time_ms,
        duration_pattern_hash: duration_hasher.finalize().into(),
        discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
        semantic_resource_pattern_hash: (!manifest.segments.is_empty()).then(|| resource_hasher.finalize().into()),
        map_and_encryption_hash: semantic_map_and_encryption_hash(body, &manifest.maps, final_manifest_url),
        container_signature_hash: container_hasher.finalize().into(),
        segment_samples,
    }
}

fn fingerprint_transient_manifest(body: &str, final_manifest_url: &str) -> (HlsManifestTimelineFingerprint, bool) {
    let timeline = parse_origin_manifest_timeline(body).ok();
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::new();
    let mut pending_duration_ms = None;
    let mut pending_discontinuity = false;
    let mut pending_program_date_time_ms = None;
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration_ms = parse_extinf_millis(value);
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_program_date_time_ms = parse_program_date_time_ms(Some(value.trim()));
        } else if !line.starts_with('#') {
            let Some(duration_ms) = pending_duration_ms.take() else {
                continue;
            };
            let resolved = resolve_fingerprint_resource(final_manifest_url, line);
            let resource_identity = HlsMediaResourceIdentity::from_url(&resolved, None);
            duration_hasher.update(duration_ms.to_be_bytes());
            discontinuity_hasher.update([u8::from(pending_discontinuity)]);
            resource_hasher.update(resource_identity.exact_path_hash());
            update_container_signature(&mut container_hasher, &resolved);
            first_program_date_time_ms = first_program_date_time_ms.or(pending_program_date_time_ms);
            if pending_program_date_time_ms.is_some() {
                last_program_date_time_ms = pending_program_date_time_ms;
            }
            if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
                segment_samples.push(HlsManifestSegmentFingerprint {
                    duration_ms,
                    discontinuity_before: pending_discontinuity,
                    program_date_time_ms: pending_program_date_time_ms,
                    normalized_resource_identity: Some(resource_identity),
                });
            }
            pending_discontinuity = false;
            pending_program_date_time_ms = None;
        }
    }
    let segment_count = timeline
        .map(|timeline| u32::try_from(timeline.origin_manifest_segment_cnt).unwrap_or(u32::MAX))
        .unwrap_or_default();
    let has_switch_segment = segment_count > 0 && !segment_samples.is_empty();
    (
        HlsManifestTimelineFingerprint {
            segment_count,
            first_program_date_time_ms,
            last_program_date_time_ms,
            duration_pattern_hash: duration_hasher.finalize().into(),
            discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
            normalized_resource_pattern_hash: has_switch_segment.then(|| resource_hasher.finalize().into()),
            map_and_encryption_hash: map_and_encryption_hash(body, &[], final_manifest_url),
            container_signature_hash: container_hasher.finalize().into(),
            segment_samples,
        },
        has_switch_segment,
    )
}

fn parse_extinf_millis(value: &str) -> Option<u64> {
    let seconds = value.split(',').next()?.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    let Ok(duration) = Duration::try_from_secs_f64(seconds) else {
        return Some(u64::MAX);
    };
    let rounded = duration.checked_add(Duration::from_micros(500)).unwrap_or(Duration::MAX);
    Some(u64::try_from(rounded.as_millis()).unwrap_or(u64::MAX))
}

fn parse_program_date_time_ms(value: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value?).ok().map(|timestamp| timestamp.timestamp_millis())
}

fn resolve_fingerprint_resource(final_manifest_url: &str, resource: &str) -> String {
    Url::parse(final_manifest_url)
        .ok()
        .and_then(|base| base.join(resource).ok())
        .map_or_else(|| resource.to_string(), |resolved| resolved.to_string())
}

fn update_container_signature(hasher: &mut Sha256, url: &str) {
    let path = Url::parse(url)
        .ok()
        .map_or_else(|| url.split('?').next().unwrap_or_default().to_string(), |parsed| parsed.path().to_string());
    let extension = path.rsplit_once('.').map(|(_, extension)| extension).unwrap_or_default();
    hasher.update(extension.to_ascii_lowercase().as_bytes());
    hasher.update([0]);
}

fn map_and_encryption_hash(
    body: &str,
    maps: &[crate::processing::parser::hls::origin_manifest::ParsedOriginMap],
    final_manifest_url: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for map in maps {
        hasher.update(
            HlsMediaResourceIdentity::from_url(&map.resolved_origin_uri, map.byte_range).exact_path_hash(),
        );
    }
    for line in body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#EXT-X-KEY:") || (maps.is_empty() && line.starts_with("#EXT-X-MAP:")))
    {
        hasher.update(normalized_tag_uri(line, final_manifest_url).as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn semantic_map_and_encryption_hash(
    body: &str,
    maps: &[crate::processing::parser::hls::origin_manifest::ParsedOriginMap],
    final_manifest_url: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for map in maps {
        hasher.update(
            HlsMediaResourceIdentity::from_url(&map.resolved_origin_uri, map.byte_range)
                .semantic_key()
                .bytes(),
        );
    }
    for line in body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#EXT-X-KEY:") || (maps.is_empty() && line.starts_with("#EXT-X-MAP:")))
    {
        update_semantic_tag_identity(&mut hasher, line, final_manifest_url);
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn update_semantic_tag_identity(hasher: &mut Sha256, line: &str, final_manifest_url: &str) {
    let Some(uri_start) = line.find("URI=\"") else {
        hasher.update(line.as_bytes());
        return;
    };
    let value_start = uri_start.saturating_add(5);
    let Some(relative_end) = line.get(value_start..).and_then(|tail| tail.find('"')) else {
        hasher.update(line.as_bytes());
        return;
    };
    let value_end = value_start.saturating_add(relative_end);
    let uri = line.get(value_start..value_end).unwrap_or_default();
    let resolved = resolve_fingerprint_resource(final_manifest_url, uri);
    hasher.update(&line.as_bytes()[..value_start]);
    hasher.update(HlsMediaResourceIdentity::from_url(&resolved, None).semantic_key().bytes());
    hasher.update(&line.as_bytes()[value_end..]);
}

fn normalized_tag_uri(line: &str, final_manifest_url: &str) -> String {
    let Some(uri_start) = line.find("URI=\"") else {
        return line.to_string();
    };
    let value_start = uri_start.saturating_add(5);
    let Some(relative_end) = line.get(value_start..).and_then(|tail| tail.find('"')) else {
        return line.to_string();
    };
    let value_end = value_start.saturating_add(relative_end);
    let uri = line.get(value_start..value_end).unwrap_or_default();
    let normalized_uri = Url::parse(uri)
        .ok()
        .or_else(|| Url::parse(final_manifest_url).ok()?.join(uri).ok())
        .map_or_else(|| uri.split(['?', '#']).next().unwrap_or_default().to_string(), |url| url.path().to_string());
    format!("{}{}{}", &line[..value_start], normalized_uri, &line[value_end..])
}

async fn fetch_manifest_recovery_burst_candidates(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> HlsManifestRecoveryBurstCollection {
    let mut tasks = JoinSet::new();
    let candidates = burst_plan.total_candidates();
    for candidate_index in 0..candidates {
        let context = context.clone();
        let binding = plan.binding.clone();
        let reject_reason = plan.reject_reason.cloned();
        let attempt_index = plan.attempt_index;
        let attempts = plan.attempts;
        tasks.spawn(async move {
            let stagger_ms = u64::try_from(burst_plan.slot_for_candidate(candidate_index))
                .unwrap_or_default()
                .saturating_mul(HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS);
            if stagger_ms > 0 {
                tokio::time::sleep(Duration::from_millis(stagger_ms)).await;
            }
            let request = HlsOriginManifestFetchRequest::recovery_direct_target(
                &context,
                &binding,
                reject_reason.as_ref(),
                ManifestRecoveryAttemptLogContext { attempt_index, attempts, candidate_index, candidates },
            );
            let fetch_started = Instant::now();
            let result = fetch_hls_origin_manifest_request(request).await;
            let fetch_elapsed_ms = u64::try_from(fetch_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            (candidate_index, fetch_elapsed_ms, result)
        });
    }

    let mut last_fetch_error = None;
    let mut last_reject_reason = None;
    let mut fetched_candidates = Vec::new();
    let mut completed_candidates = 0_usize;
    while let Some(join_result) = tasks.join_next().await {
        completed_candidates = completed_candidates.saturating_add(1);
        let Ok((candidate_index, fetch_elapsed_ms, result)) = join_result else {
            last_fetch_error = Some(OriginManifestFetchError::Request("manifest recovery task failed".to_string()));
            continue;
        };
        match result {
            Ok(fetched) => {
                match score_manifest_recovery_candidate_with_logging(
                    context,
                    candidate_index,
                    candidates,
                    &fetched,
                    plan.acceptance_mode,
                )
                .await
                {
                    Ok(report) => {
                        fetched_candidates.push(HlsManifestRecoveryCandidate {
                            candidate_index,
                            fetch_elapsed_ms,
                            fetched,
                            report,
                        });
                    }
                    Err(reason) => last_reject_reason = Some(reason),
                }
            }
            Err(err) => {
                last_fetch_error = Some(err);
            }
        }
    }
    HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        last_fetch_error,
        last_reject_reason,
    }
}

async fn score_manifest_recovery_candidate_with_logging(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    fetched: &FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let score_result = {
        let session = context.session.read().await;
        score_hls_manifest_recovery_candidate_with_mode(&session, fetched, context, acceptance_mode)
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

#[cfg(test)]
pub(crate) fn score_hls_manifest_recovery_candidate(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
    context: &HlsOriginManifestFetchContext,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    score_hls_manifest_recovery_candidate_with_mode(
        session,
        fetched,
        context,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
    )
}

fn score_hls_manifest_recovery_candidate_with_mode(
    session: &super::HlsSession,
    fetched: &FetchedOriginManifest,
    context: &HlsOriginManifestFetchContext,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let timeline = parse_manifest_timeline_for_recovery_scoring(session, fetched)?;
    let media_sequence = timeline.origin_manifest_sequence;
    Ok(HlsManifestRecoveryCandidateScoreReport {
        media_sequence,
        quality: evaluate_manifest_origin_quality_with_mode(
            session,
            fetched,
            timeline,
            context,
            current_time_millis(),
            acceptance_mode,
        ),
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
    let fresh_pinned_revalidation = matches!(acceptance_mode, HlsManifestCommitAcceptanceMode::FreshPinnedRevalidation);
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
    let previous_highwater = if fresh_baseline || host_relation == HlsManifestOriginRelation::OtherRedirectHost {
        None
    } else {
        session.origin_seq_highwater
    };
    let continuity_mode = if fresh_baseline || fresh_pinned_revalidation {
        HlsManifestContinuityMode::RebaseAllowed
    } else {
        manifest_continuity_mode(session, now_ms)
    };
    let allowed_forward_window = allowed_manifest_forward_window(session, context, Some(&fetched.body));
    let sequence_relation = if host_relation == HlsManifestOriginRelation::OtherRedirectHost {
        HlsManifestSequenceRelation::NoPreviousHighwater
    } else {
        classify_manifest_sequence_relation(
            previous_highwater,
            origin_highwater,
            allowed_forward_window,
            continuity_mode,
        )
    };
    let reject_reason =
        manifest_quality_reject_reason(sequence_relation, previous_highwater, origin_highwater, allowed_forward_window);
    let score = manifest_origin_quality_score(host_relation, sequence_relation, reject_reason.is_some());
    HlsManifestOriginQuality {
        score,
        host_relation,
        sequence_relation,
        effective_host,
        origin_highwater,
        previous_highwater,
        allowed_forward_window,
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
    if !same_host {
        return HlsManifestOriginQualityScore::OtherHostCandidate;
    }
    match sequence_relation {
        HlsManifestSequenceRelation::Next => HlsManifestOriginQualityScore::SameHostNextSequence,
        HlsManifestSequenceRelation::Rebase => HlsManifestOriginQualityScore::SameHostRebase,
        HlsManifestSequenceRelation::NoPreviousHighwater | HlsManifestSequenceRelation::PlausibleForward => {
            HlsManifestOriginQualityScore::SameHostPlausibleForward
        }
        HlsManifestSequenceRelation::RolloverCandidate => HlsManifestOriginQualityScore::SameHostRolloverCandidate,
        HlsManifestSequenceRelation::Same => HlsManifestOriginQualityScore::SameHostUnchanged,
        HlsManifestSequenceRelation::NoOriginHighwater
        | HlsManifestSequenceRelation::ForwardTooFar
        | HlsManifestSequenceRelation::Backward => HlsManifestOriginQualityScore::Rejected,
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
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
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
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
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
        host.map_or_else(|| "none".to_string(), hls_origin_log_value),
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
        "Manifest '{}' attempting URL attempt initial: request_url={} reason=origin-refresh",
        session_label,
        hls_origin_log_value(input_source.url.as_str())
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
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

pub(crate) fn format_optional_highwater(highwater: Option<u64>) -> String {
    highwater.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn recovery_burst_plan(
    context: &HlsOriginManifestFetchContext,
    current_burst_is_full_plan: bool,
) -> HlsManifestRecoveryBurstPlan {
    if current_burst_is_full_plan {
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
        | OriginManifestFetchError::RecoveryUnavailable { .. }
        | OriginManifestFetchError::DeterministicTimelineConflict(_)
        | OriginManifestFetchError::ContentCoding(_)
        | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
        | OriginManifestFetchError::InvalidUtf8 { .. } => false,
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
    binding: &HlsManifestOriginBinding,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    log_context: ManifestRecoveryAttemptLogContext,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let target_url = binding.request_url();
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let reason =
        reject_reason.map_or_else(|| "pinned-host-recovery".to_string(), HlsManifestRejectLogReason::status_label);
    if log_context.candidates > 1 {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {} candidate {} of {}: request_url={} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            log_context.candidate_index + 1,
            log_context.candidates,
            hls_origin_log_value(target_url.as_str()),
            reason
        );
    } else {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {}: request_url={} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            hls_origin_log_value(target_url.as_str()),
            reason
        );
    }
    match target_url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(OriginManifestFetchError::Request(
                "invalid non-HTTP manifest recovery binding".to_string(),
            ));
        }
    }
    timeout(
        Duration::from_millis(context.origin_manifest_timeout_ms.max(1)),
        fetch_origin_manifest_once(
            target_url,
            &context.headers,
            &context.client,
            &context.no_redirect_client,
            context.use_manual_redirects,
            binding.provider_url_index(),
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

async fn response_to_fetched_manifest(
    response: reqwest::Response,
    provider_url_index: Option<usize>,
    resolved_request_url: Url,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let status = response.status();
    debug!(
        "HLS origin manifest response received: request_url={} final_url={} status={}",
        hls_origin_log_value(resolved_request_url.as_str()),
        hls_origin_log_value(response.url().as_str()),
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
                candidate_requests: 1,
                selection: HlsManifestFetchSelection::Initial,
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
                "HLS provider URL resolution returned invalid URL: error={} request_url={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                hls_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }),
        Err(err) => {
            debug!(
                "HLS provider URL resolution failed: error={} request_url={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                hls_origin_log_value(input_source.url.as_str())
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
fn log_origin_refresh_retry_scheduled(
    origin_entry: &LiveHlsOriginEntry,
    attempt_index: usize,
    delay_ms: u64,
    detail: impl AsRef<str>,
) {
    warn!(
        "HLS origin manifest refresh retry scheduled: request_url={} attempt={} {} delay_ms={delay_ms}",
        hls_origin_log_value(origin_entry.url().as_str()),
        attempt_index + 1,
        detail.as_ref()
    );
}

#[cfg(test)]
mod tests {
    use super::{
        acceptance_attempt_may_start, attempt_limit_for_started_requalification, build_manifest_timeline_fingerprint,
        candidate_resource_timeline_evidence, deterministic_conflict_fingerprint,
        deterministic_timeline_conflict_from_rejection, fetch_origin_manifest_once, origin_manifest_content_coding_error,
        origin_manifest_fetch_error_from_io_error, origin_manifest_fetch_error_from_request_error,
        request_failed_status_from_message,
        selected_manifest_candidate, HlsEmergencyLiveHandoffCompatibility, HlsManifestCommitKind, HlsManifestCommitPlan,
        FetchedOriginManifest, HlsManifestFetchSelection, HlsManifestRejectLogReason, HlsMediaResourceIdentity,
        HlsMediaResourceSemanticKey, HlsResourceReplayDecision, HlsResourceTimelineEvidence,
        HlsTerminalAlternativeCompatibility, ManifestRetryLogKind, OriginManifestFetchError,
        OriginManifestParseOutcome, RetryPolicy, TimelineMapError, MAX_HLS_MANIFEST_BYTES,
    };
    use crate::{
        api::model::hls_cache::recovery_timing::HlsAcceptanceDeadlineMs,
        utils::content_coding::{ContentCoding, ContentCodingError},
    };
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

    #[test]
    fn hls_recovery_timing_deadline_never_abandons_started_full_burst_but_blocks_late_follow_ups() {
        let deadline = HlsAcceptanceDeadlineMs::from_millis_since_epoch;
        assert!(acceptance_attempt_may_start(true, 1_501, 500, deadline(1_000)));

        let reduced_retry_before_deadline = acceptance_attempt_may_start(false, 1_000, 499, deadline(1_500));
        let reduced_retry_at_deadline = acceptance_attempt_may_start(false, 1_000, 500, deadline(1_500));
        let requalification_after_deadline = acceptance_attempt_may_start(false, 1_501, 0, deadline(1_500));
        let saturated_follow_up = acceptance_attempt_may_start(false, u64::MAX - 5, 10, deadline(u64::MAX));

        assert!(reduced_retry_before_deadline);
        assert!(!reduced_retry_at_deadline);
        assert!(!requalification_after_deadline);
        assert!(!saturated_follow_up);
    }

    #[test]
    fn requalification_in_last_retry_slot_reserves_exactly_one_mandatory_full_burst() {
        assert_eq!(attempt_limit_for_started_requalification(5, 1), 5);
        assert_eq!(attempt_limit_for_started_requalification(5, 4), 6);
        assert_eq!(attempt_limit_for_started_requalification(usize::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn encrypted_normal_candidate_is_not_critical_emergency_handoff_evidence() {
        let encrypted =
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n";
        let (_, has_switch_segment, evidence) =
            build_manifest_timeline_fingerprint(encrypted, "http://origin.example/live/index.m3u8");

        assert!(has_switch_segment);
        assert_eq!(evidence.live_handoff, HlsEmergencyLiveHandoffCompatibility::Incompatible);
        assert_eq!(evidence.terminal_alternative, HlsTerminalAlternativeCompatibility::TerminalTailPreferred);
    }

    #[test]
    fn stage_alternative_is_forwarded_to_commit_callback_selection() {
        let plan = HlsManifestCommitPlan::StageAlternative {
            candidate_index: 7,
            kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
        };

        assert_eq!(selected_manifest_candidate(plan), Some((7, HlsManifestCommitKind::AlternativeAsNewEpoch)));
    }

    #[test]
    fn timeline_fingerprint_is_structured_and_ignores_origin_host_and_query_tokens() {
        let manifest_a = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n\
            #EXT-X-PROGRAM-DATE-TIME:2026-07-16T10:00:00Z\n#EXTINF:4,\n\
            https://origin-a.example/live/7.ts?token=secret-a\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\n\
            https://origin-a.example/live/8.ts?token=secret-a\n";
        let manifest_b = manifest_a
            .replace("origin-a.example", "origin-b.example")
            .replace("secret-a", "secret-b")
            .replace("#EXT-X-TARGETDURATION:4", "#EXT-X-TARGETDURATION:9");

        let (fingerprint_a, has_media_a, _) =
            build_manifest_timeline_fingerprint(manifest_a, "https://origin-a.example/live/index.m3u8");
        let (fingerprint_b, has_media_b, _) =
            build_manifest_timeline_fingerprint(&manifest_b, "https://origin-b.example/live/index.m3u8");

        assert!(has_media_a);
        assert!(has_media_b);
        assert_eq!(fingerprint_a, fingerprint_b);
        assert_eq!(fingerprint_a.segment_count, 2);
        assert_eq!(fingerprint_a.first_program_date_time_ms, Some(1_784_196_000_000));
        assert!(fingerprint_a.segment_samples[1].discontinuity_before);
    }

    #[test]
    fn rotating_volatile_parent_has_one_semantic_conflict_fingerprint() {
        let body_a = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n\
            /stream/0123456789abcdef/1745190_490.ts\n#EXTINF:4,\n\
            /stream/0123456789abcdef/1745180_480.ts\n#EXTINF:4,\n\
            /stream/0123456789abcdef/1745191_491.ts\n";
        let body_b = body_a.replace("0123456789abcdef", "fedcba9876543210");
        let parsed_a = match crate::processing::parser::hls::origin_manifest::parse_origin_media_manifest(
            body_a,
            "https://origin-a.example/live/index.m3u8",
        ) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
        };
        let parsed_b = match crate::processing::parser::hls::origin_manifest::parse_origin_media_manifest(
            &body_b,
            "https://origin-b.example/live/index.m3u8",
        ) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
        };

        assert_eq!(
            deterministic_conflict_fingerprint(&parsed_a, body_a, "https://origin-a.example/live/index.m3u8"),
            deterministic_conflict_fingerprint(&parsed_b, &body_b, "https://origin-b.example/live/index.m3u8")
        );
        assert_ne!(
            build_manifest_timeline_fingerprint(body_a, "https://origin-a.example/live/index.m3u8").0,
            build_manifest_timeline_fingerprint(&body_b, "https://origin-b.example/live/index.m3u8").0,
            "ordinary origin-acceptance fingerprint remains exact-path based"
        );
    }

    #[test]
    fn different_stream_namespace_is_not_the_same_conflict() {
        let body_a = "#EXTM3U\n#EXTINF:4,\n/stream-a/0123456789abcdef/1745180_480.ts\n";
        let body_b = body_a.replace("stream-a", "stream-b");
        let parse = |body: &str| {
            match crate::processing::parser::hls::origin_manifest::parse_origin_media_manifest(
                body,
                "https://origin.example/live/index.m3u8",
            ) {
                OriginManifestParseOutcome::Normal(manifest) => manifest,
                OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
            }
        };

        assert_ne!(
            deterministic_conflict_fingerprint(
                &parse(body_a),
                body_a,
                "https://origin.example/live/index.m3u8",
            ),
            deterministic_conflict_fingerprint(
                &parse(&body_b),
                &body_b,
                "https://origin.example/live/index.m3u8",
            )
        );
    }

    #[test]
    fn resource_timeline_evidence_rejects_replay_after_new_even_with_forward_sequence() {
        let published = HlsMediaResourceIdentity::from_url("https://old.example/live/484.ts", None);
        let replay_only = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n484.ts\n";
        let prefix_then_new =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n484.ts\n#EXTINF:4,\n490.ts\n";
        let new_then_replay =
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n490.ts\n#EXTINF:4,\n484.ts\n";
        let fingerprint = |body| {
            build_manifest_timeline_fingerprint(body, "https://new.example/live/index.m3u8").0
        };

        assert_eq!(
            candidate_resource_timeline_evidence(&fingerprint(replay_only), &[published]),
            HlsResourceTimelineEvidence::ReplayOnly
        );
        assert_eq!(
            candidate_resource_timeline_evidence(&fingerprint(prefix_then_new), &[published]),
            HlsResourceTimelineEvidence::Eligible
        );
        assert_eq!(
            candidate_resource_timeline_evidence(&fingerprint(new_then_replay), &[published]),
            HlsResourceTimelineEvidence::ContradictoryOrder
        );
    }

    #[test]
    fn resource_replay_diagnostic_is_bounded_and_contains_decision_evidence() {
        let reason = HlsManifestRejectLogReason::from(TimelineMapError::PublishedResourceReplay {
            previous_proxy_tail: Some(23),
            existing_proxy_seq: 17,
            candidate_position: 2,
            candidate_origin_seq: 490,
            resource_key: HlsMediaResourceSemanticKey::for_test([0xab; 32]),
            decision: HlsResourceReplayDecision::RejectContradictoryOrder,
        })
        .status_label();

        assert!(reason.contains("previous_proxy_tail=23"));
        assert!(reason.contains("candidate_position=2"));
        assert!(reason.contains("repeated_resource=abababababababab"));
        assert!(reason.contains("decision=reject-contradictory-order"));
        assert!(!reason.contains("http"));
    }

    #[test]
    fn deterministic_conflict_rejects_matching_log_token_with_different_full_semantic_key() {
        let fetched = FetchedOriginManifest {
            body: "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
                   #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n"
                .to_string(),
            final_manifest_url: "https://origin.example/live/index.m3u8".to_string(),
            resolved_request_url: "https://origin.example/live/index.m3u8".to_string(),
            redirect_host: None,
            provider_url_index: None,
            provider_session_headers: HeaderMap::new(),
            status: StatusCode::OK,
            attempts: 1,
            candidate_requests: 1,
            selection: HlsManifestFetchSelection::Initial,
        };
        let actual_key =
            HlsMediaResourceIdentity::from_url("https://origin.example/live/480.ts", None).semantic_key();
        let mut different_bytes = actual_key.bytes();
        different_bytes[31] ^= 0xff;
        let different_key = HlsMediaResourceSemanticKey::for_test(different_bytes);
        assert_eq!(actual_key.diagnostic_token(), different_key.diagnostic_token());
        assert_ne!(actual_key, different_key);

        let reason = HlsManifestRejectLogReason::PublishedResourceReplay {
            previous_proxy_tail: Some(2),
            existing_proxy_seq: 0,
            candidate_position: 1,
            candidate_origin_seq: 480,
            resource_key: different_key,
            decision: HlsResourceReplayDecision::RejectContradictoryOrder,
        };
        assert!(deterministic_timeline_conflict_from_rejection(&fetched, &reason).is_none());
    }

    #[test]
    fn timeline_fingerprint_distinguishes_technical_state_and_keeps_compatible_aes_media_stageable() {
        let clear_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4,\n1.ts\n";
        let discontinuous_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\n1.ts\n";
        let encrypted_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/key.bin?token=secret\"\n#EXTINF:4,\n1.ts\n";
        let mapped = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\n1.m4s\n";

        let (clear, _, _) = build_manifest_timeline_fingerprint(clear_ts, "https://origin.example/live/index.m3u8");
        let (discontinuous, _, _) =
            build_manifest_timeline_fingerprint(discontinuous_ts, "https://origin.example/live/index.m3u8");
        let (encrypted, encrypted_has_media, _) =
            build_manifest_timeline_fingerprint(encrypted_ts, "https://origin.example/live/index.m3u8");
        let (mapped, _, _) = build_manifest_timeline_fingerprint(mapped, "https://origin.example/live/index.m3u8");

        assert_ne!(clear.discontinuity_pattern_hash, discontinuous.discontinuity_pattern_hash);
        assert_ne!(clear.map_and_encryption_hash, encrypted.map_and_encryption_hash);
        assert_ne!(clear.map_and_encryption_hash, mapped.map_and_encryption_hash);
        assert_ne!(clear.container_signature_hash, mapped.container_signature_hash);
        assert!(encrypted_has_media);
    }

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
