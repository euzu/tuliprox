use super::{
    manifest_acceptance::HlsManifestAcceptanceEpisodeStatus,
    media_reserve::HlsLeaseReserveSnapshot,
    recovery_timing::{
        HlsTerminalCommitWindow, HlsTerminalMediaPreparationDisposition, HlsTerminalMediaPreparationKey,
        HlsTerminalMediaPreparationState,
    },
    terminal_tail::HlsTerminalTailCompatibility,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsTerminalCutoverCapability {
    NotEvaluated,
    TailCompatible { prepared_key: HlsTerminalMediaPreparationKey },
    TailUnavailable(HlsTerminalTailCompatibility),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsTerminalCutoverInput {
    pub reserve: HlsLeaseReserveSnapshot,
    pub commit_window: HlsTerminalCommitWindow,
    pub acceptance: HlsManifestAcceptanceEpisodeStatus,
    pub required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub terminal_preparation: HlsTerminalMediaPreparationState,
    pub terminal: HlsTerminalCutoverCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsTerminalCutoverDecision {
    NotRequired,
    RetrySupersededSnapshot,
    EvaluateTerminalCapability {
        preparation: HlsTerminalMediaPreparationDisposition,
    },
    CommitTerminalTail,
    CommitTerminalUnavailable(HlsTerminalTailCompatibility),
}

/// Decides terminal publication from one immutable lease/session snapshot.
///
/// A cutover is authorized only by real media reserve and a current snapshot.
/// Acceptance lifecycle states never stand in for media progress: once the
/// the bounded terminal-acquisition window opens, missing, active, exhausted,
/// and committed episodes all continue to terminal-capability evaluation. A structurally
/// superseded snapshot is retried so reserve can be evaluated again.
/// Publication lateness, request counts,
/// recovery ETA, hard operation timeouts, and generic refresh activity are
/// deliberately absent. Pre-cutover scheduling owns all waits; once the
/// acquisition window opens, neither recovery nor terminal-media preparation
/// may postpone the terminal decision beyond the unchanged safe deadline.
pub(crate) fn evaluate_terminal_cutover(input: &HlsTerminalCutoverInput) -> HlsTerminalCutoverDecision {
    match input.commit_window {
        HlsTerminalCommitWindow::NotDue => return HlsTerminalCutoverDecision::NotRequired,
        HlsTerminalCommitWindow::AcquisitionOpen | HlsTerminalCommitWindow::CutoverDue => {}
    }

    match input.acceptance {
        HlsManifestAcceptanceEpisodeStatus::Superseded { .. } => HlsTerminalCutoverDecision::RetrySupersededSnapshot,
        HlsManifestAcceptanceEpisodeStatus::Missing
        | HlsManifestAcceptanceEpisodeStatus::Committed { .. }
        | HlsManifestAcceptanceEpisodeStatus::InFlight { .. }
        | HlsManifestAcceptanceEpisodeStatus::Expired { .. }
        | HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. } => terminal_decision_for_preparation(input),
    }
}

fn terminal_decision_for_preparation(input: &HlsTerminalCutoverInput) -> HlsTerminalCutoverDecision {
    let preparation = input.terminal_preparation.disposition_for(input.required_terminal_media_key);
    terminal_decision(
        input.terminal,
        preparation,
        input.terminal_preparation,
        input.required_terminal_media_key,
    )
}

fn terminal_decision(
    capability: HlsTerminalCutoverCapability,
    preparation: HlsTerminalMediaPreparationDisposition,
    preparation_state: HlsTerminalMediaPreparationState,
    required_key: Option<HlsTerminalMediaPreparationKey>,
) -> HlsTerminalCutoverDecision {
    match capability {
        HlsTerminalCutoverCapability::NotEvaluated => {
            HlsTerminalCutoverDecision::EvaluateTerminalCapability { preparation }
        }
        HlsTerminalCutoverCapability::TailCompatible { prepared_key }
            if preparation_state.authorizes_prepared_key(required_key, prepared_key) =>
        {
            HlsTerminalCutoverDecision::CommitTerminalTail
        }
        HlsTerminalCutoverCapability::TailCompatible { .. } => HlsTerminalCutoverDecision::CommitTerminalUnavailable(
            HlsTerminalTailCompatibility::AssetRevisionMismatch,
        ),
        HlsTerminalCutoverCapability::TailUnavailable(reason) => {
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            manifest_acceptance::{HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceGeneration},
            media_reserve::HlsLeaseReserveAvailabilityBasis,
            recovery_timing::{HlsTerminalMediaPreparationKey, HlsTransitionMarginMs},
            terminal_tail::HlsTerminalAssetIdentity,
        },
        *,
    };

    const GENERATION: HlsManifestAcceptanceGeneration = HlsManifestAcceptanceGeneration(4);
    const TERMINAL_KEY: HlsTerminalMediaPreparationKey = HlsTerminalMediaPreparationKey {
        asset: HlsTerminalAssetIdentity { revision: 9, fingerprint: [9; 32] },
        target_duration_ms: 10_000,
        segment_count: 12,
    };

    fn degraded_reserve(guaranteed_reserve_ms: u64) -> HlsLeaseReserveSnapshot {
        let transition_margin = HlsTransitionMarginMs::from_millis(10_000);
        HlsLeaseReserveSnapshot {
            availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
            guaranteed_media_horizon_ms: guaranteed_reserve_ms,
            conservative_playback_position_ms: 0,
            guaranteed_reserve_ms,
            initial_hidden_ready_duration_ms: 0,
            transition_margin,
            key_readiness_valid_until_ms: None,
            recovery_required: true,
            cutover_required: guaranteed_reserve_ms <= transition_margin.as_millis(),
        }
    }

    fn input(acceptance: HlsManifestAcceptanceEpisodeStatus) -> HlsTerminalCutoverInput {
        HlsTerminalCutoverInput {
            reserve: degraded_reserve(10_000),
            commit_window: HlsTerminalCommitWindow::CutoverDue,
            acceptance,
            required_terminal_media_key: Some(TERMINAL_KEY),
            terminal_preparation: HlsTerminalMediaPreparationState::Preparing { key: TERMINAL_KEY },
            terminal: HlsTerminalCutoverCapability::TailCompatible { prepared_key: TERMINAL_KEY },
        }
    }

    fn exhausted() -> HlsManifestAcceptanceEpisodeStatus {
        HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted {
            generation: GENERATION,
            reason: HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
        }
    }

    #[test]
    fn hls_cutover_policy_full_burst_exhaustion_requires_terminal_capability() {
        let mut undecided = input(exhausted());
        undecided.terminal = HlsTerminalCutoverCapability::NotEvaluated;

        assert_eq!(
            evaluate_terminal_cutover(&undecided),
            HlsTerminalCutoverDecision::EvaluateTerminalCapability {
                preparation: HlsTerminalMediaPreparationDisposition::Preparing,
            }
        );
        assert_eq!(evaluate_terminal_cutover(&input(exhausted())), HlsTerminalCutoverDecision::CommitTerminalTail);
    }

    #[test]
    fn hls_cutover_policy_missing_episode_evaluates_terminal_capability_at_margin() {
        let mut missing = input(HlsManifestAcceptanceEpisodeStatus::Missing);
        missing.terminal = HlsTerminalCutoverCapability::NotEvaluated;

        assert_eq!(
            evaluate_terminal_cutover(&missing),
            HlsTerminalCutoverDecision::EvaluateTerminalCapability {
                preparation: HlsTerminalMediaPreparationDisposition::Preparing,
            }
        );
    }

    #[test]
    fn hls_cutover_policy_matching_in_flight_recovery_never_waits_after_cutover_is_required() {
        let mut in_flight = input(HlsManifestAcceptanceEpisodeStatus::InFlight { generation: GENERATION });
        in_flight.terminal = HlsTerminalCutoverCapability::NotEvaluated;

        assert_eq!(
            evaluate_terminal_cutover(&in_flight),
            HlsTerminalCutoverDecision::EvaluateTerminalCapability {
                preparation: HlsTerminalMediaPreparationDisposition::Preparing,
            }
        );
    }

    #[test]
    fn hls_cutover_policy_terminal_preparation_never_waits_after_cutover_is_required() {
        let mut preparing = input(exhausted());
        preparing.terminal = HlsTerminalCutoverCapability::NotEvaluated;
        preparing.terminal_preparation = HlsTerminalMediaPreparationState::Preparing { key: TERMINAL_KEY };

        assert_eq!(
            evaluate_terminal_cutover(&preparing),
            HlsTerminalCutoverDecision::EvaluateTerminalCapability {
                preparation: HlsTerminalMediaPreparationDisposition::Preparing,
            }
        );
    }

    #[test]
    fn hls_cutover_policy_terminal_preparation_states_are_explicit_at_cutover() {
        let mut at_cutover = input(exhausted());
        at_cutover.terminal = HlsTerminalCutoverCapability::NotEvaluated;

        for terminal_preparation in [
            HlsTerminalMediaPreparationState::Preparing { key: TERMINAL_KEY },
            HlsTerminalMediaPreparationState::Ready { key: TERMINAL_KEY },
            HlsTerminalMediaPreparationState::Ready {
                key: HlsTerminalMediaPreparationKey { target_duration_ms: 9_000, ..TERMINAL_KEY },
            },
            HlsTerminalMediaPreparationState::Incompatible { key: Some(TERMINAL_KEY) },
            HlsTerminalMediaPreparationState::Failed { key: Some(TERMINAL_KEY) },
        ] {
            let mut evaluated = at_cutover;
            evaluated.terminal_preparation = terminal_preparation;
            assert_eq!(
                evaluate_terminal_cutover(&evaluated),
                HlsTerminalCutoverDecision::EvaluateTerminalCapability {
                    preparation: terminal_preparation.disposition_for(Some(TERMINAL_KEY)),
                }
            );
        }
    }

    #[test]
    fn hls_cutover_policy_compatible_tail_requires_exact_preparation_key() {
        let mut mismatch = input(exhausted());
        mismatch.terminal_preparation = HlsTerminalMediaPreparationState::Ready { key: TERMINAL_KEY };
        mismatch.terminal = HlsTerminalCutoverCapability::TailCompatible {
            prepared_key: HlsTerminalMediaPreparationKey { target_duration_ms: 9_000, ..TERMINAL_KEY },
        };

        assert_eq!(
            evaluate_terminal_cutover(&mismatch),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            )
        );
    }

    #[test]
    fn hls_cutover_policy_two_leases_keep_independent_representable_reserve_states() {
        let near = input(exhausted());
        let mut far = input(exhausted());
        far.reserve = degraded_reserve(10_001);
        far.commit_window = HlsTerminalCommitWindow::NotDue;

        assert!(near.reserve.cutover_required);
        assert!(!far.reserve.cutover_required);
        assert_eq!(evaluate_terminal_cutover(&near), HlsTerminalCutoverDecision::CommitTerminalTail);
        assert_eq!(evaluate_terminal_cutover(&far), HlsTerminalCutoverDecision::NotRequired);
    }

    #[test]
    fn hls_cutover_policy_acquisition_window_can_commit_before_the_unchanged_margin() {
        let mut acquisition = input(exhausted());
        acquisition.reserve = degraded_reserve(10_001);
        acquisition.commit_window = HlsTerminalCommitWindow::AcquisitionOpen;

        assert!(!acquisition.reserve.cutover_required);
        assert_eq!(evaluate_terminal_cutover(&acquisition), HlsTerminalCutoverDecision::CommitTerminalTail);
    }

    #[test]
    fn hls_cutover_policy_matching_acceptance_commit_without_progress_still_cuts_over() {
        assert_eq!(
            evaluate_terminal_cutover(&input(HlsManifestAcceptanceEpisodeStatus::Committed { generation: GENERATION })),
            HlsTerminalCutoverDecision::CommitTerminalTail
        );
    }

    #[test]
    fn hls_cutover_policy_explicitly_superseded_snapshot_requests_fresh_reserve() {
        assert_eq!(
            evaluate_terminal_cutover(&input(HlsManifestAcceptanceEpisodeStatus::Superseded {
                generation: HlsManifestAcceptanceGeneration(3),
                current_generation: GENERATION,
            })),
            HlsTerminalCutoverDecision::RetrySupersededSnapshot
        );
    }

    #[test]
    fn hls_cutover_policy_fresh_reserve_above_transition_margin_does_not_cut_over() {
        let mut above_margin = input(exhausted());
        above_margin.reserve = degraded_reserve(10_001);
        above_margin.commit_window = HlsTerminalCommitWindow::NotDue;

        assert_eq!(evaluate_terminal_cutover(&above_margin), HlsTerminalCutoverDecision::NotRequired);
    }

    #[test]
    fn hls_cutover_policy_incompatible_tail_produces_explicit_terminal_outcome() {
        let mut incompatible = input(exhausted());
        incompatible.terminal = HlsTerminalCutoverCapability::TailUnavailable(
            HlsTerminalTailCompatibility::UnsupportedEncryptionTransition,
        );

        assert_eq!(
            evaluate_terminal_cutover(&incompatible),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(
                HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
            )
        );
    }
}
