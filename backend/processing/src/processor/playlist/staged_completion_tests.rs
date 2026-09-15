mod staged_completion {
    use super::*;

    pub(super) fn input_completions(events: &CollectSink, input_id: u16) -> Vec<PlaylistUpdateProgressEvent> {
        events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                EventMessage::PlaylistUpdateProgress(progress)
                    if progress.input_id == Some(input_id) && progress.state.is_some() =>
                {
                    Some(progress.clone())
                }
                _ => None,
            })
            .collect()
    }

    async fn staged_run_context(
        root: &Path,
        base_url: &str,
    ) -> (PlaylistProcessingContext<CollectSink>, Arc<ConfigInput>) {
        let ctx = processing_context_with_events(root, CollectSink::default());
        let parent_file = root.join("parent.m3u");
        std::fs::write(&parent_file, "#EXTM3U\n#EXTINF:-1,Parent\nhttp://stream.example/parent\n").unwrap();
        let parent = Arc::new(ConfigInput {
            id: 7,
            name: "parent".intern(),
            input_type: InputType::M3u,
            url: parent_file.to_string_lossy().into_owned(),
            enabled: true,
            ..ConfigInput::default()
        });
        let mut staged = (*test_input(
            base_url,
            ConfigInputUpdateQualityDto { live: 95, vod: 0, series: 0 },
            0,
            &[XtreamCluster::Video, XtreamCluster::Series],
        ))
        .clone();
        staged.id = 8;
        staged.name = "staged".intern();
        staged.input_type = InputType::Staged;
        staged.staged_type = shared::model::StagedInputType::Xtream;
        staged.staged = Some(tuliprox_core::model::ConfigInputStaged {
            for_input: Some(parent.name.clone()),
            clusters: ClusterFlags::Live,
        });
        staged.resolve_staged_download_type();
        let staged = Arc::new(staged);
        seed_live_baseline(&ctx, &staged, 100).await;
        let failed_companion = Arc::new(ConfigInput {
            id: 9,
            name: "failed-companion".intern(),
            input_type: InputType::M3u,
            url: root.join("missing.m3u").to_string_lossy().into_owned(),
            enabled: true,
            ..ConfigInput::default()
        });
        ctx.config.sources.store(Arc::new(SourcesConfig {
            inputs: vec![parent.clone(), staged.clone(), failed_companion.clone()],
            sources: vec![ConfigSource {
                inputs: vec![parent.name.clone(), failed_companion.name.clone()],
                targets: vec![xtream_target(true)],
            }],
            ..SourcesConfig::default()
        }));
        (ctx, staged)
    }

    fn normalized_trace(
        events: &[EventMessage],
        persisted: shared::model::PersistedPlaylistUpdateInputResult,
    ) -> serde_json::Value {
        let summary = events
            .iter()
            .find_map(|event| match event {
                EventMessage::PlaylistUpdate(summary) => Some(summary),
                _ => None,
            })
            .unwrap();
        assert_eq!(summary.run_id, Some("staged-completion-run".into()));
        assert!(summary.execution_order.is_some());
        assert_eq!(summary.state, PlaylistUpdateState::Failure);
        let progress: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                EventMessage::PlaylistUpdateProgress(progress)
                    if progress.input_id == Some(8) || progress.state.is_some() =>
                {
                    assert_eq!(progress.run_id, summary.run_id);
                    assert_eq!(progress.execution_order, summary.execution_order);
                    let mut progress = progress.clone();
                    // Only the globally allocated test-run order is normalized. All
                    // input identities, states, messages and telemetry remain exact.
                    progress.execution_order = Some(1.into());
                    Some(progress)
                }
                _ => None,
            })
            .collect();
        assert!(persisted.timestamp > 0);
        serde_json::json!({
            "progress": progress,
            "completed": {"run_id": summary.run_id, "execution_order": 1, "state": summary.state},
            "persisted_input_result": {"state": persisted.state, "timestamp": 1}
        })
    }

    #[tokio::test]
    async fn playlist_update_status_staged_reporter_trace_matches_persistence_before_foreign_run_failure() {
        let mut traces = Vec::new();
        for expected in [PlaylistUpdateState::Partial, PlaylistUpdateState::Success, PlaylistUpdateState::Failure] {
            let temp = tempfile::tempdir().unwrap();
            let mut responses =
                fixture_responses([if expected == PlaylistUpdateState::Success { 100 } else { 1 }, 0, 0]);
            if expected == PlaylistUpdateState::Failure {
                responses.insert("get_live_streams".into(), "not JSON".into());
            }
            let server = TestXtreamServer::start_with_responses(responses);
            let (ctx, staged) = staged_run_context(temp.path(), &server.base_url).await;
            exec_processing(ProcessingRun::for_run(
                "staged-completion-run".into(),
                ctx.client.clone(),
                ctx.config.clone(),
                ctx.user_targets.clone(),
                ctx.events.clone(),
            ))
            .await;
            let path = input_storage_path(&ctx, &staged).await;
            let persisted = input_cache::load_input_status(&path);
            assert!(!persisted.clusters.contains_key("default"));
            assert_eq!(persisted.last_input_update.unwrap().state, expected);
            let completions = input_completions(&ctx.events, staged.id);
            assert_eq!(completions.len(), 1, "real indirect staged input must emit one correlated completion");
            assert_eq!(completions[0].state, Some(expected));
            let telemetry = completions[0].input_telemetry.as_ref().unwrap();
            assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::NORMAL);
            assert_eq!(telemetry.source, None);
            assert_eq!(telemetry.clusters[0].source, Some(PlaylistUpdateDataSource::Provider));
            assert_eq!(telemetry.clusters[0].threshold, Some(95));
            if expected == PlaylistUpdateState::Partial {
                assert_eq!(telemetry.clusters[0].decision, Some(PlaylistUpdateClusterDecision::Rejected));
                assert_eq!(telemetry.clusters[0].active_count, Some(100));
            }
            let emitted = ctx.events.0.lock().unwrap().clone();
            let own_index = emitted.iter().position(|event| matches!(event, EventMessage::PlaylistUpdateProgress(p) if p.input_id == Some(8) && p.state.is_some())).unwrap();
            let parent_index = emitted.iter().position(|event| matches!(event, EventMessage::PlaylistUpdateProgress(p) if p.input_id == Some(7) && p.state.is_some())).unwrap();
            let completed_index =
                emitted.iter().position(|event| matches!(event, EventMessage::PlaylistUpdate(_))).unwrap();
            assert!(own_index < parent_index && parent_index < completed_index);
            assert_eq!(input_completions(&ctx.events, 9)[0].state, Some(PlaylistUpdateState::Failure));
            traces.push(normalized_trace(&emitted, persisted.last_input_update.unwrap()));
            assert_eq!(server.finish().iter().filter(|action| action.as_str() == "get_live_streams").count(), 1);
        }
        let actual = serde_json::Value::Array(traces);
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../frontend/src/model/test_data/staged_completions.json"
        )))
        .unwrap();
        assert_eq!(actual, fixture, "actual reporter trace: {}", serde_json::to_string_pretty(&actual).unwrap());
    }
}
