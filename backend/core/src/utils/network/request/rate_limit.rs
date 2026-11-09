//! A keyed token-bucket rate limiter with a bounded, cancellable queue.
//!
//! # Model
//!
//! Each key (an integration or plugin id) owns one bucket that holds up to `burst` tokens and gains
//! `permits_per_period` tokens every `period`. A request costs one token. Refill is computed lazily from
//! the injected [`Clock`] when a bucket is touched; there is no background task.
//!
//! The arithmetic is exact integer arithmetic. Tokens are held as a signed count of "units" where one
//! token is `period` units and each elapsed millisecond adds `permits_per_period` units. That keeps
//! fractional rates (one request per 10 s, three per second) free of rounding drift without floats, and
//! the `i128` width cannot overflow for any `u64` clock jump. Wait times round **up**, so a caller that
//! sleeps for the reported time never wakes early.
//!
//! # Waiting
//!
//! [`KeyedRateLimiter::acquire`] *reserves* a token, going into debt when the bucket is empty, and then
//! sleeps for the time the debt takes to clear. Reservation makes the queue first-come first-served and
//! means nothing is re-checked after the sleep. A reservation that is dropped while sleeping (the caller
//! was cancelled or timed out) gives its token back, so cancellation neither leaks capacity nor makes
//! later callers wait for a token nobody used. A refund can only shorten waits computed afterwards; waits
//! already handed out are not shortened.
//!
//! Two bounds apply to waiting: the per-key queue depth ([`RateLimitConfig::max_queued`]) and the
//! caller's `max_wait`. Both fail fast with a typed [`RateLimitRejection`] rather than sleeping.
//!
//! # Key set and eviction
//!
//! The key set is bounded by `max_keys` and is **registered, not discovered**: keys come from operator
//! configuration (integration and plugin ids), never from request data. [`KeyedRateLimiter::register`]
//! past the cap fails with [`RateLimitRegisterError`]; nothing is evicted automatically, because evicting
//! a bucket would silently reset a limit and dropping a configured key must be a decision. Callers call
//! [`KeyedRateLimiter::remove`] when an integration is unloaded and `register` again to reload a changed
//! limit (which starts a full bucket). Reservations already queued on a replaced or removed bucket finish
//! against that old bucket.
//!
//! `Retry-After` handling and adaptive limits are out of scope.

use crate::utils::{Clock, SystemClock};
use parking_lot::{Mutex, RwLock};
use shared::model::Millis;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use thiserror::Error;

/// A sensible bound on the number of registered keys when the caller has no better number.
pub const DEFAULT_MAX_RATE_LIMIT_KEYS: usize = 256;

/// Default depth of the per-key wait queue.
pub const DEFAULT_MAX_QUEUED: u32 = 64;

/// The limit applied to one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    burst: u32,
    permits_per_period: u32,
    period: Millis,
    max_queued: u32,
}

impl RateLimitConfig {
    /// `permits_per_period` requests per `period`, with up to `burst` available at once.
    ///
    /// All three are clamped to at least 1: a zero rate or an empty bucket would never admit anything,
    /// which is a misconfiguration rather than a limit. The queue depth starts at [`DEFAULT_MAX_QUEUED`].
    #[must_use]
    pub fn new(burst: u32, permits_per_period: u32, period: Millis) -> Self {
        Self {
            burst: burst.max(1),
            permits_per_period: permits_per_period.max(1),
            period: Millis::new(period.get().max(1)),
            max_queued: DEFAULT_MAX_QUEUED,
        }
    }

    /// `permits` requests per second with a bucket of `burst`.
    #[must_use]
    pub fn per_second(permits: u32, burst: u32) -> Self { Self::new(burst, permits, Millis::new(1_000)) }

    /// How many callers may wait for a token at once. `0` means callers never wait: [`KeyedRateLimiter::acquire`]
    /// then behaves like `try_acquire` plus a typed rejection.
    #[must_use]
    pub fn with_max_queued(mut self, max_queued: u32) -> Self {
        self.max_queued = max_queued;
        self
    }

    pub fn burst(&self) -> u32 { self.burst }
    pub fn permits_per_period(&self) -> u32 { self.permits_per_period }
    pub fn period(&self) -> Millis { self.period }
    pub fn max_queued(&self) -> u32 { self.max_queued }
}

/// Why a token was not granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RateLimitRejection {
    /// The key was never registered (or was removed).
    #[error("no rate limit is registered for this key")]
    UnknownKey,
    /// `try_acquire` found the bucket empty. A token will be available after `retry_after`.
    #[error("rate limit exhausted; a token is available in {retry_after}")]
    Exhausted { retry_after: Millis },
    /// The per-key wait queue is at its configured depth.
    #[error("rate limit queue is full")]
    QueueFull,
    /// The token would arrive after `required`, later than the caller's `max_wait`.
    #[error("rate limit wait of {required} exceeds the allowed {max_wait}")]
    WaitTooLong { required: Millis, max_wait: Millis },
}

/// Registration failed because the key set is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("rate limiter key capacity of {max_keys} is exhausted")]
pub struct RateLimitRegisterError {
    pub max_keys: usize,
}

/// Point-in-time copy of the limiter's counters. Keyless on purpose: low cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateLimitSnapshot {
    /// Tokens granted, immediately or after waiting.
    pub acquired: u64,
    /// Callers that entered the wait queue (including ones later cancelled).
    pub waited: u64,
    /// Waiting callers that were dropped before their token arrived.
    pub cancelled: u64,
    pub rejected_exhausted: u64,
    pub rejected_queue_full: u64,
    pub rejected_wait_too_long: u64,
    pub rejected_unknown_key: u64,
}

impl RateLimitSnapshot {
    /// Every rejection, whatever the cause.
    pub fn rejected_total(&self) -> u64 {
        self.rejected_exhausted
            .saturating_add(self.rejected_queue_full)
            .saturating_add(self.rejected_wait_too_long)
            .saturating_add(self.rejected_unknown_key)
    }
}

#[derive(Debug, Default)]
struct Counters {
    acquired: AtomicU64,
    waited: AtomicU64,
    cancelled: AtomicU64,
    rejected_exhausted: AtomicU64,
    rejected_queue_full: AtomicU64,
    rejected_wait_too_long: AtomicU64,
    rejected_unknown_key: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) { counter.fetch_add(1, Ordering::Relaxed); }

    fn record_rejection(&self, rejection: RateLimitRejection) {
        Self::bump(match rejection {
            RateLimitRejection::UnknownKey => &self.rejected_unknown_key,
            RateLimitRejection::Exhausted { .. } => &self.rejected_exhausted,
            RateLimitRejection::QueueFull => &self.rejected_queue_full,
            RateLimitRejection::WaitTooLong { .. } => &self.rejected_wait_too_long,
        });
    }

    fn snapshot(&self) -> RateLimitSnapshot {
        RateLimitSnapshot {
            acquired: self.acquired.load(Ordering::Relaxed),
            waited: self.waited.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            rejected_exhausted: self.rejected_exhausted.load(Ordering::Relaxed),
            rejected_queue_full: self.rejected_queue_full.load(Ordering::Relaxed),
            rejected_wait_too_long: self.rejected_wait_too_long.load(Ordering::Relaxed),
            rejected_unknown_key: self.rejected_unknown_key.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct BucketState {
    /// Signed units; negative while reservations are queued. See the module docs.
    tokens: i128,
    last_refill_ms: u64,
    queued: u32,
}

#[derive(Debug)]
struct Bucket {
    config: RateLimitConfig,
    state: Mutex<BucketState>,
}

enum Reserved {
    Immediate,
    Queued { wait: Millis },
}

impl Bucket {
    fn new(config: RateLimitConfig, now_ms: u64) -> Self {
        let bucket = Self { config, state: Mutex::new(BucketState { tokens: 0, last_refill_ms: now_ms, queued: 0 }) };
        bucket.state.lock().tokens = bucket.capacity();
        bucket
    }

    /// Units that make up one token.
    fn cost(&self) -> i128 { i128::from(self.config.period.get()) }

    fn capacity(&self) -> i128 { self.cost() * i128::from(self.config.burst) }

    /// Milliseconds until `deficit` units have been refilled, rounded up and saturated to `u64`.
    fn millis_for(&self, deficit: i128) -> Millis {
        let rate = i128::from(self.config.permits_per_period);
        let millis = deficit.saturating_add(rate - 1) / rate;
        Millis::new(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    /// Bring `state` up to `now_ms`. A clock that runs backwards neither refills nor rewinds.
    fn refill(&self, state: &mut BucketState, now_ms: u64) {
        if now_ms <= state.last_refill_ms {
            return;
        }
        let elapsed = i128::from(now_ms - state.last_refill_ms);
        let gained = elapsed * i128::from(self.config.permits_per_period);
        state.tokens = state.tokens.saturating_add(gained).min(self.capacity());
        state.last_refill_ms = now_ms;
    }

    fn try_take(&self, now_ms: u64) -> Result<(), Millis> {
        let mut state = self.state.lock();
        self.refill(&mut state, now_ms);
        if state.tokens >= self.cost() {
            state.tokens -= self.cost();
            Ok(())
        } else {
            Err(self.millis_for(self.cost() - state.tokens))
        }
    }

    fn reserve(&self, now_ms: u64, max_wait: Millis) -> Result<Reserved, RateLimitRejection> {
        let mut state = self.state.lock();
        self.refill(&mut state, now_ms);
        if state.tokens >= self.cost() {
            state.tokens -= self.cost();
            return Ok(Reserved::Immediate);
        }
        if state.queued >= self.config.max_queued {
            return Err(RateLimitRejection::QueueFull);
        }
        let wait = self.millis_for(self.cost() - state.tokens);
        if wait > max_wait {
            return Err(RateLimitRejection::WaitTooLong { required: wait, max_wait });
        }
        state.tokens -= self.cost();
        state.queued += 1;
        Ok(Reserved::Queued { wait })
    }

    fn release_reservation(&self, redeemed: bool) {
        let mut state = self.state.lock();
        state.queued = state.queued.saturating_sub(1);
        if !redeemed {
            state.tokens = state.tokens.saturating_add(self.cost()).min(self.capacity());
        }
    }
}

/// A token reserved while its caller sleeps. Dropping it before [`Self::redeem`] refunds the token.
struct QueuedReservation {
    bucket: Arc<Bucket>,
    counters: Arc<Counters>,
    redeemed: bool,
}

impl QueuedReservation {
    fn redeem(mut self) { self.redeemed = true; }
}

impl Drop for QueuedReservation {
    fn drop(&mut self) {
        self.bucket.release_reservation(self.redeemed);
        if !self.redeemed {
            Counters::bump(&self.counters.cancelled);
        }
    }
}

/// Token buckets by key. See the module docs for the model.
#[derive(Debug)]
pub struct KeyedRateLimiter<C: Clock = SystemClock> {
    clock: C,
    max_keys: usize,
    buckets: RwLock<HashMap<String, Arc<Bucket>>>,
    counters: Arc<Counters>,
}

impl KeyedRateLimiter<SystemClock> {
    /// A limiter on the real clock.
    #[must_use]
    pub fn new(max_keys: usize) -> Self { Self::with_clock(SystemClock, max_keys) }
}

impl<C: Clock> KeyedRateLimiter<C> {
    #[must_use]
    pub fn with_clock(clock: C, max_keys: usize) -> Self {
        Self { clock, max_keys, buckets: RwLock::new(HashMap::new()), counters: Arc::new(Counters::default()) }
    }

    /// Register `key` with `config`, or replace its limit if it already exists (starting a full bucket).
    /// Replacing never counts against the key cap.
    pub fn register(&self, key: &str, config: RateLimitConfig) -> Result<(), RateLimitRegisterError> {
        let mut buckets = self.buckets.write();
        if !buckets.contains_key(key) && buckets.len() >= self.max_keys {
            return Err(RateLimitRegisterError { max_keys: self.max_keys });
        }
        buckets.insert(key.to_owned(), Arc::new(Bucket::new(config, self.clock.now_ms().get())));
        Ok(())
    }

    /// Forget `key`. Returns whether it was registered.
    pub fn remove(&self, key: &str) -> bool { self.buckets.write().remove(key).is_some() }

    /// Number of registered keys.
    pub fn len(&self) -> usize { self.buckets.read().len() }

    pub fn is_empty(&self) -> bool { self.buckets.read().is_empty() }

    pub fn snapshot(&self) -> RateLimitSnapshot { self.counters.snapshot() }

    fn bucket(&self, key: &str) -> Result<Arc<Bucket>, RateLimitRejection> {
        self.buckets.read().get(key).cloned().ok_or(RateLimitRejection::UnknownKey)
    }

    /// Take a token if one is available now. Never waits.
    pub fn try_acquire(&self, key: &str) -> Result<(), RateLimitRejection> {
        let result = self.bucket(key).and_then(|bucket| {
            bucket
                .try_take(self.clock.now_ms().get())
                .map_err(|retry_after| RateLimitRejection::Exhausted { retry_after })
        });
        match result {
            Ok(()) => Counters::bump(&self.counters.acquired),
            Err(rejection) => self.counters.record_rejection(rejection),
        }
        result
    }

    /// Take a token, waiting up to `max_wait` for one.
    ///
    /// Returns how long the caller was made to wait (zero when a token was free). Rejects
    /// immediately, without sleeping, when the queue is full or the token is further away than `max_wait`.
    ///
    /// Cancellation safe: dropping the future while it sleeps refunds the token and frees the queue slot.
    /// The sleep runs on tokio's timer, so under `start_paused` it follows virtual time.
    pub async fn acquire(&self, key: &str, max_wait: Millis) -> Result<Millis, RateLimitRejection> {
        let bucket = match self.bucket(key) {
            Ok(bucket) => bucket,
            Err(rejection) => {
                self.counters.record_rejection(rejection);
                return Err(rejection);
            }
        };
        match bucket.reserve(self.clock.now_ms().get(), max_wait) {
            Ok(Reserved::Immediate) => {
                Counters::bump(&self.counters.acquired);
                Ok(Millis::new(0))
            }
            Ok(Reserved::Queued { wait }) => {
                Counters::bump(&self.counters.waited);
                let reservation = QueuedReservation { bucket, counters: Arc::clone(&self.counters), redeemed: false };
                tokio::time::sleep(wait.as_duration()).await;
                reservation.redeem();
                Counters::bump(&self.counters.acquired);
                Ok(wait)
            }
            Err(rejection) => {
                self.counters.record_rejection(rejection);
                Err(rejection)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyedRateLimiter, RateLimitConfig, RateLimitRegisterError, RateLimitRejection, RateLimitSnapshot};
    use crate::utils::ManualClock;
    use shared::model::Millis;
    use std::{sync::Arc, time::Duration};
    use tokio::time::Instant;

    const KEY: &str = "plugin-a";

    fn new_limiter(config: RateLimitConfig) -> (Arc<KeyedRateLimiter<ManualClock>>, ManualClock) {
        let clock = ManualClock::new(1_000);
        let limiter = Arc::new(KeyedRateLimiter::with_clock(clock.clone(), 4));
        limiter.register(KEY, config).expect("register within the cap");
        (limiter, clock)
    }

    fn retry_after(limiter: &KeyedRateLimiter<ManualClock>) -> Millis {
        match limiter.try_acquire(KEY) {
            Err(RateLimitRejection::Exhausted { retry_after }) => retry_after,
            other => panic!("expected the bucket to be exhausted, got {other:?}"),
        }
    }

    #[test]
    fn config_clamps_degenerate_values_to_one() {
        let config = RateLimitConfig::new(0, 0, Millis::new(0));
        assert_eq!((config.burst(), config.permits_per_period(), config.period()), (1, 1, Millis::new(1)));
    }

    #[test]
    fn burst_is_available_at_once_then_the_bucket_is_empty() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 3));
        for _ in 0..3 {
            limiter.try_acquire(KEY).expect("burst token");
        }
        assert_eq!(retry_after(&limiter), Millis::new(1_000));
    }

    #[test]
    fn refill_is_exact_and_caps_at_the_burst() {
        let (limiter, clock) = new_limiter(RateLimitConfig::per_second(1, 2));
        limiter.try_acquire(KEY).expect("first");
        limiter.try_acquire(KEY).expect("second");

        clock.advance(999);
        assert_eq!(retry_after(&limiter), Millis::new(1));
        clock.advance(1);
        limiter.try_acquire(KEY).expect("one token after one second");
        assert_eq!(retry_after(&limiter), Millis::new(1_000));

        // A long idle period refills to the burst and no further.
        clock.advance(3_600_000);
        limiter.try_acquire(KEY).expect("first after idle");
        limiter.try_acquire(KEY).expect("second after idle");
        assert!(matches!(limiter.try_acquire(KEY), Err(RateLimitRejection::Exhausted { .. })));
    }

    #[test]
    fn slow_rates_are_exact() {
        // One request per ten seconds.
        let (limiter, clock) = new_limiter(RateLimitConfig::new(1, 1, Millis::new(10_000)));
        limiter.try_acquire(KEY).expect("first");
        clock.advance(3_333);
        assert_eq!(retry_after(&limiter), Millis::new(6_667));
        clock.advance(6_667);
        limiter.try_acquire(KEY).expect("after the full period");
    }

    #[test]
    fn fast_rates_round_the_wait_up_and_keep_the_remainder() {
        // Three requests per second: a token every 333.33 ms. A burst of two leaves room above
        // one token, so the remainder is not cut off by the bucket capacity.
        let (limiter, clock) = new_limiter(RateLimitConfig::per_second(3, 2));
        limiter.try_acquire(KEY).expect("first");
        limiter.try_acquire(KEY).expect("second, from the burst");
        assert_eq!(retry_after(&limiter), Millis::new(334));
        clock.advance(334);
        limiter.try_acquire(KEY).expect("after the rounded-up wait");
        // 334 ms earned 1.002 tokens; the 0.002 left over is not lost, so the next wait is 333.
        clock.advance(333);
        limiter.try_acquire(KEY).expect("remainder carried forward");
    }

    #[test]
    fn a_clock_running_backwards_neither_refills_nor_panics() {
        let (limiter, clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.try_acquire(KEY).expect("first");
        clock.set(0);
        assert_eq!(retry_after(&limiter), Millis::new(1_000));
        clock.set(1_500);
        assert_eq!(retry_after(&limiter), Millis::new(500));
    }

    #[test]
    fn huge_clock_jumps_and_periods_saturate_instead_of_overflowing() {
        let (limiter, clock) = new_limiter(RateLimitConfig::new(u32::MAX, u32::MAX, Millis::new(u64::MAX)));
        clock.set(u64::MAX);
        limiter.try_acquire(KEY).expect("token after a maximal jump");

        let (limiter, _clock) = new_limiter(RateLimitConfig::new(1, 1, Millis::new(u64::MAX)));
        limiter.try_acquire(KEY).expect("first");
        assert_eq!(retry_after(&limiter), Millis::new(u64::MAX));
    }

    #[test]
    fn keys_are_independent_and_unknown_keys_are_rejected() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.register("plugin-b", RateLimitConfig::per_second(1, 1)).expect("second key");
        limiter.try_acquire(KEY).expect("a");
        assert!(limiter.try_acquire(KEY).is_err());
        limiter.try_acquire("plugin-b").expect("b is unaffected");
        assert_eq!(limiter.try_acquire("nope"), Err(RateLimitRejection::UnknownKey));
    }

    #[test]
    fn key_set_is_capped_but_existing_keys_can_be_replaced_and_removed() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        for index in 1..4 {
            limiter.register(&format!("k{index}"), RateLimitConfig::per_second(1, 1)).expect("within cap");
        }
        assert_eq!(limiter.len(), 4);
        assert_eq!(
            limiter.register("one-too-many", RateLimitConfig::per_second(1, 1)),
            Err(RateLimitRegisterError { max_keys: 4 })
        );

        // Reload: replacing a key at the cap works and starts a full bucket.
        limiter.try_acquire(KEY).expect("drain");
        assert!(limiter.try_acquire(KEY).is_err());
        limiter.register(KEY, RateLimitConfig::per_second(1, 1)).expect("replace at cap");
        limiter.try_acquire(KEY).expect("fresh bucket");

        assert!(limiter.remove(KEY));
        assert!(!limiter.remove(KEY));
        limiter.register("one-too-many", RateLimitConfig::per_second(1, 1)).expect("room after removal");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_with_a_free_token_returns_without_waiting() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        let started = Instant::now();
        assert_eq!(limiter.acquire(KEY, Millis::new(0)).await, Ok(Millis::new(0)));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waits_for_the_refill_and_reports_the_wait() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.try_acquire(KEY).expect("drain");

        let started = Instant::now();
        assert_eq!(limiter.acquire(KEY, Millis::new(5_000)).await, Ok(Millis::new(1_000)));
        assert_eq!(started.elapsed(), Duration::from_millis(1_000));

        let snapshot = limiter.snapshot();
        assert_eq!((snapshot.acquired, snapshot.waited, snapshot.cancelled), (2, 1, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn queued_callers_are_served_in_order_one_interval_apart() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.try_acquire(KEY).expect("drain");

        let started = Instant::now();
        let spawn_waiter = |limiter: Arc<KeyedRateLimiter<ManualClock>>| {
            tokio::spawn(async move {
                limiter.acquire(KEY, Millis::new(10_000)).await.expect("granted");
                started.elapsed()
            })
        };
        let first = spawn_waiter(Arc::clone(&limiter));
        tokio::task::yield_now().await;
        let second = spawn_waiter(Arc::clone(&limiter));
        tokio::task::yield_now().await;

        assert_eq!(first.await.expect("first task"), Duration::from_millis(1_000));
        assert_eq!(second.await.expect("second task"), Duration::from_millis(2_000));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_longer_than_max_wait_is_rejected_without_sleeping() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.try_acquire(KEY).expect("drain");

        let started = Instant::now();
        assert_eq!(
            limiter.acquire(KEY, Millis::new(999)).await,
            Err(RateLimitRejection::WaitTooLong { required: Millis::new(1_000), max_wait: Millis::new(999) })
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(limiter.snapshot().rejected_wait_too_long, 1);

        // The rejection reserved nothing: the exact wait is still 1000 ms and fits a 1000 ms bound.
        assert_eq!(limiter.acquire(KEY, Millis::new(1_000)).await, Ok(Millis::new(1_000)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_queue_is_bounded() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1).with_max_queued(1));
        limiter.try_acquire(KEY).expect("drain");

        let waiter = {
            let limiter = Arc::clone(&limiter);
            tokio::spawn(async move { limiter.acquire(KEY, Millis::new(10_000)).await })
        };
        tokio::task::yield_now().await;

        assert_eq!(limiter.acquire(KEY, Millis::new(10_000)).await, Err(RateLimitRejection::QueueFull));
        assert_eq!(waiter.await.expect("waiter task"), Ok(Millis::new(1_000)));

        let snapshot = limiter.snapshot();
        assert_eq!((snapshot.rejected_queue_full, snapshot.waited), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_depth_queue_never_waits() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1).with_max_queued(0));
        limiter.try_acquire(KEY).expect("drain");
        assert_eq!(limiter.acquire(KEY, Millis::new(10_000)).await, Err(RateLimitRejection::QueueFull));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_queued_caller_refunds_its_token_and_queue_slot() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1).with_max_queued(1));
        limiter.try_acquire(KEY).expect("drain");

        // Drop the waiter 100 ms into its 1000 ms wait.
        tokio::select! {
            _ = limiter.acquire(KEY, Millis::new(10_000)) => panic!("should still be waiting"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        let snapshot = limiter.snapshot();
        assert_eq!((snapshot.waited, snapshot.cancelled, snapshot.acquired), (1, 1, 1));
        // Without the refund this would be 2000: the cancelled waiter would still be holding a token.
        assert_eq!(retry_after(&limiter), Millis::new(1_000));
        // The queue slot is free again even though the depth is 1.
        assert_eq!(limiter.acquire(KEY, Millis::new(10_000)).await, Ok(Millis::new(1_000)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_removed_key_rejects_new_callers_but_queued_ones_finish() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        limiter.try_acquire(KEY).expect("drain");
        let waiter = {
            let limiter = Arc::clone(&limiter);
            tokio::spawn(async move { limiter.acquire(KEY, Millis::new(10_000)).await })
        };
        tokio::task::yield_now().await;

        assert!(limiter.remove(KEY));
        assert_eq!(limiter.acquire(KEY, Millis::new(10_000)).await, Err(RateLimitRejection::UnknownKey));
        assert_eq!(waiter.await.expect("waiter task"), Ok(Millis::new(1_000)));
    }

    #[test]
    fn counters_start_at_zero_and_total_the_rejections() {
        let (limiter, _clock) = new_limiter(RateLimitConfig::per_second(1, 1));
        assert_eq!(limiter.snapshot(), RateLimitSnapshot::default());
        limiter.try_acquire(KEY).expect("one");
        assert!(limiter.try_acquire(KEY).is_err());
        assert!(limiter.try_acquire("nope").is_err());
        let snapshot = limiter.snapshot();
        assert_eq!((snapshot.acquired, snapshot.rejected_exhausted, snapshot.rejected_unknown_key), (1, 1, 1));
        assert_eq!(snapshot.rejected_total(), 2);
    }
}
