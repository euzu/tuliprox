use crate::transport_stream_buffer::{HlsTsTimestampProfile, HlsTsTimestampProfileScanner};
use aes::{
    cipher::{Block, BlockDecrypt, KeyInit},
    Aes128,
};
use mpeg2ts_reader::{
    demultiplex::{self, DemuxContext, FilterChangeset, FilterRequest, PacketFilter},
    mpegts_crc,
    packet::{Packet, Pid},
    psi::{
        self,
        pat::{PatSection, ProgramDescriptor},
        pmt::PmtSection,
        BufferSectionSyntaxParser, CurrentNext, SectionPacketConsumer, SectionProcessor, SectionSyntaxSectionProcessor,
        WholeSectionSyntaxPayloadParser,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use zeroize::{Zeroize, Zeroizing};

const AES_128_BLOCK_BYTES: usize = 16;
const TS_PACKET_BYTES: usize = Packet::SIZE;
const TS_ALIGNMENT_CONFIRMATION_PACKETS: usize = 2;
const HLS_TS_PROBE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const HLS_TS_PROBE_MAX_PACKETS: u64 = 8_192;
const HLS_TS_PROBE_READ_CHUNK_BYTES: usize = 64 * 1024;
const HLS_TS_PROBE_MAX_RESYNC_BYTES: usize = TS_PACKET_BYTES * 4;
const PSI_SYNTAX_HEADER_BYTES: usize = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;

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
    fn bounded_read_chunk_bytes(self) -> usize {
        self.read_chunk_bytes.clamp(1, HLS_TS_PROBE_READ_CHUNK_BYTES)
    }
}

/// Stable PAT/PMT compatibility evidence. It is not cross-host content identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsTrackSignature {
    pub program_count: u16,
    pub has_pcr: bool,
    pub stream_types: Arc<[u8]>,
    programs: Arc<[HlsTsProgramTopology]>,
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

fn is_audio_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x87)
}

fn is_video_stream_type(stream_type: u8) -> bool {
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
    const fn reason_code(self) -> &'static str {
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
    const fn reason_code(self) -> &'static str {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsCompatibleSpliceEvidence {
    topology: HlsTsTrackSignature,
    pid_boundaries: Arc<[HlsTsPidBoundaryEvidence]>,
}

impl HlsTsCompatibleSpliceEvidence {
    fn pid_boundary(&self, pid: u16) -> Option<&HlsTsPidBoundaryEvidence> {
        self.pid_boundaries
            .binary_search_by_key(&pid, |boundary| boundary.pid)
            .ok()
            .and_then(|index| self.pid_boundaries.get(index))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(topology: HlsTsTrackSignature) -> Self {
        Self { topology, pid_boundaries: Arc::from([]) }
    }
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

/// Implements the existing HLS AES-128 explicit-IV and sequence-derived-IV rules.
pub fn hls_aes128_cbc_iv(
    explicit_iv: Option<&str>,
    media_sequence: u64,
) -> Result<[u8; AES_128_BLOCK_BYTES], HlsTsProbeError> {
    let mut iv = [0_u8; AES_128_BLOCK_BYTES];
    let Some(explicit_iv) = explicit_iv else {
        iv[AES_128_BLOCK_BYTES - std::mem::size_of::<u64>()..].copy_from_slice(&media_sequence.to_be_bytes());
        return Ok(iv);
    };
    let hex = explicit_iv.strip_prefix("0x").ok_or(HlsTsProbeError::InvalidIv)?;
    if hex.is_empty() || hex.len() > AES_128_BLOCK_BYTES * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HlsTsProbeError::InvalidIv);
    }
    let mut output_index = AES_128_BLOCK_BYTES;
    let mut input_index = hex.len();
    while input_index > 0 {
        let low = hex_digit(hex.as_bytes()[input_index - 1]).ok_or(HlsTsProbeError::InvalidIv)?;
        input_index = input_index.saturating_sub(1);
        let high = if input_index > 0 {
            let value = hex_digit(hex.as_bytes()[input_index - 1]).ok_or(HlsTsProbeError::InvalidIv)?;
            input_index = input_index.saturating_sub(1);
            value
        } else {
            0
        };
        output_index = output_index.checked_sub(1).ok_or(HlsTsProbeError::InvalidIv)?;
        iv[output_index] = (high << 4) | low;
    }
    Ok(iv)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum HlsPsiTableKind {
    Pat,
    Pmt,
}

impl HlsPsiTableKind {
    const fn malformed_reason(self) -> HlsTsMalformedReason {
        match self {
            Self::Pat => HlsTsMalformedReason::InvalidPat,
            Self::Pmt => HlsTsMalformedReason::InvalidPmt,
        }
    }

    const fn expected_table_id(self) -> u8 {
        match self {
            Self::Pat => 0x00,
            Self::Pmt => 0x02,
        }
    }

    const fn minimum_section_length(self) -> usize {
        match self {
            Self::Pat => 9,
            Self::Pmt => 13,
        }
    }

    fn validate_common_header(self, header: &[u8]) -> bool {
        header.len() == psi::SectionCommonHeader::SIZE
            && header[0] == self.expected_table_id()
            && header[1] & 0x80 != 0
            && header[1] & 0x30 == 0x30
            && (self.minimum_section_length()..=1_021).contains(&psi_section_length(header))
    }
}

fn psi_section_length(header: &[u8]) -> usize {
    (usize::from(header[1] & 0x0F) << 8) | usize::from(header[2])
}

/// Thin adapter for the crate's documented syntax-header buffering gap. The
/// crate still owns section framing, continuation, CRC payload collection and
/// PAT/PMT parsing; this adapter retains at most the first seven header bytes.
struct HlsPsiSectionConsumer<P>
where
    P: SectionProcessor<Context = HlsTsDemuxContext>,
{
    consumer: SectionPacketConsumer<P>,
    table_kind: HlsPsiTableKind,
    pending_syntax_header: Option<Vec<u8>>,
}

impl<P> HlsPsiSectionConsumer<P>
where
    P: SectionProcessor<Context = HlsTsDemuxContext>,
{
    fn new(consumer: SectionPacketConsumer<P>, table_kind: HlsPsiTableKind) -> Self {
        Self { consumer, table_kind, pending_syntax_header: None }
    }

    fn consume(&mut self, ctx: &mut HlsTsDemuxContext, packet: &Packet<'_>) {
        if let Some(mut pending) = self.pending_syntax_header.take() {
            let Some(payload) = packet.payload() else {
                self.pending_syntax_header = Some(pending);
                return;
            };
            if packet.payload_unit_start_indicator() {
                let Some((&pointer, section_data)) = payload.split_first() else {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
                    return;
                };
                let pointer = usize::from(pointer);
                if pointer >= section_data.len() {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
                    return;
                }
                let (previous_section, section_start) = section_data.split_at(pointer);
                pending.extend_from_slice(previous_section);
                if pending.len() < psi::SectionCommonHeader::SIZE
                    || !self.table_kind.validate_common_header(&pending[..psi::SectionCommonHeader::SIZE])
                {
                    ctx.record_malformed(self.table_kind.malformed_reason());
                    return;
                }
                let section_bytes = psi::SectionCommonHeader::SIZE
                    .saturating_add(psi_section_length(&pending[..psi::SectionCommonHeader::SIZE]));
                if pending.len() < section_bytes {
                    ctx.record_malformed(self.table_kind.malformed_reason());
                    return;
                }
                self.consume_synthetic_payload(ctx, packet.pid(), &pending[..section_bytes], true);
                if ctx.malformed.is_none() {
                    self.consume_new_section(ctx, packet.pid(), section_start);
                }
                return;
            }
            pending.extend_from_slice(payload);
            if pending.len() < psi::SectionCommonHeader::SIZE {
                self.pending_syntax_header = Some(pending);
                return;
            }
            if !self.table_kind.validate_common_header(&pending[..psi::SectionCommonHeader::SIZE]) {
                ctx.record_malformed(self.table_kind.malformed_reason());
                return;
            }
            if pending.len() < PSI_SYNTAX_HEADER_BYTES {
                self.pending_syntax_header = Some(pending);
                return;
            }
            let section_bytes = psi::SectionCommonHeader::SIZE
                .saturating_add(psi_section_length(&pending[..psi::SectionCommonHeader::SIZE]))
                .min(pending.len());
            self.consume_synthetic_payload(ctx, packet.pid(), &pending[..section_bytes], true);
            return;
        }

        let Some(payload) = packet.payload() else {
            self.consumer.consume(ctx, packet);
            return;
        };
        if !packet.payload_unit_start_indicator() {
            self.consumer.consume(ctx, packet);
            return;
        }
        let Some((&pointer, section_data)) = payload.split_first() else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
            return;
        };
        let pointer = usize::from(pointer);
        if pointer >= section_data.len() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiPointer);
            return;
        }
        let section_start = &section_data[pointer..];
        if section_start.len() < PSI_SYNTAX_HEADER_BYTES {
            if let Some(previous_section) = section_data.get(..pointer).filter(|bytes| !bytes.is_empty()) {
                self.consume_synthetic_payload(ctx, packet.pid(), previous_section, false);
            }
            if ctx.malformed.is_none() {
                self.consume_new_section(ctx, packet.pid(), section_start);
            }
            return;
        }
        if !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE]) {
            ctx.record_malformed(self.table_kind.malformed_reason());
            return;
        }
        self.consumer.consume(ctx, packet);
    }

    fn consume_new_section(&mut self, ctx: &mut HlsTsDemuxContext, pid: Pid, section_start: &[u8]) {
        if section_start.len() < PSI_SYNTAX_HEADER_BYTES {
            if section_start.len() >= psi::SectionCommonHeader::SIZE
                && !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE])
            {
                ctx.record_malformed(self.table_kind.malformed_reason());
                return;
            }
            self.pending_syntax_header = Some(section_start.to_vec());
            return;
        }
        if !self.table_kind.validate_common_header(&section_start[..psi::SectionCommonHeader::SIZE]) {
            ctx.record_malformed(self.table_kind.malformed_reason());
            return;
        }
        self.consume_synthetic_payload(ctx, pid, section_start, true);
    }

    fn consume_synthetic_payload(&mut self, ctx: &mut HlsTsDemuxContext, pid: Pid, bytes: &[u8], start: bool) {
        let mut offset = 0usize;
        let mut first = true;
        while offset < bytes.len() {
            let starts_section = start && first;
            let capacity = if starts_section { 183 } else { 184 };
            let copied = bytes.len().saturating_sub(offset).min(capacity);
            let payload = &bytes[offset..offset.saturating_add(copied)];
            let packet_bytes = synthetic_psi_packet(pid, starts_section, payload);
            if let Some(packet) = Packet::try_new(&packet_bytes) {
                self.consumer.consume(ctx, &packet);
            }
            offset = offset.saturating_add(copied);
            first = false;
        }
    }
}

fn synthetic_psi_packet(pid: Pid, starts_section: bool, payload: &[u8]) -> [u8; TS_PACKET_BYTES] {
    let pointer_bytes = usize::from(starts_section);
    let payload_length = payload.len().saturating_add(pointer_bytes).min(184);
    let pid = u16::from(pid);
    let mut packet = [0xFF_u8; TS_PACKET_BYTES];
    packet[0] = Packet::SYNC_BYTE;
    packet[1] = (u8::try_from(pid >> 8).unwrap_or(0) & 0x1F) | if starts_section { 0x40 } else { 0 };
    packet[2] = pid.to_be_bytes()[1];
    let payload_offset = if payload_length == 184 {
        packet[3] = 0x10;
        4
    } else {
        packet[3] = 0x30;
        let adaptation_length = 183usize.saturating_sub(payload_length);
        packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
        if adaptation_length > 0 {
            packet[5] = 0;
        }
        5usize.saturating_add(adaptation_length)
    };
    let data_offset = if starts_section {
        packet[payload_offset] = 0;
        payload_offset.saturating_add(1)
    } else {
        payload_offset
    };
    let copy_length = payload.len().min(TS_PACKET_BYTES.saturating_sub(data_offset));
    packet[data_offset..data_offset.saturating_add(copy_length)].copy_from_slice(&payload[..copy_length]);
    packet
}

struct HlsPatPacketFilter {
    consumer: HlsPsiSectionConsumer<SectionSyntaxSectionProcessor<BufferSectionSyntaxParser<HlsPatSectionParser>>>,
}

impl Default for HlsPatPacketFilter {
    fn default() -> Self {
        Self {
            consumer: HlsPsiSectionConsumer::new(
                SectionPacketConsumer::new(SectionSyntaxSectionProcessor::new(BufferSectionSyntaxParser::new(
                    HlsPatSectionParser,
                ))),
                HlsPsiTableKind::Pat,
            ),
        }
    }
}

impl PacketFilter for HlsPatPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) {
        self.consumer.consume(ctx, packet);
    }
}

struct HlsPmtPacketFilter {
    consumer: HlsPsiSectionConsumer<SectionSyntaxSectionProcessor<BufferSectionSyntaxParser<HlsPmtSectionParser>>>,
}

impl HlsPmtPacketFilter {
    fn new(pid: Pid, program_number: u16) -> Self {
        Self {
            consumer: HlsPsiSectionConsumer::new(
                SectionPacketConsumer::new(SectionSyntaxSectionProcessor::new(BufferSectionSyntaxParser::new(
                    HlsPmtSectionParser { pid, program_number },
                ))),
                HlsPsiTableKind::Pmt,
            ),
        }
    }
}

impl PacketFilter for HlsPmtPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) {
        self.consumer.consume(ctx, packet);
    }
}

enum HlsTsPacketFilter {
    Pat(HlsPatPacketFilter),
    Pmt(HlsPmtPacketFilter),
    Ignore(demultiplex::NullPacketFilter<HlsTsDemuxContext>),
}

impl PacketFilter for HlsTsPacketFilter {
    type Ctx = HlsTsDemuxContext;

    fn consume(&mut self, ctx: &mut Self::Ctx, packet: &Packet<'_>) {
        match self {
            Self::Pat(filter) => filter.consume(ctx, packet),
            Self::Pmt(filter) => filter.consume(ctx, packet),
            Self::Ignore(filter) => filter.consume(ctx, packet),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsPmtSectionEvidence {
    pcr_pid: u16,
    streams: Vec<HlsTsElementaryStreamBinding>,
}

#[derive(Default)]
struct HlsPmtEvidence {
    program_number: u16,
    table_version: Option<u8>,
    last_section_number: Option<u8>,
    sections: BTreeMap<u8, HlsPmtSectionEvidence>,
    has_pcr: bool,
    stream_types: BTreeSet<u8>,
}

impl HlsPmtEvidence {
    fn complete(&self) -> bool {
        self.last_section_number.is_some_and(|last| (0..=last).all(|section| self.sections.contains_key(&section)))
    }

    fn restart_version(&mut self, version: u8, last_section_number: u8) {
        self.table_version = Some(version);
        self.last_section_number = Some(last_section_number);
        self.sections.clear();
        self.has_pcr = false;
        self.stream_types.clear();
    }

    fn record_section(
        &mut self,
        section_number: u8,
        section: HlsPmtSectionEvidence,
    ) -> Result<(), HlsTsMalformedReason> {
        if self.sections.get(&section_number).is_some_and(|current| current != &section) {
            return Err(HlsTsMalformedReason::InvalidPmt);
        }
        self.sections.insert(section_number, section);
        self.has_pcr = self.sections.values().any(|section| section.pcr_pid != u16::from(mpeg2ts_reader::STUFFING_PID));
        self.stream_types = self
            .sections
            .values()
            .flat_map(|section| section.streams.iter().map(|stream| stream.stream_type))
            .collect();
        Ok(())
    }

    fn topology(&self, transport_stream_id: u16, pmt_pid: u16) -> Option<HlsTsProgramTopology> {
        if !self.complete() {
            return None;
        }
        let mut sections = self.sections.values();
        let pcr_pid = sections.next()?.pcr_pid;
        if self.sections.values().any(|section| section.pcr_pid != pcr_pid) {
            return None;
        }
        let streams = self.sections.values().flat_map(|section| section.streams.iter().copied()).collect::<Vec<_>>();
        if streams.is_empty() {
            return None;
        }
        let mut elementary_pids = BTreeSet::new();
        if streams.iter().any(|stream| !elementary_pids.insert(stream.elementary_pid)) {
            return None;
        }
        Some(HlsTsProgramTopology {
            transport_stream_id,
            program_number: self.program_number,
            pmt_pid,
            pcr_pid,
            streams: streams.into(),
        })
    }
}

struct HlsTsDemuxContext {
    changeset: FilterChangeset<HlsTsPacketFilter>,
    pat_table_identity: Option<(u16, u8)>,
    pat_last_section_number: Option<u8>,
    pat_sections: BTreeMap<u8, BTreeMap<u16, u16>>,
    programs_by_pid: BTreeMap<u16, u16>,
    pmt_evidence: BTreeMap<u16, HlsPmtEvidence>,
    malformed: Option<HlsTsMalformedReason>,
}

impl HlsTsDemuxContext {
    fn new() -> Self {
        Self {
            changeset: FilterChangeset::default(),
            pat_table_identity: None,
            pat_last_section_number: None,
            pat_sections: BTreeMap::new(),
            programs_by_pid: BTreeMap::new(),
            pmt_evidence: BTreeMap::new(),
            malformed: None,
        }
    }

    fn record_malformed(&mut self, reason: HlsTsMalformedReason) {
        if self.malformed.is_none() {
            self.malformed = Some(reason);
        }
    }

    fn register_program(&mut self, program_number: u16, pid: Pid) {
        let pid_value = u16::from(pid);
        if self.programs_by_pid.get(&pid_value).is_some_and(|registered_program| *registered_program != program_number)
            || self.programs_by_pid.iter().any(|(registered_pid, registered_program)| {
                *registered_program == program_number && *registered_pid != pid_value
            })
        {
            self.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        if self.programs_by_pid.insert(pid_value, program_number).is_none() {
            self.pmt_evidence.insert(pid_value, HlsPmtEvidence { program_number, ..HlsPmtEvidence::default() });
            self.changeset.insert(pid, HlsTsPacketFilter::Pmt(HlsPmtPacketFilter::new(pid, program_number)));
        }
    }

    fn restart_pat_version(&mut self, table_identity: (u16, u8)) {
        let mut invalid_registered_pid = false;
        for pid in self.programs_by_pid.keys().copied() {
            match Pid::try_from(pid) {
                Ok(pid) => self.changeset.remove(pid),
                Err(()) => invalid_registered_pid = true,
            }
        }
        self.pat_table_identity = Some(table_identity);
        self.pat_last_section_number = None;
        self.pat_sections.clear();
        self.programs_by_pid.clear();
        self.pmt_evidence.clear();
        if invalid_registered_pid {
            self.record_malformed(HlsTsMalformedReason::InvalidPat);
        }
    }

    fn reconcile_pat_programs(&mut self) -> Result<(), HlsTsMalformedReason> {
        let mut programs_by_pid = BTreeMap::new();
        let mut pids_by_program = BTreeMap::new();
        for section in self.pat_sections.values() {
            for (pid, program_number) in section {
                if programs_by_pid.insert(*pid, *program_number).is_some_and(|current| current != *program_number)
                    || pids_by_program.insert(*program_number, *pid).is_some_and(|current| current != *pid)
                {
                    return Err(HlsTsMalformedReason::InvalidPat);
                }
            }
        }
        for (pid, program_number) in programs_by_pid {
            match self.programs_by_pid.get(&pid) {
                Some(current) if *current == program_number => {}
                Some(_) => return Err(HlsTsMalformedReason::InvalidPat),
                None => {
                    let pid = Pid::try_from(pid).map_err(|()| HlsTsMalformedReason::InvalidPat)?;
                    self.register_program(program_number, pid);
                    if self.malformed.is_some() {
                        return Err(HlsTsMalformedReason::InvalidPat);
                    }
                }
            }
        }
        Ok(())
    }

    fn is_psi_pid(&self, pid: u16) -> bool {
        pid == u16::from(psi::pat::PAT_PID) || self.programs_by_pid.contains_key(&pid)
    }

    fn signature(&self) -> Option<HlsTsTrackSignature> {
        let pat_complete = self
            .pat_last_section_number
            .is_some_and(|last| (0..=last).all(|section| self.pat_sections.contains_key(&section)));
        if !pat_complete || self.programs_by_pid.is_empty() || self.pmt_evidence.values().any(|pmt| !pmt.complete()) {
            return None;
        }
        let transport_stream_id = self.pat_table_identity?.0;
        let programs = self
            .pmt_evidence
            .iter()
            .map(|(pmt_pid, pmt)| pmt.topology(transport_stream_id, *pmt_pid))
            .collect::<Option<Vec<_>>>()?;
        let stream_types = self
            .pmt_evidence
            .values()
            .flat_map(|pmt| pmt.stream_types.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if !stream_types
            .iter()
            .copied()
            .any(|stream_type| is_audio_stream_type(stream_type) || is_video_stream_type(stream_type))
        {
            return None;
        }
        Some(HlsTsTrackSignature {
            program_count: u16::try_from(self.programs_by_pid.len()).unwrap_or(u16::MAX),
            has_pcr: self.pmt_evidence.values().any(|pmt| pmt.has_pcr),
            stream_types: Arc::from(stream_types),
            programs: programs.into(),
        })
    }
}

impl DemuxContext for HlsTsDemuxContext {
    type F = HlsTsPacketFilter;

    fn filter_changeset(&mut self) -> &mut FilterChangeset<Self::F> {
        &mut self.changeset
    }

    fn construct(&mut self, request: FilterRequest<'_, '_>) -> Self::F {
        match request {
            FilterRequest::ByPid(pid) if pid == psi::pat::PAT_PID => {
                HlsTsPacketFilter::Pat(HlsPatPacketFilter::default())
            }
            FilterRequest::Pmt { pid, program_number } => {
                HlsTsPacketFilter::Pmt(HlsPmtPacketFilter::new(pid, program_number))
            }
            FilterRequest::ByPid(_) | FilterRequest::ByStream { .. } | FilterRequest::Nit { .. } => {
                HlsTsPacketFilter::Ignore(demultiplex::NullPacketFilter::default())
            }
        }
    }
}

struct HlsPatSectionParser;

impl WholeSectionSyntaxPayloadParser for HlsPatSectionParser {
    type Context = HlsTsDemuxContext;

    fn section<'a>(
        &mut self,
        ctx: &mut Self::Context,
        header: &psi::SectionCommonHeader,
        table_header: &psi::TableSyntaxHeader<'a>,
        data: &'a [u8],
    ) {
        if header.table_id != 0x00 || data.len() < 12 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        if mpegts_crc::sum32(data) != 0 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiCrc);
            return;
        }
        if table_header.current_next_indicator() == CurrentNext::Next {
            return;
        }
        if table_header.section_number() > table_header.last_section_number() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let table_identity = (table_header.id(), table_header.version());
        match ctx.pat_table_identity {
            Some((table_id, _)) if table_id != table_header.id() => {
                ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
                return;
            }
            Some(current) if current != table_identity => ctx.restart_pat_version(table_identity),
            Some(_) => {}
            None => ctx.pat_table_identity = Some(table_identity),
        }
        if ctx.pat_last_section_number.is_some_and(|last| last != table_header.last_section_number()) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let payload_start = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;
        let payload_end = data.len().saturating_sub(4);
        let Some(payload) = data.get(payload_start..payload_end) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        };
        if !payload.len().is_multiple_of(4) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        let mut programs = BTreeMap::new();
        let mut pids_by_program = BTreeMap::new();
        for descriptor in PatSection::new(payload).programs() {
            if let ProgramDescriptor::Program { program_number, pid } = descriptor {
                let pid = u16::from(pid);
                if programs.insert(pid, program_number).is_some_and(|current| current != program_number)
                    || pids_by_program.insert(program_number, pid).is_some_and(|current| current != pid)
                {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
                    return;
                }
            }
        }
        if ctx.pat_sections.get(&table_header.section_number()).is_some_and(|current| current != &programs) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPat);
            return;
        }
        ctx.pat_last_section_number = Some(table_header.last_section_number());
        ctx.pat_sections.insert(table_header.section_number(), programs);
        if let Err(reason) = ctx.reconcile_pat_programs() {
            ctx.record_malformed(reason);
        }
    }
}

struct HlsPmtSectionParser {
    pid: Pid,
    program_number: u16,
}

impl WholeSectionSyntaxPayloadParser for HlsPmtSectionParser {
    type Context = HlsTsDemuxContext;

    fn section<'a>(
        &mut self,
        ctx: &mut Self::Context,
        header: &psi::SectionCommonHeader,
        table_header: &psi::TableSyntaxHeader<'a>,
        data: &'a [u8],
    ) {
        if header.table_id != 0x02 || data.len() < 16 || table_header.id() != self.program_number {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        if mpegts_crc::sum32(data) != 0 {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPsiCrc);
            return;
        }
        if table_header.current_next_indicator() == CurrentNext::Next {
            return;
        }
        if table_header.section_number() > table_header.last_section_number() {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        let payload_start = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;
        let payload_end = data.len().saturating_sub(4);
        let Some(payload) = data.get(payload_start..payload_end) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        if !pmt_stream_loop_is_well_formed(payload) {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        let Ok(section) = PmtSection::from_bytes(payload) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        let pid = u16::from(self.pid);
        let Some(evidence) = ctx.pmt_evidence.get_mut(&pid) else {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        };
        if evidence.program_number != self.program_number {
            ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
            return;
        }
        match evidence.table_version {
            Some(version) if version != table_header.version() => {
                evidence.restart_version(table_header.version(), table_header.last_section_number());
            }
            Some(_) => {
                if evidence.last_section_number != Some(table_header.last_section_number()) {
                    ctx.record_malformed(HlsTsMalformedReason::InvalidPmt);
                    return;
                }
            }
            None => evidence.restart_version(table_header.version(), table_header.last_section_number()),
        }
        let section_evidence = HlsPmtSectionEvidence {
            pcr_pid: u16::from(section.pcr_pid()),
            streams: section
                .streams()
                .map(|stream| HlsTsElementaryStreamBinding {
                    stream_type: stream.stream_type().0,
                    elementary_pid: u16::from(stream.elementary_pid()),
                })
                .collect(),
        };
        if let Err(reason) = evidence.record_section(table_header.section_number(), section_evidence) {
            ctx.record_malformed(reason);
        }
    }
}

/// `mpeg2ts-reader` intentionally stops its stream iterator at a truncated
/// descriptor. Validate both descriptor loops and ES framing so truncation is
/// a typed malformed outcome instead of a partial, falsely compatible signature.
fn pmt_stream_loop_is_well_formed(payload: &[u8]) -> bool {
    let Some(program_info_length_bytes) = payload.get(2..4) else {
        return false;
    };
    let program_info_length =
        (usize::from(program_info_length_bytes[0] & 0x0F) << 8) | usize::from(program_info_length_bytes[1]);
    let Some(mut offset) = 4usize.checked_add(program_info_length).filter(|offset| *offset <= payload.len()) else {
        return false;
    };
    if !descriptor_loop_is_well_formed(&payload[4..offset]) {
        return false;
    }
    while offset < payload.len() {
        let Some(header) = payload.get(offset..offset.saturating_add(5)) else {
            return false;
        };
        let es_info_length = (usize::from(header[3] & 0x0F) << 8) | usize::from(header[4]);
        let Some(next) = offset.checked_add(5).and_then(|value| value.checked_add(es_info_length)) else {
            return false;
        };
        if next > payload.len() {
            return false;
        }
        if !descriptor_loop_is_well_formed(&payload[offset.saturating_add(5)..next]) {
            return false;
        }
        offset = next;
    }
    true
}

fn descriptor_loop_is_well_formed(mut descriptors: &[u8]) -> bool {
    while !descriptors.is_empty() {
        let Some(header) = descriptors.get(..2) else {
            return false;
        };
        let descriptor_length = usize::from(header[1]);
        let Some(next) = 2usize.checked_add(descriptor_length) else {
            return false;
        };
        let Some(remaining) = descriptors.get(next..) else {
            return false;
        };
        descriptors = remaining;
    }
    true
}

#[derive(Clone, Copy)]
struct HlsTsPidContinuityState {
    first_packet_index: u64,
    first_continuity_counter: u8,
    first_has_payload: bool,
    first_discontinuity: bool,
    last_continuity_counter: u8,
}

enum HlsTsPesInspectionState {
    Header { bytes: [u8; 6], len: usize, started_at_packet_index: u64 },
    Finite { declared_bytes: u16, observed_bytes: u64, started_at_packet_index: u64 },
    UnboundedVideo,
}

struct HlsTsTransportStreamInspector {
    continuity: Vec<Option<HlsTsPidContinuityState>>,
    pes: Vec<Option<HlsTsPesInspectionState>>,
    invalid_pes_starts: Vec<Option<u64>>,
    incompatible: Option<HlsTsSpliceIncompatibility>,
}

fn validated_adaptation_length(packet: &Packet<'_>, bytes: &[u8]) -> Result<Option<usize>, ()> {
    let adaptation_control = packet.adaptation_control();
    if !adaptation_control.has_payload() && !adaptation_control.has_adaptation_field() {
        return Err(());
    }
    let adaptation_length = adaptation_control.has_adaptation_field().then(|| usize::from(bytes[4]));
    match (adaptation_control.has_adaptation_field(), adaptation_control.has_payload(), adaptation_length) {
        (false, true, None) | (true, false, Some(183)) => Ok(adaptation_length),
        (true, true, Some(length)) if length <= 182 => Ok(adaptation_length),
        _ => Err(()),
    }
}

impl HlsTsTransportStreamInspector {
    fn new() -> Self {
        let pid_count = usize::from(u16::from(mpeg2ts_reader::STUFFING_PID)).saturating_add(1);
        Self {
            continuity: vec![None; pid_count],
            pes: (0..pid_count).map(|_| None).collect(),
            invalid_pes_starts: vec![None; pid_count],
            incompatible: None,
        }
    }

    fn push_packet(&mut self, bytes: &[u8], packet_index: u64) {
        if self.incompatible.is_some() {
            return;
        }
        if bytes.len() != TS_PACKET_BYTES {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        }
        let Some(packet) = Packet::try_new(bytes) else {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        };
        let pid = u16::from(packet.pid());
        if packet.transport_error_indicator() {
            self.incompatible = Some(HlsTsSpliceIncompatibility::TransportError { pid, packet_index });
            return;
        }
        if packet.transport_scrambling_control().is_scrambled() {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        }
        let Ok(adaptation_length) = validated_adaptation_length(&packet, bytes) else {
            self.incompatible = Some(HlsTsSpliceIncompatibility::InvalidPacket { packet_index });
            return;
        };
        let has_payload = packet.adaptation_control().has_payload();
        let discontinuity = adaptation_length.is_some_and(|length| length > 0 && bytes[5] & 0x80 != 0);
        let continuity_counter = packet.continuity_counter().count();
        if let Err(reason) = self.inspect_continuity(pid, packet_index, continuity_counter, has_payload, discontinuity)
        {
            self.incompatible = Some(reason);
            return;
        }
        if discontinuity {
            if let Some(reason) = self.take_discontinuity_pes_incompatibility(pid) {
                self.incompatible = Some(reason);
                return;
            }
        }
        let Some(payload) = packet.payload() else {
            return;
        };
        let previous = self.pes[usize::from(pid)].take();
        match advance_ts_pes_inspection(
            pid,
            packet.payload_unit_start_indicator(),
            payload,
            packet_index,
            previous,
            &mut self.invalid_pes_starts[usize::from(pid)],
        ) {
            Ok(next) => self.pes[usize::from(pid)] = next,
            Err(reason) => self.incompatible = Some(reason),
        }
    }

    fn inspect_continuity(
        &mut self,
        pid: u16,
        packet_index: u64,
        continuity_counter: u8,
        has_payload: bool,
        discontinuity: bool,
    ) -> Result<(), HlsTsSpliceIncompatibility> {
        if pid == u16::from(mpeg2ts_reader::STUFFING_PID) {
            return Ok(());
        }
        let state = &mut self.continuity[usize::from(pid)];
        let Some(previous) = state else {
            *state = Some(HlsTsPidContinuityState {
                first_packet_index: packet_index,
                first_continuity_counter: continuity_counter,
                first_has_payload: has_payload,
                first_discontinuity: discontinuity,
                last_continuity_counter: continuity_counter,
            });
            return Ok(());
        };
        let expected = if has_payload {
            previous.last_continuity_counter.wrapping_add(1) & 0x0F
        } else {
            previous.last_continuity_counter
        };
        if !discontinuity && continuity_counter != expected {
            return Err(HlsTsSpliceIncompatibility::ContinuityFailure {
                pid,
                packet_index,
                expected,
                actual: continuity_counter,
            });
        }
        previous.last_continuity_counter = continuity_counter;
        Ok(())
    }

    fn take_discontinuity_pes_incompatibility(&mut self, pid: u16) -> Option<HlsTsSpliceIncompatibility> {
        match self.pes[usize::from(pid)].take() {
            Some(HlsTsPesInspectionState::Header { started_at_packet_index, .. }) => {
                Some(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: None,
                    observed_bytes: 0,
                })
            }
            Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
                Some(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: Some(declared_bytes),
                    observed_bytes,
                })
            }
            Some(HlsTsPesInspectionState::UnboundedVideo) | None => None,
        }
    }

    fn finish(mut self, topology: Option<HlsTsTrackSignature>) -> HlsTsSpliceEvidence {
        if let Some(reason) = self.incompatible {
            return HlsTsSpliceEvidence::Incompatible(reason);
        }
        for (pid, state) in self.pes.drain(..).enumerate() {
            let Some(state) = state else {
                continue;
            };
            let pid = u16::try_from(pid).unwrap_or(u16::MAX);
            match state {
                HlsTsPesInspectionState::Header { started_at_packet_index, .. } => {
                    return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index: started_at_packet_index,
                        declared_bytes: None,
                        observed_bytes: 0,
                    });
                }
                HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index } => {
                    return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index: started_at_packet_index,
                        declared_bytes: Some(declared_bytes),
                        observed_bytes,
                    });
                }
                HlsTsPesInspectionState::UnboundedVideo => {}
            }
        }
        let Some(topology) = topology else {
            return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::TopologyUnavailable);
        };
        for stream in topology.programs.iter().flat_map(|program| program.streams.iter()) {
            if let Some(packet_index) = self.invalid_pes_starts[usize::from(stream.elementary_pid)] {
                return HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPes {
                    pid: stream.elementary_pid,
                    packet_index,
                });
            }
        }
        let pid_boundaries = self
            .continuity
            .into_iter()
            .enumerate()
            .filter_map(|(pid, state)| {
                let state = state?;
                Some(HlsTsPidBoundaryEvidence {
                    pid: u16::try_from(pid).unwrap_or(u16::MAX),
                    first_packet_index: state.first_packet_index,
                    first_continuity_counter: state.first_continuity_counter,
                    first_has_payload: state.first_has_payload,
                    first_discontinuity: state.first_discontinuity,
                    last_continuity_counter: state.last_continuity_counter,
                })
            })
            .collect::<Vec<_>>();
        HlsTsSpliceEvidence::Compatible(HlsTsCompatibleSpliceEvidence {
            topology,
            pid_boundaries: pid_boundaries.into(),
        })
    }
}

fn advance_ts_pes_inspection(
    pid: u16,
    payload_unit_start: bool,
    payload: &[u8],
    packet_index: u64,
    previous: Option<HlsTsPesInspectionState>,
    invalid_pes_start: &mut Option<u64>,
) -> Result<Option<HlsTsPesInspectionState>, HlsTsSpliceIncompatibility> {
    let state = if payload_unit_start {
        match previous {
            Some(HlsTsPesInspectionState::Header { started_at_packet_index, .. }) => {
                return Err(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: None,
                    observed_bytes: 0,
                });
            }
            Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
                return Err(HlsTsSpliceIncompatibility::IncompletePes {
                    pid,
                    packet_index: started_at_packet_index,
                    declared_bytes: Some(declared_bytes),
                    observed_bytes,
                });
            }
            Some(HlsTsPesInspectionState::UnboundedVideo) | None => {}
        }
        Some(HlsTsPesInspectionState::Header { bytes: [0; 6], len: 0, started_at_packet_index: packet_index })
    } else {
        previous
    };
    let (mut bytes, mut len, started_at_packet_index) = match state {
        Some(HlsTsPesInspectionState::Header { bytes, len, started_at_packet_index }) => {
            (bytes, len, started_at_packet_index)
        }
        Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }) => {
            let available = u64::try_from(payload.len()).unwrap_or(u64::MAX);
            let observed_bytes = observed_bytes.saturating_add(available).min(u64::from(declared_bytes));
            return if observed_bytes == u64::from(declared_bytes) {
                Ok(None)
            } else {
                Ok(Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }))
            };
        }
        Some(HlsTsPesInspectionState::UnboundedVideo) => {
            return Ok(Some(HlsTsPesInspectionState::UnboundedVideo));
        }
        None => return Ok(None),
    };
    let copied = payload.len().min(6usize.saturating_sub(len));
    bytes[len..len.saturating_add(copied)].copy_from_slice(&payload[..copied]);
    len = len.saturating_add(copied);
    if len >= 3 && bytes[..3] != [0, 0, 1] {
        if invalid_pes_start.is_none() {
            *invalid_pes_start = Some(started_at_packet_index);
        }
        return Ok(None);
    }
    if len < 6 {
        return Ok(Some(HlsTsPesInspectionState::Header { bytes, len, started_at_packet_index }));
    }
    let stream_id = bytes[3];
    let declared_bytes = u16::from_be_bytes([bytes[4], bytes[5]]);
    if declared_bytes == 0 {
        return if (0xE0..=0xEF).contains(&stream_id) {
            Ok(Some(HlsTsPesInspectionState::UnboundedVideo))
        } else {
            Err(HlsTsSpliceIncompatibility::InvalidPes { pid, packet_index })
        };
    }
    let observed_bytes =
        u64::try_from(payload.len().saturating_sub(copied)).unwrap_or(u64::MAX).min(u64::from(declared_bytes));
    if observed_bytes == u64::from(declared_bytes) {
        Ok(None)
    } else {
        Ok(Some(HlsTsPesInspectionState::Finite { declared_bytes, observed_bytes, started_at_packet_index }))
    }
}

struct HlsTsMediaStreamInspector {
    budget: HlsTsProbeBudget,
    timestamp_scanner: HlsTsTimestampProfileScanner,
    transport_scanner: HlsTsTransportStreamInspector,
    pending: Vec<u8>,
    aligned: bool,
    packets_examined: u64,
    invalid: bool,
}

#[derive(Clone, Copy)]
enum HlsTsPlaintextRemainder {
    ExactPackets,
    Aes128Pkcs7,
}

impl HlsTsMediaStreamInspector {
    fn new(budget: HlsTsProbeBudget, expected_duration_ticks_90khz: u64) -> Self {
        Self {
            budget,
            timestamp_scanner: HlsTsTimestampProfileScanner::new(expected_duration_ticks_90khz),
            transport_scanner: HlsTsTransportStreamInspector::new(),
            pending: Vec::with_capacity(budget.bounded_read_chunk_bytes().saturating_add(TS_PACKET_BYTES)),
            aligned: false,
            packets_examined: 0,
            invalid: false,
        }
    }

    fn feed_plaintext(&mut self, bytes: &[u8]) {
        if self.invalid {
            return;
        }
        self.pending.extend_from_slice(bytes);
        if !self.aligned {
            let confirmation_bytes = TS_PACKET_BYTES.saturating_mul(TS_ALIGNMENT_CONFIRMATION_PACKETS);
            if self.pending.len() < confirmation_bytes {
                return;
            }
            let available_search = self.pending.len().saturating_sub(confirmation_bytes);
            let search_end = available_search.min(self.budget.max_resync_bytes);
            let alignment = (0..=search_end).find(|offset| {
                (0..TS_ALIGNMENT_CONFIRMATION_PACKETS).all(|index| {
                    self.pending[offset.saturating_add(index.saturating_mul(TS_PACKET_BYTES))] == Packet::SYNC_BYTE
                })
            });
            let Some(alignment) = alignment else {
                if available_search >= self.budget.max_resync_bytes {
                    self.invalid = true;
                }
                return;
            };
            self.pending.drain(..alignment);
            self.aligned = true;
        }

        let mut consumed = 0usize;
        while self.pending.len().saturating_sub(consumed) >= TS_PACKET_BYTES {
            if self.packets_examined >= self.budget.max_packets {
                self.invalid = true;
                break;
            }
            let packet_end = consumed.saturating_add(TS_PACKET_BYTES);
            let packet = &self.pending[consumed..packet_end];
            self.timestamp_scanner.push_aligned_packet(packet);
            self.transport_scanner.push_packet(packet, self.packets_examined);
            self.packets_examined = self.packets_examined.saturating_add(1);
            consumed = packet_end;
        }
        self.pending.drain(..consumed);
    }

    fn finish(
        self,
        remainder: HlsTsPlaintextRemainder,
        topology: Option<HlsTsTrackSignature>,
    ) -> (Option<HlsTsTimestampProfile>, HlsTsSpliceEvidence) {
        let valid_remainder = match remainder {
            HlsTsPlaintextRemainder::ExactPackets => self.pending.is_empty(),
            HlsTsPlaintextRemainder::Aes128Pkcs7 => {
                let Some(&padding) = self.pending.last() else {
                    return (
                        None,
                        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPacket {
                            packet_index: self.packets_examined,
                        }),
                    );
                };
                let padding = usize::from(padding);
                (1..=AES_128_BLOCK_BYTES).contains(&padding)
                    && self.pending.len() == padding
                    && self.pending.iter().all(|byte| usize::from(*byte) == padding)
            }
        };
        if self.invalid || !self.aligned || !valid_remainder {
            return (
                None,
                HlsTsSpliceEvidence::Incompatible(if self.packets_examined >= self.budget.max_packets {
                    HlsTsSpliceIncompatibility::InspectionBudgetExhausted
                } else {
                    HlsTsSpliceIncompatibility::InvalidPacket { packet_index: self.packets_examined }
                }),
            );
        }
        (self.timestamp_scanner.finish(), self.transport_scanner.finish(topology))
    }
}

struct HlsTsInspector {
    budget: HlsTsProbeBudget,
    demux: demultiplex::Demultiplex<HlsTsDemuxContext>,
    context: HlsTsDemuxContext,
    pending: Vec<u8>,
    aligned: bool,
    source_bytes_examined: u64,
    packets_examined: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HlsTsInspectionMode {
    Prefix,
    Complete,
}

impl HlsTsInspector {
    fn new(budget: HlsTsProbeBudget) -> Self {
        let mut context = HlsTsDemuxContext::new();
        let demux = demultiplex::Demultiplex::new(&mut context);
        Self {
            budget,
            demux,
            context,
            pending: Vec::with_capacity(budget.bounded_read_chunk_bytes().saturating_add(TS_PACKET_BYTES)),
            aligned: false,
            source_bytes_examined: 0,
            packets_examined: 0,
        }
    }

    fn record_source_bytes(&mut self, bytes: usize) {
        self.source_bytes_examined =
            self.source_bytes_examined.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    fn budget_exhausted(&self) -> HlsTsProbeOutcome {
        HlsTsProbeOutcome::ProbeBudgetExhausted {
            bytes_examined: self.source_bytes_examined,
            packets_examined: self.packets_examined,
        }
    }

    fn feed_plaintext(&mut self, bytes: &[u8]) -> Option<HlsTsProbeOutcome> {
        self.feed_plaintext_with_mode(bytes, HlsTsInspectionMode::Prefix)
    }

    fn feed_plaintext_complete(&mut self, bytes: &[u8]) -> Option<HlsTsProbeOutcome> {
        self.feed_plaintext_with_mode(bytes, HlsTsInspectionMode::Complete)
    }

    fn feed_plaintext_with_mode(&mut self, bytes: &[u8], mode: HlsTsInspectionMode) -> Option<HlsTsProbeOutcome> {
        self.pending.extend_from_slice(bytes);
        if !self.aligned {
            let confirmation_bytes = TS_PACKET_BYTES.saturating_mul(TS_ALIGNMENT_CONFIRMATION_PACKETS);
            if self.pending.len() < confirmation_bytes {
                return None;
            }
            let available_search = self.pending.len().saturating_sub(confirmation_bytes);
            let search_end = available_search.min(self.budget.max_resync_bytes);
            let alignment = (0..=search_end).find(|offset| {
                (0..TS_ALIGNMENT_CONFIRMATION_PACKETS).all(|index| {
                    self.pending[offset.saturating_add(index.saturating_mul(TS_PACKET_BYTES))] == Packet::SYNC_BYTE
                })
            });
            let Some(alignment) = alignment else {
                if available_search >= self.budget.max_resync_bytes {
                    return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization));
                }
                return None;
            };
            self.pending.drain(..alignment);
            self.aligned = true;
        }

        let mut consumed = 0usize;
        while self.pending.len().saturating_sub(consumed) >= TS_PACKET_BYTES {
            if self.packets_examined >= self.budget.max_packets {
                self.pending.drain(..consumed);
                return Some(self.budget_exhausted());
            }
            let mut packet_bytes = [0_u8; TS_PACKET_BYTES];
            packet_bytes.copy_from_slice(&self.pending[consumed..consumed.saturating_add(TS_PACKET_BYTES)]);
            consumed = consumed.saturating_add(TS_PACKET_BYTES);
            self.packets_examined = self.packets_examined.saturating_add(1);
            if let Some(outcome) = self.validate_packet(&packet_bytes) {
                self.pending.drain(..consumed);
                return Some(outcome);
            }
            self.demux.push(&mut self.context, &packet_bytes);
            if let Some(reason) = self.context.malformed {
                self.pending.drain(..consumed);
                return Some(HlsTsProbeOutcome::Malformed(reason));
            }
            if mode == HlsTsInspectionMode::Prefix {
                if let Some(signature) = self.context.signature() {
                    self.pending.drain(..consumed);
                    return Some(HlsTsProbeOutcome::Found(signature));
                }
            }
        }
        self.pending.drain(..consumed);
        None
    }

    fn validate_packet(&self, bytes: &[u8; TS_PACKET_BYTES]) -> Option<HlsTsProbeOutcome> {
        if bytes[0] != Packet::SYNC_BYTE {
            return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization));
        }
        if bytes[1] & 0x80 != 0 {
            return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::TransportError));
        }
        if bytes[3] & 0xC0 != 0 {
            return Some(HlsTsProbeOutcome::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling));
        }
        let adaptation_control = (bytes[3] >> 4) & 0b11;
        let payload_offset = match adaptation_control {
            0b01 => 4,
            0b10 if bytes[4] == 183 => TS_PACKET_BYTES,
            0b11 if bytes[4] <= 182 => 5usize.saturating_add(usize::from(bytes[4])),
            0b00 | 0b10 | 0b11 => {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPacketHeader));
            }
            _ => return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPacketHeader)),
        };
        let pid = (u16::from(bytes[1] & 0x1F) << 8) | u16::from(bytes[2]);
        if self.context.is_psi_pid(pid) && bytes[1] & 0x40 != 0 {
            let Some(payload) = bytes.get(payload_offset..) else {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            };
            let Some(pointer) = payload.first().map(|value| usize::from(*value)) else {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            };
            let section_start = pointer.saturating_add(1);
            if section_start >= payload.len() {
                return Some(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer));
            }
        }
        None
    }

    fn finish(self) -> HlsTsProbeOutcome {
        if let Some(reason) = self.context.malformed {
            return HlsTsProbeOutcome::Malformed(reason);
        }
        if let Some(signature) = self.context.signature() {
            return HlsTsProbeOutcome::Found(signature);
        }
        if !self.aligned {
            return HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization);
        }
        if !self.pending.is_empty() {
            return HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompletePacket);
        }
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
    }

    fn finish_complete(mut self, remainder: HlsTsPlaintextRemainder) -> HlsTsProbeOutcome {
        if matches!(remainder, HlsTsPlaintextRemainder::Aes128Pkcs7) {
            if let Some(&padding) = self.pending.last() {
                let padding = usize::from(padding);
                if (1..=AES_128_BLOCK_BYTES).contains(&padding)
                    && self.pending.len() == padding
                    && self.pending.iter().all(|byte| usize::from(*byte) == padding)
                {
                    self.pending.clear();
                }
            }
        }
        self.finish()
    }
}

struct HlsAes128CbcPrefixDecoder {
    cipher: Aes128,
    previous_ciphertext: [u8; AES_128_BLOCK_BYTES],
    carry: Zeroizing<Vec<u8>>,
}

impl HlsAes128CbcPrefixDecoder {
    fn new(key: &[u8], iv: [u8; AES_128_BLOCK_BYTES]) -> Result<Self, HlsTsProbeError> {
        if key.len() != AES_128_BLOCK_BYTES {
            return Err(HlsTsProbeError::KeyUnavailable);
        }
        let cipher = Aes128::new_from_slice(key).map_err(|_| HlsTsProbeError::KeyUnavailable)?;
        Ok(Self { cipher, previous_ciphertext: iv, carry: Zeroizing::new(Vec::with_capacity(AES_128_BLOCK_BYTES)) })
    }

    fn push(&mut self, ciphertext: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut plaintext = Zeroizing::new(Vec::with_capacity(
            self.carry.len().saturating_add(ciphertext.len()) / AES_128_BLOCK_BYTES * AES_128_BLOCK_BYTES,
        ));
        let mut input = ciphertext;
        if !self.carry.is_empty() {
            let required = AES_128_BLOCK_BYTES.saturating_sub(self.carry.len());
            let copied = required.min(input.len());
            self.carry.extend_from_slice(&input[..copied]);
            input = &input[copied..];
            if self.carry.len() == AES_128_BLOCK_BYTES {
                let mut block = [0_u8; AES_128_BLOCK_BYTES];
                block.copy_from_slice(&self.carry);
                self.decrypt_block(block, &mut plaintext);
                self.carry.clear();
            }
        }
        let (chunks, remainder) = input.as_chunks::<AES_128_BLOCK_BYTES>();
        for chunk in chunks {
            let block = *chunk;
            self.decrypt_block(block, &mut plaintext);
        }
        self.carry.extend_from_slice(remainder);
        plaintext
    }

    fn decrypt_block(&mut self, ciphertext: [u8; AES_128_BLOCK_BYTES], plaintext: &mut Vec<u8>) {
        let mut decrypted = Block::<Aes128>::default();
        decrypted.copy_from_slice(&ciphertext);
        self.cipher.decrypt_block(&mut decrypted);
        plaintext.extend(decrypted.into_iter().zip(self.previous_ciphertext).map(|(byte, previous)| byte ^ previous));
        self.previous_ciphertext = ciphertext;
    }

    fn finish(&self) -> Result<(), HlsTsProbeError> {
        self.carry.is_empty().then_some(()).ok_or(HlsTsProbeError::DecryptionFailed)
    }
}

impl Drop for HlsAes128CbcPrefixDecoder {
    fn drop(&mut self) {
        self.previous_ciphertext.zeroize();
    }
}

enum HlsTsSourceDecoder {
    Clear,
    Aes128Cbc(Box<HlsAes128CbcPrefixDecoder>),
}

impl HlsTsSourceDecoder {
    fn new(protection: HlsTsProbeProtection<'_>) -> Result<Self, HlsTsProbeError> {
        match protection {
            HlsTsProbeProtection::Clear => Ok(Self::Clear),
            HlsTsProbeProtection::Aes128Cbc { key, iv } => {
                HlsAes128CbcPrefixDecoder::new(key, iv).map(Box::new).map(Self::Aes128Cbc)
            }
        }
    }

    fn with_plaintext<T>(&mut self, bytes: &[u8], consume: impl FnOnce(&[u8]) -> T) -> T {
        match self {
            Self::Clear => consume(bytes),
            Self::Aes128Cbc(decoder) => {
                let plaintext = decoder.push(bytes);
                consume(&plaintext)
            }
        }
    }

    const fn plaintext_remainder(&self) -> HlsTsPlaintextRemainder {
        match self {
            Self::Clear => HlsTsPlaintextRemainder::ExactPackets,
            Self::Aes128Cbc(_) => HlsTsPlaintextRemainder::Aes128Pkcs7,
        }
    }

    fn finish(&self) -> Result<(), HlsTsProbeError> {
        match self {
            Self::Clear => Ok(()),
            Self::Aes128Cbc(decoder) => decoder.finish(),
        }
    }
}

/// Inspects a blocking reader without retaining or mutating source media bytes.
pub fn inspect_mpeg_ts<R: Read>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
) -> Result<HlsTsProbeOutcome, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut inspector = HlsTsInspector::new(budget);
    loop {
        let remaining = budget.max_bytes.saturating_sub(inspector.source_bytes_examined);
        if remaining == 0 {
            return Ok(inspector.budget_exhausted());
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = match reader.read(&mut buffer[..read_limit]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HlsTsProbeError::Io(error)),
        };
        inspector.record_source_bytes(read);
        if let Some(outcome) = decoder.with_plaintext(&buffer[..read], |plaintext| inspector.feed_plaintext(plaintext))
        {
            return Ok(outcome);
        }
    }
    decoder.finish()?;
    Ok(inspector.finish())
}

/// Async cache-file entry point backed by the same parser and CBC state machine.
pub async fn inspect_mpeg_ts_async<R: AsyncRead + Unpin>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
) -> Result<HlsTsProbeOutcome, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut inspector = HlsTsInspector::new(budget);
    loop {
        let remaining = budget.max_bytes.saturating_sub(inspector.source_bytes_examined);
        if remaining == 0 {
            return Ok(inspector.budget_exhausted());
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = reader.read(&mut buffer[..read_limit]).await.map_err(HlsTsProbeError::Io)?;
        if read == 0 {
            break;
        }
        inspector.record_source_bytes(read);
        if let Some(outcome) = decoder.with_plaintext(&buffer[..read], |plaintext| inspector.feed_plaintext(plaintext))
        {
            return Ok(outcome);
        }
    }
    decoder.finish()?;
    Ok(inspector.finish())
}

/// Collects track compatibility and complete clock evidence from one exact cache reader.
///
/// PAT/PMT discovery may settle from the prefix, while timestamp collection continues to
/// EOF in bounded chunks. Valid AES-CBC padding is excluded from the aligned TS stream.
pub async fn inspect_mpeg_ts_media_evidence_async<R: AsyncRead + Unpin>(
    mut reader: R,
    protection: HlsTsProbeProtection<'_>,
    budget: HlsTsProbeBudget,
    expected_duration_ticks_90khz: u64,
) -> Result<HlsTsMediaEvidence, HlsTsProbeError> {
    let mut decoder = HlsTsSourceDecoder::new(protection)?;
    let plaintext_remainder = decoder.plaintext_remainder();
    let chunk_bytes = budget.bounded_read_chunk_bytes();
    let mut buffer = Zeroizing::new(vec![0_u8; chunk_bytes]);
    let mut track_inspector = HlsTsInspector::new(budget);
    let mut media_inspector = HlsTsMediaStreamInspector::new(budget, expected_duration_ticks_90khz);
    let mut track_outcome = None;
    let mut source_bytes_examined = 0_u64;
    loop {
        let remaining = budget.max_bytes.saturating_sub(source_bytes_examined);
        if remaining == 0 {
            return Ok(HlsTsMediaEvidence {
                track_outcome: track_outcome.unwrap_or_else(|| track_inspector.budget_exhausted()),
                timestamp_profile: None,
                splice_evidence: HlsTsSpliceEvidence::Incompatible(
                    HlsTsSpliceIncompatibility::InspectionBudgetExhausted,
                ),
            });
        }
        let read_limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(buffer.len());
        let read = reader.read(&mut buffer[..read_limit]).await.map_err(HlsTsProbeError::Io)?;
        if read == 0 {
            break;
        }
        source_bytes_examined = source_bytes_examined.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if track_outcome.is_none() {
            track_inspector.record_source_bytes(read);
        }
        decoder.with_plaintext(&buffer[..read], |plaintext| {
            media_inspector.feed_plaintext(plaintext);
            if track_outcome.is_none() {
                track_outcome = track_inspector.feed_plaintext_complete(plaintext);
            }
        });
    }
    decoder.finish()?;
    let track_outcome = track_outcome.unwrap_or_else(|| track_inspector.finish_complete(plaintext_remainder));
    let topology = match &track_outcome {
        HlsTsProbeOutcome::Found(signature) => Some(signature.clone()),
        HlsTsProbeOutcome::ProbeBudgetExhausted { .. }
        | HlsTsProbeOutcome::Malformed(_)
        | HlsTsProbeOutcome::UnsupportedProtection(_) => None,
    };
    let (timestamp_profile, splice_evidence) = media_inspector.finish(plaintext_remainder, topology);
    Ok(HlsTsMediaEvidence { track_outcome, timestamp_profile, splice_evidence })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;
    use std::{
        cell::Cell,
        io::Cursor,
        pin::Pin,
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
        task::{Context, Poll},
    };
    use tokio::io::ReadBuf;

    fn append_crc(mut section: Vec<u8>) -> Vec<u8> {
        let crc = mpegts_crc::sum32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        section
    }

    fn psi_version_byte(version: u8, current: bool) -> u8 {
        0xC0 | ((version & 0x1F) << 1) | u8::from(current)
    }

    fn pat_section_with_header(
        programs: &[(u16, u16)],
        version: u8,
        current: bool,
        section_number: u8,
        last_section_number: u8,
    ) -> Vec<u8> {
        let section_length = 5usize.saturating_add(programs.len().saturating_mul(4)).saturating_add(4);
        let mut section = vec![
            0x00,
            0xB0 | u8::try_from(section_length >> 8).unwrap_or(0),
            u8::try_from(section_length & 0xFF).unwrap_or(0),
        ];
        section.extend_from_slice(&[
            0x00,
            0x01,
            psi_version_byte(version, current),
            section_number,
            last_section_number,
        ]);
        for (program_number, pid) in programs {
            section.extend_from_slice(&program_number.to_be_bytes());
            section.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes());
        }
        append_crc(section)
    }

    fn pat_section(programs: &[(u16, u16)]) -> Vec<u8> {
        pat_section_with_header(programs, 0, true, 0, 0)
    }

    #[derive(Clone, Copy)]
    struct PmtSectionHeader {
        version: u8,
        current: bool,
        section_number: u8,
        last_section_number: u8,
    }

    fn pmt_section_with_header(
        program_number: u16,
        pcr_pid: u16,
        streams: &[(u8, u16)],
        descriptor_bytes: usize,
        header: PmtSectionHeader,
    ) -> Vec<u8> {
        let program_info_length = descriptor_bytes.min(0x0FFF) & !1;
        let section_length = 9usize
            .saturating_add(program_info_length)
            .saturating_add(streams.len().saturating_mul(5))
            .saturating_add(4);
        let mut section = vec![
            0x02,
            0xB0 | u8::try_from(section_length >> 8).unwrap_or(0),
            u8::try_from(section_length & 0xFF).unwrap_or(0),
        ];
        section.extend_from_slice(&program_number.to_be_bytes());
        section.extend_from_slice(&[
            psi_version_byte(header.version, header.current),
            header.section_number,
            header.last_section_number,
        ]);
        section.extend_from_slice(&(0xE000 | (pcr_pid & 0x1FFF)).to_be_bytes());
        section.extend_from_slice(&(0xF000 | u16::try_from(program_info_length).unwrap_or(0)).to_be_bytes());
        for _ in 0..program_info_length / 2 {
            section.extend_from_slice(&[0x80, 0x00]);
        }
        for (stream_type, pid) in streams {
            section.push(*stream_type);
            section.extend_from_slice(&(0xE000 | (pid & 0x1FFF)).to_be_bytes());
            section.extend_from_slice(&[0xF0, 0x00]);
        }
        append_crc(section)
    }

    fn pmt_section(program_number: u16, pcr_pid: u16, streams: &[(u8, u16)], descriptor_bytes: usize) -> Vec<u8> {
        pmt_section_with_header(
            program_number,
            pcr_pid,
            streams,
            descriptor_bytes,
            PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 0 },
        )
    }

    fn packetize_section(pid: u16, section: &[u8], first_counter: u8) -> Vec<u8> {
        let mut output = Vec::new();
        let mut offset = 0usize;
        let mut counter = first_counter & 0x0F;
        let mut first = true;
        while offset < section.len() {
            let mut packet = [0xFF_u8; TS_PACKET_BYTES];
            packet[0] = Packet::SYNC_BYTE;
            packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
            packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
            packet[3] = 0x10 | counter;
            let payload_start = if first {
                packet[1] |= 0x40;
                packet[4] = 0;
                5
            } else {
                4
            };
            let copied = section.len().saturating_sub(offset).min(TS_PACKET_BYTES.saturating_sub(payload_start));
            packet[payload_start..payload_start.saturating_add(copied)]
                .copy_from_slice(&section[offset..offset.saturating_add(copied)]);
            output.extend_from_slice(&packet);
            offset = offset.saturating_add(copied);
            counter = counter.wrapping_add(1) & 0x0F;
            first = false;
        }
        output
    }

    fn packetize_section_with_split_syntax_header(pid: u16, section: &[u8], first_header_bytes: usize) -> Vec<u8> {
        let first_header_bytes = first_header_bytes.min(PSI_SYNTAX_HEADER_BYTES.saturating_sub(1));
        let pid = Pid::new(pid);
        let mut output = synthetic_psi_packet(pid, true, &section[..first_header_bytes]).to_vec();
        for chunk in section[first_header_bytes..].chunks(184) {
            output.extend_from_slice(&synthetic_psi_packet(pid, false, chunk));
        }
        output
    }

    fn psi_start_packet_with_pointer(pid: u16, previous_section: &[u8], section_start: &[u8]) -> [u8; TS_PACKET_BYTES] {
        let payload_length = 1usize.saturating_add(previous_section.len()).saturating_add(section_start.len()).min(184);
        let mut packet = [0xFF_u8; TS_PACKET_BYTES];
        packet[0] = Packet::SYNC_BYTE;
        packet[1] = (u8::try_from(pid >> 8).unwrap_or(0) & 0x1F) | 0x40;
        packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
        packet[3] = 0x30;
        let adaptation_length = 183usize.saturating_sub(payload_length);
        packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
        if adaptation_length > 0 {
            packet[5] = 0;
        }
        let payload_offset = 5usize.saturating_add(adaptation_length);
        packet[payload_offset] = u8::try_from(previous_section.len()).unwrap_or(u8::MAX);
        let previous_offset = payload_offset.saturating_add(1);
        let previous_end = previous_offset.saturating_add(previous_section.len());
        packet[previous_offset..previous_end].copy_from_slice(previous_section);
        let section_end = previous_end.saturating_add(section_start.len());
        packet[previous_end..section_end].copy_from_slice(section_start);
        packet
    }

    fn null_packet() -> [u8; TS_PACKET_BYTES] {
        let mut packet = [0xFF_u8; TS_PACKET_BYTES];
        packet[0] = Packet::SYNC_BYTE;
        packet[1] = 0x1F;
        packet[2] = 0xFF;
        packet[3] = 0x10;
        packet
    }

    fn track_stream(descriptor_bytes: usize) -> Vec<u8> {
        let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102), (0x0F, 0x103)], descriptor_bytes),
            0,
        ));
        stream.extend_from_slice(&null_packet());
        stream
    }

    fn media_payload_packet(
        pid: u16,
        continuity_counter: u8,
        payload_unit_start: bool,
        discontinuity: bool,
        payload: &[u8],
    ) -> [u8; TS_PACKET_BYTES] {
        assert!(!payload.is_empty() && payload.len() <= 182);
        let mut packet = [0xFF_u8; TS_PACKET_BYTES];
        packet[0] = Packet::SYNC_BYTE;
        packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
        if payload_unit_start {
            packet[1] |= 0x40;
        }
        packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
        packet[3] = 0x30 | (continuity_counter & 0x0F);
        let adaptation_length = 183usize.saturating_sub(payload.len());
        packet[4] = u8::try_from(adaptation_length).unwrap_or(182);
        if adaptation_length > 0 {
            packet[5] = if discontinuity { 0x80 } else { 0 };
        }
        let payload_offset = 5usize.saturating_add(adaptation_length);
        packet[payload_offset..].copy_from_slice(payload);
        packet
    }

    fn media_adaptation_only_packet(pid: u16, continuity_counter: u8, discontinuity: bool) -> [u8; TS_PACKET_BYTES] {
        let mut packet = [0xFF_u8; TS_PACKET_BYTES];
        packet[0] = Packet::SYNC_BYTE;
        packet[1] = u8::try_from(pid >> 8).unwrap_or(0) & 0x1F;
        packet[2] = u8::try_from(pid & 0xFF).unwrap_or(0);
        packet[3] = 0x20 | (continuity_counter & 0x0F);
        packet[4] = 183;
        packet[5] = if discontinuity { 0x80 } else { 0 };
        packet
    }

    fn pes_bytes(stream_id: u8, declared_bytes: u16, payload_after_length: &[u8]) -> Vec<u8> {
        let mut pes = vec![0, 0, 1, stream_id];
        pes.extend_from_slice(&declared_bytes.to_be_bytes());
        pes.extend_from_slice(payload_after_length);
        pes
    }

    async fn complete_media_evidence(bytes: &[u8]) -> HlsTsMediaEvidence {
        complete_media_evidence_with_duration(bytes, 90_000).await
    }

    async fn complete_media_evidence_with_duration(
        bytes: &[u8],
        expected_duration_ticks_90khz: u64,
    ) -> HlsTsMediaEvidence {
        let source_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        inspect_mpeg_ts_media_evidence_async(
            bytes,
            HlsTsProbeProtection::Clear,
            HlsTsProbeBudget {
                max_bytes: source_size.saturating_add(1),
                max_packets: source_size.saturating_add(187).saturating_div(188).saturating_add(1),
                ..HlsTsProbeBudget::default()
            },
            expected_duration_ticks_90khz,
        )
        .await
        .expect("complete media evidence")
    }

    fn valid_track_prefix(minimum_bytes: usize) -> Vec<u8> {
        let mut stream = track_stream(0);
        let null_packet = null_packet();
        while stream.len() < minimum_bytes {
            stream.extend_from_slice(&null_packet);
        }
        stream
    }

    fn multi_packet_pat_stream() -> Vec<u8> {
        let programs =
            (0_u16..46).map(|index| (index.saturating_add(1), 0x100_u16.saturating_add(index))).collect::<Vec<_>>();
        let mut stream = packetize_section(0, &pat_section(&programs), 0);
        for (program_number, pmt_pid) in programs {
            stream.extend_from_slice(&packetize_section(
                pmt_pid,
                &pmt_section(program_number, 0x400, &[(0x1B, 0x400), (0x0F, 0x401)], 0),
                0,
            ));
        }
        stream.extend_from_slice(&null_packet());
        stream
    }

    fn found(outcome: HlsTsProbeOutcome) -> HlsTsTrackSignature {
        match outcome {
            HlsTsProbeOutcome::Found(signature) => signature,
            other => panic!("expected signature, got {other:?}"),
        }
    }

    #[test]
    fn ts_inspector_reads_single_packet_pat_and_pmt() {
        let signature = found(
            inspect_mpeg_ts(Cursor::new(track_stream(0)), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe succeeds"),
        );
        assert_eq!(signature.program_count, 1);
        assert!(signature.has_pcr);
        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
        assert_eq!(
            signature.programs.as_ref(),
            &[HlsTsProgramTopology {
                transport_stream_id: 1,
                program_number: 1,
                pmt_pid: 0x100,
                pcr_pid: 0x101,
                streams: Arc::from([
                    HlsTsElementaryStreamBinding { stream_type: 0x1B, elementary_pid: 0x101 },
                    HlsTsElementaryStreamBinding { stream_type: 0x0F, elementary_pid: 0x102 },
                    HlsTsElementaryStreamBinding { stream_type: 0x0F, elementary_pid: 0x103 },
                ]),
            }]
        );
    }

    #[test]
    fn equal_stream_types_with_different_pid_topology_are_not_compatible() {
        let mut first = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        first.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0),
            0,
        ));
        let mut second = packetize_section(0, &pat_section(&[(1, 0x200)]), 0);
        second.extend_from_slice(&packetize_section(
            0x200,
            &pmt_section(1, 0x201, &[(0x1B, 0x201), (0x0F, 0x202)], 0),
            0,
        ));

        let first = found(
            inspect_mpeg_ts(Cursor::new(first), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("first topology"),
        );
        let second = found(
            inspect_mpeg_ts(Cursor::new(second), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("second topology"),
        );

        assert_eq!(first.stream_types, second.stream_types);
        assert_ne!(first, second);
        assert_eq!(
            evaluate_mpeg_ts_splice_boundary(
                &HlsTsSpliceEvidence::compatible_for_test(first),
                &HlsTsSpliceEvidence::compatible_for_test(second),
            ),
            Err(HlsTsSpliceBoundaryIncompatibility::TopologyMismatch)
        );
    }

    #[tokio::test]
    async fn complete_media_evidence_enforces_ffmpeg_continuity_semantics() {
        let pid = 0x101;
        let exact_pes = pes_bytes(0xE0, 4, &[1, 2, 3, 4]);
        let next_pes = pes_bytes(0xE0, 0, &[0x80, 0, 0]);
        let mut valid = track_stream(0);
        valid.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
        valid.extend_from_slice(&media_adaptation_only_packet(pid, 3, false));
        valid.extend_from_slice(&media_payload_packet(pid, 4, true, false, &next_pes));
        assert!(matches!(complete_media_evidence(&valid).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

        let mut discontinuity = track_stream(0);
        discontinuity.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
        discontinuity.extend_from_slice(&media_adaptation_only_packet(pid, 11, true));
        discontinuity.extend_from_slice(&media_payload_packet(pid, 12, true, false, &next_pes));
        assert!(matches!(
            complete_media_evidence(&discontinuity).await.splice_evidence,
            HlsTsSpliceEvidence::Compatible(_)
        ));

        let mut invalid = track_stream(0);
        invalid.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
        invalid.extend_from_slice(&media_adaptation_only_packet(pid, 4, false));
        assert!(matches!(
            complete_media_evidence(&invalid).await.splice_evidence,
            HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::ContinuityFailure {
                pid: 0x101,
                expected: 3,
                actual: 4,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn complete_media_evidence_rejects_tei_and_internal_payload_jump() {
        let pid = 0x101;
        let first = pes_bytes(0xE0, 200, &[0x11; 100]);
        let continuation = [0x22; 100];
        let mut jumped = track_stream(0);
        jumped.extend_from_slice(&media_payload_packet(pid, 5, true, false, &first));
        jumped.extend_from_slice(&media_payload_packet(pid, 9, false, false, &continuation));
        assert!(matches!(
            complete_media_evidence(&jumped).await.splice_evidence,
            HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::ContinuityFailure {
                pid: 0x101,
                expected: 6,
                actual: 9,
                ..
            })
        ));

        let mut tei_packet = media_payload_packet(pid, 5, true, false, &pes_bytes(0xE0, 0, &[0x80]));
        tei_packet[1] |= 0x80;
        let mut tei = track_stream(0);
        tei.extend_from_slice(&tei_packet);
        assert!(matches!(
            complete_media_evidence(&tei).await.splice_evidence,
            HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::TransportError { pid: 0x101, .. })
        ));
    }

    #[tokio::test]
    async fn complete_media_evidence_accounts_finite_split_and_unbounded_pes() {
        let pid = 0x101;
        let mut truncated = track_stream(0);
        truncated.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 20, &[0xAA; 8])));
        assert!(matches!(
            complete_media_evidence(&truncated).await.splice_evidence,
            HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
                pid: 0x101,
                declared_bytes: Some(20),
                observed_bytes: 8,
                ..
            })
        ));

        let mut exact = track_stream(0);
        exact.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 8, &[0xAA; 8])));
        assert!(matches!(complete_media_evidence(&exact).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

        let header = pes_bytes(0xE0, 5, &[1, 2, 3, 4, 5]);
        let mut split = track_stream(0);
        split.extend_from_slice(&media_payload_packet(pid, 0, true, false, &header[..4]));
        split.extend_from_slice(&media_payload_packet(pid, 1, false, false, &header[4..]));
        assert!(matches!(complete_media_evidence(&split).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

        let mut unbounded = track_stream(0);
        unbounded.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 0, &[0x80, 0, 0, 1])));
        assert!(matches!(
            complete_media_evidence(&unbounded).await.splice_evidence,
            HlsTsSpliceEvidence::Compatible(_)
        ));

        let mut invalid = track_stream(0);
        invalid.extend_from_slice(&media_payload_packet(pid, 0, true, false, &[0x12, 0x34, 0x56]));
        assert!(matches!(
            complete_media_evidence(&invalid).await.splice_evidence,
            HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPes { pid: 0x101, .. })
        ));
    }

    #[tokio::test]
    async fn current_terminal_asset_and_finalized_segment_zero_have_complete_splice_evidence() {
        const TERMINAL_ASSET_BYTES: &[u8] =
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
        let renderer = crate::transport_stream_buffer::TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let duration_ticks = renderer.duration_ticks_90khz().expect("terminal asset duration");
        let asset_profile = renderer.finite_hls_timestamp_profile().expect("terminal asset profile");
        let prepared = renderer
            .render_finite_hls_segment(crate::transport_stream_buffer::HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("prepared terminal segment zero");
        let finalized = renderer
            .finalize_prepared_finite_hls_segment(
                &prepared,
                crate::transport_stream_buffer::HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 90_000,
                    discontinuity: crate::transport_stream_buffer::HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            )
            .expect("finalized terminal segment zero");

        let base = complete_media_evidence_with_duration(TERMINAL_ASSET_BYTES, duration_ticks).await;
        let terminal = complete_media_evidence_with_duration(&finalized, duration_ticks).await;

        assert_eq!(base.timestamp_profile, Some(asset_profile));
        assert!(matches!(base.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
        assert!(matches!(terminal.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
        assert_eq!(evaluate_mpeg_ts_splice_boundary(&base.splice_evidence, &terminal.splice_evidence), Ok(()));
        assert_eq!(
            terminal.timestamp_profile.map(|profile| profile.span_ticks_90khz),
            Some(asset_profile.span_ticks_90khz)
        );
        assert!(duration_ticks > asset_profile.span_ticks_90khz);
    }

    #[test]
    fn ts_inspector_reassembles_multi_packet_and_cross_chunk_psi() {
        let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };
        let track_signature = found(
            inspect_mpeg_ts(Cursor::new(track_stream(400)), HlsTsProbeProtection::Clear, budget)
                .expect("probe succeeds"),
        );
        assert_eq!(track_signature.stream_types.as_ref(), &[0x0F, 0x1B]);

        let program_signature = found(
            inspect_mpeg_ts(Cursor::new(multi_packet_pat_stream()), HlsTsProbeProtection::Clear, budget)
                .expect("probe succeeds"),
        );
        assert_eq!(program_signature.program_count, 46);
        assert_eq!(program_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    }

    #[test]
    fn ts_inspector_restarts_incomplete_pat_on_new_current_version() {
        let mut stream = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x101,
                &[(0x02, 0x101)],
                0,
                PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
            ),
            0,
        ));
        stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[(2, 0x200)], 1, true, 0, 0), 1));
        stream.extend_from_slice(&packetize_section(
            0x200,
            &pmt_section(2, 0x201, &[(0x1B, 0x201), (0x0F, 0x202)], 0),
            0,
        ));
        stream.extend_from_slice(&null_packet());

        let signature = found(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe succeeds"),
        );

        assert_eq!(signature.program_count, 1);
        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    }

    #[test]
    fn ts_inspector_restarts_incomplete_pmt_on_new_current_version() {
        let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x101,
                &[(0x1B, 0x101)],
                0,
                PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
            ),
            0,
        ));
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x201,
                &[(0x24, 0x201), (0x0F, 0x202)],
                0,
                PmtSectionHeader { version: 1, current: true, section_number: 0, last_section_number: 0 },
            ),
            1,
        ));
        stream.extend_from_slice(&null_packet());

        let signature = found(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe succeeds"),
        );

        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x24]);
    }

    #[test]
    fn ts_inspector_never_mixes_pat_or_pmt_sections_across_versions() {
        let mut pat_version_mix = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
        pat_version_mix.extend_from_slice(&packetize_section(
            0,
            &pat_section_with_header(&[(2, 0x200)], 1, true, 1, 1),
            1,
        ));
        pat_version_mix.extend_from_slice(&packetize_section(0x200, &pmt_section(2, 0x201, &[(0x1B, 0x201)], 0), 0));
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(pat_version_mix), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default(),)
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
        );

        let mut program_map_version_mix = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        program_map_version_mix.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x101,
                &[(0x1B, 0x101)],
                0,
                PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
            ),
            0,
        ));
        program_map_version_mix.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x102,
                &[(0x0F, 0x102)],
                0,
                PmtSectionHeader { version: 1, current: true, section_number: 1, last_section_number: 1 },
            ),
            1,
        ));
        assert_eq!(
            inspect_mpeg_ts(
                Cursor::new(program_map_version_mix),
                HlsTsProbeProtection::Clear,
                HlsTsProbeBudget::default(),
            )
            .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
        );
    }

    #[test]
    fn ts_inspector_rejects_same_version_pat_and_pmt_section_contradictions() {
        let mut contradictory_pat = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
        contradictory_pat.extend_from_slice(&packetize_section(
            0,
            &pat_section_with_header(&[(1, 0x101)], 0, true, 0, 1),
            1,
        ));
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(contradictory_pat), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default(),)
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPat)
        );

        let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        for (counter, stream_type) in [(0, 0x1B), (1, 0x0F)] {
            stream.extend_from_slice(&packetize_section(
                0x100,
                &pmt_section_with_header(
                    1,
                    0x101,
                    &[(stream_type, 0x101)],
                    0,
                    PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
                ),
                counter,
            ));
        }

        assert_eq!(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
        );
    }

    #[test]
    fn ts_inspector_next_pat_or_pmt_does_not_change_current_evidence() {
        let mut stream = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
        stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[(2, 0x200)], 1, false, 1, 1), 1));
        stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[], 0, true, 1, 1), 2));
        for (counter, header, stream_type, pcr_pid, elementary_pid) in [
            (
                0,
                PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
                0x1B,
                0x101,
                0x101,
            ),
            (
                1,
                PmtSectionHeader { version: 1, current: false, section_number: 1, last_section_number: 1 },
                0x24,
                0x201,
                0x201,
            ),
            (
                2,
                PmtSectionHeader { version: 0, current: true, section_number: 1, last_section_number: 1 },
                0x0F,
                0x101,
                0x102,
            ),
        ] {
            stream.extend_from_slice(&packetize_section(
                0x100,
                &pmt_section_with_header(1, pcr_pid, &[(stream_type, elementary_pid)], 0, header),
                counter,
            ));
        }

        let signature = found(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe succeeds"),
        );

        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    }

    #[test]
    fn ts_inspector_reassembles_syntax_headers_split_across_packets() {
        let mut stream = packetize_section_with_split_syntax_header(0, &pat_section(&[(1, 0x100)]), 2);
        stream.extend_from_slice(&packetize_section_with_split_syntax_header(
            0x100,
            &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0),
            1,
        ));
        stream.extend_from_slice(&null_packet());
        let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };

        let signature =
            found(inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, budget).expect("probe succeeds"));

        assert_eq!(signature.program_count, 1);
        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);

        let mut stream = packetize_section_with_split_syntax_header(0, &pat_section(&[(1, 0x100)]), 5);
        stream.extend_from_slice(&packetize_section_with_split_syntax_header(
            0x100,
            &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0),
            7,
        ));
        stream.extend_from_slice(&null_packet());
        let signature =
            found(inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, budget).expect("probe succeeds"));
        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    }

    #[test]
    fn ts_inspector_accepts_pusi_pointer_that_completes_pending_syntax_header() {
        let pat = pat_section(&[(1, 0x100)]);
        let pmt = pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0);
        let mut stream = synthetic_psi_packet(Pid::new(0), true, &pat[..2]).to_vec();
        stream.extend_from_slice(&psi_start_packet_with_pointer(0, &pat[2..], &pat));
        stream.extend_from_slice(&synthetic_psi_packet(Pid::new(0x100), true, &pmt[..5]));
        stream.extend_from_slice(&psi_start_packet_with_pointer(0x100, &pmt[5..], &pmt));
        stream.extend_from_slice(&null_packet());

        let signature = found(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe succeeds"),
        );

        assert_eq!(signature.program_count, 1);
        assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    }

    #[test]
    fn ts_inspector_rejects_pusi_without_section_start_bytes() {
        let mut stream = psi_start_packet_with_pointer(0, &[], &[]).to_vec();
        stream.extend_from_slice(&null_packet());

        let outcome = inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes");

        assert!(matches!(outcome, HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer)));
    }

    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: Rc<Cell<usize>>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = std::io::Read::read(&mut self.inner, buffer)?;
            self.bytes_read.set(self.bytes_read.get().saturating_add(read));
            Ok(read)
        }
    }

    struct VirtualTailReader {
        prefix: Arc<[u8]>,
        logical_len: u64,
        position: u64,
        bytes_read: Arc<AtomicU64>,
    }

    impl VirtualTailReader {
        fn new(prefix: Arc<[u8]>, logical_len: u64, bytes_read: Arc<AtomicU64>) -> Self {
            Self { prefix, logical_len, position: 0, bytes_read }
        }

        fn read_into(&mut self, output: &mut [u8]) -> usize {
            let remaining = self.logical_len.saturating_sub(self.position);
            let read = usize::try_from(remaining).unwrap_or(usize::MAX).min(output.len());
            if read == 0 {
                return 0;
            }
            let position = usize::try_from(self.position).unwrap_or(usize::MAX);
            let prefix_read = self.prefix.len().saturating_sub(position).min(read);
            if prefix_read > 0 {
                output[..prefix_read].copy_from_slice(&self.prefix[position..position.saturating_add(prefix_read)]);
            }
            output[prefix_read..read].fill(0);
            self.position = self.position.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            self.bytes_read.fetch_add(u64::try_from(read).unwrap_or(u64::MAX), Ordering::Relaxed);
            read
        }
    }

    impl Read for VirtualTailReader {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            Ok(self.read_into(output))
        }
    }

    impl AsyncRead for VirtualTailReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let read = self.as_mut().get_mut().read_into(output.initialize_unfilled());
            output.advance(read);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn ts_inspector_async_clear_stops_in_small_prefix_of_logical_large_segment() {
        const LOGICAL_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;
        let prefix: Arc<[u8]> = valid_track_prefix(HLS_TS_PROBE_READ_CHUNK_BYTES).into();
        let sync_read = Arc::new(AtomicU64::new(0));
        let async_read = Arc::new(AtomicU64::new(0));

        let sync_signature = found(
            inspect_mpeg_ts(
                VirtualTailReader::new(Arc::clone(&prefix), LOGICAL_SEGMENT_BYTES, Arc::clone(&sync_read)),
                HlsTsProbeProtection::Clear,
                HlsTsProbeBudget::default(),
            )
            .expect("sync probe succeeds"),
        );
        let async_signature = found(
            inspect_mpeg_ts_async(
                VirtualTailReader::new(prefix, LOGICAL_SEGMENT_BYTES, Arc::clone(&async_read)),
                HlsTsProbeProtection::Clear,
                HlsTsProbeBudget::default(),
            )
            .await
            .expect("async probe succeeds"),
        );

        assert_eq!(async_signature, sync_signature);
        assert_eq!(async_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
        assert!(sync_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
        assert!(async_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
    }

    fn encrypt_aes128_cbc(plaintext: &[u8], key: &[u8; AES_128_BLOCK_BYTES], iv: [u8; AES_128_BLOCK_BYTES]) -> Vec<u8> {
        let cipher = Aes128::new_from_slice(key).expect("valid test key");
        let mut previous = iv;
        let mut output = Vec::with_capacity(plaintext.len());
        for chunk in plaintext.as_chunks::<AES_128_BLOCK_BYTES>().0 {
            let mut block = Block::<Aes128>::default();
            for ((byte, plaintext), previous) in block.iter_mut().zip(chunk).zip(previous) {
                *byte = plaintext ^ previous;
            }
            cipher.encrypt_block(&mut block);
            previous.copy_from_slice(&block);
            output.extend_from_slice(&block);
        }
        output
    }

    fn encrypt_aes128_cbc_pkcs7(
        plaintext: &[u8],
        key: &[u8; AES_128_BLOCK_BYTES],
        iv: [u8; AES_128_BLOCK_BYTES],
    ) -> Vec<u8> {
        let padding = AES_128_BLOCK_BYTES - plaintext.len() % AES_128_BLOCK_BYTES;
        let mut padded = plaintext.to_vec();
        padded.resize(padded.len().saturating_add(padding), u8::try_from(padding).unwrap_or(0));
        encrypt_aes128_cbc(&padded, key, iv)
    }

    #[tokio::test]
    async fn aes128_terminal_base_timestamp_profile_uses_decrypted_cached_bytes() {
        const TERMINAL_ASSET_BYTES: &[u8] =
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
        let asset = crate::transport_stream_buffer::TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let expected_profile = asset.finite_hls_timestamp_profile().expect("fixture timestamp profile");
        let expected_signature = asset.finite_hls_track_signature().expect("fixture track signature");
        let key = *b"0123456789abcdef";
        let iv = [0xA5; AES_128_BLOCK_BYTES];
        let ciphertext = encrypt_aes128_cbc_pkcs7(TERMINAL_ASSET_BYTES, &key, iv);
        let source_size = u64::try_from(ciphertext.len()).expect("ciphertext size");
        let budget = HlsTsProbeBudget {
            max_bytes: source_size.saturating_add(1),
            max_packets: source_size.saturating_add(187) / 188 + 1,
            ..HlsTsProbeBudget::default()
        };

        let evidence = inspect_mpeg_ts_media_evidence_async(
            &ciphertext[..],
            HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
            budget,
            asset.duration_ticks_90khz().expect("fixture duration"),
        )
        .await
        .expect("AES media evidence");

        assert_eq!(evidence.track_outcome, HlsTsProbeOutcome::Found(expected_signature));
        assert_eq!(evidence.timestamp_profile, Some(expected_profile));
        assert!(matches!(evidence.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
    }

    #[tokio::test]
    async fn ts_inspector_async_aes_stops_in_small_prefix_of_logical_large_segment() {
        const LOGICAL_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;
        let key = *b"0123456789abcdef";
        let iv = [0x5A; AES_128_BLOCK_BYTES];
        let mut plaintext = valid_track_prefix(HLS_TS_PROBE_READ_CHUNK_BYTES);
        plaintext.resize(plaintext.len().next_multiple_of(AES_128_BLOCK_BYTES), 0xFF);
        let ciphertext: Arc<[u8]> = encrypt_aes128_cbc(&plaintext, &key, iv).into();
        let sync_read = Arc::new(AtomicU64::new(0));
        let async_read = Arc::new(AtomicU64::new(0));

        let sync_signature = found(
            inspect_mpeg_ts(
                VirtualTailReader::new(Arc::clone(&ciphertext), LOGICAL_SEGMENT_BYTES, Arc::clone(&sync_read)),
                HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
                HlsTsProbeBudget::default(),
            )
            .expect("sync AES probe succeeds"),
        );
        let async_signature = found(
            inspect_mpeg_ts_async(
                VirtualTailReader::new(ciphertext, LOGICAL_SEGMENT_BYTES, Arc::clone(&async_read)),
                HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
                HlsTsProbeBudget::default(),
            )
            .await
            .expect("async AES probe succeeds"),
        );

        assert_eq!(async_signature, sync_signature);
        assert_eq!(async_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
        assert!(sync_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
        assert!(async_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
    }

    #[tokio::test]
    async fn ts_inspector_sync_async_and_aes_prefix_paths_share_signature() {
        let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };
        let mut plaintext = track_stream(400);
        let clear_signature = found(
            inspect_mpeg_ts(Cursor::new(&plaintext), HlsTsProbeProtection::Clear, budget).expect("sync probe succeeds"),
        );
        let async_clear_signature = found(
            inspect_mpeg_ts_async(&plaintext[..], HlsTsProbeProtection::Clear, budget)
                .await
                .expect("async clear probe succeeds"),
        );
        plaintext.resize(plaintext.len().next_multiple_of(AES_128_BLOCK_BYTES), 0xFF);
        let key = *b"0123456789abcdef";
        let iv = [0xA5; AES_128_BLOCK_BYTES];
        let ciphertext = encrypt_aes128_cbc(&plaintext, &key, iv);
        let async_aes_signature = found(
            inspect_mpeg_ts_async(&ciphertext[..], HlsTsProbeProtection::Aes128Cbc { key: &key, iv }, budget)
                .await
                .expect("async AES probe succeeds"),
        );

        assert_eq!(async_clear_signature, clear_signature);
        assert_eq!(async_aes_signature, clear_signature);
    }

    #[test]
    fn ts_inspector_reports_probe_budget_without_reading_whole_source() {
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(&null_packet());
        }
        let count = Rc::new(Cell::new(0));
        let reader = CountingReader { inner: Cursor::new(bytes), bytes_read: Rc::clone(&count) };
        let budget = HlsTsProbeBudget { max_bytes: (TS_PACKET_BYTES * 4) as u64, ..HlsTsProbeBudget::default() };
        let outcome = inspect_mpeg_ts(reader, HlsTsProbeProtection::Clear, budget).expect("probe succeeds");
        assert!(matches!(outcome, HlsTsProbeOutcome::ProbeBudgetExhausted { .. }));
        assert_eq!(count.get(), TS_PACKET_BYTES * 4);
    }

    #[test]
    fn ts_inspector_policy_resolution_preserves_all_probe_reasons() {
        assert_eq!(
            HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::ProbeBudgetExhausted {
                bytes_examined: 512,
                packets_examined: 2,
            })),
            HlsTrackEvidenceResolution::InsufficientEvidence { bytes_examined: 512, packets_examined: 2 }
        );
        assert_eq!(
            HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt,)))
                .reason_code(),
            "invalid-pmt"
        );
        assert_eq!(
            HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::Malformed(
                HlsTsMalformedReason::IncompleteProgramMetadata,
            ))),
            HlsTrackEvidenceResolution::IncompleteEvidence
        );
        let unsupported = HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::UnsupportedProtection(
            HlsTsProtectionReason::TransportScrambling,
        )));
        assert_eq!(
            unsupported,
            HlsTrackEvidenceResolution::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling)
        );
        assert_eq!(unsupported.reason_code(), "transport-scrambling");
        let key_unavailable = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::KeyUnavailable));
        assert_eq!(key_unavailable, HlsTrackEvidenceResolution::KeyUnavailable);
        assert_eq!(key_unavailable.reason_code(), "key-unavailable");
        let invalid_iv = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::InvalidIv));
        assert_eq!(invalid_iv, HlsTrackEvidenceResolution::InvalidIv);
        assert_eq!(invalid_iv.reason_code(), "invalid-iv");
        let decryption_failed = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::DecryptionFailed));
        assert_eq!(decryption_failed, HlsTrackEvidenceResolution::DecryptionFailed);
        assert_eq!(decryption_failed.reason_code(), "decryption-failed");
        let io = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))));
        assert_eq!(io, HlsTrackEvidenceResolution::Io(std::io::ErrorKind::PermissionDenied));
        assert_eq!(io.reason_code(), "io");
    }

    #[test]
    fn ts_inspector_resynchronizes_only_within_named_budget() {
        let budget = HlsTsProbeBudget { max_resync_bytes: 16, ..HlsTsProbeBudget::default() };
        let mut within_budget = vec![0xAA; 16];
        within_budget.extend_from_slice(&track_stream(0));
        assert!(matches!(
            inspect_mpeg_ts(Cursor::new(within_budget), HlsTsProbeProtection::Clear, budget)
                .expect("bounded resync probe completes"),
            HlsTsProbeOutcome::Found(_)
        ));

        let mut outside_budget = vec![0xAA; 17];
        outside_budget.extend_from_slice(&track_stream(0));
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(outside_budget), HlsTsProbeProtection::Clear, budget)
                .expect("out-of-budget resync probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization)
        );
    }

    #[test]
    fn ts_inspector_types_framing_crc_and_pmt_errors() {
        let invalid_sync = vec![0_u8; TS_PACKET_BYTES * 3];
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(invalid_sync), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization)
        );

        let mut invalid_crc = track_stream(0);
        invalid_crc[10] ^= 0x01;
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(invalid_crc), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiCrc)
        );

        let mut invalid_pmt = pmt_section(1, 0x101, &[(0x1B, 0x101)], 0);
        invalid_pmt[15] = 0xFF;
        invalid_pmt[16] = 0xFF;
        let crc_start = invalid_pmt.len().saturating_sub(4);
        invalid_pmt.truncate(crc_start);
        invalid_pmt = append_crc(invalid_pmt);
        let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        stream.extend_from_slice(&packetize_section(0x100, &invalid_pmt, 0));
        stream.extend_from_slice(&null_packet());
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
        );

        let mut malformed_descriptors = pmt_section(1, 0x101, &[(0x1B, 0x101)], 4);
        malformed_descriptors[13] = 5;
        let crc_start = malformed_descriptors.len().saturating_sub(4);
        malformed_descriptors.truncate(crc_start);
        malformed_descriptors = append_crc(malformed_descriptors);
        let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
        stream.extend_from_slice(&packetize_section(0x100, &malformed_descriptors, 0));
        stream.extend_from_slice(&null_packet());
        assert_eq!(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
        );
    }

    #[test]
    fn ts_inspector_rejects_invalid_psi_header_before_exhausting_probe_budget() {
        let mut invalid_pat = pat_section(&[(1, 0x100)]);
        invalid_pat[1] &= 0x7F;
        invalid_pat[1] |= 0x0F;
        invalid_pat[2] = 0xFF;
        let mut stream = packetize_section(0, &invalid_pat, 0);
        let null_packet = null_packet();
        while stream.len() <= usize::try_from(HLS_TS_PROBE_MAX_BYTES).unwrap_or(usize::MAX) {
            stream.extend_from_slice(&null_packet);
        }

        assert_eq!(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPat)
        );
    }

    #[test]
    fn ts_inspector_types_scrambled_transport_as_unsupported_protection() {
        let mut stream = track_stream(0);
        stream[3] |= 0x80;

        assert_eq!(
            inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
                .expect("probe completes"),
            HlsTsProbeOutcome::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling)
        );
    }

    #[test]
    fn ts_inspector_applies_explicit_and_sequence_derived_hls_ivs() {
        let sequence_iv = hls_aes128_cbc_iv(None, 0x0102_0304_0506_0708).expect("sequence IV");
        assert_eq!(sequence_iv, [0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let explicit_iv = hls_aes128_cbc_iv(Some("0x10203"), 0).expect("explicit IV");
        assert_eq!(explicit_iv, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]);
        assert!(matches!(hls_aes128_cbc_iv(Some("10203"), 0), Err(HlsTsProbeError::InvalidIv)));
    }

    #[test]
    fn ts_inspector_does_not_mutate_source_bytes() {
        let bytes = track_stream(0);
        let before = bytes.clone();
        let _ = inspect_mpeg_ts(Cursor::new(&bytes), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds");
        assert_eq!(bytes, before);
    }
}
