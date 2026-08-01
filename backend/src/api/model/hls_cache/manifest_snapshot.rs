use super::{
    media_reserve::{
        HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker,
    },
    terminal_tail::{HlsEncryptionSignature, HlsMapSignature, HlsMediaContainer},
    HlsSession, MapCacheStatus,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Exact manifest delivery from which lease-local playback evidence is derived.
pub(crate) enum HlsLeaseManifestSnapshotInput<'a> {
    NormalCacheTimeline {
        session: &'a HlsSession,
        committed_body: &'a str,
        materialized_body: &'a str,
        stripped_tail_segments: usize,
    },
    TransientPassthrough {
        materialized_body: &'a str,
        source_rendered_at_ms: u64,
    },
}

#[derive(Debug, Clone)]
struct ParsedLeaseMediaUnit {
    duration_ms: u64,
    uri: String,
    discontinuity_before: bool,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<HlsEncryptionSignature>,
}

#[derive(Debug)]
struct ParsedLeaseManifest {
    media_sequence: u64,
    discontinuity_sequence: u64,
    target_duration_ms: u64,
    units: Vec<ParsedLeaseMediaUnit>,
}

enum ParsedEncryptionDirective {
    Clear,
    Encrypt(HlsEncryptionSignature),
}

/// Builds the single authoritative lease snapshot from the exact bytes delivered to the client.
///
/// The normal path additionally binds those bytes to the current shared render and cache
/// timeline. The transient path deliberately remains origin-backed; reserve and terminal
/// policies can therefore distinguish it without inspecting endpoint state.
pub(crate) fn derive_hls_lease_manifest_snapshot(
    input: &HlsLeaseManifestSnapshotInput<'_>,
    delivered_at_ms: u64,
) -> Option<HlsLeaseManifestSnapshot> {
    match input {
        HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
            session,
            committed_body,
            materialized_body,
            stripped_tail_segments,
        } => {
            derive_normal_snapshot(
                session,
                committed_body,
                materialized_body,
                *stripped_tail_segments,
                delivered_at_ms,
            )
        }
        HlsLeaseManifestSnapshotInput::TransientPassthrough { materialized_body, source_rendered_at_ms } => {
            derive_transient_snapshot(materialized_body, *source_rendered_at_ms, delivered_at_ms)
        }
    }
}

fn derive_normal_snapshot(
    session: &HlsSession,
    committed_body: &str,
    materialized_body: &str,
    stripped_tail_segments: usize,
    delivered_at_ms: u64,
) -> Option<HlsLeaseManifestSnapshot> {
    let rendered = session.last_rendered_manifest.as_ref()?;
    if rendered.body != committed_body {
        return None;
    }
    let parsed = parse_lease_manifest(materialized_body)?;
    let visible_len = rendered.segment_proxy_seqs.len().saturating_sub(stripped_tail_segments);
    let visible_proxy_seqs = rendered.segment_proxy_seqs.get(..visible_len)?;
    if parsed.units.len() != visible_proxy_seqs.len() {
        return None;
    }
    let mut segments = Vec::with_capacity(visible_proxy_seqs.len());
    for (proxy_seq, unit) in visible_proxy_seqs.iter().zip(&parsed.units) {
        let entry = session.segments.get(proxy_seq)?;
        if entry.duration_ms != unit.duration_ms || entry.discontinuity_before != unit.discontinuity_before {
            return None;
        }
        let map_ref_ready = entry.map_ref.is_none_or(|map_id| {
            session.maps.get(&map_id).is_some_and(|map| matches!(map.status, MapCacheStatus::Ready { .. }))
        });
        segments.push(HlsLeaseManifestSegment {
            proxy_seq: *proxy_seq,
            duration_ms: entry.duration_ms,
            uri: unit.uri.clone(),
            discontinuity_before: entry.discontinuity_before,
            map_ref_ready,
            encryption: unit.active_encryption.clone(),
        });
    }
    let first_proxy_seq = segments.first()?.proxy_seq;
    if parsed.media_sequence != first_proxy_seq
        || parsed.discontinuity_sequence != rendered.discontinuity_sequence
        || parsed.target_duration_ms != rendered.target_duration_ms
    {
        return None;
    }
    snapshot_from_segments(HlsLeaseManifestSnapshotParts {
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_render_marker: HlsManifestSourceRenderMarker::new(rendered.rendered_at_ms),
        delivered_at_ms,
        discontinuity_sequence: parsed.discontinuity_sequence,
        target_duration_ms: parsed.target_duration_ms,
        segments,
        active_map: parsed.units.last().and_then(|unit| unit.active_map.clone()),
        active_encryption: parsed.units.last().and_then(|unit| unit.active_encryption.clone()),
    })
}

fn derive_transient_snapshot(
    materialized_body: &str,
    source_rendered_at_ms: u64,
    delivered_at_ms: u64,
) -> Option<HlsLeaseManifestSnapshot> {
    let parsed = parse_lease_manifest(materialized_body)?;
    let mut segments = Vec::with_capacity(parsed.units.len());
    for (index, unit) in parsed.units.iter().enumerate() {
        let index = u64::try_from(index).ok()?;
        let proxy_seq = parsed.media_sequence.checked_add(index)?;
        segments.push(HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: unit.duration_ms,
            uri: unit.uri.clone(),
            discontinuity_before: unit.discontinuity_before,
            // This is syntactic MAP evidence only. Transient reserve never treats it as READY cache evidence.
            map_ref_ready: true,
            encryption: unit.active_encryption.clone(),
        });
    }
    snapshot_from_segments(HlsLeaseManifestSnapshotParts {
        delivery_mode: HlsManifestDeliveryMode::TransientPassthrough,
        source_render_marker: HlsManifestSourceRenderMarker::new(source_rendered_at_ms),
        delivered_at_ms,
        discontinuity_sequence: parsed.discontinuity_sequence,
        target_duration_ms: parsed.target_duration_ms,
        segments,
        active_map: parsed.units.last().and_then(|unit| unit.active_map.clone()),
        active_encryption: parsed.units.last().and_then(|unit| unit.active_encryption.clone()),
    })
}

struct HlsLeaseManifestSnapshotParts {
    delivery_mode: HlsManifestDeliveryMode,
    source_render_marker: HlsManifestSourceRenderMarker,
    delivered_at_ms: u64,
    discontinuity_sequence: u64,
    target_duration_ms: u64,
    segments: Vec<HlsLeaseManifestSegment>,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<HlsEncryptionSignature>,
}

fn snapshot_from_segments(parts: HlsLeaseManifestSnapshotParts) -> Option<HlsLeaseManifestSnapshot> {
    let HlsLeaseManifestSnapshotParts {
        delivery_mode,
        source_render_marker,
        delivered_at_ms,
        discontinuity_sequence,
        target_duration_ms,
        segments,
        active_map,
        active_encryption,
    } = parts;
    let first_proxy_seq = segments.first()?.proxy_seq;
    let last_proxy_seq = segments.last()?.proxy_seq;
    let playlist_duration_ms =
        segments.iter().fold(0_u64, |duration_ms, segment| duration_ms.saturating_add(segment.duration_ms));
    let container = classify_container(active_map.as_ref(), &segments);
    Some(HlsLeaseManifestSnapshot {
        delivery_mode,
        source_render_marker,
        // Assigned atomically by the lease store when these exact bytes become publishable.
        snapshot_generation: 0,
        delivered_at_ms,
        first_proxy_seq,
        last_proxy_seq,
        visible_segments: Arc::from(segments),
        discontinuity_sequence,
        target_duration_ms,
        playlist_duration_ms,
        last_visible_media_end_ms: playlist_duration_ms,
        active_map,
        active_encryption,
        container,
    })
}

fn classify_container(active_map: Option<&HlsMapSignature>, segments: &[HlsLeaseManifestSegment]) -> HlsMediaContainer {
    if active_map.is_some() {
        HlsMediaContainer::FragmentedMp4
    } else if segments
        .iter()
        .all(|segment| uri_extension(&segment.uri).is_some_and(|ext| ext.eq_ignore_ascii_case("ts")))
    {
        HlsMediaContainer::MpegTs
    } else {
        HlsMediaContainer::Unknown
    }
}

fn uri_extension(uri: &str) -> Option<&str> {
    let path = uri.split(['?', '#']).next()?;
    path.rsplit_once('.').map(|(_, extension)| extension).filter(|extension| !extension.is_empty())
}

fn parse_lease_manifest(body: &str) -> Option<ParsedLeaseManifest> {
    let mut media_sequence = 0_u64;
    let mut discontinuity_sequence = 0_u64;
    let mut target_duration_ms = None;
    let mut pending_duration_ms = None;
    let mut pending_discontinuity = false;
    let mut active_map = None;
    let mut active_encryption = None;
    let mut units = Vec::new();

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.trim().parse().ok()?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            discontinuity_sequence = value.trim().parse().ok()?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration_ms = Some(parse_decimal_millis(value.trim())?);
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let duration = value.split_once(',').map_or(value, |(duration, _)| duration);
            pending_duration_ms = Some(parse_decimal_millis(duration.trim())?);
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if line.starts_with("#EXT-X-MAP:") {
            active_map = Some(HlsMapSignature {
                fingerprint: Sha256::digest(line.as_bytes()).into(),
                container: HlsMediaContainer::FragmentedMp4,
            });
        } else if line.starts_with("#EXT-X-KEY:") {
            active_encryption = match parse_encryption_directive(line)? {
                ParsedEncryptionDirective::Clear => None,
                ParsedEncryptionDirective::Encrypt(signature) => Some(signature),
            };
        } else if !line.starts_with('#') {
            let Some(duration_ms) = pending_duration_ms.take() else {
                continue;
            };
            units.push(ParsedLeaseMediaUnit {
                duration_ms,
                uri: line.to_string(),
                discontinuity_before: std::mem::take(&mut pending_discontinuity),
                active_map: active_map.clone(),
                active_encryption: active_encryption.clone(),
            });
        }
    }
    let target_duration_ms = target_duration_ms?;
    (!units.is_empty()).then_some(ParsedLeaseManifest {
        media_sequence,
        discontinuity_sequence,
        target_duration_ms,
        units,
    })
}

fn parse_decimal_millis(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.parse::<u64>().ok()?.saturating_mul(1_000);
    let mut fraction_ms = 0_u64;
    let mut scale = 100_u64;
    for byte in fraction.bytes().take(3) {
        if !byte.is_ascii_digit() {
            return None;
        }
        fraction_ms = fraction_ms.saturating_add(u64::from(byte - b'0').saturating_mul(scale));
        scale /= 10;
    }
    if fraction.bytes().skip(3).any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    Some(whole_ms.saturating_add(fraction_ms))
}

fn parse_encryption_directive(line: &str) -> Option<ParsedEncryptionDirective> {
    let attributes = line.strip_prefix("#EXT-X-KEY:")?;
    let method = attribute_value(attributes, "METHOD").unwrap_or_default();
    if method.eq_ignore_ascii_case("NONE") {
        return Some(ParsedEncryptionDirective::Clear);
    }
    let key_format = attribute_value(attributes, "KEYFORMAT");
    let can_reset_to_clear = method.eq_ignore_ascii_case("AES-128")
        && key_format.as_deref().is_none_or(|format| format.eq_ignore_ascii_case("identity"));
    Some(ParsedEncryptionDirective::Encrypt(HlsEncryptionSignature {
        method,
        key_uri: attribute_value(attributes, "URI"),
        iv: attribute_value(attributes, "IV"),
        key_format,
        key_format_versions: attribute_value(attributes, "KEYFORMATVERSIONS"),
        can_reset_to_clear,
    }))
}

fn attribute_value(attributes: &str, expected_name: &str) -> Option<String> {
    split_attribute_list(attributes).find_map(|attribute| {
        let (name, value) = attribute.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case(expected_name) {
            return None;
        }
        let value = value.trim();
        Some(value.strip_prefix('"').and_then(|value| value.strip_suffix('"')).unwrap_or(value).to_string())
    })
}

fn split_attribute_list(attributes: &str) -> impl Iterator<Item = &str> {
    let mut in_quotes = false;
    attributes.split(move |character| {
        if character == '"' {
            in_quotes = !in_quotes;
            false
        } else {
            character == ',' && !in_quotes
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_encryption_tracks_key_rotation_and_method_none_per_segment() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:9\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"/key-a\",IV=0x1,KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
             #EXTINF:4.0,\n9.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4.0,\n10.ts\n",
        )
        .expect("valid manifest");

        let first = parsed.units[0].active_encryption.as_ref().expect("encrypted first segment");
        assert_eq!(first.method, "AES-128");
        assert_eq!(first.key_uri.as_deref(), Some("/key-a"));
        assert_eq!(first.iv.as_deref(), Some("0x1"));
        assert_eq!(first.key_format.as_deref(), Some("identity"));
        assert_eq!(first.key_format_versions.as_deref(), Some("1"));
        assert!(first.can_reset_to_clear);
        assert!(parsed.units[1].active_encryption.is_none());
    }

    #[test]
    fn unsupported_key_format_is_retained_but_not_resettable() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"/drm\",KEYFORMAT=\"com.example\"\n#EXTINF:4,\n0.ts\n",
        )
        .expect("valid manifest");
        let encryption = parsed.units[0].active_encryption.as_ref().expect("encryption signature");

        assert_eq!(encryption.method, "SAMPLE-AES");
        assert_eq!(encryption.key_format.as_deref(), Some("com.example"));
        assert!(!encryption.can_reset_to_clear);
    }

    #[test]
    fn transient_parser_accepts_decimal_target_duration_and_skips_uri_without_extinf() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4.0\n#EXT-X-MEDIA-SEQUENCE:9\n\
             stray-without-extinf.ts\n#EXTINF:4.0,\n9.ts\n#EXTINF:3.5,\n10.ts\n",
        )
        .expect("valid media units survive unrelated stray URI");

        assert_eq!(parsed.target_duration_ms, 4_000);
        assert_eq!(parsed.units.len(), 2);
        assert_eq!(parsed.units[0].uri, "9.ts");
        assert_eq!(parsed.units[1].duration_ms, 3_500);
    }

    #[test]
    fn transient_snapshot_retains_client_visible_key_and_typed_delivery_mode() {
        let snapshot = derive_hls_lease_manifest_snapshot(
            &HlsLeaseManifestSnapshotInput::TransientPassthrough {
                materialized_body: "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/session/lease/r/key.bin\"\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/20.ts\n",
                source_rendered_at_ms: 10,
            },
            12,
        )
        .expect("snapshot");

        assert_eq!(snapshot.delivery_mode, HlsManifestDeliveryMode::TransientPassthrough);
        assert_eq!(snapshot.source_render_marker.rendered_at_ms(), 10);
        assert_eq!(snapshot.snapshot_generation, 0);
        assert_eq!(snapshot.delivered_at_ms, 12);
        assert_eq!(snapshot.first_proxy_seq, 20);
        assert_eq!(
            snapshot.active_encryption.as_ref().and_then(|key| key.key_uri.as_deref()),
            Some("/hls/shared/live/session/lease/r/key.bin")
        );
    }
}
