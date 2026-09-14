mod persisted_input_status {
    use super::*;
    use crate::processor::playlist::input_status::{persist_input_completion, persist_input_job_result};
    use shared::model::PersistedPlaylistUpdateInputResult;

    fn acquired_result(state: PlaylistUpdateState) -> InputJobResult {
        let mut result = super::super::playlist_update_run_result(
            7,
            InputJobState::Ready,
            vec![],
            state == PlaylistUpdateState::Partial,
        );
        result.input_telemetry = Some(PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::REFRESH,
            source: Some(PlaylistUpdateDataSource::Provider),
            clusters: Vec::new(),
        });
        result
    }

    #[tokio::test]
    async fn playlist_update_status_run_precedence_is_isolated_by_stable_input_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = processing_context(temp.path());
        let failed = super::super::playlist_update_run_result(7, InputJobState::Failed, vec![], false);
        persist_input_job_result(&ctx, &failed).await;
        let mut independent = acquired_result(PlaylistUpdateState::Success);
        independent.input_id = 8;
        independent.input_name = "independent".intern();
        persist_input_job_result(&ctx, &independent).await;
        for (result, state) in [(failed, PlaylistUpdateState::Failure), (independent, PlaylistUpdateState::Success)] {
            let path =
                input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &result.input_name).await;
            assert_eq!(input_cache::load_input_status(&path).last_input_update.unwrap().state, state);
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_playlist_update_status_staged_completion_is_independent_of_parent_failure() {
        for (parent_fails, staged_fails) in [(false, false), (true, false), (false, true)] {
            let temp = tempfile::tempdir().unwrap();
            let events = CollectSink::default();
            let ctx = processing_context_with_events(temp.path(), events.clone());
            let parent_path = temp.path().join("parent.m3u");
            let staged_path = temp.path().join("overlay.m3u");
            for (path, fails) in [(&parent_path, parent_fails), (&staged_path, staged_fails)] {
                if !fails {
                    std::fs::write(path, "#EXTM3U\n#EXTINF:-1,Fixture\nhttp://stream.example/fixture\n").unwrap();
                }
            }
            let parent = Arc::new(ConfigInput {
                id: 7,
                name: "parent".intern(),
                input_type: InputType::M3u,
                url: parent_path.to_string_lossy().into_owned(),
                enabled: true,
                ..ConfigInput::default()
            });
            let mut staged = ConfigInput {
                id: 8,
                name: "staged".intern(),
                input_type: InputType::Staged,
                url: staged_path.to_string_lossy().into_owned(),
                enabled: true,
                staged: Some(tuliprox_core::model::ConfigInputStaged {
                    for_input: Some(parent.name.clone()),
                    clusters: ClusterFlags::Live,
                }),
                ..ConfigInput::default()
            };
            staged.resolve_staged_download_type();
            let staged = Arc::new(staged);
            ctx.config.sources.store(Arc::new(SourcesConfig {
                inputs: vec![parent.clone(), staged.clone()],
                ..SourcesConfig::default()
            }));
            let result = download_input(&ctx, &parent, false).await;
            assert_eq!(!result.errors.is_empty(), parent_fails || staged_fails);
            let path = input_storage_path(&ctx, &staged).await;
            let persisted = input_cache::load_input_status(&path);
            assert_eq!(
                persisted.last_input_update.unwrap().state,
                if staged_fails { PlaylistUpdateState::Failure } else { PlaylistUpdateState::Success }
            );
            assert_eq!(persisted.clusters.len(), 4);
            assert!(persisted.clusters["default"].last_update.is_none(), "document cache and cluster facts stay separate");
            for cluster in XTREAM_CLUSTER {
                assert!(persisted.clusters[cluster.as_ref()].last_update.is_some());
            }
            let completions = staged_completion::input_completions(&events, staged.id);
            assert_eq!(completions.len(), 1, "real staged download must report its own completion");
            assert_eq!(completions[0].state, persisted.last_input_update.map(|result| result.state));
            assert_eq!(completions[0].run_id, Some(ctx.run_id.clone()));
            assert_eq!(completions[0].execution_order, Some(ctx.execution_order));
            let telemetry = completions[0].input_telemetry.as_ref().unwrap();
            assert_eq!(telemetry.source, None);
            assert!(telemetry.clusters.iter().all(|cluster| cluster.source == Some(PlaylistUpdateDataSource::Provider)));
            assert!(staged_completion::input_completions(&events, parent.id).is_empty());
        }
    }

    #[tokio::test]
    async fn playlist_update_status_delayed_source_results_keep_strongest_completion_until_next_run() {
        for weaker in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial] {
            for failure_first in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let events = CollectSink::default();
                let ctx = processing_context_with_events(temp.path(), events.clone());
                let acquired = acquired_result(weaker);
                let failed_reuse = super::super::playlist_update_run_result(7, InputJobState::Failed, vec![], false);
                let (first, delayed) = if failure_first { (failed_reuse, acquired) } else { (acquired, failed_reuse) };
                let path =
                    input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &first.input_name).await;
                let mut original = input_cache::InputStatus::default();
                original.clusters.insert(
                    "live".into(),
                    input_cache::ClusterStatus {
                        status: input_cache::ClusterState::Failed,
                        timestamp: 123,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            source: Some(PlaylistUpdateDataSource::Cache),
                            policy: Some(InputRefreshPolicy::FORCE),
                            quality_guard_threshold: Some(95),
                            ..PersistedPlaylistUpdateClusterSnapshot::default()
                        }),
                    },
                );
                input_cache::save_input_status(&path, &original);
                let (release, other_input_finished) = tokio::sync::oneshot::channel();
                let delayed_ctx = ctx.clone();
                // Source A already holds its shared-input result, but cannot drain
                // its sorted result list until another input job has completed.
                let delayed_source = async move {
                    let source_results = vec![delayed];
                    other_input_finished.await.unwrap();
                    for result in source_results {
                        persist_input_job_result(&delayed_ctx, &result).await;
                        report_input_job_completion(
                            &delayed_ctx.events,
                            &delayed_ctx.run_id,
                            delayed_ctx.execution_order,
                            &result,
                        );
                    }
                };
                let first_source = async {
                    persist_input_job_result(&ctx, &first).await;
                    report_input_job_completion(&ctx.events, &ctx.run_id, ctx.execution_order, &first);
                    let mut persisted = input_cache::load_input_status(&path);
                    assert_eq!(persisted.last_input_update.unwrap().state, first.update_state());
                    // Distinct sentinel proves a delayed weaker result does not
                    // advance the stronger completion's timestamp, without sleeps.
                    persisted.last_input_update.as_mut().unwrap().timestamp = 456;
                    input_cache::save_input_status(&path, &persisted);
                    release.send(()).unwrap();
                };
                tokio::join!(delayed_source, first_source);
                let final_status = input_cache::load_input_status(&path);
                let completion = final_status.last_input_update.unwrap();
                assert_eq!(
                    completion.state,
                    PlaylistUpdateState::Failure,
                    "weaker={weaker:?}, failure_first={failure_first}"
                );
                if failure_first {
                    assert_eq!(completion.timestamp, 456);
                }
                assert_eq!(final_status.clusters, original.clusters);
                let states: Vec<_> = events
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|event| match event {
                        EventMessage::PlaylistUpdateProgress(progress) => progress.state,
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    states,
                    if failure_first {
                        vec![PlaylistUpdateState::Failure, weaker]
                    } else {
                        vec![weaker, PlaylistUpdateState::Failure]
                    }
                );

                let mut next = processing_context(temp.path());
                next.run_id = "next-successful-run".into();
                next.execution_order = 2.into();
                let success = acquired_result(PlaylistUpdateState::Success);
                persist_input_job_result(&next, &success).await;
                let replacement = input_cache::load_input_status(&path);
                assert_eq!(replacement.last_input_update.unwrap().state, PlaylistUpdateState::Success);
                assert_eq!(replacement.clusters, original.clusters);
            }
        }
    }

    #[tokio::test]
    async fn playlist_update_status_parent_download_persists_staged_own_per_cluster_success_and_failure() {
        for fails in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut responses = fixture_responses([1, 0, 0]);
            if fails {
                responses.insert("get_live_streams".into(), "not JSON".into());
            }
            let server = TestXtreamServer::start_with_responses(responses);
            let events = CollectSink::default();
            let ctx = processing_context_with_events(temp.path(), events.clone());
            let parent_file = temp.path().join("parent.m3u");
            std::fs::write(&parent_file, "#EXTM3U\n#EXTINF:-1,Parent\nhttp://stream.example/parent\n").unwrap();
            let parent = Arc::new(ConfigInput {
                id: 7,
                name: "parent".intern(),
                input_type: InputType::M3u,
                url: parent_file.to_string_lossy().into_owned(),
                enabled: true,
                ..ConfigInput::default()
            });
            let mut staged =
                (*test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 95, vod: 0, series: 0 }, 0, &[]))
                    .clone();
            staged.id = 8;
            staged.name = "own-staged".intern();
            staged.input_type = InputType::Staged;
            staged.staged_type = shared::model::StagedInputType::Xtream;
            staged.staged = Some(tuliprox_core::model::ConfigInputStaged {
                for_input: Some(parent.name.clone()),
                clusters: ClusterFlags::Live,
            });
            staged.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
                skip_vod: true,
                skip_series: true,
                update_quality: ConfigInputUpdateQualityDto { live: 95, vod: 0, series: 0 },
                ..ConfigInputOptionsDto::default()
            }));
            staged.resolve_staged_download_type(); // Same resolver used when SourcesConfig is built.
            let staged = Arc::new(staged);
            ctx.config.sources.store(Arc::new(SourcesConfig {
                inputs: vec![parent.clone(), staged.clone()],
                ..SourcesConfig::default()
            }));
            let staged_path = input_storage_path(&ctx, &staged).await;
            // Referencing or directly skipping a staged input is not an acquisition.
            let skipped = download_input(&ctx, &staged, false).await;
            assert!(skipped.input_telemetry.is_none());
            assert!(input_cache::load_input_status(&staged_path).last_input_update.is_none());
            assert!(staged_completion::input_completions(&events, staged.id).is_empty());

            let result = download_input(&ctx, &parent, false).await;
            let status = input_cache::load_input_status(&staged_path);
            assert!(!status.clusters.contains_key("default"));
            assert_eq!(status.clusters.len(), 1);
            let completion =
                status.last_input_update.expect("actual indirect staged acquisition has its own completion");
            assert_eq!(
                completion.state,
                if fails { PlaylistUpdateState::Failure } else { PlaylistUpdateState::Success }
            );
            assert!(completion.timestamp > 0);
            let completions = staged_completion::input_completions(&events, staged.id);
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].state, Some(completion.state));
            assert_eq!(completions[0].run_id, Some(ctx.run_id.clone()));
            assert_eq!(completions[0].execution_order, Some(ctx.execution_order));
            let snapshot = status.clusters["live"].last_update.unwrap();
            assert_eq!(snapshot.source, Some(PlaylistUpdateDataSource::Provider));
            assert_eq!(snapshot.policy, None);
            if fails {
                assert_eq!(snapshot.quality_guard_threshold, Some(95));
                assert_eq!(snapshot.quality, None);
            } else {
                assert_eq!(snapshot.quality_guard_threshold, None, "the evaluation already owns its threshold");
                let quality = snapshot.quality.unwrap();
                assert_eq!(quality.threshold, 95);
                assert_eq!(quality.decision, PersistedPlaylistUpdateQualityDecision::Accepted);
                assert_eq!(quality.achieved_quality, None, "bootstrap has no invented Quality");
            }
            assert_eq!(result.errors.is_empty(), !fails);
            let parent_path = input_storage_path(&ctx, &parent).await;
            assert_ne!(parent_path, staged_path);
            assert!(
                input_cache::load_input_status(&parent_path).last_input_update.is_none(),
                "staged result does not invent a parent job completion"
            );
            if !fails {
                let bytes = std::fs::read(staged_path.join(input_cache::STATUS_FILE)).unwrap();
                let reuse = download_input(&ctx, &parent, false).await;
                assert!(reuse.input_telemetry.is_none());
                let mut staged_reuse = download_input(&ctx, &staged, true).await;
                assert!(staged_reuse.input_telemetry.is_none());
                super::super::input_status::complete_staged_input(&ctx, &staged, &mut staged_reuse).await;
                assert_eq!(std::fs::read(staged_path.join(input_cache::STATUS_FILE)).unwrap(), bytes);
                assert_eq!(staged_completion::input_completions(&events, staged.id), completions);
            }
            let requests = server.finish();
            assert_eq!(
                requests.iter().filter(|action| action.as_str() == "get_live_streams").count(),
                1,
                "no extra staged acquisition on parent reuse"
            );
        }
    }

    #[tokio::test]
    async fn playlist_update_status_completion_replaces_one_result_without_touching_cache_or_cluster_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = processing_context(temp.path());
        let input_name: Arc<str> = "result-input".intern();
        let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &input_name).await;
        let mut original = input_cache::InputStatus::default();
        input_cache::update_cluster_status(&mut original, "default", input_cache::ClusterState::Ok);
        original.clusters.get_mut("default").unwrap().timestamp = 1; // Expired cache must stay expired.
        input_cache::replace_cluster_snapshot(
            &mut original,
            "default",
            PersistedPlaylistUpdateClusterSnapshot {
                source: Some(PlaylistUpdateDataSource::Cache),
                ..PersistedPlaylistUpdateClusterSnapshot::default()
            },
        );
        input_cache::save_input_status(&path, &original);
        for state in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial, PlaylistUpdateState::Failure] {
            persist_input_completion(&ctx, 7, &input_name, state).await;
            let reloaded = input_cache::load_input_status(&path);
            let result = reloaded.last_input_update.unwrap();
            assert_eq!(result.state, state);
            assert!(result.timestamp > 1);
            assert_eq!(reloaded.clusters, original.clusters);
            assert!(!input_cache::is_cache_valid(&reloaded, "default", 3600));
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path.join(input_cache::STATUS_FILE)).unwrap()).unwrap();
            assert!(json["last_input_update"].is_object());
            assert_eq!(json.as_object().unwrap().len(), 2, "only clusters + one result, no history");
        }
    }

    #[tokio::test]
    async fn playlist_update_status_in_run_reuse_preserves_original_partial_result_and_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = processing_context(temp.path());
        let mut result = super::super::playlist_update_run_result(7, InputJobState::Ready, vec![], true);
        result.input_telemetry = Some(PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: Some(PlaylistUpdateDataSource::Provider),
            clusters: Vec::new(),
        });
        persist_input_job_result(&ctx, &result).await;
        let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &result.input_name).await;
        let mut original = input_cache::load_input_status(&path);
        assert_eq!(original.last_input_update.unwrap().state, PlaylistUpdateState::Partial);
        original.last_input_update.as_mut().unwrap().timestamp = 123;
        input_cache::save_input_status(&path, &original);
        let before = std::fs::read(path.join(input_cache::STATUS_FILE)).unwrap();
        result.had_quality_rejections = false;
        result.input_telemetry = None; // Same existing absence of acquisition facts as a real in-run reuse.
        persist_input_job_result(&ctx, &result).await;
        assert_eq!(std::fs::read(path.join(input_cache::STATUS_FILE)).unwrap(), before);
        result.state = InputJobState::Failed; // A genuine technical completion must still replace the prior result.
        persist_input_job_result(&ctx, &result).await;
        assert_eq!(
            input_cache::load_input_status(&path).last_input_update.unwrap().state,
            PlaylistUpdateState::Failure
        );
    }

    #[tokio::test]
    async fn playlist_update_status_completion_covers_every_input_type_without_artificial_clusters() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = processing_context(temp.path());
        for (id, input_type) in [
            InputType::Xtream,
            InputType::XtreamBatch,
            InputType::Stalker,
            InputType::StalkerBatch,
            InputType::M3u,
            InputType::M3uBatch,
            InputType::Library,
            InputType::Plex,
            InputType::Emby,
            InputType::Jellyfin,
            InputType::Staged,
        ]
        .into_iter()
        .enumerate()
        {
            let input = ConfigInput {
                id: u16::try_from(id).unwrap(),
                input_type,
                name: format!("input-{id}").intern(),
                ..ConfigInput::default()
            };
            let result = panicked_input_job(id, &input);
            persist_input_job_result(&ctx, &result).await;
            let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), &input.name).await;
            let persisted = input_cache::load_input_status(&path);
            assert!(persisted.clusters.is_empty());
            assert!(
                matches!(persisted.last_input_update, Some(PersistedPlaylistUpdateInputResult {state: PlaylistUpdateState::Failure, timestamp}) if timestamp > 0)
            );
        }
    }
}
