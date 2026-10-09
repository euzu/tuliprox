use super::*;
use crate::recovery_timing::{
    HlsAcceptanceEpisodeTimingInput, HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsRecoveryBurstWorkload,
    HlsRecoveryMapWorkload, HlsRecoverySegmentWorkload, HlsRecoveryTimingPolicy, HlsTerminalMediaPreparationState,
    HlsTransitionMarginMs,
};

fn episode_timing(started_at_ms: u64, burst_plan: HlsManifestRecoveryBurstPlan) -> HlsAcceptanceEpisodeTiming {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: 4_000,
        transition_margin: HlsTransitionMarginMs::from_millis(1_000),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsOperationTimeoutMs::from_millis(2_000),
            HlsRecoveryEtaMs::from_millis(300),
            HlsRecoveryEtaMs::from_millis(400),
        ),
    })
}

fn segment(marker: u8, pdt_ms: Option<i64>) -> HlsManifestSegmentFingerprint {
    HlsManifestSegmentFingerprint {
        duration_ms: 4_000,
        discontinuity_before: false,
        program_date_time_ms: pdt_ms,
        normalized_resource_identity: Some(HlsMediaResourceIdentity::for_test(marker)),
    }
}

fn fingerprint(marker: u8) -> HlsManifestTimelineFingerprint {
    HlsManifestTimelineFingerprint {
        segment_count: 3,
        first_program_date_time_ms: None,
        last_program_date_time_ms: None,
        duration_pattern_hash: [marker; 32],
        discontinuity_pattern_hash: [0; 32],
        normalized_resource_pattern_hash: Some([marker; 32]),
        map_and_encryption_hash: [0; 32],
        container_signature_hash: [1; 32],
        segment_samples: vec![
            segment(marker, None),
            segment(marker.saturating_add(1), None),
            segment(marker.saturating_add(2), None),
        ],
    }
}

fn observation(index: usize, host: &str, relation: HlsCandidateHostRelation) -> HlsManifestCandidateObservation {
    HlsManifestCandidateObservation {
        candidate_index: index,
        candidate_slot: index / 2,
        effective_host: Some(host.to_string()),
        host_relation: relation,
        host_local_media_sequence: 5,
        host_local_highwater: Some(7),
        local_sequence_relation: (relation == HlsCandidateHostRelation::PinnedHost)
            .then_some(HlsHostLocalSequenceRelation::Next),
        resource_timeline_evidence: HlsResourceTimelineEvidence::Eligible,
        timeline_fingerprint: fingerprint(1),
        manifest_fetch_elapsed_ms: 10,
        switch_segment_readiness: HlsSwitchSegmentReadiness::RequiresStaging,
        committed_content_anchor: HlsCommittedContentAnchorEvidence::Unavailable,
        emergency_evidence: HlsEmergencyAcceptanceEvidence::INCOMPATIBLE,
        evidence: HlsCrossHostAcceptanceEvidence::Insufficient,
    }
}

fn evaluate(
    observations: &[HlsManifestCandidateObservation],
    trigger: HlsManifestAcceptanceTrigger,
) -> HlsManifestCommitPlan {
    evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
        full_burst_completed: true,
        current_burst_is_full_plan: true,
        trigger,
        previous_alternative: None,
        observations,
    })
}

fn mark_emergency_verification_eligible(candidate: &mut HlsManifestCandidateObservation) {
    candidate.emergency_evidence = HlsEmergencyAcceptanceEvidence {
        live_handoff: HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification,
        terminal_alternative: HlsTerminalAlternativeCompatibility::RequiresStagedComparison,
    };
}

fn sliding_observation(
    index: usize,
    host: &str,
    media_sequence: u64,
    markers: [u8; 3],
) -> HlsManifestCandidateObservation {
    let mut candidate = observation(index, host, HlsCandidateHostRelation::OtherHost);
    candidate.host_local_media_sequence = media_sequence;
    candidate.host_local_highwater = Some(media_sequence.saturating_add(2));
    candidate.timeline_fingerprint.segment_samples = markers.map(|marker| segment(marker, None)).to_vec();
    candidate
}

mod episode;
mod evaluation;
