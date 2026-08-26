use super::recovery_timing::{HlsObservedRecoveryLatency, HlsRecoveryEtaMs};
use std::collections::VecDeque;

const PUBLICATION_LATENESS_NUMERATOR: u64 = 3;
const PUBLICATION_LATENESS_DENOMINATOR: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginProgressPhase {
    Cold,
    Fresh,
    PublicationLate,
    RecoveryRequired,
    Recovering,
    Critical,
    TerminalPartial,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginPathCondition {
    ProgressExpected,
    PublicationLate,
    RetryableFetchFailure,
    HardFetchFailure,
    AcceptanceConflict,
    SegmentReadinessFailure,
}

impl HlsOriginPathCondition {
    pub const fn is_degraded(self) -> bool { !matches!(self, Self::ProgressExpected) }

    const fn opens_episode_immediately(self) -> bool {
        matches!(self, Self::HardFetchFailure | Self::AcceptanceConflict | Self::SegmentReadinessFailure)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsOriginProgressSnapshot {
    pub phase: HlsOriginProgressPhase,
    pub condition: HlsOriginPathCondition,
    pub target_duration_ms: u64,
    pub last_media_progress_at_ms: Option<u64>,
    pub session_recovery_required: bool,
    pub session_cutover_evaluation_required: bool,
    pub recovery_committed: bool,
    pub now_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsOriginProgressDecision {
    pub next_phase: HlsOriginProgressPhase,
    pub publication_age_ms: u64,
    pub publication_late_after_ms: u64,
    pub start_acceptance_episode: bool,
    pub close_admission: bool,
    pub evaluate_lease_cutovers: bool,
}

pub fn publication_late_after_ms(target_duration_ms: u64) -> u64 {
    let whole_target_durations = PUBLICATION_LATENESS_NUMERATOR / PUBLICATION_LATENESS_DENOMINATOR;
    let fractional_numerator = PUBLICATION_LATENESS_NUMERATOR % PUBLICATION_LATENESS_DENOMINATOR;
    target_duration_ms.saturating_mul(whole_target_durations).saturating_add(
        target_duration_ms.saturating_mul(fractional_numerator).div_ceil(PUBLICATION_LATENESS_DENOMINATOR),
    )
}

pub fn evaluate_origin_progress(snapshot: HlsOriginProgressSnapshot) -> HlsOriginProgressDecision {
    let publication_age_ms = snapshot.last_media_progress_at_ms.map_or(0, |last| snapshot.now_ms.saturating_sub(last));
    let publication_late_after_ms = publication_late_after_ms(snapshot.target_duration_ms);
    let publication_late =
        snapshot.last_media_progress_at_ms.is_some() && publication_age_ms >= publication_late_after_ms;
    let condition = if publication_late && snapshot.condition == HlsOriginPathCondition::ProgressExpected {
        HlsOriginPathCondition::PublicationLate
    } else {
        snapshot.condition
    };
    let recovery_required = condition.is_degraded() && snapshot.session_recovery_required;
    let critical =
        condition.is_degraded() && snapshot.session_cutover_evaluation_required && !snapshot.recovery_committed;
    let start_acceptance_episode =
        !snapshot.recovery_committed && (condition.opens_episode_immediately() || recovery_required);
    let next_phase = if snapshot.last_media_progress_at_ms.is_none() {
        HlsOriginProgressPhase::Cold
    } else if snapshot.recovery_committed || !condition.is_degraded() {
        HlsOriginProgressPhase::Fresh
    } else if critical {
        match snapshot.phase {
            HlsOriginProgressPhase::TerminalPartial | HlsOriginProgressPhase::Terminal => snapshot.phase,
            HlsOriginProgressPhase::Cold
            | HlsOriginProgressPhase::Fresh
            | HlsOriginProgressPhase::PublicationLate
            | HlsOriginProgressPhase::RecoveryRequired
            | HlsOriginProgressPhase::Recovering
            | HlsOriginProgressPhase::Critical => HlsOriginProgressPhase::Critical,
        }
    } else if recovery_required {
        if snapshot.phase == HlsOriginProgressPhase::Recovering {
            HlsOriginProgressPhase::Recovering
        } else {
            HlsOriginProgressPhase::RecoveryRequired
        }
    } else if publication_late {
        HlsOriginProgressPhase::PublicationLate
    } else {
        snapshot.phase
    };

    HlsOriginProgressDecision {
        next_phase,
        publication_age_ms,
        publication_late_after_ms,
        start_acceptance_episode,
        close_admission: !snapshot.recovery_committed && (recovery_required || critical),
        evaluate_lease_cutovers: critical,
    }
}

#[derive(Debug, Clone, Default)]
pub struct HlsBoundedRecoverySamples {
    samples_ms: VecDeque<u64>,
}

impl HlsBoundedRecoverySamples {
    pub const MAX_SAMPLES: usize = 32;

    pub fn record(&mut self, elapsed_ms: u64) {
        if self.samples_ms.len() == Self::MAX_SAMPLES {
            self.samples_ms.pop_front();
        }
        self.samples_ms.push_back(elapsed_ms);
    }

    pub fn p95_ms(&self) -> Option<u64> { self.percentile_ms(95) }

    pub fn p99_ms(&self) -> Option<u64> { self.percentile_ms(99) }

    pub fn latency_snapshot(&self) -> HlsObservedRecoveryLatency {
        HlsObservedRecoveryLatency {
            p95: self.p95_ms().map(HlsRecoveryEtaMs::from_millis),
            p99: self.p99_ms().map(HlsRecoveryEtaMs::from_millis),
        }
    }

    fn percentile_ms(&self, percentile: usize) -> Option<u64> {
        let mut samples = self.samples_ms.iter().copied().collect::<Vec<_>>();
        samples.sort_unstable();
        let rank = samples.len().saturating_mul(percentile).div_ceil(100).saturating_sub(1);
        samples.get(rank).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(now_ms: u64, reserve_ms: u64) -> HlsOriginProgressSnapshot {
        HlsOriginProgressSnapshot {
            phase: HlsOriginProgressPhase::Fresh,
            condition: HlsOriginPathCondition::ProgressExpected,
            target_duration_ms: 10_000,
            last_media_progress_at_ms: Some(0),
            session_recovery_required: reserve_ms <= 14_000,
            session_cutover_evaluation_required: reserve_ms <= 10_000,
            recovery_committed: false,
            now_ms,
        }
    }

    #[test]
    fn hls_recovery_timing_publication_lateness_without_reserve_pressure_is_only_a_signal() {
        let decision = evaluate_origin_progress(snapshot(15_000, 30_000));

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::PublicationLate);
        assert!(!decision.start_acceptance_episode);
        assert!(!decision.close_admission);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_publication_lateness_with_acceptance_boundary_starts_recovery() {
        let decision = evaluate_origin_progress(snapshot(15_000, 14_000));

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::RecoveryRequired);
        assert!(decision.start_acceptance_episode);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_hard_failure_can_start_acceptance_before_publication_lateness() {
        let mut input = snapshot(1_000, 30_000);
        input.condition = HlsOriginPathCondition::HardFetchFailure;

        let decision = evaluate_origin_progress(input);

        assert!(decision.start_acceptance_episode);
        assert_eq!(decision.next_phase, HlsOriginProgressPhase::Fresh);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_required_map_failure_with_sufficient_reserve_starts_recovery_without_cutover_pressure() {
        let mut input = snapshot(1_000, 30_000);
        input.condition = HlsOriginPathCondition::SegmentReadinessFailure;

        let decision = evaluate_origin_progress(input);

        assert!(decision.start_acceptance_episode);
        assert!(!decision.close_admission);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_cutover_policy_required_map_failure_at_playback_edge_reaches_cutover_pressure() {
        let mut input = snapshot(1_000, 10_000);
        input.condition = HlsOriginPathCondition::SegmentReadinessFailure;

        let decision = evaluate_origin_progress(input);

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::Critical);
        assert!(decision.start_acceptance_episode);
        assert!(decision.close_admission);
        assert!(decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_cutover_policy_cutover_is_only_evaluated_at_transition_margin() {
        let mut input = snapshot(15_000, 10_000);
        input.condition = HlsOriginPathCondition::RetryableFetchFailure;

        let decision = evaluate_origin_progress(input);

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::Critical);
        assert!(decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_publication_age_saturates_for_clock_regression() {
        let mut input = snapshot(5_000, 30_000);
        input.last_media_progress_at_ms = Some(6_000);

        assert_eq!(evaluate_origin_progress(input).publication_age_ms, 0);
    }

    #[test]
    fn hls_recovery_timing_publication_lateness_threshold_saturates_after_ratio_is_applied() {
        assert_eq!(publication_late_after_ms(u64::MAX), u64::MAX);
    }

    #[test]
    fn hls_recovery_timing_explicit_session_pressure_is_not_recomputed_from_raw_reserve() {
        let mut input = snapshot(15_000, u64::MAX);
        input.session_recovery_required = true;

        let decision = evaluate_origin_progress(input);

        assert!(decision.start_acceptance_episode);
        assert!(decision.close_admission);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_publication_lateness_without_lease_reserve_does_not_start_recovery() {
        let mut input = snapshot(15_000, 30_000);
        input.session_recovery_required = false;
        input.session_cutover_evaluation_required = false;

        let decision = evaluate_origin_progress(input);

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::PublicationLate);
        assert!(!decision.start_acceptance_episode);
        assert!(!decision.close_admission);
    }

    #[test]
    fn hls_recovery_timing_recovered_reserve_reopens_admission_when_only_publication_lateness_remains() {
        let mut input = snapshot(15_000, 30_000);
        input.phase = HlsOriginProgressPhase::Recovering;
        input.condition = HlsOriginPathCondition::PublicationLate;

        let decision = evaluate_origin_progress(input);

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::PublicationLate);
        assert!(!decision.start_acceptance_episode);
        assert!(!decision.close_admission);
        assert!(!decision.evaluate_lease_cutovers);
    }

    #[test]
    fn hls_recovery_timing_observe_episode_does_not_close_admission_without_reserve_pressure() {
        let mut input = snapshot(15_000, 30_000);
        input.phase = HlsOriginProgressPhase::Recovering;
        input.condition = HlsOriginPathCondition::PublicationLate;

        assert!(!evaluate_origin_progress(input).close_admission);
    }

    #[test]
    fn hls_recovery_timing_committed_recovery_reopens_admission_and_does_not_restart_episode() {
        let mut input = snapshot(15_000, 5_000);
        input.phase = HlsOriginProgressPhase::Recovering;
        input.condition = HlsOriginPathCondition::AcceptanceConflict;
        input.recovery_committed = true;

        let decision = evaluate_origin_progress(input);

        assert_eq!(decision.next_phase, HlsOriginProgressPhase::Fresh);
        assert!(!decision.start_acceptance_episode);
        assert!(!decision.close_admission);
    }

    #[test]
    fn hls_recovery_timing_bounded_recovery_samples_keep_latest_values_and_compute_percentiles() {
        let mut samples = HlsBoundedRecoverySamples::default();
        for value in 1..=40 {
            samples.record(value);
        }

        assert_eq!(samples.samples_ms.len(), HlsBoundedRecoverySamples::MAX_SAMPLES);
        assert_eq!(samples.samples_ms.front(), Some(&9));
        assert_eq!(samples.p95_ms(), Some(39));
        assert_eq!(samples.p99_ms(), Some(40));
        assert_eq!(
            samples.latency_snapshot(),
            HlsObservedRecoveryLatency {
                p95: Some(HlsRecoveryEtaMs::from_millis(39)),
                p99: Some(HlsRecoveryEtaMs::from_millis(40)),
            }
        );
    }
}
