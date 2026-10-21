use super::*;
use crate::{
    late_custom_response::prepare_late_custom_response,
    manager::{HlsTerminalCommitPayload, HlsTerminalCommitRequest},
};

async fn late_fixture(name: &str) -> HlsTerminalPendingCommitFixture {
    let fixture = terminal_pending_commit_fixture(name).await;
    fixture.session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
    fixture
}

async fn prepare(
    fixture: &HlsTerminalPendingCommitFixture,
) -> (Arc<crate::late_custom_response::HlsLateCustomResponsePlan>, HlsRuntimeCustomTailAssetIdentity) {
    prepare_late_custom_response(
        &fixture.ctx,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        &fixture.preparation,
        fixture.now_ms,
    )
    .await
    .unwrap_or_else(|error| match error {
        crate::late_custom_response::HlsLateCustomResponseError::Superseded => panic!("late plan superseded"),
        crate::late_custom_response::HlsLateCustomResponseError::Unavailable(reason) => {
            panic!("late plan unavailable: {reason:?}")
        }
    })
}

fn commit(
    fixture: &HlsTerminalPendingCommitFixture,
    plan: Arc<crate::late_custom_response::HlsLateCustomResponsePlan>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    now_ms: u64,
) -> HlsTerminalCommitOutcome {
    fixture.ctx.hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session: &fixture.session,
        lease_id: &fixture.lease_id,
        proxy_session_id: &fixture.proxy_session_id,
        preparation: &fixture.preparation,
        now_ms,
        payload: HlsTerminalCommitPayload::CustomResponse(plan),
        asset_revision_guard: terminal_asset_revision_guard(&fixture.ctx, expected_asset.reason, Some(expected_asset)),
    })
}

#[tokio::test]
async fn legacy_csv_only_plan_is_rejected_even_after_splice_deadline() {
    let fixture = late_fixture("legacy-late-plan-rejected").await;
    let (plan, expected_asset) = prepare(&fixture).await;
    assert_eq!(commit(&fixture, plan, expected_asset, fixture.now_ms), HlsTerminalCommitOutcome::BundleIncompatible);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("live lease");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
}

#[tokio::test]
async fn exhausted_lease_uses_last_live_segment_after_old_splice_deadline() {
    let fixture = late_fixture("late-tail-keeps-live-base").await;
    assert!(
        fixture.now_ms >= fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch()
    );
    let previous = fixture.preparation.manifest_snapshot.visible_segments.last().unwrap().clone();
    let context = HlsTerminalCommitContext {
        ctx: &fixture.ctx,
        session: &fixture.session,
        proxy_session_id: &fixture.proxy_session_id,
        lease_id: &fixture.lease_id,
        preparation: &fixture.preparation,
        now_ms: fixture.now_ms,
    };
    let bundle = fixture.ready_bundle();
    let outcome = super::super::terminal_cutover::commit_ready_terminal_bundle(
        context,
        Arc::clone(&fixture.asset),
        fixture.expected_asset,
        fixture.bundle_key,
        bundle,
    )
    .await;
    assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .unwrap();
    let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode else {
        panic!("live tail required");
    };
    assert_eq!(plan.base_manifest.visible_segments.last(), Some(&previous));
    let manifest = terminal_tail_manifest_body(&plan, &fixture.proxy_session_id, &fixture.lease_id).unwrap();
    let live = manifest.find(previous.uri.as_ref()).expect("last live URI");
    let terminal = manifest.find("/terminal/").expect("CSV tail");
    assert!(live < terminal);
    assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
    assert!(!manifest.contains("/cvs/hls/"));
    fixture.ctx.hls_proxy.run_garbage_collection_once(fixture.now_ms + 1).await.unwrap();
    let key = SegmentCacheKey::new(fixture.proxy_session_id.clone(), previous.proxy_seq, "ts");
    assert!(fixture.ctx.hls_proxy.segment_cache().metadata(&key).await.unwrap().is_some());
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Exercise real expiry GC before and after the atomic live-to-terminal protection transfer."
)]
async fn gc_expires_control_bytes_but_keeps_the_last_live_anchor_through_terminal_commit() {
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = terminal_pending_commit_fixture_with_cache("gc-live-terminal-anchor", directory.path()).await;
    let manager = &fixture.ctx.hls_proxy;
    let control = SegmentCacheKey::new(fixture.proxy_session_id.clone(), 39, "ts");
    manager.segment_cache().write_bytes_and_commit(&control, TERMINAL_ASSET_BYTES).await.unwrap();
    {
        let mut state = fixture.session.write().await;
        let mut entry = state.segments[&40].clone();
        entry.proxy_seq = 39;
        entry.cache_key = control.clone();
        entry.origin_key.host_local_sequence = 39;
        state.segments.insert(39, entry);
        state.proxy_next_seq = Some(41);
        let OriginManifestParseOutcome::Normal(origin) = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:12,\n100.ts\n#EXTINF:12,\n101.ts\n#EXTINF:12,\n102.ts\n",
            "http://origin.example/index.m3u8",
        ) else { panic!("normal origin"); };
        state.apply_origin_manifest_for_host(&origin, 1).unwrap();
        state.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
    }
    let gc = crate::HlsGarbageCollector::new(
        Arc::clone(manager.sessions()),
        Arc::clone(manager.segment_cache()),
        crate::GarbageCollectionPolicy {
            cache_duration_ms: 1,
            session_idle_timeout_ms: 1_000_000,
            ..crate::GarbageCollectionPolicy::default()
        },
        String::new(),
    );
    gc.install_access_leases(manager.access_leases());
    let report = gc.run_once(fixture.now_ms + 30_000).await.unwrap();
    assert!(
        manager.segment_cache().metadata(&control).await.unwrap().is_none(),
        "control must actually be reclaimed: {report:?}"
    );
    assert!(manager.segment_cache().metadata(&control).await.unwrap().is_none());
    let live_key = SegmentCacheKey::new(fixture.proxy_session_id.clone(), 40, "ts");
    assert!(manager.segment_cache().metadata(&live_key).await.unwrap().is_some());
    let state = fixture.session.read().await;
    let reserve = fixture.preparation.reserve;
    let preparation = manager.prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
        lease_id: &fixture.lease_id,
        proxy_session_id: &fixture.proxy_session_id,
        manifest_snapshot_generation: fixture.preparation.manifest_snapshot_generation,
        cursor_generation: fixture.preparation.cursor_generation,
        reserve,
        cutover_timing: HlsLeaseCutoverTiming::from_reserve(fixture.now_ms, 0, reserve.transition_margin, None),
        commit_window: HlsTerminalCommitWindow::CutoverDue,
        now_ms: fixture.now_ms,
        origin_progress_generation: state.origin_control.progress_generation,
        media_readiness_generation: state.activity.media_readiness_generation,
        last_media_progress_at_ms: state.origin_control.last_media_progress_at_ms,
    });
    drop(state);
    fixture.preparation = preparation.await.unwrap();
    let outcome = super::super::terminal_cutover::commit_ready_terminal_bundle(
        HlsTerminalCommitContext {
            ctx: &fixture.ctx,
            session: &fixture.session,
            proxy_session_id: &fixture.proxy_session_id,
            lease_id: &fixture.lease_id,
            preparation: &fixture.preparation,
            now_ms: fixture.now_ms,
        },
        Arc::clone(&fixture.asset),
        fixture.expected_asset,
        fixture.bundle_key,
        fixture.ready_bundle(),
    )
    .await;
    assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
    assert!(!fixture.session.read().await.live_window_demands.contains_key(&fixture.lease_id));
    gc.run_once(fixture.now_ms + 31_000).await.unwrap();
    let metadata = manager.segment_cache().metadata(&live_key).await.unwrap().unwrap();
    assert_eq!(tokio::fs::read(metadata.path).await.unwrap(), TERMINAL_ASSET_BYTES);
}
