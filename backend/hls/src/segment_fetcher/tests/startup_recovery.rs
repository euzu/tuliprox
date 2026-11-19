use super::{
    fetch_context, install_startup,
    live_window::{manifest, prepare_selected},
    spawn_segment_server, SegmentFetchPolicy,
};
use crate::{
    api::{HlsLiveWindowView, HlsManifestRenderer},
    HlsSession, HlsSessionKey, SegmentCacheStatus,
};
use shared::model::{HlsStartupMode, HlsStripMode};
use tuliprox_core::model::StripConfig;

#[tokio::test]
async fn startup_recovery_uses_contiguous_successors_and_relaxes_strip_before_short_windows() {
    let server = spawn_segment_server(0).await;
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        for (failed, expected) in [
            (vec![0], vec![1, 2, 3]),
            (vec![0, 1], vec![2, 3, 4]),
            (vec![0, 1, 2], vec![3, 4, 5]),
            (vec![1], vec![2, 3, 4]),
            (vec![0, 2], vec![3, 4, 5]),
            (vec![0, 3], vec![1, 2]),
            (vec![0, 1, 2, 3, 4], vec![5]),
            ((0..6).collect(), vec![]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
            {
                let mut state = context.session.write().await;
                *state = HlsSession::new(HlsSessionKey::new(1, "recovery"), b"secret", 0);
                state.apply_origin_manifest(&manifest(&server.base_url, 409_624, &[8; 6])).unwrap();
                state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
            }
            install_startup(&context, &worker, mode).await;
            let mut state = context.session.write().await;
            state.record_live_window_origin_commit(0);
            let original = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
            prepare_selected(&mut state, &[0, 1, 2, 3, 4, 5], mode, true);
            for seq in 0..6 {
                state.segments.get_mut(&seq).unwrap().status = if failed.contains(&seq) {
                    state.startup.as_ref().unwrap().revisions[&seq].revision().fail();
                    SegmentCacheStatus::FailedPermanent {
                        failed_at_ms: 10,
                        status: Some(axum::http::StatusCode::NOT_FOUND),
                    }
                } else {
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 }
                };
            }
            assert!(!original.is_current(&state));
            assert_eq!(state.live_startup_segments_exhausted(), expected.is_empty());
            let Some(plan) = state.plan_live_window(HlsLiveWindowView::Entry) else {
                assert_eq!(expected, Vec::<u64>::new());
                continue;
            };
            assert_eq!(plan.visible, expected, "mode={mode} failed={failed:?}");
            assert!(plan.is_current(&state));
            let rendered = HlsManifestRenderer::render_live_window(&state, &plan, 20).unwrap();
            assert_eq!(rendered.segment_proxy_seqs, expected);
            if expected.len() < 3 {
                let snapshot = crate::api::derive_hls_lease_manifest_snapshot(
                    &crate::api::HlsLeaseManifestSnapshotInput::FastStartWindow {
                        session: &state,
                        plan: &plan,
                        rendered: &rendered,
                        materialized_body: &rendered.body,
                    },
                    20,
                )
                .unwrap()
                .unwrap();
                state.record_live_window_publication(&crate::HlsAccessLeaseId("short-entry".into()), 10, &snapshot);
                assert_eq!(state.last_warned_short_startup_window, Some(expected.clone()));
                state.record_live_window_publication(&crate::HlsAccessLeaseId("second-entry".into()), 10, &snapshot);
                assert_eq!(state.last_warned_short_startup_window, Some(expected.clone()));
            }
            let active =
                state.plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: None }).unwrap();
            assert_eq!(active.visible.first(), Some(&0));
        }
    }
}

#[tokio::test]
async fn startup_recovery_waits_for_successful_media_and_promotes_reserved_fetches() {
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy { max_prefetch_queue_depth: 0, ..Default::default() };
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    {
        let mut state = context.session.write().await;
        *state = HlsSession::new(HlsSessionKey::new(1, "recovery"), b"secret", 0);
        state.apply_origin_manifest(&manifest(&server.base_url, 100, &[4; 6])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        state.configure_segment_prefetch_queue(0);
    }
    install_startup(&context, &worker, HlsStartupMode::Optimistic).await;
    let mut state = context.session.write().await;
    let original = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(original.visible, vec![0, 1, 2]);
    state.segments.get_mut(&0).unwrap().status =
        SegmentCacheStatus::FailedRetryable { failed_at_ms: 10, retry_after_ms: 1000 };
    let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![1, 2, 3]);
    prepare_selected(&mut state, &[1, 2, 3], HlsStartupMode::Optimistic, false);
    assert!(HlsManifestRenderer::render_live_window(&state, &plan, 20).is_err());
    assert!(!state.live_startup_segments_exhausted());
    let queued = state.queue_manifest_fetch_candidates(20, false);
    assert_eq!(queued.render_window_queued, 3);
    let mut queued = state.segment_prefetch_queue.proxy_seqs();
    queued.sort_unstable();
    assert_eq!(queued, vec![1, 2, 3]);
}

#[tokio::test]
async fn entry_can_render_complete_prefix_during_gateway_retry() {
    use super::{clear_scheduled_prefetch, spawn_sequence_response_server, SegmentFetchPriority, TestOriginResponse};
    use std::time::Duration;
    let server = spawn_sequence_response_server(
        [504, 504, 200]
            .into_iter()
            .map(|status| TestOriginResponse { status, headers: Vec::new(), body: b"media".to_vec() })
            .collect(),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    {
        let mut state = context.session.write().await;
        *state = HlsSession::new(HlsSessionKey::new(1, "gateway"), b"fixture", 0);
        state.apply_origin_manifest(&manifest(&server.base_url, 5, &[4; 4])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
    }
    let mode = HlsStartupMode::Optimistic;
    install_startup(&context, &worker, mode).await;
    clear_scheduled_prefetch(&context, &policy).await;
    super::grant_usable_worker_access_lease_at(
        &worker,
        &context.session.read().await.proxy_session_id,
        tuliprox_core::utils::current_time_millis(),
    )
    .await;
    let first = {
        let mut state = context.session.write().await;
        state.startup.as_mut().unwrap().worker = std::sync::Weak::new();
        prepare_selected(&mut state, &[0, 1, 3], mode, true);
        for seq in [0, 1, 3] {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        state.queue_segment_fetch_candidate(2, SegmentFetchPriority::Demand, 2);
        state.pause_next_origin_retry_for_test()
    };
    let snapshot = worker.next_fetch_snapshot(&context, 3, &policy).await.unwrap();
    assert_eq!(snapshot.proxy_seq, 2);
    let task_context = context.clone();
    let fetch =
        tokio::spawn(async move { super::super::fetch_segment_into_cache(&task_context, &snapshot, &policy).await });
    tokio::time::timeout(Duration::from_secs(5), first.retry_ready.notified()).await.unwrap();
    let second = {
        let mut state = context.session.write().await;
        // The 504 already published its evidence, so the entry window shows the prefix right away.
        assert_eq!(state.plan_live_window(HlsLiveWindowView::Entry).unwrap().visible, [0, 1]);
        state.pause_next_origin_retry_for_test()
    };
    first.resume_backoff.notify_one();
    first.resume_prepare.notify_one();
    tokio::time::timeout(Duration::from_secs(5), second.retry_ready.notified()).await.unwrap();
    {
        let state = context.session.read().await;
        let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(plan.visible, [0, 1]);
        assert_eq!(HlsManifestRenderer::render_live_window(&state, &plan, 4).unwrap().segment_proxy_seqs, [0, 1]);
        assert_eq!(
            state
                .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: None })
                .unwrap()
                .visible,
            [0, 1, 2, 3]
        );
        assert!(!state.live_startup_segments_exhausted());
    }
    second.resume_backoff.notify_one();
    second.resume_prepare.notify_one();
    assert!(fetch.await.unwrap().is_ok());
    assert_eq!(server.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn rolled_404_head_admits_the_renderable_window_by_its_ready_reserve() {
    use crate::{
        api::{
            derive_hls_lease_manifest_snapshot, hls_startup_admission_allows_snapshot, HlsLeaseManifestSnapshotInput,
        },
        origin_progress::HlsOriginPathCondition,
    };
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[8; 4])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    }
    install_startup(&context, &worker, mode).await;
    let ready = || SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    let missing = |status| SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(status) };
    let mut session = context.session.write().await;
    session.apply_origin_manifest(&manifest(&server.base_url, 8001, &[8; 4])).unwrap();
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::NOT_FOUND);
    prepare_selected(&mut session, &[1, 2, 3, 4], mode, true);
    for seq in 1..=4 {
        session.segments.get_mut(&seq).unwrap().status = ready();
    }
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![1, 2, 3]);
    assert_eq!(plan.hidden_tail, vec![4]);
    // The READY span from sequence 1 already clears the degraded admission boundary, so the
    // reserved object keeps its Prefetch priority instead of competing with the render window.
    assert_eq!(plan.reserve_promotion_tail(&session, 65), Vec::<u64>::new());
    session.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Discovered;
    assert_eq!(session.queue_manifest_fetch_candidates(65, false).prefetch_skipped, 1);
    assert!(session.segment_prefetch_queue.is_empty());
    session.segments.get_mut(&4).unwrap().status = ready();
    let rendered = HlsManifestRenderer::render_live_window(&session, &plan, 70).unwrap();
    let snapshot = derive_hls_lease_manifest_snapshot(
        &HlsLeaseManifestSnapshotInput::FastStartWindow {
            session: &session,
            plan: &plan,
            rendered: &rendered,
            materialized_body: &rendered.body,
        },
        70,
    )
    .unwrap()
    .unwrap();
    drop(session);
    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await,
        "the complete renderable window is admitted under a degraded origin path"
    );
    let mut session = context.session.write().await;
    assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    // A hole inside the visible window removes the contiguous READY span the admission counts.
    for seq in 1..=3 {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
        drop(session);
        assert!(!hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
        session = context.session.write().await;
        session.segments.get_mut(&seq).unwrap().status = ready();
    }
    // The reserved object behind the window is not part of it: an unready reserve object keeps the
    // admission while the READY span from the window start still clears the admission boundary.
    session.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Discovered;
    drop(session);
    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await,
        "the visible window alone carries the reserve"
    );
    let mut session = context.session.write().await;
    session.segments.get_mut(&4).unwrap().status = ready();
    // Why the origin no longer advertises the predecessor is no longer part of the decision: the
    // admission counts READY media, so neither a forbidden predecessor nor a discontinuity flag
    // changes it.
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::FORBIDDEN);
    drop(session);
    assert!(hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
    let mut session = context.session.write().await;
    session.segments.get_mut(&2).unwrap().discontinuity_before = true;
    drop(session);
    assert!(hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
}

#[tokio::test]
async fn longer_origin_window_with_a_rolled_404_predecessor_admits_the_renderable_window() {
    use crate::{
        api::{
            derive_hls_lease_manifest_snapshot, hls_startup_admission_allows_snapshot, HlsLeaseManifestSnapshotInput,
        },
        origin_progress::HlsOriginPathCondition,
    };
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[8; 8])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
    }
    install_startup(&context, &worker, mode).await;
    let ready = || SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    let mut session = context.session.write().await;
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    // The 404 predecessor (4) is still inside the origin window 0..=7.
    session.segments.get_mut(&4).unwrap().status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(StatusCode::NOT_FOUND) };
    prepare_selected(&mut session, &[5, 6, 7], mode, true);
    for seq in 5..=7 {
        session.segments.get_mut(&seq).unwrap().status = ready();
    }
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![5, 6, 7]);
    assert_eq!(plan.hidden_tail, Vec::<u64>::new());
    assert!(!plan.is_startup_recovery());
    // Nothing is reserved behind the window, and the READY span from sequence 5 carries the
    // admission on its own.
    assert_eq!(plan.reserve_promotion_tail(&session, 70), Vec::<u64>::new());
    let rendered = HlsManifestRenderer::render_live_window(&session, &plan, 70).unwrap();
    let snapshot = derive_hls_lease_manifest_snapshot(
        &HlsLeaseManifestSnapshotInput::FastStartWindow {
            session: &session,
            plan: &plan,
            rendered: &rendered,
            materialized_body: &rendered.body,
        },
        70,
    )
    .unwrap()
    .unwrap();
    drop(session);
    // The same complete window is admitted while the origin path is healthy.
    context.session.write().await.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await,
        "a healthy origin path admits the complete window"
    );
    // Under readiness pressure the identical window is admitted: the prospective reserve counts
    // the contiguous READY span from the window start, whatever the origin does with the segment
    // before it.
    context.session.write().await.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await,
        "a degraded origin path admits a complete, renderable window"
    );
}

#[tokio::test]
async fn rolled_404_head_promotes_the_reserve_gap_under_readiness_pressure() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[4, 4, 4, 4, 6, 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    let ready = || SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    let mut session = context.session.write().await;
    // The rolled-off first sequence leaves proxy sequences 1..=5 in the origin window.
    session.apply_origin_manifest(&manifest(&server.base_url, 8001, &[4; 5])).unwrap();
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    session.segments.get_mut(&0).unwrap().status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(StatusCode::NOT_FOUND) };
    prepare_selected(&mut session, &[1, 2, 3, 4, 5], mode, true);
    for seq in 1..=5 {
        session.segments.get_mut(&seq).unwrap().status = ready();
    }
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![1, 2, 3]);
    assert_eq!(plan.hidden_tail, vec![4, 5]);
    assert!(plan.is_ready(&session));

    // The three READY visible segments of 4s carry the contiguous reserve, and the session
    // target duration of 7s puts the degraded admission boundary at 10_500 + 7_000 ms: the first
    // reserved object is not READY, so the admission falls back to the target duration as its
    // transition margin. One 6s reserved object closes that gap, so only it competes with the
    // render window.
    for seq in [4, 5] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    assert_eq!(plan.reserve_promotion_tail(&session, 65), [4]);
    let report = session.queue_manifest_fetch_candidates(65, true);
    assert_eq!(report.render_window_queued, 1);
    assert_eq!(report.prefetch_queued, 1);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::RenderWindow)));
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::Prefetch)));
    assert!(session.segment_prefetch_queue.is_empty());

    // A healthy origin path keeps the whole stripped tail at Prefetch priority.
    session.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
    for seq in [4, 5] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    assert_eq!(plan.reserve_promotion_tail(&session, 66), Vec::<u64>::new());
    let report = session.queue_manifest_fetch_candidates(66, true);
    assert_eq!(report.render_window_queued, 0);
    assert_eq!(report.prefetch_queued, 2);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::Prefetch)));
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::Prefetch)));
    assert!(session.segment_prefetch_queue.is_empty());
}

/// The promotion reaches exactly the degraded admission boundary: a reserve equal to
/// `budget + margin` is still rejected by the admission, so the next object is promoted, and a
/// reserve above it promotes nothing.
#[tokio::test]
async fn reserve_promotion_starts_at_the_degraded_admission_boundary() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[6; 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    let mut session = context.session.write().await;
    session.apply_origin_manifest(&manifest(&server.base_url, 8001, &[6; 5])).unwrap();
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    session.segments.get_mut(&0).unwrap().status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(StatusCode::NOT_FOUND) };
    prepare_selected(&mut session, &[1, 2, 3, 4, 5], mode, true);
    // The visible window is ready; the reserved tail is not, so only the promotion can raise the
    // contiguous READY reserve above the admission boundary.
    for seq in 1..=3 {
        session.segments.get_mut(&seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    }
    for seq in [4, 5] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![1, 2, 3]);
    assert_eq!(plan.hidden_tail, vec![4, 5]);
    assert!(plan.is_ready(&session));
    // Reserve 18_000 ms (three READY segments of 6s); the transition margin of the admission is
    // the 6s of the first reserved object.
    // The origin target duration of 7s bounds the budget at 10_500 ms, so the boundary of
    // 16_500 ms is below the reserve and nothing is promoted.
    assert_eq!(plan.reserve_promotion_tail(&session, 65), Vec::<u64>::new());

    // Boundary 9_000 + 6_000 = 15_000 ms, reserve 18_000 ms: above it, nothing is promoted.
    session.origin_control.target_duration_snapshot_ms = Some(6_000);
    assert_eq!(plan.reserve_promotion_tail(&session, 66), Vec::<u64>::new());

    // Boundary 12_000 + 6_000 = 18_000 ms equals the reserve: the admission rejects at equality,
    // so the first reserved object is promoted.
    session.origin_control.target_duration_snapshot_ms = Some(8_000);
    assert_eq!(plan.reserve_promotion_tail(&session, 67), [4]);

    // Boundary 18_000 + 6_000 = 24_000 ms: the first reserved object reaches only equality, so
    // the second is promoted as well.
    session.origin_control.target_duration_snapshot_ms = Some(12_000);
    assert_eq!(plan.reserve_promotion_tail(&session, 68), [4, 5]);

    // A window without a reserved tail has nothing to promote.
    session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![3, 4, 5]);
    assert_eq!(plan.hidden_tail, Vec::<u64>::new());
    assert_eq!(plan.reserve_promotion_tail(&session, 69), Vec::<u64>::new());
}

/// The admission falls back to the target duration as its transition margin while the first
/// reserved object is not READY. A short first reserved object must therefore not shrink the
/// boundary the promotion has to reach: the promotion uses the same margin as the admission.
#[tokio::test]
async fn reserve_promotion_uses_the_target_duration_while_the_first_reserved_object_is_not_ready() {
    use crate::origin_progress::HlsOriginPathCondition;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        // The visible window is three 4s objects; the reserved tail is a 1s object followed by
        // a 6s object.
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[4, 4, 4, 1, 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    let mut session = context.session.write().await;
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    prepare_selected(&mut session, &[0, 1, 2, 3, 4], mode, true);
    for seq in 0..=2 {
        session.segments.get_mut(&seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    }
    for seq in [3, 4] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    session.origin_control.target_duration_snapshot_ms = Some(6_000);
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![0, 1, 2]);
    assert_eq!(plan.hidden_tail, vec![3, 4]);
    assert!(plan.is_ready(&session));
    // Reserve 12_000 ms. The boundary is the 9_000 ms budget ceiling plus the 6_000 ms target
    // duration the admission uses while the first reserved object is not READY: 15_000 ms. The
    // 1s reserved object alone reaches 13_000 ms, so both reserved objects are promoted.
    assert_eq!(plan.reserve_promotion_tail(&session, 65), [3, 4]);
    let report = session.queue_manifest_fetch_candidates(65, true);
    assert_eq!(report.render_window_queued, 2);
    assert_eq!(report.prefetch_queued, 0);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((3, crate::SegmentFetchPriority::RenderWindow)));
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::RenderWindow)));
    assert!(session.segment_prefetch_queue.is_empty());
}

#[tokio::test]
async fn rolled_404_head_skips_replacement_promotion_for_terminal_reserve_failures() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[4; 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    let mut session = context.session.write().await;
    // A rolled-off head leaves proxy sequences 1..=5 with 4 and 5 reserved.
    session.apply_origin_manifest(&manifest(&server.base_url, 8001, &[4; 5])).unwrap();
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    session.segments.get_mut(&0).unwrap().status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(StatusCode::NOT_FOUND) };
    prepare_selected(&mut session, &[1, 2, 3, 4, 5], mode, true);
    for seq in 1..=3 {
        session.segments.get_mut(&seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    }
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![1, 2, 3]);
    assert_eq!(plan.hidden_tail, vec![4, 5]);
    // A 12s target duration puts the degraded admission boundary at 18_000 + 4_000 ms, so both
    // reserved objects carry the reserve.
    session.origin_control.target_duration_snapshot_ms = Some(12_000);

    // A permanently failed reserve object can never complete the reserve the admission needs, so
    // no reserved object is promoted into the render window.
    for terminal in [
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 30, status: Some(StatusCode::NOT_FOUND) },
        SegmentCacheStatus::Expired,
    ] {
        session.segments.get_mut(&4).unwrap().status = terminal.clone();
        session.segments.get_mut(&5).unwrap().status = SegmentCacheStatus::Discovered;
        let report = session.queue_manifest_fetch_candidates(65, false);
        assert_eq!(report.render_window_queued, 0, "terminal reserve {terminal:?} blocks the promotion");
        assert_eq!(session.segment_prefetch_queue.pop_next(), None);
    }

    // A retryable reserve object still owns a retry, so the reserve stays promoted instead of
    // being dropped the way a terminal failure is.
    session.segments.get_mut(&4).unwrap().status =
        SegmentCacheStatus::FailedRetryable { failed_at_ms: 30, retry_after_ms: 1_000 };
    session.segments.get_mut(&5).unwrap().status = SegmentCacheStatus::Discovered;
    assert_eq!(plan.reserve_promotion_tail(&session, 66), [4, 5]);
    let report = session.queue_manifest_fetch_candidates(66, false);
    assert_eq!(report.render_window_queued, 1, "the retryable object owns its own retry");
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::RenderWindow)));
    assert!(session.segment_prefetch_queue.is_empty());
}

#[tokio::test]
async fn reserve_promotion_counts_subsequent_ready_segments_and_ignores_terminal_tail_after_target() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[6; 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    }
    install_startup(&context, &worker, mode).await;
    let mut session = context.session.write().await;
    session.record_live_window_origin_commit(45);
    session.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    prepare_selected(&mut session, &[0, 1, 2, 3, 4, 5], mode, true);
    for seq in 0..=2 {
        session.segments.get_mut(&seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    }
    session.segments.get_mut(&3).unwrap().status = SegmentCacheStatus::Discovered;
    session.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    session.segments.get_mut(&5).unwrap().status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 20, status: Some(StatusCode::NOT_FOUND) };

    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![0, 1, 2]);
    assert_eq!(plan.hidden_tail, vec![3, 4, 5]);

    session.origin_control.target_duration_snapshot_ms = Some(10_000);
    assert_eq!(plan.reserve_promotion_tail(&session, 65), [3, 4]);
}

/// A permanent clear-TS 404 removes the failed object from the render queue. The scheduler wake
/// only replans while work is still queued, so the commit replans in place: the window selected
/// after the failure must be dispatched without waiting for the next origin commit.
#[tokio::test]
async fn permanent_clear_ts_404_replans_the_render_queue() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[4; 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    {
        let mut session = context.session.write().await;
        prepare_selected(&mut session, &[1, 2, 3, 4, 5], mode, true);
        session.record_live_window_origin_commit(45);
        for seq in [2, 3, 4] {
            session.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
        }
        assert_eq!(session.plan_live_window(HlsLiveWindowView::Entry).unwrap().visible, vec![1, 2, 3]);
        assert!(session.queue_segment_fetch_candidate(1, crate::SegmentFetchPriority::Demand, 50));
    }
    let snapshot = worker.next_fetch_snapshot(&context, 51, &policy).await.expect("the failed object fetches");
    assert_eq!(snapshot.proxy_seq, 1);
    assert!(context.session.read().await.segment_prefetch_queue.is_empty(), "no other fetch is queued");
    let completion = {
        let mut session = context.session.write().await;
        worker.apply_segment_fetch_result(
            &mut session,
            &snapshot,
            &policy,
            Err(crate::HlsOriginResourceFetchError::PermanentStatus(StatusCode::NOT_FOUND)),
            60,
        )
    };
    assert!(completion.generation_valid);
    let mut session = context.session.write().await;
    assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    assert!(session.segments.get(&1).is_some_and(crate::SegmentEntry::is_clear_ts_origin_404));
    // The replacement window is selected from the surviving contiguous run, and both of its
    // visible and reserved sequences are ready or promoted on their own.
    let plan = session.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(plan.visible, vec![2, 3, 4]);
    assert_eq!(plan.hidden_tail, vec![5]);
    // The only fetch the commit can queue is the unready reserved object; without the replan the
    // queue stays empty and no dispatch happens until the next origin commit.
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::RenderWindow)));
    assert!(session.segment_prefetch_queue.is_empty());
}

/// A 504 is published as soon as the attempt is classified: a new viewer must see the
/// contiguous two-segment prefix during the retry delay, not only when the next attempt
/// starts preparing.
#[tokio::test]
async fn entry_publishes_the_prefix_before_the_gateway_retry_delay_sleeps() {
    use super::{clear_scheduled_prefetch, spawn_sequence_response_server, SegmentFetchPriority, TestOriginResponse};
    use std::time::Duration;
    let server = spawn_sequence_response_server(
        [504, 200]
            .into_iter()
            .map(|status| TestOriginResponse { status, headers: Vec::new(), body: b"media".to_vec() })
            .collect(),
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    // The second attempt waits half a second before it reaches the origin.
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0, 500, 0, 0, 0], retry_jitter_max_ms: 0, ..Default::default() };
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    {
        let mut state = context.session.write().await;
        *state = HlsSession::new(HlsSessionKey::new(1, "gateway-backoff"), b"fixture", 0);
        state.apply_origin_manifest(&manifest(&server.base_url, 5, &[4; 4])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
    }
    let mode = HlsStartupMode::Optimistic;
    install_startup(&context, &worker, mode).await;
    clear_scheduled_prefetch(&context, &policy).await;
    super::grant_usable_worker_access_lease_at(
        &worker,
        &context.session.read().await.proxy_session_id,
        tuliprox_core::utils::current_time_millis(),
    )
    .await;
    let retry = {
        let mut state = context.session.write().await;
        state.startup.as_mut().unwrap().worker = std::sync::Weak::new();
        prepare_selected(&mut state, &[0, 1, 3], mode, true);
        for seq in [0, 1, 3] {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        state.queue_segment_fetch_candidate(2, SegmentFetchPriority::Demand, 2);
        state.pause_next_origin_retry_for_test()
    };
    let snapshot = worker.next_fetch_snapshot(&context, 3, &policy).await.unwrap();
    assert_eq!(snapshot.proxy_seq, 2);
    let task_context = context.clone();
    let fetch =
        tokio::spawn(async move { super::super::fetch_segment_into_cache(&task_context, &snapshot, &policy).await });
    // The slot parks on the barrier that immediately precedes the backoff sleep.
    tokio::time::timeout(Duration::from_secs(5), retry.retry_ready.notified()).await.unwrap();
    {
        let state = context.session.read().await;
        let status = state.segments.get(&2).map(|entry| &entry.status);
        assert!(
            matches!(status, Some(SegmentCacheStatus::Fetching { gateway_timeout: true, .. })),
            "the first 504 is recorded before the retry delay starts: {status:?}"
        );
        let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(plan.visible, [0, 1], "a new viewer skips the retried tail during the backoff");
        assert_eq!(HlsManifestRenderer::render_live_window(&state, &plan, 4).unwrap().segment_proxy_seqs, [0, 1]);
    }
    retry.resume_backoff.notify_one();
    retry.resume_prepare.notify_one();
    assert!(fetch.await.unwrap().is_ok());
    assert_eq!(server.requests.lock().await.len(), 2, "the retried segment reached the origin once more");
}

/// X1: with a non-zero retry delay the evidence is published while the backoff is still
/// pending, so the Entry plan offers the two-segment prefix before the retry reaches the
/// origin — not only once the next attempt starts preparing.
#[tokio::test(start_paused = true)]
async fn entry_offers_the_prefix_while_the_gateway_retry_backoff_is_pending() {
    use super::{clear_scheduled_prefetch, spawn_sequence_response_server, SegmentFetchPriority, TestOriginResponse};
    use std::time::Duration;

    keep_paused_io_progressing(async {
        let server = spawn_sequence_response_server(
            [504, 200]
                .into_iter()
                .map(|status| TestOriginResponse { status, headers: Vec::new(), body: b"media".to_vec() })
                .collect(),
        )
        .await;
        let directory = tempfile::tempdir().unwrap();
        // The second attempt waits half a second; the first one starts immediately.
        let policy =
            SegmentFetchPolicy { retry_delays_ms: [0, 500, 0, 0, 0], retry_jitter_max_ms: 0, ..Default::default() };
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "gateway-backoff-pending"), b"fixture", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 5, &[4; 4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
        }
        let mode = HlsStartupMode::Optimistic;
        install_startup(&context, &worker, mode).await;
        clear_scheduled_prefetch(&context, &policy).await;
        super::grant_usable_worker_access_lease_at(
            &worker,
            &context.session.read().await.proxy_session_id,
            tuliprox_core::utils::current_time_millis(),
        )
        .await;
        let retry = {
            let mut state = context.session.write().await;
            state.startup.as_mut().unwrap().worker = std::sync::Weak::new();
            prepare_selected(&mut state, &[0, 1, 3], mode, true);
            for seq in [0, 1, 3] {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
            }
            state.queue_segment_fetch_candidate(2, SegmentFetchPriority::Demand, 2);
            state.pause_next_origin_retry_for_test()
        };
        let snapshot = worker.next_fetch_snapshot(&context, 3, &policy).await.unwrap();
        assert_eq!(snapshot.proxy_seq, 2);
        let task_context = context.clone();
        let fetch =
            tokio::spawn(
                async move { super::super::fetch_segment_into_cache(&task_context, &snapshot, &policy).await },
            );
        // The slot parks after classifying the first 504 and before the 500 ms backoff sleep.
        tokio::time::timeout(Duration::from_secs(5), retry.retry_ready.notified()).await.unwrap();
        retry.resume_backoff.notify_one();
        // Let the slot enter its 500 ms sleep before the clock moves.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // Half of the backoff has elapsed and the retry still did not reach the origin.
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(server.requests.lock().await.len(), 1, "the second attempt is still inside its backoff");
        {
            let state = context.session.read().await;
            let status = state.segments.get(&2).map(|entry| &entry.status);
            assert!(
                matches!(status, Some(SegmentCacheStatus::Fetching { gateway_timeout: true, .. })),
                "the evidence survives the pending backoff: {status:?}"
            );
            let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
            assert_eq!(plan.visible, [0, 1], "the entry plan offers the ready prefix while the tail retries");
            assert_eq!(plan.hidden_tail, [2, 3]);
            assert!(plan.is_startup_recovery());
            assert_eq!(HlsManifestRenderer::render_live_window(&state, &plan, 4).unwrap().segment_proxy_seqs, [0, 1]);
        }
        // The rest of the backoff elapses and the retried attempt runs to completion.
        retry.resume_prepare.notify_one();
        tokio::time::advance(Duration::from_millis(1_000)).await;
        assert!(fetch.await.unwrap().is_ok());
        assert_eq!(server.requests.lock().await.len(), 2, "the retry reached the origin after the backoff");
    })
    .await;
}

/// Keeps the current-thread runtime busy while the clock is paused, so real loopback I/O still
/// progresses without the clock auto-advancing past a pending backoff. The real-time bound turns
/// a stalled milestone into a failure instead of a hang.
async fn keep_paused_io_progressing<T>(future: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "paused-time test stalled waiting for a real I/O milestone"
        );
        let result = future.as_mut().poll(cx);
        if result.is_pending() {
            cx.waker().wake_by_ref();
        }
        result
    })
    .await
}

/// Evidence belongs to the attempt that produced it: a later transport timeout after a 504
/// must clear the flag instead of leaving a stale gateway-timeout prefix shortcut behind.
#[tokio::test]
async fn a_later_transport_timeout_clears_the_gateway_timeout_evidence() {
    use super::{clear_scheduled_prefetch, SegmentFetchPriority};
    use std::time::Duration;
    let server = spawn_gateway_then_hanging_origin().await;
    let directory = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy {
        timeouts: crate::HlsOriginTimeoutPolicy::fixed(100, 100),
        retry_delays_ms: [0; 5],
        retry_jitter_max_ms: 0,
        ..Default::default()
    };
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    {
        let mut state = context.session.write().await;
        *state = HlsSession::new(HlsSessionKey::new(1, "gateway-clear"), b"fixture", 0);
        state.apply_origin_manifest(&manifest(&server.base_url, 5, &[4; 4])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
    }
    let mode = HlsStartupMode::Optimistic;
    install_startup(&context, &worker, mode).await;
    clear_scheduled_prefetch(&context, &policy).await;
    super::grant_usable_worker_access_lease_at(
        &worker,
        &context.session.read().await.proxy_session_id,
        tuliprox_core::utils::current_time_millis(),
    )
    .await;
    let first = {
        let mut state = context.session.write().await;
        state.startup.as_mut().unwrap().worker = std::sync::Weak::new();
        prepare_selected(&mut state, &[0, 1, 3], mode, true);
        for seq in [0, 1, 3] {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        state.queue_segment_fetch_candidate(2, SegmentFetchPriority::Demand, 2);
        state.pause_next_origin_retry_for_test()
    };
    let snapshot = worker.next_fetch_snapshot(&context, 3, &policy).await.unwrap();
    assert_eq!(snapshot.proxy_seq, 2);
    let configured_attempts = policy.retry_delays_ms.len();
    let task_context = context.clone();
    let fetch =
        tokio::spawn(async move { super::super::fetch_segment_into_cache(&task_context, &snapshot, &policy).await });
    tokio::time::timeout(Duration::from_secs(5), first.retry_ready.notified()).await.unwrap();
    {
        let state = context.session.read().await;
        let status = state.segments.get(&2).map(|entry| &entry.status);
        assert!(
            matches!(status, Some(SegmentCacheStatus::Fetching { gateway_timeout: true, .. })),
            "the 504 is recorded for its own attempt: {status:?}"
        );
    }
    let second = context.session.write().await.pause_next_origin_retry_for_test();
    first.resume_backoff.notify_one();
    first.resume_prepare.notify_one();
    // The second attempt reaches the origin and never gets an answer.
    tokio::time::timeout(Duration::from_secs(5), second.retry_ready.notified()).await.unwrap();
    {
        let state = context.session.read().await;
        let status = state.segments.get(&2).map(|entry| &entry.status);
        assert!(
            matches!(status, Some(SegmentCacheStatus::Fetching { gateway_timeout: false, .. })),
            "the transport timeout clears the earlier 504 evidence: {status:?}"
        );
    }
    second.resume_backoff.notify_one();
    second.resume_prepare.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), fetch).await.unwrap().unwrap();
    assert!(
        matches!(outcome, Err(crate::HlsOriginResourceFetchError::Timeout)),
        "the last attempt reports its own transport timeout: {:?}",
        outcome.err()
    );
    assert_eq!(server.requests.lock().await.len(), configured_attempts, "every configured attempt reached the origin");
}

/// Answers the first request with 504 and never answers the following ones, so a later
/// attempt ends in a transport timeout instead of another gateway timeout.
async fn spawn_gateway_then_hanging_origin() -> super::TestSegmentServer {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("origin binds");
    let address = listener.local_addr().expect("origin address");
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let mut answered_gateway_timeout = false;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let answer_gateway_timeout = !answered_gateway_timeout;
            answered_gateway_timeout = true;
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0_u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                if answer_gateway_timeout {
                    let _ = socket
                        .write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                } else {
                    std::future::pending::<()>().await;
                }
            });
        }
    });
    super::TestSegmentServer { base_url: format!("http://{address}"), requests, task }
}
