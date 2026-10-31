//! A thin executor that composes the circuit breaker, a bulkhead and a rate limiter around one call.
//!
//! # Order
//!
//! ```text
//! circuit permit -> bulkhead permit -> rate-limit permit -> caller future -> classify -> report
//! ```
//!
//! The circuit comes first so an open circuit rejects without taking a slot or a token. The bulkhead
//! comes before the rate limiter so a caller never holds a reserved token while it waits for a slot.
//! Each stage is optional per key ([`OutboundPolicy`]).
//!
//! # What it does not do
//!
//! There is no retry loop. The breaker observes the final result of one operation, and a caller that
//! retries (the request stack, or the messaging outbox for durable delivery) owns that loop and calls
//! [`OutboundExecutor::execute`] once per operation it wants counted. Redirects, body limits and the
//! HTTP transport are the caller's future; the executor only sees a `Result`.
//!
//! # One deadline
//!
//! [`OutboundPolicy::deadline`] starts when `execute` is called and covers the bulkhead wait, the
//! rate-limit wait and the call itself. The stages share one absolute `tokio::time::Instant`, so none of
//! them restarts the budget. It runs on tokio's timer, so tests under `start_paused` follow virtual time.
//! The injected [`Clock`] feeds the breaker and the rate limiter, and measures how long the queues held
//! the caller.
//!
//! A queue stage that cannot finish inside the deadline reports its own cause: a bulkhead that stays
//! full is [`OutboundError::BulkheadFull`], and a token that arrives after the deadline is rejected up
//! front as [`OutboundError::RateLimited`] with the wait it would need, instead of sleeping towards a
//! deadline that cannot be met. [`OutboundError::DeadlineExceeded`] is the call itself running out of
//! budget.
//!
//! # Breaker state after a wait
//!
//! The circuit permit is taken first, so after a long queue wait it describes a world that may be gone:
//! the circuit may have tripped while the caller waited. When the queues held the caller for at least
//! [`OutboundPolicy::recheck_after`], the permit is returned and a fresh one is requested, which rejects
//! with [`OutboundError::CircuitOpen`] if the circuit is no longer admitting. The returned permit reports
//! `Cancelled`, which the default breaker policy does not count.
//!
//! # Outcomes
//!
//! The caller's classifier maps the final `Result` to a [`CallOutcome`], so a response with a 503 status
//! can be `Transient` even though the transport call returned `Ok`. A rejection before the call (rate
//! limit, bulkhead, deadline while queued) is reported as `LocalRejection`; running out of deadline
//! during the call is `Transient`; dropping the future, or cancelling the token, is `Cancelled`.
//!
//! # Cancellation
//!
//! Dropping the future at any stage releases everything it holds: the breaker permit reports
//! `Cancelled` (freeing a half-open probe slot), the bulkhead slot is released and a queued rate-limit
//! reservation is refunded. [`OutboundExecutor::execute_cancellable`] additionally watches a
//! [`CancellationToken`] and returns [`OutboundError::Cancelled`].
//!
//! # Keys and counters
//!
//! Keys are registered, not discovered, and must be low-cardinality integration or plugin ids: the
//! key set is bounded and nothing is ever labelled with request data. Counters are plain saturating-free
//! atomics read through [`OutboundExecutor::snapshot`]; there is no metrics recorder in this crate.

use super::{
    bulkhead::{BulkheadPermit, BulkheadRejection, BulkheadSnapshot, KeyedBulkhead},
    rate_limit::{KeyedRateLimiter, RateLimitConfig, RateLimitRejection, RateLimitSnapshot},
};
use crate::utils::{
    CallOutcome, CircuitBreaker, CircuitBreakerPolicy, CircuitPermit, CircuitPolicyError, CircuitSnapshot, Clock,
    SystemClock,
};
use parking_lot::RwLock;
use shared::model::Millis;
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use thiserror::Error;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

/// A sensible bound on the number of registered keys when the caller has no better number.
pub const DEFAULT_MAX_EXECUTOR_KEYS: usize = 256;

/// Operation deadline of [`OutboundPolicy::default`].
pub const DEFAULT_OPERATION_DEADLINE: Millis = Millis::new(30_000);

/// Queue time after which [`OutboundPolicy::default`] rechecks the breaker.
pub const DEFAULT_QUEUE_RECHECK_AFTER: Millis = Millis::new(1_000);

/// Fallback for a deadline too far away to represent as an `Instant`.
const FAR_FUTURE: Duration = Duration::from_hours(8760);

/// What one key is limited by. Every stage but the deadline is optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundPolicy {
    /// Consecutive-failure breaker. `None` disables the stage.
    pub circuit_breaker: Option<CircuitBreakerPolicy>,
    /// Token bucket. `None` disables the stage.
    pub rate_limit: Option<RateLimitConfig>,
    /// Concurrent operations. `None` disables the stage; zero is raised to one.
    pub max_concurrency: Option<usize>,
    /// Budget for the whole operation: queue waits plus the call.
    pub deadline: Millis,
    /// Queue time after which the breaker is asked again before the call runs.
    pub recheck_after: Millis,
}

impl Default for OutboundPolicy {
    fn default() -> Self {
        Self {
            circuit_breaker: Some(CircuitBreakerPolicy::default()),
            rate_limit: None,
            max_concurrency: None,
            deadline: DEFAULT_OPERATION_DEADLINE,
            recheck_after: DEFAULT_QUEUE_RECHECK_AFTER,
        }
    }
}

impl OutboundPolicy {
    #[must_use]
    pub const fn with_circuit_breaker(mut self, policy: CircuitBreakerPolicy) -> Self {
        self.circuit_breaker = Some(policy);
        self
    }

    #[must_use]
    pub const fn without_circuit_breaker(mut self) -> Self {
        self.circuit_breaker = None;
        self
    }

    #[must_use]
    pub const fn with_rate_limit(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit = Some(config);
        self
    }

    #[must_use]
    pub const fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = Some(max_concurrency);
        self
    }

    #[must_use]
    pub const fn with_deadline(mut self, deadline: Millis) -> Self {
        self.deadline = deadline;
        self
    }

    #[must_use]
    pub const fn with_recheck_after(mut self, recheck_after: Millis) -> Self {
        self.recheck_after = recheck_after;
        self
    }
}

/// Why [`OutboundExecutor::execute`] did not return the caller's value.
///
/// Everything except [`Self::Inner`] is decided locally and carries no request data.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutboundError<E> {
    /// The circuit is open, or a half-open probe is already in flight. `retry_after` is `None` when an
    /// administrator forced the circuit open.
    #[error("circuit breaker is open")]
    CircuitOpen { retry_after: Option<Millis> },
    /// No token was available within the budget. `retry_after` is how long until one would be, when known.
    #[error("rate limit exhausted")]
    RateLimited { retry_after: Option<Millis> },
    /// No concurrency slot became free within the budget.
    #[error("concurrency limit reached")]
    BulkheadFull,
    /// The call did not finish inside the operation deadline.
    #[error("operation deadline exceeded")]
    DeadlineExceeded,
    /// The cancellation token fired.
    #[error("operation cancelled")]
    Cancelled,
    /// The key is not registered, or was removed or replaced while the caller was queued.
    #[error("outbound key is not registered")]
    UnknownKey,
    /// The caller's own error.
    #[error("{0}")]
    Inner(E),
}

impl<E> OutboundError<E> {
    /// The caller's own error, if that is what this is.
    pub fn into_inner(self) -> Option<E> {
        match self {
            Self::Inner(error) => Some(error),
            _ => None,
        }
    }

    /// True for everything the executor decided without running (or finishing) the call.
    pub const fn is_local(&self) -> bool { !matches!(self, Self::Inner(_)) }
}

/// Registration failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExecutorRegisterError {
    #[error("outbound executor key capacity of {max_keys} is exhausted")]
    KeyCapacity { max_keys: usize },
    #[error(transparent)]
    InvalidCircuitPolicy(#[from] CircuitPolicyError),
}

/// Point-in-time copy of the executor's counters. Keyless on purpose: low cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutorSnapshot {
    /// Calls to `execute`, whatever became of them.
    pub started: u64,
    /// Calls that cleared every stage and ran the caller's future.
    pub admitted: u64,
    /// Calls whose classified outcome was `Success`.
    pub succeeded: u64,
    pub failed_transient: u64,
    pub failed_rate_limited: u64,
    pub failed_permanent: u64,
    /// Classified `LocalRejection` by the caller.
    pub failed_local: u64,
    pub rejected_circuit_open: u64,
    pub rejected_rate_limited: u64,
    pub rejected_bulkhead_full: u64,
    pub rejected_unknown_key: u64,
    /// The call, or the queue towards it, ran out of the operation deadline.
    pub deadline_exceeded: u64,
    /// Futures dropped, or tokens fired, before the call finished.
    pub cancelled: u64,
    /// Times the breaker was asked again after a long queue wait.
    pub queue_rechecks: u64,
    pub rate_limit: RateLimitSnapshot,
    pub bulkhead: BulkheadSnapshot,
}

#[derive(Debug, Default)]
struct Counters {
    started: AtomicU64,
    admitted: AtomicU64,
    succeeded: AtomicU64,
    failed_transient: AtomicU64,
    failed_rate_limited: AtomicU64,
    failed_permanent: AtomicU64,
    failed_local: AtomicU64,
    rejected_circuit_open: AtomicU64,
    rejected_rate_limited: AtomicU64,
    rejected_bulkhead_full: AtomicU64,
    rejected_unknown_key: AtomicU64,
    deadline_exceeded: AtomicU64,
    cancelled: AtomicU64,
    queue_rechecks: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) { counter.fetch_add(1, Ordering::Relaxed); }

    fn record_outcome(&self, outcome: CallOutcome) {
        match outcome {
            CallOutcome::Success => Self::bump(&self.succeeded),
            CallOutcome::Transient => Self::bump(&self.failed_transient),
            CallOutcome::RateLimited => Self::bump(&self.failed_rate_limited),
            CallOutcome::Permanent => Self::bump(&self.failed_permanent),
            CallOutcome::LocalRejection => Self::bump(&self.failed_local),
            CallOutcome::Cancelled => Self::bump(&self.cancelled),
        }
    }

    fn record_rejection<E>(&self, error: &OutboundError<E>) {
        match error {
            OutboundError::CircuitOpen { .. } => Self::bump(&self.rejected_circuit_open),
            OutboundError::RateLimited { .. } => Self::bump(&self.rejected_rate_limited),
            OutboundError::BulkheadFull => Self::bump(&self.rejected_bulkhead_full),
            OutboundError::DeadlineExceeded => Self::bump(&self.deadline_exceeded),
            OutboundError::Cancelled => Self::bump(&self.cancelled),
            OutboundError::UnknownKey => Self::bump(&self.rejected_unknown_key),
            OutboundError::Inner(_) => {}
        }
    }

    fn snapshot(&self, rate_limit: RateLimitSnapshot, bulkhead: BulkheadSnapshot) -> ExecutorSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ExecutorSnapshot {
            started: load(&self.started),
            admitted: load(&self.admitted),
            succeeded: load(&self.succeeded),
            failed_transient: load(&self.failed_transient),
            failed_rate_limited: load(&self.failed_rate_limited),
            failed_permanent: load(&self.failed_permanent),
            failed_local: load(&self.failed_local),
            rejected_circuit_open: load(&self.rejected_circuit_open),
            rejected_rate_limited: load(&self.rejected_rate_limited),
            rejected_bulkhead_full: load(&self.rejected_bulkhead_full),
            rejected_unknown_key: load(&self.rejected_unknown_key),
            deadline_exceeded: load(&self.deadline_exceeded),
            cancelled: load(&self.cancelled),
            queue_rechecks: load(&self.queue_rechecks),
            rate_limit,
            bulkhead,
        }
    }
}

/// Counts a future that is dropped before it finished.
struct DropCounter<'a> {
    counters: &'a Counters,
    finished: bool,
}

impl Drop for DropCounter<'_> {
    fn drop(&mut self) {
        if !self.finished {
            Counters::bump(&self.counters.cancelled);
        }
    }
}

struct Entry<C: Clock> {
    policy: OutboundPolicy,
    breaker: Option<CircuitBreaker<C>>,
}

/// What a call holds while it runs. Dropping it releases the slot and, unreported, cancels the permit.
struct Admission<C: Clock> {
    circuit: Option<CircuitPermit<C>>,
    /// Held for its `Drop`, which releases the slot.
    #[allow(dead_code)]
    slot: Option<BulkheadPermit>,
}

impl<C: Clock> Admission<C> {
    fn report(&mut self, outcome: CallOutcome) {
        if let Some(permit) = self.circuit.take() {
            permit.report(outcome);
        }
    }
}

/// Composes a breaker, a bulkhead and a rate limiter per key. See the module docs.
pub struct OutboundExecutor<C: Clock = SystemClock> {
    clock: C,
    max_keys: usize,
    entries: RwLock<HashMap<Arc<str>, Arc<Entry<C>>>>,
    rate_limiter: KeyedRateLimiter<C>,
    bulkhead: KeyedBulkhead,
    counters: Counters,
}

impl<C: Clock> std::fmt::Debug for OutboundExecutor<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundExecutor").field("keys", &self.len()).finish_non_exhaustive()
    }
}

impl OutboundExecutor<SystemClock> {
    /// An executor on the real clock with room for [`DEFAULT_MAX_EXECUTOR_KEYS`] keys.
    #[must_use]
    pub fn new() -> Self { Self::with_clock(SystemClock, DEFAULT_MAX_EXECUTOR_KEYS) }
}

impl Default for OutboundExecutor<SystemClock> {
    fn default() -> Self { Self::new() }
}

impl<C: Clock> OutboundExecutor<C> {
    /// An executor whose breakers and rate limiter read `clock`, holding at most `max_keys` keys.
    #[must_use]
    pub fn with_clock(clock: C, max_keys: usize) -> Self {
        Self {
            rate_limiter: KeyedRateLimiter::with_clock(clock.clone(), max_keys),
            bulkhead: KeyedBulkhead::new(max_keys),
            clock,
            max_keys,
            entries: RwLock::new(HashMap::new()),
            counters: Counters::default(),
        }
    }

    /// Register `key` with `policy`, or replace its policy.
    ///
    /// Replacing keeps the key's breaker (reconfigured, which closes it and makes permits issued before
    /// stale) and starts a full token bucket; callers queued on the old bulkhead are released with
    /// [`OutboundError::UnknownKey`]. Replacing never counts against the key cap.
    pub fn register(&self, key: impl Into<Arc<str>>, policy: OutboundPolicy) -> Result<(), ExecutorRegisterError> {
        let key: Arc<str> = key.into();
        if let Some(breaker_policy) = &policy.circuit_breaker {
            breaker_policy.validate()?;
        }
        let mut entries = self.entries.write();
        let previous = entries.get(&key).cloned();
        if previous.is_none() && entries.len() >= self.max_keys {
            return Err(ExecutorRegisterError::KeyCapacity { max_keys: self.max_keys });
        }
        let capacity = ExecutorRegisterError::KeyCapacity { max_keys: self.max_keys };

        match policy.rate_limit {
            Some(config) => self.rate_limiter.register(&key, config).map_err(|_| capacity)?,
            None => {
                self.rate_limiter.remove(&key);
            }
        }
        match policy.max_concurrency {
            Some(max_concurrency) => self.bulkhead.register(&key, max_concurrency).map_err(|_| capacity)?,
            None => {
                self.bulkhead.remove(&key);
            }
        }
        let breaker = match (policy.circuit_breaker, previous.and_then(|entry| entry.breaker.clone())) {
            (Some(breaker_policy), Some(existing)) => {
                existing.reconfigure(breaker_policy)?;
                Some(existing)
            }
            (Some(breaker_policy), None) => Some(CircuitBreaker::new(breaker_policy, self.clock.clone())?),
            (None, _) => None,
        };
        entries.insert(key, Arc::new(Entry { policy, breaker }));
        Ok(())
    }

    /// Forget `key`, releasing anything queued on it. Returns whether it was registered.
    pub fn remove(&self, key: &str) -> bool {
        let removed = self.entries.write().remove(key).is_some();
        if removed {
            self.rate_limiter.remove(key);
            self.bulkhead.remove(key);
        }
        removed
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize { self.entries.read().len() }

    pub fn is_empty(&self) -> bool { self.entries.read().is_empty() }

    /// The breaker state for `key`, or `None` if it is not registered or has no breaker.
    pub fn circuit_snapshot(&self, key: &str) -> Option<CircuitSnapshot> {
        self.entry(key)?.breaker.as_ref().map(CircuitBreaker::snapshot)
    }

    /// Administrative reset of `key`'s breaker. Returns whether it has one.
    pub fn reset_circuit(&self, key: &str) -> bool {
        let Some(breaker) = self.entry(key).and_then(|entry| entry.breaker.clone()) else {
            return false;
        };
        breaker.reset();
        true
    }

    pub fn snapshot(&self) -> ExecutorSnapshot {
        self.counters.snapshot(self.rate_limiter.snapshot(), self.bulkhead.snapshot())
    }

    fn entry(&self, key: &str) -> Option<Arc<Entry<C>>> { self.entries.read().get(key).cloned() }

    /// Run `call` for `key` under its breaker, bulkhead and rate limit.
    ///
    /// `call` is only invoked once every stage has admitted the operation. `classify` turns the final
    /// result into the outcome the breaker is told about; it runs exactly once, on the result the caller
    /// is about to receive. Dropping the returned future cancels the operation at whatever stage it is in.
    pub async fn execute<T, E, Fut>(
        &self,
        key: &str,
        classify: impl FnOnce(&Result<T, E>) -> CallOutcome,
        call: impl FnOnce() -> Fut,
    ) -> Result<T, OutboundError<E>>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        self.run(key, None, classify, call).await
    }

    /// [`Self::execute`] that also stops when `cancel` fires, at any stage, with [`OutboundError::Cancelled`].
    pub async fn execute_cancellable<T, E, Fut>(
        &self,
        key: &str,
        cancel: &CancellationToken,
        classify: impl FnOnce(&Result<T, E>) -> CallOutcome,
        call: impl FnOnce() -> Fut,
    ) -> Result<T, OutboundError<E>>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        self.run(key, Some(cancel), classify, call).await
    }

    async fn run<T, E, Fut>(
        &self,
        key: &str,
        cancel: Option<&CancellationToken>,
        classify: impl FnOnce(&Result<T, E>) -> CallOutcome,
        call: impl FnOnce() -> Fut,
    ) -> Result<T, OutboundError<E>>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        Counters::bump(&self.counters.started);
        let mut in_flight = DropCounter { counters: &self.counters, finished: false };
        let result = self.run_stages(key, cancel, classify, call).await;
        in_flight.finished = true;
        match &result {
            Err(OutboundError::Inner(_)) | Ok(_) => {}
            Err(error) => self.counters.record_rejection(error),
        }
        result
    }

    async fn run_stages<T, E, Fut>(
        &self,
        key: &str,
        cancel: Option<&CancellationToken>,
        classify: impl FnOnce(&Result<T, E>) -> CallOutcome,
        call: impl FnOnce() -> Fut,
    ) -> Result<T, OutboundError<E>>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        let entry = self.entry(key).ok_or(OutboundError::UnknownKey)?;
        let started = Instant::now();
        let deadline = started.checked_add(entry.policy.deadline.as_duration()).unwrap_or(started + FAR_FUTURE);

        let mut admission = match cancel {
            Some(token) => tokio::select! {
                biased;
                () = token.cancelled() => return Err(OutboundError::Cancelled),
                admitted = self.admit(key, &entry, deadline) => admitted?,
            },
            None => self.admit(key, &entry, deadline).await?,
        };
        Counters::bump(&self.counters.admitted);

        let guarded = timeout_at(deadline, call());
        let timed = match cancel {
            Some(token) => tokio::select! {
                biased;
                () = token.cancelled() => {
                    admission.report(CallOutcome::Cancelled);
                    return Err(OutboundError::Cancelled);
                }
                finished = guarded => finished,
            },
            None => guarded.await,
        };
        match timed {
            Ok(result) => {
                let outcome = classify(&result);
                self.counters.record_outcome(outcome);
                admission.report(outcome);
                result.map_err(OutboundError::Inner)
            }
            Err(_elapsed) => {
                admission.report(CallOutcome::Transient);
                Err(OutboundError::DeadlineExceeded)
            }
        }
    }

    /// Take the circuit permit, then the bulkhead slot, then the rate-limit token.
    async fn admit<E>(&self, key: &str, entry: &Entry<C>, deadline: Instant) -> Result<Admission<C>, OutboundError<E>> {
        let started_ms = self.clock.now_ms().get();
        let policy = &entry.policy;
        let circuit = entry.breaker.as_ref().map(CircuitBreaker::acquire).transpose().map_err(circuit_open)?;
        let mut admission = Admission { circuit, slot: None };

        if policy.max_concurrency.is_some() {
            match self.bulkhead.acquire(key, deadline).await {
                Ok(slot) => admission.slot = Some(slot),
                Err(rejection) => {
                    admission.report(CallOutcome::LocalRejection);
                    return Err(match rejection {
                        BulkheadRejection::Saturated | BulkheadRejection::DeadlineExceeded => {
                            OutboundError::BulkheadFull
                        }
                        BulkheadRejection::Closed | BulkheadRejection::UnknownKey => OutboundError::UnknownKey,
                    });
                }
            }
        }

        if policy.rate_limit.is_some() {
            let budget = millis_until(deadline);
            let acquired = timeout_at(deadline, self.rate_limiter.acquire(key, budget)).await;
            let rejection = match acquired {
                Ok(Ok(_waited)) => None,
                Ok(Err(rejection)) => Some(match rejection {
                    RateLimitRejection::Exhausted { retry_after } => {
                        OutboundError::RateLimited { retry_after: Some(retry_after) }
                    }
                    RateLimitRejection::WaitTooLong { required, .. } => {
                        OutboundError::RateLimited { retry_after: Some(required) }
                    }
                    RateLimitRejection::QueueFull => OutboundError::RateLimited { retry_after: None },
                    RateLimitRejection::UnknownKey => OutboundError::UnknownKey,
                }),
                Err(_elapsed) => Some(OutboundError::DeadlineExceeded),
            };
            if let Some(error) = rejection {
                admission.report(CallOutcome::LocalRejection);
                return Err(error);
            }
        }

        if let Some(breaker) = entry.breaker.as_ref() {
            let queued_ms = self.clock.now_ms().get().saturating_sub(started_ms);
            if queued_ms >= policy.recheck_after.get() {
                Counters::bump(&self.counters.queue_rechecks);
                // Return the old permit before asking again: a half-open probe slot it holds would
                // otherwise reject its own replacement.
                drop(admission.circuit.take());
                admission.circuit = Some(breaker.acquire().map_err(circuit_open)?);
            }
        }

        if Instant::now() >= deadline {
            admission.report(CallOutcome::LocalRejection);
            return Err(OutboundError::DeadlineExceeded);
        }
        Ok(admission)
    }
}

fn circuit_open<E>(rejection: crate::utils::CircuitRejection) -> OutboundError<E> {
    OutboundError::CircuitOpen { retry_after: rejection.retry_after }
}

/// Whole milliseconds left until `deadline`, rounded down so a wait of that length never overshoots it.
fn millis_until(deadline: Instant) -> Millis {
    let remaining = deadline.saturating_duration_since(Instant::now());
    Millis::new(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
