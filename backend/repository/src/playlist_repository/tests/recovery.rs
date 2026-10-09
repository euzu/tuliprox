use super::*;

#[tokio::test]
async fn target_cache_reload_failures_return_errors_without_mutating_the_previous_cache() {
    for stage in [TargetCacheReloadStage::IdMapping, TargetCacheReloadStage::XtreamStorage] {
        let directory = tempdir().expect("temporary storage");
        let app_config = test_app_config(directory.path());
        let playlist_state = Arc::new(PlaylistStorageState::new());
        let target = xtream_target(&format!("cache-reload-{stage:?}"), true);
        seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
        let before = cached_target_signature(&playlist_state, &target.name).await;
        let mut candidate = vec![
            target_group(XtreamCluster::Live, 102, "new-live"),
            target_group(XtreamCluster::Series, 302, "new-series"),
        ];

        let errors = persist_playlist_with_mode(
            &app_config,
            &mut candidate,
            None,
            &target,
            Some(&playlist_state),
            TargetPlaylistPersistOptions {
                accepted_empty_clusters: ClusterFlags::Vod,
                ..TargetPlaylistPersistOptions::default()
            },
            TargetPersistenceMode::FailCacheReloadAt(stage),
        )
        .await
        .expect_err("injected target cache reload must fail");

        assert!(errors.iter().any(|error| error.to_string().contains("could not be reloaded")));
        assert_eq!(cached_target_signature(&playlist_state, &target.name).await, before);
        let persisted = load_xtream_target_storage(&app_config, &target).await.expect("persisted target storage");
        assert_eq!([persisted.live.len(), persisted.vod.len(), persisted.series.len()], [1, 0, 1]);
    }
}

#[tokio::test]
async fn target_force_empty_reload_replaces_the_complete_memory_cache() {
    let directory = tempdir().expect("temporary storage");
    let app_config = test_app_config(directory.path());
    let playlist_state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("force-empty-cache-success", true);
    seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
    let mut candidate = vec![
        target_group(XtreamCluster::Live, 102, "new-live"),
        target_group(XtreamCluster::Series, 302, "new-series"),
    ];

    persist_playlist_with_mode(
        &app_config,
        &mut candidate,
        None,
        &target,
        Some(&playlist_state),
        TargetPlaylistPersistOptions {
            accepted_empty_clusters: ClusterFlags::Vod,
            ..TargetPlaylistPersistOptions::default()
        },
        TargetPersistenceMode::Persist,
    )
    .await
    .expect("force-empty target persistence and cache reload");

    let (_, cached_items) = cached_target_signature(&playlist_state, &target.name).await;
    assert_eq!(cached_items.iter().filter(|(cluster, _, _)| *cluster == XtreamCluster::Live).count(), 1);
    assert!(!cached_items.iter().any(|(cluster, _, _)| *cluster == XtreamCluster::Video));
    assert_eq!(cached_items.iter().filter(|(cluster, _, _)| *cluster == XtreamCluster::Series).count(), 1);
    assert!(cached_items.iter().any(|(_, _, name)| name.as_ref() == "new-live"));
    assert!(cached_items.iter().any(|(_, _, name)| name.as_ref() == "new-series"));
}
