use super::{
    HlsTsCompatibleSpliceEvidence, HlsTsTrackSignature, AES_128_BLOCK_BYTES, HLS_TS_PROBE_MAX_BYTES,
    HLS_TS_PROBE_MAX_PACKETS, HLS_TS_PROBE_MAX_RESYNC_BYTES, HLS_TS_PROBE_READ_CHUNK_BYTES,
};
use crate::transport_stream_buffer::HlsTsTimestampProfile;
use std::sync::Arc;

/// Hard limits for one read-only MPEG-TS compatibility probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTsProbeBudget {
    pub max_bytes: u64,
    pub max_packets: u64,
    pub read_chunk_bytes: usize,
    pub max_resync_bytes: usize,
}

impl Default for HlsTsProbeBudget {
    fn default() -> Self {
        Self {
            max_bytes: HLS_TS_PROBE_MAX_BYTES,
            max_packets: HLS_TS_PROBE_MAX_PACKETS,
            read_chunk_bytes: HLS_TS_PROBE_READ_CHUNK_BYTES,
            max_resync_bytes: HLS_TS_PROBE_MAX_RESYNC_BYTES,
        }
    }
}

impl HlsTsProbeBudget {
    pub(super) fn bounded_read_chunk_bytes(self) -> usize {
        self.read_chunk_bytes.clamp(1, HLS_TS_PROBE_READ_CHUNK_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTsElementaryStreamBinding {
    pub stream_type: u8,
    pub elementary_pid: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsProgramTopology {
    pub transport_stream_id: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub streams: Arc<[HlsTsElementaryStreamBinding]>,
}

impl HlsTsTrackSignature {
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_stream_types(stream_types: impl Into<Arc<[u8]>>) -> Self {
        Self { program_count: 1, has_pcr: true, stream_types: stream_types.into(), programs: Arc::from([]) }
    }
}

pub(super) fn is_audio_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x87)
}

pub(super) fn is_video_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x01 | 0x02 | 0x10 | 0x1B | 0x24 | 0x42)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTsMalformedReason {
    InvalidSynchronization,
    InvalidPacketHeader,
    TransportError,
    InvalidPsiPointer,
    InvalidPat,
    InvalidPmt,
    InvalidPsiCrc,
    IncompletePacket,
    IncompleteProgramMetadata,
}

impl HlsTsMalformedReason {
    pub(super) const fn reason_code(self) -> &'static str {
        match self {
            Self::InvalidSynchronization => "invalid-synchronization",
            Self::InvalidPacketHeader => "invalid-packet-header",
            Self::TransportError => "transport-error",
            Self::InvalidPsiPointer => "invalid-psi-pointer",
            Self::InvalidPat => "invalid-pat",
            Self::InvalidPmt => "invalid-pmt",
            Self::InvalidPsiCrc => "invalid-psi-crc",
            Self::IncompletePacket => "incomplete-packet",
            Self::IncompleteProgramMetadata => "incomplete-program-metadata",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTsProtectionReason {
    TransportScrambling,
    UnsupportedEncryption,
}

impl HlsTsProtectionReason {
    pub(super) const fn reason_code(self) -> &'static str {
        match self {
            Self::TransportScrambling => "transport-scrambling",
            Self::UnsupportedEncryption => "unsupported-encryption",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsTsProbeOutcome {
    Found(HlsTsTrackSignature),
    ProbeBudgetExhausted { bytes_examined: u64, packets_examined: u64 },
    Malformed(HlsTsMalformedReason),
    UnsupportedProtection(HlsTsProtectionReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsMediaEvidence {
    pub track_outcome: HlsTsProbeOutcome,
    pub timestamp_profile: Option<HlsTsTimestampProfile>,
    pub splice_evidence: HlsTsSpliceEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTsPidBoundaryEvidence {
    pub pid: u16,
    pub first_packet_index: u64,
    pub first_continuity_counter: u8,
    pub first_has_payload: bool,
    pub first_discontinuity: bool,
    pub last_continuity_counter: u8,
}

impl HlsTsCompatibleSpliceEvidence {
    pub(super) fn pid_boundary(&self, pid: u16) -> Option<&HlsTsPidBoundaryEvidence> {
        self.pid_boundaries
            .binary_search_by_key(&pid, |boundary| boundary.pid)
            .ok()
            .and_then(|index| self.pid_boundaries.get(index))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(topology: HlsTsTrackSignature) -> Self { Self { topology, pid_boundaries: Arc::from([]) } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTsSpliceIncompatibility {
    InvalidPacket { packet_index: u64 },
    TransportError { pid: u16, packet_index: u64 },
    ContinuityFailure { pid: u16, packet_index: u64, expected: u8, actual: u8 },
    IncompletePes { pid: u16, packet_index: u64, declared_bytes: Option<u16>, observed_bytes: u64 },
    InvalidPes { pid: u16, packet_index: u64 },
    InspectionBudgetExhausted,
    TopologyUnavailable,
}

impl HlsTsSpliceIncompatibility {
    pub const fn result_code(self) -> &'static str {
        match self {
            Self::InvalidPacket { .. } | Self::TransportError { .. } | Self::ContinuityFailure { .. } => {
                "continuity-failure"
            }
            Self::IncompletePes { .. } | Self::InvalidPes { .. } => "incomplete-pes",
            Self::InspectionBudgetExhausted | Self::TopologyUnavailable => "topology-mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsTsSpliceEvidence {
    Compatible(HlsTsCompatibleSpliceEvidence),
    Incompatible(HlsTsSpliceIncompatibility),
}

impl HlsTsSpliceEvidence {
    #[cfg(any(test, feature = "test-support"))]
    pub fn compatible_for_test(topology: HlsTsTrackSignature) -> Self {
        Self::Compatible(HlsTsCompatibleSpliceEvidence::for_test(topology))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTsSpliceBoundaryIncompatibility {
    Media(HlsTsSpliceIncompatibility),
    TopologyMismatch,
}

pub fn evaluate_mpeg_ts_splice_boundary(
    base: &HlsTsSpliceEvidence,
    terminal: &HlsTsSpliceEvidence,
) -> Result<(), HlsTsSpliceBoundaryIncompatibility> {
    let base = match base {
        HlsTsSpliceEvidence::Compatible(evidence) => evidence,
        HlsTsSpliceEvidence::Incompatible(reason) => {
            return Err(HlsTsSpliceBoundaryIncompatibility::Media(*reason));
        }
    };
    let terminal = match terminal {
        HlsTsSpliceEvidence::Compatible(evidence) => evidence,
        HlsTsSpliceEvidence::Incompatible(reason) => {
            return Err(HlsTsSpliceBoundaryIncompatibility::Media(*reason));
        }
    };
    if base.topology != terminal.topology {
        return Err(HlsTsSpliceBoundaryIncompatibility::TopologyMismatch);
    }
    for boundary in terminal.pid_boundaries.iter() {
        if boundary.first_discontinuity {
            continue;
        }
        let Some(base_boundary) = base.pid_boundary(boundary.pid) else {
            continue;
        };
        let expected = if boundary.first_has_payload {
            base_boundary.last_continuity_counter.wrapping_add(1) & 0x0F
        } else {
            base_boundary.last_continuity_counter
        };
        if boundary.first_continuity_counter != expected {
            return Err(HlsTsSpliceBoundaryIncompatibility::Media(HlsTsSpliceIncompatibility::ContinuityFailure {
                pid: boundary.pid,
                packet_index: boundary.first_packet_index,
                expected,
                actual: boundary.first_continuity_counter,
            }));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum HlsTsProbeError {
    #[error("MPEG-TS probe I/O failed")]
    Io(#[source] std::io::Error),
    #[error("MPEG-TS probe key is unavailable")]
    KeyUnavailable,
    #[error("MPEG-TS probe IV is invalid")]
    InvalidIv,
    #[error("MPEG-TS probe decryption failed")]
    DecryptionFailed,
}

/// Policy-facing track evidence with stable diagnostics and no parser-crate types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsTrackEvidenceResolution {
    Found(HlsTsTrackSignature),
    InsufficientEvidence { bytes_examined: u64, packets_examined: u64 },
    IncompleteEvidence,
    Malformed(HlsTsMalformedReason),
    UnsupportedProtection(HlsTsProtectionReason),
    KeyUnavailable,
    InvalidIv,
    DecryptionFailed,
    Io(std::io::ErrorKind),
}

impl HlsTrackEvidenceResolution {
    pub fn signature(&self) -> Option<&HlsTsTrackSignature> {
        match self {
            Self::Found(signature) => Some(signature),
            Self::InsufficientEvidence { .. }
            | Self::IncompleteEvidence
            | Self::Malformed(_)
            | Self::UnsupportedProtection(_)
            | Self::KeyUnavailable
            | Self::InvalidIv
            | Self::DecryptionFailed
            | Self::Io(_) => None,
        }
    }

    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Found(_) => "found",
            Self::InsufficientEvidence { .. } => "insufficient-evidence",
            Self::IncompleteEvidence => "incomplete-evidence",
            Self::Malformed(reason) => reason.reason_code(),
            Self::UnsupportedProtection(reason) => reason.reason_code(),
            Self::KeyUnavailable => "key-unavailable",
            Self::InvalidIv => "invalid-iv",
            Self::DecryptionFailed => "decryption-failed",
            // ErrorKind remains available on the typed value; logs deliberately use
            // one stable non-sensitive code rather than exposing platform wording.
            Self::Io(_) => "io",
        }
    }
}

impl From<Result<HlsTsProbeOutcome, HlsTsProbeError>> for HlsTrackEvidenceResolution {
    fn from(result: Result<HlsTsProbeOutcome, HlsTsProbeError>) -> Self {
        match result {
            Ok(HlsTsProbeOutcome::Found(signature)) => Self::Found(signature),
            Ok(HlsTsProbeOutcome::ProbeBudgetExhausted { bytes_examined, packets_examined }) => {
                Self::InsufficientEvidence { bytes_examined, packets_examined }
            }
            Ok(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)) => {
                Self::IncompleteEvidence
            }
            Ok(HlsTsProbeOutcome::Malformed(reason)) => Self::Malformed(reason),
            Ok(HlsTsProbeOutcome::UnsupportedProtection(reason)) => Self::UnsupportedProtection(reason),
            Err(HlsTsProbeError::Io(error)) => Self::Io(error.kind()),
            Err(HlsTsProbeError::KeyUnavailable) => Self::KeyUnavailable,
            Err(HlsTsProbeError::InvalidIv) => Self::InvalidIv,
            Err(HlsTsProbeError::DecryptionFailed) => Self::DecryptionFailed,
        }
    }
}

#[derive(Clone, Copy)]
pub enum HlsTsProbeProtection<'a> {
    Clear,
    Aes128Cbc { key: &'a [u8], iv: [u8; AES_128_BLOCK_BYTES] },
}
