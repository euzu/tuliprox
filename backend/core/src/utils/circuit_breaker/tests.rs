use super::{
    CallOutcome, CircuitBreaker, CircuitBreakerPolicy, CircuitPermit, CircuitPolicyError, CircuitState,
    CircuitTransition, CountedOutcomes, RejectionReason, ReportStatus, TransitionCause, MAX_HALF_OPEN_PROBES,
};
use crate::utils::ManualClock;
use parking_lot::Mutex;
use shared::model::Millis;
use std::sync::{Arc, Barrier};

const OPEN_MS: u64 = 1_000;
const LEASE_MS: u64 = 500;
const START_MS: u64 = 10_000;

fn policy() -> CircuitBreakerPolicy {
    CircuitBreakerPolicy {
        failure_threshold: 3,
        open_duration: Millis::new(OPEN_MS),
        probe_lease: Millis::new(LEASE_MS),
        half_open_probes: 1,
        counted: CountedOutcomes::default(),
    }
}

struct Fixture {
    clock: ManualClock,
    breaker: CircuitBreaker<ManualClock>,
    events: Arc<Mutex<Vec<CircuitTransition>>>,
}

fn fixture_with(policy: CircuitBreakerPolicy) -> Fixture {
    let clock = ManualClock::new(START_MS);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let breaker =
        CircuitBreaker::with_observer(policy, clock.clone(), Arc::new(move |event| sink.lock().push(*event))).unwrap();
    Fixture { clock, breaker, events }
}

fn fixture() -> Fixture { fixture_with(policy()) }

impl Fixture {
    fn fail(&self) { self.breaker.acquire().unwrap().report(CallOutcome::Transient); }

    fn trip(&self) {
        for _ in 0..self.breaker.snapshot().failure_threshold {
            self.fail();
        }
        assert_eq!(self.breaker.snapshot().state, CircuitState::Open);
    }

    /// Trip, then let the open duration elapse. The state reads `Open` until the
    /// next `acquire` moves it to `HalfOpen`.
    fn half_open(&self) {
        self.trip();
        self.clock.advance(OPEN_MS);
    }

    fn causes(&self) -> Vec<(CircuitState, CircuitState, TransitionCause)> {
        self.events.lock().iter().map(|e| (e.from, e.to, e.cause)).collect()
    }
}

#[test]
fn starts_closed_and_admits() {
    let f = fixture();
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Closed);
    assert_eq!(snapshot.consecutive_failures, 0);
    assert!(f.breaker.acquire().is_ok());
    assert_eq!(f.events.lock().len(), 0);
}

#[test]
fn policy_validation_rejects_every_degenerate_field() {
    let bad = |edit: fn(&mut CircuitBreakerPolicy)| {
        let mut p = policy();
        edit(&mut p);
        p.validate().unwrap_err()
    };
    assert_eq!(bad(|p| p.failure_threshold = 0), CircuitPolicyError::ZeroFailureThreshold);
    assert_eq!(bad(|p| p.open_duration = Millis::new(0)), CircuitPolicyError::ZeroOpenDuration);
    assert_eq!(bad(|p| p.probe_lease = Millis::new(0)), CircuitPolicyError::ZeroProbeLease);
    assert_eq!(bad(|p| p.half_open_probes = 0), CircuitPolicyError::ProbeLimitOutOfRange);
    assert_eq!(bad(|p| p.half_open_probes = MAX_HALF_OPEN_PROBES + 1), CircuitPolicyError::ProbeLimitOutOfRange);
    assert!(
        CircuitBreaker::new(CircuitBreakerPolicy { failure_threshold: 0, ..policy() }, ManualClock::new(0)).is_err()
    );
    assert!(CircuitBreakerPolicy::default().validate().is_ok());
}

#[test]
fn default_policy_counts_failures_but_not_cancellation_or_local_rejection() {
    let p = CircuitBreakerPolicy::default();
    assert!(p.counts(CallOutcome::Transient));
    assert!(p.counts(CallOutcome::RateLimited));
    assert!(p.counts(CallOutcome::Permanent));
    assert!(!p.counts(CallOutcome::LocalRejection));
    assert!(!p.counts(CallOutcome::Cancelled));
    assert!(!p.counts(CallOutcome::Success));
    assert!(!CountedOutcomes::NONE.with(CallOutcome::Success).contains(CallOutcome::Success));
}

#[test]
fn closed_to_open_at_threshold_only() {
    let f = fixture();
    f.fail();
    f.fail();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.breaker.snapshot().consecutive_failures, 2);
    f.fail();
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Open);
    assert_eq!(snapshot.diagnostics.total_opens, 1);
    assert_eq!(snapshot.diagnostics.opened_at, Some(Millis::new(START_MS)));
    assert_eq!(f.causes(), [(CircuitState::Closed, CircuitState::Open, TransitionCause::ThresholdReached)]);
}

#[test]
fn success_resets_the_streak() {
    let f = fixture();
    f.fail();
    f.fail();
    f.breaker.acquire().unwrap().report(CallOutcome::Success);
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.diagnostics.first_failure_at, None);
    f.fail();
    f.fail();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
}

#[test]
fn uncounted_outcomes_neither_advance_nor_reset_the_streak() {
    let f = fixture();
    f.fail();
    f.fail();
    let a = f.breaker.acquire().unwrap().report(CallOutcome::Cancelled);
    let b = f.breaker.acquire().unwrap().report(CallOutcome::LocalRejection);
    assert_eq!((a, b), (ReportStatus::Neutral, ReportStatus::Neutral));
    assert_eq!(f.breaker.snapshot().consecutive_failures, 2);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
}

#[test]
fn dropping_a_closed_permit_is_a_neutral_cancellation() {
    let f = fixture();
    f.fail();
    drop(f.breaker.acquire().unwrap());
    assert_eq!(f.breaker.snapshot().consecutive_failures, 1);
}

#[test]
fn a_policy_can_opt_in_to_counting_a_class() {
    let counted = CountedOutcomes::NONE.with(CallOutcome::LocalRejection);
    let f = fixture_with(CircuitBreakerPolicy { failure_threshold: 1, counted, ..policy() });
    assert_eq!(f.breaker.acquire().unwrap().report(CallOutcome::Transient), ReportStatus::Neutral);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    f.breaker.acquire().unwrap().report(CallOutcome::LocalRejection);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Open);
    assert_eq!(counted.without(CallOutcome::LocalRejection), CountedOutcomes::NONE);
}

#[test]
fn every_counted_class_trips_the_breaker() {
    for outcome in [CallOutcome::Transient, CallOutcome::RateLimited, CallOutcome::Permanent] {
        let f = fixture_with(CircuitBreakerPolicy { failure_threshold: 1, ..policy() });
        f.breaker.acquire().unwrap().report(outcome);
        let snapshot = f.breaker.snapshot();
        assert_eq!(snapshot.state, CircuitState::Open, "{outcome:?}");
        assert_eq!(snapshot.diagnostics.last_failure_kind, Some(outcome));
    }
}

#[test]
fn open_rejects_with_retry_after_and_counts_rejections() {
    let f = fixture();
    f.trip();
    f.clock.advance(400);
    let rejection = f.breaker.acquire().unwrap_err();
    assert_eq!(rejection.reason, RejectionReason::Open);
    assert_eq!(rejection.retry_after, Some(Millis::new(600)));
    assert!(f.breaker.acquire().is_err());
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.diagnostics.rejected_calls, 2);
    assert_eq!(snapshot.retry_after, Some(Millis::new(600)));
    assert_eq!(snapshot.state, CircuitState::Open);
}

#[test]
fn open_to_half_open_after_the_open_duration_and_snapshot_is_read_only() {
    let f = fixture();
    f.trip();
    f.clock.advance(OPEN_MS - 1);
    assert!(f.breaker.acquire().is_err());
    f.clock.advance(1);

    let before = f.breaker.snapshot();
    assert_eq!(before.state, CircuitState::Open);
    assert_eq!(before.retry_after, Some(Millis::new(0)));
    assert_eq!(f.breaker.snapshot(), before, "taking a snapshot must not change anything");
    assert_eq!(f.events.lock().len(), 1);

    let permit = f.breaker.acquire().unwrap();
    assert!(permit.is_probe());
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);
    assert_eq!(f.breaker.snapshot().probes_in_flight, 1);
    assert_eq!(f.causes()[1], (CircuitState::Open, CircuitState::HalfOpen, TransitionCause::CooldownElapsed));
    drop(permit);
}

#[test]
fn half_open_probe_success_closes() {
    let f = fixture();
    f.half_open();
    let probe = f.breaker.acquire().unwrap();
    assert_eq!(probe.report(CallOutcome::Success), ReportStatus::Recorded);
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Closed);
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.probes_in_flight, 0);
    assert_eq!(f.causes().last().unwrap().2, TransitionCause::ProbeSucceeded);
    // Fully closed again: it needs a fresh full streak to reopen.
    f.fail();
    f.fail();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
}

#[test]
fn half_open_probe_failure_reopens_with_a_fresh_open_duration() {
    let f = fixture();
    f.half_open();
    f.breaker.acquire().unwrap().report(CallOutcome::Permanent);
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Open);
    assert_eq!(snapshot.diagnostics.total_opens, 2);
    assert_eq!(snapshot.retry_after, Some(Millis::new(OPEN_MS)));
    assert_eq!(f.causes().last().unwrap(), &(CircuitState::HalfOpen, CircuitState::Open, TransitionCause::ProbeFailed));
}

#[test]
fn half_open_uncounted_outcome_frees_the_slot_without_a_transition() {
    let f = fixture();
    f.half_open();
    assert_eq!(f.breaker.acquire().unwrap().report(CallOutcome::Cancelled), ReportStatus::Neutral);
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);
    assert_eq!(f.breaker.snapshot().probes_in_flight, 0);
    assert!(f.breaker.acquire().unwrap().is_probe());
}

#[test]
fn half_open_admits_exactly_one_probe_by_default() {
    let f = fixture();
    f.half_open();
    let probe = f.breaker.acquire().unwrap();
    let rejection = f.breaker.acquire().unwrap_err();
    assert_eq!(rejection.reason, RejectionReason::ProbeInFlight);
    assert_eq!(rejection.retry_after, Some(Millis::new(LEASE_MS)));
    drop(probe);
}

#[test]
fn dropping_a_probe_permit_unreported_frees_the_slot() {
    let f = fixture();
    f.half_open();
    drop(f.breaker.acquire().unwrap());
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);
    assert!(f.breaker.acquire().is_ok());
}

#[test]
fn probe_lease_expiry_frees_the_slot_and_a_late_report_is_stale() {
    let f = fixture();
    f.half_open();
    let lost = f.breaker.acquire().unwrap();
    f.clock.advance(LEASE_MS - 1);
    assert!(f.breaker.acquire().is_err(), "lease still held one millisecond before expiry");
    f.clock.advance(1);

    let replacement = f.breaker.acquire().unwrap();
    assert!(replacement.is_probe());
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);

    // The expired probe finally comes back with a success: it must not close
    // the circuit, and must not free the replacement's slot either.
    assert_eq!(lost.report(CallOutcome::Success), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);
    assert_eq!(f.breaker.snapshot().probes_in_flight, 1);
    assert_eq!(f.breaker.snapshot().diagnostics.stale_reports, 1);
    assert!(f.breaker.acquire().is_err());
    assert_eq!(replacement.report(CallOutcome::Success), ReportStatus::Recorded);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
}

#[test]
fn a_report_exactly_at_lease_expiry_is_stale() {
    let f = fixture();
    f.half_open();
    let probe = f.breaker.acquire().unwrap();
    f.clock.advance(LEASE_MS);
    assert_eq!(probe.report(CallOutcome::Success), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::HalfOpen);
}

#[test]
fn configurable_probe_count_is_honoured_and_first_result_decides() {
    let f = fixture_with(CircuitBreakerPolicy { half_open_probes: 3, ..policy() });
    f.half_open();
    let a = f.breaker.acquire().unwrap();
    let b = f.breaker.acquire().unwrap();
    let c = f.breaker.acquire().unwrap();
    assert!(f.breaker.acquire().is_err());
    assert_eq!(a.report(CallOutcome::Success), ReportStatus::Recorded);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    // The siblings belong to the half-open generation that just ended.
    assert_eq!(b.report(CallOutcome::Transient), ReportStatus::Stale);
    assert_eq!(c.report(CallOutcome::Success), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.breaker.snapshot().consecutive_failures, 0);
}

#[test]
fn stale_closed_permit_cannot_poison_the_next_closed_period() {
    let f = fixture();
    let slow = f.breaker.acquire().unwrap();
    f.half_open(); // trips (the generation moves on), then cools down
    f.breaker.acquire().unwrap().report(CallOutcome::Success); // closes again
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(slow.report(CallOutcome::Transient), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().consecutive_failures, 0);
}

#[test]
fn stale_permits_after_reset_are_no_ops() {
    let f = fixture();
    let old = f.breaker.acquire().unwrap();
    let generation = old.generation();
    f.fail();
    f.fail();
    f.breaker.reset();
    assert!(f.breaker.snapshot().generation > generation);
    assert_eq!(old.report(CallOutcome::Transient), ReportStatus::Stale);
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.diagnostics.stale_reports, 1);
    assert!(f.events.lock().is_empty(), "reset of an already-closed breaker is not a transition");
}

#[test]
fn stale_permits_after_reconfigure_are_no_ops() {
    let f = fixture();
    let old = f.breaker.acquire().unwrap();
    f.breaker.reconfigure(CircuitBreakerPolicy { failure_threshold: 1, ..policy() }).unwrap();
    assert_eq!(old.report(CallOutcome::Transient), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.breaker.snapshot().failure_threshold, 1);
}

#[test]
fn manual_reset_closes_an_open_circuit_and_keeps_lifetime_counters() {
    let f = fixture();
    f.trip();
    let _ = f.breaker.acquire();
    f.breaker.reset();
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Closed);
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.diagnostics.total_opens, 1);
    assert_eq!(snapshot.diagnostics.rejected_calls, 1);
    assert!(f.breaker.acquire().is_ok());
    assert_eq!(f.causes().last().unwrap(), &(CircuitState::Open, CircuitState::Closed, TransitionCause::ManualReset));
}

#[test]
fn manual_reset_during_half_open_invalidates_the_probe() {
    let f = fixture();
    f.half_open();
    let probe = f.breaker.acquire().unwrap();
    f.breaker.reset();
    assert_eq!(probe.report(CallOutcome::Transient), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
}

#[test]
fn force_open_never_times_out_and_is_not_counted_as_an_operational_open() {
    let f = fixture();
    f.breaker.force_open();
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Open);
    assert!(snapshot.forced_open);
    assert_eq!(snapshot.retry_after, None);
    assert_eq!(snapshot.diagnostics.total_opens, 0);
    f.clock.advance(OPEN_MS * 1_000);
    let rejection = f.breaker.acquire().unwrap_err();
    assert_eq!(rejection.reason, RejectionReason::ForcedOpen);
    assert_eq!(rejection.retry_after, None);
    f.breaker.reset();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert!(!f.breaker.snapshot().forced_open);
    assert!(f.breaker.acquire().is_ok());
}

#[test]
fn force_open_upgrades_an_operational_open_without_a_second_event() {
    let f = fixture();
    f.trip();
    f.breaker.force_open();
    assert_eq!(f.events.lock().len(), 1);
    f.clock.advance(OPEN_MS * 2);
    assert_eq!(f.breaker.acquire().unwrap_err().reason, RejectionReason::ForcedOpen);
}

#[test]
fn force_open_invalidates_in_flight_permits() {
    let f = fixture();
    let permit = f.breaker.acquire().unwrap();
    f.breaker.force_open();
    assert_eq!(permit.report(CallOutcome::Success), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Open);
}

#[test]
fn disabled_is_a_distinct_bypass_state() {
    let f = fixture();
    f.trip();
    f.breaker.disable();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Disabled);
    for _ in 0..10 {
        let permit = f.breaker.acquire().unwrap();
        assert!(!permit.is_probe());
        assert_eq!(permit.report(CallOutcome::Permanent), ReportStatus::Neutral);
    }
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.state, CircuitState::Disabled);
    assert_eq!(snapshot.consecutive_failures, 0);
    assert_eq!(snapshot.diagnostics.rejected_calls, 0);

    // Reset is an operational action and does not lift an administrative disable.
    f.breaker.reset();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Disabled);

    f.breaker.enable();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.causes().last().unwrap(), &(CircuitState::Disabled, CircuitState::Closed, TransitionCause::Enabled));
    f.breaker.enable(); // idempotent
    assert_eq!(f.events.lock().len(), 3);
}

#[test]
fn permit_issued_while_disabled_is_stale_once_enabled() {
    let f = fixture();
    f.breaker.disable();
    let permit = f.breaker.acquire().unwrap();
    f.breaker.enable();
    assert_eq!(permit.report(CallOutcome::Transient), ReportStatus::Stale);
    assert_eq!(f.breaker.snapshot().consecutive_failures, 0);
}

#[test]
fn reconfigure_resets_operational_state_but_keeps_administrative_states() {
    let f = fixture();
    f.trip();
    f.breaker.reconfigure(policy()).unwrap();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.causes().last().unwrap().2, TransitionCause::Reloaded);

    f.breaker.force_open();
    f.breaker.reconfigure(policy()).unwrap();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Open);
    assert!(f.breaker.snapshot().forced_open);

    f.breaker.disable();
    f.breaker.reconfigure(policy()).unwrap();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Disabled);
}

#[test]
fn reconfigure_with_an_invalid_policy_leaves_the_breaker_untouched() {
    let f = fixture();
    f.trip();
    let before = f.breaker.snapshot();
    let err = f.breaker.reconfigure(CircuitBreakerPolicy { probe_lease: Millis::new(0), ..policy() }).unwrap_err();
    assert_eq!(err, CircuitPolicyError::ZeroProbeLease);
    assert_eq!(f.breaker.snapshot(), before);
}

#[test]
fn transitions_are_emitted_only_when_the_state_changes_and_in_order() {
    use CircuitState::{Closed, HalfOpen, Open};
    use TransitionCause::{CooldownElapsed, ManualReset, ProbeSucceeded, ThresholdReached};

    let f = fixture();
    f.trip();
    f.breaker.reset();
    f.breaker.reset();
    f.half_open();
    f.breaker.acquire().unwrap().report(CallOutcome::Success);
    assert_eq!(
        f.causes(),
        [
            (Closed, Open, ThresholdReached),
            (Open, Closed, ManualReset),
            (Closed, Open, ThresholdReached),
            (Open, HalfOpen, CooldownElapsed),
            (HalfOpen, Closed, ProbeSucceeded),
        ]
    );
    let timestamps: Vec<_> = f.events.lock().iter().map(|e| e.at.get()).collect();
    assert!(timestamps.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn diagnostics_record_the_streak_and_the_last_failure() {
    let f = fixture();
    f.fail();
    f.clock.advance(5);
    f.breaker.acquire().unwrap().report(CallOutcome::RateLimited);
    f.clock.advance(5);
    f.breaker.acquire().unwrap().report(CallOutcome::Permanent);
    let diagnostics = f.breaker.snapshot().diagnostics;
    assert_eq!(diagnostics.first_failure_at, Some(Millis::new(START_MS)));
    assert_eq!(diagnostics.last_failure_at, Some(Millis::new(START_MS + 10)));
    assert_eq!(diagnostics.last_failure_kind, Some(CallOutcome::Permanent));
    assert_eq!(diagnostics.opened_at, Some(Millis::new(START_MS + 10)));
}

#[test]
fn counters_saturate() {
    let f = fixture_with(CircuitBreakerPolicy { failure_threshold: u32::MAX, ..policy() });
    {
        let mut inner = f.breaker.shared.inner.lock();
        inner.consecutive_failures = u32::MAX - 1;
        inner.diagnostics.stale_reports = u64::MAX;
        inner.diagnostics.rejected_calls = u64::MAX;
        inner.generation = u64::MAX - 1;
    }
    f.fail();
    f.fail();
    f.fail();
    assert_eq!(f.breaker.snapshot().consecutive_failures, u32::MAX);
    f.breaker.reset();
    f.breaker.reset();
    assert_eq!(f.breaker.snapshot().generation, u64::MAX);

    let stale = f.breaker.acquire().unwrap();
    f.breaker.force_open();
    let _ = stale.report(CallOutcome::Success);
    let _ = f.breaker.acquire();
    let diagnostics = f.breaker.snapshot().diagnostics;
    assert_eq!(diagnostics.stale_reports, u64::MAX);
    assert_eq!(diagnostics.rejected_calls, u64::MAX);
}

#[test]
fn total_opens_saturates() {
    let f = fixture_with(CircuitBreakerPolicy { failure_threshold: 1, ..policy() });
    f.breaker.shared.inner.lock().diagnostics.total_opens = u64::MAX;
    f.fail();
    assert_eq!(f.breaker.snapshot().state, CircuitState::Open);
    assert_eq!(f.breaker.snapshot().diagnostics.total_opens, u64::MAX);
}

#[test]
fn clock_overflow_saturates_instead_of_panicking() {
    let f = fixture();
    f.clock.set(u64::MAX - 10);
    f.trip();
    assert_eq!(f.breaker.snapshot().retry_after, Some(Millis::new(10)));
    f.clock.set(u64::MAX);
    assert!(f.breaker.acquire().unwrap().is_probe());
}

#[test]
fn a_clock_that_moves_backwards_does_not_underflow() {
    let f = fixture();
    f.trip();
    f.clock.set(1);
    assert_eq!(f.breaker.acquire().unwrap_err().retry_after, Some(Millis::new(START_MS + OPEN_MS - 1)));
}

#[test]
fn many_threads_acquiring_under_half_open_yield_exactly_one_probe() {
    const THREADS: usize = 64;
    let f = fixture();
    f.half_open();
    let barrier = Barrier::new(THREADS);
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    f.breaker.acquire()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let admitted = results.iter().filter(|r| r.as_ref().is_ok_and(CircuitPermit::is_probe)).count();
    assert_eq!(admitted, 1);
    assert!(results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .all(|rejection| rejection.reason == RejectionReason::ProbeInFlight));
    let snapshot = f.breaker.snapshot();
    assert_eq!(snapshot.probes_in_flight, 1);
    assert_eq!(snapshot.diagnostics.rejected_calls, u64::try_from(THREADS - 1).unwrap());
    // Exactly one HalfOpen event despite 64 racing callers.
    assert_eq!(f.causes().iter().filter(|c| c.1 == CircuitState::HalfOpen).count(), 1);
}

#[test]
fn many_threads_under_half_open_never_exceed_the_configured_probe_count() {
    const THREADS: usize = 64;
    let f = fixture_with(CircuitBreakerPolicy { half_open_probes: 4, ..policy() });
    f.half_open();
    let barrier = Barrier::new(THREADS);
    let permits: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    f.breaker.acquire().ok()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(permits.iter().flatten().count(), 4);
}

#[test]
fn many_threads_racing_probe_reports_close_the_circuit_exactly_once() {
    const THREADS: usize = 16;
    let f = fixture_with(CircuitBreakerPolicy { half_open_probes: u32::try_from(THREADS).unwrap(), ..policy() });
    f.half_open();
    let probes: Vec<_> = (0..THREADS).map(|_| f.breaker.acquire().unwrap()).collect();
    let barrier = Barrier::new(THREADS);
    let statuses: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = probes
            .into_iter()
            .map(|probe| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    probe.report(CallOutcome::Success)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(statuses.iter().filter(|s| **s == ReportStatus::Recorded).count(), 1);
    assert_eq!(statuses.iter().filter(|s| **s == ReportStatus::Stale).count(), THREADS - 1);
    assert_eq!(f.breaker.snapshot().state, CircuitState::Closed);
    assert_eq!(f.causes().iter().filter(|c| c.1 == CircuitState::Closed).count(), 1);
}
