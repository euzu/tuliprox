use super::*;

#[test]
fn persisted_cluster_snapshot_preserves_quality_facts_without_merging_technical_state() {
    let accepted = PlaylistUpdateClusterTelemetry {
        cluster: XtreamCluster::Live,
        requested: true,
        source: Some(PlaylistUpdateDataSource::Provider),
        baseline_count: Some(1_000),
        candidate_count: Some(950),
        active_count: Some(950),
        threshold: Some(90),
        quality: Some(95),
        decision: Some(PlaylistUpdateClusterDecision::Accepted),
        technical_state: None,
    };

    let completed = persisted_cluster_snapshot(
        &accepted,
        completed_snapshot_context(InputRefreshPolicy::NORMAL, Some(InputRefreshPolicy::REFRESH), 1),
    );
    assert_eq!(completed.policy, Some(InputRefreshPolicy::REFRESH));
    assert_eq!(completed.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(completed.quality_guard_threshold, None, "an evaluation already owns its threshold");
    assert_eq!(
        completed.quality,
        Some(PersistedPlaylistUpdateQualitySnapshot {
            threshold: 90,
            baseline_count: Some(1_000),
            candidate_count: Some(950),
            achieved_quality: Some(95),
            decision: PersistedPlaylistUpdateQualityDecision::Accepted,
        })
    );
    assert_eq!(completed.active_count, Some(950));
    assert_eq!(completed.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

    let mut bootstrap = accepted.clone();
    bootstrap.baseline_count = None;
    bootstrap.candidate_count = Some(42);
    bootstrap.active_count = Some(42);
    bootstrap.quality = None;
    let bootstrap =
        persisted_cluster_snapshot(&bootstrap, completed_snapshot_context(InputRefreshPolicy::NORMAL, None, 1));
    assert_eq!(bootstrap.policy, None, "an automatic run must not acquire a default policy");
    assert_eq!(bootstrap.quality_guard_threshold, None, "bootstrap evaluation already owns its threshold");
    assert_eq!(
        bootstrap.quality,
        Some(PersistedPlaylistUpdateQualitySnapshot {
            threshold: 90,
            baseline_count: None,
            candidate_count: Some(42),
            achieved_quality: None,
            decision: PersistedPlaylistUpdateQualityDecision::Accepted,
        }),
        "bootstrap acceptance must not invent achieved Quality"
    );

    let mut rejected = accepted.clone();
    rejected.candidate_count = Some(217);
    rejected.active_count = Some(1_000);
    rejected.quality = Some(21);
    rejected.decision = Some(PlaylistUpdateClusterDecision::Rejected);
    let rejected =
        persisted_cluster_snapshot(&rejected, completed_snapshot_context(InputRefreshPolicy::NORMAL, None, 1));
    assert_eq!(
        rejected.quality.map(|quality| quality.decision),
        Some(PersistedPlaylistUpdateQualityDecision::Rejected)
    );
    assert_eq!(rejected.active_count, Some(1_000), "only the typed active baseline is retained");
    assert_eq!(rejected.quality_guard_threshold, None, "a rejection evaluation already owns its threshold");
    assert_eq!(rejected.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

    let mut accepted_then_failed = accepted;
    accepted_then_failed.active_count = None;
    let accepted_then_failed = persisted_cluster_snapshot(
        &accepted_then_failed,
        PersistedSnapshotContext {
            effective_policy: InputRefreshPolicy::NORMAL,
            request_policy: None,
            technical_failure: true,
            storage_failure: true,
            provider_failure_evidence: ProviderFailureEvidence::None,
            provider_cluster_count: 1,
        },
    );
    assert_eq!(
        accepted_then_failed.quality.map(|quality| quality.decision),
        Some(PersistedPlaylistUpdateQualityDecision::Accepted)
    );
    assert_eq!(accepted_then_failed.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
    assert_eq!(accepted_then_failed.active_count, None);
}

#[test]
fn persisted_cluster_snapshot_keeps_policy_source_and_disabled_guard_independent() {
    let forced = PlaylistUpdateClusterTelemetry {
        cluster: XtreamCluster::Video,
        requested: true,
        source: Some(PlaylistUpdateDataSource::Provider),
        baseline_count: None,
        candidate_count: Some(73),
        active_count: Some(73),
        threshold: Some(95),
        quality: None,
        decision: Some(PlaylistUpdateClusterDecision::Accepted),
        technical_state: None,
    };

    let forced = persisted_cluster_snapshot(
        &forced,
        completed_snapshot_context(InputRefreshPolicy::FORCE, Some(InputRefreshPolicy::FORCE), 1),
    );
    assert_eq!(forced.policy, Some(InputRefreshPolicy::FORCE));
    assert_eq!(forced.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(forced.quality_guard_threshold, Some(95));
    assert_eq!(forced.quality, None, "a Force bypass is not a Quality evaluation");

    let disabled = PlaylistUpdateClusterTelemetry {
        cluster: XtreamCluster::Series,
        requested: true,
        source: Some(PlaylistUpdateDataSource::Provider),
        baseline_count: None,
        candidate_count: Some(12),
        active_count: Some(12),
        threshold: None,
        quality: None,
        decision: Some(PlaylistUpdateClusterDecision::Accepted),
        technical_state: None,
    };
    let disabled = persisted_cluster_snapshot(
        &disabled,
        completed_snapshot_context(InputRefreshPolicy::FORCE, Some(InputRefreshPolicy::FORCE), 1),
    );
    assert_eq!(disabled.quality_guard_threshold, Some(0));
    assert_eq!(disabled.quality, None, "threshold zero stays disabled even during Force");

    let cached = PlaylistUpdateClusterTelemetry {
        cluster: XtreamCluster::Live,
        requested: true,
        source: Some(PlaylistUpdateDataSource::Cache),
        baseline_count: None,
        candidate_count: None,
        active_count: None,
        threshold: Some(90),
        quality: None,
        decision: None,
        technical_state: None,
    };
    let cached = persisted_cluster_snapshot(
        &cached,
        completed_snapshot_context(InputRefreshPolicy::REFRESH, Some(InputRefreshPolicy::REFRESH), 0),
    );
    assert_eq!(cached.policy, Some(InputRefreshPolicy::REFRESH));
    assert_eq!(cached.quality_guard_threshold, Some(90));
    assert_eq!(
        cached.source,
        Some(PlaylistUpdateDataSource::Cache),
        "the actual source remains stronger than request policy"
    );
}

#[test]
fn persisted_cluster_snapshot_final_status_boundary_replaces_provider_with_cache_without_history() {
    let temp = tempfile::tempdir().expect("temporary status directory");
    let mut status = input_cache::InputStatus::default();
    status.clusters.insert(
        XtreamCluster::Live.as_ref().to_string(),
        input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: 17, last_update: None },
    );
    let provider_telemetry = PlaylistUpdateInputTelemetry {
        refresh_policy: InputRefreshPolicy::REFRESH,
        source: None,
        clusters: vec![PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Live,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: Some(100),
            candidate_count: Some(95),
            active_count: Some(95),
            threshold: Some(90),
            quality: Some(95),
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        }],
    };
    let mut provider_result = PlaylistDownloadResult::new(Vec::new(), Vec::new(), false, false)
        .with_input_telemetry(provider_telemetry)
        .with_pending_status(PendingInputStatusUpdate {
            storage_path: temp.path().to_path_buf(),
            status,
            status_changed: true,
            cluster_status_source: ClusterStatusSource::PerCluster,
        });

    persist_finalized_cluster_snapshots(&mut provider_result, Some(InputRefreshPolicy::REFRESH), None);

    let provider_status = input_cache::load_input_status(temp.path());
    let provider_snapshot = provider_status.clusters["live"].last_update.expect("provider snapshot");
    assert_eq!(provider_snapshot.source, Some(PlaylistUpdateDataSource::Provider));
    assert!(provider_snapshot.quality.is_some());

    let cache_telemetry = PlaylistUpdateInputTelemetry {
        refresh_policy: InputRefreshPolicy::NORMAL,
        source: None,
        clusters: vec![PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Live,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Cache),
            baseline_count: None,
            candidate_count: None,
            active_count: None,
            threshold: Some(90),
            quality: None,
            decision: None,
            technical_state: None,
        }],
    };
    let mut cache_result = PlaylistDownloadResult::new(Vec::new(), Vec::new(), true, false)
        .with_input_telemetry(cache_telemetry)
        .with_pending_status(PendingInputStatusUpdate {
            storage_path: temp.path().to_path_buf(),
            status: provider_status,
            status_changed: false,
            cluster_status_source: ClusterStatusSource::PerCluster,
        });

    persist_finalized_cluster_snapshots(&mut cache_result, Some(InputRefreshPolicy::NORMAL), None);

    let cached_status = input_cache::load_input_status(temp.path());
    assert_eq!(cached_status.clusters.len(), 1);
    let cached_snapshot = cached_status.clusters["live"].last_update.expect("replacement cache snapshot");
    assert_eq!(cached_snapshot.policy, Some(InputRefreshPolicy::NORMAL));
    assert_eq!(cached_snapshot.source, Some(PlaylistUpdateDataSource::Cache));
    assert_eq!(cached_snapshot.quality_guard_threshold, Some(90));
    assert_eq!(cached_snapshot.quality, None);
    assert_eq!(cached_snapshot.active_count, None);
    assert_eq!(cached_snapshot.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));
    let encoded = std::fs::read_to_string(temp.path().join(input_cache::STATUS_FILE)).expect("persisted status JSON");
    assert_eq!(encoded.matches(r#""last_update""#).count(), 1);
    assert!(!encoded.contains("history"));
}

#[test]
fn persisted_cluster_snapshot_discards_unproven_staged_overlay_facts() {
    let mut telemetry = PlaylistUpdateInputTelemetry {
        refresh_policy: InputRefreshPolicy::FORCE,
        source: None,
        clusters: vec![PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Video,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: Some(100),
            candidate_count: Some(95),
            active_count: Some(95),
            threshold: Some(90),
            quality: Some(95),
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        }],
    };
    neutralize_overlaid_cluster_facts(&mut telemetry, ClusterFlags::Vod);

    let snapshot = persisted_cluster_snapshot(
        cluster(&telemetry, XtreamCluster::Video),
        completed_snapshot_context(telemetry.refresh_policy, Some(InputRefreshPolicy::FORCE), 0),
    );

    assert_eq!(snapshot, PersistedPlaylistUpdateClusterSnapshot::default());
}

#[test]
fn persisted_cluster_snapshot_leaves_unscoped_multi_cluster_failure_unknown() {
    let cluster = PlaylistUpdateClusterTelemetry {
        cluster: XtreamCluster::Live,
        requested: true,
        source: Some(PlaylistUpdateDataSource::Provider),
        baseline_count: Some(100),
        candidate_count: Some(95),
        active_count: None,
        threshold: Some(90),
        quality: Some(95),
        decision: Some(PlaylistUpdateClusterDecision::Accepted),
        technical_state: None,
    };

    let snapshot =
        persisted_cluster_snapshot(&cluster, failed_provider_snapshot_context(InputRefreshPolicy::NORMAL, None, 2));

    assert_eq!(snapshot.technical_state, None);
    assert_eq!(
        snapshot.quality.map(|quality| quality.decision),
        Some(PersistedPlaylistUpdateQualityDecision::Accepted)
    );

    let unscoped_ancillary_failure = persisted_cluster_snapshot(
        &cluster,
        PersistedSnapshotContext {
            effective_policy: InputRefreshPolicy::NORMAL,
            request_policy: None,
            technical_failure: true,
            storage_failure: false,
            provider_failure_evidence: ProviderFailureEvidence::None,
            provider_cluster_count: 1,
        },
    );
    assert_eq!(
        unscoped_ancillary_failure.technical_state, None,
        "an input-wide ancillary failure is not invented as a cluster-scoped provider failure"
    );
}
