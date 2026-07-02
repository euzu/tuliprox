use super::rewrite_hls_url;
use crate::api::model::{
    ProxySessionId, TransientResourceId, TransientResourceKind, TransientResourceRef,
    HLS_ACCESS_LEASE_ID_PLACEHOLDER,
};
use crate::model::{StripConfig, StripMode};
use shared::utils::CONSTANTS;
use std::{collections::HashMap, time::Duration};
use url::Url;

pub(super) const MIN_HLS_INITIAL_VISIBLE_SEGMENTS: usize = 3;

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
    /// Rewrites HLS URI surfaces to transient proxy resources.
    ///
    /// `final_manifest_url` must be the concrete URL of the manifest that was actually fetched. For provider-url
    /// failover and HTTP redirects this can be the selected mirror or final CDN/origin host. Relative segment, MAP and
    /// key URIs are resolved against this URL so transient resource fetches keep working after redirects.
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

    /// Rewrites HLS URI surfaces to transient proxy resources with extra rendering options.
    ///
    /// Keep `final_manifest_url` aligned with the actual fetched manifest URL. Passing the original `provider://` input
    /// URL or a pre-redirect URL would make relative transient resources point at the wrong fetch target.
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

pub fn materialize_transient_provisioning_handoff_view(
    origin_body: &str,
    previous_provisioning_body: Option<&str>,
    strip: &StripConfig,
    provisioning_segment_duration_ms: u64,
) -> Option<String> {
    let previous_provisioning_body = previous_provisioning_body?;
    let origin_lines = manifest_lines(origin_body);
    let origin_units = media_segment_units(&origin_lines);
    if origin_units.is_empty() {
        return None;
    }

    let origin_window_segments = configured_strip_segments(strip, &origin_lines, &origin_units)
        .saturating_add(MIN_HLS_INITIAL_VISIBLE_SEGMENTS)
        .min(origin_units.len());
    let origin_window_start = origin_units.len().saturating_sub(origin_window_segments);
    let origin_batch_segments = origin_window_segments.min(MIN_HLS_INITIAL_VISIBLE_SEGMENTS);
    if origin_batch_segments == 0 {
        return None;
    }
    let selected_origin_units = &origin_units[origin_window_start..origin_window_start + origin_batch_segments];

    let previous_lines = manifest_lines(previous_provisioning_body);
    let previous_units = media_segment_units(&previous_lines);
    if previous_units.is_empty() {
        return None;
    }
    let provisioning_tail_segments =
        TARGET_TRANSIENT_HANDOFF_SEGMENTS.saturating_sub(origin_batch_segments).saturating_sub(1);
    let selected_previous_units = previous_units
        .len()
        .checked_sub(provisioning_tail_segments)
        .map_or(previous_units.as_slice(), |start| &previous_units[start..]);
    let gap_unit = selected_previous_units.last().copied().or_else(|| previous_units.last().copied())?;

    let first_origin_unit = origin_units[0];
    let media_sequence = handoff_provisioning_media_sequence(
        &previous_lines,
        &previous_units,
        previous_units.len().saturating_sub(selected_previous_units.len()),
    );
    let mut rewritten = String::with_capacity(
        origin_body
            .len()
            .saturating_add(previous_provisioning_body.len())
            .saturating_add(64),
    );
    for line in &origin_lines[..first_origin_unit.start] {
        if line.line.trim().starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            if let Some(media_sequence) = media_sequence {
                let _ = std::fmt::Write::write_fmt(
                    &mut rewritten,
                    format_args!("#EXT-X-MEDIA-SEQUENCE:{media_sequence}{}", line.ending),
                );
                continue;
            }
        }
        rewritten.push_str(line.line);
        rewritten.push_str(line.ending);
    }
    append_manifest_block_separator(&mut rewritten);
    for unit in selected_previous_units {
        append_media_unit_with_duration_override(
            &mut rewritten,
            &previous_lines,
            *unit,
            Some(provisioning_segment_duration_ms),
        );
    }
    append_manifest_block_separator(&mut rewritten);
    append_gap_media_unit(&mut rewritten, &previous_lines, gap_unit, provisioning_segment_duration_ms);
    append_manifest_block_separator(&mut rewritten);
    for (index, unit) in selected_origin_units.iter().enumerate() {
        if index == 0 && !media_unit_has_discontinuity(&origin_lines, *unit) {
            rewritten.push_str("#EXT-X-DISCONTINUITY\n");
        }
        append_media_unit_with_duration_override(&mut rewritten, &origin_lines, *unit, None);
    }

    Some(rewritten)
}

pub fn apply_transient_discontinuity_sequence(body: &str, discontinuity_sequence: u64) -> String {
    let lines = manifest_lines(body);
    let existing_sequence_index = lines.iter().position(|line| is_discontinuity_sequence_tag(line.line.trim()));
    let insert_sequence_index = existing_sequence_index.unwrap_or_else(|| discontinuity_sequence_insert_index(&lines));

    let mut rewritten = String::with_capacity(body.len().saturating_add(40));
    for index in 0..=lines.len() {
        if existing_sequence_index.is_none() && index == insert_sequence_index {
            let _ = std::fmt::Write::write_fmt(
                &mut rewritten,
                format_args!("#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}\n"),
            );
        }
        if index == lines.len() {
            break;
        }
        if existing_sequence_index == Some(index) {
            let _ = std::fmt::Write::write_fmt(
                &mut rewritten,
                format_args!("#EXT-X-DISCONTINUITY-SEQUENCE:{}{}", discontinuity_sequence, lines[index].ending),
            );
        } else {
            rewritten.push_str(lines[index].line);
            rewritten.push_str(lines[index].ending);
        }
    }
    rewritten
}

pub fn transient_discontinuity_sequence(body: &str) -> Option<u64> {
    manifest_lines(body)
        .iter()
        .find_map(|line| parse_discontinuity_sequence(line.line))
}

pub fn transient_visible_discontinuity_count(body: &str) -> u64 {
    u64::try_from(
        manifest_lines(body)
            .iter()
            .filter(|line| is_discontinuity_tag(line.line.trim()))
            .count(),
    )
    .unwrap_or(u64::MAX)
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
    // Resolve against the final fetched manifest URL, not the configured provider:// entry or pre-redirect URL. Relative
    // resources commonly live under the redirect/CDN host returned by the manifest request.
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
            "/hls/shared/live/{}/{}/r/{}.{}",
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
pub(super) struct ManifestLine<'a> {
    pub(super) line: &'a str,
    pub(super) ending: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct MediaSegmentUnit {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl MediaSegmentUnit {
    pub(super) const fn contains(self, index: usize) -> bool { self.start <= index && index <= self.end }
}

pub(super) fn manifest_lines(body: &str) -> Vec<ManifestLine<'_>> {
    body.split_inclusive('\n')
        .map(|part| {
            let (line, ending) = split_line_ending(part);
            ManifestLine { line, ending }
        })
        .collect()
}

pub(super) fn media_segment_units(lines: &[ManifestLine<'_>]) -> Vec<MediaSegmentUnit> {
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

pub(super) fn configured_strip_segments(
    strip: &StripConfig,
    lines: &[ManifestLine<'_>],
    units: &[MediaSegmentUnit],
) -> usize {
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

const TARGET_TRANSIENT_HANDOFF_SEGMENTS: usize = 6;

fn append_media_unit_with_duration_override(
    output: &mut String,
    lines: &[ManifestLine<'_>],
    unit: MediaSegmentUnit,
    duration_ms: Option<u64>,
) {
    for line in &lines[unit.start..=unit.end] {
        if line.line.trim().starts_with("#EXTINF:") {
            if let Some(duration_ms) = duration_ms {
                let _ = std::fmt::Write::write_fmt(
                    output,
                    format_args!("#EXTINF:{},{}", format_duration_ms(duration_ms), line.ending),
                );
                continue;
            }
        }
        output.push_str(line.line);
        output.push_str(line.ending);
    }
}

fn append_gap_media_unit(
    output: &mut String,
    lines: &[ManifestLine<'_>],
    unit: MediaSegmentUnit,
    duration_ms: u64,
) {
    output.push_str("#EXT-X-GAP\n");
    let uri_index = lines[unit.start..=unit.end]
        .iter()
        .rposition(|line| {
            let trimmed = line.line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .map(|relative_index| unit.start + relative_index);
    if let Some(uri_index) = uri_index {
        let _ = std::fmt::Write::write_fmt(
            output,
            format_args!("#EXTINF:{},{}", format_duration_ms(duration_ms), lines[uri_index].ending),
        );
        output.push_str(lines[uri_index].line);
        output.push_str(lines[uri_index].ending);
    }
}

fn append_manifest_block_separator(output: &mut String) {
    if output.is_empty() || output.ends_with("\n\n") {
        return;
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');
}

fn media_unit_has_discontinuity(lines: &[ManifestLine<'_>], unit: MediaSegmentUnit) -> bool {
    lines[unit.start..=unit.end]
        .iter()
        .any(|line| is_discontinuity_tag(line.line.trim()))
}

fn handoff_provisioning_media_sequence(
    lines: &[ManifestLine<'_>],
    units: &[MediaSegmentUnit],
    selected_unit_start: usize,
) -> Option<u64> {
    let media_sequence = lines.iter().find_map(|line| parse_media_sequence(line.line))?;
    if selected_unit_start >= units.len() {
        return None;
    }
    Some(media_sequence.saturating_add(u64::try_from(selected_unit_start).ok()?))
}

fn parse_media_sequence(line: &str) -> Option<u64> {
    line.trim().strip_prefix("#EXT-X-MEDIA-SEQUENCE:")?.trim().parse().ok()
}

fn format_duration_ms(duration_ms: u64) -> String { format!("{}.{:03}", duration_ms / 1_000, duration_ms % 1_000) }

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

pub(super) const fn strip_mode_log_value(mode: StripMode) -> &'static str {
    match mode {
        StripMode::Segments => "segments",
        StripMode::Seconds => "seconds",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_transient_discontinuity_sequence, materialize_transient_provisioning_handoff_view,
        transient_discontinuity_sequence, TransientManifestRewriter, TransientRewriteOptions,
    };
    use crate::api::model::{build_transient_resource_id, ProxySessionId, TransientResourceKind};
    use crate::model::{StripConfig, StripMode};

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
                handoff_discontinuity_sequence: Some(0),
            },
        )
    }

    #[test]
    fn ext_x_key_uri_is_rewritten_without_changing_other_attributes() {
        let result = rewrite("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"keys/key.bin\",IV=0x1\n#EXTINF:4.0,\nseg.ts\n");

        assert!(result.body.contains("#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".bin\",IV=0x1"));
        assert!(!result.body.contains("keys/key.bin"));
    }

    #[test]
    fn segment_uri_lines_are_rewritten_to_transient_resources() {
        let result = rewrite("#EXTM3U\n#EXTINF:4.0,\nmedia/seg001.ts\n");

        assert!(result.body.contains("/hls/shared/live/proxy-id/__hls_access_lease_id__/r/"));
        assert!(result.body.contains(".ts"));
        assert!(!result.body.contains("media/seg001.ts"));
    }

    #[test]
    fn ext_x_map_uri_is_rewritten_without_changing_other_attributes() {
        let result = rewrite("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"10@5\"\n#EXTINF:4.0,\nseg.m4s\n");

        assert!(result.body.contains("#EXT-X-MAP:URI=\"/hls/shared/live/proxy-id/__hls_access_lease_id__/r/"));
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
        assert_eq!(result.body.matches("/hls/shared/live/proxy-id/__hls_access_lease_id__/r/").count(), 3);
    }

    #[test]
    fn unsupported_uri_attributes_are_rewritten_to_transient_resources() {
        let result = rewrite("#EXTM3U\n#EXT-X-PART:DURATION=1.0,URI=\"parts/part001.m4s\"\n");

        assert!(result.body.contains("#EXT-X-PART:DURATION=1.0,URI=\"/hls/shared/live/proxy-id/__hls_access_lease_id__/r/"));
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
    fn relative_resources_use_final_manifest_url_after_http_redirect() {
        let final_manifest_url = "https://cdn-final.example.net/redirected/live/index.m3u8";
        let result = TransientManifestRewriter::rewrite(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"keys/key.bin\"\n#EXT-X-MAP:URI=\"init/init.mp4\"\n#EXTINF:4.0,\nmedia/seg001.ts\n",
            final_manifest_url,
            &ProxySessionId("proxy-id".to_string()),
            b"secret",
            100,
            1_000,
        );

        let key_uri = "https://cdn-final.example.net/redirected/live/keys/key.bin";
        let map_uri = "https://cdn-final.example.net/redirected/live/init/init.mp4";
        let segment_uri = "https://cdn-final.example.net/redirected/live/media/seg001.ts";
        let expected_segment_id = build_transient_resource_id(segment_uri, b"secret");

        assert_eq!(result.resources.len(), 3);
        assert!(result.resources.iter().any(|resource| {
            resource.kind == TransientResourceKind::Key && resource.resolved_origin_uri == key_uri
        }));
        assert!(result.resources.iter().any(|resource| {
            resource.kind == TransientResourceKind::Map && resource.resolved_origin_uri == map_uri
        }));
        let segment = result
            .resources
            .iter()
            .find(|resource| resource.kind == TransientResourceKind::Segment)
            .expect("segment resource");
        assert_eq!(segment.resolved_origin_uri, segment_uri);
        assert_eq!(segment.id, expected_segment_id);
        assert!(result.body.contains(&format!("/r/{}.ts", expected_segment_id.0)));
    }

    #[test]
    fn same_relative_resource_on_different_final_hosts_uses_distinct_transient_ids() {
        let first = TransientManifestRewriter::rewrite(
            "#EXTM3U\n#EXTINF:4.0,\nseg.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
            &ProxySessionId("proxy-id".to_string()),
            b"secret",
            100,
            1_000,
        );
        let second = TransientManifestRewriter::rewrite(
            "#EXTM3U\n#EXTINF:4.0,\nseg.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
            &ProxySessionId("proxy-id".to_string()),
            b"secret",
            100,
            1_000,
        );

        let first_resource = first.resources.first().expect("first resource");
        let second_resource = second.resources.first().expect("second resource");

        assert_eq!(first_resource.resolved_origin_uri, "https://cdn-a.example.net/live/seg.ts");
        assert_eq!(second_resource.resolved_origin_uri, "https://cdn-b.example.net/live/seg.ts");
        assert_ne!(first_resource.id, second_resource.id);
    }

    #[test]
    fn handoff_boundary_sets_discontinuity_sequence_and_first_segment_discontinuity() {
        let result = rewrite_with_handoff(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg.ts\n",
        );

        assert!(result.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:7\n"));
        assert!(result.body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\n"));
        assert_eq!(result.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    }

    #[test]
    fn handoff_boundary_does_not_duplicate_existing_first_segment_discontinuity() {
        let result = rewrite_with_handoff(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nseg.ts\n",
        );

        assert!(result.body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:7\n"));
        assert_eq!(result.body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    }

    #[test]
    fn transient_provisioning_handoff_view_keeps_tail_gap_and_origin_head() {
        let previous = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-DISCONTINUITY-SEQUENCE:0\n#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000000.ts?pseq=0\n#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000001.ts?pseq=1\n#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000002.ts?pseq=2\n#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000003.ts?pseq=3\n";
        let rewritten_origin = rewrite_with_handoff(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:2455\n#EXTINF:0.64,\na.ts\n#EXTINF:1.92,\nb.ts\n#EXTINF:0.84,\nc.ts\n#EXTINF:1.08,\nd.ts\n#EXTINF:0.56,\ne.ts\n#EXTINF:1.36,\nf.ts\n",
        );

        let body = materialize_transient_provisioning_handoff_view(
            &rewritten_origin.body,
            Some(previous),
            &strip_segments(2),
            2_000,
        )
        .expect("handoff view should render");

        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert!(body.contains("#EXT-X-TARGETDURATION:2\n"));
        assert!(!body.contains("#EXT-X-TARGETDURATION:3\n"));
        assert!(body.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0\n\n#EXTINF:2.000,"));
        assert_eq!(body.matches("#EXTINF:").count(), 6);
        assert!(body.contains("#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000002.ts?pseq=2"));
        assert!(body.contains("\n#EXT-X-GAP\n#EXTINF:2.000,\n/hls/shared/live/proxy-id/__hls_access_lease_id__/000003.ts?pseq=3"));
        assert!(body.contains("\n#EXT-X-DISCONTINUITY\n#EXTINF:1.92,"));
        let gap = body.find("\n#EXT-X-GAP\n").expect("gap tag");
        let discontinuity = body.find("\n#EXT-X-DISCONTINUITY\n#EXTINF:1.92,").expect("handoff discontinuity");
        assert!(gap < discontinuity);
    }

    #[test]
    fn transient_discontinuity_sequence_is_kept_after_handoff() {
        let body = apply_transient_discontinuity_sequence(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:6560\n#EXT-X-TARGETDURATION:12\n#EXTINF:9.6,\na.ts\n",
            1,
        );

        assert_eq!(transient_discontinuity_sequence(&body), Some(1));
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:6560\n#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
    }

    fn strip_segments(value: u64) -> StripConfig { StripConfig { mode: StripMode::Segments, value } }

}
