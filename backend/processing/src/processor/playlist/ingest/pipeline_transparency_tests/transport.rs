use super::*;

#[test]
fn pipeline_transparency_telemetry_reports_non_cluster_cache_without_synthetic_clusters() {
    let telemetry = input_telemetry_for_fetch(
        &input(InputType::Plex),
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Cache,
        &[],
        None,
    );

    assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::NORMAL);
    assert_eq!(telemetry.source, Some(PlaylistUpdateDataSource::Cache));
    assert!(telemetry.clusters.is_empty());
}

#[test]
fn pipeline_transparency_telemetry_types_complete_provider_failure_without_guessing_metrics() {
    let mut input = input(InputType::Xtream);
    input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
        skip_vod: true,
        skip_series: true,
        ..ConfigInputOptionsDto::default()
    }));
    let fetch = PlaylistFetch::failed(TuliproxError::Download("provider unavailable".to_string()));

    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::REFRESH,
        PlaylistUpdateDataSource::Provider,
        &[XtreamCluster::Live],
        Some(&fetch),
    );

    assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::REFRESH);
    let live = cluster(&telemetry, XtreamCluster::Live);
    assert!(live.requested);
    assert_eq!(live.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::TechnicalError));
    assert!(live.baseline_count.is_none());
    assert!(live.candidate_count.is_none());
    assert!(live.active_count.is_none());
    assert!(live.quality.is_none());
}
