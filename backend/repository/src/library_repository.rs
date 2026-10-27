use crate::{
    bplustree::{BPlusTree, BPlusTreeQuery},
    xtream_repository::CategoryKey,
};
use indexmap::IndexMap;
use shared::{
    error::TuliproxError,
    model::{PlaylistGroup, PlaylistItem, StreamProperties, UUIDType, XtreamPlaylistItem},
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::task;
use tuliprox_core::{model::AppConfig, utils::file_exists_async};

pub async fn persist_input_library_playlist(
    app_config: &Arc<AppConfig>,
    library_path: &Path,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Result<(), TuliproxError>) {
    let file_lock = app_config.file_locks.write_lock(library_path).await;
    let library_path = library_path.to_path_buf();
    let library_path_err = library_path.clone();
    let playlist = Arc::new(playlist);
    let playlist_for_task = Arc::clone(&playlist);

    let result = task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;

        // Keep previously probed technical metadata for unchanged local files.
        let mut existing_by_uuid: HashMap<UUIDType, XtreamPlaylistItem> = HashMap::new();
        if library_path.exists() {
            let mut query = BPlusTreeQuery::<UUIDType, XtreamPlaylistItem>::try_new(&library_path)
                .map_err(|error| repository_read_error(&library_path, error))?;
            for entry in query.iter() {
                let (uuid, item) = entry.map_err(|error| repository_read_error(&library_path, error))?;
                existing_by_uuid.insert(uuid, item);
            }
        }

        let mut tree = BPlusTree::new();
        for pg in playlist_for_task.iter() {
            for item in &pg.channels {
                let mut xtream = XtreamPlaylistItem::from(item);
                if let Some(existing) = existing_by_uuid.get(&item.header.uuid) {
                    preserve_local_probe_state_if_unchanged(&mut xtream, existing);
                }
                tree.insert(item.header.uuid, xtream);
            }
        }

        tree.store(&library_path).map(|_| ()).map_err(|err| {
            TuliproxError::RepositoryLibrary(format!(
                "failed to write local library playlist: {} - {err}",
                library_path.display()
            ))
        })
    })
    .await
    .map_err(|err| {
        TuliproxError::RepositoryLibrary(format!(
            "failed to write local library playlist: {} - {err}",
            library_path_err.display()
        ))
    });

    let playlist = match Arc::try_unwrap(playlist) {
        Ok(playlist) => playlist,
        Err(playlist) => (*playlist).clone(),
    };

    match result {
        Ok(res) => (playlist, res),
        Err(err) => (playlist, Err(err)),
    }
}

fn preserve_local_probe_state_if_unchanged(new_item: &mut XtreamPlaylistItem, old_item: &XtreamPlaylistItem) {
    if new_item.url != old_item.url {
        return;
    }

    match (new_item.additional_properties.as_mut(), old_item.additional_properties.as_ref()) {
        (Some(StreamProperties::Video(new_props)), Some(StreamProperties::Video(old_props))) => {
            // For local movies we store file mtime in `added`; changed mtime means file changed.
            if new_props.added != old_props.added {
                return;
            }
            if let (Some(new_details), Some(old_details)) = (new_props.details.as_mut(), old_props.details.as_ref()) {
                if new_details.video.is_none() {
                    new_details.video.clone_from(&old_details.video);
                }
                if new_details.audio.is_none() {
                    new_details.audio.clone_from(&old_details.audio);
                }
                if new_details.duration_secs.is_none() {
                    new_details.duration_secs.clone_from(&old_details.duration_secs);
                }
                if new_details.bitrate == 0 {
                    new_details.bitrate = old_details.bitrate;
                }
            }
        }
        (Some(StreamProperties::Episode(new_props)), Some(StreamProperties::Episode(old_props))) => {
            // For local series episodes we store file mtime in `added`.
            if new_props.added != old_props.added {
                return;
            }
            if new_props.video.is_none() {
                new_props.video.clone_from(&old_props.video);
            }
            if new_props.audio.is_none() {
                new_props.audio.clone_from(&old_props.audio);
            }
        }
        _ => {}
    }
}

fn repository_read_error(path: &Path, error: impl std::fmt::Display) -> TuliproxError {
    TuliproxError::Repository(format!("failed to read B+Tree database {}: {error}", path.display()))
}

pub async fn load_input_local_library_playlist(
    app_config: &Arc<AppConfig>,
    lib_path: &Path,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    if file_exists_async(lib_path).await {
        let file_lock = app_config.file_locks.read_lock(lib_path).await;
        let lib_path = lib_path.to_path_buf();
        let lib_path_err = lib_path.clone();

        let groups = task::spawn_blocking(move || -> Result<Vec<PlaylistGroup>, TuliproxError> {
            let _guard = file_lock;
            let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
            let mut query = BPlusTreeQuery::<UUIDType, XtreamPlaylistItem>::try_new(&lib_path)
                .map_err(|error| repository_read_error(&lib_path, error))?;
            let mut group_cnt = 0;
            for entry in query.iter() {
                let (_, item) = entry.map_err(|error| repository_read_error(&lib_path, error))?;
                let cluster = item.item_type.cluster();
                let key = (cluster, item.group.clone());
                groups
                    .entry(key)
                    .or_insert_with(|| {
                        group_cnt += 1;
                        PlaylistGroup {
                            id: group_cnt,
                            title: item.group.clone(),
                            channels: Vec::new(),
                            xtream_cluster: cluster,
                        }
                    })
                    .channels
                    .push(PlaylistItem::from(&item));
            }
            Ok(groups.into_values().collect())
        })
        .await
        .map_err(|err| {
            TuliproxError::RepositoryLibrary(format!(
                "failed to read local library playlist: {} - {err}",
                lib_path_err.display()
            ))
        })??;

        return Ok(groups);
    }

    Err(TuliproxError::RepositoryLibrary(format!("Library input is missing: {}", lib_path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{
            EpisodeStreamProperties, VideoStreamDetailProperties, VideoStreamProperties, VirtualId, XtreamCluster,
        },
        utils::Internable,
    };

    #[test]
    fn bplustree_read_errors_are_generic_repository_errors() {
        let error = repository_read_error(Path::new("library.db"), std::io::Error::other("corrupt page"));
        assert!(
            error.kind() == shared::error::ErrorKind::Repository
                && error.message().contains("library.db")
                && error.message().contains("corrupt page")
        );
    }

    fn video_item(
        url: &str,
        added: &str,
        video: Option<&str>,
        audio: Option<&str>,
        duration_secs: Option<&str>,
        bitrate: u32,
    ) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: VirtualId::new(1),
            provider_id: 1,
            name: "movie".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Movies".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: url.intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Video,
            additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                added: added.intern(),
                details: Some(VideoStreamDetailProperties {
                    video: video.map(Internable::intern),
                    audio: audio.map(Internable::intern),
                    duration_secs: duration_secs.map(Internable::intern),
                    bitrate,
                    ..Default::default()
                }),
                ..Default::default()
            }))),
            item_type: shared::model::PlaylistItemType::LocalVideo,
            category_id: 0,
            input_name: "lib".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "1".intern(),
            upstream_user_agent: None,
        }
    }

    fn episode_item(url: &str, added: Option<&str>, video: Option<&str>, audio: Option<&str>) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: VirtualId::new(2),
            provider_id: 2,
            name: "ep".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Series".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: url.intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                episode_id: 0,
                episode: 0,
                season: 0,
                added: added.map(Internable::intern),
                release_date: None,
                series_release_date: None,
                plot: None,
                tmdb: None,
                movie_image: "".intern(),
                container_extension: "".intern(),
                video: video.map(Internable::intern),
                audio: audio.map(Internable::intern),
            }))),
            item_type: shared::model::PlaylistItemType::LocalSeries,
            category_id: 0,
            input_name: "lib".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "2".intern(),
            upstream_user_agent: None,
        }
    }

    #[test]
    fn keeps_movie_probe_when_file_unchanged() {
        let mut new_item = video_item("file:///media/a.mkv", "100", None, None, None, 0);
        let old_item = video_item(
            "file:///media/a.mkv",
            "100",
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some("120"),
            1500,
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Video(v)) => {
                let details = v.details.expect("details expected");
                assert_eq!(details.video, Some("{\"codec_name\":\"h264\"}".intern()));
                assert_eq!(details.audio, Some("{\"codec_name\":\"aac\"}".intern()));
                assert_eq!(details.duration_secs, Some("120".intern()));
                assert_eq!(details.bitrate, 1500);
            }
            _ => panic!("expected video properties"),
        }
    }

    #[test]
    fn does_not_keep_movie_probe_when_mtime_changed() {
        let mut new_item = video_item("file:///media/a.mkv", "200", None, None, None, 0);
        let old_item = video_item(
            "file:///media/a.mkv",
            "100",
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some("120"),
            1500,
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Video(v)) => {
                let details = v.details.expect("details expected");
                assert!(details.video.is_none());
                assert!(details.audio.is_none());
                assert!(details.duration_secs.is_none());
                assert_eq!(details.bitrate, 0);
            }
            _ => panic!("expected video properties"),
        }
    }

    #[test]
    fn keeps_episode_probe_when_file_unchanged() {
        let mut new_item = episode_item("file:///media/s01e01.mkv", Some("100"), None, None);
        let old_item = episode_item(
            "file:///media/s01e01.mkv",
            Some("100"),
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Episode(e)) => {
                assert_eq!(e.video, Some("{\"codec_name\":\"h264\"}".intern()));
                assert_eq!(e.audio, Some("{\"codec_name\":\"aac\"}".intern()));
            }
            _ => panic!("expected episode properties"),
        }
    }

    #[test]
    fn does_not_keep_episode_probe_when_mtime_changed() {
        let mut new_item = episode_item("file:///media/s01e01.mkv", Some("200"), None, None);
        let old_item = episode_item(
            "file:///media/s01e01.mkv",
            Some("100"),
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Episode(e)) => {
                assert!(e.video.is_none());
                assert!(e.audio.is_none());
            }
            _ => panic!("expected episode properties"),
        }
    }

    #[tokio::test]
    async fn media_server_items_keep_distinct_identity_after_persist_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app_config = crate::test_config::app_config_in(dir.path());
        let path = dir.path().join("media_server_input.db");
        let item = |stable_id: &str| {
            let mut item = PlaylistItem::from(&video_item(
                &format!("media-server://jellyfin/server/{stable_id}"),
                "1",
                None,
                None,
                None,
                0,
            ));
            item.header.item_type = shared::model::PlaylistItemType::Video;
            item.header.id = stable_id.intern();
            item.header.input_stream_id = stable_id.intern();
            item.header.gen_uuid();
            item
        };
        let items = vec![item("media-server:s:l:movie:aaaa"), item("media-server:s:l:movie:bbbb")];
        let mut expected: Vec<_> = items.iter().map(|item| *item.header.get_uuid()).collect();
        let group =
            PlaylistGroup { id: 1, title: "Movies".intern(), channels: items, xtream_cluster: XtreamCluster::Video };

        let (_, result) = persist_input_library_playlist(&app_config, &path, vec![group]).await;
        result.expect("persist");
        let loaded = load_input_local_library_playlist(&app_config, &path).await.expect("load");

        let mut loaded_uuids: Vec<_> =
            loaded.iter().flat_map(|group| &group.channels).map(|item| *item.header.get_uuid()).collect();
        loaded_uuids.sort();
        expected.sort();
        assert_eq!(loaded_uuids, expected);
    }
}
