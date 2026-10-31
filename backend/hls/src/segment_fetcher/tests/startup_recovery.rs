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
            let active = state.plan_live_window(HlsLiveWindowView::Active { first_published: 0 }).unwrap();
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
