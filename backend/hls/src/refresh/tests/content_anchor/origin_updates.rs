use super::*;
use crate::refresh::{
    origin_probe::{commit_pinned_manifest, prepare_origin_update, HlsOriginContentEvidence},
    tests::support::mark_full_burst_ready_for_switch_staging_at,
};
use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

const PINNED: HlsManifestCommitAcceptanceMode = HlsManifestCommitAcceptanceMode::StrictPinnedHost;

#[tokio::test]
async fn ap8_stale_entry_reconfirms_the_same_origin_window_without_media_probes() {
    let origin = Origin::start(false).await;
    let mut request = warm(&origin).await;
    let (commits, epoch, progress, sequences) = {
        let mut session = request.session.write().await;
        session.origin_refresh.last_success_at_ms = Some(request.now_ms - 4_500);
        (
            session.origin_manifest_commits(),
            session.origin_epoch,
            session.origin_control.progress_generation,
            session.live_origin_sequences().to_vec(),
        )
    };
    request.manifest_commit_requirement = crate::manifest_commit::hls_manifest_commit_requirement(
        &request.session,
        crate::session_store::HlsSessionStoreOutcome::Reused,
        None,
        true,
        request.now_ms,
    )
    .await;
    assert_eq!(
        request.manifest_commit_requirement.fresh_reason(),
        Some(crate::HlsFreshManifestRequiredReason::StaleEntryWindow)
    );
    let result = commit_pinned_manifest(&request, &origin.fetched(true, 432), PINNED).await;
    let state = request.session.read().await;
    let probes = origin.media_requests().len();
    println!(
        "ap8_stale_entry accepted={} commits={} probes={probes} epoch={} progress={}",
        result.is_ok(),
        state.origin_manifest_commits() - commits,
        state.origin_epoch,
        state.origin_control.progress_generation
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(probes, 0);
    assert_eq!(state.origin_manifest_commits(), commits + 1);
    assert_eq!((state.origin_epoch, state.origin_control.progress_generation), (epoch, progress));
    assert_eq!(state.live_origin_sequences(), sequences);
}

async fn ap8_handoff_window(origin: &Origin) -> OriginRefreshRequest {
    let request = warm(origin).await;
    let mut candidate = origin.fetched(false, 437);
    writeln!(candidate.body, "#EXTINF:8,\n{}/media/443.ts?fixture=1", origin.base(false)).unwrap();
    mark_full_burst_ready_for_switch_staging_at(&request.session, &candidate, request.now_ms).await;
    let committed = super::super::super::commit_manifest_recovery_candidate(
        &request,
        candidate,
        HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
    )
    .await
    .unwrap();
    {
        let mut state = request.session.write().await;
        // The runner normally completes the episode after the candidate commit.
        state.origin_control.acceptance_episode.as_mut().unwrap().complete();
        super::super::super::record_committed_manifest_success(
            &mut state,
            committed.progress_evidence,
            request.now_ms,
            request.hls_proxy.now_ms(),
        );
        assert!(state.origin_control.acceptance_episode.is_none());
        assert_eq!(state.origin_epoch, 1);
        assert_eq!(state.live_origin_sequences(), [51, 52, 53, 54, 55, 56]);
        assert_eq!(state.segments[&51].origin_key.host_local_sequence, 438);
        assert!(state.segments[&51].discontinuity_before);
        assert!(matches!(state.segments[&51].status, SegmentCacheStatus::Ready { .. }));
    }
    origin.state.requests.lock().unwrap().clear();
    request
}

#[tokio::test]
async fn ap8_handoff_stale_entry_uses_origin_timing() {
    let origin = Origin::start(false).await;
    let mut request = ap8_handoff_window(&origin).await;
    let current = origin.fetched(false, 438);
    for proxy_seq in 51..=56 {
        let outcome = request
            .segment_worker_pool
            .demand_fetch_and_wait(
                media_context(&request),
                &crate::HlsSegmentFile { proxy_seq, extension: "ts".into() },
                request.now_ms,
            )
            .await;
        assert_eq!(outcome, crate::segment_fetcher::SegmentDemandFetchOutcome::Ready);
    }
    origin.state.requests.lock().unwrap().clear();
    let (commits, progress) = {
        let mut state = request.session.write().await;
        if state.origin_qualification_awaits_publication() {
            let rendered = crate::renderer::HlsManifestRenderer::render(&state, request.now_ms).unwrap();
            assert_eq!(state.store_rendered_manifest(rendered), crate::renderer::RenderedManifestStoreOutcome::Stored);
        }
        assert!(!state.origin_qualification_awaits_publication());
        state.origin_refresh.last_success_at_ms = Some(request.now_ms - 4_500);
        (state.origin_manifest_commits(), state.origin_control.progress_generation)
    };
    request.manifest_commit_requirement = crate::manifest_commit::hls_manifest_commit_requirement(
        &request.session,
        crate::session_store::HlsSessionStoreOutcome::Reused,
        None,
        true,
        request.now_ms,
    )
    .await;
    assert_eq!(
        request.manifest_commit_requirement.fresh_reason(),
        Some(crate::HlsFreshManifestRequiredReason::StaleEntryWindow)
    );
    let result = commit_pinned_manifest(&request, &current, PINNED).await;
    let state = request.session.read().await;
    let probes = origin.media_requests().len();
    println!(
        "ap8_handoff_stale accepted={} probes={probes} commits={} proxy_marker={}",
        result.is_ok(),
        state.origin_manifest_commits() - commits,
        state.segments[&51].discontinuity_before
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(probes, 0);
    assert_eq!(state.origin_manifest_commits(), commits + 1);
    assert_eq!((state.origin_epoch, state.origin_control.progress_generation), (1, progress));
    assert_eq!(state.live_origin_sequences(), [51, 52, 53, 54, 55, 56]);
    assert!(state.segments[&51].discontinuity_before);
}

#[tokio::test]
async fn ap8_handoff_first_ready_slot_accepts_verified_query_rotation() {
    let origin = Origin::start(false).await;
    let request = ap8_handoff_window(&origin).await;
    let (key, status) = {
        let state = request.session.read().await;
        (state.segments[&51].cache_key.clone(), state.segments[&51].status.clone())
    };
    let mut rotated = origin.fetched(false, 438);
    rotated.body = rotated.body.replace("438.ts?fixture=1", "438.ts?fixture=2");
    let result = commit_pinned_manifest(&request, &rotated, PINNED).await;
    let state = request.session.read().await;
    let probes = origin.media_requests();
    let entry = &state.segments[&51];
    println!(
        "ap8_handoff_query accepted={} probes={} proxy_marker={}",
        result.is_ok(),
        probes.len(),
        entry.discontinuity_before
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(probes.len(), 1);
    assert!(probes[0].ends_with("/438.ts?fixture=2"));
    assert_eq!((entry.cache_key.clone(), entry.status.clone()), (key, status));
    assert!(entry.discontinuity_before);
    assert!(entry.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.ends_with("/438.ts?fixture=2"));
    assert!(state.origin_content.digest(entry, state.activity.origin_work_generation).is_some());
    drop(state);

    let mut altered_timing = rotated;
    altered_timing.body = altered_timing.body.replace("438.ts?fixture=2", "438.ts?fixture=3");
    altered_timing.body = altered_timing.body.replacen("#EXTINF:", "#EXT-X-DISCONTINUITY\n#EXTINF:", 1);
    assert!(commit_pinned_manifest(&request, &altered_timing, PINNED).await.is_err());
    assert_eq!(origin.media_requests().len(), 1, "a changed origin tag must be rejected before probing");
}

fn rotated_manifest(origin: &Origin) -> FetchedOriginManifest {
    let mut candidate = origin.fetched(true, 433);
    candidate.body = candidate.body.replace("438.ts?fixture=1", "438.ts?fixture=2");
    candidate
}

async fn assert_rebound_without_proof(request: &OriginRefreshRequest, origin: &Origin) -> SegmentCacheStatus {
    let state = request.session.read().await;
    let entry = &state.segments[&51];
    assert_eq!(entry.origin_key.host_local_sequence, 438);
    assert!(entry.origin_fetch_ref.as_ref().unwrap().resolved_origin_url.ends_with("438.ts?fixture=2"));
    assert!(state.origin_content.digest(entry, state.activity.origin_work_generation).is_none());
    assert_eq!(anchor(&state).proxy_seq, 50);
    assert_eq!(origin.media_requests(), [] as [String; 0]);
    entry.status.clone()
}

async fn unfinished_query_rotation(retained: bool) {
    let origin = Origin::start(false).await;
    let request = warm(&origin).await;
    discover_438(&request, &origin).await;
    if retained {
        let mut state = request.session.write().await;
        let binding = crate::HlsOriginAccountBinding::rebound(
            Arc::from("fixture"),
            Arc::from("successor"),
            crate::origin::build_hls_origin_session_owner(&state.proxy_session_id),
            2,
            request.now_ms,
        );
        state.handoff_origin_account(binding);
        assert!(state.segments[&51].origin_fetch_ref.is_none());
        assert!(state.segments[&51].retained_resource_identity.is_some());
        assert_eq!(state.segments[&51].status, SegmentCacheStatus::Discovered);
    }
    let status = request.session.read().await.segments[&51].status.clone();
    assert!(matches!(status, SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. }));
    let result = commit_pinned_manifest(&request, &rotated_manifest(&origin), PINNED).await;
    println!(
        "ap8_query_rotation retained={retained} accepted={} probes={}",
        result.is_ok(),
        origin.media_requests().len()
    );
    assert!(result.is_ok(), "{result:?}");
    let rebound_status = assert_rebound_without_proof(&request, &origin).await;
    if retained {
        // The normal manifest path can now schedule this previously unbound slot.
        assert!(matches!(
            rebound_status,
            SegmentCacheStatus::Queued { priority: crate::SegmentFetchPriority::RenderWindow, .. }
        ));
    } else {
        assert_eq!(rebound_status, status);
    }
    fetch_438(&request).await;
    assert!(origin.media_requests()[0].ends_with("/438.ts?fixture=2"));
    assert_eq!(origin.media_requests().len(), 1);
    println!("ap8_query_rotation retained={retained} slot=51 next_get=updated-query");
}

#[tokio::test]
async fn ap8_query_rotation_rebinds_an_unfinished_slot() { unfinished_query_rotation(false).await; }

#[tokio::test]
async fn ap8_query_rotation_rebinds_a_slot_with_only_retained_identity() { unfinished_query_rotation(true).await; }

#[tokio::test]
async fn ap8_query_rotation_fences_an_in_flight_old_response() {
    let origin = Origin::start(false).await;
    let request = warm(&origin).await;
    discover_438(&request, &origin).await;
    origin.state.pause_pinned.store(true, Ordering::Release);
    origin.state.pause_sequence.store(438, Ordering::Release);
    let worker = Arc::clone(&request.segment_worker_pool);
    let context = media_context(&request);
    let old_fetch = tokio::spawn(async move {
        worker
            .demand_fetch_and_wait(
                context,
                &crate::HlsSegmentFile { proxy_seq: 51, extension: "ts".into() },
                tuliprox_core::utils::current_time_millis(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), origin.state.entered.notified()).await.unwrap();
    assert!(origin.media_requests()[0].ends_with("/438.ts?fixture=1"));
    let status = request.session.read().await.segments[&51].status.clone();
    assert!(matches!(status, SegmentCacheStatus::Fetching { .. }));
    origin.state.requests.lock().unwrap().clear();
    let result = commit_pinned_manifest(&request, &rotated_manifest(&origin), PINNED).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(assert_rebound_without_proof(&request, &origin).await, status);
    origin.state.pause_sequence.store(0, Ordering::Release);
    origin.state.release.add_permits(8);
    let _ = old_fetch.await.unwrap();
    let key = request.session.read().await.segments[&51].cache_key.clone();
    assert!(request.segment_cache.metadata(&key).await.unwrap().is_none());
    assert_eq!(assert_rebound_without_proof(&request, &origin).await, SegmentCacheStatus::Discovered);
    fetch_438(&request).await;
    assert!(origin.media_requests()[0].ends_with("/438.ts?fixture=2"));
    assert_eq!(origin.media_requests().len(), 1);
    println!("ap8_query_rotation inflight_old_discarded=true status_released=true next_get=updated-query");
}

#[tokio::test]
async fn ap8_query_rotation_rechecks_a_slot_that_becomes_ready_before_commit() {
    let origin = Origin::start(false).await;
    let request = warm(&origin).await;
    discover_438(&request, &origin).await;
    let candidate = rotated_manifest(&origin);
    let update = prepare_origin_update(&request, &candidate, PINNED).await.unwrap().unwrap();
    fetch_438(&request).await;
    let OriginManifestParseOutcome::Normal(parsed) =
        parse_origin_media_manifest(&candidate.body, &candidate.final_manifest_url)
    else {
        panic!("expected regular candidate");
    };
    {
        let mut state = request.session.write().await;
        assert!(update.apply(&mut state, &parsed).is_err());
        assert!(state.segments[&51]
            .origin_fetch_ref
            .as_ref()
            .unwrap()
            .resolved_origin_url
            .ends_with("438.ts?fixture=1"));
        assert_eq!(HlsOriginContentEvidence::latest_ready(&state).unwrap().proxy_seq, 51);
    }
    origin.state.requests.lock().unwrap().clear();
    assert!(commit_pinned_manifest(&request, &candidate, PINNED).await.is_ok());
    assert!(origin.media_requests()[0].ends_with("/438.ts?fixture=2"));
    assert_eq!(origin.media_requests().len(), 1);
    println!("ap8_query_rotation became_ready=true stale_plan_rejected=true ready_probes=1");
}
