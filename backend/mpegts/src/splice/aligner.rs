use super::{
    live_edge::{TrimOutput, TsLiveEdgeTrim},
    pcr_clock::{PcrClock, PcrProgress},
    program_map::{ProgramMap, PAT_PID, PAT_TABLE_ID, PMT_TABLE_ID},
};
use crate::packet::{
    es_has_keyframe, is_idr_start, pes_elementary_data, rewrite_section_version, section_version,
    single_packet_section, ts_sync_offset, TsPacket, KEYFRAME_SCAN_BYTES, TS_PACKET_SIZE, TS_SYNC_BYTE,
};
use bytes::{Buf, Bytes, BytesMut};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

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

/// Version rewrite of one member: per table, the offset added to the source version.
#[derive(Default)]
struct VersionOffsets {
    pat: Option<u8>,
    pmt: HashMap<u16, u8>,
}

struct FirstRapState {
    started: Instant,
    buffer: BytesMut,
    first_pat: Option<usize>,
    last_pat: Option<usize>,
    pmt_after_last_pat: bool,
    /// Video PES whose start packet showed no keyframe yet: the PAT before its start and the
    /// elementary data read so far, searched on as its next packets arrive.
    open_pes: Option<(usize, Vec<u8>)>,
}

impl FirstRapState {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            buffer: BytesMut::new(),
            first_pat: None,
            last_pat: None,
            pmt_after_last_pat: false,
            open_pes: None,
        }
    }
}

enum Aligning {
    FirstRap(FirstRapState),
    Trim(TsLiveEdgeTrim),
}

/// Aligns provider bytes to whole TS packets and splices a new member into the output: after
/// `begin_member` the new member is held back until it can start at a PAT/PMT followed by a
/// keyframe, its PSI versions continue the ones already emitted, and the first packet per PID
/// announces the discontinuity.
pub struct TsSpliceAligner {
    carry: BytesMut,
    budget_bytes: usize,
    first_rap_budget: Duration,
    trim_budget: Duration,
    trim_target: Duration,
    programs: ProgramMap,
    aligning: Option<Aligning>,
    /// Number of members started; the first one is never rewritten.
    members: u32,
    offsets: VersionOffsets,
    emitted_pat_version: Option<u8>,
    emitted_pmt_versions: HashMap<u16, u8>,
    discontinuity_seen: PidSet,
    buffer: Duration,
    last_update: Option<Instant>,
    emitted_clock: PcrClock,
    new_member_pending: bool,
}

impl TsSpliceAligner {
    #[must_use]
    pub fn new(budget_bytes: usize, first_rap_budget: Duration, trim_budget: Duration, trim_target: Duration) -> Self {
        let aligning = (!trim_target.is_zero())
            .then(|| Aligning::Trim(TsLiveEdgeTrim::new(trim_target, trim_budget, budget_bytes)));
        Self {
            carry: BytesMut::new(),
            budget_bytes,
            first_rap_budget,
            trim_budget,
            trim_target,
            programs: ProgramMap::default(),
            aligning,
            members: 1,
            offsets: VersionOffsets::default(),
            emitted_pat_version: None,
            emitted_pmt_versions: HashMap::new(),
            discontinuity_seen: PidSet::new(),
            buffer: Duration::ZERO,
            last_update: None,
            emitted_clock: PcrClock::new(),
            new_member_pending: false,
        }
    }

    /// Drains the estimated player buffer to `now` and returns the remaining duration.
    pub fn buffer_at(&mut self, now: Instant) -> Duration {
        if let Some(last) = self.last_update {
            let wall_delta = now.saturating_duration_since(last);
            self.buffer = self.buffer.saturating_sub(wall_delta);
        }
        self.last_update = Some(now);
        self.buffer
    }

    /// Wall-clock deadline for the running hold, or `None` if not aligning or hold has not started.
    #[must_use]
    pub fn hold_deadline(&self, now: Instant) -> Option<Instant> {
        match &self.aligning {
            Some(Aligning::FirstRap(state)) => Some(state.started + self.first_rap_budget),
            Some(Aligning::Trim(trim)) => trim.next_wake(now),
            None => None,
        }
    }

    /// Re-evaluates a running hold: returns held bytes if ready to emit, or `None`.
    pub fn poll_hold(&mut self, now: Instant) -> Option<Bytes> {
        match &self.aligning {
            Some(Aligning::Trim(_)) => self.push_aligning(Bytes::new(), now).filter(|b| !b.is_empty()),
            Some(Aligning::FirstRap(state)) => {
                if now.saturating_duration_since(state.started) >= self.first_rap_budget {
                    self.finish_at(now)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Starts the next member: its bytes are held until the splice point.
    pub fn begin_member(&mut self, now: Instant) {
        self.carry.clear();
        self.programs.reset();
        self.offsets = VersionOffsets::default();
        self.discontinuity_seen.clear();
        self.members += 1;
        self.new_member_pending = true;
        let buffered = self.buffer_at(now);
        self.aligning = Some(if self.trim_target.is_zero() {
            Aligning::FirstRap(FirstRapState::new(now))
        } else {
            let mut trim = TsLiveEdgeTrim::new(self.trim_target, self.trim_budget, self.budget_bytes);
            trim.set_player_buffer(buffered, now);
            Aligning::Trim(trim)
        });
    }

    #[must_use]
    pub fn is_aligning(&self) -> bool { self.aligning.is_some() }

    /// Ends a running hold: returns the held bytes from the best start point, or `None` if nothing is held.
    /// After this call aligning is finished and future packets pass through.
    pub fn finish(&mut self) -> Option<Bytes> { self.finish_at(Instant::now()) }

    /// Ends a running hold at `now`: returns the held bytes from the best start point, or `None` if nothing is held.
    /// After this call aligning is finished and future packets pass through.
    pub fn finish_at(&mut self, now: Instant) -> Option<Bytes> {
        let aligning = self.aligning.take()?;
        let output = match aligning {
            Aligning::Trim(mut trim) => trim.finish_at(now)?,
            Aligning::FirstRap(mut state) => {
                let start = state.first_pat?.min(state.buffer.len());
                let mut buffer = std::mem::take(&mut state.buffer);
                if buffer.is_empty() {
                    return None;
                }
                buffer.advance(start);
                buffer.freeze()
            }
        };
        if output.is_empty() {
            return None;
        }
        Some(self.complete_aligning(output, now))
    }

    pub fn push(&mut self, chunk: Bytes, now: Instant) -> SpliceOutput {
        let (head, body) = self.whole_packets(chunk);
        let head = head.and_then(|head| self.emit_packets(head, now));
        let body = self.emit_packets(body, now);
        SpliceOutput::of(head, body)
    }

    fn emit_packets(&mut self, packets: Bytes, now: Instant) -> Option<Bytes> {
        if self.aligning.is_some() {
            return self.push_aligning(packets, now);
        }
        (!packets.is_empty()).then(|| self.process_emitted(packets, now))
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

    fn push_aligning(&mut self, packets: Bytes, now: Instant) -> Option<Bytes> {
        let mut aligning = self.aligning.take()?;
        let output = match &mut aligning {
            Aligning::FirstRap(state) => {
                let mut start = None;
                for packet in packets.as_chunks::<TS_PACKET_SIZE>().0 {
                    let position = state.buffer.len();
                    state.buffer.extend_from_slice(packet);
                    if start.is_some() {
                        continue;
                    }
                    let view = TsPacket(packet);
                    if view.pid() == PAT_PID && view.payload_unit_start() {
                        state.first_pat.get_or_insert(position);
                        state.last_pat = Some(position);
                        state.pmt_after_last_pat = false;
                    }
                    if self.programs.observe(view) && state.last_pat.is_some() {
                        if self.programs.video().is_none() {
                            state.pmt_after_last_pat = true;
                            start = state.last_pat;
                            continue;
                        }
                        if self.programs.video_pmt() == Some(view.pid()) {
                            state.pmt_after_last_pat = true;
                        }
                    }
                    if !state.pmt_after_last_pat {
                        continue;
                    }
                    if self.is_keyframe(view) {
                        start = state.last_pat;
                    } else if let Some(pat) = self.continue_open_pes(state, view) {
                        start = Some(pat);
                    }
                }
                let over_budget = state.buffer.len() > self.budget_bytes
                    || now.saturating_duration_since(state.started) >= self.first_rap_budget;
                let start = match start {
                    Some(start) => start,
                    None if over_budget => state.first_pat.unwrap_or(0),
                    None => {
                        self.aligning = Some(aligning);
                        return None;
                    }
                };
                // Dropping the skipped head keeps the buffer the only owner, so the PSI rewrite below
                // edits it in place instead of copying it.
                let mut buffer = std::mem::take(&mut state.buffer);
                buffer.advance(start);
                buffer.freeze()
            }
            Aligning::Trim(trim) => match trim.push(packets, now) {
                TrimOutput::Hold => {
                    self.aligning = Some(aligning);
                    return None;
                }
                TrimOutput::Emit(output) => output,
            },
        };
        Some(self.complete_aligning(output, now))
    }

    fn complete_aligning(&mut self, output: Bytes, now: Instant) -> Bytes {
        // The emitted part is observed again from its start on for versions and discontinuity.
        self.programs.reset();
        self.process_emitted(output, now)
    }

    /// Follows the elementary data of a video PES across its packets; returns the PAT before the
    /// PES start once a keyframe shows up within its first bytes.
    fn continue_open_pes(&self, state: &mut FirstRapState, view: TsPacket<'_>) -> Option<usize> {
        let (pid, stream_type) = self.programs.video()?;
        if view.pid() != pid {
            return None;
        }
        if view.payload_unit_start() {
            // A new PES without keyframe in its start packet: its next packets may hold it.
            state.open_pes = state.last_pat.zip(pes_elementary_data(view.payload()).map(<[u8]>::to_vec));
            return None;
        }
        let (pat, data) = state.open_pes.as_mut()?;
        let room = KEYFRAME_SCAN_BYTES.saturating_sub(data.len());
        let payload = view.payload();
        data.extend_from_slice(&payload[..payload.len().min(room)]);
        if es_has_keyframe(stream_type, data) {
            let pat = *pat;
            state.open_pes = None;
            return Some(pat);
        }
        if data.len() >= KEYFRAME_SCAN_BYTES {
            state.open_pes = None;
        }
        None
    }

    fn is_keyframe(&self, view: TsPacket<'_>) -> bool {
        let Some((pid, stream_type)) = self.programs.video() else {
            return false;
        };
        view.pid() == pid
            && view.payload_unit_start()
            && (view.random_access() || is_idr_start(stream_type, view.payload()))
    }

    /// Observes packets about to be emitted and applies the version rewrite and discontinuity
    /// flags of a member after the first. Copies only when a packet changes.
    fn process_emitted(&mut self, packets: Bytes, now: Instant) -> Bytes {
        if self.new_member_pending {
            self.new_member_pending = false;
            self.emitted_clock.begin_member();
        }
        self.buffer_at(now);

        let rewrite = self.members > 1;
        let mut edits: Vec<PacketEdit> = Vec::new();
        let mut media_delta = Duration::ZERO;

        for (index, packet) in packets.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
            let view = TsPacket(packet);
            match self.emitted_clock.observe(view) {
                PcrProgress::Advance { delta, .. } => media_delta = media_delta.saturating_add(delta),
                PcrProgress::None | PcrProgress::First { .. } | PcrProgress::Discontinuity { .. } => {}
            }
            self.programs.observe(view);
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
        self.buffer = self.buffer.saturating_add(media_delta);

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
        let program = if pid == PAT_PID { None } else { Some(self.programs.program_of_pmt(pid)?) };
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
