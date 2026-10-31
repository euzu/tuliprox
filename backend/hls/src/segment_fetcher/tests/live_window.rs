use super::{
    fetch_context, install_startup, normal_manifest, spawn_segment_server, SegmentFetchPolicy, SegmentFetchPriority,
};
use crate::{
    api::{HlsLiveWindowView, HlsManifestRenderer},
    CachedSegmentMetadata, HlsSession, HlsSessionKey, SegmentCacheStatus, SegmentRevisionKind,
};
use shared::model::{HlsStartupMode, HlsStripMode};
use std::{fmt::Write as _, sync::atomic::Ordering};
use tuliprox_core::model::StripConfig;

const MODES: [HlsStartupMode; 3] =
    [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic];

pub(super) fn manifest(
    origin: &str,
    first: u64,
    durations: &[u64],
) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for (offset, duration) in durations.iter().enumerate() {
        writeln!(body, "#EXTINF:{duration},\n{origin}/{}.ts", first + u64::try_from(offset).unwrap()).unwrap();
    }
    normal_manifest(&body)
}

pub(super) fn prepare_selected(session: &mut HlsSession, sequences: &[u64], mode: HlsStartupMode, complete: bool) {
    let startup = session.startup.as_mut().unwrap();
    for (index, seq) in sequences.iter().enumerate() {
        let revision = startup
            .store
            .create(
                session.proxy_session_id.clone(),
                *seq,
                if mode == HlsStartupMode::FirstReady {
                    SegmentRevisionKind::Processed
                } else {
                    SegmentRevisionKind::Raw
                },
            )
            .unwrap();
        if complete || (mode == HlsStartupMode::FirstReady && index == 0) {
            revision.revision().complete(CachedSegmentMetadata { path: std::path::PathBuf::from("unused"), size: 1 });
        } else if index == 0 || mode != HlsStartupMode::Optimistic {
            revision.revision().prefix_available.store(1, Ordering::Release);
        }
        startup.revisions.insert(*seq, revision);
    }
}

#[tokio::test]
async fn rebase_overlap_renders_after_the_next_segment_without_weakening_window_readiness() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 182_137, &[7; 4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2, 3], mode, true);
        state.render_and_store_manifest(10).expect("baseline is published");
        let previous = state.last_rendered_manifest.clone();
        let host = crate::timeline::effective_origin_host_id("127.0.0.1");
        state.apply_origin_rebase_manifest(&manifest(&server.base_url, 182_139, &[7; 4]), host, false).unwrap();
        assert_eq!(state.live_origin_sequences(), &[4, 5]);
        prepare_selected(&mut state, &[4, 5], mode, true);
        for seq in [4, 5] {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20 };
        }
        let too_short = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(too_short.visible, [4, 5]);
        assert!(
            matches!(
                HlsManifestRenderer::render_live_window(&state, &too_short, 20),
                Err(crate::renderer::RenderError::NoRenderableWindow)
            ),
            "{mode:?}: two cached segments do not weaken the regular minimum"
        );
        assert_eq!(state.last_rendered_manifest, previous);

        state
            .apply_origin_manifest(&manifest(&server.base_url, 182_140, &[7; 4]))
            .expect("the next sliding origin window keeps its current overlap");
        let ready = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(ready.visible, [4, 5, 6]);
        assert!(
            matches!(
                HlsManifestRenderer::render_live_window(&state, &ready, 30),
                Err(crate::renderer::RenderError::NoRenderableWindow)
            ),
            "{mode:?}: the third segment must still become publishable"
        );

        prepare_selected(&mut state, &[6], mode, true);
        state.segments.get_mut(&6).unwrap().status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 40 };
        let rendered = HlsManifestRenderer::render_live_window(&state, &ready, 40).expect("current media is ready");
        assert_eq!(rendered.segment_proxy_seqs, [4, 5, 6]);
        assert_eq!(rendered.first_proxy_seq, 4);
        assert_eq!(state.store_rendered_manifest(rendered), crate::RenderedManifestStoreOutcome::Stored);
        assert_eq!(state.proxy_next_seq, Some(7));
    }
}

#[tokio::test]
async fn live_window_matrix_renders_only_selected_revisions_and_selects_real_fetch_snapshots() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        for (count, strip_mode, strip, offset, expected) in [
            (6, HlsStripMode::Segments, 0, 0, vec![3, 4, 5]),
            (6, HlsStripMode::Segments, 1, 0, vec![2, 3, 4]),
            (6, HlsStripMode::Segments, 2, 0, vec![1, 2, 3]),
            (6, HlsStripMode::Segments, 3, 0, vec![0, 1, 2]),
            (6, HlsStripMode::Segments, u64::MAX, 0, vec![0, 1, 2]),
            (12, HlsStripMode::Segments, 1, 0, vec![8, 9, 10]),
            (12, HlsStripMode::Segments, 1, 9_000_000, vec![9_000_008, 9_000_009, 9_000_010]),
            (6, HlsStripMode::Seconds, 0, 0, vec![3, 4, 5]),
            (6, HlsStripMode::Seconds, 7, 0, vec![2, 3, 4]),
            (6, HlsStripMode::Seconds, 8, 0, vec![1, 2, 3]),
            (6, HlsStripMode::Seconds, 10, 0, vec![1, 2, 3]),
            (6, HlsStripMode::Seconds, u64::MAX, 0, vec![0, 1, 2]),
        ] {
            for parallelism in [1, 2, 3] {
                let directory = tempfile::tempdir().unwrap();
                let policy = SegmentFetchPolicy {
                    max_session_segment_fetches: parallelism,
                    max_prefetch_queue_depth: 0,
                    ..Default::default()
                };
                let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
                {
                    let mut state = context.session.write().await;
                    *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
                    state.proxy_next_seq = Some(offset);
                    let durations = if count == 6 { vec![4, 4, 4, 4, 3, 7] } else { vec![4; count] };
                    state.apply_origin_manifest(&manifest(&server.base_url, 100_000, &durations)).unwrap();
                    state.live_window_strip = StripConfig { mode: strip_mode, value: strip };
                    state.configure_segment_prefetch_queue(0);
                }
                install_startup(&context, &worker, mode).await;
                let plan = {
                    let mut state = context.session.write().await;
                    let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
                    assert_eq!(plan.visible, expected, "mode={mode} strip={strip} offset={offset}");
                    state.queue_manifest_fetch_candidates(10, false);
                    assert_eq!(crate::renderer_candidate_window_proxy_seqs(&state), expected);
                    let protected = crate::gc::ProtectedSet::from_session(&state);
                    assert_eq!(
                        protected.segment_proxy_seqs.into_iter().collect::<std::collections::BTreeSet<_>>(),
                        expected.iter().copied().collect()
                    );
                    plan
                };
                // Exercise the real scheduler, including the entry barrier. Its
                // next fetch must be the chosen head, never an older origin URI.
                for expected_seq in &expected {
                    let snapshot = worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap();
                    assert_eq!(snapshot.proxy_seq, *expected_seq);
                    assert_eq!(snapshot.origin_seq, 100_000 + expected_seq - offset);
                    assert_eq!(snapshot.priority, SegmentFetchPriority::RenderWindow);
                    let mut state = context.session.write().await;
                    state.active_segment_fetches -= 1;
                    prepare_selected(&mut state, &[*expected_seq], mode, true);
                }
                assert!(worker.next_fetch_snapshot(&context, 12, &policy).await.is_none());
                let mut state = context.session.write().await;
                prepare_selected(&mut state, &expected, mode, false);
                let rendered = HlsManifestRenderer::render_live_window(&state, &plan, 12).unwrap();
                assert_eq!(rendered.segment_proxy_seqs, expected);
                assert_eq!(state.startup.as_ref().unwrap().revisions.len(), 3);
                assert!(rendered.body.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", expected[0])));
            }
        }
    }
}

#[tokio::test]
async fn live_window_short_empty_readiness_and_stale_plans_are_explicit() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        for count in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let policy = SegmentFetchPolicy::default();
            let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
            {
                let mut state = context.session.write().await;
                *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
                state.apply_origin_manifest(&manifest(&server.base_url, 0, &vec![4; count])).unwrap();
                state.live_window_strip = StripConfig { mode: HlsStripMode::Seconds, value: u64::MAX };
            }
            install_startup(&context, &worker, mode).await;
            let mut state = context.session.write().await;
            state.record_live_window_origin_commit(0);
            let Some(plan) = state.plan_live_window(HlsLiveWindowView::Entry) else {
                assert_eq!(count, 0);
                continue;
            };
            assert_eq!(plan.visible.len(), count);
            assert_eq!(plan.effective_strip_segments, 0);
            assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_err());
            prepare_selected(&mut state, &plan.visible, mode, false);
            // Qualification of a one/two-segment source is still mandatory.
            assert_eq!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_ok(), count == 3);
            state.origin_control.qualification = crate::short_manifest::HlsOriginQualification::Qualified {
                basis: crate::short_manifest::HlsOriginQualificationBasis::capture(&state),
                generation: crate::manifest_acceptance::HlsManifestAcceptanceGeneration(1),
                windows: crate::short_manifest::HlsQualifiedOriginWindows::ShortAllowed,
                publication_boundary: None,
            };
            assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_ok());
            state.activity.origin_work_generation += 1;
            assert!(!plan.is_current(&state));
            assert!(state.plan_live_window(HlsLiveWindowView::Entry).is_none());
            assert_eq!(
                HlsManifestRenderer::render_live_window(&state, &plan, 0),
                Err(crate::renderer::RenderError::InvalidState),
            );
        }
    }
}

#[tokio::test]
async fn live_window_http_fills_selected_head_with_existing_progressive_delivery() {
    assert_live_window_http_fill(None).await;
}

#[tokio::test]
#[ignore = "manual probe: set TULIPROX_HLS_WINDOW_ORIGIN to a controlled streaming fixture"]
async fn live_window_http_fills_selected_head_against_manual_origin() {
    let origin = std::env::var("TULIPROX_HLS_WINDOW_ORIGIN").expect("manual origin URL must be configured");
    assert_live_window_http_fill(Some(&origin)).await;
}

async fn assert_live_window_http_fill(external: Option<&str>) {
    let server = spawn_segment_server(0).await;
    let base = external.unwrap_or(&server.base_url);
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy { max_prefetch_queue_depth: 0, ..Default::default() };
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            let origin = if external.is_some() {
                let url = format!("{base}/index.m3u8");
                let body =
                    context.client.get(&url).send().await.unwrap().error_for_status().unwrap().text().await.unwrap();
                match tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(&body, &url) {
                    tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) => manifest,
                    tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::TransientPassthrough {
                        ..
                    } => panic!("expected ordinary live origin"),
                }
            } else {
                manifest(base, 0, &[4; 6])
            };
            state.apply_origin_manifest(&origin).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
            state.configure_segment_prefetch_queue(0);
        }
        install_startup(&context, &worker, mode).await;
        context.session.write().await.startup.as_mut().unwrap().worker = std::sync::Weak::new();
        super::grant_usable_worker_access_lease_at(
            &worker,
            &context.session.read().await.proxy_session_id,
            tuliprox_core::utils::current_time_millis(),
        )
        .await;
        context.session.write().await.queue_manifest_fetch_candidates(10, false);
        for seq in [2, 3, 4] {
            let snapshot = worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap();
            assert_eq!(snapshot.proxy_seq, seq);
            let task_context = context.clone();
            let task_snapshot = snapshot.clone();
            let task_policy = policy.clone();
            let task = tokio::spawn(async move {
                super::super::fetch_segment_into_cache(&task_context, &task_snapshot, &task_policy).await
            });
            if external.is_some() && mode != HlsStartupMode::FirstReady {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if context
                            .session
                            .read()
                            .await
                            .startup
                            .as_ref()
                            .unwrap()
                            .revisions
                            .get(&seq)
                            .is_some_and(|revision| revision.revision().is_publishable())
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let revision = context.session.read().await.startup.as_ref().unwrap().revisions[&seq].clone();
                let mut response =
                    crate::progressive_startup::revision_body(revision, std::time::Duration::from_secs(5), 0, None);
                let prefix = futures::StreamExt::next(&mut response).await.unwrap().unwrap();
                assert!(prefix.starts_with(b"segment-"));
                assert!(prefix.len() < 12);
                assert!(!task.is_finished(), "raw prefix must arrive before held origin EOF");
            }
            let metadata = task.await.unwrap().unwrap();
            assert!(metadata.content_length > 0);
            let mut state = context.session.write().await;
            state.active_segment_fetches -= 1;
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: metadata.content_length, ready_at_ms: 12 };
        }
        let state = context.session.read().await;
        let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(
            HlsManifestRenderer::render_live_window(&state, &plan, 12).unwrap().segment_proxy_seqs,
            vec![2, 3, 4]
        );
    }
}

#[tokio::test]
async fn live_window_readiness_preserves_active_failures_and_invalidates_failed_entry_plans() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        state.apply_origin_manifest(&manifest(&server.base_url, 1, &[4; 6])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
        let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        let head = plan.visible[0];
        prepare_selected(&mut state, &plan.visible, mode, false);
        let startup = state.startup.as_mut().unwrap();
        let entry = startup.revisions.remove(&head).unwrap();
        assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_err());
        state.startup.as_mut().unwrap().revisions.insert(head, entry);
        assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_ok());
        let successor = plan.visible[1];
        state.startup.as_ref().unwrap().revisions[&successor].revision().fail();
        assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_err());
        state.segments.get_mut(&head).unwrap().status =
            SegmentCacheStatus::FailedPermanent { failed_at_ms: 0, status: Some(axum::http::StatusCode::NOT_FOUND) };
        let origin_head = state.publishable_origin_head_proxy_seq;
        super::super::recompute_unpublished_live_head(&mut state);
        assert_eq!(state.publishable_origin_head_proxy_seq, origin_head);
        assert!(!plan.is_current(&state));
        assert_ne!(state.plan_live_window(HlsLiveWindowView::Entry).unwrap().visible, plan.visible);
        let active = state.plan_live_window(HlsLiveWindowView::Active { first_published: head }).unwrap();
        assert_eq!(active.visible.first(), Some(&head));
        state.apply_origin_manifest(&manifest(&server.base_url, 2, &[4; 6])).unwrap();
        assert!(!plan.is_current(&state));
        assert!(HlsManifestRenderer::render_live_window(&state, &plan, 0).is_err());
        state.apply_origin_manifest(&manifest(&server.base_url, 8, &[])).unwrap();
        assert!(state.uses_fast_start_live_window());
        assert!(state.plan_live_window(HlsLiveWindowView::Entry).is_none());
        assert!(HlsManifestRenderer::render(&state, 0).is_err());
    }
}

#[tokio::test]
async fn live_window_active_successors_are_required_with_zero_reserve_and_demand_keeps_priority() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy =
            SegmentFetchPolicy { max_prefetch_queue_depth: 0, max_session_segment_fetches: 3, ..Default::default() };
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[4; 6])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
            state.configure_segment_prefetch_queue(0);
        }
        install_startup(&context, &worker, mode).await;
        {
            let mut state = context.session.write().await;
            prepare_selected(&mut state, &[2, 3, 4], mode, true);
            for seq in [2, 3, 4] {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 0 };
            }
            state.live_window_demands.insert(
                crate::HlsAccessLeaseId("active".into()),
                crate::live_window::HlsLiveWindowDemand {
                    issued_at_ms: 0,
                    view: HlsLiveWindowView::Active { first_published: 2 },
                    published: vec![2, 3, 4],
                },
            );
            let report = state.queue_manifest_fetch_candidates(10, false);
            assert_eq!(report.render_window_queued, 1);
            assert_eq!(report.prefetch_queued, 0);
            assert!(state.queue_segment_fetch_candidate(0, SegmentFetchPriority::Demand, 10));
        }
        let demand = worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap();
        assert_eq!((demand.proxy_seq, demand.priority), (0, SegmentFetchPriority::Demand));
        let successor = worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap();
        assert_eq!((successor.proxy_seq, successor.priority), (5, SegmentFetchPriority::RenderWindow));
        assert!(worker.next_fetch_snapshot(&context, 11, &policy).await.is_none());
    }
}

#[tokio::test]
async fn live_window_entry_barrier_and_parallel_limits_follow_the_selected_head() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        for parallelism in [1, 2, 3] {
            let directory = tempfile::tempdir().unwrap();
            let policy = SegmentFetchPolicy { max_session_segment_fetches: parallelism, ..Default::default() };
            let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
            {
                let mut state = context.session.write().await;
                *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
                state.apply_origin_manifest(&manifest(&server.base_url, 0, &[4; 6])).unwrap();
                state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
            }
            install_startup(&context, &worker, mode).await;
            context.session.write().await.queue_manifest_fetch_candidates(10, false);
            assert_eq!(worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap().proxy_seq, 2);
            if mode != HlsStartupMode::Optimistic {
                assert!(worker.next_fetch_snapshot(&context, 11, &policy).await.is_none());
                prepare_selected(&mut *context.session.write().await, &[2], mode, false);
            }
            for seq in 3..2 + u64::try_from(parallelism).unwrap() {
                assert_eq!(worker.next_fetch_snapshot(&context, 11, &policy).await.unwrap().proxy_seq, seq);
            }
            assert!(worker.next_fetch_snapshot(&context, 11, &policy).await.is_none());
            assert_eq!(context.session.read().await.active_segment_fetches, parallelism);
        }
    }
}

#[tokio::test]
async fn live_window_raw_fallback_is_decided_by_the_selected_head_even_with_an_older_demand() {
    let server = spawn_segment_server(0).await;
    for mode in [HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[4; 6])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
        }
        install_startup(&context, &worker, mode).await;
        {
            let mut state = context.session.write().await;
            let startup = state.startup.as_mut().unwrap();
            startup.config.max_progressive_bytes_per_segment = shared::model::Bytes::new(4);
            startup.worker = std::sync::Weak::new();
            state.queue_manifest_fetch_candidates(10, false);
            state.queue_segment_fetch_candidate(0, SegmentFetchPriority::Demand, 10);
        }
        super::grant_usable_worker_access_lease_at(
            &worker,
            &context.session.read().await.proxy_session_id,
            tuliprox_core::utils::current_time_millis(),
        )
        .await;
        for seq in [0, 2] {
            let snapshot = worker.next_fetch_snapshot(&context, 10, &policy).await.unwrap();
            assert_eq!(snapshot.proxy_seq, seq);
            let result = super::super::fetch_segment_into_cache(&context, &snapshot, &policy).await;
            assert!(result.is_ok());
            let mut state = context.session.write().await;
            worker.apply_segment_fetch_result(&mut state, &snapshot, &policy, result, 11);
            if seq == 0 {
                assert_eq!(state.startup_mode(), mode, "an older demand cannot choose the entry fallback");
                assert!(!state.startup.as_ref().unwrap().policy_fixed);
            } else {
                assert_eq!(state.startup_mode(), HlsStartupMode::FirstReady);
                assert!(state.startup.as_ref().unwrap().policy_fixed);
            }
        }
    }
}
