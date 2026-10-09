use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn intentional_empty_output_views_clear_managed_artifacts_without_alias_leakage() {
    let directory = tempdir().expect("tempdir");
    let app_config = target_test_app_config(directory.path());
    let target = mixed_output_target();
    let playlist_state = Arc::new(crate::PlaylistStorageState::new());
    let mut standard =
        vec![target_video_group("Base movie", UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000021"))];
    let mut xtream =
        vec![target_video_group("Curated alias", UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000022"))];

    let first = persist_playlist_views(
        &app_config,
        &mut standard,
        Some(&mut xtream),
        None,
        &target,
        Some(&playlist_state),
        curation_persist_options(PlaylistPublicationPlan::complete_curation(true, false)),
    )
    .await;
    assert!(first.is_ok(), "initial mixed-output persist failed: {first:?}");

    let m3u = load_m3u_target_storage(&app_config, &target).await.expect("M3U storage");
    let xtream = load_xtream_target_storage(&app_config, &target).await.expect("Xtream storage");
    assert_eq!(m3u.len(), 1);
    assert_eq!(m3u.iter().next().expect("M3U item").1.title.as_ref(), "Base movie");
    assert_eq!(xtream.vod.len(), 1);
    let xtream_item = xtream.vod.iter().next().expect("Xtream item").1;
    assert_eq!(xtream_item.title.as_ref(), "Curated alias");
    assert_ne!(xtream_item.category_id, 0, "base-suppressed unfiltered rows remain category-backed aliases");
    {
        let cache = playlist_state.data.read().await;
        let cached = cache.get(&target.name).expect("target cache");
        assert_eq!(cached.m3u.as_ref().expect("M3U cache").len(), 1);
        assert_eq!(cached.xtream.as_ref().expect("Xtream cache").vod.len(), 1);
    }
    let m3u_text_path = directory.path().join("curated.m3u");
    assert!(std::fs::read_to_string(&m3u_text_path).expect("M3U text").contains("Base movie"));
    let strm_root = directory.path().join("strm");
    let strm_files = strm_files_below(&strm_root);
    assert_eq!(strm_files.len(), 1);
    assert!(std::fs::read_to_string(&strm_files[0]).expect("STRM content").contains("Base movie"));

    let mut empty_standard = Vec::new();
    let mut empty_xtream = Vec::new();
    let retained = persist_playlist_views(
        &app_config,
        &mut empty_standard,
        Some(&mut empty_xtream),
        None,
        &target,
        Some(&playlist_state),
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(retained.is_err(), "untrusted empty input must retain the published snapshot");
    assert_eq!(load_m3u_target_storage(&app_config, &target).await.expect("retained M3U").len(), 1);
    assert_eq!(load_xtream_target_storage(&app_config, &target).await.expect("retained Xtream").vod.len(), 1);
    {
        let cache = playlist_state.data.read().await;
        let cached = cache.get(&target.name).expect("retained target cache");
        assert_eq!(cached.m3u.as_ref().expect("retained M3U cache").len(), 1);
        assert_eq!(cached.xtream.as_ref().expect("retained Xtream cache").vod.len(), 1);
    }
    assert_eq!(strm_files_below(&strm_root).len(), 1, "untrusted empty refresh must retain STRM files");

    let published = persist_playlist_views(
        &app_config,
        &mut empty_standard,
        Some(&mut empty_xtream),
        None,
        &target,
        Some(&playlist_state),
        curation_persist_options(PlaylistPublicationPlan::complete_curation(true, false)),
    )
    .await;
    assert!(published.is_ok(), "trusted empty snapshot failed: {published:?}");
    assert!(load_m3u_target_storage(&app_config, &target).await.expect("empty M3U").is_empty());
    assert_eq!(std::fs::read_to_string(&m3u_text_path).expect("empty M3U text"), "#EXTM3U\n");
    let empty_xtream = load_xtream_target_storage(&app_config, &target).await.expect("empty Xtream");
    assert!(empty_xtream.live.is_empty());
    assert!(empty_xtream.vod.is_empty());
    assert!(empty_xtream.series.is_empty());
    let target_storage = {
        let config = app_config.config.load();
        get_target_storage_path(&config, &target.name).expect("target storage")
    };
    let xtream_storage = {
        let config = app_config.config.load();
        xtream_get_storage_path(&config, &target.name).expect("Xtream storage path")
    };
    assert_eq!(
        std::fs::read_to_string(get_vod_cat_collection_path(&xtream_storage)).expect("empty VOD categories"),
        "[]"
    );
    assert_eq!(
        std::fs::read_to_string(get_series_cat_collection_path(&xtream_storage)).expect("empty series categories"),
        "[]"
    );
    let strm_index = strm_get_file_paths(&hash_string_as_hex(&normalize_string_path("strm")), &target_storage);
    assert!(std::fs::read_to_string(strm_index).expect("empty STRM index").is_empty());
    {
        let cache = playlist_state.data.read().await;
        let cached = cache.get(&target.name).expect("empty target cache");
        assert!(cached.m3u.as_ref().expect("empty M3U cache").is_empty());
        assert!(cached.xtream.as_ref().expect("empty Xtream cache").vod.is_empty());
    }
    assert!(strm_files_below(&strm_root).is_empty(), "trusted empty refresh must clean stale STRM files");
}

#[test]
fn skipped_clusters_converts_loaded_clusters_to_exclusions() {
    let skipped = skipped_clusters(&[XtreamCluster::Live, XtreamCluster::Series]);

    assert_eq!(skipped.len(), 1);
    assert!(skipped.contains(&XtreamCluster::Video));
}
