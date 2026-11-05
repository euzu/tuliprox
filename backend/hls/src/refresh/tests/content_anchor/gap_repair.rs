use super::*;
use crate::{
    manifest_snapshot::{derive_hls_lease_manifest_snapshot, HlsLeaseManifestSnapshotInput},
    media_reserve::{evaluate_lease_reserve, HlsLeaseManifestSnapshot, HlsLeasePlaybackCursor, HlsLeaseReserveInput},
    recovery_timing::HlsRecoveryTriggerBudgetMs,
    refresh::tests::support::mark_full_burst_ready_for_switch_staging_at,
    segment_fetcher::SegmentDemandFetchOutcome,
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseTiming, HlsPlaybackFamilyKey,
};
use tuliprox_core::utils::current_time_millis;

fn segment(proxy_seq: u64) -> crate::HlsSegmentFile { crate::HlsSegmentFile { proxy_seq, extension: "ts".into() } }

async fn demand(request: &OriginRefreshRequest, proxy_seq: u64) -> SegmentDemandFetchOutcome {
    request
        .segment_worker_pool
        .demand_fetch_and_wait(media_context(request), &segment(proxy_seq), current_time_millis())
        .await
}

struct GapCase {
    origin: Origin,
    request: OriginRefreshRequest,
    manifest: HlsLeaseManifestSnapshot,
    cursor: HlsLeasePlaybackCursor,
    old_slot: crate::SegmentEntry,
    anchor_slot: crate::SegmentEntry,
    anchor_bytes: Vec<u8>,
    measured_at_ms: u64,
}

fn rendered_snapshot(state: &crate::HlsSession, now_ms: u64) -> HlsLeaseManifestSnapshot {
    let rendered = state.last_rendered_manifest.as_ref().expect("rendered manifest");
    derive_hls_lease_manifest_snapshot(
        &HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
            session: state,
            committed_body: &rendered.body,
            materialized_body: &rendered.body,
            stripped_tail_segments: 0,
        },
        now_ms,
    )
    .unwrap()
    .unwrap()
}

async fn install_lease(
    request: &OriginRefreshRequest,
    manifest: &HlsLeaseManifestSnapshot,
    cursor: &HlsLeasePlaybackCursor,
    name: &str,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let id = HlsAccessLeaseId(name.to_string());
    let proxy = request.session.read().await.proxy_session_id.clone();
    let mut lease = HlsAccessLease::pending(
        id.clone(),
        HlsPlaybackFamilyKey::new(name, name),
        proxy.clone(),
        name.into(),
        name.into(),
        1,
        "stream".into(),
        1,
        now_ms,
        120_000,
    );
    lease.playback_cursor = cursor.clone();
    let mut leases = request.hls_proxy.access_leases().write().await;
    leases.prepare_access_lease(lease);
    let guard = leases.prepare_manifest_publication(&id, &proxy, now_ms).unwrap();
    assert!(leases.commit_manifest_publication(&id, &proxy, guard, manifest.clone(), now_ms).is_committed());
    assert!(leases
        .activate_access_lease(
            &id,
            &proxy,
            now_ms + 1,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 120_000 }
        )
        .is_activated());
    id
}

async fn gap_case(missing: bool) -> GapCase {
    let origin = Origin::start(false).await;
    let request = warm(&origin).await;
    origin.state.gap_a_438.store(missing, Ordering::Release);
    let measured_at_ms = current_time_millis() + 100;
    let (manifest, cursor) = {
        let mut state = request.session.write().await;
        state.render_and_store_manifest(current_time_millis()).unwrap();
        commit_fetched_manifest(&mut state, &origin.fetched(true, 434), &request, current_time_millis()).unwrap();
        let manifest = rendered_snapshot(&state, measured_at_ms - 10);
        assert!(manifest.visible_proxy_seqs().any(|seq| seq == 51), "438 has an existing client URI");
        let mut cursor = HlsLeasePlaybackCursor::default();
        let token = cursor.record_request_started(50, measured_at_ms - 8_000);
        let token = cursor.bind_request_timing(token, &state.segments[&50]);
        assert!(cursor.record_transfer_started(token, measured_at_ms - 8_000));
        let _ = cursor.record_request_completed(token, measured_at_ms - 7_900);
        (manifest, cursor)
    };
    let _ = install_lease(&request, &manifest, &cursor, "ap7-old", measured_at_ms - 10).await;
    assert_eq!(
        demand(&request, 51).await,
        if missing { SegmentDemandFetchOutcome::Unavailable } else { SegmentDemandFetchOutcome::Ready }
    );
    assert_eq!(demand(&request, 52).await, SegmentDemandFetchOutcome::Ready);
    let (old_slot, anchor_slot) = {
        let state = request.session.read().await;
        assert_eq!(anchor(&state).origin_key.host_local_sequence, 439);
        assert_eq!(state.segments[&51].origin_key.host_local_sequence, 438);
        if missing {
            assert!(matches!(
                state.segments[&51].status,
                SegmentCacheStatus::FailedPermanent { status: Some(HttpStatus::NOT_FOUND), .. }
            ));
        }
        (state.segments[&51].clone(), state.segments[&52].clone())
    };
    let anchor_bytes = cache_bytes(&request, &anchor_slot.cache_key).await;
    origin.state.requests.lock().unwrap().clear();
    GapCase { origin, request, manifest, cursor, old_slot, anchor_slot, anchor_bytes, measured_at_ms }
}

async fn cache_bytes(request: &OriginRefreshRequest, key: &crate::SegmentCacheKey) -> Vec<u8> {
    let metadata = request.segment_cache.metadata(key).await.unwrap().unwrap();
    tokio::fs::read(metadata.path).await.unwrap()
}

async fn recovery(case: &mut GapCase, head: u64, expected: HlsManifestCommitAcceptanceMode) -> bool {
    case.origin.state.head.store(head, Ordering::Release);
    let request = &mut case.request;
    request.attempt = Some(Arc::new(crate::manifest_fetch::HlsManifestAttemptContext::new(
        request.manifest_commit_requirement,
        true,
        None,
    )));
    let context = manifest_fetch_context(request, Some(8));
    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(request.origin_entry.url().clone()),
        None,
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, mode| {
            assert_eq!(mode, expected);
            crate::refresh::commit_manifest_recovery_candidate(request, fetched, mode)
        },
    )
    .await;
    println!("ap7_selection head={head} mode={expected:?} accepted={}", result.is_ok());
    result.is_ok()
}

fn reserve(
    state: &crate::HlsSession,
    manifest: &HlsLeaseManifestSnapshot,
    cursor: &HlsLeasePlaybackCursor,
    now_ms: u64,
) -> u64 {
    let timeline =
        state.ready_timeline_snapshot(cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq), now_ms);
    evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest,
        cursor,
        ready_timeline: &timeline,
        now_ms,
        recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(12_000),
        origin_path_degraded: state.origin_control.path_condition.is_degraded(),
        recovery_committed: false,
    })
    .guaranteed_reserve_ms
}

#[tokio::test]
async fn r1_fills_proxy_51_from_verified_b_without_moving_media() {
    let mut case = gap_case(true).await;
    assert!(
        recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate).await
    );
    let gets = case.origin.media_requests();
    let gap_gets = gets.iter().filter(|get| get.starts_with("localhost:") && get.contains("/438.ts?")).count();
    {
        let state = case.request.session.read().await;
        println!(
            "ap7_r1 manifest_gets={} segment_gets={gets:?} b438_gets={gap_gets} slot51={:?} epoch={}",
            case.origin.state.manifest_calls.load(Ordering::Acquire),
            state.segments[&51].status,
            state.origin_epoch
        );
        assert_eq!(gap_gets, 1, "B/438 must be loaded once into retained Proxy 51");
        assert!(matches!(state.segments[&51].status, SegmentCacheStatus::Ready { .. }));
        assert_eq!(state.origin_epoch, 1);
        assert_eq!(state.segments[&51].media_start_ms, case.old_slot.media_start_ms);
        assert_eq!(state.segments[&51].duration_ms, case.old_slot.duration_ms);
        assert_eq!(state.segments[&51].discontinuity_before, case.old_slot.discontinuity_before);
        assert_eq!(state.segments[&51].origin_key, case.old_slot.origin_key);
        assert!(state.segments[&51]
            .origin_fetch_ref
            .as_ref()
            .unwrap()
            .resolved_origin_url
            .starts_with("http://localhost:"));
        assert_eq!(state.segments[&52].cache_key, case.anchor_slot.cache_key);
        assert_eq!(state.segments[&53].origin_key.host_local_sequence, 440);
        assert!(state.segments[&53].discontinuity_before);
        assert_eq!(state.segments[&54].origin_key.host_local_sequence, 441);
    }
    assert_eq!(demand(&case.request, 54).await, SegmentDemandFetchOutcome::Ready);
    let before = case.origin.media_requests().len();
    assert_eq!(demand(&case.request, 51).await, SegmentDemandFetchOutcome::Ready);
    assert_eq!(case.origin.media_requests().len(), before);
    assert_eq!(cache_bytes(&case.request, &case.anchor_slot.cache_key).await, case.anchor_bytes);
    let (key, uri) = {
        let mut state = case.request.session.write().await;
        state.render_and_store_manifest(current_time_millis()).unwrap();
        let manifest = rendered_snapshot(&state, current_time_millis());
        assert_eq!(manifest.last_proxy_seq, 54);
        let uri = manifest.visible_segments.iter().find(|s| s.proxy_seq == 51).unwrap().uri.clone();
        let old_uri = &case.manifest.visible_segments.iter().find(|s| s.proxy_seq == 51).unwrap().uri;
        assert_eq!(&uri, old_uri);
        let reserve = reserve(&state, &case.manifest, &case.cursor, case.measured_at_ms);
        assert_eq!(reserve, 32_000);
        println!("ap7_r1 cursor=50 reserve_ms={reserve} stable_uri=true anchor_bytes_equal=true");
        (state.segments[&51].cache_key.clone(), uri)
    };
    assert_eq!(cache_bytes(&case.request, &key).await, raw_media(438));
    let segment = crate::HlsSegmentFile::parse(uri.rsplit('/').next().unwrap()).unwrap();
    let log_identity = crate::observability::HlsLogIdentity::from_session(&*case.request.session.read().await);
    let response_context = crate::response::HlsCacheResponseContext::new(
        HlsAccessLeaseId("ap7-old".into()),
        log_identity,
        1,
        Arc::clone(case.request.hls_proxy.metrics()),
        Arc::clone(case.request.hls_proxy.segment_repair()),
        None,
        None,
        current_time_millis(),
    );
    let outcome = crate::response::serve_hls_segment_cache_outcome(
        Arc::clone(&case.request.segment_cache),
        Arc::clone(&case.request.session),
        segment,
        None,
        &response_context,
    )
    .await;
    let crate::response::HlsResourceServeOutcome::Ready(response) = outcome else {
        panic!("existing URI must serve READY media")
    };
    assert_eq!(response.status(), HttpStatus::OK);
    assert_eq!(axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap().as_ref(), raw_media(438));
    assert_eq!(case.origin.media_requests().len(), before);
}

fn b_gets(origin: &Origin, sequence: u64) -> usize {
    origin
        .media_requests()
        .iter()
        .filter(|get| get.starts_with("localhost:") && get.contains(&format!("/{sequence}.ts?")))
        .count()
}

#[tokio::test]
async fn r2_ready_overlap_keeps_both_old_cache_objects_and_uris() {
    let mut case = gap_case(false).await;
    let old_bytes = cache_bytes(&case.request, &case.old_slot.cache_key).await;
    assert!(
        recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate).await
    );
    assert_eq!(b_gets(&case.origin, 438), 0);
    assert_eq!(b_gets(&case.origin, 439), 1);
    assert_eq!(b_gets(&case.origin, 440), 1);
    assert_eq!(demand(&case.request, 54).await, SegmentDemandFetchOutcome::Ready);
    let mut state = case.request.session.write().await;
    assert_eq!(state.origin_epoch, 1);
    state.render_and_store_manifest(current_time_millis()).unwrap();
    let snapshot = rendered_snapshot(&state, current_time_millis());
    for old in [&case.old_slot, &case.anchor_slot] {
        let current = &state.segments[&old.proxy_seq];
        assert_eq!(old.cache_key, current.cache_key);
        assert_eq!(old.origin_fetch_ref, current.origin_fetch_ref);
        assert_eq!(old.status, current.status);
        let uri = |manifest: &HlsLeaseManifestSnapshot| {
            manifest.visible_segments.iter().find(|s| s.proxy_seq == old.proxy_seq).unwrap().uri.clone()
        };
        assert_eq!(uri(&case.manifest), uri(&snapshot));
    }
    drop(state);
    assert_eq!(cache_bytes(&case.request, &case.old_slot.cache_key).await, old_bytes);
    assert_eq!(cache_bytes(&case.request, &case.anchor_slot.cache_key).await, case.anchor_bytes);
    println!("ap7_r2 b438_gets=0 old_slots=51,52 old_bytes_equal=true old_uris_equal=true");
}

async fn assert_open_gap(case: &GapCase) {
    assert_eq!(demand(&case.request, 51).await, SegmentDemandFetchOutcome::Unavailable);
    let state = case.request.session.read().await;
    assert_eq!(state.segments[&51], case.old_slot);
    assert_eq!(state.segments[&52].cache_key, case.anchor_slot.cache_key);
}

#[tokio::test]
async fn r3_transport_failure_keeps_old_lease_in_production_cutover_decision() {
    let mut case = gap_case(true).await;
    case.origin.state.gap_b_438.store(1, Ordering::Release);
    assert!(
        recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate).await
    );
    assert_eq!(b_gets(&case.origin, 438), 1);
    assert_open_gap(&case).await;
    assert_eq!(demand(&case.request, 54).await, SegmentDemandFetchOutcome::Ready);
    assert!(recovery(&mut case, 437, HlsManifestCommitAcceptanceMode::StrictPinnedHost).await);
    assert_eq!(demand(&case.request, 55).await, SegmentDemandFetchOutcome::Ready);
    let (new_snapshot, new_cursor, proxy, log_identity) = {
        let mut state = case.request.session.write().await;
        let render = state.render_and_store_manifest(current_time_millis()).unwrap();
        assert_eq!(render.segment_proxy_seqs, vec![53, 54, 55]);
        assert_eq!(
            state.origin_control.path_condition,
            crate::origin_progress::HlsOriginPathCondition::ProgressExpected
        );
        let new_snapshot = rendered_snapshot(&state, current_time_millis());
        let mut new_cursor = HlsLeasePlaybackCursor::default();
        let token = new_cursor.record_request_started(53, case.measured_at_ms);
        let token = new_cursor.bind_request_timing(token, &state.segments[&53]);
        assert!(new_cursor.record_transfer_started(token, case.measured_at_ms));
        let old_reserve = reserve(&state, &case.manifest, &case.cursor, case.measured_at_ms);
        let new_reserve = reserve(&state, &new_snapshot, &new_cursor, case.measured_at_ms);
        assert_eq!((old_reserve, new_reserve), (0, 24_000));
        println!("ap7_r3 retained_404=true b438_gets=1 old_reserve_ms={old_reserve} fresh_reserve_ms={new_reserve} input_condition=ProgressExpected");
        (
            new_snapshot,
            new_cursor,
            state.proxy_session_id.clone(),
            crate::observability::HlsLogIdentity::from_session(&state),
        )
    };
    let now = current_time_millis();
    let _ = install_lease(&case.request, &new_snapshot, &new_cursor, "ap7-fresh", now.saturating_sub(1)).await;
    assert_eq!(
        case.request.hls_proxy.active_live_playback_snapshots_for_session(&proxy, current_time_millis()).await.len(),
        2
    );
    let mut ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    ctx.hls_proxy = Arc::clone(&case.request.hls_proxy);
    let decision =
        crate::availability::hls_manifest_acceptance_directive_for_session(&ctx, &case.request.session, &proxy).await;
    let crate::availability::HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) = decision else {
        panic!("production recovery entry could not evaluate: {decision:?}");
    };
    assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::Critical);
    let diagnostic = directive.recovery_diagnostic.unwrap();
    let fields =
        crate::observability::hls_manifest_recovery_log_fields(&log_identity, directive.trigger, &diagnostic).unwrap();
    assert!(fields.contains(&format!(
        "controlling_lease={}",
        crate::safe_hls_access_lease_id(&HlsAccessLeaseId("ap7-old".into()))
    )));
    assert!(fields.contains("cutover_required=true"));
    assert!(fields.contains("recovery_required=true"));
    println!("ap7_r3 live_leases=2 {fields}");
    assert_open_gap(&case).await;
    assert!(!case.request.segment_cache.has_active_temp_files());
}

#[tokio::test]
async fn r3_missing_or_wrong_binding_does_not_download_the_gap() {
    for (head, mode, expected_acceptance) in [(439, 0, true), (436, 3, false), (436, 5, true)] {
        let mut case = gap_case(true).await;
        case.origin.state.gap_b_438.store(mode, Ordering::Release);
        assert_eq!(
            recovery(&mut case, head, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate)
                .await,
            expected_acceptance
        );
        assert_eq!(b_gets(&case.origin, 438), 0);
        assert_open_gap(&case).await;
        println!("ap7_r3 binding_case={mode} head={head} b438_gets=0 slot_unavailable=true");
    }
}

#[tokio::test]
async fn r3_incomplete_or_invalid_ts_cannot_publish_ready() {
    for mode in [2, 4] {
        let mut case = gap_case(true).await;
        case.origin.state.gap_b_438.store(mode, Ordering::Release);
        assert!(
            recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate)
                .await
        );
        let gets = b_gets(&case.origin, 438);
        assert!((1..=5).contains(&gets));
        assert_open_gap(&case).await;
        assert!(!case.request.segment_cache.has_active_temp_files());
        assert_eq!(prepared_files(&case).await, 0);
        println!("ap7_r3 invalid_media={mode} b438_gets={gets} slot_unavailable=true staged_files=0");
    }
}

#[tokio::test]
async fn r3_failed_anchor_does_not_download_the_gap() {
    let mut case = gap_case(true).await;
    case.origin.state.gap_b_bad_anchor.store(true, Ordering::Release);
    assert!(
        !recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate).await
    );
    assert_eq!(b_gets(&case.origin, 439), 1);
    assert_eq!(b_gets(&case.origin, 438), 0);
    assert_eq!(b_gets(&case.origin, 440), 0);
    assert_open_gap(&case).await;
    println!("ap7_r3 anchor_mismatch=true b439_gets=1 b438_gets=0 b440_gets=0");
}

#[tokio::test]
async fn r3_elapsed_acceptance_budget_does_not_start_a_repair() {
    let case = gap_case(true).await;
    let candidate = case.origin.fetched(false, 436);
    // The shared fixture sets an already elapsed episode deadline.
    mark_full_burst_ready_for_switch_staging(&case.request.session, &candidate).await;
    let result = crate::refresh::commit_manifest_recovery_candidate(
        &case.request,
        candidate,
        HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(b_gets(&case.origin, 439), 1);
    assert_eq!(b_gets(&case.origin, 438), 0);
    assert_open_gap(&case).await;
    println!("ap7_r3 budget_elapsed=true b439_gets=1 b438_gets=0 slot_unavailable=true");
}

async fn prepared_files(case: &GapCase) -> usize {
    let old_path = case.request.segment_cache.object_path(&case.old_slot.cache_key);
    let mut dir = tokio::fs::read_dir(old_path.parent().unwrap()).await.unwrap();
    let mut prepared = 0;
    while let Some(entry) = dir.next_entry().await.unwrap() {
        if entry.file_name().to_string_lossy().starts_with("000051.repair-") {
            prepared += 1;
        }
    }
    prepared
}

#[tokio::test]
async fn r4_stale_generation_and_parallel_ready_preserve_existing_files() {
    for parallel_ready in [false, true] {
        let case = gap_case(true).await;
        let candidate = case.origin.fetched(false, 436);
        mark_full_burst_ready_for_switch_staging_at(&case.request.session, &candidate, current_time_millis()).await;
        case.origin.state.pause_sequence.store(440, Ordering::Release);
        let request = case.request.clone();
        let job = tokio::spawn(async move {
            crate::refresh::commit_manifest_recovery_candidate(
                &request,
                candidate,
                HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), case.origin.state.entered.notified()).await.unwrap();
        assert_eq!(prepared_files(&case).await, 1);
        assert_open_gap(&case).await;
        let new_bytes = raw_media(777);
        if parallel_ready {
            let metadata = case
                .request
                .segment_cache
                .write_temp_and_commit(&case.old_slot.cache_key, std::io::Cursor::new(new_bytes.clone()))
                .await
                .unwrap();
            let mut state = case.request.session.write().await;
            let entry = state.segments.get_mut(&51).unwrap();
            entry.status =
                SegmentCacheStatus::Ready { content_length: metadata.size, ready_at_ms: current_time_millis() };
            let entry = entry.clone();
            let generation = state.activity.origin_work_generation;
            state.origin_content.record(&entry, generation, sha2::Sha256::digest(&new_bytes).into());
            state.advance_media_readiness_generation();
        } else {
            case.request.session.write().await.activity.origin_work_generation += 1;
        }
        case.origin.state.release.add_permits(1);
        let result = job.await.unwrap();
        assert_eq!(result.is_ok(), parallel_ready, "handoff result: {:?}", result.as_ref().err());
        if parallel_ready {
            assert_eq!(cache_bytes(&case.request, &case.old_slot.cache_key).await, new_bytes);
            let state = case.request.session.read().await;
            assert_eq!(state.segments[&51].cache_key, case.old_slot.cache_key);
            assert!(matches!(state.segments[&51].status, SegmentCacheStatus::Ready { .. }));
        } else {
            assert_open_gap(&case).await;
            assert_eq!(case.request.session.read().await.origin_epoch, 0);
        }
        case.request.hls_proxy.garbage_collector().run_once(current_time_millis()).await.unwrap();
        assert_eq!(prepared_files(&case).await, 0);
        assert!(!case.request.segment_cache.has_active_temp_files());
        assert_eq!(cache_bytes(&case.request, &case.anchor_slot.cache_key).await, case.anchor_bytes);
        println!("ap7_r4 parallel_ready={parallel_ready} staged_before=1 stale_repair_published=false staged_after=0 anchor_bytes_equal=true");
    }
}

#[tokio::test]
async fn reviewer_lease_store_contention_does_not_drop_valid_gap_repair() {
    let case = gap_case(true).await;
    let candidate = case.origin.fetched(false, 436);
    mark_full_burst_ready_for_switch_staging_at(&case.request.session, &candidate, current_time_millis()).await;
    case.origin.state.pause_sequence.store(440, Ordering::Release);
    let request = case.request.clone();
    let job = tokio::spawn(async move {
        crate::refresh::commit_manifest_recovery_candidate(
            &request,
            candidate,
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), case.origin.state.entered.notified()).await.unwrap();
    assert_eq!(prepared_files(&case).await, 1);
    assert_open_gap(&case).await;
    let leases = case.request.hls_proxy.access_leases();
    let read_guard = leases.read().await;
    case.origin.state.release.add_permits(1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), job).await.unwrap().unwrap();
    drop(read_guard);
    assert!(result.is_ok(), "handoff result: {:?}", result.as_ref().err());
    let state = case.request.session.read().await;
    let gets = case.origin.media_requests();
    println!(
        "reviewer_lease_store_contention handoff=Ok epoch={} slot51={:?} segment_gets={gets:?}",
        state.origin_epoch, state.segments[&51].status
    );
    assert_eq!(state.origin_epoch, 1);
    assert_eq!(gets.iter().filter(|get| get.contains("/438.ts?")).count(), 1);
    assert!(
        matches!(state.segments[&51].status, SegmentCacheStatus::Ready { .. }),
        "valid staged repair was discarded on transient lease-lock contention"
    );
}

async fn ordinary_cutover(
    case: &GapCase,
    ctx: &crate::HlsCtx,
    lease: &HlsAccessLease,
    now_ms: u64,
) -> crate::availability::HlsTerminalResolution {
    crate::availability::commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
        ctx,
        &case.request.session,
        &lease.proxy_session_id,
        lease,
        now_ms,
        crate::availability::HlsTerminalDecisionPurpose::OrdinaryCutover,
    )
    .await
    .resolution
}

#[tokio::test]
async fn reviewer_retained_gap_does_not_degrade_unaffected_lease() {
    use crate::availability::HlsTerminalResolution;
    let mut case = gap_case(true).await;
    case.origin.state.gap_b_438.store(1, Ordering::Release);
    assert!(
        recovery(&mut case, 436, HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate).await
    );
    assert_eq!(demand(&case.request, 54).await, SegmentDemandFetchOutcome::Ready);
    assert!(recovery(&mut case, 437, HlsManifestCommitAcceptanceMode::StrictPinnedHost).await);
    assert_eq!(demand(&case.request, 55).await, SegmentDemandFetchOutcome::Ready);
    let now = current_time_millis();
    let (new_snapshot, new_cursor, proxy) = {
        let mut state = case.request.session.write().await;
        let render = state.render_and_store_manifest(now).unwrap();
        assert_eq!(render.segment_proxy_seqs, vec![53, 54, 55]);
        assert_eq!(
            state.origin_control.path_condition,
            crate::origin_progress::HlsOriginPathCondition::ProgressExpected
        );
        let snapshot = rendered_snapshot(&state, now);
        let mut cursor = HlsLeasePlaybackCursor::default();
        let token = cursor.record_request_started(55, now - 8_000);
        let token = cursor.bind_request_timing(token, &state.segments[&55]);
        assert!(cursor.record_transfer_started(token, now - 8_000));
        let _ = cursor.record_request_completed(token, now - 7_900);
        assert_eq!(cursor.highest_contiguous_completed_proxy_seq, Some(55));
        assert_eq!(reserve(&state, &snapshot, &cursor, now), 0);
        (snapshot, cursor, state.proxy_session_id.clone())
    };
    let id = install_lease(&case.request, &new_snapshot, &new_cursor, "ap7-unaffected", now - 1).await;
    let lease = case.request.hls_proxy.access_lease(&id, &proxy, now).await.unwrap();
    assert!(case.request.session.read().await.first_playback_blocker(&lease).is_none());
    let mut ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    ctx.hls_proxy = Arc::clone(&case.request.hls_proxy);
    let before = ordinary_cutover(&case, &ctx, &lease, now).await;
    assert_eq!(before, HlsTerminalResolution::LiveAllowed);
    let decision =
        crate::availability::hls_manifest_acceptance_directive_for_session(&ctx, &case.request.session, &proxy).await;
    let crate::availability::HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) = decision else {
        panic!("production recovery entry could not evaluate: {decision:?}");
    };
    assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::Critical);
    let condition = case.request.session.read().await.origin_control.path_condition;
    let after = ordinary_cutover(&case, &ctx, &lease, now).await;
    println!("reviewer_gap_scope new_cursor=55 own_blocker=none own_reserve_ms=0 before={before:?} after={after:?} shared_path_condition={condition:?}");
    assert_eq!(
        after,
        HlsTerminalResolution::LiveAllowed,
        "a retained gap before this lease's cursor must not change its terminal result"
    );
    assert_eq!(condition, crate::origin_progress::HlsOriginPathCondition::ProgressExpected);

    let old_id = HlsAccessLeaseId("ap7-old".into());
    let old_lease = case.request.hls_proxy.access_lease(&old_id, &proxy, now).await.unwrap();
    assert_eq!(case.request.session.read().await.first_playback_blocker(&old_lease), Some(&case.old_slot));
    let old_cutover = ordinary_cutover(&case, &ctx, &old_lease, now).await;
    let after_old_cutover = ordinary_cutover(&case, &ctx, &lease, now).await;
    let state = case.request.session.read().await;
    println!(
        "reviewer_gap_cutover affected={old_cutover:?} unaffected={after_old_cutover:?} revalidate_entry={}",
        state.live_entry_revalidation.required
    );
    assert_eq!(old_cutover, HlsTerminalResolution::Committed);
    assert_eq!(after_old_cutover, HlsTerminalResolution::LiveAllowed);
    assert!(!state.live_entry_revalidation.required);
}
