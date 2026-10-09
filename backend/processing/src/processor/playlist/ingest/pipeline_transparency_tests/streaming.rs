use super::*;

#[test]
fn pipeline_transparency_accepted_quality_survives_fetch_error_without_active_claim() {
    let mut input = input(InputType::Xtream);
    input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
        update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
        ..ConfigInputOptionsDto::default()
    }));
    let acceptance = ClusterUpdateAcceptance {
        cluster: XtreamCluster::Live,
        current_count: Some(100),
        candidate_count: 95,
        threshold: 90,
        quality: Some(95),
    };
    let fetch = PlaylistFetch::groups(Vec::new())
        .with_quality_acceptances(vec![acceptance])
        .with_errors(vec![TuliproxError::RepositoryXtream("post-quality publish failure".to_string())]);
    let telemetry = input_telemetry_for_fetch(
        &input,
        InputRefreshPolicy::NORMAL,
        PlaylistUpdateDataSource::Provider,
        &[XtreamCluster::Live],
        Some(&fetch),
    );
    let mut download_result = PlaylistDownloadResult::from(fetch).with_input_telemetry(telemetry);

    finalize_input_telemetry(&mut download_result, None);

    let telemetry = download_result.input_telemetry.as_ref().expect("input telemetry");
    let live = cluster(telemetry, XtreamCluster::Live);
    assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
    assert_eq!((live.baseline_count, live.candidate_count), (Some(100), Some(95)));
    assert_eq!((live.threshold, live.quality), (Some(90), Some(95)));
    assert_eq!(live.active_count, None);
    assert_eq!(download_result.download_err.len(), 1);
}
