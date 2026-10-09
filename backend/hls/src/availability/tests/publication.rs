use super::*;

#[tokio::test]
async fn preemption_does_not_fetch_another_origin_manifest() {
    let fixture = post_refresh_terminal_fixture("runtime-preemption-no-origin", true).await;
    let refresh_before = fixture.session.read().await.origin_refresh.clone();

    let _ = commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    assert_eq!(fixture.session.read().await.origin_refresh, refresh_before);
}

#[tokio::test]
async fn first_committed_custom_reason_is_immutable() {
    let fixture = post_refresh_terminal_fixture("runtime-first-reason", true).await;
    let (_, first) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    let outcome = commit_hls_runtime_custom_tail(
        fixture.ctx.clone(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(&fixture.session),
            proxy_session_id: fixture.proxy_session_id.clone(),
            lease_id: fixture.lease_id.clone(),
            reason: HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            now_ms: fixture.now_ms.saturating_add(1),
        },
    )
    .await;
    let replay = wait_for_runtime_custom_plan(&fixture).await;

    assert_eq!(outcome, HlsRuntimeCustomTailOutcome::AlreadyCommitted);
    assert_eq!(replay.reason, HlsRuntimeCustomTailReason::LowPriorityPreempted);
    assert_eq!(replay.generation, first.generation);
    assert_eq!(segment_bytes(&replay, 0), segment_bytes(&first, 0));
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_transient_lock_busy_retries_regular_evaluation() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(
        &mut leases,
        &proxy_session_id,
        "availability-transient",
        pressure_manifest_at(0, 8_000),
        1_000,
    );
    let attempts = std::cell::Cell::new(0_usize);
    let clock_calls = std::cell::Cell::new(0_usize);

    let access = retry_availability_state_access(|| {
        let attempt = attempts.get();
        attempts.set(attempt.saturating_add(1));
        let access = if attempt == 0 {
            HlsCriticalHandoffStateAccess::LockBusy
        } else {
            HlsCriticalHandoffStateAccess::Acquired(evaluate_and_commit_session_recovery_pressure_in_snapshot(
                &mut leases,
                &mut session,
                &proxy_session_id,
                atomic_pressure_policy(),
                || {
                    assert_eq!(attempts.get(), 2);
                    clock_calls.set(clock_calls.get().saturating_add(1));
                    101
                },
            ))
        };
        std::future::ready(access)
    })
    .await;

    let evidence = availability_snapshot_or_contention(access)
        .expect("transient contention must reach the regular snapshot evaluation")
        .expect("the active lease supplies recovery evidence");
    assert_eq!(evidence.timing_seed.target_duration_ms, 8_000);
    assert_eq!(attempts.get(), 2);
    assert_eq!(clock_calls.get(), 1);
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_exhausted_contention_is_typed() {
    let attempts = std::cell::Cell::new(0_usize);
    let access: HlsCriticalHandoffStateAccess<u8> = retry_availability_state_access(|| {
        attempts.set(attempts.get().saturating_add(1));
        std::future::ready(HlsCriticalHandoffStateAccess::LockBusy)
    })
    .await;

    let outcome = availability_snapshot_or_contention(access)
        .expect_err("exhausted contention must remain a typed endpoint outcome");

    assert_eq!(attempts.get(), HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS);
    assert_eq!(outcome, HlsAvailabilitySnapshotAccessError::StateContention);
}

#[test]
fn hls_manifest_acceptance_directive_samples_time_inside_snapshot_scope() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(
        &mut leases,
        &proxy_session_id,
        "availability-clock",
        pressure_manifest_at(0, 8_000),
        150,
    );
    let clock_calls = std::cell::Cell::new(0_usize);

    let evidence = evaluate_and_commit_session_recovery_pressure_in_snapshot(
        &mut leases,
        &mut session,
        &proxy_session_id,
        atomic_pressure_policy(),
        || {
            clock_calls.set(clock_calls.get().saturating_add(1));
            200
        },
    );

    assert_eq!(clock_calls.get(), 1);
    assert!(evidence.is_none(), "the snapshot-local clock must exclude the lease expired at evaluation time");
}
