use super::*;

#[tokio::test]
async fn force_empty_output_filters_reject_m3u_and_strm_before_mutation() {
    let filter = get_filter(r#"EpgId ~ "^Mixed\.Case$""#, None).expect("case-sensitive EPG-ID filter");
    let cases = [
        (
            "filtered-m3u",
            TargetOutput::M3u(M3uTargetOutput {
                filename: Some("client.m3u".to_string()),
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: Some(filter.clone()),
            }),
            "client.m3u",
        ),
        (
            "filtered-strm",
            TargetOutput::Strm(StrmTargetOutput {
                directory: "client-strm".to_string(),
                username: None,
                style: StrmExportStyle::Jellyfin,
                flags: StrmTargetFlagsSet::new(),
                strm_props: None,
                filter: Some(filter),
                probe_probe_size_bytes: None,
                probe_analyze_duration: None,
            }),
            "client-strm/old.strm",
        ),
    ];

    for (target_name, output, artifact) in cases {
        let directory = tempdir().expect("temporary storage");
        let app_config = test_app_config(directory.path());
        let artifact_path = directory.path().join(artifact);
        tokio::fs::create_dir_all(artifact_path.parent().expect("artifact parent")).await.expect("artifact directory");
        tokio::fs::write(&artifact_path, b"previous-client-state").await.expect("previous client artifact");
        let mut target = target_with_options(target_name, vec![output], false);
        target.options = Some(epg_normalization_options(true));
        let target_path = {
            let config = app_config.config.load();
            crate::get_target_storage_path(&config, &target.name).expect("target storage path")
        };
        tokio::fs::create_dir_all(&target_path).await.expect("target storage directory");
        let mapping_path = crate::get_target_id_mapping_file(&target_path);
        let mapping_uuid_path = mapping_path.with_extension("uuid.db");
        let seed_group = target_group(XtreamCluster::Live, 100, "mapping-seed");
        {
            let seed_header = &seed_group.channels[0].header;
            let mut mapping = TargetIdMapping::new(&mapping_path, false).expect("seed target ID mapping");
            mapping.get_and_update_virtual_id(seed_header.get_uuid(), 0, seed_header.item_type, VirtualId::default());
            mapping.persist().expect("persist target ID mapping seed");
        }
        let mapping_before = [
            tokio::fs::read(&mapping_path).await.expect("target ID mapping"),
            tokio::fs::read(&mapping_uuid_path).await.expect("target UUID mapping"),
        ];
        let mut candidate = vec![target_group(XtreamCluster::Live, 101, "filtered-after-normalization")];
        candidate[0].channels[0].header.epg_channel_id = Some("Mixed.Case".intern());

        let errors = persist_playlist_with_mode(
            &app_config,
            &mut candidate,
            None,
            &target,
            None,
            TargetPlaylistPersistOptions {
                accepted_empty_clusters: ClusterFlags::Vod,
                ..TargetPlaylistPersistOptions::default()
            },
            TargetPersistenceMode::Persist,
        )
        .await
        .expect_err("force-empty filtered output must be rejected");

        assert!(errors.iter().any(|error| error.to_string().contains("after its output filter")));
        assert_eq!(candidate[0].channels[0].header.epg_channel_id.as_deref(), Some("mixed.case"));
        assert_eq!(tokio::fs::read(&artifact_path).await.expect("retained client artifact"), b"previous-client-state");
        assert_eq!(tokio::fs::read(&mapping_path).await.expect("retained target ID mapping"), mapping_before[0]);
        assert_eq!(tokio::fs::read(&mapping_uuid_path).await.expect("retained target UUID mapping"), mapping_before[1]);
    }
}

#[test]
fn playlist_without_channels_is_empty_for_persistence() {
    assert!(!playlist_has_items(&[]));
    assert!(!playlist_has_items(&[PlaylistGroup {
        id: 1,
        title: "empty".intern(),
        channels: Vec::new(),
        xtream_cluster: XtreamCluster::Live,
    }]));
}

#[test]
fn media_server_playlist_file_path_uses_separate_prefix() {
    let dir = tempdir().expect("tempdir");
    let path = get_input_media_server_playlist_file_path(dir.path(), &"Media Server Input".intern());

    assert!(path.ends_with("media_server_Media_Server_Input.db"));
}
