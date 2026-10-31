//! A clock-injected, consecutive-failure circuit breaker.
//!
//! # Shape
//!
//! A caller asks [`CircuitBreaker::acquire`] for a [`CircuitPermit`] before it
//! talks to a dependency, performs the **whole** operation (retries included -
//! the breaker observes the final result, not each attempt), and reports the
//! [`CallOutcome`] through [`CircuitPermit::report`]. A permit that is dropped
//! without a report is reported as [`CallOutcome::Cancelled`], so a cancelled
//! future can never leak a half-open probe slot.
//!
//! ```text
//!            threshold consecutive counted failures
//!   Closed ------------------------------------------> Open
//!     ^                                                  |
//!     |  probe succeeded            open duration passed |
//!     +------------------- HalfOpen <--------------------+
//!                              |
//!                              +--- probe failed ---> Open
//! ```
//!
//! [`CircuitState::Disabled`] is a separate, administrative state. It is not an
//! operationally open circuit: a disabled breaker admits every call, records
//! nothing and counts no rejections.
//!
//! # Generations
//!
//! Every state change and every administrative action starts a new generation,
//! and a permit carries the generation it was issued in. Reporting through a
//! permit from an older generation is a no-op (counted in
//! [`CircuitDiagnostics::stale_reports`]). That makes a result arriving after
//! [`reset`](CircuitBreaker::reset), [`reconfigure`](CircuitBreaker::reconfigure)
//! or a trip harmless: it describes a world that no longer exists.
//!
//! # Half-open probes
//!
//! [`CircuitBreakerPolicy::half_open_probes`] (default `1`) bounds concurrent
//! probes. Each probe holds a lease of [`CircuitBreakerPolicy::probe_lease`]; a
//! probe that has neither reported nor been dropped when its lease lapses frees
//! its slot, stays in `HalfOpen`, and any late report through it is stale. The
//! first successful probe closes the circuit; the first counted failure reopens
//! it.
//!
//! # Transitions
//!
//! There is no metrics recorder in this crate yet, so transitions are delivered
//! to an injected observer ([`CircuitBreaker::with_observer`]). It is called
//! **after** the internal lock is released, only when the state actually
//! changes, and may therefore interleave across threads; each event carries its
//! own timestamp.
//!
//! # Diagnostics
//!
//! Only closed-set enums, timestamps and saturating counters are kept. No
//! error text, URL or header ever enters the breaker, so there is nothing to
//! sanitize and the memory is bounded.
//!
//! # Deliberately absent
//!
//! No rolling failure ratio and no persistence. The state is in memory and
//! starts `Closed`.

use crate::utils::{Clock, SystemClock};
use parking_lot::Mutex;
use shared::model::Millis;
use std::sync::Arc;

/// Upper bound for [`CircuitBreakerPolicy::half_open_probes`].
pub const MAX_HALF_OPEN_PROBES: u32 = 16;

/// The classified final result of one complete operation.
///
/// The classification is shared: the breaker, the retry policy and the metrics
/// all speak it. Which classes count against a breaker is a
/// [`CircuitBreakerPolicy`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOutcome {
    Success,
    /// Connection failure, timeout, retryable 5xx.
    Transient,
    /// The dependency asked us to slow down (429 and equivalents).
    RateLimited,
    /// Failure that retrying will not fix.
    Permanent,
    /// Rejected locally (validation, size limit, queue full). Says nothing
    /// about the dependency.
    LocalRejection,
    /// The caller gave up. Says nothing about the dependency.
    Cancelled,
}

impl CallOutcome {
    #[inline]
    pub const fn is_success(self) -> bool { matches!(self, Self::Success) }

    const fn bit(self) -> u8 {
        match self {
            Self::Success => 1,
            Self::Transient => 1 << 1,
            Self::RateLimited => 1 << 2,
            Self::Permanent => 1 << 3,
            Self::LocalRejection => 1 << 4,
            Self::Cancelled => 1 << 5,
        }
    }
}

/// The set of failure classes that count towards opening the circuit.
///
/// [`CallOutcome::Success`] is never part of the set; it always resets the
/// streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountedOutcomes(u8);

impl CountedOutcomes {
    /// Counts nothing.
    pub const NONE: Self = Self(0);

    #[must_use]
    pub const fn with(self, outcome: CallOutcome) -> Self {
        if outcome.is_success() {
            self
        } else {
            Self(self.0 | outcome.bit())
        }
    }

    #[must_use]
    pub const fn without(self, outcome: CallOutcome) -> Self { Self(self.0 & !outcome.bit()) }

    #[inline]
    pub const fn contains(self, outcome: CallOutcome) -> bool { self.0 & outcome.bit() != 0 }
}

impl Default for CountedOutcomes {
    /// Transient, rate-limited and permanent failures count; local rejections
    /// and cancellations do not.
    fn default() -> Self {
        Self::NONE.with(CallOutcome::Transient).with(CallOutcome::RateLimited).with(CallOutcome::Permanent)
    }
}

/// Resolved breaker settings. Construction validates; nothing is silently
/// defaulted or clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitBreakerPolicy {
    /// Consecutive counted failures that open the circuit.
    pub failure_threshold: u32,
    /// How long the circuit stays open before admitting a probe.
    pub open_duration: Millis,
    /// How long a half-open probe may stay unreported before its slot is freed.
    pub probe_lease: Millis,
    /// Concurrent half-open probes, `1..=`[`MAX_HALF_OPEN_PROBES`].
    pub half_open_probes: u32,
    /// Which failure classes count.
    pub counted: CountedOutcomes,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_duration: Millis::new(30_000),
            probe_lease: Millis::new(10_000),
            half_open_probes: 1,
            counted: CountedOutcomes::default(),
        }
    }
}

impl CircuitBreakerPolicy {
    /// Whether `outcome` is a failure that counts under this policy.
    #[inline]
    pub const fn counts(&self, outcome: CallOutcome) -> bool { self.counted.contains(outcome) }

    /// # Errors
    /// Returns the first invalid field.
    pub const fn validate(&self) -> Result<(), CircuitPolicyError> {
        if self.failure_threshold == 0 {
            return Err(CircuitPolicyError::ZeroFailureThreshold);
        }
        if self.open_duration.get() == 0 {
            return Err(CircuitPolicyError::ZeroOpenDuration);
        }
        if self.probe_lease.get() == 0 {
            return Err(CircuitPolicyError::ZeroProbeLease);
        }
        if self.half_open_probes == 0 || self.half_open_probes > MAX_HALF_OPEN_PROBES {
            return Err(CircuitPolicyError::ProbeLimitOutOfRange);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CircuitPolicyError {
    #[error("circuit breaker failure threshold must be at least 1")]
    ZeroFailureThreshold,
    #[error("circuit breaker open duration must be at least 1ms")]
    ZeroOpenDuration,
    #[error("circuit breaker probe lease must be at least 1ms")]
    ZeroProbeLease,
    #[error("circuit breaker half-open probe limit must be within 1..=16")]
    ProbeLimitOutOfRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
    /// Administrative bypass. Distinct from `Open`.
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionCause {
    ThresholdReached,
    CooldownElapsed,
    ProbeSucceeded,
    ProbeFailed,
    ManualReset,
    ForcedOpen,
    Disabled,
    Enabled,
    Reloaded,
}

/// Delivered to the observer when, and only when, the state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitTransition {
    pub from: CircuitState,
    pub to: CircuitState,
    pub cause: TransitionCause,
    pub at: Millis,
}

pub type TransitionObserver = Arc<dyn Fn(&CircuitTransition) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// Open, waiting out the open duration.
    Open,
    /// Administratively forced open; stays open until reset.
    ForcedOpen,
    /// Half-open and every probe slot is leased.
    ProbeInFlight,
}

/// An admission refusal. Counted in [`CircuitDiagnostics::rejected_calls`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("circuit breaker rejected the call: {reason:?}")]
pub struct CircuitRejection {
    pub reason: RejectionReason,
    /// When a retry could next be admitted; `None` if that needs an
    /// administrative action.
    pub retry_after: Option<Millis>,
}

/// What [`CircuitPermit::report`] did with the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportStatus {
    /// Applied to the breaker (success, or a counted failure).
    Recorded,
    /// Valid permit, but the outcome does not move the breaker (an uncounted
    /// class, or a disabled breaker). A probe slot, if held, is freed.
    Neutral,
    /// Issued in an earlier generation, or its probe lease had lapsed. Ignored.
    Stale,
}

/// Bounded, sanitized diagnostics: closed-set kinds, timestamps, counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CircuitDiagnostics {
    pub last_failure_kind: Option<CallOutcome>,
    /// Start of the current failure streak; cleared when the streak ends.
    pub first_failure_at: Option<Millis>,
    pub last_failure_at: Option<Millis>,
    pub opened_at: Option<Millis>,
    /// Operational opens only; an administrative force-open is not counted.
    pub total_opens: u64,
    pub rejected_calls: u64,
    pub stale_reports: u64,
}

/// A read-only view; taking one never changes the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitSnapshot {
    /// The stored state. An `Open` circuit whose open duration has elapsed
    /// still reads `Open` here, with `retry_after` of zero, until the next
    /// [`CircuitBreaker::acquire`] moves it to `HalfOpen`.
    pub state: CircuitState,
    pub generation: u64,
    pub consecutive_failures: u32,
    pub failure_threshold: u32,
    pub forced_open: bool,
    /// Remaining open duration; only for an operationally open circuit.
    pub retry_after: Option<Millis>,
    /// Unexpired half-open probe leases.
    pub probes_in_flight: u32,
    pub diagnostics: CircuitDiagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitKind {
    Closed,
    Probe(u64),
    Bypass,
}

#[derive(Debug, Clone, Copy)]
struct ProbeSlot {
    id: u64,
    expires_at: u64,
}

struct Inner {
    policy: CircuitBreakerPolicy,
    state: CircuitState,
    generation: u64,
    forced_open: bool,
    /// Meaningful only for `Open` and not forced.
    reopen_at: u64,
    consecutive_failures: u32,
    probes: Vec<ProbeSlot>,
    next_probe_id: u64,
    diagnostics: CircuitDiagnostics,
}

type TransitionSlot = Option<CircuitTransition>;

impl Inner {
    fn new(policy: CircuitBreakerPolicy) -> Self {
        Self {
            policy,
            state: CircuitState::Closed,
            generation: 0,
            forced_open: false,
            reopen_at: 0,
            consecutive_failures: 0,
            probes: Vec::new(),
            next_probe_id: 0,
            diagnostics: CircuitDiagnostics::default(),
        }
    }

    /// Start a new generation in `to`, reporting a transition only if the state
    /// differs.
    fn enter(&mut self, to: CircuitState, cause: TransitionCause, now: u64, out: &mut TransitionSlot) {
        let from = self.state;
        self.generation = self.generation.saturating_add(1);
        self.probes.clear();
        self.state = to;
        match to {
            CircuitState::Closed | CircuitState::Disabled => {
                self.consecutive_failures = 0;
                self.diagnostics.first_failure_at = None;
                self.forced_open = false;
            }
            CircuitState::Open | CircuitState::HalfOpen => {}
        }
        if from != to {
            *out = Some(CircuitTransition { from, to, cause, at: Millis::new(now) });
        }
    }

    fn open_operationally(&mut self, cause: TransitionCause, now: u64, out: &mut TransitionSlot) {
        self.diagnostics.total_opens = self.diagnostics.total_opens.saturating_add(1);
        self.diagnostics.opened_at = Some(Millis::new(now));
        self.forced_open = false;
        self.reopen_at = now.saturating_add(self.policy.open_duration.get());
        self.enter(CircuitState::Open, cause, now, out);
    }

    fn record_failure(&mut self, now: u64, outcome: CallOutcome) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.diagnostics.last_failure_kind = Some(outcome);
        self.diagnostics.last_failure_at = Some(Millis::new(now));
        self.diagnostics.first_failure_at.get_or_insert(Millis::new(now));
    }

    fn purge_expired_probes(&mut self, now: u64) { self.probes.retain(|slot| now < slot.expires_at); }

    fn reject(&mut self, reason: RejectionReason, retry_after: Option<u64>) -> CircuitRejection {
        self.diagnostics.rejected_calls = self.diagnostics.rejected_calls.saturating_add(1);
        CircuitRejection { reason, retry_after: retry_after.map(Millis::new) }
    }

    fn admit(&mut self, now: u64, out: &mut TransitionSlot) -> Result<(u64, PermitKind), CircuitRejection> {
        if self.state == CircuitState::Open && !self.forced_open && now >= self.reopen_at {
            self.enter(CircuitState::HalfOpen, TransitionCause::CooldownElapsed, now, out);
        }
        match self.state {
            CircuitState::Disabled => Ok((self.generation, PermitKind::Bypass)),
            CircuitState::Closed => Ok((self.generation, PermitKind::Closed)),
            CircuitState::Open if self.forced_open => Err(self.reject(RejectionReason::ForcedOpen, None)),
            CircuitState::Open => {
                let remaining = self.reopen_at.saturating_sub(now);
                Err(self.reject(RejectionReason::Open, Some(remaining)))
            }
            CircuitState::HalfOpen => {
                self.purge_expired_probes(now);
                if self.probes.len() >= self.policy.half_open_probes as usize {
                    let soonest = self.probes.iter().map(|slot| slot.expires_at).min().unwrap_or(now);
                    return Err(self.reject(RejectionReason::ProbeInFlight, Some(soonest.saturating_sub(now))));
                }
                let id = self.next_probe_id;
                self.next_probe_id = self.next_probe_id.saturating_add(1);
                let expires_at = now.saturating_add(self.policy.probe_lease.get());
                self.probes.push(ProbeSlot { id, expires_at });
                Ok((self.generation, PermitKind::Probe(id)))
            }
        }
    }

    fn stale(&mut self) -> ReportStatus {
        self.diagnostics.stale_reports = self.diagnostics.stale_reports.saturating_add(1);
        ReportStatus::Stale
    }

    fn apply_report(
        &mut self,
        now: u64,
        generation: u64,
        kind: PermitKind,
        outcome: CallOutcome,
        out: &mut TransitionSlot,
    ) -> ReportStatus {
        if generation != self.generation {
            return self.stale();
        }
        match (self.state, kind) {
            (CircuitState::Disabled, PermitKind::Bypass) => ReportStatus::Neutral,
            (CircuitState::Closed, PermitKind::Closed) => {
                if outcome.is_success() {
                    self.consecutive_failures = 0;
                    self.diagnostics.first_failure_at = None;
                    ReportStatus::Recorded
                } else if self.policy.counts(outcome) {
                    self.record_failure(now, outcome);
                    if self.consecutive_failures >= self.policy.failure_threshold {
                        self.open_operationally(TransitionCause::ThresholdReached, now, out);
                    }
                    ReportStatus::Recorded
                } else {
                    ReportStatus::Neutral
                }
            }
            (CircuitState::HalfOpen, PermitKind::Probe(id)) => {
                self.purge_expired_probes(now);
                let Some(position) = self.probes.iter().position(|slot| slot.id == id) else {
                    return self.stale();
                };
                self.probes.swap_remove(position);
                if outcome.is_success() {
                    self.enter(CircuitState::Closed, TransitionCause::ProbeSucceeded, now, out);
                    ReportStatus::Recorded
                } else if self.policy.counts(outcome) {
                    self.record_failure(now, outcome);
                    self.open_operationally(TransitionCause::ProbeFailed, now, out);
                    ReportStatus::Recorded
                } else {
                    ReportStatus::Neutral
                }
            }
            _ => self.stale(),
        }
    }

    fn snapshot(&self, now: u64) -> CircuitSnapshot {
        let operationally_open = self.state == CircuitState::Open && !self.forced_open;
        let probes_in_flight = self.probes.iter().filter(|slot| now < slot.expires_at).count();
        CircuitSnapshot {
            state: self.state,
            generation: self.generation,
            consecutive_failures: self.consecutive_failures,
            failure_threshold: self.policy.failure_threshold,
            forced_open: self.forced_open,
            retry_after: operationally_open.then(|| Millis::new(self.reopen_at.saturating_sub(now))),
            probes_in_flight: u32::try_from(probes_in_flight).unwrap_or(u32::MAX),
            diagnostics: self.diagnostics,
        }
    }
}

struct Shared<C: Clock> {
    clock: C,
    observer: Option<TransitionObserver>,
    inner: Mutex<Inner>,
}

/// A consecutive-failure circuit breaker. Cloning shares the same breaker.
pub struct CircuitBreaker<C: Clock = SystemClock> {
    shared: Arc<Shared<C>>,
}

impl<C: Clock> Clone for CircuitBreaker<C> {
    fn clone(&self) -> Self { Self { shared: Arc::clone(&self.shared) } }
}

impl<C: Clock> std::fmt::Debug for CircuitBreaker<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker").field("snapshot", &self.snapshot()).finish()
    }
}

impl<C: Clock> CircuitBreaker<C> {
    /// # Errors
    /// Returns the first invalid policy field.
    pub fn new(policy: CircuitBreakerPolicy, clock: C) -> Result<Self, CircuitPolicyError> {
        Self::build(policy, clock, None)
    }

    /// Like [`Self::new`], with a transition observer; see the module docs for
    /// its delivery guarantees.
    ///
    /// # Errors
    /// Returns the first invalid policy field.
    pub fn with_observer(
        policy: CircuitBreakerPolicy,
        clock: C,
        observer: TransitionObserver,
    ) -> Result<Self, CircuitPolicyError> {
        Self::build(policy, clock, Some(observer))
    }

    fn build(
        policy: CircuitBreakerPolicy,
        clock: C,
        observer: Option<TransitionObserver>,
    ) -> Result<Self, CircuitPolicyError> {
        policy.validate()?;
        Ok(Self { shared: Arc::new(Shared { clock, observer, inner: Mutex::new(Inner::new(policy)) }) })
    }

    /// Run `f` under the lock with the current time, then deliver any
    /// transition after the lock is released.
    fn locked<R>(&self, f: impl FnOnce(&mut Inner, u64, &mut TransitionSlot) -> R) -> R {
        let mut transition = None;
        let result = {
            let mut inner = self.shared.inner.lock();
            // Read the clock under the lock so state and time cannot be
            // observed out of order by concurrent callers.
            let now = self.shared.clock.now_ms().get();
            f(&mut inner, now, &mut transition)
        };
        if let (Some(event), Some(observer)) = (transition, self.shared.observer.as_ref()) {
            observer(&event);
        }
        result
    }

    /// Ask to make one call.
    ///
    /// # Errors
    /// Returns a [`CircuitRejection`] while the circuit is open or every
    /// half-open probe slot is leased.
    pub fn acquire(&self) -> Result<CircuitPermit<C>, CircuitRejection> {
        let (generation, kind) = self.locked(Inner::admit)?;
        Ok(CircuitPermit { breaker: self.clone(), generation, kind, reported: false })
    }

    /// A read-only view of the breaker.
    pub fn snapshot(&self) -> CircuitSnapshot {
        let inner = self.shared.inner.lock();
        inner.snapshot(self.shared.clock.now_ms().get())
    }

    /// Return to `Closed` with a fresh generation, clearing the streak, probes
    /// and any force-open. Lifetime counters are kept. Does nothing while
    /// disabled; use [`Self::enable`].
    pub fn reset(&self) {
        self.locked(|inner, now, out| {
            if inner.state != CircuitState::Disabled {
                inner.enter(CircuitState::Closed, TransitionCause::ManualReset, now, out);
            }
        });
    }

    /// Open the circuit and keep it open until [`Self::reset`]. Force-open is
    /// administrative: it does not time out and survives
    /// [`Self::reconfigure`].
    pub fn force_open(&self) {
        self.locked(|inner, now, out| {
            inner.diagnostics.opened_at = Some(Millis::new(now));
            inner.enter(CircuitState::Open, TransitionCause::ForcedOpen, now, out);
            inner.forced_open = true;
        });
    }

    /// Bypass the breaker: every call is admitted and nothing is recorded.
    pub fn disable(&self) {
        self.locked(|inner, now, out| inner.enter(CircuitState::Disabled, TransitionCause::Disabled, now, out));
    }

    /// Leave `Disabled` for a fresh `Closed`. Does nothing otherwise.
    pub fn enable(&self) {
        self.locked(|inner, now, out| {
            if inner.state == CircuitState::Disabled {
                inner.enter(CircuitState::Closed, TransitionCause::Enabled, now, out);
            }
        });
    }

    /// Replace the policy and invalidate every outstanding permit.
    ///
    /// Operational state (`Closed`, `Open`, `HalfOpen`) resets to `Closed`; the
    /// administrative states `Disabled` and force-open are kept.
    ///
    /// # Errors
    /// Returns the first invalid policy field; the breaker is left untouched.
    pub fn reconfigure(&self, policy: CircuitBreakerPolicy) -> Result<(), CircuitPolicyError> {
        policy.validate()?;
        self.locked(|inner, now, out| {
            inner.policy = policy;
            if inner.state == CircuitState::Disabled || inner.forced_open {
                inner.generation = inner.generation.saturating_add(1);
                inner.probes.clear();
            } else {
                inner.enter(CircuitState::Closed, TransitionCause::Reloaded, now, out);
            }
        });
        Ok(())
    }

    fn report(&self, generation: u64, kind: PermitKind, outcome: CallOutcome) -> ReportStatus {
        self.locked(|inner, now, out| inner.apply_report(now, generation, kind, outcome, out))
    }
}

/// Admission to make one call. Report its [`CallOutcome`] exactly once.
///
/// Dropping the permit unreported reports [`CallOutcome::Cancelled`].
#[must_use = "report the outcome through `report`; dropping the permit reports a cancellation"]
pub struct CircuitPermit<C: Clock = SystemClock> {
    breaker: CircuitBreaker<C>,
    generation: u64,
    kind: PermitKind,
    reported: bool,
}

impl<C: Clock> CircuitPermit<C> {
    pub fn report(mut self, outcome: CallOutcome) -> ReportStatus {
        self.reported = true;
        self.breaker.report(self.generation, self.kind, outcome)
    }

    /// Whether this permit is a half-open probe.
    pub fn is_probe(&self) -> bool { matches!(self.kind, PermitKind::Probe(_)) }

    pub fn generation(&self) -> u64 { self.generation }
}

impl<C: Clock> Drop for CircuitPermit<C> {
    fn drop(&mut self) {
        if !self.reported {
            self.breaker.report(self.generation, self.kind, CallOutcome::Cancelled);
        }
    }
}

impl<C: Clock> std::fmt::Debug for CircuitPermit<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitPermit")
            .field("generation", &self.generation)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
