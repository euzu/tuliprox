#![allow(dead_code)]

use super::rewrite_hls_url;
use shared::utils::CONSTANTS;
use std::{borrow::Cow, collections::HashMap, fmt};

/// Result of parsing an origin media playlist for the live HLS cache proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginManifestParseOutcome {
    Normal(ParsedOriginManifest),
    TransientPassthrough { reason: OriginManifestTransientReason },
}

/// Reason why an origin manifest cannot enter the normal cache timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginManifestTransientReason {
    ExtXKey,
    UnsupportedTag { tag: String },
    ParserUnsupportedFeature { feature: String },
}

/// Parsed normal-timeline view of a live HLS origin media playlist.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedOriginManifest {
    pub origin_manifest_sequence: u64,
    pub origin_manifest_segment_cnt: usize,
    pub version: Option<u16>,
    pub target_duration: Option<u32>,
    pub discontinuity_sequence: Option<u64>,
    pub independent_segments: bool,
    pub maps: Vec<ParsedOriginMap>,
    pub segments: Vec<ParsedOriginSegment>,
}

/// Timing values parsed from HLS manifest attributes used for origin refresh debounce.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParsedManifestTiming {
    pub target_duration_ms: Option<u64>,
    pub last_segment_duration_ms: Option<u64>,
}

/// Lightweight timeline markers used to validate transient passthrough manifests
/// without applying normal timeline parsing or URI normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedOriginManifestTimeline {
    pub origin_manifest_sequence: u64,
    pub origin_manifest_segment_cnt: usize,
}

impl ParsedOriginManifestTimeline {
    pub fn origin_highwater(self) -> Option<u64> {
        let segment_count = u64::try_from(self.origin_manifest_segment_cnt).ok()?;
        if segment_count == 0 {
            return None;
        }
        self.origin_manifest_sequence.checked_add(segment_count.saturating_sub(1))
    }
}

impl fmt::Debug for ParsedOriginManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedOriginManifest")
            .field("origin_manifest_sequence", &self.origin_manifest_sequence)
            .field("origin_manifest_segment_cnt", &self.origin_manifest_segment_cnt)
            .field("version", &self.version)
            .field("target_duration", &self.target_duration)
            .field("discontinuity_sequence", &self.discontinuity_sequence)
            .field("independent_segments", &self.independent_segments)
            .field("maps_len", &self.maps.len())
            .field("segments_len", &self.segments.len())
            .finish()
    }
}

/// Parsed origin segment metadata before proxy sequence mapping and rendering.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedOriginSegment {
    pub origin_seq: u64,
    pub duration_ms: u64,
    pub resolved_origin_url: String,
    pub discontinuity_before: bool,
    pub program_date_time: Option<String>,
    pub daterange_tags_before: Vec<String>,
    pub origin_byte_range: Option<ParsedByteRange>,
    pub map_ref: Option<usize>,
}

impl fmt::Debug for ParsedOriginSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedOriginSegment")
            .field("origin_seq", &self.origin_seq)
            .field("duration_ms", &self.duration_ms)
            .field("resolved_origin_url", &"<redacted>")
            .field("discontinuity_before", &self.discontinuity_before)
            .field("program_date_time", &self.program_date_time)
            .field("daterange_tags_before", &self.daterange_tags_before)
            .field("origin_byte_range", &self.origin_byte_range)
            .field("map_ref", &self.map_ref)
            .finish()
    }
}

/// Parsed origin MAP metadata before proxy MAP ID assignment and rendering.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedOriginMap {
    pub map_id: usize,
    pub resolved_origin_uri: String,
    pub byte_range: Option<ParsedByteRange>,
}

impl fmt::Debug for ParsedOriginMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedOriginMap")
            .field("map_id", &self.map_id)
            .field("resolved_origin_uri", &"<redacted>")
            .field("byte_range", &self.byte_range)
            .finish()
    }
}

/// Absolute origin byte range normalized from an `EXT-X-BYTERANGE` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedByteRange {
    pub length: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingByteRange {
    WithOffset(ParsedByteRange),
    WithoutOffset(u64),
}

pub fn parse_origin_media_manifest(body: &str, final_manifest_url: &str) -> OriginManifestParseOutcome {
    match parse_origin_media_manifest_result(body, final_manifest_url) {
        Ok(manifest) => OriginManifestParseOutcome::Normal(manifest),
        Err(reason) => OriginManifestParseOutcome::TransientPassthrough { reason },
    }
}

pub fn parse_origin_manifest_timeline(
    body: &str,
) -> Result<ParsedOriginManifestTimeline, OriginManifestTransientReason> {
    let mut seen_extm3u = false;
    let mut media_sequence = None;
    let mut pending_extinf = false;
    let mut segment_count = 0_usize;

    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with('#') {
            let tag = tag_name(line);
            match tag {
                "#EXTM3U" => seen_extm3u = true,
                "#EXT-X-MEDIA-SEQUENCE" => {
                    media_sequence = Some(parse_tag_value(line, tag)?.parse_numeric("invalid_media_sequence")?);
                }
                "#EXTINF" => pending_extinf = true,
                _ => {}
            }
            continue;
        }

        if !pending_extinf {
            return Err(parser_feature("segment_uri_without_extinf"));
        }
        pending_extinf = false;
        segment_count = segment_count.saturating_add(1);
    }

    if !seen_extm3u {
        return Err(parser_feature("missing_extm3u"));
    }
    if pending_extinf {
        return Err(parser_feature("extinf_without_segment_uri"));
    }

    Ok(ParsedOriginManifestTimeline {
        origin_manifest_sequence: media_sequence.unwrap_or(0),
        origin_manifest_segment_cnt: segment_count,
    })
}

pub fn parse_manifest_timing(body: &str) -> ParsedManifestTiming {
    let mut timing = ParsedManifestTiming::default();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            if let Ok(duration) = value.trim().parse::<u64>() {
                timing.target_duration_ms = Some(duration.saturating_mul(1_000));
            }
        } else if let Some(value) = line.strip_prefix("#EXTINF:").and_then(parse_extinf_duration_millis) {
            timing.last_segment_duration_ms = Some(value);
        }
    }
    timing
}

fn parse_extinf_duration_millis(value: &str) -> Option<u64> {
    let duration = value.split(',').next()?.trim();
    let (seconds, fraction) = duration.split_once('.').unwrap_or((duration, ""));
    let seconds = seconds.parse::<u64>().ok()?;
    let millis = fraction.chars().take(3).try_fold((0_u64, 0_u32), |(value, digits), digit| {
        digit.to_digit(10).map(|digit| (value.saturating_mul(10).saturating_add(u64::from(digit)), digits + 1))
    })?;
    let millis = millis.0.saturating_mul(10_u64.saturating_pow(3_u32.saturating_sub(millis.1)));
    Some(seconds.saturating_mul(1_000).saturating_add(millis))
}

fn parse_origin_media_manifest_result(
    body: &str,
    final_manifest_url: &str,
) -> Result<ParsedOriginManifest, OriginManifestTransientReason> {
    let mut parser = OriginManifestParser::new(final_manifest_url);
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        parser.parse_line(line)?;
    }
    parser.finish()
}

struct OriginManifestParser<'a> {
    final_manifest_url: &'a str,
    seen_extm3u: bool,
    version: Option<u16>,
    target_duration: Option<u32>,
    media_sequence: Option<u64>,
    discontinuity_sequence: Option<u64>,
    independent_segments: bool,
    pending_extinf_duration_ms: Option<u64>,
    pending_discontinuity: bool,
    pending_program_date_time: Option<String>,
    pending_daterange_tags: Vec<String>,
    pending_byte_range: Option<PendingByteRange>,
    current_map_ref: Option<usize>,
    maps: Vec<ParsedOriginMap>,
    segments_without_seq: Vec<ParsedOriginSegment>,
    next_byte_range_offset_by_uri: HashMap<String, u64>,
}

impl<'a> OriginManifestParser<'a> {
    fn new(final_manifest_url: &'a str) -> Self {
        Self {
            final_manifest_url,
            seen_extm3u: false,
            version: None,
            target_duration: None,
            media_sequence: None,
            discontinuity_sequence: None,
            independent_segments: false,
            pending_extinf_duration_ms: None,
            pending_discontinuity: false,
            pending_program_date_time: None,
            pending_daterange_tags: Vec::new(),
            pending_byte_range: None,
            current_map_ref: None,
            maps: Vec::new(),
            segments_without_seq: Vec::new(),
            next_byte_range_offset_by_uri: HashMap::new(),
        }
    }

    fn parse_line(&mut self, line: &str) -> Result<(), OriginManifestTransientReason> {
        if line.starts_with('#') {
            return self.parse_tag_line(line);
        }
        self.parse_segment_uri(line)
    }

    fn parse_tag_line(&mut self, line: &str) -> Result<(), OriginManifestTransientReason> {
        if !line.starts_with("#EXT") {
            return Ok(());
        }
        let tag = tag_name(line);
        if tag == "#EXT-X-KEY" {
            return Err(OriginManifestTransientReason::ExtXKey);
        }
        if !is_allowed_normal_timeline_tag(tag) {
            return Err(OriginManifestTransientReason::UnsupportedTag { tag: tag.to_string() });
        }

        match tag {
            "#EXTM3U" => self.seen_extm3u = true,
            "#EXT-X-VERSION" => self.version = Some(parse_tag_value(line, tag)?.parse_numeric("invalid_version")?),
            "#EXT-X-TARGETDURATION" => {
                self.target_duration = Some(parse_tag_value(line, tag)?.parse_numeric("invalid_target_duration")?);
            }
            "#EXT-X-MEDIA-SEQUENCE" => {
                self.media_sequence = Some(parse_tag_value(line, tag)?.parse_numeric("invalid_media_sequence")?);
            }
            "#EXTINF" => self.pending_extinf_duration_ms = Some(parse_extinf_duration_ms(line)?),
            "#EXT-X-DISCONTINUITY" => self.pending_discontinuity = true,
            "#EXT-X-DISCONTINUITY-SEQUENCE" => {
                self.discontinuity_sequence =
                    Some(parse_tag_value(line, tag)?.parse_numeric("invalid_discontinuity_sequence")?);
            }
            "#EXT-X-MAP" => self.current_map_ref = Some(self.parse_map(line)?),
            "#EXT-X-BYTERANGE" => self.pending_byte_range = Some(parse_byterange_tag(line)?),
            "#EXT-X-PROGRAM-DATE-TIME" => {
                self.pending_program_date_time = Some(parse_tag_value(line, tag)?.to_string());
            }
            "#EXT-X-DATERANGE" => self.pending_daterange_tags.push(line.to_string()),
            "#EXT-X-INDEPENDENT-SEGMENTS" => self.independent_segments = true,
            _ => {}
        }
        Ok(())
    }

    fn parse_map(&mut self, line: &str) -> Result<usize, OriginManifestTransientReason> {
        let Some(captures) = CONSTANTS.re_hls_uri.captures(line) else {
            return Err(parser_feature("map_without_uri"));
        };
        let Some(uri) = captures.get(1).map(|m| m.as_str()) else {
            return Err(parser_feature("map_without_uri"));
        };

        let resolved_origin_uri = resolve_uri(self.final_manifest_url, uri);
        let byte_range = parse_attribute(line, "BYTERANGE").map(parse_map_byterange).transpose()?;
        let map_id = self.maps.len();
        self.maps.push(ParsedOriginMap { map_id, resolved_origin_uri, byte_range });
        Ok(map_id)
    }

    fn parse_segment_uri(&mut self, line: &str) -> Result<(), OriginManifestTransientReason> {
        let Some(duration_ms) = self.pending_extinf_duration_ms.take() else {
            return Err(parser_feature("segment_uri_without_extinf"));
        };
        let resolved_origin_url = resolve_uri(self.final_manifest_url, line);
        let origin_byte_range = self.resolve_pending_byte_range(&resolved_origin_url)?;
        self.segments_without_seq.push(ParsedOriginSegment {
            origin_seq: 0,
            duration_ms,
            resolved_origin_url,
            discontinuity_before: std::mem::take(&mut self.pending_discontinuity),
            program_date_time: self.pending_program_date_time.take(),
            daterange_tags_before: std::mem::take(&mut self.pending_daterange_tags),
            origin_byte_range,
            map_ref: self.current_map_ref,
        });
        Ok(())
    }

    fn resolve_pending_byte_range(
        &mut self,
        resolved_origin_url: &str,
    ) -> Result<Option<ParsedByteRange>, OriginManifestTransientReason> {
        let Some(pending) = self.pending_byte_range.take() else {
            return Ok(None);
        };
        let byte_range = match pending {
            PendingByteRange::WithOffset(byte_range) => byte_range,
            PendingByteRange::WithoutOffset(length) => {
                let Some(offset) = self.next_byte_range_offset_by_uri.get(resolved_origin_url).copied() else {
                    return Err(parser_feature("byterange_without_resolvable_offset"));
                };
                ParsedByteRange { length, offset }
            }
        };
        self.next_byte_range_offset_by_uri.insert(
            resolved_origin_url.to_string(),
            byte_range
                .offset
                .checked_add(byte_range.length)
                .ok_or_else(|| parser_feature("byterange_offset_overflow"))?,
        );
        Ok(Some(byte_range))
    }

    fn finish(mut self) -> Result<ParsedOriginManifest, OriginManifestTransientReason> {
        if !self.seen_extm3u {
            return Err(parser_feature("missing_extm3u"));
        }
        if self.pending_extinf_duration_ms.is_some() {
            return Err(parser_feature("extinf_without_segment_uri"));
        }

        let origin_manifest_sequence = self.media_sequence.unwrap_or(0);
        for (idx, segment) in self.segments_without_seq.iter_mut().enumerate() {
            segment.origin_seq = origin_manifest_sequence
                .checked_add(u64::try_from(idx).map_err(|_| parser_feature("too_many_segments"))?)
                .ok_or_else(|| parser_feature("origin_sequence_overflow"))?;
        }

        Ok(ParsedOriginManifest {
            origin_manifest_sequence,
            origin_manifest_segment_cnt: self.segments_without_seq.len(),
            version: self.version,
            target_duration: self.target_duration,
            discontinuity_sequence: self.discontinuity_sequence,
            independent_segments: self.independent_segments,
            maps: self.maps,
            segments: self.segments_without_seq,
        })
    }
}

trait ParseNumericTag {
    fn parse_numeric<T>(&self, feature: &'static str) -> Result<T, OriginManifestTransientReason>
    where
        T: std::str::FromStr;
}

impl ParseNumericTag for str {
    fn parse_numeric<T>(&self, feature: &'static str) -> Result<T, OriginManifestTransientReason>
    where
        T: std::str::FromStr,
    {
        self.trim().parse().map_err(|_| parser_feature(feature))
    }
}

fn tag_name(line: &str) -> &str { line.split_once(':').map_or(line, |(tag, _)| tag) }

fn is_allowed_normal_timeline_tag(tag: &str) -> bool {
    matches!(
        tag,
        "#EXTM3U"
            | "#EXT-X-VERSION"
            | "#EXT-X-TARGETDURATION"
            | "#EXT-X-MEDIA-SEQUENCE"
            | "#EXTINF"
            | "#EXT-X-DISCONTINUITY"
            | "#EXT-X-DISCONTINUITY-SEQUENCE"
            | "#EXT-X-MAP"
            | "#EXT-X-BYTERANGE"
            | "#EXT-X-PROGRAM-DATE-TIME"
            | "#EXT-X-DATERANGE"
            | "#EXT-X-INDEPENDENT-SEGMENTS"
    )
}

fn parse_tag_value<'a>(line: &'a str, tag: &str) -> Result<&'a str, OriginManifestTransientReason> {
    line.strip_prefix(tag)
        .and_then(|value| value.strip_prefix(':'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| parser_feature("missing_tag_value"))
}

fn parse_extinf_duration_ms(line: &str) -> Result<u64, OriginManifestTransientReason> {
    let value = parse_tag_value(line, "#EXTINF")?
        .split_once(',')
        .map_or_else(|| parse_tag_value(line, "#EXTINF").unwrap_or_default(), |(duration, _)| duration)
        .trim();
    parse_decimal_seconds_to_ms(value).ok_or_else(|| parser_feature("invalid_extinf"))
}

fn parse_decimal_seconds_to_ms(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole_ms = whole.parse::<u64>().ok()?.checked_mul(1_000)?;
    let mut fraction_ms = 0_u64;
    let mut multiplier = 100_u64;
    for byte in fraction.bytes().take(3) {
        if !byte.is_ascii_digit() {
            return None;
        }
        fraction_ms = fraction_ms.checked_add(u64::from(byte - b'0').checked_mul(multiplier)?)?;
        multiplier /= 10;
    }
    if fraction.bytes().skip(3).any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    whole_ms.checked_add(fraction_ms)
}

fn parse_byterange_tag(line: &str) -> Result<PendingByteRange, OriginManifestTransientReason> {
    parse_byterange_value(parse_tag_value(line, "#EXT-X-BYTERANGE")?)
}

fn parse_map_byterange(value: &str) -> Result<ParsedByteRange, OriginManifestTransientReason> {
    match parse_byterange_value(value)? {
        PendingByteRange::WithOffset(byte_range) => Ok(byte_range),
        PendingByteRange::WithoutOffset(_) => Err(parser_feature("map_byterange_without_offset")),
    }
}

fn parse_byterange_value(value: &str) -> Result<PendingByteRange, OriginManifestTransientReason> {
    let trimmed = value.trim().trim_matches('"');
    if let Some((length, offset)) = trimmed.split_once('@') {
        return Ok(PendingByteRange::WithOffset(ParsedByteRange {
            length: length.parse_numeric("invalid_byterange_length")?,
            offset: offset.parse_numeric("invalid_byterange_offset")?,
        }));
    }
    Ok(PendingByteRange::WithoutOffset(trimmed.parse_numeric("invalid_byterange_length")?))
}

fn parse_attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_once(':')?
        .1
        .split(',')
        .filter_map(|part| part.split_once('='))
        .find_map(|(key, value)| (key.trim() == name).then(|| value.trim()))
}

fn resolve_uri(base: &str, reference: &str) -> String {
    match rewrite_hls_url(base, reference) {
        Cow::Borrowed(value) => value.to_string(),
        Cow::Owned(value) => value,
    }
}

fn parser_feature(feature: &str) -> OriginManifestTransientReason {
    OriginManifestTransientReason::ParserUnsupportedFeature { feature: feature.to_string() }
}

#[cfg(test)]
mod tests {
    use super::{
        OriginManifestParseOutcome, OriginManifestTransientReason, ParsedByteRange, parse_manifest_timing,
        parse_origin_manifest_timeline, parse_origin_media_manifest,
    };

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn normal_manifest(body: &str) -> super::ParsedOriginManifest {
        match parse_origin_media_manifest(body, BASE_URL) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    #[test]
    fn missing_media_sequence_defaults_to_zero() {
        let manifest = normal_manifest("#EXTM3U\n#EXTINF:4.0,\nseg001.ts\n");

        assert_eq!(manifest.origin_manifest_sequence, 0);
        assert_eq!(manifest.segments[0].origin_seq, 0);
    }

    #[test]
    fn present_media_sequence_is_used() {
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:322\n#EXTINF:4.0,\nseg001.ts\n");

        assert_eq!(manifest.origin_manifest_sequence, 322);
        assert_eq!(manifest.segments[0].origin_seq, 322);
    }

    #[test]
    fn timeline_markers_parse_media_sequence_through_transient_tags() {
        let timeline = parse_origin_manifest_timeline(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:226\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg001.ts\n#EXTINF:4.0,\nseg002.ts\n",
        )
        .expect("timeline markers parse");

        assert_eq!(timeline.origin_manifest_sequence, 226);
        assert_eq!(timeline.origin_manifest_segment_cnt, 2);
        assert_eq!(timeline.origin_highwater(), Some(227));
    }

    #[test]
    fn timeline_markers_default_missing_media_sequence_to_zero() {
        let timeline = parse_origin_manifest_timeline(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
        )
        .expect("timeline markers parse");

        assert_eq!(timeline.origin_manifest_sequence, 0);
        assert_eq!(timeline.origin_manifest_segment_cnt, 1);
    }

    #[test]
    fn extinf_and_segment_uri_create_segment_model() {
        let manifest = normal_manifest("#EXTM3U\n#EXTINF:4.567,\nmedia/seg001.ts\n");
        let segment = &manifest.segments[0];

        assert_eq!(manifest.origin_manifest_segment_cnt, 1);
        assert_eq!(segment.duration_ms, 4_567);
        assert_eq!(segment.resolved_origin_url, "http://origin.example.com/live/final/media/seg001.ts");
    }

    #[test]
    fn ext_x_key_triggers_transient_passthrough() {
        let outcome = parse_origin_media_manifest("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n", BASE_URL);

        assert_eq!(
            outcome,
            OriginManifestParseOutcome::TransientPassthrough { reason: OriginManifestTransientReason::ExtXKey }
        );
    }

    #[test]
    fn unsupported_tag_triggers_transient_passthrough() {
        let outcome = parse_origin_media_manifest("#EXTM3U\n#EXT-X-PART:DURATION=1.0,URI=\"part.ts\"\n", BASE_URL);

        assert_eq!(
            outcome,
            OriginManifestParseOutcome::TransientPassthrough {
                reason: OriginManifestTransientReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() }
            }
        );
    }

    #[test]
    fn ext_x_map_is_parsed_with_resolved_uri() {
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg001.m4s\n");

        assert_eq!(manifest.maps[0].resolved_origin_uri, "http://origin.example.com/live/final/init.mp4");
        assert_eq!(manifest.segments[0].map_ref, Some(0));
    }

    #[test]
    fn byterange_with_explicit_offset_is_parsed() {
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-BYTERANGE:500@1000\n#EXTINF:4.0,\nbig.m4s\n");

        assert_eq!(manifest.segments[0].origin_byte_range, Some(ParsedByteRange { length: 500, offset: 1_000 }));
    }

    #[test]
    fn byterange_without_offset_uses_previous_offset_for_same_uri() {
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-BYTERANGE:500@1000\n#EXTINF:4.0,\nbig.m4s\n#EXT-X-BYTERANGE:250\n#EXTINF:4.0,\nbig.m4s\n",
        );

        assert_eq!(manifest.segments[1].origin_byte_range, Some(ParsedByteRange { length: 250, offset: 1_500 }));
    }

    #[test]
    fn byterange_without_previous_offset_fails_controlled() {
        let outcome = parse_origin_media_manifest("#EXTM3U\n#EXT-X-BYTERANGE:250\n#EXTINF:4.0,\nbig.m4s\n", BASE_URL);

        assert_eq!(
            outcome,
            OriginManifestParseOutcome::TransientPassthrough {
                reason: OriginManifestTransientReason::ParserUnsupportedFeature {
                    feature: "byterange_without_resolvable_offset".to_string()
                }
            }
        );
    }

    #[test]
    fn byterange_offset_overflow_triggers_parser_unsupported_feature() {
        let body = "#EXTM3U\n#EXT-X-BYTERANGE:2@18446744073709551614\n#EXTINF:4.0,\nbig.m4s\n";
        let outcome = parse_origin_media_manifest(body, BASE_URL);

        assert_eq!(
            outcome,
            OriginManifestParseOutcome::TransientPassthrough {
                reason: OriginManifestTransientReason::ParserUnsupportedFeature {
                    feature: "byterange_offset_overflow".to_string()
                }
            }
        );
    }

    #[test]
    fn discontinuity_and_discontinuity_sequence_are_parsed() {
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nseg001.ts\n",
        );

        assert_eq!(manifest.discontinuity_sequence, Some(7));
        assert!(manifest.segments[0].discontinuity_before);
    }

    #[test]
    fn program_date_time_and_daterange_attach_to_next_segment() {
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-05-27T10:11:12Z\n#EXT-X-DATERANGE:ID=\"ad-1\",START-DATE=\"2026-05-27T10:11:12Z\"\n#EXTINF:4.0,\nseg001.ts\n",
        );
        let segment = &manifest.segments[0];

        assert_eq!(segment.program_date_time.as_deref(), Some("2026-05-27T10:11:12Z"));
        assert_eq!(
            segment.daterange_tags_before,
            vec!["#EXT-X-DATERANGE:ID=\"ad-1\",START-DATE=\"2026-05-27T10:11:12Z\"".to_string()]
        );
    }

    #[test]
    fn version_and_target_duration_are_parsed() {
        let manifest =
            normal_manifest("#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:10\n#EXTINF:4.0,\nseg001.ts\n");

        assert_eq!(manifest.version, Some(6));
        assert_eq!(manifest.target_duration, Some(10));
    }

    #[test]
    fn manifest_timing_parses_target_duration_and_last_extinf() {
        let timing = parse_manifest_timing(
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:4.000,\na.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:6.250,\nb.ts\n",
        );

        assert_eq!(timing.target_duration_ms, Some(10_000));
        assert_eq!(timing.last_segment_duration_ms, Some(6_250));
    }

    #[test]
    fn manifest_timing_normalizes_extinf_fraction_to_milliseconds() {
        assert_eq!(
            parse_manifest_timing("#EXTM3U\n#EXTINF:9.23,title\nseg.ts\n").last_segment_duration_ms,
            Some(9_230)
        );
        assert_eq!(parse_manifest_timing("#EXTM3U\n#EXTINF:7,\nseg.ts\n").last_segment_duration_ms, Some(7_000));
    }
}
