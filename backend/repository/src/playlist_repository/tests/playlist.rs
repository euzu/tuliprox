use super::*;

#[test]
fn target_empty_playlist_without_force_authorization_remains_rejected() {
    let target = target_with_outputs(vec![TargetOutput::Xtream(XtreamTargetOutput {
        flags: XtreamTargetFlagsSet::new(),
        trakt: None,
        filter: None,
    })]);
    let error = validate_target_playlist_persistence(&target, true, TargetPlaylistPersistOptions::default())
        .expect_err("unauthorized empty target must remain rejected");
    assert!(error.to_string().contains("Refusing to persist empty playlist"));
}

#[test]
fn complete_curation_empty_authorization_is_projection_and_cluster_scoped() {
    let compatibility = PlaylistPublicationPlan::complete_curation(false, false);
    assert!(!compatibility.allows_any_empty_output());

    let suppressed_xtream_base = PlaylistPublicationPlan::complete_curation(false, true);
    assert!(!suppressed_xtream_base.allows_empty_base_output());
    assert!(!suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Live));
    assert!(suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Video));
    assert!(suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Series));

    let persist_filtered = PlaylistPublicationPlan::complete_curation_with_filter(false, false, true);
    assert!(persist_filtered.allows_empty_base_output());
    assert!(persist_filtered.allows_empty_xtream_cluster(XtreamCluster::Video));
    let output_filtered = compatibility.with_output_filter(true);
    assert!(output_filtered.allows_empty_base_output());
    assert_eq!(PlaylistPublicationPlan::Ordinary.with_output_filter(true), PlaylistPublicationPlan::Ordinary);
}

#[tokio::test]
async fn intentional_empty_curation_never_authorizes_empty_live_cluster_replacement() {
    let directory = tempdir().expect("tempdir");
    let app_config = target_test_app_config(directory.path());
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: "curated-live-retention-test".to_string(),
        output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto::default())],
        ..ConfigTargetDto::default()
    });
    let mut live = vec![
        PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "1".intern(),
                    name: "Live channel".intern(),
                    title: "Live channel".intern(),
                    group: "Live".intern(),
                    url: "http://example.invalid/live".intern(),
                    uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000031"),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        },
        target_video_group(
            "Previously published movie",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000032"),
        ),
    ];

    let initial = persist_playlist_views(
        &app_config,
        &mut live,
        None,
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(initial.is_ok(), "initial Live persist failed: {initial:?}");

    let mut ordinary_live_only = vec![live[0].clone()];
    let ordinary = persist_playlist_views(
        &app_config,
        &mut ordinary_live_only,
        None,
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(ordinary.is_ok(), "ordinary Live-only persist failed: {ordinary:?}");
    let ordinary_storage = load_xtream_target_storage(&app_config, &target).await.expect("ordinary storage");
    assert_eq!(ordinary_storage.live.len(), 1);
    assert_eq!(ordinary_storage.vod.len(), 1, "ordinary refresh retains an absent cluster");

    let mut empty_standard = Vec::new();
    let mut empty_xtream = Vec::new();
    let curated_empty = persist_playlist_views(
        &app_config,
        &mut empty_standard,
        Some(&mut empty_xtream),
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::complete_curation(true, false)),
    )
    .await;
    assert!(curated_empty.is_ok(), "curated empty persist failed: {curated_empty:?}");

    let storage = load_xtream_target_storage(&app_config, &target).await.expect("Xtream storage");
    assert_eq!(storage.live.len(), 1);
    assert!(storage.vod.is_empty());
    assert!(storage.series.is_empty());
}

#[test]
fn target_playlist_epg_normalization_is_consistent_for_m3u_and_xtream() {
    let options = epg_normalization_options(true);
    let mut playlist = epg_normalization_playlist();
    let lowercase_id =
        Arc::clone(playlist[0].channels[1].header.epg_channel_id.as_ref().expect("lowercase EPG ID should exist"));

    normalize_target_playlist_epg_ids(&mut playlist, Some(&options));

    let mixed = &playlist[0].channels[0];
    assert_eq!(mixed.header.epg_channel_id.as_deref(), Some("example.channel"));
    assert_eq!(mixed.header.name.as_ref(), "Mixed Case");
    assert_eq!(mixed.header.title.as_ref(), "Visible Title");
    assert_eq!(mixed.header.group.as_ref(), "Visible Group");
    assert!(Arc::ptr_eq(
        playlist[0].channels[1].header.epg_channel_id.as_ref().expect("lowercase EPG ID should remain"),
        &lowercase_id,
    ));
    assert_eq!(playlist[0].channels[2].header.epg_channel_id.as_deref(), Some(""));
    assert!(playlist[0].channels[3].header.epg_channel_id.is_none());

    let m3u = M3uPlaylistItem::from(mixed);
    let xtream = XtreamPlaylistItem::from(mixed);
    assert_eq!(m3u.epg_channel_id.as_deref(), Some("example.channel"));
    assert_eq!(xtream.epg_channel_id.as_deref(), Some("example.channel"));
    assert!(m3u.to_m3u(None, false).contains(r#"tvg-id="example.channel""#));
}

#[test]
fn target_playlist_epg_normalization_preserves_ids_when_disabled() {
    let options = epg_normalization_options(false);
    let mut playlist = epg_normalization_playlist();

    normalize_target_playlist_epg_ids(&mut playlist, Some(&options));

    assert_eq!(playlist[0].channels[0].header.epg_channel_id.as_deref(), Some("Example.Channel"));
}
