//! Turning a fetched origin manifest into committed session state.
//!
//! Acceptance is evaluated first (`evaluate_manifest_acceptance`), then the
//! manifest is committed down one of two paths: the normal path, which queues
//! and renders the visible window, and the transient path, which passes the
//! origin's own manifest through. Both end by recording the media progress and
//! the acceptance outcome on the session.

use super::{
    manifest_fetch_context,
    switch_staging::{
        ensure_staged_switch_media_compatible, handoff_preview_error, switch_staging_error,
        switch_staging_generation_matches, HlsStagedSwitchCommit,
    },
    timing::{
        build_manifest_refresh_timing, manifest_progress_from_highwater, HlsManifestProgress, HlsManifestRefreshTiming,
    },
    OriginRefreshRequest,
};
use crate::{
    hls_origin_log_value,
    initial_strip::initial_hls_strip_segments_for_durations,
    is_hls_provisioning_gap_segment, is_hls_provisioning_segment,
    manifest_fetch::{
        evaluate_manifest_origin_quality_with_mode, fetched_effective_manifest_host, next_committed_origin_highwater,
        FetchedOriginManifest, HlsManifestAcceptanceRejectReason, HlsManifestCommitAcceptanceMode,
        HlsManifestCommitError, HlsManifestOriginQuality, HlsManifestOriginRelation, HlsManifestRejectLogReason,
        HlsManifestSequenceRelation, HlsOriginManifestFetchContext,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    safe_proxy_session_id, safe_session_key,
    timeline::effective_origin_host_id,
    transient_manifest::{
        apply_transient_discontinuity_sequence, materialize_transient_provisioning_handoff_view,
        transient_discontinuity_sequence, transient_visible_discontinuity_count, TransientManifestRewriter,
        TransientRewriteOptions,
    },
    CacheAccessState, HlsManifestRenderer, HlsSessionMode, MapCacheStatus, RenderedManifestStoreOutcome,
    RenderedManifestStoreRejectReason, SegmentCacheStatus, TransientPassthroughReason, TransientResourceKind,
    TransientResourceRef,
};
use axum::http::HeaderMap;
use log::{debug, info, warn};
use shared::model::HlsStripMode;
use std::sync::Arc;
use tuliprox_core::model::StripConfig;
use tuliprox_parser::hls::origin_manifest::{
    parse_manifest_timing, parse_manifest_validity, parse_origin_manifest_timeline, parse_origin_media_manifest,
    OriginManifestParseOutcome, OriginManifestTransientReason, ParsedOriginManifest, ParsedOriginManifestTimeline,
};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
enum HlsManifestAcceptanceDecision {
    Accept { quality: HlsManifestOriginQuality },
    RetryCurrentTarget { quality: HlsManifestOriginQuality },
    AcceptHostSwitch { quality: HlsManifestOriginQuality },
    Reject { reason: HlsManifestAcceptanceRejectReason, quality: HlsManifestOriginQuality },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsManifestCommitProgressEvidence {
    CacheTimeline(HlsManifestRefreshTiming),
    Transient(HlsManifestRefreshTiming),
}

impl HlsManifestCommitProgressEvidence {
    pub(super) const fn refresh_timing(self) -> HlsManifestRefreshTiming {
        match self {
            Self::CacheTimeline(refresh_timing) | Self::Transient(refresh_timing) => refresh_timing,
        }
    }

    pub(super) fn success_bookkeeping_timing(self) -> HlsManifestRefreshTiming {
        let mut refresh_timing = self.refresh_timing();
        if matches!(self, Self::Transient(_)) {
            refresh_timing.progress = HlsManifestProgress::Unchanged;
        }
        refresh_timing
    }

    pub(super) const fn is_media_progress(self) -> bool {
        matches!(
            self,
            Self::CacheTimeline(HlsManifestRefreshTiming {
                progress: HlsManifestProgress::Advanced | HlsManifestProgress::Rollover,
                ..
            })
        )
    }
}

/// Pins one committed comparison object from selection through asynchronous
/// hash/track inspection so GC cannot invalidate acceptance evidence.
pub(super) struct HlsCommittedAcceptanceReadPin {
    pub(super) access: Arc<CacheAccessState>,
}

impl HlsCommittedAcceptanceReadPin {
    pub(super) fn acquire(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for HlsCommittedAcceptanceReadPin {
    fn drop(&mut self) { self.access.reader_finished(); }
}

pub(super) fn commit_fetched_manifest(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
) -> Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    commit_fetched_manifest_with_acceptance_mode(
        session,
        fetched,
        request,
        fetch_finished_at_ms,
        request.manifest_commit_requirement.acceptance_mode(),
        None,
    )
}

pub(super) fn commit_fetched_manifest_with_acceptance_mode(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    staged_switch: Option<&HlsStagedSwitchCommit>,
) -> Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    ensure_refresh_origin_work_generation_is_current(session, request)?;
    let existing_transient_reason = match &session.mode {
        HlsSessionMode::TransientPassthrough { reason } => Some(reason.clone()),
        HlsSessionMode::NormalCacheTimeline => None,
    };
    let parsed = parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url);
    let result = match (existing_transient_reason, parsed) {
        (None, OriginManifestParseOutcome::Normal(manifest)) => commit_normal_fetched_manifest(
            session,
            fetched,
            request,
            fetch_finished_at_ms,
            acceptance_mode,
            staged_switch,
            &manifest,
        ),
        (Some(reason), _) => commit_transient_fetched_manifest(
            session,
            fetched,
            request,
            fetch_finished_at_ms,
            acceptance_mode,
            reason,
            HlsTransientSwitchMetric::PreserveMode,
        ),
        (None, OriginManifestParseOutcome::TransientPassthrough { reason }) => commit_transient_fetched_manifest(
            session,
            fetched,
            request,
            fetch_finished_at_ms,
            acceptance_mode,
            map_transient_reason(reason),
            HlsTransientSwitchMetric::RecordSwitch,
        ),
    };
    if result.is_ok() {
        clear_satisfied_fresh_manifest_requirement(session, request);
    }
    let configured_target_duration = session.target_duration;
    if let Ok((progress_evidence, _, _)) = &result {
        record_committed_manifest_media_progress(
            &mut session.origin_control,
            *progress_evidence,
            configured_target_duration,
            fetch_finished_at_ms,
        );
    }
    result
}

pub(super) fn record_committed_manifest_media_progress(
    origin_control: &mut crate::session::HlsSessionOriginControl,
    evidence: HlsManifestCommitProgressEvidence,
    configured_target_duration_secs: Option<u32>,
    committed_at_ms: u64,
) {
    let refresh_timing = match evidence {
        HlsManifestCommitProgressEvidence::CacheTimeline(refresh_timing) => refresh_timing,
        HlsManifestCommitProgressEvidence::Transient(_) => return,
    };
    let target_duration_ms = match refresh_timing.progress {
        HlsManifestProgress::Advanced | HlsManifestProgress::Rollover => refresh_timing
            .target_duration_ms
            .or_else(|| configured_target_duration_secs.map(|duration| u64::from(duration).saturating_mul(1_000)))
            .unwrap_or(refresh_timing.base_interval_ms.saturating_mul(2)),
        HlsManifestProgress::Unchanged => return,
    };
    origin_control.record_media_progress(committed_at_ms, target_duration_ms);
}

fn commit_normal_fetched_manifest(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    staged_switch: Option<&HlsStagedSwitchCommit>,
    manifest: &ParsedOriginManifest,
) -> Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    let quality = evaluate_manifest_acceptance_for_commit(
        session,
        fetched,
        ParsedOriginManifestTimeline {
            origin_manifest_sequence: manifest.origin_manifest_sequence,
            origin_manifest_segment_cnt: manifest.origin_manifest_segment_cnt,
        },
        request,
        fetch_finished_at_ms,
        acceptance_mode,
    )?;
    let alternative_host = quality.host_relation == HlsManifestOriginRelation::OtherRedirectHost;
    if alternative_host != staged_switch.is_some() {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    }
    if !alternative_host {
        mark_manifest_handoff_discontinuity_if_needed(session, &quality);
    }
    let effective_host_id = fetched_effective_manifest_host(fetched).map_or(0, |host| effective_origin_host_id(&host));
    let refresh_timing = commit_normal_manifest(
        session,
        request,
        &HlsNormalManifestCommitInput {
            manifest,
            rendered_at_ms: fetch_finished_at_ms,
            sequence_relation: quality.sequence_relation,
            effective_host_id,
            staged_switch,
        },
    )?;
    update_origin_provider_session_headers(session, fetched);
    mark_manifest_acceptance_success(session, fetched, &quality, fetch_finished_at_ms);
    Ok((HlsManifestCommitProgressEvidence::CacheTimeline(refresh_timing), true, true))
}

#[derive(Clone, Copy)]
enum HlsTransientSwitchMetric {
    PreserveMode,
    RecordSwitch,
}

fn commit_transient_fetched_manifest(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    request: &OriginRefreshRequest,
    fetch_finished_at_ms: u64,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    reason: TransientPassthroughReason,
    switch_metric: HlsTransientSwitchMetric,
) -> Result<(HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    let timeline = parse_transient_manifest_timeline_for_commit(session, &fetched.body)?;
    let quality = evaluate_manifest_acceptance_for_commit(
        session,
        fetched,
        timeline,
        request,
        fetch_finished_at_ms,
        acceptance_mode,
    )?;
    if quality.host_relation == HlsManifestOriginRelation::OtherRedirectHost {
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    match switch_metric {
        HlsTransientSwitchMetric::PreserveMode => {}
        HlsTransientSwitchMetric::RecordSwitch => request.segment_worker_pool.metrics().record_transient_switch(),
    }
    mark_manifest_handoff_discontinuity_if_needed(session, &quality);
    let refresh_timing = commit_transient_manifest(
        session,
        HlsTransientManifestCommitInput {
            body: &fetched.body,
            final_manifest_url: &fetched.final_manifest_url,
            request_headers: &request.headers,
            reason,
            reverse_proxy_rewrite_secret: &request.reverse_proxy_rewrite_secret,
            transient_resource_ttl_ms: request.transient_resource_ttl_ms,
            rendered_at_ms: fetch_finished_at_ms,
            timeline,
            quality: &quality,
            strip: &request.strip,
        },
    );
    update_origin_provider_session_headers(session, fetched);
    mark_manifest_acceptance_success(session, fetched, &quality, fetch_finished_at_ms);
    Ok((HlsManifestCommitProgressEvidence::Transient(refresh_timing), false, false))
}

fn clear_satisfied_fresh_manifest_requirement(session: &mut crate::HlsSession, request: &OriginRefreshRequest) {
    let (Some(reason), Some(generation)) =
        (request.manifest_commit_requirement.fresh_reason(), request.fresh_manifest_requirement_generation)
    else {
        return;
    };
    session.clear_fresh_manifest_commit_requirement_if_current(reason, generation);
}

pub(super) fn refresh_origin_work_generation_is_current(
    session: &crate::HlsSession,
    request: &OriginRefreshRequest,
) -> bool {
    refresh_origin_work_generation_matches(
        session,
        request.origin_io.as_ref().and_then(|origin_io| origin_io.started_generation),
    )
}

pub(super) fn refresh_origin_work_generation_matches(
    session: &crate::HlsSession,
    started_generation: Option<u64>,
) -> bool {
    started_generation.is_none_or(|started_generation| started_generation == session.activity.origin_work_generation)
}

pub(super) fn ensure_refresh_origin_work_generation_is_current(
    session: &crate::HlsSession,
    request: &OriginRefreshRequest,
) -> Result<(), HlsManifestCommitError> {
    if refresh_origin_work_generation_is_current(session, request) {
        return Ok(());
    }
    debug!(
        "HLS origin manifest commit rejected: session={} reason=origin-work-generation-invalidated",
        safe_session_key(&session.key)
    );
    Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))
}

fn update_origin_provider_session_headers(session: &mut crate::HlsSession, fetched: &FetchedOriginManifest) {
    if !fetched.provider_session_headers.is_empty() {
        session.origin_provider_session_headers = fetched.provider_session_headers.clone();
    }
}

fn parse_transient_manifest_timeline_for_commit(
    session: &crate::HlsSession,
    body: &str,
) -> Result<ParsedOriginManifestTimeline, HlsManifestCommitError> {
    parse_origin_manifest_timeline(body).map_err(|reason| {
        warn!(
            "HLS origin manifest rejected: session={} proxy_session={} reason=malformed-transient-timeline error={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            hls_origin_log_value(format!("{reason:?}"))
        );
        HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::MalformedTransientTimeline }
    })
}

fn evaluate_manifest_acceptance_for_commit(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    request: &OriginRefreshRequest,
    now_ms: u64,
    mode: HlsManifestCommitAcceptanceMode,
) -> Result<HlsManifestOriginQuality, HlsManifestCommitError> {
    let fetch_context = manifest_fetch_context(request);
    match evaluate_manifest_acceptance(session, fetched, timeline, &fetch_context, now_ms, mode) {
        HlsManifestAcceptanceDecision::Accept { quality }
        | HlsManifestAcceptanceDecision::AcceptHostSwitch { quality } => Ok(quality),
        HlsManifestAcceptanceDecision::RetryCurrentTarget { .. } => Err(HlsManifestCommitError::RetryCurrentTarget),
        HlsManifestAcceptanceDecision::Reject { reason, .. } => {
            session.origin_control.path_condition = crate::origin_progress::HlsOriginPathCondition::AcceptanceConflict;
            let log_reason = HlsManifestRejectLogReason::from(reason.clone());
            warn!(
                "HLS origin manifest rejected: session={} proxy_session={} reason={} media_sequence={} segments={}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id),
                log_reason.status_label(),
                timeline.origin_manifest_sequence,
                timeline.origin_manifest_segment_cnt
            );
            Err(HlsManifestCommitError::TimelineRejected { reason: log_reason })
        }
    }
}

fn evaluate_manifest_acceptance(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    timeline: ParsedOriginManifestTimeline,
    fetch_context: &HlsOriginManifestFetchContext,
    now_ms: u64,
    mode: HlsManifestCommitAcceptanceMode,
) -> HlsManifestAcceptanceDecision {
    let quality = evaluate_manifest_origin_quality_with_mode(session, fetched, timeline, fetch_context, now_ms, mode);

    match quality.host_relation {
        HlsManifestOriginRelation::Initial
        | HlsManifestOriginRelation::SameRedirectHost
        | HlsManifestOriginRelation::UnknownHost => {
            if let Some(reason) = quality.reject_reason.clone() {
                return HlsManifestAcceptanceDecision::Reject { reason, quality };
            }
            HlsManifestAcceptanceDecision::Accept { quality }
        }
        HlsManifestOriginRelation::OtherRedirectHost => {
            if matches!(
                mode,
                HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
                    | HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate
                    | HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate
            ) {
                if let Some(reason) = quality.reject_reason.clone() {
                    return HlsManifestAcceptanceDecision::Reject { reason, quality };
                }
                return HlsManifestAcceptanceDecision::AcceptHostSwitch { quality };
            }

            HlsManifestAcceptanceDecision::RetryCurrentTarget { quality }
        }
    }
}

fn mark_manifest_handoff_discontinuity_if_needed(session: &mut crate::HlsSession, quality: &HlsManifestOriginQuality) {
    if !quality.requires_handoff_discontinuity {
        return;
    }
    if matches!(quality.host_relation, HlsManifestOriginRelation::OtherRedirectHost) {
        session.mark_pending_origin_epoch_handoff_discontinuity(0);
    } else if session.pending_handoff_discontinuity_sequence.is_none() {
        session.mark_pending_handoff_discontinuity(0);
    }
}

fn mark_manifest_acceptance_success(
    session: &mut crate::HlsSession,
    fetched: &FetchedOriginManifest,
    quality: &HlsManifestOriginQuality,
    observed_at_ms: u64,
) {
    let binding = Url::parse(&fetched.resolved_request_url)
        .ok()
        .and_then(|request_url| HlsManifestOriginBinding::new(request_url, fetched.provider_url_index).ok());
    if let Some(binding) = binding {
        session.origin_control.manifest_origin_binding = Some(binding);
    } else {
        session.origin_control.manifest_origin_binding = None;
        warn!(
            "HLS accepted manifest has no concrete recovery binding: session={} proxy_session={} reason=invalid-resolved-request-url",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id)
        );
    }
    if let Some(effective_host) = fetched_effective_manifest_host(fetched) {
        session.last_effective_manifest_host = Some(effective_host.clone());
        session.origin_control.pinned_host = Some(effective_host.clone());
        session.origin_control.origin_epoch = session.origin_epoch;
        if let Some(highwater) = quality.origin_highwater {
            session.origin_control.record_host_local_highwater(
                session.origin_epoch,
                effective_host,
                highwater,
                observed_at_ms,
            );
        }
    }
}

struct HlsTransientManifestCommitInput<'a> {
    body: &'a str,
    final_manifest_url: &'a str,
    request_headers: &'a HeaderMap,
    reason: TransientPassthroughReason,
    reverse_proxy_rewrite_secret: &'a [u8],
    transient_resource_ttl_ms: u64,
    rendered_at_ms: u64,
    timeline: ParsedOriginManifestTimeline,
    quality: &'a HlsManifestOriginQuality,
    strip: &'a StripConfig,
}

fn commit_transient_manifest(
    session: &mut crate::HlsSession,
    input: HlsTransientManifestCommitInput<'_>,
) -> HlsManifestRefreshTiming {
    let HlsTransientManifestCommitInput {
        body,
        final_manifest_url,
        request_headers,
        reason,
        reverse_proxy_rewrite_secret,
        transient_resource_ttl_ms,
        rendered_at_ms,
        timeline,
        quality,
        strip,
    } = input;
    let was_normal = matches!(session.mode, HlsSessionMode::NormalCacheTimeline);
    let reason_log_fields = transient_reason_log_fields(&reason);
    session.mode = HlsSessionMode::TransientPassthrough { reason };
    if was_normal {
        info!(
            "HLS session switched to transient passthrough: session={} proxy_session={} {}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            reason_log_fields
        );
    }
    session.origin_request_headers = request_headers.clone();
    session.transient.set_resource_ttl_ms(transient_resource_ttl_ms);
    session.transient.prune_expired(rendered_at_ms);

    let previous_provisioning_manifest_body = session
        .last_rendered_manifest
        .as_ref()
        .filter(|_| session.segments.values().any(is_hls_provisioning_segment))
        .map(|rendered| rendered.body.clone());
    let provisioning_segment_duration_ms = session
        .segments
        .values()
        .find(|entry| is_hls_provisioning_segment(entry) || is_hls_provisioning_gap_segment(entry))
        .map_or(2_000, |entry| entry.duration_ms);
    let handoff_discontinuity_sequence = session.take_pending_handoff_discontinuity_sequence();
    let mut rewritten = if handoff_discontinuity_sequence.is_some() {
        TransientManifestRewriter::rewrite_with_options(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
            TransientRewriteOptions { handoff_discontinuity_sequence },
        )
    } else {
        TransientManifestRewriter::rewrite(
            body,
            final_manifest_url,
            &session.proxy_session_id,
            reverse_proxy_rewrite_secret,
            rendered_at_ms,
            transient_resource_ttl_ms,
        )
    };
    if handoff_discontinuity_sequence.is_some() {
        if let Some(handoff_body) = materialize_transient_provisioning_handoff_view(
            &rewritten.body,
            previous_provisioning_manifest_body.as_deref(),
            strip,
            provisioning_segment_duration_ms,
        ) {
            rewritten.body = handoff_body;
        }
        let current_discontinuity_sequence = transient_discontinuity_sequence(&rewritten.body)
            .unwrap_or(session.transient_discontinuity_sequence.unwrap_or(0));
        session.transient_discontinuity_sequence =
            Some(current_discontinuity_sequence.saturating_add(transient_visible_discontinuity_count(&rewritten.body)));
    } else if let Some(discontinuity_sequence) = session.transient_discontinuity_sequence {
        rewritten.body = apply_transient_discontinuity_sequence(&rewritten.body, discontinuity_sequence);
    }
    let manifest_validity = parse_manifest_validity(&rewritten.body);
    session.transient.upsert_resources(rewritten.resources);
    if let Some(validity) = manifest_validity {
        session.transient.replace_manifest_with_validity(rewritten.body, rendered_at_ms, validity.playlist_duration_ms);
    } else {
        session.transient.replace_manifest(rewritten.body, rendered_at_ms);
    }
    let previous_highwater = session.origin_seq_highwater;
    if let Some(highwater) = timeline.origin_highwater() {
        session.origin_seq_highwater =
            Some(next_committed_origin_highwater(session.origin_seq_highwater, highwater, quality.sequence_relation));
    }

    let timing = parse_manifest_timing(body);
    if let Some(target_duration_ms) = timing.target_duration_ms {
        if let Ok(target_duration_secs) = u32::try_from(target_duration_ms / 1_000) {
            session.target_duration = Some(target_duration_secs);
        }
    }
    let target_duration_ms =
        timing.target_duration_ms.or_else(|| session.target_duration.map(|duration| u64::from(duration) * 1_000));
    let progress =
        manifest_progress_from_highwater(previous_highwater, session.origin_seq_highwater, quality.sequence_relation);
    build_manifest_refresh_timing(timing.last_segment_duration_ms, target_duration_ms, progress)
}

struct HlsNormalManifestCommitInput<'a> {
    manifest: &'a ParsedOriginManifest,
    rendered_at_ms: u64,
    sequence_relation: HlsManifestSequenceRelation,
    effective_host_id: u64,
    staged_switch: Option<&'a HlsStagedSwitchCommit>,
}

fn commit_normal_manifest(
    session: &mut crate::HlsSession,
    request: &OriginRefreshRequest,
    input: &HlsNormalManifestCommitInput<'_>,
) -> Result<HlsManifestRefreshTiming, HlsManifestCommitError> {
    let rendered_at_ms = input.rendered_at_ms;
    let sequence_relation = input.sequence_relation;
    let effective_host_id = input.effective_host_id;
    let staged_switch = input.staged_switch;
    let mut manifest = input.manifest.clone();
    let key_resources = materialize_normal_key_resources(
        &mut manifest,
        &request.reverse_proxy_rewrite_secret,
        rendered_at_ms,
        request.transient_resource_ttl_ms,
    );
    let manifest = &manifest;
    let previous_highwater = session.origin_seq_highwater;
    let provisioning_handoff = session.pending_handoff_discontinuity_sequence.is_some()
        && session.segments.values().any(is_hls_provisioning_segment);
    let segment_durations = manifest.segments.iter().map(|segment| segment.duration_ms).collect::<Vec<_>>();
    let initial_prefetch_gap_segments = initial_hls_strip_segments_for_durations(&request.strip, &segment_durations);
    if let Some(staged_switch) = staged_switch {
        if staged_switch.effective_host_id != effective_host_id
            || !switch_staging_generation_matches(session, &staged_switch.generation)
        {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        let Some(first_segment) = staged_switch.preview.segments.first() else {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        };
        ensure_staged_switch_media_compatible(session, first_segment, &key_resources, rendered_at_ms)?;
        session
            .apply_origin_handoff_manifest_if_preview_matches(manifest, effective_host_id, 0, &staged_switch.preview)
            .map_err(handoff_preview_error)?;
        let Some(segment) = session.segments.get_mut(&staged_switch.ready_segment_proxy_seq) else {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        };
        segment.status = SegmentCacheStatus::Ready {
            content_length: staged_switch.ready_segment_content_length,
            ready_at_ms: rendered_at_ms,
        };
        if let Some((map_id, content_length)) = staged_switch.ready_map {
            let Some(map) = session.maps.get_mut(&map_id) else {
                return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
            };
            map.status = MapCacheStatus::Ready { content_length, ready_at_ms: rendered_at_ms };
        }
        session.advance_media_readiness_generation();
    } else {
        let apply_result = if sequence_relation == HlsManifestSequenceRelation::Rebase {
            session.apply_origin_rebase_manifest(manifest, effective_host_id)
        } else {
            session.apply_origin_manifest_for_host(manifest, effective_host_id)
        };
        apply_result.map_err(|err| HlsManifestCommitError::TimelineRejected {
            reason: HlsManifestRejectLogReason::from(err),
        })?;
    }
    session.transient.upsert_resources(key_resources);
    session.initial_prefetch_gap_segments = initial_prefetch_gap_segments;
    if provisioning_handoff {
        limit_publishable_normal_provisioning_handoff_tail(session, &request.strip, manifest.segments.len());
    }
    queue_and_render_normal_manifest(session, request, rendered_at_ms);

    let last_segment_duration_ms = manifest.segments.last().map(|segment| segment.duration_ms);
    let target_duration_ms = manifest.target_duration.map(|duration| u64::from(duration) * 1_000);
    let progress = if staged_switch.is_some() {
        HlsManifestProgress::Advanced
    } else {
        manifest_progress_from_highwater(previous_highwater, session.origin_seq_highwater, sequence_relation)
    };
    Ok(build_manifest_refresh_timing(last_segment_duration_ms, target_duration_ms, progress))
}

fn queue_and_render_normal_manifest(
    session: &mut crate::HlsSession,
    request: &OriginRefreshRequest,
    rendered_at_ms: u64,
) {
    session.origin_request_headers.clone_from(&request.headers);
    session.queue_map_fetch_candidates(rendered_at_ms);
    let backpressure = request.segment_worker_pool.classify_backpressure_for_session(session);
    let queue_report = session.queue_manifest_fetch_candidates(rendered_at_ms, backpressure.allows_prefetch());
    request.segment_worker_pool.metrics().record_prefetch_queued(queue_report.prefetch_queued);
    request.segment_worker_pool.metrics().record_prefetch_skipped(queue_report.prefetch_skipped);
    if queue_report.prefetch_queued > 0 {
        debug!(
            "HLS segment queued for prefetch: session={} proxy_session={} count={}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_queued
        );
    }
    if queue_report.prefetch_skipped > 0 {
        debug!(
            "HLS segment queued for prefetch skipped by backpressure: session={} proxy_session={} count={} state={backpressure:?}",
            safe_session_key(&session.key),
            safe_proxy_session_id(&session.proxy_session_id),
            queue_report.prefetch_skipped
        );
    }
    match HlsManifestRenderer::render(session, rendered_at_ms) {
        Ok(rendered) => {
            let segment_count = rendered.segment_proxy_seqs.len();
            let render_gap_segments = rendered.render_gap_segments;
            let media_sequence = rendered.first_proxy_seq;
            match session.store_rendered_manifest(rendered) {
                RenderedManifestStoreOutcome::Stored => {
                    request.segment_worker_pool.metrics().record_manifest_rendered();
                    info!(
                        "HLS manifest rendered: session={} proxy_session={} media_sequence={} segments={} render_gap_segments={}",
                        safe_session_key(&session.key),
                        safe_proxy_session_id(&session.proxy_session_id),
                        media_sequence,
                        segment_count,
                        render_gap_segments
                    );
                }
                RenderedManifestStoreOutcome::Rejected(
                    RenderedManifestStoreRejectReason::RegressiveMediaSequence {
                        previous_first_proxy_seq,
                        candidate_first_proxy_seq,
                    },
                ) => {
                    request.segment_worker_pool.metrics().record_manifest_render_skipped();
                    debug!(
                        "HLS manifest render rejected: session={} proxy_session={} reason=regressive-media-sequence previous={} candidate={}",
                        safe_session_key(&session.key),
                        safe_proxy_session_id(&session.proxy_session_id),
                        previous_first_proxy_seq,
                        candidate_first_proxy_seq
                    );
                }
                RenderedManifestStoreOutcome::Rejected(RenderedManifestStoreRejectReason::DuplicateMediaResource {
                    existing_proxy_seq,
                    candidate_proxy_seq,
                }) => {
                    request.segment_worker_pool.metrics().record_manifest_render_skipped();
                    debug!(
                        "HLS manifest render rejected: session={} proxy_session={} reason=duplicate-media-resource existing_proxy_seq={} candidate_proxy_seq={}",
                        safe_session_key(&session.key),
                        safe_proxy_session_id(&session.proxy_session_id),
                        existing_proxy_seq,
                        candidate_proxy_seq
                    );
                }
            }
        }
        Err(err) => {
            request.segment_worker_pool.metrics().record_manifest_render_skipped();
            debug!(
                "HLS manifest render skipped: session={} proxy_session={} reason={err:?}",
                safe_session_key(&session.key),
                safe_proxy_session_id(&session.proxy_session_id)
            );
        }
    }
}

pub(super) fn materialize_normal_key_resources(
    manifest: &mut ParsedOriginManifest,
    reverse_proxy_rewrite_secret: &[u8],
    now_ms: u64,
    ttl_ms: u64,
) -> Vec<TransientResourceRef> {
    let mut resources = Vec::new();
    for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
        let extension = key_resource_extension(&encryption.resolved_origin_uri);
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            encryption.resolved_origin_uri.clone(),
            reverse_proxy_rewrite_secret,
            now_ms,
            ttl_ms,
            Some(extension.clone()),
        );
        encryption.proxy_resource_id = Some(resource.id.0.clone());
        encryption.proxy_resource_extension = Some(extension);
        resources.push(resource);
    }
    resources
}

pub(super) fn key_resource_extension(resolved_origin_uri: &str) -> String {
    Url::parse(resolved_origin_uri)
        .ok()
        .and_then(|url| url.path_segments()?.next_back()?.rsplit_once('.').map(|(_, extension)| extension.to_string()))
        .map(|extension| extension.to_ascii_lowercase())
        .filter(|extension| matches!(extension.as_str(), "key" | "bin"))
        .unwrap_or_else(|| "key".to_string())
}

fn limit_publishable_normal_provisioning_handoff_tail(
    session: &mut crate::HlsSession,
    strip: &StripConfig,
    origin_segment_count: usize,
) {
    let Some(gap_seq) = session
        .segments
        .iter()
        .filter_map(|(proxy_seq, entry)| is_hls_provisioning_gap_segment(entry).then_some(*proxy_seq))
        .max()
    else {
        return;
    };
    let first_origin_seq = gap_seq.saturating_add(1);
    if !session.segments.contains_key(&first_origin_seq) {
        return;
    }
    if let Some(head_seq) = visible_provisioning_handoff_head_proxy_seq(session) {
        session.publishable_origin_head_proxy_seq = Some(head_seq);
    }
    let initial_origin_segments = configured_handoff_origin_window_segments(strip, origin_segment_count).clamp(1, 3);
    let tail_seq =
        first_origin_seq.saturating_add(u64::try_from(initial_origin_segments.saturating_sub(1)).unwrap_or(0));
    if session.segments.contains_key(&tail_seq) {
        session.publishable_origin_tail_proxy_seq = Some(tail_seq);
    }
}

fn visible_provisioning_handoff_head_proxy_seq(session: &crate::HlsSession) -> Option<u64> {
    session
        .last_rendered_manifest
        .as_ref()
        .and_then(|rendered| {
            rendered
                .segment_proxy_seqs
                .iter()
                .copied()
                .find(|proxy_seq| session.segments.get(proxy_seq).is_some_and(is_hls_provisioning_segment))
        })
        .or_else(|| {
            session
                .segments
                .iter()
                .filter_map(|(proxy_seq, entry)| is_hls_provisioning_segment(entry).then_some(*proxy_seq))
                .min()
        })
}

fn configured_handoff_origin_window_segments(strip: &StripConfig, origin_segment_count: usize) -> usize {
    match strip.mode {
        HlsStripMode::Segments => usize::try_from(strip.value).unwrap_or(usize::MAX),
        HlsStripMode::Seconds => 0,
    }
    .saturating_add(3)
    .min(origin_segment_count)
}

fn map_transient_reason(reason: OriginManifestTransientReason) -> TransientPassthroughReason {
    match reason {
        OriginManifestTransientReason::ExtXKey => TransientPassthroughReason::ExtXKey,
        OriginManifestTransientReason::UnsupportedTag { tag } => TransientPassthroughReason::UnsupportedTag { tag },
        OriginManifestTransientReason::ParserUnsupportedFeature { feature } => {
            TransientPassthroughReason::ParserUnsupportedFeature { feature }
        }
    }
}

pub(super) fn transient_reason_log_fields(reason: &TransientPassthroughReason) -> String {
    match reason {
        TransientPassthroughReason::ExtXKey => "reason=ext_x_key".to_string(),
        TransientPassthroughReason::UnsupportedTag { tag } => format!("reason=unsupported_tag tag={tag}"),
        TransientPassthroughReason::ParserUnsupportedFeature { feature } => {
            format!("reason=parser_unsupported_feature feature={feature}")
        }
    }
}
