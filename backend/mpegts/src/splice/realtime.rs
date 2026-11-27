use super::pcr_clock::{PcrClock, PcrProgress};
use crate::packet::{TsPacket, TS_PACKET_SIZE};
use std::time::{Duration, Instant};

const BUCKET: Duration = Duration::from_secs(1);
/// A bucket in which the consumer waited less than this never counts as an underrun: the
/// provider delivered faster than the consumer read.
const MIN_BUCKET_WAIT: Duration = Duration::from_millis(100);

/// Detects a provider that delivers media slower than real time, from the PCR advance against the
/// time the consumer waited for data.
pub struct RealtimeMeter {
    ratio_pct: u8,
    window: Duration,
    clock: PcrClock,
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
            clock: PcrClock::new(),
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
            match self.clock.observe(TsPacket(packet)) {
                PcrProgress::Advance { delta, .. } => {
                    self.bucket_media += delta;
                }
                PcrProgress::Discontinuity { .. } => {
                    self.reset_bucket(now);
                }
                PcrProgress::None | PcrProgress::First { .. } => {}
            }
        }
        if !self.clock.has_pcr_pid() {
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
        self.clock.begin_member();
        self.reset_bucket(now);
    }

    pub fn reset(&mut self, now: Instant) {
        self.clock.reset();
        self.reset_bucket(now);
    }

    fn reset_bucket(&mut self, now: Instant) {
        self.bucket_start = Some(now);
        self.bucket_media = Duration::ZERO;
        self.bucket_waited = Duration::ZERO;
        self.streak_start = None;
    }
}
