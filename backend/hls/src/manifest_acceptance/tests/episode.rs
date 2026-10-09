use super::*;

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
            decision: super::super::super::timeline::HlsResourceReplayDecision::RejectContradictoryOrder,
            candidate_fingerprint: super::super::super::deterministic_conflict::HlsDeterministicConflictFingerprint {
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
