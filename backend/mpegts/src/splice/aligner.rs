use crate::packet::{
    es_has_keyframe, is_idr_start, is_video_stream_type, pes_elementary_data, pid_from_bytes, rewrite_section_version,
    section_version, single_packet_section, ts_sync_offset, TsPacket, KEYFRAME_SCAN_BYTES, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};
use bytes::{Buf, Bytes, BytesMut};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const PAT_PID: u16 = 0;
const PAT_TABLE_ID: u8 = 0x00;
const PMT_TABLE_ID: u8 = 0x02;

/// A change to an emitted packet, applied after the chunk was observed.
enum PacketEdit {
    Discontinuity { offset: usize },
    Version { payload: std::ops::Range<usize>, version: u8 },
}

pub enum SpliceOutput {
    /// Nothing to send yet.
    Hold,
    /// Whole TS packets to send.
    Emit(Bytes),
    /// Whole TS packets in two pieces, sent in order: the packet completed from carried-over
    /// bytes and the aligned rest of the chunk, which is passed on without copying.
    EmitTwo(Bytes, Bytes),
}

impl SpliceOutput {
    fn of(first: Option<Bytes>, second: Option<Bytes>) -> Self {
        match (first.filter(|bytes| !bytes.is_empty()), second.filter(|bytes| !bytes.is_empty())) {
            (Some(first), Some(second)) => Self::EmitTwo(first, second),
            (Some(single), None) | (None, Some(single)) => Self::Emit(single),
            (None, None) => Self::Hold,
        }
    }
}

/// Set of 13-bit PIDs.
struct PidSet(Box<[u64; 128]>);

impl PidSet {
    fn new() -> Self { Self(Box::new([0; 128])) }

    fn clear(&mut self) { self.0.fill(0); }

    /// Adds `pid`; true when it was not in the set.
    fn insert(&mut self, pid: u16) -> bool {
        let (word, bit) = (usize::from(pid >> 6) & 127, 1u64 << (pid & 63));
        let added = self.0[word] & bit == 0;
        self.0[word] |= bit;
        added
    }
}

/// Program structure of the member currently read, learned from its PAT and PMT.
#[derive(Default)]
struct Programs {
    /// PMT PID → program number.
    pmt_pids: HashMap<u16, u16>,
    /// First video elementary stream: PID and stream type.
    video: Option<(u16, u8)>,
    /// CRC of the PAT the program map was built from.
    pat_crc: Option<u32>,
}

/// Version rewrite of one member: per table, the offset added to the source version.
#[derive(Default)]
struct VersionOffsets {
    pat: Option<u8>,
    pmt: HashMap<u16, u8>,
}

struct Aligning {
    started: Instant,
    buffer: BytesMut,
    first_pat: Option<usize>,
    last_pat: Option<usize>,
    pmt_after_last_pat: bool,
    /// Video PES whose start packet showed no keyframe yet: the PAT before its start and the
    /// elementary data read so far, searched on as its next packets arrive.
    open_pes: Option<(usize, Vec<u8>)>,
}

/// Aligns provider bytes to whole TS packets and splices a new member into the output: after
/// `begin_member` the new member is held back until it can start at a PAT/PMT followed by a
/// keyframe, its PSI versions continue the ones already emitted, and the first packet per PID
/// announces the discontinuity.
pub struct TsSpliceAligner {
    carry: BytesMut,
    budget_bytes: usize,
    budget: Duration,
    programs: Programs,
    aligning: Option<Aligning>,
    /// Number of members started; the first one is never rewritten.
    members: u32,
    offsets: VersionOffsets,
    emitted_pat_version: Option<u8>,
    emitted_pmt_versions: HashMap<u16, u8>,
    discontinuity_seen: PidSet,
}

impl TsSpliceAligner {
    #[must_use]
    pub fn new(budget_bytes: usize, budget: Duration) -> Self {
        Self {
            carry: BytesMut::new(),
            budget_bytes,
            budget,
            programs: Programs::default(),
            aligning: None,
            members: 1,
            offsets: VersionOffsets::default(),
            emitted_pat_version: None,
            emitted_pmt_versions: HashMap::new(),
            discontinuity_seen: PidSet::new(),
        }
    }

    /// Starts the next member: its bytes are held until the splice point.
    pub fn begin_member(&mut self, now: Instant) {
        self.carry.clear();
        self.programs = Programs::default();
        self.offsets = VersionOffsets::default();
        self.discontinuity_seen.clear();
        self.members += 1;
        self.aligning = Some(Aligning {
            started: now,
            buffer: BytesMut::new(),
            first_pat: None,
            last_pat: None,
            pmt_after_last_pat: false,
            open_pes: None,
        });
    }

    #[must_use]
    pub fn is_aligning(&self) -> bool { self.aligning.is_some() }

    pub fn push(&mut self, chunk: Bytes, now: Instant) -> SpliceOutput {
        let (head, body) = self.whole_packets(chunk);
        let head = head.and_then(|head| self.emit_packets(head, now));
        let body = self.emit_packets(body, now);
        SpliceOutput::of(head, body)
    }

    fn emit_packets(&mut self, packets: Bytes, now: Instant) -> Option<Bytes> {
        if self.aligning.is_some() {
            return self.push_aligning(&packets, now);
        }
        (!packets.is_empty()).then(|| self.process_emitted(packets))
    }

    /// Whole packets of `chunk` plus what was carried over: the packet completed from the carry
    /// (copied, at most 187 bytes from the chunk) and the aligned rest of the chunk (not copied).
    fn whole_packets(&mut self, mut chunk: Bytes) -> (Option<Bytes>, Bytes) {
        let mut head = None;
        if !self.carry.is_empty() {
            if self.carry[0] != TS_SYNC_BYTE {
                return (None, self.resync(&chunk));
            }
            let need = TS_PACKET_SIZE - self.carry.len();
            if chunk.len() < need {
                self.carry.extend_from_slice(&chunk);
                return (None, Bytes::new());
            }
            self.carry.extend_from_slice(&chunk[..need]);
            chunk.advance(need);
            head = Some(self.carry.split().freeze());
        }
        if chunk.first().is_some_and(|byte| *byte != TS_SYNC_BYTE) {
            return (head, self.resync(&chunk));
        }
        let whole = chunk.len() / TS_PACKET_SIZE * TS_PACKET_SIZE;
        self.carry.extend_from_slice(&chunk[whole..]);
        chunk.truncate(whole);
        (head, chunk)
    }

    /// Copying path for input without packet sync at its start: finds the sync and returns the
    /// whole packets after it.
    fn resync(&mut self, chunk: &[u8]) -> Bytes {
        self.carry.extend_from_slice(chunk);
        if self.carry.first() != Some(&TS_SYNC_BYTE) {
            let Some(offset) = ts_sync_offset(&self.carry) else {
                let keep = self.carry.len().min(TS_PACKET_SIZE - 1);
                let drop = self.carry.len() - keep;
                self.carry.advance(drop);
                return Bytes::new();
            };
            self.carry.advance(offset);
        }
        let whole = self.carry.len() / TS_PACKET_SIZE * TS_PACKET_SIZE;
        self.carry.split_to(whole).freeze()
    }

    fn push_aligning(&mut self, packets: &[u8], now: Instant) -> Option<Bytes> {
        let mut aligning = self.aligning.take()?;
        let mut start = None;
        for packet in packets.as_chunks::<TS_PACKET_SIZE>().0 {
            let position = aligning.buffer.len();
            aligning.buffer.extend_from_slice(packet);
            if start.is_some() {
                continue;
            }
            let view = TsPacket(packet);
            if view.pid() == PAT_PID && view.payload_unit_start() {
                aligning.first_pat.get_or_insert(position);
                aligning.last_pat = Some(position);
                aligning.pmt_after_last_pat = false;
            }
            if self.observe(view) && aligning.last_pat.is_some() {
                aligning.pmt_after_last_pat = true;
                if self.programs.video.is_none() {
                    start = aligning.last_pat;
                    continue;
                }
            }
            if !aligning.pmt_after_last_pat {
                continue;
            }
            if self.is_keyframe(view) {
                start = aligning.last_pat;
            } else if let Some(pat) = self.continue_open_pes(&mut aligning, view) {
                start = Some(pat);
            }
        }
        let over_budget =
            aligning.buffer.len() > self.budget_bytes || now.saturating_duration_since(aligning.started) >= self.budget;
        let start = match start {
            Some(start) => start,
            None if over_budget => aligning.first_pat.unwrap_or(0),
            None => {
                self.aligning = Some(aligning);
                return None;
            }
        };
        // Dropping the skipped head keeps the buffer the only owner, so the PSI rewrite below
        // edits it in place instead of copying it.
        let mut buffer = aligning.buffer;
        buffer.advance(start);
        let output = buffer.freeze();
        // The emitted part is observed again from its PAT on for versions and discontinuity.
        self.programs = Programs::default();
        Some(self.process_emitted(output))
    }

    /// Follows the elementary data of a video PES across its packets; returns the PAT before the
    /// PES start once a keyframe shows up within its first bytes.
    fn continue_open_pes(&self, aligning: &mut Aligning, view: TsPacket<'_>) -> Option<usize> {
        let (pid, stream_type) = self.programs.video?;
        if view.pid() != pid {
            return None;
        }
        if view.payload_unit_start() {
            // A new PES without keyframe in its start packet: its next packets may hold it.
            aligning.open_pes = aligning.last_pat.zip(pes_elementary_data(view.payload()).map(<[u8]>::to_vec));
            return None;
        }
        let (pat, data) = aligning.open_pes.as_mut()?;
        let room = KEYFRAME_SCAN_BYTES.saturating_sub(data.len());
        let payload = view.payload();
        data.extend_from_slice(&payload[..payload.len().min(room)]);
        if es_has_keyframe(stream_type, data) {
            let pat = *pat;
            aligning.open_pes = None;
            return Some(pat);
        }
        if data.len() >= KEYFRAME_SCAN_BYTES {
            aligning.open_pes = None;
        }
        None
    }

    fn is_keyframe(&self, view: TsPacket<'_>) -> bool {
        let Some((pid, stream_type)) = self.programs.video else {
            return false;
        };
        view.pid() == pid
            && view.payload_unit_start()
            && (view.random_access() || is_idr_start(stream_type, view.payload()))
    }

    /// Learns PAT/PMT structure from `view`; returns true when it was a PMT of a known program.
    fn observe(&mut self, view: TsPacket<'_>) -> bool {
        if !view.payload_unit_start() {
            return false;
        }
        let pid = view.pid();
        let is_pmt = self.programs.pmt_pids.contains_key(&pid);
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
            if self.programs.pat_crc == Some(crc) {
                return false;
            }
            self.programs.pat_crc = Some(crc);
            self.programs.pmt_pids = entries
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
            self.programs.video = None;
            while streams.len() >= 5 {
                let stream_type = streams[0];
                let es_pid = pid_from_bytes(streams[1], streams[2]);
                let es_info_len = (usize::from(streams[3] & 0x0F) << 8) | usize::from(streams[4]);
                if self.programs.video.is_none() && is_video_stream_type(stream_type) {
                    self.programs.video = Some((es_pid, stream_type));
                }
                streams = streams.get(5 + es_info_len..).unwrap_or(&[]);
            }
            return true;
        }
        false
    }

    /// Observes packets about to be emitted and applies the version rewrite and discontinuity
    /// flags of a member after the first. Copies only when a packet changes.
    fn process_emitted(&mut self, packets: Bytes) -> Bytes {
        let rewrite = self.members > 1;
        let mut edits: Vec<PacketEdit> = Vec::new();
        for (index, packet) in packets.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
            let view = TsPacket(packet);
            self.observe(view);
            let offset = index * TS_PACKET_SIZE;
            let pid = view.pid();
            if rewrite && self.discontinuity_seen.insert(pid) && view.adaptation_len().is_some_and(|len| len >= 1) {
                edits.push(PacketEdit::Discontinuity { offset });
            }
            if let Some(version) = self.emitted_version(view, rewrite) {
                if let Some(payload_offset) = view.payload_offset() {
                    edits.push(PacketEdit::Version {
                        payload: offset + payload_offset..offset + TS_PACKET_SIZE,
                        version,
                    });
                }
            }
        }
        if edits.is_empty() {
            return packets;
        }
        // In place when the chunk is ours alone, a copy only when it is shared.
        let mut buffer = packets.try_into_mut().unwrap_or_else(|shared| BytesMut::from(&shared[..]));
        for edit in edits {
            match edit {
                PacketEdit::Discontinuity { offset } => buffer[offset + 5] |= 0x80,
                PacketEdit::Version { payload, version } => {
                    let payload = &mut buffer[payload];
                    if let Some(range) = single_packet_section(payload) {
                        rewrite_section_version(&mut payload[range], version);
                    }
                }
            }
        }
        buffer.freeze()
    }

    /// Records the version a PSI section in `view` is emitted with; returns it when it differs from
    /// the source version and the packet has to be rewritten.
    fn emitted_version(&mut self, view: TsPacket<'_>, rewrite: bool) -> Option<u8> {
        if !view.payload_unit_start() {
            return None;
        }
        let pid = view.pid();
        let program = if pid == PAT_PID { None } else { Some(*self.programs.pmt_pids.get(&pid)?) };
        let payload = view.payload();
        let range = single_packet_section(payload)?;
        let section = &payload[range];
        let expected_table = if program.is_some() { PMT_TABLE_ID } else { PAT_TABLE_ID };
        if section[0] != expected_table {
            return None;
        }
        let source = section_version(section);
        let (offset, last_emitted) = match program {
            None => (&mut self.offsets.pat, &mut self.emitted_pat_version),
            Some(program) => {
                let last = self.emitted_pmt_versions.get(&program).copied();
                let offset = self.offsets.pmt.entry(program).or_insert_with(|| {
                    if rewrite {
                        last.map_or(0, |last| (last + 1 + 32 - source) % 32)
                    } else {
                        0
                    }
                });
                let version = (source + *offset) % 32;
                self.emitted_pmt_versions.insert(program, version);
                return (version != source).then_some(version);
            }
        };
        let offset = *offset.get_or_insert_with(|| {
            if rewrite {
                last_emitted.map_or(0, |last| (last + 1 + 32 - source) % 32)
            } else {
                0
            }
        });
        let version = (source + offset) % 32;
        *last_emitted = Some(version);
        (version != source).then_some(version)
    }
}
