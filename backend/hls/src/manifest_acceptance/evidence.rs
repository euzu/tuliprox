use super::{HlsManifestAcceptanceTrigger, HlsMediaResourceIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsCandidateHostRelation {
    PinnedHost,
    OtherHost,
    InitialBaseline,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsHostLocalSequenceRelation {
    NoBaseline,
    Same,
    Next,
    PlausibleForward,
    Backward,
    RolloverCandidate,
    Rebase,
}

/// Bounded, URL-normalized metadata for one candidate segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HlsManifestSegmentFingerprint {
    pub duration_ms: u64,
    pub discontinuity_before: bool,
    pub program_date_time_ms: Option<i64>,
    pub normalized_resource_identity: Option<HlsMediaResourceIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HlsManifestTimelineFingerprint {
    pub segment_count: u32,
    pub first_program_date_time_ms: Option<i64>,
    pub last_program_date_time_ms: Option<i64>,
    pub duration_pattern_hash: [u8; 32],
    pub discontinuity_pattern_hash: [u8; 32],
    pub normalized_resource_pattern_hash: Option<[u8; 32]>,
    pub map_and_encryption_hash: [u8; 32],
    pub container_signature_hash: [u8; 32],
    pub segment_samples: Vec<HlsManifestSegmentFingerprint>,
}

/// Stable, window-independent technical identity of an alternative timeline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HlsManifestTechnicalSignature {
    pub map_and_encryption_hash: [u8; 32],
    pub container_signature_hash: [u8; 32],
}

impl HlsManifestTechnicalSignature {
    pub(super) fn from_fingerprint(fingerprint: &HlsManifestTimelineFingerprint) -> Self {
        Self {
            map_and_encryption_hash: fingerprint.map_and_encryption_hash,
            container_signature_hash: fingerprint.container_signature_hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HlsAlternativeOriginCohortIdentity {
    pub effective_host: String,
    pub technical_signature: HlsManifestTechnicalSignature,
}

/// One concrete sliding playlist window observed for a stable cohort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsAlternativeOriginWindow {
    pub host_local_media_sequence: u64,
    pub host_local_highwater: Option<u64>,
    pub fingerprint: HlsManifestTimelineFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsCrossHostAcceptanceEvidence {
    Insufficient,
    StrongTimelineAnchor { overlapping_segments: u16 },
    BurstConsensusNewEpoch { successful_samples: u16 },
}

/// State of the segment that would become the first segment after a host switch.
///
/// Merely parsing a URI is not READY evidence. Alternative plans carrying
/// `RequiresStaging` must fetch and atomically commit that segment before the
/// commit callback may publish the selected manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsSwitchSegmentReadiness {
    Unavailable,
    RequiresStaging,
}

impl HlsSwitchSegmentReadiness {
    pub(super) const fn can_be_staged(self) -> bool { matches!(self, Self::RequiresStaging) }
}

/// Manifest-level eligibility for the Critical single-candidate path. Track
/// compatibility is deliberately deferred until the candidate bytes are
/// staged and READY; it cannot be inferred from a URI or extension alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsEmergencyLiveHandoffCompatibility {
    Incompatible,
    RequiresStagedTrackVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalAlternativeCompatibility {
    /// Staged media and the configured terminal asset must still be compared.
    RequiresStagedComparison,
    LiveHandoffSafer,
    /// Available terminal media is already known to be at least as safe.
    TerminalTailPreferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsEmergencyAcceptanceEvidence {
    pub live_handoff: HlsEmergencyLiveHandoffCompatibility,
    pub terminal_alternative: HlsTerminalAlternativeCompatibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsCommittedContentAnchorEvidence {
    Unavailable,
    RequiresStagedByteVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsResourceTimelineEvidence {
    Eligible,
    ReplayOnly,
    ContradictoryOrder,
}

impl HlsResourceTimelineEvidence {
    pub(super) const fn permits_acceptance(self) -> bool { matches!(self, Self::Eligible) }
}

impl HlsEmergencyAcceptanceEvidence {
    pub const INCOMPATIBLE: Self = Self {
        live_handoff: HlsEmergencyLiveHandoffCompatibility::Incompatible,
        terminal_alternative: HlsTerminalAlternativeCompatibility::TerminalTailPreferred,
    };

    pub(super) const fn requires_staged_verification(self) -> bool {
        matches!(
            self,
            Self {
                live_handoff: HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification,
                terminal_alternative: HlsTerminalAlternativeCompatibility::RequiresStagedComparison,
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestCandidateObservation {
    pub candidate_index: usize,
    pub candidate_slot: usize,
    pub effective_host: Option<String>,
    pub host_relation: HlsCandidateHostRelation,
    pub host_local_media_sequence: u64,
    pub host_local_highwater: Option<u64>,
    pub local_sequence_relation: Option<HlsHostLocalSequenceRelation>,
    pub resource_timeline_evidence: HlsResourceTimelineEvidence,
    pub timeline_fingerprint: HlsManifestTimelineFingerprint,
    pub manifest_fetch_elapsed_ms: u64,
    pub switch_segment_readiness: HlsSwitchSegmentReadiness,
    pub committed_content_anchor: HlsCommittedContentAnchorEvidence,
    pub emergency_evidence: HlsEmergencyAcceptanceEvidence,
    pub evidence: HlsCrossHostAcceptanceEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsAlternativeOriginCohort {
    pub identity: HlsAlternativeOriginCohortIdentity,
    pub window: HlsAlternativeOriginWindow,
    pub successful_samples: u16,
    pub total_samples: u16,
    pub consecutive_confirmed_full_bursts: u16,
    pub evidence: HlsCrossHostAcceptanceEvidence,
    pub best_candidate_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestCommitKind {
    Pinned,
    AnchoredAlternative,
    ContentVerifiedAlternative,
    AlternativeAsNewEpoch,
    EmergencyAlternativeAsNewEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsPinnedOriginObservationState {
    Missing,
    Unchanged,
    Progressed,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestAcceptanceLandscape {
    pub pinned_state: HlsPinnedOriginObservationState,
    pub alternatives: Vec<(HlsAlternativeOriginCohortIdentity, HlsAlternativeOriginWindow)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsReducedRetryLandscapeChange {
    Unchanged,
    NewCohort,
    TimelineConflict,
    PinnedStateChanged,
}

impl HlsReducedRetryLandscapeChange {
    pub const fn requires_full_requalification(self) -> bool { !matches!(self, Self::Unchanged) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestCommitPlan {
    Commit { candidate_index: usize, kind: HlsManifestCommitKind },
    StageAlternative { candidate_index: usize, kind: HlsManifestCommitKind },
    HoldAlternative,
    RejectAll,
}

#[derive(Clone, Copy)]
pub struct HlsManifestAcceptanceInput<'a> {
    pub full_burst_completed: bool,
    pub current_burst_is_full_plan: bool,
    pub trigger: HlsManifestAcceptanceTrigger,
    pub previous_alternative: Option<&'a HlsAlternativeOriginCohort>,
    pub observations: &'a [HlsManifestCandidateObservation],
}
