//! MPEG-TS packet basics shared by the buffer, the inspector and the splicer: sizes, sync search,
//! header fields, PSI sections and keyframe detection.

use bytes::Bytes;

pub const TS_PACKET_SIZE: usize = 188;
pub const TS_SYNC_BYTE: u8 = 0x47;
pub(crate) const NULL_PID: u16 = 0x1FFF;
/// PCR flag in the adaptation field flags.
pub(crate) const ADAPTATION_FIELD_FLAG_PCR: u8 = 0x10;
const NULL_PID_HIGH: u8 = 0x1F;
const NULL_PID_LOW: u8 = 0xFF;

const fn null_packet() -> [u8; TS_PACKET_SIZE] {
    let mut packet = [0xFF; TS_PACKET_SIZE];
    packet[0] = TS_SYNC_BYTE;
    packet[1] = NULL_PID_HIGH;
    packet[2] = NULL_PID_LOW;
    packet[3] = 0x10;
    packet
}

/// A null packet (PID 0x1FFF): decoders discard it, players and proxies see bytes flowing.
pub static TS_NULL_PACKET: [u8; TS_PACKET_SIZE] = null_packet();

pub fn ts_null_packet_bytes() -> Bytes { Bytes::from_static(&TS_NULL_PACKET) }

/// First offset in `offsets` with a sync byte at it and at the next `confirmations - 1` packet
/// starts. Packet starts past the end of `buf` count as matching only with `partial_tail`.
#[must_use]
pub(crate) fn find_sync(
    buf: &[u8],
    offsets: impl IntoIterator<Item = usize>,
    confirmations: usize,
    partial_tail: bool,
) -> Option<usize> {
    offsets.into_iter().take_while(|offset| *offset < buf.len()).find(|offset| {
        (0..confirmations).all(|index| match buf.get(offset + index * TS_PACKET_SIZE) {
            Some(byte) => *byte == TS_SYNC_BYTE,
            None => partial_tail,
        })
    })
}

/// Finds the first offset `o < 188` where `0x47` appears at `o`, `o + 188`, `o + 376`.
/// With fewer than 564 bytes at EOF, all available positions must match.
#[must_use]
pub fn ts_sync_offset(buf: &[u8]) -> Option<usize> { find_sync(buf, 0..TS_PACKET_SIZE, 3, true) }

/// A 13-bit PID from the two bytes that carry it (packet header, PAT and PMT entries).
#[must_use]
pub(crate) fn pid_from_bytes(high: u8, low: u8) -> u16 { (u16::from(high & 0x1F) << 8) | u16::from(low) }

#[must_use]
pub(crate) fn ts_packet_pid(packet: &[u8]) -> u16 { pid_from_bytes(packet[1], packet[2]) }

/// CRC-32/MPEG-2 of PSI sections: polynomial 0x04C11DB7, initial value 0xFFFFFFFF, no reflection,
/// no final xor. A section including its CRC yields 0.
#[must_use]
pub fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 { crc << 1 } else { (crc << 1) ^ 0x04C1_1DB7 };
        }
    }
    crc
}

/// Read access to the header fields of one 188-byte packet.
#[derive(Clone, Copy)]
pub(crate) struct TsPacket<'a>(pub(crate) &'a [u8]);

impl<'a> TsPacket<'a> {
    pub(crate) fn pid(self) -> u16 { ts_packet_pid(self.0) }

    pub(crate) fn payload_unit_start(self) -> bool { self.0[1] & 0x40 != 0 }

    /// `adaptation_field_control`: 0b01 payload only, 0b10 adaptation field only, 0b11 both.
    pub(crate) fn adaptation_field_control(self) -> u8 { (self.0[3] >> 4) & 0b11 }

    pub(crate) fn has_payload(self) -> bool { self.adaptation_field_control() & 0b01 != 0 }

    /// Length of the adaptation field (after its length byte), `None` without one.
    pub(crate) fn adaptation_len(self) -> Option<usize> { (self.0[3] & 0x20 != 0).then(|| usize::from(self.0[4])) }

    /// Whether the adaptation field carries a PCR (flag set and room for its 6 bytes).
    pub(crate) fn has_pcr(self) -> bool {
        self.adaptation_len().is_some_and(|len| len >= 7 && self.0[5] & ADAPTATION_FIELD_FLAG_PCR != 0)
    }

    pub(crate) fn random_access(self) -> bool {
        self.adaptation_len().is_some_and(|len| len >= 1 && self.0[5] & 0x40 != 0)
    }

    pub(crate) fn payload(self) -> &'a [u8] {
        if self.0[3] & 0x10 == 0 {
            return &[];
        }
        let start = self.adaptation_len().map_or(4, |len| 5 + len);
        self.0.get(start..).unwrap_or(&[])
    }

    pub(crate) fn payload_offset(self) -> Option<usize> {
        (self.0[3] & 0x10 != 0)
            .then(|| self.adaptation_len().map_or(4, |len| 5 + len))
            .filter(|start| *start < TS_PACKET_SIZE)
    }
}

/// Bounds of the PSI section that starts in `payload`, when it lies completely inside it.
pub(crate) fn single_packet_section(payload: &[u8]) -> Option<std::ops::Range<usize>> {
    let pointer = usize::from(*payload.first()?);
    let start = 1 + pointer;
    let header = payload.get(start..start + 3)?;
    let length = 3 + ((usize::from(header[1] & 0x0F) << 8) | usize::from(header[2]));
    (length >= 12 && start + length <= payload.len()).then_some(start..start + length)
}

/// `version_number` of a long-form section.
pub(crate) fn section_version(section: &[u8]) -> u8 { (section[5] >> 1) & 0x1F }

/// Sets `version_number` of a long-form section and recomputes its CRC.
pub(crate) fn rewrite_section_version(section: &mut [u8], version: u8) {
    section[5] = (section[5] & 0xC1) | ((version & 0x1F) << 1);
    let crc_at = section.len() - 4;
    let crc = mpeg2_crc32(&section[..crc_at]);
    section[crc_at..].copy_from_slice(&crc.to_be_bytes());
}

/// Most elementary-stream bytes of a PES searched for a keyframe.
pub(crate) const KEYFRAME_SCAN_BYTES: usize = 512;

/// Whether the PES payload starting in a packet begins an intra-coded picture, judged from the
/// NAL units (H.264, HEVC) or sequence header (MPEG-2 video) in this packet's elementary data.
#[must_use]
pub fn is_idr_start(stream_type: u8, pes_payload: &[u8]) -> bool {
    pes_elementary_data(pes_payload).is_some_and(|data| es_has_keyframe(stream_type, data))
}

/// The elementary data after the PES header in the payload of a PES start packet.
pub(crate) fn pes_elementary_data(pes_payload: &[u8]) -> Option<&[u8]> {
    if pes_payload.len() < 9 || pes_payload[..3] != [0, 0, 1] {
        return None;
    }
    pes_payload.get(9 + usize::from(pes_payload[8])..)
}

/// Whether `data`, the start of a PES's elementary data, opens an intra-coded picture: an IDR
/// slice or the SPS that opens an IDR access unit (H.264), IDR/CRA pictures or the VPS/SPS that
/// open them (HEVC), a sequence header (MPEG-2).
pub(crate) fn es_has_keyframe(stream_type: u8, data: &[u8]) -> bool {
    let data = &data[..data.len().min(KEYFRAME_SCAN_BYTES)];
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index..index + 3] == [0, 0, 1] {
            let unit = data[index + 3];
            let keyframe = match stream_type {
                0x1B => matches!(unit & 0x1F, 5 | 7),
                0x24 => matches!((unit >> 1) & 0x3F, 19..=21 | 32 | 33),
                0x01 | 0x02 => unit == 0xB3,
                _ => false,
            };
            if keyframe {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

pub(crate) fn is_video_stream_type(stream_type: u8) -> bool { matches!(stream_type, 0x01 | 0x02 | 0x1B | 0x24) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_sync_offset_empty() {
        assert_eq!(ts_sync_offset(&[]), None);
    }

    #[test]
    fn ts_sync_offset_aligned() {
        let mut buf = vec![0u8; 564];
        buf[0] = 0x47;
        buf[188] = 0x47;
        buf[376] = 0x47;
        assert_eq!(ts_sync_offset(&buf), Some(0));
    }

    #[test]
    fn ts_sync_offset_with_garbage_prefix() {
        let mut buf = vec![0u8; 17];
        let mut ts = vec![0u8; 564];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        buf.extend(ts);
        assert_eq!(ts_sync_offset(&buf), Some(17));
    }

    #[test]
    fn ts_sync_offset_short_buffer_at_eof() {
        let mut buf = vec![0u8; 200];
        buf[10] = 0x47;
        buf[198] = 0x47;
        assert_eq!(ts_sync_offset(&buf), Some(10));
    }

    #[test]
    fn ts_sync_offset_corrupt_packet() {
        let mut buf = vec![0u8; 564];
        buf[0] = 0x47;
        buf[188] = 0x48; // corrupt
        buf[376] = 0x47;
        assert_eq!(ts_sync_offset(&buf), None);
    }

    fn with_sync_at(len: usize, positions: &[usize]) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        for position in positions {
            buf[*position] = 0x47;
        }
        buf
    }

    #[test]
    fn find_sync_without_partial_tail_needs_every_confirmation() {
        let buf = with_sync_at(300, &[0, 188]);
        assert_eq!(find_sync(&buf, 0..=187, 3, false), None);
        assert_eq!(find_sync(&buf, 0..=187, 3, true), Some(0));
    }

    #[test]
    fn find_sync_searches_the_given_offsets() {
        let buf = with_sync_at(400 + 3 * TS_PACKET_SIZE, &[400, 588, 776]);
        assert_eq!(find_sync(&buf, 0..=500, 3, false), Some(400));
        assert_eq!(find_sync(&buf, 0..=399, 3, false), None);
    }

    #[test]
    fn packet_view_reads_pid_and_pcr_flag() {
        let mut packet = vec![0x47, 0x41, 0x00, 0x30, 7, 0x10];
        packet.resize(TS_PACKET_SIZE, 0);
        assert_eq!(ts_packet_pid(&packet), 0x100);
        assert!(TsPacket(&packet).has_pcr());
        packet[4] = 6;
        assert!(!TsPacket(&packet).has_pcr(), "a PCR needs 7 adaptation bytes");
    }
}
