use crate::{packet::TsPacket, transport_stream_buffer::decode_pcr};
use std::time::Duration;

pub(crate) const PCR_HZ: u64 = 27_000_000;
/// PCR wraps at 2^33 * 300.
pub(crate) const PCR_MODULUS: u64 = (1 << 33) * 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcrProgress {
    /// Packet did not contain a PCR on the bound PCR PID.
    None,
    /// First PCR seen on the bound PID; establishes the initial clock baseline.
    First { pcr: u64 },
    /// Normal PCR advance within the 1-second limit.
    Advance { delta: Duration, media_time: Duration },
    /// Discontinuity (> 1 s forward or backward); timeline was reset to 0 with this PCR as new baseline.
    Discontinuity { pcr: u64 },
}

/// Tracks the PCR clock timeline across packets for a single stream or member.
///
/// Binds to the first PID carrying a PCR. Computes wrap-safe deltas against `PCR_MODULUS`.
/// If a delta exceeds 1 second (`PCR_HZ`), it treats the jump as a discontinuity, resetting
/// the media timeline to zero and starting fresh from the new PCR value.
pub(crate) struct PcrClock {
    pcr_pid: Option<u16>,
    last_pcr: Option<u64>,
    media_time: Duration,
}

impl Default for PcrClock {
    fn default() -> Self { Self::new() }
}

impl PcrClock {
    #[must_use]
    pub(crate) fn new() -> Self { Self { pcr_pid: None, last_pcr: None, media_time: Duration::ZERO } }

    /// Observes one packet. If it carries a PCR on the bound PID, advances or resets the clock.
    pub(crate) fn observe(&mut self, packet: TsPacket<'_>) -> PcrProgress {
        if !packet.has_pcr() {
            return PcrProgress::None;
        }
        let pid = packet.pid();
        if *self.pcr_pid.get_or_insert(pid) != pid {
            return PcrProgress::None;
        }
        let Some(pcr_bytes) = packet.0.get(6..12) else {
            return PcrProgress::None;
        };
        let pcr = decode_pcr(pcr_bytes);
        let Some(last) = self.last_pcr.replace(pcr) else {
            return PcrProgress::First { pcr };
        };
        let delta = (pcr + PCR_MODULUS - last) % PCR_MODULUS;
        if delta > PCR_HZ {
            // A jump forward or backward is a new timeline.
            self.media_time = Duration::ZERO;
            self.last_pcr = Some(pcr);
            return PcrProgress::Discontinuity { pcr };
        }
        let delta_dur = Duration::from_nanos(delta * 1_000 / 27);
        self.media_time += delta_dur;
        PcrProgress::Advance { delta: delta_dur, media_time: self.media_time }
    }

    #[must_use]
    pub(crate) fn has_pcr_pid(&self) -> bool { self.pcr_pid.is_some() }

    #[must_use]
    pub(crate) fn media_time(&self) -> Duration { self.media_time }

    pub(crate) fn reset(&mut self) {
        self.last_pcr = None;
        self.media_time = Duration::ZERO;
    }

    pub(crate) fn begin_member(&mut self) {
        self.pcr_pid = None;
        self.reset();
    }
}
