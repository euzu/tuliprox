use super::transient_manifest::{
    configured_strip_segments, manifest_lines, media_segment_units, strip_mode_log_value,
    MIN_HLS_INITIAL_VISIBLE_SEGMENTS,
};
use crate::model::{StripConfig, StripMode};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsInitialStripView {
    pub body: String,
    pub outcome: HlsInitialStripOutcome,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsInitialStripOutcome {
    Applied {
        mode: &'static str,
        configured: u64,
        effective: usize,
        visible_segments: usize,
    },
    Skipped {
        reason: HlsInitialStripSkipReason,
        visible_segments: usize,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsInitialStripSkipReason {
    StripDisabled,
    NotEnoughSegments,
}

impl HlsInitialStripSkipReason {
    pub const fn as_log_reason(self) -> &'static str {
        match self {
            Self::StripDisabled => "strip-disabled",
            Self::NotEnoughSegments => "not-enough-segments",
        }
    }
}

pub fn materialize_initial_hls_strip_view(body: &str, strip: &StripConfig) -> HlsInitialStripView {
    let lines = manifest_lines(body);
    let units = media_segment_units(&lines);
    let media_segment_count = units.len();
    if strip.value == 0 {
        return HlsInitialStripView {
            body: body.to_string(),
            outcome: HlsInitialStripOutcome::Skipped {
                reason: HlsInitialStripSkipReason::StripDisabled,
                visible_segments: media_segment_count,
            },
        };
    }

    let effective_strip_segments = effective_initial_hls_strip_segments(strip, &lines, &units);
    if effective_strip_segments == 0 {
        return HlsInitialStripView {
            body: body.to_string(),
            outcome: HlsInitialStripOutcome::Skipped {
                reason: HlsInitialStripSkipReason::NotEnoughSegments,
                visible_segments: media_segment_count,
            },
        };
    }

    let visible_segments = media_segment_count.saturating_sub(effective_strip_segments);
    let strip_start_unit = media_segment_count.saturating_sub(effective_strip_segments);
    let strip_ranges = &units[strip_start_unit..];
    let mut stripped = String::with_capacity(body.len());
    for (index, line) in lines.iter().enumerate() {
        if strip_ranges.iter().any(|unit| unit.contains(index)) {
            continue;
        }
        stripped.push_str(line.line);
        stripped.push_str(line.ending);
    }

    HlsInitialStripView {
        body: stripped,
        outcome: HlsInitialStripOutcome::Applied {
            mode: strip_mode_log_value(strip.mode),
            configured: strip.value,
            effective: effective_strip_segments,
            visible_segments,
        },
    }
}

pub fn initial_hls_strip_segments_for_durations(strip: &StripConfig, segment_durations_ms: &[u64]) -> usize {
    if strip.value == 0 {
        return 0;
    }
    let configured_segments = match strip.mode {
        StripMode::Segments => usize::try_from(strip.value).unwrap_or(usize::MAX),
        StripMode::Seconds => configured_strip_segments_from_durations(strip.value, segment_durations_ms),
    };
    let max_removable_segments = segment_durations_ms.len().saturating_sub(MIN_HLS_INITIAL_VISIBLE_SEGMENTS);
    configured_segments.min(max_removable_segments)
}

fn effective_initial_hls_strip_segments(
    strip: &StripConfig,
    lines: &[super::transient_manifest::ManifestLine<'_>],
    units: &[super::transient_manifest::MediaSegmentUnit],
) -> usize {
    let configured_segments = configured_strip_segments(strip, lines, units);
    let max_removable_segments = units.len().saturating_sub(MIN_HLS_INITIAL_VISIBLE_SEGMENTS);
    configured_segments.min(max_removable_segments)
}

fn configured_strip_segments_from_durations(strip_seconds: u64, segment_durations_ms: &[u64]) -> usize {
    let target_ms = strip_seconds.saturating_mul(1_000);
    let mut accumulated_ms = 0_u64;
    let mut strip_segments = 0_usize;
    for duration_ms in segment_durations_ms.iter().rev() {
        strip_segments = strip_segments.saturating_add(1);
        accumulated_ms = accumulated_ms.saturating_add(*duration_ms);
        if accumulated_ms >= target_ms {
            break;
        }
    }
    strip_segments
}

#[cfg(test)]
mod tests {
    use super::{materialize_initial_hls_strip_view, HlsInitialStripOutcome, HlsInitialStripSkipReason};
    use crate::model::{StripConfig, StripMode};
    use std::fmt::Write as _;

    fn strip_segments(value: u64) -> StripConfig { StripConfig { mode: StripMode::Segments, value } }

    fn strip_seconds(value: u64) -> StripConfig { StripConfig { mode: StripMode::Seconds, value } }

    fn manifest_with_segments(count: usize) -> String {
        let mut body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n".to_string();
        for index in 0..count {
            body.push_str("#EXTINF:10.0,\n");
            writeln!(&mut body, "seg{index}.ts").expect("write segment URI");
        }
        body
    }

    fn media_segment_count(body: &str) -> usize {
        body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count()
    }

    #[test]
    fn pending_strip_segments_keeps_visible_head_window() {
        let view = materialize_initial_hls_strip_view(&manifest_with_segments(6), &strip_segments(3));

        assert_eq!(media_segment_count(&view.body), 3);
        assert!(matches!(
            view.outcome,
            HlsInitialStripOutcome::Applied {
                mode: "segments",
                configured: 3,
                effective: 3,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn pending_strip_segments_never_keeps_less_than_three_segments() {
        let four_segment_view = materialize_initial_hls_strip_view(&manifest_with_segments(4), &strip_segments(3));
        let three_segment_view = materialize_initial_hls_strip_view(&manifest_with_segments(3), &strip_segments(3));

        assert_eq!(media_segment_count(&four_segment_view.body), 3);
        assert_eq!(media_segment_count(&three_segment_view.body), 3);
        assert!(matches!(
            three_segment_view.outcome,
            HlsInitialStripOutcome::Skipped {
                reason: HlsInitialStripSkipReason::NotEnoughSegments,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn pending_strip_seconds_counts_tail_extinf_durations() {
        let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:10.0,\nseg100.ts\n#EXTINF:9.0,\nseg101.ts\n#EXTINF:9.0,\nseg102.ts\n#EXTINF:9.0,\nseg103.ts\n#EXTINF:9.0,\nseg104.ts\n#EXTINF:9.0,\nseg105.ts\n";

        let view = materialize_initial_hls_strip_view(body, &strip_seconds(30));

        assert_eq!(media_segment_count(&view.body), 3);
        assert!(matches!(
            view.outcome,
            HlsInitialStripOutcome::Applied {
                mode: "seconds",
                configured: 30,
                effective: 3,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn pending_strip_disabled_keeps_full_body() {
        let view = materialize_initial_hls_strip_view(&manifest_with_segments(4), &strip_segments(0));

        assert_eq!(media_segment_count(&view.body), 4);
        assert!(matches!(
            view.outcome,
            HlsInitialStripOutcome::Skipped {
                reason: HlsInitialStripSkipReason::StripDisabled,
                visible_segments: 4,
            }
        ));
    }

    #[test]
    fn pending_strip_preserves_media_sequence_and_byterange_semantics() {
        let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-BYTERANGE:100@200\n#EXTINF:4.0,\nseg42.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg43.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg44.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg45.ts\n";

        let view = materialize_initial_hls_strip_view(body, &strip_segments(1));

        assert!(view.body.contains("#EXT-X-MEDIA-SEQUENCE:42"));
        assert!(view.body.contains("#EXT-X-BYTERANGE:100@200"));
        assert!(view.body.contains("#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg43.ts"));
        assert!(!view.body.contains("seg45.ts"));
    }
}
