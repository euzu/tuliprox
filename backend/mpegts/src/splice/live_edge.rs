use super::{
    pcr_clock::{PcrClock, PcrProgress},
    program_map::PAT_PID,
    rap_detector::RapDetector,
};
use crate::packet::{TsPacket, TS_PACKET_SIZE, TS_SYNC_BYTE};
use bytes::{Bytes, BytesMut};
use std::time::{Duration, Instant};

/// Observation window over which lead growth and media advance are checked to confirm the burst has settled.
const SETTLE_WINDOW: Duration = Duration::from_millis(400);

/// Maximum lead growth allowed within `SETTLE_WINDOW` for the stream to be considered settled into real time.
const SETTLE_GROWTH_MICROS: i64 = 100_000;

/// Minimum media progress required within `SETTLE_WINDOW` to confirm stream progress and prevent settling on a stall.
const SETTLE_MIN_MEDIA_ADVANCE: Duration = Duration::from_millis(300);

/// Interval between samples recorded in the settle ring buffer.
const SETTLE_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);

/// Capacity of the settle lead sample ring buffer.
const RING_CAP: usize = 32;

/// Capacity of the PAT stream position history ring buffer.
const PAT_RING_CAP: usize = 8;

/// Longest wall time since the first PCR the hold waits for a first keyframe after the burst is over. A stream
/// whose keyframes are not detectable (scrambled) starts at a PAT or PCR boundary after it.
const FIRST_RAP_WAIT: Duration = Duration::from_secs(2);

/// Wall-clock observation time since the first PCR required before releasing a non-bursting stream early.
const NO_BURST_PROBE: Duration = Duration::from_millis(300);

/// Maximum lead (media received minus wall time since the first PCR) allowed for early release.
/// If lead exceeds this at any time during the hold, early release is disqualified.
const NO_BURST_MAX_LEAD: Duration = Duration::from_millis(150);
const NO_BURST_MAX_LEAD_MICROS: i64 = 150_000;
const _: () = assert!(NO_BURST_MAX_LEAD.as_micros() == NO_BURST_MAX_LEAD_MICROS as u128);

/// Slack allowance for RAP freshness during switch holds. A candidate RAP whose media time is within
/// `effective_target + FRESH_RAP_SLACK` of the newest media time is considered fresh enough to emit.
pub const FRESH_RAP_SLACK: Duration = Duration::from_millis(500);

/// Waiting for a fresher RAP stops once the remaining player buffer drains to this floor, so the player keeps a
/// reserve against network jitter right after the splice.
pub const PLAYER_BUFFER_FLOOR: Duration = Duration::from_secs(1);

// Invariant: the sample ring buffer must cover the settle window plus margin.
// At 32 samples with a 50 ms interval, the ring spans 1600 ms, which exceeds the 400 ms settle window.
const _: () = assert!(RING_CAP as u128 * SETTLE_SAMPLE_INTERVAL.as_millis() >= SETTLE_WINDOW.as_millis());

fn compute_lead_micros(media_time: Duration, wall_elapsed: Duration) -> i64 {
    let media_micros = i64::try_from(media_time.as_micros()).unwrap_or(i64::MAX);
    let wall_micros = i64::try_from(wall_elapsed.as_micros()).unwrap_or(i64::MAX);
    media_micros.saturating_sub(wall_micros)
}

/// Output of pushing packets into `TsLiveEdgeTrim`.
#[derive(Debug, PartialEq, Eq)]
pub enum TrimOutput {
    /// Still holding the stream startup burst.
    Hold,
    /// Burst hold ended or pass-through: emitted MPEG-TS packets.
    Emit(Bytes),
}

/// Trims a startup burst by holding the start of a stream until it settles into real time,
/// then emitting from a random access point that is target media seconds behind the newest media.
pub struct TsLiveEdgeTrim {
    target: Duration,
    max_hold: Duration,
    max_bytes: usize,
    /// Player buffer and the instant it was sampled; it drains with wall time from then on.
    player_buffer: Option<(Duration, Instant)>,
    state: TrimState,
}

enum TrimState {
    Holding(Box<HoldingState>),
    PassThrough,
}

struct HoldingState {
    hold_start: Option<Instant>,
    buffer: BytesMut,
    dropped_bytes: u64,
    stream_pos: u64,
    clock: PcrClock,
    first_pcr_wall: Option<Instant>,
    detector: RapDetector,
    candidates: Vec<RapCandidate>,
    pcr_samples: Vec<PcrSample>,
    settle_tracker: SettleTracker,
    pat_history: PatHistory,
    lead_exceeded_no_burst: bool,
}

impl HoldingState {
    fn new() -> Self {
        Self {
            hold_start: None,
            buffer: BytesMut::new(),
            dropped_bytes: 0,
            stream_pos: 0,
            clock: PcrClock::new(),
            first_pcr_wall: None,
            detector: RapDetector::new(),
            candidates: Vec::new(),
            pcr_samples: Vec::new(),
            settle_tracker: SettleTracker::new(),
            pat_history: PatHistory::new(),
            lead_exceeded_no_burst: false,
        }
    }

    fn process_packets(&mut self, packets: &[u8], now: Instant) {
        let chunk_start_stream_pos = self.stream_pos;
        self.buffer.extend_from_slice(packets);

        for (idx, packet_bytes) in packets.as_chunks::<TS_PACKET_SIZE>().0.iter().enumerate() {
            let packet = TsPacket(packet_bytes);
            let packet_stream_pos = chunk_start_stream_pos.saturating_add((idx * TS_PACKET_SIZE) as u64);
            let packet_offset = usize::try_from(packet_stream_pos.saturating_sub(self.dropped_bytes))
                .unwrap_or(self.buffer.len())
                .min(self.buffer.len());

            match self.clock.observe(packet) {
                PcrProgress::First { .. } => {
                    self.first_pcr_wall = Some(now);
                    self.pcr_samples.push(PcrSample { offset: packet_offset, media_time: Duration::ZERO });
                }
                PcrProgress::Advance { media_time, .. } => {
                    self.pcr_samples.push(PcrSample { offset: packet_offset, media_time });
                }
                PcrProgress::Discontinuity { .. } => {
                    if let Some(first_wall) = self.first_pcr_wall {
                        let wall_elapsed = now.saturating_duration_since(first_wall);
                        let lead_micros = compute_lead_micros(self.clock.media_time(), wall_elapsed);
                        if lead_micros > NO_BURST_MAX_LEAD_MICROS {
                            self.lead_exceeded_no_burst = true;
                        }
                    }
                    self.candidates.clear();
                    self.pcr_samples.clear();
                    self.pcr_samples.push(PcrSample { offset: packet_offset, media_time: Duration::ZERO });
                    self.settle_tracker.reset();
                    self.first_pcr_wall = Some(now);
                    self.pat_history.reset();
                    self.detector.reset();
                }
                PcrProgress::None => {}
            }

            if packet.pid() == PAT_PID && packet.payload_unit_start() {
                self.pat_history.record(packet_stream_pos, self.clock.media_time());
            }

            if let Some(pat_stream_pos) = self.detector.observe_packet(packet, packet_stream_pos) {
                let media_time = self.pat_history.find(pat_stream_pos).unwrap_or_else(|| self.clock.media_time());
                let offset = usize::try_from(pat_stream_pos.saturating_sub(self.dropped_bytes))
                    .unwrap_or(self.buffer.len())
                    .min(self.buffer.len());
                self.candidates.push(RapCandidate { offset, media_time });
            }
        }

        self.stream_pos = self.stream_pos.saturating_add(u64::try_from(packets.len()).unwrap_or(0));

        if let Some(first_wall) = self.first_pcr_wall {
            let wall_elapsed = now.saturating_duration_since(first_wall);
            let media_time = self.clock.media_time();
            let lead_micros = compute_lead_micros(media_time, wall_elapsed);
            if lead_micros > NO_BURST_MAX_LEAD_MICROS {
                self.lead_exceeded_no_burst = true;
            }
            self.settle_tracker.record_lead(now, lead_micros, media_time);
        }
    }

    fn drop_to_offset(&mut self, drop_bytes: usize) {
        if drop_bytes == 0 {
            return;
        }
        let drop_bytes = drop_bytes.min(self.buffer.len());
        let _ = self.buffer.split_to(drop_bytes);
        self.dropped_bytes = self.dropped_bytes.saturating_add(drop_bytes as u64);
        self.candidates.retain_mut(|c| {
            if c.offset >= drop_bytes {
                c.offset -= drop_bytes;
                true
            } else {
                false
            }
        });
        self.pcr_samples.retain_mut(|s| {
            if s.offset >= drop_bytes {
                s.offset -= drop_bytes;
                true
            } else {
                false
            }
        });
    }

    /// Prefix drop during the hold uses `target` directly as an upper bound on retention.
    /// Because the player buffer drains over the hold (`effective_target = trim_target - remaining_player_buffer`),
    /// the effective target increases towards `trim_target` over time. Dropping with a smaller target
    /// would discard older media that a later, larger effective target at emit time requires.
    fn drop_prefix_if_needed(&mut self, target: Duration) {
        if !self.clock.has_pcr_pid() {
            return;
        }
        let newest_media_time = self.clock.media_time();
        if newest_media_time < target {
            return;
        }
        let threshold = newest_media_time.saturating_sub(target);
        let Some(drop_cand) = self.candidates.iter().rfind(|c| c.media_time <= threshold) else {
            return;
        };
        self.drop_to_offset(drop_cand.offset);
    }

    fn force_drop_on_cap(&mut self, target: Duration) {
        if !self.clock.has_pcr_pid() {
            if let Some(s) = self.pcr_samples.iter().rfind(|s| s.offset > 0).copied() {
                self.drop_to_offset(s.offset);
            }
            return;
        }
        let newest_media_time = self.clock.media_time();
        let target_cand = if newest_media_time >= target {
            let thresh = newest_media_time.saturating_sub(target);
            self.candidates.iter().rfind(|c| c.offset > 0 && c.media_time <= thresh).copied()
        } else {
            None
        };

        if let Some(c) = target_cand {
            self.drop_to_offset(c.offset);
        } else if let Some(c) = self.candidates.iter().rfind(|c| c.offset > 0).copied() {
            self.drop_to_offset(c.offset);
        } else if let Some(s) = self.pcr_samples.iter().rfind(|s| s.offset > 0).copied() {
            self.drop_to_offset(s.offset);
        }
    }

    fn check_settled(&self, now: Instant) -> bool {
        if !self.clock.has_pcr_pid() {
            return false;
        }
        let Some(first_wall) = self.first_pcr_wall else {
            return false;
        };
        let wall_elapsed = now.saturating_duration_since(first_wall);
        // Without a keyframe the start would land inside a GOP; wait for one a little longer.
        // Streams whose keyframes cannot be detected (scrambled) fall back after this wait.
        if self.candidates.is_empty() && wall_elapsed < FIRST_RAP_WAIT {
            return false;
        }
        let media_time = self.clock.media_time();
        let lead_micros = compute_lead_micros(media_time, wall_elapsed);
        self.settle_tracker.is_settled(now, lead_micros, media_time)
    }

    fn check_no_burst_settled(&self, now: Instant) -> bool {
        if !self.clock.has_pcr_pid() {
            return false;
        }
        let Some(first_wall) = self.first_pcr_wall else {
            return false;
        };
        let wall_elapsed = now.saturating_duration_since(first_wall);
        if wall_elapsed < NO_BURST_PROBE || self.lead_exceeded_no_burst {
            return false;
        }
        let media_time = self.clock.media_time();
        let lead_micros = compute_lead_micros(media_time, wall_elapsed);
        if lead_micros > NO_BURST_MAX_LEAD_MICROS {
            return false;
        }
        !self.candidates.is_empty()
    }

    fn pick_start_offset(&self, target: Duration) -> usize {
        if !self.clock.has_pcr_pid() {
            return 0;
        }
        let newest_media_time = self.clock.media_time();
        let threshold = newest_media_time.saturating_sub(target);

        if let Some(c) = self.candidates.iter().rfind(|c| c.media_time <= threshold) {
            return c.offset;
        }
        if let Some(oldest) = self.candidates.first() {
            return oldest.offset;
        }
        if let Some(offset) = self.pat_history.find_newest_before(threshold, self.dropped_bytes, self.buffer.len()) {
            return offset;
        }
        if let Some(offset) = self.pat_history.find_oldest_in_buffer(self.dropped_bytes, self.buffer.len()) {
            return offset;
        }
        if let Some(s) = self.pcr_samples.iter().rfind(|s| s.media_time <= threshold) {
            return s.offset;
        }
        if let Some(s) = self.pcr_samples.first() {
            return s.offset;
        }
        0
    }

    fn chosen_rap_media_time(&self, target: Duration) -> Option<Duration> {
        if !self.clock.has_pcr_pid() {
            return None;
        }
        let newest_media_time = self.clock.media_time();
        let threshold = newest_media_time.saturating_sub(target);

        if let Some(c) = self.candidates.iter().rfind(|c| c.media_time <= threshold) {
            return Some(c.media_time);
        }
        self.candidates.first().map(|c| c.media_time)
    }

    /// Evaluates whether the chosen RAP is fresh enough or waiting is no longer free.
    /// When no player buffer is set (stream start, first member), this returns true immediately.
    fn rap_fresh_or_drain_limit(
        &self,
        player_buffer: Option<(Duration, Instant)>,
        target: Duration,
        now: Instant,
    ) -> bool {
        let Some((buffer, sampled_at)) = player_buffer else {
            return true;
        };
        let remaining_buffer = buffer.saturating_sub(now.saturating_duration_since(sampled_at));
        if remaining_buffer <= PLAYER_BUFFER_FLOOR {
            return true;
        }
        let eff = target.saturating_sub(remaining_buffer);
        let Some(rap_media_time) = self.chosen_rap_media_time(eff) else {
            return true;
        };
        let newest_media_time = self.clock.media_time();
        let span = newest_media_time.saturating_sub(rap_media_time);
        span <= eff.saturating_add(FRESH_RAP_SLACK)
    }
}

#[derive(Clone, Copy, Debug)]
struct RapCandidate {
    offset: usize,
    media_time: Duration,
}

#[derive(Clone, Copy, Debug)]
struct PcrSample {
    offset: usize,
    media_time: Duration,
}

#[derive(Clone, Copy)]
struct LeadSample {
    wall: Instant,
    lead_micros: i64,
    media_time: Duration,
}

struct SettleTracker {
    samples: [Option<LeadSample>; RING_CAP],
    head: usize,
    len: usize,
}

impl SettleTracker {
    fn new() -> Self { Self { samples: [None; RING_CAP], head: 0, len: 0 } }

    fn reset(&mut self) {
        self.samples = [None; RING_CAP];
        self.head = 0;
        self.len = 0;
    }

    fn record_lead(&mut self, now: Instant, lead_micros: i64, media_time: Duration) {
        if self.len == 0 {
            self.samples[0] = Some(LeadSample { wall: now, lead_micros, media_time });
            self.head = 0;
            self.len = 1;
            return;
        }
        let last_idx = (self.head + self.len - 1) % RING_CAP;
        let Some(last) = self.samples.get(last_idx).copied().flatten() else {
            return;
        };
        if now.saturating_duration_since(last.wall) >= SETTLE_SAMPLE_INTERVAL {
            if self.len < RING_CAP {
                self.len += 1;
                let new_idx = (self.head + self.len - 1) % RING_CAP;
                if let Some(slot) = self.samples.get_mut(new_idx) {
                    *slot = Some(LeadSample { wall: now, lead_micros, media_time });
                }
            } else {
                self.head = (self.head + 1) % RING_CAP;
                let new_idx = (self.head + self.len - 1) % RING_CAP;
                if let Some(slot) = self.samples.get_mut(new_idx) {
                    *slot = Some(LeadSample { wall: now, lead_micros, media_time });
                }
            }
        } else if let Some(slot) = self.samples.get_mut(last_idx) {
            *slot = Some(LeadSample { wall: now, lead_micros, media_time });
        }
    }

    fn is_settled(&self, now: Instant, current_lead_micros: i64, current_media_time: Duration) -> bool {
        let mut best_sample: Option<LeadSample> = None;
        for i in 0..self.len {
            let idx = (self.head + i) % RING_CAP;
            if let Some(s) = self.samples.get(idx).copied().flatten() {
                if now.saturating_duration_since(s.wall) >= SETTLE_WINDOW {
                    best_sample = Some(s);
                }
            }
        }
        let Some(old) = best_sample else {
            return false;
        };
        let growth = current_lead_micros.saturating_sub(old.lead_micros);
        let media_advance = current_media_time.saturating_sub(old.media_time);
        growth < SETTLE_GROWTH_MICROS && media_advance >= SETTLE_MIN_MEDIA_ADVANCE
    }
}

struct PatHistory {
    entries: [Option<(u64, Duration)>; PAT_RING_CAP],
    head: usize,
    len: usize,
}

impl PatHistory {
    fn new() -> Self { Self { entries: [None; PAT_RING_CAP], head: 0, len: 0 } }

    fn reset(&mut self) {
        self.entries = [None; PAT_RING_CAP];
        self.head = 0;
        self.len = 0;
    }

    fn record(&mut self, pos: u64, media_time: Duration) {
        if self.len < PAT_RING_CAP {
            let idx = (self.head + self.len) % PAT_RING_CAP;
            if let Some(slot) = self.entries.get_mut(idx) {
                *slot = Some((pos, media_time));
                self.len += 1;
            }
        } else {
            if let Some(slot) = self.entries.get_mut(self.head) {
                *slot = Some((pos, media_time));
            }
            self.head = (self.head + 1) % PAT_RING_CAP;
        }
    }

    fn find(&self, pos: u64) -> Option<Duration> {
        for i in (0..self.len).rev() {
            let idx = (self.head + i) % PAT_RING_CAP;
            if let Some(Some((p, mt))) = self.entries.get(idx) {
                if *p == pos {
                    return Some(*mt);
                }
            }
        }
        None
    }

    fn find_newest_before(&self, threshold: Duration, dropped_bytes: u64, buf_len: usize) -> Option<usize> {
        for i in (0..self.len).rev() {
            let idx = (self.head + i) % PAT_RING_CAP;
            if let Some(Some((pos, mt))) = self.entries.get(idx) {
                if *pos >= dropped_bytes && *mt <= threshold {
                    let offset = usize::try_from(pos.saturating_sub(dropped_bytes)).unwrap_or(buf_len);
                    return Some(offset.min(buf_len));
                }
            }
        }
        None
    }

    fn find_oldest_in_buffer(&self, dropped_bytes: u64, buf_len: usize) -> Option<usize> {
        for i in 0..self.len {
            let idx = (self.head + i) % PAT_RING_CAP;
            if let Some(Some((pos, _))) = self.entries.get(idx) {
                if *pos >= dropped_bytes {
                    let offset = usize::try_from(pos.saturating_sub(dropped_bytes)).unwrap_or(buf_len);
                    return Some(offset.min(buf_len));
                }
            }
        }
        None
    }
}

impl TsLiveEdgeTrim {
    /// `target`: media seconds the start point stays behind the newest PCR. A target of zero requests the
    /// newest random access point. `max_hold`: wall-time limit of the hold. `max_bytes`: memory cap of the hold buffer.
    #[must_use]
    pub fn new(target: Duration, max_hold: Duration, max_bytes: usize) -> Self {
        Self {
            target,
            max_hold,
            max_bytes,
            player_buffer: None,
            state: TrimState::Holding(Box::new(HoldingState::new())),
        }
    }

    /// Creates an immediate pass-through trimmer that holds nothing and emits all packets as-is.
    #[must_use]
    pub fn passthrough() -> Self {
        Self {
            target: Duration::ZERO,
            max_hold: Duration::ZERO,
            max_bytes: 0,
            player_buffer: None,
            state: TrimState::PassThrough,
        }
    }

    /// Sets the player buffer sampled at `sampled_at`. At emit time the buffer is drained by the
    /// wall time since then, and the effective target is `target` minus what remains.
    pub fn set_player_buffer(&mut self, player_buffer: Duration, sampled_at: Instant) {
        self.player_buffer = Some((player_buffer, sampled_at));
    }

    /// Instant when the first packet of the hold arrived, or `None` if holding has not started or ended.
    #[must_use]
    pub fn hold_start(&self) -> Option<Instant> {
        let TrimState::Holding(holding) = &self.state else {
            return None;
        };
        holding.hold_start
    }

    /// Remaining player buffer at `now`, drained by wall elapsed since it was sampled.
    #[must_use]
    pub fn remaining_player_buffer(&self, now: Instant) -> Duration {
        self.player_buffer.map_or(Duration::ZERO, |(buffer, sampled_at)| {
            buffer.saturating_sub(now.saturating_duration_since(sampled_at))
        })
    }

    fn effective_target(&self, now: Instant) -> Duration {
        self.target.saturating_sub(self.remaining_player_buffer(now))
    }

    /// `packets`: whole TS packets (length multiple of 188, starting at a sync byte; the caller aligns).
    /// Returns `Emit` exactly once (the trimmed held bytes, possibly together with this call's packets), `Hold`
    /// before that. After the emit, every call returns `Emit(packets)` unchanged (pass-through, zero copy).
    pub fn push(&mut self, packets: Bytes, now: Instant) -> TrimOutput {
        let TrimState::Holding(holding) = &mut self.state else {
            return TrimOutput::Emit(packets);
        };

        let hold_start = *holding.hold_start.get_or_insert(now);

        if packets.is_empty() {
            let no_pcr_settled =
                !holding.clock.has_pcr_pid() && now.saturating_duration_since(hold_start) >= SETTLE_WINDOW;
            let timed_out = now.saturating_duration_since(hold_start) >= self.max_hold;
            let settled = holding.check_settled(now);
            let no_burst_settled = holding.check_no_burst_settled(now);
            let normal_settled = settled || no_burst_settled;
            let unconditional_end = timed_out || no_pcr_settled;
            if unconditional_end
                || (normal_settled && holding.rap_fresh_or_drain_limit(self.player_buffer, self.target, now))
            {
                return self.emit_held(now, no_burst_settled);
            }
            return TrimOutput::Hold;
        }

        // Non-TS input: first byte must be TS_SYNC_BYTE and length a multiple of 188.
        if packets[0] != TS_SYNC_BYTE || !packets.len().is_multiple_of(TS_PACKET_SIZE) {
            if holding.buffer.is_empty() {
                self.state = TrimState::PassThrough;
                return TrimOutput::Emit(packets);
            }
            holding.buffer.extend_from_slice(&packets);
            let out = std::mem::take(&mut holding.buffer).freeze();
            self.state = TrimState::PassThrough;
            return TrimOutput::Emit(out);
        }

        holding.process_packets(&packets, now);
        holding.drop_prefix_if_needed(self.target);

        if holding.buffer.len() > self.max_bytes {
            holding.force_drop_on_cap(self.target);
        }

        let no_pcr_settled = !holding.clock.has_pcr_pid() && now.saturating_duration_since(hold_start) >= SETTLE_WINDOW;
        let timed_out = now.saturating_duration_since(hold_start) >= self.max_hold;
        let still_memory_capped = holding.buffer.len() > self.max_bytes;
        let settled = holding.check_settled(now);
        let no_burst_settled = holding.check_no_burst_settled(now);
        let normal_settled = settled || no_burst_settled;
        let unconditional_end = timed_out || still_memory_capped || no_pcr_settled;
        let rap_fresh = holding.rap_fresh_or_drain_limit(self.player_buffer, self.target, now);

        if unconditional_end || (normal_settled && rap_fresh) {
            return self.emit_held(now, no_burst_settled);
        }

        TrimOutput::Hold
    }

    /// Ends the hold at `now`: returns the held bytes from the best start point,
    /// or `None` if nothing is held. After this call `push` is pass-through.
    pub fn finish_at(&mut self, now: Instant) -> Option<Bytes> {
        let TrimState::Holding(mut holding) = std::mem::replace(&mut self.state, TrimState::PassThrough) else {
            return None;
        };
        if holding.buffer.is_empty() {
            return None;
        }
        let effective_target = self.effective_target(now);
        let start_offset = holding.pick_start_offset(effective_target);
        let start_offset = start_offset.min(holding.buffer.len());
        let _ = holding.buffer.split_to(start_offset);
        Some(holding.buffer.freeze())
    }

    /// Ends the hold now (upstream ended or caller gives up): returns the held bytes from the best start point,
    /// or `None` if nothing is held. After this call `push` is pass-through.
    pub fn finish(&mut self) -> Option<Bytes> { self.finish_at(Instant::now()) }

    /// Whether the trimmer is currently holding stream startup bytes.
    #[must_use]
    pub fn is_holding(&self) -> bool { matches!(self.state, TrimState::Holding(_)) }

    #[cfg(test)]
    #[must_use]
    pub fn candidate_media_times(&self) -> Vec<(usize, Duration)> {
        let TrimState::Holding(holding) = &self.state else {
            return Vec::new();
        };
        holding.candidates.iter().map(|c| (c.offset, c.media_time)).collect()
    }

    fn emit_held(&mut self, now: Instant, no_burst_settled: bool) -> TrimOutput {
        let TrimState::Holding(mut holding) = std::mem::replace(&mut self.state, TrimState::PassThrough) else {
            return TrimOutput::Hold;
        };
        let effective_target = self.effective_target(now);
        let start_offset = if no_burst_settled && self.player_buffer.is_none() {
            holding.candidates.first().map_or_else(|| holding.pick_start_offset(effective_target), |c| c.offset)
        } else {
            holding.pick_start_offset(effective_target)
        };
        let start_offset = start_offset.min(holding.buffer.len());
        let _ = holding.buffer.split_to(start_offset);
        let out = holding.buffer.freeze();
        TrimOutput::Emit(out)
    }
}
