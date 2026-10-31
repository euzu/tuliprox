use super::{ExecutorRegisterError, OutboundError, OutboundExecutor, OutboundPolicy, DEFAULT_OPERATION_DEADLINE};
use crate::utils::{request::RateLimitConfig, CallOutcome, CircuitBreakerPolicy, CircuitState, ManualClock};
use shared::model::Millis;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

const KEY: &str = "plugin-a";
const OPEN_FOR_MS: u64 = 30_000;

type Reply = Result<u32, &'static str>;
type Outcome = Result<u32, OutboundError<&'static str>>;

fn classify(result: &Reply) -> CallOutcome {
    match result {
        Ok(_) => CallOutcome::Success,
        Err("transient") => CallOutcome::Transient,
        Err(_) => CallOutcome::Permanent,
    }
}

fn breaker(failure_threshold: u32) -> CircuitBreakerPolicy {
    CircuitBreakerPolicy {
        failure_threshold,
        open_duration: Millis::new(OPEN_FOR_MS),
        probe_lease: Millis::new(120_000),
        half_open_probes: 1,
        ..CircuitBreakerPolicy::default()
    }
}

fn executor(policy: OutboundPolicy) -> (Arc<OutboundExecutor<ManualClock>>, ManualClock) {
    let clock = ManualClock::new(1_000);
    let executor = Arc::new(OutboundExecutor::with_clock(clock.clone(), 4));
    executor.register(KEY, policy).expect("register within the cap");
    (executor, clock)
}

async fn succeed(executor: &OutboundExecutor<ManualClock>, value: u32) -> Outcome {
    executor.execute(KEY, classify, || async move { Ok(value) }).await
}

async fn fail(executor: &OutboundExecutor<ManualClock>) -> Outcome {
    executor.execute(KEY, classify, || async { Err("transient") }).await
}

/// Let spawned tasks run until each is parked on something.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn consecutive_failures(executor: &OutboundExecutor<ManualClock>) -> u32 {
    executor.circuit_snapshot(KEY).expect("a breaker is registered").consecutive_failures
}

#[tokio::test(start_paused = true)]
async fn an_open_circuit_rejects_without_running_the_call() {
    let (executor, clock) = executor(OutboundPolicy::default().with_circuit_breaker(breaker(2)));

    assert_eq!(fail(&executor).await, Err(OutboundError::Inner("transient")));
    assert_eq!(fail(&executor).await, Err(OutboundError::Inner("transient")));
    assert_eq!(executor.circuit_snapshot(KEY).map(|s| s.state), Some(CircuitState::Open));

    let ran = AtomicBool::new(false);
    let rejected = executor
        .execute(KEY, classify, || {
            ran.store(true, Ordering::SeqCst);
            async { Ok(1) }
        })
        .await;
    assert_eq!(rejected, Err(OutboundError::CircuitOpen { retry_after: Some(Millis::new(OPEN_FOR_MS)) }));
    assert!(!ran.load(Ordering::SeqCst), "the call must not run behind an open circuit");

    // The cooldown elapses, one probe is admitted and its success closes the circuit.
    clock.advance(OPEN_FOR_MS);
    assert_eq!(succeed(&executor, 7).await, Ok(7));
    assert_eq!(executor.circuit_snapshot(KEY).map(|s| s.state), Some(CircuitState::Closed));

    let snapshot = executor.snapshot();
    assert_eq!((snapshot.started, snapshot.admitted), (4, 3));
    assert_eq!((snapshot.failed_transient, snapshot.succeeded, snapshot.rejected_circuit_open), (2, 1, 1));
}

#[tokio::test(start_paused = true)]
async fn a_half_open_circuit_admits_one_probe_among_concurrent_callers() {
    let (executor, clock) = executor(OutboundPolicy::default().with_circuit_breaker(breaker(1)));
    assert_eq!(fail(&executor).await, Err(OutboundError::Inner("transient")));
    clock.advance(OPEN_FOR_MS);

    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (executor, entered, gate) = (Arc::clone(&executor), Arc::clone(&entered), Arc::clone(&gate));
            tokio::spawn(async move {
                executor
                    .execute(KEY, classify, move || async move {
                        entered.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        Ok(1)
                    })
                    .await
            })
        })
        .collect();
    settle().await;
    assert_eq!(entered.load(Ordering::SeqCst), 1, "only the probe may reach the dependency");

    gate.notify_one();
    let mut admitted = 0;
    let mut rejected = 0;
    for handle in handles {
        match handle.await.expect("task joins") {
            Ok(1) => admitted += 1,
            Err(OutboundError::CircuitOpen { .. }) => rejected += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!((admitted, rejected), (1, 7));
    assert_eq!(executor.circuit_snapshot(KEY).map(|s| s.state), Some(CircuitState::Closed));
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_probe_frees_the_half_open_slot() {
    let (executor, clock) = executor(OutboundPolicy::default().with_circuit_breaker(breaker(1)));
    assert_eq!(fail(&executor).await, Err(OutboundError::Inner("transient")));
    clock.advance(OPEN_FOR_MS);

    let started = Arc::new(Notify::new());
    let probe = {
        let (executor, started) = (Arc::clone(&executor), Arc::clone(&started));
        tokio::spawn(async move {
            executor
                .execute(KEY, classify, move || async move {
                    started.notify_one();
                    std::future::pending::<Reply>().await
                })
                .await
        })
    };
    started.notified().await;
    assert_eq!(
        succeed(&executor, 2).await,
        Err(OutboundError::CircuitOpen { retry_after: Some(Millis::new(120_000)) }),
        "the probe is in flight"
    );

    probe.abort();
    assert!(probe.await.is_err());
    assert_eq!(succeed(&executor, 3).await, Ok(3), "the slot was released by the cancellation");
    assert_eq!(executor.snapshot().cancelled, 1);
}

#[tokio::test(start_paused = true)]
async fn a_queued_rate_limited_call_waits_for_its_token() {
    let policy =
        OutboundPolicy::default().with_rate_limit(RateLimitConfig::per_second(1, 1)).with_deadline(Millis::new(5_000));
    let (executor, clock) = executor(policy);

    assert_eq!(succeed(&executor, 1).await, Ok(1), "the burst token is free");

    let queued = {
        let executor = Arc::clone(&executor);
        tokio::spawn(async move { succeed(&executor, 2).await })
    };
    settle().await;
    assert!(!queued.is_finished());
    tokio::time::advance(Duration::from_millis(999)).await;
    settle().await;
    assert!(!queued.is_finished(), "the token is one millisecond away");

    clock.advance(1_000);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(queued.await.expect("task joins"), Ok(2));

    let snapshot = executor.snapshot();
    assert_eq!(snapshot.rate_limit.waited, 1);
    assert_eq!(snapshot.succeeded, 2);
}

#[tokio::test(start_paused = true)]
async fn a_token_that_arrives_after_the_deadline_is_refused_up_front() {
    let policy =
        OutboundPolicy::default().with_rate_limit(RateLimitConfig::per_second(1, 1)).with_deadline(Millis::new(500));
    let (executor, _clock) = executor(policy);
    assert_eq!(succeed(&executor, 1).await, Ok(1));

    let before = Instant::now();
    let refused = succeed(&executor, 2).await;
    assert_eq!(refused, Err(OutboundError::RateLimited { retry_after: Some(Millis::new(1_000)) }));
    assert_eq!(Instant::now(), before, "a refusal must not sleep towards a deadline it cannot meet");
    assert_eq!(consecutive_failures(&executor), 0, "a local rejection is not the dependency's failure");
    assert_eq!(executor.snapshot().rejected_rate_limited, 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_covers_the_call_and_counts_against_the_breaker() {
    let policy = OutboundPolicy::default().with_deadline(Millis::new(1_000));
    let (executor, _clock) = executor(policy);

    let before = Instant::now();
    let result = executor
        .execute(KEY, classify, || async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(1)
        })
        .await;
    assert_eq!(result, Err(OutboundError::DeadlineExceeded));
    assert_eq!(Instant::now() - before, Duration::from_secs(1));
    assert_eq!(consecutive_failures(&executor), 1);
    assert_eq!(executor.snapshot().deadline_exceeded, 1);
}

#[tokio::test(start_paused = true)]
async fn queue_wait_and_call_share_one_deadline() {
    let policy =
        OutboundPolicy::default().with_rate_limit(RateLimitConfig::per_second(1, 1)).with_deadline(Millis::new(1_500));
    let (executor, _clock) = executor(policy);
    assert_eq!(succeed(&executor, 1).await, Ok(1));

    // The token costs 1s of the 1.5s budget, so the call gets 0.5s and not a fresh 1.5s.
    let before = Instant::now();
    let result = executor
        .execute(KEY, classify, || async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(2)
        })
        .await;
    assert_eq!(result, Err(OutboundError::DeadlineExceeded));
    assert_eq!(Instant::now() - before, Duration::from_millis(1_500));
}

#[tokio::test(start_paused = true)]
async fn a_saturated_bulkhead_times_out_as_bulkhead_full() {
    let policy = OutboundPolicy::default().with_max_concurrency(1).with_deadline(Millis::new(1_000));
    let (executor, _clock) = executor(policy);

    let gate = Arc::new(Notify::new());
    let holder = {
        let (executor, gate) = (Arc::clone(&executor), Arc::clone(&gate));
        tokio::spawn(async move {
            executor
                .execute(KEY, classify, move || async move {
                    gate.notified().await;
                    Ok(1)
                })
                .await
        })
    };
    settle().await;

    let before = Instant::now();
    assert_eq!(succeed(&executor, 2).await, Err(OutboundError::BulkheadFull));
    assert_eq!(Instant::now() - before, Duration::from_secs(1), "it queued until the deadline");
    assert_eq!(consecutive_failures(&executor), 0);

    gate.notify_one();
    assert_eq!(holder.await.expect("task joins"), Ok(1));
    assert_eq!(succeed(&executor, 3).await, Ok(3), "the slot came back");

    let snapshot = executor.snapshot();
    assert_eq!((snapshot.rejected_bulkhead_full, snapshot.bulkhead.waited), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn dropping_a_queued_call_refunds_its_token_and_reports_a_cancellation() {
    let policy =
        OutboundPolicy::default().with_rate_limit(RateLimitConfig::per_second(1, 1)).with_deadline(Millis::new(10_000));
    let (executor, _clock) = executor(policy);
    assert_eq!(succeed(&executor, 1).await, Ok(1));

    let queued = {
        let executor = Arc::clone(&executor);
        tokio::spawn(async move { succeed(&executor, 2).await })
    };
    settle().await;
    queued.abort();
    assert!(queued.await.is_err());

    let snapshot = executor.snapshot();
    assert_eq!((snapshot.cancelled, snapshot.rate_limit.cancelled), (1, 1));
    assert_eq!(consecutive_failures(&executor), 0, "cancellation is not a failure");

    // The abandoned reservation was refunded, so the next caller waits one period, not two.
    let before = Instant::now();
    assert_eq!(succeed(&executor, 3).await, Ok(3));
    assert_eq!(Instant::now() - before, Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn a_cancellation_token_stops_a_queued_call() {
    let policy =
        OutboundPolicy::default().with_rate_limit(RateLimitConfig::per_second(1, 1)).with_deadline(Millis::new(10_000));
    let (executor, _clock) = executor(policy);
    assert_eq!(succeed(&executor, 1).await, Ok(1));

    let token = CancellationToken::new();
    let queued = {
        let (executor, token) = (Arc::clone(&executor), token.clone());
        tokio::spawn(async move {
            let ran = AtomicBool::new(false);
            let result = executor
                .execute_cancellable(KEY, &token, classify, || {
                    ran.store(true, Ordering::SeqCst);
                    async { Ok(2) }
                })
                .await;
            (result, ran.load(Ordering::SeqCst))
        })
    };
    settle().await;
    token.cancel();

    let (result, ran) = queued.await.expect("task joins");
    assert_eq!(result, Err(OutboundError::Cancelled));
    assert!(!ran);
    let snapshot = executor.snapshot();
    assert_eq!((snapshot.cancelled, snapshot.rate_limit.cancelled), (1, 1));
    assert_eq!(consecutive_failures(&executor), 0);
}

#[tokio::test(start_paused = true)]
async fn a_cancellation_token_stops_a_running_call() {
    let (executor, _clock) = executor(OutboundPolicy::default());
    let token = CancellationToken::new();
    let running = {
        let (executor, token) = (Arc::clone(&executor), token.clone());
        tokio::spawn(
            async move { executor.execute_cancellable(KEY, &token, classify, std::future::pending::<Reply>).await },
        )
    };
    settle().await;
    token.cancel();
    assert_eq!(running.await.expect("task joins"), Err(OutboundError::Cancelled));
    assert_eq!(consecutive_failures(&executor), 0);
}

#[tokio::test(start_paused = true)]
async fn the_breaker_is_asked_again_after_a_long_queue_wait() {
    let policy = OutboundPolicy::default()
        .with_circuit_breaker(breaker(1))
        .with_max_concurrency(1)
        .with_recheck_after(Millis::new(500))
        .with_deadline(Millis::new(60_000));
    let (executor, clock) = executor(policy);

    let gate = Arc::new(Notify::new());
    let first = {
        let (executor, gate) = (Arc::clone(&executor), Arc::clone(&gate));
        tokio::spawn(async move {
            executor
                .execute(KEY, classify, move || async move {
                    gate.notified().await;
                    Err("transient")
                })
                .await
        })
    };
    settle().await;

    let second_ran = Arc::new(AtomicBool::new(false));
    let second = {
        let (executor, second_ran) = (Arc::clone(&executor), Arc::clone(&second_ran));
        tokio::spawn(async move {
            executor
                .execute(KEY, classify, move || async move {
                    second_ran.store(true, Ordering::SeqCst);
                    Ok(2)
                })
                .await
        })
    };
    settle().await;

    // The second caller holds a closed-circuit permit and waits for the slot. Meanwhile the first fails
    // and trips the circuit.
    clock.advance(600);
    gate.notify_one();
    assert_eq!(first.await.expect("task joins"), Err(OutboundError::Inner("transient")));
    assert!(matches!(second.await.expect("task joins"), Err(OutboundError::CircuitOpen { .. })));
    assert!(!second_ran.load(Ordering::SeqCst));
    assert_eq!(executor.snapshot().queue_rechecks, 1);
}

#[tokio::test(start_paused = true)]
async fn a_short_queue_wait_does_not_recheck_the_breaker() {
    let policy = OutboundPolicy::default().with_circuit_breaker(breaker(1)).with_recheck_after(Millis::new(500));
    let (executor, _clock) = executor(policy);
    assert_eq!(succeed(&executor, 1).await, Ok(1));
    assert_eq!(executor.snapshot().queue_rechecks, 0);
}

#[tokio::test(start_paused = true)]
async fn caller_errors_pass_through_and_are_classified_once() {
    let (executor, _clock) = executor(OutboundPolicy::default().with_circuit_breaker(breaker(3)));
    let classified = AtomicUsize::new(0);
    let result: Outcome = executor
        .execute(
            KEY,
            |reply: &Reply| {
                classified.fetch_add(1, Ordering::SeqCst);
                classify(reply)
            },
            || async { Err("bad request") },
        )
        .await;
    assert_eq!(result.expect_err("the caller's error comes back").into_inner(), Some("bad request"));
    assert_eq!(classified.load(Ordering::SeqCst), 1);
    assert_eq!(executor.snapshot().failed_permanent, 1);
}

#[tokio::test(start_paused = true)]
async fn an_unregistered_key_is_rejected_and_removal_forgets_a_key() {
    let (executor, _clock) = executor(OutboundPolicy::default());
    let unknown: Outcome = executor.execute("other", classify, || async { Ok(1) }).await;
    assert_eq!(unknown, Err(OutboundError::UnknownKey));

    assert!(executor.remove(KEY));
    assert!(!executor.remove(KEY));
    assert_eq!(succeed(&executor, 1).await, Err(OutboundError::UnknownKey));
    assert_eq!(executor.snapshot().rejected_unknown_key, 2);
}

#[test]
fn registration_is_bounded_and_validates_the_breaker_policy() {
    let executor = OutboundExecutor::with_clock(ManualClock::new(0), 1);
    executor.register("a", OutboundPolicy::default()).expect("first key fits");
    assert_eq!(
        executor.register("b", OutboundPolicy::default()),
        Err(ExecutorRegisterError::KeyCapacity { max_keys: 1 })
    );
    executor.register("a", OutboundPolicy::default().with_max_concurrency(2)).expect("replacing is free");
    assert_eq!(executor.len(), 1);

    let invalid = CircuitBreakerPolicy { failure_threshold: 0, ..CircuitBreakerPolicy::default() };
    assert!(matches!(
        executor.register("a", OutboundPolicy::default().with_circuit_breaker(invalid)),
        Err(ExecutorRegisterError::InvalidCircuitPolicy(_))
    ));
}

#[test]
fn the_default_policy_has_a_breaker_and_no_queues() {
    let policy = OutboundPolicy::default();
    assert!(policy.circuit_breaker.is_some() && policy.rate_limit.is_none() && policy.max_concurrency.is_none());
    assert_eq!(policy.deadline, DEFAULT_OPERATION_DEADLINE);
}
