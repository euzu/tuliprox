mod publication_handoff;
mod retained;
mod startup_recovery;

use super::{normal_manifest, response_body, test_app_state_with_hls_proxy, try_test_hls_cached_manifest_response};
use crate::api::model::{HlsAccessLeaseId, HlsAccessLeaseState, HlsProxyManager, HlsSessionKey, SegmentCacheStatus};
use axum::http::StatusCode;
use shared::model::{HlsCacheConfigDto, HlsStartupMode, HlsStripMode};
use std::{fmt::Write as _, sync::Arc, time::Duration};
use tuliprox_core::{
    model::{HlsCacheConfig, StripConfig},
    utils::current_time_millis,
};

fn origin(
    first: u64,
    count: usize,
    durations: &[u64],
) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for index in 0..count {
        let sequence = first + u64::try_from(index).unwrap();
        writeln!(body, "#EXTINF:{},\nhttp://origin.invalid/{sequence}.ts", durations[index % durations.len()]).unwrap();
    }
    normal_manifest(&body)
}

fn prepare_revisions(session: &mut tuliprox_hls::api::HlsSession, sequences: &[u64]) {
    let mode = session.startup_mode();
    let startup = session.startup.as_mut().unwrap();
    for seq in sequences {
        if startup.revisions.contains_key(seq) {
            continue;
        }
        let revision = startup
            .store
            .create(
                session.proxy_session_id.clone(),
                *seq,
                if mode == HlsStartupMode::FirstReady {
                    tuliprox_hls::SegmentRevisionKind::Processed
                } else {
                    tuliprox_hls::SegmentRevisionKind::Raw
                },
            )
            .unwrap();
        revision
            .revision()
            .complete(tuliprox_hls::api::CachedSegmentMetadata { path: std::path::PathBuf::from("unused"), size: 1 });
        startup.revisions.insert(*seq, revision);
        session.segments.get_mut(seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: current_time_millis() };
    }
}

#[tokio::test]
async fn live_window_timeout_preserves_unpublished_fallback_and_existing_lease_retries() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        dto.startup.mode = mode;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        super::enable_channel_unavailable_custom_response(&app);
        let session = manager
            .get_or_create_session(HlsSessionKey::new(1, "unready-window"), b"secret", current_time_millis())
            .await;
        let proxy_id = session.read().await.proxy_session_id.clone();
        for has_origin in [false, true] {
            if has_origin {
                session.write().await.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            }
            for state in [HlsAccessLeaseState::Pending, HlsAccessLeaseState::Activated, HlsAccessLeaseState::Idle] {
                for wait in [Duration::ZERO, Duration::from_millis(1)] {
                    let id = HlsAccessLeaseId(format!("timeout-{has_origin}-{state:?}-{wait:?}"));
                    super::prepare_pending_test_hls_access_lease(&app, &proxy_id, &id).await;
                    let mut lease =
                        manager.access_lease_response_snapshot(&id, &proxy_id, current_time_millis()).await.unwrap();
                    lease.state = state;
                    if state == HlsAccessLeaseState::Activated {
                        lease.active_until_ms = Some(current_time_millis() + 60_000);
                    }
                    manager.prepare_access_lease(lease).await;
                    let response = try_test_hls_cached_manifest_response(
                        &app,
                        &session,
                        &id,
                        state,
                        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
                        None,
                        super::HlsCachedManifestOptions::initial(wait),
                    )
                    .await;
                    if state == HlsAccessLeaseState::Pending {
                        assert!(response.is_none(), "{mode}: unpublished fallback must remain reachable");
                        let fallback =
                            super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_id, &id)
                                .await;
                        assert_eq!(fallback.status(), StatusCode::OK);
                        let body = String::from_utf8(response_body(fallback).await.to_vec()).unwrap();
                        assert!(body.contains("#EXT-X-ENDLIST"));
                        assert!(!body.contains("/hls/shared/live/"));
                        super::disable_custom_stream_response(&app);
                        let fallback =
                            super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_id, &id)
                                .await;
                        assert_eq!(fallback.status(), StatusCode::NOT_FOUND);
                        super::enable_channel_unavailable_custom_response(&app);
                    } else {
                        let response = response.expect("existing leases keep the temporary failure response");
                        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                        assert!(response.headers().contains_key(axum::http::header::RETRY_AFTER));
                    }
                    let lease =
                        manager.access_lease_response_snapshot(&id, &proxy_id, current_time_millis()).await.unwrap();
                    assert!(lease.last_manifest_snapshot.is_none());
                }
            }
        }
    }
}

#[tokio::test]
async fn live_window_provisioning_fallback_wakes_scheduler_for_the_requesting_lease() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            max_segments_prefetch: 0,
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 1;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        super::owner_handoff::enable_hls_provisioning_custom_response(&app);
        let session = manager
            .get_or_create_session(HlsSessionKey::new(1, "provisioning-window"), b"secret", current_time_millis())
            .await;
        {
            let mut state = session.write().await;
            state.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
            prepare_revisions(&mut state, &[3, 4, 5]);
            state.render_and_store_manifest(current_time_millis()).unwrap();
        }
        let proxy_id = session.read().await.proxy_session_id.clone();
        let id = HlsAccessLeaseId("provisioning-lease".into());
        super::prepare_pending_test_hls_access_lease(&app, &proxy_id, &id).await;
        // Pause before origin I/O: this test verifies the scheduler handoff,
        // independently of whether a provider slot is available afterwards.
        let gate = manager.segment_worker_pool().pause_next_origin_prepare_for_test();
        let response = super::super::hls_shared_provisioning_timeline_manifest_response(
            &app,
            Some(&super::test_fingerprint()),
            &session,
            &id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 1 },
            None,
        )
        .await;
        assert!(response.is_none(), "the requesting lease still needs its earlier entry segment");
        tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
            .await
            .expect("the fallback must wake the scheduler for queued live-window work");
        {
            let state = session.read().await;
            assert_eq!(state.segment_prefetch_queue.proxy_seqs(), vec![2]);
            assert!(matches!(
                state.segments[&2].status,
                SegmentCacheStatus::Queued { priority: tuliprox_hls::api::SegmentFetchPriority::RenderWindow, .. }
            ));
        }
        assert!(manager.access_leases().write().await.remove_access_lease(&id).is_some());
        gate.prepare.notify_one();
        tokio::time::timeout(Duration::from_secs(1), gate.prepared.notified()).await.unwrap();
        gate.proceed.notify_one();
    }
}

#[tokio::test]
async fn live_window_api_matrix_has_exact_playlist_snapshot_and_no_second_strip() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        for (count, strip_mode, strip, expected) in [
            (6, HlsStripMode::Segments, 0, vec![3, 4, 5]),
            (6, HlsStripMode::Segments, 1, vec![2, 3, 4]),
            (6, HlsStripMode::Segments, 2, vec![1, 2, 3]),
            (6, HlsStripMode::Segments, 3, vec![0, 1, 2]),
            (6, HlsStripMode::Segments, u64::MAX, vec![0, 1, 2]),
            (12, HlsStripMode::Segments, 1, vec![8, 9, 10]),
            (6, HlsStripMode::Seconds, 0, vec![3, 4, 5]),
            (6, HlsStripMode::Seconds, 7, vec![2, 3, 4]),
            (6, HlsStripMode::Seconds, 8, vec![1, 2, 3]),
            (6, HlsStripMode::Seconds, 10, vec![1, 2, 3]),
            (6, HlsStripMode::Seconds, u64::MAX, vec![0, 1, 2]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut dto = HlsCacheConfigDto {
                cache_path: Some(directory.path().to_string_lossy().into_owned()),
                ..Default::default()
            };
            dto.startup.mode = mode;
            dto.strip.mode = strip_mode;
            dto.strip.value = strip;
            let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
            let app = test_app_state_with_hls_proxy(manager.clone());
            let session =
                manager.get_or_create_session(HlsSessionKey::new(1, "window"), b"secret", current_time_millis()).await;
            {
                let mut state = session.write().await;
                state.apply_origin_manifest(&origin(9_000_000, count, &[4, 4, 4, 4, 3, 7])).unwrap();
                state.live_window_strip = StripConfig { mode: strip_mode, value: strip };
                prepare_revisions(&mut state, &expected);
                state.render_and_store_manifest(current_time_millis()).unwrap();
            }
            let id = HlsAccessLeaseId("entry".into());
            let response = try_test_hls_cached_manifest_response(
                &app,
                &session,
                &id,
                HlsAccessLeaseState::Pending,
                &StripConfig { mode: strip_mode, value: strip },
                Some("/prefix"),
                super::HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{mode} {strip_mode:?} {strip}");
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            let lease = manager
                .access_lease_response_snapshot(&id, &session.read().await.proxy_session_id, current_time_millis())
                .await
                .unwrap();
            let snapshot = lease.last_manifest_snapshot.unwrap();
            assert_eq!(snapshot.visible_proxy_seqs().collect::<Vec<_>>(), expected);
            let uris = body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect::<Vec<_>>();
            assert_eq!(uris.len(), 3);
            for (uri, seq) in uris.iter().zip(expected) {
                assert!(uri.starts_with("/prefix/hls/shared/live/"));
                assert!(uri.ends_with(&format!("/{seq:06}.ts")));
                assert!(uri.contains("/entry/"));
            }
        }
    }
}

#[tokio::test]
async fn live_window_entry_does_not_depend_on_the_saved_shared_manifest_prefix() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 1;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let session = manager
            .get_or_create_session(HlsSessionKey::new(1, "saved-prefix"), b"secret", current_time_millis())
            .await;
        let saved = {
            let mut state = session.write().await;
            let startup = state.startup.take();
            state.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            for entry in state.segments.values_mut() {
                entry.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: current_time_millis() };
            }
            let saved = state.render_and_store_manifest(current_time_millis()).unwrap();
            assert_eq!(saved.segment_proxy_seqs, vec![0, 1, 2, 3, 4, 5]);
            state.startup = startup;
            for entry in state.segments.values_mut() {
                entry.status = SegmentCacheStatus::Discovered;
            }
            prepare_revisions(&mut state, &[2, 3, 4]);
            saved.body
        };
        let id = HlsAccessLeaseId("new-lease".into());
        let response = try_test_hls_cached_manifest_response(
            &app,
            &session,
            &id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 1 },
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert_eq!(body.lines().filter(|line| !line.starts_with('#') && !line.is_empty()).count(), 3);
        assert_eq!(session.read().await.last_rendered_manifest.as_ref().unwrap().body, saved);
    }
}

#[tokio::test]
async fn live_window_leases_enter_independently_and_active_windows_only_grow_forward() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 1;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let session =
            manager.get_or_create_session(HlsSessionKey::new(1, "leases"), b"secret", current_time_millis()).await;
        {
            let mut state = session.write().await;
            state.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
            prepare_revisions(&mut state, &[2, 3, 4]);
            state.render_and_store_manifest(current_time_millis()).unwrap();
        }
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
        let first = HlsAccessLeaseId("first".into());
        for (slide, expected) in [
            (None, vec![2, 3, 4]),
            (Some(0), vec![2, 3, 4, 5]),
            (Some(1), vec![2, 3, 4, 5, 6]),
            (Some(2), vec![2, 3, 4, 5, 6, 7]),
            (Some(2), vec![2, 3, 4, 5, 6, 7]),
        ] {
            if let Some(slide) = slide {
                let mut state = session.write().await;
                state.apply_origin_manifest(&origin(slide, 6, &[4])).unwrap();
                prepare_revisions(&mut state, &expected);
                state.render_and_store_manifest(current_time_millis()).unwrap();
            }
            let response = try_test_hls_cached_manifest_response(
                &app,
                &session,
                &first,
                HlsAccessLeaseState::Pending,
                &strip,
                None,
                super::HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response_body(response).await;
            let id = session.read().await.proxy_session_id.clone();
            let mut lease = manager.access_lease_response_snapshot(&first, &id, current_time_millis()).await.unwrap();
            assert_eq!(
                lease.last_manifest_snapshot.as_ref().unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
                expected
            );
            // Preserve this exact lease's bindings and progressive claim state.
            lease.state = HlsAccessLeaseState::Activated;
            lease.active_until_ms = Some(current_time_millis() + 60_000);
            manager.prepare_access_lease(lease).await;
        }
        let second = HlsAccessLeaseId("second".into());
        let response = try_test_hls_cached_manifest_response(
            &app,
            &session,
            &second,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response_body(response).await;
        let id = session.read().await.proxy_session_id.clone();
        let lease = manager.access_lease_response_snapshot(&second, &id, current_time_millis()).await.unwrap();
        assert_eq!(lease.last_manifest_snapshot.unwrap().visible_proxy_seqs().collect::<Vec<_>>(), vec![4, 5, 6]);
        let first = manager.access_lease_response_snapshot(&first, &id, current_time_millis()).await.unwrap();
        assert_eq!(
            first.last_manifest_snapshot.unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
            vec![2, 3, 4, 5, 6, 7]
        );
    }
}

#[tokio::test]
async fn live_window_active_slides_preserve_duration_without_padding_another_leases_entry() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 0;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let session = manager
            .get_or_create_session(HlsSessionKey::new(1, "duration-slides"), b"secret", current_time_millis())
            .await;
        let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
        let id = HlsAccessLeaseId("active".into());
        for (first, durations, expected) in [
            (100, vec![6, 6, 6, 5], vec![1, 2, 3]),
            (101, vec![6, 6, 5, 7], vec![1, 2, 3, 4]),
            (103, vec![5, 7, 6], vec![2, 3, 4, 5]),
        ] {
            {
                let mut state = session.write().await;
                state.apply_origin_manifest(&origin(first, durations.len(), &durations)).unwrap();
                prepare_revisions(&mut state, &expected);
                state.render_and_store_manifest(current_time_millis()).unwrap();
            }
            let response = try_test_hls_cached_manifest_response(
                &app,
                &session,
                &id,
                HlsAccessLeaseState::Pending,
                &strip,
                None,
                super::HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{mode} slide {first}");
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert!(body.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", expected[0])));
            assert_eq!(body.lines().filter(|line| !line.starts_with('#') && !line.is_empty()).count(), expected.len());
            let proxy_id = session.read().await.proxy_session_id.clone();
            let mut lease =
                manager.access_lease_response_snapshot(&id, &proxy_id, current_time_millis()).await.unwrap();
            let snapshot = lease.last_manifest_snapshot.as_ref().unwrap();
            assert_eq!(snapshot.visible_proxy_seqs().collect::<Vec<_>>(), expected);
            if first != 100 {
                assert!(snapshot.playlist_duration_ms >= 21_000);
            }
            lease.state = HlsAccessLeaseState::Activated;
            lease.active_until_ms = Some(current_time_millis() + 60_000);
            manager.prepare_access_lease(lease).await;
        }
        let new_id = HlsAccessLeaseId("new".into());
        // Both the first publication and its active refresh must honor this lease's own head.
        for _ in 0..2 {
            let response = try_test_hls_cached_manifest_response(
                &app,
                &session,
                &new_id,
                HlsAccessLeaseState::Pending,
                &strip,
                None,
                super::HlsCachedManifestOptions::initial(Duration::ZERO),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response_body(response).await;
            let proxy_id = session.read().await.proxy_session_id.clone();
            let mut lease =
                manager.access_lease_response_snapshot(&new_id, &proxy_id, current_time_millis()).await.unwrap();
            let snapshot = lease.last_manifest_snapshot.as_ref().unwrap();
            assert_eq!(snapshot.visible_proxy_seqs().collect::<Vec<_>>(), vec![3, 4, 5]);
            assert_eq!(snapshot.playlist_duration_ms, 18_000);
            lease.state = HlsAccessLeaseState::Activated;
            lease.active_until_ms = Some(current_time_millis() + 60_000);
            manager.prepare_access_lease(lease).await;
        }
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "One ordered reload, re-entry and stale-publication transaction.")]
async fn live_window_config_changes_reentry_and_generation_races_preserve_lease_evidence() {
    use tuliprox_hls::api::{
        derive_hls_lease_manifest_snapshot, HlsLeaseManifestSnapshotInput, HlsLiveWindowView, HlsManifestRenderer,
    };
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        dto.startup.mode = mode;
        dto.strip.value = 1;
        let manager = Arc::new(HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto)));
        let app = test_app_state_with_hls_proxy(manager.clone());
        let session =
            manager.get_or_create_session(HlsSessionKey::new(1, "races"), b"secret", current_time_millis()).await;
        {
            let mut state = session.write().await;
            state.apply_origin_manifest(&origin(0, 6, &[4])).unwrap();
            state.live_window_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
            prepare_revisions(&mut state, &[2, 3, 4, 5]);
            state.render_and_store_manifest(current_time_millis()).unwrap();
        }
        let id = HlsAccessLeaseId("entry".into());
        let request_strip = StripConfig { mode: HlsStripMode::Segments, value: 1 };
        let request = || {
            try_test_hls_cached_manifest_response(
                &app,
                &session,
                &id,
                HlsAccessLeaseState::Pending,
                &request_strip,
                None,
                super::HlsCachedManifestOptions::initial(Duration::ZERO),
            )
        };
        let first = request().await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = response_body(first).await;
        let proxy = session.read().await.proxy_session_id.clone();
        let original = manager.access_lease_response_snapshot(&id, &proxy, current_time_millis()).await.unwrap();
        let mut leases = manager.access_leases().write().await;
        if mode != HlsStartupMode::FirstReady {
            assert!(leases.try_claim_progressive_startup(&id, 2, current_time_millis()));
        }
        drop(leases);
        // A larger strip must retain the published body, never rewind to 0,1,2.
        dto.strip.value = 3;
        let mut config = app.app_config.config.load().as_ref().clone();
        config.reverse_proxy =
            Some(tuliprox_core::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto {
                hls_cache: Some(dto.clone()),
                ..Default::default()
            }));
        app.app_config.config.store(Arc::new(config));
        manager.update_config(&app.app_config).await;
        let retained = request().await.unwrap();
        assert_eq!(retained.status(), StatusCode::OK);
        assert_eq!(response_body(retained).await, first_body);
        dto.strip.value = 0;
        let mut config = app.app_config.config.load().as_ref().clone();
        config.reverse_proxy.as_mut().unwrap().hls_cache = Some(HlsCacheConfig::from(&dto));
        app.app_config.config.store(Arc::new(config));
        manager.update_config(&app.app_config).await;
        let next = request().await.unwrap();
        assert_eq!(next.status(), StatusCode::OK);
        let _ = response_body(next).await;
        let mut lease = manager.access_lease_response_snapshot(&id, &proxy, current_time_millis()).await.unwrap();
        assert_eq!(
            lease.last_manifest_snapshot.as_ref().unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        lease.state = HlsAccessLeaseState::Idle;
        manager.prepare_access_lease(lease).await;
        let idle = request().await.unwrap();
        assert_eq!(idle.status(), StatusCode::OK);
        let _ = response_body(idle).await;
        if mode != HlsStartupMode::FirstReady {
            assert!(!manager.access_leases().write().await.try_claim_progressive_startup(
                &id,
                3,
                current_time_millis()
            ));
        }
        let guard =
            manager.prepare_access_lease_manifest_publication(&id, &proxy, current_time_millis()).await.unwrap();
        let stale = {
            let state = session.read().await;
            let plan = state.plan_live_window(HlsLiveWindowView::Entry).unwrap();
            let rendered = HlsManifestRenderer::render_live_window(&state, &plan, current_time_millis()).unwrap();
            derive_hls_lease_manifest_snapshot(
                &HlsLeaseManifestSnapshotInput::FastStartWindow {
                    session: &state,
                    plan: &plan,
                    rendered: &rendered,
                    materialized_body: &rendered.body,
                },
                current_time_millis(),
            )
            .unwrap()
            .unwrap()
        };
        session.write().await.activity.origin_work_generation += 1;
        assert!(!manager
            .commit_access_lease_manifest_publication(&id, &proxy, guard, stale, current_time_millis())
            .await
            .is_committed());
        let after = manager.access_lease_response_snapshot(&id, &proxy, current_time_millis()).await.unwrap();
        assert_eq!(
            after.last_manifest_snapshot.as_ref().unwrap().visible_proxy_seqs().collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(after.issued_at_ms, original.issued_at_ms);
    }
}
