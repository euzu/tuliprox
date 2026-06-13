use super::rewrite_hls_url;
use crate::api::model::{
    ProxySessionId, TransientResourceId, TransientResourceKind, TransientResourceRef,
    HLS_ACCESS_LEASE_ID_PLACEHOLDER,
};
use crate::model::{StripConfig, StripMode};
use shared::utils::CONSTANTS;
use std::{collections::HashMap, time::Duration};
use url::Url;

const MIN_TRANSIENT_VISIBLE_SEGMENTS: usize = 3;

/// Result of a transient passthrough manifest rewrite.
#[derive(Debug, Clone)]
pub struct TransientRewriteResult {
    pub body: String,
    pub resources: Vec<TransientResourceRef>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct TransientRewriteOptions {
    pub handoff_discontinuity_sequence: Option<u64>,
}

/// Rewrites only HLS URI surfaces to transient live-HLS proxy resources.
pub struct TransientManifestRewriter;

impl TransientManifestRewriter {
    pub fn rewrite(
        body: &str,
        final_manifest_url: &str,
        proxy_session_id: &ProxySessionId,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
        ttl_ms: u64,
    ) -> TransientRewriteResult {
        Self::rewrite_with_options(
            body,
            final_manifest_url,
            proxy_session_id,
            reverse_proxy_rewrite_secret,
            now_ms,
            ttl_ms,
            TransientRewriteOptions::default(),
        )
    }

    pub fn rewrite_with_options(
        body: &str,
        final_manifest_url: &str,
        proxy_session_id: &ProxySessionId,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
        ttl_ms: u64,
        options: TransientRewriteOptions,
    ) -> TransientRewriteResult {
        let mut rewritten_body = String::with_capacity(body.len());
        let mut resources = HashMap::<TransientResourceId, TransientResourceRef>::new();

        for part in body.split_inclusive('\n') {
            let (line, line_ending) = split_line_ending(part);
            let rewritten_line = rewrite_line(
                line,
                final_manifest_url,
                proxy_session_id,
                reverse_proxy_rewrite_secret,
                now_ms,
                ttl_ms,
                &mut resources,
            );
            rewritten_body.push_str(&rewritten_line);
            rewritten_body.push_str(line_ending);
        }

        if body.is_empty() {
            return TransientRewriteResult {
                body: rewritten_body,
                resources: Vec::new(),
            };
        }

        if let Some(discontinuity_sequence) = options.handoff_discontinuity_sequence {
            rewritten_body = apply_handoff_discontinuity_boundary(&rewritten_body, discontinuity_sequence);
        }

        TransientRewriteResult {
            body: rewritten_body,
            resources: resources.into_values().collect(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TransientInitialStripView {
    pub body: String,
    pub outcome: TransientInitialStripOutcome,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransientInitialStripOutcome {
    Applied {
        mode: &'static str,
        configured: u64,
        effective: usize,
        visible_segments: usize,
    },
    Skipped {
        reason: TransientInitialStripSkipReason,
        visible_segments: usize,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransientInitialStripSkipReason {
    StripDisabled,
    NotEnoughSegments,
}

impl TransientInitialStripSkipReason {
    pub const fn as_log_reason(self) -> &'static str {
        match self {
            Self::StripDisabled => "strip-disabled",
            Self::NotEnoughSegments => "not-enough-segments",
        }
    }
}

pub fn materialize_initial_transient_strip_view(body: &str, strip: &StripConfig) -> TransientInitialStripView {
    let lines = manifest_lines(body);
    let units = media_segment_units(&lines);
    let media_segment_count = units.len();
    if strip.value == 0 {
        return TransientInitialStripView {
            body: body.to_string(),
            outcome: TransientInitialStripOutcome::Skipped {
                reason: TransientInitialStripSkipReason::StripDisabled,
                visible_segments: media_segment_count,
            },
        };
    }

    let configured_segments = configured_strip_segments(strip, &lines, &units);
    let max_removable_segments = media_segment_count.saturating_sub(MIN_TRANSIENT_VISIBLE_SEGMENTS);
    let effective_strip_segments = configured_segments.min(max_removable_segments);
    if effective_strip_segments == 0 {
        return TransientInitialStripView {
            body: body.to_string(),
            outcome: TransientInitialStripOutcome::Skipped {
                reason: TransientInitialStripSkipReason::NotEnoughSegments,
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

    TransientInitialStripView {
        body: stripped,
        outcome: TransientInitialStripOutcome::Applied {
            mode: strip_mode_log_value(strip.mode),
            configured: strip.value,
            effective: effective_strip_segments,
            visible_segments,
        },
    }
}

fn rewrite_line(
    line: &str,
    final_manifest_url: &str,
    proxy_session_id: &ProxySessionId,
    reverse_proxy_rewrite_secret: &[u8],
    now_ms: u64,
    ttl_ms: u64,
    resources: &mut HashMap<TransientResourceId, TransientResourceRef>,
) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return line.to_string();
    }

    if trimmed.starts_with("#EXT-X-KEY:") {
        return rewrite_uri_attribute(
            line,
            final_manifest_url,
            proxy_session_id,
            reverse_proxy_rewrite_secret,
            now_ms,
            ttl_ms,
            resources,
            TransientResourceKind::Key,
        );
    }

    if trimmed.starts_with("#EXT-X-MAP:") {
        return rewrite_uri_attribute(
            line,
            final_manifest_url,
            proxy_session_id,
            reverse_proxy_rewrite_secret,
            now_ms,
            ttl_ms,
            resources,
            TransientResourceKind::Map,
        );
    }

    if trimmed.starts_with('#') && CONSTANTS.re_hls_uri.is_match(line) {
        return rewrite_uri_attribute(
            line,
            final_manifest_url,
            proxy_session_id,
            reverse_proxy_rewrite_secret,
            now_ms,
            ttl_ms,
            resources,
            TransientResourceKind::Other,
        );
    }

    if trimmed.starts_with('#') {
        return line.to_string();
    }

    rewrite_resource_uri(
        trimmed,
        final_manifest_url,
        proxy_session_id,
        reverse_proxy_rewrite_secret,
        now_ms,
        ttl_ms,
        resources,
        TransientResourceKind::Segment,
    )
    .0
}

#[allow(clippy::too_many_arguments)]
fn rewrite_uri_attribute(
    line: &str,
    final_manifest_url: &str,
    proxy_session_id: &ProxySessionId,
    reverse_proxy_rewrite_secret: &[u8],
    now_ms: u64,
    ttl_ms: u64,
    resources: &mut HashMap<TransientResourceId, TransientResourceRef>,
    kind: TransientResourceKind,
) -> String {
    let Some(caps) = CONSTANTS.re_hls_uri.captures(line) else {
        return line.to_string();
    };
    let uri = &caps[1];
    let (proxy_uri, _) = rewrite_resource_uri(
        uri,
        final_manifest_url,
        proxy_session_id,
        reverse_proxy_rewrite_secret,
        now_ms,
        ttl_ms,
        resources,
        kind,
    );
    CONSTANTS
        .re_hls_uri
        .replace(line, format!(r#"URI="{proxy_uri}""#))
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn rewrite_resource_uri(
    uri: &str,
    final_manifest_url: &str,
    proxy_session_id: &ProxySessionId,
    reverse_proxy_rewrite_secret: &[u8],
    now_ms: u64,
    ttl_ms: u64,
    resources: &mut HashMap<TransientResourceId, TransientResourceRef>,
    kind: TransientResourceKind,
) -> (String, TransientResourceId) {
    let resolved_origin_uri = rewrite_hls_url(final_manifest_url, uri).into_owned();
    let extension = extension_for_resource(&resolved_origin_uri, kind);
    let resource = TransientResourceRef::new(
        kind,
        resolved_origin_uri,
        reverse_proxy_rewrite_secret,
        now_ms,
        ttl_ms,
        Some(extension.clone()),
    );
    let resource_id = resource.id.clone();
    resources
        .entry(resource_id.clone())
        .and_modify(|existing| *existing = resource.clone())
        .or_insert(resource);
    (
        format!(
            "/proxy/hls/live/{}/{}/r/{}.{}",
            proxy_session_id.0, HLS_ACCESS_LEASE_ID_PLACEHOLDER, resource_id.0, extension
        ),
        resource_id,
    )
}

fn extension_for_resource(resolved_origin_uri: &str, kind: TransientResourceKind) -> String {
    extract_extension(resolved_origin_uri)
        .filter(|extension| extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .map_or_else(|| fallback_extension(kind).to_string(), |extension| extension.to_ascii_lowercase())
}

fn extract_extension(resolved_origin_uri: &str) -> Option<String> {
    if let Ok(url) = Url::parse(resolved_origin_uri) {
        return url
            .path_segments()
            .and_then(Iterator::last)
            .and_then(|file_name| file_name.rsplit_once('.').map(|(_, extension)| extension.to_string()))
            .filter(|extension| !extension.is_empty());
    }

    let without_query = resolved_origin_uri
        .split_once('?')
        .map_or(resolved_origin_uri, |(path, _)| path);
    let without_fragment = without_query
        .split_once('#')
        .map_or(without_query, |(path, _)| path);
    without_fragment
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_string())
        .filter(|extension| !extension.is_empty())
}

fn fallback_extension(kind: TransientResourceKind) -> &'static str {
    match kind {
        TransientResourceKind::Key => "key",
        TransientResourceKind::Map => "mp4",
        TransientResourceKind::Segment | TransientResourceKind::Other => "bin",
    }
}

fn split_line_ending(part: &str) -> (&str, &str) {
    if let Some(line) = part.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = part.strip_suffix('\n') {
        (line, "\n")
    } else {
        (part, "")
    }
}

#[derive(Debug, Clone, Copy)]
struct ManifestLine<'a> {
    line: &'a str,
    ending: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct MediaSegmentUnit {
    start: usize,
    end: usize,
}

impl MediaSegmentUnit {
    const fn contains(self, index: usize) -> bool { self.start <= index && index <= self.end }
}

fn manifest_lines(body: &str) -> Vec<ManifestLine<'_>> {
    body.split_inclusive('\n')
        .map(|part| {
            let (line, ending) = split_line_ending(part);
            ManifestLine { line, ending }
        })
        .collect()
}

fn media_segment_units(lines: &[ManifestLine<'_>]) -> Vec<MediaSegmentUnit> {
    let mut units = Vec::new();
    let mut unit_start = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.line.trim();
        if is_segment_unit_tag(trimmed) {
            unit_start.get_or_insert(index);
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            unit_start = None;
            continue;
        }
        let start = unit_start.take().unwrap_or(index);
        units.push(MediaSegmentUnit { start, end: index });
    }
    units
}

fn configured_strip_segments(strip: &StripConfig, lines: &[ManifestLine<'_>], units: &[MediaSegmentUnit]) -> usize {
    match strip.mode {
        StripMode::Segments => usize::try_from(strip.value).unwrap_or(usize::MAX),
        StripMode::Seconds => {
            let target_ms = strip.value.saturating_mul(1_000);
            let mut accumulated_ms = 0_u64;
            let mut strip_segments = 0_usize;
            for unit in units.iter().rev() {
                strip_segments = strip_segments.saturating_add(1);
                accumulated_ms = accumulated_ms.saturating_add(segment_duration_ms(lines, *unit));
                if accumulated_ms >= target_ms {
                    break;
                }
            }
            strip_segments
        }
    }
}

fn segment_duration_ms(lines: &[ManifestLine<'_>], unit: MediaSegmentUnit) -> u64 {
    lines[unit.start..=unit.end]
        .iter()
        .find_map(|line| {
            let trimmed = line.line.trim();
            let extinf_value = trimmed.strip_prefix("#EXTINF:")?;
            let duration = extinf_value.split_once(',').map_or(extinf_value, |(value, _)| value);
            duration_ms_from_extinf(duration)
        })
        .unwrap_or(0)
}

fn apply_handoff_discontinuity_boundary(body: &str, handoff_discontinuity_sequence: u64) -> String {
    let lines = manifest_lines(body);
    let origin_discontinuity_sequence = lines
        .iter()
        .find_map(|line| parse_discontinuity_sequence(line.line))
        .unwrap_or(0);
    let effective_discontinuity_sequence =
        origin_discontinuity_sequence.saturating_add(handoff_discontinuity_sequence);
    let existing_sequence_index = lines.iter().position(|line| is_discontinuity_sequence_tag(line.line.trim()));
    let insert_sequence_index = existing_sequence_index.unwrap_or_else(|| discontinuity_sequence_insert_index(&lines));
    let first_segment = first_media_segment_boundary(&lines);

    let mut rewritten = String::with_capacity(body.len().saturating_add(64));
    for index in 0..=lines.len() {
        if existing_sequence_index.is_none() && index == insert_sequence_index {
            let _ = std::fmt::Write::write_fmt(
                &mut rewritten,
                format_args!("#EXT-X-DISCONTINUITY-SEQUENCE:{effective_discontinuity_sequence}\n"),
            );
        }
        if let Some((boundary_index, has_discontinuity)) = first_segment {
            if !has_discontinuity && index == boundary_index {
                rewritten.push_str("#EXT-X-DISCONTINUITY\n");
            }
        }
        if index == lines.len() {
            break;
        }
        if existing_sequence_index == Some(index) {
            let _ = std::fmt::Write::write_fmt(
                &mut rewritten,
                format_args!(
                    "#EXT-X-DISCONTINUITY-SEQUENCE:{}{}",
                    effective_discontinuity_sequence, lines[index].ending
                ),
            );
        } else {
            rewritten.push_str(lines[index].line);
            rewritten.push_str(lines[index].ending);
        }
    }
    rewritten
}

fn discontinuity_sequence_insert_index(lines: &[ManifestLine<'_>]) -> usize {
    lines
        .iter()
        .position(|line| line.line.trim().starts_with("#EXT-X-MEDIA-SEQUENCE:"))
        .or_else(|| lines.iter().position(|line| line.line.trim().starts_with("#EXT-X-TARGETDURATION:")))
        .or_else(|| lines.iter().position(|line| line.line.trim() == "#EXTM3U"))
        .map_or(0, |index| index + 1)
}

fn first_media_segment_boundary(lines: &[ManifestLine<'_>]) -> Option<(usize, bool)> {
    let mut unit_start = None;
    let mut unit_has_discontinuity = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_media_segment_unit_tag(trimmed) {
            unit_start.get_or_insert(index);
            if is_discontinuity_tag(trimmed) {
                unit_has_discontinuity = true;
            }
            continue;
        }
        if trimmed.starts_with('#') {
            unit_start = None;
            unit_has_discontinuity = false;
            continue;
        }
        return Some((unit_start.unwrap_or(index), unit_has_discontinuity));
    }
    None
}

fn parse_discontinuity_sequence(line: &str) -> Option<u64> {
    line.trim().strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:")?.trim().parse().ok()
}

fn is_discontinuity_sequence_tag(line: &str) -> bool {
    line.starts_with("#EXT-X-DISCONTINUITY-SEQUENCE:")
}

fn is_discontinuity_tag(line: &str) -> bool { line == "#EXT-X-DISCONTINUITY" }

fn duration_ms_from_extinf(value: &str) -> Option<u64> {
    let seconds = value.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    u64::try_from(Duration::from_secs_f64(seconds).as_millis()).ok()
}

fn is_segment_unit_tag(line: &str) -> bool {
    is_media_segment_unit_tag(line)
}

fn is_media_segment_unit_tag(line: &str) -> bool {
    line.starts_with("#EXTINF:")
        || line.starts_with("#EXT-X-BYTERANGE:")
        || line.starts_with("#EXT-X-PROGRAM-DATE-TIME:")
        || is_discontinuity_tag(line)
}

const fn strip_mode_log_value(mode: StripMode) -> &'static str {
    match mode {
        StripMode::Segments => "segments",
        StripMode::Seconds => "seconds",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        materialize_initial_transient_strip_view, TransientInitialStripOutcome, TransientInitialStripSkipReason,
        TransientManifestRewriter, TransientRewriteOptions,
    };
    use crate::api::model::{build_transient_resource_id, ProxySessionId};
    use crate::model::{StripConfig, StripMode};
    use std::fmt::Write as _;

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn rewrite(body: &str) -> crate::processing::parser::hls::transient_manifest::TransientRewriteResult {
        TransientManifestRewriter::rewrite(body, BASE_URL, &ProxySessionId("proxy-id".to_string()), b"secret", 100, 1_000)
    }

    fn rewrite_with_handoff(body: &str) -> crate::processing::parser::hls::transient_manifest::TransientRewriteResult {
        TransientManifestRewriter::rewrite_with_options(
            body,
            BASE_URL,
            &ProxySessionId("proxy-id".to_string()),
            b"secret",
            100,
            1_000,
            TransientRewriteOptions {
                handoff_discontinuity_sequence: Some(61),
            },
        )
    }

    #[test]
    fn ext_x_key_uri_is_rewritten_without_changing_other_attributes() {
        let result = rewrite("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"keys/key.bin\",IV=0x1\n#EXTINF:4.0,\nseg.ts\n");

        assert!(result.body.contains("#EXT-X-KEY:METHOD=AES-128,URI=\"/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".bin\",IV=0x1"));
        assert!(!result.body.contains("keys/key.bin"));
    }

    #[test]
    fn segment_uri_lines_are_rewritten_to_transient_resources() {
        let result = rewrite("#EXTM3U\n#EXTINF:4.0,\nmedia/seg001.ts\n");

        assert!(result.body.contains("/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".ts"));
        assert!(!result.body.contains("media/seg001.ts"));
    }

    #[test]
    fn ext_x_map_uri_is_rewritten_without_changing_other_attributes() {
        let result = rewrite("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"10@5\"\n#EXTINF:4.0,\nseg.m4s\n");

        assert!(result.body.contains("#EXT-X-MAP:URI=\"/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".mp4\",BYTERANGE=\"10@5\""));
        assert!(!result.body.contains("init.mp4"));
    }

    #[test]
    fn byterange_and_media_sequence_remain_unchanged() {
        let result = rewrite("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-BYTERANGE:100@200\n#EXTINF:4.0,\nseg.ts\n");

        assert!(result.body.contains("#EXT-X-MEDIA-SEQUENCE:42"));
        assert!(result.body.contains("#EXT-X-BYTERANGE:100@200"));
    }

    #[test]
    fn transient_output_never_uses_legacy_hls_token_route() {
        let result = rewrite(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg.ts\n",
        );

        assert!(!result.body.contains("/hls/user/pass/"));
        assert!(!result.body.contains("key.bin"));
        assert!(!result.body.contains("init.mp4"));
        assert!(!result.body.contains("seg.ts"));
        assert_eq!(result.body.matches("/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/").count(), 3);
    }

    #[test]
    fn unsupported_uri_attributes_are_rewritten_to_transient_resources() {
        let result = rewrite("#EXTM3U\n#EXT-X-PART:DURATION=1.0,URI=\"parts/part001.m4s\"\n");

        assert!(result.body.contains("#EXT-X-PART:DURATION=1.0,URI=\"/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".m4s\""));
        assert!(!result.body.contains("parts/part001.m4s"));
    }

    #[test]
    fn same_origin_uri_uses_same_transient_resource_id() {
        let result = rewrite("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n#EXTINF:4.0,\n./seg.ts\n");
        let expected = build_transient_resource_id("http://origin.example.com/live/final/seg.ts", b"secret");

        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id, expected);
        assert_eq!(result.body.matches(&expected.0).count(), 2);
    }

    #[test]
    fn handoff_boundary_sets_discontinuity_sequence_and_first_segment_discontinuity() {
        let result = rewrite_with_handoff(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg.ts\n",
        );

        assert!(result.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:68\n"));
        assert!(result.body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\n"));
        assert_eq!(result.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    }

    #[test]
    fn handoff_boundary_does_not_duplicate_existing_first_segment_discontinuity() {
        let result = rewrite_with_handoff(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nseg.ts\n",
        );

        assert!(result.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:68\n"));
        assert_eq!(result.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    }

    fn strip_segments(value: u64) -> StripConfig { StripConfig { mode: StripMode::Segments, value } }

    fn strip_seconds(value: u64) -> StripConfig { StripConfig { mode: StripMode::Seconds, value } }

    fn manifest_with_segments(count: u64) -> String {
        let mut body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n".to_string();
        for index in 0..count {
            body.push_str("#EXTINF:10.0,\n");
            let _ = writeln!(body, "/proxy/hls/live/proxy-id/__hls_access_lease_id__/r/seg{index}.ts");
        }
        body
    }

    fn media_segment_count(body: &str) -> usize { body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count() }

    #[test]
    fn pending_transient_strip_segments_keeps_visible_head_window() {
        let view = materialize_initial_transient_strip_view(&manifest_with_segments(6), &strip_segments(3));

        assert_eq!(media_segment_count(&view.body), 3);
        assert!(matches!(
            view.outcome,
            TransientInitialStripOutcome::Applied {
                mode: "segments",
                configured: 3,
                effective: 3,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn pending_transient_strip_segments_never_keeps_less_than_three_segments() {
        let four_segment_view = materialize_initial_transient_strip_view(&manifest_with_segments(4), &strip_segments(3));
        let three_segment_view = materialize_initial_transient_strip_view(&manifest_with_segments(3), &strip_segments(3));

        assert_eq!(media_segment_count(&four_segment_view.body), 3);
        assert_eq!(media_segment_count(&three_segment_view.body), 3);
        assert!(matches!(
            three_segment_view.outcome,
            TransientInitialStripOutcome::Skipped {
                reason: TransientInitialStripSkipReason::NotEnoughSegments,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn pending_transient_strip_seconds_counts_tail_extinf_durations() {
        let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:10.0,\nseg100.ts\n#EXTINF:9.0,\nseg101.ts\n#EXTINF:9.0,\nseg102.ts\n#EXTINF:9.0,\nseg103.ts\n#EXTINF:9.0,\nseg104.ts\n#EXTINF:9.0,\nseg105.ts\n";

        let view = materialize_initial_transient_strip_view(body, &strip_seconds(30));

        assert_eq!(media_segment_count(&view.body), 3);
        assert!(matches!(
            view.outcome,
            TransientInitialStripOutcome::Applied {
                mode: "seconds",
                configured: 30,
                effective: 3,
                visible_segments: 3,
            }
        ));
    }

    #[test]
    fn transient_strip_preserves_media_sequence_and_byterange_semantics() {
        let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-BYTERANGE:100@200\n#EXTINF:4.0,\nseg42.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg43.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg44.ts\n#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg45.ts\n";

        let view = materialize_initial_transient_strip_view(body, &strip_segments(1));

        assert!(view.body.contains("#EXT-X-MEDIA-SEQUENCE:42"));
        assert!(view.body.contains("#EXT-X-BYTERANGE:100@200"));
        assert!(view.body.contains("#EXT-X-BYTERANGE:100\n#EXTINF:4.0,\nseg43.ts"));
        assert!(!view.body.contains("seg45.ts"));
    }

    #[test]
    fn transient_strip_disabled_keeps_manifest_body() {
        let body = manifest_with_segments(6);

        let view = materialize_initial_transient_strip_view(&body, &strip_segments(0));

        assert_eq!(view.body, body);
        assert!(matches!(
            view.outcome,
            TransientInitialStripOutcome::Skipped {
                reason: TransientInitialStripSkipReason::StripDisabled,
                visible_segments: 6,
            }
        ));
    }
}
