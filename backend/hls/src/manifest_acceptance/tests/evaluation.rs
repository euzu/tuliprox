use super::*;

#[test]
fn forward_media_sequence_does_not_override_published_resource_replay() {
    let mut replay = observation(0, "origin-a", HlsCandidateHostRelation::PinnedHost);
    replay.local_sequence_relation = Some(HlsHostLocalSequenceRelation::PlausibleForward);
    replay.resource_timeline_evidence = HlsResourceTimelineEvidence::ReplayOnly;

    assert_eq!(evaluate(&[replay], HlsManifestAcceptanceTrigger::RecoveryRequired), HlsManifestCommitPlan::RejectAll);
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

    assert_eq!(evaluate(&observations, HlsManifestAcceptanceTrigger::Observe), HlsManifestCommitPlan::HoldAlternative);
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
    assert_eq!(evaluate(&[candidate], HlsManifestAcceptanceTrigger::Critical), HlsManifestCommitPlan::HoldAlternative);
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
