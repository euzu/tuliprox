use crate::media_server::{
    MediaServerAudioTechnicalFacts, MediaServerCatalogSnapshot, MediaServerEpisode, MediaServerMovie,
    MediaServerProviderIdHint, MediaServerStreamRef, MediaServerTechnicalFacts, MediaServerVideoTechnicalFacts,
};
use serde_json::{Map, Number, Value};
use shared::{
    model::{
        EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, StreamProperties,
        VideoStreamDetailProperties, VideoStreamProperties, XtreamCluster,
    },
    utils::{generate_provider_playlist_uuid, Internable},
};
use std::{fmt::Write as _, sync::Arc};

pub fn media_server_catalog_snapshot_to_playlist(snapshot: &MediaServerCatalogSnapshot) -> Vec<PlaylistGroup> {
    let mut groups = Vec::new();

    if !snapshot.movies.is_empty() {
        groups.push(PlaylistGroup {
            id: 1,
            title: "Media Server Movies".intern(),
            channels: snapshot.movies.iter().map(media_server_movie_to_playlist_item).collect(),
            xtream_cluster: XtreamCluster::Video,
        });
    }

    if !snapshot.episodes.is_empty() {
        groups.push(PlaylistGroup {
            id: next_group_id(groups.len()),
            title: "Media Server Series".intern(),
            channels: snapshot.episodes.iter().map(media_server_episode_to_playlist_item).collect(),
            xtream_cluster: XtreamCluster::Series,
        });
    }

    groups
}

fn next_group_id(group_count: usize) -> u32 { u32::try_from(group_count.saturating_add(1)).unwrap_or(u32::MAX) }

fn media_server_movie_to_playlist_item(movie: &MediaServerMovie) -> PlaylistItem {
    let stable_id = stable_media_server_item_id(&movie.server_id, &movie.library_id, &movie.item_id, "movie");
    let url = movie.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "media-server://unavailable/{}/{}/{}",
                escape_internal_url_component(&movie.server_id),
                escape_internal_url_component(&movie.library_id),
                escape_internal_url_component(&movie.item_id)
            )
        },
        media_server_stream_ref_to_internal_url,
    );
    let uuid = generate_provider_playlist_uuid(&movie.input_name, &stable_id, PlaylistItemType::Video);
    let release_date = movie.release_date.clone().or_else(|| release_date_from_year(movie.year));
    let details = movie_details(movie.technical_facts.as_ref(), release_date);

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: movie.title.clone(),
            title: movie.title.clone(),
            group: "Media Server Movies".intern(),
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
                container_extension: media_server_container_extension(movie.technical_facts.as_ref()),
                rating: None,
                rating_5based: None,
                stream_type: Some("movie".intern()),
                trailer: None,
                tmdb: provider_tmdb_id(&movie.provider_hints),
                is_adult: 0,
                details,
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn media_server_episode_to_playlist_item(episode: &MediaServerEpisode) -> PlaylistItem {
    let stable_id = stable_media_server_item_id(&episode.server_id, &episode.library_id, &episode.item_id, "episode");
    let url = episode.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "media-server://unavailable/{}/{}/{}",
                escape_internal_url_component(&episode.server_id),
                escape_internal_url_component(&episode.library_id),
                escape_internal_url_component(&episode.item_id)
            )
        },
        media_server_stream_ref_to_internal_url,
    );
    let uuid = generate_provider_playlist_uuid(&episode.input_name, &stable_id, PlaylistItemType::Series);
    let title = if episode.title.is_empty() {
        episode.series_title.clone().unwrap_or_else(|| "Media Server Episode".intern())
    } else {
        episode.title.clone()
    };

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: title.clone(),
            title,
            group: "Media Server Series".intern(),
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
                release_date: episode.release_date.clone(),
                series_release_date: None,
                tmdb: provider_tmdb_id(&episode.provider_hints),
                movie_image: "".intern(),
                container_extension: media_server_container_extension(episode.technical_facts.as_ref()),
                video: episode.technical_facts.as_ref().and_then(media_server_video_json),
                audio: episode.technical_facts.as_ref().and_then(media_server_audio_json),
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn movie_details(
    technical: Option<&MediaServerTechnicalFacts>,
    release_date: Option<Arc<str>>,
) -> Option<VideoStreamDetailProperties> {
    let video = technical.and_then(media_server_video_json);
    let audio = technical.and_then(media_server_audio_json);
    let duration_secs = technical.and_then(|facts| facts.duration_secs).map(|duration| Arc::<str>::from(duration.to_string()));
    let bitrate = technical.and_then(|facts| facts.bitrate).unwrap_or_default();

    if release_date.is_none() && video.is_none() && audio.is_none() && duration_secs.is_none() && bitrate == 0 {
        return None;
    }

    Some(VideoStreamDetailProperties {
        release_date,
        video,
        audio,
        duration_secs,
        bitrate,
        ..VideoStreamDetailProperties::default()
    })
}

fn provider_tmdb_id(hints: &[MediaServerProviderIdHint]) -> Option<u32> {
    hints.iter().find_map(|hint| {
        if !hint.namespace.eq_ignore_ascii_case("tmdb") {
            return None;
        }
        let value = hint.value.trim();
        let parsed = value.parse::<u32>().ok()?;
        (parsed > 0).then_some(parsed)
    })
}

fn release_date_from_year(year: Option<u32>) -> Option<Arc<str>> {
    year.filter(|year| *year > 0).map(|year| Arc::<str>::from(format!("{year}-01-01")))
}

fn media_server_container_extension(technical: Option<&MediaServerTechnicalFacts>) -> Arc<str> {
    technical
        .and_then(|facts| facts.container.as_ref())
        .map(|container| container.trim().trim_start_matches('.'))
        .filter(|container| !container.is_empty())
        .map_or_else(|| "".intern(), Internable::intern)
}

fn media_server_video_json(technical: &MediaServerTechnicalFacts) -> Option<Arc<str>> {
    technical.video.as_ref().and_then(video_technical_facts_json)
}

fn media_server_audio_json(technical: &MediaServerTechnicalFacts) -> Option<Arc<str>> {
    technical.audio.as_ref().and_then(audio_technical_facts_json)
}

fn video_technical_facts_json(video: &MediaServerVideoTechnicalFacts) -> Option<Arc<str>> {
    let mut fields = Map::new();
    fields.insert("codec_type".to_string(), Value::String("video".to_string()));
    insert_non_blank_string(&mut fields, "codec_name", video.codec.as_deref());
    insert_u32(&mut fields, "width", video.width);
    insert_u32(&mut fields, "height", video.height);

    (fields.len() > 1).then(|| Arc::<str>::from(Value::Object(fields).to_string()))
}

fn audio_technical_facts_json(audio: &MediaServerAudioTechnicalFacts) -> Option<Arc<str>> {
    let mut fields = Map::new();
    fields.insert("codec_type".to_string(), Value::String("audio".to_string()));
    insert_non_blank_string(&mut fields, "codec_name", audio.codec.as_deref());
    insert_u32(&mut fields, "channels", audio.channels);

    (fields.len() > 1).then(|| Arc::<str>::from(Value::Object(fields).to_string()))
}

fn insert_non_blank_string(fields: &mut Map<String, Value>, name: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        fields.insert(name.to_string(), Value::String(value.to_string()));
    }
}

fn insert_u32(fields: &mut Map<String, Value>, name: &str, value: Option<u32>) {
    if let Some(value) = value.filter(|value| *value > 0) {
        fields.insert(name.to_string(), Value::Number(Number::from(value)));
    }
}

fn stable_media_server_item_id(server_id: &Arc<str>, library_id: &Arc<str>, item_id: &Arc<str>, kind: &str) -> String {
    format!("media-server:{server_id}:{library_id}:{kind}:{item_id}")
}

pub fn media_server_stream_ref_to_internal_url(stream_ref: &MediaServerStreamRef) -> String {
    match stream_ref {
        MediaServerStreamRef::Emby { server_id, item_id, media_source_id, .. } => {
            format!(
                "media-server://emby/{}/{}{}",
                escape_internal_url_component(server_id),
                escape_internal_url_component(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", escape_internal_url_component(id)))
                    .unwrap_or_default()
            )
        }
        MediaServerStreamRef::Jellyfin { server_id, item_id, media_source_id, .. } => {
            format!(
                "media-server://jellyfin/{}/{}{}",
                escape_internal_url_component(server_id),
                escape_internal_url_component(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", escape_internal_url_component(id)))
                    .unwrap_or_default()
            )
        }
        MediaServerStreamRef::Plex { server_id, rating_key, part_key, .. } => format!(
            "media-server://plex/{}/{}?part_key={}",
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
    use crate::media_server::{MediaServerCatalogSnapshot, MediaServerProviderIdHint};
    use serde_json::Value;

    fn movie() -> MediaServerMovie {
        MediaServerMovie {
            input_name: "media_server".into(),
            server_id: "server/one".into(),
            library_id: "movies".into(),
            item_id: "item?one plus+space".into(),
            title: "Movie".into(),
            year: Some(2024),
            release_date: None,
            source_version_hint: None,
            provider_hints: Vec::<MediaServerProviderIdHint>::new(),
            technical_facts: None,
            stream_ref: Some(MediaServerStreamRef::Emby {
                input_name: "media_server".into(),
                server_id: "server/one".into(),
                item_id: "item?one plus+space".into(),
                media_source_id: Some("media/source".into()),
            }),
            image_ref: None,
        }
    }

    fn episode() -> MediaServerEpisode {
        MediaServerEpisode {
            input_name: "media_server".into(),
            server_id: "server".into(),
            library_id: "shows".into(),
            item_id: "episode".into(),
            series_id: Some("series".into()),
            series_title: Some("Show".into()),
            title: "Episode".into(),
            season: Some(1),
            episode: Some(2),
            release_date: None,
            source_version_hint: None,
            provider_hints: Vec::<MediaServerProviderIdHint>::new(),
            technical_facts: None,
            stream_ref: Some(MediaServerStreamRef::Plex {
                input_name: "media_server".into(),
                server_id: "server".into(),
                rating_key: "rating".into(),
                part_key: "/library/parts/redacted/file.mkv".into(),
            }),
            image_ref: None,
        }
    }

    #[test]
    fn maps_media_server_movies_and_episodes_to_playlist_groups_without_virtual_ids() {
        let groups = media_server_catalog_snapshot_to_playlist(&MediaServerCatalogSnapshot {
            movies: vec![movie()],
            episodes: vec![episode()],
            ..MediaServerCatalogSnapshot::default()
        });

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].xtream_cluster, XtreamCluster::Video);
        assert_eq!(groups[0].channels[0].header.item_type, PlaylistItemType::Video);
        assert_eq!(groups[0].channels[0].header.virtual_id, 0);
        assert!(groups[0].channels[0].header.id.starts_with("media-server:server/one:movies:movie:item?one plus+space"));
        assert!(groups[0].channels[0].header.url.contains("media-server://emby/server%2Fone/item%3Fone%20plus%2Bspace"));
        assert_eq!(groups[1].channels[0].header.item_type, PlaylistItemType::Series);
        assert!(groups[1].channels[0].header.url.contains("part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv"));
    }

    #[test]
    fn maps_safe_media_server_catalog_facts_to_stream_properties_without_network_enrichment() {
        let mut movie = movie();
        movie.provider_hints = vec![
            MediaServerProviderIdHint { namespace: "imdb".into(), value: "tt-redacted".into() },
            MediaServerProviderIdHint { namespace: "tmdb".into(), value: "12345".into() },
        ];
        movie.technical_facts = Some(MediaServerTechnicalFacts {
            container: Some(".mkv".into()),
            duration_secs: Some(7_200),
            bitrate: Some(8_000),
            video: Some(MediaServerVideoTechnicalFacts {
                codec: Some("hevc".into()),
                width: Some(1_920),
                height: Some(1_080),
            }),
            audio: Some(MediaServerAudioTechnicalFacts {
                codec: Some("eac3".into()),
                channels: Some(6),
            }),
        });

        let mut episode = episode();
        episode.provider_hints = vec![MediaServerProviderIdHint { namespace: "TmDb".into(), value: "67890".into() }];
        episode.release_date = Some("2024-02-03".into());
        episode.technical_facts = Some(MediaServerTechnicalFacts {
            container: Some("mp4".into()),
            video: Some(MediaServerVideoTechnicalFacts { codec: Some("h264".into()), width: Some(1_280), height: Some(720) }),
            audio: Some(MediaServerAudioTechnicalFacts { codec: Some("aac".into()), channels: Some(2) }),
            ..MediaServerTechnicalFacts::default()
        });

        let groups = media_server_catalog_snapshot_to_playlist(&MediaServerCatalogSnapshot {
            movies: vec![movie],
            episodes: vec![episode],
            ..MediaServerCatalogSnapshot::default()
        });

        let Some(StreamProperties::Video(video)) = &groups[0].channels[0].header.additional_properties else {
            panic!("expected video properties");
        };
        assert_eq!(video.tmdb, Some(12345));
        assert_eq!(video.container_extension.as_ref(), "mkv");
        let details = video.details.as_ref().expect("movie technical facts should create details");
        assert_eq!(details.release_date.as_deref(), Some("2024-01-01"));
        assert_eq!(details.duration_secs.as_deref(), Some("7200"));
        assert_eq!(details.bitrate, 8_000);
        assert_eq!(json_field(details.video.as_deref(), "codec_name"), Some(Value::String("hevc".to_string())));
        assert_eq!(json_field(details.video.as_deref(), "height"), Some(Value::Number(1_080.into())));
        assert_eq!(json_field(details.audio.as_deref(), "channels"), Some(Value::Number(6.into())));

        let Some(StreamProperties::Episode(episode)) = &groups[1].channels[0].header.additional_properties else {
            panic!("expected episode properties");
        };
        assert_eq!(episode.tmdb, Some(67890));
        assert_eq!(episode.release_date.as_deref(), Some("2024-02-03"));
        assert_eq!(episode.container_extension.as_ref(), "mp4");
        assert_eq!(json_field(episode.video.as_deref(), "height"), Some(Value::Number(720.into())));
        assert_eq!(json_field(episode.audio.as_deref(), "codec_name"), Some(Value::String("aac".to_string())));
    }

    #[test]
    fn ignores_invalid_media_server_tmdb_hints() {
        let hints = vec![
            MediaServerProviderIdHint { namespace: "tmdb".into(), value: "0".into() },
            MediaServerProviderIdHint { namespace: "tmdb".into(), value: "not-a-number".into() },
        ];

        assert_eq!(provider_tmdb_id(&hints), None);
    }

    fn json_field(json: Option<&str>, field: &str) -> Option<Value> {
        let value = serde_json::from_str::<Value>(json?).ok()?;
        value.get(field).cloned()
    }
}
