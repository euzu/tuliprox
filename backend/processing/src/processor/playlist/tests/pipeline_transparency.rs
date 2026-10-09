use super::{
    apply_staged_overlay_groups, neutralize_overlaid_cluster_facts, playlist_update_run_result,
    report_input_job_completion, test_group, InputJobState, PlaylistRunCollectSink,
};
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, InputRefreshPolicy, InputType, PlaylistUpdateClusterDecision,
        PlaylistUpdateClusterTelemetry, PlaylistUpdateDataSource, PlaylistUpdateInputTelemetry, PlaylistUpdateRunId,
        PlaylistUpdateRunOrder, PlaylistUpdateState, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_core::model::{ConfigInput, ConfigInputOptions};

#[test]
fn playlist_update_run_input_completion_uses_existing_success_partial_failure_classification() {
    let success = playlist_update_run_result(1, InputJobState::Ready, Vec::new(), false);
    let rejection = playlist_update_run_result(2, InputJobState::Ready, Vec::new(), true);
    let resumable = playlist_update_run_result(3, InputJobState::Pending, Vec::new(), false);
    let failure = playlist_update_run_result(
        4,
        InputJobState::Ready,
        vec![TuliproxError::RepositoryPlaylist("technical".to_string())],
        false,
    );

    assert_eq!(success.update_state(), PlaylistUpdateState::Success);
    assert_eq!(rejection.update_state(), PlaylistUpdateState::Partial);
    assert_eq!(resumable.update_state(), PlaylistUpdateState::Partial);
    assert_eq!(failure.update_state(), PlaylistUpdateState::Failure);
}

#[test]
fn pipeline_transparency_input_completion_reuses_correlated_progress_event_for_typed_telemetry() {
    let events = PlaylistRunCollectSink::default();
    let run_id = PlaylistUpdateRunId::from("pipeline-transparency-run");
    let execution_order = PlaylistUpdateRunOrder::from(19);
    let mut result = playlist_update_run_result(7, InputJobState::Ready, Vec::new(), true);
    let telemetry = PlaylistUpdateInputTelemetry {
        refresh_policy: InputRefreshPolicy::NORMAL,
        source: None,
        clusters: vec![PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Video,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: Some(12_543),
            candidate_count: Some(217),
            active_count: Some(12_543),
            threshold: Some(90),
            quality: Some(1),
            decision: Some(PlaylistUpdateClusterDecision::Rejected),
            technical_state: None,
        }],
    };
    result.input_telemetry = Some(telemetry.clone());

    report_input_job_completion(&events, &run_id, execution_order, &result);

    let emitted = events.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let EventMessage::PlaylistUpdateProgress(progress) = &emitted[0] else {
        panic!("expected existing playlist progress event");
    };
    assert_eq!(progress.run_id.as_ref(), Some(&run_id));
    assert_eq!(progress.execution_order, Some(execution_order));
    assert_eq!(progress.input_id, Some(7));
    assert_eq!(progress.state, Some(PlaylistUpdateState::Partial));
    assert_eq!(progress.input_telemetry, Some(telemetry));
}

#[test]
fn playlist_update_run_technical_failure_progress_is_correlated_sanitized_and_input_scoped() {
    let events = PlaylistRunCollectSink::default();
    let run_id = PlaylistUpdateRunId::from("processing-run");
    let execution_order = PlaylistUpdateRunOrder::from(17);
    let failed = playlist_update_run_result(
        7,
        InputJobState::Failed,
        vec![TuliproxError::RepositoryPlaylist("https://user:secret@provider.example/playlist".to_string())],
        false,
    );
    let succeeded = playlist_update_run_result(8, InputJobState::Ready, Vec::new(), false);

    report_input_job_completion(&events, &run_id, execution_order, &failed);
    report_input_job_completion(&events, &run_id, execution_order, &succeeded);

    let emitted = events.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let progress = emitted
        .iter()
        .filter_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) => Some(progress),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 2);
    assert!(progress.iter().all(|event| event.run_id.as_ref() == Some(&run_id)));
    assert!(progress.iter().all(|event| event.execution_order == Some(execution_order)));
    assert_eq!(progress[0].input_id, Some(7));
    assert_eq!(progress[0].state, Some(PlaylistUpdateState::Failure));
    assert_eq!(progress[0].message, "Input 'input-7' failed during update");
    assert!(!progress[0].message.contains("secret"));
    assert!(!progress[0].message.contains("previous accepted data"));
    assert_eq!(progress[1].input_id, Some(8));
    assert_eq!(progress[1].state, Some(PlaylistUpdateState::Success));
    assert!(!progress[1].message.contains("failed during update"));
}

#[test]
fn pipeline_transparency_staged_overlay_replaces_groups_and_neutralizes_overlaid_runtime_facts() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        options: Some(ConfigInputOptions::defaults().clone()),
        ..Default::default()
    };
    let provider_groups = vec![
        test_group(XtreamCluster::Live, "provider-live", "provider"),
        test_group(XtreamCluster::Video, "provider-vod", "provider"),
    ];
    let mut staged_live = test_group(XtreamCluster::Live, "staged-live", "staged");
    staged_live.channels[0].header.id = "11203".intern();
    staged_live.channels[0].header.url = "http://iptvhost.example/live/fake-user/fake-pass/11203.ts".intern();
    let mut invalid = test_group(XtreamCluster::Live, "invalid", "staged").channels.remove(0);
    invalid.header.id = "invalid".intern();
    invalid.header.url = "http://iptvhost.example/live/fake-user/fake-pass/invalid.ts".intern();
    staged_live.channels.push(invalid);
    let staged_groups = vec![staged_live, test_group(XtreamCluster::Series, "staged-series", "staged")];

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Live, provider_groups, staged_groups);

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "staged-live");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[0].channels[0].header.url.as_ref(), "http://provider.example/live/real-user/real-pass/11203.ts");
    assert_eq!(groups[1].title.as_ref(), "provider-vod");
    assert_eq!(groups[1].channels[0].header.input_name.as_ref(), "provider");

    let mut telemetry = PlaylistUpdateInputTelemetry {
        refresh_policy: InputRefreshPolicy::NORMAL,
        source: None,
        clusters: vec![
            PlaylistUpdateClusterTelemetry {
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
            },
            PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Video,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Provider),
                baseline_count: Some(200),
                candidate_count: Some(200),
                active_count: Some(200),
                threshold: Some(100),
                quality: Some(100),
                decision: Some(PlaylistUpdateClusterDecision::Accepted),
                technical_state: None,
            },
        ],
    };

    neutralize_overlaid_cluster_facts(&mut telemetry, ClusterFlags::Live);

    let live =
        telemetry.clusters.iter().find(|cluster| cluster.cluster == XtreamCluster::Live).expect("Live telemetry");
    assert!(live.requested);
    assert_eq!(live.threshold, Some(90));
    assert_eq!(live.source, None);
    assert_eq!(live.baseline_count, None);
    assert_eq!(live.candidate_count, None);
    assert_eq!(live.active_count, None);
    assert_eq!(live.quality, None);
    assert_eq!(live.decision, None);

    let video =
        telemetry.clusters.iter().find(|cluster| cluster.cluster == XtreamCluster::Video).expect("Video telemetry");
    assert_eq!(video.source, Some(PlaylistUpdateDataSource::Provider));
    assert_eq!(video.baseline_count, Some(200));
    assert_eq!(video.candidate_count, Some(200));
    assert_eq!(video.active_count, Some(200));
    assert_eq!(video.quality, Some(100));
    assert_eq!(video.decision, Some(PlaylistUpdateClusterDecision::Accepted));
}
