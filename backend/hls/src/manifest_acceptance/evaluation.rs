use super::{
    alternative_cohorts_with_history, evidence::HlsManifestWindowClass, HlsAlternativeOriginCohort,
    HlsCandidateHostRelation, HlsCommittedContentAnchorEvidence, HlsCrossHostAcceptanceEvidence,
    HlsHostLocalSequenceRelation, HlsManifestAcceptanceInput, HlsManifestAcceptanceTrigger,
    HlsManifestCandidateObservation, HlsManifestCommitKind, HlsManifestCommitPlan, HlsSwitchSegmentReadiness,
};

pub fn evaluate_manifest_acceptance(input: HlsManifestAcceptanceInput<'_>) -> HlsManifestCommitPlan {
    let pinned = preferred_pinned_candidate(input);
    if let Some(candidate) =
        pinned.filter(|candidate| candidate.timeline_fingerprint.window_class() == HlsManifestWindowClass::Regular)
    {
        return pinned_commit(candidate);
    }

    if !input.full_burst_completed {
        return if has_stageable_alternative(input.observations) {
            HlsManifestCommitPlan::HoldAlternative
        } else {
            HlsManifestCommitPlan::RejectAll
        };
    }

    let cohorts = alternative_cohorts_with_history(
        input.observations,
        input.previous_alternative,
        input.current_burst_is_full_plan,
    );
    // Exhaust all existing evidence rules for regular windows before considering
    // short fallbacks. Short observations remain available as timeline anchors.
    for window_class in [HlsManifestWindowClass::Regular, HlsManifestWindowClass::Short] {
        if let Some(initial) = initial_baseline_candidate(input.observations, window_class) {
            return pinned_commit(initial);
        }
        if let Some(candidate) = pinned.filter(|_| window_class == HlsManifestWindowClass::Short) {
            return pinned_commit(candidate);
        }
        if let Some(plan) = qualified_alternative_plan(input, &cohorts, window_class) {
            return plan;
        }
    }
    if cohorts.is_empty() {
        HlsManifestCommitPlan::RejectAll
    } else {
        HlsManifestCommitPlan::HoldAlternative
    }
}

fn qualified_alternative_plan(
    input: HlsManifestAcceptanceInput<'_>,
    cohorts: &[HlsAlternativeOriginCohort],
    window_class: HlsManifestWindowClass,
) -> Option<HlsManifestCommitPlan> {
    if let Some(cohort) = cohorts
        .iter()
        .filter(|cohort| cohort.window.fingerprint.window_class() == window_class)
        .find(|cohort| matches!(cohort.evidence, HlsCrossHostAcceptanceEvidence::StrongTimelineAnchor { .. }))
    {
        return Some(alternative_plan(
            input.observations,
            cohort.best_candidate_index,
            HlsManifestCommitKind::AnchoredAlternative,
        ));
    }

    if let Some(candidate) = input
        .current_burst_is_full_plan
        .then(|| {
            input
                .observations
                .iter()
                .filter(|candidate| candidate.host_relation == HlsCandidateHostRelation::OtherHost)
                .filter(|candidate| candidate.timeline_fingerprint.window_class() == window_class)
                .filter(|candidate| candidate.resource_timeline_evidence.permits_acceptance())
                .filter(|candidate| candidate.switch_segment_readiness.can_be_staged())
                .filter(|candidate| {
                    candidate.committed_content_anchor
                        == HlsCommittedContentAnchorEvidence::RequiresStagedByteVerification
                })
                .min_by_key(|candidate| (candidate.manifest_fetch_elapsed_ms, candidate.candidate_index))
        })
        .flatten()
    {
        return Some(alternative_plan(
            input.observations,
            candidate.candidate_index,
            HlsManifestCommitKind::ContentVerifiedAlternative,
        ));
    }

    if input.trigger.recovery_required() {
        if let Some(cohort) =
            cohorts.iter().filter(|cohort| cohort.window.fingerprint.window_class() == window_class).find(|cohort| {
                matches!(cohort.evidence, HlsCrossHostAcceptanceEvidence::BurstConsensusNewEpoch { .. })
                    || cohort.consecutive_confirmed_full_bursts >= 2
            })
        {
            return Some(alternative_plan(
                input.observations,
                cohort.best_candidate_index,
                HlsManifestCommitKind::AlternativeAsNewEpoch,
            ));
        }
    }

    if input.trigger == HlsManifestAcceptanceTrigger::Critical {
        if let Some(cohort) = critical_single_candidate_cohort(cohorts, input.observations)
            .filter(|cohort| cohort.window.fingerprint.window_class() == window_class)
        {
            return Some(alternative_plan(
                input.observations,
                cohort.best_candidate_index,
                HlsManifestCommitKind::EmergencyAlternativeAsNewEpoch,
            ));
        }
    }

    None
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
    full_burst_completed: bool,
) -> (Option<&HlsManifestCandidateObservation>, Option<&HlsManifestCandidateObservation>) {
    let mut progressed = None;
    let mut unchanged = None;
    for candidate in observations.iter().filter(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::PinnedHost
            && candidate.resource_timeline_evidence.permits_acceptance()
            && candidate.timeline_fingerprint.window_class() != HlsManifestWindowClass::Empty
            && (full_burst_completed
                || candidate.timeline_fingerprint.window_class() == HlsManifestWindowClass::Regular)
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
    left.timeline_fingerprint
        .window_class()
        .cmp(&right.timeline_fingerprint.window_class())
        .then_with(|| {
            pinned_priority(left.local_sequence_relation).cmp(&pinned_priority(right.local_sequence_relation))
        })
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

fn initial_baseline_candidate(
    observations: &[HlsManifestCandidateObservation],
    window_class: HlsManifestWindowClass,
) -> Option<&HlsManifestCandidateObservation> {
    observations
        .iter()
        .filter(|candidate| {
            candidate.host_relation == HlsCandidateHostRelation::InitialBaseline
                && candidate.resource_timeline_evidence.permits_acceptance()
                && candidate.timeline_fingerprint.window_class() == window_class
        })
        .min_by_key(|candidate| (candidate.manifest_fetch_elapsed_ms, candidate.candidate_index))
}

fn preferred_pinned_candidate(input: HlsManifestAcceptanceInput<'_>) -> Option<&HlsManifestCandidateObservation> {
    let (progressed_pinned, unchanged_pinned) = best_pinned_candidates(input.observations, input.full_burst_completed);
    let eligible_unchanged = (!input.trigger.recovery_required()).then_some(unchanged_pinned).flatten();
    progressed_pinned
        .filter(|candidate| candidate.timeline_fingerprint.window_class() == HlsManifestWindowClass::Regular)
        .or_else(|| {
            eligible_unchanged
                .filter(|candidate| candidate.timeline_fingerprint.window_class() == HlsManifestWindowClass::Regular)
        })
        .or(progressed_pinned)
        .or(eligible_unchanged)
}
