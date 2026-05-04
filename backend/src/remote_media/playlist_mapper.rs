use crate::remote_media::{RemoteCatalogSnapshot, RemoteEpisode, RemoteMovie, RemoteStreamRef};
use shared::{
    model::{
        EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, StreamProperties,
        VideoStreamProperties, XtreamCluster,
    },
    utils::{generate_provider_playlist_uuid, Internable},
};
use std::{fmt::Write as _, sync::Arc};

pub fn remote_catalog_snapshot_to_playlist(snapshot: &RemoteCatalogSnapshot) -> Vec<PlaylistGroup> {
    let mut groups = Vec::new();

    if !snapshot.movies.is_empty() {
        groups.push(PlaylistGroup {
            id: 1,
            title: "Remote Movies".intern(),
            channels: snapshot.movies.iter().map(remote_movie_to_playlist_item).collect(),
            xtream_cluster: XtreamCluster::Video,
        });
    }

    if !snapshot.episodes.is_empty() {
        groups.push(PlaylistGroup {
            id: next_group_id(groups.len()),
            title: "Remote Series".intern(),
            channels: snapshot.episodes.iter().map(remote_episode_to_playlist_item).collect(),
            xtream_cluster: XtreamCluster::Series,
        });
    }

    groups
}

fn next_group_id(group_count: usize) -> u32 { u32::try_from(group_count.saturating_add(1)).unwrap_or(u32::MAX) }

fn remote_movie_to_playlist_item(movie: &RemoteMovie) -> PlaylistItem {
    let stable_id = stable_remote_item_id(&movie.server_id, &movie.library_id, &movie.item_id, "movie");
    let url = movie.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "remote://unavailable/{}/{}/{}",
                escape_internal_url_component(&movie.server_id),
                escape_internal_url_component(&movie.library_id),
                escape_internal_url_component(&movie.item_id)
            )
        },
        remote_stream_ref_to_internal_url,
    );
    let uuid = generate_provider_playlist_uuid(&movie.input_name, &stable_id, PlaylistItemType::Video);

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: movie.title.clone(),
            title: movie.title.clone(),
            group: "Remote Movies".intern(),
            url: url.intern(),
            input_name: movie.input_name.clone(),
            xtream_cluster: XtreamCluster::Video,
            item_type: PlaylistItemType::Video,
            additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                name: movie.title.clone(),
                stream_id: 0,
                stream_icon: "".intern(),
                direct_source: "".intern(),
                category_id: 0,
                custom_sid: None,
                added: movie.source_version_hint.clone().unwrap_or_else(|| "".intern()),
                container_extension: "".intern(),
                rating: None,
                rating_5based: None,
                stream_type: Some("movie".intern()),
                trailer: None,
                tmdb: None,
                is_adult: 0,
                details: None,
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn remote_episode_to_playlist_item(episode: &RemoteEpisode) -> PlaylistItem {
    let stable_id = stable_remote_item_id(&episode.server_id, &episode.library_id, &episode.item_id, "episode");
    let url = episode.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "remote://unavailable/{}/{}/{}",
                escape_internal_url_component(&episode.server_id),
                escape_internal_url_component(&episode.library_id),
                escape_internal_url_component(&episode.item_id)
            )
        },
        remote_stream_ref_to_internal_url,
    );
    let uuid = generate_provider_playlist_uuid(&episode.input_name, &stable_id, PlaylistItemType::Series);
    let title = if episode.title.is_empty() {
        episode.series_title.clone().unwrap_or_else(|| "Remote Episode".intern())
    } else {
        episode.title.clone()
    };

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: title.clone(),
            title,
            group: "Remote Series".intern(),
            parent_code: episode.series_id.clone().unwrap_or_else(|| "".intern()),
            url: url.intern(),
            input_name: episode.input_name.clone(),
            xtream_cluster: XtreamCluster::Series,
            item_type: PlaylistItemType::Series,
            additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                episode_id: 0,
                episode: episode.episode.unwrap_or_default(),
                season: episode.season.unwrap_or_default(),
                added: episode.source_version_hint.clone(),
                release_date: None,
                series_release_date: None,
                tmdb: None,
                movie_image: "".intern(),
                container_extension: "".intern(),
                video: None,
                audio: None,
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn stable_remote_item_id(server_id: &Arc<str>, library_id: &Arc<str>, item_id: &Arc<str>, kind: &str) -> String {
    format!("remote:{server_id}:{library_id}:{kind}:{item_id}")
}

pub fn remote_stream_ref_to_internal_url(stream_ref: &RemoteStreamRef) -> String {
    match stream_ref {
        RemoteStreamRef::Emby { server_id, item_id, media_source_id, .. } => {
            format!(
                "remote://emby/{}/{}{}",
                escape_internal_url_component(server_id),
                escape_internal_url_component(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", escape_internal_url_component(id)))
                    .unwrap_or_default()
            )
        }
        RemoteStreamRef::Jellyfin { server_id, item_id, media_source_id, .. } => {
            format!(
                "remote://jellyfin/{}/{}{}",
                escape_internal_url_component(server_id),
                escape_internal_url_component(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", escape_internal_url_component(id)))
                    .unwrap_or_default()
            )
        }
        RemoteStreamRef::Plex { server_id, rating_key, part_key, .. } => format!(
            "remote://plex/{}/{}?part_key={}",
            escape_internal_url_component(server_id),
            escape_internal_url_component(rating_key),
            escape_internal_url_component(part_key)
        ),
    }
}

fn escape_internal_url_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_media::{RemoteCatalogSnapshot, RemoteProviderIdHint};

    fn movie() -> RemoteMovie {
        RemoteMovie {
            input_name: "remote".into(),
            server_id: "server/one".into(),
            library_id: "movies".into(),
            item_id: "item?one plus+space".into(),
            title: "Movie".into(),
            year: Some(2024),
            source_version_hint: None,
            provider_hints: Vec::<RemoteProviderIdHint>::new(),
            stream_ref: Some(RemoteStreamRef::Emby {
                input_name: "remote".into(),
                server_id: "server/one".into(),
                item_id: "item?one plus+space".into(),
                media_source_id: Some("media/source".into()),
            }),
            image_ref: None,
        }
    }

    fn episode() -> RemoteEpisode {
        RemoteEpisode {
            input_name: "remote".into(),
            server_id: "server".into(),
            library_id: "shows".into(),
            item_id: "episode".into(),
            series_id: Some("series".into()),
            series_title: Some("Show".into()),
            title: "Episode".into(),
            season: Some(1),
            episode: Some(2),
            source_version_hint: None,
            provider_hints: Vec::<RemoteProviderIdHint>::new(),
            stream_ref: Some(RemoteStreamRef::Plex {
                input_name: "remote".into(),
                server_id: "server".into(),
                rating_key: "rating".into(),
                part_key: "/library/parts/redacted/file.mkv".into(),
            }),
            image_ref: None,
        }
    }

    #[test]
    fn maps_remote_movies_and_episodes_to_playlist_groups_without_virtual_ids() {
        let groups = remote_catalog_snapshot_to_playlist(&RemoteCatalogSnapshot {
            movies: vec![movie()],
            episodes: vec![episode()],
            ..RemoteCatalogSnapshot::default()
        });

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].xtream_cluster, XtreamCluster::Video);
        assert_eq!(groups[0].channels[0].header.item_type, PlaylistItemType::Video);
        assert_eq!(groups[0].channels[0].header.virtual_id, 0);
        assert!(groups[0].channels[0].header.id.starts_with("remote:server/one:movies:movie:item?one plus+space"));
        assert!(groups[0].channels[0].header.url.contains("remote://emby/server%2Fone/item%3Fone%20plus%2Bspace"));
        assert_eq!(groups[1].channels[0].header.item_type, PlaylistItemType::Series);
        assert!(groups[1].channels[0].header.url.contains("part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv"));
    }
}
