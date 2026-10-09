use super::{pes::HlsFiniteTsLayoutError, HlsTsPcrFieldLocation, HlsTsTimestampFieldLocation};
use bytes::BytesMut;

// PCR wraps at 2^33 * 300 (base is 33-bit, multiplied by 300 to get 27 MHz units).
// Using 1<<42 was slightly too large and could cause strict-decoder issues on hardware
// that computes modulo 2^33 on the base before multiplying.
pub(super) const MAX_PCR: u64 = (1u64 << 33) * 300;

pub(super) const MAX_PTS_DTS: u64 = 1 << 33;

// 33 bit PTS/DTS cycle
pub(super) const HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ: u64 = 90;

pub(super) const HLS_TS_PROFILE_MIN_TOLERANCE_TICKS_90KHZ: u64 = 2 * 90_000;

#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn add_pts_dts_offset(timestamp: u64, offset_90khz: u64) -> u64 {
    ((u128::from(timestamp) + u128::from(offset_90khz)) % u128::from(MAX_PTS_DTS)) as u64
}

#[inline]
pub(super) fn forward_clock_distance_90khz(start: u64, end: u64) -> u64 {
    end.wrapping_add(MAX_PTS_DTS).wrapping_sub(start) % MAX_PTS_DTS
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn pcr_offset_27mhz(offset_90khz: u64) -> u64 {
    (u128::from(offset_90khz) * 300_u128 % u128::from(MAX_PCR)) as u64
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn add_pcr_offset_27mhz(timestamp_27mhz: u64, offset_27mhz: u64) -> u64 {
    ((u128::from(timestamp_27mhz) + u128::from(offset_27mhz)) % u128::from(MAX_PCR)) as u64
}

pub(super) const TS_PACKET_SIZE: usize = 188;

pub(super) const SYNC_BYTE: u8 = 0x47;

pub(super) const PACKET_COUNT: usize = 7;

// Reduced from 250 to 7 (1316 bytes) to prevent latency/timeout on low-bitrate streams
const MAX_PACKET_COUNT: usize = 250;

/// Packets per emitted chunk; overridable via `TULIPROX_TS_CHUNK_PACKETS` (1-250).
/// Larger chunks raise throughput, smaller chunks lower latency on low-bitrate streams.
pub(super) fn ts_chunk_packet_count() -> usize {
    static PACKET_COUNT_OVERRIDE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("TULIPROX_TS_CHUNK_PACKETS")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|count| (1..=MAX_PACKET_COUNT).contains(count))
            .unwrap_or(PACKET_COUNT)
    });
    *PACKET_COUNT_OVERRIDE
}

pub(super) const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10;

// PCR flag bit in adaptation field flags
pub(super) const NULL_PID: u16 = 0x1FFF;

pub(super) const HLS_TS_MAX_PENDING_PES_HEADERS: usize = 64;

pub(super) const HLS_TS_TIMESTAMP_HEADER_BYTES: usize = 19;

/// Byte offset of PTS within a PES payload (after the 3-byte start code, `stream_id`, length, flags).
pub(super) const PES_PTS_OFFSET: usize = 9;

/// Byte offset of DTS within a PES payload when both PTS and DTS are present.
pub(super) const PES_DTS_OFFSET: usize = 14;

/// Decodes a 5-byte DTS/PTS field from PES header into u64 timestamp.
#[inline]
pub(super) fn decode_timestamp(ts_bytes: &[u8]) -> u64 {
    (((u64::from(ts_bytes[0]) >> 1) & 0x07) << 30)
        | (u64::from(ts_bytes[1]) << 22)
        | (((u64::from(ts_bytes[2]) >> 1) & 0x7F) << 15)
        | (u64::from(ts_bytes[3]) << 7)
        | ((u64::from(ts_bytes[4]) >> 1) & 0x7F)
}

/// Encodes a u64 timestamp into 5-byte PES DTS/PTS field
#[inline]
pub(super) fn encode_timestamp(ts: u64) -> [u8; 5] {
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
pub(super) fn decode_pcr(pcr_bytes: &[u8]) -> u64 {
    let pcr_base = (u64::from(pcr_bytes[0]) << 25)
        | ((u64::from(pcr_bytes[1])) << 17)
        | ((u64::from(pcr_bytes[2])) << 9)
        | ((u64::from(pcr_bytes[3])) << 1)
        | ((u64::from(pcr_bytes[4])) >> 7);
    let pcr_ext = ((u64::from(pcr_bytes[4]) & 1) << 8) | u64::from(pcr_bytes[5]);
    pcr_base * 300 + pcr_ext
}

#[derive(Clone, Copy)]
pub(super) enum HlsTsTimestampKind {
    PtsOrDts,
    Pcr,
}

#[inline]
pub(super) fn ts_packet_pid(packet: &[u8]) -> u16 { (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]) }

pub(super) fn same_finite_ts_packet_layout(source: &[u8], prepared: &[u8]) -> bool {
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

pub(super) fn append_finite_discontinuity_packet(following_packet: &[u8], output: &mut BytesMut) {
    let following_cc = following_packet[3] & 0x0F;
    let marker_cc = if following_packet[3] & 0x10 != 0 { following_cc.wrapping_sub(1) & 0x0F } else { following_cc };
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
pub(super) fn encode_pcr(pcr: u64) -> [u8; 6] {
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

pub(super) fn gather_timestamp_bytes(
    buffer: &[u8],
    location: HlsTsTimestampFieldLocation,
) -> Result<[u8; 5], HlsFiniteTsLayoutError> {
    let mut bytes = [0u8; 5];
    for (destination, offset) in bytes.iter_mut().zip(location.byte_offsets) {
        *destination = buffer.get(offset).copied().ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    }
    Ok(bytes)
}

pub(super) fn decode_timestamp_at_location(
    buffer: &[u8],
    location: HlsTsTimestampFieldLocation,
) -> Result<u64, HlsFiniteTsLayoutError> {
    gather_timestamp_bytes(buffer, location).map(|bytes| decode_timestamp(&bytes))
}

pub(super) fn decode_pcr_at_location(
    buffer: &[u8],
    location: HlsTsPcrFieldLocation,
) -> Result<u64, HlsFiniteTsLayoutError> {
    let end = location.byte_offset.checked_add(6).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    let bytes = buffer.get(location.byte_offset..end).ok_or(HlsFiniteTsLayoutError::InvalidTimestampLocation)?;
    Ok(decode_pcr(bytes))
}

pub(super) fn ticks_90khz_to_rounded_millis(ticks: u64) -> Option<u64> { ticks.checked_add(45)?.checked_div(90) }
