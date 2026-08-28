use super::{
    manifest_acceptance::HlsManifestAcceptanceGeneration,
    media_reserve::{
        HlsLeaseManifestSnapshot, HlsLeasePlaybackCursor, HlsLeaseReserveSnapshot, HlsManifestDeliveryMode,
        HlsPlaybackCompletionOutcome, HlsPlaybackRequestToken,
    },
    recovery_timing::{
        HlsLeaseCutoverTiming, HlsTerminalCommitWindow, HlsTerminalMediaPreparationKey,
        HlsTerminalMediaPreparationState,
    },
    runtime_custom_tail::{HlsFiniteTailTrigger, HlsRuntimeCustomTailBasePolicy, HlsRuntimeCustomTailReason},
    terminal_commit::HlsTerminalCommitOutcome,
    terminal_tail::{
        HlsLeasePlaybackMode, HlsTerminalAssetIdentity, HlsTerminalTailCompatibility, HlsTerminalTailGeneration,
        HlsTerminalTailPlan,
    },
    HlsEffectiveOriginAcquirePolicy, ProxySessionId,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use std::{collections::HashMap, fmt, sync::Arc};
use tuliprox_session::ConnectionKind;

const HLS_ACCESS_LEASE_ID_BYTES: usize = 16;

/// Monotonic identity of the lease/cursor evidence used for one proxy
/// session's availability decision. The zero value denotes a session without
/// stored lease evidence; committed generations are process-lifetime unique.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct HlsAvailabilityEvidenceGeneration(u64);

impl HlsAvailabilityEvidenceGeneration {
    const NONE: Self = Self(0);

    #[cfg(any(test, feature = "test-support"))]
    pub const fn for_test(generation: u64) -> Self {
        Self(generation)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsAvailabilityEvidenceAdvanceError {
    Exhausted,
}

/// Short opaque lookup key for a server-side HLS access lease.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct HlsAccessLeaseId(pub String);

impl fmt::Debug for HlsAccessLeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HlsAccessLeaseId").field(&"<redacted>").finish()
    }
}

/// Stable user/player family used only for diagnostics or future UX grouping.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HlsPlaybackFamilyKey {
    pub username: String,
    pub client_fingerprint: String,
}

impl HlsPlaybackFamilyKey {
    pub fn new(username: impl Into<String>, client_fingerprint: impl Into<String>) -> Self {
        Self { username: username.into(), client_fingerprint: client_fingerprint.into() }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessLeaseState {
    Pending,
    Activated,
    Idle,
    PolicyRevoking,
    Expired,
    Denied,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsLeaseStartupAdmissionState {
    Pending,
    Admitted,
}

impl HlsAccessLeaseState {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Activated => "Activated",
            Self::Idle => "Idle",
            Self::PolicyRevoking => "PolicyRevoking",
            Self::Expired => "Expired",
            Self::Denied => "Denied",
        }
    }
}

/// Immutable authorization evidence frozen before an active HLS entitlement is revoked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsRuntimePolicyRevocation {
    pub reason: HlsRuntimeCustomTailReason,
    pub lease_issued_at_ms: u64,
    pub expected_admission_generation: u64,
    pub manifest_snapshot_generation: u64,
    pub cursor_generation: u64,
    pub started_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HlsRuntimePolicyRevocationOutcome {
    Started { token: HlsRuntimePolicyRevocation },
    AlreadyPending { token: HlsRuntimePolicyRevocation },
    AlreadyCommitted { plan: Arc<HlsTerminalTailPlan> },
    NoPublishedManifest,
    NoLongerEligible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsAccessLeaseDenialMode {
    #[cfg(test)]
    ImmediateEnd,
    ImmediateRuntimePolicyEnd {
        reason: HlsRuntimeCustomTailReason,
    },
    PreserveCommittedFiniteTail,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HlsAccessLeaseDenialOutcome {
    UnknownLease,
    PolicyRevocationPending,
    FiniteDecisionPreserved,
    Ended { terminal_release: Option<HlsDeniedTerminalTailRelease> },
}

/// Explains why a canonical HLS request required a newly committed manifest.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsFreshManifestRequiredReason {
    ColdStart,
    ExpiredRevalidation,
    PreviousHardManifestFailure,
    ProvisioningHandoff,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsAccessLeaseTiming {
    pub active_window_ms: u64,
    pub valid_window_ms: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsAccessLeasePendingDeadline {
    Bootstrap { deadline_ms: u64 },
    FollowUp { deadline_ms: u64 },
}

impl HlsAccessLeasePendingDeadline {
    pub const fn deadline_ms(self) -> u64 {
        match self {
            Self::Bootstrap { deadline_ms } | Self::FollowUp { deadline_ms } => deadline_ms,
        }
    }

    const fn tightened_with(self, candidate: Self) -> Self {
        let deadline_ms =
            if self.deadline_ms() <= candidate.deadline_ms() { self.deadline_ms() } else { candidate.deadline_ms() };
        match (self, candidate) {
            (Self::FollowUp { .. }, _) | (_, Self::FollowUp { .. }) => Self::FollowUp { deadline_ms },
            (Self::Bootstrap { .. }, Self::Bootstrap { .. }) => Self::Bootstrap { deadline_ms },
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLease {
    pub lease_id: HlsAccessLeaseId,
    pub family_key: HlsPlaybackFamilyKey,
    pub proxy_session_id: ProxySessionId,
    pub username: String,
    pub user_session_token: String,
    pub input_id: u16,
    pub stream_ref: String,
    pub virtual_id: u32,
    pub known_bitrate_bps: Option<u32>,
    pub origin_connection_kind: ConnectionKind,
    pub origin_priority: i8,
    pub state: HlsAccessLeaseState,
    pub issued_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub active_until_ms: Option<u64>,
    pub pending_deadline: Option<HlsAccessLeasePendingDeadline>,
    pub valid_until_ms: u64,
    pub epg_reference_ts: Option<i64>,
    pub archive_origin_url: Option<String>,
    pub playback_mode: HlsLeasePlaybackMode,
    pub startup_admission: HlsLeaseStartupAdmissionState,
    pub playback_cursor: HlsLeasePlaybackCursor,
    pub last_manifest_snapshot: Option<HlsLeaseManifestSnapshot>,
    manifest_snapshot_generation: u64,
    pub admission_generation: u64,
    pub runtime_policy_revocation: Option<HlsRuntimePolicyRevocation>,
    runtime_policy_denial_reason: Option<HlsRuntimeCustomTailReason>,
    /// Retained until session cleanup is acknowledged so cancellation cannot orphan a GC pin.
    pending_terminal_protection_release: Option<HlsTerminalTailGeneration>,
}

/// Exact lease incarnation and playback generation authorized to account one
/// media response. A completion from an older live/terminal generation cannot
/// be rebound to the lease's current state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsMediaLeaseIdentity {
    issued_at_ms: u64,
    playback: HlsMediaLeasePlaybackIdentity,
}

impl HlsMediaLeaseIdentity {
    pub const fn is_live(self) -> bool {
        matches!(self.playback, HlsMediaLeasePlaybackIdentity::Live { .. })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsMediaLeasePlaybackIdentity {
    Live { admission_generation: u64 },
    TerminalTail { generation: HlsTerminalTailGeneration },
}

/// Lease incarnation and admission generation observed before building a manifest response.
///
/// The store accepts the corresponding snapshot only while this exact live lease identity is
/// current. This prevents a delayed request from publishing into a replacement or terminal lease.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsLeaseManifestPublicationGuard {
    issued_at_ms: u64,
    admission_generation: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsLeaseManifestPublicationRejectReason {
    UnknownLease,
    SessionMismatch,
    LeaseExpired,
    LeaseUnavailable,
    LeaseIncarnationChanged,
    AdmissionGenerationChanged,
    LeaseNotLive,
    SourceRegressive,
    SnapshotGenerationExhausted,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[must_use]
pub enum HlsLeaseManifestPublicationOutcome {
    Committed { snapshot_generation: u64 },
    Rejected(HlsLeaseManifestPublicationRejectReason),
}

impl HlsLeaseManifestPublicationOutcome {
    pub const fn is_committed(self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    pub const fn snapshot_generation(self) -> Option<u64> {
        match self {
            Self::Committed { snapshot_generation } => Some(snapshot_generation),
            Self::Rejected(_) => None,
        }
    }
}

impl HlsAccessLease {
    #[allow(clippy::too_many_arguments)]
    pub fn pending(
        lease_id: HlsAccessLeaseId,
        family_key: HlsPlaybackFamilyKey,
        proxy_session_id: ProxySessionId,
        username: String,
        user_session_token: String,
        input_id: u16,
        stream_ref: String,
        virtual_id: u32,
        now_ms: u64,
        valid_window_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            family_key,
            proxy_session_id,
            username,
            user_session_token,
            input_id,
            stream_ref,
            virtual_id,
            known_bitrate_bps: None,
            origin_connection_kind: ConnectionKind::Normal,
            origin_priority: 0,
            state: HlsAccessLeaseState::Pending,
            issued_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            active_until_ms: None,
            pending_deadline: Some(HlsAccessLeasePendingDeadline::Bootstrap {
                deadline_ms: now_ms.saturating_add(valid_window_ms),
            }),
            valid_until_ms: now_ms.saturating_add(valid_window_ms),
            epg_reference_ts: None,
            archive_origin_url: None,
            playback_mode: HlsLeasePlaybackMode::Live,
            startup_admission: HlsLeaseStartupAdmissionState::Pending,
            playback_cursor: HlsLeasePlaybackCursor::default(),
            last_manifest_snapshot: None,
            manifest_snapshot_generation: 0,
            admission_generation: 0,
            runtime_policy_revocation: None,
            runtime_policy_denial_reason: None,
            pending_terminal_protection_release: None,
        }
    }

    pub fn with_archive_playback(mut self, epg_reference_ts: Option<i64>, archive_origin_url: Option<String>) -> Self {
        self.epg_reference_ts = epg_reference_ts;
        self.archive_origin_url = archive_origin_url;
        self
    }

    pub const fn with_origin_acquire_policy(mut self, connection_kind: ConnectionKind, priority: i8) -> Self {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
        self
    }

    pub const fn with_known_bitrate_bps(mut self, known_bitrate_bps: Option<u32>) -> Self {
        self.known_bitrate_bps = match known_bitrate_bps {
            Some(0) | None => None,
            Some(bitrate_bps) => Some(bitrate_bps),
        };
        self
    }

    pub fn update_origin_acquire_policy(&mut self, connection_kind: ConnectionKind, priority: i8) {
        self.origin_connection_kind = connection_kind;
        self.origin_priority = priority;
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.issued_at_ms)
    }

    pub fn media_identity(&self) -> Option<HlsMediaLeaseIdentity> {
        let playback = match &self.playback_mode {
            HlsLeasePlaybackMode::Live => {
                HlsMediaLeasePlaybackIdentity::Live { admission_generation: self.admission_generation }
            }
            HlsLeasePlaybackMode::TerminalTail(plan) => {
                HlsMediaLeasePlaybackIdentity::TerminalTail { generation: plan.generation }
            }
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => return None,
        };
        Some(HlsMediaLeaseIdentity { issued_at_ms: self.issued_at_ms, playback })
    }

    pub fn runtime_policy_revocation_outcome(&self) -> Option<HlsRuntimePolicyRevocationOutcome> {
        match &self.playback_mode {
            HlsLeasePlaybackMode::TerminalTail(plan) => {
                Some(HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { plan: Arc::clone(plan) })
            }
            HlsLeasePlaybackMode::Live => self
                .runtime_policy_revocation
                .clone()
                .map(|token| HlsRuntimePolicyRevocationOutcome::AlreadyPending { token }),
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => None,
        }
    }

    pub fn runtime_policy_denial_reason(&self) -> Option<HlsRuntimeCustomTailReason> {
        match &self.playback_mode {
            HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.reason),
            HlsLeasePlaybackMode::Live => self.runtime_policy_revocation.as_ref().map(|token| token.reason),
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
                self.runtime_policy_denial_reason
            }
        }
    }

    pub fn permits_unpublished_standalone_tail(&self, reason: HlsRuntimeCustomTailReason) -> bool {
        reason.permits_unpublished_lease_standalone_tail()
            && self.state == HlsAccessLeaseState::Pending
            && self.playback_mode == HlsLeasePlaybackMode::Live
            && self.startup_admission == HlsLeaseStartupAdmissionState::Pending
            && self.last_manifest_snapshot.is_none()
            && self.runtime_policy_revocation.is_none()
    }

    fn manifest_publication_guard(&self) -> Option<HlsLeaseManifestPublicationGuard> {
        (lease_state_allows_use(self.state) && self.playback_mode == HlsLeasePlaybackMode::Live).then_some(
            HlsLeaseManifestPublicationGuard {
                issued_at_ms: self.issued_at_ms,
                admission_generation: self.admission_generation,
            },
        )
    }

    pub fn pending_deadline_ms(&self) -> Option<u64> {
        self.pending_deadline.map(HlsAccessLeasePendingDeadline::deadline_ms)
    }

    fn validity_due_at_ms(&self) -> u64 {
        if self.state == HlsAccessLeaseState::Pending {
            self.pending_deadline_ms().unwrap_or(self.valid_until_ms)
        } else {
            self.valid_until_ms
        }
    }

    fn apply_pending_deadline(&mut self, deadline: HlsAccessLeasePendingDeadline) -> bool {
        let previous = self.pending_deadline;
        let deadline = self.pending_deadline.map_or(deadline, |current| current.tightened_with(deadline));
        self.pending_deadline = Some(deadline);
        self.valid_until_ms = deadline.deadline_ms();
        previous != self.pending_deadline
    }

    fn refresh_validity(&mut self, now_ms: u64) -> bool {
        if self.state != HlsAccessLeaseState::Expired && self.validity_due_at_ms() <= now_ms {
            self.state = HlsAccessLeaseState::Expired;
            if self.playback_mode == HlsLeasePlaybackMode::Live {
                self.end_playback();
            }
            return true;
        }
        false
    }

    fn end_playback(&mut self) {
        self.runtime_policy_revocation = None;
        let release_generation = match std::mem::replace(&mut self.playback_mode, HlsLeasePlaybackMode::Ended) {
            HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.generation),
            HlsLeasePlaybackMode::TerminalUnavailable { decision_generation, .. } => {
                Some(HlsTerminalTailGeneration(decision_generation))
            }
            HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => None,
        };
        if self.pending_terminal_protection_release.is_none() {
            self.pending_terminal_protection_release = release_generation;
        }
    }

    fn terminal_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        expected_issued_at_ms: u64,
        expected_admission_generation: u64,
        manifest_snapshot_generation: u64,
        cursor_generation: u64,
        _now_ms: u64,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        if !lease_state_allows_use(self.state)
            || self.proxy_session_id != *proxy_session_id
            || self.issued_at_ms != expected_issued_at_ms
            || self.playback_mode != HlsLeasePlaybackMode::Live
        {
            return Err(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
        }
        if self.admission_generation != expected_admission_generation
            || self.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
                != Some(manifest_snapshot_generation)
            || self.playback_cursor.cursor_generation != cursor_generation
        {
            return Err(HlsTerminalCommitOutcome::SupersededGeneration);
        }
        Ok(())
    }

    fn runtime_policy_tail_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        revocation: &HlsRuntimePolicyRevocation,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        if self.state != HlsAccessLeaseState::PolicyRevoking
            || self.proxy_session_id != *proxy_session_id
            || self.issued_at_ms != revocation.lease_issued_at_ms
            || self.playback_mode != HlsLeasePlaybackMode::Live
            || self.runtime_policy_revocation.as_ref() != Some(revocation)
            || preparation.runtime_policy_revocation.as_ref() != Some(revocation)
            || preparation.trigger != HlsFiniteTailTrigger::RuntimePolicy(revocation.reason)
        {
            return Err(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
        }
        if self.admission_generation != revocation.expected_admission_generation
            || preparation.expected_admission_generation != revocation.expected_admission_generation
            || self.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
                != Some(revocation.manifest_snapshot_generation)
            || preparation.manifest_snapshot_generation != revocation.manifest_snapshot_generation
            || self.playback_cursor.cursor_generation != revocation.cursor_generation
            || preparation.cursor_generation != revocation.cursor_generation
        {
            return Err(HlsTerminalCommitOutcome::SupersededGeneration);
        }
        Ok(())
    }

    fn terminal_decision_commit_preconditions_match(
        &mut self,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Result<(), HlsTerminalCommitOutcome> {
        match preparation.runtime_policy_revocation.as_ref() {
            Some(revocation) => {
                self.runtime_policy_tail_commit_preconditions_match(proxy_session_id, preparation, revocation)
            }
            None => self.terminal_commit_preconditions_match(
                proxy_session_id,
                preparation.lease_issued_at_ms,
                preparation.expected_admission_generation,
                preparation.manifest_snapshot_generation,
                preparation.cursor_generation,
                now_ms,
            ),
        }
    }
}

fn terminal_commit_existing_outcome(
    lease: &mut HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    preparation: &HlsTerminalTailPreparation,
    _now_ms: u64,
) -> Option<HlsTerminalCommitOutcome> {
    if lease.proxy_session_id != *proxy_session_id || lease.issued_at_ms != preparation.lease_issued_at_ms {
        return Some(HlsTerminalCommitOutcome::LeaseNoLongerEligible);
    }
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => None,
        HlsLeasePlaybackMode::TerminalTail(current) if current.generation.0 == preparation.decision_generation => {
            Some(HlsTerminalCommitOutcome::AlreadyCommitted)
        }
        HlsLeasePlaybackMode::TerminalUnavailable { decision_generation, .. }
            if *decision_generation == preparation.decision_generation =>
        {
            Some(HlsTerminalCommitOutcome::AlreadyCommitted)
        }
        HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
            Some(HlsTerminalCommitOutcome::SupersededGeneration)
        }
        HlsLeasePlaybackMode::Ended => Some(HlsTerminalCommitOutcome::LeaseNoLongerEligible),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTerminalTailPreparation {
    pub trigger: HlsFiniteTailTrigger,
    pub runtime_policy_revocation: Option<HlsRuntimePolicyRevocation>,
    pub lease_issued_at_ms: u64,
    pub decision_generation: u64,
    pub expected_admission_generation: u64,
    pub manifest_snapshot_generation: u64,
    pub cursor_generation: u64,
    pub origin_progress_generation: u64,
    pub media_readiness_generation: u64,
    pub origin_epoch: u64,
    pub last_media_progress_at_ms: Option<u64>,
    pub expected_acceptance_generation: HlsManifestAcceptanceGeneration,
    pub terminal_media_requirement_source: HlsTerminalMediaRequirementSource,
    pub cutover_timing: HlsLeaseCutoverTiming,
    pub commit_window: HlsTerminalCommitWindow,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_media_preparation: HlsTerminalMediaPreparationState,
    pub reserve: HlsLeaseReserveSnapshot,
    pub manifest_snapshot: HlsLeaseManifestSnapshot,
}

impl HlsTerminalTailPreparation {
    pub fn bind_ready_terminal_media_requirement(
        &mut self,
        prepared_key: HlsTerminalMediaPreparationKey,
    ) -> Result<(), HlsTerminalTailCompatibility> {
        let source = match self.terminal_media_requirement_source {
            HlsTerminalMediaRequirementSource::AcceptanceEpisode { .. } => {
                if self.required_terminal_media_key != Some(prepared_key) {
                    return Err(HlsTerminalTailCompatibility::AssetRevisionMismatch);
                }
                self.terminal_media_requirement_source
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
                if decision_generation == self.decision_generation =>
            {
                HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, asset: prepared_key.asset }
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, asset }
                if decision_generation == self.decision_generation && asset == prepared_key.asset =>
            {
                self.terminal_media_requirement_source
            }
            HlsTerminalMediaRequirementSource::CutoverSnapshotPending { .. }
            | HlsTerminalMediaRequirementSource::CutoverSnapshot { .. } => {
                return Err(HlsTerminalTailCompatibility::AssetRevisionMismatch);
            }
        };
        self.terminal_media_requirement_source = source;
        self.required_terminal_media_key = Some(prepared_key);
        self.terminal_media_preparation = HlsTerminalMediaPreparationState::Ready { key: prepared_key };
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalMediaRequirementOrigin {
    AcceptanceEpisode { generation: HlsManifestAcceptanceGeneration },
    CutoverSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalMediaRequirementSource {
    AcceptanceEpisode { generation: HlsManifestAcceptanceGeneration },
    CutoverSnapshotPending { decision_generation: u64 },
    CutoverSnapshot { decision_generation: u64, asset: HlsTerminalAssetIdentity },
}

impl HlsTerminalMediaRequirementSource {
    pub fn authorizes_tail(self, decision_generation: u64, prepared_key: HlsTerminalMediaPreparationKey) -> bool {
        match self {
            Self::AcceptanceEpisode { .. } => true,
            Self::CutoverSnapshotPending { .. } => false,
            Self::CutoverSnapshot { decision_generation: source_generation, asset } => {
                source_generation == decision_generation && asset == prepared_key.asset
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTerminalTailPreparationInput {
    pub trigger: HlsFiniteTailTrigger,
    pub expected_manifest_snapshot_generation: u64,
    pub expected_cursor_generation: u64,
    pub origin_progress_generation: u64,
    pub media_readiness_generation: u64,
    pub origin_epoch: u64,
    pub last_media_progress_at_ms: Option<u64>,
    pub expected_acceptance_generation: HlsManifestAcceptanceGeneration,
    pub terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin,
    pub cutover_timing: HlsLeaseCutoverTiming,
    pub commit_window: HlsTerminalCommitWindow,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_media_preparation: HlsTerminalMediaPreparationState,
    pub reserve: HlsLeaseReserveSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsDeniedTerminalTailRelease {
    pub proxy_session_id: ProxySessionId,
    pub generation: HlsTerminalTailGeneration,
}

/// Stable identity and pending GC cleanup required before deleting a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsAccessLeaseRemovalPreparation {
    pub proxy_session_id: ProxySessionId,
    pub issued_at_ms: u64,
    pub terminal_protection_generation: Option<HlsTerminalTailGeneration>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsAccessLeaseActivation {
    Activated { lease: Box<HlsAccessLease>, previous_state: HlsAccessLeaseState },
    Expired,
    Denied,
    UnknownLease,
    SessionMismatch,
}

impl HlsAccessLeaseActivation {
    pub const fn is_activated(&self) -> bool {
        matches!(self, Self::Activated { .. })
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsAccessLeaseTouch {
    Touched { lease: Box<HlsAccessLease> },
    Expired,
    Denied,
    UnknownLease,
    SessionMismatch,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseIdleRelease {
    pub lease_id: HlsAccessLeaseId,
    pub username: String,
    pub user_session_token: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseLifecycleSnapshot {
    pub lease_id: HlsAccessLeaseId,
    pub proxy_session_id: ProxySessionId,
    pub state: HlsAccessLeaseState,
    pub active_until_ms: Option<u64>,
    pub pending_deadline: Option<HlsAccessLeasePendingDeadline>,
    pub valid_until_ms: u64,
    pub idle_release: Option<HlsAccessLeaseIdleRelease>,
}

/// Registry for user-specific HLS access leases above shared content sessions.
#[derive(Debug, Default)]
pub struct HlsAccessLeaseStore {
    by_lease_id: HashMap<HlsAccessLeaseId, HlsAccessLease>,
    availability_generation_by_proxy_session: HashMap<ProxySessionId, HlsAvailabilityEvidenceGeneration>,
    last_availability_evidence_generation: HlsAvailabilityEvidenceGeneration,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsAccessLeaseSessionSnapshot {
    pub active_count: usize,
    pub effective_origin_policy: Option<HlsEffectiveOriginAcquirePolicy>,
    pub idle_releases: Vec<HlsAccessLeaseIdleRelease>,
}

impl HlsAccessLeaseStore {
    pub fn repair_prewarm_is_current(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        issued_at_ms: u64,
        snapshot_generation: u64,
    ) -> bool {
        self.by_lease_id.get(lease_id).is_some_and(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && lease.issued_at_ms == issued_at_ms
                && lease.playback_mode == HlsLeasePlaybackMode::Live
                && lease
                    .last_manifest_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.snapshot_generation == snapshot_generation)
        })
    }

    pub fn availability_evidence_generation(
        &self,
        proxy_session_id: &ProxySessionId,
    ) -> HlsAvailabilityEvidenceGeneration {
        self.availability_generation_by_proxy_session
            .get(proxy_session_id)
            .copied()
            .unwrap_or(HlsAvailabilityEvidenceGeneration::NONE)
    }

    fn availability_evidence_can_advance(&self) -> Result<(), HlsAvailabilityEvidenceAdvanceError> {
        self.last_availability_evidence_generation
            .0
            .checked_add(1)
            .map(|_| ())
            .ok_or(HlsAvailabilityEvidenceAdvanceError::Exhausted)
    }

    fn advance_availability_evidence(
        &mut self,
        proxy_session_id: &ProxySessionId,
    ) -> Result<HlsAvailabilityEvidenceGeneration, HlsAvailabilityEvidenceAdvanceError> {
        let generation = self
            .last_availability_evidence_generation
            .0
            .checked_add(1)
            .map(HlsAvailabilityEvidenceGeneration)
            .ok_or(HlsAvailabilityEvidenceAdvanceError::Exhausted)?;
        self.last_availability_evidence_generation = generation;
        self.availability_generation_by_proxy_session.insert(proxy_session_id.clone(), generation);
        Ok(generation)
    }

    fn refresh_access_lease_validity(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Result<bool, HlsAvailabilityEvidenceAdvanceError> {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return Ok(false);
        };
        if lease.state == HlsAccessLeaseState::Expired || lease.validity_due_at_ms() > now_ms {
            return Ok(false);
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        self.availability_evidence_can_advance()?;
        let changed = self.by_lease_id.get_mut(lease_id).is_some_and(|lease| lease.refresh_validity(now_ms));
        if changed {
            self.advance_availability_evidence(&proxy_session_id)?;
        }
        Ok(changed)
    }

    fn refresh_access_lease_validities_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Result<(), HlsAvailabilityEvidenceAdvanceError> {
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            self.refresh_access_lease_validity(&lease_id, now_ms)?;
        }
        Ok(())
    }

    fn refresh_access_lease_activity(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Result<Option<HlsAccessLeaseIdleRelease>, HlsAvailabilityEvidenceAdvanceError> {
        let previous_state = match self.by_lease_id.get(lease_id) {
            Some(lease) => lease.state,
            None => return Ok(None),
        };
        self.refresh_access_lease_validity(lease_id, now_ms)?;
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return Ok(None);
        };
        if lease.state == HlsAccessLeaseState::Expired
            && matches!(previous_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated)
        {
            return Ok(Some(HlsAccessLeaseIdleRelease {
                lease_id: lease.lease_id.clone(),
                username: lease.username.clone(),
                user_session_token: lease.user_session_token.clone(),
            }));
        }
        if lease.state != HlsAccessLeaseState::Activated
            || lease.active_until_ms.is_none_or(|active_until| active_until > now_ms)
        {
            return Ok(None);
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let release = HlsAccessLeaseIdleRelease {
            lease_id: lease.lease_id.clone(),
            username: lease.username.clone(),
            user_session_token: lease.user_session_token.clone(),
        };
        self.availability_evidence_can_advance()?;
        if let Some(lease) = self.by_lease_id.get_mut(lease_id) {
            lease.state = HlsAccessLeaseState::Idle;
        }
        self.advance_availability_evidence(&proxy_session_id)?;
        Ok(Some(release))
    }

    /// Returns false only when the process-lifetime evidence generation is
    /// exhausted. In that case no lease replacement is published.
    pub fn prepare_access_lease(&mut self, lease: HlsAccessLease) -> bool {
        if self.by_lease_id.get(&lease.lease_id) == Some(&lease) {
            return true;
        }
        let new_proxy_session_id = lease.proxy_session_id.clone();
        let replaced_proxy_session_id = self
            .by_lease_id
            .get(&lease.lease_id)
            .map(|current| current.proxy_session_id.clone())
            .filter(|current| *current != new_proxy_session_id);
        let advances = 1_u64.saturating_add(u64::from(replaced_proxy_session_id.is_some()));
        if self.last_availability_evidence_generation.0.checked_add(advances).is_none() {
            return false;
        }
        self.by_lease_id.insert(lease.lease_id.clone(), lease);
        if self.advance_availability_evidence(&new_proxy_session_id).is_err() {
            return false;
        }
        if let Some(old_proxy_session_id) = replaced_proxy_session_id {
            if self.advance_availability_evidence(&old_proxy_session_id).is_err() {
                return false;
            }
        }
        true
    }

    pub fn begin_runtime_policy_revocation(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> HlsRuntimePolicyRevocationOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        if lease.proxy_session_id != *proxy_session_id {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        if let HlsLeasePlaybackMode::TerminalTail(plan) = &lease.playback_mode {
            return HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { plan: Arc::clone(plan) };
        }
        if lease.state == HlsAccessLeaseState::PolicyRevoking {
            return lease
                .runtime_policy_revocation
                .clone()
                .filter(|token| token.reason == reason)
                .map_or(HlsRuntimePolicyRevocationOutcome::NoLongerEligible, |token| {
                    HlsRuntimePolicyRevocationOutcome::AlreadyPending { token }
                });
        }
        let Some(manifest) = lease.last_manifest_snapshot.as_ref() else {
            return HlsRuntimePolicyRevocationOutcome::NoPublishedManifest;
        };
        if lease.state != HlsAccessLeaseState::Activated || lease.playback_mode != HlsLeasePlaybackMode::Live {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let token = HlsRuntimePolicyRevocation {
            reason,
            lease_issued_at_ms: lease.issued_at_ms,
            expected_admission_generation: lease.admission_generation,
            manifest_snapshot_generation: manifest.snapshot_generation,
            cursor_generation: lease.playback_cursor.cursor_generation,
            started_at_ms: now_ms,
        };
        if self.availability_evidence_can_advance().is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        let lease_proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        };
        lease.state = HlsAccessLeaseState::PolicyRevoking;
        lease.runtime_policy_revocation = Some(token.clone());
        lease.runtime_policy_denial_reason = None;
        if self.advance_availability_evidence(&lease_proxy_session_id).is_err() {
            return HlsRuntimePolicyRevocationOutcome::NoLongerEligible;
        }
        HlsRuntimePolicyRevocationOutcome::Started { token }
    }

    pub fn fail_runtime_policy_revocation(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        token: &HlsRuntimePolicyRevocation,
    ) -> HlsAccessLeaseDenialOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if lease.proxy_session_id != *proxy_session_id
            || lease.state != HlsAccessLeaseState::PolicyRevoking
            || lease.runtime_policy_revocation.as_ref() != Some(token)
        {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        let lease_proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        lease.state = HlsAccessLeaseState::Denied;
        lease.runtime_policy_denial_reason = Some(token.reason);
        lease.end_playback();
        if self.advance_availability_evidence(&lease_proxy_session_id).is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    }

    pub fn remove_access_lease(&mut self, lease_id: &HlsAccessLeaseId) -> Option<HlsAccessLease> {
        let proxy_session_id = self.by_lease_id.get(lease_id)?.proxy_session_id.clone();
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        self.advance_availability_evidence(&proxy_session_id).ok()?;
        let removed = self.by_lease_id.remove(lease_id)?;
        Some(removed)
    }

    /// Snapshots cleanup state without deleting the persistent cancellation-recovery ticket.
    pub fn prepare_access_lease_removal(
        &self,
        lease_id: &HlsAccessLeaseId,
    ) -> Option<HlsAccessLeaseRemovalPreparation> {
        let lease = self.by_lease_id.get(lease_id)?;
        Some(HlsAccessLeaseRemovalPreparation {
            proxy_session_id: lease.proxy_session_id.clone(),
            issued_at_ms: lease.issued_at_ms,
            terminal_protection_generation: lease.pending_terminal_protection_release.or_else(|| {
                if let HlsLeasePlaybackMode::TerminalTail(plan) = &lease.playback_mode {
                    Some(plan.generation)
                } else {
                    None
                }
            }),
        })
    }

    /// Deletes only the lease instance observed before its protection cleanup.
    pub fn remove_access_lease_if_preparation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsAccessLeaseRemovalPreparation,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != preparation.proxy_session_id || lease.issued_at_ms != preparation.issued_at_ms {
            return None;
        }
        self.remove_access_lease(lease_id)
    }

    pub fn remove_access_leases_for_session(&mut self, proxy_session_id: &ProxySessionId) -> Vec<HlsAccessLease> {
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        if lease_ids.is_empty() || self.availability_evidence_can_advance().is_err() {
            return Vec::new();
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return Vec::new();
        }
        lease_ids.into_iter().filter_map(|lease_id| self.by_lease_id.remove(&lease_id)).collect()
    }

    pub fn clear(&mut self) -> usize {
        let removed = self.by_lease_id.len();
        self.by_lease_id.clear();
        self.availability_generation_by_proxy_session.clear();
        removed
    }

    pub fn len(&self) -> usize {
        self.by_lease_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_lease_id.is_empty()
    }

    pub fn first_username_for_session(&self, proxy_session_id: &ProxySessionId) -> Option<String> {
        self.by_lease_id
            .values()
            .find(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.username.clone())
    }

    pub fn all_live_leases_terminal_for_session(&self, proxy_session_id: &ProxySessionId) -> bool {
        let mut found = false;
        for lease in self.by_lease_id.values().filter(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && matches!(
                    lease.state,
                    HlsAccessLeaseState::Pending
                        | HlsAccessLeaseState::Activated
                        | HlsAccessLeaseState::Idle
                        | HlsAccessLeaseState::PolicyRevoking
                        | HlsAccessLeaseState::Denied
                )
        }) {
            found = true;
            if !matches!(
                lease.playback_mode,
                HlsLeasePlaybackMode::TerminalTail(_)
                    | HlsLeasePlaybackMode::TerminalUnavailable { .. }
                    | HlsLeasePlaybackMode::Ended
            ) {
                return false;
            }
        }
        found
    }

    /// Returns the oldest sequence which every usable live lease has consumed.
    ///
    /// `None` deliberately keeps the canonical render protected when a lease
    /// has not published or completed media yet. Terminal leases are excluded
    /// here because their exact base media is protected by the terminal-tail
    /// generation stored on the shared session.
    pub fn capacity_release_through(&self, proxy_session_id: &ProxySessionId) -> Option<u64> {
        let mut found_live_lease = false;
        let mut release_through = None;
        for lease in self.by_lease_id.values().filter(|lease| {
            lease.proxy_session_id == *proxy_session_id
                && lease_state_protects_live_evidence(lease.state)
                && lease.playback_mode == HlsLeasePlaybackMode::Live
        }) {
            found_live_lease = true;
            let snapshot = lease.last_manifest_snapshot.as_ref()?;
            if snapshot.delivery_mode != HlsManifestDeliveryMode::NormalCacheTimeline {
                return None;
            }
            let completed = lease.playback_cursor.highest_contiguous_completed_proxy_seq?;
            let completed = completed.min(snapshot.last_proxy_seq);
            release_through = Some(release_through.map_or(completed, |current: u64| current.min(completed)));
        }
        if found_live_lease {
            release_through
        } else {
            None
        }
    }

    pub fn response_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return None;
        }
        self.by_lease_id.get(lease_id).cloned()
    }

    /// Returns immutable playback evidence for leases that are actively consuming this shared session.
    pub fn active_live_playback_snapshots_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Vec<HlsAccessLease> {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return Vec::new();
        }
        self.by_lease_id
            .values()
            .filter_map(|lease| {
                if lease.proxy_session_id != *proxy_session_id {
                    return None;
                }
                (lease.state == HlsAccessLeaseState::Activated
                    && lease.active_until_ms.is_some_and(|active_until_ms| active_until_ms > now_ms)
                    && lease.playback_mode == HlsLeasePlaybackMode::Live
                    && lease.last_manifest_snapshot.is_some())
                .then(|| lease.clone())
            })
            .collect()
    }

    pub fn prepare_manifest_publication(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsLeaseManifestPublicationGuard> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return None;
        }
        self.by_lease_id.get(lease_id)?.manifest_publication_guard()
    }

    pub fn commit_manifest_publication(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        mut snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        if lease.proxy_session_id != *proxy_session_id {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SessionMismatch,
            );
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        if lease.state == HlsAccessLeaseState::Expired {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired);
        }
        if !lease_state_allows_use(lease.state) {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::LeaseUnavailable,
            );
        }
        if lease.issued_at_ms != expected.issued_at_ms {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::LeaseIncarnationChanged,
            );
        }
        if lease.admission_generation != expected.admission_generation {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged,
            );
        }
        if lease.playback_mode != HlsLeasePlaybackMode::Live {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseNotLive);
        }
        if lease
            .last_manifest_snapshot
            .as_ref()
            .is_some_and(|current| snapshot.source_render_marker < current.source_render_marker)
        {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SourceRegressive,
            );
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        let next_generation = lease.manifest_snapshot_generation.saturating_add(1);
        if next_generation == lease.manifest_snapshot_generation {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        lease.manifest_snapshot_generation = next_generation;
        snapshot.snapshot_generation = next_generation;
        lease.last_manifest_snapshot = Some(snapshot);
        lease.startup_admission = HlsLeaseStartupAdmissionState::Admitted;
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: next_generation }
    }

    pub fn media_identity_is_current(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        now_ms: u64,
    ) -> bool {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return false;
        };
        if lease.proxy_session_id != *proxy_session_id || self.refresh_access_lease_validity(lease_id, now_ms).is_err()
        {
            return false;
        }
        self.by_lease_id.get(lease_id).is_some_and(|lease| {
            lease_state_allows_use(lease.state)
                && lease.issued_at_ms == expected.issued_at_ms
                && lease.media_identity() == Some(expected)
        })
    }

    pub fn record_segment_request_started_if_identity_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        proxy_seq: u64,
        requested_at_ms: u64,
    ) -> Option<HlsPlaybackRequestToken> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, requested_at_ms).is_err() {
            return None;
        }
        let lease = self.by_lease_id.get(lease_id)?;
        if !lease_state_allows_use(lease.state)
            || lease.issued_at_ms != expected.issued_at_ms
            || lease.media_identity() != Some(expected)
            || !matches!(expected.playback, HlsMediaLeasePlaybackIdentity::Live { .. })
            || lease.playback_cursor.cursor_generation == u64::MAX
            || self.availability_evidence_can_advance().is_err()
        {
            return None;
        }
        let token =
            self.by_lease_id.get_mut(lease_id)?.playback_cursor.record_request_started(proxy_seq, requested_at_ms);
        self.advance_availability_evidence(proxy_session_id).ok()?;
        Some(token)
    }

    pub fn record_segment_request_completed_if_identity_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        token: HlsPlaybackRequestToken,
        completed_at_ms: u64,
    ) -> Option<HlsPlaybackCompletionOutcome> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, completed_at_ms).is_err() {
            return None;
        }
        let lease = self.by_lease_id.get(lease_id)?;
        if !lease_state_allows_use(lease.state)
            || lease.issued_at_ms != expected.issued_at_ms
            || lease.media_identity() != Some(expected)
            || !matches!(expected.playback, HlsMediaLeasePlaybackIdentity::Live { .. })
        {
            return None;
        }
        let mut cursor = lease.playback_cursor.clone();
        let outcome = cursor.record_request_completed(token, completed_at_ms);
        if cursor.cursor_generation == lease.playback_cursor.cursor_generation {
            return Some(outcome);
        }
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        self.by_lease_id.get_mut(lease_id)?.playback_cursor = cursor;
        self.advance_availability_evidence(proxy_session_id).ok()?;
        Some(outcome)
    }

    pub fn prepare_terminal_tail(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        input: &HlsTerminalTailPreparationInput,
    ) -> Option<HlsTerminalTailPreparation> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id || lease.playback_mode != HlsLeasePlaybackMode::Live {
            return None;
        }
        let mut manifest_snapshot = lease.last_manifest_snapshot.clone()?;
        if manifest_snapshot.snapshot_generation != input.expected_manifest_snapshot_generation
            || lease.playback_cursor.cursor_generation != input.expected_cursor_generation
        {
            return None;
        }
        let runtime_policy_revocation = match input.trigger {
            HlsFiniteTailTrigger::AvailabilityReserve => None,
            HlsFiniteTailTrigger::RuntimePolicy(reason) => {
                let revocation = lease.runtime_policy_revocation.clone()?;
                if lease.state != HlsAccessLeaseState::PolicyRevoking
                    || revocation.reason != reason
                    || revocation.lease_issued_at_ms != lease.issued_at_ms
                    || revocation.expected_admission_generation != lease.admission_generation
                    || revocation.manifest_snapshot_generation != manifest_snapshot.snapshot_generation
                    || revocation.cursor_generation != lease.playback_cursor.cursor_generation
                {
                    return None;
                }
                if reason.base_policy() == HlsRuntimeCustomTailBasePolicy::PreserveCompletedOrInFlightPrefix {
                    manifest_snapshot =
                        runtime_policy_authorized_manifest_prefix(manifest_snapshot, &lease.playback_cursor)?;
                }
                Some(revocation)
            }
        };
        let expected_admission_generation = lease.admission_generation;
        let decision_generation = expected_admission_generation.saturating_add(1);
        if decision_generation == expected_admission_generation {
            return None;
        }
        let terminal_media_requirement_source = match input.terminal_media_requirement_origin {
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode { generation }
                if generation == input.expected_acceptance_generation =>
            {
                HlsTerminalMediaRequirementSource::AcceptanceEpisode { generation }
            }
            HlsTerminalMediaRequirementOrigin::CutoverSnapshot => {
                HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
            }
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode { .. } => return None,
        };
        Some(HlsTerminalTailPreparation {
            trigger: input.trigger,
            runtime_policy_revocation,
            lease_issued_at_ms: lease.issued_at_ms,
            decision_generation,
            expected_admission_generation,
            manifest_snapshot_generation: manifest_snapshot.snapshot_generation,
            cursor_generation: lease.playback_cursor.cursor_generation,
            origin_progress_generation: input.origin_progress_generation,
            media_readiness_generation: input.media_readiness_generation,
            origin_epoch: input.origin_epoch,
            last_media_progress_at_ms: input.last_media_progress_at_ms,
            expected_acceptance_generation: input.expected_acceptance_generation,
            terminal_media_requirement_source,
            cutover_timing: input.cutover_timing,
            commit_window: input.commit_window,
            required_terminal_media_key: input.required_terminal_media_key,
            terminal_media_preparation: input.terminal_media_preparation,
            reserve: input.reserve,
            manifest_snapshot,
        })
    }

    pub fn commit_terminal_tail_if_generation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
        plan: std::sync::Arc<HlsTerminalTailPlan>,
    ) -> HlsTerminalCommitOutcome {
        if !plan.matches_route(proxy_session_id, lease_id)
            || plan.generation.0 != preparation.decision_generation
            || plan.base_manifest.snapshot_generation != preparation.manifest_snapshot_generation
        {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        if let Some(outcome) = terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms) {
            return outcome;
        }
        if let Err(outcome) = lease.terminal_decision_commit_preconditions_match(proxy_session_id, preparation, now_ms)
        {
            return outcome;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        lease.admission_generation = preparation.decision_generation;
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(plan);
        if let Some(revocation) = preparation.runtime_policy_revocation.as_ref() {
            lease.state = HlsAccessLeaseState::Denied;
            lease.runtime_policy_denial_reason = Some(revocation.reason);
            lease.runtime_policy_revocation = None;
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        HlsTerminalCommitOutcome::Committed
    }

    pub fn terminal_tail_replay_outcome(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get_mut(lease_id)?;
        terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms)
    }

    pub fn commit_terminal_unavailable_if_generation_matches(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
        reason: HlsTerminalTailCompatibility,
    ) -> HlsTerminalCommitOutcome {
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        if let Some(outcome) = terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms) {
            return outcome;
        }
        if let Err(outcome) = lease.terminal_decision_commit_preconditions_match(proxy_session_id, preparation, now_ms)
        {
            return outcome;
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsTerminalCommitOutcome::LeaseNoLongerEligible;
        };
        lease.admission_generation = preparation.decision_generation;
        lease.playback_mode =
            HlsLeasePlaybackMode::TerminalUnavailable { decision_generation: preparation.decision_generation, reason };
        if let Some(revocation) = preparation.runtime_policy_revocation.as_ref() {
            lease.state = HlsAccessLeaseState::Denied;
            lease.runtime_policy_denial_reason = Some(revocation.reason);
            lease.runtime_policy_revocation = None;
        }
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        }
        HlsTerminalCommitOutcome::Committed
    }

    pub fn terminal_unavailable_replay_outcome(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &HlsTerminalTailPreparation,
        now_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get_mut(lease_id)?;
        terminal_commit_existing_outcome(lease, proxy_session_id, preparation, now_ms)
    }

    pub fn access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let state = self.by_lease_id.get(lease_id)?.state;
        if state == HlsAccessLeaseState::Expired {
            return None;
        }
        if matches!(state, HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied) {
            return self.by_lease_id.get(lease_id).cloned();
        }
        if !lease_state_allows_use(state) {
            return None;
        }
        self.by_lease_id.get(lease_id).cloned()
    }

    pub fn update_origin_acquire_policy(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        connection_kind: ConnectionKind,
        priority: i8,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if !lease_state_allows_use(lease.state) {
            return None;
        }
        lease.update_origin_acquire_policy(connection_kind, priority);
        Some(lease.clone())
    }

    pub fn activate_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> HlsAccessLeaseActivation {
        let Some(new_lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        if &new_lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseActivation::SessionMismatch;
        }
        if !lease_state_allows_use(new_lease.state) {
            return activation_for_state(new_lease.state);
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsAccessLeaseActivation::Expired;
        }
        let Some(new_lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        if new_lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseActivation::Expired;
        }
        let previous_state = new_lease.state;
        let active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        let valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        let evidence_changed = new_lease.state != HlsAccessLeaseState::Activated
            || new_lease.active_until_ms != active_until_ms
            || new_lease.pending_deadline.is_some()
            || new_lease.valid_until_ms != valid_until_ms;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseActivation::Expired;
        }
        let proxy_session_id = new_lease.proxy_session_id.clone();
        let Some(new_lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseActivation::UnknownLease;
        };
        new_lease.state = HlsAccessLeaseState::Activated;
        new_lease.last_seen_at_ms = now_ms;
        new_lease.active_until_ms = active_until_ms;
        new_lease.pending_deadline = None;
        new_lease.valid_until_ms = valid_until_ms;
        let lease = new_lease.clone();
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseActivation::Expired;
        }

        HlsAccessLeaseActivation::Activated { lease: Box::new(lease), previous_state }
    }

    pub fn touch_manifest_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        active_timing: Option<HlsAccessLeaseTiming>,
        pending_deadline: Option<HlsAccessLeasePendingDeadline>,
        valid_window_ms: u64,
    ) -> HlsAccessLeaseTouch {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        if &lease.proxy_session_id != path_proxy_session_id {
            return HlsAccessLeaseTouch::SessionMismatch;
        }
        if !lease_state_allows_use(lease.state) {
            return touch_for_state(lease.state);
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        if lease.state == HlsAccessLeaseState::Expired {
            return HlsAccessLeaseTouch::Expired;
        }
        let before = (lease.active_until_ms, lease.pending_deadline, lease.valid_until_ms);
        let proxy_session_id = lease.proxy_session_id.clone();
        if self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseTouch::UnknownLease;
        };
        lease.last_seen_at_ms = now_ms;
        match lease.state {
            HlsAccessLeaseState::Pending => {
                if let Some(pending_deadline) = pending_deadline {
                    lease.apply_pending_deadline(pending_deadline);
                }
            }
            HlsAccessLeaseState::Activated => {
                if let Some(timing) = active_timing {
                    lease.active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
                    lease.valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
                } else {
                    lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
                }
            }
            HlsAccessLeaseState::Idle => {
                lease.valid_until_ms = now_ms.saturating_add(valid_window_ms);
            }
            HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied => {}
        }
        let evidence_changed = before != (lease.active_until_ms, lease.pending_deadline, lease.valid_until_ms);
        let lease = lease.clone();
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseTouch::Expired;
        }
        HlsAccessLeaseTouch::Touched { lease: Box::new(lease) }
    }

    pub fn mark_pending_manifest_follow_up_for_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        path_proxy_session_id: &ProxySessionId,
        now_ms: u64,
        deadline: HlsAccessLeasePendingDeadline,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if &lease.proxy_session_id != path_proxy_session_id {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state != HlsAccessLeaseState::Pending {
            return None;
        }
        let next_deadline = lease.pending_deadline.map_or(deadline, |current| current.tightened_with(deadline));
        if lease.pending_deadline == Some(next_deadline) {
            return None;
        }
        if self.availability_evidence_can_advance().is_err() {
            return None;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let lease = self.by_lease_id.get_mut(lease_id)?;
        lease.last_seen_at_ms = now_ms;
        if !lease.apply_pending_deadline(deadline) {
            return None;
        }
        let lease = lease.clone();
        self.advance_availability_evidence(&proxy_session_id).ok()?;
        Some(lease)
    }

    pub fn mark_pending_manifest_follow_up_for_session(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        deadline: HlsAccessLeasePendingDeadline,
    ) -> Vec<HlsAccessLease> {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return Vec::new();
        }
        if self.availability_evidence_can_advance().is_err() {
            return Vec::new();
        }
        let mut leases = Vec::new();
        for lease in self.by_lease_id.values_mut() {
            if lease.proxy_session_id != *proxy_session_id {
                continue;
            }
            if lease.state != HlsAccessLeaseState::Pending {
                continue;
            }
            lease.last_seen_at_ms = now_ms;
            if lease.apply_pending_deadline(deadline) {
                leases.push(lease.clone());
            }
        }
        if !leases.is_empty() && self.advance_availability_evidence(proxy_session_id).is_err() {
            return Vec::new();
        }
        leases
    }

    pub fn touch_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> bool {
        self.touch_access_lease_snapshot(lease_id, now_ms, timing).is_some()
    }

    pub fn touch_access_lease_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> Option<HlsAccessLease> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state != HlsAccessLeaseState::Activated {
            return None;
        }
        self.refresh_access_lease_validity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.state == HlsAccessLeaseState::Expired {
            return None;
        }
        let active_until_ms = Some(now_ms.saturating_add(timing.active_window_ms));
        let valid_until_ms = now_ms.saturating_add(timing.valid_window_ms);
        let evidence_changed = lease.active_until_ms != active_until_ms || lease.valid_until_ms != valid_until_ms;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return None;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let lease = self.by_lease_id.get_mut(lease_id)?;
        lease.last_seen_at_ms = now_ms;
        lease.active_until_ms = active_until_ms;
        lease.valid_until_ms = valid_until_ms;
        let lease = lease.clone();
        if evidence_changed {
            self.advance_availability_evidence(&proxy_session_id).ok()?;
        }
        Some(lease)
    }

    pub fn deny_access_lease(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        mode: HlsAccessLeaseDenialMode,
    ) -> HlsAccessLeaseDenialOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if lease.state == HlsAccessLeaseState::PolicyRevoking {
            return HlsAccessLeaseDenialOutcome::PolicyRevocationPending;
        }
        let evidence_changed = lease.state != HlsAccessLeaseState::Denied;
        if evidence_changed && self.availability_evidence_can_advance().is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        let proxy_session_id = lease.proxy_session_id.clone();
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        };
        if evidence_changed {
            lease.state = HlsAccessLeaseState::Denied;
        }
        let preserve_finite_decision = matches!(
            lease.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. }
        );
        if preserve_finite_decision {
            if evidence_changed {
                lease.runtime_policy_denial_reason = match &lease.playback_mode {
                    HlsLeasePlaybackMode::TerminalTail(plan) => Some(plan.reason),
                    HlsLeasePlaybackMode::TerminalUnavailable { .. } => lease.runtime_policy_denial_reason,
                    HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => None,
                };
            }
            if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
                return HlsAccessLeaseDenialOutcome::UnknownLease;
            }
            return HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved;
        }
        if evidence_changed {
            lease.end_playback();
            lease.runtime_policy_denial_reason = match mode {
                HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd { reason } => Some(reason),
                HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail => None,
                #[cfg(test)]
                HlsAccessLeaseDenialMode::ImmediateEnd => None,
            };
        }
        let generation = lease.pending_terminal_protection_release;
        if evidence_changed && self.advance_availability_evidence(&proxy_session_id).is_err() {
            return HlsAccessLeaseDenialOutcome::UnknownLease;
        }
        HlsAccessLeaseDenialOutcome::Ended {
            terminal_release: generation
                .map(|generation| HlsDeniedTerminalTailRelease { proxy_session_id, generation }),
        }
    }

    /// Clears a persisted cleanup ticket only for the same ended lease/session/generation.
    pub fn acknowledge_terminal_protection_release(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        generation: HlsTerminalTailGeneration,
    ) -> bool {
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return false;
        };
        if !matches!(lease.state, HlsAccessLeaseState::Denied | HlsAccessLeaseState::Expired)
            || lease.proxy_session_id != *proxy_session_id
            || lease.playback_mode != HlsLeasePlaybackMode::Ended
            || lease.pending_terminal_protection_release != Some(generation)
        {
            return false;
        }
        lease.pending_terminal_protection_release = None;
        true
    }

    pub fn lease_state(&self, lease_id: &HlsAccessLeaseId, now_ms: u64) -> Option<HlsAccessLeaseState> {
        self.by_lease_id.get(lease_id).map(|lease| {
            if lease.validity_due_at_ms() <= now_ms {
                HlsAccessLeaseState::Expired
            } else {
                lease.state
            }
        })
    }

    pub fn active_access_lease_count_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> usize {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return 0;
        }
        let mut active_count = 0;
        for lease in self.by_lease_id.values() {
            if lease.proxy_session_id == *proxy_session_id
                && lease.state == HlsAccessLeaseState::Activated
                && lease.active_until_ms.is_some_and(|active_until| active_until > now_ms)
            {
                active_count += 1;
            }
        }
        active_count
    }

    pub fn has_usable_access_lease_for_session(&mut self, proxy_session_id: &ProxySessionId, now_ms: u64) -> bool {
        if self.refresh_access_lease_validities_for_session(proxy_session_id, now_ms).is_err() {
            return false;
        }
        let mut has_usable_lease = false;
        for lease in self.by_lease_id.values() {
            if lease.proxy_session_id == *proxy_session_id
                && (lease.state == HlsAccessLeaseState::Pending
                    || lease.state == HlsAccessLeaseState::Idle
                    || (lease.state == HlsAccessLeaseState::Activated && lease.valid_until_ms > now_ms))
            {
                has_usable_lease = true;
            }
        }
        has_usable_lease
    }

    pub fn session_snapshot(
        &mut self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseSessionSnapshot {
        let mut active_count = 0;
        let mut effective_origin_policy = None;
        let mut idle_releases = Vec::new();
        let lease_ids = self
            .by_lease_id
            .values()
            .filter(|lease| lease.proxy_session_id == *proxy_session_id)
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            match self.refresh_access_lease_activity(&lease_id, now_ms) {
                Ok(Some(release)) => idle_releases.push(release),
                Ok(None) => {}
                Err(HlsAvailabilityEvidenceAdvanceError::Exhausted) => continue,
            }
            let Some(lease) = self.by_lease_id.get(&lease_id) else {
                continue;
            };
            if lease.state == HlsAccessLeaseState::Activated {
                active_count += 1;
            }
            if matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated) {
                let candidate =
                    HlsEffectiveOriginAcquirePolicy::new(lease.origin_connection_kind, lease.origin_priority, now_ms);
                effective_origin_policy = Some(effective_origin_policy.map_or(candidate, |current| {
                    if candidate.is_better_than(current) {
                        candidate
                    } else {
                        current
                    }
                }));
            }
        }
        HlsAccessLeaseSessionSnapshot { active_count, effective_origin_policy, idle_releases }
    }

    pub fn lifecycle_snapshot(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        let idle_release = self.refresh_access_lease_activity(lease_id, now_ms).ok()?;
        let lease = self.by_lease_id.get(lease_id)?;
        Some(HlsAccessLeaseLifecycleSnapshot {
            lease_id: lease.lease_id.clone(),
            proxy_session_id: lease.proxy_session_id.clone(),
            state: lease.state,
            active_until_ms: lease.active_until_ms,
            pending_deadline: lease.pending_deadline,
            valid_until_ms: lease.valid_until_ms,
            idle_release,
        })
    }
}

pub fn new_hls_access_lease_id() -> HlsAccessLeaseId {
    let mut bytes = [0u8; HLS_ACCESS_LEASE_ID_BYTES];
    if OsRng.try_fill_bytes(&mut bytes).is_err() {
        rand::rng().fill_bytes(&mut bytes);
    }
    HlsAccessLeaseId(general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

const fn lease_state_allows_use(state: HlsAccessLeaseState) -> bool {
    matches!(state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
}

const fn lease_state_protects_live_evidence(state: HlsAccessLeaseState) -> bool {
    lease_state_allows_use(state) || matches!(state, HlsAccessLeaseState::PolicyRevoking)
}

const fn activation_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseActivation {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseActivation::Expired,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied => HlsAccessLeaseActivation::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseActivation::UnknownLease
        }
    }
}

const fn touch_for_state(state: HlsAccessLeaseState) -> HlsAccessLeaseTouch {
    match state {
        HlsAccessLeaseState::Expired => HlsAccessLeaseTouch::Expired,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Denied => HlsAccessLeaseTouch::Denied,
        HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle => {
            HlsAccessLeaseTouch::UnknownLease
        }
    }
}

fn runtime_policy_authorized_manifest_prefix(
    mut manifest: HlsLeaseManifestSnapshot,
    cursor: &HlsLeasePlaybackCursor,
) -> Option<HlsLeaseManifestSnapshot> {
    let authorized_last_proxy_seq =
        match (cursor.highest_contiguous_completed_proxy_seq, cursor.last_requested_proxy_seq) {
            (Some(completed), Some(requested)) => completed.max(requested),
            (Some(completed), None) => completed,
            (None, Some(requested)) => requested,
            (None, None) => return None,
        }
        .min(manifest.last_proxy_seq);
    let end = manifest
        .visible_segments
        .iter()
        .position(|segment| segment.proxy_seq > authorized_last_proxy_seq)
        .unwrap_or(manifest.visible_segments.len());
    let selected = manifest.visible_segments.get(..end)?.to_vec();
    let first = selected.first()?;
    let last = selected.last()?;
    let first_proxy_seq = first.proxy_seq;
    let last_proxy_seq = last.proxy_seq;
    let active_encryption = last.encryption.clone();
    let playlist_duration_ms = selected.iter().fold(0_u64, |total, segment| total.saturating_add(segment.duration_ms));
    manifest.first_proxy_seq = first_proxy_seq;
    manifest.last_proxy_seq = last_proxy_seq;
    manifest.visible_segments = Arc::from(selected);
    manifest.playlist_duration_ms = playlist_duration_ms;
    manifest.last_visible_media_end_ms = playlist_duration_ms;
    manifest.active_encryption = active_encryption;
    Some(manifest)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            build_terminal_tail_plan,
            manifest_acceptance::HlsManifestAcceptanceGeneration,
            media_reserve::{HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot, HlsPlaybackCompletionOutcome},
            recovery_timing::{
                HlsLeaseCutoverTiming, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState, HlsTransitionMarginMs,
            },
            runtime_custom_tail::{
                HlsFiniteTailTrigger, HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason,
            },
            terminal_commit::HlsTerminalCommitOutcome,
            terminal_tail::{
                snapshot_terminal_media_asset, HlsLeasePlaybackMode, HlsTerminalTailCompatibility, HlsTerminalTailPlan,
            },
            HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker,
            HlsMediaContainer, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
            HlsTerminalBaseSegmentAvailability, HlsTerminalTailBuildInput, HlsTerminalTailGeneration,
        },
        new_hls_access_lease_id, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseDenialMode,
        HlsAccessLeaseDenialOutcome, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState,
        HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsAvailabilityEvidenceGeneration,
        HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason, HlsPlaybackFamilyKey,
        HlsRuntimePolicyRevocationOutcome, HlsTerminalMediaRequirementOrigin, HlsTerminalTailPreparationInput,
    };
    use crate::ProxySessionId;
    use std::sync::Arc;
    use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;
    use tuliprox_session::ConnectionKind;

    const TERMINAL_ASSET_BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

    fn cutover_reserve() -> HlsLeaseReserveSnapshot {
        HlsLeaseReserveSnapshot {
            availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
            guaranteed_media_horizon_ms: 12_000,
            conservative_playback_position_ms: 12_000,
            guaranteed_reserve_ms: 0,
            initial_hidden_ready_duration_ms: 0,
            transition_margin: HlsTransitionMarginMs::from_millis(12_000),
            key_readiness_valid_until_ms: None,
            recovery_required: true,
            cutover_required: true,
        }
    }

    fn cutover_timing() -> HlsLeaseCutoverTiming {
        HlsLeaseCutoverTiming::from_reserve(2_000, 0, HlsTransitionMarginMs::from_millis(12_000), None)
    }

    fn lease(lease_id: HlsAccessLeaseId, proxy_session_id: &str, now_ms: u64) -> HlsAccessLease {
        HlsAccessLease::pending(
            lease_id,
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            ProxySessionId(proxy_session_id.to_string()),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            15_000,
        )
    }

    const fn timing(active_window_ms: u64, valid_window_ms: u64) -> HlsAccessLeaseTiming {
        HlsAccessLeaseTiming { active_window_ms, valid_window_ms }
    }

    #[test]
    fn known_bitrate_builder_normalizes_zero_and_survives_response_snapshot() {
        let proxy_session_id = ProxySessionId("proxy-a".to_string());
        let zero_lease_id = HlsAccessLeaseId("lease-zero".to_string());
        let measured_lease_id = HlsAccessLeaseId("lease-measured".to_string());
        let mut store = HlsAccessLeaseStore::default();
        assert!(store.prepare_access_lease(
            lease(zero_lease_id.clone(), &proxy_session_id.0, 1_000).with_known_bitrate_bps(Some(0))
        ));
        assert!(store.prepare_access_lease(
            lease(measured_lease_id.clone(), &proxy_session_id.0, 1_000).with_known_bitrate_bps(Some(2_500_000)),
        ));

        assert_eq!(
            store
                .response_snapshot(&zero_lease_id, &proxy_session_id, 2_000)
                .expect("zero bitrate lease")
                .known_bitrate_bps,
            None
        );
        assert_eq!(
            store
                .response_snapshot(&measured_lease_id, &proxy_session_id, 2_000)
                .expect("measured bitrate lease")
                .known_bitrate_bps,
            Some(2_500_000)
        );
    }

    #[test]
    fn standalone_tail_policy_is_limited_to_pending_unpublished_lease() {
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let mut candidate = lease(lease_id, "proxy-a", 1_000);

        assert!(candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));
        assert!(!candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::SessionOrLeaseExpired));

        candidate.last_manifest_snapshot = Some(manifest_snapshot(1));
        candidate.startup_admission = super::HlsLeaseStartupAdmissionState::Admitted;
        assert!(!candidate.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));

        let mut activated = lease(HlsAccessLeaseId("lease-b".to_string()), "proxy-a", 1_000);
        activated.state = HlsAccessLeaseState::Activated;
        assert!(!activated.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));

        let mut terminal = lease(HlsAccessLeaseId("lease-c".to_string()), "proxy-a", 1_000);
        terminal.playback_mode = HlsLeasePlaybackMode::TerminalUnavailable {
            decision_generation: 1,
            reason: HlsTerminalTailCompatibility::MissingAsset,
        };
        assert!(!terminal.permits_unpublished_standalone_tail(HlsRuntimeCustomTailReason::ChannelUnavailable));
    }

    fn manifest_snapshot(source_rendered_at_ms: u64) -> HlsLeaseManifestSnapshot {
        HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(source_rendered_at_ms),
            snapshot_generation: 0,
            delivered_at_ms: 2_000,
            first_proxy_seq: 40,
            last_proxy_seq: 41,
            visible_segments: Arc::from([
                HlsLeaseManifestSegment {
                    proxy_seq: 40,
                    duration_ms: 6_000,
                    uri: "/live/40.ts".to_string(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
                HlsLeaseManifestSegment {
                    proxy_seq: 41,
                    duration_ms: 6_000,
                    uri: "/live/41.ts".to_string(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
            ]),
            discontinuity_sequence: 3,
            target_duration_ms: 12_000,
            playlist_duration_ms: 12_000,
            last_visible_media_end_ms: 12_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        }
    }

    fn manifest_snapshot_for_route(
        source_rendered_at_ms: u64,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
    ) -> HlsLeaseManifestSnapshot {
        let mut snapshot = manifest_snapshot(source_rendered_at_ms);
        for segment in Arc::make_mut(&mut snapshot.visible_segments) {
            segment.uri = format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, segment.proxy_seq);
        }
        snapshot
    }

    fn publish_manifest_snapshot(
        store: &mut HlsAccessLeaseStore,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        let guard =
            store.prepare_manifest_publication(lease_id, proxy_session_id, now_ms).expect("live manifest publication");
        store.commit_manifest_publication(lease_id, proxy_session_id, guard, snapshot, now_ms)
    }

    #[test]
    fn repair_prewarm_guard_rejects_newer_publication_and_terminal_lease() {
        let proxy_session_id = ProxySessionId("proxy-a".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let mut store = HlsAccessLeaseStore::default();
        assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
        let first_generation =
            publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
                .snapshot_generation()
                .expect("first publication");
        assert!(store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, first_generation,));

        let second_generation =
            publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2), 2_100)
                .snapshot_generation()
                .expect("second publication");
        assert!(!store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, first_generation,));
        assert!(store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, second_generation,));

        store.by_lease_id.get_mut(&lease_id).expect("lease").playback_mode = HlsLeasePlaybackMode::Ended;
        assert!(!store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, second_generation,));
    }

    fn terminal_plan(
        generation: u64,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
    ) -> Arc<HlsTerminalTailPlan> {
        terminal_plan_at(generation, proxy_session_id, lease_id, 3_000)
    }

    fn terminal_plan_at(
        generation: u64,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        created_at_ms: u64,
    ) -> Arc<HlsTerminalTailPlan> {
        let mut base_manifest = manifest_snapshot_for_route(1, proxy_session_id, lease_id);
        base_manifest.snapshot_generation = 1;
        let transport_stream = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let asset = snapshot_terminal_media_asset(&transport_stream).expect("terminal asset");
        let expected_asset =
            HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(&asset));
        let availability = Arc::from(
            base_manifest
                .visible_proxy_seqs()
                .map(|proxy_seq| HlsTerminalBaseSegmentAvailability {
                    proxy_seq,
                    media_state: HlsTerminalBaseMediaState::Ready,
                    required_map_ready: true,
                    required_key_ready: true,
                    protection: HlsTerminalBaseProtection::Protectable,
                })
                .collect::<Vec<_>>(),
        );
        let base_track_signature = Some(asset.track_signature().clone());
        let anchored_bundle =
            HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, base_manifest.target_duration_ms);
        let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
        let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
        let terminal_splice_evidence = base_splice_evidence.clone();
        Arc::new(
            build_terminal_tail_plan(HlsTerminalTailBuildInput {
                generation: HlsTerminalTailGeneration(generation),
                created_at_ms,
                base_manifest,
                base_availability: availability,
                base_track_signature,
                base_splice_evidence,
                terminal_splice_evidence,
                base_timing,
                base_key_bindings: Arc::from([]),
                expected_asset,
                asset,
                anchored_bundle,
            })
            .expect("compatible terminal plan"),
        )
    }

    fn prepare_terminal(
        store: &mut HlsAccessLeaseStore,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        origin_progress_generation: u64,
        last_media_progress_at_ms: Option<u64>,
    ) -> super::HlsTerminalTailPreparation {
        let lease = store.response_snapshot(lease_id, proxy_session_id, 2_000).expect("test lease snapshot");
        let snapshot_generation =
            lease.last_manifest_snapshot.as_ref().expect("test manifest snapshot").snapshot_generation;
        store
            .prepare_terminal_tail(
                lease_id,
                proxy_session_id,
                &HlsTerminalTailPreparationInput {
                    trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                    expected_manifest_snapshot_generation: snapshot_generation,
                    expected_cursor_generation: lease.playback_cursor.cursor_generation,
                    origin_progress_generation,
                    media_readiness_generation: 0,
                    origin_epoch: 0,
                    last_media_progress_at_ms,
                    expected_acceptance_generation: HlsManifestAcceptanceGeneration(origin_progress_generation),
                    terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::AcceptanceEpisode {
                        generation: HlsManifestAcceptanceGeneration(origin_progress_generation),
                    },
                    cutover_timing: cutover_timing(),
                    commit_window: HlsTerminalCommitWindow::CutoverDue,
                    required_terminal_media_key: None,
                    terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                    reserve: cutover_reserve(),
                },
            )
            .expect("terminal preparation")
    }

    fn prepare_runtime_policy_terminal(
        store: &HlsAccessLeaseStore,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
    ) -> super::HlsTerminalTailPreparation {
        let lease = store.by_lease_id.get(lease_id).expect("runtime policy lease");
        let manifest = lease.last_manifest_snapshot.as_ref().expect("published runtime policy manifest");
        store
            .prepare_terminal_tail(
                lease_id,
                proxy_session_id,
                &HlsTerminalTailPreparationInput {
                    trigger: HlsFiniteTailTrigger::RuntimePolicy(reason),
                    expected_manifest_snapshot_generation: manifest.snapshot_generation,
                    expected_cursor_generation: lease.playback_cursor.cursor_generation,
                    origin_progress_generation: 4,
                    media_readiness_generation: 5,
                    origin_epoch: 6,
                    last_media_progress_at_ms: Some(2_000),
                    expected_acceptance_generation: HlsManifestAcceptanceGeneration(4),
                    terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::CutoverSnapshot,
                    cutover_timing: cutover_timing(),
                    commit_window: HlsTerminalCommitWindow::CutoverDue,
                    required_terminal_media_key: None,
                    terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                    reserve: cutover_reserve(),
                },
            )
            .expect("runtime policy terminal preparation")
    }

    fn commit_prepared_terminal(
        store: &mut HlsAccessLeaseStore,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &super::HlsTerminalTailPreparation,
    ) -> HlsTerminalCommitOutcome {
        store.commit_terminal_tail_if_generation_matches(
            lease_id,
            proxy_session_id,
            preparation,
            3_000,
            terminal_plan(preparation.decision_generation, proxy_session_id, lease_id),
        )
    }

    #[test]
    fn access_lease_id_is_short_and_opaque() {
        let lease_id = new_hls_access_lease_id();

        assert_eq!(lease_id.0.len(), 22);
        assert!(!lease_id.0.contains("alice"));
        assert!(!lease_id.0.contains("session"));
    }

    #[test]
    fn archive_playback_context_is_opt_in() {
        let live = lease(HlsAccessLeaseId("live".to_string()), "proxy-live", 1_000);
        assert_eq!(live.epg_reference_ts, None);
        assert_eq!(live.archive_origin_url, None);

        let archive = lease(HlsAccessLeaseId("archive".to_string()), "proxy-archive", 1_000).with_archive_playback(
            Some(1_784_898_000),
            Some("http://provider/channel/timeshift_abs-1784898000.m3u8".to_string()),
        );
        assert_eq!(archive.epg_reference_ts, Some(1_784_898_000));
        assert_eq!(
            archive.archive_origin_url.as_deref(),
            Some("http://provider/channel/timeshift_abs-1784898000.m3u8")
        );
    }

    #[test]
    fn policy_revocation_token_is_reason_and_generation_bound() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("policy-cas".to_string());
        let proxy_session_id = ProxySessionId("proxy-policy-cas".to_string());
        assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
        assert!(publish_manifest_snapshot(
            &mut store,
            &lease_id,
            &proxy_session_id,
            manifest_snapshot_for_route(1, &proxy_session_id, &lease_id),
            2_000,
        )
        .is_committed());
        assert!(store
            .activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000))
            .is_activated());
        let identity = store
            .response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .and_then(|lease| lease.media_identity())
            .expect("live identity");
        assert!(store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100,)
            .is_some());

        let started = store.begin_runtime_policy_revocation(
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            2_200,
        );
        let HlsRuntimePolicyRevocationOutcome::Started { token } = started else {
            panic!("expected a new policy revocation, got {started:?}");
        };
        assert_eq!(
            store.begin_runtime_policy_revocation(
                &lease_id,
                &proxy_session_id,
                HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                2_201,
            ),
            HlsRuntimePolicyRevocationOutcome::AlreadyPending { token: token.clone() }
        );
        assert_eq!(
            store.begin_runtime_policy_revocation(
                &lease_id,
                &proxy_session_id,
                HlsRuntimeCustomTailReason::UserAccountExpired,
                2_202,
            ),
            HlsRuntimePolicyRevocationOutcome::NoLongerEligible
        );
        let preparation = prepare_runtime_policy_terminal(
            &store,
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        );
        assert_eq!(preparation.runtime_policy_revocation, Some(token));

        store.by_lease_id.get_mut(&lease_id).expect("policy lease").admission_generation =
            preparation.expected_admission_generation.saturating_add(1);
        assert_eq!(
            store.commit_terminal_unavailable_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_300,
                HlsTerminalTailCompatibility::MissingAsset,
            ),
            HlsTerminalCommitOutcome::SupersededGeneration
        );
        let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("revoking lease");
        assert_eq!(lease.state, HlsAccessLeaseState::PolicyRevoking);
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    }

    #[test]
    fn unpublished_runtime_policy_denial_retains_first_reason_without_live_access() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("cold-policy-denial".to_string());
        let proxy_session_id = ProxySessionId("proxy-cold-policy-denial".to_string());
        assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
        assert!(store
            .activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000))
            .is_activated());
        assert_eq!(
            store.begin_runtime_policy_revocation(
                &lease_id,
                &proxy_session_id,
                HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                2_100,
            ),
            HlsRuntimePolicyRevocationOutcome::NoPublishedManifest
        );
        assert_eq!(
            store.deny_access_lease(
                &lease_id,
                HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd {
                    reason: HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                },
            ),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        );
        let denied = store.response_snapshot(&lease_id, &proxy_session_id, 2_100).expect("denied lease");
        assert_eq!(denied.state, HlsAccessLeaseState::Denied);
        assert_eq!(denied.playback_mode, HlsLeasePlaybackMode::Ended);
        assert_eq!(denied.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));

        assert_eq!(
            store.deny_access_lease(
                &lease_id,
                HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd {
                    reason: HlsRuntimeCustomTailReason::UserAccountExpired,
                },
            ),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        );
        let repeated = store.response_snapshot(&lease_id, &proxy_session_id, 2_100).expect("retained denied lease");
        assert_eq!(repeated.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));
    }

    #[test]
    fn user_revocation_tail_does_not_authorize_unfetched_live_suffix() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("policy-prefix".to_string());
        let proxy_session_id = ProxySessionId("proxy-policy-prefix".to_string());
        let mut snapshot = manifest_snapshot_for_route(1, &proxy_session_id, &lease_id);
        let mut segments = snapshot.visible_segments.to_vec();
        for proxy_seq in [42_u64, 43] {
            segments.push(HlsLeaseManifestSegment {
                proxy_seq,
                duration_ms: 6_000,
                uri: format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, proxy_seq),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            });
        }
        snapshot.visible_segments = Arc::from(segments);
        snapshot.last_proxy_seq = 43;
        snapshot.playlist_duration_ms = 24_000;
        snapshot.last_visible_media_end_ms = 24_000;
        assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, snapshot, 2_000).is_committed());
        assert!(store
            .activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000))
            .is_activated());
        let identity = store
            .response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .and_then(|lease| lease.media_identity())
            .expect("live identity");
        let completed = store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100)
            .expect("completed request token");
        assert_eq!(
            store.record_segment_request_completed_if_identity_matches(
                &lease_id,
                &proxy_session_id,
                identity,
                completed,
                2_150,
            ),
            Some(HlsPlaybackCompletionOutcome::Advanced)
        );
        assert!(store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 41, 2_175,)
            .is_some());
        assert!(matches!(
            store.begin_runtime_policy_revocation(
                &lease_id,
                &proxy_session_id,
                HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                2_200,
            ),
            HlsRuntimePolicyRevocationOutcome::Started { .. }
        ));
        assert!(store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 42, 2_201,)
            .is_none());

        let preparation = prepare_runtime_policy_terminal(
            &store,
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        );
        assert_eq!(preparation.manifest_snapshot.first_proxy_seq, 40);
        assert_eq!(preparation.manifest_snapshot.last_proxy_seq, 41);
        assert_eq!(preparation.manifest_snapshot.visible_proxy_seqs().collect::<Vec<_>>(), vec![40, 41]);
    }

    #[test]
    fn access_lease_activates_and_slides_validity() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store
            .activate_access_lease(&lease_id, &proxy_session_id, 10_000, timing(5_000, 30_000))
            .is_activated());
        assert_eq!(store.lease_state(&lease_id, 24_999), Some(HlsAccessLeaseState::Activated));
        assert!(store.touch_access_lease(&lease_id, 24_000, timing(5_000, 30_000)));
        assert_eq!(store.lease_state(&lease_id, 53_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn active_playback_snapshot_excludes_pending_idle_and_terminal_leases() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let active_id = HlsAccessLeaseId("active".to_string());
        let pending_id = HlsAccessLeaseId("pending".to_string());
        let terminal_id = HlsAccessLeaseId("terminal".to_string());
        for lease_id in [&active_id, &pending_id, &terminal_id] {
            store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
            assert!(publish_manifest_snapshot(&mut store, lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
                .is_committed());
        }
        assert!(store
            .activate_access_lease(&active_id, &proxy_session_id, 2_000, timing(5_000, 30_000))
            .is_activated());
        assert!(store
            .activate_access_lease(&terminal_id, &proxy_session_id, 2_000, timing(5_000, 30_000))
            .is_activated());
        let preparation = prepare_terminal(&mut store, &terminal_id, &proxy_session_id, 1, Some(1_000));
        assert_eq!(
            commit_prepared_terminal(&mut store, &terminal_id, &proxy_session_id, &preparation),
            HlsTerminalCommitOutcome::Committed
        );

        let snapshots = store.active_live_playback_snapshots_for_session(&proxy_session_id, 2_100);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].lease_id, active_id);
    }

    #[test]
    fn access_lease_idles_at_exact_active_until_boundary() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

        let snapshot = store.lifecycle_snapshot(&lease_id, 7_000).expect("lease should exist");

        assert_eq!(snapshot.state, HlsAccessLeaseState::Idle);
        assert!(snapshot.idle_release.is_some());
        assert_eq!(store.active_access_lease_count_for_session(&proxy_session_id, 7_000), 0);
    }

    #[test]
    fn access_lease_expires_at_exact_valid_until_boundary() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        let snapshot = store.lifecycle_snapshot(&lease_id, 17_000).expect("lease should exist");

        assert_eq!(snapshot.state, HlsAccessLeaseState::Expired);
        assert!(snapshot.idle_release.is_some());
        assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn access_lease_expires_without_activity() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert_eq!(
            store.activate_access_lease(&lease_id, &proxy_session_id, 17_000, timing(5_000, 15_000)),
            HlsAccessLeaseActivation::Expired
        );
    }

    #[test]
    fn same_family_leases_remain_independently_valid() {
        let mut store = HlsAccessLeaseStore::default();
        let old_lease_id = HlsAccessLeaseId("old".to_string());
        let new_lease_id = HlsAccessLeaseId("new".to_string());
        let proxy_a = ProxySessionId("proxy-a".to_string());
        let proxy_b = ProxySessionId("proxy-b".to_string());
        let family = HlsPlaybackFamilyKey::new("alice", "client-a");

        store.prepare_access_lease(HlsAccessLease::pending(
            old_lease_id.clone(),
            family.clone(),
            proxy_a.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "12345".to_string(),
            12345,
            1_000,
            15_000,
        ));
        assert!(store.activate_access_lease(&old_lease_id, &proxy_a, 2_000, timing(5_000, 15_000)).is_activated());
        store.prepare_access_lease(HlsAccessLease::pending(
            new_lease_id.clone(),
            family,
            proxy_b.clone(),
            "alice".to_string(),
            "session-b".to_string(),
            1,
            "67890".to_string(),
            67890,
            3_000,
            15_000,
        ));

        let activation = store.activate_access_lease(&new_lease_id, &proxy_b, 4_000, timing(5_000, 15_000));
        assert!(activation.is_activated());
        assert_eq!(store.lease_state(&old_lease_id, 4_000), Some(HlsAccessLeaseState::Activated));
        assert!(store.touch_access_lease(&old_lease_id, 5_000, timing(5_000, 15_000)));
        assert_eq!(store.lease_state(&old_lease_id, 19_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn manifest_touch_extends_activated_lease_active_window() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                6_000,
                Some(timing(10_000, 30_000)),
                None,
                15_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));
        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Activated);
        assert_eq!(lease.last_seen_at_ms, 6_000);
        assert_eq!(lease.pending_deadline, None);
        assert_eq!(lease.active_until_ms, Some(16_000));
        assert_eq!(lease.valid_until_ms, 36_000);
    }

    #[test]
    fn pending_lease_expires_at_pending_deadline_even_when_valid_window_is_longer() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let mut lease = lease(lease_id.clone(), &proxy_session_id.0, 1_000);
        lease.pending_deadline = Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 6_000 });
        lease.valid_until_ms = 31_000;
        store.prepare_access_lease(lease);

        assert_eq!(store.lease_state(&lease_id, 5_999), Some(HlsAccessLeaseState::Pending));
        let snapshot = store.lifecycle_snapshot(&lease_id, 6_000).expect("lease should exist");
        assert_eq!(snapshot.state, HlsAccessLeaseState::Expired);
        assert!(snapshot.idle_release.is_some(), "pending expiry must release counted user admission");
        assert_eq!(store.lease_state(&lease_id, 6_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn manifest_touch_can_shorten_pending_lease_to_follow_up_deadline() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                2_000,
                None,
                Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
                300_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));

        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Pending);
        assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
        assert_eq!(lease.valid_until_ms, 12_000);
        assert_eq!(store.lease_state(&lease_id, 11_999), Some(HlsAccessLeaseState::Pending));
        assert_eq!(store.lease_state(&lease_id, 12_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn bootstrap_touch_cannot_extend_existing_follow_up_pending_deadline() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                2_000,
                None,
                Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
                300_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));
        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                3_000,
                None,
                Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms: 100_000 }),
                300_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));

        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
        assert_eq!(lease.valid_until_ms, 12_000);
    }

    #[test]
    fn repeated_follow_up_touch_cannot_extend_existing_pending_deadline() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                2_000,
                None,
                Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }),
                300_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));
        assert!(matches!(
            store.touch_manifest_access_lease(
                &lease_id,
                &proxy_session_id,
                3_000,
                None,
                Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 30_000 }),
                300_000,
            ),
            HlsAccessLeaseTouch::Touched { .. }
        ));

        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
        assert_eq!(lease.valid_until_ms, 12_000);
    }

    #[test]
    fn session_follow_up_shortens_pending_lease_once() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        let shortened = store.mark_pending_manifest_follow_up_for_session(
            &proxy_session_id,
            2_000,
            HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 },
        );
        assert_eq!(shortened.len(), 1);
        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
        assert_eq!(lease.valid_until_ms, 12_000);

        let unchanged = store.mark_pending_manifest_follow_up_for_session(
            &proxy_session_id,
            3_000,
            HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 30_000 },
        );
        assert!(unchanged.is_empty());
        let lease = store.by_lease_id.get(&lease_id).expect("lease should remain stored");
        assert_eq!(lease.pending_deadline, Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms: 12_000 }));
        assert_eq!(lease.valid_until_ms, 12_000);
    }

    #[test]
    fn activated_lease_remains_valid_after_media_touch() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 15_000)).is_activated());

        assert!(store.touch_access_lease(&lease_id, 3_000, timing(5_000, 15_000)));
        assert_eq!(store.lease_state(&lease_id, 17_999), Some(HlsAccessLeaseState::Activated));
    }

    #[test]
    fn activated_lease_becomes_idle_after_active_window_but_remains_reactivatable() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());

        let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
        assert_eq!(snapshot.active_count, 0);
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
        assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Idle));
        assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 8_000));
        assert!(store.access_lease(&lease_id, &proxy_session_id, 8_000).is_some());

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 8_000, timing(5_000, 30_000)).is_activated());
        assert_eq!(store.session_snapshot(&proxy_session_id, 8_000).active_count, 1);
    }

    #[test]
    fn activated_lease_validity_expiry_reports_idle_release() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(30_000, 5_000)).is_activated());

        let snapshot = store.session_snapshot(&proxy_session_id, 8_000);
        assert_eq!(snapshot.active_count, 0);
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(snapshot.idle_releases[0].lease_id, lease_id);
        assert_eq!(store.lease_state(&lease_id, 8_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn usable_access_lease_query_accepts_pending_idle_and_active_activated_only() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let pending_id = HlsAccessLeaseId("pending".to_string());
        let idle_id = HlsAccessLeaseId("idle".to_string());
        let activated_id = HlsAccessLeaseId("activated".to_string());
        let denied_id = HlsAccessLeaseId("denied".to_string());
        let expired_id = HlsAccessLeaseId("expired".to_string());

        store.prepare_access_lease(lease(pending_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(idle_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(activated_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(denied_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(expired_id.clone(), &proxy_session_id.0, 1_000));
        assert!(store.activate_access_lease(&idle_id, &proxy_session_id, 2_000, timing(1_000, 15_000)).is_activated());
        assert!(store
            .activate_access_lease(&activated_id, &proxy_session_id, 2_000, timing(5_000, 15_000))
            .is_activated());
        assert!(matches!(
            store.deny_access_lease(&denied_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));

        assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));
        let snapshot = store.session_snapshot(&proxy_session_id, 3_000);
        assert_eq!(snapshot.active_count, 1);
        assert_eq!(snapshot.idle_releases.len(), 1);
        assert_eq!(snapshot.idle_releases[0].lease_id, idle_id);
        assert_eq!(store.lease_state(&idle_id, 3_000), Some(HlsAccessLeaseState::Idle));
        assert_eq!(store.active_access_lease_count_for_session(&proxy_session_id, 3_000), 1);
        assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 3_000));

        assert!(matches!(
            store.deny_access_lease(&pending_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));
        assert!(matches!(
            store.deny_access_lease(&activated_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));
        assert!(matches!(
            store.deny_access_lease(&expired_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));
        assert!(store.has_usable_access_lease_for_session(&proxy_session_id, 3_000));
        assert!(matches!(
            store.deny_access_lease(&idle_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));
        assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 2_000));
        assert!(!store.has_usable_access_lease_for_session(&proxy_session_id, 17_000));
        assert_eq!(store.lease_state(&expired_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn session_snapshot_prefers_normal_origin_policy_over_soft() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("soft".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Soft, -20),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("normal".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, 50),
        );

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, 50);
    }

    #[test]
    fn origin_policy_update_reclassifies_existing_access_lease() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        store.prepare_access_lease(
            lease(lease_id.clone(), &proxy_session_id.0, 1_000).with_origin_acquire_policy(ConnectionKind::Soft, 20),
        );

        let updated =
            store.update_origin_acquire_policy(&lease_id, ConnectionKind::Normal, -5).expect("lease should update");
        assert_eq!(updated.origin_connection_kind, ConnectionKind::Normal);
        assert_eq!(updated.origin_priority, -5);

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("updated policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, -5);
    }

    #[test]
    fn session_snapshot_uses_best_priority_within_same_origin_policy_kind() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("low-priority".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, 30),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("high-priority".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -5),
        );

        let snapshot = store.session_snapshot(&proxy_session_id, 2_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(policy.priority, -5);
    }

    #[test]
    fn session_snapshot_ignores_expired_and_denied_origin_policies() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let denied_id = HlsAccessLeaseId("denied".to_string());
        store.prepare_access_lease(
            lease(denied_id.clone(), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -100),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("expired".to_string()), &proxy_session_id.0, 1_000)
                .with_origin_acquire_policy(ConnectionKind::Normal, -50),
        );
        store.prepare_access_lease(
            lease(HlsAccessLeaseId("active-soft".to_string()), &proxy_session_id.0, 10_000)
                .with_origin_acquire_policy(ConnectionKind::Soft, 10),
        );
        assert!(matches!(
            store.deny_access_lease(&denied_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));

        let snapshot = store.session_snapshot(&proxy_session_id, 17_000);
        let policy = snapshot.effective_origin_policy.expect("usable lease policy");
        assert_eq!(policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(policy.priority, 10);
    }

    #[test]
    fn lease_manifest_generation_advances_for_snapshots_rendered_in_the_same_millisecond() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2_000), 2_000)
            .is_committed());
        let first = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("first manifest snapshot");
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2_000), 2_000)
            .is_committed());
        let second = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("second manifest snapshot");

        assert_eq!(first.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(1));
        assert_eq!(second.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(2));
        assert_eq!(
            first.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.delivered_at_ms),
            second.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.delivered_at_ms)
        );
    }

    #[test]
    fn newer_source_publication_wins_before_delayed_older_request() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        let older_request =
            store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("older request guard");
        let newer_request =
            store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("newer request guard");

        assert_eq!(
            store.commit_manifest_publication(
                &lease_id,
                &proxy_session_id,
                newer_request,
                manifest_snapshot(20),
                2_100,
            ),
            HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
        );
        assert_eq!(
            store.commit_manifest_publication(
                &lease_id,
                &proxy_session_id,
                older_request,
                manifest_snapshot(10),
                2_200,
            ),
            HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
        );
        let current = store
            .response_snapshot(&lease_id, &proxy_session_id, 2_200)
            .and_then(|lease| lease.last_manifest_snapshot)
            .expect("newer snapshot remains current");
        assert_eq!(current.source_render_marker, HlsManifestSourceRenderMarker::new(20));
        assert_eq!(current.snapshot_generation, 1);
    }

    #[test]
    fn lease_expiry_between_manifest_publication_prepare_and_commit_rejects_snapshot() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        let guard =
            store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("live publication guard");

        assert_eq!(
            store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 16_000,),
            HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired)
        );
        let lease = store.response_snapshot(&lease_id, &proxy_session_id, 16_000).expect("expired lease retained");
        assert_eq!(lease.state, HlsAccessLeaseState::Expired);
        assert!(lease.last_manifest_snapshot.is_none());
    }

    #[test]
    fn replacement_lease_incarnation_rejects_older_manifest_publication() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        let guard = store
            .prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000)
            .expect("original incarnation guard");
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 3_000));

        assert_eq!(
            store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 3_100,),
            HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::LeaseIncarnationChanged
            )
        );
    }

    #[test]
    fn changed_admission_generation_rejects_manifest_publication() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        let guard =
            store.prepare_manifest_publication(&lease_id, &proxy_session_id, 2_000).expect("original admission guard");
        let lease = store.by_lease_id.get_mut(&lease_id).expect("stored lease");
        lease.admission_generation = lease.admission_generation.saturating_add(1);

        assert_eq!(
            store.commit_manifest_publication(&lease_id, &proxy_session_id, guard, manifest_snapshot(10), 2_100,),
            HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged
            )
        );
    }

    #[test]
    fn terminal_preparation_is_read_only() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(7), 2_000)
            .is_committed());
        let before = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("live lease");
        let manifest_generation =
            before.last_manifest_snapshot.as_ref().expect("manifest snapshot").snapshot_generation;

        let preparation = store
            .prepare_terminal_tail(
                &lease_id,
                &proxy_session_id,
                &HlsTerminalTailPreparationInput {
                    trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                    expected_manifest_snapshot_generation: manifest_generation,
                    expected_cursor_generation: before.playback_cursor.cursor_generation,
                    origin_progress_generation: 4,
                    media_readiness_generation: 9,
                    origin_epoch: 0,
                    last_media_progress_at_ms: Some(1_900),
                    expected_acceptance_generation: HlsManifestAcceptanceGeneration(4),
                    terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::AcceptanceEpisode {
                        generation: HlsManifestAcceptanceGeneration(4),
                    },
                    cutover_timing: cutover_timing(),
                    commit_window: HlsTerminalCommitWindow::CutoverDue,
                    required_terminal_media_key: None,
                    terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                    reserve: cutover_reserve(),
                },
            )
            .expect("terminal preparation");
        let after = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("live lease");

        assert_eq!(before, after);
        assert_eq!(preparation.expected_admission_generation, before.admission_generation);
        assert_eq!(preparation.media_readiness_generation, 9);
        assert_eq!(preparation.manifest_snapshot_generation, manifest_generation);
        assert_eq!(preparation.cutover_timing, cutover_timing());
        assert_eq!(preparation.terminal_media_preparation, HlsTerminalMediaPreparationState::Failed { key: None });
    }

    #[test]
    fn expired_lease_lookup_rejects_stale_entry_without_removing_before_lifecycle() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));

        assert!(store.access_lease(&lease_id, &proxy_session_id, 17_000).is_none());
        assert_eq!(store.lease_state(&lease_id, 17_000), Some(HlsAccessLeaseState::Expired));
    }

    #[test]
    fn hls_terminal_commit_stale_snapshot_cannot_replace_live_generation() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());

        let stale = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());

        assert_eq!(
            store.commit_terminal_tail_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &stale,
                3_000,
                terminal_plan(stale.decision_generation, &proxy_session_id, &lease_id),
            ),
            HlsTerminalCommitOutcome::SupersededGeneration
        );
        let lease = store.response_snapshot(&lease_id, &proxy_session_id, 3_000).expect("lease remains stored");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert_eq!(lease.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(2));
    }

    #[test]
    fn hls_terminal_commit_plan_bound_to_another_route_is_superseded() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
        let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
        let other_lease_id = HlsAccessLeaseId("other-lease".to_string());

        assert_eq!(
            store.commit_terminal_tail_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                3_000,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &other_lease_id),
            ),
            HlsTerminalCommitOutcome::SupersededGeneration
        );
        assert_eq!(
            store.response_snapshot(&lease_id, &proxy_session_id, 3_000).expect("live lease").playback_mode,
            HlsLeasePlaybackMode::Live
        );
    }

    #[test]
    fn hls_terminal_commit_lease_expiry_wins_the_race() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
        let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));

        assert_eq!(
            store.commit_terminal_tail_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                16_000,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            ),
            HlsTerminalCommitOutcome::LeaseNoLongerEligible
        );
        let lease =
            store.response_snapshot(&lease_id, &proxy_session_id, 16_000).expect("expired lease remains stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Expired);
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
        assert_eq!(lease.admission_generation, preparation.expected_admission_generation);
    }

    #[test]
    fn hls_terminal_commit_exact_replay_is_idempotent_and_sticky() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
        let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
        let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
        let concurrent_plan = terminal_plan_at(preparation.decision_generation, &proxy_session_id, &lease_id, 3_100);

        assert_eq!(
            store.commit_terminal_tail_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                3_000,
                Arc::clone(&plan),
            ),
            HlsTerminalCommitOutcome::Committed
        );
        assert_eq!(
            store.commit_terminal_tail_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                3_100,
                concurrent_plan,
            ),
            HlsTerminalCommitOutcome::AlreadyCommitted
        );
        assert!(store.prepare_manifest_publication(&lease_id, &proxy_session_id, 3_100).is_none());
    }

    #[test]
    fn hls_terminal_commit_unavailable_replay_is_idempotent_for_the_same_decision_generation() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
        let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));

        assert_eq!(
            store.commit_terminal_unavailable_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                3_000,
                HlsTerminalTailCompatibility::MissingAsset,
            ),
            HlsTerminalCommitOutcome::Committed
        );
        assert_eq!(
            store.commit_terminal_unavailable_if_generation_matches(
                &lease_id,
                &proxy_session_id,
                &preparation,
                3_100,
                HlsTerminalTailCompatibility::InvalidAsset,
            ),
            HlsTerminalCommitOutcome::AlreadyCommitted
        );
    }

    #[test]
    fn hls_terminal_commit_terminal_lease_rejects_recovery_while_other_lease_remains_live() {
        let mut store = HlsAccessLeaseStore::default();
        let terminal_lease_id = HlsAccessLeaseId("terminal".to_string());
        let live_lease_id = HlsAccessLeaseId("live".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(terminal_lease_id.clone(), &proxy_session_id.0, 1_000));
        store.prepare_access_lease(lease(live_lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(
            &mut store,
            &terminal_lease_id,
            &proxy_session_id,
            manifest_snapshot(1),
            2_000,
        )
        .is_committed());
        assert!(publish_manifest_snapshot(&mut store, &live_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,)
            .is_committed());
        let stale_terminal_publication = store
            .prepare_manifest_publication(&terminal_lease_id, &proxy_session_id, 2_000)
            .expect("pre-terminal publication guard");
        let preparation = prepare_terminal(&mut store, &terminal_lease_id, &proxy_session_id, 7, Some(2_000));
        assert_eq!(
            commit_prepared_terminal(&mut store, &terminal_lease_id, &proxy_session_id, &preparation),
            HlsTerminalCommitOutcome::Committed
        );

        assert_eq!(
            store.commit_manifest_publication(
                &terminal_lease_id,
                &proxy_session_id,
                stale_terminal_publication,
                manifest_snapshot(2),
                3_000,
            ),
            HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged
            )
        );
        assert!(store.prepare_manifest_publication(&terminal_lease_id, &proxy_session_id, 3_000).is_none());
        assert!(publish_manifest_snapshot(&mut store, &live_lease_id, &proxy_session_id, manifest_snapshot(2), 3_000,)
            .is_committed());
        assert!(matches!(
            store
                .response_snapshot(&terminal_lease_id, &proxy_session_id, 3_000)
                .expect("terminal lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
        assert_eq!(
            store.response_snapshot(&live_lease_id, &proxy_session_id, 3_000).expect("live lease").playback_mode,
            HlsLeasePlaybackMode::Live
        );
    }

    #[test]
    fn late_segment_completion_after_terminal_transition_does_not_advance_cursor() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
        let identity = store
            .response_snapshot(&lease_id, &proxy_session_id, 2_100)
            .and_then(|lease| lease.media_identity())
            .expect("live identity");
        let token = store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100)
            .expect("live request token");
        let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 8, Some(2_000));
        assert_eq!(
            commit_prepared_terminal(&mut store, &lease_id, &proxy_session_id, &preparation),
            HlsTerminalCommitOutcome::Committed
        );

        assert_eq!(
            store.record_segment_request_completed_if_identity_matches(
                &lease_id,
                &proxy_session_id,
                identity,
                token,
                2_200,
            ),
            None
        );
        let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("terminal lease");
        assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
    }

    #[test]
    fn late_segment_completion_after_forward_seek_is_stale_and_does_not_advance_cursor() {
        let mut store = HlsAccessLeaseStore::default();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("proxy".to_string());
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        let identity = store
            .response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .and_then(|lease| lease.media_identity())
            .expect("live identity");
        let stale_token = store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_000)
            .expect("first request token");
        let _current_token = store
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 50, 2_100)
            .expect("forward-seek request token");

        assert_eq!(
            store.record_segment_request_completed_if_identity_matches(
                &lease_id,
                &proxy_session_id,
                identity,
                stale_token,
                2_200,
            ),
            Some(HlsPlaybackCompletionOutcome::StaleRequest)
        );
        let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("live lease");
        assert_eq!(lease.playback_cursor.first_requested_proxy_seq, Some(50));
        assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
    }

    #[test]
    fn hls_availability_reevaluation_evidence_generation_is_per_session_and_noop_stable() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_a = ProxySessionId("availability-evidence-a".to_string());
        let proxy_b = ProxySessionId("availability-evidence-b".to_string());
        let primary_lease_id = HlsAccessLeaseId("availability-evidence-a".to_string());
        let secondary_lease_id = HlsAccessLeaseId("availability-evidence-b".to_string());
        let lease_a = lease(primary_lease_id.clone(), &proxy_a.0, 1_000);
        assert!(store.prepare_access_lease(lease_a.clone()));
        let added_a = store.availability_evidence_generation(&proxy_a);
        assert!(added_a.as_u64() > 0);

        assert!(store.prepare_access_lease(lease_a));
        assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);
        assert!(matches!(
            store.activate_access_lease(&primary_lease_id, &proxy_b, 2_000, timing(5_000, 30_000)),
            HlsAccessLeaseActivation::SessionMismatch
        ));
        assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);

        assert!(store.prepare_access_lease(lease(secondary_lease_id, &proxy_b.0, 1_000)));
        assert_eq!(store.availability_evidence_generation(&proxy_a), added_a);
        assert!(store.availability_evidence_generation(&proxy_b).as_u64() > added_a.as_u64());

        assert!(store.activate_access_lease(&primary_lease_id, &proxy_a, 2_000, timing(5_000, 30_000)).is_activated());
        let activated_a = store.availability_evidence_generation(&proxy_a);
        assert!(activated_a > added_a);
        assert_eq!(store.availability_evidence_generation(&proxy_b).as_u64(), added_a.as_u64().saturating_add(1));

        let identity = store
            .response_snapshot(&primary_lease_id, &proxy_a, 2_000)
            .and_then(|lease| lease.media_identity())
            .expect("activated live lease identity");
        let token = store
            .record_segment_request_started_if_identity_matches(&primary_lease_id, &proxy_a, identity, 7, 2_100)
            .expect("cursor request starts");
        let requested_a = store.availability_evidence_generation(&proxy_a);
        assert!(requested_a > activated_a);
        assert_eq!(
            store.record_segment_request_completed_if_identity_matches(
                &primary_lease_id,
                &proxy_a,
                identity,
                token,
                2_200,
            ),
            Some(HlsPlaybackCompletionOutcome::Advanced)
        );
        let completed_a = store.availability_evidence_generation(&proxy_a);
        assert!(completed_a > requested_a);
        assert_eq!(
            store.record_segment_request_completed_if_identity_matches(
                &primary_lease_id,
                &proxy_a,
                identity,
                token,
                2_300,
            ),
            Some(HlsPlaybackCompletionOutcome::Duplicate)
        );
        assert_eq!(store.availability_evidence_generation(&proxy_a), completed_a);

        assert!(store.remove_access_lease(&primary_lease_id).is_some());
        let removed_a = store.availability_evidence_generation(&proxy_a);
        assert!(removed_a > completed_a);
        assert_eq!(store.availability_evidence_generation(&proxy_b).as_u64(), added_a.as_u64().saturating_add(1));
        assert!(store.prepare_access_lease(lease(primary_lease_id, &proxy_a.0, 3_000)));
        assert!(store.availability_evidence_generation(&proxy_a) > removed_a);
    }

    #[test]
    fn hls_availability_reevaluation_evidence_generation_fails_closed_on_overflow() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("availability-evidence-overflow".to_string());
        let lease_id = HlsAccessLeaseId("availability-evidence-overflow".to_string());
        store.last_availability_evidence_generation = HlsAvailabilityEvidenceGeneration::for_test(u64::MAX);

        assert!(!store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
        assert!(store.response_snapshot(&lease_id, &proxy_session_id, 1_000).is_none());
        assert_eq!(store.availability_evidence_generation(&proxy_session_id).as_u64(), 0);
    }

    #[test]
    fn bulk_lease_removal_advances_and_preserves_session_evidence_generation() {
        let mut store = HlsAccessLeaseStore::default();
        let proxy_session_id = ProxySessionId("bulk-removal".to_string());
        let first = HlsAccessLeaseId("bulk-removal-a".to_string());
        let second = HlsAccessLeaseId("bulk-removal-b".to_string());
        assert!(store.prepare_access_lease(lease(first, &proxy_session_id.0, 1_000)));
        assert!(store.prepare_access_lease(lease(second, &proxy_session_id.0, 1_001)));
        let before_removal = store.availability_evidence_generation(&proxy_session_id);

        assert_eq!(store.remove_access_leases_for_session(&proxy_session_id).len(), 2);
        let after_removal = store.availability_evidence_generation(&proxy_session_id);
        assert!(after_removal > before_removal);
        assert!(store.remove_access_leases_for_session(&proxy_session_id).is_empty());
        assert_eq!(store.availability_evidence_generation(&proxy_session_id), after_removal);
    }
}
