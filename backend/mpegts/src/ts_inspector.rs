use crate::transport_stream_buffer::HlsTsTimestampProfileScanner;
use aes::Aes128;
use mpeg2ts_reader::{
    demultiplex::{self, FilterChangeset},
    packet::{Packet, Pid},
    psi::{self, BufferSectionSyntaxParser, SectionPacketConsumer, SectionProcessor, SectionSyntaxSectionProcessor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use zeroize::Zeroizing;

const AES_128_BLOCK_BYTES: usize = 16;

const TS_PACKET_BYTES: usize = Packet::SIZE;

const TS_ALIGNMENT_CONFIRMATION_PACKETS: usize = 2;

const HLS_TS_PROBE_MAX_BYTES: u64 = 2 * 1024 * 1024;

const HLS_TS_PROBE_MAX_PACKETS: u64 = 8_192;

const HLS_TS_PROBE_READ_CHUNK_BYTES: usize = 64 * 1024;

const HLS_TS_PROBE_MAX_RESYNC_BYTES: usize = TS_PACKET_BYTES * 4;

const PSI_SYNTAX_HEADER_BYTES: usize = psi::SectionCommonHeader::SIZE + psi::TableSyntaxHeader::SIZE;

/// Stable PAT/PMT compatibility evidence. It is not cross-host content identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsTrackSignature {
    pub program_count: u16,
    pub has_pcr: bool,
    pub stream_types: Arc<[u8]>,
    programs: Arc<[HlsTsProgramTopology]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTsCompatibleSpliceEvidence {
    topology: HlsTsTrackSignature,
    pid_boundaries: Arc<[HlsTsPidBoundaryEvidence]>,
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

struct HlsPatPacketFilter {
    consumer: HlsPsiSectionConsumer<SectionSyntaxSectionProcessor<BufferSectionSyntaxParser<HlsPatSectionParser>>>,
}

struct HlsPmtPacketFilter {
    consumer: HlsPsiSectionConsumer<SectionSyntaxSectionProcessor<BufferSectionSyntaxParser<HlsPmtSectionParser>>>,
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

struct HlsTsDemuxContext {
    changeset: FilterChangeset<HlsTsPacketFilter>,
    pat_table_identity: Option<(u16, u8)>,
    pat_last_section_number: Option<u8>,
    pat_sections: BTreeMap<u8, BTreeMap<u16, u16>>,
    programs_by_pid: BTreeMap<u16, u16>,
    pmt_evidence: BTreeMap<u16, HlsPmtEvidence>,
    malformed: Option<HlsTsMalformedReason>,
}

struct HlsPmtSectionParser {
    pid: Pid,
    program_number: u16,
}

#[derive(Clone, Copy)]
struct HlsTsPidContinuityState {
    first_packet_index: u64,
    first_continuity_counter: u8,
    first_has_payload: bool,
    first_discontinuity: bool,
    last_continuity_counter: u8,
}

struct HlsTsTransportStreamInspector {
    continuity: Vec<Option<HlsTsPidContinuityState>>,
    pes: Vec<Option<HlsTsPesInspectionState>>,
    invalid_pes_starts: Vec<Option<u64>>,
    incompatible: Option<HlsTsSpliceIncompatibility>,
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

struct HlsTsInspector {
    budget: HlsTsProbeBudget,
    demux: demultiplex::Demultiplex<HlsTsDemuxContext>,
    context: HlsTsDemuxContext,
    pending: Vec<u8>,
    aligned: bool,
    source_bytes_examined: u64,
    packets_examined: u64,
}

struct HlsAes128CbcPrefixDecoder {
    cipher: Aes128,
    previous_ciphertext: [u8; AES_128_BLOCK_BYTES],
    carry: Zeroizing<Vec<u8>>,
}

#[cfg(test)]
mod tests;

mod demux;
mod evidence;
mod inspector;
mod protection;
mod sections;
mod transport;
use demux::HlsPatSectionParser;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use evidence::{
    evaluate_mpeg_ts_splice_boundary, HlsTrackEvidenceResolution, HlsTsElementaryStreamBinding, HlsTsMalformedReason,
    HlsTsMediaEvidence, HlsTsPidBoundaryEvidence, HlsTsProbeBudget, HlsTsProbeError, HlsTsProbeOutcome,
    HlsTsProbeProtection, HlsTsProgramTopology, HlsTsProtectionReason, HlsTsSpliceBoundaryIncompatibility,
    HlsTsSpliceEvidence, HlsTsSpliceIncompatibility,
};
pub use inspector::{inspect_mpeg_ts, inspect_mpeg_ts_async, inspect_mpeg_ts_media_evidence_async};
pub use protection::hls_aes128_cbc_iv;
#[cfg(test)]
use sections::synthetic_psi_packet;
use sections::{HlsPsiTableKind, HlsTsPacketFilter};
use transport::HlsTsPesInspectionState;
