use super::{
    behavior::{
        active_category_bytes, active_cluster_count, active_cluster_snapshot, assert_no_refresh_artifacts,
        cluster_fixture_readers, disk_test_input, fail_detail_preservation_after_quality, json_reader,
        live_fixture_stream, publish_live_fixture_rows, publish_test_cluster, read_series_props,
    },
    fixed_refresh_paths, get_collection_path, persist_input_xtream_playlist_cluster_to_disk,
    persist_input_xtream_playlist_clusters_to_disk, persist_input_xtream_playlist_clusters_to_disk_with_operations,
    persists_input_series_info, test_app_config, xtream_cluster_category_collection, XtreamClusterPublishOutcome,
    XtreamClusterQualityPolicy, XtreamClusterRefreshRequest, XtreamClusterStageOperations,
};
use crate::{build_input_storage_path, get_input_storage_path, BPlusTreeQuery};
use shared::{
    model::{
        InputType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties,
        XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::fs;
use tempfile::tempdir;
use tuliprox_core::model::{
    ClusterForceUpdate, ClusterUpdateAcceptance, ClusterUpdateRejection, ConfigInput, UpdateQualityDecision,
};

#[tokio::test]
async fn empty_disk_refresh_retains_published_playlist_and_catalog() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let app_config = test_app_config(directory.path());
    let input = ConfigInput { name: "empty-guard".intern(), input_type: InputType::Xtream, ..Default::default() };

    persist_input_xtream_playlist_cluster_to_disk(
        &app_config,
        &input,
        XtreamCluster::Live,
        0,
        json_reader(r#"[{"category_id":"1","category_name":"News"}]"#),
        json_reader(r#"[{"name":"Channel","stream_id":7,"category_id":"1","added":"0"}]"#),
    )
    .await?;

    let storage = build_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref());
    let database = super::super::xtream_get_file_path(&storage, XtreamCluster::Live);
    let catalog = crate::raw_group_catalog_path(&storage, XtreamCluster::Live);
    let database_before = tokio::fs::read(&database).await?;
    let catalog_before = tokio::fs::read(&catalog).await?;

    let result = persist_input_xtream_playlist_cluster_to_disk(
        &app_config,
        &input,
        XtreamCluster::Live,
        0,
        json_reader("[]"),
        json_reader("[]"),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(tokio::fs::read(database).await?, database_before);
    assert_eq!(tokio::fs::read(catalog).await?, catalog_before);
    Ok(())
}

#[tokio::test]
async fn empty_cluster_is_published_when_another_cluster_still_has_items() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let app_config = test_app_config(directory.path());
    let input = ConfigInput { name: "empty-cluster".intern(), input_type: InputType::Xtream, ..Default::default() };

    for (cluster, category, stream) in [
        (
            XtreamCluster::Live,
            r#"[{"category_id":"1","category_name":"News"}]"#,
            r#"[{"name":"Channel","stream_id":7,"category_id":"1","added":"0"}]"#,
        ),
        (
            XtreamCluster::Video,
            r#"[{"category_id":"2","category_name":"Movies"}]"#,
            r#"[{"name":"Movie","stream_id":8,"category_id":"2","added":"0"}]"#,
        ),
    ] {
        persist_input_xtream_playlist_cluster_to_disk(
            &app_config,
            &input,
            cluster,
            0,
            json_reader(category),
            json_reader(stream),
        )
        .await?;
    }

    persist_input_xtream_playlist_cluster_to_disk(
        &app_config,
        &input,
        XtreamCluster::Live,
        0,
        json_reader("[]"),
        json_reader("[]"),
    )
    .await?;

    let storage = build_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref());
    let playlist =
        super::super::load_input_xtream_playlist(&app_config, &storage, &[XtreamCluster::Live, XtreamCluster::Video])
            .await?;
    assert!(playlist.iter().all(|group| group.xtream_cluster != XtreamCluster::Live));
    assert!(playlist.iter().any(|group| group.xtream_cluster == XtreamCluster::Video));
    Ok(())
}

#[test]
fn disk_quality_disabled_does_not_read_staging_or_baseline() {
    let directory = tempdir().expect("temp directory");
    let paths = fixed_refresh_paths(directory.path(), 18);

    let decision = super::super::evaluate_staged_xtream_cluster_quality(&paths, XtreamCluster::Live, 0)
        .expect("disabled quality evaluation should not access missing files");

    assert_eq!(decision, UpdateQualityDecision::Disabled);
}

#[tokio::test]
async fn disk_force_publishes_an_empty_cluster_and_cleans_staging() {
    let directory = tempdir().expect("temporary directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("force-empty");
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Video, 0, "old-vod", 3, 2_000).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path =
        get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref()).await.expect("force storage");
    let before = active_cluster_snapshot(&storage_path, XtreamCluster::Video);

    let result = persist_input_xtream_playlist_clusters_to_disk(
        &app_config,
        &input,
        vec![XtreamClusterRefreshRequest {
            cluster: XtreamCluster::Video,
            quality: XtreamClusterQualityPolicy::Bypass { configured_threshold: 95 },
            categories: json_reader("[]"),
            streams: json_reader("[]"),
        }],
    )
    .await;

    assert!(result.errors.is_empty());
    assert_eq!(
        result.outcomes,
        vec![XtreamClusterPublishOutcome::ForcePublished(ClusterForceUpdate {
            cluster: XtreamCluster::Video,
            candidate_count: 0,
            configured_threshold: 95,
        })]
    );
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Video), 0);
    assert_ne!(active_cluster_snapshot(&storage_path, XtreamCluster::Video), before);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&active_category_bytes(&storage_path, XtreamCluster::Video,))
            .expect("empty category JSON"),
        serde_json::json!([])
    );
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn disk_force_keeps_active_files_after_a_technical_staging_failure() {
    let directory = tempdir().expect("temporary directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("force-technical-error");
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 3, 1_000).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path =
        get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref()).await.expect("force storage");
    let before = active_cluster_snapshot(&storage_path, XtreamCluster::Live);

    let result = persist_input_xtream_playlist_clusters_to_disk(
        &app_config,
        &input,
        vec![XtreamClusterRefreshRequest {
            cluster: XtreamCluster::Live,
            quality: XtreamClusterQualityPolicy::Bypass { configured_threshold: 100 },
            categories: json_reader(r#"[{"category_id":"1","category_name":"new-live"}]"#),
            streams: json_reader("{"),
        }],
    )
    .await;

    assert_eq!(result.errors.len(), 1);
    assert!(result.outcomes.is_empty());
    assert!(result.force_updates.is_empty());
    assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Live), before);
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn pipeline_transparency_disk_quality_survives_post_evaluation_staging_failure() {
    let directory = tempdir().expect("temporary directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("accepted-then-detail-error");
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 100, 1_000).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path =
        get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref()).await.expect("storage path");
    let before = active_cluster_snapshot(&storage_path, XtreamCluster::Live);
    let (categories, streams) = cluster_fixture_readers(XtreamCluster::Live, "new-live", 95, 10_000);

    let result = persist_input_xtream_playlist_clusters_to_disk_with_operations(
        &app_config,
        &input,
        vec![XtreamClusterRefreshRequest {
            cluster: XtreamCluster::Live,
            quality: XtreamClusterQualityPolicy::Enforce { threshold: 90 },
            categories,
            streams,
        }],
        XtreamClusterStageOperations { preserve_details: fail_detail_preservation_after_quality },
    )
    .await;

    assert_eq!(
        result.quality_acceptances,
        vec![ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: Some(100),
            candidate_count: 95,
            threshold: 90,
            quality: Some(95),
        }]
    );
    assert!(result.outcomes.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.failed_cluster, Some(XtreamCluster::Live));
    assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Live), before);
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn pipeline_transparency_disk_batch_keeps_prior_publish_and_all_evaluated_quality_on_later_error() {
    let directory = tempdir().expect("temporary directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("later-cluster-publish-error");
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 100, 1_000).await,
        XtreamClusterPublishOutcome::Published
    );
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Video, 0, "old-vod", 100, 2_000).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path =
        get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref()).await.expect("storage path");
    let blocked_category_path =
        get_collection_path(&storage_path, xtream_cluster_category_collection(XtreamCluster::Video));
    fs::remove_file(&blocked_category_path).expect("replace published VOD categories with a directory");
    fs::create_dir(&blocked_category_path).expect("create category publication blocker");
    let (live_categories, live_streams) = cluster_fixture_readers(XtreamCluster::Live, "new-live", 90, 10_000);
    let (vod_categories, vod_streams) = cluster_fixture_readers(XtreamCluster::Video, "new-vod", 90, 20_000);

    let result = persist_input_xtream_playlist_clusters_to_disk(
        &app_config,
        &input,
        vec![
            XtreamClusterRefreshRequest {
                cluster: XtreamCluster::Live,
                quality: XtreamClusterQualityPolicy::Enforce { threshold: 90 },
                categories: live_categories,
                streams: live_streams,
            },
            XtreamClusterRefreshRequest {
                cluster: XtreamCluster::Video,
                quality: XtreamClusterQualityPolicy::Enforce { threshold: 90 },
                categories: vod_categories,
                streams: vod_streams,
            },
        ],
    )
    .await;

    assert_eq!(result.quality_acceptances.len(), 2);
    assert_eq!(result.quality_acceptances[0].cluster, XtreamCluster::Live);
    assert_eq!(result.quality_acceptances[1].cluster, XtreamCluster::Video);
    assert_eq!(result.outcomes, vec![XtreamClusterPublishOutcome::QualityAccepted(result.quality_acceptances[0])]);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.failed_cluster, Some(XtreamCluster::Video));
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Live), 90);
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn disk_quality_bootstrap_publishes_nonempty_candidate_and_rejects_empty_without_baseline() {
    let populated_directory = tempdir().expect("populated bootstrap directory");
    let populated_config = test_app_config(populated_directory.path());
    let populated_input = disk_test_input("bootstrap-populated");

    let populated_outcome =
        publish_test_cluster(&populated_config, &populated_input, XtreamCluster::Live, 90, "bootstrap-live", 3, 1_000)
            .await;
    let populated_storage =
        get_input_storage_path(&populated_input.name, populated_directory.path().to_string_lossy().as_ref())
            .await
            .expect("populated bootstrap storage");

    assert_eq!(
        populated_outcome,
        XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: None,
            candidate_count: 3,
            threshold: 90,
            quality: None,
        })
    );
    assert_eq!(active_cluster_count(&populated_storage, XtreamCluster::Live), 3);
    assert!(String::from_utf8_lossy(&active_category_bytes(&populated_storage, XtreamCluster::Live))
        .contains("bootstrap-live"));
    assert_no_refresh_artifacts(&populated_storage);

    let empty_directory = tempdir().expect("empty bootstrap directory");
    let empty_config = test_app_config(empty_directory.path());
    let empty_input = disk_test_input("bootstrap-empty");
    let empty_outcome =
        publish_test_cluster(&empty_config, &empty_input, XtreamCluster::Video, 90, "empty-vod", 0, 2_000).await;
    let empty_storage = get_input_storage_path(&empty_input.name, empty_directory.path().to_string_lossy().as_ref())
        .await
        .expect("empty bootstrap storage");

    assert_eq!(
        empty_outcome,
        XtreamClusterPublishOutcome::RetainedPrevious(ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 0,
            candidate_count: 0,
            threshold: 90,
            quality: 0,
        })
    );
    assert!(!super::super::xtream_get_file_path(&empty_storage, XtreamCluster::Video).exists());
    assert!(!get_collection_path(&empty_storage, xtream_cluster_category_collection(XtreamCluster::Video)).exists());
    assert_no_refresh_artifacts(&empty_storage);
}

#[tokio::test]
async fn disk_quality_enforces_exact_90_percent_boundaries_and_retains_rejected_files() {
    for (name, candidate_count, accepted) in [
        ("lower-boundary", 90, true),
        ("below-lower-boundary", 89, false),
        ("upper-boundary", 110, true),
        ("above-upper-boundary", 111, false),
    ] {
        let directory = tempdir().expect("boundary directory");
        let app_config = test_app_config(directory.path());
        let input = disk_test_input(name);
        assert_eq!(
            publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 100, 10_000,).await,
            XtreamClusterPublishOutcome::Published,
            "baseline publish failed for {name}"
        );
        let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
            .await
            .expect("boundary storage");
        let before = active_cluster_snapshot(&storage_path, XtreamCluster::Live);

        let outcome =
            publish_test_cluster(&app_config, &input, XtreamCluster::Live, 90, "new-live", candidate_count, 20_000)
                .await;

        if accepted {
            assert_eq!(
                outcome,
                XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
                    cluster: XtreamCluster::Live,
                    current_count: Some(100),
                    candidate_count,
                    threshold: 90,
                    quality: Some(90),
                }),
                "case: {name}"
            );
            assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Live), candidate_count);
            assert!(String::from_utf8_lossy(&active_category_bytes(&storage_path, XtreamCluster::Live))
                .contains("new-live"));
        } else {
            assert_eq!(
                outcome,
                XtreamClusterPublishOutcome::RetainedPrevious(ClusterUpdateRejection {
                    cluster: XtreamCluster::Live,
                    current_count: 100,
                    candidate_count,
                    threshold: 90,
                    quality: 89,
                }),
                "case: {name}"
            );
            assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Live), before, "case: {name}");
        }
        assert_no_refresh_artifacts(&storage_path);
    }
}

#[tokio::test]
async fn disk_quality_rejects_duplicate_rows_that_represent_only_half_the_provider_ids() {
    let directory = tempdir().expect("duplicate rejection directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("duplicate-rejection");
    let first_provider_id = 1_000_u32;
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 100, first_provider_id,).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
        .await
        .expect("duplicate rejection storage");
    let before = active_cluster_snapshot(&storage_path, XtreamCluster::Live);
    let streams = (0..50_u32)
        .flat_map(|offset| {
            let provider_id = first_provider_id + offset;
            [
                live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}")),
                live_fixture_stream(provider_id, 1, &format!("duplicate-{provider_id}")),
            ]
        })
        .collect();

    let outcome = publish_live_fixture_rows(
        &app_config,
        &input,
        100,
        serde_json::json!([{"category_id": 1, "category_name": "candidate-live"}]),
        streams,
    )
    .await;

    assert_eq!(
        outcome,
        XtreamClusterPublishOutcome::RetainedPrevious(ClusterUpdateRejection {
            cluster: XtreamCluster::Live,
            current_count: 100,
            candidate_count: 50,
            threshold: 100,
            quality: 50,
        })
    );
    assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Live), before);
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn pipeline_transparency_disk_quality_preserves_accepted_publish_facts() {
    let directory = tempdir().expect("duplicate acceptance directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("duplicate-acceptance");
    let first_provider_id = 1_000_u32;
    let winning_provider_id = first_provider_id + 42;
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Live, 0, "old-live", 100, first_provider_id,).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
        .await
        .expect("duplicate acceptance storage");
    let mut streams = (0..100_u32)
        .map(|offset| {
            let provider_id = first_provider_id + offset;
            live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}"))
        })
        .collect::<Vec<_>>();
    streams.push(live_fixture_stream(winning_provider_id, 2, "winning-duplicate"));

    let outcome = publish_live_fixture_rows(
        &app_config,
        &input,
        100,
        serde_json::json!([
            {"category_id": 1, "category_name": "candidate-live"},
            {"category_id": 2, "category_name": "winning-category"}
        ]),
        streams,
    )
    .await;

    assert_eq!(
        outcome,
        XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: Some(100),
            candidate_count: 100,
            threshold: 100,
            quality: Some(100),
        })
    );
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Live), 100);
    let active_path = super::super::xtream_get_file_path(&storage_path, XtreamCluster::Live);
    let mut query =
        BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&active_path).expect("accepted Live cluster should open");
    let winner = query
        .query_zero_copy(&winning_provider_id)
        .expect("winner lookup should succeed")
        .expect("winner should exist");
    assert_eq!(winner.name.as_ref(), "winning-duplicate");
    assert_eq!(winner.category_id, 2);
    assert!(String::from_utf8_lossy(&active_category_bytes(&storage_path, XtreamCluster::Live))
        .contains("winning-category"));
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn disk_series_quality_compares_catalog_rows_and_ignores_embedded_episode_details() {
    let directory = tempdir().expect("series population directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("series-logical-population");
    let provider_id = 30_000;
    assert_eq!(
        publish_test_cluster(&app_config, &input, XtreamCluster::Series, 0, "old-series", 1, provider_id,).await,
        XtreamClusterPublishOutcome::Published
    );
    let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
        .await
        .expect("series population storage");
    let episodes = (0..200_u32)
        .map(|id| SeriesStreamDetailEpisodeProperties { id, ..SeriesStreamDetailEpisodeProperties::default() })
        .collect();
    let enriched = SeriesStreamProperties {
        series_id: provider_id,
        details: Some(SeriesStreamDetailProperties::new(None, Vec::new(), Some(episodes))),
        ..SeriesStreamProperties::default()
    };
    persists_input_series_info(&app_config, &storage_path, XtreamCluster::Series, &input.name, provider_id, &enriched)
        .await
        .expect("series enrichment should persist");
    let active_path = super::super::xtream_get_file_path(&storage_path, XtreamCluster::Series);
    assert_eq!(
        read_series_props(&active_path, provider_id)
            .details
            .and_then(|details| details.episodes)
            .map(|episodes| episodes.len()),
        Some(200)
    );

    let equal_catalog =
        publish_test_cluster(&app_config, &input, XtreamCluster::Series, 100, "equal-series", 1, provider_id).await;

    assert_eq!(
        equal_catalog,
        XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
            cluster: XtreamCluster::Series,
            current_count: Some(1),
            candidate_count: 1,
            threshold: 100,
            quality: Some(100),
        })
    );
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Series), 1);
    let accepted_snapshot = active_cluster_snapshot(&storage_path, XtreamCluster::Series);

    let different_catalog =
        publish_test_cluster(&app_config, &input, XtreamCluster::Series, 100, "different-series", 2, 40_000).await;

    assert_eq!(
        different_catalog,
        XtreamClusterPublishOutcome::RetainedPrevious(ClusterUpdateRejection {
            cluster: XtreamCluster::Series,
            current_count: 1,
            candidate_count: 2,
            threshold: 100,
            quality: 0,
        })
    );
    assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Series), accepted_snapshot);
    assert_no_refresh_artifacts(&storage_path);
}

#[tokio::test]
async fn disk_quality_publishes_accepted_clusters_and_retains_rejected_cluster_independently() {
    let directory = tempdir().expect("mixed cluster directory");
    let app_config = test_app_config(directory.path());
    let input = disk_test_input("mixed-clusters");

    for (cluster, category_name, first_provider_id) in [
        (XtreamCluster::Live, "old-live", 10_000),
        (XtreamCluster::Video, "old-vod", 20_000),
        (XtreamCluster::Series, "old-series", 30_000),
    ] {
        assert_eq!(
            publish_test_cluster(&app_config, &input, cluster, 0, category_name, 100, first_provider_id).await,
            XtreamClusterPublishOutcome::Published
        );
    }
    let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
        .await
        .expect("mixed cluster storage");
    let previous_vod = active_cluster_snapshot(&storage_path, XtreamCluster::Video);

    let live_outcome = publish_test_cluster(&app_config, &input, XtreamCluster::Live, 90, "new-live", 90, 40_000).await;
    let vod_outcome = publish_test_cluster(&app_config, &input, XtreamCluster::Video, 90, "new-vod", 89, 50_000).await;
    let series_outcome =
        publish_test_cluster(&app_config, &input, XtreamCluster::Series, 90, "new-series", 110, 60_000).await;

    assert_eq!(
        live_outcome,
        XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: Some(100),
            candidate_count: 90,
            threshold: 90,
            quality: Some(90),
        })
    );
    assert_eq!(
        vod_outcome,
        XtreamClusterPublishOutcome::RetainedPrevious(ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 100,
            candidate_count: 89,
            threshold: 90,
            quality: 89,
        })
    );
    assert_eq!(
        series_outcome,
        XtreamClusterPublishOutcome::QualityAccepted(ClusterUpdateAcceptance {
            cluster: XtreamCluster::Series,
            current_count: Some(100),
            candidate_count: 110,
            threshold: 90,
            quality: Some(90),
        })
    );
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Live), 90);
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Video), 100);
    assert_eq!(active_cluster_count(&storage_path, XtreamCluster::Series), 110);
    assert!(String::from_utf8_lossy(&active_category_bytes(&storage_path, XtreamCluster::Live)).contains("new-live"));
    assert_eq!(active_cluster_snapshot(&storage_path, XtreamCluster::Video), previous_vod);
    assert!(
        String::from_utf8_lossy(&active_category_bytes(&storage_path, XtreamCluster::Series)).contains("new-series")
    );
    assert_no_refresh_artifacts(&storage_path);
}
