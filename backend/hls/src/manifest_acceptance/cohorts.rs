use super::{
    timeline::{host_local_windows_compatible, same_host_timeline_compatible, strong_anchor_overlap},
    HlsAlternativeOriginCohort, HlsAlternativeOriginCohortIdentity, HlsAlternativeOriginWindow,
    HlsCandidateHostRelation, HlsCrossHostAcceptanceEvidence, HlsHostLocalSequenceRelation,
    HlsManifestAcceptanceLandscape, HlsManifestCandidateObservation, HlsManifestTechnicalSignature,
    HlsPinnedOriginObservationState, HlsReducedRetryLandscapeChange, HLS_MANIFEST_ACCEPTANCE_COHORT_SAMPLE_LIMIT,
};

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
