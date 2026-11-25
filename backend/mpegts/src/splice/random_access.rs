use super::rap_detector::RapDetector;
use crate::packet::{ts_sync_offset, TsPacket, TS_PACKET_SIZE, TS_SYNC_BYTE};

/// Scans a live MPEG-TS byte stream chunk by chunk for random access points.
///
/// A random access point opens at the sync byte of the last PAT packet that is followed
/// by a PMT of a known program and a video keyframe PES.
pub struct TsRandomAccessScanner {
    stream_position: u64,
    carry: [u8; TS_PACKET_SIZE],
    carry_len: usize,
    carry_start: u64,
    synced: bool,
    detector: RapDetector,
}

impl Default for TsRandomAccessScanner {
    fn default() -> Self { Self::new() }
}

impl TsRandomAccessScanner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream_position: 0,
            carry: [0; TS_PACKET_SIZE],
            carry_len: 0,
            carry_start: 0,
            synced: false,
            detector: RapDetector::new(),
        }
    }

    /// Observes the next chunk. Returns the absolute stream byte position (counted over all bytes
    /// ever passed to `observe`, starting at 0) of the PAT packet sync byte that opens the newest
    /// random access point completed within this chunk, if any.
    pub fn observe(&mut self, chunk: &[u8]) -> Option<u64> {
        if chunk.is_empty() {
            return None;
        }

        let chunk_start = self.stream_position;
        let chunk_len_u64 = u64::try_from(chunk.len()).unwrap_or(0);
        self.stream_position = self.stream_position.saturating_add(chunk_len_u64);

        let mut chunk_offset = 0;
        let mut newest_rap = None;

        while chunk_offset < chunk.len() || (self.synced && self.carry_len == TS_PACKET_SIZE) {
            if self.synced {
                self.observe_synced(chunk, chunk_start, &mut chunk_offset, &mut newest_rap);
            } else {
                self.observe_unsynced(chunk, chunk_start, &mut chunk_offset);
            }
        }

        newest_rap
    }

    fn observe_synced(
        &mut self,
        chunk: &[u8],
        chunk_start: u64,
        chunk_offset: &mut usize,
        newest_rap: &mut Option<u64>,
    ) {
        if self.carry_len > 0 {
            let need = TS_PACKET_SIZE - self.carry_len;
            let avail = chunk.len() - *chunk_offset;
            if avail < need {
                self.carry[self.carry_len..self.carry_len + avail].copy_from_slice(&chunk[*chunk_offset..]);
                self.carry_len += avail;
                *chunk_offset = chunk.len();
                return;
            }
            self.carry[self.carry_len..TS_PACKET_SIZE].copy_from_slice(&chunk[*chunk_offset..*chunk_offset + need]);
            *chunk_offset += need;
            self.carry_len = TS_PACKET_SIZE;

            if self.carry[0] != TS_SYNC_BYTE {
                self.drop_sync();
                if let Some(offset) = ts_sync_offset(&self.carry) {
                    self.carry.copy_within(offset..TS_PACKET_SIZE, 0);
                    self.carry_len = TS_PACKET_SIZE - offset;
                    self.carry_start = self.carry_start.saturating_add(u64::try_from(offset).unwrap_or(0));
                    self.synced = true;
                } else {
                    self.carry_len = 0;
                }
                return;
            }

            let packet = self.carry;
            let packet_pos = self.carry_start;
            self.carry_len = 0;
            if let Some(pos) = self.detector.observe_packet(TsPacket(&packet), packet_pos) {
                *newest_rap = Some(pos);
            }
            return;
        }

        let remaining = chunk.len() - *chunk_offset;
        if remaining >= TS_PACKET_SIZE {
            let packet_bytes = &chunk[*chunk_offset..*chunk_offset + TS_PACKET_SIZE];
            if packet_bytes[0] != TS_SYNC_BYTE {
                self.drop_sync();
                return;
            }
            let packet_pos = chunk_start.saturating_add(u64::try_from(*chunk_offset).unwrap_or(0));
            *chunk_offset += TS_PACKET_SIZE;
            if let Some(pos) = self.detector.observe_packet(TsPacket(packet_bytes), packet_pos) {
                *newest_rap = Some(pos);
            }
        } else {
            if chunk[*chunk_offset] != TS_SYNC_BYTE {
                self.drop_sync();
                return;
            }
            self.carry[..remaining].copy_from_slice(&chunk[*chunk_offset..]);
            self.carry_len = remaining;
            self.carry_start = chunk_start.saturating_add(u64::try_from(*chunk_offset).unwrap_or(0));
            *chunk_offset = chunk.len();
        }
    }

    fn observe_unsynced(&mut self, chunk: &[u8], chunk_start: u64, chunk_offset: &mut usize) {
        let rem = &chunk[*chunk_offset..];
        if rem.len() >= TS_PACKET_SIZE {
            if let Some(offset) = ts_sync_offset(rem) {
                *chunk_offset += offset;
                self.synced = true;
            } else {
                *chunk_offset += TS_PACKET_SIZE;
            }
        } else {
            self.carry[..rem.len()].copy_from_slice(rem);
            self.carry_len = rem.len();
            self.carry_start = chunk_start.saturating_add(u64::try_from(*chunk_offset).unwrap_or(0));
            *chunk_offset += rem.len();

            if let Some(offset) = ts_sync_offset(&self.carry[..self.carry_len]) {
                self.carry.copy_within(offset..self.carry_len, 0);
                self.carry_len -= offset;
                self.carry_start = self.carry_start.saturating_add(u64::try_from(offset).unwrap_or(0));
                self.synced = true;
            } else {
                self.carry_len = 0;
            }
        }
    }

    fn drop_sync(&mut self) {
        self.synced = false;
        // Lost sync invalidates any carried partial packet.
        self.carry_len = 0;
        self.carry_start = 0;
        self.detector.reset();
    }
}
