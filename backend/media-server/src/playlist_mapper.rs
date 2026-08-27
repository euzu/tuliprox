use crate::{
    encode_url_path_segment, MediaServerAudioTechnicalFacts, MediaServerCatalogSnapshot, MediaServerDescriptiveFacts,
    MediaServerEpisode, MediaServerImageRef, MediaServerMovie, MediaServerProviderIdHint, MediaServerSeason,
    MediaServerSeries, MediaServerStreamRef, MediaServerTechnicalFacts, MediaServerVideoTechnicalFacts,
};
use serde_json::{Map, Number, Value};
use shared::{
    model::{
        EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties, SeriesStreamProperties, StreamProperties,
        VideoStreamDetailProperties, VideoStreamProperties, XtreamCluster,
    },
    utils::{generate_provider_playlist_uuid, Internable},
};
use std::{collections::HashMap, sync::Arc};

pub fn media_server_catalog_snapshot_to_playlist(snapshot: &MediaServerCatalogSnapshot) -> Vec<PlaylistGroup> {
    let mut groups = Vec::new();

    if !snapshot.movies.is_empty() {
        groups.push(PlaylistGroup {
            id: 1,
            // TODO Presa  read from config like LibraryPlaylistConfig
            title: "Media Server Movies".intern(),
            channels: snapshot.movies.iter().map(media_server_movie_to_playlist_item).collect(),
            xtream_cluster: XtreamCluster::Video,
        });
    }

    let mut series_channels = Vec::with_capacity(snapshot.series.len().saturating_add(snapshot.episodes.len()));
    let seasons_by_series = media_server_seasons_by_series(&snapshot.seasons);
    for series in &snapshot.series {
        let season_key =
            media_server_series_season_key(&series.input_name, &series.server_id, &series.library_id, &series.item_id);
        let seasons = seasons_by_series.get(&season_key).map(Vec::as_slice).unwrap_or_default();
        series_channels.push(media_server_series_to_playlist_item(series, seasons));
    }

    let parent_codes = series_parent_code_map(&snapshot.series);
    series_channels.extend(
        snapshot
            .episodes
            .iter()
            .map(|episode| media_server_episode_to_playlist_item(episode, episode_parent_code(episode, &parent_codes))),
    );

    if !series_channels.is_empty() {
        groups.push(PlaylistGroup {
            id: next_group_id(groups.len()),
            // TODO Presa  read from config like LibraryPlaylistConfig
            title: "Media Server Series".intern(),
            channels: series_channels,
            xtream_cluster: XtreamCluster::Series,
        });
    }

    groups
}

fn next_group_id(group_count: usize) -> u32 { u32::try_from(group_count.saturating_add(1)).unwrap_or(u32::MAX) }

type MediaServerSeriesKey<'a> = (&'a str, &'a str, &'a str, &'a str);

fn media_server_seasons_by_series(
    seasons: &[MediaServerSeason],
) -> HashMap<MediaServerSeriesKey<'_>, Vec<&MediaServerSeason>> {
    let mut seasons_by_series: HashMap<MediaServerSeriesKey<'_>, Vec<&MediaServerSeason>> = HashMap::new();

    for season in seasons {
        let Some(series_id) = season.series_id.as_ref() else { continue };
        seasons_by_series
            .entry(media_server_series_season_key(&season.input_name, &season.server_id, &season.library_id, series_id))
            .or_default()
            .push(season);
    }

    for seasons in seasons_by_series.values_mut() {
        seasons.sort_by_key(|season| season.season.unwrap_or_default());
    }

    seasons_by_series
}

fn media_server_series_season_key<'a>(
    input_name: &'a Arc<str>,
    server_id: &'a Arc<str>,
    library_id: &'a Arc<str>,
    series_id: &'a Arc<str>,
) -> MediaServerSeriesKey<'a> {
    (input_name.as_ref(), server_id.as_ref(), library_id.as_ref(), series_id.as_ref())
}

fn series_parent_code_map(series: &[MediaServerSeries]) -> HashMap<MediaServerSeriesKey<'_>, Arc<str>> {
    series
        .iter()
        .map(|series| {
            let stable_id =
                stable_media_server_item_id(&series.server_id, &series.library_id, &series.item_id, "series");
            let uuid = generate_provider_playlist_uuid(&series.input_name, &stable_id, PlaylistItemType::SeriesInfo);
            (
                media_server_series_parent_key(
                    &series.input_name,
                    &series.server_id,
                    &series.library_id,
                    &series.item_id,
                ),
                uuid.intern(),
            )
        })
        .collect()
}

fn episode_parent_code(
    episode: &MediaServerEpisode,
    parent_codes: &HashMap<MediaServerSeriesKey<'_>, Arc<str>>,
) -> Arc<str> {
    // Orphan episodes keep a stable media-server parent key when no SeriesInfo anchor exists.
    // materialize_media_server_series_info_episodes and rewrite_series_episode_parent_virtual_ids
    // only link episodes whose parent_code is a SeriesInfo uuid.intern(); this fallback is
    // intentionally stable but unlinkable rather than inventing a public series anchor.
    let Some(series_id) = episode.series_id.as_ref() else {
        return "".intern();
    };
    let stable_id = stable_media_server_item_id(&episode.server_id, &episode.library_id, series_id, "series");
    parent_codes
        .get(&media_server_series_parent_key(&episode.input_name, &episode.server_id, &episode.library_id, series_id))
        .map_or_else(|| stable_id.intern(), Arc::clone)
}

fn media_server_series_parent_key<'a>(
    input_name: &'a Arc<str>,
    server_id: &'a Arc<str>,
    library_id: &'a Arc<str>,
    series_id: &'a Arc<str>,
) -> MediaServerSeriesKey<'a> {
    (input_name.as_ref(), server_id.as_ref(), library_id.as_ref(), series_id.as_ref())
}

fn media_server_movie_to_playlist_item(movie: &MediaServerMovie) -> PlaylistItem {
    let stable_id = stable_media_server_item_id(&movie.server_id, &movie.library_id, &movie.item_id, "movie");
    let url = movie.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "media-server://unavailable/{}/{}/{}",
                encode_url_path_segment(&movie.server_id),
                encode_url_path_segment(&movie.library_id),
                encode_url_path_segment(&movie.item_id)
            )
        },
        media_server_stream_ref_to_internal_url,
    );
    let uuid = generate_provider_playlist_uuid(&movie.input_name, &stable_id, PlaylistItemType::Video);
    let release_date = movie.release_date.clone().or_else(|| release_date_from_year(movie.year));
    let image_url = movie.image_ref.as_ref().map(media_server_image_ref_to_internal_url).map(Arc::<str>::from);
    let details = movie_details(movie, release_date, image_url.clone());
    let rating = media_server_rating(movie.descriptive_facts.as_ref());

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
                stream_icon: image_url.clone().unwrap_or_else(|| "".intern()),
                direct_source: "".intern(),
                category_id: 0,
                custom_sid: None,
                added: movie.source_version_hint.clone().unwrap_or_else(|| "".intern()),
                container_extension: media_server_container_extension(movie.technical_facts.as_ref()),
                rating,
                rating_5based: rating.map(rating_5based),
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

fn media_server_series_to_playlist_item(series: &MediaServerSeries, seasons: &[&MediaServerSeason]) -> PlaylistItem {
    let stable_id = stable_media_server_item_id(&series.server_id, &series.library_id, &series.item_id, "series");
    let url = format!(
        "media-server://unavailable/{}/{}/{}",
        encode_url_path_segment(&series.server_id),
        encode_url_path_segment(&series.library_id),
        encode_url_path_segment(&series.item_id)
    );
    let uuid = generate_provider_playlist_uuid(&series.input_name, &stable_id, PlaylistItemType::SeriesInfo);
    let release_date = series.release_date.clone().or_else(|| release_date_from_year(series.year));
    let rating = media_server_rating(series.descriptive_facts.as_ref()).unwrap_or_default();
    let image_url = series.image_ref.as_ref().map(media_server_image_ref_to_internal_url).map(Arc::<str>::from);

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: series.title.clone(),
            title: series.title.clone(),
            group: "Media Server Series".intern(),
            url: url.intern(),
            input_name: series.input_name.clone(),
            xtream_cluster: XtreamCluster::Series,
            item_type: PlaylistItemType::SeriesInfo,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                name: series.title.clone(),
                series_id: 0,
                cover: image_url.unwrap_or_else(|| "".intern()),
                backdrop_path: None,
                category_id: 0,
                cast: joined_values(series.descriptive_facts.as_ref().map(|facts| facts.cast.as_slice()))
                    .unwrap_or_else(|| "".intern()),
                director: joined_values(series.descriptive_facts.as_ref().map(|facts| facts.directors.as_slice()))
                    .unwrap_or_else(|| "".intern()),
                episode_run_time: None,
                genre: joined_values(series.descriptive_facts.as_ref().map(|facts| facts.genres.as_slice())),
                last_modified: series.source_version_hint.clone(),
                plot: series.descriptive_facts.as_ref().and_then(|facts| facts.summary.clone()),
                rating,
                rating_5based: rating_5based(rating),
                release_date,
                youtube_trailer: "".intern(),
                tmdb: provider_tmdb_id(&series.provider_hints),
                details: series_details(series, seasons),
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn media_server_episode_to_playlist_item(episode: &MediaServerEpisode, parent_code: Arc<str>) -> PlaylistItem {
    let stable_id = stable_media_server_item_id(&episode.server_id, &episode.library_id, &episode.item_id, "episode");
    let url = episode.stream_ref.as_ref().map_or_else(
        || {
            format!(
                "media-server://unavailable/{}/{}/{}",
                encode_url_path_segment(&episode.server_id),
                encode_url_path_segment(&episode.library_id),
                encode_url_path_segment(&episode.item_id)
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
    let technical = episode.technical_facts.as_ref();
    let plot = episode.descriptive_facts.as_ref().and_then(|facts| facts.summary.clone());
    let image_url = episode.image_ref.as_ref().map(media_server_image_ref_to_internal_url).map(Arc::<str>::from);

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid,
            id: stable_id.intern(),
            name: title.clone(),
            title,
            group: "Media Server Series".intern(),
            parent_code,
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
                plot,
                tmdb: provider_tmdb_id(&episode.provider_hints),
                movie_image: image_url.unwrap_or_else(|| "".intern()),
                container_extension: media_server_container_extension(technical),
                video: technical.and_then(media_server_video_json),
                audio: technical.and_then(media_server_audio_json),
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn movie_details(
    movie: &MediaServerMovie,
    release_date: Option<Arc<str>>,
    image_url: Option<Arc<str>>,
) -> Option<VideoStreamDetailProperties> {
    let technical = movie.technical_facts.as_ref();
    let descriptive = movie.descriptive_facts.as_ref();
    let video = technical.and_then(media_server_video_json);
    let audio = technical.and_then(media_server_audio_json);
    let duration_secs =
        technical.and_then(|facts| facts.duration_secs).map(|duration| Arc::<str>::from(duration.to_string()));
    let bitrate = technical.and_then(|facts| facts.bitrate).unwrap_or_default();
    let summary = descriptive.and_then(|facts| facts.summary.clone());
    let age = descriptive.and_then(|facts| facts.parental_age.map(|age| Arc::<str>::from(age.to_string())));

    let details = VideoStreamDetailProperties {
        kinopoisk_url: None,
        o_name: descriptive.and_then(|facts| facts.original_title.clone()),
        cover_big: image_url.clone(),
        movie_image: image_url,
        release_date,
        episode_run_time: technical.and_then(|facts| facts.duration_secs.map(|duration| duration / 60)),
        youtube_trailer: None,
        director: joined_values(descriptive.map(|facts| facts.directors.as_slice())),
        actors: joined_values(descriptive.map(|facts| facts.cast.as_slice())),
        cast: joined_values(descriptive.map(|facts| facts.cast.as_slice())),
        description: summary.clone(),
        plot: summary,
        age,
        mpaa_rating: descriptive.and_then(|facts| facts.content_rating.clone()),
        rating_count_kinopoisk: 0,
        country: joined_values(descriptive.map(|facts| facts.countries.as_slice())),
        genre: joined_values(descriptive.map(|facts| facts.genres.as_slice())),
        backdrop_path: None,
        duration_secs,
        duration: technical.and_then(|facts| facts.duration_secs.map(duration_secs_to_xtream_duration)),
        video,
        audio,
        bitrate,
        runtime: technical.and_then(|facts| facts.duration_secs.map(|duration| Arc::<str>::from(duration.to_string()))),
        status: None,
    };

    has_movie_details(&details).then_some(details)
}

fn series_details(series: &MediaServerSeries, seasons: &[&MediaServerSeason]) -> Option<SeriesStreamDetailProperties> {
    if series.year.is_none() && seasons.is_empty() {
        return None;
    }

    let season_details = seasons
        .iter()
        .map(|season| SeriesStreamDetailSeasonProperties {
            name: season.title.clone(),
            season_number: season.season.unwrap_or_default(),
            episode_count: season.episode_count.unwrap_or_default(),
            overview: season.descriptive_facts.as_ref().and_then(|facts| facts.summary.clone()),
            air_date: season.release_date.clone().or_else(|| release_date_from_year(season.year)),
            cover: None,
            cover_tmdb: None,
            cover_big: None,
            duration: None,
        })
        .collect::<Vec<_>>();

    Some(SeriesStreamDetailProperties {
        year: series.year,
        seasons: (!season_details.is_empty()).then_some(season_details),
        episodes: None,
    })
}

fn has_movie_details(details: &VideoStreamDetailProperties) -> bool {
    details.kinopoisk_url.is_some()
        || details.o_name.is_some()
        || details.cover_big.is_some()
        || details.movie_image.is_some()
        || details.release_date.is_some()
        || details.episode_run_time.is_some()
        || details.youtube_trailer.is_some()
        || details.director.is_some()
        || details.actors.is_some()
        || details.cast.is_some()
        || details.description.is_some()
        || details.plot.is_some()
        || details.age.is_some()
        || details.mpaa_rating.is_some()
        || details.country.is_some()
        || details.genre.is_some()
        || details.backdrop_path.is_some()
        || details.duration_secs.is_some()
        || details.duration.is_some()
        || details.video.is_some()
        || details.audio.is_some()
        || details.bitrate > 0
        || details.runtime.is_some()
        || details.status.is_some()
}

fn joined_values(values: Option<&[Arc<str>]>) -> Option<Arc<str>> {
    let values = values?.iter().map(|value| value.trim()).filter(|value| !value.is_empty()).collect::<Vec<_>>();
    (!values.is_empty()).then(|| Arc::<str>::from(values.join(", ")))
}

fn media_server_rating(descriptive: Option<&MediaServerDescriptiveFacts>) -> Option<f64> {
    descriptive
        .and_then(|facts| facts.audience_rating.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn rating_5based(rating: f64) -> f64 { rating / 2.0 }

fn duration_secs_to_xtream_duration(duration_secs: u32) -> Arc<str> {
    let hours = duration_secs / 3600;
    let minutes = (duration_secs % 3600) / 60;
    let seconds = duration_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}").into()
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
    // Synthetic year-only fallback for Xtream compatibility; callers must treat this as
    // non-authoritative and not as proof of an exact mid-year release date.
    year.filter(|year| *year > 0).map(|year| Arc::<str>::from(format!("{year}-07-01")))
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
                encode_url_path_segment(server_id),
                encode_url_path_segment(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", encode_url_path_segment(id)))
                    .unwrap_or_default()
            )
        }
        MediaServerStreamRef::Jellyfin { server_id, item_id, media_source_id, .. } => {
            format!(
                "media-server://jellyfin/{}/{}{}",
                encode_url_path_segment(server_id),
                encode_url_path_segment(item_id),
                media_source_id
                    .as_ref()
                    .map(|id| format!("?media_source_id={}", encode_url_path_segment(id)))
                    .unwrap_or_default()
            )
        }
        MediaServerStreamRef::Plex { server_id, rating_key, part_key, .. } => format!(
            "media-server://plex/{}/{}?part_key={}",
            encode_url_path_segment(server_id),
            encode_url_path_segment(rating_key),
            encode_url_path_segment(part_key)
        ),
    }
}

pub fn media_server_image_ref_to_internal_url(image_ref: &MediaServerImageRef) -> String {
    match image_ref {
        MediaServerImageRef::Emby { input_name, server_id, item_id, image_kind, tag } => format!(
            "media-server://image/emby/{}/{}/{}?image_kind={}{}",
            encode_url_path_segment(input_name),
            encode_url_path_segment(server_id),
            encode_url_path_segment(item_id),
            encode_url_path_segment(image_kind),
            tag.as_ref().map(|tag| format!("&tag={}", encode_url_path_segment(tag))).unwrap_or_default()
        ),
        MediaServerImageRef::Jellyfin { input_name, server_id, item_id, image_kind, tag } => format!(
            "media-server://image/jellyfin/{}/{}/{}?image_kind={}{}",
            encode_url_path_segment(input_name),
            encode_url_path_segment(server_id),
            encode_url_path_segment(item_id),
            encode_url_path_segment(image_kind),
            tag.as_ref().map(|tag| format!("&tag={}", encode_url_path_segment(tag))).unwrap_or_default()
        ),
        MediaServerImageRef::Plex { input_name, server_id, rating_key, image_path } => format!(
            "media-server://image/plex/{}/{}/{}?image_path={}",
            encode_url_path_segment(input_name),
            encode_url_path_segment(server_id),
            encode_url_path_segment(rating_key),
            encode_url_path_segment(image_path)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaServerCatalogSnapshot, MediaServerImageRef, MediaServerProviderIdHint};
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
            descriptive_facts: None,
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
            descriptive_facts: None,
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

    fn series() -> MediaServerSeries {
        MediaServerSeries {
            input_name: "media_server".into(),
            server_id: "server".into(),
            library_id: "shows".into(),
            item_id: "series".into(),
            title: "Show".into(),
            year: Some(2024),
            release_date: Some("2024-01-02".into()),
            source_version_hint: Some("updated".into()),
            provider_hints: vec![MediaServerProviderIdHint { namespace: "tmdb".into(), value: "222".into() }],
            descriptive_facts: Some(MediaServerDescriptiveFacts {
                summary: Some("show summary".into()),
                audience_rating: Some("8.0".into()),
                genres: vec!["Drama".into(), "Mystery".into()],
                directors: vec!["Director Redacted".into()],
                cast: vec!["Actor One".into(), "Actor Two".into()],
                ..MediaServerDescriptiveFacts::default()
            }),
            child_count: Some(1),
            episode_count: Some(2),
            image_ref: None,
        }
    }

    fn season() -> MediaServerSeason {
        MediaServerSeason {
            input_name: "media_server".into(),
            server_id: "server".into(),
            library_id: "shows".into(),
            item_id: "season".into(),
            series_id: Some("series".into()),
            series_title: Some("Show".into()),
            title: "Season 1".into(),
            season: Some(1),
            year: None,
            release_date: Some("2024-01-03".into()),
            source_version_hint: None,
            provider_hints: Vec::new(),
            descriptive_facts: Some(MediaServerDescriptiveFacts {
                summary: Some("season summary".into()),
                ..MediaServerDescriptiveFacts::default()
            }),
            episode_count: Some(2),
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
        assert_eq!(groups[0].channels[0].header.virtual_id, shared::model::VirtualId::default());
        assert!(groups[0].channels[0]
            .header
            .id
            .starts_with("media-server:server/one:movies:movie:item?one plus+space"));
        assert!(groups[0].channels[0]
            .header
            .url
            .contains("media-server://emby/server%2Fone/item%3Fone%20plus%2Bspace"));
        assert_eq!(groups[1].channels[0].header.item_type, PlaylistItemType::Series);
        assert_eq!(groups[1].channels[0].header.parent_code.as_ref(), "media-server:server:shows:series:series");
        assert!(groups[1].channels[0].header.url.contains("part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv"));
    }

    #[test]
    fn maps_media_server_series_and_seasons_as_catalog_anchors() {
        let groups = media_server_catalog_snapshot_to_playlist(&MediaServerCatalogSnapshot {
            series: vec![series()],
            seasons: vec![season()],
            episodes: vec![episode()],
            ..MediaServerCatalogSnapshot::default()
        });

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].channels.len(), 2);
        assert_eq!(groups[0].channels[0].header.item_type, PlaylistItemType::SeriesInfo);
        assert_eq!(groups[0].channels[1].header.item_type, PlaylistItemType::Series);
        assert_eq!(groups[0].channels[1].header.parent_code, groups[0].channels[0].header.uuid.intern());

        let Some(StreamProperties::Series(series)) = &groups[0].channels[0].header.additional_properties else {
            panic!("expected series properties");
        };
        assert_eq!(series.tmdb, Some(222));
        assert_eq!(series.cover.as_ref(), "");
        assert!(series.backdrop_path.is_none());
        assert_eq!(series.plot.as_deref(), Some("show summary"));
        assert_eq!(series.genre.as_deref(), Some("Drama, Mystery"));
        assert_eq!(series.cast.as_ref(), "Actor One, Actor Two");
        assert_eq!(series.director.as_ref(), "Director Redacted");
        assert!(series.rating - 8.0 < 0.0001);
        assert!(series.rating_5based - 4.0 < 0.0001);
        let details = series.details.as_ref().expect("series details should contain season anchors");
        let seasons = details.seasons.as_ref().expect("season anchors should be mapped");
        assert_eq!(seasons[0].name.as_ref(), "Season 1");
        assert_eq!(seasons[0].overview.as_deref(), Some("season summary"));
        assert_eq!(seasons[0].air_date.as_deref(), Some("2024-01-03"));
        assert_eq!(seasons[0].episode_count, 2);
        assert!(seasons[0].cover.is_none());
        assert!(details.episodes.is_none());
    }

    #[test]
    fn maps_media_server_series_relationships_scoped_by_input_name() -> Result<(), String> {
        let mut series_a = series();
        series_a.input_name = "media_server_a".into();
        let mut series_b = series();
        series_b.input_name = "media_server_b".into();

        let mut season_a = season();
        season_a.input_name = "media_server_a".into();
        season_a.title = "Season A".into();
        let mut season_b = season();
        season_b.input_name = "media_server_b".into();
        season_b.title = "Season B".into();

        let mut episode_a = episode();
        episode_a.input_name = "media_server_a".into();
        episode_a.item_id = "episode-a".into();
        let mut episode_b = episode();
        episode_b.input_name = "media_server_b".into();
        episode_b.item_id = "episode-b".into();

        let groups = media_server_catalog_snapshot_to_playlist(&MediaServerCatalogSnapshot {
            series: vec![series_a, series_b],
            seasons: vec![season_b, season_a],
            episodes: vec![episode_a, episode_b],
            ..MediaServerCatalogSnapshot::default()
        });

        let [group] = groups.as_slice() else {
            return Err(format!("expected one series group, got {}", groups.len()));
        };
        let [series_a_item, series_b_item, episode_first_item, episode_second_item] = group.channels.as_slice() else {
            return Err(format!("expected two series anchors and two episodes, got {}", group.channels.len()));
        };

        assert_eq!(episode_first_item.header.parent_code, series_a_item.header.uuid.intern());
        assert_eq!(episode_second_item.header.parent_code, series_b_item.header.uuid.intern());

        let Some(StreamProperties::Series(series_first_properties)) = &series_a_item.header.additional_properties
        else {
            return Err("expected series A properties".to_string());
        };
        let Some(StreamProperties::Series(series_second_properties)) = &series_b_item.header.additional_properties
        else {
            return Err("expected series B properties".to_string());
        };
        let seasons_first = series_first_properties
            .details
            .as_ref()
            .and_then(|details| details.seasons.as_ref())
            .ok_or_else(|| "expected series A seasons".to_string())?;
        let seasons_second = series_second_properties
            .details
            .as_ref()
            .and_then(|details| details.seasons.as_ref())
            .ok_or_else(|| "expected series B seasons".to_string())?;

        assert_eq!(seasons_first.iter().map(|season| season.name.as_ref()).collect::<Vec<_>>(), vec!["Season A"]);
        assert_eq!(seasons_second.iter().map(|season| season.name.as_ref()).collect::<Vec<_>>(), vec!["Season B"]);

        Ok(())
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
            bitrate: Some(8_000_000),
            video: Some(MediaServerVideoTechnicalFacts {
                codec: Some("hevc".into()),
                width: Some(1_920),
                height: Some(1_080),
            }),
            audio: Some(MediaServerAudioTechnicalFacts { codec: Some("eac3".into()), channels: Some(6) }),
        });

        let mut episode = episode();
        episode.provider_hints = vec![MediaServerProviderIdHint { namespace: "TmDb".into(), value: "67890".into() }];
        episode.release_date = Some("2024-02-03".into());
        episode.descriptive_facts = Some(MediaServerDescriptiveFacts {
            summary: Some("episode summary".into()),
            ..MediaServerDescriptiveFacts::default()
        });
        episode.technical_facts = Some(MediaServerTechnicalFacts {
            container: Some("mp4".into()),
            video: Some(MediaServerVideoTechnicalFacts {
                codec: Some("h264".into()),
                width: Some(1_280),
                height: Some(720),
            }),
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
        assert_eq!(details.release_date.as_deref(), Some("2024-07-01"));
        assert_eq!(details.duration_secs.as_deref(), Some("7200"));
        assert_eq!(details.bitrate, 8_000_000);
        assert_eq!(json_field(details.video.as_deref(), "codec_name"), Some(Value::String("hevc".to_string())));
        assert_eq!(json_field(details.video.as_deref(), "height"), Some(Value::Number(1_080.into())));
        assert_eq!(json_field(details.audio.as_deref(), "channels"), Some(Value::Number(6.into())));

        let Some(StreamProperties::Episode(episode)) = &groups[1].channels[0].header.additional_properties else {
            panic!("expected episode properties");
        };
        assert_eq!(episode.tmdb, Some(67890));
        assert_eq!(episode.release_date.as_deref(), Some("2024-02-03"));
        assert_eq!(episode.plot.as_deref(), Some("episode summary"));
        assert_eq!(episode.container_extension.as_ref(), "mp4");
        assert_eq!(json_field(episode.video.as_deref(), "height"), Some(Value::Number(720.into())));
        assert_eq!(json_field(episode.audio.as_deref(), "codec_name"), Some(Value::String("aac".to_string())));
    }

    #[test]
    fn projects_media_server_image_refs_as_internal_resource_urls() {
        let mut movie = movie();
        movie.image_ref = Some(MediaServerImageRef::Plex {
            input_name: "media_server".into(),
            server_id: "server/one".into(),
            rating_key: "rating".into(),
            image_path: "/library/metadata/rating/thumb/redacted".into(),
        });
        let mut episode = episode();
        episode.image_ref = Some(MediaServerImageRef::Emby {
            input_name: "media_server".into(),
            server_id: "server".into(),
            item_id: "episode".into(),
            image_kind: "Primary".into(),
            tag: Some("tag-redacted".into()),
        });
        let mut series = series();
        series.image_ref = Some(MediaServerImageRef::Jellyfin {
            input_name: "media_server".into(),
            server_id: "server".into(),
            item_id: "series".into(),
            image_kind: "Primary".into(),
            tag: Some("tag-redacted".into()),
        });

        let groups = media_server_catalog_snapshot_to_playlist(&MediaServerCatalogSnapshot {
            movies: vec![movie],
            series: vec![series],
            episodes: vec![episode],
            ..MediaServerCatalogSnapshot::default()
        });

        let Some(StreamProperties::Video(video)) = &groups[0].channels[0].header.additional_properties else {
            panic!("expected video properties");
        };
        assert_eq!(
            video.stream_icon.as_ref(),
            "media-server://image/plex/media_server/server%2Fone/rating?image_path=%2Flibrary%2Fmetadata%2Frating%2Fthumb%2Fredacted"
        );
        let details = video.details.as_ref().expect("movie year should create details");
        assert_eq!(
            details.cover_big.as_deref(),
            Some("media-server://image/plex/media_server/server%2Fone/rating?image_path=%2Flibrary%2Fmetadata%2Frating%2Fthumb%2Fredacted")
        );
        assert_eq!(
            details.movie_image.as_deref(),
            Some("media-server://image/plex/media_server/server%2Fone/rating?image_path=%2Flibrary%2Fmetadata%2Frating%2Fthumb%2Fredacted")
        );
        assert!(details.backdrop_path.is_none());

        let Some(StreamProperties::Series(series)) = &groups[1].channels[0].header.additional_properties else {
            panic!("expected series properties");
        };
        assert_eq!(
            series.cover.as_ref(),
            "media-server://image/jellyfin/media_server/server/series?image_kind=Primary&tag=tag-redacted"
        );
        assert!(series.backdrop_path.is_none());

        let Some(StreamProperties::Episode(episode)) = &groups[1].channels[1].header.additional_properties else {
            panic!("expected episode properties");
        };
        assert_eq!(
            episode.movie_image.as_ref(),
            "media-server://image/emby/media_server/server/episode?image_kind=Primary&tag=tag-redacted"
        );
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
