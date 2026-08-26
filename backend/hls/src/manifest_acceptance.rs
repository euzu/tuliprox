#[cfg(any(test, feature = "test-support"))]
use super::recovery_timing::HlsRecoveryEtaMs;
use super::{
    deterministic_conflict::HlsDeterministicTimelineConflict,
    recovery_timing::{
        HlsAcceptanceEpisodeTiming, HlsEstimatedRecoveryCompletionAtMs, HlsRecoveryWorkload,
        HlsRecoveryWorkloadEnvelope,
    },
    resource_identity::HlsMediaResourceIdentity,
};
use sha2::{Digest, Sha256};
use shared::model::HlsManifestRecoveryBurstPlan;

pub const HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS: u64 = 100;
pub const HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT: usize = 64;
const HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT: usize = 32;
pub const HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HlsManifestAcceptanceGeneration(pub u64);

/// Fixed-size, non-sensitive identity of one selected recovery candidate.
#[derive(Debug, Clone, Copy)]
pub struct HlsManifestRecoveryCandidateIdentity {
    candidate_index: usize,
    effective_host_fingerprint: [u8; 32],
    manifest_fingerprint: [u8; 32],
}

impl PartialEq for HlsManifestRecoveryCandidateIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.candidate_index == other.candidate_index
            && self.effective_host_fingerprint == other.effective_host_fingerprint
            && self.manifest_fingerprint == other.manifest_fingerprint
    }
}

impl Eq for HlsManifestRecoveryCandidateIdentity {}

impl HlsManifestRecoveryCandidateIdentity {
    pub fn from_candidate(candidate_index: usize, effective_host: Option<&str>, manifest_body: &str) -> Self {
        let mut host_hasher = Sha256::new();
        host_hasher.update(b"tuliprox-hls-candidate-host-v1\0");
        match effective_host {
            Some(host) => {
                host_hasher.update([1]);
                host_hasher.update(host.as_bytes());
            }
            None => host_hasher.update([0]),
        }
        let mut manifest_hasher = Sha256::new();
        manifest_hasher.update(b"tuliprox-hls-candidate-manifest-v1\0");
        manifest_hasher.update(manifest_body.as_bytes());
        Self {
            candidate_index,
            effective_host_fingerprint: host_hasher.finalize().into(),
            manifest_fingerprint: manifest_hasher.finalize().into(),
        }
    }

    pub fn matches_candidate(self, effective_host: Option<&str>, manifest_body: &str) -> bool {
        self == Self::from_candidate(self.candidate_index, effective_host, manifest_body)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsSelectedRecoveryCandidate {
    acceptance_generation: HlsManifestAcceptanceGeneration,
    candidate_identity: HlsManifestRecoveryCandidateIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsRecoveryWorkloadBinding {
    CandidateUnknown {
        envelope: HlsRecoveryWorkloadEnvelope,
        selected_candidate: Option<HlsSelectedRecoveryCandidate>,
    },
    CandidateBound {
        envelope: HlsRecoveryWorkloadEnvelope,
        acceptance_generation: HlsManifestAcceptanceGeneration,
        candidate_identity: HlsManifestRecoveryCandidateIdentity,
        workload: HlsRecoveryWorkload,
    },
}

impl HlsRecoveryWorkloadBinding {
    const fn envelope(self) -> HlsRecoveryWorkloadEnvelope {
        match self {
            Self::CandidateUnknown { envelope, .. } | Self::CandidateBound { envelope, .. } => envelope,
        }
    }

    const fn workload(self) -> HlsRecoveryWorkload {
        match self {
            Self::CandidateUnknown { envelope, .. } => envelope.ceiling(),
            Self::CandidateBound { workload, .. } => workload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoveryWorkloadBindingUpdate {
    Applied,
    StaleGeneration,
    EpisodeInactive,
    CandidateMismatch,
    OutsideEnvelope,
}

/// Immutable pressure snapshot that authorizes one manifest-acceptance episode.
///
/// Entering `Recovering` is an execution-state transition, not evidence that
/// reserve pressure exists. Consumers must therefore derive acceptance policy
/// from this trigger captured before the episode starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HlsManifestAcceptanceTrigger {
    #[default]
    None,
    Observe,
    RecoveryRequired,
    Critical,
}

impl HlsManifestAcceptanceTrigger {
    pub const fn starts_episode(self) -> bool { !matches!(self, Self::None) }

    pub const fn recovery_required(self) -> bool { matches!(self, Self::RecoveryRequired | Self::Critical) }

    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Observe => "observe",
            Self::RecoveryRequired => "recovery_required",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestAcceptanceEpisode {
    pub generation: HlsManifestAcceptanceGeneration,
    pub started_at_ms: u64,
    pub burst_plan: HlsManifestRecoveryBurstPlan,
    trigger: HlsManifestAcceptanceTrigger,
    timing: HlsAcceptanceEpisodeTiming,
    workload_binding: HlsRecoveryWorkloadBinding,
    pub full_burst_completed: bool,
    pub full_bursts_completed: u16,
    pub completed_burst_candidates: usize,
    pub state: HlsManifestAcceptanceState,
    outcome: HlsManifestAcceptanceEpisodeOutcome,
    pub held_alternative: Option<HlsAlternativeOriginCohort>,
    pub observed_landscape: Option<HlsManifestAcceptanceLandscape>,
    pub next_retry_at_ms: Option<u64>,
    deterministic_conflict_receipt: Option<HlsDeterministicConflictReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsDeterministicConflictReceipt {
    pub conflict: HlsDeterministicTimelineConflict,
    pub origin_progress_generation: u64,
    pub published_resource_history_generation: u64,
    pub pinned_host_generation: u64,
}

impl HlsManifestAcceptanceEpisode {
    pub fn new(
        generation: HlsManifestAcceptanceGeneration,
        started_at_ms: u64,
        burst_plan: HlsManifestRecoveryBurstPlan,
        trigger: HlsManifestAcceptanceTrigger,
        timing: &HlsAcceptanceEpisodeTiming,
    ) -> Self {
        let envelope = HlsRecoveryWorkloadEnvelope::from_timing_ceiling(timing.initial_workload);
        Self {
            generation,
            started_at_ms,
            burst_plan,
            trigger,
            timing: *timing,
            workload_binding: HlsRecoveryWorkloadBinding::CandidateUnknown { envelope, selected_candidate: None },
            full_burst_completed: false,
            full_bursts_completed: 0,
            completed_burst_candidates: 0,
            state: HlsManifestAcceptanceState::FullBurstPending,
            outcome: HlsManifestAcceptanceEpisodeOutcome::Pending,
            held_alternative: None,
            observed_landscape: None,
            next_retry_at_ms: None,
            deterministic_conflict_receipt: None,
        }
    }

    pub fn required_candidates(&self) -> usize { self.burst_plan.total_candidates() }

    pub const fn trigger(&self) -> HlsManifestAcceptanceTrigger { self.trigger }

    pub const fn timing(&self) -> HlsAcceptanceEpisodeTiming { self.timing }

    pub fn deterministic_conflict_receipt(&self) -> Option<&HlsDeterministicConflictReceipt> {
        self.deterministic_conflict_receipt.as_ref()
    }

    pub fn record_deterministic_conflict(&mut self, receipt: HlsDeterministicConflictReceipt) {
        if self.state != HlsManifestAcceptanceState::Completed {
            self.deterministic_conflict_receipt = Some(receipt);
            if self.full_burst_completed {
                self.held_alternative = None;
                self.next_retry_at_ms = None;
                self.state = HlsManifestAcceptanceState::Holding;
                self.outcome = HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(
                    HlsManifestAcceptanceExhaustionReason::DeterministicTimelineConflict,
                );
            }
        }
    }

    pub fn selected_candidate_identity(&self) -> Option<HlsManifestRecoveryCandidateIdentity> {
        match self.workload_binding {
            HlsRecoveryWorkloadBinding::CandidateBound { candidate_identity, .. } => Some(candidate_identity),
            HlsRecoveryWorkloadBinding::CandidateUnknown { selected_candidate, .. } => match selected_candidate {
                Some(selected) if selected.acceptance_generation == self.generation => {
                    Some(selected.candidate_identity)
                }
                Some(_) | None => None,
            },
        }
    }

    /// Records the selected candidate identity without claiming any media-workload evidence.
    ///
    /// Same-host commits deliberately remain in `CandidateUnknown`; an alternative candidate is
    /// bound only after its generation-local handoff preview identifies the exact recovery medium.
    pub fn select_candidate(
        &mut self,
        expected_generation: HlsManifestAcceptanceGeneration,
        candidate_identity: HlsManifestRecoveryCandidateIdentity,
    ) -> HlsRecoveryWorkloadBindingUpdate {
        if self.generation != expected_generation {
            return HlsRecoveryWorkloadBindingUpdate::StaleGeneration;
        }
        if self.outcome != HlsManifestAcceptanceEpisodeOutcome::Pending
            || !matches!(
                self.state,
                HlsManifestAcceptanceState::StagingSwitchSegment | HlsManifestAcceptanceState::Committing
            )
        {
            return HlsRecoveryWorkloadBindingUpdate::EpisodeInactive;
        }
        let HlsRecoveryWorkloadBinding::CandidateUnknown { envelope, selected_candidate } = self.workload_binding
        else {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        };
        let selected = HlsSelectedRecoveryCandidate { acceptance_generation: expected_generation, candidate_identity };
        if selected_candidate.is_some_and(|current| current != selected) {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        }
        self.workload_binding =
            HlsRecoveryWorkloadBinding::CandidateUnknown { envelope, selected_candidate: Some(selected) };
        HlsRecoveryWorkloadBindingUpdate::Applied
    }

    pub fn bind_selected_candidate(
        &mut self,
        expected_generation: HlsManifestAcceptanceGeneration,
        candidate_identity: HlsManifestRecoveryCandidateIdentity,
        workload: HlsRecoveryWorkload,
    ) -> HlsRecoveryWorkloadBindingUpdate {
        if self.generation != expected_generation {
            return HlsRecoveryWorkloadBindingUpdate::StaleGeneration;
        }
        if self.outcome != HlsManifestAcceptanceEpisodeOutcome::Pending
            || !matches!(
                self.state,
                HlsManifestAcceptanceState::StagingSwitchSegment | HlsManifestAcceptanceState::Committing
            )
        {
            return HlsRecoveryWorkloadBindingUpdate::EpisodeInactive;
        }
        let HlsRecoveryWorkloadBinding::CandidateUnknown { envelope, selected_candidate } = self.workload_binding
        else {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        };
        if selected_candidate
            != Some(HlsSelectedRecoveryCandidate { acceptance_generation: expected_generation, candidate_identity })
        {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        }
        if !envelope.contains(workload) {
            return HlsRecoveryWorkloadBindingUpdate::OutsideEnvelope;
        }
        self.workload_binding = HlsRecoveryWorkloadBinding::CandidateBound {
            envelope,
            acceptance_generation: expected_generation,
            candidate_identity,
            workload,
        };
        HlsRecoveryWorkloadBindingUpdate::Applied
    }

    pub fn advance_bound_candidate(
        &mut self,
        expected_generation: HlsManifestAcceptanceGeneration,
        expected_identity: HlsManifestRecoveryCandidateIdentity,
        workload: HlsRecoveryWorkload,
    ) -> HlsRecoveryWorkloadBindingUpdate {
        if self.generation != expected_generation {
            return HlsRecoveryWorkloadBindingUpdate::StaleGeneration;
        }
        if self.outcome != HlsManifestAcceptanceEpisodeOutcome::Pending
            || self.state != HlsManifestAcceptanceState::StagingSwitchSegment
        {
            return HlsRecoveryWorkloadBindingUpdate::EpisodeInactive;
        }
        let HlsRecoveryWorkloadBinding::CandidateBound {
            envelope,
            acceptance_generation,
            candidate_identity,
            workload: previous_workload,
        } = self.workload_binding
        else {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        };
        if acceptance_generation != expected_generation || candidate_identity != expected_identity {
            return HlsRecoveryWorkloadBindingUpdate::CandidateMismatch;
        }
        if !envelope.contains(workload) || !workload.is_no_greater_than(previous_workload) {
            return HlsRecoveryWorkloadBindingUpdate::OutsideEnvelope;
        }
        self.workload_binding = HlsRecoveryWorkloadBinding::CandidateBound {
            envelope,
            acceptance_generation,
            candidate_identity,
            workload,
        };
        HlsRecoveryWorkloadBindingUpdate::Applied
    }

    /// Returns the work still attributable to this exact episode generation.
    ///
    /// The timing snapshot itself remains unchanged. Only completed burst work
    /// and exact candidate-bound evidence may reduce the current estimate.
    pub fn remaining_recovery_workload(
        &self,
        expected_generation: HlsManifestAcceptanceGeneration,
    ) -> Option<HlsRecoveryWorkload> {
        if self.generation != expected_generation || self.outcome != HlsManifestAcceptanceEpisodeOutcome::Pending {
            return None;
        }
        let workload = self.workload_binding.workload();
        Some(if self.full_burst_completed { workload.after_full_burst() } else { workload })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn remaining_recovery_eta(
        &self,
        expected_generation: HlsManifestAcceptanceGeneration,
    ) -> Option<HlsRecoveryEtaMs> {
        self.remaining_recovery_workload(expected_generation).map(|workload| self.timing.remaining_eta(workload))
    }

    pub fn estimated_recovery_completion_at(
        &self,
        expected_generation: HlsManifestAcceptanceGeneration,
        now_ms: u64,
    ) -> Option<HlsEstimatedRecoveryCompletionAtMs> {
        self.remaining_recovery_workload(expected_generation)
            .map(|workload| self.timing.estimated_completion_at(now_ms, workload))
    }

    pub const fn exhaustion_reason(&self) -> Option<HlsManifestAcceptanceExhaustionReason> {
        match self.outcome {
            HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(reason) => Some(reason),
            HlsManifestAcceptanceEpisodeOutcome::Pending | HlsManifestAcceptanceEpisodeOutcome::Committed => None,
        }
    }

    pub fn burst_max_stagger_ms(&self) -> u64 {
        u64::try_from(self.burst_plan.slots.saturating_sub(1))
            .unwrap_or(u64::MAX)
            .saturating_mul(HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn record_full_burst(&mut self) { self.record_full_burst_candidates(self.required_candidates()); }

    pub fn record_full_burst_candidates(&mut self, completed_candidates: usize) {
        self.completed_burst_candidates = completed_candidates.min(self.required_candidates());
        if self.completed_burst_candidates != self.required_candidates() {
            return;
        }
        self.full_burst_completed = true;
        self.full_bursts_completed = self.full_bursts_completed.saturating_add(1);
        self.state = HlsManifestAcceptanceState::Evaluating;
    }

    pub fn complete(&mut self) {
        self.held_alternative = None;
        self.next_retry_at_ms = None;
        self.deterministic_conflict_receipt = None;
        self.state = HlsManifestAcceptanceState::Completed;
        self.outcome = HlsManifestAcceptanceEpisodeOutcome::Committed;
    }

    pub fn record_exhaustion(&mut self, reason: HlsManifestAcceptanceExhaustionReason) {
        if self.full_burst_completed && self.state != HlsManifestAcceptanceState::Completed {
            self.outcome = HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(reason);
        }
    }

    pub fn hold_after_uncommitted_burst(
        &mut self,
        mut cohort: Option<HlsAlternativeOriginCohort>,
        next_retry_at_ms: Option<u64>,
    ) {
        if self.full_burst_completed && self.state != HlsManifestAcceptanceState::Completed {
            if let Some(cohort) = cohort.as_mut() {
                let limit = u16::try_from(HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT).unwrap_or(u16::MAX);
                cohort.successful_samples = cohort.successful_samples.min(limit);
                cohort.total_samples = cohort.total_samples.min(limit);
            }
            self.held_alternative = cohort;
            self.next_retry_at_ms = next_retry_at_ms;
            self.state = HlsManifestAcceptanceState::Holding;
            self.workload_binding = HlsRecoveryWorkloadBinding::CandidateUnknown {
                envelope: self.workload_binding.envelope(),
                selected_candidate: None,
            };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestAcceptanceState {
    FullBurstPending,
    Collecting,
    Evaluating,
    Holding,
    StagingSwitchSegment,
    Committing,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestAcceptanceExhaustionReason {
    AllFailed,
    NoProgress,
    NoCommittableCandidate,
    DeterministicTimelineConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsManifestAcceptanceEpisodeOutcome {
    Pending,
    FullBurstExhausted(HlsManifestAcceptanceExhaustionReason),
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsManifestAcceptanceEpisodeStatus {
    Missing,
    Expired { generation: HlsManifestAcceptanceGeneration },
    InFlight { generation: HlsManifestAcceptanceGeneration },
    FullBurstExhausted { generation: HlsManifestAcceptanceGeneration, reason: HlsManifestAcceptanceExhaustionReason },
    Committed { generation: HlsManifestAcceptanceGeneration },
    Superseded { generation: HlsManifestAcceptanceGeneration, current_generation: HlsManifestAcceptanceGeneration },
}

/// Normalizes private episode internals into one generation-safe policy input.
/// Callers must not infer cutover state from `state` plus counters themselves.
pub fn manifest_acceptance_episode_status(
    episode: Option<&HlsManifestAcceptanceEpisode>,
    current_generation: HlsManifestAcceptanceGeneration,
    now_ms: u64,
) -> HlsManifestAcceptanceEpisodeStatus {
    let Some(episode) = episode else {
        return HlsManifestAcceptanceEpisodeStatus::Missing;
    };
    if episode.generation != current_generation {
        return HlsManifestAcceptanceEpisodeStatus::Superseded { generation: episode.generation, current_generation };
    }
    match episode.outcome {
        HlsManifestAcceptanceEpisodeOutcome::Pending
            if now_ms >= episode.timing.acceptance_deadline.as_millis_since_epoch() =>
        {
            HlsManifestAcceptanceEpisodeStatus::Expired { generation: episode.generation }
        }
        HlsManifestAcceptanceEpisodeOutcome::Pending => {
            HlsManifestAcceptanceEpisodeStatus::InFlight { generation: episode.generation }
        }
        HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(reason) => {
            HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { generation: episode.generation, reason }
        }
        HlsManifestAcceptanceEpisodeOutcome::Committed => {
            HlsManifestAcceptanceEpisodeStatus::Committed { generation: episode.generation }
        }
    }
}

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
    fn from_fingerprint(fingerprint: &HlsManifestTimelineFingerprint) -> Self {
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
    const fn can_be_staged(self) -> bool { matches!(self, Self::RequiresStaging) }
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
    const fn permits_acceptance(self) -> bool { matches!(self, Self::Eligible) }
}

impl HlsEmergencyAcceptanceEvidence {
    pub const INCOMPATIBLE: Self = Self {
        live_handoff: HlsEmergencyLiveHandoffCompatibility::Incompatible,
        terminal_alternative: HlsTerminalAlternativeCompatibility::TerminalTailPreferred,
    };

    const fn requires_staged_verification(self) -> bool {
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

pub fn evaluate_manifest_acceptance(input: HlsManifestAcceptanceInput<'_>) -> HlsManifestCommitPlan {
    let (progressed_pinned, unchanged_pinned) = best_pinned_candidates(input.observations);
    if let Some(pinned) = progressed_pinned {
        return pinned_commit(pinned);
    }

    // An unchanged pinned response is useful while reserve remains. Once the
    // recovery deadline is reached it must not hide a qualified alternative.
    if !input.trigger.recovery_required() {
        if let Some(pinned) = unchanged_pinned {
            return pinned_commit(pinned);
        }
    }

    if !input.full_burst_completed {
        return if has_stageable_alternative(input.observations) {
            HlsManifestCommitPlan::HoldAlternative
        } else {
            HlsManifestCommitPlan::RejectAll
        };
    }

    if let Some(initial) = input
        .observations
        .iter()
        .filter(|candidate| {
            candidate.host_relation == HlsCandidateHostRelation::InitialBaseline
                && candidate.resource_timeline_evidence.permits_acceptance()
        })
        .min_by_key(|candidate| (candidate.manifest_fetch_elapsed_ms, candidate.candidate_index))
    {
        return pinned_commit(initial);
    }

    let cohorts = alternative_cohorts_with_history(
        input.observations,
        input.previous_alternative,
        input.current_burst_is_full_plan,
    );
    if let Some(cohort) = cohorts
        .iter()
        .find(|cohort| matches!(cohort.evidence, HlsCrossHostAcceptanceEvidence::StrongTimelineAnchor { .. }))
    {
        return alternative_plan(
            input.observations,
            cohort.best_candidate_index,
            HlsManifestCommitKind::AnchoredAlternative,
        );
    }

    if let Some(candidate) = input
        .current_burst_is_full_plan
        .then(|| {
            input
                .observations
                .iter()
                .filter(|candidate| candidate.host_relation == HlsCandidateHostRelation::OtherHost)
                .filter(|candidate| candidate.resource_timeline_evidence.permits_acceptance())
                .filter(|candidate| {
                    candidate.committed_content_anchor
                        == HlsCommittedContentAnchorEvidence::RequiresStagedByteVerification
                })
                .min_by_key(|candidate| (candidate.manifest_fetch_elapsed_ms, candidate.candidate_index))
        })
        .flatten()
    {
        return alternative_plan(
            input.observations,
            candidate.candidate_index,
            HlsManifestCommitKind::ContentVerifiedAlternative,
        );
    }

    if input.trigger.recovery_required() {
        if let Some(cohort) = cohorts.iter().find(|cohort| {
            matches!(cohort.evidence, HlsCrossHostAcceptanceEvidence::BurstConsensusNewEpoch { .. })
                || cohort.consecutive_confirmed_full_bursts >= 2
        }) {
            return alternative_plan(
                input.observations,
                cohort.best_candidate_index,
                HlsManifestCommitKind::AlternativeAsNewEpoch,
            );
        }
    }

    if input.trigger == HlsManifestAcceptanceTrigger::Critical {
        if let Some(cohort) = critical_single_candidate_cohort(&cohorts, input.observations) {
            return alternative_plan(
                input.observations,
                cohort.best_candidate_index,
                HlsManifestCommitKind::EmergencyAlternativeAsNewEpoch,
            );
        }
    }

    if cohorts.is_empty() {
        HlsManifestCommitPlan::RejectAll
    } else {
        HlsManifestCommitPlan::HoldAlternative
    }
}

fn critical_single_candidate_cohort<'a>(
    cohorts: &'a [HlsAlternativeOriginCohort],
    observations: &[HlsManifestCandidateObservation],
) -> Option<&'a HlsAlternativeOriginCohort> {
    let [cohort] = cohorts else {
        return None;
    };
    if cohort.successful_samples != 1 || cohort.evidence != HlsCrossHostAcceptanceEvidence::Insufficient {
        return None;
    }
    observations.iter().find(|candidate| candidate.candidate_index == cohort.best_candidate_index).filter(
        |candidate| {
            candidate.switch_segment_readiness.can_be_staged()
                && candidate.emergency_evidence.requires_staged_verification()
        },
    )?;
    Some(cohort)
}

fn pinned_commit(candidate: &HlsManifestCandidateObservation) -> HlsManifestCommitPlan {
    HlsManifestCommitPlan::Commit { candidate_index: candidate.candidate_index, kind: HlsManifestCommitKind::Pinned }
}

fn alternative_plan(
    observations: &[HlsManifestCandidateObservation],
    candidate_index: usize,
    kind: HlsManifestCommitKind,
) -> HlsManifestCommitPlan {
    match observations
        .iter()
        .find(|candidate| candidate.candidate_index == candidate_index)
        .map(|candidate| candidate.switch_segment_readiness)
    {
        Some(HlsSwitchSegmentReadiness::RequiresStaging) => {
            HlsManifestCommitPlan::StageAlternative { candidate_index, kind }
        }
        Some(HlsSwitchSegmentReadiness::Unavailable) | None => HlsManifestCommitPlan::RejectAll,
    }
}

fn best_pinned_candidates(
    observations: &[HlsManifestCandidateObservation],
) -> (Option<&HlsManifestCandidateObservation>, Option<&HlsManifestCandidateObservation>) {
    let mut progressed = None;
    let mut unchanged = None;
    for candidate in observations.iter().filter(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::PinnedHost
            && candidate.resource_timeline_evidence.permits_acceptance()
    }) {
        match candidate.local_sequence_relation {
            Some(
                HlsHostLocalSequenceRelation::Next
                | HlsHostLocalSequenceRelation::PlausibleForward
                | HlsHostLocalSequenceRelation::RolloverCandidate
                | HlsHostLocalSequenceRelation::Rebase,
            ) => {
                if progressed.is_none_or(|current| pinned_order(candidate, current).is_lt()) {
                    progressed = Some(candidate);
                }
            }
            Some(HlsHostLocalSequenceRelation::Same | HlsHostLocalSequenceRelation::NoBaseline) => {
                if unchanged.is_none_or(|current| pinned_order(candidate, current).is_lt()) {
                    unchanged = Some(candidate);
                }
            }
            Some(HlsHostLocalSequenceRelation::Backward) | None => {}
        }
    }
    (progressed, unchanged)
}

fn pinned_order(left: &HlsManifestCandidateObservation, right: &HlsManifestCandidateObservation) -> std::cmp::Ordering {
    pinned_priority(left.local_sequence_relation)
        .cmp(&pinned_priority(right.local_sequence_relation))
        .then_with(|| left.manifest_fetch_elapsed_ms.cmp(&right.manifest_fetch_elapsed_ms))
        .then_with(|| left.candidate_index.cmp(&right.candidate_index))
}

fn pinned_priority(relation: Option<HlsHostLocalSequenceRelation>) -> u8 {
    match relation {
        Some(HlsHostLocalSequenceRelation::Next) => 0,
        Some(HlsHostLocalSequenceRelation::PlausibleForward | HlsHostLocalSequenceRelation::Rebase) => 1,
        Some(HlsHostLocalSequenceRelation::RolloverCandidate) => 2,
        Some(HlsHostLocalSequenceRelation::Same | HlsHostLocalSequenceRelation::NoBaseline) => 3,
        Some(HlsHostLocalSequenceRelation::Backward) | None => u8::MAX,
    }
}

fn has_stageable_alternative(observations: &[HlsManifestCandidateObservation]) -> bool {
    observations.iter().any(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::OtherHost
            && candidate.resource_timeline_evidence.permits_acceptance()
            && candidate.switch_segment_readiness.can_be_staged()
    })
}

pub fn classify_host_local_sequence(
    previous_highwater: Option<u64>,
    candidate_highwater: Option<u64>,
    forward_window: u64,
    rebase_allowed: bool,
) -> Option<HlsHostLocalSequenceRelation> {
    let candidate = candidate_highwater?;
    if rebase_allowed {
        return Some(HlsHostLocalSequenceRelation::Rebase);
    }
    let Some(previous) = previous_highwater else {
        return Some(HlsHostLocalSequenceRelation::NoBaseline);
    };
    if candidate == previous {
        return Some(HlsHostLocalSequenceRelation::Same);
    }
    if previous.checked_add(1) == Some(candidate) {
        return Some(HlsHostLocalSequenceRelation::Next);
    }
    if candidate > previous {
        return Some(if candidate.saturating_sub(previous) <= forward_window.max(1) {
            HlsHostLocalSequenceRelation::PlausibleForward
        } else {
            HlsHostLocalSequenceRelation::Backward
        });
    }
    Some(if candidate <= forward_window.max(1) {
        HlsHostLocalSequenceRelation::RolloverCandidate
    } else {
        HlsHostLocalSequenceRelation::Backward
    })
}

pub fn alternative_cohorts(observations: &[HlsManifestCandidateObservation]) -> Vec<HlsAlternativeOriginCohort> {
    let pinned = observations
        .iter()
        .filter(|candidate| {
            candidate.host_relation == HlsCandidateHostRelation::PinnedHost
                && candidate.resource_timeline_evidence.permits_acceptance()
        })
        .collect::<Vec<_>>();
    let mut alternatives = observations
        .iter()
        .filter(|candidate| {
            candidate.host_relation == HlsCandidateHostRelation::OtherHost
                && candidate.resource_timeline_evidence.permits_acceptance()
                && candidate.switch_segment_readiness.can_be_staged()
                && candidate.effective_host.is_some()
        })
        .collect::<Vec<_>>();
    alternatives.sort_by(|left, right| {
        left.effective_host.cmp(&right.effective_host).then_with(|| left.candidate_index.cmp(&right.candidate_index))
    });
    let mut groups = Vec::<Vec<&HlsManifestCandidateObservation>>::new();
    for observation in alternatives {
        if let Some(group) = groups.iter_mut().find(|group| {
            group.iter().all(|sample| {
                sample.effective_host == observation.effective_host
                    && same_host_timeline_compatible(sample, observation)
            })
        }) {
            if group.len() < HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT {
                group.push(observation);
            }
        } else if groups.len() < HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT {
            groups.push(vec![observation]);
        }
    }

    let mut cohorts =
        groups.into_iter().filter_map(|samples| build_alternative_cohort(&samples, &pinned)).collect::<Vec<_>>();
    cohorts.sort_by_key(|cohort| {
        (evidence_priority(cohort.evidence), std::cmp::Reverse(cohort.successful_samples), cohort.best_candidate_index)
    });
    cohorts
}

pub fn manifest_acceptance_landscape(
    observations: &[HlsManifestCandidateObservation],
) -> HlsManifestAcceptanceLandscape {
    let pinned_state = pinned_observation_state(observations);
    let alternatives = alternative_cohorts(observations)
        .into_iter()
        .take(HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT)
        .map(|cohort| (cohort.identity, cohort.window))
        .collect();
    HlsManifestAcceptanceLandscape { pinned_state, alternatives }
}

pub fn classify_reduced_retry_landscape(
    previous: &HlsManifestAcceptanceLandscape,
    observations: &[HlsManifestCandidateObservation],
) -> HlsReducedRetryLandscapeChange {
    if observations.is_empty() {
        return HlsReducedRetryLandscapeChange::Unchanged;
    }
    let current_pinned = pinned_observation_state(observations);
    if current_pinned != HlsPinnedOriginObservationState::Missing && current_pinned != previous.pinned_state {
        return HlsReducedRetryLandscapeChange::PinnedStateChanged;
    }
    if observations.iter().any(|candidate| candidate.host_relation == HlsCandidateHostRelation::Unknown) {
        return HlsReducedRetryLandscapeChange::TimelineConflict;
    }
    for cohort in alternative_cohorts(observations) {
        let Some((_, previous_window)) =
            previous.alternatives.iter().find(|(identity, _)| identity == &cohort.identity)
        else {
            return HlsReducedRetryLandscapeChange::NewCohort;
        };
        if !host_local_windows_compatible(previous_window, &cohort.window) {
            return HlsReducedRetryLandscapeChange::TimelineConflict;
        }
    }
    HlsReducedRetryLandscapeChange::Unchanged
}

fn pinned_observation_state(observations: &[HlsManifestCandidateObservation]) -> HlsPinnedOriginObservationState {
    if observations.iter().any(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::PinnedHost
            && !candidate.resource_timeline_evidence.permits_acceptance()
    }) {
        return HlsPinnedOriginObservationState::Rejected;
    }
    let relations = observations
        .iter()
        .filter(|candidate| candidate.host_relation == HlsCandidateHostRelation::PinnedHost)
        .filter_map(|candidate| candidate.local_sequence_relation);
    let mut state = HlsPinnedOriginObservationState::Missing;
    for relation in relations {
        match relation {
            HlsHostLocalSequenceRelation::Next
            | HlsHostLocalSequenceRelation::PlausibleForward
            | HlsHostLocalSequenceRelation::RolloverCandidate
            | HlsHostLocalSequenceRelation::Rebase => return HlsPinnedOriginObservationState::Progressed,
            HlsHostLocalSequenceRelation::Same | HlsHostLocalSequenceRelation::NoBaseline => {
                state = HlsPinnedOriginObservationState::Unchanged;
            }
            HlsHostLocalSequenceRelation::Backward => {
                if state == HlsPinnedOriginObservationState::Missing {
                    state = HlsPinnedOriginObservationState::Rejected;
                }
            }
        }
    }
    state
}

pub fn alternative_cohorts_with_history(
    observations: &[HlsManifestCandidateObservation],
    previous: Option<&HlsAlternativeOriginCohort>,
    current_burst_is_full_plan: bool,
) -> Vec<HlsAlternativeOriginCohort> {
    let mut cohorts = alternative_cohorts(observations);
    if current_burst_is_full_plan {
        for cohort in &mut cohorts {
            merge_full_burst_cohort_history(cohort, previous);
        }
        return cohorts;
    }
    // A reduced follow-up may recover the pinned host, but it cannot spend
    // historical full-burst evidence on a new cross-host staging attempt.
    Vec::new()
}

pub fn held_alternative_after_burst(
    observations: &[HlsManifestCandidateObservation],
    previous: Option<&HlsAlternativeOriginCohort>,
    current_burst_is_full_plan: bool,
) -> Option<HlsAlternativeOriginCohort> {
    if current_burst_is_full_plan {
        return alternative_cohorts_with_history(observations, previous, true).into_iter().next();
    }
    // Cheap failures and contradictions do not erase the last completed
    // configured burst. A later episode must run another full plan before the
    // history can become consecutive acceptance evidence.
    let previous = previous?;
    let mut matching =
        alternative_cohorts(observations).into_iter().find(|cohort| same_alternative_cohort(cohort, previous));
    if let Some(cohort) = matching.as_mut() {
        cohort.successful_samples = cohort.successful_samples.max(previous.successful_samples);
        cohort.total_samples = cohort
            .total_samples
            .saturating_add(previous.total_samples)
            .min(u16::try_from(HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT).unwrap_or(u16::MAX));
        cohort.consecutive_confirmed_full_bursts = previous.consecutive_confirmed_full_bursts;
        if evidence_priority(previous.evidence) < evidence_priority(cohort.evidence) {
            cohort.evidence = previous.evidence;
        }
    }
    matching.or_else(|| Some(previous.clone()))
}

fn merge_full_burst_cohort_history(
    current: &mut HlsAlternativeOriginCohort,
    previous: Option<&HlsAlternativeOriginCohort>,
) {
    current.consecutive_confirmed_full_bursts = 1;
    let Some(previous) = previous.filter(|previous| same_alternative_cohort(current, previous)) else {
        return;
    };
    current.total_samples = current
        .total_samples
        .saturating_add(previous.total_samples)
        .min(u16::try_from(HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT).unwrap_or(u16::MAX));
    current.consecutive_confirmed_full_bursts = previous.consecutive_confirmed_full_bursts.max(1).saturating_add(1);
    if evidence_priority(previous.evidence) < evidence_priority(current.evidence) {
        current.evidence = previous.evidence;
    }
}

fn same_alternative_cohort(left: &HlsAlternativeOriginCohort, right: &HlsAlternativeOriginCohort) -> bool {
    left.identity == right.identity && host_local_windows_compatible(&left.window, &right.window)
}

fn build_alternative_cohort(
    samples: &[&HlsManifestCandidateObservation],
    pinned: &[&HlsManifestCandidateObservation],
) -> Option<HlsAlternativeOriginCohort> {
    let first = samples.first()?;
    let successful_samples = u16::try_from(samples.len()).unwrap_or(u16::MAX);
    let anchored_overlap = samples
        .iter()
        .flat_map(|sample| pinned.iter().map(move |baseline| strong_anchor_overlap(sample, baseline)))
        .max()
        .unwrap_or_default();
    let externally_anchored_overlap = samples
        .iter()
        .filter_map(|sample| match sample.evidence {
            HlsCrossHostAcceptanceEvidence::StrongTimelineAnchor { overlapping_segments } => Some(overlapping_segments),
            HlsCrossHostAcceptanceEvidence::Insufficient
            | HlsCrossHostAcceptanceEvidence::BurstConsensusNewEpoch { .. } => None,
        })
        .max()
        .unwrap_or_default();
    let overlapping_segments = anchored_overlap.max(externally_anchored_overlap);
    let evidence = if overlapping_segments > 0 {
        HlsCrossHostAcceptanceEvidence::StrongTimelineAnchor { overlapping_segments }
    } else if successful_samples >= 2 {
        HlsCrossHostAcceptanceEvidence::BurstConsensusNewEpoch { successful_samples }
    } else {
        HlsCrossHostAcceptanceEvidence::Insufficient
    };
    let best = samples.iter().min_by_key(|sample| (sample.manifest_fetch_elapsed_ms, sample.candidate_index))?;
    let best_candidate_index = best.candidate_index;
    let effective_host = first.effective_host.clone()?;
    Some(HlsAlternativeOriginCohort {
        identity: HlsAlternativeOriginCohortIdentity {
            effective_host,
            technical_signature: HlsManifestTechnicalSignature::from_fingerprint(&first.timeline_fingerprint),
        },
        window: HlsAlternativeOriginWindow {
            host_local_media_sequence: best.host_local_media_sequence,
            host_local_highwater: best.host_local_highwater,
            fingerprint: best.timeline_fingerprint.clone(),
        },
        successful_samples,
        total_samples: successful_samples,
        consecutive_confirmed_full_bursts: 0,
        evidence,
        best_candidate_index,
    })
}

fn evidence_priority(evidence: HlsCrossHostAcceptanceEvidence) -> u8 {
    match evidence {
        HlsCrossHostAcceptanceEvidence::StrongTimelineAnchor { .. } => 0,
        HlsCrossHostAcceptanceEvidence::BurstConsensusNewEpoch { .. } => 1,
        HlsCrossHostAcceptanceEvidence::Insufficient => 2,
    }
}

fn same_host_timeline_compatible(
    left: &HlsManifestCandidateObservation,
    right: &HlsManifestCandidateObservation,
) -> bool {
    let left_window = HlsAlternativeOriginWindow {
        host_local_media_sequence: left.host_local_media_sequence,
        host_local_highwater: left.host_local_highwater,
        fingerprint: left.timeline_fingerprint.clone(),
    };
    let right_window = HlsAlternativeOriginWindow {
        host_local_media_sequence: right.host_local_media_sequence,
        host_local_highwater: right.host_local_highwater,
        fingerprint: right.timeline_fingerprint.clone(),
    };
    host_local_windows_compatible(&left_window, &right_window)
}

fn host_local_windows_compatible(left: &HlsAlternativeOriginWindow, right: &HlsAlternativeOriginWindow) -> bool {
    let left_fingerprint = &left.fingerprint;
    let right_fingerprint = &right.fingerprint;
    if HlsManifestTechnicalSignature::from_fingerprint(left_fingerprint)
        != HlsManifestTechnicalSignature::from_fingerprint(right_fingerprint)
        || left_fingerprint.segment_samples.is_empty()
        || right_fingerprint.segment_samples.is_empty()
    {
        return false;
    }

    let Some(left_highwater) = left.host_local_highwater else {
        return false;
    };
    let Some(right_highwater) = right.host_local_highwater else {
        return false;
    };
    let overlap_start = left.host_local_media_sequence.max(right.host_local_media_sequence);
    let overlap_end = left_highwater.min(right_highwater);
    if overlap_start <= overlap_end {
        return (overlap_start..=overlap_end).all(|sequence| {
            segment_in_window(left, sequence)
                .zip(segment_in_window(right, sequence))
                .is_some_and(|(left, right)| segment_shape_matches(left, right))
        });
    }

    // Adjacent local windows are monotonic only when their non-resource shape
    // and technical signatures agree. Origin sequence is used solely inside
    // this already host-local cohort.
    let gap = if left_highwater < right.host_local_media_sequence {
        right.host_local_media_sequence.saturating_sub(left_highwater)
    } else {
        left.host_local_media_sequence.saturating_sub(right_highwater)
    };
    gap <= 1
        && left_fingerprint.segment_count == right_fingerprint.segment_count
        && left_fingerprint.duration_pattern_hash == right_fingerprint.duration_pattern_hash
        && left_fingerprint.discontinuity_pattern_hash == right_fingerprint.discontinuity_pattern_hash
}

fn segment_in_window(window: &HlsAlternativeOriginWindow, sequence: u64) -> Option<&HlsManifestSegmentFingerprint> {
    let offset = sequence.checked_sub(window.host_local_media_sequence)?;
    window.fingerprint.segment_samples.get(usize::try_from(offset).ok()?)
}

fn strong_anchor_overlap(
    alternative: &HlsManifestCandidateObservation,
    pinned: &HlsManifestCandidateObservation,
) -> u16 {
    if alternative.timeline_fingerprint.map_and_encryption_hash != pinned.timeline_fingerprint.map_and_encryption_hash
        || alternative.timeline_fingerprint.container_signature_hash
            != pinned.timeline_fingerprint.container_signature_hash
    {
        return 0;
    }
    let mut longest_pdt_run = 0_usize;
    let alternative_segments = &alternative.timeline_fingerprint.segment_samples;
    let pinned_segments = &pinned.timeline_fingerprint.segment_samples;
    for (alternative_index, alternative_segment) in alternative_segments.iter().enumerate() {
        for (pinned_index, pinned_segment) in pinned_segments.iter().enumerate() {
            if !pdt_intervals_overlap(alternative_segment, pinned_segment) {
                continue;
            }
            let mut run = 0_usize;
            while let (Some(alternative_sample), Some(pinned_sample)) = (
                alternative_segments.get(alternative_index.saturating_add(run)),
                pinned_segments.get(pinned_index.saturating_add(run)),
            ) {
                if alternative_sample.duration_ms != pinned_sample.duration_ms
                    || alternative_sample.discontinuity_before != pinned_sample.discontinuity_before
                    || !pdt_intervals_overlap(alternative_sample, pinned_sample)
                {
                    break;
                }
                run = run.saturating_add(1);
            }
            longest_pdt_run = longest_pdt_run.max(run);
        }
    }
    if longest_pdt_run >= 2 {
        u16::try_from(longest_pdt_run).unwrap_or(u16::MAX)
    } else {
        0
    }
}

fn pdt_intervals_overlap(left: &HlsManifestSegmentFingerprint, right: &HlsManifestSegmentFingerprint) -> bool {
    if left.duration_ms != right.duration_ms {
        return false;
    }
    let (Some(left_start), Some(right_start)) = (left.program_date_time_ms, right.program_date_time_ms) else {
        return false;
    };
    let left_duration = i64::try_from(left.duration_ms).unwrap_or(i64::MAX);
    let right_duration = i64::try_from(right.duration_ms).unwrap_or(i64::MAX);
    let left_end = left_start.saturating_add(left_duration);
    let right_end = right_start.saturating_add(right_duration);
    left_start < right_end && right_start < left_end
}

fn segment_shape_matches(left: &HlsManifestSegmentFingerprint, right: &HlsManifestSegmentFingerprint) -> bool {
    left.duration_ms == right.duration_ms
        && left.discontinuity_before == right.discontinuity_before
        && match (left.normalized_resource_identity, right.normalized_resource_identity) {
            (Some(left), Some(right)) => left.matches(right),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_timing::{
        HlsAcceptanceEpisodeTimingInput, HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsRecoveryBurstWorkload,
        HlsRecoveryMapWorkload, HlsRecoverySegmentWorkload, HlsRecoveryTimingPolicy, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    };

    fn episode_timing(started_at_ms: u64, burst_plan: HlsManifestRecoveryBurstPlan) -> HlsAcceptanceEpisodeTiming {
        HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms,
            burst_plan,
            target_duration_ms: 4_000,
            transition_margin: HlsTransitionMarginMs::from_millis(1_000),
            workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
            observed_latency: HlsObservedRecoveryLatency::default(),
            required_terminal_media_key: None,
            terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
            policy: HlsRecoveryTimingPolicy::new(
                HlsOperationTimeoutMs::from_millis(1_000),
                HlsOperationTimeoutMs::from_millis(2_000),
                HlsRecoveryEtaMs::from_millis(300),
                HlsRecoveryEtaMs::from_millis(400),
            ),
        })
    }

    fn segment(marker: u8, pdt_ms: Option<i64>) -> HlsManifestSegmentFingerprint {
        HlsManifestSegmentFingerprint {
            duration_ms: 4_000,
            discontinuity_before: false,
            program_date_time_ms: pdt_ms,
            normalized_resource_identity: Some(HlsMediaResourceIdentity::for_test(marker)),
        }
    }

    fn fingerprint(marker: u8) -> HlsManifestTimelineFingerprint {
        HlsManifestTimelineFingerprint {
            segment_count: 3,
            first_program_date_time_ms: None,
            last_program_date_time_ms: None,
            duration_pattern_hash: [marker; 32],
            discontinuity_pattern_hash: [0; 32],
            normalized_resource_pattern_hash: Some([marker; 32]),
            map_and_encryption_hash: [0; 32],
            container_signature_hash: [1; 32],
            segment_samples: vec![
                segment(marker, None),
                segment(marker.saturating_add(1), None),
                segment(marker.saturating_add(2), None),
            ],
        }
    }

    fn observation(index: usize, host: &str, relation: HlsCandidateHostRelation) -> HlsManifestCandidateObservation {
        HlsManifestCandidateObservation {
            candidate_index: index,
            candidate_slot: index / 2,
            effective_host: Some(host.to_string()),
            host_relation: relation,
            host_local_media_sequence: 5,
            host_local_highwater: Some(7),
            local_sequence_relation: (relation == HlsCandidateHostRelation::PinnedHost)
                .then_some(HlsHostLocalSequenceRelation::Next),
            resource_timeline_evidence: HlsResourceTimelineEvidence::Eligible,
            timeline_fingerprint: fingerprint(1),
            manifest_fetch_elapsed_ms: 10,
            switch_segment_readiness: HlsSwitchSegmentReadiness::RequiresStaging,
            committed_content_anchor: HlsCommittedContentAnchorEvidence::Unavailable,
            emergency_evidence: HlsEmergencyAcceptanceEvidence::INCOMPATIBLE,
            evidence: HlsCrossHostAcceptanceEvidence::Insufficient,
        }
    }

    fn evaluate(
        observations: &[HlsManifestCandidateObservation],
        trigger: HlsManifestAcceptanceTrigger,
    ) -> HlsManifestCommitPlan {
        evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
            full_burst_completed: true,
            current_burst_is_full_plan: true,
            trigger,
            previous_alternative: None,
            observations,
        })
    }

    #[test]
    fn forward_media_sequence_does_not_override_published_resource_replay() {
        let mut replay = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
        replay.local_sequence_relation = Some(HlsHostLocalSequenceRelation::PlausibleForward);
        replay.resource_timeline_evidence = HlsResourceTimelineEvidence::ReplayOnly;

        assert_eq!(
            evaluate(&[replay], HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::RejectAll
        );
    }

    fn mark_emergency_verification_eligible(candidate: &mut HlsManifestCandidateObservation) {
        candidate.emergency_evidence = HlsEmergencyAcceptanceEvidence {
            live_handoff: HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification,
            terminal_alternative: HlsTerminalAlternativeCompatibility::RequiresStagedComparison,
        };
    }

    fn sliding_observation(
        index: usize,
        host: &str,
        media_sequence: u64,
        markers: [u8; 3],
    ) -> HlsManifestCandidateObservation {
        let mut candidate = observation(index, host, HlsCandidateHostRelation::OtherHost);
        candidate.host_local_media_sequence = media_sequence;
        candidate.host_local_highwater = Some(media_sequence.saturating_add(2));
        candidate.timeline_fingerprint.segment_samples = markers.map(|marker| segment(marker, None)).to_vec();
        candidate
    }

    #[test]
    fn first_episode_uses_entire_configured_beast_plan() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(1),
            0,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &episode_timing(0, plan),
        );

        assert_eq!(episode.required_candidates(), plan.total_candidates());
        assert_eq!(episode.burst_max_stagger_ms(), 500);
        assert!(!episode.full_burst_completed);
    }

    #[test]
    fn acceptance_trigger_semantics_are_explicit() {
        assert!(!HlsManifestAcceptanceTrigger::None.starts_episode());
        assert!(HlsManifestAcceptanceTrigger::Observe.starts_episode());
        assert!(!HlsManifestAcceptanceTrigger::Observe.recovery_required());
        assert!(HlsManifestAcceptanceTrigger::RecoveryRequired.recovery_required());
        assert!(HlsManifestAcceptanceTrigger::Critical.recovery_required());
    }

    #[test]
    fn held_episode_state_is_bounded_and_cleared_on_completion() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let mut episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(1),
            0,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &episode_timing(0, plan),
        );
        let mut cohort = alternative_cohorts(&[
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ])
        .remove(0);
        cohort.successful_samples = u16::MAX;
        cohort.total_samples = u16::MAX;

        episode.record_full_burst();
        episode.hold_after_uncommitted_burst(Some(cohort), Some(123));
        assert_eq!(episode.held_alternative.as_ref().map(|held| held.successful_samples), Some(32));
        assert_eq!(episode.next_retry_at_ms, Some(123));
        episode.complete();
        assert_eq!(episode.held_alternative, None);
        assert_eq!(episode.next_retry_at_ms, None);
    }

    #[test]
    fn completing_episode_clears_deterministic_conflict_receipt() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let mut episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(1),
            0,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &episode_timing(0, plan),
        );
        let resource_key = HlsMediaResourceIdentity::for_test(7).semantic_key();
        episode.record_deterministic_conflict(HlsDeterministicConflictReceipt {
            conflict: HlsDeterministicTimelineConflict {
                previous_proxy_tail: Some(2),
                existing_proxy_seq: 0,
                candidate_position: 1,
                candidate_origin_seq: 491,
                resource_key,
                decision: super::super::timeline::HlsResourceReplayDecision::RejectContradictoryOrder,
                candidate_fingerprint: super::super::deterministic_conflict::HlsDeterministicConflictFingerprint {
                    segment_count: 1,
                    first_program_date_time_ms: None,
                    last_program_date_time_ms: None,
                    duration_pattern_hash: [1; 32],
                    discontinuity_pattern_hash: [2; 32],
                    semantic_resource_pattern_hash: Some(resource_key.bytes()),
                    map_and_encryption_hash: [3; 32],
                    container_signature_hash: [4; 32],
                    segment_samples: Vec::new(),
                },
            },
            origin_progress_generation: 1,
            published_resource_history_generation: 1,
            pinned_host_generation: 1,
        });

        assert!(episode.deterministic_conflict_receipt().is_some());
        episode.complete();
        assert!(episode.deterministic_conflict_receipt().is_none());
    }

    #[test]
    fn uncommitted_completed_burst_returns_episode_to_holding() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let mut episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(1),
            0,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &episode_timing(0, plan),
        );

        episode.record_full_burst();
        episode.state = HlsManifestAcceptanceState::StagingSwitchSegment;
        episode.hold_after_uncommitted_burst(None, Some(123));

        assert_eq!(episode.state, HlsManifestAcceptanceState::Holding);
        assert!(episode.full_burst_completed);
        assert_eq!(episode.full_bursts_completed, 1);
        assert_eq!(episode.next_retry_at_ms, Some(123));
    }

    #[test]
    fn hls_recovery_timing_status_uses_frozen_episode_deadline_and_normalizes_outcomes() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Friendly.plan();
        let generation = HlsManifestAcceptanceGeneration(9);
        let timing = episode_timing(100, plan);
        let deadline_ms = timing.acceptance_deadline.as_millis_since_epoch();
        let mut episode = HlsManifestAcceptanceEpisode::new(
            generation,
            100,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &timing,
        );
        assert_eq!(
            manifest_acceptance_episode_status(Some(&episode), generation, deadline_ms.saturating_sub(1)),
            HlsManifestAcceptanceEpisodeStatus::InFlight { generation }
        );
        episode.record_full_burst();
        episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate);
        assert_eq!(
            manifest_acceptance_episode_status(Some(&episode), generation, deadline_ms.saturating_sub(1)),
            HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted {
                generation,
                reason: HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
            }
        );
        assert_eq!(
            manifest_acceptance_episode_status(
                Some(&episode),
                HlsManifestAcceptanceGeneration(10),
                deadline_ms.saturating_sub(1),
            ),
            HlsManifestAcceptanceEpisodeStatus::Superseded {
                generation,
                current_generation: HlsManifestAcceptanceGeneration(10),
            }
        );
        assert_eq!(
            manifest_acceptance_episode_status(Some(&episode), generation, deadline_ms),
            HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted {
                generation,
                reason: HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
            }
        );

        let pending_episode = HlsManifestAcceptanceEpisode::new(
            generation,
            100,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &timing,
        );
        assert_eq!(
            manifest_acceptance_episode_status(Some(&pending_episode), generation, deadline_ms),
            HlsManifestAcceptanceEpisodeStatus::Expired { generation }
        );
    }

    #[test]
    fn hls_recovery_timing_episode_keeps_construction_snapshot_immutable() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let frozen_timing = episode_timing(100, plan);
        let episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(3),
            100,
            plan,
            HlsManifestAcceptanceTrigger::Critical,
            &frozen_timing,
        );
        let later_timing = episode_timing(10_000, plan);

        assert_ne!(later_timing.acceptance_deadline, frozen_timing.acceptance_deadline);
        assert_eq!(episode.timing(), frozen_timing);
        assert_eq!(episode.generation, HlsManifestAcceptanceGeneration(3));
        assert_eq!(episode.started_at_ms, 100);
        assert_eq!(episode.burst_plan, plan);
        assert_eq!(episode.trigger(), HlsManifestAcceptanceTrigger::Critical);
    }

    #[test]
    fn hls_recovery_timing_candidate_binding_and_staging_reduce_eta_without_moving_deadline() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let generation = HlsManifestAcceptanceGeneration(4);
        let mut episode = HlsManifestAcceptanceEpisode::new(
            generation,
            100,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &episode_timing(100, plan),
        );
        let frozen_deadline = episode.timing().acceptance_deadline;
        let initial_eta = episode.remaining_recovery_eta(generation).map(HlsRecoveryEtaMs::as_millis);

        episode.record_full_burst();
        let completed_burst_eta = episode.remaining_recovery_eta(generation).map(HlsRecoveryEtaMs::as_millis);
        episode.state = HlsManifestAcceptanceState::StagingSwitchSegment;
        let identity = HlsManifestRecoveryCandidateIdentity::from_candidate(
            2,
            Some("candidate.example.com"),
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n",
        );
        let bound_workload = HlsRecoveryWorkload {
            burst: HlsRecoveryBurstWorkload::FullBurstPending,
            segment: HlsRecoverySegmentWorkload::ClearSegmentFetch,
            map: HlsRecoveryMapWorkload::Fetch,
        };
        assert_eq!(episode.select_candidate(generation, identity), HlsRecoveryWorkloadBindingUpdate::Applied);
        assert_eq!(
            episode.bind_selected_candidate(generation, identity, bound_workload),
            HlsRecoveryWorkloadBindingUpdate::Applied
        );
        assert_eq!(
            episode.remaining_recovery_workload(generation).map(|workload| workload.map),
            Some(HlsRecoveryMapWorkload::Fetch)
        );
        let bound_eta = episode.remaining_recovery_eta(generation).map(HlsRecoveryEtaMs::as_millis);
        episode.state = HlsManifestAcceptanceState::Committing;
        let committing_eta = episode.remaining_recovery_eta(generation).map(HlsRecoveryEtaMs::as_millis);
        episode.state = HlsManifestAcceptanceState::StagingSwitchSegment;
        assert_eq!(
            episode.advance_bound_candidate(
                generation,
                identity,
                HlsRecoveryWorkload {
                    burst: HlsRecoveryBurstWorkload::FullBurstCompleted,
                    segment: HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady,
                    map: HlsRecoveryMapWorkload::Ready,
                },
            ),
            HlsRecoveryWorkloadBindingUpdate::Applied
        );
        let staged_eta = episode.remaining_recovery_eta(generation).map(HlsRecoveryEtaMs::as_millis);

        assert!(initial_eta > completed_burst_eta);
        assert!(completed_burst_eta > bound_eta);
        assert_eq!(committing_eta, bound_eta);
        assert!(bound_eta > staged_eta);
        assert_eq!(episode.timing().acceptance_deadline, frozen_deadline);
        assert_eq!(
            episode
                .estimated_recovery_completion_at(generation, 5_000)
                .map(HlsEstimatedRecoveryCompletionAtMs::as_millis_since_epoch),
            staged_eta.map(|eta| 5_000_u64.saturating_add(eta))
        );
        assert_eq!(episode.remaining_recovery_workload(HlsManifestAcceptanceGeneration(5)), None);

        episode.complete();
        assert_eq!(episode.remaining_recovery_workload(generation), None);
        assert_eq!(episode.estimated_recovery_completion_at(generation, 5_000), None);
    }

    #[test]
    fn hls_recovery_timing_candidate_stale_generation_and_wrong_identity_cannot_update_binding() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let generation = HlsManifestAcceptanceGeneration(9);
        let mut episode = HlsManifestAcceptanceEpisode::new(
            generation,
            100,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &episode_timing(100, plan),
        );
        episode.record_full_burst();
        episode.state = HlsManifestAcceptanceState::StagingSwitchSegment;
        let selected = HlsManifestRecoveryCandidateIdentity::from_candidate(1, Some("a.example"), "body");
        let other = HlsManifestRecoveryCandidateIdentity::from_candidate(2, Some("a.example"), "body");
        let candidate_workload = HlsRecoveryWorkload {
            burst: HlsRecoveryBurstWorkload::FullBurstPending,
            segment: HlsRecoverySegmentWorkload::ClearSegmentFetch,
            map: HlsRecoveryMapWorkload::Fetch,
        };

        assert_eq!(
            episode.select_candidate(HlsManifestAcceptanceGeneration(8), selected),
            HlsRecoveryWorkloadBindingUpdate::StaleGeneration
        );
        assert_eq!(episode.select_candidate(generation, selected), HlsRecoveryWorkloadBindingUpdate::Applied);
        assert_eq!(
            episode.bind_selected_candidate(generation, other, candidate_workload),
            HlsRecoveryWorkloadBindingUpdate::CandidateMismatch
        );
        assert_eq!(
            episode.bind_selected_candidate(generation, selected, candidate_workload),
            HlsRecoveryWorkloadBindingUpdate::Applied
        );
        let before = episode.remaining_recovery_workload(generation);
        assert_eq!(
            episode.advance_bound_candidate(
                generation,
                other,
                HlsRecoveryWorkload {
                    burst: HlsRecoveryBurstWorkload::FullBurstCompleted,
                    segment: HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady,
                    map: HlsRecoveryMapWorkload::Ready,
                },
            ),
            HlsRecoveryWorkloadBindingUpdate::CandidateMismatch
        );
        assert_eq!(
            episode.advance_bound_candidate(
                HlsManifestAcceptanceGeneration(8),
                selected,
                HlsRecoveryWorkload {
                    burst: HlsRecoveryBurstWorkload::FullBurstCompleted,
                    segment: HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady,
                    map: HlsRecoveryMapWorkload::Ready,
                },
            ),
            HlsRecoveryWorkloadBindingUpdate::StaleGeneration
        );
        assert_eq!(episode.remaining_recovery_workload(generation), before);
    }

    #[test]
    fn hls_recovery_timing_candidate_same_host_selection_keeps_conservative_unknown_workload() {
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
        let generation = HlsManifestAcceptanceGeneration(10);
        let mut episode = HlsManifestAcceptanceEpisode::new(
            generation,
            100,
            plan,
            HlsManifestAcceptanceTrigger::RecoveryRequired,
            &episode_timing(100, plan),
        );
        episode.record_full_burst();
        episode.state = HlsManifestAcceptanceState::Committing;
        let selected = HlsManifestRecoveryCandidateIdentity::from_candidate(
            1,
            Some("pinned.example"),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:8\n",
        );
        let conservative = episode.remaining_recovery_workload(generation);

        assert_eq!(episode.select_candidate(generation, selected), HlsRecoveryWorkloadBindingUpdate::Applied);

        assert_eq!(episode.selected_candidate_identity(), Some(selected));
        assert_eq!(episode.remaining_recovery_workload(generation), conservative);
    }

    #[test]
    fn hls_recovery_timing_candidate_identity_is_host_local_even_for_identical_manifest_bytes() {
        let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n";

        let origin_a = HlsManifestRecoveryCandidateIdentity::from_candidate(0, Some("a.example"), body);
        let origin_b = HlsManifestRecoveryCandidateIdentity::from_candidate(0, Some("b.example"), body);

        assert_ne!(origin_a, origin_b);
    }

    #[test]
    fn identical_media_sequence_on_other_host_has_no_local_continuity_relation() {
        let candidate = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);

        assert_eq!(candidate.host_local_media_sequence, 5);
        assert_eq!(candidate.local_sequence_relation, None);
    }

    #[test]
    fn progressed_pinned_candidate_wins_over_alternative() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-a", HlsCandidateHostRelation::PinnedHost),
        ];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::Critical),
            HlsManifestCommitPlan::Commit { candidate_index: 1, kind: HlsManifestCommitKind::Pinned }
        );
    }

    #[test]
    fn unchanged_pinned_does_not_hide_consensus_under_recovery_pressure() {
        let alternatives = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];
        let mut pinned = observation(2, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Same);
        pinned.timeline_fingerprint = fingerprint(40);
        let observations = [alternatives[0].clone(), alternatives[1].clone(), pinned];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
            }
        );
    }

    #[test]
    fn unchanged_pinned_wins_while_recovery_is_not_required() {
        let mut pinned = observation(1, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Same);
        let observations = [observation(0, "origin-b", HlsCandidateHostRelation::OtherHost), pinned];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::Observe),
            HlsManifestCommitPlan::Commit { candidate_index: 1, kind: HlsManifestCommitKind::Pinned }
        );
    }

    #[test]
    fn alternative_cannot_commit_before_full_burst() {
        let observations = [observation(0, "origin-b", HlsCandidateHostRelation::OtherHost)];

        assert_eq!(
            evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
                full_burst_completed: false,
                current_burst_is_full_plan: false,
                trigger: HlsManifestAcceptanceTrigger::Critical,
                previous_alternative: None,
                observations: &observations,
            }),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn configured_off_single_initial_candidate_uses_acceptance_pipeline() {
        let mut initial = observation(0, "origin-a", HlsCandidateHostRelation::InitialBaseline);
        initial.local_sequence_relation = Some(HlsHostLocalSequenceRelation::NoBaseline);
        let observations = [initial];

        assert_eq!(
            evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
                full_burst_completed: false,
                current_burst_is_full_plan: false,
                trigger: HlsManifestAcceptanceTrigger::RecoveryRequired,
                previous_alternative: None,
                observations: &observations,
            }),
            HlsManifestCommitPlan::RejectAll
        );
        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::Commit { candidate_index: 0, kind: HlsManifestCommitKind::Pinned }
        );
    }

    #[test]
    fn observe_consensus_is_held_without_reserve_pressure() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::Observe),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn recovery_required_consensus_may_stage_new_epoch() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
            }
        );
    }

    #[test]
    fn episode_trigger_remains_authoritative_after_execution_enters_recovery() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];
        let plan = shared::model::HlsManifestRecoveryBurstLevel::Friendly.plan();
        let mut episode = HlsManifestAcceptanceEpisode::new(
            HlsManifestAcceptanceGeneration(7),
            10,
            plan,
            HlsManifestAcceptanceTrigger::Observe,
            &episode_timing(10, plan),
        );
        episode.state = HlsManifestAcceptanceState::Collecting;
        episode.record_full_burst();

        assert_eq!(
            evaluate(&observations, episode.trigger()),
            HlsManifestCommitPlan::HoldAlternative,
            "execution state must not upgrade Observe to reserve pressure"
        );
    }

    #[test]
    fn alternative_requires_ready_staging_before_commit() {
        let mut first = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        let mut second = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        first.switch_segment_readiness = HlsSwitchSegmentReadiness::RequiresStaging;
        second.switch_segment_readiness = HlsSwitchSegmentReadiness::RequiresStaging;
        let observations = [first, second];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
            }
        );
    }

    #[test]
    fn strong_pdt_anchor_qualifies_alternative_without_sequence_comparison() {
        let pdt = 1_700_000_000_000;
        let mut pinned = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Backward);
        pinned.timeline_fingerprint.segment_samples[0].program_date_time_ms = Some(pdt);
        pinned.timeline_fingerprint.segment_samples[1].program_date_time_ms = Some(pdt + 4_000);
        let mut alternative = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        alternative.host_local_media_sequence = 900;
        alternative.host_local_highwater = Some(902);
        alternative.timeline_fingerprint.segment_samples[0].program_date_time_ms = Some(pdt + 500);
        alternative.timeline_fingerprint.segment_samples[1].program_date_time_ms = Some(pdt + 4_500);
        for (index, segment) in alternative.timeline_fingerprint.segment_samples.iter_mut().enumerate() {
            segment.normalized_resource_identity =
                Some(HlsMediaResourceIdentity::for_test(u8::try_from(index).unwrap_or(u8::MAX).saturating_add(90)));
        }

        assert_eq!(
            evaluate(&[pinned, alternative], HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 1,
                kind: HlsManifestCommitKind::AnchoredAlternative,
            }
        );
    }

    #[test]
    fn one_accidental_pdt_overlap_is_not_a_strong_cross_host_anchor() {
        let pdt = 1_700_000_000_000;
        let mut pinned = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Backward);
        pinned.timeline_fingerprint.segment_samples[0].program_date_time_ms = Some(pdt);
        let mut alternative = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        alternative.timeline_fingerprint.segment_samples[0].program_date_time_ms = Some(pdt + 500);
        for (index, segment) in alternative.timeline_fingerprint.segment_samples.iter_mut().enumerate() {
            segment.normalized_resource_identity =
                Some(HlsMediaResourceIdentity::for_test(u8::try_from(index).unwrap_or(u8::MAX).saturating_add(90)));
        }

        assert_eq!(
            evaluate(&[pinned, alternative], HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn same_sequence_on_different_hosts_does_not_create_strong_anchor() {
        let mut pinned = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Backward);
        pinned.timeline_fingerprint = fingerprint(1);
        let mut alternative = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        alternative.timeline_fingerprint = fingerprint(20);

        assert_eq!(
            evaluate(&[pinned, alternative], HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn same_normalized_path_from_different_queries_with_unknown_global_identity_is_not_a_strong_anchor() {
        let mut pinned = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
        pinned.local_sequence_relation = Some(HlsHostLocalSequenceRelation::Backward);
        let mut alternative = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        // Equal normalized identities model equal paths after host and query-token removal.
        // Without PDT or staged byte equality the global content identity remains unknown.
        alternative.timeline_fingerprint.normalized_resource_pattern_hash =
            pinned.timeline_fingerprint.normalized_resource_pattern_hash;

        assert_eq!(
            evaluate(&[pinned, alternative], HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn normalized_path_only_requests_staged_byte_verification_and_never_direct_anchor_commit() {
        let mut candidate = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        candidate.committed_content_anchor = HlsCommittedContentAnchorEvidence::RequiresStagedByteVerification;

        assert_eq!(
            evaluate(&[candidate], HlsManifestAcceptanceTrigger::Observe),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::ContentVerifiedAlternative,
            }
        );
    }

    #[test]
    fn different_local_shapes_do_not_form_a_consensus_cohort() {
        let first = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        let mut second = observation(1, "origin-b", HlsCandidateHostRelation::OtherHost);
        second.timeline_fingerprint.segment_samples[1].duration_ms = 9_000;

        let cohorts = alternative_cohorts(&[first, second]);

        assert_eq!(cohorts.len(), 2);
        assert!(cohorts.iter().all(|cohort| cohort.successful_samples == 1));
    }

    #[test]
    fn reduced_follow_up_preserves_matching_full_burst_evidence_without_incrementing_it() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];
        let full = alternative_cohorts_with_history(&observations, None, true).remove(0);
        let follow_up =
            held_alternative_after_burst(&observations[..1], Some(&full), false).expect("matching follow-up cohort");

        assert_eq!(full.consecutive_confirmed_full_bursts, 1);
        assert_eq!(follow_up.consecutive_confirmed_full_bursts, 1);
        assert_eq!(follow_up.successful_samples, full.successful_samples);
        assert!(follow_up.total_samples > full.total_samples);
    }

    #[test]
    fn matching_later_full_burst_advances_consecutive_evidence() {
        let first_window = [sliding_observation(0, "origin-b", 5, [1, 2, 3])];
        let second_window = [sliding_observation(0, "origin-b", 6, [2, 3, 4])];
        let first = alternative_cohorts_with_history(&first_window, None, true).remove(0);
        let second = alternative_cohorts_with_history(&second_window, Some(&first), true).remove(0);

        assert_eq!(second.consecutive_confirmed_full_bursts, 2);
        assert_ne!(first.window.fingerprint, second.window.fingerprint);
    }

    #[test]
    fn reduced_retry_classifies_sliding_same_cohort_separately_from_new_or_conflicting_cohorts() {
        let first = [sliding_observation(0, "origin-b", 5, [1, 2, 3])];
        let landscape = manifest_acceptance_landscape(&first);
        let sliding = [sliding_observation(0, "origin-b", 6, [2, 3, 4])];
        let new_host = [sliding_observation(0, "origin-c", 6, [2, 3, 4])];
        let conflict = [sliding_observation(0, "origin-b", 6, [90, 91, 92])];

        assert_eq!(classify_reduced_retry_landscape(&landscape, &sliding), HlsReducedRetryLandscapeChange::Unchanged);
        assert_eq!(classify_reduced_retry_landscape(&landscape, &new_host), HlsReducedRetryLandscapeChange::NewCohort);
        assert_eq!(
            classify_reduced_retry_landscape(&landscape, &conflict),
            HlsReducedRetryLandscapeChange::TimelineConflict
        );
    }

    #[test]
    fn two_matching_single_sample_full_bursts_may_stage_under_recovery_pressure() {
        let first_window = [sliding_observation(0, "origin-b", 5, [1, 2, 3])];
        let second_window = [sliding_observation(0, "origin-b", 6, [2, 3, 4])];
        let first = alternative_cohorts_with_history(&first_window, None, true).remove(0);

        assert_eq!(
            evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
                full_burst_completed: true,
                current_burst_is_full_plan: true,
                trigger: HlsManifestAcceptanceTrigger::RecoveryRequired,
                previous_alternative: Some(&first),
                observations: &second_window,
            }),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
            }
        );
    }

    #[test]
    fn conflicting_reduced_follow_up_cannot_reuse_previous_full_burst() {
        let first_observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];
        let first = alternative_cohorts_with_history(&first_observations, None, true).remove(0);
        let mut conflict = observation(0, "origin-c", HlsCandidateHostRelation::OtherHost);
        conflict.timeline_fingerprint = fingerprint(20);

        assert!(alternative_cohorts_with_history(&[conflict], Some(&first), false).is_empty());
    }

    #[test]
    fn failed_reduced_follow_up_does_not_erase_previous_full_burst_evidence() {
        let observations = [
            observation(0, "origin-b", HlsCandidateHostRelation::OtherHost),
            observation(1, "origin-b", HlsCandidateHostRelation::OtherHost),
        ];
        let full = held_alternative_after_burst(&observations, None, true).expect("full burst cohort");

        assert_eq!(held_alternative_after_burst(&[], Some(&full), false), Some(full));
    }

    #[test]
    fn critical_single_candidate_requires_typed_staged_verification() {
        let mut candidate = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        candidate.switch_segment_readiness = HlsSwitchSegmentReadiness::RequiresStaging;
        mark_emergency_verification_eligible(&mut candidate);
        let observations = [candidate.clone()];

        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::Critical),
            HlsManifestCommitPlan::StageAlternative {
                candidate_index: 0,
                kind: HlsManifestCommitKind::EmergencyAlternativeAsNewEpoch,
            }
        );
        assert_eq!(
            evaluate(&observations, HlsManifestAcceptanceTrigger::RecoveryRequired),
            HlsManifestCommitPlan::HoldAlternative
        );

        candidate.emergency_evidence.terminal_alternative = HlsTerminalAlternativeCompatibility::TerminalTailPreferred;
        assert_eq!(
            evaluate(&[candidate], HlsManifestAcceptanceTrigger::Critical),
            HlsManifestCommitPlan::HoldAlternative
        );
    }

    #[test]
    fn critical_single_candidate_without_stageable_first_segment_is_rejected() {
        let mut candidate = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        mark_emergency_verification_eligible(&mut candidate);
        candidate.switch_segment_readiness = HlsSwitchSegmentReadiness::Unavailable;

        assert_eq!(evaluate(&[candidate], HlsManifestAcceptanceTrigger::Critical), HlsManifestCommitPlan::RejectAll);
    }

    #[test]
    fn critical_mode_rejects_ambiguous_single_sample_cohorts() {
        let first = observation(0, "origin-b", HlsCandidateHostRelation::OtherHost);
        let mut second = observation(1, "origin-c", HlsCandidateHostRelation::OtherHost);
        second.timeline_fingerprint = fingerprint(20);

        assert_eq!(
            evaluate(&[first, second], HlsManifestAcceptanceTrigger::Critical),
            HlsManifestCommitPlan::HoldAlternative
        );
    }
}
