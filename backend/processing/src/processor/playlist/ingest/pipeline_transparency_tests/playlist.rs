use super::*;

#[test]
fn pipeline_transparency_known_cluster_failure_keeps_quality_decision_and_neutralizes_overlay() {
    let input = input(InputType::Xtream);
    let acceptance = ClusterUpdateAcceptance {
        cluster: XtreamCluster::Series,
        current_count: Some(100),
        candidate_count: 95,
        threshold: 90,
        quality: Some(95),
    };
    let mut fetch = PlaylistFetch::groups(Vec::new()).with_quality_acceptances(vec![acceptance]);
    fetch.record_cluster_error(XtreamCluster::Series, TuliproxError::RepositoryXtream("publish failure".to_string()));
    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &PIPELINE_TRANSPARENCY_CLUSTERS,
        Some(&fetch),
    );
    let mut result = PlaylistDownloadResult::from(fetch).with_input_telemetry(telemetry);
    finalize_input_telemetry(&mut result, None);
    let telemetry = result.input_telemetry.as_mut().unwrap();
    let shows = cluster(telemetry, XtreamCluster::Series);
    assert_eq!(shows.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!(shows.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
    assert_eq!(shows.active_count, None);
    let context = failed_provider_snapshot_context(InputRefreshPolicy::NORMAL, None, 3);
    let snapshot = persisted_cluster_snapshot(shows, context);
    assert_eq!(snapshot.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
    assert_eq!(snapshot.quality.unwrap().decision, PersistedPlaylistUpdateQualityDecision::Accepted);
    neutralize_overlaid_cluster_facts(telemetry, ClusterFlags::Series);
    let overlaid = cluster(telemetry, XtreamCluster::Series);
    assert_eq!(overlaid.technical_state, None);
    assert_eq!(persisted_cluster_snapshot(overlaid, context).technical_state, None);
}

#[test]
fn pipeline_transparency_telemetry_does_not_assign_an_unscoped_error_to_multiple_clusters() {
    let input = input(InputType::Stalker);
    let fetch = PlaylistFetch::failed(TuliproxError::Download("provider unavailable".to_string()));

    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &PIPELINE_TRANSPARENCY_CLUSTERS,
        Some(&fetch),
    );

    assert!(telemetry.clusters.iter().all(|item| item.requested && item.decision.is_none()));
}
