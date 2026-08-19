use bytes::{Bytes, BytesMut};
use futures::task::AtomicWaker;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    task::Waker,
};

// PCR wraps at 2^33 * 300 (base is 33-bit, multiplied by 300 to get 27 MHz units).
// Using 1<<42 was slightly too large and could cause strict-decoder issues on hardware
// that computes modulo 2^33 on the base before multiplying.
const MAX_PCR: u64 = (1u64 << 33) * 300;
const MAX_PTS_DTS: u64 = 1 << 33; // 33 bit PTS/DTS cycle
const HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ: u64 = 90;
const HLS_TS_PROFILE_MIN_TOLERANCE_TICKS_90KHZ: u64 = 2 * 90_000;

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn add_pts_dts_offset(timestamp: u64, offset_90khz: u64) -> u64 {
    ((u128::from(timestamp) + u128::from(offset_90khz)) % u128::from(MAX_PTS_DTS)) as u64
}

#[inline]
fn forward_clock_distance_90khz(start: u64, end: u64) -> u64 {
    end.wrapping_add(MAX_PTS_DTS).wrapping_sub(start) % MAX_PTS_DTS
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn pcr_offset_27mhz(offset_90khz: u64) -> u64 {
    (u128::from(offset_90khz) * 300_u128 % u128::from(MAX_PCR)) as u64
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn add_pcr_offset_27mhz(timestamp_27mhz: u64, offset_27mhz: u64) -> u64 {
    ((u128::from(timestamp_27mhz) + u128::from(offset_27mhz)) % u128::from(MAX_PCR)) as u64
}

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PACKET_COUNT: usize = 7; // Reduced from 250 to 7 (1316 bytes) to prevent latency/timeout on low-bitrate streams
const MAX_PACKET_COUNT: usize = 250;

/// Packets per emitted chunk; overridable via `TULIPROX_TS_CHUNK_PACKETS` (1-250).
/// Larger chunks raise throughput, smaller chunks lower latency on low-bitrate streams.
fn ts_chunk_packet_count() -> usize {
    static PACKET_COUNT_OVERRIDE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("TULIPROX_TS_CHUNK_PACKETS")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|count| (1..=MAX_PACKET_COUNT).contains(count))
            .unwrap_or(PACKET_COUNT)
    });
    *PACKET_COUNT_OVERRIDE
}

const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10; // PCR flag bit in adaptation field flags
const NULL_PID: u16 = 0x1FFF;
const HLS_TS_MAX_PENDING_PES_HEADERS: usize = 64;
const HLS_TS_TIMESTAMP_HEADER_BYTES: usize = 19;

/// Byte offset of PTS within a PES payload (after the 3-byte start code, `stream_id`, length, flags).
const PES_PTS_OFFSET: usize = 9;
/// Byte offset of DTS within a PES payload when both PTS and DTS are present.
const PES_DTS_OFFSET: usize = 14;

/// Decodes a 5-byte DTS/PTS field from PES header into u64 timestamp.
#[inline]
fn decode_timestamp(ts_bytes: &[u8]) -> u64 {
    (((u64::from(ts_bytes[0]) >> 1) & 0x07) << 30)
        | (u64::from(ts_bytes[1]) << 22)
        | (((u64::from(ts_bytes[2]) >> 1) & 0x7F) << 15)
        | (u64::from(ts_bytes[3]) << 7)
        | ((u64::from(ts_bytes[4]) >> 1) & 0x7F)
}

/// Encodes a u64 timestamp into 5-byte PES DTS/PTS field
#[inline]
fn encode_timestamp(ts: u64) -> [u8; 5] {
    [
        0x20 | ((((ts >> 30) & 0x07) as u8) << 1) | 1,
        ((ts >> 22) & 0xFF) as u8,
        ((((ts >> 15) & 0x7F) as u8) << 1) | 1,
        ((ts >> 7) & 0xFF) as u8,
        (((ts & 0x7F) as u8) << 1) | 1,
    ]
}

/// Decode PCR from 6 bytes (adaptation field) into 42-bit PCR base + 9-bit extension as u64
#[inline]
fn decode_pcr(pcr_bytes: &[u8]) -> u64 {
    let pcr_base = (u64::from(pcr_bytes[0]) << 25)
        | ((u64::from(pcr_bytes[1])) << 17)
        | ((u64::from(pcr_bytes[2])) << 9)
        | ((u64::from(pcr_bytes[3])) << 1)
        | ((u64::from(pcr_bytes[4])) >> 7);
    let pcr_ext = ((u64::from(pcr_bytes[4]) & 1) << 8) | u64::from(pcr_bytes[5]);
    pcr_base * 300 + pcr_ext
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsTsTimestampFieldKind {
    Pts,
    Dts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsTimestampFieldLocation {
    pub pid: u16,
    pub kind: HlsTsTimestampFieldKind,
    /// Absolute byte offsets in the immutable aligned TS buffer. One field may span packets.
    pub byte_offsets: [usize; 5],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsPcrFieldLocation {
    pub pid: u16,
    pub byte_offset: usize,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
enum HlsFiniteTsLayoutError {
    #[error("transport stream asset is empty or invalid")]
    InvalidAsset,
    #[error("transport stream packet layout is invalid")]
    InvalidTransportPacket,
    #[error("transport stream timestamp location is invalid")]
    InvalidTimestampLocation,
    #[error("PES timestamp header for PID {pid} uses an unsupported layout")]
    UnsupportedPesTimestampHeader { pid: u16 },
    #[error("PES {kind:?} field for PID {pid} is invalid")]
    InvalidPesTimestampField { pid: u16, kind: HlsTsTimestampFieldKind },
    #[error("PES timestamp header for PID {pid} was interrupted by a new payload unit")]
    PesTimestampHeaderInterrupted { pid: u16 },
    #[error(
        "PES timestamp header continuity failed for PID {pid}: expected {expected:?}, actual {actual}"
    )]
    PesTimestampContinuityDiscontinuity {
        pid: u16,
        expected: Option<u8>,
        actual: u8,
    },
    #[error("PES timestamp header for PID {pid} is incomplete")]
    IncompletePesTimestampHeader { pid: u16 },
    #[error("too many concurrent split PES timestamp headers")]
    TooManyPendingPesTimestampHeaders,
    #[error("transport stream contains no presentation clock")]
    PresentationClockUnavailable,
    #[error("PID {pid} has no defensible presentation cadence")]
    PresentationCadenceUnavailable { pid: u16 },
    #[error("transport stream presentation duration overflows its clock domain")]
    PresentationDurationOverflow,
}

#[derive(Clone, Debug)]
pub(crate) struct HlsPendingPesHeader {
    pub pid: u16,
    pub bytes: [u8; HLS_TS_TIMESTAMP_HEADER_BYTES],
    pub byte_offsets: [usize; HLS_TS_TIMESTAMP_HEADER_BYTES],
    pub len: usize,
    pub expected_len: Option<usize>,
    last_payload_continuity_counter: u8,
}

impl HlsPendingPesHeader {
    fn new(pid: u16, continuity_counter: u8) -> Self {
        Self {
            pid,
            bytes: [0; HLS_TS_TIMESTAMP_HEADER_BYTES],
            byte_offsets: [0; HLS_TS_TIMESTAMP_HEADER_BYTES],
            len: 0,
            expected_len: None,
            last_payload_continuity_counter: continuity_counter,
        }
    }
}

#[derive(Clone, Copy)]
struct HlsCompletedTimestampField {
    location: HlsTsTimestampFieldLocation,
    bytes: [u8; 5],
}

#[derive(Clone, Copy, Default)]
struct HlsCompletedPesTimestamps {
    fields: [Option<HlsCompletedTimestampField>; 2],
}

#[derive(Clone, Copy)]
struct HlsTsPacketEvidence {
    pid: u16,
    payload_unit_start: bool,
    payload_offset: Option<usize>,
    continuity_counter: u8,
    discontinuity: bool,
    pcr_field: Option<HlsTsPcrFieldLocation>,
}

impl HlsTsPacketEvidence {
    const fn has_payload(self) -> bool { self.payload_offset.is_some() }
}

fn inspect_hls_ts_packet(
    packet: &[u8],
    packet_start: usize,
) -> Result<HlsTsPacketEvidence, HlsFiniteTsLayoutError> {
    if packet.len() != TS_PACKET_SIZE
        || packet[0] != SYNC_BYTE
        || packet[1] & 0x80 != 0
        || packet[3] & 0xC0 != 0
    {
        return Err(HlsFiniteTsLayoutError::InvalidTransportPacket);
    }
    let pid = ts_packet_pid(packet);
    let adaptation_field_control = (packet[3] >> 4) & 0b11;
    let continuity_counter = packet[3] & 0x0F;
    let (payload_offset, adaptation_length) = match adaptation_field_control {
        0b01 => (Some(4), None),
        0b10 if packet[4] == 183 => (None, Some(183)),
        0b11 if packet[4] <= 182 => {
            let adaptation_length = usize::from(packet[4]);
            (Some(5usize.saturating_add(adaptation_length)), Some(adaptation_length))
        }
        _ => return Err(HlsFiniteTsLayoutError::InvalidTransportPacket),
    };
    let mut discontinuity = false;
    let mut pcr_field = None;
    if let Some(adaptation_length) = adaptation_length {
        if adaptation_length > 0 {
            discontinuity = packet[5] & 0x80 != 0;
            if packet[5] & ADAPTATION_FIELD_FLAG_PCR != 0 {
                if adaptation_length < 7 {
                    return Err(HlsFiniteTsLayoutError::InvalidTransportPacket);
                }
                pcr_field = Some(HlsTsPcrFieldLocation {
                    pid,
                    byte_offset: packet_start
                        .checked_add(6)
                        .ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?,
                });
            }
        }
    }
    Ok(HlsTsPacketEvidence {
        pid,
        payload_unit_start: packet[1] & 0x40 != 0,
        payload_offset,
        continuity_counter,
        discontinuity,
        pcr_field,
    })
}

fn pes_stream_id_has_optional_header(stream_id: u8) -> bool {
    !matches!(
        stream_id,
        0xBC | // program_stream_map
        0xBE | // padding_stream
        0xBF | // private_stream_2
        0xF0 | // ECM
        0xF1 | // EMM
        0xFF | // program_stream_directory
        0xF2 | // DSM-CC
        0xF8 // ITU-T Rec. H.222.1 type E
    )
}

fn timestamp_field_from_pending(
    pending: &HlsPendingPesHeader,
    kind: HlsTsTimestampFieldKind,
    start: usize,
    prefix: u8,
) -> Result<HlsCompletedTimestampField, HlsFiniteTsLayoutError> {
    let end = start
        .checked_add(5)
        .ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    let bytes: [u8; 5] = pending
        .bytes
        .get(start..end)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    let byte_offsets: [usize; 5] = pending
        .byte_offsets
        .get(start..end)
        .and_then(|offsets| offsets.try_into().ok())
        .ok_or(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind })?;
    if bytes[0] >> 4 != prefix || bytes[0] & 1 == 0 || bytes[2] & 1 == 0 || bytes[4] & 1 == 0 {
        return Err(HlsFiniteTsLayoutError::InvalidPesTimestampField { pid: pending.pid, kind });
    }
    Ok(HlsCompletedTimestampField {
        location: HlsTsTimestampFieldLocation { pid: pending.pid, kind, byte_offsets },
        bytes,
    })
}

fn complete_pes_timestamps(
    pending: &HlsPendingPesHeader,
) -> Result<HlsCompletedPesTimestamps, HlsFiniteTsLayoutError> {
    let pts_dts_flags = (pending.bytes[7] >> 6) & 0b11;
    match pts_dts_flags {
        0b00 => Ok(HlsCompletedPesTimestamps::default()),
        0b10 => Ok(HlsCompletedPesTimestamps {
            fields: [
                Some(timestamp_field_from_pending(
                    pending,
                    HlsTsTimestampFieldKind::Pts,
                    PES_PTS_OFFSET,
                    0x02,
                )?),
                None,
            ],
        }),
        0b11 => Ok(HlsCompletedPesTimestamps {
            fields: [
                Some(timestamp_field_from_pending(
                    pending,
                    HlsTsTimestampFieldKind::Pts,
                    PES_PTS_OFFSET,
                    0x03,
                )?),
                Some(timestamp_field_from_pending(
                    pending,
                    HlsTsTimestampFieldKind::Dts,
                    PES_DTS_OFFSET,
                    0x01,
                )?),
            ],
        }),
        _ => Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid }),
    }
}

fn append_pending_pes_header(
    pending: &mut HlsPendingPesHeader,
    payload: &[u8],
    payload_start: usize,
) -> Result<Option<HlsCompletedPesTimestamps>, HlsFiniteTsLayoutError> {
    for (payload_index, byte) in payload.iter().copied().enumerate() {
        if pending.expected_len.is_some_and(|expected| pending.len >= expected) {
            break;
        }
        if pending.len >= HLS_TS_TIMESTAMP_HEADER_BYTES {
            return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid });
        }
        pending.bytes[pending.len] = byte;
        pending.byte_offsets[pending.len] = payload_start
            .checked_add(payload_index)
            .ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        pending.len = pending.len.saturating_add(1);

        if pending.len == 4 && !pes_stream_id_has_optional_header(pending.bytes[3]) {
            pending.expected_len = Some(4);
        }
        if pending.len == 7 && (pending.bytes[6] & 0xC0) != 0x80 {
            return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid });
        }
        if pending.len == PES_PTS_OFFSET {
            let header_data_length = usize::from(pending.bytes[8]);
            pending.expected_len = match (pending.bytes[7] >> 6) & 0b11 {
                0b00 => Some(PES_PTS_OFFSET),
                0b10 if header_data_length >= 5 => Some(PES_PTS_OFFSET + 5),
                0b11 if header_data_length >= 10 => Some(PES_DTS_OFFSET + 5),
                _ => return Err(HlsFiniteTsLayoutError::UnsupportedPesTimestampHeader { pid: pending.pid }),
            };
        }
    }

    if pending.expected_len.is_some_and(|expected| pending.len >= expected) {
        return complete_pes_timestamps(pending).map(Some);
    }
    Ok(None)
}

#[derive(Debug)]
struct HlsPesHeaderAssembler {
    pending: HashMap<u16, HlsPendingPesHeader>,
}

impl HlsPesHeaderAssembler {
    fn new() -> Self {
        Self { pending: HashMap::with_capacity(8) }
    }

    fn push_packet(
        &mut self,
        packet: &[u8],
        packet_start: usize,
        evidence: HlsTsPacketEvidence,
    ) -> Result<HlsCompletedPesTimestamps, HlsFiniteTsLayoutError> {
        if evidence.payload_unit_start && self.pending.contains_key(&evidence.pid) {
            return Err(HlsFiniteTsLayoutError::PesTimestampHeaderInterrupted { pid: evidence.pid });
        }
        if evidence.discontinuity && self.pending.contains_key(&evidence.pid) {
            return Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity {
                pid: evidence.pid,
                expected: None,
                actual: evidence.continuity_counter,
            });
        }
        let Some(payload_offset) = evidence.payload_offset else {
            return Ok(HlsCompletedPesTimestamps::default());
        };
        let payload = packet
            .get(payload_offset..)
            .ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        let payload_start = packet_start
            .checked_add(payload_offset)
            .ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;

        if let Some(mut pending) = self.pending.remove(&evidence.pid) {
            let expected = pending.last_payload_continuity_counter.wrapping_add(1) & 0x0F;
            if evidence.continuity_counter != expected {
                return Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity {
                    pid: evidence.pid,
                    expected: Some(expected),
                    actual: evidence.continuity_counter,
                });
            }
            pending.last_payload_continuity_counter = evidence.continuity_counter;
            if let Some(completed) = append_pending_pes_header(&mut pending, payload, payload_start)? {
                return Ok(completed);
            }
            self.pending.insert(evidence.pid, pending);
            return Ok(HlsCompletedPesTimestamps::default());
        }

        if !evidence.payload_unit_start || payload.len() < 3 || !payload.starts_with(&[0x00, 0x00, 0x01]) {
            return Ok(HlsCompletedPesTimestamps::default());
        }
        if self.pending.len() >= HLS_TS_MAX_PENDING_PES_HEADERS {
            return Err(HlsFiniteTsLayoutError::TooManyPendingPesTimestampHeaders);
        }
        let mut pending = HlsPendingPesHeader::new(evidence.pid, evidence.continuity_counter);
        if let Some(completed) = append_pending_pes_header(&mut pending, payload, payload_start)? {
            return Ok(completed);
        }
        self.pending.insert(evidence.pid, pending);
        Ok(HlsCompletedPesTimestamps::default())
    }

    fn finish(self) -> Result<(), HlsFiniteTsLayoutError> {
        match self.pending.keys().copied().min() {
            Some(pid) => Err(HlsFiniteTsLayoutError::IncompletePesTimestampHeader { pid }),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsTimestampProfile {
    pub first_clock_90khz: u64,
    pub last_clock_90khz: u64,
    pub span_ticks_90khz: u64,
    pub observed_pts_or_dts: bool,
    pub observed_pcr: bool,
}

/// MPEG-TS live-to-terminal timestamp anchor expressed in 90 kHz clock ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsSpliceAnchor {
    pub live_last_clock: u64,
    pub terminal_first_clock: u64,
    pub timestamp_delta_ticks: u64,
}

impl HlsTsSpliceAnchor {
    pub(crate) fn between(
        live_tail: HlsTsTimestampProfile,
        terminal_asset: HlsTsTimestampProfile,
    ) -> Option<Self> {
        if !timestamp_profile_is_coherent(live_tail) || !timestamp_profile_is_coherent(terminal_asset)
        {
            return None;
        }
        let terminal_first_clock_90khz =
            add_pts_dts_offset(live_tail.last_clock_90khz, HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ);
        let timestamp_delta_ticks_90khz = terminal_first_clock_90khz
            .wrapping_add(MAX_PTS_DTS)
            .wrapping_sub(terminal_asset.first_clock_90khz)
            % MAX_PTS_DTS;
        let rebased_first =
            add_pts_dts_offset(terminal_asset.first_clock_90khz, timestamp_delta_ticks_90khz);
        (rebased_first == terminal_first_clock_90khz).then_some(Self {
            live_last_clock: live_tail.last_clock_90khz,
            terminal_first_clock: terminal_first_clock_90khz,
            timestamp_delta_ticks: timestamp_delta_ticks_90khz,
        })
    }
}

fn timestamp_profile_is_coherent(profile: HlsTsTimestampProfile) -> bool {
    (profile.observed_pts_or_dts || profile.observed_pcr)
        && profile.first_clock_90khz < MAX_PTS_DTS
        && profile.last_clock_90khz < MAX_PTS_DTS
        && profile.span_ticks_90khz < MAX_PTS_DTS
        && forward_clock_distance_90khz(profile.first_clock_90khz, profile.last_clock_90khz)
            == profile.span_ticks_90khz
}

#[derive(Debug)]
struct HlsTsTimestampProfileAccumulator {
    reference_clock_90khz: Option<u64>,
    earliest_relative_ticks: i64,
    latest_relative_ticks: i64,
    maximum_span_ticks_90khz: u64,
    observed_pts_or_dts: bool,
    observed_pcr: bool,
    observation_count: u64,
    invalid: bool,
}

impl HlsTsTimestampProfileAccumulator {
    fn new(expected_duration_ticks_90khz: u64) -> Self {
        let tolerance = (expected_duration_ticks_90khz / 2).max(HLS_TS_PROFILE_MIN_TOLERANCE_TICKS_90KHZ);
        Self {
            reference_clock_90khz: None,
            earliest_relative_ticks: 0,
            latest_relative_ticks: 0,
            maximum_span_ticks_90khz: expected_duration_ticks_90khz.saturating_add(tolerance),
            observed_pts_or_dts: false,
            observed_pcr: false,
            observation_count: 0,
            invalid: expected_duration_ticks_90khz == 0,
        }
    }

    fn observe_clock(&mut self, clock_90khz: u64, kind: HlsTsTimestampKind) {
        let clock_90khz = clock_90khz % MAX_PTS_DTS;
        match kind {
            HlsTsTimestampKind::PtsOrDts => self.observed_pts_or_dts = true,
            HlsTsTimestampKind::Pcr => self.observed_pcr = true,
        }
        self.observation_count = self.observation_count.saturating_add(1);
        let Some(reference) = self.reference_clock_90khz else {
            self.reference_clock_90khz = Some(clock_90khz);
            return;
        };
        let forward = clock_90khz.wrapping_add(MAX_PTS_DTS).wrapping_sub(reference) % MAX_PTS_DTS;
        let relative = if forward <= MAX_PTS_DTS / 2 {
            i64::try_from(forward).unwrap_or(i64::MAX)
        } else {
            -i64::try_from(MAX_PTS_DTS.saturating_sub(forward)).unwrap_or(i64::MAX)
        };
        self.earliest_relative_ticks = self.earliest_relative_ticks.min(relative);
        self.latest_relative_ticks = self.latest_relative_ticks.max(relative);
        let span = self.latest_relative_ticks.saturating_sub(self.earliest_relative_ticks);
        if u64::try_from(span).unwrap_or(u64::MAX) > self.maximum_span_ticks_90khz {
            self.invalid = true;
        }
    }

    fn finish(self) -> Option<HlsTsTimestampProfile> {
        let reference = self.reference_clock_90khz?;
        if self.invalid || self.observation_count < 2 || (!self.observed_pts_or_dts && !self.observed_pcr) {
            return None;
        }
        let span_ticks_90khz =
            u64::try_from(self.latest_relative_ticks.saturating_sub(self.earliest_relative_ticks)).ok()?;
        if span_ticks_90khz == 0 {
            return None;
        }
        let cycle = i128::from(MAX_PTS_DTS);
        let first_clock_90khz =
            u64::try_from((i128::from(reference) + i128::from(self.earliest_relative_ticks)).rem_euclid(cycle))
                .ok()?;
        let last_clock_90khz =
            u64::try_from((i128::from(reference) + i128::from(self.latest_relative_ticks)).rem_euclid(cycle)).ok()?;
        Some(HlsTsTimestampProfile {
            first_clock_90khz,
            last_clock_90khz,
            span_ticks_90khz,
            observed_pts_or_dts: self.observed_pts_or_dts,
            observed_pcr: self.observed_pcr,
        })
    }
}

#[derive(Debug)]
pub(crate) struct HlsTsTimestampProfileScanner {
    assembler: HlsPesHeaderAssembler,
    accumulator: HlsTsTimestampProfileAccumulator,
    next_packet_start: usize,
    invalid: bool,
}

impl HlsTsTimestampProfileScanner {
    pub(crate) fn new(expected_duration_ticks_90khz: u64) -> Self {
        Self {
            assembler: HlsPesHeaderAssembler::new(),
            accumulator: HlsTsTimestampProfileAccumulator::new(expected_duration_ticks_90khz),
            next_packet_start: 0,
            invalid: false,
        }
    }

    pub(crate) fn push_aligned_packet(&mut self, packet: &[u8]) {
        if self.invalid {
            return;
        }
        let packet_start = self.next_packet_start;
        let Some(next_packet_start) = packet_start.checked_add(TS_PACKET_SIZE) else {
            self.invalid = true;
            return;
        };
        self.next_packet_start = next_packet_start;
        let Ok(evidence) = inspect_hls_ts_packet(packet, packet_start) else {
            self.invalid = true;
            return;
        };
        if let Some(pcr) = evidence.pcr_field {
            let Some(relative_offset) = pcr.byte_offset.checked_sub(packet_start) else {
                self.invalid = true;
                return;
            };
            let Some(bytes) = packet.get(relative_offset..relative_offset.saturating_add(6)) else {
                self.invalid = true;
                return;
            };
            self.accumulator
                .observe_clock(decode_pcr(bytes) / 300, HlsTsTimestampKind::Pcr);
        }
        let Ok(completed) = self.assembler.push_packet(packet, packet_start, evidence) else {
            self.invalid = true;
            return;
        };
        for field in completed.fields.into_iter().flatten() {
            self.accumulator
                .observe_clock(decode_timestamp(&field.bytes), HlsTsTimestampKind::PtsOrDts);
        }
    }

    pub(crate) fn finish(self) -> Option<HlsTsTimestampProfile> {
        if self.invalid || self.assembler.finish().is_err() {
            return None;
        }
        self.accumulator.finish()
    }
}

#[derive(Clone, Copy)]
enum HlsTsTimestampKind {
    PtsOrDts,
    Pcr,
}

#[inline]
fn ts_packet_pid(packet: &[u8]) -> u16 {
    (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2])
}

fn same_finite_ts_packet_layout(source: &[u8], prepared: &[u8]) -> bool {
    if source.len() != TS_PACKET_SIZE
        || prepared.len() != TS_PACKET_SIZE
        || source[0] != SYNC_BYTE
        || prepared[0] != SYNC_BYTE
        || source[1] & 0x7F != prepared[1] & 0x7F
        || source[2] != prepared[2]
        || source[3] & 0xF0 != prepared[3] & 0xF0
    {
        return false;
    }
    let adaptation_field_control = (source[3] >> 4) & 0b11;
    !matches!(adaptation_field_control, 0b10 | 0b11) || source[4] == prepared[4]
}

fn append_finite_discontinuity_packet(following_packet: &[u8], output: &mut BytesMut) {
    let following_cc = following_packet[3] & 0x0F;
    let marker_cc = if following_packet[3] & 0x10 != 0 {
        following_cc.wrapping_sub(1) & 0x0F
    } else {
        following_cc
    };
    let start = output.len();
    output.resize(start.saturating_add(TS_PACKET_SIZE), 0xFF);
    let marker = &mut output[start..start + TS_PACKET_SIZE];
    marker[0] = SYNC_BYTE;
    marker[1] = following_packet[1] & 0x1F;
    marker[2] = following_packet[2];
    // FFmpeg retains this adaptation-only marker as the per-PID CC baseline.
    marker[3] = 0x20 | marker_cc;
    marker[4] = 183;
    marker[5] = 0x80;
}

/// Encode PCR timestamp (u64) back into 6 bytes
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn encode_pcr(pcr: u64) -> [u8; 6] {
    let pcr_base = pcr / 300;
    let pcr_ext = pcr % 300;

    [
        ((pcr_base >> 25) & 0xFF) as u8,
        ((pcr_base >> 17) & 0xFF) as u8,
        ((pcr_base >> 9) & 0xFF) as u8,
        ((pcr_base >> 1) & 0xFF) as u8,
        // Bit 7 = bit0 of pcr_base, Bits 6-1 reserved '111111', Bit 0 = high bit of pcr_ext
        (((pcr_base & 1) << 7) as u8) | 0x7E | (((pcr_ext >> 8) & 1) as u8),
        (pcr_ext & 0xFF) as u8,
    ]
}

/// Finds TS alignment by checking for 0x47 sync byte every 188 bytes
fn find_ts_alignment(buf: &[u8]) -> Option<usize> {
    for offset in 0..TS_PACKET_SIZE {
        let mut valid = true;
        for i in 0..5 {
            if buf.get(offset + i * TS_PACKET_SIZE) != Some(&SYNC_BYTE) {
                valid = false;
                break;
            }
        }
        if valid {
            return Some(offset);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HlsFiniteTsPacketLayout {
    packet_start: usize,
    pid: u16,
    has_payload: bool,
    timestamp_field_indices_start: usize,
    timestamp_field_indices_end: usize,
    pcr_field_index: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsFiniteTsLayout {
    packets: Arc<[HlsFiniteTsPacketLayout]>,
    pub timestamp_fields: Arc<[HlsTsTimestampFieldLocation]>,
    pub pcr_fields: Arc<[HlsTsPcrFieldLocation]>,
    packet_timestamp_field_indices: Arc<[usize]>,
}

fn build_hls_finite_ts_layout(buffer: &[u8]) -> Result<HlsFiniteTsLayout, HlsFiniteTsLayoutError> {
    if buffer.is_empty() || !buffer.len().is_multiple_of(TS_PACKET_SIZE) {
        return Err(HlsFiniteTsLayoutError::InvalidAsset);
    }
    let packet_count = buffer.len() / TS_PACKET_SIZE;
    let mut packet_evidence = Vec::with_capacity(packet_count);
    let mut pcr_fields = Vec::new();
    let mut packet_pcr_indices = Vec::with_capacity(packet_count);
    let mut timestamp_fields = Vec::new();
    let mut assembler = HlsPesHeaderAssembler::new();

    for (packet_index, packet) in buffer.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
        let packet_start = packet_index
            .checked_mul(TS_PACKET_SIZE)
            .ok_or(HlsFiniteTsLayoutError::InvalidTransportPacket)?;
        let evidence = inspect_hls_ts_packet(packet, packet_start)?;
        let pcr_field_index = evidence.pcr_field.map(|field| {
            let index = pcr_fields.len();
            pcr_fields.push(field);
            index
        });
        let completed = assembler.push_packet(packet, packet_start, evidence)?;
        timestamp_fields.extend(
            completed
                .fields
                .into_iter()
                .flatten()
                .map(|field| field.location),
        );
        packet_evidence.push(evidence);
        packet_pcr_indices.push(pcr_field_index);
    }
    assembler.finish()?;
    timestamp_fields.sort_unstable_by_key(|field| field.byte_offsets[0]);

    let mut timestamp_field_counts = vec![0usize; packet_count];
    for field in &timestamp_fields {
        let mut previous_packet = None;
        for offset in field.byte_offsets {
            if offset >= buffer.len() {
                return Err(HlsFiniteTsLayoutError::InvalidTimestampLocation);
            }
            let packet_index = offset / TS_PACKET_SIZE;
            if previous_packet != Some(packet_index) {
                timestamp_field_counts[packet_index] = timestamp_field_counts[packet_index]
                    .checked_add(1)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                previous_packet = Some(packet_index);
            }
        }
    }
    let mut timestamp_field_starts = Vec::with_capacity(packet_count.saturating_add(1));
    timestamp_field_starts.push(0usize);
    for count in &timestamp_field_counts {
        let next = timestamp_field_starts
            .last()
            .copied()
            .and_then(|start| start.checked_add(*count))
            .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
        timestamp_field_starts.push(next);
    }
    let timestamp_membership_count = timestamp_field_starts.last().copied().unwrap_or(0);
    let mut packet_timestamp_field_indices = vec![0usize; timestamp_membership_count];
    let mut packet_cursors = timestamp_field_starts[..packet_count].to_vec();
    for (field_index, field) in timestamp_fields.iter().enumerate() {
        let mut previous_packet = None;
        for offset in field.byte_offsets {
            let packet_index = offset / TS_PACKET_SIZE;
            if previous_packet == Some(packet_index) {
                continue;
            }
            let cursor = packet_cursors[packet_index];
            packet_timestamp_field_indices[cursor] = field_index;
            packet_cursors[packet_index] = cursor.saturating_add(1);
            previous_packet = Some(packet_index);
        }
    }
    let packets = packet_evidence
        .into_iter()
        .enumerate()
        .map(|(packet_index, evidence)| HlsFiniteTsPacketLayout {
            packet_start: packet_index.saturating_mul(TS_PACKET_SIZE),
            pid: evidence.pid,
            has_payload: evidence.has_payload(),
            timestamp_field_indices_start: timestamp_field_starts[packet_index],
            timestamp_field_indices_end: timestamp_field_starts[packet_index.saturating_add(1)],
            pcr_field_index: packet_pcr_indices[packet_index],
        })
        .collect::<Vec<_>>();
    Ok(HlsFiniteTsLayout {
        packets: Arc::from(packets),
        timestamp_fields: Arc::from(timestamp_fields),
        pcr_fields: Arc::from(pcr_fields),
        packet_timestamp_field_indices: Arc::from(packet_timestamp_field_indices),
    })
}

fn gather_timestamp_bytes(
    buffer: &[u8],
    location: HlsTsTimestampFieldLocation,
) -> Result<[u8; 5], HlsFiniteTsLayoutError> {
    let mut bytes = [0u8; 5];
    for (destination, offset) in bytes.iter_mut().zip(location.byte_offsets) {
        *destination = buffer
            .get(offset)
            .copied()
            .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    }
    Ok(bytes)
}

fn decode_timestamp_at_location(
    buffer: &[u8],
    location: HlsTsTimestampFieldLocation,
) -> Result<u64, HlsFiniteTsLayoutError> {
    gather_timestamp_bytes(buffer, location).map(|bytes| decode_timestamp(&bytes))
}

fn decode_pcr_at_location(
    buffer: &[u8],
    location: HlsTsPcrFieldLocation,
) -> Result<u64, HlsFiniteTsLayoutError> {
    let end = location
        .byte_offset
        .checked_add(6)
        .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    let bytes = buffer
        .get(location.byte_offset..end)
        .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    Ok(decode_pcr(bytes))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsTsPresentationClockSource {
    Pts,
    PcrFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsPidPresentationTimeline {
    pub pid: u16,
    pub first_pts_90khz: u64,
    pub last_pts_90khz: u64,
    pub cadence_ticks_90khz: u64,
    pub end_exclusive_90khz: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsTsPresentationDuration {
    pub first_presentation_clock_90khz: u64,
    pub end_exclusive_clock_90khz: u64,
    pub duration_ticks_90khz: u64,
    pub timelines: Arc<[HlsTsPidPresentationTimeline]>,
    pub source: HlsTsPresentationClockSource,
}

fn stable_presentation_cadence(mut deltas: Vec<u64>) -> Option<u64> {
    if deltas.is_empty() {
        return None;
    }
    deltas.sort_unstable();
    let mut dominant_delta = deltas[0];
    let mut dominant_count = 1usize;
    let mut current_delta = deltas[0];
    let mut current_count = 1usize;
    for delta in deltas.iter().copied().skip(1) {
        if delta == current_delta {
            current_count = current_count.saturating_add(1);
        } else {
            if current_count > dominant_count {
                dominant_delta = current_delta;
                dominant_count = current_count;
            }
            current_delta = delta;
            current_count = 1;
        }
    }
    if current_count > dominant_count {
        dominant_delta = current_delta;
        dominant_count = current_count;
    }
    if dominant_count.saturating_mul(2) > deltas.len() {
        Some(dominant_delta)
    } else {
        deltas.get(deltas.len() / 2).copied()
    }
}

fn presentation_timeline_for_pid(
    pid: u16,
    timestamps: &[u64],
) -> Result<HlsTsPidPresentationTimeline, HlsFiniteTsLayoutError> {
    let Some(first_raw) = timestamps.first().copied() else {
        return Err(HlsFiniteTsLayoutError::PresentationCadenceUnavailable { pid });
    };
    let cycle = i128::from(MAX_PTS_DTS);
    let mut previous_raw = first_raw % MAX_PTS_DTS;
    let mut previous_unwrapped = i128::from(previous_raw);
    let mut first_unwrapped = previous_unwrapped;
    let mut last_unwrapped = previous_unwrapped;
    let mut seen_raw_timestamps = HashSet::with_capacity(timestamps.len());
    seen_raw_timestamps.insert(previous_raw);
    let mut deltas = Vec::with_capacity(timestamps.len().saturating_sub(1));

    for raw in timestamps.iter().copied().skip(1).map(|value| value % MAX_PTS_DTS) {
        if !seen_raw_timestamps.insert(raw) {
            continue;
        }
        let forward = raw.wrapping_add(MAX_PTS_DTS).wrapping_sub(previous_raw) % MAX_PTS_DTS;
        let signed_delta = if forward <= MAX_PTS_DTS / 2 {
            i128::from(forward)
        } else {
            -i128::from(MAX_PTS_DTS.saturating_sub(forward))
        };
        let unwrapped = previous_unwrapped
            .checked_add(signed_delta)
            .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
        if unwrapped > previous_unwrapped {
            deltas.push(
                u64::try_from(unwrapped - previous_unwrapped)
                    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
            );
        }
        first_unwrapped = first_unwrapped.min(unwrapped);
        last_unwrapped = last_unwrapped.max(unwrapped);
        previous_raw = raw;
        previous_unwrapped = unwrapped;
    }
    let cadence_ticks_90khz = stable_presentation_cadence(deltas)
        .filter(|cadence| *cadence > 0 && *cadence < MAX_PTS_DTS / 2)
        .ok_or(HlsFiniteTsLayoutError::PresentationCadenceUnavailable { pid })?;
    let normalization = if first_unwrapped < 0 {
        (-first_unwrapped)
            .checked_add(cycle.saturating_sub(1))
            .and_then(|value| value.checked_div(cycle))
            .and_then(|cycles| cycles.checked_mul(cycle))
            .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?
    } else {
        0
    };
    let first_pts_90khz = u64::try_from(
        first_unwrapped
            .checked_add(normalization)
            .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
    )
    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    let last_pts_90khz = u64::try_from(
        last_unwrapped
            .checked_add(normalization)
            .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?,
    )
    .map_err(|_| HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    let end_exclusive_90khz = last_pts_90khz
        .checked_add(cadence_ticks_90khz)
        .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    Ok(HlsTsPidPresentationTimeline {
        pid,
        first_pts_90khz,
        last_pts_90khz,
        cadence_ticks_90khz,
        end_exclusive_90khz,
    })
}

fn presentation_duration_from_pid_clocks(
    clocks: HashMap<u16, Vec<u64>>,
    source: HlsTsPresentationClockSource,
) -> Result<HlsTsPresentationDuration, HlsFiniteTsLayoutError> {
    if clocks.is_empty() {
        return Err(HlsFiniteTsLayoutError::PresentationClockUnavailable);
    }
    let mut timelines = clocks
        .into_iter()
        .map(|(pid, timestamps)| presentation_timeline_for_pid(pid, &timestamps))
        .collect::<Result<Vec<_>, _>>()?;
    timelines.sort_unstable_by_key(|timeline| timeline.pid);
    let first_presentation_clock_90khz = timelines
        .iter()
        .map(|timeline| timeline.first_pts_90khz)
        .min()
        .ok_or(HlsFiniteTsLayoutError::PresentationClockUnavailable)?;
    let end_exclusive_clock_90khz = timelines
        .iter()
        .map(|timeline| timeline.end_exclusive_90khz)
        .max()
        .ok_or(HlsFiniteTsLayoutError::PresentationClockUnavailable)?;
    let duration_ticks_90khz = end_exclusive_clock_90khz
        .checked_sub(first_presentation_clock_90khz)
        .filter(|duration| *duration > 0 && *duration < MAX_PTS_DTS)
        .ok_or(HlsFiniteTsLayoutError::PresentationDurationOverflow)?;
    Ok(HlsTsPresentationDuration {
        first_presentation_clock_90khz,
        end_exclusive_clock_90khz,
        duration_ticks_90khz,
        timelines: Arc::from(timelines),
        source,
    })
}

fn finite_hls_presentation_duration(
    buffer: &[u8],
    layout: &HlsFiniteTsLayout,
) -> Result<HlsTsPresentationDuration, HlsFiniteTsLayoutError> {
    let mut pts_by_pid = HashMap::<u16, Vec<u64>>::new();
    for location in layout
        .timestamp_fields
        .iter()
        .copied()
        .filter(|location| location.kind == HlsTsTimestampFieldKind::Pts)
    {
        pts_by_pid
            .entry(location.pid)
            .or_default()
            .push(decode_timestamp_at_location(buffer, location)?);
    }
    if !pts_by_pid.is_empty() {
        return presentation_duration_from_pid_clocks(pts_by_pid, HlsTsPresentationClockSource::Pts);
    }

    let mut pcr_by_pid = HashMap::<u16, Vec<u64>>::new();
    for location in layout.pcr_fields.iter().copied() {
        pcr_by_pid
            .entry(location.pid)
            .or_default()
            .push(decode_pcr_at_location(buffer, location)? / 300);
    }
    presentation_duration_from_pid_clocks(pcr_by_pid, HlsTsPresentationClockSource::PcrFallback)
}

fn timestamp_profile_from_finite_layout(
    buffer: &[u8],
    layout: &HlsFiniteTsLayout,
    expected_duration_ticks_90khz: u64,
) -> Option<HlsTsTimestampProfile> {
    let mut accumulator = HlsTsTimestampProfileAccumulator::new(expected_duration_ticks_90khz);
    for location in layout.timestamp_fields.iter().copied() {
        accumulator.observe_clock(
            decode_timestamp_at_location(buffer, location).ok()?,
            HlsTsTimestampKind::PtsOrDts,
        );
    }
    for location in layout.pcr_fields.iter().copied() {
        accumulator.observe_clock(
            decode_pcr_at_location(buffer, location).ok()? / 300,
            HlsTsTimestampKind::Pcr,
        );
    }
    accumulator.finish()
}

fn ticks_90khz_to_rounded_millis(ticks: u64) -> Option<u64> {
    ticks.checked_add(45)?.checked_div(90)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsFiniteTsRenderSpec {
    pub timestamp_offset_ticks_90khz: u64,
    /// Seed used by logical segment zero. Later segments derive each PID's starting counter from
    /// the number of payload packets for that PID in one immutable asset cycle.
    pub continuity_seed: u8,
    pub logical_segment_index: u16,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsFiniteTsRenderError {
    #[error("transport stream asset is empty or invalid")]
    InvalidAsset,
    #[error("prepared transport stream does not match the immutable asset packet layout")]
    PreparedLayoutMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsFiniteTsDiscontinuityMode {
    None,
    FirstPacketPerPid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsFiniteTsFinalizeSpec {
    pub additional_timestamp_offset_ticks_90khz: u64,
    pub discontinuity: HlsFiniteTsDiscontinuityMode,
}

pub struct TransportStreamBuffer {
    // `Bytes` instead of `Arc<Vec<u8>>`: cloning the inner payload (which
    // happens once per HLS-CVS fallback response) is then a refcount bump
    // instead of a deep copy. All existing `&self.buffer[..]` / `.len()` /
    // `.is_empty()` call sites continue to work via `Bytes`'s `Deref<Target=[u8]>`.
    buffer: Bytes,
    packet_starts: Arc<[usize]>,
    finite_hls_layout: Result<Arc<HlsFiniteTsLayout>, HlsFiniteTsLayoutError>,
    finite_hls_presentation_duration: Option<HlsTsPresentationDuration>,
    current_pos: usize,
    current_dts: u64,
    timestamp_offset: u64,
    length: usize,
    /// Per-PID continuity counter and discontinuity-sent flag.
    /// Indexed directly by PID (0–8191) for O(1) lookup.
    cc_entries: Box<[Option<(u8, bool)>; 8192]>,
    waker: Arc<AtomicWaker>,
    first_pcr: Option<u64>,
    finite_hls_timestamp_profile: Option<HlsTsTimestampProfile>,
    finite_hls_track_signature: Option<crate::api::model::HlsTsTrackSignature>,
    finite_hls_asset_fingerprint: [u8; 32],
    #[cfg(test)]
    finite_hls_render_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    finite_hls_finalize_count: Arc<std::sync::atomic::AtomicUsize>,
    force_discontinuity_on_wrap: bool,
}

impl std::fmt::Debug for TransportStreamBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportStreamBuffer")
            .field("length", &self.length)
            .field("current_pos", &self.current_pos)
            .field("current_dts", &self.current_dts)
            .field("timestamp_offset", &self.timestamp_offset)
            .field("finite_hls_presentation_duration", &self.finite_hls_presentation_duration)
            .field("first_pcr", &self.first_pcr)
            .field("finite_hls_timestamp_profile", &self.finite_hls_timestamp_profile)
            .field("force_discontinuity_on_wrap", &self.force_discontinuity_on_wrap)
            .finish_non_exhaustive()
    }
}

impl Clone for TransportStreamBuffer {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            packet_starts: Arc::clone(&self.packet_starts),
            finite_hls_layout: self.finite_hls_layout.clone(),
            finite_hls_presentation_duration: self.finite_hls_presentation_duration.clone(),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length: self.length,
            // Each clone starts with a fresh CC state; the discontinuity packets at the first
            // loop boundary will signal decoders to reset their CC expectations.
            cc_entries: Box::new([None; 8192]),
            waker: Arc::clone(&self.waker),
            first_pcr: self.first_pcr,
            finite_hls_timestamp_profile: self.finite_hls_timestamp_profile,
            finite_hls_track_signature: self.finite_hls_track_signature.clone(),
            finite_hls_asset_fingerprint: self.finite_hls_asset_fingerprint,
            #[cfg(test)]
            finite_hls_render_count: Arc::clone(&self.finite_hls_render_count),
            #[cfg(test)]
            finite_hls_finalize_count: Arc::clone(&self.finite_hls_finalize_count),
            // Start each clone with one initial discontinuity marker to keep
            // continuity behavior consistent for fresh consumers.
            force_discontinuity_on_wrap: true,
        }
    }
}

impl TransportStreamBuffer {
    pub fn new(mut raw: Vec<u8>) -> Self {
        let offset = find_ts_alignment(&raw).unwrap_or(0);
        raw.drain(..offset);

        // Remove trailing partial packets
        let valid_length = (raw.len() / TS_PACKET_SIZE) * TS_PACKET_SIZE;
        raw.truncate(valid_length);

        let length = raw.len() / TS_PACKET_SIZE;
        let packet_starts = (0..length)
            .map(|packet_index| packet_index.saturating_mul(TS_PACKET_SIZE))
            .collect::<Arc<[_]>>();
        let finite_hls_layout = build_hls_finite_ts_layout(&raw).map(Arc::new);
        let finite_hls_presentation_duration = finite_hls_layout
            .as_ref()
            .ok()
            .and_then(|layout| finite_hls_presentation_duration(&raw, layout).ok());
        let finite_hls_track_signature = match crate::api::model::inspect_mpeg_ts(
            std::io::Cursor::new(&raw),
            crate::api::model::HlsTsProbeProtection::Clear,
            crate::api::model::HlsTsProbeBudget::default(),
        ) {
            Ok(crate::api::model::HlsTsProbeOutcome::Found(signature)) => Some(signature),
            Ok(
                crate::api::model::HlsTsProbeOutcome::ProbeBudgetExhausted { .. }
                | crate::api::model::HlsTsProbeOutcome::Malformed(_)
                | crate::api::model::HlsTsProbeOutcome::UnsupportedProtection(_),
            )
            | Err(_) => None,
        };
        let finite_hls_asset_fingerprint = Sha256::digest(&raw).into();
        let finite_hls_timestamp_profile = finite_hls_layout.as_ref().ok().and_then(|layout| {
            finite_hls_presentation_duration.as_ref().and_then(|duration| {
                timestamp_profile_from_finite_layout(&raw, layout, duration.duration_ticks_90khz)
            })
        });
        let first_pcr = finite_hls_layout
            .as_ref()
            .ok()
            .and_then(|layout| layout.pcr_fields.first().copied())
            .and_then(|location| decode_pcr_at_location(&raw, location).ok());

        Self {
            buffer: Bytes::from(raw),
            current_pos: 0,
            current_dts: 0,
            timestamp_offset: 0,
            length,
            packet_starts,
            finite_hls_layout,
            finite_hls_presentation_duration,
            cc_entries: Box::new([None; 8192]),
            waker: Arc::new(AtomicWaker::new()),
            first_pcr,
            finite_hls_timestamp_profile,
            finite_hls_track_signature,
            finite_hls_asset_fingerprint,
            #[cfg(test)]
            finite_hls_render_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            finite_hls_finalize_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            // Emit a discontinuity packet on startup. Subsequent injections are
            // governed by wrap handling and duration/discontinuity logic.
            force_discontinuity_on_wrap: true,
        }
    }

    /// Fallible constructor: returns an error if the raw bytes contain no valid MPEG-TS data.
    pub fn try_new(raw: Vec<u8>) -> Result<Self, crate::api::model::StreamError> {
        let buf = Self::new(raw);
        if buf.length == 0 {
            Err(crate::api::model::StreamError::MalformedPacket(
                "TS buffer does not contain decodable packet indices".to_string(),
            ))
        } else {
            Ok(buf)
        }
    }

    pub fn as_bytes(&self) -> &[u8] { &self.buffer }

    /// Cheap clone of the underlying buffer as `Bytes` (refcount bump).
    /// Use this from response builders to avoid `Bytes::copy_from_slice(&[u8])`.
    pub fn clone_bytes(&self) -> Bytes { self.buffer.clone() }

    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ticks_90khz().and_then(ticks_90khz_to_rounded_millis)
    }

    pub(crate) fn duration_ticks_90khz(&self) -> Option<u64> {
        self.finite_hls_presentation_duration
            .as_ref()
            .map(|duration| duration.duration_ticks_90khz)
    }

    pub(crate) const fn finite_hls_timestamp_profile(&self) -> Option<HlsTsTimestampProfile> {
        self.finite_hls_timestamp_profile
    }

    pub(crate) fn finite_hls_track_signature(&self) -> Option<crate::api::model::HlsTsTrackSignature> {
        self.finite_hls_track_signature.clone()
    }

    pub(crate) const fn has_finite_hls_track_signature(&self) -> bool {
        self.finite_hls_track_signature.is_some()
    }

    pub(crate) const fn finite_hls_asset_fingerprint(&self) -> [u8; 32] {
        self.finite_hls_asset_fingerprint
    }

    #[cfg(test)]
    pub(crate) fn finite_hls_render_count(&self) -> usize {
        self.finite_hls_render_count.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn finite_hls_finalize_count(&self) -> usize {
        self.finite_hls_finalize_count.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Renders one immutable, finite HLS media segment without mutating the looping stream state.
    pub(crate) fn render_finite_hls_segment(
        &self,
        spec: HlsFiniteTsRenderSpec,
    ) -> Result<Bytes, HlsFiniteTsRenderError> {
        #[cfg(test)]
        self.finite_hls_render_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let layout = self
            .finite_hls_layout
            .as_ref()
            .map_err(|_| HlsFiniteTsRenderError::InvalidAsset)?;
        if self.buffer.is_empty() || layout.packets.is_empty() {
            return Err(HlsFiniteTsRenderError::InvalidAsset);
        }
        let mut output = BytesMut::with_capacity(self.buffer.len());
        let mut payload_counts = [0_u8; 8192];
        for packet_layout in layout.packets.iter().copied() {
            if packet_layout.has_payload {
                let count = &mut payload_counts[usize::from(packet_layout.pid)];
                *count = count.wrapping_add(1) & 0x0F;
            }
        }
        let mut continuity = [None; 8192];
        for packet_layout in layout.packets.iter().copied() {
            let packet_end = packet_layout.packet_start.saturating_add(TS_PACKET_SIZE);
            let Some(packet) = self.buffer.get(packet_layout.packet_start..packet_end) else {
                return Err(HlsFiniteTsRenderError::InvalidAsset);
            };
            let output_start = output.len();
            output.extend_from_slice(packet);
            let counter = continuity[usize::from(packet_layout.pid)].get_or_insert_with(|| {
                let cycle_advance = payload_counts[usize::from(packet_layout.pid)]
                    .wrapping_mul((spec.logical_segment_index & 0x0F) as u8)
                    & 0x0F;
                spec.continuity_seed.wrapping_add(cycle_advance) & 0x0F
            });
            output[output_start + 3] = (output[output_start + 3] & 0xF0) | *counter;
            if packet_layout.has_payload {
                *counter = counter.wrapping_add(1) & 0x0F;
            }
        }
        Self::rewrite_layout_timestamps(&mut output, layout, spec.timestamp_offset_ticks_90khz)
            .map_err(|_| HlsFiniteTsRenderError::InvalidAsset)?;
        Ok(output.freeze())
    }

    /// Applies one lease-specific timestamp anchor to an already prepared relative segment.
    ///
    /// The immutable source packet layout is verified before any bytes are published. Optional
    /// splice markers are adaptation-only packets and therefore do not advance payload CC.
    pub(crate) fn finalize_prepared_finite_hls_segment(
        &self,
        prepared: &Bytes,
        spec: HlsFiniteTsFinalizeSpec,
    ) -> Result<Bytes, HlsFiniteTsRenderError> {
        #[cfg(test)]
        self.finite_hls_finalize_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let layout = self
            .finite_hls_layout
            .as_ref()
            .map_err(|_| HlsFiniteTsRenderError::PreparedLayoutMismatch)?;
        if self.buffer.is_empty()
            || layout.packets.is_empty()
            || prepared.len() != layout.packets.len().saturating_mul(TS_PACKET_SIZE)
        {
            return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
        }
        let mut first_pid_seen = [false; 8192];
        let mut marker_count = 0usize;
        for (position, packet_layout) in layout.packets.iter().enumerate() {
            let prepared_start = position.saturating_mul(TS_PACKET_SIZE);
            let Some(source) = self.buffer.get(
                packet_layout.packet_start..packet_layout.packet_start.saturating_add(TS_PACKET_SIZE),
            ) else {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            };
            let Some(candidate) = prepared.get(prepared_start..prepared_start.saturating_add(TS_PACKET_SIZE)) else {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            };
            if !same_finite_ts_packet_layout(source, candidate) {
                return Err(HlsFiniteTsRenderError::PreparedLayoutMismatch);
            }
            let pid = ts_packet_pid(candidate);
            if spec.discontinuity == HlsFiniteTsDiscontinuityMode::FirstPacketPerPid
                && pid != NULL_PID
                && !first_pid_seen[usize::from(pid)]
            {
                first_pid_seen[usize::from(pid)] = true;
                marker_count = marker_count.saturating_add(1);
            }
        }

        let mut rewritten = BytesMut::from(prepared.as_ref());
        Self::rewrite_layout_timestamps(
            &mut rewritten,
            layout,
            spec.additional_timestamp_offset_ticks_90khz,
        )
        .map_err(|_| HlsFiniteTsRenderError::PreparedLayoutMismatch)?;
        first_pid_seen.fill(false);
        let additional_bytes = marker_count.saturating_mul(TS_PACKET_SIZE);
        let mut output = BytesMut::with_capacity(prepared.len().saturating_add(additional_bytes));
        for position in 0..layout.packets.len() {
            let prepared_start = position.saturating_mul(TS_PACKET_SIZE);
            let packet = &rewritten[prepared_start..prepared_start + TS_PACKET_SIZE];
            let pid = ts_packet_pid(packet);
            if spec.discontinuity == HlsFiniteTsDiscontinuityMode::FirstPacketPerPid
                && pid != NULL_PID
                && !first_pid_seen[usize::from(pid)]
            {
                append_finite_discontinuity_packet(packet, &mut output);
                first_pid_seen[usize::from(pid)] = true;
            }
            output.extend_from_slice(packet);
        }
        Ok(output.freeze())
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn estimated_bitrate_kbps(&self) -> Option<usize> {
        let stream_duration_90khz = self.duration_ticks_90khz()?;
        if self.buffer.is_empty() {
            return None;
        }
        let duration_secs = stream_duration_90khz as f64 / 90_000.0;
        if duration_secs <= 0.0 {
            return None;
        }
        let kbps = ((self.buffer.len() as f64 * 8.0) / duration_secs / 1_000.0).round();
        if !kbps.is_finite() || kbps <= 0.0 {
            return None;
        }
        Some(kbps as usize)
    }

    pub fn register_waker(&self, waker: &Waker) { self.waker.register(waker); }

    /// Sets the timestamp offset used when rewriting PTS/DTS/PCR values.
    pub fn set_timestamp_offset(&mut self, offset: u64) { self.timestamp_offset = offset; }

    /// Generates a Discontinuity packet for the given packet/PID state, writing it directly into `out`.
    fn generate_discontinuity_packet(
        new_packet: &[u8],
        cc: u8,
        first_pcr: Option<u64>,
        timestamp_offset: u64,
        out: &mut BytesMut,
    ) {
        let start = out.len();
        out.resize(start + TS_PACKET_SIZE, 0xFF);
        let pkt = &mut out[start..start + TS_PACKET_SIZE];

        pkt[0] = SYNC_BYTE;
        pkt[1] = new_packet[1] & 0x1F;
        pkt[2] = new_packet[2];

        // Check if the current packet has a PCR (need at least 7 adaptation bytes: 1 flag + 6 PCR).
        let new_pkt_has_pcr = {
            let afc = (new_packet[3] >> 4) & 0b11;
            if afc == 2 || afc == 3 {
                let adaptation_len = new_packet[4] as usize;
                adaptation_len >= 7 && (new_packet[5] & ADAPTATION_FIELD_FLAG_PCR) != 0
            } else {
                false
            }
        };

        // AFC=2 (Adaptation Only), Scrambling=00 (Unscrambled), CC=cc
        pkt[3] = 0x20 | (cc & 0x0F);

        // Adaptation Field covers rest of packet (183 bytes)
        pkt[4] = 183;

        // If we contain a PCR, inject it. Otherwise just Discontinuity.
        if new_pkt_has_pcr {
            if let Some(base_pcr) = first_pcr {
                pkt[5] = 0x80 | 0x10; // Discontinuity (0x80) + PCR Flag (0x10)
                let offset_27mhz = pcr_offset_27mhz(timestamp_offset);
                let new_pcr = add_pcr_offset_27mhz(base_pcr, offset_27mhz);
                let pcr_bytes = encode_pcr(new_pcr);
                pkt[6..12].copy_from_slice(&pcr_bytes);
            } else {
                pkt[5] = 0x80;
            }
        } else {
            pkt[5] = 0x80; // Discontinuity Indicator Only
        }
    }

    fn rewrite_layout_timestamps(
        bytes: &mut BytesMut,
        layout: &HlsFiniteTsLayout,
        timestamp_offset: u64,
    ) -> Result<(), HlsFiniteTsLayoutError> {
        if timestamp_offset == 0 {
            return Ok(());
        }
        for location in layout.pcr_fields.iter().copied() {
            let original = decode_pcr_at_location(bytes, location)?;
            let adjusted = add_pcr_offset_27mhz(original, pcr_offset_27mhz(timestamp_offset));
            let end = location
                .byte_offset
                .checked_add(6)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination = bytes
                .get_mut(location.byte_offset..end)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            destination.copy_from_slice(&encode_pcr(adjusted));
        }
        for location in layout.timestamp_fields.iter().copied() {
            let original = gather_timestamp_bytes(bytes, location)?;
            let prefix = original[0] & 0xF0;
            let mut encoded = encode_timestamp(add_pts_dts_offset(
                decode_timestamp(&original),
                timestamp_offset,
            ));
            encoded[0] = (encoded[0] & 0x0F) | prefix;
            for (marker_index, (source_offset, value)) in
                location.byte_offsets.into_iter().zip(encoded).enumerate()
            {
                let destination = bytes
                    .get_mut(source_offset)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                *destination = value;
                if matches!(marker_index, 0 | 2 | 4) && value & 1 == 0 {
                    return Err(HlsFiniteTsLayoutError::InvalidPesTimestampField {
                        pid: location.pid,
                        kind: location.kind,
                    });
                }
            }
        }
        Ok(())
    }

    fn rewrite_source_packet_timestamps(
        &self,
        bytes: &mut BytesMut,
        output_start: usize,
        layout: &HlsFiniteTsLayout,
        packet_layout: HlsFiniteTsPacketLayout,
        timestamp_offset: u64,
    ) -> Result<(), HlsFiniteTsLayoutError> {
        if timestamp_offset == 0 {
            return Ok(());
        }
        let packet_end = packet_layout.packet_start.saturating_add(TS_PACKET_SIZE);
        if let Some(pcr_field_index) = packet_layout.pcr_field_index {
            let location = layout
                .pcr_fields
                .get(pcr_field_index)
                .copied()
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let original = decode_pcr_at_location(&self.buffer, location)?;
            let adjusted = add_pcr_offset_27mhz(original, pcr_offset_27mhz(timestamp_offset));
            let relative_offset = location
                .byte_offset
                .checked_sub(packet_layout.packet_start)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination_start = output_start
                .checked_add(relative_offset)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let destination_end = destination_start
                .checked_add(6)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            bytes
                .get_mut(destination_start..destination_end)
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?
                .copy_from_slice(&encode_pcr(adjusted));
        }
        let field_indices = layout
            .packet_timestamp_field_indices
            .get(packet_layout.timestamp_field_indices_start..packet_layout.timestamp_field_indices_end)
            .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
        for field_index in field_indices.iter().copied() {
            let location = layout
                .timestamp_fields
                .get(field_index)
                .copied()
                .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
            let original = gather_timestamp_bytes(&self.buffer, location)?;
            let prefix = original[0] & 0xF0;
            let mut encoded = encode_timestamp(add_pts_dts_offset(
                decode_timestamp(&original),
                timestamp_offset,
            ));
            encoded[0] = (encoded[0] & 0x0F) | prefix;
            for (source_offset, value) in location.byte_offsets.into_iter().zip(encoded) {
                if !(packet_layout.packet_start..packet_end).contains(&source_offset) {
                    continue;
                }
                let relative_offset = source_offset
                    .checked_sub(packet_layout.packet_start)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                let destination_offset = output_start
                    .checked_add(relative_offset)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
                *bytes
                    .get_mut(destination_offset)
                    .ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)? = value;
            }
        }
        Ok(())
    }

    /// Returns next chunks with adjusted PTS/DTS and PCR.
    /// All timestamp rewrites are performed in-place on the `BytesMut` output buffer to avoid
    /// per-packet heap allocations. PID continuity-counter lookup is O(1) via a fixed 8192-entry array.
    pub fn next_chunk(&mut self) -> Option<Bytes> {
        if self.length == 0 {
            return None;
        }
        let packet_count = ts_chunk_packet_count();
        let mut bytes = BytesMut::with_capacity(TS_PACKET_SIZE * packet_count);
        let mut packets_remaining = packet_count;

        while packets_remaining > 0 {
            if self.current_pos >= self.length {
                // Loop back — advance timestamp offset so PTS/DTS/PCR remain monotonically
                // increasing across loops. Resetting to 0 causes decoders (MPV, ffmpeg) to see
                // a backward timestamp jump and treat the loop as end-of-stream or corrupt data.
                self.current_pos = 0;
                // Advance timestamps by one full source duration per loop so output time is
                // monotonic for clients. Resetting to zero causes backward jumps that some
                // players interpret as stream end/corruption after the first cycle.
                if let Some(stream_duration_90khz) = self.duration_ticks_90khz() {
                    self.timestamp_offset = add_pts_dts_offset(self.timestamp_offset, stream_duration_90khz);
                    self.current_dts = add_pts_dts_offset(self.current_dts, stream_duration_90khz);
                } else {
                    // PCR-only (or malformed) assets may not expose PTS/DTS-derived duration.
                    // Force one discontinuity marker after wrap so decoders do not see identical
                    // timestamp cycles as a continuous timeline.
                    self.force_discontinuity_on_wrap = true;
                }

                // Reset only the discontinuity-sent flag so injection packets are emitted at the
                // start of the next loop. Continuity counter values keep running so CC remains
                // globally monotonic across loops.
                for entry in self.cc_entries.iter_mut().flatten() {
                    entry.1 = false;
                }
            }

            let current_pos = self.current_pos;
            let packet_start = self.packet_starts[current_pos];
            let packet = &self.buffer[packet_start..packet_start + TS_PACKET_SIZE];
            let packet_layout = self
                .finite_hls_layout
                .as_ref()
                .ok()
                .and_then(|layout| layout.packets.get(current_pos))
                .copied();
            let packet_has_payload = packet_layout.map_or_else(
                || matches!((packet[3] >> 4) & 0b11, 0b01 | 0b11),
                |layout| layout.has_payload,
            );

            // O(1) PID lookup — PID is at most 13 bits (0–8191).
            let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
            let entry = &mut self.cc_entries[pid as usize];
            // Normalize payload continuity per PID to a clean local sequence.
            let (counter, discontinuity_sent) = entry.get_or_insert((0, false));

            // Disable synthetic discontinuity packet insertion for looped custom streams.
            // In practice this can produce demuxer corruption on some clients (PES mismatch).
            // Monotonic timestamps + stable continuity counters are sufficient here.
            let inject_discontinuity = self.force_discontinuity_on_wrap && !*discontinuity_sent;
            if inject_discontinuity {
                let extra_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };
                Self::generate_discontinuity_packet(
                    packet,
                    extra_packet_cc,
                    self.first_pcr,
                    self.timestamp_offset,
                    &mut bytes,
                );
                self.force_discontinuity_on_wrap = false;
            }
            *discontinuity_sent = true;
            let payload_packet_cc = if packet_has_payload { *counter } else { packet[3] & 0x0F };

            // TS continuity counter increments only when payload is present (AFC=01/11).
            if packet_has_payload {
                *counter = (*counter + 1) % 16;
            }

            // Append the original packet into `bytes`, then mutate the appended slice in-place.
            let pkt_start = bytes.len();
            bytes.extend_from_slice(packet);

            // Apply the computed CC to the payload packet.
            bytes[pkt_start + 3] = (bytes[pkt_start + 3] & 0xF0) | (payload_packet_cc & 0x0F);

            if let (Ok(layout), Some(packet_layout)) = (&self.finite_hls_layout, packet_layout) {
                if self
                    .rewrite_source_packet_timestamps(
                        &mut bytes,
                        pkt_start,
                        layout,
                        packet_layout,
                        self.timestamp_offset,
                    )
                    .is_err()
                {
                    return None;
                }
            }

            self.current_pos += 1;
            packets_remaining -= 1;
        }

        Some(bytes.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pts_dts_payload_packet(pid: u16, cc: u8, pts: u64, dts: u64) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID high bits
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10 | (cc & 0x0F); // payload only

        let payload = &mut packet[4..];
        payload[0] = 0x00;
        payload[1] = 0x00;
        payload[2] = 0x01;
        payload[3] = 0xE0;
        payload[4] = 0x00;
        payload[5] = 0x00;
        payload[6] = 0x80;
        payload[7] = 0xC0; // PTS + DTS present
        payload[8] = 0x0A;

        let mut pts_bytes = encode_timestamp(pts);
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
        payload[9..14].copy_from_slice(&pts_bytes);

        let mut dts_bytes = encode_timestamp(dts);
        dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
        payload[14..19].copy_from_slice(&dts_bytes);

        packet
    }

    fn build_pts_only_payload_packet(pid: u16, cc: u8, pts: u64) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10 | (cc & 0x0F);
        let payload = &mut packet[4..];
        payload[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05]);
        payload[9..14].copy_from_slice(&encode_timestamp(pts));
        packet
    }

    fn pts_only_pes_header(pts: u64) -> [u8; 14] {
        let mut header = [0u8; 14];
        header[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05]);
        header[9..14].copy_from_slice(&encode_timestamp(pts));
        header
    }

    fn pts_dts_pes_header(pts: u64, dts: u64) -> [u8; 19] {
        let mut header = [0u8; 19];
        header[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0xC0, 0x0A]);
        let mut pts_bytes = encode_timestamp(pts);
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
        header[9..14].copy_from_slice(&pts_bytes);
        let mut dts_bytes = encode_timestamp(dts);
        dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
        header[14..19].copy_from_slice(&dts_bytes);
        header
    }

    fn build_split_pes_header_packets(
        pid: u16,
        first_cc: u8,
        header: &[u8],
        payload_lengths: &[usize],
    ) -> Vec<[u8; TS_PACKET_SIZE]> {
        let mut header_offset = 0usize;
        payload_lengths
            .iter()
            .copied()
            .enumerate()
            .map(|(packet_index, payload_length)| {
                assert!((1..=184).contains(&payload_length));
                let mut packet = [0xFF_u8; TS_PACKET_SIZE];
                let packet_index = u8::try_from(packet_index).expect("test packet index fits");
                packet[0] = SYNC_BYTE;
                packet[1] = ((pid >> 8) as u8 & 0x1F) | (u8::from(packet_index == 0) * 0x40);
                packet[2] = (pid & 0xFF) as u8;
                packet[3] = if payload_length == 184 {
                    0x10 | (first_cc.wrapping_add(packet_index) & 0x0F)
                } else {
                    0x30 | (first_cc.wrapping_add(packet_index) & 0x0F)
                };
                let payload_offset = if payload_length == 184 {
                    4
                } else {
                    let adaptation_length = 183usize.saturating_sub(payload_length);
                    packet[4] = u8::try_from(adaptation_length).expect("test adaptation length fits");
                    packet[5] = 0;
                    5 + adaptation_length
                };
                let remaining = header.len().saturating_sub(header_offset);
                let copied = remaining.min(payload_length);
                packet[payload_offset..payload_offset + copied]
                    .copy_from_slice(&header[header_offset..header_offset + copied]);
                header_offset = header_offset.saturating_add(copied);
                packet
            })
            .collect()
    }

    fn build_pts_dts_pcr_packet(pid: u16, cc: u8, pts: u64, dts: u64, pcr_90khz: u64) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x30 | (cc & 0x0F);
        packet[4] = 7;
        packet[5] = ADAPTATION_FIELD_FLAG_PCR;
        packet[6..12].copy_from_slice(&encode_pcr(pcr_90khz.saturating_mul(300)));

        let payload = &mut packet[12..];
        payload[0] = 0x00;
        payload[1] = 0x00;
        payload[2] = 0x01;
        payload[3] = 0xE0;
        payload[4] = 0x00;
        payload[5] = 0x00;
        payload[6] = 0x80;
        payload[7] = 0xC0;
        payload[8] = 0x0A;

        let mut pts_bytes = encode_timestamp(pts);
        pts_bytes[0] = (pts_bytes[0] & 0x0F) | 0x30;
        payload[9..14].copy_from_slice(&pts_bytes);
        let mut dts_bytes = encode_timestamp(dts);
        dts_bytes[0] = (dts_bytes[0] & 0x0F) | 0x10;
        payload[14..19].copy_from_slice(&dts_bytes);
        packet
    }

    fn packet_timestamps(packet: &[u8]) -> (u64, u64, u64) {
        let layout = build_hls_finite_ts_layout(packet).expect("test packet layout");
        let pts = layout
            .timestamp_fields
            .iter()
            .copied()
            .find(|field| field.kind == HlsTsTimestampFieldKind::Pts)
            .and_then(|field| decode_timestamp_at_location(packet, field).ok())
            .expect("test packet PTS");
        let dts = layout
            .timestamp_fields
            .iter()
            .copied()
            .find(|field| field.kind == HlsTsTimestampFieldKind::Dts)
            .and_then(|field| decode_timestamp_at_location(packet, field).ok())
            .expect("test packet DTS");
        let pcr = layout
            .pcr_fields
            .first()
            .copied()
            .and_then(|field| decode_pcr_at_location(packet, field).ok())
            .expect("test packet PCR")
            / 300;
        (pts, dts, pcr)
    }

    fn build_adaptation_only_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = (pid >> 8) as u8 & 0x1F;
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x20 | (cc & 0x0F); // adaptation only
        packet[4] = 183;
        packet[5] = 0;
        packet
    }

    fn build_pcr_only_packet(pid: u16, cc: u8, pcr_90khz: u64) -> [u8; TS_PACKET_SIZE] {
        let mut packet = build_adaptation_only_packet(pid, cc);
        packet[5] = ADAPTATION_FIELD_FLAG_PCR;
        packet[6..12].copy_from_slice(&encode_pcr(pcr_90khz.saturating_mul(300)));
        packet
    }

    fn build_payload_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFF_u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = (pid >> 8) as u8 & 0x1F;
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10 | (cc & 0x0F);
        packet
    }

    fn assert_ffmpeg_compatible_continuity(bytes: &[u8]) {
        assert_eq!(bytes.len() % TS_PACKET_SIZE, 0, "unaligned MPEG-TS fixture");
        let mut last_continuity: [Option<u8>; 8192] = [None; 8192];
        for (packet_index, packet) in bytes.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
            let evidence =
                inspect_hls_ts_packet(packet, packet_index.saturating_mul(TS_PACKET_SIZE))
                    .expect("valid MPEG-TS fixture packet");
            if evidence.pid == NULL_PID {
                continue;
            }
            if let Some(previous) = last_continuity[usize::from(evidence.pid)] {
                let expected = if evidence.has_payload() {
                    previous.wrapping_add(1) & 0x0F
                } else {
                    previous
                };
                assert!(
                    evidence.discontinuity || evidence.continuity_counter == expected,
                    "PID {} continuity failed at packet {packet_index}: expected {expected}, actual {}",
                    evidence.pid,
                    evidence.continuity_counter
                );
            }
            last_continuity[usize::from(evidence.pid)] = Some(evidence.continuity_counter);
        }
    }

    #[test]
    fn finite_discontinuity_marker_sets_ffmpeg_compatible_counter_baseline() {
        let cases = [
            ("payload wrap", build_payload_packet(0x0100, 0), 15),
            ("payload increment", build_payload_packet(0x0101, 7), 6),
            ("adaptation only", build_adaptation_only_packet(0x0102, 5), 5),
        ];
        for (name, following, expected_marker_cc) in cases {
            let mut output = BytesMut::new();
            append_finite_discontinuity_packet(&following, &mut output);
            let marker = &output[..TS_PACKET_SIZE];

            assert_eq!((marker[3] >> 4) & 0b11, 0b10, "{name} marker AFC");
            assert_eq!(marker[5] & 0x80, 0x80, "{name} discontinuity");
            assert_eq!(marker[3] & 0x0F, expected_marker_cc, "{name} marker CC");

            output.extend_from_slice(&following);
            assert_ffmpeg_compatible_continuity(&output);
        }
    }

    #[test]
    fn discontinuity_packet_does_not_advance_payload_cc() {
        let packet = build_pts_dts_payload_packet(0x0100, 7, 90_000, 87_000);
        let mut buf = TransportStreamBuffer::new(packet.to_vec());
        let chunk = buf.next_chunk().expect("expected chunk");
        assert!(chunk.len() >= TS_PACKET_SIZE * 2);

        // First emitted packet is injected discontinuity (adaptation-only),
        // second is the actual payload packet for the same PID.
        let disc_cc = chunk[3] & 0x0F;
        let disc_afc = (chunk[3] >> 4) & 0b11;
        let payload_cc = chunk[TS_PACKET_SIZE + 3] & 0x0F;
        assert_eq!(disc_afc, 0b10);
        assert_eq!(disc_cc, payload_cc);
    }

    #[test]
    fn adaptation_only_packets_keep_same_continuity_counter() {
        let packet = build_adaptation_only_packet(0x0011, 5);
        let mut buf = TransportStreamBuffer::new(packet.to_vec());
        let chunk = buf.next_chunk().expect("expected chunk");
        // Each of the 7 loop iterations emits a discontinuity packet + the actual packet
        // because the single-packet buffer loops on every iteration.
        let total_packets = PACKET_COUNT * 2;
        assert_eq!(chunk.len(), TS_PACKET_SIZE * total_packets);

        for i in 0..total_packets {
            let cc = chunk[i * TS_PACKET_SIZE + 3] & 0x0F;
            assert_eq!(cc, 5, "packet {i} CC mismatch");
        }
    }

    #[test]
    fn runtime_custom_assets_use_exact_per_pid_presentation_duration() {
        let fixtures: [(&str, &[u8]); 6] = [
            (
                "channel_unavailable.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/channel_unavailable.ts")).as_slice(),
            ),
            (
                "hls_session_or_lease_expired.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/hls_session_or_lease_expired.ts")).as_slice(),
            ),
            (
                "low_priority_preempted.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/low_priority_preempted.ts")).as_slice(),
            ),
            (
                "provider_connections_exhausted.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/provider_connections_exhausted.ts")).as_slice(),
            ),
            (
                "user_account_expired.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/user_account_expired.ts")).as_slice(),
            ),
            (
                "user_connections_exhausted.ts",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/user_connections_exhausted.ts")).as_slice(),
            ),
        ];
        for (name, bytes) in fixtures {
            let buffer = TransportStreamBuffer::new(bytes.to_vec());
            assert_eq!(buffer.duration_ticks_90khz(), Some(902_400), "{name} tick duration");
            assert_eq!(buffer.duration_ms(), Some(10_027), "{name} rounded HLS duration");
        }
    }

    #[test]
    fn provisioning_asset_uses_exact_per_pid_presentation_duration() {
        let buffer = TransportStreamBuffer::new(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/panel_api_provisioning.ts")).to_vec(),
        );

        assert_eq!(buffer.duration_ticks_90khz(), Some(182_400));
        assert_eq!(buffer.duration_ms(), Some(2_027));
    }

    #[test]
    fn custom_asset_stride_does_not_overlap_previous_audio_presentation() {
        let buffer = TransportStreamBuffer::new(
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/channel_unavailable.ts")).to_vec(),
        );
        let duration = buffer.duration_ticks_90khz().expect("exact custom asset duration");
        let segment_zero = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("segment zero");
        let segment_one = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: duration,
                continuity_seed: 0,
                logical_segment_index: 1,
            })
            .expect("segment one");
        let zero = TransportStreamBuffer::new(segment_zero.to_vec());
        let one = TransportStreamBuffer::new(segment_one.to_vec());
        let zero_audio = zero
            .finite_hls_presentation_duration
            .as_ref()
            .and_then(|duration| {
                duration
                    .timelines
                    .iter()
                    .find(|timeline| timeline.cadence_ticks_90khz == 1_920)
            })
            .expect("segment zero audio timeline");
        let one_audio = one
            .finite_hls_presentation_duration
            .as_ref()
            .and_then(|duration| {
                duration
                    .timelines
                    .iter()
                    .find(|timeline| timeline.cadence_ticks_90khz == 1_920)
            })
            .expect("segment one audio timeline");

        assert_eq!(duration, 902_400);
        assert_ne!(duration, 901_650);
        assert!(one_audio.first_pts_90khz >= zero_audio.end_exclusive_90khz);
        assert!(
            zero_audio.first_pts_90khz.saturating_add(901_650) < zero_audio.end_exclusive_90khz,
            "the previous mixed-clock stride would overlap the final audio presentation"
        );
    }

    #[test]
    fn finite_hls_segments_are_deterministic_aligned_and_timestamp_shifted() {
        let first = build_pts_dts_payload_packet(0x0100, 7, 90_000, 87_000);
        let second = build_pts_dts_payload_packet(0x0100, 8, 180_000, 177_000);
        let mut raw = Vec::new();
        raw.extend_from_slice(&first);
        raw.extend_from_slice(&second);
        let buffer = TransportStreamBuffer::new(raw);
        let spec = HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 900_000,
            continuity_seed: 3,
            logical_segment_index: 0,
        };

        let rendered = buffer.render_finite_hls_segment(spec).expect("finite render");
        let repeated = buffer.render_finite_hls_segment(spec).expect("repeated finite render");

        assert_eq!(rendered, repeated);
        assert_eq!(rendered.len() % TS_PACKET_SIZE, 0);
        assert_eq!(rendered[3] & 0x0F, 3);
        assert_eq!(rendered[TS_PACKET_SIZE + 3] & 0x0F, 4);
        assert_eq!(decode_timestamp(&rendered[13..18]), 990_000);
        assert_eq!(decode_timestamp(&rendered[18..23]), 987_000);
    }

    #[test]
    fn consecutive_finite_hls_segments_advance_cc_pts_dts_and_pcr() {
        let first = build_pts_dts_pcr_packet(0x0100, 7, 90_000, 87_000, 90_000);
        let second = build_pts_dts_pcr_packet(0x0100, 8, 180_000, 177_000, 180_000);
        let mut raw = Vec::new();
        raw.extend_from_slice(&first);
        raw.extend_from_slice(&second);
        let buffer = TransportStreamBuffer::new(raw);
        let duration = buffer.duration_ticks_90khz().expect("transportable duration");

        let segment_zero = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 3,
                logical_segment_index: 0,
            })
            .expect("first segment");
        let segment_one = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: duration,
                continuity_seed: 3,
                logical_segment_index: 1,
            })
            .expect("second segment");

        assert_eq!(segment_zero[TS_PACKET_SIZE + 3] & 0x0F, 4);
        assert_eq!(segment_one[3] & 0x0F, 5);
        assert_eq!(segment_one[TS_PACKET_SIZE + 3] & 0x0F, 6);

        let zero_last = packet_timestamps(&segment_zero[TS_PACKET_SIZE..]);
        let one_first = packet_timestamps(&segment_one[..TS_PACKET_SIZE]);
        assert!(one_first.0 > zero_last.0, "PTS must advance across segment boundary");
        assert!(one_first.1 > zero_last.1, "DTS must advance across segment boundary");
        assert!(one_first.2 > zero_last.2, "PCR must advance across segment boundary");
    }

    #[test]
    fn finite_hls_timestamp_rewrite_wraps_pts_dts_and_pcr_without_overflow() {
        let packet = build_pts_dts_pcr_packet(
            0x0100,
            7,
            MAX_PTS_DTS.saturating_sub(10),
            MAX_PTS_DTS.saturating_sub(20),
            MAX_PTS_DTS.saturating_sub(10),
        );
        let buffer = TransportStreamBuffer::new(packet.to_vec());

        let rendered = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 30,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("finite wrapped segment");

        assert_eq!(packet_timestamps(&rendered), (20, 10, 20));
    }

    #[test]
    fn split_pts_layout_profile_relative_and_anchored_rewrite_are_exact() {
        let mut packets = build_split_pes_header_packets(
            0x0101,
            0,
            &pts_only_pes_header(90_000),
            &[12, 2],
        );
        packets.push(build_pts_only_payload_packet(0x0101, 2, 93_000));
        let raw = packets.into_iter().flatten().collect::<Vec<_>>();
        let buffer = TransportStreamBuffer::new(raw);
        let layout = buffer.finite_hls_layout.as_ref().expect("split PTS layout");
        let pts_fields = layout
            .timestamp_fields
            .iter()
            .copied()
            .filter(|field| field.kind == HlsTsTimestampFieldKind::Pts)
            .collect::<Vec<_>>();
        assert_eq!(pts_fields.len(), 2);
        let split_field = pts_fields[0];
        assert_ne!(
            split_field.byte_offsets[0] / TS_PACKET_SIZE,
            split_field.byte_offsets[4] / TS_PACKET_SIZE
        );
        assert_eq!(
            buffer.finite_hls_timestamp_profile(),
            Some(HlsTsTimestampProfile {
                first_clock_90khz: 90_000,
                last_clock_90khz: 93_000,
                span_ticks_90khz: 3_000,
                observed_pts_or_dts: true,
                observed_pcr: false,
            })
        );

        let relative = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 7_000,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative split PTS");
        assert_eq!(
            decode_timestamp_at_location(&relative, split_field).expect("relative split PTS value"),
            97_000
        );
        let anchored = buffer
            .finalize_prepared_finite_hls_segment(
                &relative,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 11_000,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("anchored split PTS");
        assert_eq!(
            decode_timestamp_at_location(&anchored, split_field).expect("anchored split PTS value"),
            108_000
        );
        assert_ne!(
            decode_timestamp_at_location(&anchored, split_field).expect("rewritten split PTS"),
            90_000
        );
        let encoded = gather_timestamp_bytes(&anchored, split_field).expect("anchored split PTS bytes");
        assert!(encoded[0] & 1 != 0 && encoded[2] & 1 != 0 && encoded[4] & 1 != 0);
    }

    #[test]
    fn split_pts_dts_layout_rewrites_both_fields_across_distinct_packet_boundaries() {
        let packets = build_split_pes_header_packets(
            0x0101,
            5,
            &pts_dts_pes_header(90_000, 87_000),
            &[11, 5, 3],
        );
        let raw = packets.into_iter().flatten().collect::<Vec<_>>();
        let buffer = TransportStreamBuffer::new(raw);
        let layout = buffer.finite_hls_layout.as_ref().expect("split PTS+DTS layout");
        let pts = layout
            .timestamp_fields
            .iter()
            .copied()
            .find(|field| field.kind == HlsTsTimestampFieldKind::Pts)
            .expect("split PTS location");
        let dts = layout
            .timestamp_fields
            .iter()
            .copied()
            .find(|field| field.kind == HlsTsTimestampFieldKind::Dts)
            .expect("split DTS location");
        assert_ne!(pts.byte_offsets[0] / TS_PACKET_SIZE, pts.byte_offsets[4] / TS_PACKET_SIZE);
        assert_ne!(dts.byte_offsets[0] / TS_PACKET_SIZE, dts.byte_offsets[4] / TS_PACKET_SIZE);
        assert_ne!(pts.byte_offsets[0] / TS_PACKET_SIZE, dts.byte_offsets[0] / TS_PACKET_SIZE);

        let relative = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 1_000,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative split PTS+DTS");
        let anchored = buffer
            .finalize_prepared_finite_hls_segment(
                &relative,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 2_000,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("anchored split PTS+DTS");
        assert_eq!(decode_timestamp_at_location(&anchored, pts), Ok(93_000));
        assert_eq!(decode_timestamp_at_location(&anchored, dts), Ok(90_000));
        for field in [pts, dts] {
            let encoded = gather_timestamp_bytes(&anchored, field).expect("split timestamp bytes");
            assert!(encoded[0] & 1 != 0 && encoded[2] & 1 != 0 && encoded[4] & 1 != 0);
        }
    }

    #[test]
    fn timestamp_profile_scanner_reads_split_pts_across_packets() {
        let mut packets = build_split_pes_header_packets(
            0x0101,
            0,
            &pts_only_pes_header(90_000),
            &[12, 2],
        );
        packets.push(build_pts_only_payload_packet(0x0101, 2, 93_000));
        let mut scanner = HlsTsTimestampProfileScanner::new(10_000);
        for packet in packets {
            scanner.push_aligned_packet(&packet);
        }

        assert_eq!(
            scanner.finish(),
            Some(HlsTsTimestampProfile {
                first_clock_90khz: 90_000,
                last_clock_90khz: 93_000,
                span_ticks_90khz: 3_000,
                observed_pts_or_dts: true,
                observed_pcr: false,
            })
        );
    }

    #[test]
    fn timestamp_profile_scanner_rejects_one_clock_sample() {
        let packet = build_pts_only_payload_packet(0x0101, 0, 90_000);
        let mut scanner = HlsTsTimestampProfileScanner::new(10_000);
        scanner.push_aligned_packet(&packet);

        assert_eq!(scanner.finish(), None);
    }

    #[test]
    fn split_pts_incomplete_at_eof_fails_with_typed_layout_error() {
        let packets = build_split_pes_header_packets(
            0x0101,
            0,
            &pts_only_pes_header(90_000),
            &[12, 2],
        );
        let buffer = TransportStreamBuffer::new(packets[0].to_vec());

        assert!(matches!(
            buffer.finite_hls_layout.as_ref(),
            Err(HlsFiniteTsLayoutError::IncompletePesTimestampHeader { pid: 0x0101 })
        ));
        assert_eq!(
            buffer.render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 1,
                continuity_seed: 0,
                logical_segment_index: 0,
            }),
            Err(HlsFiniteTsRenderError::InvalidAsset)
        );
        assert_eq!(buffer.duration_ticks_90khz(), None);
    }

    #[test]
    fn split_pts_continuity_discontinuity_fails_with_typed_layout_error() {
        let mut packets = build_split_pes_header_packets(
            0x0101,
            0,
            &pts_only_pes_header(90_000),
            &[12, 2],
        );
        packets[1][3] = (packets[1][3] & 0xF0) | 3;
        let raw = packets.into_iter().flatten().collect::<Vec<_>>();
        let buffer = TransportStreamBuffer::new(raw);

        assert!(matches!(
            buffer.finite_hls_layout.as_ref(),
            Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity {
                pid: 0x0101,
                expected: Some(1),
                actual: 3,
            })
        ));
    }

    #[test]
    fn per_pid_presentation_duration_unwraps_33_bit_audio_and_video_clocks() {
        let video_pid = 0x0101;
        let audio_pid = 0x0102;
        let packets = [
            build_pts_only_payload_packet(video_pid, 0, MAX_PTS_DTS - 1_080),
            build_pts_only_payload_packet(audio_pid, 0, MAX_PTS_DTS - 1_000),
            build_pts_only_payload_packet(video_pid, 1, 1_920),
            build_pts_only_payload_packet(audio_pid, 1, 920),
            build_pts_only_payload_packet(video_pid, 2, 4_920),
            build_pts_only_payload_packet(audio_pid, 2, 2_840),
        ];
        let raw = packets.into_iter().flatten().collect::<Vec<_>>();
        let buffer = TransportStreamBuffer::new(raw);
        let duration = buffer
            .finite_hls_presentation_duration
            .as_ref()
            .expect("per-PID wrapped duration");

        assert_eq!(duration.source, HlsTsPresentationClockSource::Pts);
        assert_eq!(duration.first_presentation_clock_90khz, MAX_PTS_DTS - 1_080);
        assert_eq!(duration.end_exclusive_clock_90khz, MAX_PTS_DTS + 7_920);
        assert_eq!(duration.duration_ticks_90khz, 9_000);
        assert_eq!(
            duration.timelines.as_ref(),
            [
                HlsTsPidPresentationTimeline {
                    pid: video_pid,
                    first_pts_90khz: MAX_PTS_DTS - 1_080,
                    last_pts_90khz: MAX_PTS_DTS + 4_920,
                    cadence_ticks_90khz: 3_000,
                    end_exclusive_90khz: MAX_PTS_DTS + 7_920,
                },
                HlsTsPidPresentationTimeline {
                    pid: audio_pid,
                    first_pts_90khz: MAX_PTS_DTS - 1_000,
                    last_pts_90khz: MAX_PTS_DTS + 2_840,
                    cadence_ticks_90khz: 1_920,
                    end_exclusive_90khz: MAX_PTS_DTS + 4_760,
                },
            ]
        );
    }

    #[test]
    fn pcr_fallback_uses_observed_cadence_and_rejects_a_single_sample() {
        let pid = 0x0101;
        let packets = [
            build_pcr_only_packet(pid, 0, 90_000),
            build_pcr_only_packet(pid, 0, 91_920),
        ];
        let buffer = TransportStreamBuffer::new(packets.into_iter().flatten().collect());
        let duration = buffer
            .finite_hls_presentation_duration
            .as_ref()
            .expect("PCR fallback duration");

        assert_eq!(duration.source, HlsTsPresentationClockSource::PcrFallback);
        assert_eq!(duration.first_presentation_clock_90khz, 90_000);
        assert_eq!(duration.end_exclusive_clock_90khz, 93_840);
        assert_eq!(duration.duration_ticks_90khz, 3_840);
        assert_eq!(
            duration.timelines.as_ref(),
            [HlsTsPidPresentationTimeline {
                pid,
                first_pts_90khz: 90_000,
                last_pts_90khz: 91_920,
                cadence_ticks_90khz: 1_920,
                end_exclusive_90khz: 93_840,
            }]
        );

        let single = TransportStreamBuffer::new(build_pcr_only_packet(pid, 0, 90_000).to_vec());
        assert_eq!(single.duration_ticks_90khz(), None);
    }

    #[test]
    fn timestamp_profile_scanner_unwraps_one_33_bit_boundary() {
        let before_wrap = MAX_PTS_DTS - 1_000;
        let packets = [
            build_pts_dts_pcr_packet(
                0x0100,
                0,
                before_wrap + 200,
                before_wrap + 100,
                before_wrap,
            ),
            build_pts_dts_pcr_packet(0x0100, 1, 1_200, 1_100, 1_000),
        ];
        let mut scanner = HlsTsTimestampProfileScanner::new(5_000);
        for packet in packets {
            scanner.push_aligned_packet(&packet);
        }

        assert_eq!(
            scanner.finish(),
            Some(HlsTsTimestampProfile {
                first_clock_90khz: before_wrap,
                last_clock_90khz: 1_200,
                span_ticks_90khz: 2_200,
                observed_pts_or_dts: true,
                observed_pcr: true,
            })
        );
    }

    #[test]
    fn timestamp_profile_scanner_rejects_implausible_backward_segment_jump() {
        let packets = [
            build_pts_dts_pcr_packet(0x0100, 0, 500_200, 500_100, 500_000),
            build_pts_dts_pcr_packet(0x0100, 1, 100_200, 100_100, 100_000),
        ];
        let mut scanner = HlsTsTimestampProfileScanner::new(90_000);
        for packet in packets {
            scanner.push_aligned_packet(&packet);
        }

        assert_eq!(scanner.finish(), None);
    }

    #[test]
    fn terminal_splice_anchor_places_first_terminal_clock_after_live_tail() {
        let live_last_clock_90khz = 1_003_618_800;
        let live = HlsTsTimestampProfile {
            first_clock_90khz: live_last_clock_90khz - 900_000,
            last_clock_90khz: live_last_clock_90khz,
            span_ticks_90khz: 900_000,
            observed_pts_or_dts: true,
            observed_pcr: true,
        };
        let terminal = HlsTsTimestampProfile {
            first_clock_90khz: 0,
            last_clock_90khz: 902_400,
            span_ticks_90khz: 902_400,
            observed_pts_or_dts: true,
            observed_pcr: true,
        };

        let anchor = HlsTsSpliceAnchor::between(live, terminal).expect("valid modular splice");

        assert_eq!(
            forward_clock_distance_90khz(live_last_clock_90khz, anchor.terminal_first_clock),
            HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ
        );
        assert_eq!(anchor.timestamp_delta_ticks, anchor.terminal_first_clock);
        assert!(
            forward_clock_distance_90khz(live_last_clock_90khz, anchor.terminal_first_clock) < 90_000,
            "splice must not contain the observed near-full-cycle jump"
        );
    }

    #[test]
    fn terminal_splice_preserves_terminal_asset_audio_video_offset() {
        let audio = build_pts_dts_payload_packet(0x0102, 0, 0, 0);
        let video = build_pts_dts_payload_packet(0x0101, 0, 1_920, 1_920);
        let mut raw = Vec::new();
        raw.extend_from_slice(&audio);
        raw.extend_from_slice(&video);
        raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 1, 1_920, 1_920));
        raw.extend_from_slice(&build_pts_dts_payload_packet(0x0101, 1, 4_920, 4_920));
        let buffer = TransportStreamBuffer::new(raw);
        let asset_profile = buffer.finite_hls_timestamp_profile().expect("asset timestamp profile");
        let live = HlsTsTimestampProfile {
            first_clock_90khz: 1_003_000_000,
            last_clock_90khz: 1_003_618_800,
            span_ticks_90khz: 618_800,
            observed_pts_or_dts: true,
            observed_pcr: false,
        };
        let anchor = HlsTsSpliceAnchor::between(live, asset_profile).expect("splice anchor");
        let prepared = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative segment");
        let anchored = buffer
            .finalize_prepared_finite_hls_segment(
                &prepared,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("anchored segment");
        let audio_pts = decode_timestamp(&anchored[13..18]);
        let video_pts = decode_timestamp(&anchored[TS_PACKET_SIZE + 13..TS_PACKET_SIZE + 18]);

        assert_eq!(forward_clock_distance_90khz(audio_pts, video_pts), 1_920);
    }

    #[test]
    fn terminal_splice_anchor_wraps_pts_dts_and_pcr_exactly() {
        let original_clock = 10;
        let packet = build_pts_dts_pcr_packet(0x0100, 7, original_clock, original_clock, original_clock);
        let next_packet = build_pts_dts_pcr_packet(0x0100, 8, 3_010, 3_010, 3_010);
        let mut raw = packet.to_vec();
        raw.extend_from_slice(&next_packet);
        let buffer = TransportStreamBuffer::new(raw);
        let asset_profile = buffer.finite_hls_timestamp_profile().expect("asset timestamp profile");
        let live_last_clock_90khz = MAX_PTS_DTS - 40;
        let live = HlsTsTimestampProfile {
            first_clock_90khz: live_last_clock_90khz - 90_000,
            last_clock_90khz: live_last_clock_90khz,
            span_ticks_90khz: 90_000,
            observed_pts_or_dts: true,
            observed_pcr: true,
        };
        let anchor = HlsTsSpliceAnchor::between(live, asset_profile).expect("wrapped splice anchor");
        let prepared = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative segment");
        let anchored = buffer
            .finalize_prepared_finite_hls_segment(
                &prepared,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("wrapped anchored segment");

        assert_eq!(anchor.terminal_first_clock, 50);
        assert_eq!(packet_timestamps(&anchored), (50, 50, 50));
        assert_eq!(forward_clock_distance_90khz(live_last_clock_90khz, 50), 90);
    }

    #[test]
    fn first_terminal_segment_marks_discontinuity_for_each_non_null_pid() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&build_payload_packet(0x0000, 9));
        raw.extend_from_slice(&build_payload_packet(0x0100, 3));
        raw.extend_from_slice(&build_pts_dts_payload_packet(0x0101, 7, 90_000, 87_000));
        raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 5, 90_000, 90_000));
        raw.extend_from_slice(&build_pts_dts_pcr_packet(0x0103, 4, 90_000, 90_000, 90_000));
        raw.extend_from_slice(&build_payload_packet(NULL_PID, 12));
        let buffer = TransportStreamBuffer::new(raw);
        let prepared = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 2,
                logical_segment_index: 0,
            })
            .expect("relative segment");
        let finalized = buffer
            .finalize_prepared_finite_hls_segment(
                &prepared,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 90,
                    discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            )
            .expect("splice segment");

        let packets = finalized.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
        let mut marked_pids = Vec::new();
        for pair in packets.windows(2) {
            let marker = pair[0];
            let following = pair[1];
            if (marker[3] >> 4) & 0b11 == 0b10 && marker[4] == 183 && marker[5] & 0x80 != 0 {
                assert_eq!(ts_packet_pid(marker), ts_packet_pid(following));
                assert_eq!((marker[3] & 0x0F).wrapping_add(1) & 0x0F, following[3] & 0x0F);
                marked_pids.push(ts_packet_pid(marker));
            }
        }
        marked_pids.sort_unstable();

        assert_eq!(marked_pids, vec![0x0000, 0x0100, 0x0101, 0x0102, 0x0103]);
        assert!(!marked_pids.contains(&NULL_PID));
    }

    #[test]
    fn finalized_terminal_asset_preserves_tracks_and_ffmpeg_continuity() {
        const TERMINAL_ASSET_BYTES: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/channel_unavailable.ts"));
        let buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let expected_signature = buffer.finite_hls_track_signature().expect("terminal asset track signature");
        let prepared = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative terminal asset");
        let finalized = buffer
            .finalize_prepared_finite_hls_segment(
                &prepared,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 1_003_618_890,
                    discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            )
            .expect("anchored terminal asset");

        assert_ffmpeg_compatible_continuity(&finalized);
        let outcome = crate::api::model::inspect_mpeg_ts(
            std::io::Cursor::new(finalized),
            crate::api::model::HlsTsProbeProtection::Clear,
            crate::api::model::HlsTsProbeBudget::default(),
        )
        .expect("finalized terminal asset remains inspectable");
        assert_eq!(outcome, crate::api::model::HlsTsProbeOutcome::Found(expected_signature));
    }

    #[test]
    fn prepared_terminal_finalization_rejects_packet_layout_mismatch() {
        let packet = build_pts_dts_pcr_packet(0x0100, 0, 90_000, 87_000, 86_000);
        let buffer = TransportStreamBuffer::new(packet.to_vec());
        let prepared = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative terminal segment");
        let mut mismatched = BytesMut::from(prepared.as_ref());
        mismatched[2] ^= 1;

        assert_eq!(
            buffer.finalize_prepared_finite_hls_segment(
                &mismatched.freeze(),
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 1,
                    discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            ),
            Err(HlsFiniteTsRenderError::PreparedLayoutMismatch)
        );
    }

    #[test]
    fn later_terminal_segments_continue_cc_without_repeating_splice_marker() {
        let first = build_pts_dts_pcr_packet(0x0101, 7, 90_000, 87_000, 90_000);
        let second = build_pts_dts_payload_packet(0x0102, 5, 90_000, 90_000);
        let mut raw = Vec::new();
        raw.extend_from_slice(&first);
        raw.extend_from_slice(&second);
        raw.extend_from_slice(&build_pts_dts_pcr_packet(0x0101, 8, 180_000, 177_000, 180_000));
        raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 6, 180_000, 180_000));
        let buffer = TransportStreamBuffer::new(raw);
        let duration = buffer.duration_ticks_90khz().expect("asset duration");
        let relative_zero = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("relative zero");
        let relative_one = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: duration,
                continuity_seed: 0,
                logical_segment_index: 1,
            })
            .expect("relative one");
        let finalized_zero = buffer
            .finalize_prepared_finite_hls_segment(
                &relative_zero,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 1_000_000,
                    discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            )
            .expect("anchored zero");
        let finalized_one = buffer
            .finalize_prepared_finite_hls_segment(
                &relative_one,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: 1_000_000,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("anchored one");

        assert_eq!(finalized_one.len(), relative_one.len());
        assert!(finalized_one
            .as_chunks::<TS_PACKET_SIZE>()
            .0
            .iter()
            .all(|packet| !((packet[3] >> 4) & 0b11 == 0b10 && packet[4] == 183 && packet[5] & 0x80 != 0)));
        let zero_payload = finalized_zero
            .as_chunks::<TS_PACKET_SIZE>()
            .0
            .iter()
            .filter(|packet| (packet[3] >> 4) & 0b11 != 0b10)
            .collect::<Vec<_>>();
        let one_payload = finalized_one.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
        for (zero, one) in zero_payload.into_iter().zip(one_payload) {
            assert_eq!(ts_packet_pid(zero), ts_packet_pid(one));
            assert_eq!((zero[3] & 0x0F).wrapping_add(2) & 0x0F, one[3] & 0x0F);
        }
        let mut concatenated = finalized_zero.to_vec();
        concatenated.extend_from_slice(&finalized_one);
        assert_ffmpeg_compatible_continuity(&concatenated);
    }

    #[test]
    fn live_to_terminal_splice_has_monotone_pts_dts_pcr_and_valid_cc_reset() {
        let live_first_clock = 1_003_528_800;
        let live_last_clock = 1_003_618_800;
        let mut live_bytes = Vec::new();
        live_bytes.extend_from_slice(&build_pts_dts_pcr_packet(
            0x0101,
            13,
            live_first_clock,
            live_first_clock,
            live_first_clock,
        ));
        live_bytes.extend_from_slice(&build_pts_dts_pcr_packet(
            0x0101,
            14,
            live_last_clock,
            live_last_clock,
            live_last_clock,
        ));
        let live_buffer = TransportStreamBuffer::new(live_bytes.clone());
        let live_profile = live_buffer.finite_hls_timestamp_profile().expect("live tail profile");

        let mut terminal_bytes = Vec::new();
        terminal_bytes.extend_from_slice(&build_pts_dts_pcr_packet(0x0101, 0, 0, 0, 0));
        terminal_bytes.extend_from_slice(&build_pts_dts_pcr_packet(
            0x0101,
            1,
            90_000,
            90_000,
            90_000,
        ));
        let terminal_buffer = TransportStreamBuffer::new(terminal_bytes);
        let terminal_profile = terminal_buffer
            .finite_hls_timestamp_profile()
            .expect("terminal asset profile");
        let anchor = HlsTsSpliceAnchor::between(live_profile, terminal_profile).expect("splice anchor");
        let duration = terminal_buffer.duration_ticks_90khz().expect("terminal duration");
        let relative_zero = terminal_buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("terminal zero");
        let relative_one = terminal_buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: duration,
                continuity_seed: 0,
                logical_segment_index: 1,
            })
            .expect("terminal one");
        let terminal_zero = terminal_buffer
            .finalize_prepared_finite_hls_segment(
                &relative_zero,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                    discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
                },
            )
            .expect("anchored terminal zero");
        let terminal_one = terminal_buffer
            .finalize_prepared_finite_hls_segment(
                &relative_one,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                    discontinuity: HlsFiniteTsDiscontinuityMode::None,
                },
            )
            .expect("anchored terminal one");

        let zero_packets = terminal_zero.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
        assert_eq!((zero_packets[0][3] >> 4) & 0b11, 0b10);
        assert_eq!(zero_packets[0][5] & 0x80, 0x80);
        assert_eq!(
            (zero_packets[0][3] & 0x0F).wrapping_add(1) & 0x0F,
            zero_packets[1][3] & 0x0F
        );
        let terminal_one_packets = terminal_one.as_chunks::<TS_PACKET_SIZE>().0;
        assert!(terminal_one_packets.iter().all(|packet| !((packet[3] >> 4) & 0b11 == 0b10 && packet[5] & 0x80 != 0)));

        let zero_first = packet_timestamps(zero_packets[1]);
        let zero_last = packet_timestamps(zero_packets[2]);
        let one_first = packet_timestamps(&terminal_one[..TS_PACKET_SIZE]);
        assert_eq!(forward_clock_distance_90khz(live_last_clock, zero_first.0), 90);
        assert!(forward_clock_distance_90khz(zero_first.0, zero_last.0) < MAX_PTS_DTS / 2);
        assert!(forward_clock_distance_90khz(zero_last.0, one_first.0) < MAX_PTS_DTS / 2);
        assert_eq!(
            forward_clock_distance_90khz(zero_last.0, one_first.0),
            duration.saturating_sub(90_000)
        );
        assert_eq!(zero_first.0, zero_first.1);
        assert_eq!(zero_first.0, zero_first.2);
        assert_eq!(one_first.0, one_first.1);
        assert_eq!(one_first.0, one_first.2);

        let mut concatenated = live_bytes;
        concatenated.extend_from_slice(&terminal_zero);
        concatenated.extend_from_slice(&terminal_one);
        assert_ffmpeg_compatible_continuity(&concatenated);
        let mut scanner = HlsTsTimestampProfileScanner::new(duration.saturating_mul(3));
        for packet in concatenated.as_chunks::<TS_PACKET_SIZE>().0 {
            scanner.push_aligned_packet(packet);
        }
        let combined = scanner.finish().expect("combined splice profile");
        assert!(combined.span_ticks_90khz < duration.saturating_mul(3));
    }

    #[test]
    fn hls_prepared_terminal_bundle_pcr_uses_300_factor_and_large_offsets_wrap_exactly() {
        let base_pcr_90khz = 123_456;
        let large_offset_90khz = u64::MAX - 37;
        let packet = build_pts_dts_pcr_packet(
            0x0100,
            7,
            90_000,
            87_000,
            base_pcr_90khz,
        );
        let buffer = TransportStreamBuffer::new(packet.to_vec());

        let rendered = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: large_offset_90khz,
                continuity_seed: 0,
                logical_segment_index: 11,
            })
            .expect("finite segment with a large timestamp offset");

        let actual_pcr_27mhz = decode_pcr(&rendered[6..12]);
        let expected_offset_27mhz =
            (u128::from(large_offset_90khz) * 300_u128) % u128::from(MAX_PCR);
        let expected_pcr_27mhz = u64::try_from(
            (u128::from(base_pcr_90khz) * 300_u128 + expected_offset_27mhz)
                % u128::from(MAX_PCR),
        )
        .expect("PCR modulo fits in u64");
        let legacy_wrapped_offset = large_offset_90khz.wrapping_mul(300) % MAX_PCR;

        assert_eq!(pcr_offset_27mhz(1), 300);
        assert_ne!(
            pcr_offset_27mhz(large_offset_90khz),
            legacy_wrapped_offset,
            "large offsets must be multiplied before modulo without u64 wrap"
        );
        assert_eq!(actual_pcr_27mhz, expected_pcr_27mhz);
    }

    #[test]
    fn hls_prepared_terminal_bundle_pts_and_dts_large_offsets_wrap_modulo_33_bits() {
        let presentation_timestamp = MAX_PTS_DTS - 123;
        let decoding_timestamp = MAX_PTS_DTS - 456;
        let large_offset_90khz = u64::MAX - 37;
        let packet = build_pts_dts_pcr_packet(
            0x0100,
            7,
            presentation_timestamp,
            decoding_timestamp,
            90_000,
        );
        let buffer = TransportStreamBuffer::new(packet.to_vec());

        let rendered = buffer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: large_offset_90khz,
                continuity_seed: 0,
                logical_segment_index: 11,
            })
            .expect("finite segment with wrapped PTS and DTS");

        let expected_timestamp = |timestamp| {
            u64::try_from(
                (u128::from(timestamp) + u128::from(large_offset_90khz))
                    % u128::from(MAX_PTS_DTS),
            )
            .expect("PTS/DTS modulo fits in u64")
        };
        let (rendered_presentation_timestamp, rendered_decoding_timestamp, _) =
            packet_timestamps(&rendered);

        assert_eq!(
            rendered_presentation_timestamp,
            expected_timestamp(presentation_timestamp)
        );
        assert_eq!(
            rendered_decoding_timestamp,
            expected_timestamp(decoding_timestamp)
        );
    }
}
