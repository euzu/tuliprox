use super::{
    commit_terminal_plan, config_with_hls_cache, prepared_terminal_commit_fixture, test_app_config, HlsProxyManager,
    HlsTerminalAssetRevisionGuard, HlsTerminalCommitMediaGuard, HlsTerminalCommitOutcome, HlsTerminalCommitPayload,
    HlsTerminalCommitRequest,
};
use crate::{terminal_tail::HlsLeasePlaybackMode, HlsSessionKey, HlsTerminalTailCompatibility};
use shared::model::HlsCacheConfigDto;
use std::sync::Arc;
use tuliprox_core::model::HlsCacheConfig;

#[tokio::test]
async fn startup_reload_keeps_existing_session_policy_and_global_budget_ownership() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut initial_dto =
        HlsCacheConfigDto { cache_path: Some(directory.path().to_string_lossy().into_owned()), ..Default::default() };
    initial_dto.startup.mode = shared::model::HlsStartupMode::Progressive;
    let manager = HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&initial_dto));
    let (original, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "original"), b"secret", 100).await;
    let budget = Arc::clone(&manager.progressive_budget);
    let held = budget.try_claim().ok_or_else(|| std::io::Error::other("progressive slot"))?;
    let mut updated = initial_dto;
    updated.startup.mode = shared::model::HlsStartupMode::FirstReady;
    updated.startup.max_progressive_segments = 1;
    manager.update_config(&test_app_config(config_with_hls_cache(updated))).await;
    assert!(Arc::ptr_eq(&budget, &manager.progressive_budget));
    assert!(budget.try_claim().is_none());
    let (existing, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "original"), b"secret", 200).await;
    assert!(Arc::ptr_eq(&existing, &original));
    assert_eq!(existing.read().await.startup_mode(), shared::model::HlsStartupMode::Progressive);
    let (new_session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "new"), b"secret", 200).await;
    assert_eq!(new_session.read().await.startup_mode(), shared::model::HlsStartupMode::FirstReady);
    drop(held);
    assert_eq!(budget.usage(), (0, 0));
    Ok(())
}

#[tokio::test]
async fn hls_terminal_commit_changed_asset_revision_fails_closed_before_tail_mutation() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("asset-revision").await;
    let expected_asset = plan.asset_identity;

    let outcome = manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session: &session,
        lease_id: &lease_id,
        proxy_session_id: &proxy_session_id,
        preparation: &preparation,
        now_ms: 2_000,
        payload: HlsTerminalCommitPayload::Tail { plan, media_guard: HlsTerminalCommitMediaGuard::empty_for_test() },
        asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, || None),
    });

    assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("asset mismatch terminalizes the lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingAsset, .. }
    ));
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_exact_replay_stays_idempotent_after_asset_revision_change() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("asset-replay").await;
    let expected_asset = plan.asset_identity;

    assert_eq!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, Arc::clone(&plan),),
        HlsTerminalCommitOutcome::Committed
    );
    let replay = manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session: &session,
        lease_id: &lease_id,
        proxy_session_id: &proxy_session_id,
        preparation: &preparation,
        now_ms: 2_000,
        payload: HlsTerminalCommitPayload::Tail { plan, media_guard: HlsTerminalCommitMediaGuard::empty_for_test() },
        asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, || None),
    });

    assert_eq!(replay, HlsTerminalCommitOutcome::AlreadyCommitted);
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("terminal replay stays available")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
}

#[tokio::test]
async fn idle_session_with_a_bound_startup_revision_expires_without_restart() -> std::io::Result<()> {
    for reader_holds_revision in [false, true] {
        idle_startup_revision_round(reader_holds_revision).await?;
    }
    Ok(())
}

async fn idle_startup_revision_round(reader_holds_revision: bool) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir()?;
    let mut dto =
        HlsCacheConfigDto { cache_path: Some(directory.path().to_string_lossy().into_owned()), ..Default::default() };
    dto.startup.mode = shared::model::HlsStartupMode::Optimistic;
    let manager = HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto));
    let (session, _) = manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "idle"), b"secret", 100).await;
    let manifest = match tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\n0.ts\n",
        "http://origin.example.com/live/index.m3u8",
    ) {
        tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) => manifest,
        other @ tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::TransientPassthrough { .. } => {
            return Err(std::io::Error::other(format!("unexpected manifest: {other:?}")))
        }
    };
    let proxy_session_id = {
        let mut guard = session.write().await;
        guard.apply_origin_manifest(&manifest).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        guard.proxy_session_id.clone()
    };
    let seq = *session.read().await.segments.keys().next().ok_or_else(|| std::io::Error::other("segment"))?;
    // A finished raw startup revision, as left behind by a viewer that switched away.
    let revision = manager.revision_store.create(proxy_session_id.clone(), seq, crate::SegmentRevisionKind::Raw)?;
    let (mut file, pin) = manager.segment_cache.create_revision_spool(&revision.revision().key).await?;
    file.write_all(b"segment-body").await?;
    file.flush().await?;
    drop(file);
    *revision.revision().file_pin.lock().map_err(|_| std::io::Error::other("pin"))? = Some(pin);
    let path = manager.segment_cache.object_path(&revision.revision().key);
    revision.revision().complete(crate::CachedSegmentMetadata { path: path.clone(), size: 12 });
    // A lease binding or an open response body holds its own guard.
    let reader = reader_holds_revision.then(|| revision.clone());
    session
        .write()
        .await
        .startup
        .as_mut()
        .ok_or_else(|| std::io::Error::other("startup policy"))?
        .revisions
        .insert(seq, revision);
    drop(session);

    let idle_now_ms = 100 + 24 * 60 * 60 * 1000;
    manager.run_garbage_collection_once(idle_now_ms).await?;

    let retained = manager.sessions.get_by_proxy_session_id(&proxy_session_id).await.is_some();
    assert_eq!(retained, reader_holds_revision);
    assert_eq!(manager.segment_cache.has_active_temp_files_for_session(&proxy_session_id), reader_holds_revision);
    assert_eq!(tokio::fs::try_exists(&path).await?, reader_holds_revision);
    drop(reader);
    Ok(())
}
