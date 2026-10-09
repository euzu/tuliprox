use super::*;

#[tokio::test]
async fn intentional_empty_strm_cleanup_false_removes_indexed_files_but_keeps_unmanaged_files() {
    let directory = tempdir().expect("tempdir");
    let app_config = target_test_app_config(directory.path());
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: "curated-strm-retention-test".to_string(),
        output: vec![TargetOutputDto::Strm(StrmTargetOutputDto {
            directory: "strm-retained".to_string(),
            flat: true,
            cleanup: false,
            ..StrmTargetOutputDto::default()
        })],
        ..ConfigTargetDto::default()
    });
    let mut seeded =
        vec![target_video_group("Retained movie", UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000029"))];
    let initial = persist_playlist_views(
        &app_config,
        &mut seeded,
        None,
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(initial.is_ok(), "initial STRM persist failed: {initial:?}");
    let strm_root = directory.path().join("strm-retained");
    let managed_files = strm_files_below(&strm_root);
    assert_eq!(managed_files.len(), 1);
    let managed_file = managed_files[0].clone();
    let unmanaged_file = strm_root.join("unmanaged.strm");
    std::fs::write(&unmanaged_file, "unmanaged").expect("unmanaged STRM fixture");

    let mut empty = Vec::new();
    let published = persist_playlist_views(
        &app_config,
        &mut empty,
        None,
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::complete_curation(true, false)),
    )
    .await;
    assert!(published.is_ok(), "empty STRM persist failed: {published:?}");

    assert!(!managed_file.exists(), "cleanup=false removes files tracked by the managed index");
    assert!(unmanaged_file.exists(), "cleanup=false does not scan and remove unmanaged files");
    let target_storage = {
        let config = app_config.config.load();
        get_target_storage_path(&config, &target.name).expect("target storage")
    };
    let index = strm_get_file_paths(&hash_string_as_hex(&normalize_string_path("strm-retained")), &target_storage);
    assert!(std::fs::read_to_string(index).expect("empty STRM index").is_empty());
}
