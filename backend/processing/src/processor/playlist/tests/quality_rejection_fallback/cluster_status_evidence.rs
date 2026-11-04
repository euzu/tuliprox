use super::*;
use shared::model::{PersistedPlaylistUpdateTechnicalState as Technical, UpdateQualityPolicy};

#[tokio::test]
async fn pipeline_transparency_real_disk_publication_is_guard_independent_and_cluster_scoped() {
    for quality in [None, Some([0, 0, 0]), Some([90, 0, 0]), Some([90, 90, 90]), Some([100, 100, 100])] {
        for failure in [None, Some(XtreamCluster::Series), Some(XtreamCluster::Video)] {
            for policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE] {
                let dir = tempfile::tempdir().unwrap();
                let ctx = processing_context(dir.path());
                let mut input = ConfigInput {
                    id: 17,
                    name: "publication-test".intern(),
                    input_type: InputType::Xtream,
                    enabled: true,
                    url: "http://fixture.invalid".into(),
                    username: Some("fixture".into()),
                    password: None,
                    ..Default::default()
                };
                input.options = quality.map(|thresholds| {
                    ConfigInputOptions::from(&ConfigInputOptionsDto {
                        update_quality: ConfigInputUpdateQualityDto {
                            live: thresholds[0],
                            vod: thresholds[1],
                            series: thresholds[2],
                        },
                        ..Default::default()
                    })
                });
                let requested = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
                let (event, saved) =
                    crate::processor::publication_fixture_status(&ctx.config, &input, &requested, failure, policy)
                        .await;
                let telemetry = event.input_telemetry.as_ref().unwrap();
                for cluster in &requested {
                    let state = telemetry.clusters.iter().find(|c| c.cluster == *cluster).unwrap();
                    let index = requested.iter().position(|c| c == cluster).unwrap();
                    let failed_index = failure.and_then(|c| requested.iter().position(|x| *x == c));
                    let expected = match failed_index {
                        Some(i) if index > i => None,
                        Some(i) if index == i => Some(Technical::Failed),
                        _ => Some(Technical::Succeeded),
                    };
                    assert_eq!(state.technical_state, expected, "{quality:?} {failure:?} {policy:?} {cluster:?}");
                    let snapshot = saved.clusters.get(cluster.as_ref()).and_then(|c| c.last_update);
                    if let Some(expected) = expected {
                        assert_eq!(snapshot.unwrap().technical_state, Some(expected));
                        if quality.is_none() || quality == Some([0, 0, 0]) {
                            assert_eq!(snapshot.unwrap().quality, None);
                            assert_eq!(snapshot.unwrap().quality_guard_threshold, Some(0));
                        }
                        if policy.quality == UpdateQualityPolicy::Bypass {
                            assert_eq!(snapshot.unwrap().quality, None);
                        }
                    } else {
                        assert!(snapshot.is_none(), "unpublished later cluster has no completed snapshot");
                    }
                }
                assert_eq!(event.run_id.as_ref().unwrap().as_ref(), "publication-fixture-R2");
                assert_eq!(event.execution_order, Some(42.into()));
            }
        }
    }
}

#[tokio::test]
async fn pipeline_transparency_real_publication_subset_does_not_depend_on_array_position() {
    for requested in [
        vec![XtreamCluster::Series],
        vec![XtreamCluster::Video, XtreamCluster::Series],
        vec![XtreamCluster::Series, XtreamCluster::Live],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = processing_context(dir.path());
        let input = ConfigInput {
            id: 17,
            name: "subset".intern(),
            input_type: InputType::Xtream,
            enabled: true,
            options: Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
                skip_live: !requested.contains(&XtreamCluster::Live),
                skip_vod: !requested.contains(&XtreamCluster::Video),
                skip_series: !requested.contains(&XtreamCluster::Series),
                ..Default::default()
            })),
            ..Default::default()
        };
        let (event, saved) = crate::processor::publication_fixture_status(
            &ctx.config,
            &input,
            &requested,
            None,
            InputRefreshPolicy::NORMAL,
        )
        .await;
        for fact in event.input_telemetry.unwrap().clusters {
            if requested.contains(&fact.cluster) {
                assert_eq!(fact.technical_state, Some(Technical::Succeeded));
                assert!(saved.clusters.contains_key(fact.cluster.as_ref()));
            } else {
                assert_eq!(fact.source, None);
                assert!(!fact.requested);
                assert_eq!(fact.technical_state, None);
            }
        }
    }
}

#[tokio::test]
async fn pipeline_transparency_document_providers_keep_technical_status_without_guards() {
    for input_type in [InputType::M3u, InputType::Stalker] {
        for configuration in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let ctx = processing_context(dir.path());
            let responses=HashMap::from([
                ("handshake".into(),r#"{"js":{"token":"fixture-session"}}"#.into()),
                ("get_profile".into(),r#"{"js":{}}"#.into()),("get_capabilities".into(),r#"{"js":{}}"#.into()),
                ("get_genres".into(),r#"{"js":[{"id":"1","title":"Live"}]}"#.into()),
                ("get_categories".into(),r#"{"js":[]}"#.into()),
                ("get_all_channels".into(),r#"{"js":{"data":[{"id":"1","name":"Channel","cmd":"ffmpeg http://stream.example/live","tv_genre_id":"1"}]}}"#.into()),
                ("get_ordered_list".into(),r#"{"js":{"data":[],"total_items":0,"max_page_items":10}}"#.into()),
                ("get_epg_info".into(),r#"{"js":{}}"#.into())]);
            let server = TestXtreamServer::start_with_responses(responses);
            let mut input = ConfigInput {
                id: 17,
                name: "document-fixture".intern(),
                input_type,
                enabled: true,
                cache_duration_seconds: 0,
                ..Default::default()
            };
            let thresholds = match configuration {
                3 => [90, 0, 0],
                4 => [90, 90, 90],
                _ => [0, 0, 0],
            };
            input.options = if configuration == 0 {
                None
            } else {
                Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
                    update_quality: ConfigInputUpdateQualityDto {
                        live: thresholds[0],
                        vod: thresholds[1],
                        series: thresholds[2],
                    },
                    ..Default::default()
                }))
            };
            if configuration == 1 {
                let options: ConfigInputOptionsDto = serde_json::from_value(serde_json::json!({})).unwrap();
                input.options = Some(ConfigInputOptions::from(&options));
            }
            if input_type == InputType::M3u {
                input.url = dir.path().join("candidate.m3u").to_string_lossy().into_owned();
                tokio::fs::write(&input.url, super::m3u_update_quality::document("candidate", [2, 2, 2]))
                    .await
                    .unwrap();
            } else {
                input.url = server.base_url.clone();
                input.stalker = Some(tuliprox_core::model::StalkerInputConfig {
                    device: Some(tuliprox_core::model::StalkerDeviceProfile {
                        mac_address: Some("00:1a:79:aa:bb:cc".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            let result = download_input(&ctx, &Arc::new(input.clone()), false).await;
            assert!(result.errors.is_empty(), "{input_type:?} K{configuration}: {:?}", result.errors);
            assert!(result.storage_error.is_none(), "{input_type:?}: {:?}", result.storage_error);
            let telemetry = result.input_telemetry.unwrap();
            let path = input_storage_path(&ctx, &input).await;
            let saved = input_cache::load_input_status(&path);
            for cluster in telemetry.clusters {
                assert_eq!(
                    cluster.technical_state,
                    Some(Technical::Succeeded),
                    "{input_type:?} K{configuration} {:?}",
                    cluster.cluster
                );
                let snapshot = saved.clusters[cluster.cluster.as_ref()].last_update.unwrap();
                assert_eq!(snapshot.technical_state, cluster.technical_state);
                if configuration <= 2 {
                    assert!(snapshot.quality.is_none());
                    assert_eq!(snapshot.quality_guard_threshold, Some(0));
                }
            }
            server.finish();
        }
    }
}
