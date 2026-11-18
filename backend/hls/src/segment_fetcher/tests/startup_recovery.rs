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
        assert_eq!(state.plan_live_window(HlsLiveWindowView::Entry).unwrap().visible, [0, 1, 2]);
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
async fn rolled_404_head_can_admit_only_the_current_complete_origin_window() {
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
    session.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Discovered;
    assert_eq!(session.queue_manifest_fetch_candidates(65, false).render_window_queued, 1);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::RenderWindow)));
    session.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Discovered;
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::FORBIDDEN);
    assert_eq!(session.queue_manifest_fetch_candidates(66, false).prefetch_skipped, 1);
    assert!(session.segment_prefetch_queue.is_empty());
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::NOT_FOUND);
    session.segments.get_mut(&2).unwrap().discontinuity_before = true;
    assert_eq!(session.queue_manifest_fetch_candidates(67, false).prefetch_skipped, 1);
    assert!(session.segment_prefetch_queue.is_empty());
    session.segments.get_mut(&2).unwrap().discontinuity_before = false;
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
    assert!(hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
    let mut session = context.session.write().await;
    assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    for seq in 1..=4 {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
        drop(session);
        assert!(!hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
        session = context.session.write().await;
        session.segments.get_mut(&seq).unwrap().status = ready();
    }
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::FORBIDDEN);
    drop(session);
    assert!(!hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
    let mut session = context.session.write().await;
    session.segments.get_mut(&0).unwrap().status = missing(StatusCode::NOT_FOUND);
    session.segments.get_mut(&2).unwrap().discontinuity_before = true;
    drop(session);
    assert!(!hls_startup_admission_allows_snapshot(&ctx, &context.session, &snapshot, 71).await);
}

#[tokio::test]
async fn rolled_404_head_requires_every_reserved_tail_object_under_readiness_pressure() {
    use crate::origin_progress::HlsOriginPathCondition;
    use axum::http::StatusCode;

    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let (worker, context, _) = fetch_context(&server, &directory, &SegmentFetchPolicy::default()).await;
    let mode = HlsStartupMode::Optimistic;
    {
        let mut session = context.session.write().await;
        *session = HlsSession::new(HlsSessionKey::new(1, "window"), b"secret", 0);
        session.apply_origin_manifest(&manifest(&server.base_url, 8000, &[8; 6])).unwrap();
        session.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 2 };
    }
    install_startup(&context, &worker, mode).await;
    let ready = || SegmentCacheStatus::Ready { content_length: 8, ready_at_ms: 50 };
    let mut session = context.session.write().await;
    // The rolled-off first sequence leaves proxy sequences 1..=5 in the origin window.
    session.apply_origin_manifest(&manifest(&server.base_url, 8001, &[8; 5])).unwrap();
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
    assert!(plan.replaces_expired_origin_head(&session));

    // The replacement plan needs the complete reserved tail, so both reserved objects must
    // compete with the render window instead of waiting at Prefetch priority.
    for seq in [4, 5] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    assert_eq!(session.queue_manifest_fetch_candidates(65, false).render_window_queued, 2);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::RenderWindow)));
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::RenderWindow)));
    assert!(session.segment_prefetch_queue.is_empty());

    // A non-qualifying plan keeps an ordinary stripped tail at Prefetch priority.
    session.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
    for seq in [4, 5] {
        session.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Discovered;
    }
    let report = session.queue_manifest_fetch_candidates(66, true);
    assert_eq!(report.render_window_queued, 0);
    assert_eq!(report.prefetch_queued, 2);
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((4, crate::SegmentFetchPriority::Prefetch)));
    assert_eq!(session.segment_prefetch_queue.pop_next(), Some((5, crate::SegmentFetchPriority::Prefetch)));
    assert!(session.segment_prefetch_queue.is_empty());
}
