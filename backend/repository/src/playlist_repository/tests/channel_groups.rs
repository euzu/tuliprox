use super::*;
use crate::{persist_playlist_views_with_groups, ChannelGroupOrigin, ChannelGroupStore, PendingChannelGroup};
use shared::model::ChannelGroupPolicy;
use std::path::PathBuf;

fn persist_test_env(_config_json: &str) -> (Arc<AppConfig>, ConfigTarget, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = test_app_config(dir.path());
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: "target-groups-test".to_string(),
        output: vec![TargetOutputDto::M3u(M3uTargetOutputDto {
            filename: Some("playlist.m3u".to_string()),
            ..M3uTargetOutputDto::default()
        })],
        ..ConfigTargetDto::default()
    });
    (app_config, target, dir)
}

fn persist_test_env_rejecting_empty() -> (Arc<AppConfig>, ConfigTarget, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = test_app_config(dir.path());
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: "target-reject-test".to_string(),
        output: vec![TargetOutputDto::M3u(M3uTargetOutputDto {
            filename: Some("playlist.m3u".to_string()),
            ..M3uTargetOutputDto::default()
        })],
        ..ConfigTargetDto::default()
    });
    (app_config, target, dir)
}

fn target_path(app_config: &AppConfig, target: &ConfigTarget) -> PathBuf {
    let config = app_config.config.load();
    get_target_storage_path(&config, &target.name).expect("target storage path")
}

fn grouped_fixture() -> (Vec<PlaylistGroup>, Vec<PendingChannelGroup>) {
    let group_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000001");
    let group_channel = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: group_uuid,
            id: "bbc-one".intern(),
            input_stream_id: "bbc-one".intern(),
            name: "BBC One".intern(),
            title: "BBC One".intern(),
            group: "Entertainment".intern(),
            url: "http://korath/live/101".intern(),
            input_name: "korath".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            channel_group_uuid: Some(group_uuid),
            ..PlaylistItemHeader::default()
        },
    };
    let normal_channel = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000002"),
            id: "cnn".intern(),
            input_stream_id: "cnn".intern(),
            name: "CNN".intern(),
            title: "CNN".intern(),
            group: "News".intern(),
            url: "http://korath/live/cnn".intern(),
            input_name: "korath".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    let member1 = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000011"),
            id: "101".intern(),
            input_stream_id: "101".intern(),
            name: "BBC One FHD".intern(),
            title: "BBC One FHD".intern(),
            group: "Entertainment".intern(),
            url: "http://korath/live/101".intern(),
            input_name: "korath".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    let member2 = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000012"),
            id: "201".intern(),
            input_stream_id: "201".intern(),
            name: "BBC One HD".intern(),
            title: "BBC One HD".intern(),
            group: "Entertainment".intern(),
            url: "http://xandar/live/201".intern(),
            input_name: "xandar".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    let playlist = vec![
        PlaylistGroup {
            id: 1,
            title: "Entertainment".intern(),
            channels: vec![group_channel],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "News".intern(),
            channels: vec![normal_channel],
            xtream_cluster: XtreamCluster::Live,
        },
    ];
    let pending = vec![PendingChannelGroup {
        group_uuid,
        key: Arc::from("bbc one"),
        origin: ChannelGroupOrigin::Auto,
        policy: ChannelGroupPolicy::Priority,
        members: vec![member1, member2],
        orphaned: vec![],
        epg_split: false,
    }];
    (playlist, pending)
}

fn grouped_fixture_ungrouped() -> (Vec<PlaylistGroup>, Vec<PendingChannelGroup>) {
    let member1 = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000011"),
            id: "101".intern(),
            input_stream_id: "101".intern(),
            name: "BBC One FHD".intern(),
            title: "BBC One FHD".intern(),
            group: "Entertainment".intern(),
            url: "http://korath/live/101".intern(),
            input_name: "korath".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    let member2 = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000012"),
            id: "201".intern(),
            input_stream_id: "201".intern(),
            name: "BBC One HD".intern(),
            title: "BBC One HD".intern(),
            group: "Entertainment".intern(),
            url: "http://xandar/live/201".intern(),
            input_name: "xandar".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    let playlist = vec![PlaylistGroup {
        id: 1,
        title: "Entertainment".intern(),
        channels: vec![member1, member2],
        xtream_cluster: XtreamCluster::Live,
    }];
    (playlist, Vec::new())
}

fn grouped_fixture_with_favourite_copy() -> (Vec<PlaylistGroup>, Vec<PendingChannelGroup>) {
    let (mut playlist, pending) = grouped_fixture();
    let group_item = &playlist[0].channels[0];
    let fav_copy = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000099"),
            channel_group_uuid: Some(group_item.header.uuid),
            group: "My Favourites".intern(),
            ..group_item.header.clone()
        },
    };
    playlist.push(PlaylistGroup {
        id: 99,
        title: "My Favourites".intern(),
        channels: vec![fav_copy],
        xtream_cluster: XtreamCluster::Live,
    });
    (playlist, pending)
}

#[tokio::test]
async fn members_get_stable_virtual_ids_and_store_is_written() {
    let (app_config, target, _dir) = persist_test_env(r#"{"channel_groups":{"enabled":true}}"#);
    let (mut playlist, mut pending) = grouped_fixture();
    persist_playlist_views_with_groups(
        &app_config,
        &mut playlist,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .expect("persists");
    let path = target_path(&app_config, &target);
    let group_vid = playlist[0].channels[0].header.virtual_id;
    let record = ChannelGroupStore::read_group(&path, group_vid).expect("reads").expect("exists");
    assert_eq!(record.members.len(), 2);
    assert!(record.members.iter().all(|m| !m.member_vid.is_unset() && m.member_vid != group_vid));
    let (via_member, idx) =
        ChannelGroupStore::read_member(&path, record.members[1].member_vid).expect("reads").expect("exists");
    assert_eq!((via_member.group_vid, idx), (group_vid, 1));

    // second run keeps ids
    let (mut playlist2, mut pending2) = grouped_fixture();
    persist_playlist_views_with_groups(
        &app_config,
        &mut playlist2,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending2,
    )
    .await
    .expect("persists");
    let record2 = ChannelGroupStore::read_group(&path, playlist2[0].channels[0].header.virtual_id).unwrap().unwrap();
    assert_eq!(record.members[0].member_vid, record2.members[0].member_vid);
}

#[tokio::test]
async fn member_keeps_vid_of_previously_ungrouped_channel() {
    let (app_config, target, _dir) = persist_test_env(r#"{"channel_groups":{"enabled":true}}"#);
    let (mut plain, _) = grouped_fixture_ungrouped();
    persist_playlist_views_with_groups(
        &app_config,
        &mut plain,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut [],
    )
    .await
    .unwrap();
    let korath_vid_before =
        plain[0].channels.iter().find(|c| c.header.input_name.as_ref() == "korath").unwrap().header.virtual_id;
    let (mut playlist, mut pending) = grouped_fixture();
    persist_playlist_views_with_groups(
        &app_config,
        &mut playlist,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .unwrap();
    let record =
        ChannelGroupStore::read_group(&target_path(&app_config, &target), playlist[0].channels[0].header.virtual_id)
            .unwrap()
            .unwrap();
    assert!(record.members.iter().any(|m| m.member_vid == korath_vid_before));
}

#[tokio::test]
async fn favourite_copy_of_group_item_is_registered_as_alias() {
    let (app_config, target, _dir) = persist_test_env(r#"{"channel_groups":{"enabled":true}}"#);
    let (mut playlist, mut pending) = grouped_fixture_with_favourite_copy();
    persist_playlist_views_with_groups(
        &app_config,
        &mut playlist,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .unwrap();
    let group_vid = playlist[0].channels[0].header.virtual_id;
    let fav_vid = playlist.iter().find(|g| g.title.as_ref() == "My Favourites").unwrap().channels[0].header.virtual_id;
    assert_ne!(fav_vid, group_vid);
    assert_eq!(ChannelGroupStore::read_alias(&target_path(&app_config, &target), fav_vid).unwrap(), Some(group_vid));
}

#[tokio::test]
async fn rejected_persistence_writes_no_group_store() {
    let (app_config, target, _dir) = persist_test_env_rejecting_empty();
    let mut playlist = vec![];
    let mut pending = grouped_fixture().1;
    assert!(persist_playlist_views_with_groups(
        &app_config,
        &mut playlist,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .is_err());
    assert!(!target_path(&app_config, &target).join("channel_groups.db").exists());
}

#[tokio::test]
async fn target_without_groups_removes_stale_group_store() {
    let (app_config, target, _dir) = persist_test_env(r#"{"channel_groups":{"enabled":true}}"#);
    let (mut playlist, mut pending) = grouped_fixture();
    persist_playlist_views_with_groups(
        &app_config,
        &mut playlist,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .unwrap();
    let (mut plain, _) = grouped_fixture_ungrouped();
    persist_playlist_views_with_groups(
        &app_config,
        &mut plain,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut [],
    )
    .await
    .unwrap();
    assert!(!target_path(&app_config, &target).join("channel_groups.db").exists());
}
