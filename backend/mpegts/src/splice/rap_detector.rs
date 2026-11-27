use super::program_map::{ProgramMap, PAT_PID};
use crate::packet::{es_has_keyframe, is_idr_start, pes_elementary_data, TsPacket, KEYFRAME_SCAN_BYTES};

/// Detects random access points packet by packet in an aligned MPEG-TS stream.
///
/// A random access point opens at the sync byte of the last PAT packet that is followed
/// by a PMT of a known program and a video keyframe PES (or without a video stream in the PMT,
/// PAT + PMT).
pub(crate) struct RapDetector {
    programs: ProgramMap,
    last_pat: Option<u64>,
    pmt_after_last_pat: bool,
    pes_open: bool,
    /// PAT position that opened the random access point the open PES may complete.
    pes_pat: Option<u64>,
    pes_scratch: Vec<u8>,
}

impl Default for RapDetector {
    fn default() -> Self { Self::new() }
}

impl RapDetector {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            programs: ProgramMap::new(),
            last_pat: None,
            pmt_after_last_pat: false,
            pes_open: false,
            pes_pat: None,
            pes_scratch: Vec::with_capacity(KEYFRAME_SCAN_BYTES),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.programs.reset();
        self.last_pat = None;
        self.pmt_after_last_pat = false;
        self.close_open_pes();
    }

    #[must_use]
    pub(crate) fn pending_pat(&self) -> Option<u64> {
        if self.pes_open {
            self.pes_pat
        } else {
            None
        }
    }

    /// Observes one whole 188-byte packet at `packet_pos`. Returns the PAT position
    /// if this packet completes a random access point.
    pub(crate) fn observe_packet(&mut self, view: TsPacket<'_>, packet_pos: u64) -> Option<u64> {
        let mut completed_rap = None;
        self.inspect_packet(view, packet_pos, &mut completed_rap);
        completed_rap
    }

    fn inspect_packet(&mut self, view: TsPacket<'_>, packet_pos: u64, newest_rap: &mut Option<u64>) {
        let pid = view.pid();
        let pusi = view.payload_unit_start();

        // A repeated PAT inside an open video PES leaves it open: its keyframe still belongs to
        // the PAT captured when the PES started.
        if pid == PAT_PID && pusi {
            self.last_pat = Some(packet_pos);
            self.pmt_after_last_pat = false;
        }

        let selected_video = self.programs.video();
        let was_pmt = self.programs.observe(view);
        // An open PES belongs to the selected video stream; a new selection invalidates it.
        if self.programs.video() != selected_video {
            self.close_open_pes();
        }
        if was_pmt && self.last_pat.is_some() {
            if self.programs.video().is_none() {
                *newest_rap = self.last_pat;
                self.pmt_after_last_pat = false;
                self.close_open_pes();
                return;
            }
            if self.programs.video_pmt() == Some(pid) {
                self.pmt_after_last_pat = true;
            }
        }

        let Some((video_pid, stream_type)) = self.programs.video() else {
            return;
        };

        if pid != video_pid {
            return;
        }

        if !pusi {
            if self.pes_open {
                self.continue_open_pes(view, stream_type, newest_rap);
            }
            return;
        }

        // A new video PES replaces any open one.
        self.close_open_pes();
        if !self.pmt_after_last_pat {
            return;
        }
        if view.random_access() || is_idr_start(stream_type, view.payload()) {
            *newest_rap = self.last_pat;
            self.pmt_after_last_pat = false;
        } else if let Some(data) = pes_elementary_data(view.payload()) {
            self.pes_open = true;
            self.pes_pat = self.last_pat;
            self.pes_scratch.extend_from_slice(&data[..data.len().min(KEYFRAME_SCAN_BYTES)]);
        }
    }

    fn continue_open_pes(&mut self, view: TsPacket<'_>, stream_type: u8, newest_rap: &mut Option<u64>) {
        let room = KEYFRAME_SCAN_BYTES.saturating_sub(self.pes_scratch.len());
        let payload = view.payload();
        self.pes_scratch.extend_from_slice(&payload[..payload.len().min(room)]);
        if es_has_keyframe(stream_type, &self.pes_scratch) {
            *newest_rap = self.pes_pat;
            if self.pes_pat == self.last_pat {
                self.pmt_after_last_pat = false;
            }
            self.close_open_pes();
        } else if self.pes_scratch.len() >= KEYFRAME_SCAN_BYTES {
            self.close_open_pes();
        }
    }

    fn close_open_pes(&mut self) {
        self.pes_open = false;
        self.pes_pat = None;
        self.pes_scratch.clear();
    }
}
