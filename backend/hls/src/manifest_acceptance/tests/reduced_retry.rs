use super::*;
use crate::manifest_acceptance::episode::HlsManifestAcceptanceEpisodeOutcome;
use shared::model::HlsManifestRecoveryBurstLevel;

fn selectable_episode() -> HlsManifestAcceptanceEpisode {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let mut episode = HlsManifestAcceptanceEpisode::new(
        HlsManifestAcceptanceGeneration(7),
        100,
        plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &episode_timing(100, plan),
    );
    episode.record_full_burst();
    episode.state = HlsManifestAcceptanceState::Committing;
    episode.observed_landscape =
        Some(manifest_acceptance_landscape(&[observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost)]));
    episode
}

fn identity(marker: usize) -> HlsManifestRecoveryCandidateIdentity {
    HlsManifestRecoveryCandidateIdentity::from_candidate(marker, Some("origin-a"), "manifest")
}

fn assert_rejected_unchanged(
    episode: &mut HlsManifestAcceptanceEpisode,
    generation: HlsManifestAcceptanceGeneration,
    candidate: HlsManifestRecoveryCandidateIdentity,
    expected: HlsRecoveryWorkloadBindingUpdate,
) {
    let before = episode.clone();
    assert_eq!(episode.select_candidate_for_reduced_retry(generation, candidate), expected);
    assert_eq!(*episode, before, "a rejected selection is atomic");
}

#[test]
fn reduced_retry_reopens_only_eligible_exhaustion_and_preserves_frozen_evidence() {
    for outcome in [
        HlsManifestAcceptanceEpisodeOutcome::Pending,
        HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(HlsManifestAcceptanceExhaustionReason::AllFailed),
        HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(HlsManifestAcceptanceExhaustionReason::NoProgress),
        HlsManifestAcceptanceEpisodeOutcome::FullBurstExhausted(
            HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
        ),
    ] {
        let mut episode = selectable_episode();
        episode.outcome = outcome;
        let generation = episode.generation;
        let before = episode.clone();
        if outcome != HlsManifestAcceptanceEpisodeOutcome::Pending {
            assert_eq!(
                episode.select_candidate(generation, identity(0)),
                HlsRecoveryWorkloadBindingUpdate::EpisodeInactive
            );
            assert_eq!(episode, before, "normal selection still rejects exhaustion");
        }
        let mut expected = before;
        expected.outcome = HlsManifestAcceptanceEpisodeOutcome::Pending;
        assert_eq!(expected.select_candidate(generation, identity(0)), HlsRecoveryWorkloadBindingUpdate::Applied);
        assert_eq!(
            episode.select_candidate_for_reduced_retry(generation, identity(0)),
            HlsRecoveryWorkloadBindingUpdate::Applied
        );
        assert_eq!(episode, expected, "only outcome and the selected identity may change");
    }
}

#[test]
fn reduced_retry_rejects_stale_generation_and_inactive_state_without_mutation() {
    let mut episode = selectable_episode();
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::NoProgress);
    let generation = episode.generation;
    assert_rejected_unchanged(
        &mut episode,
        HlsManifestAcceptanceGeneration(generation.0 + 1),
        identity(0),
        HlsRecoveryWorkloadBindingUpdate::StaleGeneration,
    );
    for state in [
        HlsManifestAcceptanceState::Holding,
        HlsManifestAcceptanceState::Collecting,
        HlsManifestAcceptanceState::Completed,
    ] {
        episode.state = state;
        assert_rejected_unchanged(
            &mut episode,
            generation,
            identity(0),
            HlsRecoveryWorkloadBindingUpdate::EpisodeInactive,
        );
    }
}

#[test]
fn reduced_retry_keeps_identity_and_bound_workload_guards_until_holding_reset() {
    let mut episode = selectable_episode();
    let generation = episode.generation;
    assert_eq!(episode.select_candidate(generation, identity(0)), HlsRecoveryWorkloadBindingUpdate::Applied);
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate);
    assert_rejected_unchanged(
        &mut episode,
        generation,
        identity(1),
        HlsRecoveryWorkloadBindingUpdate::CandidateMismatch,
    );

    assert_eq!(
        episode.select_candidate_for_reduced_retry(generation, identity(0)),
        HlsRecoveryWorkloadBindingUpdate::Applied
    );
    let workload = HlsRecoveryWorkload {
        burst: HlsRecoveryBurstWorkload::FullBurstCompleted,
        segment: HlsRecoverySegmentWorkload::ClearSegmentFetch,
        map: HlsRecoveryMapWorkload::NotRequired,
    };
    assert_eq!(
        episode.bind_selected_candidate(generation, identity(0), workload),
        HlsRecoveryWorkloadBindingUpdate::Applied
    );
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate);
    assert_rejected_unchanged(
        &mut episode,
        generation,
        identity(0),
        HlsRecoveryWorkloadBindingUpdate::CandidateMismatch,
    );
    assert_rejected_unchanged(
        &mut episode,
        generation,
        identity(1),
        HlsRecoveryWorkloadBindingUpdate::CandidateMismatch,
    );

    episode.hold_after_uncommitted_burst(None, Some(500));
    episode.state = HlsManifestAcceptanceState::Committing;
    assert_eq!(
        episode.select_candidate_for_reduced_retry(generation, identity(1)),
        HlsRecoveryWorkloadBindingUpdate::Applied
    );
    assert_eq!(episode.selected_candidate_identity(), Some(identity(1)));
}

#[test]
fn reduced_retry_never_reopens_incomplete_committed_or_deterministic_episodes() {
    let mut episode = selectable_episode();
    let generation = episode.generation;
    episode.full_burst_completed = false;
    assert_rejected_unchanged(&mut episode, generation, identity(0), HlsRecoveryWorkloadBindingUpdate::EpisodeInactive);
    episode.full_burst_completed = true;
    episode.complete();
    // Even an otherwise selectable state cannot reopen a committed outcome.
    episode.state = HlsManifestAcceptanceState::Committing;
    assert_rejected_unchanged(&mut episode, generation, identity(0), HlsRecoveryWorkloadBindingUpdate::EpisodeInactive);

    let resource_key = HlsMediaResourceIdentity::for_test(7).semantic_key();
    let conflict = HlsDeterministicTimelineConflict {
        previous_proxy_tail: Some(2),
        existing_proxy_seq: 0,
        candidate_position: 1,
        candidate_origin_seq: 491,
        resource_key,
        decision: crate::timeline::HlsResourceReplayDecision::RejectContradictoryOrder,
        candidate_fingerprint: crate::deterministic_conflict::HlsDeterministicConflictFingerprint {
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
    };
    episode = selectable_episode();
    episode.record_deterministic_conflict(HlsDeterministicConflictReceipt {
        conflict,
        origin_progress_generation: 1,
        published_resource_history_generation: 1,
        pinned_host_generation: 1,
    });
    episode.state = HlsManifestAcceptanceState::Committing;
    assert_rejected_unchanged(&mut episode, generation, identity(0), HlsRecoveryWorkloadBindingUpdate::EpisodeInactive);
    // A receipt remains authoritative independently of how another caller recorded exhaustion.
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::NoProgress);
    assert_rejected_unchanged(&mut episode, generation, identity(0), HlsRecoveryWorkloadBindingUpdate::EpisodeInactive);
    episode.deterministic_conflict_receipt = None;
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::DeterministicTimelineConflict);
    assert_rejected_unchanged(&mut episode, generation, identity(0), HlsRecoveryWorkloadBindingUpdate::EpisodeInactive);
}
