use super::{
    recovery_timing::{HlsRecoveryTriggerBudgetMs, HlsTransitionMarginMs},
    terminal_tail::{HlsEncryptionSignature, HlsMapSignature, HlsMediaContainer},
};
use std::{collections::BTreeSet, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsLeaseManifestSegment {
    pub proxy_seq: u64,
    pub duration_ms: u64,
    pub uri: String,
    pub discontinuity_before: bool,
    pub map_ref_ready: bool,
    pub encryption: Option<HlsEncryptionSignature>,
}

/// Identifies which delivery path produced the exact manifest advertised to a lease.
///
/// Transient passthrough media has no READY-backed shared-cache timeline. Keeping that
/// distinction in the immutable snapshot prevents reserve and terminal policies from
/// accidentally treating origin-backed resources as cached media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsManifestDeliveryMode {
    NormalCacheTimeline,
    TransientPassthrough,
}

/// Immutable ordering marker of the shared manifest render used to build a lease snapshot.
///
/// This is deliberately distinct from the lease-local snapshot generation. Concurrent
/// requests may publish snapshots from different shared renders, while every successful
/// lease publication receives its own monotonically increasing snapshot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HlsManifestSourceRenderMarker(u64);

impl HlsManifestSourceRenderMarker {
    pub(crate) const fn new(rendered_at_ms: u64) -> Self { Self(rendered_at_ms) }

    #[cfg(test)]
    pub(crate) const fn rendered_at_ms(self) -> u64 { self.0 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsLeaseManifestSnapshot {
    pub delivery_mode: HlsManifestDeliveryMode,
    pub source_render_marker: HlsManifestSourceRenderMarker,
    pub snapshot_generation: u64,
    pub delivered_at_ms: u64,
    pub first_proxy_seq: u64,
    pub last_proxy_seq: u64,
    pub visible_segments: Arc<[HlsLeaseManifestSegment]>,
    pub discontinuity_sequence: u64,
    pub target_duration_ms: u64,
    pub playlist_duration_ms: u64,
    pub last_visible_media_end_ms: u64,
    pub active_map: Option<HlsMapSignature>,
    pub active_encryption: Option<HlsEncryptionSignature>,
    pub container: HlsMediaContainer,
}

impl HlsLeaseManifestSnapshot {
    pub(crate) fn visible_proxy_seqs(&self) -> impl Iterator<Item = u64> + '_ {
        self.visible_segments.iter().map(|segment| segment.proxy_seq)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HlsLeasePlaybackCursor {
    pub first_requested_proxy_seq: Option<u64>,
    pub first_segment_completed_at_ms: Option<u64>,
    pub highest_contiguous_completed_proxy_seq: Option<u64>,
    pub last_requested_proxy_seq: Option<u64>,
    pub last_request_at_ms: Option<u64>,
    pub cursor_generation: u64,
    request_epoch: u64,
    highest_requested_proxy_seq: Option<u64>,
    out_of_order_completed_proxy_seqs: BTreeSet<u64>,
}

const MAX_OUT_OF_ORDER_COMPLETIONS: usize = 64;

/// Identifies a full-segment request within the cursor epoch in which it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct HlsPlaybackRequestToken {
    proxy_seq: u64,
    request_epoch: u64,
}

/// Result of committing a fully delivered segment to a playback cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum HlsPlaybackCompletionOutcome {
    Advanced,
    Buffered,
    Duplicate,
    BeforeCurrentPosition,
    StaleRequest,
}

impl HlsLeasePlaybackCursor {
    /// Earliest sequence whose duration is needed to align the current manifest
    /// window with this lease's measured playback position.
    pub(crate) fn ready_timeline_start_proxy_seq(&self, manifest_first_proxy_seq: u64) -> u64 {
        self.first_requested_proxy_seq.map_or(manifest_first_proxy_seq, |first_requested_proxy_seq| {
            first_requested_proxy_seq.min(manifest_first_proxy_seq)
        })
    }

    /// Records a full-segment request; dropping the returned token models an aborted response.
    pub(crate) fn record_request_started(&mut self, proxy_seq: u64, requested_at_ms: u64) -> HlsPlaybackRequestToken {
        let forward_seek =
            self.highest_requested_proxy_seq.is_some_and(|highest| proxy_seq > highest.saturating_add(1));
        if forward_seek {
            self.request_epoch = self.request_epoch.saturating_add(1);
            self.first_requested_proxy_seq = Some(proxy_seq);
            self.first_segment_completed_at_ms = None;
            self.highest_contiguous_completed_proxy_seq = None;
            self.highest_requested_proxy_seq = Some(proxy_seq);
            self.out_of_order_completed_proxy_seqs.clear();
        } else {
            self.first_requested_proxy_seq.get_or_insert(proxy_seq);
            self.highest_requested_proxy_seq =
                Some(self.highest_requested_proxy_seq.map_or(proxy_seq, |highest| highest.max(proxy_seq)));
        }
        self.last_requested_proxy_seq = Some(proxy_seq);
        self.last_request_at_ms = Some(
            self.last_request_at_ms
                .map_or(requested_at_ms, |last_request_at_ms| last_request_at_ms.max(requested_at_ms)),
        );
        self.cursor_generation = self.cursor_generation.saturating_add(1);

        HlsPlaybackRequestToken { proxy_seq, request_epoch: self.request_epoch }
    }

    /// Commits a token only after the corresponding response body was delivered completely.
    pub(crate) fn record_request_completed(
        &mut self,
        token: HlsPlaybackRequestToken,
        completed_at_ms: u64,
    ) -> HlsPlaybackCompletionOutcome {
        if token.request_epoch != self.request_epoch {
            return HlsPlaybackCompletionOutcome::StaleRequest;
        }
        let Some(first_requested_proxy_seq) = self.first_requested_proxy_seq else {
            return HlsPlaybackCompletionOutcome::StaleRequest;
        };
        if token.proxy_seq < first_requested_proxy_seq {
            return HlsPlaybackCompletionOutcome::BeforeCurrentPosition;
        }
        if self.highest_contiguous_completed_proxy_seq.is_some_and(|highest| token.proxy_seq <= highest)
            || self.out_of_order_completed_proxy_seqs.contains(&token.proxy_seq)
        {
            return HlsPlaybackCompletionOutcome::Duplicate;
        }

        let next_expected_proxy_seq = self
            .highest_contiguous_completed_proxy_seq
            .and_then(|highest| highest.checked_add(1))
            .unwrap_or(first_requested_proxy_seq);
        if token.proxy_seq != next_expected_proxy_seq {
            self.buffer_out_of_order_completion(token.proxy_seq);
            self.cursor_generation = self.cursor_generation.saturating_add(1);
            return HlsPlaybackCompletionOutcome::Buffered;
        }

        if self.first_segment_completed_at_ms.is_none() {
            self.first_segment_completed_at_ms = Some(completed_at_ms);
        }
        self.highest_contiguous_completed_proxy_seq = Some(token.proxy_seq);
        self.advance_over_buffered_completions();
        self.cursor_generation = self.cursor_generation.saturating_add(1);
        HlsPlaybackCompletionOutcome::Advanced
    }

    fn buffer_out_of_order_completion(&mut self, proxy_seq: u64) {
        self.out_of_order_completed_proxy_seqs.insert(proxy_seq);
        if self.out_of_order_completed_proxy_seqs.len() <= MAX_OUT_OF_ORDER_COMPLETIONS {
            return;
        }
        if let Some(furthest_proxy_seq) = self.out_of_order_completed_proxy_seqs.last().copied() {
            self.out_of_order_completed_proxy_seqs.remove(&furthest_proxy_seq);
        }
    }

    fn advance_over_buffered_completions(&mut self) {
        while let Some(next_proxy_seq) =
            self.highest_contiguous_completed_proxy_seq.and_then(|highest| highest.checked_add(1))
        {
            if !self.out_of_order_completed_proxy_seqs.remove(&next_proxy_seq) {
                break;
            }
            self.highest_contiguous_completed_proxy_seq = Some(next_proxy_seq);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsReadyMediaState {
    Ready,
    NotReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsReadyTimelineUnit {
    pub proxy_seq: u64,
    pub start_ms: u64,
    pub duration_ms: u64,
    pub state: HlsReadyMediaState,
    pub required_map_ready: bool,
    pub required_key_ready: bool,
    pub key_ready_valid_until_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HlsReadyTimelineSnapshot {
    pub units: Arc<[HlsReadyTimelineUnit]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsLeaseReserveSnapshot {
    pub availability_basis: HlsLeaseReserveAvailabilityBasis,
    pub guaranteed_media_horizon_ms: u64,
    pub conservative_playback_position_ms: u64,
    pub guaranteed_reserve_ms: u64,
    pub initial_hidden_ready_duration_ms: u64,
    pub transition_margin: HlsTransitionMarginMs,
    pub key_readiness_valid_until_ms: Option<u64>,
    pub recovery_required: bool,
    pub cutover_required: bool,
}

/// Typed source of guaranteed reserve evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsLeaseReserveAvailabilityBasis {
    ReadyCacheTimeline,
    TransientOriginBacked,
}

#[derive(Clone, Copy)]
pub(crate) struct HlsLeaseReserveInput<'a> {
    pub manifest: &'a HlsLeaseManifestSnapshot,
    pub cursor: &'a HlsLeasePlaybackCursor,
    pub ready_timeline: &'a HlsReadyTimelineSnapshot,
    pub now_ms: u64,
    pub playback_rate_guard_milli: u16,
    pub recovery_trigger_budget: HlsRecoveryTriggerBudgetMs,
    pub origin_path_degraded: bool,
    pub recovery_committed: bool,
}

pub(crate) fn evaluate_lease_reserve(input: HlsLeaseReserveInput<'_>) -> HlsLeaseReserveSnapshot {
    if input.manifest.delivery_mode == HlsManifestDeliveryMode::TransientPassthrough {
        let transition_margin = HlsTransitionMarginMs::from_millis(input.manifest.target_duration_ms);
        return HlsLeaseReserveSnapshot {
            availability_basis: HlsLeaseReserveAvailabilityBasis::TransientOriginBacked,
            guaranteed_media_horizon_ms: 0,
            conservative_playback_position_ms: 0,
            guaranteed_reserve_ms: 0,
            initial_hidden_ready_duration_ms: 0,
            transition_margin,
            key_readiness_valid_until_ms: None,
            recovery_required: input.origin_path_degraded,
            cutover_required: input.origin_path_degraded && !input.recovery_committed,
        };
    }
    let guaranteed_ready_span = contiguous_ready_span(input.ready_timeline, input.manifest);
    let visible_tail = aligned_visible_tail_ms(input.ready_timeline, input.manifest);
    let guaranteed_media_horizon_ms = guaranteed_ready_span.horizon_ms;
    let conservative_playback_position_ms =
        conservative_playback_position(&input, visible_tail, guaranteed_media_horizon_ms);
    let guaranteed_reserve_ms = guaranteed_media_horizon_ms.saturating_sub(conservative_playback_position_ms);
    let initial_hidden_ready_duration_ms = guaranteed_media_horizon_ms.saturating_sub(visible_tail);
    let transition_margin = HlsTransitionMarginMs::from_millis(
        guaranteed_transition_margin(input.ready_timeline, guaranteed_ready_span, conservative_playback_position_ms)
            .unwrap_or(input.manifest.target_duration_ms),
    );
    let recovery_boundary = input.recovery_trigger_budget.as_millis().saturating_add(transition_margin.as_millis());
    let recovery_required = input.origin_path_degraded && guaranteed_reserve_ms <= recovery_boundary;
    let cutover_required = input.origin_path_degraded
        && guaranteed_reserve_ms <= transition_margin.as_millis()
        && !input.recovery_committed;

    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms,
        conservative_playback_position_ms,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms,
        transition_margin,
        key_readiness_valid_until_ms: guaranteed_ready_span.key_readiness_valid_until_ms,
        recovery_required,
        cutover_required,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsGuaranteedReadySpan {
    start_ms: u64,
    horizon_ms: u64,
    first_unit_index: Option<usize>,
    end_unit_index: usize,
    key_readiness_valid_until_ms: Option<u64>,
}

fn contiguous_ready_span(
    timeline: &HlsReadyTimelineSnapshot,
    manifest: &HlsLeaseManifestSnapshot,
) -> HlsGuaranteedReadySpan {
    let fallback_start_ms = manifest_visible_start_ms(manifest);
    let Some(start_index) = timeline.units.iter().position(|unit| unit.proxy_seq == manifest.first_proxy_seq) else {
        return HlsGuaranteedReadySpan {
            start_ms: fallback_start_ms,
            horizon_ms: fallback_start_ms,
            first_unit_index: None,
            end_unit_index: 0,
            key_readiness_valid_until_ms: None,
        };
    };
    let first_unit = timeline.units[start_index];
    if first_unit.state != HlsReadyMediaState::Ready || !first_unit.required_map_ready || !first_unit.required_key_ready
    {
        return HlsGuaranteedReadySpan {
            start_ms: first_unit.start_ms,
            horizon_ms: first_unit.start_ms,
            first_unit_index: None,
            end_unit_index: start_index,
            key_readiness_valid_until_ms: None,
        };
    }

    let mut expected_seq = manifest.first_proxy_seq;
    let mut expected_start_ms = first_unit.start_ms;
    let mut horizon_ms = first_unit.start_ms;
    let mut end_unit_index = start_index;
    let mut key_readiness_valid_until_ms = None;
    for (unit_index, unit) in timeline.units.iter().enumerate().skip(start_index) {
        if unit.proxy_seq != expected_seq
            || unit.start_ms != expected_start_ms
            || unit.state != HlsReadyMediaState::Ready
            || !unit.required_map_ready
            || !unit.required_key_ready
        {
            break;
        }
        if let Some(valid_until_ms) = unit.key_ready_valid_until_ms {
            key_readiness_valid_until_ms =
                Some(key_readiness_valid_until_ms.map_or(valid_until_ms, |current: u64| current.min(valid_until_ms)));
        }
        horizon_ms = unit.start_ms.saturating_add(unit.duration_ms);
        end_unit_index = unit_index.saturating_add(1);
        let Some(next) = expected_seq.checked_add(1) else {
            break;
        };
        expected_seq = next;
        expected_start_ms = horizon_ms;
    }

    HlsGuaranteedReadySpan {
        start_ms: first_unit.start_ms,
        horizon_ms,
        first_unit_index: Some(start_index),
        end_unit_index,
        key_readiness_valid_until_ms,
    }
}

fn manifest_visible_start_ms(manifest: &HlsLeaseManifestSnapshot) -> u64 {
    let visible_duration_ms = manifest
        .visible_segments
        .iter()
        .fold(0_u64, |duration_ms, segment| duration_ms.saturating_add(segment.duration_ms));
    manifest.last_visible_media_end_ms.saturating_sub(visible_duration_ms.max(manifest.playlist_duration_ms))
}

fn aligned_visible_tail_ms(timeline: &HlsReadyTimelineSnapshot, manifest: &HlsLeaseManifestSnapshot) -> u64 {
    timeline
        .units
        .iter()
        .find(|unit| unit.proxy_seq == manifest.first_proxy_seq)
        .map_or(manifest.last_visible_media_end_ms, |unit| unit.start_ms.saturating_add(manifest.playlist_duration_ms))
}

fn conservative_playback_position(
    input: &HlsLeaseReserveInput<'_>,
    visible_tail_ms: u64,
    guaranteed_media_horizon_ms: u64,
) -> u64 {
    let Some(first_seq) = input.cursor.first_requested_proxy_seq else {
        return visible_tail_ms;
    };
    let Some(first_unit) = input.ready_timeline.units.iter().find(|unit| unit.proxy_seq == first_seq) else {
        return visible_tail_ms.max(guaranteed_media_horizon_ms);
    };
    if input.cursor.first_segment_completed_at_ms.is_none() {
        return visible_tail_ms.max(first_unit.start_ms);
    }
    let delivered_end = input
        .cursor
        .highest_contiguous_completed_proxy_seq
        .and_then(|seq| input.ready_timeline.units.iter().find(|unit| unit.proxy_seq == seq))
        .map_or(guaranteed_media_horizon_ms, |unit| unit.start_ms.saturating_add(unit.duration_ms));
    let elapsed_guarded = input.cursor.first_segment_completed_at_ms.map_or(0, |completed_at_ms| {
        input.now_ms.saturating_sub(completed_at_ms).saturating_mul(u64::from(input.playback_rate_guard_milli)) / 1_000
    });
    delivered_end.min(first_unit.start_ms.saturating_add(elapsed_guarded))
}

fn guaranteed_transition_margin(
    timeline: &HlsReadyTimelineSnapshot,
    span: HlsGuaranteedReadySpan,
    position_ms: u64,
) -> Option<u64> {
    let first_unit_index = span.first_unit_index?;
    timeline.units[first_unit_index..span.end_unit_index]
        .iter()
        .find_map(|unit| (unit.start_ms.saturating_add(unit.duration_ms) > position_ms).then_some(unit.duration_ms))
}

/// Origin state relevant to admission of a new shared-HLS lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsStartupAdmissionOriginState {
    Healthy,
    Degraded,
}

const HLS_STARTUP_TARGET_DURATION_MULTIPLIER: u64 = 3;

/// Returns the shared startup window promised by rendering and warm admission.
///
/// Three target durations are preferred, while a shorter playlist can only
/// promise the media duration it actually exposes.
pub(crate) fn minimum_hls_startup_window_ms(target_duration_ms: u64, visible_playlist_duration_ms: u64) -> u64 {
    target_duration_ms.saturating_mul(HLS_STARTUP_TARGET_DURATION_MULTIPLIER).min(visible_playlist_duration_ms)
}

/// Immutable inputs used to decide whether a new lease has enough READY media.
#[derive(Clone, Copy)]
pub(crate) struct HlsStartupAdmissionInput<'a> {
    pub manifest: &'a HlsLeaseManifestSnapshot,
    pub ready_timeline: &'a HlsReadyTimelineSnapshot,
    pub origin_state: HlsStartupAdmissionOriginState,
    pub recovery_trigger_budget: HlsRecoveryTriggerBudgetMs,
}

/// Reason why the immutable READY evidence cannot admit a new lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsStartupAdmissionRejection {
    InsufficientVisibleReadyMedia,
    InsufficientDegradedReserve,
    TransientOriginDegraded,
}

/// Typed startup-admission result; callers must handle rejection explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsStartupAdmissionDecision {
    Admit,
    Reject(HlsStartupAdmissionRejection),
}

/// READY-media evidence and the resulting startup-admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct HlsStartupAdmissionSnapshot {
    pub visible_contiguous_ready_duration_ms: u64,
    pub prospective_lease_reserve_ms: u64,
    pub transition_margin: HlsTransitionMarginMs,
    pub decision: HlsStartupAdmissionDecision,
}

/// Evaluates startup admission exclusively from an immutable manifest and READY snapshot.
pub(crate) fn evaluate_startup_admission(input: HlsStartupAdmissionInput<'_>) -> HlsStartupAdmissionSnapshot {
    let minimum_startup_duration_ms =
        minimum_hls_startup_window_ms(input.manifest.target_duration_ms, input.manifest.playlist_duration_ms);
    if input.manifest.delivery_mode == HlsManifestDeliveryMode::TransientPassthrough {
        let visible_duration_ms = input
            .manifest
            .visible_segments
            .iter()
            .fold(0_u64, |duration_ms, segment| duration_ms.saturating_add(segment.duration_ms));
        let decision = if visible_duration_ms < minimum_startup_duration_ms {
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientVisibleReadyMedia)
        } else if input.origin_state == HlsStartupAdmissionOriginState::Degraded {
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::TransientOriginDegraded)
        } else {
            HlsStartupAdmissionDecision::Admit
        };
        return HlsStartupAdmissionSnapshot {
            visible_contiguous_ready_duration_ms: 0,
            prospective_lease_reserve_ms: 0,
            transition_margin: HlsTransitionMarginMs::from_millis(input.manifest.target_duration_ms),
            decision,
        };
    }
    let guaranteed_ready_span = contiguous_ready_span(input.ready_timeline, input.manifest);
    let visible_tail_ms = aligned_visible_tail_ms(input.ready_timeline, input.manifest);
    let visible_contiguous_ready_duration_ms =
        guaranteed_ready_span.horizon_ms.min(visible_tail_ms).saturating_sub(guaranteed_ready_span.start_ms);
    let prospective_lease_reserve_ms = guaranteed_ready_span.horizon_ms.saturating_sub(visible_tail_ms);
    let transition_margin = HlsTransitionMarginMs::from_millis(
        guaranteed_transition_margin(input.ready_timeline, guaranteed_ready_span, visible_tail_ms)
            .unwrap_or(input.manifest.target_duration_ms),
    );
    let decision = if visible_contiguous_ready_duration_ms < minimum_startup_duration_ms {
        HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientVisibleReadyMedia)
    } else if input.origin_state == HlsStartupAdmissionOriginState::Degraded
        && prospective_lease_reserve_ms
            <= input.recovery_trigger_budget.as_millis().saturating_add(transition_margin.as_millis())
    {
        HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientDegradedReserve)
    } else {
        HlsStartupAdmissionDecision::Admit
    };

    HlsStartupAdmissionSnapshot {
        visible_contiguous_ready_duration_ms,
        prospective_lease_reserve_ms,
        transition_margin,
        decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> HlsLeaseManifestSnapshot {
        HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
            snapshot_generation: 1,
            delivered_at_ms: 0,
            first_proxy_seq: 0,
            last_proxy_seq: 2,
            visible_segments: Arc::from([
                HlsLeaseManifestSegment {
                    proxy_seq: 0,
                    duration_ms: 4_000,
                    uri: "0.ts".into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
                HlsLeaseManifestSegment {
                    proxy_seq: 1,
                    duration_ms: 5_000,
                    uri: "1.ts".into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
                HlsLeaseManifestSegment {
                    proxy_seq: 2,
                    duration_ms: 6_000,
                    uri: "2.ts".into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
            ]),
            discontinuity_sequence: 0,
            target_duration_ms: 6_000,
            playlist_duration_ms: 15_000,
            last_visible_media_end_ms: 15_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        }
    }

    fn ready_timeline() -> HlsReadyTimelineSnapshot {
        HlsReadyTimelineSnapshot {
            units: Arc::from([
                ready_unit(0, 0, 4_000),
                ready_unit(1, 4_000, 5_000),
                ready_unit(2, 9_000, 6_000),
                ready_unit(3, 15_000, 3_000),
                ready_unit(4, 18_000, 7_000),
                ready_unit(5, 25_000, 2_000),
            ]),
        }
    }

    fn ready_unit(proxy_seq: u64, start_ms: u64, duration_ms: u64) -> HlsReadyTimelineUnit {
        HlsReadyTimelineUnit {
            proxy_seq,
            start_ms,
            duration_ms,
            state: HlsReadyMediaState::Ready,
            required_map_ready: true,
            required_key_ready: true,
            key_ready_valid_until_ms: None,
        }
    }

    fn reserve(
        manifest: &HlsLeaseManifestSnapshot,
        cursor: &HlsLeasePlaybackCursor,
        ready_timeline: &HlsReadyTimelineSnapshot,
        now_ms: u64,
    ) -> HlsLeaseReserveSnapshot {
        evaluate_lease_reserve(HlsLeaseReserveInput {
            manifest,
            cursor,
            ready_timeline,
            now_ms,
            playback_rate_guard_milli: 1_050,
            recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(4_000),
            origin_path_degraded: true,
            recovery_committed: false,
        })
    }

    fn admission(
        manifest: &HlsLeaseManifestSnapshot,
        ready_timeline: &HlsReadyTimelineSnapshot,
        origin_state: HlsStartupAdmissionOriginState,
        recovery_trigger_budget_ms: u64,
    ) -> HlsStartupAdmissionSnapshot {
        evaluate_startup_admission(HlsStartupAdmissionInput {
            manifest,
            ready_timeline,
            origin_state,
            recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(recovery_trigger_budget_ms),
        })
    }

    #[test]
    fn strip_reserve_is_sum_of_actual_hidden_ready_durations() {
        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &ready_timeline(), 0);

        assert_eq!(reserve.initial_hidden_ready_duration_ms, 12_000);
        assert_eq!(reserve.guaranteed_reserve_ms, 12_000);
        assert_eq!(reserve.transition_margin.as_millis(), 3_000);
    }

    #[test]
    fn non_ready_hidden_segment_stops_guaranteed_reserve() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[4].state = HlsReadyMediaState::NotReady;
        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.initial_hidden_ready_duration_ms, 3_000);
    }

    #[test]
    fn missing_first_visible_unit_provides_no_ready_horizon() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[0].proxy_seq = 99;

        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 0);
        assert_eq!(reserve.guaranteed_reserve_ms, 0);
        assert_eq!(reserve.initial_hidden_ready_duration_ms, 0);
        assert_eq!(reserve.transition_margin.as_millis(), 6_000);
        assert!(reserve.recovery_required);
        assert!(reserve.cutover_required);
    }

    #[test]
    fn transient_delivery_never_claims_ready_cache_reserve() {
        let mut transient = manifest();
        transient.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;

        let reserve = reserve(&transient, &HlsLeasePlaybackCursor::default(), &ready_timeline(), 0);

        assert_eq!(reserve.availability_basis, HlsLeaseReserveAvailabilityBasis::TransientOriginBacked);
        assert_eq!(reserve.guaranteed_media_horizon_ms, 0);
        assert_eq!(reserve.guaranteed_reserve_ms, 0);
        assert_eq!(reserve.transition_margin.as_millis(), transient.target_duration_ms);
        assert!(reserve.recovery_required);
        assert!(reserve.cutover_required);
    }

    #[test]
    fn non_ready_first_visible_unit_does_not_adopt_isolated_ready_tail() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[0].state = HlsReadyMediaState::NotReady;

        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 0);
        assert_eq!(reserve.guaranteed_reserve_ms, 0);
        assert_eq!(reserve.transition_margin.as_millis(), 6_000);
    }

    #[test]
    fn missing_ready_map_stops_guaranteed_span_before_hidden_media() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[3].required_map_ready = false;

        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 15_000);
        assert_eq!(reserve.initial_hidden_ready_duration_ms, 0);
        assert_eq!(reserve.transition_margin.as_millis(), 6_000);
    }

    #[test]
    fn missing_ready_key_stops_guaranteed_span_at_first_encrypted_dependency() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[3].required_key_ready = false;

        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 15_000);
        assert_eq!(reserve.initial_hidden_ready_duration_ms, 0);
        assert_eq!(reserve.key_readiness_valid_until_ms, None);
    }

    #[test]
    fn rotating_ready_keys_bound_the_reserve_evidence_lifetime() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[1].key_ready_valid_until_ms = Some(9_000);
        Arc::make_mut(&mut timeline.units)[3].key_ready_valid_until_ms = Some(7_000);

        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &timeline, 1_000);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 27_000);
        assert_eq!(reserve.key_readiness_valid_until_ms, Some(7_000));
    }

    #[test]
    fn timeline_gap_stops_ready_horizon_and_uses_target_duration_margin() {
        let mut manifest = manifest();
        manifest.target_duration_ms = 11_000;
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[3].start_ms = 16_000;

        let reserve = reserve(&manifest, &HlsLeasePlaybackCursor::default(), &timeline, 0);

        assert_eq!(reserve.guaranteed_media_horizon_ms, 15_000);
        assert_eq!(reserve.guaranteed_reserve_ms, 0);
        assert_eq!(reserve.transition_margin.as_millis(), 11_000);
    }

    #[test]
    fn completed_first_segment_uses_measured_start_position() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let request = cursor.record_request_started(1, 1_000);
        assert_eq!(cursor.record_request_completed(request, 1_000), HlsPlaybackCompletionOutcome::Advanced);
        let reserve = reserve(&manifest(), &cursor, &ready_timeline(), 3_000);

        assert_eq!(reserve.conservative_playback_position_ms, 6_100);
        assert_eq!(reserve.guaranteed_reserve_ms, 20_900);
    }

    #[test]
    fn sliding_manifest_keeps_cursor_and_ready_horizon_on_one_timeline() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let request = cursor.record_request_started(0, 100);
        assert_eq!(cursor.record_request_completed(request, 100), HlsPlaybackCompletionOutcome::Advanced);
        let mut current_manifest = manifest();
        current_manifest.first_proxy_seq = 2;
        current_manifest.last_proxy_seq = 3;
        current_manifest.visible_segments = Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 2,
                duration_ms: 6_000,
                uri: "2.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 3,
                duration_ms: 3_000,
                uri: "3.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]);
        current_manifest.playlist_duration_ms = 9_000;
        current_manifest.last_visible_media_end_ms = 9_000;

        assert_eq!(cursor.ready_timeline_start_proxy_seq(current_manifest.first_proxy_seq), 0);
        let reserve = reserve(&current_manifest, &cursor, &ready_timeline(), 2_100);

        assert_eq!(reserve.conservative_playback_position_ms, 2_100);
        assert_eq!(reserve.guaranteed_media_horizon_ms, 27_000);
        assert_eq!(reserve.initial_hidden_ready_duration_ms, 9_000);
        assert_eq!(reserve.guaranteed_reserve_ms, 24_900);
        assert!(!reserve.cutover_required);
    }

    #[test]
    fn aborted_response_does_not_extend_completed_cursor() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let _request = cursor.record_request_started(4, 1_000);

        let reserve = reserve(&manifest(), &cursor, &ready_timeline(), 4_000);

        assert_eq!(cursor.first_segment_completed_at_ms, None);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(reserve.conservative_playback_position_ms, 18_000);
        assert_eq!(reserve.guaranteed_reserve_ms, 9_000);
    }

    #[test]
    fn out_of_order_completions_advance_only_after_gap_closes() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let first = cursor.record_request_started(0, 100);
        let second = cursor.record_request_started(1, 200);
        let third = cursor.record_request_started(2, 300);

        assert_eq!(cursor.record_request_completed(third, 600), HlsPlaybackCompletionOutcome::Buffered);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(cursor.record_request_completed(first, 700), HlsPlaybackCompletionOutcome::Advanced);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, Some(0));
        assert_eq!(cursor.record_request_completed(second, 800), HlsPlaybackCompletionOutcome::Advanced);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, Some(2));
        assert_eq!(cursor.first_segment_completed_at_ms, Some(700));
    }

    #[test]
    fn forward_seek_invalidates_late_completion_and_reduces_reserve() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let old_request = cursor.record_request_started(0, 100);
        let seek_request = cursor.record_request_started(4, 200);

        assert_eq!(cursor.record_request_completed(old_request, 300), HlsPlaybackCompletionOutcome::StaleRequest);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(cursor.record_request_completed(seek_request, 400), HlsPlaybackCompletionOutcome::Advanced);

        let reserve = reserve(&manifest(), &cursor, &ready_timeline(), 1_400);
        assert_eq!(reserve.conservative_playback_position_ms, 19_050);
        assert_eq!(reserve.guaranteed_reserve_ms, 7_950);
    }

    #[test]
    fn regressing_clock_saturates_playback_estimate() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let request = cursor.record_request_started(1, 2_000);
        assert_eq!(cursor.record_request_completed(request, 2_000), HlsPlaybackCompletionOutcome::Advanced);

        let reserve = reserve(&manifest(), &cursor, &ready_timeline(), 1_000);

        assert_eq!(reserve.conservative_playback_position_ms, 4_000);
    }

    #[test]
    fn out_of_order_completion_evidence_is_bounded() {
        let mut cursor = HlsLeasePlaybackCursor::default();
        let _first = cursor.record_request_started(0, 0);
        let mut tokens = Vec::new();
        for proxy_seq in 1..=u64::try_from(MAX_OUT_OF_ORDER_COMPLETIONS).unwrap_or(u64::MAX) + 8 {
            tokens.push(cursor.record_request_started(proxy_seq, proxy_seq));
        }
        for token in tokens.into_iter().skip(1) {
            let _ = cursor.record_request_completed(token, 1_000);
        }

        assert_eq!(cursor.out_of_order_completed_proxy_seqs.len(), MAX_OUT_OF_ORDER_COMPLETIONS);
    }

    #[test]
    fn publication_lateness_cannot_cut_over_with_sufficient_reserve() {
        let reserve = reserve(&manifest(), &HlsLeasePlaybackCursor::default(), &ready_timeline(), 20_000);

        assert!(!reserve.cutover_required);
    }

    #[test]
    fn healthy_startup_admission_uses_contiguous_visible_ready_window() {
        let admission = admission(&manifest(), &ready_timeline(), HlsStartupAdmissionOriginState::Healthy, u64::MAX);

        assert_eq!(admission.visible_contiguous_ready_duration_ms, 15_000);
        assert_eq!(admission.prospective_lease_reserve_ms, 12_000);
        assert_eq!(admission.transition_margin.as_millis(), 3_000);
        assert_eq!(admission.decision, HlsStartupAdmissionDecision::Admit);
    }

    #[test]
    fn startup_admission_rejects_non_contiguous_visible_ready_window() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[1].state = HlsReadyMediaState::NotReady;

        let admission = admission(&manifest(), &timeline, HlsStartupAdmissionOriginState::Healthy, 0);

        assert_eq!(admission.visible_contiguous_ready_duration_ms, 4_000);
        assert_eq!(
            admission.decision,
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientVisibleReadyMedia)
        );
    }

    #[test]
    fn startup_admission_rejects_visible_media_with_missing_key() {
        let mut timeline = ready_timeline();
        Arc::make_mut(&mut timeline.units)[1].required_key_ready = false;

        let admission = admission(&manifest(), &timeline, HlsStartupAdmissionOriginState::Healthy, 0);

        assert_eq!(admission.visible_contiguous_ready_duration_ms, 4_000);
        assert_eq!(
            admission.decision,
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientVisibleReadyMedia)
        );
    }

    #[test]
    fn degraded_startup_admission_requires_reserve_strictly_above_budget_and_margin() {
        let rejected = admission(&manifest(), &ready_timeline(), HlsStartupAdmissionOriginState::Degraded, 9_000);
        let admitted = admission(&manifest(), &ready_timeline(), HlsStartupAdmissionOriginState::Degraded, 8_999);

        assert_eq!(
            rejected.decision,
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientDegradedReserve)
        );
        assert_eq!(admitted.decision, HlsStartupAdmissionDecision::Admit);
    }

    #[test]
    fn transient_startup_is_explicit_and_rejects_degraded_origin() {
        let mut transient = manifest();
        transient.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;

        let healthy =
            admission(&transient, &HlsReadyTimelineSnapshot::default(), HlsStartupAdmissionOriginState::Healthy, 0);
        let degraded =
            admission(&transient, &HlsReadyTimelineSnapshot::default(), HlsStartupAdmissionOriginState::Degraded, 0);

        assert_eq!(healthy.decision, HlsStartupAdmissionDecision::Admit);
        assert_eq!(
            degraded.decision,
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::TransientOriginDegraded)
        );
    }

    #[test]
    fn hls_startup_policy_clamps_three_target_durations_to_visible_playlist() {
        assert_eq!(minimum_hls_startup_window_ms(4_000, 20_000), 12_000);
        assert_eq!(minimum_hls_startup_window_ms(4_000, 8_000), 8_000);
    }

    #[test]
    fn hls_startup_policy_saturates_target_duration_overflow() {
        assert_eq!(minimum_hls_startup_window_ms(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(minimum_hls_startup_window_ms(u64::MAX, 7_000), 7_000);
    }

    #[test]
    fn hls_startup_policy_admission_uses_shared_clamped_threshold() {
        let mut short_manifest = manifest();
        short_manifest.target_duration_ms = 4_000;
        short_manifest.playlist_duration_ms = 6_000;
        short_manifest.last_visible_media_end_ms = 6_000;
        short_manifest.visible_segments = Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 0,
                duration_ms: 2_000,
                uri: "0.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 1,
                duration_ms: 2_000,
                uri: "1.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 2,
                duration_ms: 2_000,
                uri: "2.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]);
        let short_ready_timeline = HlsReadyTimelineSnapshot {
            units: Arc::from([ready_unit(0, 0, 2_000), ready_unit(1, 2_000, 2_000), ready_unit(2, 4_000, 2_000)]),
        };

        let admission = admission(&short_manifest, &short_ready_timeline, HlsStartupAdmissionOriginState::Healthy, 0);

        assert_eq!(minimum_hls_startup_window_ms(4_000, 6_000), 6_000);
        assert_eq!(admission.visible_contiguous_ready_duration_ms, 6_000);
        assert_eq!(admission.decision, HlsStartupAdmissionDecision::Admit);
    }

    #[test]
    fn hls_startup_policy_admission_rejects_ready_window_below_clamped_visible_duration() {
        let mut manifest = manifest();
        manifest.target_duration_ms = 4_000;
        manifest.playlist_duration_ms = 7_000;
        manifest.last_visible_media_end_ms = 7_000;
        manifest.last_proxy_seq = 6;
        manifest.visible_segments = Arc::from(
            (0_u64..7)
                .map(|proxy_seq| HlsLeaseManifestSegment {
                    proxy_seq,
                    duration_ms: 1_000,
                    uri: format!("{proxy_seq}.ts"),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                })
                .collect::<Vec<_>>(),
        );
        let mut units = (0_u64..7)
            .map(|proxy_seq| ready_unit(proxy_seq, proxy_seq.saturating_mul(1_000), 1_000))
            .collect::<Vec<_>>();
        if let Some(last) = units.last_mut() {
            last.state = HlsReadyMediaState::NotReady;
        }
        let ready_timeline = HlsReadyTimelineSnapshot { units: Arc::from(units) };

        let admission = admission(&manifest, &ready_timeline, HlsStartupAdmissionOriginState::Healthy, 0);

        assert_eq!(minimum_hls_startup_window_ms(4_000, 7_000), 7_000);
        assert_eq!(admission.visible_contiguous_ready_duration_ms, 6_000);
        assert_eq!(
            admission.decision,
            HlsStartupAdmissionDecision::Reject(HlsStartupAdmissionRejection::InsufficientVisibleReadyMedia)
        );
    }
}
