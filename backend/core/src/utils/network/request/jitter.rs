//! Jitter over a base delay, with the random source injected.
//!
//! Backoff without jitter makes every client that failed together retry together. These helpers spread
//! a delay out using the two schemes from the AWS "Exponential Backoff And Jitter" analysis:
//!
//! * **Full** jitter draws uniformly from `[0, delay]`. It decorrelates best and is the right default.
//! * **Equal** jitter keeps half the delay as a guaranteed floor and draws the other half, so the result
//!   is in `[delay / 2, delay]`. Use it when a minimum spacing between attempts matters.
//!
//! The source of randomness is a trait ([`JitterSource`]) so production uses `fastrand` and tests use a
//! fixed sequence. Nothing here sleeps and nothing reads a clock.
//!
//! `calculate_retry_backoff` itself stays deterministic. Jitter is opt-in through
//! `calculate_retry_backoff_with_jitter`, which applies it to the delay that function returns, after the
//! exponential growth and the cap. `OutboundExecutor` has no retry loop and does not use it.

use shared::model::Millis;

/// Where the random draw comes from.
///
/// `&self` rather than `&mut self` so one source can be shared by concurrent operations; implementors
/// that keep state use interior mutability.
pub trait JitterSource: Send + Sync {
    /// A value in `0..=upper_inclusive`.
    ///
    /// The caller clamps whatever is returned, so a faulty source cannot push a delay outside its bounds.
    fn next_up_to(&self, upper_inclusive: u64) -> u64;
}

/// Production source, backed by `fastrand`'s thread-local generator.
#[derive(Debug, Clone, Copy, Default)]
pub struct FastrandJitter;

impl JitterSource for FastrandJitter {
    #[inline]
    fn next_up_to(&self, upper_inclusive: u64) -> u64 { fastrand::u64(0..=upper_inclusive) }
}

/// How a base delay is spread out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JitterStrategy {
    /// Return the base delay unchanged.
    None,
    /// Uniform over `[0, delay]`.
    #[default]
    Full,
    /// `delay / 2` plus uniform over `[0, delay - delay / 2]`, so the result is in `[delay / 2, delay]`.
    Equal,
}

impl JitterStrategy {
    /// Apply this strategy to `delay`. The result never exceeds `delay`, so applying jitter after a
    /// maximum-delay cap cannot break the cap. Arithmetic cannot overflow, including for `u64::MAX`.
    pub fn apply(self, delay: Millis, source: &(impl JitterSource + ?Sized)) -> Millis {
        let delay_ms = delay.get();
        match self {
            Self::None => delay,
            Self::Full => Millis::new(source.next_up_to(delay_ms).min(delay_ms)),
            Self::Equal => {
                let floor = delay_ms / 2;
                let spread = delay_ms - floor;
                Millis::new(floor + source.next_up_to(spread).min(spread))
            }
        }
    }
}

/// Full jitter: uniform over `[0, delay]`.
pub fn full_jitter(delay: Millis, source: &(impl JitterSource + ?Sized)) -> Millis {
    JitterStrategy::Full.apply(delay, source)
}

/// Equal jitter: `[delay / 2, delay]`.
pub fn equal_jitter(delay: Millis, source: &(impl JitterSource + ?Sized)) -> Millis {
    JitterStrategy::Equal.apply(delay, source)
}

/// A source that replays a fixed sequence of draws, cycling at the end. Test support.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FixedSequenceJitter {
    draws: Vec<u64>,
    next: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FixedSequenceJitter {
    pub(crate) fn new(draws: impl Into<Vec<u64>>) -> Self {
        let draws = draws.into();
        assert!(!draws.is_empty(), "a fixed jitter sequence needs at least one draw");
        Self { draws, next: std::sync::atomic::AtomicUsize::new(0) }
    }
}

#[cfg(test)]
impl JitterSource for FixedSequenceJitter {
    fn next_up_to(&self, _upper_inclusive: u64) -> u64 {
        let index = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.draws[index % self.draws.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::{equal_jitter, full_jitter, FastrandJitter, FixedSequenceJitter, JitterStrategy};
    use shared::model::Millis;

    #[test]
    fn none_returns_the_base_delay_and_ignores_the_source() {
        let source = FixedSequenceJitter::new([7]);
        assert_eq!(JitterStrategy::None.apply(Millis::new(100), &source), Millis::new(100));
    }

    #[test]
    fn full_jitter_replays_the_injected_sequence() {
        let source = FixedSequenceJitter::new([0, 40, 100]);
        let base = Millis::new(100);
        assert_eq!(full_jitter(base, &source), Millis::new(0));
        assert_eq!(full_jitter(base, &source), Millis::new(40));
        assert_eq!(full_jitter(base, &source), Millis::new(100));
        // The sequence cycles.
        assert_eq!(full_jitter(base, &source), Millis::new(0));
    }

    #[test]
    fn full_jitter_bounds_are_zero_and_the_base() {
        assert_eq!(full_jitter(Millis::new(100), &FixedSequenceJitter::new([0])), Millis::new(0));
        assert_eq!(full_jitter(Millis::new(100), &FixedSequenceJitter::new([100])), Millis::new(100));
    }

    #[test]
    fn equal_jitter_keeps_half_the_delay_as_a_floor() {
        let base = Millis::new(100);
        assert_eq!(equal_jitter(base, &FixedSequenceJitter::new([0])), Millis::new(50));
        assert_eq!(equal_jitter(base, &FixedSequenceJitter::new([20])), Millis::new(70));
        assert_eq!(equal_jitter(base, &FixedSequenceJitter::new([50])), Millis::new(100));
    }

    #[test]
    fn equal_jitter_odd_delay_spans_floor_to_full_delay() {
        let base = Millis::new(101);
        assert_eq!(equal_jitter(base, &FixedSequenceJitter::new([0])), Millis::new(50));
        assert_eq!(equal_jitter(base, &FixedSequenceJitter::new([51])), Millis::new(101));
    }

    #[test]
    fn a_misbehaving_source_is_clamped_to_the_bounds() {
        let wild = FixedSequenceJitter::new([u64::MAX]);
        assert_eq!(full_jitter(Millis::new(100), &wild), Millis::new(100));
        assert_eq!(equal_jitter(Millis::new(100), &wild), Millis::new(100));
    }

    #[test]
    fn zero_delay_stays_zero() {
        let wild = FixedSequenceJitter::new([u64::MAX]);
        assert_eq!(full_jitter(Millis::new(0), &wild), Millis::new(0));
        assert_eq!(equal_jitter(Millis::new(0), &wild), Millis::new(0));
    }

    #[test]
    fn maximum_delay_saturates_instead_of_overflowing() {
        let max = Millis::new(u64::MAX);
        let wild = FixedSequenceJitter::new([u64::MAX]);
        assert_eq!(full_jitter(max, &wild), max);
        assert_eq!(equal_jitter(max, &wild), max);
        assert_eq!(equal_jitter(max, &FixedSequenceJitter::new([0])), Millis::new(u64::MAX / 2));
    }

    #[test]
    fn production_source_stays_within_bounds() {
        let base = Millis::new(1_000);
        for _ in 0..2_000 {
            assert!(full_jitter(base, &FastrandJitter) <= base);
            let equal = equal_jitter(base, &FastrandJitter);
            assert!((Millis::new(500)..=base).contains(&equal));
        }
    }

    #[test]
    fn trait_objects_can_be_used_as_the_source() {
        let source: Box<dyn super::JitterSource> = Box::new(FixedSequenceJitter::new([5]));
        assert_eq!(full_jitter(Millis::new(10), source.as_ref()), Millis::new(5));
    }
}
