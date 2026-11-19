//! Reserve-driven member failover (R1-R5).
//!
//! One group-like session carries a lease whose published playlist is shorter than the READY
//! timeline behind it, so the guaranteed playback reserve decays exactly with the playback clock.
//! Every test drives the same evidence the workers see: a player request, origin media progress,
//! the recovery-pressure evaluation that records the reserve sample, and the terminal-reserve
//! evaluation the reevaluation worker runs.

use super::{runtime_fixtures::advance_post_refresh_fixture_playback, *};
use crate::{
    availability::{
        recovery_pressure::{hls_recovery_pressure_policy, lease_paused_for_member_failover, member_failover_required},
        terminal_cutover::member_failover_controlling_lease,
        HLS_MEMBER_FAILOVER_IDLE_TARGET_DURATIONS,
    },
    manager::HlsCriticalHandoffStateAccess,
    media_reserve::minimum_hls_startup_window_ms,
    member_health::{test_support::RecordingMemberRuntime, HlsMemberReserveSample},
    origin_account_runtime::HlsMemberUnhealthyReason,
};

const TARGET_DURATION_MS: u64 = 4_000;
const VISIBLE_SEGMENTS: u64 = 3;
const READY_SEGMENTS: u64 = 8;
const NOW_MS: u64 = 100_000;
/// The published 3 x 4 s playlist promises 12 s, exactly the `3 x T` startup window.
const STARTUP_WINDOW_MS: u64 = TARGET_DURATION_MS * VISIBLE_SEGMENTS;

struct MemberFailoverFixture {
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    runtime: Arc<RecordingMemberRuntime>,
}

fn ready_origin_body() -> String {
    use std::fmt::Write as _;
    let mut body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for proxy_seq in 0..READY_SEGMENTS {
        let _ = write!(body, "#EXTINF:4.0,\n{proxy_seq}.ts\n");
    }
    body
}

fn lease_manifest(visible_segments: u64) -> HlsLeaseManifestSnapshot {
    let segments = (0..visible_segments)
        .map(|proxy_seq| HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: TARGET_DURATION_MS,
            uri: format!("{proxy_seq}.ts").into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        })
        .collect::<Vec<_>>();
    let playlist_duration_ms = TARGET_DURATION_MS.saturating_mul(visible_segments);
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: NOW_MS,
        first_proxy_seq: 0,
        last_proxy_seq: visible_segments.saturating_sub(1),
        visible_segments: segments.into(),
        discontinuity_sequence: 0,
        target_duration_ms: TARGET_DURATION_MS,
        playlist_duration_ms,
        last_visible_media_end_ms: playlist_duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

async fn publish_member_failover_lease(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    name: &str,
    visible_segments: u64,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(format!("{name}-lease"));
    ctx.hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new(name, name),
            proxy_session_id.clone(),
            name.to_string(),
            "token".to_string(),
            1,
            "stream".to_string(),
            1,
            now_ms,
            600_000,
        ))
        .await;
    let publication = ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication_for_test(&lease_id, proxy_session_id, now_ms)
        .await
        .expect("member failover publication guard");
    assert!(ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            proxy_session_id,
            publication,
            lease_manifest(visible_segments),
            now_ms,
        )
        .await
        .is_committed());
    assert!(ctx
        .hls_proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 600_000, valid_window_ms: 600_000 },
        )
        .await
        .is_activated());
    lease_id
}

async fn member_failover_fixture(name: &str, visible_segments: u64) -> MemberFailoverFixture {
    let ctx = HlsCtx::for_test(Config::default());
    let (session, _) =
        ctx.hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, name), b"secret", NOW_MS).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&ready_origin_body(), "http://origin.example/live/index.m3u8")
    else {
        panic!("member failover origin manifest parses");
    };
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("member failover timeline applies");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: NOW_MS };
        }
        // A slow but healthy origin: the path is not degraded and media keeps progressing.
        session.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
        session.origin_control.record_media_progress(NOW_MS, TARGET_DURATION_MS);
        session.origin_control.target_duration_snapshot_ms = Some(TARGET_DURATION_MS);
    }
    let runtime = RecordingMemberRuntime::new(Some(RecordingMemberRuntime::default_policy()));
    session.write().await.origin_account_runtime = Some(runtime.clone());
    let lease_id = publish_member_failover_lease(&ctx, &proxy_session_id, name, visible_segments, NOW_MS).await;
    MemberFailoverFixture { ctx, session, proxy_session_id, lease_id, runtime }
}

impl MemberFailoverFixture {
    /// Starts the playback clock `elapsed_ms` before `NOW_MS`.
    async fn start_playback(&self, elapsed_ms: u64) {
        advance_post_refresh_fixture_playback(
            &self.ctx,
            &self.session,
            &self.proxy_session_id,
            &self.lease_id,
            elapsed_ms,
            NOW_MS,
        )
        .await;
    }

    /// Origin media progress: the slow origin keeps publishing, it just does not refill the horizon.
    async fn telemetry(&self, now_ms: u64) {
        self.session.write().await.origin_control.record_media_progress(now_ms, TARGET_DURATION_MS);
    }

    /// One player poll: a media request and the recovery-pressure evaluation that records the
    /// reserve sample of this round.
    async fn player_poll(&self, now_ms: u64) {
        let identity = self
            .ctx
            .hls_proxy
            .access_lease_response_snapshot(&self.lease_id, &self.proxy_session_id, now_ms)
            .await
            .and_then(|lease| lease.media_identity())
            .expect("live member failover lease identity");
        assert!(self
            .ctx
            .hls_proxy
            .record_access_lease_segment_request_started_if_identity_matches(
                &self.lease_id,
                &self.proxy_session_id,
                identity,
                1,
                now_ms,
            )
            .await
            .is_some());
        self.record_reserve_sample(now_ms).await;
    }

    /// One player poll without a media request: a paused lease.
    async fn idle_poll(&self, now_ms: u64) { self.record_reserve_sample(now_ms).await; }

    async fn record_reserve_sample(&self, now_ms: u64) {
        self.telemetry(now_ms).await;
        let policy = hls_recovery_pressure_policy(
            &self.ctx.hls_proxy,
            self.ctx.hls_proxy.origin_manifest_timeout_ms(),
            Some(u32::try_from(TARGET_DURATION_MS / 1_000).unwrap_or(u32::MAX)),
        );
        let proxy_session_id = self.proxy_session_id.clone();
        let recorded = self
            .ctx
            .hls_proxy
            .with_critical_handoff_state(&self.session, move |leases, session| {
                evaluate_and_commit_session_recovery_pressure(leases, session, &proxy_session_id, now_ms, policy)
            })
            .await;
        assert!(
            matches!(recorded, HlsCriticalHandoffStateAccess::Acquired(Some(_))),
            "the recovery-pressure evaluation records the reserve sample"
        );
    }

    /// The reevaluation worker's read-only reserve decision at `now_ms`; it never replaces the
    /// sample of the last player poll and records no origin progress of its own.
    async fn worker_wake(&self, now_ms: u64) -> Option<HlsAccessLeaseId> {
        member_failover_controlling_lease(&self.ctx, &self.session, &self.proxy_session_id, now_ms).await
    }

    async fn reserve(&self, now_ms: u64) -> HlsLeaseReserveSnapshot {
        let lease = self
            .ctx
            .hls_proxy
            .access_lease_response_snapshot(&self.lease_id, &self.proxy_session_id, now_ms)
            .await
            .expect("live member failover lease remains stored");
        let manifest = lease.last_manifest_snapshot.clone().expect("published member failover manifest");
        let ready_timeline = self.session.read().await.ready_timeline_snapshot(
            lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
            now_ms,
        );
        evaluate_lease_reserve(crate::HlsLeaseReserveInput {
            manifest: &manifest,
            cursor: &lease.playback_cursor,
            ready_timeline: &ready_timeline,
            now_ms,
            recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(0),
            origin_path_degraded: false,
            recovery_committed: false,
        })
    }
}

/// The group-session reserve owner is a real `TerminalReserve` worker. While it waits for its
/// reserve-derived wake instant it must hold no task permit, or `HLS_AVAILABILITY_REEVALUATION_CAPACITY`
/// parked sessions would starve every other reevaluation.
#[tokio::test(start_paused = true)]
async fn retained_reserve_owner_holds_no_task_permit_while_it_waits() {
    let fixture = member_failover_fixture("member-failover-parked-owner", VISIBLE_SEGMENTS).await;
    // The real worker reads its clock from the manager; anchor it to the paused clock of the fixture.
    fixture.ctx.hls_proxy.follow_tokio_clock_for_test(NOW_MS);
    fixture.start_playback(TARGET_DURATION_MS).await;
    fixture.player_poll(NOW_MS).await;
    let coordinator = fixture.ctx.hls_proxy.availability_reevaluations();
    let permits_before = coordinator.available_task_permits_for_test();

    assert_eq!(
        crate::availability::reevaluation::register_terminal_reserve_reevaluation(
            fixture.ctx.clone(),
            Arc::clone(&fixture.session),
        )
        .await,
        crate::HlsAvailabilityReevaluationRegistration::Scheduled
    );
    // Let the owner evaluate once and reach its reserve-derived wait.
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }

    assert_eq!(coordinator.owner_count(), 1, "one owner per session, also while it waits");
    assert_eq!(
        coordinator.available_task_permits_for_test(),
        permits_before,
        "a waiting reserve owner holds no task permit"
    );
}

/// A retained owner whose reserve has already reached the member trigger must not poll: the
/// trigger cannot ripen any further, so the evaluation requests the switch itself and the owner
/// waits for exhaustion. A wake instant that collapses to `now` re-runs one full terminal
/// evaluation per millisecond for the rest of the reserve.
#[tokio::test(start_paused = true)]
async fn retained_owner_at_the_member_window_evaluates_once_per_target_duration() {
    let fixture = member_failover_fixture("member-failover-due-trigger-wake", VISIBLE_SEGMENTS).await;
    fixture.ctx.hls_proxy.follow_tokio_clock_for_test(NOW_MS);
    // 24 s of playback leave 8 s of ready media, below the 12 s startup window, so the member
    // trigger is already due. The lease never requests, so no advancing playback proves the origin
    // gave up and no switch is requested.
    fixture.start_playback(6 * TARGET_DURATION_MS).await;
    let reserve_ms = fixture.reserve(NOW_MS).await.guaranteed_reserve_ms;
    assert_eq!(reserve_ms, 2 * TARGET_DURATION_MS);
    assert!(reserve_ms < STARTUP_WINDOW_MS, "the member trigger has already fired at this reserve");
    fixture.idle_poll(NOW_MS).await;

    let owner_key = fixture
        .ctx
        .hls_proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let ownership = crate::availability_reevaluation::HlsAvailabilityReevaluationOwnership::for_test(
        fixture.ctx.hls_proxy.availability_reevaluations(),
        fixture.proxy_session_id.clone(),
    );
    let mut cycle = HlsAvailabilityReevaluationCycle::new(NOW_MS);

    let mut evaluations = 0_u64;
    let mut wake_delays = Vec::new();
    loop {
        assert!(evaluations < 16, "the retained owner polls instead of waiting: {wake_delays:?}");
        let now_ms = fixture.ctx.hls_proxy.now_ms();
        let decision = evaluate_post_refresh_terminal_attempt(
            &fixture.ctx,
            &fixture.session,
            &owner_key,
            &ownership,
            HlsAvailabilityReevaluationMode::TerminalReserve,
            &mut cycle,
            now_ms,
        )
        .await;
        evaluations += 1;
        if !matches!(decision, HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true }) {
            break;
        }
        // The retained owner's between-cycle step, exactly as `run_hls_availability_reevaluation`
        // performs it: record the resolution, wait for its instant, evaluate again.
        let resolution = cycle.retained_owner_resolution(now_ms).expect("the live lease retains the owner");
        let outcome = crate::post_refresh_availability::wait_for_owner_resolution(&ownership, resolution, now_ms).await;
        assert_eq!(outcome, crate::availability_reevaluation::HlsAvailabilityOwnerWait::DeadlineReached);
        wake_delays.push(fixture.ctx.hls_proxy.now_ms().saturating_sub(now_ms));
    }

    // One evaluation now, one at exhaustion: the whole reserve in one wait.
    assert_eq!(wake_delays, vec![reserve_ms], "the due trigger waits for exhaustion, not for itself");
    assert_eq!(evaluations, 2, "one evaluation per reserve window, not one per millisecond");
    assert!(
        wake_delays.iter().all(|delay| *delay >= TARGET_DURATION_MS),
        "a retained owner evaluates at most once per target duration: {wake_delays:?}"
    );
}

fn member_failover_reserve(
    playback_position_ms: u64,
    guaranteed_reserve_ms: u64,
    playback_position_known: bool,
) -> HlsLeaseReserveSnapshot {
    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: playback_position_ms.saturating_add(guaranteed_reserve_ms),
        conservative_playback_position_ms: playback_position_ms,
        playback_position_known,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms: 0,
        transition_margin: HlsTransitionMarginMs::from_millis(TARGET_DURATION_MS),
        key_readiness_valid_until_ms: None,
        recovery_required: false,
        cutover_required: false,
    }
}

#[test]
fn member_failover_predicate_needs_the_reserve_at_or_below_the_startup_window() {
    assert!(!member_failover_reserve(0, STARTUP_WINDOW_MS + 1, true).member_failover_required(STARTUP_WINDOW_MS));
    assert!(member_failover_reserve(0, STARTUP_WINDOW_MS, true).member_failover_required(STARTUP_WINDOW_MS));
    assert!(member_failover_reserve(0, STARTUP_WINDOW_MS - 1, true).member_failover_required(STARTUP_WINDOW_MS));
    assert!(
        !member_failover_reserve(0, 0, false).member_failover_required(u64::MAX),
        "a lease without a playback position has consumed nothing"
    );
}

#[test]
fn member_failover_gate_needs_origin_evidence_beyond_the_reserve_window() {
    let reserve = member_failover_reserve(20_000, STARTUP_WINDOW_MS, true);
    let now_ms = 50_000;
    // Previous evaluation: 4 s less playback for 4 s more reserve.
    let fell_while_advancing =
        Some(HlsMemberReserveSample::for_test(16_000, STARTUP_WINDOW_MS + 4_000, now_ms - 1_000));

    // Origin path degraded: the reserve window alone is evidence enough.
    assert!(member_failover_required(reserve, STARTUP_WINDOW_MS, true, false, None, now_ms, TARGET_DURATION_MS));
    // Healthy path, no previous evaluation: nothing proves the origin stopped refilling.
    assert!(!member_failover_required(reserve, STARTUP_WINDOW_MS, false, false, None, now_ms, TARGET_DURATION_MS));
    // Healthy path, reserve fell while the position advanced: the origin cannot keep up.
    assert!(member_failover_required(
        reserve,
        STARTUP_WINDOW_MS,
        false,
        false,
        fell_while_advancing,
        now_ms,
        TARGET_DURATION_MS,
    ));
    // Same fall, but a local capacity repair owns the timeline.
    assert!(!member_failover_required(
        reserve,
        STARTUP_WINDOW_MS,
        false,
        true,
        fell_while_advancing,
        now_ms,
        TARGET_DURATION_MS,
    ));
    // Same fall, but the previous sample is older than the idle bound: it spans a pause.
    assert!(!member_failover_required(
        reserve,
        STARTUP_WINDOW_MS,
        false,
        false,
        Some(HlsMemberReserveSample::for_test(
            16_000,
            STARTUP_WINDOW_MS + 4_000,
            now_ms - TARGET_DURATION_MS * HLS_MEMBER_FAILOVER_IDLE_TARGET_DURATIONS - 1,
        )),
        now_ms,
        TARGET_DURATION_MS,
    ));
    // The reserve was already at the window: no drop, so nothing proves the origin gave up.
    assert!(!member_failover_required(
        reserve,
        STARTUP_WINDOW_MS,
        false,
        false,
        Some(HlsMemberReserveSample::for_test(16_000, STARTUP_WINDOW_MS, now_ms - 1_000)),
        now_ms,
        TARGET_DURATION_MS,
    ));
    // The reserve dropped through the window but the playback did not advance: a local collapse.
    assert!(!member_failover_required(
        reserve,
        STARTUP_WINDOW_MS,
        false,
        false,
        Some(HlsMemberReserveSample::for_test(20_000, STARTUP_WINDOW_MS + 4_000, now_ms - 1_000)),
        now_ms,
        TARGET_DURATION_MS,
    ));
}

#[test]
fn paused_lease_boundary_is_three_target_durations_without_a_request() {
    let idle_bound_ms = TARGET_DURATION_MS * HLS_MEMBER_FAILOVER_IDLE_TARGET_DURATIONS;
    let mut fresh = HlsLeasePlaybackCursor::default();
    let _ = fresh.record_request_started(1, NOW_MS - idle_bound_ms + 1);
    assert!(!lease_paused_for_member_failover(&fresh, TARGET_DURATION_MS, NOW_MS));
    let mut idle = HlsLeasePlaybackCursor::default();
    let _ = idle.record_request_started(1, NOW_MS - idle_bound_ms);
    assert!(
        lease_paused_for_member_failover(&idle, TARGET_DURATION_MS, NOW_MS),
        "the idle bound itself counts as paused"
    );
    assert!(
        lease_paused_for_member_failover(&HlsLeasePlaybackCursor::default(), TARGET_DURATION_MS, NOW_MS),
        "a lease without any playback request never counts"
    );
}

#[tokio::test]
async fn member_failover_switches_when_the_reserve_falls_under_a_steady_origin() {
    let fixture = member_failover_fixture("member-failover-steady-fall", VISIBLE_SEGMENTS).await;
    // 4 s of playback are behind the player; the origin keeps publishing but its READY horizon
    // stays where it is, so every player poll consumes one more target duration of hidden tail.
    fixture.start_playback(TARGET_DURATION_MS).await;
    assert_eq!(fixture.reserve(NOW_MS).await.guaranteed_reserve_ms, 28_000);

    for poll in 0..VISIBLE_SEGMENTS {
        let now_ms = NOW_MS + poll * TARGET_DURATION_MS;
        let reserve_ms = fixture.reserve(now_ms).await.guaranteed_reserve_ms;
        assert!(reserve_ms > STARTUP_WINDOW_MS, "poll {poll}: reserve {reserve_ms} is above the window");
        fixture.player_poll(now_ms).await;
        assert_eq!(fixture.worker_wake(now_ms).await, None, "poll {poll}: the reserve is still healthy");
    }

    // The player stops polling and the worker wakes at the reserve deadline of the last poll.
    // The origin is still publishing at that instant: it just publishes slower than playback.
    let trigger_ms = NOW_MS + (VISIBLE_SEGMENTS + 1) * TARGET_DURATION_MS;
    fixture.telemetry(trigger_ms).await;
    assert_eq!(fixture.reserve(trigger_ms).await.guaranteed_reserve_ms, STARTUP_WINDOW_MS);
    assert_eq!(fixture.worker_wake(trigger_ms).await, Some(fixture.lease_id.clone()));
    assert_eq!(
        fixture.reserve(trigger_ms).await.guaranteed_reserve_ms,
        STARTUP_WINDOW_MS,
        "the switch is requested while ready media is still ahead of the player"
    );
}

#[tokio::test]
async fn member_failover_needs_a_position_advance_next_to_the_reserve_drop() {
    let fixture = member_failover_fixture("member-failover-no-advance", VISIBLE_SEGMENTS).await;
    fixture.start_playback(VISIBLE_SEGMENTS * TARGET_DURATION_MS + TARGET_DURATION_MS).await;
    // 16 s of ready media ahead of the player: above the 12 s startup window.
    assert_eq!(fixture.reserve(NOW_MS).await.guaranteed_reserve_ms, 16_000);
    fixture.player_poll(NOW_MS).await;
    assert_eq!(fixture.worker_wake(NOW_MS).await, None, "the reserve is still above the window");

    // A hidden segment leaves the READY timeline while the playback clock stands still: the
    // reserve drops through the window, but no advancing playback proves an origin shortfall.
    fixture.session.write().await.segments.get_mut(&VISIBLE_SEGMENTS).expect("hidden segment").status =
        SegmentCacheStatus::Expired;
    assert_eq!(fixture.reserve(NOW_MS).await.guaranteed_reserve_ms, 0);
    assert_eq!(fixture.worker_wake(NOW_MS).await, None);

    // One target duration later the position advanced next to the same drop.
    assert_eq!(fixture.worker_wake(NOW_MS + TARGET_DURATION_MS).await, Some(fixture.lease_id.clone()));
}

#[tokio::test]
async fn member_failover_does_not_switch_when_a_local_capacity_repair_owns_the_timeline() {
    let fixture = member_failover_fixture("member-failover-capacity", VISIBLE_SEGMENTS).await;
    fixture.start_playback(TARGET_DURATION_MS).await;
    fixture.player_poll(NOW_MS).await;
    // The first hidden segment waits for local capacity: the READY horizon stops growing without
    // any origin evidence, so the reserve drops through the window on the next evaluations.
    fixture.session.write().await.segments.get_mut(&(VISIBLE_SEGMENTS + 2)).expect("third hidden segment").status =
        SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Demand, deferred_at_ms: NOW_MS };
    assert_eq!(fixture.reserve(NOW_MS).await.guaranteed_reserve_ms, 16_000);
    for poll in 0..=(VISIBLE_SEGMENTS + 1) {
        let now_ms = NOW_MS + poll * TARGET_DURATION_MS;
        assert_eq!(fixture.worker_wake(now_ms).await, None, "poll {poll}: local capacity recovery does not switch");
        fixture.player_poll(now_ms).await;
    }
    // The same lease under a degraded origin path does switch.
    fixture.session.write().await.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    assert_eq!(
        fixture.worker_wake(NOW_MS + (VISIBLE_SEGMENTS + 2) * TARGET_DURATION_MS).await,
        Some(fixture.lease_id.clone())
    );
}

#[tokio::test]
async fn member_failover_ignores_a_paused_lease_but_switches_for_an_advancing_one() {
    let paused = member_failover_fixture("member-failover-paused", VISIBLE_SEGMENTS).await;
    paused.start_playback(VISIBLE_SEGMENTS * TARGET_DURATION_MS + TARGET_DURATION_MS).await;
    assert_eq!(paused.reserve(NOW_MS).await.guaranteed_reserve_ms, 16_000);
    paused.idle_poll(NOW_MS).await;
    assert!(paused
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&paused.lease_id, &paused.proxy_session_id, NOW_MS)
        .await
        .is_some_and(|lease| lease_paused_for_member_failover(&lease.playback_cursor, TARGET_DURATION_MS, NOW_MS)));
    // The reserve drops through the window, but the player stopped requesting: the lease is paused.
    assert_eq!(paused.reserve(NOW_MS + TARGET_DURATION_MS).await.guaranteed_reserve_ms, STARTUP_WINDOW_MS);
    assert_eq!(paused.worker_wake(NOW_MS + TARGET_DURATION_MS).await, None);

    let advancing = member_failover_fixture("member-failover-advancing", VISIBLE_SEGMENTS).await;
    advancing.start_playback(VISIBLE_SEGMENTS * TARGET_DURATION_MS + TARGET_DURATION_MS).await;
    // Same lease and playback state, but the player keeps requesting: the drop is real evidence.
    advancing.player_poll(NOW_MS).await;
    assert_eq!(advancing.worker_wake(NOW_MS + TARGET_DURATION_MS).await, Some(advancing.lease_id.clone()));
}

#[tokio::test]
async fn member_failover_window_is_clamped_to_the_published_playlist() {
    // Two visible segments promise 8 s, not 3 x 4 s = 12 s.
    let short = member_failover_fixture("member-failover-clamped", 2).await;
    short.start_playback(5 * TARGET_DURATION_MS).await;
    assert_eq!(minimum_hls_startup_window_ms(TARGET_DURATION_MS, 2 * TARGET_DURATION_MS), 2 * TARGET_DURATION_MS);
    assert_eq!(short.reserve(NOW_MS).await.guaranteed_reserve_ms, STARTUP_WINDOW_MS);
    short.player_poll(NOW_MS).await;
    assert_eq!(short.worker_wake(NOW_MS).await, None, "12 s is above the 8 s the playlist promises");
    let clamped_ms = short.reserve(NOW_MS + TARGET_DURATION_MS).await.guaranteed_reserve_ms;
    assert_eq!(clamped_ms, 2 * TARGET_DURATION_MS);
    assert!(
        clamped_ms < minimum_hls_startup_window_ms(TARGET_DURATION_MS, TARGET_DURATION_MS * VISIBLE_SEGMENTS),
        "the clamped window is smaller than 3 x T"
    );
    assert_eq!(short.worker_wake(NOW_MS + TARGET_DURATION_MS).await, Some(short.lease_id.clone()));
}

#[tokio::test]
async fn member_failover_controlling_lease_is_the_smallest_reserve_of_an_advancing_playback() {
    let fixture = member_failover_fixture("member-failover-controlling", VISIBLE_SEGMENTS).await;
    let paused_lease = fixture.lease_id.clone();
    let advancing_lease = publish_member_failover_lease(
        &fixture.ctx,
        &fixture.proxy_session_id,
        "member-failover-controlling-advancing",
        VISIBLE_SEGMENTS,
        NOW_MS,
    )
    .await;

    // Lease A is paused one startup window before exhaustion: it has the smallest reserve.
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &paused_lease,
        5 * TARGET_DURATION_MS,
        NOW_MS,
    )
    .await;
    // Lease B plays behind with 16 s of reserve and asked for media just now.
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &advancing_lease,
        4 * TARGET_DURATION_MS,
        NOW_MS,
    )
    .await;
    let identity = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&advancing_lease, &fixture.proxy_session_id, NOW_MS)
        .await
        .and_then(|lease| lease.media_identity())
        .expect("live advancing lease identity");
    assert!(fixture
        .ctx
        .hls_proxy
        .record_access_lease_segment_request_started_if_identity_matches(
            &advancing_lease,
            &fixture.proxy_session_id,
            identity,
            1,
            NOW_MS,
        )
        .await
        .is_some());
    fixture.idle_poll(NOW_MS).await;

    // The paused lease sits at the startup window, but only an advancing playback controls the
    // switch, so the session stays on its member.
    assert_eq!(fixture.worker_wake(NOW_MS).await, None);
    // One target duration later the advancing lease crosses the window and controls the switch.
    assert_eq!(fixture.worker_wake(NOW_MS + TARGET_DURATION_MS).await, Some(advancing_lease.clone()));
    assert_ne!(paused_lease, advancing_lease);
}

#[tokio::test]
async fn member_failover_never_triggers_without_a_playback_position() {
    let fixture = member_failover_fixture("member-failover-no-position", VISIBLE_SEGMENTS).await;
    fixture.session.write().await.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    assert!(!fixture.reserve(NOW_MS).await.playback_position_known);
    fixture.player_poll(NOW_MS).await;
    assert_eq!(fixture.worker_wake(NOW_MS).await, None);
    assert_eq!(fixture.worker_wake(NOW_MS + TARGET_DURATION_MS).await, None);
}

#[tokio::test]
async fn reevaluation_requests_reserve_pressure_once_and_never_while_a_handoff_is_pending() {
    let fixture = member_failover_fixture("member-failover-reevaluation", VISIBLE_SEGMENTS).await;
    fixture.start_playback(VISIBLE_SEGMENTS * TARGET_DURATION_MS + TARGET_DURATION_MS).await;
    fixture.player_poll(NOW_MS).await;
    let trigger_ms = NOW_MS + TARGET_DURATION_MS;
    fixture.telemetry(trigger_ms).await;

    let owner_key = fixture
        .ctx
        .hls_proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let ownership = crate::availability_reevaluation::HlsAvailabilityReevaluationOwnership::for_test(
        fixture.ctx.hls_proxy.availability_reevaluations(),
        fixture.proxy_session_id.clone(),
    );
    let mut cycle = HlsAvailabilityReevaluationCycle::new(trigger_ms);

    let decision = evaluate_post_refresh_terminal_attempt(
        &fixture.ctx,
        &fixture.session,
        &owner_key,
        &ownership,
        HlsAvailabilityReevaluationMode::TerminalReserve,
        &mut cycle,
        trigger_ms,
    )
    .await;

    assert_eq!(decision, HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true });
    assert_eq!(cycle.handoff_defer_until_ms, Some(trigger_ms + HLS_MEMBER_HANDOFF_TERMINAL_DEFER_MAX_MS));
    assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::ReservePressure]);

    // A cycle that already deferred the handoff requests nothing more.
    let mut deferred_cycle = HlsAvailabilityReevaluationCycle::new(trigger_ms);
    deferred_cycle.handoff_defer_until_ms = Some(trigger_ms + HLS_MEMBER_HANDOFF_TERMINAL_DEFER_MAX_MS);
    let _ = evaluate_post_refresh_terminal_attempt(
        &fixture.ctx,
        &fixture.session,
        &owner_key,
        &ownership,
        HlsAvailabilityReevaluationMode::TerminalReserve,
        &mut deferred_cycle,
        trigger_ms + TARGET_DURATION_MS,
    )
    .await;
    assert_eq!(fixture.runtime.reasons(), vec![HlsMemberUnhealthyReason::ReservePressure]);

    // A pending handoff keeps the trigger quiet even after the deferral bound.
    fixture.runtime.set_member_handoff_pending(true);
    let mut pending_cycle = HlsAvailabilityReevaluationCycle::new(trigger_ms);
    let _ = evaluate_post_refresh_terminal_attempt(
        &fixture.ctx,
        &fixture.session,
        &owner_key,
        &ownership,
        HlsAvailabilityReevaluationMode::TerminalReserve,
        &mut pending_cycle,
        trigger_ms + HLS_MEMBER_HANDOFF_TERMINAL_DEFER_MAX_MS + TARGET_DURATION_MS,
    )
    .await;
    assert_eq!(
        fixture.runtime.reasons(),
        vec![HlsMemberUnhealthyReason::ReservePressure],
        "a pending handoff must not request another switch"
    );
}

#[tokio::test]
async fn reevaluation_wakes_at_the_reserve_trigger_and_not_at_exhaustion() {
    let fixture = member_failover_fixture("member-failover-wake", VISIBLE_SEGMENTS).await;
    // 18 s of playback leave 14 s of reserve: 2 s above the startup window.
    fixture.start_playback(4 * TARGET_DURATION_MS + 2_000).await;
    fixture.player_poll(NOW_MS).await;
    assert_eq!(fixture.reserve(NOW_MS).await.guaranteed_reserve_ms, 14_000);

    let owner_key = fixture
        .ctx
        .hls_proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let ownership = crate::availability_reevaluation::HlsAvailabilityReevaluationOwnership::for_test(
        fixture.ctx.hls_proxy.availability_reevaluations(),
        fixture.proxy_session_id.clone(),
    );
    let mut cycle = HlsAvailabilityReevaluationCycle::new(NOW_MS);

    let decision = evaluate_post_refresh_terminal_attempt(
        &fixture.ctx,
        &fixture.session,
        &owner_key,
        &ownership,
        HlsAvailabilityReevaluationMode::TerminalReserve,
        &mut cycle,
        NOW_MS,
    )
    .await;

    assert_eq!(decision, HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true });
    let reserve_ms = fixture.reserve(NOW_MS).await.guaranteed_reserve_ms;
    assert_eq!(reserve_ms, 14_000);
    let deadline = cycle.live_reserve_deadline.expect("the live lease keeps a reserve deadline");
    assert_eq!(deadline.next_reevaluation_at_ms, NOW_MS + (reserve_ms - STARTUP_WINDOW_MS));
    assert_eq!(deadline.next_reevaluation_at_ms, NOW_MS + 2_000);
    assert_eq!(deadline.latest_safe_terminal_commit_at_ms, NOW_MS + reserve_ms);
    assert!(fixture.runtime.reasons().is_empty(), "no switch is requested before the window");
}
