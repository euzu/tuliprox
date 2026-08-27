use super::manifest_acceptance::HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS;
pub use super::prepared_terminal_bundle::HlsPreparedTerminalBundleKey as HlsTerminalMediaPreparationKey;
use shared::model::HlsManifestRecoveryBurstPlan;

const HLS_ACCEPTANCE_EVALUATION_ETA_MS: u64 = 100;
const HLS_ACCEPTANCE_COMMIT_ETA_MS: u64 = 100;
const HLS_ACCEPTANCE_SCHEDULING_ETA_MS: u64 = 100;
const HLS_MANIFEST_REQUEST_FALLBACK_ETA_MS: u64 = 3_000;

/// Converts a hard per-operation limit into a conservative fallback estimate
/// without treating unusually large protection timeouts as expected latency.
pub const fn bounded_manifest_request_eta_ms(operation_timeout_ms: u64) -> u64 {
    if operation_timeout_ms < HLS_MANIFEST_REQUEST_FALLBACK_ETA_MS {
        operation_timeout_ms
    } else {
        HLS_MANIFEST_REQUEST_FALLBACK_ETA_MS
    }
}

macro_rules! duration_millis_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_millis(milliseconds: u64) -> Self { Self(milliseconds) }
        }
    };
}

macro_rules! instant_millis_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_millis_since_epoch(milliseconds: u64) -> Self { Self(milliseconds) }
        }
    };
}

duration_millis_newtype!(HlsOperationTimeoutMs);
duration_millis_newtype!(HlsRecoveryTriggerBudgetMs);
duration_millis_newtype!(HlsRecoveryEtaMs);
duration_millis_newtype!(HlsTransitionMarginMs);

instant_millis_newtype!(HlsAcceptanceDeadlineMs);
instant_millis_newtype!(HlsEstimatedRecoveryCompletionAtMs);
instant_millis_newtype!(HlsLeaseExhaustionAtMs);
instant_millis_newtype!(HlsLatestSafeTerminalCommitAtMs);

impl HlsOperationTimeoutMs {
    #[cfg(any(test, feature = "test-support"))]
    pub const fn as_millis(self) -> u64 { self.0 }
}

impl HlsRecoveryTriggerBudgetMs {
    pub const fn as_millis(self) -> u64 { self.0 }
}

impl HlsRecoveryEtaMs {
    pub const fn as_millis(self) -> u64 { self.0 }
}

impl HlsTransitionMarginMs {
    pub const fn as_millis(self) -> u64 { self.0 }
}

impl HlsAcceptanceDeadlineMs {
    pub const fn as_millis_since_epoch(self) -> u64 { self.0 }
}

impl HlsEstimatedRecoveryCompletionAtMs {
    #[cfg(any(test, feature = "test-support"))]
    pub const fn as_millis_since_epoch(self) -> u64 { self.0 }
}

impl HlsLeaseExhaustionAtMs {
    #[cfg(any(test, feature = "test-support"))]
    pub const fn as_millis_since_epoch(self) -> u64 { self.0 }
}

impl HlsLatestSafeTerminalCommitAtMs {
    pub const fn as_millis_since_epoch(self) -> u64 { self.0 }
}

/// Small bounded interval reserved exclusively for acquiring the final
/// terminal-publication locks before the full transition margin begins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct HlsTerminalCommitAcquisitionBudgetMs(u64);

impl HlsTerminalCommitAcquisitionBudgetMs {
    pub fn from_retry_policy() -> Self { Self(super::terminal_commit::terminal_commit_retry_schedule_budget_ms()) }

    pub fn fail_closed_handoff_from_retry_policy() -> Self {
        Self(super::terminal_commit::terminal_commit_retry_handoff_budget_ms())
    }

    pub const fn as_millis(self) -> u64 { self.0 }
}

/// Lease-local phase of terminal acquisition. `AcquisitionOpen` is not an
/// admission or origin-progress transition; it only permits the bounded final
/// commit to begin before the unchanged product transition margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalCommitWindow {
    NotDue,
    AcquisitionOpen,
    CutoverDue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoveryBurstWorkload {
    FullBurstPending,
    FullBurstCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoverySegmentWorkload {
    SegmentStagedWithDependenciesReady,
    ClearSegmentFetch,
    Aes128SegmentFetchWithReadyKey,
    Aes128SegmentFetchWithKeyFetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoveryMapWorkload {
    NotRequired,
    Ready,
    Fetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoveryObjectReadiness {
    Ready,
    Fetch,
    Staged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsRecoveryEncryptionReadiness {
    Clear,
    Aes128 { key: HlsRecoveryObjectReadiness },
}

/// Generation-local state of the exact medium that recovery would publish next.
///
/// Parsing and cache inspection stay in the orchestration layer. This type only
/// converts already established READY/FETCH/STAGED evidence into timing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsRecoveryMediumReadiness {
    pub segment: HlsRecoveryObjectReadiness,
    pub map: Option<HlsRecoveryObjectReadiness>,
    pub encryption: HlsRecoveryEncryptionReadiness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsRecoveryWorkload {
    pub burst: HlsRecoveryBurstWorkload,
    pub segment: HlsRecoverySegmentWorkload,
    pub map: HlsRecoveryMapWorkload,
}

impl HlsRecoveryWorkload {
    #[cfg(any(test, feature = "test-support"))]
    pub const fn clear_fetch() -> Self {
        Self {
            burst: HlsRecoveryBurstWorkload::FullBurstPending,
            segment: HlsRecoverySegmentWorkload::ClearSegmentFetch,
            map: HlsRecoveryMapWorkload::NotRequired,
        }
    }

    pub const fn after_full_burst(self) -> Self { Self { burst: HlsRecoveryBurstWorkload::FullBurstCompleted, ..self } }

    pub const fn from_recovery_medium(medium: HlsRecoveryMediumReadiness) -> Self {
        let segment = match (medium.segment, medium.encryption) {
            (
                HlsRecoveryObjectReadiness::Fetch,
                HlsRecoveryEncryptionReadiness::Aes128 { key: HlsRecoveryObjectReadiness::Fetch },
            ) => HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch,
            (HlsRecoveryObjectReadiness::Fetch, HlsRecoveryEncryptionReadiness::Aes128 { .. }) => {
                HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey
            }
            (HlsRecoveryObjectReadiness::Fetch, HlsRecoveryEncryptionReadiness::Clear) => {
                HlsRecoverySegmentWorkload::ClearSegmentFetch
            }
            (
                HlsRecoveryObjectReadiness::Ready | HlsRecoveryObjectReadiness::Staged,
                HlsRecoveryEncryptionReadiness::Clear
                | HlsRecoveryEncryptionReadiness::Aes128 {
                    key: HlsRecoveryObjectReadiness::Ready | HlsRecoveryObjectReadiness::Staged,
                },
            ) => HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady,
            (
                HlsRecoveryObjectReadiness::Ready | HlsRecoveryObjectReadiness::Staged,
                HlsRecoveryEncryptionReadiness::Aes128 { key: HlsRecoveryObjectReadiness::Fetch },
            ) => {
                // The media object is staged, but its AES key is still missing.
                // Keep the estimate conservative so this state cannot claim a
                // zero-work handoff.
                HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch
            }
        };
        let map = match medium.map {
            None => HlsRecoveryMapWorkload::NotRequired,
            Some(HlsRecoveryObjectReadiness::Fetch) => HlsRecoveryMapWorkload::Fetch,
            Some(HlsRecoveryObjectReadiness::Ready | HlsRecoveryObjectReadiness::Staged) => {
                HlsRecoveryMapWorkload::Ready
            }
        };
        Self { burst: HlsRecoveryBurstWorkload::FullBurstCompleted, segment, map }
    }

    pub const fn is_no_greater_than(self, ceiling: Self) -> bool {
        recovery_burst_work_units(self.burst) <= recovery_burst_work_units(ceiling.burst)
            && recovery_segment_object_count(self.segment) <= recovery_segment_object_count(ceiling.segment)
            && recovery_map_object_count(self.map) <= recovery_map_object_count(ceiling.map)
    }
}

/// Conservative work admitted before a concrete manifest candidate exists.
///
/// It deliberately covers a full configured burst, an AES-128 segment whose
/// key still needs fetching, and a required MAP. Candidate binding may reduce
/// this estimate, but can never expand the immutable episode budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsRecoveryWorkloadEnvelope {
    ceiling: HlsRecoveryWorkload,
}

impl HlsRecoveryWorkloadEnvelope {
    pub const fn acceptance_policy() -> Self {
        Self {
            ceiling: HlsRecoveryWorkload {
                burst: HlsRecoveryBurstWorkload::FullBurstPending,
                segment: HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch,
                map: HlsRecoveryMapWorkload::Fetch,
            },
        }
    }

    pub const fn from_timing_ceiling(ceiling: HlsRecoveryWorkload) -> Self { Self { ceiling } }

    pub const fn ceiling(self) -> HlsRecoveryWorkload { self.ceiling }

    pub const fn contains(self, workload: HlsRecoveryWorkload) -> bool { workload.is_no_greater_than(self.ceiling) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalMediaPreparationState {
    Ready { key: HlsTerminalMediaPreparationKey },
    Preparing { key: HlsTerminalMediaPreparationKey },
    Incompatible { key: Option<HlsTerminalMediaPreparationKey> },
    Failed { key: Option<HlsTerminalMediaPreparationKey> },
}

/// Generation-frozen interpretation consumed by the cutover policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalMediaPreparationDisposition {
    Ready,
    Preparing,
    Incompatible,
    Failed,
    KeyMismatch,
}

impl HlsTerminalMediaPreparationState {
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_ready_for(self, required: HlsTerminalMediaPreparationKey) -> bool {
        matches!(self, Self::Ready { key } if key == required)
    }

    pub fn disposition_for(
        self,
        required: Option<HlsTerminalMediaPreparationKey>,
    ) -> HlsTerminalMediaPreparationDisposition {
        match (self, required) {
            (Self::Ready { key }, Some(required)) if key == required => HlsTerminalMediaPreparationDisposition::Ready,
            (Self::Preparing { key }, Some(required)) if key == required => {
                HlsTerminalMediaPreparationDisposition::Preparing
            }
            (Self::Incompatible { .. }, _) => HlsTerminalMediaPreparationDisposition::Incompatible,
            (Self::Failed { .. }, _) => HlsTerminalMediaPreparationDisposition::Failed,
            (Self::Ready { .. } | Self::Preparing { .. }, _) => HlsTerminalMediaPreparationDisposition::KeyMismatch,
        }
    }

    /// Accepts synchronously prepared media only when both the frozen state
    /// and the completed preparation refer to the exact required key.
    pub fn authorizes_prepared_key(
        self,
        required: Option<HlsTerminalMediaPreparationKey>,
        prepared: HlsTerminalMediaPreparationKey,
    ) -> bool {
        match self {
            Self::Ready { key } | Self::Preparing { key } => required == Some(key) && prepared == key,
            Self::Incompatible { .. } | Self::Failed { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HlsObservedRecoveryLatency {
    pub p95: Option<HlsRecoveryEtaMs>,
    pub p99: Option<HlsRecoveryEtaMs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsRecoveryTimingPolicy {
    pub manifest_operation_timeout: HlsOperationTimeoutMs,
    pub media_operation_timeout: HlsOperationTimeoutMs,
    pub manifest_request_eta: HlsRecoveryEtaMs,
    pub media_object_eta: HlsRecoveryEtaMs,
    pub evaluation_eta: HlsRecoveryEtaMs,
    pub commit_eta: HlsRecoveryEtaMs,
    pub scheduling_eta: HlsRecoveryEtaMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsAcceptanceEpisodeTimingSeed {
    pub target_duration_ms: u64,
    pub transition_margin: HlsTransitionMarginMs,
    pub workload: HlsRecoveryWorkload,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_media_preparation: HlsTerminalMediaPreparationState,
}

impl HlsRecoveryTimingPolicy {
    pub const fn new(
        manifest_operation_timeout: HlsOperationTimeoutMs,
        media_operation_timeout: HlsOperationTimeoutMs,
        manifest_request_eta: HlsRecoveryEtaMs,
        media_object_eta: HlsRecoveryEtaMs,
    ) -> Self {
        Self {
            manifest_operation_timeout,
            media_operation_timeout,
            manifest_request_eta,
            media_object_eta,
            evaluation_eta: HlsRecoveryEtaMs::from_millis(HLS_ACCEPTANCE_EVALUATION_ETA_MS),
            commit_eta: HlsRecoveryEtaMs::from_millis(HLS_ACCEPTANCE_COMMIT_ETA_MS),
            scheduling_eta: HlsRecoveryEtaMs::from_millis(HLS_ACCEPTANCE_SCHEDULING_ETA_MS),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsAcceptanceEpisodeTiming {
    pub manifest_operation_timeout: HlsOperationTimeoutMs,
    pub media_operation_timeout: HlsOperationTimeoutMs,
    pub burst_eta: HlsRecoveryEtaMs,
    pub evaluation_eta: HlsRecoveryEtaMs,
    pub commit_eta: HlsRecoveryEtaMs,
    pub media_object_eta: HlsRecoveryEtaMs,
    pub first_segment_eta: HlsRecoveryEtaMs,
    pub scheduling_eta: HlsRecoveryEtaMs,
    pub trigger_budget: HlsRecoveryTriggerBudgetMs,
    pub initial_recovery_eta: HlsRecoveryEtaMs,
    pub transition_margin: HlsTransitionMarginMs,
    pub acceptance_deadline: HlsAcceptanceDeadlineMs,
    pub target_duration_ms: u64,
    pub initial_workload: HlsRecoveryWorkload,
    pub observed_latency: HlsObservedRecoveryLatency,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_media_preparation: HlsTerminalMediaPreparationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsAcceptanceEpisodeTimingInput {
    pub started_at_ms: u64,
    pub burst_plan: HlsManifestRecoveryBurstPlan,
    pub target_duration_ms: u64,
    pub transition_margin: HlsTransitionMarginMs,
    pub workload: HlsRecoveryWorkload,
    pub observed_latency: HlsObservedRecoveryLatency,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_media_preparation: HlsTerminalMediaPreparationState,
    pub policy: HlsRecoveryTimingPolicy,
}

impl HlsAcceptanceEpisodeTiming {
    pub fn from_input(input: &HlsAcceptanceEpisodeTimingInput) -> Self {
        let burst_eta = HlsRecoveryEtaMs::from_millis(
            manifest_recovery_burst_max_stagger_ms(input.burst_plan)
                .saturating_add(input.policy.manifest_request_eta.as_millis()),
        );
        let initial_burst_eta = match input.workload.burst {
            HlsRecoveryBurstWorkload::FullBurstPending => burst_eta,
            HlsRecoveryBurstWorkload::FullBurstCompleted => HlsRecoveryEtaMs::default(),
        };
        let first_segment_eta = recovery_media_eta(input.workload, input.policy.media_object_eta);
        let calculated_eta_ms = recovery_eta_sum_ms(
            initial_burst_eta,
            input.policy.evaluation_eta,
            input.policy.commit_eta,
            first_segment_eta,
            input.policy.scheduling_eta,
        );
        let trigger_budget_ms =
            calculated_eta_ms.max(input.observed_latency.p95.map_or(0, HlsRecoveryEtaMs::as_millis));
        let initial_recovery_eta_ms =
            calculated_eta_ms.max(input.observed_latency.p99.map_or(trigger_budget_ms, HlsRecoveryEtaMs::as_millis));
        let deadline_budget_ms = trigger_budget_ms.max(initial_recovery_eta_ms);
        Self {
            manifest_operation_timeout: input.policy.manifest_operation_timeout,
            media_operation_timeout: input.policy.media_operation_timeout,
            burst_eta,
            evaluation_eta: input.policy.evaluation_eta,
            commit_eta: input.policy.commit_eta,
            media_object_eta: input.policy.media_object_eta,
            first_segment_eta,
            scheduling_eta: input.policy.scheduling_eta,
            trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(trigger_budget_ms),
            initial_recovery_eta: HlsRecoveryEtaMs::from_millis(initial_recovery_eta_ms),
            transition_margin: input.transition_margin,
            acceptance_deadline: HlsAcceptanceDeadlineMs::from_millis_since_epoch(
                input.started_at_ms.saturating_add(deadline_budget_ms),
            ),
            target_duration_ms: input.target_duration_ms,
            initial_workload: input.workload,
            observed_latency: input.observed_latency,
            required_terminal_media_key: input.required_terminal_media_key,
            terminal_media_preparation: input.terminal_media_preparation,
        }
    }

    pub fn remaining_eta(self, workload: HlsRecoveryWorkload) -> HlsRecoveryEtaMs {
        let burst_eta = match workload.burst {
            HlsRecoveryBurstWorkload::FullBurstPending => self.burst_eta,
            HlsRecoveryBurstWorkload::FullBurstCompleted => HlsRecoveryEtaMs::default(),
        };
        let media_eta = recovery_media_eta(workload, self.media_object_eta);
        let workload_eta = HlsRecoveryEtaMs::from_millis(recovery_eta_sum_ms(
            burst_eta,
            self.evaluation_eta,
            self.commit_eta,
            media_eta,
            self.scheduling_eta,
        ));
        if workload == self.initial_workload {
            workload_eta.max(self.initial_recovery_eta)
        } else {
            workload_eta
        }
    }

    pub fn estimated_completion_at(
        self,
        now_ms: u64,
        workload: HlsRecoveryWorkload,
    ) -> HlsEstimatedRecoveryCompletionAtMs {
        HlsEstimatedRecoveryCompletionAtMs::from_millis_since_epoch(
            now_ms.saturating_add(self.remaining_eta(workload).as_millis()),
        )
    }
}

fn recovery_media_eta(workload: HlsRecoveryWorkload, media_object_eta: HlsRecoveryEtaMs) -> HlsRecoveryEtaMs {
    HlsRecoveryEtaMs::from_millis(media_object_eta.as_millis().saturating_mul(recovery_media_object_count(workload)))
}

const fn recovery_media_object_count(workload: HlsRecoveryWorkload) -> u64 {
    recovery_segment_object_count(workload.segment).saturating_add(recovery_map_object_count(workload.map))
}

const fn recovery_burst_work_units(workload: HlsRecoveryBurstWorkload) -> u8 {
    match workload {
        HlsRecoveryBurstWorkload::FullBurstCompleted => 0,
        HlsRecoveryBurstWorkload::FullBurstPending => 1,
    }
}

const fn recovery_segment_object_count(workload: HlsRecoverySegmentWorkload) -> u64 {
    match workload {
        HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady => 0,
        HlsRecoverySegmentWorkload::ClearSegmentFetch | HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey => 1,
        HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch => 2,
    }
}

const fn recovery_map_object_count(workload: HlsRecoveryMapWorkload) -> u64 {
    match workload {
        HlsRecoveryMapWorkload::NotRequired | HlsRecoveryMapWorkload::Ready => 0,
        HlsRecoveryMapWorkload::Fetch => 1,
    }
}

fn recovery_eta_sum_ms(
    burst: HlsRecoveryEtaMs,
    evaluation: HlsRecoveryEtaMs,
    commit: HlsRecoveryEtaMs,
    first_segment: HlsRecoveryEtaMs,
    scheduling: HlsRecoveryEtaMs,
) -> u64 {
    [burst, evaluation, commit, first_segment, scheduling]
        .into_iter()
        .fold(0_u64, |total, duration| total.saturating_add(duration.as_millis()))
}

pub fn manifest_recovery_burst_max_stagger_ms(plan: HlsManifestRecoveryBurstPlan) -> u64 {
    u64::try_from(plan.slots.saturating_sub(1))
        .unwrap_or(u64::MAX)
        .saturating_mul(HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsLeaseCutoverTiming {
    pub guaranteed_reserve_ms: u64,
    pub transition_margin: HlsTransitionMarginMs,
    pub lease_exhaustion_at: HlsLeaseExhaustionAtMs,
    pub latest_safe_terminal_commit_at: HlsLatestSafeTerminalCommitAtMs,
    pub estimated_recovery_completion_at: Option<HlsEstimatedRecoveryCompletionAtMs>,
}

impl HlsLeaseCutoverTiming {
    pub fn from_reserve(
        now_ms: u64,
        guaranteed_reserve_ms: u64,
        transition_margin: HlsTransitionMarginMs,
        estimated_recovery_completion_at: Option<HlsEstimatedRecoveryCompletionAtMs>,
    ) -> Self {
        let lease_exhaustion_at_ms = now_ms.saturating_add(guaranteed_reserve_ms);
        Self {
            guaranteed_reserve_ms,
            transition_margin,
            lease_exhaustion_at: HlsLeaseExhaustionAtMs::from_millis_since_epoch(lease_exhaustion_at_ms),
            latest_safe_terminal_commit_at: HlsLatestSafeTerminalCommitAtMs::from_millis_since_epoch(
                lease_exhaustion_at_ms.saturating_sub(transition_margin.as_millis()),
            ),
            estimated_recovery_completion_at,
        }
    }

    pub const fn with_estimated_recovery_completion_at(
        self,
        estimated_recovery_completion_at: Option<HlsEstimatedRecoveryCompletionAtMs>,
    ) -> Self {
        Self { estimated_recovery_completion_at, ..self }
    }

    pub fn terminal_commit_window(
        self,
        origin_path_degraded: bool,
        recovery_committed: bool,
        acquisition_budget: HlsTerminalCommitAcquisitionBudgetMs,
    ) -> HlsTerminalCommitWindow {
        if !origin_path_degraded || recovery_committed {
            return HlsTerminalCommitWindow::NotDue;
        }
        if self.guaranteed_reserve_ms <= self.transition_margin.as_millis() {
            return HlsTerminalCommitWindow::CutoverDue;
        }
        let trigger_reserve_ms = self.transition_margin.as_millis().saturating_add(acquisition_budget.as_millis());
        if self.guaranteed_reserve_ms <= trigger_reserve_ms {
            HlsTerminalCommitWindow::AcquisitionOpen
        } else {
            HlsTerminalCommitWindow::NotDue
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn recovery_may_delay_cutover(self) -> bool {
        match self.estimated_recovery_completion_at {
            Some(completion) => {
                completion.as_millis_since_epoch() < self.latest_safe_terminal_commit_at.as_millis_since_epoch()
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::terminal_tail::HlsTerminalAssetIdentity, *};
    use shared::model::HlsManifestRecoveryBurstLevel;

    const KEY: HlsTerminalMediaPreparationKey = HlsTerminalMediaPreparationKey {
        asset: HlsTerminalAssetIdentity { revision: 7, fingerprint: [7; 32] },
        target_duration_ms: 10_000,
        segment_count: 12,
    };

    fn policy() -> HlsRecoveryTimingPolicy {
        HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(15_000),
            HlsOperationTimeoutMs::from_millis(125_200),
            HlsRecoveryEtaMs::from_millis(3_000),
            HlsRecoveryEtaMs::from_millis(5_000),
        )
    }

    fn timing(workload: HlsRecoveryWorkload) -> HlsAcceptanceEpisodeTiming {
        HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: 1_000,
            burst_plan: HlsManifestRecoveryBurstLevel::Beast.plan(),
            target_duration_ms: 10_000,
            transition_margin: HlsTransitionMarginMs::from_millis(10_000),
            workload,
            observed_latency: HlsObservedRecoveryLatency::default(),
            required_terminal_media_key: Some(KEY),
            terminal_media_preparation: HlsTerminalMediaPreparationState::Preparing { key: KEY },
            policy: policy(),
        })
    }

    #[test]
    fn hls_recovery_timing_workload_classes_have_distinct_expected_etas() {
        let staged = timing(HlsRecoveryWorkload {
            burst: HlsRecoveryBurstWorkload::FullBurstCompleted,
            segment: HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady,
            map: HlsRecoveryMapWorkload::NotRequired,
        });
        let clear = timing(HlsRecoveryWorkload::clear_fetch());
        let aes_ready = timing(HlsRecoveryWorkload {
            segment: HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey,
            ..HlsRecoveryWorkload::clear_fetch()
        });
        let aes_key_fetch = timing(HlsRecoveryWorkload {
            segment: HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch,
            ..HlsRecoveryWorkload::clear_fetch()
        });
        let with_map_fetch =
            timing(HlsRecoveryWorkload { map: HlsRecoveryMapWorkload::Fetch, ..HlsRecoveryWorkload::clear_fetch() });

        assert!(staged.initial_recovery_eta < clear.initial_recovery_eta);
        assert_eq!(clear.first_segment_eta, aes_ready.first_segment_eta);
        assert!(aes_key_fetch.first_segment_eta > aes_ready.first_segment_eta);
        assert!(with_map_fetch.first_segment_eta > clear.first_segment_eta);
        assert!(clear.initial_recovery_eta.as_millis() < clear.media_operation_timeout.as_millis());
    }

    #[test]
    fn hls_recovery_timing_candidate_medium_maps_actual_ready_fetch_and_staged_dependencies() {
        let map_fetch = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
            segment: HlsRecoveryObjectReadiness::Fetch,
            map: Some(HlsRecoveryObjectReadiness::Fetch),
            encryption: HlsRecoveryEncryptionReadiness::Clear,
        });
        let map_ready = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
            map: Some(HlsRecoveryObjectReadiness::Ready),
            ..HlsRecoveryMediumReadiness {
                segment: HlsRecoveryObjectReadiness::Fetch,
                map: None,
                encryption: HlsRecoveryEncryptionReadiness::Clear,
            }
        });
        let staged = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
            segment: HlsRecoveryObjectReadiness::Staged,
            map: Some(HlsRecoveryObjectReadiness::Staged),
            encryption: HlsRecoveryEncryptionReadiness::Clear,
        });

        assert_eq!(map_fetch.segment, HlsRecoverySegmentWorkload::ClearSegmentFetch);
        assert_eq!(map_fetch.map, HlsRecoveryMapWorkload::Fetch);
        assert_eq!(map_ready.map, HlsRecoveryMapWorkload::Ready);
        assert_eq!(staged.segment, HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady);
        assert_eq!(staged.map, HlsRecoveryMapWorkload::Ready);
        assert!(timing(map_fetch).first_segment_eta > timing(map_ready).first_segment_eta);
        assert!(timing(map_ready).first_segment_eta > timing(staged).first_segment_eta);
    }

    #[test]
    fn hls_recovery_timing_candidate_medium_distinguishes_ready_key_from_key_fetch() {
        let workload = |key| {
            HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
                segment: HlsRecoveryObjectReadiness::Fetch,
                map: None,
                encryption: HlsRecoveryEncryptionReadiness::Aes128 { key },
            })
        };
        let ready = workload(HlsRecoveryObjectReadiness::Ready);
        let staged = workload(HlsRecoveryObjectReadiness::Staged);
        let fetch = workload(HlsRecoveryObjectReadiness::Fetch);

        assert_eq!(ready.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey);
        assert_eq!(staged.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey);
        assert_eq!(fetch.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch);
        assert!(timing(fetch).first_segment_eta > timing(ready).first_segment_eta);

        let staged_media = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
            segment: HlsRecoveryObjectReadiness::Staged,
            map: None,
            encryption: HlsRecoveryEncryptionReadiness::Aes128 { key: HlsRecoveryObjectReadiness::Ready },
        });
        assert_eq!(staged_media.segment, HlsRecoverySegmentWorkload::SegmentStagedWithDependenciesReady);
    }

    #[test]
    fn hls_recovery_timing_completed_burst_removes_burst_eta_from_remaining_work() {
        let timing = timing(HlsRecoveryWorkload::clear_fetch());
        let pending = timing.remaining_eta(HlsRecoveryWorkload::clear_fetch());
        let completed = timing.remaining_eta(HlsRecoveryWorkload::clear_fetch().after_full_burst());

        assert_eq!(pending.as_millis().saturating_sub(completed.as_millis()), timing.burst_eta.as_millis());
    }

    #[test]
    fn hls_recovery_timing_observations_are_frozen_without_reusing_operation_timeout() {
        let mut input = HlsAcceptanceEpisodeTimingInput {
            started_at_ms: 500,
            burst_plan: HlsManifestRecoveryBurstLevel::Friendly.plan(),
            target_duration_ms: 8_000,
            transition_margin: HlsTransitionMarginMs::from_millis(8_000),
            workload: HlsRecoveryWorkload::clear_fetch(),
            observed_latency: HlsObservedRecoveryLatency {
                p95: Some(HlsRecoveryEtaMs::from_millis(12_000)),
                p99: Some(HlsRecoveryEtaMs::from_millis(14_000)),
            },
            required_terminal_media_key: Some(KEY),
            terminal_media_preparation: HlsTerminalMediaPreparationState::Preparing { key: KEY },
            policy: policy(),
        };
        let frozen = HlsAcceptanceEpisodeTiming::from_input(&input);
        input.observed_latency = HlsObservedRecoveryLatency {
            p95: Some(HlsRecoveryEtaMs::from_millis(60_000)),
            p99: Some(HlsRecoveryEtaMs::from_millis(70_000)),
        };
        let later = HlsAcceptanceEpisodeTiming::from_input(&input);

        assert_eq!(frozen.observed_latency.p99.map(HlsRecoveryEtaMs::as_millis), Some(14_000));
        assert_eq!(frozen.remaining_eta(frozen.initial_workload), frozen.initial_recovery_eta);
        assert_ne!(frozen.acceptance_deadline, later.acceptance_deadline);
        assert!(frozen.initial_recovery_eta.as_millis() < frozen.media_operation_timeout.as_millis());
        assert_eq!(bounded_manifest_request_eta_ms(120_000), 3_000);
        assert_eq!(bounded_manifest_request_eta_ms(750), 750);
    }

    #[test]
    fn hls_recovery_timing_cutover_uses_strict_safe_deadline() {
        let before = HlsEstimatedRecoveryCompletionAtMs::from_millis_since_epoch(10_999);
        let equal = HlsEstimatedRecoveryCompletionAtMs::from_millis_since_epoch(11_000);
        let after = HlsEstimatedRecoveryCompletionAtMs::from_millis_since_epoch(11_001);
        let timing = |completion| {
            HlsLeaseCutoverTiming::from_reserve(
                1_000,
                20_000,
                HlsTransitionMarginMs::from_millis(10_000),
                Some(completion),
            )
        };

        assert!(timing(before).recovery_may_delay_cutover());
        assert!(!timing(equal).recovery_may_delay_cutover());
        assert!(!timing(after).recovery_may_delay_cutover());
    }

    #[test]
    fn hls_recovery_timing_margin_or_overflow_never_creates_extra_safe_time() {
        let at_margin = HlsLeaseCutoverTiming::from_reserve(
            5_000,
            10_000,
            HlsTransitionMarginMs::from_millis(10_000),
            Some(HlsEstimatedRecoveryCompletionAtMs::from_millis_since_epoch(5_001)),
        );
        let saturated =
            HlsLeaseCutoverTiming::from_reserve(u64::MAX - 5, 10, HlsTransitionMarginMs::from_millis(3), None);

        assert_eq!(at_margin.latest_safe_terminal_commit_at.as_millis_since_epoch(), 5_000);
        assert!(!at_margin.recovery_may_delay_cutover());
        assert_eq!(saturated.lease_exhaustion_at.as_millis_since_epoch(), u64::MAX);
        assert_eq!(saturated.latest_safe_terminal_commit_at.as_millis_since_epoch(), u64::MAX - 3);
    }

    #[test]
    fn hls_terminal_commit_acquisition_window_is_bounded_by_the_retry_policy() {
        let now_ms = 5_000;
        let margin = HlsTransitionMarginMs::from_millis(10_000);
        let budget = HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy();
        assert_eq!(budget.as_millis(), super::super::terminal_commit::terminal_commit_retry_schedule_budget_ms());

        let at_trigger = HlsLeaseCutoverTiming::from_reserve(
            now_ms,
            margin.as_millis().saturating_add(budget.as_millis()),
            margin,
            None,
        );
        let before_trigger = HlsLeaseCutoverTiming::from_reserve(
            now_ms,
            margin.as_millis().saturating_add(budget.as_millis()).saturating_add(1),
            margin,
            None,
        );
        let at_margin = HlsLeaseCutoverTiming::from_reserve(now_ms, margin.as_millis(), margin, None);

        assert_eq!(at_trigger.terminal_commit_window(true, false, budget), HlsTerminalCommitWindow::AcquisitionOpen);
        assert_eq!(before_trigger.terminal_commit_window(true, false, budget), HlsTerminalCommitWindow::NotDue);
        assert_eq!(at_margin.terminal_commit_window(true, false, budget), HlsTerminalCommitWindow::CutoverDue);
        assert_eq!(at_trigger.terminal_commit_window(false, false, budget), HlsTerminalCommitWindow::NotDue);
        assert_eq!(at_trigger.terminal_commit_window(true, true, budget), HlsTerminalCommitWindow::NotDue);
        assert_eq!(
            at_trigger.latest_safe_terminal_commit_at.as_millis_since_epoch(),
            now_ms.saturating_add(budget.as_millis())
        );
    }

    #[test]
    fn hls_recovery_timing_episode_deadline_saturates_at_maximum_instant() {
        let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: u64::MAX - 5,
            burst_plan: HlsManifestRecoveryBurstLevel::Beast.plan(),
            target_duration_ms: 10_000,
            transition_margin: HlsTransitionMarginMs::from_millis(10_000),
            workload: HlsRecoveryWorkload::clear_fetch(),
            observed_latency: HlsObservedRecoveryLatency {
                p95: Some(HlsRecoveryEtaMs::from_millis(u64::MAX)),
                p99: Some(HlsRecoveryEtaMs::from_millis(u64::MAX)),
            },
            required_terminal_media_key: Some(KEY),
            terminal_media_preparation: HlsTerminalMediaPreparationState::Preparing { key: KEY },
            policy: policy(),
        });

        assert_eq!(timing.acceptance_deadline.as_millis_since_epoch(), u64::MAX);
        assert_eq!(timing.trigger_budget.as_millis(), u64::MAX);
        assert_eq!(timing.initial_recovery_eta.as_millis(), u64::MAX);
    }

    #[test]
    fn hls_prepared_terminal_bundle_timing_state_requires_exact_canonical_key() {
        let other = HlsTerminalMediaPreparationKey { target_duration_ms: 9_000, ..KEY };
        let other_revision =
            HlsTerminalMediaPreparationKey { asset: HlsTerminalAssetIdentity { revision: 8, ..KEY.asset }, ..KEY };
        let mut fingerprint = KEY.asset.fingerprint;
        fingerprint[0] ^= 0xff;
        let other_fingerprint =
            HlsTerminalMediaPreparationKey { asset: HlsTerminalAssetIdentity { fingerprint, ..KEY.asset }, ..KEY };
        let other_count = HlsTerminalMediaPreparationKey { segment_count: 11, ..KEY };

        assert!(HlsTerminalMediaPreparationState::Ready { key: KEY }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: other }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: other_revision }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: other_fingerprint }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: other_count }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Preparing { key: KEY }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Incompatible { key: Some(KEY) }.is_ready_for(KEY));
        assert!(!HlsTerminalMediaPreparationState::Failed { key: Some(KEY) }.is_ready_for(KEY));
        assert_eq!(
            HlsTerminalMediaPreparationState::Ready { key: other }.disposition_for(Some(KEY)),
            HlsTerminalMediaPreparationDisposition::KeyMismatch
        );
        assert_eq!(
            HlsTerminalMediaPreparationState::Preparing { key: KEY }.disposition_for(Some(KEY)),
            HlsTerminalMediaPreparationDisposition::Preparing
        );
        assert_eq!(
            HlsTerminalMediaPreparationState::Incompatible { key: Some(KEY) }.disposition_for(Some(KEY)),
            HlsTerminalMediaPreparationDisposition::Incompatible
        );
        assert!(HlsTerminalMediaPreparationState::Ready { key: KEY }.authorizes_prepared_key(Some(KEY), KEY));
        assert!(HlsTerminalMediaPreparationState::Preparing { key: KEY }.authorizes_prepared_key(Some(KEY), KEY));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: KEY }.authorizes_prepared_key(Some(KEY), other));
        assert!(!HlsTerminalMediaPreparationState::Ready { key: other }.authorizes_prepared_key(Some(KEY), KEY));
        assert!(
            !HlsTerminalMediaPreparationState::Incompatible { key: Some(KEY) }.authorizes_prepared_key(Some(KEY), KEY)
        );
    }
}
