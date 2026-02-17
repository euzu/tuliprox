use crate::model::AppConfig;
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery};
use shared::error::{notify_err, TuliproxError};
use shared::model::{PlaylistGroup, PlaylistItem, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use std::path::Path;
use std::sync::Arc;
use indexmap::IndexMap;
use shared::model::UUIDType;
use crate::repository::xtream_repository::CategoryKey;
use crate::utils::file_exists_async;
use std::collections::HashMap;
use tokio::task;

pub async fn persist_input_library_playlist(
    app_config: &Arc<AppConfig>,
    library_path: &Path,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Result<(), TuliproxError>) {
    if playlist.is_empty() {
        return (playlist, Ok(()));
    }

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
            if let Ok(mut query) = BPlusTreeQuery::<UUIDType, XtreamPlaylistItem>::try_new(&library_path) {
                for (uuid, item) in query.iter() {
                    existing_by_uuid.insert(uuid, item);
                }
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

        tree
            .store(&library_path)
            .map(|_| ())
            .map_err(|err| notify_err!("failed to write local library playlist: {} - {err}", library_path.display()))
    })
    .await
    .map_err(|err| notify_err!("failed to write local library playlist: {} - {err}", library_path_err.display()));

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


pub async fn load_input_local_library_playlist(app_config: &Arc<AppConfig>, lib_path: &Path) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    if file_exists_async(lib_path).await {
        let file_lock = app_config.file_locks.read_lock(lib_path).await;
        let lib_path = lib_path.to_path_buf();
        let lib_path_err = lib_path.clone();

        let groups = task::spawn_blocking(move || -> Result<Vec<PlaylistGroup>, TuliproxError> {
            let _guard = file_lock;
            let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
            if let Ok(mut query) = BPlusTreeQuery::<UUIDType, XtreamPlaylistItem>::try_new(&lib_path) {
                let mut group_cnt = 0;
                for (_, ref item) in query.iter() {
                    let cluster = XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live);
                    let key = (cluster, item.group.clone());
                    groups.entry(key)
                        .or_insert_with(|| {
                            group_cnt += 1;
                            PlaylistGroup {
                                id: group_cnt,
                                title: item.group.clone(),
                                channels: Vec::new(),
                                xtream_cluster: cluster,
                            }
                        })
                        .channels.push(PlaylistItem::from(item));
                }
            }
            Ok(groups.into_values().collect())
        })
        .await
        .map_err(|err| notify_err!("failed to read local library playlist: {} - {err}", lib_path_err.display()))??;

        return Ok(groups);
    }

    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::utils::Internable;
    use shared::model::{EpisodeStreamProperties, VideoStreamDetailProperties, VideoStreamProperties};

    fn video_item(url: &str, added: &str, video: Option<&str>, audio: Option<&str>) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 1,
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
                    ..Default::default()
                }),
                ..Default::default()
            }))),
            item_type: shared::model::PlaylistItemType::LocalVideo,
            category_id: 0,
            input_name: "lib".intern(),
            channel_no: 0,
            source_ordinal: 0,
        }
    }

    fn episode_item(url: &str, added: Option<&str>, video: Option<&str>, audio: Option<&str>) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 2,
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
        }
    }

    #[test]
    fn keeps_movie_probe_when_file_unchanged() {
        let mut new_item = video_item("file:///media/a.mkv", "100", None, None);
        let old_item = video_item(
            "file:///media/a.mkv",
            "100",
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Video(v)) => {
                let details = v.details.expect("details expected");
                assert_eq!(details.video, Some("{\"codec_name\":\"h264\"}".intern()));
                assert_eq!(details.audio, Some("{\"codec_name\":\"aac\"}".intern()));
            }
            _ => panic!("expected video properties"),
        }
    }

    #[test]
    fn does_not_keep_movie_probe_when_mtime_changed() {
        let mut new_item = video_item("file:///media/a.mkv", "200", None, None);
        let old_item = video_item(
            "file:///media/a.mkv",
            "100",
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
        );

        preserve_local_probe_state_if_unchanged(&mut new_item, &old_item);

        match new_item.additional_properties {
            Some(StreamProperties::Video(v)) => {
                let details = v.details.expect("details expected");
                assert!(details.video.is_none());
                assert!(details.audio.is_none());
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
}
