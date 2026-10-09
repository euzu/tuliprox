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
    HlsEffectiveOriginAcquirePolicy, HlsPublishedTransientResourceIds, ProxySessionId, TransientManifestLeaseBinding,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};
use tuliprox_session::ConnectionKind;

const HLS_ACCESS_LEASE_ID_BYTES: usize = 16;

/// Monotonic identity of the lease/cursor evidence used for one proxy
/// session's availability decision. The zero value denotes a session without
/// stored lease evidence; committed generations are process-lifetime unique.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct HlsAvailabilityEvidenceGeneration(u64);

/// Short opaque lookup key for a server-side HLS access lease.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct HlsAccessLeaseId(pub String);

/// Stable user/player family used only for diagnostics or future UX grouping.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HlsPlaybackFamilyKey {
    pub username: String,
    pub client_fingerprint: String,
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
    revision_bindings: Option<Arc<HlsLeaseRevisionBindings>>,
    progressive_startup_claimed: bool,
    published_transient_resource_ids: HlsPublishedTransientResourceIds,
    published_finalized_manifest_generations: BTreeSet<super::TransientManifestGeneration>,
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
    ManifestGenerationUnavailable,
    RevisionConflict,
    SnapshotGenerationExhausted,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[must_use]
pub enum HlsLeaseManifestPublicationOutcome {
    Committed { snapshot_generation: u64 },
    Rejected(HlsLeaseManifestPublicationRejectReason),
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
    pub(crate) finalized_transient_manifest_bindings: Vec<TransientManifestLeaseBinding>,
}

#[cfg(test)]
mod tests;

mod error;
mod identity;
mod lifecycle;
mod publication;
mod revision_bindings;
mod runtime_policy;
mod terminal;
mod timing;

pub use self::identity::new_hls_access_lease_id;
use self::{
    error::HlsAvailabilityEvidenceAdvanceError,
    identity::HlsMediaLeasePlaybackIdentity,
    lifecycle::lease_state_allows_use,
    revision_bindings::{commit_lease_revision_bindings, HlsLeaseRevisionBindings},
    runtime_policy::runtime_policy_authorized_manifest_prefix,
};
