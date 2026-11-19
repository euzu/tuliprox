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
async fn rebase_continuation_renders_the_first_refreshed_window_in_all_startup_modes() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 37, &[7; 4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
            for segment in state.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
            }
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2, 3], mode, true);
        state.render_and_store_manifest(10).unwrap();
        let epoch = state.origin_epoch;
        let previous = state.last_rendered_manifest.clone();
        let owner = crate::origin::build_hls_origin_session_owner(&state.proxy_session_id);
        state.restart_admitted_origin_account(crate::HlsOriginAccountBinding::rebound(
            std::sync::Arc::from("input"),
            std::sync::Arc::from("account"),
            owner,
            1,
            30_000,
        ));

        state
            .apply_origin_rebase_manifest(
                &manifest(&server.base_url, 39, &[7; 4]),
                crate::timeline::effective_origin_host_id("127.0.0.1"),
                false,
            )
            .unwrap();
        assert_eq!(state.live_origin_sequences(), &[2, 3, 4, 5], "{mode:?}: preserve the cached overlap");
        assert_eq!(state.origin_epoch, epoch);
        let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
        assert_eq!(plan.visible, [2, 3, 4]);
        assert!(matches!(
            HlsManifestRenderer::render_live_window(&state, &plan, 20),
            Err(crate::renderer::RenderError::NoRenderableWindow)
        ));
        assert_eq!(state.last_rendered_manifest, previous);

        prepare_selected(&mut state, &[4], mode, true);
        state.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 30 };
        let rendered = HlsManifestRenderer::render_live_window(&state, &plan, 30)
            .expect("the first refreshed window renders as soon as its selected new segment is ready");
        assert_eq!(rendered.segment_proxy_seqs, [2, 3, 4]);
        assert!(rendered.body.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert_eq!(state.store_rendered_manifest(rendered), crate::RenderedManifestStoreOutcome::Stored);
        assert_eq!(state.proxy_next_seq, Some(6));
        assert!(matches!(state.segments[&5].status, SegmentCacheStatus::Discovered));
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
        // An explicit epoch handoff must still trim historical overlap.
        state.mark_pending_origin_epoch_handoff_discontinuity(0);
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
        let active =
            state.plan_live_window(HlsLiveWindowView::Active { first_published: head, last_published: None }).unwrap();
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
                    view: HlsLiveWindowView::Active { first_published: 2, last_published: None },
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

#[tokio::test]
async fn active_live_window_drops_trailing_unready_origin_tail_without_skipping_holes() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 8])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 5 };
            for seq in 0..=2 {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
            }
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2], mode, true);
        let published = state.render_and_store_manifest(10).expect("baseline is published");
        assert_eq!(published.segment_proxy_seqs, [0, 1, 2]);
        assert_eq!(state.last_rendered_manifest.as_ref().map(|m| m.last_proxy_seq), Some(2));

        // 2. Make 3,4,5 ready (bound revisions, Ready status) and leave 6,7 unfetched (Discovered, no revision).
        prepare_selected(&mut state, &[3, 4, 5], mode, true);
        for seq in 3..=5 {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20 };
        }
        assert!(matches!(state.segments[&6].status, SegmentCacheStatus::Discovered));
        assert!(matches!(state.segments[&7].status, SegmentCacheStatus::Discovered));
        assert!(!state.startup.as_ref().unwrap().revisions.contains_key(&6));
        assert!(!state.startup.as_ref().unwrap().revisions.contains_key(&7));

        // The viewer last received 2; ready 3..5 are published, unfetched 6..7 stay demanded.
        let plan = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) })
            .expect("active live window plan");
        assert_eq!(plan.visible, [0, 1, 2, 3, 4, 5], "{mode:?}: visible window drops unready trailing tail");
        assert_eq!(plan.pending_tail, [6, 7], "{mode:?}: the unready tail stays demanded");

        // 4. HlsManifestRenderer::render_live_window(&state, &plan, now) is Ok and its segment_proxy_seqs ends with 5 (HEAD: Err(NoRenderableWindow)).
        let rendered = HlsManifestRenderer::render_live_window(&state, &plan, 30)
            .expect("manifest renders with ready media prefix");
        assert_eq!(rendered.segment_proxy_seqs.last(), Some(&5));
        assert_eq!(rendered.segment_proxy_seqs, [0, 1, 2, 3, 4, 5]);

        // 5. Guard: if a sequence <= last published (e.g. 2) becomes unrenderable, it is not dropped (render fails as before)
        let revision_2 = state.startup.as_mut().unwrap().revisions.remove(&2).unwrap();
        let plan_unrenderable_published = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) })
            .expect("plan includes sequence <= last published even when unrenderable");
        assert!(
            plan_unrenderable_published.visible.contains(&2),
            "{mode:?}: sequences <= last published are never dropped"
        );
        assert!(
            matches!(
                HlsManifestRenderer::render_live_window(&state, &plan_unrenderable_published, 30),
                Err(crate::renderer::RenderError::NoRenderableWindow)
            ),
            "{mode:?}: unrenderable published sequence causes render to fail"
        );
        state.startup.as_mut().unwrap().revisions.insert(2, revision_2);

        // Guard: a hole (4 unrenderable, 5 ready) is never published as if 4 had been published.
        let revision_4 = state.startup.as_mut().unwrap().revisions.remove(&4).unwrap();
        let status_4 = state.segments.get(&4).unwrap().status.clone();
        state.segments.get_mut(&4).unwrap().status = SegmentCacheStatus::Discovered;

        let hole_plan = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) })
            .expect("plan with hole at 4");
        // The trim keeps the ready prefix as the published window, so the window still contains
        // the unrenderable 4 and ends on the last ready sequence.
        assert_eq!(hole_plan.visible, [0, 1, 2, 3, 4, 5], "{mode:?}: the ready prefix is the window");
        // The window never skips the hole: the renderer refuses the whole window instead of
        // serving 5 without 4.
        let available = state.live_origin_sequences().to_vec();
        let first_visible = hole_plan
            .visible
            .first()
            .and_then(|seq| available.iter().position(|candidate| candidate == seq))
            .expect("the visible window starts on an available sequence");
        assert_eq!(
            hole_plan.visible.as_slice(),
            available
                .get(first_visible..first_visible + hole_plan.visible.len())
                .expect("the visible window fits the available sequences"),
            "{mode:?}: the visible window is a contiguous run of the available sequences"
        );
        assert_eq!(
            HlsManifestRenderer::render_live_window(&state, &hole_plan, 30),
            Err(crate::renderer::RenderError::NoRenderableWindow),
            "{mode:?}: a window that contains the unrenderable 4 fails to render"
        );

        state.startup.as_mut().unwrap().revisions.insert(4, revision_4);
        state.segments.get_mut(&4).unwrap().status = status_4;
    }
}

#[tokio::test]
async fn active_live_window_preserves_demand_for_gap_sequences_beyond_demanded_tail() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 8])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 5 };
            for seq in 0..=2 {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
            }
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2], mode, true);
        // Baseline is published with 8 origin sequences (0..=7), 0..=2 ready.
        let published = state.render_and_store_manifest(10).expect("baseline is published");
        assert_eq!(published.segment_proxy_seqs, [0, 1, 2]);
        assert_eq!(state.last_rendered_manifest.as_ref().map(|m| m.last_proxy_seq), Some(2));

        // Origin updates to 15 sequences (0..=14).
        state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 15])).unwrap();

        // Advance ready prefix with 3..=4, leaving 5..=14 (10 unready sequences > 6) unready.
        prepare_selected(&mut state, &[3, 4], mode, true);
        for seq in 3..=4 {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20 };
        }

        let plan = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) })
            .expect("active live window plan");
        assert_eq!(plan.visible, [0, 1, 2, 3, 4], "{mode:?}: the trimmed ready prefix is the visible window");
        assert_eq!(
            plan.pending_tail,
            (5..=14).collect::<Vec<_>>(),
            "{mode:?}: every unready sequence past the visible end stays demanded"
        );
    }
}

#[tokio::test]
async fn active_live_window_republishes_the_ready_prefix_and_keeps_successors_demanded() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 8])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 5 };
            for seq in 0..=2 {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
            }
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2], mode, true);
        // Baseline is published with 8 origin sequences (0..=7), 0..=2 ready.
        let published = state.render_and_store_manifest(10).expect("baseline is published");
        assert_eq!(published.segment_proxy_seqs, [0, 1, 2]);
        assert_eq!(state.last_rendered_manifest.as_ref().map(|m| m.last_proxy_seq), Some(2));

        // Origin updates to 15 sequences (0..=14) and 3..=4 become renderable. Publish them, so
        // the active view below plans with the last sequence this session actually delivered.
        state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 15])).unwrap();
        prepare_selected(&mut state, &[3, 4], mode, true);
        for seq in 3..=4 {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20 };
        }
        let advanced = state.render_and_store_manifest(20).expect("the ready successors are published");
        assert_eq!(advanced.segment_proxy_seqs, [0, 1, 2, 3, 4]);
        assert_eq!(state.last_rendered_manifest.as_ref().map(|m| m.last_proxy_seq), Some(4));

        // The ready prefix ends exactly at the last published sequence (4). The plan republishes
        // that renderable window with a fresh validity and keeps 5..=14 demanded in origin order;
        // it must never fall back to the newest origin sequences the trim classified as unready.
        let plan = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(4) })
            .expect("active live window plan");
        assert_eq!(plan.visible, [0, 1, 2, 3, 4], "{mode:?}: the ready prefix is the visible window");
        assert_eq!(
            plan.pending_tail,
            (5..=14).collect::<Vec<_>>(),
            "{mode:?}: every successor past the ready prefix stays demanded"
        );
        let rendered =
            HlsManifestRenderer::render_live_window(&state, &plan, 30).expect("the republished ready prefix renders");
        assert_eq!(rendered.segment_proxy_seqs, [0, 1, 2, 3, 4]);

        // The scheduler must dispatch the demanded successor next, not the sequence the visible
        // window starts with: demand carries the whole plan, not only its published head.
        state.live_window_demands.insert(
            crate::HlsAccessLeaseId("active".into()),
            crate::live_window::HlsLiveWindowDemand {
                issued_at_ms: 0,
                view: HlsLiveWindowView::Active { first_published: 0, last_published: Some(4) },
                published: vec![0, 1, 2, 3, 4],
            },
        );
        let depth = state.segment_prefetch_queue.max_prefetch_depth();
        let unready_tail = plan
            .pending_tail
            .iter()
            .filter(|seq| !matches!(state.segments[seq].status, SegmentCacheStatus::Ready { .. }))
            .count();
        let report = state.queue_manifest_fetch_candidates(20, true);
        assert_eq!(
            report.render_window_queued, depth,
            "{mode:?}: only the head of the unready tail is promoted to the render window"
        );
        assert_eq!(
            report.prefetch_queued,
            unready_tail - depth,
            "{mode:?}: the rest of the unready tail stays prefetch"
        );
        drop(state);
        let next = worker.next_fetch_snapshot(&context, 21, &policy).await.expect("a demanded fetch is dispatched");
        assert_eq!((next.proxy_seq, next.priority), (5, SegmentFetchPriority::RenderWindow));
    }
}

#[tokio::test]
async fn active_live_window_promotes_only_the_head_of_a_stalled_tail() {
    let server = spawn_segment_server(0).await;
    for mode in MODES {
        let directory = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
        {
            let mut state = context.session.write().await;
            *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
            state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 8])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 5 };
            for seq in 0..=2 {
                state.segments.get_mut(&seq).unwrap().status =
                    SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
            }
        }
        install_startup(&context, &worker, mode).await;
        let mut state = context.session.write().await;
        prepare_selected(&mut state, &[0, 1, 2], mode, true);
        let published = state.render_and_store_manifest(10).expect("baseline is published");
        assert_eq!(published.segment_proxy_seqs, [0, 1, 2]);

        // The origin extends to 20 sequences while only 0..=4 become renderable: 15 sequences
        // stall behind the ready prefix for the whole window.
        state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 20])).unwrap();
        prepare_selected(&mut state, &[3, 4], mode, true);
        for seq in 3..=4 {
            state.segments.get_mut(&seq).unwrap().status =
                SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20 };
        }
        state.live_window_demands.insert(
            crate::HlsAccessLeaseId("active".into()),
            crate::live_window::HlsLiveWindowDemand {
                issued_at_ms: 0,
                view: HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) },
                published: vec![0, 1, 2],
            },
        );
        let plan = state
            .plan_live_window(HlsLiveWindowView::Active { first_published: 0, last_published: Some(2) })
            .expect("active live window plan");
        assert_eq!(plan.visible, [0, 1, 2, 3, 4], "{mode:?}: the ready prefix is the window");
        assert_eq!(plan.pending_tail, (5..=19).collect::<Vec<_>>(), "{mode:?}: the whole stalled tail stays demanded");

        let depth = state.segment_prefetch_queue.max_prefetch_depth();
        let report = state.queue_manifest_fetch_candidates(20, true);
        assert_eq!(report.render_window_queued, depth, "{mode:?}: the promoted tail is bounded by the prefetch depth");
        assert_eq!(
            report.prefetch_queued, depth,
            "{mode:?}: the rest of the stalled tail is prefetch behind its own depth"
        );

        // The promoted head is the render window; the rest of the tail waits behind it as
        // prefetch, so the next publication has its successor without the whole tail competing
        // for the shared origin permits at render-window priority.
        let mut render_window = Vec::new();
        let mut prefetch = Vec::new();
        while let Some((seq, priority)) = state.segment_prefetch_queue.pop_next() {
            match priority {
                SegmentFetchPriority::RenderWindow => render_window.push(seq),
                SegmentFetchPriority::Prefetch => prefetch.push(seq),
                SegmentFetchPriority::Demand => panic!("{mode:?}: the window queues no demand"),
            }
        }
        assert_eq!(
            render_window,
            plan.pending_tail.iter().copied().take(depth).collect::<Vec<_>>(),
            "{mode:?}: the render window is the head of the stalled tail"
        );
        assert_eq!(
            prefetch,
            plan.pending_tail.iter().copied().skip(depth).take(depth).collect::<Vec<_>>(),
            "{mode:?}: the rest of the tail is queued as prefetch"
        );
    }
}

fn manifest_with_replaced_segment_url(
    origin: &str,
    first: u64,
    durations: &[u64],
    replaced: u64,
) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for (offset, duration) in durations.iter().enumerate() {
        let seq = first + u64::try_from(offset).unwrap();
        if seq == replaced {
            writeln!(body, "#EXTINF:{duration},\n{origin}/{seq}.ts?fixture=2").unwrap();
        } else {
            writeln!(body, "#EXTINF:{duration},\n{origin}/{seq}.ts").unwrap();
        }
    }
    normal_manifest(&body)
}

#[tokio::test]
async fn manifest_refresh_replacing_fetch_reference_clears_gateway_timeout_prefix_shortcut() {
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    {
        let mut state = context.session.write().await;
        *state = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        state.apply_origin_manifest(&manifest(&server.base_url, 0, &[7; 6])).unwrap();
        state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    }
    install_startup(&context, &worker, HlsStartupMode::Optimistic).await;
    let mut state = context.session.write().await;
    prepare_selected(&mut state, &[0, 1, 2], HlsStartupMode::Optimistic, true);
    state.record_live_window_origin_commit(10);
    for seq in 0..=1 {
        state.segments.get_mut(&seq).unwrap().status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 10 };
    }
    state.segments.get_mut(&2).unwrap().status = SegmentCacheStatus::Fetching {
        priority: SegmentFetchPriority::RenderWindow,
        started_at_ms: 11,
        gateway_timeout: true,
    };
    // The gateway timeout of the last selected object licenses a two-segment prefix, but only
    // for the fetch reference that produced it.
    let prefix = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(prefix.visible, [0, 1]);
    assert_eq!(prefix.hidden_tail, [2, 3, 4, 5]);
    assert!(prefix.is_startup_recovery());

    // A refresh that replaces the concrete fetch reference of that sequence requires a fresh
    // timeout observation before the shortcut may apply again.
    let refresh = manifest_with_replaced_segment_url(&server.base_url, 0, &[7; 6], 2);
    let refreshed_ref = refresh
        .segments
        .iter()
        .find(|segment| segment.origin_seq == 2)
        .map(|segment| crate::timeline::OriginSegmentFetchRef {
            resolved_origin_url: segment.resolved_origin_url.clone(),
            byte_range: segment.origin_byte_range,
            valid_until_ms: None,
        })
        .expect("refreshed fetch reference for sequence 2");
    let host = state.segments[&2].origin_key.effective_host_id;
    state.apply_verified_query_manifest(&refresh, host, &[(2, refreshed_ref)]).unwrap();
    state.record_live_window_origin_commit(20);
    match &state.segments[&2].status {
        SegmentCacheStatus::Fetching { priority, started_at_ms, gateway_timeout } => {
            assert_eq!((*priority, *started_at_ms), (SegmentFetchPriority::RenderWindow, 11));
            assert!(!gateway_timeout, "gateway timeout recorded for the replaced fetch reference must be cleared");
        }
        other => panic!("the refresh must keep the status variant, got {other:?}"),
    }
    assert!(
        state.segments[&2]
            .origin_fetch_ref
            .as_ref()
            .is_some_and(|fetch_ref| fetch_ref.resolved_origin_url.ends_with("2.ts?fixture=2")),
        "the refresh must have replaced the fetch reference of sequence 2"
    );
    let refreshed = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
    assert_eq!(refreshed.visible, [0, 1, 2]);
    assert_eq!(refreshed.hidden_tail, [3, 4, 5]);
    assert!(!refreshed.is_startup_recovery());
}
