//! Scoring a candidate manifest against what the session already committed.
//!
//! Quality is the sequence relation (advanced, rollover, behind, unrelated) plus
//! the continuity mode the session is in, bounded by the forward window the
//! origin's target duration and the idle timeout allow.

use super::{
    current_time_millis,
    error::{HlsManifestAcceptanceRejectReason, HlsManifestRejectLogReason},
    fetched_effective_manifest_host, request_hls_session_idle_timeout_secs_from_config,
    selection_log::{log_manifest_recovery_candidate_rejected, log_manifest_recovery_candidate_scored},
    FetchedOriginManifest, HlsManifestCommitAcceptanceMode, HlsManifestContinuityMode, HlsManifestOriginQuality,
    HlsManifestOriginQualityScore, HlsManifestOriginRelation, HlsManifestRecoveryCandidateScoreReport,
    HlsManifestSequenceRelation, HlsOriginManifestFetchContext, DEFAULT_HLS_TARGET_DURATION_SECS,
};
use crate::{HlsAccountBindingProtection, HlsSessionMode};
use tuliprox_parser::hls::origin_manifest::{
    parse_manifest_timing, parse_origin_manifest_timeline, parse_origin_media_manifest, OriginManifestParseOutcome,
    ParsedOriginManifestTimeline,
};

pub async fn score_hls_manifest_candidate_for_selection_log(
    context: &HlsOriginManifestFetchContext,
    fetched: &FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Option<HlsManifestRecoveryCandidateScoreReport> {
    let session = context.session.read().await;
    let timeline = parse_manifest_timeline_for_recovery_scoring(&session, fetched).ok()?;
    Some(HlsManifestRecoveryCandidateScoreReport {
        media_sequence: timeline.origin_manifest_sequence,
        quality: evaluate_manifest_origin_quality_with_mode(
            &session,
            fetched,
            timeline,
            context,
            current_time_millis(),
            acceptance_mode,
        ),
    })
}

pub(super) async fn score_manifest_recovery_candidate_with_logging(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    fetched: &FetchedOriginManifest,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let score_result = {
        let session = context.session.read().await;
        score_hls_manifest_recovery_candidate_with_mode(&session, fetched, context, acceptance_mode)
    };
    match score_result {
        Ok(report) => {
            log_manifest_recovery_candidate_scored(context, candidate_index, candidates, &report).await;
            Ok(report)
        }
        Err(reason) => {
            log_manifest_recovery_candidate_rejected(
                context,
                candidate_index,
                candidates,
                fetched_effective_manifest_host(fetched).as_deref(),
                None,
                &reason,
            )
            .await;
            Err(reason)
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn score_hls_manifest_recovery_candidate(
    session: &crate::HlsSession,
    fetched: &FetchedOriginManifest,
    context: &HlsOriginManifestFetchContext,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    score_hls_manifest_recovery_candidate_with_mode(
        session,
        fetched,
        context,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
    )
}

fn score_hls_manifest_recovery_candidate_with_mode(
    session: &crate::HlsSession,
    fetched: &FetchedOriginManifest,
    context: &HlsOriginManifestFetchContext,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestRecoveryCandidateScoreReport, HlsManifestRejectLogReason> {
    let timeline = parse_manifest_timeline_for_recovery_scoring(session, fetched)?;
    let media_sequence = timeline.origin_manifest_sequence;
    Ok(HlsManifestRecoveryCandidateScoreReport {
        media_sequence,
        quality: evaluate_manifest_origin_quality_with_mode(
            session,
            fetched,
            timeline,
            context,
            current_time_millis(),
            acceptance_mode,
        ),
    })
}

fn parse_manifest_timeline_for_recovery_scoring(
    session: &crate::HlsSession,
    fetched: &FetchedOriginManifest,
) -> Result<ParsedOriginManifestTimeline, HlsManifestRejectLogReason> {
    if matches!(session.mode, HlsSessionMode::TransientPassthrough { .. }) {
        return parse_origin_manifest_timeline(&fetched.body)
            .map_err(|_| HlsManifestRejectLogReason::MalformedTransientTimeline);
    }
    match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) => Ok(ParsedOriginManifestTimeline {
            origin_manifest_sequence: manifest.origin_manifest_sequence,
            origin_manifest_segment_cnt: manifest.origin_manifest_segment_cnt,
        }),
        OriginManifestParseOutcome::TransientPassthrough { .. } => parse_origin_manifest_timeline(&fetched.body)
            .map_err(|_| HlsManifestRejectLogReason::MalformedTransientTimeline),
    }
}

pub fn evaluate_manifest_origin_quality_with_mode(
    session: &crate::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    context: &HlsOriginManifestFetchContext,
    now_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
) -> HlsManifestOriginQuality {
    let effective_host = fetched_effective_manifest_host(fetched);
    let fresh_baseline = matches!(acceptance_mode, HlsManifestCommitAcceptanceMode::FreshBaseline);
    let fresh_pinned_revalidation = matches!(acceptance_mode, HlsManifestCommitAcceptanceMode::FreshPinnedRevalidation);
    let host_relation = if fresh_baseline {
        if effective_host.is_some() {
            HlsManifestOriginRelation::Initial
        } else {
            HlsManifestOriginRelation::UnknownHost
        }
    } else {
        match (session.last_effective_manifest_host.as_deref(), effective_host.as_deref()) {
            (None, _) => HlsManifestOriginRelation::Initial,
            (_, None) => HlsManifestOriginRelation::UnknownHost,
            (Some(pinned), Some(effective)) if pinned == effective => HlsManifestOriginRelation::SameRedirectHost,
            (Some(_), Some(_)) => HlsManifestOriginRelation::OtherRedirectHost,
        }
    };
    let origin_highwater = timeline.origin_highwater();
    let previous_highwater = if fresh_baseline || host_relation == HlsManifestOriginRelation::OtherRedirectHost {
        None
    } else {
        session.origin_seq_highwater
    };
    let continuity_mode = if fresh_baseline || fresh_pinned_revalidation {
        HlsManifestContinuityMode::RebaseAllowed
    } else {
        manifest_continuity_mode(session, now_ms)
    };
    let allowed_forward_window = allowed_manifest_forward_window(session, context, Some(&fetched.body));
    let sequence_relation = if host_relation == HlsManifestOriginRelation::OtherRedirectHost {
        HlsManifestSequenceRelation::NoPreviousHighwater
    } else {
        classify_manifest_sequence_relation(
            previous_highwater,
            origin_highwater,
            allowed_forward_window,
            continuity_mode,
        )
    };
    let reject_reason =
        manifest_quality_reject_reason(sequence_relation, previous_highwater, origin_highwater, allowed_forward_window);
    let score = manifest_origin_quality_score(host_relation, sequence_relation, reject_reason.is_some());
    HlsManifestOriginQuality {
        score,
        host_relation,
        sequence_relation,
        effective_host,
        origin_highwater,
        previous_highwater,
        allowed_forward_window,
        requires_handoff_discontinuity: matches!(
            (host_relation, sequence_relation),
            (HlsManifestOriginRelation::OtherRedirectHost, _) | (_, HlsManifestSequenceRelation::RolloverCandidate)
        ),
        reject_reason,
    }
}

fn classify_manifest_sequence_relation(
    previous_highwater: Option<u64>,
    origin_highwater: Option<u64>,
    allowed_forward_window: Option<u64>,
    continuity_mode: HlsManifestContinuityMode,
) -> HlsManifestSequenceRelation {
    if matches!(continuity_mode, HlsManifestContinuityMode::RebaseAllowed) && origin_highwater.is_some() {
        return HlsManifestSequenceRelation::Rebase;
    }
    let Some(previous_highwater) = previous_highwater else {
        return HlsManifestSequenceRelation::NoPreviousHighwater;
    };
    let Some(origin_highwater) = origin_highwater else {
        return HlsManifestSequenceRelation::NoOriginHighwater;
    };
    if origin_highwater == previous_highwater {
        return HlsManifestSequenceRelation::Same;
    }
    if previous_highwater.checked_add(1) == Some(origin_highwater) {
        return HlsManifestSequenceRelation::Next;
    }
    if origin_highwater > previous_highwater {
        return if manifest_highwater_delta_within_window(
            origin_highwater.saturating_sub(previous_highwater),
            allowed_forward_window,
        ) {
            HlsManifestSequenceRelation::PlausibleForward
        } else {
            HlsManifestSequenceRelation::ForwardTooFar
        };
    }
    if origin_highwater_is_within_limit(origin_highwater, allowed_forward_window) {
        HlsManifestSequenceRelation::RolloverCandidate
    } else {
        HlsManifestSequenceRelation::Backward
    }
}

fn manifest_continuity_mode(session: &crate::HlsSession, now_ms: u64) -> HlsManifestContinuityMode {
    if session.origin_seq_highwater.is_none() {
        return HlsManifestContinuityMode::RebaseAllowed;
    }
    match session.account_binding_protection(now_ms) {
        HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired => {
            HlsManifestContinuityMode::RebaseAllowed
        }
        HlsAccountBindingProtection::HardActive { .. } | HlsAccountBindingProtection::SoftActive { .. } => {
            HlsManifestContinuityMode::StrictContinuity
        }
    }
}

fn manifest_highwater_delta_within_window(delta: u64, allowed_forward_window: Option<u64>) -> bool {
    allowed_forward_window.is_none_or(|window| delta <= window.max(1))
}

fn manifest_quality_reject_reason(
    sequence_relation: HlsManifestSequenceRelation,
    previous_highwater: Option<u64>,
    origin_highwater: Option<u64>,
    allowed_forward_window: Option<u64>,
) -> Option<HlsManifestAcceptanceRejectReason> {
    match sequence_relation {
        HlsManifestSequenceRelation::NoOriginHighwater => {
            Some(HlsManifestAcceptanceRejectReason::MissingOriginHighwater)
        }
        HlsManifestSequenceRelation::ForwardTooFar => Some(HlsManifestAcceptanceRejectReason::ForwardTooFar {
            previous: previous_highwater.unwrap_or_default(),
            origin: origin_highwater.unwrap_or_default(),
            window: allowed_forward_window,
        }),
        HlsManifestSequenceRelation::Backward => Some(HlsManifestAcceptanceRejectReason::BackwardOutsideRollover {
            previous: previous_highwater.unwrap_or_default(),
            origin: origin_highwater.unwrap_or_default(),
            window: allowed_forward_window,
        }),
        HlsManifestSequenceRelation::NoPreviousHighwater
        | HlsManifestSequenceRelation::Rebase
        | HlsManifestSequenceRelation::Same
        | HlsManifestSequenceRelation::Next
        | HlsManifestSequenceRelation::PlausibleForward
        | HlsManifestSequenceRelation::RolloverCandidate => None,
    }
}

fn manifest_origin_quality_score(
    host_relation: HlsManifestOriginRelation,
    sequence_relation: HlsManifestSequenceRelation,
    rejected: bool,
) -> HlsManifestOriginQualityScore {
    if rejected {
        return HlsManifestOriginQualityScore::Rejected;
    }
    let same_host =
        matches!(host_relation, HlsManifestOriginRelation::Initial | HlsManifestOriginRelation::SameRedirectHost);
    if !same_host {
        return HlsManifestOriginQualityScore::OtherHostCandidate;
    }
    match sequence_relation {
        HlsManifestSequenceRelation::Next => HlsManifestOriginQualityScore::SameHostNextSequence,
        HlsManifestSequenceRelation::Rebase => HlsManifestOriginQualityScore::SameHostRebase,
        HlsManifestSequenceRelation::NoPreviousHighwater | HlsManifestSequenceRelation::PlausibleForward => {
            HlsManifestOriginQualityScore::SameHostPlausibleForward
        }
        HlsManifestSequenceRelation::RolloverCandidate => HlsManifestOriginQualityScore::SameHostRolloverCandidate,
        HlsManifestSequenceRelation::Same => HlsManifestOriginQualityScore::SameHostUnchanged,
        HlsManifestSequenceRelation::NoOriginHighwater
        | HlsManifestSequenceRelation::ForwardTooFar
        | HlsManifestSequenceRelation::Backward => HlsManifestOriginQualityScore::Rejected,
    }
}

pub fn allowed_manifest_forward_window(
    session: &crate::HlsSession,
    context: &HlsOriginManifestFetchContext,
    body: Option<&str>,
) -> Option<u64> {
    let timing = body.map(parse_manifest_timing);
    let target_duration_secs = timing
        .and_then(|timing| timing.target_duration_ms.and_then(|duration_ms| u32::try_from(duration_ms / 1_000).ok()));
    origin_highwater_policy_limit(
        request_hls_session_idle_timeout_secs_from_config(&context.app_config),
        target_duration_secs.or(session.target_duration),
    )
}

pub fn origin_highwater_policy_limit(session_idle_timeout_secs: u64, target_duration_secs: Option<u32>) -> Option<u64> {
    let target_duration_secs = u64::from(target_duration_secs.unwrap_or(DEFAULT_HLS_TARGET_DURATION_SECS));
    if target_duration_secs == 0 {
        return None;
    }
    Some(session_idle_timeout_secs.div_ceil(target_duration_secs))
}

fn origin_highwater_is_within_limit(origin_highwater: u64, origin_highwater_limit: Option<u64>) -> bool {
    origin_highwater_limit.is_some_and(|limit| origin_highwater <= limit)
}

pub fn next_committed_origin_highwater(
    current_highwater: Option<u64>,
    origin_highwater: u64,
    sequence_relation: HlsManifestSequenceRelation,
) -> u64 {
    match sequence_relation {
        HlsManifestSequenceRelation::NoPreviousHighwater
        | HlsManifestSequenceRelation::Rebase
        | HlsManifestSequenceRelation::Next
        | HlsManifestSequenceRelation::PlausibleForward
        | HlsManifestSequenceRelation::RolloverCandidate => origin_highwater,
        HlsManifestSequenceRelation::NoOriginHighwater
        | HlsManifestSequenceRelation::Same
        | HlsManifestSequenceRelation::ForwardTooFar
        | HlsManifestSequenceRelation::Backward => {
            current_highwater.map_or(origin_highwater, |current| current.max(origin_highwater))
        }
    }
}
