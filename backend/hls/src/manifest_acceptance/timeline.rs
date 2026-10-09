use super::{
    HlsAlternativeOriginWindow, HlsHostLocalSequenceRelation, HlsManifestCandidateObservation,
    HlsManifestSegmentFingerprint, HlsManifestTechnicalSignature,
};

pub fn classify_host_local_sequence(
    previous_highwater: Option<u64>,
    candidate_highwater: Option<u64>,
    forward_window: u64,
    rebase_allowed: bool,
) -> Option<HlsHostLocalSequenceRelation> {
    let candidate = candidate_highwater?;
    if rebase_allowed {
        return Some(HlsHostLocalSequenceRelation::Rebase);
    }
    let Some(previous) = previous_highwater else {
        return Some(HlsHostLocalSequenceRelation::NoBaseline);
    };
    if candidate == previous {
        return Some(HlsHostLocalSequenceRelation::Same);
    }
    if previous.checked_add(1) == Some(candidate) {
        return Some(HlsHostLocalSequenceRelation::Next);
    }
    if candidate > previous {
        return Some(if candidate.saturating_sub(previous) <= forward_window.max(1) {
            HlsHostLocalSequenceRelation::PlausibleForward
        } else {
            HlsHostLocalSequenceRelation::Backward
        });
    }
    Some(if candidate <= forward_window.max(1) {
        HlsHostLocalSequenceRelation::RolloverCandidate
    } else {
        HlsHostLocalSequenceRelation::Backward
    })
}

pub(super) fn same_host_timeline_compatible(
    left: &HlsManifestCandidateObservation,
    right: &HlsManifestCandidateObservation,
) -> bool {
    let left_window = HlsAlternativeOriginWindow {
        host_local_media_sequence: left.host_local_media_sequence,
        host_local_highwater: left.host_local_highwater,
        fingerprint: left.timeline_fingerprint.clone(),
    };
    let right_window = HlsAlternativeOriginWindow {
        host_local_media_sequence: right.host_local_media_sequence,
        host_local_highwater: right.host_local_highwater,
        fingerprint: right.timeline_fingerprint.clone(),
    };
    host_local_windows_compatible(&left_window, &right_window)
}

pub(super) fn host_local_windows_compatible(
    left: &HlsAlternativeOriginWindow,
    right: &HlsAlternativeOriginWindow,
) -> bool {
    let left_fingerprint = &left.fingerprint;
    let right_fingerprint = &right.fingerprint;
    if HlsManifestTechnicalSignature::from_fingerprint(left_fingerprint)
        != HlsManifestTechnicalSignature::from_fingerprint(right_fingerprint)
        || left_fingerprint.segment_samples.is_empty()
        || right_fingerprint.segment_samples.is_empty()
    {
        return false;
    }

    let Some(left_highwater) = left.host_local_highwater else {
        return false;
    };
    let Some(right_highwater) = right.host_local_highwater else {
        return false;
    };
    let overlap_start = left.host_local_media_sequence.max(right.host_local_media_sequence);
    let overlap_end = left_highwater.min(right_highwater);
    if overlap_start <= overlap_end {
        return (overlap_start..=overlap_end).all(|sequence| {
            segment_in_window(left, sequence)
                .zip(segment_in_window(right, sequence))
                .is_some_and(|(left, right)| segment_shape_matches(left, right))
        });
    }

    // Adjacent local windows are monotonic only when their non-resource shape
    // and technical signatures agree. Origin sequence is used solely inside
    // this already host-local cohort.
    let gap = if left_highwater < right.host_local_media_sequence {
        right.host_local_media_sequence.saturating_sub(left_highwater)
    } else {
        left.host_local_media_sequence.saturating_sub(right_highwater)
    };
    gap <= 1
        && left_fingerprint.segment_count == right_fingerprint.segment_count
        && left_fingerprint.duration_pattern_hash == right_fingerprint.duration_pattern_hash
        && left_fingerprint.discontinuity_pattern_hash == right_fingerprint.discontinuity_pattern_hash
}

fn segment_in_window(window: &HlsAlternativeOriginWindow, sequence: u64) -> Option<&HlsManifestSegmentFingerprint> {
    let offset = sequence.checked_sub(window.host_local_media_sequence)?;
    window.fingerprint.segment_samples.get(usize::try_from(offset).ok()?)
}

pub(super) fn strong_anchor_overlap(
    alternative: &HlsManifestCandidateObservation,
    pinned: &HlsManifestCandidateObservation,
) -> u16 {
    if alternative.timeline_fingerprint.map_and_encryption_hash != pinned.timeline_fingerprint.map_and_encryption_hash
        || alternative.timeline_fingerprint.container_signature_hash
            != pinned.timeline_fingerprint.container_signature_hash
    {
        return 0;
    }
    let mut longest_pdt_run = 0_usize;
    let alternative_segments = &alternative.timeline_fingerprint.segment_samples;
    let pinned_segments = &pinned.timeline_fingerprint.segment_samples;
    for (alternative_index, alternative_segment) in alternative_segments.iter().enumerate() {
        for (pinned_index, pinned_segment) in pinned_segments.iter().enumerate() {
            if !pdt_intervals_overlap(alternative_segment, pinned_segment) {
                continue;
            }
            let mut run = 0_usize;
            while let (Some(alternative_sample), Some(pinned_sample)) = (
                alternative_segments.get(alternative_index.saturating_add(run)),
                pinned_segments.get(pinned_index.saturating_add(run)),
            ) {
                if alternative_sample.duration_ms != pinned_sample.duration_ms
                    || alternative_sample.discontinuity_before != pinned_sample.discontinuity_before
                    || !pdt_intervals_overlap(alternative_sample, pinned_sample)
                {
                    break;
                }
                run = run.saturating_add(1);
            }
            longest_pdt_run = longest_pdt_run.max(run);
        }
    }
    if longest_pdt_run >= 2 {
        u16::try_from(longest_pdt_run).unwrap_or(u16::MAX)
    } else {
        0
    }
}

fn pdt_intervals_overlap(left: &HlsManifestSegmentFingerprint, right: &HlsManifestSegmentFingerprint) -> bool {
    if left.duration_ms != right.duration_ms {
        return false;
    }
    let (Some(left_start), Some(right_start)) = (left.program_date_time_ms, right.program_date_time_ms) else {
        return false;
    };
    let left_duration = i64::try_from(left.duration_ms).unwrap_or(i64::MAX);
    let right_duration = i64::try_from(right.duration_ms).unwrap_or(i64::MAX);
    let left_end = left_start.saturating_add(left_duration);
    let right_end = right_start.saturating_add(right_duration);
    left_start < right_end && right_start < left_end
}

fn segment_shape_matches(left: &HlsManifestSegmentFingerprint, right: &HlsManifestSegmentFingerprint) -> bool {
    left.duration_ms == right.duration_ms
        && left.discontinuity_before == right.discontinuity_before
        && match (left.normalized_resource_identity, right.normalized_resource_identity) {
            (Some(left), Some(right)) => left.matches(right),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
}
