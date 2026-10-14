use crate::{ChannelGroupRecord, PlaylistStorageState};
use shared::{
    model::{M3uPlaylistItem, VirtualId, XtreamPlaylistItem},
    utils::Internable,
};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, ConfigTarget};

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedStreamTarget {
    Item,
    Group(Arc<ChannelGroupRecord>),
    Member { group: Arc<ChannelGroupRecord>, index: usize },
}

pub async fn classify_virtual_id(
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    vid: VirtualId,
) -> ResolvedStreamTarget {
    let target_data = playlists.channel_groups.get_or_load(app_config, playlists, target).await;
    if let Some(group) = target_data.groups.get(&vid) {
        return ResolvedStreamTarget::Group(Arc::clone(group));
    }
    if let Some(group_vid) = target_data.aliases.get(&vid) {
        if let Some(group) = target_data.groups.get(group_vid) {
            return ResolvedStreamTarget::Group(Arc::clone(group));
        }
    }
    if let Some((group_vid, index)) = target_data.members.get(&vid) {
        if let Some(group) = target_data.groups.get(group_vid) {
            return ResolvedStreamTarget::Member { group: Arc::clone(group), index: *index };
        }
    }
    ResolvedStreamTarget::Item
}

#[must_use]
pub fn member_to_xtream_item(group: &ChannelGroupRecord, index: usize, target: &ConfigTarget) -> XtreamPlaylistItem {
    let _ = target;
    let member = &group.members[index];
    XtreamPlaylistItem {
        virtual_id: member.member_vid,
        provider_id: member.provider_id,
        name: Arc::clone(&member.title),
        logo: "".intern(),
        logo_small: "".intern(),
        group: Arc::clone(&group.key),
        title: Arc::clone(&member.title),
        parent_code: "".intern(),
        rec: "".intern(),
        url: Arc::clone(&member.url),
        epg_channel_id: member.epg_channel_id.as_ref().map(Arc::clone),
        xtream_cluster: member.cluster,
        additional_properties: member.additional_properties.clone(),
        item_type: member.item_type,
        category_id: 0,
        input_name: Arc::clone(&member.input_name),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: Arc::clone(&member.input_stream_id),
        upstream_user_agent: None,
    }
}

#[must_use]
pub fn member_to_m3u_item(group: &ChannelGroupRecord, index: usize, target: &ConfigTarget) -> M3uPlaylistItem {
    let _ = target;
    let member = &group.members[index];
    M3uPlaylistItem {
        virtual_id: member.member_vid,
        provider_id: member.provider_id.to_string().intern(),
        name: Arc::clone(&member.title),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: Arc::clone(&group.key),
        title: Arc::clone(&member.title),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: Arc::clone(&member.url),
        epg_channel_id: member.epg_channel_id.as_ref().map(Arc::clone),
        input_name: Arc::clone(&member.input_name),
        item_type: member.item_type,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
        input_stream_id: Arc::clone(&member.input_stream_id),
        upstream_user_agent: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        persist_playlist_views_with_groups, ChannelGroupOrigin, ChannelGroupStore, PendingChannelGroup,
        PlaylistStorageState, TargetPlaylistPersistOptions,
    };
    use arc_swap::ArcSwap;
    use shared::model::{
        ChannelGroupPolicy, ConfigPaths, ConfigTargetDto, M3uTargetOutputDto, PlaylistGroup, PlaylistItem,
        PlaylistItemHeader, PlaylistItemType, TargetOutputDto, UUIDType, XtreamCluster,
    };
    use std::path::PathBuf;
    use tuliprox_core::model::{AppConfig, Config, SourcesConfig};

    struct PersistedGroupEnv {
        app_config: Arc<AppConfig>,
        playlists: Arc<PlaylistStorageState>,
        target: ConfigTarget,
        _dir: tempfile::TempDir,
        group_vid: VirtualId,
        plain_vid: VirtualId,
        member_vids: Vec<VirtualId>,
        favourite_vid: VirtualId,
    }

    impl PersistedGroupEnv {
        async fn republish_without_groups(&mut self) {
            let (mut plain, _) = grouped_fixture_ungrouped();
            persist_playlist_views_with_groups(
                &self.app_config,
                &mut plain,
                None,
                None,
                &self.target,
                Some(&self.playlists),
                TargetPlaylistPersistOptions::default(),
                &mut [],
            )
            .await
            .expect("republished");
        }
    }

    fn test_app_config(storage_dir: &std::path::Path) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                storage_dir: storage_dir.to_string_lossy().into_owned(),
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::default()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::default()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
        })
    }

    fn target_path(app_config: &AppConfig, target: &ConfigTarget) -> PathBuf {
        let config = app_config.config.load();
        crate::get_target_storage_path(&config, &target.name).expect("target storage path")
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
        let member2 = PlaylistItem {
            header: PlaylistItemHeader {
                uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000012"),
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
        let (mut playlist, _) = grouped_fixture();
        playlist.remove(0);
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

    async fn persisted_group_env() -> PersistedGroupEnv {
        let dir = tempfile::tempdir().expect("tempdir");
        let app_config = test_app_config(dir.path());
        let playlists = Arc::new(PlaylistStorageState::new());
        let target = ConfigTarget::from(&ConfigTargetDto {
            name: "target-group-env".to_string(),
            output: vec![TargetOutputDto::M3u(M3uTargetOutputDto {
                filename: Some("playlist.m3u".to_string()),
                ..M3uTargetOutputDto::default()
            })],
            ..ConfigTargetDto::default()
        });
        let (mut playlist, mut pending) = grouped_fixture();
        persist_playlist_views_with_groups(
            &app_config,
            &mut playlist,
            None,
            None,
            &target,
            Some(&playlists),
            TargetPlaylistPersistOptions::default(),
            &mut pending,
        )
        .await
        .expect("persists");
        let group_vid = playlist[0].channels[0].header.virtual_id;
        let plain_vid = playlist[1].channels[0].header.virtual_id;
        let path = target_path(&app_config, &target);
        let record = ChannelGroupStore::read_group(&path, group_vid).expect("read").expect("group");
        let member_vids = record.members.iter().map(|m| m.member_vid).collect();
        PersistedGroupEnv {
            app_config,
            playlists,
            target,
            _dir: dir,
            group_vid,
            plain_vid,
            member_vids,
            favourite_vid: VirtualId::default(),
        }
    }

    async fn persisted_group_env_with_favourite() -> PersistedGroupEnv {
        let dir = tempfile::tempdir().expect("tempdir");
        let app_config = test_app_config(dir.path());
        let playlists = Arc::new(PlaylistStorageState::new());
        let target = ConfigTarget::from(&ConfigTargetDto {
            name: "target-fav-env".to_string(),
            output: vec![TargetOutputDto::M3u(M3uTargetOutputDto {
                filename: Some("playlist.m3u".to_string()),
                ..M3uTargetOutputDto::default()
            })],
            ..ConfigTargetDto::default()
        });
        let (mut playlist, mut pending) = grouped_fixture_with_favourite_copy();
        persist_playlist_views_with_groups(
            &app_config,
            &mut playlist,
            None,
            None,
            &target,
            Some(&playlists),
            TargetPlaylistPersistOptions::default(),
            &mut pending,
        )
        .await
        .expect("persists");
        let group_vid = playlist[0].channels[0].header.virtual_id;
        let plain_vid = playlist[1].channels[0].header.virtual_id;
        let favourite_vid =
            playlist.iter().find(|g| g.title.as_ref() == "My Favourites").unwrap().channels[0].header.virtual_id;
        let path = target_path(&app_config, &target);
        let record = ChannelGroupStore::read_group(&path, group_vid).expect("read").expect("group");
        let member_vids = record.members.iter().map(|m| m.member_vid).collect();
        PersistedGroupEnv { app_config, playlists, target, _dir: dir, group_vid, plain_vid, member_vids, favourite_vid }
    }

    #[tokio::test]
    async fn classify_distinguishes_item_group_and_member() {
        let env = persisted_group_env().await;
        assert!(matches!(
            classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.plain_vid).await,
            ResolvedStreamTarget::Item
        ));
        assert!(matches!(
            classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.group_vid).await,
            ResolvedStreamTarget::Group(_)
        ));
        match classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.member_vids[1]).await {
            ResolvedStreamTarget::Member { group, index } => {
                assert_eq!(group.group_vid, env.group_vid);
                assert_eq!(index, 1);
            }
            other => panic!("expected member, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn favourite_alias_vid_resolves_to_group() {
        let env = persisted_group_env_with_favourite().await;
        match classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.favourite_vid).await {
            ResolvedStreamTarget::Group(group) => assert_eq!(group.group_vid, env.group_vid),
            other => panic!("expected group, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn member_item_carries_member_input_and_url() {
        let env = persisted_group_env().await;
        let ResolvedStreamTarget::Group(group) =
            classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.group_vid).await
        else {
            panic!()
        };
        let item = member_to_xtream_item(&group, 1, &env.target);
        assert_eq!(item.virtual_id, env.member_vids[1]);
        assert_eq!(item.input_name.as_ref(), "korath");
        assert!(item.url.contains("korath"));
    }

    #[tokio::test]
    async fn cache_reloads_after_republish() {
        let mut env = persisted_group_env().await;
        let _ = classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.group_vid).await;
        env.republish_without_groups().await;
        assert!(matches!(
            classify_virtual_id(&env.app_config, &env.playlists, &env.target, env.group_vid).await,
            ResolvedStreamTarget::Item
        ));
    }
}
