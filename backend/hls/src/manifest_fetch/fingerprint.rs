//! Proving that two origin manifests describe the same timeline - or that they
//! deterministically cannot.
//!
//! Fingerprints reduce a parsed manifest to a hash over the identity-bearing
//! parts (segment URIs, MAP and KEY tags, durations, program dates). A
//! deterministic conflict is a fingerprint mismatch the burst has proven is not
//! a transient disagreement, and the receipt of that verdict is recorded on the
//! session so later attempts do not re-litigate it.

use super::{
    episode::HlsCommittedResourceEvidence, error::HlsManifestRejectLogReason, recovery::HlsManifestRecoveryCandidate,
    FetchedOriginManifest, HlsOriginManifestFetchContext,
};
use crate::{
    deterministic_conflict::{
        HlsDeterministicConflictFingerprint, HlsDeterministicConflictSegmentFingerprint,
        HlsDeterministicTimelineConflict,
    },
    manifest_acceptance::{
        HlsDeterministicConflictReceipt, HlsEmergencyAcceptanceEvidence, HlsEmergencyLiveHandoffCompatibility,
        HlsManifestAcceptanceGeneration, HlsManifestCandidateObservation, HlsManifestSegmentFingerprint,
        HlsManifestTimelineFingerprint, HlsTerminalAlternativeCompatibility, HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT,
    },
    resource_identity::HlsMediaResourceIdentity,
    timeline::HlsResourceReplayDecision,
};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tuliprox_parser::hls::origin_manifest::{
    parse_origin_manifest_timeline, parse_origin_media_manifest, OriginManifestParseOutcome, ParsedOriginManifest,
};
use url::Url;

pub(super) fn deterministic_timeline_conflict_for_candidate(
    candidate: &HlsManifestRecoveryCandidate,
    evidence: &HlsCommittedResourceEvidence,
) -> Option<HlsDeterministicTimelineConflict> {
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&candidate.fetched.body, &candidate.fetched.final_manifest_url)
    else {
        return None;
    };
    let candidate_fingerprint =
        deterministic_conflict_fingerprint(&manifest, &candidate.fetched.body, &candidate.fetched.final_manifest_url);
    let mut saw_new = false;
    for (candidate_position, segment) in manifest.segments.iter().enumerate() {
        let identity = HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range);
        let resource_key = identity.semantic_key();
        let published =
            evidence.published_entries.iter().find(|(published, _)| published.semantic_key() == resource_key);
        if let Some((_, existing_proxy_seq)) = published {
            if saw_new {
                return Some(HlsDeterministicTimelineConflict {
                    previous_proxy_tail: evidence.previous_proxy_tail,
                    existing_proxy_seq: *existing_proxy_seq,
                    candidate_position,
                    candidate_origin_seq: segment.origin_seq,
                    resource_key,
                    decision: HlsResourceReplayDecision::RejectContradictoryOrder,
                    candidate_fingerprint,
                });
            }
        } else {
            saw_new = true;
        }
    }
    None
}

pub(super) fn deterministic_conflict_proven_by_full_burst(
    initial: Option<&HlsDeterministicTimelineConflict>,
    evaluations: &[(HlsManifestCandidateObservation, Option<HlsDeterministicTimelineConflict>)],
    fetched_candidates: usize,
    required_candidates: usize,
) -> Option<HlsDeterministicTimelineConflict> {
    if fetched_candidates != required_candidates || evaluations.len() != required_candidates {
        return None;
    }
    let first = evaluations.first()?.1.as_ref()?;
    if initial.is_some_and(|initial| initial != first)
        || evaluations.iter().any(|(_, conflict)| conflict.as_ref() != Some(first))
    {
        return None;
    }
    Some(first.clone())
}

pub(super) async fn record_deterministic_conflict_receipt(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    conflict: HlsDeterministicTimelineConflict,
    evidence: &HlsCommittedResourceEvidence,
) {
    let Some(generation) = generation else {
        return;
    };
    let mut session = context.session.write().await;
    let receipt = HlsDeterministicConflictReceipt {
        conflict,
        origin_progress_generation: evidence.origin_progress_generation,
        published_resource_history_generation: evidence.published_resource_history_generation,
        pinned_host_generation: evidence.pinned_host_generation,
    };
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return;
    };
    if episode.generation == generation {
        episode.record_deterministic_conflict(receipt);
    }
}

pub fn deterministic_conflict_receipt_matches(
    session: &crate::HlsSession,
    conflict: &HlsDeterministicTimelineConflict,
) -> bool {
    session
        .origin_control
        .acceptance_episode
        .as_ref()
        .and_then(|episode| episode.deterministic_conflict_receipt())
        .is_some_and(|receipt| {
            receipt.conflict == *conflict && deterministic_conflict_receipt_is_current_for_session(receipt, session)
        })
}

pub fn deterministic_conflict_receipt_is_current(session: &crate::HlsSession) -> bool {
    session
        .origin_control
        .acceptance_episode
        .as_ref()
        .and_then(|episode| episode.deterministic_conflict_receipt())
        .is_some_and(|receipt| deterministic_conflict_receipt_is_current_for_session(receipt, session))
}

fn deterministic_conflict_receipt_is_current_for_session(
    receipt: &HlsDeterministicConflictReceipt,
    session: &crate::HlsSession,
) -> bool {
    receipt.origin_progress_generation == session.origin_control.progress_generation
        && receipt.published_resource_history_generation == session.published_resource_history.generation()
        && receipt.pinned_host_generation == session.origin_epoch
}

pub(super) fn build_manifest_timeline_fingerprint(
    body: &str,
    final_manifest_url: &str,
) -> (HlsManifestTimelineFingerprint, bool, HlsEmergencyAcceptanceEvidence) {
    match parse_origin_media_manifest(body, final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) => {
            let has_switch_segment = !manifest.segments.is_empty();
            let emergency_evidence = emergency_manifest_evidence(&manifest);
            (fingerprint_parsed_manifest(&manifest, body, final_manifest_url), has_switch_segment, emergency_evidence)
        }
        OriginManifestParseOutcome::TransientPassthrough { .. } => {
            let (fingerprint, _has_media_uri) = fingerprint_transient_manifest(body, final_manifest_url);
            // Cross-host transient manifests cannot use the normal typed timeline/MAP staging contract. They remain
            // valid on the pinned host, but an alternative host is not acceptance-ready until a dedicated typed
            // transient receipt exists.
            (fingerprint, false, HlsEmergencyAcceptanceEvidence::INCOMPATIBLE)
        }
    }
}

pub fn deterministic_timeline_conflict_from_rejection(
    fetched: &FetchedOriginManifest,
    reason: &HlsManifestRejectLogReason,
) -> Option<HlsDeterministicTimelineConflict> {
    let HlsManifestRejectLogReason::PublishedResourceReplay {
        previous_proxy_tail,
        existing_proxy_seq,
        candidate_position,
        candidate_origin_seq,
        resource_key: expected_resource_key,
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
    } = reason
    else {
        return None;
    };
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)
    else {
        return None;
    };
    let segment = manifest.segments.get(*candidate_position)?;
    let resource_key =
        HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range).semantic_key();
    if resource_key != *expected_resource_key {
        return None;
    }
    let candidate_fingerprint =
        deterministic_conflict_fingerprint(&manifest, &fetched.body, &fetched.final_manifest_url);
    Some(HlsDeterministicTimelineConflict {
        previous_proxy_tail: *previous_proxy_tail,
        existing_proxy_seq: *existing_proxy_seq,
        candidate_position: *candidate_position,
        candidate_origin_seq: *candidate_origin_seq,
        resource_key,
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
        candidate_fingerprint,
    })
}

fn emergency_manifest_evidence(manifest: &ParsedOriginManifest) -> HlsEmergencyAcceptanceEvidence {
    let clear_mpeg_ts_without_map = manifest.maps.is_empty()
        && !manifest.segments.is_empty()
        && manifest.segments.iter().all(|segment| segment.encryption.is_none())
        && manifest.segments.iter().all(|segment| is_mpeg_ts_resource(&segment.resolved_origin_url));
    if clear_mpeg_ts_without_map {
        HlsEmergencyAcceptanceEvidence {
            live_handoff: HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification,
            terminal_alternative: HlsTerminalAlternativeCompatibility::RequiresStagedComparison,
        }
    } else {
        HlsEmergencyAcceptanceEvidence::INCOMPATIBLE
    }
}

fn is_mpeg_ts_resource(resource: &str) -> bool {
    Url::parse(resource).ok().map_or_else(
        || {
            resource
                .split(['?', '#'])
                .next()
                .and_then(|path| path.rsplit_once('.'))
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("ts"))
        },
        |url| url.path().rsplit_once('.').is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("ts")),
    )
}

fn fingerprint_parsed_manifest(
    manifest: &ParsedOriginManifest,
    body: &str,
    final_manifest_url: &str,
) -> HlsManifestTimelineFingerprint {
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::with_capacity(manifest.segments.len().min(HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT));
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for segment in &manifest.segments {
        duration_hasher.update(segment.duration_ms.to_be_bytes());
        discontinuity_hasher.update([u8::from(segment.discontinuity_before)]);
        let resource_identity =
            HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range);
        resource_hasher.update(resource_identity.exact_path_hash());
        update_container_signature(&mut container_hasher, &segment.resolved_origin_url);
        let program_date_time_ms = parse_program_date_time_ms(segment.program_date_time.as_deref());
        first_program_date_time_ms = first_program_date_time_ms.or(program_date_time_ms);
        if program_date_time_ms.is_some() {
            last_program_date_time_ms = program_date_time_ms;
        }
        if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
            segment_samples.push(HlsManifestSegmentFingerprint {
                duration_ms: segment.duration_ms,
                discontinuity_before: segment.discontinuity_before,
                program_date_time_ms,
                normalized_resource_identity: Some(resource_identity),
            });
        }
    }
    if !manifest.maps.is_empty() {
        container_hasher.update(b"map");
    }
    HlsManifestTimelineFingerprint {
        segment_count: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
        first_program_date_time_ms,
        last_program_date_time_ms,
        duration_pattern_hash: duration_hasher.finalize().into(),
        discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
        normalized_resource_pattern_hash: (!manifest.segments.is_empty()).then(|| resource_hasher.finalize().into()),
        map_and_encryption_hash: map_and_encryption_hash(body, &manifest.maps, final_manifest_url),
        container_signature_hash: container_hasher.finalize().into(),
        segment_samples,
    }
}

pub(super) fn deterministic_conflict_fingerprint(
    manifest: &ParsedOriginManifest,
    body: &str,
    final_manifest_url: &str,
) -> HlsDeterministicConflictFingerprint {
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::with_capacity(manifest.segments.len().min(HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT));
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for segment in &manifest.segments {
        duration_hasher.update(segment.duration_ms.to_be_bytes());
        discontinuity_hasher.update([u8::from(segment.discontinuity_before)]);
        let resource_key =
            HlsMediaResourceIdentity::from_url(&segment.resolved_origin_url, segment.origin_byte_range).semantic_key();
        resource_hasher.update(resource_key.bytes());
        update_container_signature(&mut container_hasher, &segment.resolved_origin_url);
        let program_date_time_ms = parse_program_date_time_ms(segment.program_date_time.as_deref());
        first_program_date_time_ms = first_program_date_time_ms.or(program_date_time_ms);
        if program_date_time_ms.is_some() {
            last_program_date_time_ms = program_date_time_ms;
        }
        if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
            segment_samples.push(HlsDeterministicConflictSegmentFingerprint {
                duration_ms: segment.duration_ms,
                discontinuity_before: segment.discontinuity_before,
                program_date_time_ms,
                resource_key: Some(resource_key),
            });
        }
    }
    if !manifest.maps.is_empty() {
        container_hasher.update(b"map");
    }
    HlsDeterministicConflictFingerprint {
        segment_count: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
        first_program_date_time_ms,
        last_program_date_time_ms,
        duration_pattern_hash: duration_hasher.finalize().into(),
        discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
        semantic_resource_pattern_hash: (!manifest.segments.is_empty()).then(|| resource_hasher.finalize().into()),
        map_and_encryption_hash: semantic_map_and_encryption_hash(body, &manifest.maps, final_manifest_url),
        container_signature_hash: container_hasher.finalize().into(),
        segment_samples,
    }
}

fn fingerprint_transient_manifest(body: &str, final_manifest_url: &str) -> (HlsManifestTimelineFingerprint, bool) {
    let timeline = parse_origin_manifest_timeline(body).ok();
    let mut duration_hasher = Sha256::new();
    let mut discontinuity_hasher = Sha256::new();
    let mut resource_hasher = Sha256::new();
    let mut container_hasher = Sha256::new();
    let mut segment_samples = Vec::new();
    let mut pending_duration_ms = None;
    let mut pending_discontinuity = false;
    let mut pending_program_date_time_ms = None;
    let mut first_program_date_time_ms = None;
    let mut last_program_date_time_ms = None;
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration_ms = parse_extinf_millis(value);
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_program_date_time_ms = parse_program_date_time_ms(Some(value.trim()));
        } else if !line.starts_with('#') {
            let Some(duration_ms) = pending_duration_ms.take() else {
                continue;
            };
            let resolved = resolve_fingerprint_resource(final_manifest_url, line);
            let resource_identity = HlsMediaResourceIdentity::from_url(&resolved, None);
            duration_hasher.update(duration_ms.to_be_bytes());
            discontinuity_hasher.update([u8::from(pending_discontinuity)]);
            resource_hasher.update(resource_identity.exact_path_hash());
            update_container_signature(&mut container_hasher, &resolved);
            first_program_date_time_ms = first_program_date_time_ms.or(pending_program_date_time_ms);
            if pending_program_date_time_ms.is_some() {
                last_program_date_time_ms = pending_program_date_time_ms;
            }
            if segment_samples.len() < HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT {
                segment_samples.push(HlsManifestSegmentFingerprint {
                    duration_ms,
                    discontinuity_before: pending_discontinuity,
                    program_date_time_ms: pending_program_date_time_ms,
                    normalized_resource_identity: Some(resource_identity),
                });
            }
            pending_discontinuity = false;
            pending_program_date_time_ms = None;
        }
    }
    let segment_count = timeline
        .map(|timeline| u32::try_from(timeline.origin_manifest_segment_cnt).unwrap_or(u32::MAX))
        .unwrap_or_default();
    let has_switch_segment = segment_count > 0 && !segment_samples.is_empty();
    (
        HlsManifestTimelineFingerprint {
            segment_count,
            first_program_date_time_ms,
            last_program_date_time_ms,
            duration_pattern_hash: duration_hasher.finalize().into(),
            discontinuity_pattern_hash: discontinuity_hasher.finalize().into(),
            normalized_resource_pattern_hash: has_switch_segment.then(|| resource_hasher.finalize().into()),
            map_and_encryption_hash: map_and_encryption_hash(body, &[], final_manifest_url),
            container_signature_hash: container_hasher.finalize().into(),
            segment_samples,
        },
        has_switch_segment,
    )
}

fn parse_extinf_millis(value: &str) -> Option<u64> {
    let seconds = value.split(',').next()?.trim().parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    let Ok(duration) = Duration::try_from_secs_f64(seconds) else {
        return Some(u64::MAX);
    };
    let rounded = duration.checked_add(Duration::from_micros(500)).unwrap_or(Duration::MAX);
    Some(u64::try_from(rounded.as_millis()).unwrap_or(u64::MAX))
}

fn parse_program_date_time_ms(value: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value?).ok().map(|timestamp| timestamp.timestamp_millis())
}

fn resolve_fingerprint_resource(final_manifest_url: &str, resource: &str) -> String {
    Url::parse(final_manifest_url)
        .ok()
        .and_then(|base| base.join(resource).ok())
        .map_or_else(|| resource.to_string(), |resolved| resolved.to_string())
}

fn update_container_signature(hasher: &mut Sha256, url: &str) {
    let path = Url::parse(url)
        .ok()
        .map_or_else(|| url.split('?').next().unwrap_or_default().to_string(), |parsed| parsed.path().to_string());
    let extension = path.rsplit_once('.').map(|(_, extension)| extension).unwrap_or_default();
    hasher.update(extension.to_ascii_lowercase().as_bytes());
    hasher.update([0]);
}

pub(super) fn map_and_encryption_hash(
    body: &str,
    maps: &[tuliprox_parser::hls::origin_manifest::ParsedOriginMap],
    final_manifest_url: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for map in maps {
        hasher.update(HlsMediaResourceIdentity::from_url(&map.resolved_origin_uri, map.byte_range).exact_path_hash());
    }
    for line in body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#EXT-X-KEY:") || (maps.is_empty() && line.starts_with("#EXT-X-MAP:")))
    {
        hasher.update(normalized_tag_uri(line, final_manifest_url).as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn semantic_map_and_encryption_hash(
    body: &str,
    maps: &[tuliprox_parser::hls::origin_manifest::ParsedOriginMap],
    final_manifest_url: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for map in maps {
        hasher.update(
            HlsMediaResourceIdentity::from_url(&map.resolved_origin_uri, map.byte_range).semantic_key().bytes(),
        );
    }
    for line in body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#EXT-X-KEY:") || (maps.is_empty() && line.starts_with("#EXT-X-MAP:")))
    {
        update_semantic_tag_identity(&mut hasher, line, final_manifest_url);
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn update_semantic_tag_identity(hasher: &mut Sha256, line: &str, final_manifest_url: &str) {
    let Some(uri_start) = line.find("URI=\"") else {
        hasher.update(line.as_bytes());
        return;
    };
    let value_start = uri_start.saturating_add(5);
    let Some(relative_end) = line.get(value_start..).and_then(|tail| tail.find('"')) else {
        hasher.update(line.as_bytes());
        return;
    };
    let value_end = value_start.saturating_add(relative_end);
    let uri = line.get(value_start..value_end).unwrap_or_default();
    let resolved = resolve_fingerprint_resource(final_manifest_url, uri);
    hasher.update(&line.as_bytes()[..value_start]);
    hasher.update(HlsMediaResourceIdentity::from_url(&resolved, None).semantic_key().bytes());
    hasher.update(&line.as_bytes()[value_end..]);
}

fn normalized_tag_uri(line: &str, final_manifest_url: &str) -> String {
    let Some(uri_start) = line.find("URI=\"") else {
        return line.to_string();
    };
    let value_start = uri_start.saturating_add(5);
    let Some(relative_end) = line.get(value_start..).and_then(|tail| tail.find('"')) else {
        return line.to_string();
    };
    let value_end = value_start.saturating_add(relative_end);
    let uri = line.get(value_start..value_end).unwrap_or_default();
    let normalized_uri = Url::parse(uri)
        .ok()
        .or_else(|| Url::parse(final_manifest_url).ok()?.join(uri).ok())
        .map_or_else(|| uri.split(['?', '#']).next().unwrap_or_default().to_string(), |url| url.path().to_string());
    format!("{}{}{}", &line[..value_start], normalized_uri, &line[value_end..])
}
