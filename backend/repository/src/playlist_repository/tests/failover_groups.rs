use super::*;
use crate::{
    persist_playlist_views_with_groups, CatalogRecord, FailoverGroupBatch, FailoverGroupMeta, FailoverGroupOrigin,
    FailoverGroupStore, PendingFailoverGroup,
};
use shared::model::FailoverGroupPolicy;
use std::path::PathBuf;

fn persist_test_env(_config_json: &str) -> (Arc<AppConfig>, ConfigTarget, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = app_config_in(dir.path());
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
    let app_config = app_config_in(dir.path());
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

fn grouped_fixture() -> (Vec<PlaylistGroup>, FailoverGroupBatch) {
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
            failover_group_uuid: Some(group_uuid),
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
    let pending = FailoverGroupBatch::with_groups(vec![PendingFailoverGroup {
        group_uuid,
        key: Arc::from("bbc one"),
        origin: FailoverGroupOrigin::Auto,
        policy: FailoverGroupPolicy::Priority,
        members: vec![member1, member2],
        orphaned: vec![],
        epg_split: false,
    }]);
    (playlist, pending)
}

fn grouped_fixture_ungrouped() -> (Vec<PlaylistGroup>, FailoverGroupBatch) {
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
    (playlist, FailoverGroupBatch::default())
}

fn grouped_fixture_with_favourite_copy() -> (Vec<PlaylistGroup>, FailoverGroupBatch) {
    let (mut playlist, pending) = grouped_fixture();
    let group_item = &playlist[0].channels[0];
    let fav_copy = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000099"),
            failover_group_uuid: Some(group_item.header.uuid),
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
    let (app_config, target, _dir) = persist_test_env(r#"{"failover_groups":{"enabled":true}}"#);
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
    let record = FailoverGroupStore::read_group(&path, group_vid).expect("reads").expect("exists");
    assert_eq!(record.members.len(), 2);
    assert!(record.members.iter().all(|m| !m.member_vid.is_unset() && m.member_vid != group_vid));
    let (via_member, idx) =
        FailoverGroupStore::read_member(&path, record.members[1].member_vid).expect("reads").expect("exists");
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
    let record2 = FailoverGroupStore::read_group(&path, playlist2[0].channels[0].header.virtual_id).unwrap().unwrap();
    assert_eq!(record.members[0].member_vid, record2.members[0].member_vid);
}

#[tokio::test]
async fn member_keeps_vid_of_previously_ungrouped_channel() {
    let (app_config, target, _dir) = persist_test_env(r#"{"failover_groups":{"enabled":true}}"#);
    let (mut plain, _) = grouped_fixture_ungrouped();
    persist_playlist_views_with_groups(
        &app_config,
        &mut plain,
        None,
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
        &mut FailoverGroupBatch::default(),
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
        FailoverGroupStore::read_group(&target_path(&app_config, &target), playlist[0].channels[0].header.virtual_id)
            .unwrap()
            .unwrap();
    assert!(record.members.iter().any(|m| m.member_vid == korath_vid_before));
}

#[tokio::test]
async fn favourite_copy_of_group_item_is_registered_as_alias() {
    let (app_config, target, _dir) = persist_test_env(r#"{"failover_groups":{"enabled":true}}"#);
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
    assert_eq!(FailoverGroupStore::read_alias(&target_path(&app_config, &target), fav_vid).unwrap(), Some(group_vid));
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
    assert!(!target_path(&app_config, &target).join("failover_groups.db").exists());
}

#[tokio::test]
async fn target_without_groups_removes_stale_group_store() {
    let (app_config, target, _dir) = persist_test_env(r#"{"failover_groups":{"enabled":true}}"#);
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
        &mut FailoverGroupBatch::default(),
    )
    .await
    .unwrap();
    assert!(!target_path(&app_config, &target).join("failover_groups.db").exists());
}

fn enable_groups(target: &mut ConfigTarget) {
    target.options = Some(shared::model::ConfigTargetOptions {
        failover_groups: Some(shared::model::FailoverGroupsTargetConfig { enabled: true, ..Default::default() }),
        ..Default::default()
    });
}

async fn persist_grouped(app_config: &Arc<AppConfig>, target: &ConfigTarget, state: &Arc<PlaylistStorageState>) {
    let (mut playlist, mut pending) = grouped_fixture();
    persist_playlist_views_with_groups(
        app_config,
        &mut playlist,
        None,
        None,
        target,
        Some(state),
        TargetPlaylistPersistOptions::default(),
        &mut pending,
    )
    .await
    .expect("persists");
}

#[tokio::test]
async fn classify_without_failover_groups_returns_item_without_loading() {
    let (app_config, target, _dir) = persist_test_env("");
    let state = PlaylistStorageState::new();
    let resolved =
        crate::target_stream_item::classify_virtual_id(&app_config, &state, &target, VirtualId::new(1)).await;
    assert_eq!(resolved, crate::target_stream_item::ResolvedStreamTarget::Item);
    assert_eq!(state.failover_groups.loaded_target_count().await, 0);
}

#[tokio::test]
async fn bumping_one_target_keeps_other_target_cache() {
    let (app_config, mut target_a, _dir) = persist_test_env("");
    enable_groups(&mut target_a);
    let mut target_b = target_a.clone();
    target_b.name = "target-groups-other".to_string();
    let state = Arc::new(PlaylistStorageState::new());
    let a_before = state.failover_groups.get_or_load(&app_config, &state, &target_a).await;
    let b_before = state.failover_groups.get_or_load(&app_config, &state, &target_b).await;
    state.bump_failover_group_generation(&target_a.name);
    let a_after = state.failover_groups.get_or_load(&app_config, &state, &target_a).await;
    let b_after = state.failover_groups.get_or_load(&app_config, &state, &target_b).await;
    assert!(!Arc::ptr_eq(&a_before, &a_after));
    assert!(Arc::ptr_eq(&b_before, &b_after));
}

#[tokio::test]
async fn concurrent_loads_read_store_once() {
    let (app_config, mut target, _dir) = persist_test_env("");
    enable_groups(&mut target);
    let state = Arc::new(PlaylistStorageState::new());
    persist_grouped(&app_config, &target, &state).await;
    let loads_before = state.failover_groups.load_count();
    let (first, second) = tokio::join!(
        state.failover_groups.get_or_load(&app_config, &state, &target),
        state.failover_groups.get_or_load(&app_config, &state, &target)
    );
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(state.failover_groups.load_count() - loads_before, 1);
    assert_eq!(first.groups.len(), 1);
}

#[tokio::test]
async fn loads_of_different_targets_do_not_wait_on_each_other() {
    let (app_config, mut target_a, _dir) = persist_test_env("");
    enable_groups(&mut target_a);
    let mut target_b = target_a.clone();
    target_b.name = "target-groups-other".to_string();
    let state = Arc::new(PlaylistStorageState::new());
    let _loading_a = state.failover_groups.hold_load_lock_for_test(&target_a.name).await;
    let loaded_b = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        state.failover_groups.get_or_load(&app_config, &state, &target_b),
    )
    .await;
    assert!(loaded_b.is_ok(), "target B loads while target A is loading");
}

#[tokio::test]
async fn cache_loads_wait_for_the_lock_persistence_and_hot_apply_write() {
    let (app_config, mut target, _dir) = persist_test_env("");
    enable_groups(&mut target);
    let state = Arc::new(PlaylistStorageState::new());
    persist_grouped(&app_config, &target, &state).await;
    state.bump_failover_group_generation(&target.name);
    let mapping_file = crate::get_target_id_mapping_file(&target_path(&app_config, &target));

    let write_lock = app_config.file_locks.write_lock(&mapping_file).await;
    let groups = tokio::spawn({
        let (app_config, state, target) = (Arc::clone(&app_config), Arc::clone(&state), target.clone());
        async move { state.failover_groups.get_or_load(&app_config, &state, &target).await }
    });
    let catalog = tokio::spawn({
        let (app_config, state, target) = (Arc::clone(&app_config), Arc::clone(&state), target.clone());
        async move { state.failover_groups.get_or_load_catalog(&app_config, &state, &target).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!groups.is_finished(), "the group load must wait for the target write lock");
    assert!(!catalog.is_finished(), "the catalog load must wait for the target write lock");

    drop(write_lock);
    assert_eq!(groups.await.expect("joins").groups.len(), 1);
    assert!(catalog.await.expect("joins").is_ok());
}

fn catalog_record(stream_id: &str, title: &str) -> CatalogRecord {
    CatalogRecord {
        member_ref: shared::model::ChannelMemberRefDto {
            input: "korath".to_string(),
            stream_id: stream_id.to_string(),
        },
        natural_key: Some("bbc one".intern()),
        dsl_forced: false,
        epg_channel_id: Some("bbc1.uk".intern()),
        name: title.intern(),
        title: title.intern(),
        logo: "http://logo/bbc.png".intern(),
        playlist_group: "Entertainment".intern(),
        provider_id: 101,
        has_archive: true,
    }
}

fn sample_meta() -> FailoverGroupMeta {
    FailoverGroupMeta {
        overrides_etag: Some("etag-1".intern()),
        overrides_error: Some("broken file".to_string()),
        applied_overrides: Some(shared::model::TargetFailoverOverridesDto {
            target: "target-groups-test".to_string(),
            groups: Vec::new(),
            auto_overrides: Vec::new(),
            excluded: vec![shared::model::ChannelMemberRefDto {
                input: "korath".to_string(),
                stream_id: "101".to_string(),
            }],
        }),
        display_stale_groups: vec!["bbc one".to_string()],
    }
}

fn enabled_batch(mut batch: FailoverGroupBatch) -> FailoverGroupBatch {
    batch.enabled = true;
    batch.catalog = vec![catalog_record("101", "BBC One FHD"), catalog_record("201", "BBC One HD")];
    batch.meta = sample_meta();
    batch
}

async fn persist_batch(
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    batch: &mut FailoverGroupBatch,
) -> Result<(), Vec<shared::error::TuliproxError>> {
    persist_playlist_views_with_groups(
        app_config,
        playlist,
        None,
        None,
        target,
        None,
        TargetPlaylistPersistOptions::default(),
        batch,
    )
    .await
}

fn stage_leftovers(path: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".stage-"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn catalog_cache_loads_once_per_generation() {
    let (app_config, mut target, _dir) = persist_test_env("");
    enable_groups(&mut target);
    let state = Arc::new(PlaylistStorageState::new());
    let (mut playlist, batch) = grouped_fixture();
    let mut batch = enabled_batch(batch);
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");

    let loads_before = state.failover_groups.catalog_load_count();
    let first = state.failover_groups.get_or_load_catalog(&app_config, &state, &target).await.expect("loads");
    let second = state.failover_groups.get_or_load_catalog(&app_config, &state, &target).await.expect("cached");
    assert_eq!(first.as_ref(), batch.catalog.as_slice());
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(state.failover_groups.catalog_load_count() - loads_before, 1);

    state.bump_failover_group_generation(&target.name);
    let third = state.failover_groups.get_or_load_catalog(&app_config, &state, &target).await.expect("reloads");
    assert!(!Arc::ptr_eq(&first, &third));
    assert_eq!(state.failover_groups.catalog_load_count() - loads_before, 2);
}

#[tokio::test]
async fn catalog_cache_is_empty_without_catalog_file() {
    let (app_config, mut target, _dir) = persist_test_env("");
    enable_groups(&mut target);
    let state = Arc::new(PlaylistStorageState::new());
    let catalog = state.failover_groups.get_or_load_catalog(&app_config, &state, &target).await.expect("loads");
    assert!(catalog.is_empty());
}

#[tokio::test]
async fn catalog_and_meta_published_with_groups() {
    let (app_config, target, _dir) = persist_test_env("");
    let (mut playlist, batch) = grouped_fixture();
    let mut batch = enabled_batch(batch);
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");

    let path = target_path(&app_config, &target);
    assert!(path.join("failover_groups.db").exists());
    assert!(path.join("failover_groups.catalog.db").exists());
    assert!(path.join("failover_groups.meta.json").exists());
    assert_eq!(stage_leftovers(&path).len(), 0);
    assert_eq!(FailoverGroupStore::read_catalog(&path).expect("catalog"), batch.catalog);
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta"), batch.meta);

    let data = FailoverGroupStore::load_target_data(&path, 7).expect("loads");
    assert_eq!(data.meta, batch.meta);
    assert_eq!(data.groups.len(), 1);
}

#[tokio::test]
async fn catalog_written_without_groups() {
    let (app_config, target, _dir) = persist_test_env("");
    let (mut playlist, _) = grouped_fixture_ungrouped();
    let mut batch = enabled_batch(FailoverGroupBatch::default());
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");

    let path = target_path(&app_config, &target);
    assert!(!path.join("failover_groups.db").exists());
    assert!(!path.join("failover_groups.member.db").exists());
    assert_eq!(FailoverGroupStore::read_catalog(&path).expect("catalog"), batch.catalog);
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta"), batch.meta);

    let (mut playlist, _) = grouped_fixture_ungrouped();
    let mut empty = FailoverGroupBatch { enabled: true, ..FailoverGroupBatch::default() };
    persist_batch(&app_config, &target, &mut playlist, &mut empty).await.expect("persists");
    assert!(path.join("failover_groups.catalog.db").exists());
    assert_eq!(FailoverGroupStore::read_catalog(&path).expect("catalog").len(), 0);
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta"), FailoverGroupMeta::default());
}

#[tokio::test]
async fn rejected_persistence_writes_no_catalog_or_meta() {
    let (app_config, target, _dir) = persist_test_env_rejecting_empty();
    let mut playlist = vec![];
    let mut batch = enabled_batch(grouped_fixture().1);
    assert!(persist_batch(&app_config, &target, &mut playlist, &mut batch).await.is_err());
    let path = target_path(&app_config, &target);
    assert!(!path.join("failover_groups.catalog.db").exists());
    assert!(!path.join("failover_groups.meta.json").exists());
    assert_eq!(stage_leftovers(&path).len(), 0);
}

#[cfg(unix)]
#[test]
fn failed_stage_leaves_no_catalog_or_meta() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if std::fs::File::create(locked.join("probe")).is_ok() {
        // Permission bits are not enforced (root), so the stage cannot be made to fail.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return;
    }

    let catalog = vec![catalog_record("101", "BBC One FHD")];
    let meta = sample_meta();
    let result = FailoverGroupStore::write_staged(
        &locked,
        &[],
        &[],
        Some(crate::FailoverGroupSideFiles { catalog: &catalog, meta: &meta }),
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(&locked).expect("readdir").count(), 0);
}

#[test]
fn dropped_stage_removes_catalog_and_meta_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = vec![catalog_record("101", "BBC One FHD")];
    let meta = sample_meta();
    let staged = FailoverGroupStore::write_staged(
        dir.path(),
        &[],
        &[],
        Some(crate::FailoverGroupSideFiles { catalog: &catalog, meta: &meta }),
    )
    .expect("staged");
    assert_eq!(stage_leftovers(dir.path()).len(), 2);
    assert!(!dir.path().join("failover_groups.catalog.db").exists());
    staged.discard();
    assert_eq!(stage_leftovers(dir.path()).len(), 0);
}

#[tokio::test]
async fn disabled_target_removes_catalog_and_meta() {
    let (app_config, target, _dir) = persist_test_env("");
    let (mut playlist, batch) = grouped_fixture();
    let mut batch = enabled_batch(batch);
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");
    let path = target_path(&app_config, &target);
    assert!(path.join("failover_groups.catalog.db").exists());

    let (mut plain, _) = grouped_fixture_ungrouped();
    persist_batch(&app_config, &target, &mut plain, &mut FailoverGroupBatch::default()).await.expect("persists");
    for name in
        ["failover_groups.db", "failover_groups.member.db", "failover_groups.catalog.db", "failover_groups.meta.json"]
    {
        assert!(!path.join(name).exists(), "{name} removed");
    }
    assert_eq!(FailoverGroupStore::read_catalog(&path).expect("catalog").len(), 0);
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta"), FailoverGroupMeta::default());
}

#[tokio::test]
async fn meta_records_overrides_etag_and_error() {
    let (app_config, target, _dir) = persist_test_env("");
    let (mut playlist, _) = grouped_fixture_ungrouped();
    let mut batch = FailoverGroupBatch {
        enabled: true,
        meta: FailoverGroupMeta { overrides_etag: Some("abc".intern()), ..FailoverGroupMeta::default() },
        ..FailoverGroupBatch::default()
    };
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");
    let path = target_path(&app_config, &target);
    let meta = FailoverGroupStore::read_meta(&path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("abc"));
    assert_eq!(meta.overrides_error, None);
    assert_eq!(meta.applied_overrides, None);
    let json = std::fs::read_to_string(path.join("failover_groups.meta.json")).expect("json");
    assert!(!json.contains("overrides_error") && !json.contains("applied_overrides"), "{json}");

    let (mut playlist, _) = grouped_fixture_ungrouped();
    let mut batch = enabled_batch(FailoverGroupBatch::default());
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");
    let meta = FailoverGroupStore::read_meta(&path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("etag-1"));
    assert_eq!(meta.overrides_error.as_deref(), Some("broken file"));
    assert_eq!(meta.applied_overrides.as_ref().map(|o| o.excluded.len()), Some(1));
}

#[tokio::test]
async fn publication_clears_display_stale() {
    let (app_config, target, _dir) = persist_test_env("");
    let (mut playlist, batch) = grouped_fixture();
    let mut batch = enabled_batch(batch);
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");
    let path = target_path(&app_config, &target);
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta").display_stale_groups, vec!["bbc one".to_string()]);

    let (mut playlist, batch) = grouped_fixture();
    let mut batch = FailoverGroupBatch { enabled: true, meta: FailoverGroupMeta::default(), ..batch };
    persist_batch(&app_config, &target, &mut playlist, &mut batch).await.expect("persists");
    assert_eq!(FailoverGroupStore::read_meta(&path).expect("meta").display_stale_groups.len(), 0);
    let json = std::fs::read_to_string(path.join("failover_groups.meta.json")).expect("json");
    assert!(!json.contains("display_stale_groups"), "{json}");
}
