use super::*;

#[test]
fn target_force_empty_authorization_is_limited_to_xtream_outputs() {
    let xtream_target = target_with_outputs(vec![TargetOutput::Xtream(XtreamTargetOutput {
        flags: XtreamTargetFlagsSet::new(),
        trakt: None,
        filter: None,
    })]);
    let options = TargetPlaylistPersistOptions {
        accepted_empty_clusters: ClusterFlags::Vod,
        ..TargetPlaylistPersistOptions::default()
    };
    assert!(validate_target_playlist_persistence(&xtream_target, true, &options).is_ok());

    let mixed_target = target_with_outputs(vec![
        TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        TargetOutput::M3u(M3uTargetOutput {
            filename: None,
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        }),
    ]);
    let error = validate_target_playlist_persistence(&mixed_target, true, &options)
        .expect_err("fully empty mixed output must fail before persistence");
    assert!(error.to_string().contains("non-Xtream outputs"));
}

#[tokio::test]
async fn target_empty_replacement_failures_leave_the_memory_cache_unchanged() {
    for failure in [
        crate::TargetEmptyReplacementFailure::CategoryPersistence,
        crate::TargetEmptyReplacementFailure::BTreePersistence,
        crate::TargetEmptyReplacementFailure::Publication,
    ] {
        let directory = tempdir().expect("temporary storage");
        let app_config = app_config_in(directory.path());
        let playlist_state = Arc::new(PlaylistStorageState::new());
        let target = xtream_target(&format!("empty-persist-{failure:?}"), true);
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
            TargetPersistenceMode::FailEmptyReplacementAt(failure),
        )
        .await
        .expect_err("injected empty replacement must fail");

        assert!(errors.iter().any(|error| error.to_string().contains("target cluster failed")));
        assert_eq!(cached_target_signature(&playlist_state, &target.name).await, before);
        let persisted = load_xtream_target_storage(&app_config, &target).await.expect("retained target storage");
        assert_eq!(persisted.vod.len(), 1);
    }
}

#[test]
// Unchanged by the move; rustfmt reflowed it past the 100-line threshold.
#[allow(clippy::too_many_lines)]
fn materializes_media_server_series_info_episodes_after_target_virtual_ids_are_assigned() {
    let series_uuid = "123e4567-e89b-12d3-a456-426614174111";
    let mut series_info = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid(series_uuid),
            id: "media-server:server:shows:series:series".intern(),
            name: "Media Server Series".intern(),
            title: "Media Server Series".intern(),
            url: "media-server://unavailable/server/shows/series".intern(),
            item_type: PlaylistItemType::SeriesInfo,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                name: "Media Server Series".intern(),
                details: Some(SeriesStreamDetailProperties {
                    year: Some(2024),
                    seasons: Some(vec![SeriesStreamDetailSeasonProperties {
                        name: "Season 1".intern(),
                        season_number: 1,
                        episode_count: 2,
                        overview: Some("season summary".intern()),
                        air_date: Some("2024-01-01".intern()),
                        cover: None,
                        cover_tmdb: None,
                        cover_big: None,
                        duration: None,
                    }]),
                    episodes: None,
                }),
                ..SeriesStreamProperties::default()
            }))),
            ..PlaylistItemHeader::default()
        },
    };
    let series_parent_code = series_info.header.uuid.to_string();
    let media_episode_two =
        PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-two", 7002, 1, 2) };
    let media_episode_one =
        PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-one", 7001, 1, 1) };
    let malformed_media_episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "media-server:server:shows:episode:malformed".intern(),
            parent_code: series_parent_code.clone().intern(),
            url: "media-server://plex/server/malformed?part_key=%2Flibrary%2Fparts%2Fredacted".intern(),
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(7003),
            additional_properties: None,
            ..PlaylistItemHeader::default()
        },
    };
    let provider_episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "999".intern(),
            parent_code: series_parent_code.clone().intern(),
            url: "http://provider.example.invalid/series/999.mkv".intern(),
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(7999),
            ..PlaylistItemHeader::default()
        },
    };

    let mut media_server_series = HashMap::<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>::new();
    assign_media_server_series_info_episode(&mut media_server_series, &media_episode_two.header);
    assign_media_server_series_info_episode(&mut media_server_series, &provider_episode.header);
    assign_media_server_series_info_episode(&mut media_server_series, &malformed_media_episode.header);
    assign_media_server_series_info_episode(&mut media_server_series, &media_episode_one.header);

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Media Server Series".intern(),
        channels: vec![series_info, media_episode_two, provider_episode, malformed_media_episode, media_episode_one],
        xtream_cluster: XtreamCluster::Series,
    }];

    materialize_media_server_series_info_episodes(&mut playlist, &media_server_series);
    series_info = playlist[0].channels[0].clone();

    let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let details = series.details.as_ref().expect("series details should be present");
    assert_eq!(details.year, Some(2024));
    assert_eq!(details.seasons.as_ref().expect("seasons should be preserved")[0].episode_count, 2);
    let episodes = details.episodes.as_ref().expect("media-server episodes should be materialized");
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[0].episode_num, 1);
    assert_eq!(episodes[0].season, 1);
    assert_eq!(episodes[0].title.as_ref(), "Episode 1");
    assert_eq!(episodes[0].container_extension.as_ref(), "mkv");
    assert_eq!(episodes[0].release_date.as_ref(), "2024-02-03");
    assert_eq!(episodes[0].series_release_date.as_deref(), Some("2024-01-01"));
    assert_eq!(episodes[0].plot.as_deref(), Some("Episode summary"));
    assert_eq!(episodes[0].tmdb, Some(67890));
    assert_eq!(episodes[0].direct_source.as_ref(), "");
    assert_eq!(
        episodes[0].movie_image.as_ref(),
        "media-server://image/plex/server/episode?image_path=%2Flibrary%2Fmetadata%2Fredacted%2Fthumb"
    );
    assert!(episodes[0].video.as_deref().is_some_and(|video| video.contains("h264")));
    assert!(episodes[0].audio.as_deref().is_some_and(|audio| audio.contains("aac")));
    assert_eq!(episodes[1].id, 7002);
    assert!(!episodes.iter().any(|episode| episode.id == 7003));
}

#[test]
fn rewrite_series_episode_parent_virtual_ids_updates_local_episode_mapping() {
    let series_uuid = "123e4567-e89b-12d3-a456-426614174000";
    let series_uuid_type = UUIDType::from_valid_uuid(series_uuid);
    let episode_uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174001");
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![
            PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: episode_uuid,
                    id: "101".intern(),
                    parent_code: series_uuid.intern(),
                    url: "/library/episode1.mkv".intern(),
                    item_type: PlaylistItemType::LocalSeries,
                    xtream_cluster: XtreamCluster::Series,
                    ..PlaylistItemHeader::default()
                },
            },
            PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: series_uuid_type,
                    id: series_uuid.intern(),
                    item_type: PlaylistItemType::LocalSeriesInfo,
                    xtream_cluster: XtreamCluster::Series,
                    ..PlaylistItemHeader::default()
                },
            },
        ],
        xtream_cluster: XtreamCluster::Series,
    }];

    for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
        let uuid = channel.header.uuid;
        let provider_id = channel.header.get_provider_id().unwrap_or_default();
        let item_type = channel.header.item_type;
        channel.header.virtual_id =
            target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
        channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
    }

    let series_virtual_id = playlist[0].channels[1].header.virtual_id;
    let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

    rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, series_virtual_id);
    assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
}

#[test]
fn rewrite_series_episode_parent_virtual_ids_updates_provider_episode_mapping_using_series_info_uuid() {
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let input_name = "provider-input".intern();
    let xtream_series_info = XtreamPlaylistItem {
        virtual_id: VirtualId::new(0),
        provider_id: 9001,
        name: "Provider Series".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Provider Series".intern(),
        title: "Provider Series".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://provider.example.com/series/user/pass/9001".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: None,
        item_type: PlaylistItemType::SeriesInfo,
        category_id: 0,
        input_name: Arc::clone(&input_name),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "9001".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };
    let provider_parent_code = xtream_series_info.get_uuid().intern();
    let xtream_provider_episode = XtreamPlaylistItem {
        virtual_id: VirtualId::new(0),
        provider_id: 201,
        name: "Episode 1".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Provider Series".intern(),
        title: "Episode 1".intern(),
        parent_code: provider_parent_code,
        rec: "".intern(),
        url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: None,
        item_type: PlaylistItemType::Series,
        category_id: 0,
        input_name,
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "201".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };
    let provider_episode = PlaylistItem::from(&xtream_provider_episode);
    let mut series_info = PlaylistItem::from(&xtream_series_info);
    series_info.header.uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174099");

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Provider Series".intern(),
        channels: vec![provider_episode, series_info],
        xtream_cluster: XtreamCluster::Series,
    }];

    for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
        let uuid = channel.get_uuid();
        let provider_id = channel.header.get_provider_id().unwrap_or_default();
        let item_type = channel.header.item_type;
        channel.header.virtual_id =
            target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
        channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
    }

    let series_virtual_id = playlist[0].channels[1].header.virtual_id;
    let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

    rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, series_virtual_id);
    assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
}

#[test]
fn initial_virtual_id_assignment_preserves_existing_parent_for_series_episode_without_parent_match() {
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let input_name = "provider-input".intern();
    let mut episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "201".intern(),
            url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
            input_name,
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            ..PlaylistItemHeader::default()
        },
    };

    let provider_id = episode.header.get_provider_id().unwrap_or_default();
    let uuid = *episode.header.get_uuid();
    let original_virtual_id =
        target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, episode.header.item_type, VirtualId::new(77));

    let preserved_parent_virtual_id = target_id_mapping.get_parent_virtual_id_by_uuid(&uuid).unwrap_or_default();
    episode.header.virtual_id = target_id_mapping.get_and_update_virtual_id(
        &uuid,
        provider_id,
        episode.header.item_type,
        preserved_parent_virtual_id,
    );
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&original_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, VirtualId::new(77));
    assert_eq!(episode.header.virtual_id, original_virtual_id);
}

#[test]
fn rewrite_local_series_info_uses_series_uuid_lookup_and_updates_episode_virtual_ids() {
    let series_uuid = "series-uuid";
    let mut series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    local_library_series.insert(
        series_uuid.intern(),
        vec![
            LocalEpisodeKey { path: "/library/episode1.mkv".intern(), virtual_id: 7001 },
            LocalEpisodeKey { path: "/library/episode2.mkv".intern(), virtual_id: 7002 },
        ],
    );

    rewrite_local_series_info_episode_virtual_id(&mut series_info, &local_library_series);

    let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
}

#[test]
fn rewrite_series_info_updates_local_episode_ids_before_parent_code_is_cleared() {
    let series_uuid = "series-uuid";
    let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
    let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_one, PlaylistItemType::LocalSeries);
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_two, PlaylistItemType::LocalSeries);

    let series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let local_episode_one = PlaylistItem { header: episode_one };
    let local_episode_two = PlaylistItem { header: episode_two };
    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![series_info, local_episode_one, local_episode_two],
        xtream_cluster: XtreamCluster::Series,
    }];

    rewrite_series_info_episode_virtual_id(
        &mut playlist,
        &local_library_series,
        &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
    );

    let Some(StreamProperties::Series(series)) = playlist[0].channels[0].header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
    assert!(playlist[0].channels[1].header.parent_code.is_empty());
    assert!(playlist[0].channels[2].header.parent_code.is_empty());
}

#[test]
fn rewrite_series_info_updates_local_episode_ids_when_episodes_come_first() {
    // Test with episodes BEFORE series_info to verify iteration-order doesn't matter
    let series_uuid = "series-uuid";
    let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
    let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_one, PlaylistItemType::LocalSeries);
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_two, PlaylistItemType::LocalSeries);

    let series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let local_episode_one = PlaylistItem { header: episode_one };
    let local_episode_two = PlaylistItem { header: episode_two };

    // Episodes FIRST, then series_info (reversed order)
    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![local_episode_one, local_episode_two, series_info],
        xtream_cluster: XtreamCluster::Series,
    }];

    rewrite_series_info_episode_virtual_id(
        &mut playlist,
        &local_library_series,
        &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
    );

    let Some(StreamProperties::Series(series)) = playlist[0].channels[2].header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
    assert!(playlist[0].channels[0].header.parent_code.is_empty());
    assert!(playlist[0].channels[1].header.parent_code.is_empty());
}

#[tokio::test]
async fn replace_target_contents_rolls_back_all_artifacts_then_allows_a_retry() {
    use crate::{get_live_cat_collection_path, xtream_get_item_for_stream_id};
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("replace", true);
    let other = xtream_target("unselected", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;
    seed_memory_cached_xtream_target(&app, &state, &other).await;
    let before = cached_target_signature(&state, &target.name).await;
    let other_before = cached_target_signature(&state, &other.name).await;
    let config = app.config.load();
    let target_path = get_target_storage_path(&config, &target.name).unwrap();
    let storage = xtream_get_storage_path(&config, &target.name).unwrap();
    let category_paths = [
        get_live_cat_collection_path(&storage),
        get_vod_cat_collection_path(&storage),
        get_series_cat_collection_path(&storage),
    ];
    let old_categories = category_paths.iter().map(|path| std::fs::read(path).unwrap()).collect::<Vec<_>>();
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    let old_mapping = std::fs::read(&mapping_path).unwrap();
    let options = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let mut candidate = vec![target_group(XtreamCluster::Live, 102, "new-live")];

    let errors = persist_playlist_with_mode(
        &app,
        &mut candidate,
        None,
        &target,
        Some(&state),
        options.clone(),
        TargetPersistenceMode::FailEmptyReplacementAt(crate::TargetEmptyReplacementFailure::Publication),
    )
    .await
    .unwrap_err();
    assert!(errors[0].message().contains("injected"));
    assert_eq!(cached_target_signature(&state, &target.name).await, before);
    assert_eq!(std::fs::read(&mapping_path).unwrap(), old_mapping);
    for (path, previous) in category_paths.iter().zip(&old_categories) {
        assert_eq!(&std::fs::read(path).unwrap(), previous);
    }
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [1, 1, 1]);

    persist_playlist_with_mode(
        &app,
        &mut candidate,
        None,
        &target,
        Some(&state),
        options.clone(),
        TargetPersistenceMode::Persist,
    )
    .await
    .unwrap();
    let cached = cached_target_signature(&state, &target.name).await;
    assert_eq!(cached.0, before.0 + 1);
    assert_eq!(
        cached.1.iter().map(|(cluster, _, name)| (*cluster, name.as_ref())).collect::<Vec<_>>(),
        [(XtreamCluster::Live, "new-live")]
    );
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [1, 0, 0]);
    for (cluster, id, _) in before.1 {
        assert!(xtream_get_item_for_stream_id(id, &app, &state, &target, Some(cluster)).await.is_err());
    }
    assert_eq!(std::fs::read_to_string(&category_paths[1]).unwrap(), "[]");
    assert_eq!(std::fs::read_to_string(&category_paths[2]).unwrap(), "[]");
    assert_eq!(cached_target_signature(&state, &other.name).await, other_before);

    persist_playlist_with_mode(&app, &mut [], None, &target, Some(&state), options, TargetPersistenceMode::Persist)
        .await
        .unwrap();
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [0, 0, 0]);
    assert_eq!(cached_target_signature(&state, &target.name).await, (cached.0, Vec::new()));
    for path in &category_paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[]");
    }
}

#[tokio::test(start_paused = true)]
async fn replace_target_contents_deadline_waits_for_started_publication() {
    use crate::{xtream_repository::target_publish::TargetEmptyPublicationHook, TargetReplacementDeadline};
    use std::time::Duration;
    use tokio::time::{advance, Instant};

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("deadline-started", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;

    let deadline = Arc::new(TargetReplacementDeadline::new(Instant::now() + Duration::from_secs(60)));
    let options = TargetPlaylistPersistOptions {
        replace_target_contents: true,
        replacement_deadline: Some(deadline.clone()),
        ..Default::default()
    };
    let hook = TargetEmptyPublicationHook::new();
    let entered = Arc::clone(&hook.backup_window_entered);
    let resume = Arc::clone(&hook.resume_publication);
    let writer = tokio::spawn({
        let (app, state, target) = (app.clone(), state.clone(), target.clone());
        async move {
            let mut candidate = [target_group(XtreamCluster::Live, 102, "new-live")];
            deadline
                .run(async {
                    persist_playlist_with_mode(
                        &app,
                        &mut candidate,
                        None,
                        &target,
                        Some(&state),
                        options,
                        TargetPersistenceMode::PauseDuringReplacement(hook),
                    )
                    .await
                    .unwrap();
                    std::future::pending::<()>().await;
                })
                .await
        }
    });

    tokio::task::spawn_blocking(move || entered.wait()).await.unwrap();
    advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    let returned_before_commit = writer.is_finished();
    tokio::task::spawn_blocking(move || resume.wait()).await.unwrap();
    assert!(!returned_before_commit, "the run must retain the publication result after the deadline");
    let published = tokio::time::timeout(Duration::from_secs(1), writer)
        .await
        .expect("only the active commit may delay the timeout")
        .unwrap()
        .unwrap_err();
    assert_eq!(published.as_slice(), std::slice::from_ref(&target.name));

    let stored = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([stored.live.len(), stored.vod.len(), stored.series.len()], [1, 0, 0]);
    assert_eq!(cached_target_signature(&state, &target.name).await.1[0].2.as_ref(), "new-live");
    let config = app.config.load();
    let storage = xtream_get_storage_path(&config, &target.name).unwrap();
    assert_eq!(std::fs::read_to_string(get_vod_cat_collection_path(&storage)).unwrap(), "[]");
}

#[tokio::test(start_paused = true)]
async fn replace_target_contents_deadline_rejects_late_publication() {
    use crate::{xtream_category_view_lock_path, TargetReplacementDeadline};
    use std::time::Duration;
    use tokio::time::{advance, Instant};

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("deadline-expired", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;
    let before = cached_target_signature(&state, &target.name).await;
    let storage = xtream_get_storage_path(&app.config.load(), &target.name).unwrap();
    let vod_path = get_vod_cat_collection_path(&storage);
    let categories = std::fs::read(&vod_path).unwrap();
    let reader = app.file_locks.read_lock(&xtream_category_view_lock_path(&storage)).await;

    let deadline = Arc::new(TargetReplacementDeadline::new(Instant::now() + Duration::from_secs(60)));
    let options = TargetPlaylistPersistOptions {
        replace_target_contents: true,
        replacement_deadline: Some(deadline.clone()),
        ..Default::default()
    };
    let mut writer = tokio::spawn({
        let (app, state, target) = (app.clone(), state.clone(), target.clone());
        async move {
            persist_playlist_with_mode(
                &app,
                &mut [],
                None,
                &target,
                Some(&state),
                options,
                TargetPersistenceMode::Persist,
            )
            .await
        }
    });
    advance(Duration::from_secs(61)).await;
    // Keep the test task alive across timeout to exercise the final guard even
    // if its caller no longer waits for it.
    assert_eq!(deadline.run(&mut writer).await.unwrap_err(), Vec::<String>::new());
    drop(reader);
    let errors = writer.await.unwrap().unwrap_err();
    assert!(errors[0].message().contains("deadline expired before publication"));
    let stored = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([stored.live.len(), stored.vod.len(), stored.series.len()], [1, 1, 1]);
    assert_eq!(cached_target_signature(&state, &target.name).await, before);
    assert_eq!(std::fs::read(&vod_path).unwrap(), categories);
}
