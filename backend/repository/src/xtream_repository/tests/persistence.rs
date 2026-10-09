use super::{
    fixed_refresh_paths, preserve_details_with_injected_operation_failure, target_writer_config, target_writer_group,
    test_app_config, write_detail_preservation_fixture, xtream_write_playlist,
    xtream_write_playlist_with_injected_empty_replacement_failure, DetailPreservationOperation,
    TargetEmptyReplacementFailure,
};
use crate::get_file_path_for_db_index;
use shared::model::{ClusterFlags, XtreamCluster};
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn target_force_empty_failures_restore_database_index_and_categories() -> Result<(), Box<dyn std::error::Error>> {
    for failure in [
        TargetEmptyReplacementFailure::CategoryPersistence,
        TargetEmptyReplacementFailure::BTreePersistence,
        TargetEmptyReplacementFailure::Publication,
    ] {
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
        let database = super::super::xtream_get_file_path(&storage_path, XtreamCluster::Video);
        let index = get_file_path_for_db_index(&database);
        let categories = super::super::get_vod_cat_collection_path(&storage_path);
        let before =
            [tokio::fs::read(&database).await?, tokio::fs::read(&index).await?, tokio::fs::read(&categories).await?];
        let mut candidate =
            vec![target_writer_group(XtreamCluster::Live, 1, 102), target_writer_group(XtreamCluster::Series, 3, 302)];

        let error = xtream_write_playlist_with_injected_empty_replacement_failure(
            &app_config,
            &target,
            &mut candidate,
            ClusterFlags::Vod,
            failure,
        )
        .await
        .expect_err("injected empty replacement must fail");

        assert!(error.to_string().contains("target cluster failed"));
        assert_eq!(tokio::fs::read(&database).await?, before[0]);
        assert_eq!(tokio::fs::read(&index).await?, before[1]);
        assert_eq!(tokio::fs::read(&categories).await?, before[2]);
        let leaked = fs::read_dir(&storage_path)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("force-empty"))
            .collect::<Vec<_>>();
        assert!(leaked.is_empty(), "staging or backup files leaked after {failure:?}: {leaked:?}");
    }
    Ok(())
}

#[test]
fn preserve_details_propagates_staging_commit_failure() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 17);
    write_detail_preservation_fixture(&paths, 108);

    let error = preserve_details_with_injected_operation_failure(
        &paths.published_database,
        &paths.staging_database,
        DetailPreservationOperation::Commit,
    )
    .expect_err("a staging commit failure must fail the merge");
    let message = error.to_string();

    assert!(message.contains("Failed to commit staging Xtream tree"));
    assert!(message.contains(&paths.staging_database.display().to_string()));
    assert!(message.contains("injected Commit failure"));
}
