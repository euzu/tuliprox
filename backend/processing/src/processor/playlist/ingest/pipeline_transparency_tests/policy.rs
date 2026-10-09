use super::*;

#[test]
fn pipeline_transparency_telemetry_preserves_cache_rejection_and_force_decisions() {
    let input = input(InputType::Xtream);
    let fetch = PlaylistFetch::groups(Vec::new())
        .with_quality_rejections(vec![ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 12_543,
            candidate_count: 217,
            threshold: 90,
            quality: 1,
        }])
        .with_force_updates(vec![ClusterForceUpdate {
            cluster: XtreamCluster::Series,
            candidate_count: 8_412,
            configured_threshold: 100,
        }]);

    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::FORCE,
        PlaylistUpdateDataSource::Provider,
        &[XtreamCluster::Video, XtreamCluster::Series],
        Some(&fetch),
    );

    let live = cluster(&telemetry, XtreamCluster::Live);
    assert!(live.requested);
    assert_eq!(live.source, Some(PlaylistUpdateDataSource::Cache));
    assert_eq!(live.decision, None);

    let video = cluster(&telemetry, XtreamCluster::Video);
    assert_eq!(video.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(video.decision, Some(PlaylistUpdateClusterDecision::Rejected));
    assert_eq!(
        (video.baseline_count, video.candidate_count, video.active_count),
        (Some(12_543), Some(217), Some(12_543))
    );
    assert_eq!((video.threshold, video.quality), (Some(90), Some(1)));

    let series = cluster(&telemetry, XtreamCluster::Series);
    assert_eq!(series.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(series.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!((series.candidate_count, series.active_count), (Some(8_412), None));
    assert_eq!(series.threshold, Some(100));

    let mut published = telemetry;
    confirm_published_cluster_counts(&mut published);
    assert_eq!(cluster(&published, XtreamCluster::Series).active_count, Some(8_412));
}

#[test]
fn pipeline_transparency_telemetry_preserves_accepted_quality_and_bootstrap_facts() {
    let mut input = input(InputType::Xtream);
    input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
        update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 90, series: 0 },
        ..ConfigInputOptionsDto::default()
    }));
    let fetch = PlaylistFetch::groups(Vec::new()).with_quality_acceptances(vec![
        ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: Some(12_543),
            candidate_count: 12_000,
            threshold: 90,
            quality: Some(95),
        },
        ClusterUpdateAcceptance {
            cluster: XtreamCluster::Video,
            current_count: None,
            candidate_count: 217,
            threshold: 90,
            quality: None,
        },
    ]);

    let mut telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &PIPELINE_TRANSPARENCY_CLUSTERS,
        Some(&fetch),
    );

    let live = cluster(&telemetry, XtreamCluster::Live);
    assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!((live.baseline_count, live.candidate_count), (Some(12_543), Some(12_000)));
    assert_eq!((live.threshold, live.quality, live.active_count), (Some(90), Some(95), None));

    let video = cluster(&telemetry, XtreamCluster::Video);
    assert_eq!(video.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!((video.baseline_count, video.candidate_count), (None, Some(217)));
    assert_eq!((video.threshold, video.quality, video.active_count), (Some(90), None, None));

    confirm_published_cluster_counts(&mut telemetry);
    assert_eq!(cluster(&telemetry, XtreamCluster::Live).active_count, Some(12_000));
    assert_eq!(cluster(&telemetry, XtreamCluster::Video).active_count, Some(217));
}

#[test]
fn pipeline_transparency_telemetry_keeps_zero_threshold_quality_disabled() {
    let input = input(InputType::Xtream);
    let fetch = PlaylistFetch::groups(Vec::new());

    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &[XtreamCluster::Live],
        Some(&fetch),
    );

    let live = cluster(&telemetry, XtreamCluster::Live);
    assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!(live.threshold, None);
    assert_eq!(live.quality, None);
    assert_eq!(live.active_count, None);
}

#[test]
fn pipeline_transparency_telemetry_does_not_infer_an_unreported_quality_acceptance() {
    let mut input = input(InputType::Xtream);
    input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
        skip_vod: true,
        update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
        ..ConfigInputOptionsDto::default()
    }));
    let fetch = PlaylistFetch::groups(Vec::new());

    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &[XtreamCluster::Live],
        Some(&fetch),
    );

    let live = cluster(&telemetry, XtreamCluster::Live);
    assert_eq!(live.threshold, Some(90));
    assert_eq!(live.decision, None, "an accepted quality decision was not retained by the provider result");
    let video = cluster(&telemetry, XtreamCluster::Video);
    assert!(!video.requested);
    assert_eq!(video.source, None);
    assert_eq!(video.decision, None);
}
