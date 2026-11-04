use super::*;
use crate::{HlsFreshManifestRequiredReason, HlsProxyManager};
use axum::{body::Body, extract::State, http::Uri, response::Response, routing::any, Router};
use shared::model::{HlsManifestRecoveryBurstLevel, HlsStartupMode};
use std::{fmt::Write as _, sync::Arc, time::Duration};

#[derive(Clone)]
struct OriginState {
    bytes: Arc<tokio::sync::RwLock<Vec<u8>>>,
    requests: Arc<tokio::sync::Mutex<Vec<String>>>,
    manifests: Arc<tokio::sync::Mutex<std::collections::VecDeque<String>>>,
    fallback_manifest: Arc<tokio::sync::RwLock<Option<String>>>,
    new_namespace_bytes: Arc<tokio::sync::RwLock<Option<Vec<u8>>>>,
}

struct Origin {
    base: String,
    state: OriginState,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Origin {
    fn drop(&mut self) { self.task.abort(); }
}

impl Origin {
    async fn start() -> Self {
        let state = OriginState {
            bytes: Arc::new(tokio::sync::RwLock::new(media(1))),
            requests: Arc::default(),
            manifests: Arc::default(),
            fallback_manifest: Arc::default(),
            new_namespace_bytes: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(any(|State(state): State<OriginState>, uri: Uri| async move {
                state.requests.lock().await.push(uri.to_string());
                if path_has_extension(uri.path(), "m3u8") {
                    let body = state.manifests.lock().await.pop_front();
                    return Response::new(Body::from(if let Some(body) = body {
                        body
                    } else {
                        state.fallback_manifest.read().await.clone().unwrap_or_default()
                    }));
                }
                let body = if uri.path().contains("fedcba9876543210") {
                    state.new_namespace_bytes.read().await.clone()
                } else {
                    None
                };
                Response::new(Body::from(body.unwrap_or(state.bytes.read().await.clone())))
            }))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }
}

fn media(marker: u8) -> Vec<u8> {
    let mut bytes = CRITICAL_HANDOFF_TS_BODY.to_vec();
    bytes.extend_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    bytes.extend_from_slice(&[marker; 184]);
    bytes
}

fn playlist(base: &str, namespace: &str, count: u64, query: &str) -> FetchedOriginManifest {
    let mut body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n".to_owned();
    for seq in 0..count {
        writeln!(body, "#EXTINF:4,\n{base}/{namespace}/{seq}.ts{query}").unwrap();
    }
    let mut fetched = fetched_manifest(&body);
    fetched.final_manifest_url = format!("{base}/index.m3u8");
    fetched.resolved_request_url = fetched.final_manifest_url.clone();
    fetched.redirect_host = Some("127.0.0.1".into());
    fetched
}

async fn warm(origin: &Origin, mode: HlsStartupMode) -> OriginRefreshRequest {
    let manager = Arc::new(HlsProxyManager::with_private_cache(&shared::model::HlsCacheConfigDto {
        startup: shared::model::HlsStartupConfigDto { mode, ..Default::default() },
        ..Default::default()
    }));
    let session = manager.get_or_create_session(crate::HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(manager.segment_cache()), &origin.base);
    request.hls_proxy = manager;
    if let Some(startup) = session.write().await.startup.as_mut() {
        startup.worker = Arc::downgrade(&request.segment_worker_pool);
        startup.policy_fixed = true;
    }
    let fetched = playlist(&origin.base, "0123456789abcdef", 5, "?token=old");
    let context = manifest_fetch_context(&request, None);
    super::super::commit_initial_fetched_manifest(&request, &context, fetched, None, false).await.unwrap();
    let context = crate::SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::clone(&request.segment_cache),
        segment_repair: Arc::clone(&request.segment_repair),
        repair_access_lease_id: None,
        headers: axum::http::HeaderMap::new(),
        origin_provider_session_headers: axum::http::HeaderMap::new(),
        client: request.client.clone(),
        no_redirect_client: request.no_redirect_client.clone(),
        use_manual_redirects: false,
        origin_io: None,
    };
    for seq in 0..5 {
        assert_eq!(
            request
                .segment_worker_pool
                .demand_fetch_and_wait(
                    context.clone(),
                    &crate::HlsSegmentFile { proxy_seq: seq, extension: "ts".into() },
                    100
                )
                .await,
            crate::segment_fetcher::SegmentDemandFetchOutcome::Ready
        );
    }
    assert_eq!(session.read().await.origin_seq_highwater, Some(4));
    request
}

async fn authorize_reentry(request: &mut OriginRefreshRequest) {
    let mut session = request.session.write().await;
    let generation = session.origin_control.progress_generation;
    session.require_live_entry_revalidation(generation);
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::OriginReserveExhausted,
    };
    request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        false,
        session.live_entry_revalidation_generation(),
    )));
}

async fn candidate(
    request: &OriginRefreshRequest,
    fetched: FetchedOriginManifest,
) -> Result<super::super::CommittedOriginManifest, HlsManifestCommitError> {
    super::super::commit_manifest_recovery_candidate(
        request,
        fetched,
        request.manifest_commit_requirement.acceptance_mode(),
    )
    .await
}

#[tokio::test]
async fn restart_with_new_paths_accepts_lower_and_equal_highwater() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        for count in [4, 5] {
            let origin = Origin::start().await;
            let mut request = warm(&origin, mode).await;
            let (epoch, progress, old_key) = {
                let session = request.session.read().await;
                (
                    session.origin_epoch,
                    session.origin_control.progress_generation,
                    session.segments[&0].cache_key.clone(),
                )
            };
            let old_bytes =
                tokio::fs::read(request.segment_cache.metadata(&old_key).await.unwrap().unwrap().path).await.unwrap();
            authorize_reentry(&mut request).await;
            *origin.state.bytes.write().await = media(2);
            let fetched = playlist(&origin.base, "fedcba9876543210", count, "?token=new");
            assert!(candidate(&request, fetched.clone()).await.is_ok(), "{mode:?}/{count}");
            let session = request.session.read().await;
            assert_eq!(session.origin_epoch, epoch + 1);
            assert_eq!(session.origin_seq_highwater, Some(count - 1));
            assert_eq!(session.proxy_next_seq, Some(5 + count));
            assert!(session.segments[&5].discontinuity_before);
            assert!(!session.live_entry_revalidation.required);
            assert!(session.origin_control.progress_generation > progress);
            assert!(session.last_rendered_manifest.as_ref().is_some_and(|manifest| manifest.first_proxy_seq >= 5));
            for seq in 5..5 + count {
                assert_ne!(session.segments[&seq].cache_key, old_key);
                let revision = session.startup.as_ref().unwrap().revisions[&seq].revision();
                assert_eq!(
                    tokio::fs::read(revision.wait_complete(tokio::time::Instant::now()).await.unwrap().path)
                        .await
                        .unwrap(),
                    media(2)
                );
            }
            drop(session);
            assert_eq!(
                tokio::fs::read(request.segment_cache.metadata(&old_key).await.unwrap().unwrap().path).await.unwrap(),
                old_bytes
            );
            request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
            request.attempt = None;
            assert!(candidate(&request, fetched).await.is_ok());
            assert_eq!(request.session.read().await.origin_epoch, epoch + 1);
        }
    }
}

#[tokio::test]
async fn short_outage_restart_requires_failure_and_a_completed_burst() {
    for mode in [
        HlsStartupMode::Conservative,
        HlsStartupMode::FirstReady,
        HlsStartupMode::Progressive,
        HlsStartupMode::Optimistic,
    ] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, mode).await;
        request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
        let fetched = playlist(&origin.base, "fedcba9876543210", 5, "?token=new");
        *origin.state.bytes.write().await = media(2);
        {
            let mut session = request.session.write().await;
            let plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
            session.origin_control.begin_acceptance_episode(
                100,
                plan,
                HlsManifestAcceptanceTrigger::Observe,
                &test_acceptance_episode_timing(100, plan),
            );
            session.origin_control.acceptance_episode.as_mut().unwrap().record_full_burst();
        }
        assert!(candidate(&request, fetched.clone()).await.is_err(), "healthy Observe cannot authorize reset");
        {
            let mut session = request.session.write().await;
            session.origin_refresh.mark_failure(200);
            session.origin_control.acceptance_episode.as_mut().unwrap().full_burst_completed = false;
        }
        assert!(candidate(&request, fetched.clone()).await.is_err(), "failure alone cannot skip the burst");
        request.session.write().await.origin_control.acceptance_episode.as_mut().unwrap().record_full_burst();
        assert!(candidate(&request, fetched).await.is_ok(), "observed short outage may verify new bytes: {mode:?}");
        assert_eq!(request.session.read().await.origin_epoch, 1);
    }
}

#[tokio::test]
async fn recovery_burst_verifies_same_host_resets_and_rejects_replayed_bytes() {
    for mode in [
        HlsStartupMode::Conservative,
        HlsStartupMode::FirstReady,
        HlsStartupMode::Progressive,
        HlsStartupMode::Optimistic,
    ] {
        for fresh in [false, true] {
            let origin = Origin::start().await;
            let mut request = warm(&origin, mode).await;
            request.manifest_commit_requirement = HlsManifestCommitRequirement::CommittedManifestAllowed;
            {
                let mut session = request.session.write().await;
                session.origin_refresh.mark_failure(200);
                session.last_effective_manifest_host = Some("127.0.0.1".into());
                session.mark_authorized_media_access(super::super::current_time_millis());
            }
            let fetched = playlist(&origin.base, "fedcba9876543210", 5, "?token=new");
            *origin.state.fallback_manifest.write().await = Some(fetched.body);
            *origin.state.new_namespace_bytes.write().await = Some(media(if fresh { 2 } else { 1 }));
            let context = manifest_fetch_context(&request, Some(4));
            let binding =
                HlsManifestOriginBinding::new(format!("{}/index.m3u8", origin.base).parse().unwrap(), None).unwrap();
            let result = retry_hls_origin_manifest_recovery_chain(
                &context,
                binding,
                None,
                None,
                HlsManifestAcceptanceTrigger::RecoveryRequired,
                HlsManifestCommitAcceptanceMode::StrictPinnedHost,
                |fetched, mode| super::super::commit_manifest_recovery_candidate(&request, fetched, mode),
            )
            .await;
            assert_eq!(result.is_ok(), fresh, "{mode:?}: only fresh raw bytes prove restart: {:?}", result.err());
            assert_eq!(request.session.read().await.origin_epoch, u64::from(fresh));
        }
    }
}

#[tokio::test]
async fn restart_with_reused_urls_requires_fresh_origin_bytes() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, mode).await;
        let old_revision = request.session.read().await.startup.as_ref().unwrap().revisions[&0].clone();
        let before = origin.state.requests.lock().await.len();
        authorize_reentry(&mut request).await;
        *origin.state.bytes.write().await = media(3);
        assert!(candidate(&request, playlist(&origin.base, "0123456789abcdef", 5, "?token=old")).await.is_ok());
        assert_eq!(origin.state.requests.lock().await.len() - before, 5);
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 1);
        let new_revision = session.startup.as_ref().unwrap().revisions[&5].clone();
        assert_ne!(new_revision.revision().id(), old_revision.revision().id());
        drop(session);
        assert_eq!(
            tokio::fs::read(new_revision.revision().wait_complete(tokio::time::Instant::now()).await.unwrap().path)
                .await
                .unwrap(),
            media(3)
        );
        assert_eq!(
            tokio::fs::read(old_revision.revision().wait_complete(tokio::time::Instant::now()).await.unwrap().path)
                .await
                .unwrap(),
            media(1)
        );
    }
}

#[tokio::test]
async fn query_rotation_reuses_media_only_after_content_verification() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let origin = Origin::start().await;
        let request = warm(&origin, mode).await;
        let (epoch, progress, revision) = {
            let session = request.session.read().await;
            (
                session.origin_epoch,
                session.origin_control.progress_generation,
                session.startup.as_ref().unwrap().revisions[&0].clone(),
            )
        };
        let before = origin.state.requests.lock().await.len();
        let fetched = playlist(&origin.base, "0123456789abcdef", 5, "?unknown=1&unknown=2&token=new");
        assert!(candidate(&request, fetched.clone()).await.is_ok());
        assert_eq!(origin.state.requests.lock().await.len() - before, 5);
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, epoch);
        assert_eq!(session.origin_control.progress_generation, progress);
        assert_eq!(session.startup.as_ref().unwrap().revisions[&0], revision);
        assert!(session.segments[&0]
            .origin_fetch_ref
            .as_ref()
            .unwrap()
            .resolved_origin_url
            .ends_with("?unknown=1&unknown=2&token=new"));
        assert!(revision.revision().origin_binding_is_current(&session));
        drop(session);
        assert!(candidate(&request, fetched).await.is_ok());
        assert_eq!(origin.state.requests.lock().await.len() - before, 5, "no probe for an already verified poll");
        *origin.state.bytes.write().await = media(4);
        assert!(candidate(&request, playlist(&origin.base, "0123456789abcdef", 5, "?unknown=changed")).await.is_err());
        assert_eq!(request.session.read().await.origin_epoch, epoch, "active conflict cannot create an epoch");
    }
}

#[tokio::test]
async fn repeated_old_media_is_not_restart_progress() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, mode).await;
        authorize_reentry(&mut request).await;
        let before = request.session.read().await.origin_control.progress_generation;
        for query in ["?token=old", "?token=new"] {
            assert!(candidate(&request, playlist(&origin.base, "0123456789abcdef", 5, query)).await.is_err());
            let session = request.session.read().await;
            assert_eq!(session.origin_epoch, 0);
            assert_eq!(session.origin_control.progress_generation, before);
            assert!(session.live_entry_revalidation.required);
        }
    }
}

#[tokio::test]
async fn restart_commit_is_idempotent_and_generation_fenced() {
    for mutation in [0, 1, 2] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, HlsStartupMode::FirstReady).await;
        authorize_reentry(&mut request).await;
        *origin.state.bytes.write().await = media(5);
        let fetched = playlist(&origin.base, "fedcba9876543210", 4, "");
        let update = super::super::origin_probe::prepare_origin_update(
            &request,
            &fetched,
            request.manifest_commit_requirement.acceptance_mode(),
        )
        .await
        .unwrap()
        .unwrap();
        let mut session = request.session.write().await;
        match mutation {
            0 => session.activity.origin_work_generation += 1,
            1 => session.transient.invalidate_origin_resources(),
            _ => session.record_origin_manifest_commit(),
        }
        let result = super::super::commit::commit_fetched_manifest_with_origin_update(
            &mut session,
            &fetched,
            &request,
            200,
            request.manifest_commit_requirement.acceptance_mode(),
            None,
            Some(&update),
        );
        assert!(result.is_err());
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(5));
        let old_key = session.segments[&0].cache_key.clone();
        let old_access = Arc::clone(&session.segments[&0].access);
        let rejected_key = crate::SegmentCacheKey::new(session.proxy_session_id.clone(), 5, "ts");
        drop(session);
        drop(update);
        request.hls_proxy.garbage_collector().run_once(200).await.unwrap();
        assert!(request.segment_cache.metadata(&rejected_key).await.unwrap().is_none());
        assert!(request.segment_cache.metadata(&old_key).await.unwrap().is_some());
        assert_eq!(old_access.active_readers(), 0);
    }
}

#[tokio::test]
async fn cancelled_restart_probe_releases_capacity_and_reference_pins() {
    let origin = Origin::start().await;
    let mut request = warm(&origin, HlsStartupMode::FirstReady).await;
    authorize_reentry(&mut request).await;
    let (old_key, old_access) = {
        let session = request.session.read().await;
        (session.segments[&0].cache_key.clone(), Arc::clone(&session.segments[&0].access))
    };
    let previous_requests = origin.state.requests.lock().await.len();
    let body_gate = origin.state.bytes.write().await;
    let pending_request = request.clone();
    let fetched = playlist(&origin.base, "fedcba9876543210", 4, "");
    let task = tokio::spawn(async move {
        super::super::origin_probe::prepare_origin_update(
            &pending_request,
            &fetched,
            pending_request.manifest_commit_requirement.acceptance_mode(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while origin.state.requests.lock().await.len() == previous_requests {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fresh body probe is in flight");
    assert_eq!(request.session.read().await.active_segment_fetches, 1);
    assert!(old_access.active_readers() > 0);
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    drop(body_gate);
    tokio::time::timeout(Duration::from_secs(2), async {
        while request.session.read().await.active_segment_fetches != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled probe returns its media slot");

    request.origin_manifest_timeout_ms = 50;
    let body_gate = origin.state.bytes.write().await;
    let timed = tokio::time::timeout(
        Duration::from_secs(2),
        super::super::origin_probe::prepare_origin_update(
            &request,
            &playlist(&origin.base, "fedcba9876543210", 4, ""),
            request.manifest_commit_requirement.acceptance_mode(),
        ),
    )
    .await
    .expect("configured probe deadline finishes despite a blocked origin body");
    assert!(timed.is_err());
    drop(body_gate);
    tokio::time::timeout(Duration::from_secs(2), async {
        while request.session.read().await.active_segment_fetches != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out probe returns its media slot");
    request.hls_proxy.garbage_collector().run_once(200).await.unwrap();
    assert_eq!(old_access.active_readers(), 0);
    assert!(request.segment_cache.metadata(&old_key).await.unwrap().is_some());
    let session = request.session.read().await;
    assert_eq!(session.origin_epoch, 0);
    assert_eq!(session.proxy_next_seq, Some(5));
    let rejected = crate::SegmentCacheKey::new(session.proxy_session_id.clone(), 5, "ts");
    drop(session);
    assert!(request.segment_cache.metadata(&rejected).await.unwrap().is_none());
}

#[tokio::test]
async fn reentry_uses_remaining_budget_after_unchanged_http_200() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, mode).await;
        authorize_reentry(&mut request).await;
        request.session.write().await.origin_control.progress_phase =
            crate::origin_progress::HlsOriginProgressPhase::Terminal;
        assert!(request.session.read().await.established_manifest_recovery_binding().is_none());
        let old = playlist(&origin.base, "0123456789abcdef", 5, "?token=old").body;
        let new = playlist(&origin.base, "fedcba9876543210", 4, "?token=new").body;
        origin.state.manifests.lock().await.extend([old.clone(), old]);
        *origin.state.fallback_manifest.write().await = Some(new);
        *origin.state.new_namespace_bytes.write().await = Some(media(6));
        let result = fetch_and_commit_manifest_with_policy(&mut request).await;
        assert!(
            result.is_ok(),
            "{mode:?}: a later candidate remains eligible; error={:?} used={} manifest_requests={}",
            result.as_ref().err(),
            request.attempt.as_ref().unwrap().logical_attempts(),
            origin.state.requests.lock().await.iter().filter(|path| path_has_extension(path, "m3u8")).count()
        );
        let attempt = request.attempt.as_ref().unwrap();
        assert!((2..=5).contains(&attempt.logical_attempts()));
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 1);
        assert_eq!(session.origin_seq_highwater, Some(3));
        assert!(!session.live_entry_revalidation.required);
    }
}

#[tokio::test]
async fn reentry_exhaustion_is_bounded_and_does_not_publish_old_media() {
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, mode).await;
        authorize_reentry(&mut request).await;
        *origin.state.fallback_manifest.write().await =
            Some(playlist(&origin.base, "0123456789abcdef", 5, "?token=old").body);
        let before = request.session.read().await.origin_control.progress_generation;
        assert!(fetch_and_commit_manifest_with_policy(&mut request).await.is_err());
        assert!(
            (2..=5).contains(&request.attempt.as_ref().unwrap().logical_attempts()),
            "used={} requests={}",
            request.attempt.as_ref().unwrap().logical_attempts(),
            origin.state.requests.lock().await.iter().filter(|path| path_has_extension(path, "m3u8")).count()
        );
        let manifests =
            origin.state.requests.lock().await.iter().filter(|path| path_has_extension(path, "m3u8")).count();
        assert!(manifests <= request.manifest_recovery_burst.level.plan().total_candidates() + 4);
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(5));
        assert_eq!(session.origin_control.progress_generation, before);
        assert!(session.live_entry_revalidation.required);
    }
}

#[tokio::test]
async fn restart_preserves_range_map_and_encryption_boundaries() {
    for tag in [
        "#EXT-X-MAP:URI=\"init.mp4\"\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000001\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"other-key.bin\",IV=0x00000000000000000000000000000002\n",
        "#EXT-X-BYTERANGE:188@188\n",
    ] {
        let origin = Origin::start().await;
        let mut request = warm(&origin, HlsStartupMode::FirstReady).await;
        authorize_reentry(&mut request).await;
        *origin.state.bytes.write().await = media(7);
        let mut fetched = playlist(&origin.base, "fedcba9876543210", 4, "");
        fetched.body = fetched.body.replacen("#EXTINF:4,\n", &format!("{tag}#EXTINF:4,\n"), 1);
        assert!(candidate(&request, fetched).await.is_err(), "binding mismatch must not publish a candidate");
        let session = request.session.read().await;
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(5));
        let key = crate::SegmentCacheKey::new(session.proxy_session_id.clone(), 5, "ts");
        drop(session);
        assert!(request.segment_cache.metadata(&key).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn invalid_restart_media_and_missing_content_reference_remain_unverified() {
    let origin = Origin::start().await;
    let mut request = warm(&origin, HlsStartupMode::FirstReady).await;
    authorize_reentry(&mut request).await;
    *origin.state.bytes.write().await = b"invalid transport stream".to_vec();
    assert!(candidate(&request, playlist(&origin.base, "fedcba9876543210", 4, "")).await.is_err());
    *origin.state.bytes.write().await = media(8);
    request.session.write().await.origin_content = super::super::origin_probe::HlsOriginContentEvidence::default();
    assert!(candidate(&request, playlist(&origin.base, "0123456789abcdef", 5, "?token=old")).await.is_err());
    assert_eq!(request.session.read().await.origin_epoch, 0);
}
