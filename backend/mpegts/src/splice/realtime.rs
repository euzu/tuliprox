use crate::{
    packet::{TsPacket, TS_PACKET_SIZE},
    transport_stream_buffer::decode_pcr,
};
use std::time::{Duration, Instant};

const PCR_HZ: u64 = 27_000_000;
/// PCR wraps at 2^33 * 300.
const PCR_MODULUS: u64 = (1 << 33) * 300;
const BUCKET: Duration = Duration::from_secs(1);
/// A bucket in which the consumer waited less than this never counts as an underrun: the
/// provider delivered faster than the consumer read.
const MIN_BUCKET_WAIT: Duration = Duration::from_millis(100);

/// Detects a provider that delivers media slower than real time, from the PCR advance against the
/// time the consumer waited for data.
pub struct RealtimeMeter {
    ratio_pct: u8,
    window: Duration,
    pcr_pid: Option<u16>,
    last_pcr: Option<u64>,
    bucket_start: Option<Instant>,
    bucket_media: Duration,
    bucket_waited: Duration,
    streak_start: Option<Instant>,
}

impl RealtimeMeter {
    #[must_use]
    pub fn new(ratio_pct: u8, window: Duration) -> Self {
        Self {
            ratio_pct,
            window,
            pcr_pid: None,
            last_pcr: None,
            bucket_start: None,
            bucket_media: Duration::ZERO,
            bucket_waited: Duration::ZERO,
            streak_start: None,
        }
    }

    /// `packets` are whole TS packets just delivered, `waited` the time the consumer waited for
    /// them. Returns true once media arrived slower than `ratio_pct` of real time for `window`.
    pub fn observe(&mut self, packets: &[u8], waited: Duration, now: Instant) -> bool {
        if self.ratio_pct == 0 {
            return false;
        }
        for packet in packets.as_chunks::<TS_PACKET_SIZE>().0 {
            self.observe_pcr(TsPacket(packet), now);
        }
        if self.pcr_pid.is_none() {
            return false;
        }
        self.bucket_waited += waited;
        let bucket_start = *self.bucket_start.get_or_insert(now);
        if now.saturating_duration_since(bucket_start) >= BUCKET {
            let slow = self.bucket_waited >= MIN_BUCKET_WAIT
                && self.bucket_media.as_micros() * 100 < u128::from(self.ratio_pct) * self.bucket_waited.as_micros();
            if slow {
                self.streak_start.get_or_insert(bucket_start);
            } else {
                self.streak_start = None;
            }
            self.bucket_start = Some(now);
            self.bucket_media = Duration::ZERO;
            self.bucket_waited = Duration::ZERO;
        }
        self.streak_start.is_some_and(|start| now.saturating_duration_since(start) >= self.window)
    }

    /// Starts measuring a new member: its PCR may run on another PID than the previous one's.
    pub fn begin_member(&mut self, now: Instant) {
        self.pcr_pid = None;
        self.reset(now);
    }

    pub fn reset(&mut self, now: Instant) {
        self.last_pcr = None;
        self.bucket_start = Some(now);
        self.bucket_media = Duration::ZERO;
        self.bucket_waited = Duration::ZERO;
        self.streak_start = None;
    }

    fn observe_pcr(&mut self, packet: TsPacket<'_>, now: Instant) {
        if !packet.has_pcr() {
            return;
        }
        let pid = packet.pid();
        if *self.pcr_pid.get_or_insert(pid) != pid {
            return;
        }
        let pcr = decode_pcr(&packet.0[6..12]);
        let Some(last) = self.last_pcr.replace(pcr) else { return };
        let delta = (pcr + PCR_MODULUS - last) % PCR_MODULUS;
        if delta > PCR_HZ {
            // A jump forward or backwards is a new timeline, not delivery speed.
            self.reset(now);
            self.last_pcr = Some(pcr);
            return;
        }
        self.bucket_media += Duration::from_nanos(delta * 1_000 / 27);
    }
}
