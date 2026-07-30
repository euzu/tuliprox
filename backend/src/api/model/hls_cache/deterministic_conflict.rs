use super::{resource_identity::HlsMediaResourceSemanticKey, timeline::HlsResourceReplayDecision};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct HlsDeterministicConflictSegmentFingerprint {
    pub duration_ms: u64,
    pub discontinuity_before: bool,
    pub program_date_time_ms: Option<i64>,
    pub resource_key: Option<HlsMediaResourceSemanticKey>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct HlsDeterministicConflictFingerprint {
    pub segment_count: u32,
    pub first_program_date_time_ms: Option<i64>,
    pub last_program_date_time_ms: Option<i64>,
    pub duration_pattern_hash: [u8; 32],
    pub discontinuity_pattern_hash: [u8; 32],
    pub semantic_resource_pattern_hash: Option<[u8; 32]>,
    pub map_and_encryption_hash: [u8; 32],
    pub container_signature_hash: [u8; 32],
    pub segment_samples: Vec<HlsDeterministicConflictSegmentFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct HlsDeterministicTimelineConflict {
    pub previous_proxy_tail: Option<u64>,
    pub existing_proxy_seq: u64,
    pub candidate_position: usize,
    pub candidate_origin_seq: u64,
    pub resource_key: HlsMediaResourceSemanticKey,
    pub decision: HlsResourceReplayDecision,
    pub candidate_fingerprint: HlsDeterministicConflictFingerprint,
}

impl HlsDeterministicTimelineConflict {
    pub(crate) fn diagnostic_resource_token(&self) -> [u8; 8] { self.resource_key.diagnostic_token() }
}
