use super::*;

pub(super) fn pressure_manifest(target_duration_ms: u64) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1,
        first_proxy_seq: 0,
        last_proxy_seq: 0,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 0,
            duration_ms: 4_000,
            uri: "0.ts".into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms,
        playlist_duration_ms: 4_000,
        last_visible_media_end_ms: 4_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(super) fn pressure_manifest_at(proxy_seq: u64, target_duration_ms: u64) -> HlsLeaseManifestSnapshot {
    let mut manifest = pressure_manifest(target_duration_ms);
    manifest.first_proxy_seq = proxy_seq;
    manifest.last_proxy_seq = proxy_seq;
    Arc::make_mut(&mut manifest.visible_segments)[0].proxy_seq = proxy_seq;
    manifest
}

pub(super) fn pressure_timeline(hidden_durations_ms: &[u64]) -> HlsReadyTimelineSnapshot {
    let mut start_ms = 0_u64;
    let mut units = Vec::with_capacity(hidden_durations_ms.len().saturating_add(1));
    for (index, duration_ms) in std::iter::once(4_000_u64).chain(hidden_durations_ms.iter().copied()).enumerate() {
        units.push(HlsReadyTimelineUnit {
            proxy_seq: u64::try_from(index).unwrap_or(u64::MAX),
            start_ms,
            duration_ms,
            state: HlsReadyMediaState::Ready,
            required_map_ready: true,
            required_key_ready: true,
            key_ready_valid_until_ms: None,
        });
        start_ms = start_ms.saturating_add(duration_ms);
    }
    HlsReadyTimelineSnapshot { units: units.into() }
}

pub(super) fn evaluated_pressure(
    lease_id: &str,
    target_duration_ms: u64,
    hidden_durations_ms: &[u64],
    recovery_budget_ms: u64,
) -> HlsLeaseRecoveryEvidence {
    let manifest = pressure_manifest(target_duration_ms);
    let ready_timeline = pressure_timeline(hidden_durations_ms);
    let recovery_trigger_budget = HlsRecoveryTriggerBudgetMs::from_millis(recovery_budget_ms);
    let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest: &manifest,
        cursor: &HlsLeasePlaybackCursor::default(),
        ready_timeline: &ready_timeline,
        now_ms: 100,
        playback_rate_guard_milli: HLS_PLAYBACK_RATE_GUARD_MILLI,
        recovery_trigger_budget,
        origin_path_degraded: true,
        recovery_committed: false,
    });
    let boundary_ms = recovery_trigger_budget.as_millis().saturating_add(reserve.transition_margin.as_millis());
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(100, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    HlsLeaseRecoveryEvidence {
        lease_id: HlsAccessLeaseId(lease_id.to_string()),
        reserve,
        cursor: HlsLeasePlaybackCursor::default(),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        target_duration_ms,
        latest_safe_terminal_commit_at: cutover_timing.latest_safe_terminal_commit_at,
        recovery_boundary_slack_ms: HlsRecoveryBoundarySlackMs::from_reserve_and_boundary(
            reserve.guaranteed_reserve_ms,
            boundary_ms,
        ),
    }
}

pub(super) fn atomic_pressure_policy() -> HlsRecoveryPressurePolicy {
    HlsRecoveryPressurePolicy {
        burst_plan: shared::model::HlsManifestRecoveryBurstPlan { slots: 1, lanes_per_slot: 1 },
        timing: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsRecoveryEtaMs::from_millis(0),
            HlsRecoveryEtaMs::from_millis(0),
        ),
    }
}

pub(super) fn atomic_pressure_session() -> HlsSession {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "pressure"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:8\n\
        #EXTINF:4.0,\n0.ts\n#EXTINF:8.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n";
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(body, "http://origin.example/live/index.m3u8")
    else {
        panic!("pressure manifest parses");
    };
    session.apply_origin_manifest(&manifest).expect("pressure timeline applies");
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
    }
    session.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    session.origin_control.last_media_progress_at_ms = Some(90);
    session.origin_control.target_duration_snapshot_ms = Some(8_000);
    session
}

pub(super) fn install_atomic_pressure_lease(
    store: &mut HlsAccessLeaseStore,
    proxy_session_id: &ProxySessionId,
    lease_id: &str,
    manifest: HlsLeaseManifestSnapshot,
    valid_window_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    store.prepare_access_lease(HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new(lease_id.0.clone(), lease_id.0.clone()),
        proxy_session_id.clone(),
        lease_id.0.clone(),
        "token".to_string(),
        1,
        "stream".to_string(),
        1,
        0,
        valid_window_ms,
    ));
    let guard = store.prepare_manifest_publication(&lease_id, proxy_session_id, 1).expect("manifest publication guard");
    assert!(store.commit_manifest_publication(&lease_id, proxy_session_id, guard, manifest, 1).is_committed());
    assert!(store
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            2,
            HlsAccessLeaseTiming { active_window_ms: valid_window_ms, valid_window_ms },
        )
        .is_activated());
    lease_id
}
