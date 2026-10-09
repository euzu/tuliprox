use super::{
    fixed_refresh_paths, get_collection_path, make_live_item, preserve_details_input_xtream_playlist_cluster_to_disk,
    preserve_details_with_injected_operation_failure, target_writer_config, target_writer_group, test_app_config,
    write_detail_preservation_fixture, write_single_item, xtream_cluster_category_collection, xtream_write_playlist,
    DetailPreservationOperation,
};
use shared::model::{ClusterFlags, XtreamCluster};
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn target_writer_creates_missing_empty_category_files_without_force() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let app_config = test_app_config(directory.path());
    let target = target_writer_config();
    let mut live_only = vec![target_writer_group(XtreamCluster::Live, 1, 101)];

    xtream_write_playlist(&app_config, &target, &mut live_only, ClusterFlags::empty()).await?;

    let storage_path = {
        let config = app_config.config.load();
        super::super::xtream_get_storage_path(&config, &target.name).expect("target Xtream storage")
    };
    for cluster in [XtreamCluster::Video, XtreamCluster::Series] {
        let categories = get_collection_path(&storage_path, xtream_cluster_category_collection(cluster));
        assert_eq!(tokio::fs::read(categories).await?, b"[]");
        assert!(!super::super::xtream_get_file_path(&storage_path, cluster).exists());
    }
    Ok(())
}

#[tokio::test]
async fn target_writer_preserves_an_unauthorized_empty_cluster() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let app_config = test_app_config(directory.path());
    let target = target_writer_config();
    let mut baseline = vec![
        target_writer_group(XtreamCluster::Live, 1, 101),
        target_writer_group(XtreamCluster::Video, 2, 201),
        target_writer_group(XtreamCluster::Series, 3, 301),
    ];
    xtream_write_playlist(&app_config, &target, &mut baseline, ClusterFlags::empty()).await?;

    let storage_path = {
        let config = app_config.config.load();
        super::super::xtream_get_storage_path(&config, &target.name).expect("target Xtream storage")
    };
    let retained_paths = [XtreamCluster::Video, XtreamCluster::Series].map(|cluster| {
        (
            super::super::xtream_get_file_path(&storage_path, cluster),
            get_collection_path(&storage_path, xtream_cluster_category_collection(cluster)),
        )
    });
    let mut retained_before = Vec::new();
    for (database, categories) in &retained_paths {
        retained_before.push((tokio::fs::read(database).await?, tokio::fs::read(categories).await?));
    }

    let mut live_only = vec![target_writer_group(XtreamCluster::Live, 1, 102)];
    xtream_write_playlist(&app_config, &target, &mut live_only, ClusterFlags::empty()).await?;

    for ((database, categories), (database_before, categories_before)) in retained_paths.iter().zip(retained_before) {
        assert_eq!(tokio::fs::read(database).await?, database_before);
        assert_eq!(tokio::fs::read(categories).await?, categories_before);
    }
    Ok(())
}

#[test]
fn preserve_details_propagates_corrupt_published_database() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 5);
    fs::write(&paths.published_database, b"corrupt").expect("corrupt fixture should be written");
    write_single_item(&paths.staging_database, &make_live_item(102, None, None, None, None, 0));

    let error =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect_err("corrupt published data must fail");

    assert!(error.to_string().contains(&paths.published_database.display().to_string()));
}

#[test]
fn preserve_details_propagates_corrupt_staging_database() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 6);
    write_single_item(&paths.published_database, &make_live_item(103, None, None, None, None, 0));
    fs::write(&paths.staging_database, b"corrupt").expect("corrupt fixture should be written");

    let error =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect_err("corrupt staging data must fail");

    assert!(error.to_string().contains(&paths.staging_database.display().to_string()));
}

#[test]
fn preserve_details_propagates_staging_query_failure() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 15);
    write_detail_preservation_fixture(&paths, 106);

    let error = preserve_details_with_injected_operation_failure(
        &paths.published_database,
        &paths.staging_database,
        DetailPreservationOperation::Query,
    )
    .expect_err("a staging query failure must fail the merge");
    let message = error.to_string();

    assert!(message.contains("Failed to query staging Xtream tree"));
    assert!(message.contains(&paths.staging_database.display().to_string()));
    assert!(message.contains("injected Query failure"));
}
