use super::{
    clock::{
        add_pts_dts_offset, decode_pcr, decode_pcr_at_location, decode_timestamp, decode_timestamp_at_location,
        forward_clock_distance_90khz, HlsTsTimestampKind, HLS_TS_PROFILE_MIN_TOLERANCE_TICKS_90KHZ,
        HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ, MAX_PTS_DTS, TS_PACKET_SIZE,
    },
    pes::inspect_hls_ts_packet,
    HlsFiniteTsLayout, HlsPesHeaderAssembler, HlsTsTimestampProfileAccumulator, HlsTsTimestampProfileScanner,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsTsTimestampProfile {
    pub first_clock_90khz: u64,
    pub last_clock_90khz: u64,
    pub span_ticks_90khz: u64,
    pub observed_pts_or_dts: bool,
    pub observed_pcr: bool,
}

/// MPEG-TS live-to-terminal timestamp anchor expressed in 90 kHz clock ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsTsSpliceAnchor {
    pub live_last_clock: u64,
    pub terminal_first_clock: u64,
    pub timestamp_delta_ticks: u64,
}

impl HlsTsSpliceAnchor {
    pub fn between(live_tail: HlsTsTimestampProfile, terminal_asset: HlsTsTimestampProfile) -> Option<Self> {
        if !timestamp_profile_is_coherent(live_tail) || !timestamp_profile_is_coherent(terminal_asset) {
            return None;
        }
        let terminal_first_clock_90khz =
            add_pts_dts_offset(live_tail.last_clock_90khz, HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ);
        let timestamp_delta_ticks_90khz =
            terminal_first_clock_90khz.wrapping_add(MAX_PTS_DTS).wrapping_sub(terminal_asset.first_clock_90khz)
                % MAX_PTS_DTS;
        let rebased_first = add_pts_dts_offset(terminal_asset.first_clock_90khz, timestamp_delta_ticks_90khz);
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
        && forward_clock_distance_90khz(profile.first_clock_90khz, profile.last_clock_90khz) == profile.span_ticks_90khz
}

impl HlsTsTimestampProfileAccumulator {
    pub(super) fn new(expected_duration_ticks_90khz: u64) -> Self {
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

    pub(super) fn observe_clock(&mut self, clock_90khz: u64, kind: HlsTsTimestampKind) {
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

    pub(super) fn finish(self) -> Option<HlsTsTimestampProfile> {
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
            u64::try_from((i128::from(reference) + i128::from(self.earliest_relative_ticks)).rem_euclid(cycle)).ok()?;
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

impl HlsTsTimestampProfileScanner {
    pub fn new(expected_duration_ticks_90khz: u64) -> Self {
        Self {
            assembler: HlsPesHeaderAssembler::new(),
            accumulator: HlsTsTimestampProfileAccumulator::new(expected_duration_ticks_90khz),
            next_packet_start: 0,
            invalid: false,
        }
    }

    pub fn push_aligned_packet(&mut self, packet: &[u8]) {
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
            self.accumulator.observe_clock(decode_pcr(bytes) / 300, HlsTsTimestampKind::Pcr);
        }
        let Ok(completed) = self.assembler.push_packet(packet, packet_start, evidence) else {
            self.invalid = true;
            return;
        };
        for field in completed.fields.into_iter().flatten() {
            self.accumulator.observe_clock(decode_timestamp(&field.bytes), HlsTsTimestampKind::PtsOrDts);
        }
    }

    pub fn finish(self) -> Option<HlsTsTimestampProfile> {
        if self.invalid || self.assembler.finish().is_err() {
            return None;
        }
        self.accumulator.finish()
    }
}

pub(super) fn timestamp_profile_from_finite_layout(
    buffer: &[u8],
    layout: &HlsFiniteTsLayout,
    expected_duration_ticks_90khz: u64,
) -> Option<HlsTsTimestampProfile> {
    let mut accumulator = HlsTsTimestampProfileAccumulator::new(expected_duration_ticks_90khz);
    for location in layout.timestamp_fields.iter().copied() {
        accumulator.observe_clock(decode_timestamp_at_location(buffer, location).ok()?, HlsTsTimestampKind::PtsOrDts);
    }
    for location in layout.pcr_fields.iter().copied() {
        accumulator.observe_clock(decode_pcr_at_location(buffer, location).ok()? / 300, HlsTsTimestampKind::Pcr);
    }
    accumulator.finish()
}
