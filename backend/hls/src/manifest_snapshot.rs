use super::{
    manifest_limits::{
        HlsManifestLimitKind, HlsManifestLimitViolation, MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES,
        MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES, MAX_HLS_LEASE_SNAPSHOT_SEGMENTS, MAX_HLS_LEASE_SNAPSHOT_URI_BYTES,
    },
    media_reserve::{
        HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeaseManifestUriMaterialization,
        HlsManifestCommitIdentity, HlsManifestDeliveryMode,
    },
    terminal_tail::{HlsEncryptionSignature, HlsMapSignature, HlsMediaContainer},
    HlsSession, MapCacheStatus,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Exact manifest delivery from which lease-local playback evidence is derived.
pub enum HlsLeaseManifestSnapshotInput<'a> {
    NormalCacheTimeline {
        session: &'a HlsSession,
        committed_body: &'a str,
        materialized_body: &'a str,
        stripped_tail_segments: usize,
    },
    TransientPassthrough {
        materialized_body: &'a str,
        source_commit_identity: HlsManifestCommitIdentity,
        finalized_manifest_generation: Option<super::TransientManifestGeneration>,
    },
    TransientPassthroughTemplate {
        template: &'a HlsTransientManifestTemplate,
        source_commit_identity: HlsManifestCommitIdentity,
        uri_materialization: HlsLeaseManifestUriMaterialization,
        finalized_manifest_generation: Option<super::TransientManifestGeneration>,
    },
}

#[derive(Debug, Clone)]
struct ParsedLeaseMediaUnit {
    duration_ms: u64,
    uri: String,
    discontinuity_before: bool,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<Arc<HlsEncryptionSignature>>,
}

#[derive(Debug)]
struct ParsedLeaseManifest {
    media_sequence: u64,
    discontinuity_sequence: u64,
    target_duration_ms: u64,
    units: Vec<ParsedLeaseMediaUnit>,
    estimated_metadata_bytes: usize,
    uri_bytes: usize,
}

/// Lease-independent representation of one committed transient manifest.
#[derive(Debug)]
pub struct HlsTransientManifestTemplate {
    discontinuity_sequence: u64,
    target_duration_ms: u64,
    segments: Arc<[HlsLeaseManifestSegment]>,
    playlist_duration_ms: u64,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<Arc<HlsEncryptionSignature>>,
    container: HlsMediaContainer,
    estimated_metadata_bytes: usize,
    uri_bytes: usize,
}

impl HlsTransientManifestTemplate {
    pub(crate) const fn estimated_metadata_bytes(&self) -> usize { self.estimated_metadata_bytes }

    pub(crate) const fn playlist_duration_ms(&self) -> u64 { self.playlist_duration_ms }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &Arc<[HlsLeaseManifestSegment]> { &self.segments }
}

enum ParsedEncryptionDirective {
    Clear,
    Encrypt(Arc<HlsEncryptionSignature>),
}

/// Builds the single authoritative lease snapshot from the exact bytes delivered to the client.
///
/// The normal path additionally binds those bytes to the current shared render and cache
/// timeline. The transient path deliberately remains origin-backed; reserve and terminal
/// policies can therefore distinguish it without inspecting endpoint state.
pub fn derive_hls_lease_manifest_snapshot(
    input: &HlsLeaseManifestSnapshotInput<'_>,
    delivered_at_ms: u64,
) -> Result<Option<HlsLeaseManifestSnapshot>, HlsManifestLimitViolation> {
    match input {
        HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
            session,
            committed_body,
            materialized_body,
            stripped_tail_segments,
        } => {
            derive_normal_snapshot(session, committed_body, materialized_body, *stripped_tail_segments, delivered_at_ms)
        }
        HlsLeaseManifestSnapshotInput::TransientPassthrough {
            materialized_body,
            source_commit_identity,
            finalized_manifest_generation,
        } => derive_transient_materialized_snapshot(
            materialized_body,
            *source_commit_identity,
            *finalized_manifest_generation,
            delivered_at_ms,
        ),
        HlsLeaseManifestSnapshotInput::TransientPassthroughTemplate {
            template,
            source_commit_identity,
            uri_materialization,
            finalized_manifest_generation,
        } => Ok(derive_transient_snapshot(
            template,
            *source_commit_identity,
            Some(uri_materialization.clone()),
            *finalized_manifest_generation,
            delivered_at_ms,
        )),
    }
}

fn derive_transient_materialized_snapshot(
    materialized_body: &str,
    source_commit_identity: HlsManifestCommitIdentity,
    finalized_manifest_generation: Option<super::TransientManifestGeneration>,
    delivered_at_ms: u64,
) -> Result<Option<HlsLeaseManifestSnapshot>, HlsManifestLimitViolation> {
    let Some(template) = parse_hls_transient_manifest_template(materialized_body)? else {
        return Ok(None);
    };
    Ok(derive_transient_snapshot(
        &template,
        source_commit_identity,
        None,
        finalized_manifest_generation,
        delivered_at_ms,
    ))
}

fn derive_normal_snapshot(
    session: &HlsSession,
    committed_body: &str,
    materialized_body: &str,
    stripped_tail_segments: usize,
    delivered_at_ms: u64,
) -> Result<Option<HlsLeaseManifestSnapshot>, HlsManifestLimitViolation> {
    let Some(rendered) = session.last_rendered_manifest.as_ref() else {
        return Ok(None);
    };
    if rendered.body != committed_body {
        return Ok(None);
    }
    let Some(source_commit_identity) = session.last_normal_manifest_commit_identity() else {
        return Ok(None);
    };
    let Some(parsed) = parse_lease_manifest(materialized_body)? else {
        return Ok(None);
    };
    Ok((|| {
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
                uri: Arc::from(unit.uri.as_str()),
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
            source_commit_identity,
            uri_materialization: None,
            finalized_transient_manifest_generation: None,
            delivered_at_ms,
            discontinuity_sequence: parsed.discontinuity_sequence,
            target_duration_ms: parsed.target_duration_ms,
            segments,
            active_map: parsed.units.last().and_then(|unit| unit.active_map.clone()),
            active_encryption: parsed.units.last().and_then(|unit| unit.active_encryption.clone()),
        })
    })())
}

fn derive_transient_snapshot(
    template: &HlsTransientManifestTemplate,
    source_commit_identity: HlsManifestCommitIdentity,
    uri_materialization: Option<HlsLeaseManifestUriMaterialization>,
    finalized_manifest_generation: Option<super::TransientManifestGeneration>,
    delivered_at_ms: u64,
) -> Option<HlsLeaseManifestSnapshot> {
    let first_proxy_seq = template.segments.first()?.proxy_seq;
    let last_proxy_seq = template.segments.last()?.proxy_seq;
    Some(HlsLeaseManifestSnapshot {
        delivery_mode: HlsManifestDeliveryMode::TransientPassthrough,
        source_commit_identity,
        uri_materialization,
        finalized_transient_manifest_generation: finalized_manifest_generation,
        snapshot_generation: 0,
        delivered_at_ms,
        first_proxy_seq,
        last_proxy_seq,
        visible_segments: Arc::clone(&template.segments),
        discontinuity_sequence: template.discontinuity_sequence,
        target_duration_ms: template.target_duration_ms,
        playlist_duration_ms: template.playlist_duration_ms,
        last_visible_media_end_ms: template.playlist_duration_ms,
        active_map: template.active_map.clone(),
        active_encryption: template.active_encryption.clone(),
        container: template.container,
    })
}

struct HlsLeaseManifestSnapshotParts {
    delivery_mode: HlsManifestDeliveryMode,
    source_commit_identity: HlsManifestCommitIdentity,
    uri_materialization: Option<HlsLeaseManifestUriMaterialization>,
    finalized_transient_manifest_generation: Option<super::TransientManifestGeneration>,
    delivered_at_ms: u64,
    discontinuity_sequence: u64,
    target_duration_ms: u64,
    segments: Vec<HlsLeaseManifestSegment>,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<Arc<HlsEncryptionSignature>>,
}

fn snapshot_from_segments(parts: HlsLeaseManifestSnapshotParts) -> Option<HlsLeaseManifestSnapshot> {
    let HlsLeaseManifestSnapshotParts {
        delivery_mode,
        source_commit_identity,
        uri_materialization,
        finalized_transient_manifest_generation,
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
        source_commit_identity,
        uri_materialization,
        finalized_transient_manifest_generation,
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

struct LeaseManifestParser {
    media_sequence: u64,
    discontinuity_sequence: u64,
    target_duration_ms: Option<u64>,
    pending_duration_ms: Option<u64>,
    pending_discontinuity: bool,
    active_map: Option<HlsMapSignature>,
    active_encryption: Option<Arc<HlsEncryptionSignature>>,
    units: Vec<ParsedLeaseMediaUnit>,
    prior_segment_count: usize,
    uri_bytes: usize,
    metadata_bytes: usize,
}

impl LeaseManifestParser {
    fn new() -> Self {
        Self {
            media_sequence: 0,
            discontinuity_sequence: 0,
            target_duration_ms: None,
            pending_duration_ms: None,
            pending_discontinuity: false,
            active_map: None,
            active_encryption: None,
            units: Vec::new(),
            prior_segment_count: 0,
            uri_bytes: 0,
            metadata_bytes: 0,
        }
    }

    fn from_template(template: &HlsTransientManifestTemplate) -> Self {
        Self {
            media_sequence: template.segments.first().map_or(0, |segment| segment.proxy_seq),
            discontinuity_sequence: template.discontinuity_sequence,
            target_duration_ms: Some(template.target_duration_ms),
            pending_duration_ms: None,
            pending_discontinuity: false,
            active_map: template.active_map.clone(),
            active_encryption: template.active_encryption.clone(),
            units: Vec::new(),
            prior_segment_count: template.segments.len(),
            uri_bytes: template.uri_bytes,
            metadata_bytes: template.estimated_metadata_bytes,
        }
    }

    fn parse(mut self, body: &str) -> Result<Option<ParsedLeaseManifest>, HlsManifestLimitViolation> {
        for raw_line in body.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                let Ok(parsed) = value.trim().parse() else {
                    return Ok(None);
                };
                self.media_sequence = parsed;
            } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
                let Ok(parsed) = value.trim().parse() else {
                    return Ok(None);
                };
                self.discontinuity_sequence = parsed;
            } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                let Some(parsed) = parse_decimal_millis(value.trim()) else {
                    return Ok(None);
                };
                self.target_duration_ms = Some(parsed);
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                let duration = value.split_once(',').map_or(value, |(duration, _)| duration);
                let Some(parsed) = parse_decimal_millis(duration.trim()) else {
                    return Ok(None);
                };
                self.pending_duration_ms = Some(parsed);
            } else if line == "#EXT-X-DISCONTINUITY" {
                self.pending_discontinuity = true;
            } else if line.starts_with("#EXT-X-MAP:") {
                self.active_map = Some(HlsMapSignature {
                    fingerprint: Sha256::digest(line.as_bytes()).into(),
                    container: HlsMediaContainer::FragmentedMp4,
                });
            } else if line.starts_with("#EXT-X-KEY:") {
                let Some(directive) = parse_checked_encryption_directive(line, &mut self.metadata_bytes)? else {
                    return Ok(None);
                };
                self.active_encryption = match directive {
                    ParsedEncryptionDirective::Clear => None,
                    ParsedEncryptionDirective::Encrypt(signature) => Some(signature),
                };
            } else if !line.starts_with('#') {
                let Some(duration_ms) = self.pending_duration_ms.take() else {
                    continue;
                };
                let segment_count = self.prior_segment_count.saturating_add(self.units.len()).saturating_add(1);
                ensure_lease_snapshot_limit(
                    HlsManifestLimitKind::LeaseSnapshotSegments,
                    segment_count,
                    MAX_HLS_LEASE_SNAPSHOT_SEGMENTS,
                )?;
                self.uri_bytes = self.uri_bytes.saturating_add(line.len());
                ensure_lease_snapshot_limit(
                    HlsManifestLimitKind::LeaseSnapshotUriBytes,
                    self.uri_bytes,
                    MAX_HLS_LEASE_SNAPSHOT_URI_BYTES,
                )?;
                self.metadata_bytes = self
                    .metadata_bytes
                    .saturating_add(std::mem::size_of::<ParsedLeaseMediaUnit>())
                    .saturating_add(line.len());
                ensure_lease_snapshot_limit(
                    HlsManifestLimitKind::LeaseSnapshotMetadataBytes,
                    self.metadata_bytes,
                    MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES,
                )?;
                self.units.push(ParsedLeaseMediaUnit {
                    duration_ms,
                    uri: line.to_string(),
                    discontinuity_before: std::mem::take(&mut self.pending_discontinuity),
                    active_map: self.active_map.clone(),
                    active_encryption: self.active_encryption.clone(),
                });
            }
        }
        let Some(target_duration_ms) = self.target_duration_ms else {
            return Ok(None);
        };
        Ok((!self.units.is_empty()).then_some(ParsedLeaseManifest {
            media_sequence: self.media_sequence,
            discontinuity_sequence: self.discontinuity_sequence,
            target_duration_ms,
            units: self.units,
            estimated_metadata_bytes: self.metadata_bytes,
            uri_bytes: self.uri_bytes,
        }))
    }
}

fn parse_lease_manifest(body: &str) -> Result<Option<ParsedLeaseManifest>, HlsManifestLimitViolation> {
    LeaseManifestParser::new().parse(body)
}

pub(crate) fn parse_hls_transient_manifest_template(
    body: &str,
) -> Result<Option<Arc<HlsTransientManifestTemplate>>, HlsManifestLimitViolation> {
    let Some(parsed) = parse_lease_manifest(body)? else {
        return Ok(None);
    };
    let active_map = parsed.units.last().and_then(|unit| unit.active_map.clone());
    let active_encryption = parsed.units.last().and_then(|unit| unit.active_encryption.clone());
    let mut segments = Vec::with_capacity(parsed.units.len());
    for (index, unit) in parsed.units.into_iter().enumerate() {
        let Ok(index) = u64::try_from(index) else {
            return Ok(None);
        };
        let Some(proxy_seq) = parsed.media_sequence.checked_add(index) else {
            return Ok(None);
        };
        segments.push(HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: unit.duration_ms,
            uri: Arc::from(unit.uri),
            discontinuity_before: unit.discontinuity_before,
            // This is syntactic MAP evidence only. Transient reserve never treats it as READY cache evidence.
            map_ref_ready: true,
            encryption: unit.active_encryption,
        });
    }
    let playlist_duration_ms = segments.iter().fold(0_u64, |total, segment| total.saturating_add(segment.duration_ms));
    let container = classify_container(active_map.as_ref(), &segments);
    Ok(Some(Arc::new(HlsTransientManifestTemplate {
        discontinuity_sequence: parsed.discontinuity_sequence,
        target_duration_ms: parsed.target_duration_ms,
        segments: Arc::from(segments),
        playlist_duration_ms,
        active_map,
        active_encryption,
        container,
        estimated_metadata_bytes: parsed.estimated_metadata_bytes,
        uri_bytes: parsed.uri_bytes,
    })))
}

pub(crate) fn extend_hls_transient_manifest_template(
    previous: &Arc<HlsTransientManifestTemplate>,
    rewritten_suffix: &str,
) -> Result<Option<Arc<HlsTransientManifestTemplate>>, HlsManifestLimitViolation> {
    let Some(parsed) = LeaseManifestParser::from_template(previous).parse(rewritten_suffix)? else {
        return Ok(None);
    };
    let active_map = parsed.units.last().and_then(|unit| unit.active_map.clone());
    let active_encryption = parsed.units.last().and_then(|unit| unit.active_encryption.clone());
    let Some(mut next_proxy_seq) = previous.segments.last().and_then(|segment| segment.proxy_seq.checked_add(1)) else {
        return Ok(None);
    };
    let mut segments = Vec::with_capacity(previous.segments.len().saturating_add(parsed.units.len()));
    segments.extend(previous.segments.iter().cloned());
    for unit in parsed.units {
        segments.push(HlsLeaseManifestSegment {
            proxy_seq: next_proxy_seq,
            duration_ms: unit.duration_ms,
            uri: Arc::from(unit.uri),
            discontinuity_before: unit.discontinuity_before,
            map_ref_ready: true,
            encryption: unit.active_encryption,
        });
        let Some(next) = next_proxy_seq.checked_add(1) else {
            return Ok(None);
        };
        next_proxy_seq = next;
    }
    let appended_duration_ms = segments[previous.segments.len()..]
        .iter()
        .fold(0_u64, |total, segment| total.saturating_add(segment.duration_ms));
    let container = if active_map.is_some() {
        HlsMediaContainer::FragmentedMp4
    } else if previous.container == HlsMediaContainer::MpegTs
        && segments[previous.segments.len()..]
            .iter()
            .all(|segment| uri_extension(&segment.uri).is_some_and(|ext| ext.eq_ignore_ascii_case("ts")))
    {
        HlsMediaContainer::MpegTs
    } else {
        HlsMediaContainer::Unknown
    };
    Ok(Some(Arc::new(HlsTransientManifestTemplate {
        discontinuity_sequence: parsed.discontinuity_sequence,
        target_duration_ms: parsed.target_duration_ms,
        segments: Arc::from(segments),
        playlist_duration_ms: previous.playlist_duration_ms.saturating_add(appended_duration_ms),
        active_map,
        active_encryption,
        container,
        estimated_metadata_bytes: parsed.estimated_metadata_bytes,
        uri_bytes: parsed.uri_bytes,
    })))
}

fn ensure_lease_snapshot_limit(
    kind: HlsManifestLimitKind,
    actual: usize,
    limit: usize,
) -> Result<(), HlsManifestLimitViolation> {
    if actual > limit {
        return Err(HlsManifestLimitViolation::new(kind, actual, limit));
    }
    Ok(())
}

fn parse_checked_encryption_directive(
    line: &str,
    metadata_bytes: &mut usize,
) -> Result<Option<ParsedEncryptionDirective>, HlsManifestLimitViolation> {
    ensure_lease_snapshot_limit(
        HlsManifestLimitKind::LeaseSnapshotEncryptionDirectiveBytes,
        line.len(),
        MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES,
    )?;
    *metadata_bytes = (*metadata_bytes).saturating_add(line.len());
    ensure_lease_snapshot_limit(
        HlsManifestLimitKind::LeaseSnapshotMetadataBytes,
        *metadata_bytes,
        MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES,
    )?;
    Ok(parse_encryption_directive(line))
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
    Some(ParsedEncryptionDirective::Encrypt(Arc::new(HlsEncryptionSignature {
        method,
        key_uri: attribute_value(attributes, "URI"),
        iv: attribute_value(attributes, "IV"),
        key_format,
        key_format_versions: attribute_value(attributes, "KEYFORMATVERSIONS"),
        can_reset_to_clear,
    })))
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
    use std::fmt::Write as _;

    fn lease_manifest_with_segments(segment_count: usize) -> String {
        let mut body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:1\n");
        for index in 0..segment_count {
            writeln!(body, "#EXTINF:6,\n{index}.ts").expect("synthetic lease manifest renders");
        }
        body
    }

    #[test]
    fn active_encryption_tracks_key_rotation_and_method_none_per_segment() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:9\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"/key-a\",IV=0x1,KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
             #EXTINF:4.0,\n9.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4.0,\n10.ts\n",
        )
        .expect("within snapshot limits")
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
    fn consecutive_encrypted_segments_share_one_encryption_signature() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"/key-a\",KEYFORMAT=\"identity\"\n\
             #EXTINF:4,\n0.ts\n#EXTINF:4,\n1.ts\n",
        )
        .expect("within snapshot limits")
        .expect("valid manifest");

        let first = parsed.units[0].active_encryption.as_ref().expect("first encryption");
        let second = parsed.units[1].active_encryption.as_ref().expect("second encryption");
        assert!(Arc::ptr_eq(first, second));
    }

    #[test]
    fn oversized_encryption_directive_is_rejected_before_attribute_allocation() {
        let key_format = "x".repeat(MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES);
        let body = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,KEYFORMAT=\"{key_format}\"\n#EXTINF:4,\n0.ts\n"
        );

        let violation = parse_lease_manifest(&body).expect_err("oversized key directive is rejected");

        assert_eq!(violation.kind, HlsManifestLimitKind::LeaseSnapshotEncryptionDirectiveBytes);
        assert!(violation.actual > MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES);
    }

    #[test]
    fn cumulative_encryption_metadata_is_bounded() {
        let mut body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
        let key_format = "x".repeat(MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES / 2);
        while body.len() <= MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES {
            writeln!(body, "#EXT-X-KEY:METHOD=AES-128,URI=\"/key\",KEYFORMAT=\"{key_format}\"")
                .expect("synthetic key directive renders");
        }
        body.push_str("#EXTINF:4,\n0.ts\n");

        let violation = parse_lease_manifest(&body).expect_err("cumulative metadata is rejected");

        assert_eq!(violation.kind, HlsManifestLimitKind::LeaseSnapshotMetadataBytes);
        assert!(violation.actual > MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES);
    }

    #[test]
    fn unsupported_key_format_is_retained_but_not_resettable() {
        let parsed = parse_lease_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"/drm\",KEYFORMAT=\"com.example\"\n#EXTINF:4,\n0.ts\n",
        )
        .expect("within snapshot limits")
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
        .expect("within snapshot limits")
        .expect("valid media units survive unrelated stray URI");

        assert_eq!(parsed.target_duration_ms, 4_000);
        assert_eq!(parsed.units.len(), 2);
        assert_eq!(parsed.units[0].uri, "9.ts");
        assert_eq!(parsed.units[1].duration_ms, 3_500);
    }

    #[test]
    fn transient_snapshot_retains_client_visible_key_and_typed_delivery_mode() {
        let manifest_generation = super::super::TransientManifestGeneration::for_test(7);
        let snapshot = derive_hls_lease_manifest_snapshot(
            &HlsLeaseManifestSnapshotInput::TransientPassthrough {
                materialized_body: "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/session/lease/r/key.bin\"\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/20.ts\n",
                source_commit_identity: HlsManifestCommitIdentity::committed(1, 10),
                finalized_manifest_generation: Some(manifest_generation),
            },
            12,
        )
        .expect("within snapshot limits")
        .expect("snapshot");

        assert_eq!(snapshot.delivery_mode, HlsManifestDeliveryMode::TransientPassthrough);
        assert_eq!(snapshot.source_commit_identity.rendered_at_ms(), 10);
        assert_eq!(snapshot.finalized_transient_manifest_generation, Some(manifest_generation));
        assert_eq!(snapshot.snapshot_generation, 0);
        assert_eq!(snapshot.delivered_at_ms, 12);
        assert_eq!(snapshot.first_proxy_seq, 20);
        assert_eq!(
            snapshot.active_encryption.as_ref().and_then(|key| key.key_uri.as_deref()),
            Some("/hls/shared/live/session/lease/r/key.bin")
        );
    }

    #[test]
    fn transient_template_segments_are_shared_across_lease_snapshots() {
        let template = parse_hls_transient_manifest_template(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n\
             #EXTINF:4,\n/hls/shared/live/session/__hls_access_lease_id__/r/20.ts\n",
        )
        .expect("within snapshot limits")
        .expect("template");
        let snapshot = |lease_id: &str| {
            derive_hls_lease_manifest_snapshot(
                &HlsLeaseManifestSnapshotInput::TransientPassthroughTemplate {
                    template: &template,
                    source_commit_identity: HlsManifestCommitIdentity::committed(1, 10),
                    uri_materialization: HlsLeaseManifestUriMaterialization::new(
                        &super::super::HlsAccessLeaseId(lease_id.to_string()),
                        None,
                    ),
                    finalized_manifest_generation: None,
                },
                12,
            )
            .expect("within snapshot limits")
            .expect("snapshot")
        };

        let first = snapshot("lease-a");
        let second = snapshot("lease-b");

        assert!(Arc::ptr_eq(&first.visible_segments, &second.visible_segments));
        assert_eq!(first.materialize_uri(&first.visible_segments[0].uri), "/hls/shared/live/session/lease-a/r/20.ts");
        assert_eq!(second.materialize_uri(&second.visible_segments[0].uri), "/hls/shared/live/session/lease-b/r/20.ts");
    }

    #[test]
    fn lease_snapshot_segment_limit_accepts_boundary_and_rejects_next_segment() {
        let boundary = parse_lease_manifest(&lease_manifest_with_segments(MAX_HLS_LEASE_SNAPSHOT_SEGMENTS))
            .expect("boundary stays within limits")
            .expect("boundary manifest parses");
        assert_eq!(boundary.units.len(), MAX_HLS_LEASE_SNAPSHOT_SEGMENTS);

        let violation = parse_lease_manifest(&lease_manifest_with_segments(MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1))
            .expect_err("next segment exceeds snapshot limit");
        assert_eq!(violation.kind, HlsManifestLimitKind::LeaseSnapshotSegments);
        assert_eq!(violation.actual, MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1);
    }

    #[test]
    fn lease_snapshot_uri_byte_limit_rejects_before_uri_clone() {
        let oversized_uri = "x".repeat(MAX_HLS_LEASE_SNAPSHOT_URI_BYTES + 1);
        let body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{oversized_uri}\n");

        let violation = parse_lease_manifest(&body).expect_err("URI byte overflow is rejected");

        assert_eq!(violation.kind, HlsManifestLimitKind::LeaseSnapshotUriBytes);
        assert_eq!(violation.actual, MAX_HLS_LEASE_SNAPSHOT_URI_BYTES + 1);
    }
}
