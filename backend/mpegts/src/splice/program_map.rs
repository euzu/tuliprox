use crate::packet::{is_video_stream_type, pid_from_bytes, single_packet_section, TsPacket};
use std::collections::HashMap;

pub(super) const PAT_PID: u16 = 0;
pub(super) const PAT_TABLE_ID: u8 = 0x00;
pub(super) const PMT_TABLE_ID: u8 = 0x02;

/// Program structure of the MPEG-TS stream learned from its PAT and PMT.
#[derive(Default)]
pub(crate) struct ProgramMap {
    /// PMT PID → program number.
    pmt_pids: HashMap<u16, u16>,
    /// First video elementary stream: PID and stream type.
    video: Option<(u16, u8)>,
    /// CRC of the PAT the program map was built from.
    pat_crc: Option<u32>,
}

impl ProgramMap {
    #[must_use]
    pub(crate) fn new() -> Self { Self::default() }

    pub(crate) fn reset(&mut self) {
        self.pmt_pids.clear();
        self.video = None;
        self.pat_crc = None;
    }

    #[must_use]
    pub(crate) fn video(&self) -> Option<(u16, u8)> { self.video }

    #[must_use]
    pub(crate) fn program_of_pmt(&self, pmt_pid: u16) -> Option<u16> { self.pmt_pids.get(&pmt_pid).copied() }

    /// Learns PAT/PMT structure from `view`; returns true when it was a PMT of a known program.
    pub(crate) fn observe(&mut self, view: TsPacket<'_>) -> bool {
        if !view.payload_unit_start() {
            return false;
        }
        let pid = view.pid();
        let is_pmt = self.pmt_pids.contains_key(&pid);
        if pid != PAT_PID && !is_pmt {
            return false;
        }
        let payload = view.payload();
        let Some(range) = single_packet_section(payload) else {
            return false;
        };
        let section = &payload[range];
        let entries = &section[8..section.len() - 4];
        if pid == PAT_PID && section[0] == PAT_TABLE_ID {
            // A repeated PAT (several per second) leaves the program map as it is.
            let crc = u32::from_be_bytes([
                section[section.len() - 4],
                section[section.len() - 3],
                section[section.len() - 2],
                section[section.len() - 1],
            ]);
            if self.pat_crc == Some(crc) {
                return false;
            }
            self.pat_crc = Some(crc);
            self.pmt_pids = entries
                .as_chunks::<4>()
                .0
                .iter()
                .filter_map(|entry| {
                    let program = u16::from_be_bytes([entry[0], entry[1]]);
                    let pmt_pid = pid_from_bytes(entry[2], entry[3]);
                    (program != 0).then_some((pmt_pid, program))
                })
                .collect();
            return false;
        }
        if is_pmt && section[0] == PMT_TABLE_ID && entries.len() >= 4 {
            let info_len = (usize::from(entries[2] & 0x0F) << 8) | usize::from(entries[3]);
            let mut streams = entries.get(4 + info_len..).unwrap_or(&[]);
            self.video = None;
            while streams.len() >= 5 {
                let stream_type = streams[0];
                let es_pid = pid_from_bytes(streams[1], streams[2]);
                let es_info_len = (usize::from(streams[3] & 0x0F) << 8) | usize::from(streams[4]);
                if self.video.is_none() && is_video_stream_type(stream_type) {
                    self.video = Some((es_pid, stream_type));
                }
                streams = streams.get(5 + es_info_len..).unwrap_or(&[]);
            }
            return true;
        }
        false
    }
}
