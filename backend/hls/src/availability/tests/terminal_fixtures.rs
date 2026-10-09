use super::*;

impl HlsTerminalPendingCommitFixture {
    pub(super) fn owner_key(&self) -> HlsTerminalPendingOwnerKey {
        HlsTerminalPendingOwnerKey {
            session_incarnation: self
                .ctx
                .hls_proxy
                .sessions()
                .session_incarnation(&self.session)
                .expect("fixture session has a current incarnation"),
            proxy_session_id: self.proxy_session_id.clone(),
            lease_id: self.lease_id.clone(),
            lease_issued_at_ms: self.preparation.lease_issued_at_ms,
            expected_admission_generation: self.preparation.expected_admission_generation,
            manifest_snapshot_generation: self.preparation.manifest_snapshot_generation,
            cursor_generation: self.preparation.cursor_generation,
            decision_generation: self.preparation.decision_generation,
            reason: self.expected_asset.reason,
            bundle_key: self.bundle_key,
            latest_safe_commit_at_ms: self
                .preparation
                .cutover_timing
                .latest_safe_terminal_commit_at
                .as_millis_since_epoch(),
        }
    }

    pub(super) fn ready_bundle(&self) -> Arc<HlsPreparedTerminalBundle> {
        build_prepared_terminal_bundle(&self.asset, self.bundle_key).expect("fixture relative terminal bundle")
    }

    pub(super) fn register_owner(&self, ticket: HlsPreparedTerminalBundleCompletionTicket) -> oneshot::Receiver<()> {
        let coordinator = self.ctx.hls_proxy.terminal_pending();
        let owner_key = self.owner_key();
        let ctx = self.ctx.clone();
        let session = Arc::clone(&self.session);
        let proxy_session_id = self.proxy_session_id.clone();
        let lease_id = self.lease_id.clone();
        let preparation = self.preparation.clone();
        let asset = Arc::clone(&self.asset);
        let expected_asset = self.expected_asset;
        let bundle_key = self.bundle_key;
        let (completed_tx, completed_rx) = oneshot::channel();
        let asset_guard = terminal_asset_revision_guard(&self.ctx, expected_asset.reason, Some(expected_asset));

        assert_eq!(
            coordinator.register(owner_key, &asset_guard, move |ownership| async move {
                run_terminal_pending_owner(
                    ctx,
                    session,
                    proxy_session_id,
                    lease_id,
                    preparation,
                    asset,
                    expected_asset,
                    bundle_key,
                    ticket,
                    ownership,
                )
                .await;
                assert!(completed_tx.send(()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        completed_rx
    }
}

pub(super) fn terminal_pending_commit_reserve() -> HlsLeaseReserveSnapshot {
    let transition_margin = HlsTransitionMarginMs::from_millis(12_000);
    let guaranteed_reserve_ms = transition_margin
        .as_millis()
        .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: guaranteed_reserve_ms,
        conservative_playback_position_ms: 0,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms: 0,
        transition_margin,
        key_readiness_valid_until_ms: None,
        recovery_required: true,
        cutover_required: false,
    }
}

pub(super) fn terminal_pending_commit_manifest(
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    duration_ms: u64,
) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: 0,
        first_proxy_seq: 40,
        last_proxy_seq: 40,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 40,
            duration_ms,
            uri: format!("/hls/shared/live/{}/{}/40.ts", proxy_session_id.0, lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 12_000,
        playlist_duration_ms: duration_ms,
        last_visible_media_end_ms: duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(super) async fn terminal_pending_commit_fixture(name: &str) -> HlsTerminalPendingCommitFixture {
    terminal_pending_commit_fixture_with_base(name, TERMINAL_ASSET_BYTES).await
}

pub(super) async fn install_terminal_pending_ready_base(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    asset: &super::super::super::terminal_tail::HlsTerminalMediaAsset,
    base_segment_bytes: &[u8],
    now_ms: u64,
) {
    let cache_key = SegmentCacheKey::new(proxy_session_id.clone(), 40, "ts");
    ctx.hls_proxy
        .segment_cache()
        .write_bytes_and_commit(&cache_key, base_segment_bytes)
        .await
        .expect("READY terminal-base bytes commit");
    let mut session = session.write().await;
    let origin_epoch = session.origin_control.origin_epoch;
    session.segments.insert(
        40,
        SegmentEntry {
            origin_key: OriginSegmentKey {
                origin_epoch,
                effective_host_id: 1,
                host_local_sequence: 40,
                host_local_index: 0,
            },
            proxy_seq: 40,
            duration_ms: asset.duration_ms(),
            proxy_file_ext: "ts".to_string(),
            content_type: "video/mp2t".to_string(),
            cache_key,
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            encryption: None,
            origin_fetch_ref: None,
            status: SegmentCacheStatus::Ready {
                content_length: u64::try_from(base_segment_bytes.len()).unwrap_or(u64::MAX),
                ready_at_ms: now_ms,
            },
            last_rendered_at_ms: Some(now_ms),
            access: Arc::new(CacheAccessState::new()),
        },
    );
}

pub(super) async fn publish_terminal_pending_lease(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    name: &str,
    duration_ms: u64,
    now_ms: u64,
) {
    ctx.hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("pending-owner", name),
            proxy_session_id.clone(),
            "pending-owner".to_string(),
            name.to_string(),
            1,
            name.to_string(),
            1,
            now_ms,
            60_000,
        ))
        .await;
    let publication = ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms)
        .await
        .expect("manifest publication guard");
    assert!(ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            lease_id,
            proxy_session_id,
            publication,
            terminal_pending_commit_manifest(proxy_session_id, lease_id, duration_ms),
            now_ms,
        )
        .await
        .is_committed());
}

pub(super) async fn terminal_pending_commit_fixture_with_base(
    name: &str,
    base_segment_bytes: &[u8],
) -> HlsTerminalPendingCommitFixture {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: true, ..Config::default() });
    let ctx = &hls_ctx;
    let terminal_buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let asset = snapshot_terminal_media_asset(&terminal_buffer).expect("valid terminal asset fixture");
    ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let (session, _) =
        ctx.hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, name), b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId(name.to_string());
    install_terminal_pending_ready_base(ctx, &session, &proxy_session_id, &asset, base_segment_bytes, now_ms).await;
    publish_terminal_pending_lease(ctx, &proxy_session_id, &lease_id, name, asset.duration_ms(), now_ms).await;
    let (origin_progress_generation, media_readiness_generation, last_media_progress_at_ms) = {
        let session = session.read().await;
        (
            session.origin_control.progress_generation,
            session.activity.media_readiness_generation,
            session.origin_control.last_media_progress_at_ms,
        )
    };
    let reserve = terminal_pending_commit_reserve();
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    let preparation = ctx
        .hls_proxy
        .prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            manifest_snapshot_generation: 1,
            cursor_generation: 0,
            reserve,
            cutover_timing,
            commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
            now_ms,
            origin_progress_generation,
            media_readiness_generation,
            last_media_progress_at_ms,
        })
        .await
        .expect("cutover-local terminal preparation");
    let expected_asset =
        HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(&asset));
    let bundle_key = prepared_terminal_bundle_key(
        &asset,
        preparation.manifest_snapshot.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );

    HlsTerminalPendingCommitFixture {
        ctx: ctx.clone(),
        session,
        proxy_session_id,
        lease_id,
        preparation,
        asset,
        expected_asset,
        bundle_key,
        now_ms,
    }
}

pub(super) fn terminal_base_without_timestamps() -> Vec<u8> {
    let mut bytes = TERMINAL_ASSET_BYTES.to_vec();
    for packet in bytes.as_chunks_mut::<188>().0 {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        if matches!(adaptation_field_control, 0b10 | 0b11) && packet[4] > 0 {
            packet[5] &= !0x10;
        }
        if packet[1] & 0x40 == 0 {
            continue;
        }
        let payload_offset = match adaptation_field_control {
            0b01 => 4,
            0b11 => 5usize.saturating_add(usize::from(packet[4])),
            _ => continue,
        };
        let Some(payload) = packet.get_mut(payload_offset..) else {
            continue;
        };
        if payload.len() >= 9 && payload.starts_with(&[0x00, 0x00, 0x01]) {
            payload[7] &= 0x3F;
        }
    }
    bytes
}

pub(super) async fn assert_terminal_pending_registration_failure_commits_unavailable(
    failure: HlsTerminalPendingRegistration,
    name: &str,
) {
    let fixture = terminal_pending_commit_fixture(name).await;
    let resolution = terminal_resolution_for_pending_registration(
        HlsTerminalCommitContext {
            ctx: &fixture.ctx,
            session: &fixture.session,
            proxy_session_id: &fixture.proxy_session_id,
            lease_id: &fixture.lease_id,
            preparation: &fixture.preparation,
            now_ms: fixture.now_ms,
        },
        fixture.expected_asset,
        fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
        failure,
    );

    assert_eq!(resolution, HlsTerminalResolution::Committed);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::TerminalMediaNotReady, .. }
    ));
}

pub(super) async fn assert_post_refresh_registration_failure_leaves_no_unowned_live_lease(
    failure: HlsAvailabilityReevaluationRegistration,
    name: &str,
) {
    let fixture = terminal_pending_commit_fixture(name).await;
    let (origin_progress_generation, media_readiness_generation) = {
        let mut session = fixture.session.write().await;
        session.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
        let origin_epoch = session.origin_control.origin_epoch;
        for proxy_seq in 41_u64..=44 {
            session.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch,
                        effective_host_id: 1,
                        host_local_sequence: proxy_seq,
                        host_local_index: u32::try_from(proxy_seq.saturating_sub(40)).unwrap_or(u32::MAX),
                    },
                    proxy_seq,
                    duration_ms: 20_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key: SegmentCacheKey::new(fixture.proxy_session_id.clone(), proxy_seq, "ts"),
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: fixture.now_ms },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
        }
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert!(fixture
        .ctx
        .hls_proxy
        .activate_access_lease(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            fixture.now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    let outcome = commit_post_refresh_terminal_fallback(
        fixture.ctx.clone(),
        Arc::clone(&fixture.session),
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: super::super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
            origin_progress_generation,
            media_readiness_generation,
        },
        failure,
    )
    .await;

    assert_eq!(outcome, HlsPostRefreshFallbackOutcome::TerminalCommitted);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("fallback lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::TerminalMediaNotReady, .. }
    ));
}
