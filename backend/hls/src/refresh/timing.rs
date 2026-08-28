//! When the next refresh is due, and how the last one is reported.
//!
//! The interval comes from the origin's own segment/target duration, ramped down
//! while the origin keeps publishing nothing new. The rest of this module is the
//! diagnostic view of one refresh: where its timing came from, how far the media
//! progressed, and the single log line that states both.

use crate::{manifest_fetch::HlsManifestSequenceRelation, safe_session_key};
use log::debug;

const COLD_START_RETRY_AFTER_SECONDS: u64 = 2;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsManifestProgress {
    Advanced,
    Rollover,
    Unchanged,
}

impl HlsManifestProgress {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::Advanced => "advanced",
            Self::Rollover => "rollover",
            Self::Unchanged => "unchanged",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsManifestTimingSource {
    LastSegmentDuration,
    TargetDuration,
    Fallback,
}

impl HlsManifestTimingSource {
    fn as_log_value(self) -> &'static str {
        match self {
            Self::LastSegmentDuration => "last_segment_duration",
            Self::TargetDuration => "target_duration",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) struct HlsManifestRefreshTiming {
    pub(super) last_segment_duration_ms: Option<u64>,
    pub(super) target_duration_ms: Option<u64>,
    pub(super) base_interval_ms: u64,
    pub(super) source: HlsManifestTimingSource,
    pub(super) progress: HlsManifestProgress,
}

fn build_manifest_refresh_timing_base(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> HlsManifestRefreshTiming {
    let source = if last_segment_duration_ms.is_some() {
        HlsManifestTimingSource::LastSegmentDuration
    } else if target_duration_ms.is_some() {
        HlsManifestTimingSource::TargetDuration
    } else {
        HlsManifestTimingSource::Fallback
    };
    let base_interval_ms = compute_origin_refresh_interval_ms(last_segment_duration_ms, target_duration_ms);
    HlsManifestRefreshTiming {
        last_segment_duration_ms,
        target_duration_ms,
        base_interval_ms,
        source,
        progress: HlsManifestProgress::Unchanged,
    }
}

pub(super) fn build_manifest_refresh_timing(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
    progress: HlsManifestProgress,
) -> HlsManifestRefreshTiming {
    let mut timing = build_manifest_refresh_timing_base(last_segment_duration_ms, target_duration_ms);
    timing.progress = progress;
    timing
}

pub(super) fn manifest_progress_from_highwater(
    before: Option<u64>,
    after: Option<u64>,
    sequence_relation: HlsManifestSequenceRelation,
) -> HlsManifestProgress {
    match (sequence_relation, before, after) {
        (HlsManifestSequenceRelation::RolloverCandidate, _, _) => HlsManifestProgress::Rollover,
        (_, None, Some(_)) => HlsManifestProgress::Advanced,
        (_, Some(before), Some(after)) if after > before => HlsManifestProgress::Advanced,
        _ => HlsManifestProgress::Unchanged,
    }
}

pub(super) fn apply_empty_refresh_rampdown_ms(base_interval_ms: u64, empty_refresh_count: u32) -> u64 {
    base_interval_ms.checked_shr(empty_refresh_count.min(16)).unwrap_or(0).max(1_000)
}

pub(super) fn log_manifest_refresh_timing(
    session: &crate::HlsSession,
    timing: HlsManifestRefreshTiming,
    refresh_interval_ms: u64,
) {
    debug!(
        "HLS manifest timing parsed: session={} target_duration={} last_segment_duration={} next_refresh_in_s={} source={} progress={} empty_refreshes={}",
        safe_session_key(&session.key),
        format_optional_millis_as_seconds(timing.target_duration_ms),
        format_optional_millis_as_seconds(timing.last_segment_duration_ms),
        format_millis_as_seconds(refresh_interval_ms),
        timing.source.as_log_value(),
        timing.progress.as_log_value(),
        session.origin_refresh.consecutive_empty_refreshes
    );
}

pub(super) fn format_optional_millis_as_seconds(value_ms: Option<u64>) -> String {
    value_ms.map_or_else(|| "none".to_string(), format_millis_as_seconds)
}

pub(super) fn format_millis_as_seconds(value_ms: u64) -> String {
    let seconds = value_ms / 1_000;
    let millis = value_ms % 1_000;
    format!("{seconds}.{millis:03}")
}

pub(super) fn compute_origin_refresh_interval_ms(
    last_segment_duration_ms: Option<u64>,
    target_duration_ms: Option<u64>,
) -> u64 {
    last_segment_duration_ms.or(target_duration_ms).map_or(2_000, |duration_ms| duration_ms / 2).max(1_000)
}

pub fn cold_start_retry_after_seconds() -> u64 {
    COLD_START_RETRY_AFTER_SECONDS
}
