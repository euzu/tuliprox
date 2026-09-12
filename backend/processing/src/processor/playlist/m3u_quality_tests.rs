mod m3u_update_quality {
    use super::*;
    use std::fmt::Write;
    use tuliprox_repository::{load_input_m3u_playlist, raw_group_catalog_path, RawInputGroupCatalog};

    fn document(prefix: &str, counts: [usize; 3]) -> String {
        let mut text = String::from("#EXTM3U\n");
        for (cluster, count) in XTREAM_CLUSTER.into_iter().zip(counts) {
            for index in 0..count {
                let (kind, group, title) = match cluster {
                    XtreamCluster::Live => ("live", format!("{prefix}-live"), format!("Channel {index}")),
                    XtreamCluster::Video => ("movie", format!("{prefix}-vod"), format!("Movie {index}")),
                    XtreamCluster::Series => {
                        ("series", format!("{prefix}-show-{index}"), format!("Show {index} S01E01"))
                    }
                };
                let id = match cluster {
                    XtreamCluster::Live => 1000,
                    XtreamCluster::Video => 2000,
                    XtreamCluster::Series => 3000,
                } + index;
                writeln!(text, "#EXTGRP:{prefix}-{cluster}\n#EXTINF:-1 xui-id=\"{id}\" tvg-type=\"{kind}\" group-title=\"{group}\",{title}\nhttp://stream.example/{prefix}/{cluster}/{id}").unwrap();
            }
        }
        text
    }

    async fn fixture(
        root: &Path,
        previous: Option<[usize; 3]>,
        thresholds: [u8; 3],
    ) -> (PlaylistProcessingContext<shared::model::NoopSink>, Arc<ConfigInput>) {
        let ctx = processing_context(root);
        let input = Arc::new(ConfigInput {
            id: 27,
            name: "m3u-quality".intern(),
            input_type: InputType::M3u,
            enabled: true,
            url: root.join("provider.m3u").to_string_lossy().into_owned(),
            options: Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
                update_quality: ConfigInputUpdateQualityDto {
                    live: thresholds[0],
                    vod: thresholds[1],
                    series: thresholds[2],
                },
                ..ConfigInputOptionsDto::default()
            })),
            ..ConfigInput::default()
        });
        if let Some(counts) = previous {
            tokio::fs::write(&input.url, document("previous", counts)).await.unwrap();
            let (groups, errors) =
                tuliprox_iptv::m3u::download_m3u_playlist(&ctx.config, &ctx.client, &ctx.config.config.load(), &input)
                    .await;
            assert!(errors.is_empty());
            let (_, error) = tuliprox_repository::persist_input_playlist(&ctx.config, &input, groups).await;
            assert!(error.is_none(), "{error:?}");
        }
        (ctx, input)
    }

    async fn run(
        ctx: &PlaylistProcessingContext<shared::model::NoopSink>,
        input: &Arc<ConfigInput>,
        counts: [usize; 3],
    ) -> InputDownloadResult {
        tokio::fs::write(&input.url, document("candidate", counts)).await.unwrap();
        download_input(ctx, input, false).await
    }

    fn counts(groups: &[PlaylistGroup]) -> [usize; 3] {
        XTREAM_CLUSTER
            .map(|cluster| groups.iter().filter(|g| g.xtream_cluster == cluster).map(|g| g.channels.len()).sum())
    }

    async fn verify_effective(
        ctx: &PlaylistProcessingContext<shared::model::NoopSink>,
        input: &ConfigInput,
        result: &mut InputDownloadResult,
        expected: [usize; 3],
    ) {
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.storage_error.is_none(), "{:?}", result.storage_error);
        assert_eq!(result.job_state(), InputJobState::Ready);
        let effective = result.source.take_groups();
        assert_eq!(counts(&effective), expected);
        let storage = tuliprox_repository::get_input_storage_path(&input.name, &ctx.config.config.load().storage_dir)
            .await
            .unwrap();
        let saved = load_input_m3u_playlist(
            &ctx.config,
            &tuliprox_repository::get_input_m3u_playlist_file_path(&storage, &input.name),
        )
        .await
        .unwrap();
        assert_eq!(counts(&saved), expected);
        for cluster in XTREAM_CLUSTER {
            let catalog: RawInputGroupCatalog =
                serde_json::from_slice(&tokio::fs::read(raw_group_catalog_path(&storage, cluster)).await.unwrap())
                    .unwrap();
            let mut expected_groups: Vec<_> =
                effective.iter().filter(|g| g.xtream_cluster == cluster).map(|g| g.title.to_string()).collect();
            expected_groups.sort();
            expected_groups.dedup();
            assert_eq!(catalog.groups, expected_groups);
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_all_clusters_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
        let mut result = run(&ctx, &input, [9, 11, 10]).await;
        assert!(result.quality_rejections.is_empty());
        assert_eq!(result.update_state(), PlaylistUpdateState::Success);
        verify_effective(&ctx, &input, &mut result, [9, 11, 10]).await;
    }

    async fn order_candidate(
        ctx: &PlaylistProcessingContext<shared::model::NoopSink>,
        input: &ConfigInput,
        prefix: &str,
    ) -> tuliprox_iptv::provider::PlaylistFetch {
        let mut body = String::from("#EXTM3U\n");
        for (id, kind, group) in [
            (2000, "movie", "obsolete"),
            (2000, "movie", "movies-a"),
            (1000, "live", "live-a"),
            (2001, "movie", "movies-b"),
            (1001, "live", "live-b"),
            (3000, "series", "shows-a"),
            (3001, "series", "shows-b"),
        ] {
            writeln!(body, "#EXTGRP:{prefix}-{group}\n#EXTINF:-1 tvg-type=\"{kind}\" group-title=\"{prefix}-{group}\",Item {id} S01E01\nhttp://stream.example/{kind}/{id}").unwrap();
        }
        tokio::fs::write(&input.url, body).await.unwrap();
        let (groups, errors) =
            tuliprox_iptv::m3u::download_m3u_playlist(&ctx.config, &ctx.client, &ctx.config.config.load(), input).await;
        assert!(errors.is_empty());
        assert_eq!(
            group_names(&groups),
            ["obsolete", "movies-a", "live-a", "movies-b", "live-b", "shows-a", "shows-b"]
                .map(|name| format!("{prefix}-{name}"))
        );
        tuliprox_iptv::provider::PlaylistFetch::groups(groups)
    }

    fn group_names(groups: &[PlaylistGroup]) -> Vec<String> {
        groups.iter().map(|group| group.title.to_string()).collect()
    }

    #[tokio::test]
    async fn m3u_update_quality_preserves_candidate_group_order_when_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), None, [0, 0, 0]).await;
        let candidate = order_candidate(&ctx, &input, "candidate").await;
        let effective = super::super::super::m3u_quality::effective_m3u_fetch(
            &ctx.config,
            &input,
            shared::model::UpdateQualityPolicy::Enforce,
            candidate,
        )
        .await;
        assert!(effective.errors.is_empty());
        assert!(effective.quality_acceptances.is_empty() && effective.quality_rejections.is_empty());
        assert_eq!(
            group_names(&effective.groups),
            ["movies-a", "live-a", "movies-b", "live-b", "shows-a", "shows-b"].map(|name| format!("candidate-{name}"))
        );
        assert_eq!(counts(&effective.groups), [2, 2, 2], "last-wins population remains canonical");
    }

    #[tokio::test]
    async fn m3u_update_quality_preserves_candidate_group_order_when_all_clusters_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([2, 2, 2]), [100, 100, 100]).await;
        let candidate = order_candidate(&ctx, &input, "candidate").await;
        let effective = super::super::super::m3u_quality::effective_m3u_fetch(
            &ctx.config,
            &input,
            shared::model::UpdateQualityPolicy::Enforce,
            candidate,
        )
        .await;
        assert!(effective.errors.is_empty() && effective.quality_rejections.is_empty());
        assert_eq!(effective.quality_acceptances.len(), 3);
        assert_eq!(
            group_names(&effective.groups),
            ["movies-a", "live-a", "movies-b", "live-b", "shows-a", "shows-b"].map(|name| format!("candidate-{name}"))
        );
    }

    #[tokio::test]
    async fn m3u_update_quality_mixed_fallback_preserves_relative_group_order() {
        for _ in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let (ctx, input) = fixture(temp.path(), None, [100, 100, 100]).await;
            let mut previous = order_candidate(&ctx, &input, "previous").await.groups;
            previous.retain(|group| group.title.as_ref() != "previous-obsolete");
            let (_, error) = tuliprox_repository::persist_input_playlist(&ctx.config, &input, previous).await;
            assert!(error.is_none());
            let mut previous = load_input_playlist(&ctx.config, &input, None).await.unwrap();
            let previous = previous.take_groups();
            assert_eq!(
                previous
                    .iter()
                    .filter(|group| group.xtream_cluster == XtreamCluster::Series)
                    .map(|group| group.title.to_string())
                    .collect::<Vec<_>>(),
                ["previous-shows-a", "previous-shows-b"]
            );
            let mut candidate = order_candidate(&ctx, &input, "candidate").await;
            candidate.groups.retain(|group| group.title.as_ref() != "candidate-shows-b");
            let effective = super::super::super::m3u_quality::effective_m3u_fetch(
                &ctx.config,
                &input,
                shared::model::UpdateQualityPolicy::Enforce,
                candidate,
            )
            .await;
            assert!(effective.errors.is_empty());
            assert_eq!(effective.quality_rejections.len(), 1);
            assert_eq!(effective.quality_rejections[0].cluster, XtreamCluster::Series);
            assert_eq!(
                group_names(&effective.groups),
                [
                    "candidate-movies-a",
                    "candidate-live-a",
                    "candidate-movies-b",
                    "candidate-live-b",
                    "previous-shows-a",
                    "previous-shows-b"
                ]
            );
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_retains_only_rejected_cluster() {
        for disk in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
            let mut config = (**ctx.config.config.load()).clone();
            config.disk_based_processing = disk;
            ctx.config.config.store(Arc::new(config));
            let mut result = run(&ctx, &input, [9, 11, 8]).await;
            assert_eq!(result.update_state(), PlaylistUpdateState::Partial);
            assert_eq!(result.quality_rejections.len(), 1);
            assert_eq!(result.quality_rejections[0].cluster, XtreamCluster::Series);
            verify_effective(&ctx, &input, &mut result, [9, 11, 10]).await;
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_zero_candidate_retains_previous_cluster() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
        let mut result = run(&ctx, &input, [10, 10, 0]).await;
        assert_eq!(result.quality_rejections[0].candidate_count, 0);
        verify_effective(&ctx, &input, &mut result, [10, 10, 10]).await;
    }

    #[tokio::test]
    async fn m3u_update_quality_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [0, 0, 0]).await;
        let mut result = run(&ctx, &input, [1, 1, 0]).await;
        assert!(result.quality_rejections.is_empty());
        assert!(result
            .input_telemetry
            .as_ref()
            .unwrap()
            .clusters
            .iter()
            .all(|c| c.quality.is_none() && c.threshold.is_none()));
        verify_effective(&ctx, &input, &mut result, [1, 1, 0]).await;
    }

    #[tokio::test]
    async fn m3u_update_quality_first_import() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), None, [90, 90, 90]).await;
        let mut result = run(&ctx, &input, [1, 1, 1]).await;
        assert!(result.quality_rejections.is_empty());
        assert!(result.input_telemetry.as_ref().unwrap().clusters.iter().all(|c| c.quality.is_none()
            && c.baseline_count.is_none()
            && c.decision == Some(PlaylistUpdateClusterDecision::Accepted)));
        verify_effective(&ctx, &input, &mut result, [1, 1, 1]).await;
    }

    #[tokio::test]
    async fn m3u_update_quality_download_or_parse_failure() {
        for body in [None, Some("<html>provider unavailable</html>"), Some("#EXTM3U\n#EXTINF:-1,Truncated\n")] {
            let temp = tempfile::tempdir().unwrap();
            let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
            if let Some(body) = body {
                tokio::fs::write(&input.url, body).await.unwrap();
            } else {
                tokio::fs::remove_file(&input.url).await.unwrap();
            }
            let mut result = download_input(&ctx, &input, false).await;
            assert!(!result.errors.is_empty());
            assert!(result.quality_rejections.is_empty());
            assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
            let mut saved = load_input_playlist(&ctx.config, &input, None).await.unwrap();
            assert_eq!(counts(&saved.take_groups()), [10, 10, 10]);
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_empty_bootstrap_and_structural_empty_use_existing_domain() {
        for previous in [None, Some([10, 10, 0])] {
            let temp = tempfile::tempdir().unwrap();
            let (ctx, input) = fixture(temp.path(), previous, [90, 90, 90]).await;
            let mut result = run(&ctx, &input, if previous.is_some() { [10, 10, 0] } else { [0, 0, 0] }).await;
            if previous.is_none() {
                assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
                assert_eq!(result.quality_rejections.len(), 3);
            } else {
                assert_eq!(result.update_state(), PlaylistUpdateState::Partial);
                assert_eq!(result.quality_rejections.len(), 1);
                assert_eq!(result.quality_rejections[0].current_count, 0);
                assert_eq!(result.quality_rejections[0].cluster, XtreamCluster::Series);
            }
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_skip_preserves_previous_and_reuse_emits_no_new_facts() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, mut input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
        Arc::make_mut(&mut input).options.as_mut().unwrap().flags.set(ConfigInputFlags::SkipSeries);
        let mut result = run(&ctx, &input, [10, 10, 0]).await;
        assert_eq!(result.update_state(), PlaylistUpdateState::Success);
        assert!(result.quality_rejections.is_empty());
        let mut saved = load_input_playlist(&ctx.config, &input, None).await.unwrap();
        assert_eq!(counts(&result.source.take_groups()), [10, 10, 0]);
        assert_eq!(counts(&saved.take_groups()), [10, 10, 10]);
        let telemetry = result.input_telemetry.unwrap();
        assert!(!telemetry.clusters.iter().find(|c| c.cluster == XtreamCluster::Series).unwrap().requested);
        tokio::fs::remove_file(&input.url).await.unwrap();
        let reused = download_input(&ctx, &input, false).await;
        assert!(reused.errors.is_empty());
        assert!(reused.input_telemetry.is_none());
    }

    #[tokio::test]
    async fn m3u_update_quality_force_bypasses_guard_and_cache_without_inventing_quality() {
        for threshold in [0, 95] {
            for policy in [InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE] {
                let temp = tempfile::tempdir().unwrap();
                let (mut ctx, mut input) = fixture(temp.path(), Some([10, 10, 10]), [threshold; 3]).await;
                Arc::make_mut(&mut input).cache_duration_seconds = 3_600;
                let storage = input_storage_path(&ctx, &input).await;
                let mut status = input_cache::InputStatus::default();
                input_cache::update_cluster_status(&mut status, "default", input_cache::ClusterState::Ok);
                input_cache::save_input_status(&storage, &status);
                set_refresh_policy(&mut ctx, &input, policy);
                let mut result = run(&ctx, &input, [1, 1, 1]).await;
                let rejected = threshold > 0 && policy == InputRefreshPolicy::REFRESH;
                assert_eq!(
                    result.update_state(),
                    if rejected { PlaylistUpdateState::Partial } else { PlaylistUpdateState::Success }
                );
                assert_eq!(counts(&result.source.take_groups()), if rejected { [10; 3] } else { [1; 3] });
                let telemetry = result.input_telemetry.as_ref().unwrap();
                assert_eq!(telemetry.refresh_policy, policy);
                assert!(telemetry
                    .clusters
                    .iter()
                    .all(|cluster| cluster.source == Some(PlaylistUpdateDataSource::Provider)));
                let saved = input_cache::load_input_status(&storage);
                for cluster in &telemetry.clusters {
                    if policy == InputRefreshPolicy::FORCE {
                        assert_eq!(cluster.decision, Some(PlaylistUpdateClusterDecision::Accepted));
                        assert_eq!((cluster.baseline_count, cluster.quality), (None, None));
                        assert_eq!((cluster.candidate_count, cluster.active_count), (Some(1), Some(1)));
                        let snapshot = saved.clusters[cluster.cluster.as_ref()].last_update.unwrap();
                        assert_eq!(snapshot.quality, None);
                        assert_eq!(snapshot.quality_guard_threshold, Some(threshold));
                        assert_eq!(snapshot.policy, Some(policy));
                    }
                }
                let reused = download_input(&ctx, &input, false).await;
                assert!(reused.input_telemetry.is_none(), "in-run reuse must not create another Force acquisition");
            }
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_force_keeps_parse_and_empty_publication_errors() {
        for body in ["#EXTM3U\n", "#EXTM3U\n#EXTINF:-1,Truncated\n"] {
            let temp = tempfile::tempdir().unwrap();
            let (mut ctx, input) = fixture(temp.path(), Some([10; 3]), [95; 3]).await;
            set_refresh_policy(&mut ctx, &input, InputRefreshPolicy::FORCE);
            tokio::fs::write(&input.url, body).await.unwrap();
            let mut result = download_input(&ctx, &input, false).await;
            assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
            assert!(result.quality_rejections.is_empty());
            let mut saved = load_input_playlist(&ctx.config, &input, None).await.unwrap();
            assert_eq!(counts(&saved.take_groups()), [10; 3]);
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_target_processing_uses_retained_and_accepted_groups() {
        for policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::FORCE] {
            let temp = tempfile::tempdir().unwrap();
            let (mut ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
            set_refresh_policy(&mut ctx, &input, policy);
            tokio::fs::write(&input.url, document("candidate", [10, 0, 10])).await.unwrap();
            let output = temp.path().join("client.m3u");
            let mut target = ConfigTarget::from(&ConfigTargetDto {
                id: 1,
                name: "m3u-target".into(),
                enabled: true,
                ..ConfigTargetDto::default()
            });
            target.filter = get_filter(r#"Name ~ ".*""#, None).unwrap().into();
            target.output = vec![TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
                filename: Some(output.to_string_lossy().into_owned()),
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: None,
            })];
            ctx.config.sources.store(Arc::new(SourcesConfig {
                inputs: vec![input.clone()],
                sources: vec![ConfigSource { inputs: vec![input.name.clone()], targets: vec![Arc::new(target)] }],
                ..SourcesConfig::default()
            }));
            let (_, targets, errors) = process_source(0, Arc::new(ctx)).await;
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(targets.len(), 1);
            let published = tokio::fs::read_to_string(output).await.unwrap();
            assert!(published.contains("/candidate/live/") && published.contains("/candidate/series/"), "{published}");
            assert_eq!(published.contains("/previous/video/"), policy == InputRefreshPolicy::NORMAL, "{published}");
            assert!(!published.contains("/previous/live/") && !published.contains("/candidate/video/"));
        }
    }

    #[tokio::test]
    async fn m3u_update_quality_raw_group_catalog_uses_effective_state() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([10, 10, 10]), [90, 90, 90]).await;
        let mut result = run(&ctx, &input, [10, 10, 0]).await;
        let storage = input_cache::resolve_input_storage_path(&ctx.config.config.load().storage_dir, &input.name).await;
        let status = input_cache::load_input_status(&storage);
        for cluster in XTREAM_CLUSTER {
            let saved = &status.clusters[cluster.as_ref()];
            let rejected = cluster == XtreamCluster::Series;
            assert_eq!(
                saved.status,
                if rejected { input_cache::ClusterState::Failed } else { input_cache::ClusterState::Ok }
            );
            let snapshot = saved.last_update.unwrap();
            assert_eq!(
                snapshot.quality.unwrap().decision,
                if rejected {
                    PersistedPlaylistUpdateQualityDecision::Rejected
                } else {
                    PersistedPlaylistUpdateQualityDecision::Accepted
                }
            );
            assert_eq!(snapshot.active_count, Some(10));
        }
        assert_eq!(status.clusters["default"].status, input_cache::ClusterState::Failed);
        let series: RawInputGroupCatalog = serde_json::from_slice(
            &tokio::fs::read(raw_group_catalog_path(&storage, XtreamCluster::Series)).await.unwrap(),
        )
        .unwrap();
        assert_eq!(series.groups, ["previous-series"]);
        verify_effective(&ctx, &input, &mut result, [10, 10, 10]).await;
    }

    #[tokio::test]
    async fn m3u_update_quality_duplicate_keys_match_the_existing_persisted_population() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([1, 1, 0]), [100, 100, 0]).await;
        let candidate = document("superseded", [1, 1, 0]) + &document("last", [1, 1, 0]);
        tokio::fs::write(&input.url, candidate).await.unwrap();
        let mut result = download_input(&ctx, &input, false).await;
        assert_eq!(result.update_state(), PlaylistUpdateState::Success);
        assert!(result.source.take_groups().iter().all(|group| group.title.starts_with("last")));
        let mut saved = load_input_playlist(&ctx.config, &input, None).await.unwrap();
        assert_eq!(counts(&saved.take_groups()), [1, 1, 0]);
    }

    #[tokio::test]
    async fn m3u_update_quality_conflicting_retained_key_cannot_diverge_from_storage() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, input) = fixture(temp.path(), Some([1, 1, 0]), [90, 0, 0]).await;
        let candidate = document("candidate", [0, 1, 0]).replace("2000", "1000");
        tokio::fs::write(&input.url, candidate).await.unwrap();
        let mut result = download_input(&ctx, &input, false).await;
        assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
        assert!(!result.errors.is_empty());
        let mut saved = load_input_playlist(&ctx.config, &input, None).await.unwrap();
        assert_eq!(counts(&saved.take_groups()), [1, 1, 0]);
        let storage = input_storage_path(&ctx, &input).await;
        let catalog: RawInputGroupCatalog = serde_json::from_slice(
            &tokio::fs::read(raw_group_catalog_path(&storage, XtreamCluster::Live)).await.unwrap(),
        )
        .unwrap();
        assert_eq!(catalog.groups, ["previous-live"]);
    }
}
