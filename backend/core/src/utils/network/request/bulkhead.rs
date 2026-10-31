//! A keyed semaphore bulkhead: a cap on concurrent operations per integration or plugin.
//!
//! Concurrency and request rate are separate limits. A rate limiter admits N requests per second however
//! long each takes; a bulkhead bounds how many are in flight at once, so one slow integration cannot use
//! up every connection or task the process has. They compose; neither implies the other.
//!
//! Each key owns a `tokio::sync::Semaphore`. A successful acquire returns an owned
//! [`BulkheadPermit`], which releases its slot when dropped, on every path including panic and
//! cancellation. Waiters are served first-come first-served by the semaphore, and a waiter that is
//! dropped gives up its place without taking a permit.
//!
//! # Key set
//!
//! As with the rate limiter, the key set is bounded by `max_keys` and registered, not discovered; see
//! `rate_limit.rs` for the reasoning. [`KeyedBulkhead::remove`] and replacing a key through
//! [`KeyedBulkhead::register`] close the old semaphore, so callers queued on it are released with
//! [`BulkheadRejection::Closed`] instead of waiting out their deadline. Permits already held keep
//! working and return to the old semaphore when dropped.
//!
//! # Deadline
//!
//! The wait is bounded by a caller-supplied [`tokio::time::Instant`]. Taking an absolute deadline lets a
//! caller that owns one operation deadline pass the same instant to every stage rather than juggle
//! remaining durations. Tokio's timer is used, so tests run under `start_paused`.

use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
    time::{timeout_at, Instant},
};

/// A sensible bound on the number of registered keys when the caller has no better number.
pub const DEFAULT_MAX_BULKHEAD_KEYS: usize = 256;

/// An in-flight slot. The slot is released when this is dropped.
#[derive(Debug)]
#[must_use = "the slot is released as soon as the permit is dropped"]
pub struct BulkheadPermit {
    _permit: OwnedSemaphorePermit,
}

/// Why a slot was not granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BulkheadRejection {
    /// The key was never registered (or was removed).
    #[error("no bulkhead is registered for this key")]
    UnknownKey,
    /// `try_acquire` found every slot taken.
    #[error("bulkhead is saturated")]
    Saturated,
    /// No slot became free before the caller's deadline.
    #[error("bulkhead wait deadline exceeded")]
    DeadlineExceeded,
    /// The bulkhead was removed or replaced while the caller was queued.
    #[error("bulkhead was closed while waiting")]
    Closed,
}

/// Registration failed because the key set is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("bulkhead key capacity of {max_keys} is exhausted")]
pub struct BulkheadRegisterError {
    pub max_keys: usize,
}

/// Point-in-time copy of the bulkhead's counters. Keyless on purpose: low cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BulkheadSnapshot {
    /// Slots granted, immediately or after waiting.
    pub acquired: u64,
    /// Callers that had to queue for a slot (including ones later cancelled or timed out).
    pub waited: u64,
    pub rejected_saturated: u64,
    pub rejected_deadline: u64,
    pub rejected_closed: u64,
    pub rejected_unknown_key: u64,
}

impl BulkheadSnapshot {
    /// Every rejection, whatever the cause.
    pub fn rejected_total(&self) -> u64 {
        self.rejected_saturated
            .saturating_add(self.rejected_deadline)
            .saturating_add(self.rejected_closed)
            .saturating_add(self.rejected_unknown_key)
    }
}

#[derive(Debug, Default)]
struct Counters {
    acquired: AtomicU64,
    waited: AtomicU64,
    rejected_saturated: AtomicU64,
    rejected_deadline: AtomicU64,
    rejected_closed: AtomicU64,
    rejected_unknown_key: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) { counter.fetch_add(1, Ordering::Relaxed); }

    fn record_rejection(&self, rejection: BulkheadRejection) {
        Self::bump(match rejection {
            BulkheadRejection::UnknownKey => &self.rejected_unknown_key,
            BulkheadRejection::Saturated => &self.rejected_saturated,
            BulkheadRejection::DeadlineExceeded => &self.rejected_deadline,
            BulkheadRejection::Closed => &self.rejected_closed,
        });
    }

    fn snapshot(&self) -> BulkheadSnapshot {
        BulkheadSnapshot {
            acquired: self.acquired.load(Ordering::Relaxed),
            waited: self.waited.load(Ordering::Relaxed),
            rejected_saturated: self.rejected_saturated.load(Ordering::Relaxed),
            rejected_deadline: self.rejected_deadline.load(Ordering::Relaxed),
            rejected_closed: self.rejected_closed.load(Ordering::Relaxed),
            rejected_unknown_key: self.rejected_unknown_key.load(Ordering::Relaxed),
        }
    }
}

/// Semaphores by key. See the module docs.
#[derive(Debug)]
pub struct KeyedBulkhead {
    max_keys: usize,
    semaphores: RwLock<HashMap<String, Arc<Semaphore>>>,
    counters: Counters,
}

impl KeyedBulkhead {
    #[must_use]
    pub fn new(max_keys: usize) -> Self {
        Self { max_keys, semaphores: RwLock::new(HashMap::new()), counters: Counters::default() }
    }

    /// Register `key` with room for `max_concurrency` operations at once, or replace its limit.
    ///
    /// `max_concurrency` is clamped to `1..=Semaphore::MAX_PERMITS`: zero would admit nothing. Replacing
    /// closes the previous semaphore (see the module docs) and never counts against the key cap.
    pub fn register(&self, key: &str, max_concurrency: usize) -> Result<(), BulkheadRegisterError> {
        let permits = max_concurrency.clamp(1, Semaphore::MAX_PERMITS);
        let mut semaphores = self.semaphores.write();
        if !semaphores.contains_key(key) && semaphores.len() >= self.max_keys {
            return Err(BulkheadRegisterError { max_keys: self.max_keys });
        }
        if let Some(previous) = semaphores.insert(key.to_owned(), Arc::new(Semaphore::new(permits))) {
            previous.close();
        }
        Ok(())
    }

    /// Forget `key` and release anything queued on it. Returns whether it was registered.
    pub fn remove(&self, key: &str) -> bool {
        match self.semaphores.write().remove(key) {
            Some(semaphore) => {
                semaphore.close();
                true
            }
            None => false,
        }
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize { self.semaphores.read().len() }

    pub fn is_empty(&self) -> bool { self.semaphores.read().is_empty() }

    /// Slots currently free for `key`, or `None` if it is not registered.
    pub fn available(&self, key: &str) -> Option<usize> {
        self.semaphores.read().get(key).map(|semaphore| semaphore.available_permits())
    }

    pub fn snapshot(&self) -> BulkheadSnapshot { self.counters.snapshot() }

    fn semaphore(&self, key: &str) -> Result<Arc<Semaphore>, BulkheadRejection> {
        self.semaphores.read().get(key).cloned().ok_or(BulkheadRejection::UnknownKey)
    }

    fn finish(&self, result: Result<BulkheadPermit, BulkheadRejection>) -> Result<BulkheadPermit, BulkheadRejection> {
        match &result {
            Ok(_) => Counters::bump(&self.counters.acquired),
            Err(rejection) => self.counters.record_rejection(*rejection),
        }
        result
    }

    /// Take a slot if one is free now. Never waits.
    pub fn try_acquire(&self, key: &str) -> Result<BulkheadPermit, BulkheadRejection> {
        let result = self.semaphore(key).and_then(|semaphore| match semaphore.try_acquire_owned() {
            Ok(permit) => Ok(BulkheadPermit { _permit: permit }),
            Err(TryAcquireError::NoPermits) => Err(BulkheadRejection::Saturated),
            Err(TryAcquireError::Closed) => Err(BulkheadRejection::Closed),
        });
        self.finish(result)
    }

    /// Take a slot, waiting until `deadline` for one.
    ///
    /// A free slot is taken even when `deadline` has already passed; the deadline only bounds the wait.
    /// Cancellation safe: dropping the future leaves the queue without taking a permit.
    pub async fn acquire(&self, key: &str, deadline: Instant) -> Result<BulkheadPermit, BulkheadRejection> {
        let semaphore = match self.semaphore(key) {
            Ok(semaphore) => semaphore,
            Err(rejection) => return self.finish(Err(rejection)),
        };
        match Arc::clone(&semaphore).try_acquire_owned() {
            Ok(permit) => return self.finish(Ok(BulkheadPermit { _permit: permit })),
            Err(TryAcquireError::Closed) => return self.finish(Err(BulkheadRejection::Closed)),
            Err(TryAcquireError::NoPermits) => Counters::bump(&self.counters.waited),
        }
        let result = match timeout_at(deadline, semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(BulkheadPermit { _permit: permit }),
            Ok(Err(_closed)) => Err(BulkheadRejection::Closed),
            Err(_elapsed) => Err(BulkheadRejection::DeadlineExceeded),
        };
        self.finish(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{BulkheadRegisterError, BulkheadRejection, BulkheadSnapshot, KeyedBulkhead};
    use std::{sync::Arc, time::Duration};
    use tokio::time::Instant;

    const KEY: &str = "plugin-a";

    fn bulkhead(max_concurrency: usize) -> Arc<KeyedBulkhead> {
        let bulkhead = Arc::new(KeyedBulkhead::new(3));
        bulkhead.register(KEY, max_concurrency).expect("register within the cap");
        bulkhead
    }

    #[test]
    fn saturates_at_max_concurrency_and_releases_on_drop() {
        let bulkhead = bulkhead(2);
        let first = bulkhead.try_acquire(KEY).expect("first slot");
        let _second = bulkhead.try_acquire(KEY).expect("second slot");
        assert_eq!(bulkhead.available(KEY), Some(0));
        assert_eq!(bulkhead.try_acquire(KEY).unwrap_err(), BulkheadRejection::Saturated);

        drop(first);
        assert_eq!(bulkhead.available(KEY), Some(1));
        let _third = bulkhead.try_acquire(KEY).expect("slot freed by the drop");
        assert_eq!(bulkhead.try_acquire(KEY).unwrap_err(), BulkheadRejection::Saturated);
    }

    #[test]
    fn the_permit_is_released_when_its_scope_unwinds() {
        let bulkhead = bulkhead(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _permit = bulkhead.try_acquire(KEY).expect("slot");
            panic!("operation failed");
        }));
        assert!(result.is_err());
        assert_eq!(bulkhead.available(KEY), Some(1));
    }

    #[test]
    fn zero_concurrency_is_clamped_to_one() {
        let bulkhead = bulkhead(0);
        let _permit = bulkhead.try_acquire(KEY).expect("one slot");
        assert_eq!(bulkhead.try_acquire(KEY).unwrap_err(), BulkheadRejection::Saturated);
    }

    #[test]
    fn keys_are_independent_and_unknown_keys_are_rejected() {
        let bulkhead = bulkhead(1);
        bulkhead.register("plugin-b", 1).expect("second key");
        let _a = bulkhead.try_acquire(KEY).expect("a");
        let _b = bulkhead.try_acquire("plugin-b").expect("b is unaffected");
        assert_eq!(bulkhead.try_acquire("nope").unwrap_err(), BulkheadRejection::UnknownKey);
    }

    #[test]
    fn key_set_is_capped_but_existing_keys_can_be_replaced_and_removed() {
        let bulkhead = bulkhead(1);
        bulkhead.register("k1", 1).expect("within cap");
        bulkhead.register("k2", 1).expect("within cap");
        assert_eq!(bulkhead.register("one-too-many", 1), Err(BulkheadRegisterError { max_keys: 3 }));

        bulkhead.register(KEY, 5).expect("replace at cap");
        assert_eq!(bulkhead.available(KEY), Some(5));

        assert!(bulkhead.remove(KEY));
        assert!(!bulkhead.remove(KEY));
        assert_eq!(bulkhead.available(KEY), None);
        bulkhead.register("one-too-many", 1).expect("room after removal");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_takes_a_free_slot_even_with_an_expired_deadline() {
        let bulkhead = bulkhead(1);
        let permit = bulkhead.acquire(KEY, Instant::now()).await.expect("free slot");
        drop(permit);
        assert_eq!(bulkhead.snapshot().waited, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_caller_gets_the_slot_when_the_holder_drops_it() {
        let bulkhead = bulkhead(1);
        let held = bulkhead.try_acquire(KEY).expect("holder");
        let started = Instant::now();

        let waiter = {
            let bulkhead = Arc::clone(&bulkhead);
            tokio::spawn(async move {
                let _permit = bulkhead.acquire(KEY, Instant::now() + Duration::from_secs(10)).await.expect("granted");
                started.elapsed()
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(bulkhead.snapshot().waited, 1);

        tokio::time::advance(Duration::from_millis(300)).await;
        drop(held);
        assert_eq!(waiter.await.expect("waiter task"), Duration::from_millis(300));
        assert_eq!(bulkhead.available(KEY), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_is_bounded_by_the_deadline() {
        let bulkhead = bulkhead(1);
        let _held = bulkhead.try_acquire(KEY).expect("holder");

        let started = Instant::now();
        let outcome = bulkhead.acquire(KEY, started + Duration::from_millis(500)).await;
        assert_eq!(outcome.unwrap_err(), BulkheadRejection::DeadlineExceeded);
        assert_eq!(started.elapsed(), Duration::from_millis(500));

        let snapshot = bulkhead.snapshot();
        assert_eq!((snapshot.waited, snapshot.rejected_deadline, snapshot.rejected_total()), (1, 1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_queued_caller_does_not_swallow_the_slot() {
        let bulkhead = bulkhead(1);
        let held = bulkhead.try_acquire(KEY).expect("holder");

        tokio::select! {
            _ = bulkhead.acquire(KEY, Instant::now() + Duration::from_secs(10)) => panic!("should still be waiting"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        drop(held);
        assert_eq!(bulkhead.available(KEY), Some(1));
        let _permit = bulkhead.try_acquire(KEY).expect("slot is still free after the cancelled waiter");
    }

    #[tokio::test(start_paused = true)]
    async fn removing_a_key_releases_queued_callers_with_closed() {
        let bulkhead = bulkhead(1);
        let _held = bulkhead.try_acquire(KEY).expect("holder");

        let started = Instant::now();
        let waiter = {
            let bulkhead = Arc::clone(&bulkhead);
            tokio::spawn(async move { bulkhead.acquire(KEY, Instant::now() + Duration::from_secs(10)).await })
        };
        tokio::task::yield_now().await;

        assert!(bulkhead.remove(KEY));
        assert_eq!(waiter.await.expect("waiter task").unwrap_err(), BulkheadRejection::Closed);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(bulkhead.snapshot().rejected_closed, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn counters_total_every_outcome() {
        let bulkhead = bulkhead(1);
        assert_eq!(bulkhead.snapshot(), BulkheadSnapshot::default());
        let held = bulkhead.try_acquire(KEY).expect("holder");
        assert!(bulkhead.try_acquire(KEY).is_err());
        assert!(bulkhead.try_acquire("nope").is_err());
        drop(held);

        let snapshot = bulkhead.snapshot();
        assert_eq!(
            (snapshot.acquired, snapshot.rejected_saturated, snapshot.rejected_unknown_key, snapshot.rejected_total()),
            (1, 1, 1, 2)
        );
    }
}
