//! A clock seam, so "what time is it" can be a dependency rather than a global.
//!
//! # Why a trait at all
//!
//! `fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis()
//! .try_into().unwrap_or_default() }` was duplicated **character for character
//! in 14 files** across `backend/app` and `backend/hls`. This module holds the
//! single copy.
//!
//! # Why a generic parameter and not `Arc<dyn Clock>`
//!
//! [`SystemClock`] is zero-sized. A struct that owns one is exactly the size it
//! was before, `now_ms()` inlines to the same instructions the direct call
//! emitted, and there is no vtable and no allocation anywhere in the production
//! path. Hold it as `C: Clock` with `SystemClock` as the default type
//! parameter — `struct Deadlines<C: Clock = SystemClock> { clock: C }` — so
//! existing construction sites keep compiling and only tests name the other
//! implementor. The `Arc` in [`ManualClock`] is confined to the test-support
//! type, which is the point.
//!
//! # What deliberately does not use this
//!
//! Most of the deadline logic in `tuliprox-hls` already takes `now_ms: u64` as a
//! function parameter — `HlsAccessLease`, `HlsAvailabilityReevaluationCycle` and
//! their neighbours are already injectable and already tested that way. Passing
//! the instant in is a better pattern than reaching for a clock, so those are
//! left alone; this trait is for the callers that have to *produce* the instant.
//!
//! `HlsTerminalCommitClock` is also left alone. It fakes time with an
//! `AtomicU64` sentinel, which costs an atomic load per read in production and
//! would be a natural fit here, but it is owned by `HlsProxy` — so making it
//! generic would push a type parameter onto `HlsProxy` and from there onto
//! `AppState`. That trade is not worth it, and a type parameter that infects the
//! root state is exactly the case where the enum or the status quo wins.

use shared::model::Millis;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Milliseconds since the Unix epoch.
///
/// Saturates to `0` rather than panicking on a pre-epoch or out-of-range clock,
/// matching the behaviour of the 14 copies this replaces.
#[inline]
#[must_use]
pub fn current_time_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default()
}

/// Reads wall-clock time.
///
/// Implementors are held as a generic parameter; see the module docs for why
/// this is never a trait object.
pub trait Clock: Clone + Send + Sync + 'static {
    fn now_ms(&self) -> Millis;
}

/// The real clock. Zero-sized: no field, no vtable, no allocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now_ms(&self) -> Millis {
        Millis::new(current_time_millis())
    }
}

/// A clock the caller sets, for tests that need time to be deterministic.
///
/// Cloning shares the underlying instant, so a clone handed to the code under
/// test observes advances made through the original.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now_ms)))
    }

    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::Release);
    }

    /// Move time forward, saturating rather than wrapping.
    pub fn advance(&self, delta_ms: u64) {
        self.0.fetch_update(Ordering::Release, Ordering::Acquire, |now| Some(now.saturating_add(delta_ms))).ok();
    }
}

impl Clock for ManualClock {
    #[inline]
    fn now_ms(&self) -> Millis {
        Millis::new(self.0.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, ManualClock, SystemClock};
    use shared::model::Millis;

    /// The shape production code is expected to use: a generic parameter
    /// defaulted to the ZST, so existing construction sites keep compiling.
    #[allow(dead_code)]
    struct Deadlines<C: Clock = SystemClock> {
        clock: C,
        budget_ms: u64,
    }

    #[test]
    fn system_clock_is_zero_sized_so_owning_one_costs_nothing() {
        assert_eq!(size_of::<SystemClock>(), 0);
        // The whole point: adding the clock did not change the layout.
        assert_eq!(size_of::<Deadlines<SystemClock>>(), size_of::<u64>());
    }

    #[test]
    fn system_clock_reports_a_plausible_epoch_millisecond() {
        // 2020-01-01 in ms; a sanity floor rather than a precise assertion.
        assert!(SystemClock.now_ms() > Millis::new(1_577_836_800_000));
    }

    #[test]
    fn manual_clock_is_deterministic_and_shared_across_clones() {
        let clock = ManualClock::new(1_000);
        let handed_to_code_under_test = clock.clone();
        assert_eq!(handed_to_code_under_test.now_ms(), Millis::new(1_000));

        clock.advance(500);
        assert_eq!(handed_to_code_under_test.now_ms(), Millis::new(1_500));

        clock.set(42);
        assert_eq!(handed_to_code_under_test.now_ms(), Millis::new(42));
    }

    #[test]
    fn manual_clock_advance_saturates() {
        let clock = ManualClock::new(u64::MAX - 1);
        clock.advance(10);
        assert_eq!(clock.now_ms(), Millis::new(u64::MAX));
    }
}
