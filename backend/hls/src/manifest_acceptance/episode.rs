#[cfg(any(test, feature = "test-support"))]
use super::HlsRecoveryEtaMs;
use super::{
    HlsAcceptanceEpisodeTiming, HlsAlternativeOriginCohort, HlsDeterministicTimelineConflict,
    HlsEstimatedRecoveryCompletionAtMs, HlsManifestAcceptanceEpisode, HlsManifestAcceptanceGeneration,
    HlsManifestRecoveryCandidateIdentity, HlsRecoveryWorkload, HlsRecoveryWorkloadEnvelope,
    HlsSelectedRecoveryCandidate, HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT,
    HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS,
};
use sha2::{Digest, Sha256};
use shared::model::HlsManifestRecoveryBurstPlan;

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
pub(super) enum HlsRecoveryWorkloadBinding {
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
    pub(super) const fn envelope(self) -> HlsRecoveryWorkloadEnvelope {
        match self {
            Self::CandidateUnknown { envelope, .. } | Self::CandidateBound { envelope, .. } => envelope,
        }
    }

    pub(super) const fn workload(self) -> HlsRecoveryWorkload {
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
pub(super) enum HlsManifestAcceptanceEpisodeOutcome {
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
