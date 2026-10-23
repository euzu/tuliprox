use super::error::{HlsManifestAcceptanceRejectReason, HlsManifestRejectLogReason, OriginManifestFetchError};
use crate::{
    recovery_timing::{HlsAcceptanceEpisodeTimingSeed, HlsRecoveryTimingPolicy},
    session_store::HlsSessionHandle,
    timeline::TimelineMapError,
};
use axum::http::{HeaderMap, StatusCode};
use reqwest::Client;
use shared::model::InputFetchMethod;
use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tuliprox_core::model::{AppConfig, ConfigProvider, HlsManifestRecoveryBurstConfig, InputSource, ProviderConfig};
use url::Url;

pub(super) const DEFAULT_HLS_TARGET_DURATION_SECS: u32 = 15;

/// One logical client attempt, shared by fetch, qualification and availability owners.
/// Its original playback contract cannot be weakened by a later qualification.
#[derive(Debug)]
pub struct HlsManifestAttemptContext {
    pub requirement: crate::HlsManifestCommitRequirement,
    pub active_live_playback: bool,
    pub revalidation_generation: Option<u64>,
    attempts: AtomicUsize,
    closed: AtomicBool,
    failed: AtomicBool,
    admission_backoff_paid: AtomicBool,
}

impl PartialEq for HlsManifestAttemptContext {
    fn eq(&self, other: &Self) -> bool { std::ptr::eq(self, other) }
}
impl Eq for HlsManifestAttemptContext {}

impl HlsManifestAttemptContext {
    pub fn new(
        requirement: crate::HlsManifestCommitRequirement,
        active_live_playback: bool,
        revalidation_generation: Option<u64>,
    ) -> Self {
        Self {
            requirement,
            active_live_playback,
            revalidation_generation,
            attempts: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            admission_backoff_paid: AtomicBool::new(false),
        }
    }

    pub fn is_closed(&self) -> bool { self.closed.load(Ordering::Acquire) }
    pub fn failed(&self) -> bool { self.failed.load(Ordering::Acquire) }
    pub fn finish(&self, failed: bool) {
        self.failed.store(failed, Ordering::Release);
        self.closed.store(true, Ordering::Release);
    }
    pub fn permits_407_retry(&self) -> bool {
        self.requirement == crate::HlsManifestCommitRequirement::CommittedManifestAllowed && self.active_live_playback
    }
    pub fn logical_attempts(&self) -> usize { self.attempts.load(Ordering::Acquire) }

    pub fn remaining_attempts(&self) -> usize {
        RetryPolicy::default().attempt_count().saturating_sub(self.logical_attempts())
    }

    pub(crate) fn begin_logical_attempt(&self) -> bool {
        let started = !self.is_closed()
            && self
                .attempts
                .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    (used < RetryPolicy::default().attempt_count()).then_some(used + 1)
                })
                .is_ok();
        if started {
            self.admission_backoff_paid.store(false, Ordering::Release);
        }
        started
    }
    pub(crate) fn mark_admission_backoff_paid(&self) { self.admission_backoff_paid.store(true, Ordering::Release); }
    pub(super) fn admission_backoff_paid(&self) -> bool { self.admission_backoff_paid.load(Ordering::Acquire) }
    pub(crate) fn classify_error(&self, error: OriginManifestFetchError) -> OriginManifestFetchError {
        let error = match error {
            OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, _)
                if !self.permits_407_retry() =>
            {
                OriginManifestFetchError::NonRetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
            }
            error => error,
        };
        if crate::refresh::failure::manifest_hard_fetch_error(&error) {
            self.finish(true);
        }
        error
    }
}

const DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS: u64 = 300;
pub(super) const HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT: usize = 64;
pub use crate::manifest_limits::MAX_HLS_MANIFEST_BYTES;

/// Origin manifest entrypoint snapshot for live HLS refreshes.
#[derive(Clone)]
pub struct LiveHlsOriginEntry {
    url: Url,
    url_failover_provider: Option<Arc<ConfigProvider>>,
    runtime_provider_config: Option<Arc<ProviderConfig>>,
}

impl LiveHlsOriginEntry {
    pub fn parse(url: &str) -> Option<Self> { Self::parse_with_url_failover_provider(url, None) }

    pub fn parse_with_url_failover_provider(
        url: &str,
        url_failover_provider: Option<Arc<ConfigProvider>>,
    ) -> Option<Self> {
        Self::parse_with_provider_configs(url, url_failover_provider, None)
    }

    pub fn parse_with_provider_configs(
        url: &str,
        url_failover_provider: Option<Arc<ConfigProvider>>,
        runtime_provider_config: Option<Arc<ProviderConfig>>,
    ) -> Option<Self> {
        Url::parse(url).ok().map(|url| Self { url, url_failover_provider, runtime_provider_config })
    }

    pub fn url(&self) -> &Url { &self.url }

    pub fn url_failover_provider(&self) -> Option<&Arc<ConfigProvider>> { self.url_failover_provider.as_ref() }

    pub fn to_input_source(&self) -> InputSource {
        let user_info = if self.url.scheme() == "provider" {
            self.runtime_provider_config.as_ref().and_then(|provider| provider.get_user_info())
        } else {
            None
        };
        InputSource {
            name: Arc::<str>::from("hls-origin"),
            url: self.url.to_string(),
            // In this HLS context, InputSource.provider is the URL-failover provider from source.yml,
            // not a runtime origin-account provider.
            provider: self.url_failover_provider.clone(),
            username: user_info.as_ref().map(|info| info.username.clone()),
            password: user_info.map(|info| info.password),
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
            .field(
                "runtime_provider_config",
                &self.runtime_provider_config.as_ref().map(|provider| provider.name.as_ref()),
            )
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

/// Work performed by a recovery chain, including chains that selected no manifest.
#[derive(Default)]
pub(crate) struct HlsManifestRecoveryDiagnostics {
    attempts: usize,
    candidate_requests: usize,
    selection: Option<HlsManifestFetchSelection>,
}

impl HlsManifestRecoveryDiagnostics {
    pub(super) const fn candidate_requests(&self) -> usize { self.candidate_requests }

    pub(super) fn record_attempt(&mut self, attempts: usize, candidate_requests: usize, burst: bool) {
        self.attempts = attempts;
        self.candidate_requests = self.candidate_requests.saturating_add(candidate_requests);
        self.selection =
            Some(if burst { HlsManifestFetchSelection::Burst } else { HlsManifestFetchSelection::Recovery });
    }

    pub(crate) fn include_initial_fetch(&self, initial: &FetchedOriginManifest, selected: &mut FetchedOriginManifest) {
        selected.attempts = if self.selection.is_some() {
            self.attempts.saturating_add(initial.attempts.saturating_sub(1))
        } else {
            initial.attempts
        };
        selected.candidate_requests = self.candidate_requests.saturating_add(initial.candidate_requests);
        selected.selection = self.selection.unwrap_or(initial.selection);
    }
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

    pub(super) fn with_recovery_diagnostics(
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
    /// Identity captured by the refresh owner; retries must not send an obsolete entry URL.
    pub origin_account_snapshot: Option<crate::origin_account_runtime::HlsOriginAccountSnapshot>,
    pub attempt: Option<Arc<HlsManifestAttemptContext>>,
    pub app_config: Arc<AppConfig>,
    pub session: HlsSessionHandle,
    /// Availability owner used to react to failed attempts while the logical fetch is still in flight.
    pub availability_runtime: Option<crate::hls_ctx::WeakHlsCtx>,
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

impl HlsOriginManifestFetchContext {
    pub(crate) async fn classify_current_error(&self, error: OriginManifestFetchError) -> OriginManifestFetchError {
        let session = self.session.read().await;
        if self.origin_account_snapshot.as_ref().is_some_and(|snapshot| !snapshot.matches_session_identity(&session)) {
            return OriginManifestFetchError::RecoveryUnavailable {
                reason: super::error::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            };
        }
        // A hard failure closes the shared attempt while its binding is still current.
        // A handoff cannot interleave between the identity check and that close.
        match self.attempt.as_ref() {
            Some(attempt) => attempt.classify_error(error),
            None => error,
        }
    }
    pub(crate) async fn ensure_current_origin(&self) -> Result<(), OriginManifestFetchError> {
        if let Some(snapshot) = self.origin_account_snapshot.as_ref() {
            if !snapshot.matches_session_identity(&*self.session.read().await) {
                return Err(OriginManifestFetchError::RecoveryUnavailable {
                    reason: super::error::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
                });
            }
        }
        Ok(())
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
    pub(super) const fn as_log_value(self) -> &'static str {
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

pub(super) fn request_hls_session_idle_timeout_secs_from_config(app_config: &AppConfig) -> u64 {
    app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.hls_cache.as_ref())
        .map_or(DEFAULT_HLS_SESSION_IDLE_TIMEOUT_SECS, |hls_cache| hls_cache.session_idle_timeout.get())
        .max(1)
}

pub fn fetched_effective_manifest_host(fetched: &FetchedOriginManifest) -> Option<String> {
    if fetched.redirect_host.is_some() {
        return fetched.redirect_host.clone();
    }
    Url::parse(&fetched.resolved_request_url).ok().and_then(|url| url.host_str().map(str::to_string))
}
