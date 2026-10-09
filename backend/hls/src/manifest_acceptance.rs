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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsSelectedRecoveryCandidate {
    acceptance_generation: HlsManifestAcceptanceGeneration,
    candidate_identity: HlsManifestRecoveryCandidateIdentity,
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

#[cfg(test)]
mod tests;

mod cohorts;
mod episode;
mod evaluation;
mod evidence;
mod timeline;
#[allow(unused_imports, reason = "Preserves the existing module interface.")]
pub use cohorts::{
    alternative_cohorts, alternative_cohorts_with_history, classify_reduced_retry_landscape,
    held_alternative_after_burst, manifest_acceptance_landscape,
};
pub use episode::{
    manifest_acceptance_episode_status, HlsDeterministicConflictReceipt, HlsManifestAcceptanceEpisodeStatus,
    HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceState, HlsManifestAcceptanceTrigger,
    HlsRecoveryWorkloadBindingUpdate,
};
use episode::{HlsManifestAcceptanceEpisodeOutcome, HlsRecoveryWorkloadBinding};
pub use evaluation::evaluate_manifest_acceptance;
pub use evidence::{
    HlsAlternativeOriginCohort, HlsAlternativeOriginCohortIdentity, HlsAlternativeOriginWindow,
    HlsCandidateHostRelation, HlsCommittedContentAnchorEvidence, HlsCrossHostAcceptanceEvidence,
    HlsEmergencyAcceptanceEvidence, HlsEmergencyLiveHandoffCompatibility, HlsHostLocalSequenceRelation,
    HlsManifestAcceptanceInput, HlsManifestAcceptanceLandscape, HlsManifestCandidateObservation, HlsManifestCommitKind,
    HlsManifestCommitPlan, HlsManifestSegmentFingerprint, HlsManifestTechnicalSignature,
    HlsManifestTimelineFingerprint, HlsPinnedOriginObservationState, HlsReducedRetryLandscapeChange,
    HlsResourceTimelineEvidence, HlsSwitchSegmentReadiness, HlsTerminalAlternativeCompatibility,
};
pub use timeline::classify_host_local_sequence;
