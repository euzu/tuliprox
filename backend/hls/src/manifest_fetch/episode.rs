//! The acceptance episode: one bounded attempt to get a manifest accepted.
//!
//! An episode opens when recovery starts, accumulates candidate observations and
//! the evidence the committed resources give, and closes either committed, held,
//! or exhausted. Everything here reads and writes that episode's state on the
//! session.

use super::{
    current_time_millis,
    fingerprint::build_manifest_timeline_fingerprint,
    recovery::{acceptance_attempt_may_start, HlsManifestRecoveryAttemptPlan, HlsManifestRecoveryCandidate},
    HlsManifestOriginRelation, HlsManifestSequenceRelation, HlsOriginManifestFetchContext,
    HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT,
};
use crate::{
    hls_origin_log_value,
    manifest_acceptance::{
        classify_host_local_sequence, HlsAlternativeOriginCohort, HlsCandidateHostRelation,
        HlsCommittedContentAnchorEvidence, HlsCrossHostAcceptanceEvidence, HlsEmergencyLiveHandoffCompatibility,
        HlsHostLocalSequenceRelation, HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceGeneration,
        HlsManifestAcceptanceLandscape, HlsManifestAcceptanceState, HlsManifestAcceptanceTrigger,
        HlsManifestCandidateObservation, HlsManifestTimelineFingerprint, HlsResourceTimelineEvidence,
        HlsSwitchSegmentReadiness,
    },
    recovery_timing::{
        HlsAcceptanceDeadlineMs, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput,
        HlsAcceptanceEpisodeTimingSeed, HlsRecoveryWorkloadEnvelope, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    },
    resource_identity::HlsMediaResourceIdentity,
};
use log::debug;
use shared::model::HlsManifestRecoveryBurstPlan;

pub(super) async fn begin_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> Option<HlsManifestAcceptanceGeneration> {
    let mut session = context.session.write().await;
    if plan.attempt_index != 0 {
        let episode = session.origin_control.acceptance_episode.as_mut()?;
        if episode.trigger() != plan.trigger || episode.state == HlsManifestAcceptanceState::Completed {
            return None;
        }
        episode.state = HlsManifestAcceptanceState::Collecting;
        return Some(episode.generation);
    }
    let started_at_ms = current_time_millis();
    let timing = acceptance_episode_timing(context, &session, started_at_ms, burst_plan);
    let generation = session.origin_control.begin_acceptance_episode(started_at_ms, burst_plan, plan.trigger, &timing);
    if let Some(episode) = session.origin_control.acceptance_episode.as_mut() {
        episode.state = HlsManifestAcceptanceState::Collecting;
        debug!(
            "HLS manifest acceptance full burst started: generation={} candidates={} max_stagger_ms={} binding_scheme={} binding_host={} provider_url_index={}",
            generation.0,
            episode.required_candidates(),
            episode.burst_max_stagger_ms(),
            plan.binding.request_url().scheme(),
            plan.binding
                .request_url()
                .host_str()
                .map_or_else(|| "none".to_string(), hls_origin_log_value),
            plan.binding
                .provider_url_index()
                .map_or_else(|| "none".to_string(), |index| index.to_string())
        );
    }
    Some(generation)
}

fn acceptance_episode_timing(
    context: &HlsOriginManifestFetchContext,
    session: &crate::HlsSession,
    started_at_ms: u64,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    let fallback_target_duration_ms = session
        .origin_control
        .target_duration_snapshot_ms
        .or_else(|| session.target_duration.map(|seconds| u64::from(seconds).saturating_mul(1_000)))
        .unwrap_or(15_000);
    let seed = context.acceptance_timing_seed.unwrap_or(HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: fallback_target_duration_ms,
        transition_margin: HlsTransitionMarginMs::from_millis(fallback_target_duration_ms),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    });
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: seed.target_duration_ms,
        transition_margin: seed.transition_margin,
        workload: seed.workload,
        observed_latency: session.origin_control.recovery_samples.latency_snapshot(),
        required_terminal_media_key: seed.required_terminal_media_key,
        terminal_media_preparation: seed.terminal_media_preparation,
        policy: context.recovery_timing_policy,
    })
}

#[derive(Debug, Clone)]
pub(super) struct HlsManifestAcceptanceEpisodeSnapshot {
    pub(super) trigger: HlsManifestAcceptanceTrigger,
    pub(super) full_burst_completed: bool,
    pub(super) held_alternative: Option<HlsAlternativeOriginCohort>,
    pub(super) observed_landscape: Option<HlsManifestAcceptanceLandscape>,
}

pub(super) async fn manifest_acceptance_episode_snapshot(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
) -> Option<HlsManifestAcceptanceEpisodeSnapshot> {
    let generation = generation?;
    let session = context.session.read().await;
    session.origin_control.acceptance_episode.as_ref().filter(|episode| episode.generation == generation).map(
        |episode| HlsManifestAcceptanceEpisodeSnapshot {
            trigger: episode.trigger(),
            full_burst_completed: episode.full_burst_completed,
            held_alternative: episode.held_alternative.clone(),
            observed_landscape: episode.observed_landscape.clone(),
        },
    )
}

pub(super) async fn record_manifest_acceptance_landscape(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    landscape: HlsManifestAcceptanceLandscape,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.observed_landscape = Some(landscape);
    }
}

pub(super) async fn begin_requalified_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let mut session = context.session.write().await;
    let now_ms = current_time_millis();
    if !acceptance_attempt_may_start(false, now_ms, 0, acceptance_deadline) {
        return false;
    }
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return false;
    };
    if episode.generation != generation
        || !episode.full_burst_completed
        || episode.state == HlsManifestAcceptanceState::Completed
    {
        return false;
    }
    episode.state = HlsManifestAcceptanceState::Holding;
    let configured_plan = context.manifest_recovery_burst.level.plan();
    let timing = acceptance_episode_timing(context, &session, now_ms, configured_plan);
    let next_generation = session.origin_control.begin_acceptance_episode(now_ms, configured_plan, trigger, &timing);
    debug!(
        "HLS manifest acceptance landscape changed: previous_generation={} next_generation={} candidates={} decision=full-requalification",
        generation.0,
        next_generation.0,
        configured_plan.total_candidates()
    );
    true
}

pub(super) async fn record_full_manifest_acceptance_burst(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    current_burst_is_full_plan: bool,
    completed_candidates: usize,
) {
    if !current_burst_is_full_plan {
        return;
    }
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_full_burst_candidates(completed_candidates);
    }
}

pub(super) async fn update_manifest_acceptance_episode_state(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    state: HlsManifestAcceptanceState,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.state = state;
    }
}

pub(super) async fn hold_uncommitted_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.hold_after_uncommitted_burst(held_alternative, Some(next_retry_at_ms));
        session.origin_control.path_condition = crate::origin_progress::HlsOriginPathCondition::AcceptanceConflict;
    }
}

pub(super) async fn complete_manifest_acceptance_episode(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.complete();
    }
}

pub(super) fn manifest_acceptance_exhaustion_reason(
    observations: &[HlsManifestCandidateObservation],
) -> HlsManifestAcceptanceExhaustionReason {
    if observations.is_empty() {
        return HlsManifestAcceptanceExhaustionReason::AllFailed;
    }
    if observations.iter().all(|candidate| {
        candidate.host_relation == HlsCandidateHostRelation::PinnedHost
            && matches!(
                candidate.local_sequence_relation,
                Some(HlsHostLocalSequenceRelation::Same | HlsHostLocalSequenceRelation::Backward)
            )
    }) {
        HlsManifestAcceptanceExhaustionReason::NoProgress
    } else {
        HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate
    }
}

pub(super) async fn record_manifest_acceptance_exhaustion(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    reason: HlsManifestAcceptanceExhaustionReason,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_exhaustion(reason);
    }
}

pub(super) fn manifest_candidate_observation(
    candidate: &HlsManifestRecoveryCandidate,
    burst_plan: HlsManifestRecoveryBurstPlan,
    committed_resource_identities: &[HlsMediaResourceIdentity],
    published_resource_identities: &[HlsMediaResourceIdentity],
) -> HlsManifestCandidateObservation {
    let host_relation = match candidate.report.quality.host_relation {
        HlsManifestOriginRelation::SameRedirectHost => HlsCandidateHostRelation::PinnedHost,
        HlsManifestOriginRelation::OtherRedirectHost => HlsCandidateHostRelation::OtherHost,
        HlsManifestOriginRelation::Initial => HlsCandidateHostRelation::InitialBaseline,
        HlsManifestOriginRelation::UnknownHost => HlsCandidateHostRelation::Unknown,
    };
    let (timeline_fingerprint, has_switch_segment, emergency_evidence) =
        build_manifest_timeline_fingerprint(&candidate.fetched.body, &candidate.fetched.final_manifest_url);
    let resource_timeline_evidence =
        candidate_resource_timeline_evidence(&timeline_fingerprint, published_resource_identities);
    let committed_content_anchor = timeline_fingerprint
        .segment_samples
        .first()
        .and_then(|segment| segment.normalized_resource_identity)
        .filter(|identity| committed_resource_identities.iter().any(|committed| committed.matches(*identity)))
        .filter(|_| {
            emergency_evidence.live_handoff == HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification
        })
        .map_or(HlsCommittedContentAnchorEvidence::Unavailable, |_| {
            HlsCommittedContentAnchorEvidence::RequiresStagedByteVerification
        });
    HlsManifestCandidateObservation {
        candidate_index: candidate.candidate_index,
        candidate_slot: burst_plan.slot_for_candidate(candidate.candidate_index),
        effective_host: candidate.report.quality.effective_host.clone(),
        host_relation,
        host_local_media_sequence: candidate.report.media_sequence,
        host_local_highwater: candidate.report.quality.origin_highwater,
        local_sequence_relation: matches!(
            host_relation,
            HlsCandidateHostRelation::PinnedHost | HlsCandidateHostRelation::InitialBaseline
        )
        .then(|| {
            classify_host_local_sequence(
                candidate.report.quality.previous_highwater,
                candidate.report.quality.origin_highwater,
                candidate.report.quality.allowed_forward_window.unwrap_or(1),
                candidate.report.quality.sequence_relation == HlsManifestSequenceRelation::Rebase,
            )
        })
        .flatten(),
        resource_timeline_evidence,
        timeline_fingerprint,
        manifest_fetch_elapsed_ms: candidate.fetch_elapsed_ms,
        switch_segment_readiness: if has_switch_segment {
            HlsSwitchSegmentReadiness::RequiresStaging
        } else {
            HlsSwitchSegmentReadiness::Unavailable
        },
        committed_content_anchor,
        emergency_evidence,
        evidence: HlsCrossHostAcceptanceEvidence::Insufficient,
    }
}

pub(super) struct HlsCommittedResourceEvidence {
    pub(super) ready_identities: Vec<HlsMediaResourceIdentity>,
    pub(super) published_identities: Vec<HlsMediaResourceIdentity>,
    pub(super) published_entries: Vec<(HlsMediaResourceIdentity, u64)>,
    pub(super) previous_proxy_tail: Option<u64>,
    pub(super) origin_progress_generation: u64,
    pub(super) published_resource_history_generation: u64,
    pub(super) pinned_host_generation: u64,
}

pub(super) async fn committed_resource_evidence(
    context: &HlsOriginManifestFetchContext,
) -> HlsCommittedResourceEvidence {
    let session = context.session.read().await;
    let ready_identities = session
        .segments
        .values()
        .rev()
        .filter(|entry| entry.origin_key.origin_epoch == session.origin_epoch)
        .filter(|entry| matches!(&entry.status, crate::SegmentCacheStatus::Ready { .. }))
        .filter_map(|entry| {
            let fetch_ref = entry.origin_fetch_ref.as_ref()?;
            Some(HlsMediaResourceIdentity::from_url(&fetch_ref.resolved_origin_url, fetch_ref.byte_range))
        })
        .take(HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT)
        .collect::<Vec<_>>();
    let published_entries = session.published_resource_history.recent_entries(usize::MAX).collect::<Vec<_>>();
    let published_identities = published_entries.iter().map(|(identity, _)| *identity).collect();
    HlsCommittedResourceEvidence {
        ready_identities,
        published_identities,
        published_entries,
        previous_proxy_tail: session.proxy_next_seq.and_then(|next| next.checked_sub(1)),
        origin_progress_generation: session.origin_control.progress_generation,
        published_resource_history_generation: session.published_resource_history.generation(),
        pinned_host_generation: session.origin_epoch,
    }
}

pub(super) fn candidate_resource_timeline_evidence(
    fingerprint: &HlsManifestTimelineFingerprint,
    published: &[HlsMediaResourceIdentity],
) -> HlsResourceTimelineEvidence {
    let mut saw_published = false;
    let mut saw_new = false;
    for identity in fingerprint.segment_samples.iter().filter_map(|segment| segment.normalized_resource_identity) {
        let was_published = published.iter().any(|existing| existing.matches(identity));
        if was_published && saw_new {
            return HlsResourceTimelineEvidence::ContradictoryOrder;
        }
        saw_published |= was_published;
        saw_new |= !was_published;
    }
    if saw_published && !saw_new {
        HlsResourceTimelineEvidence::ReplayOnly
    } else {
        HlsResourceTimelineEvidence::Eligible
    }
}
