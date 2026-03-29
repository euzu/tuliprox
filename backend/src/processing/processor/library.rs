use crate::library::{EpisodeMetadata, MediaMetadata, MetadataAsyncIter, MetadataCacheEntry};
use crate::library::resolve_metadata_storage_path;
use crate::model::{AppConfig, ConfigInput};
use shared::concat_string;
use shared::error::TuliproxError;
use shared::model::UUIDType;
use shared::model::{EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties, SeriesStreamProperties, StreamProperties, VideoStreamDetailProperties, VideoStreamProperties, XtreamCluster, normalize_episode_title};
use shared::utils::{concat_path_leading_slash, generate_local_playlist_uuid, Internable};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn thumbnail_url(entry: &MetadataCacheEntry, api_base_path: &str) -> Option<String> {
    entry.thumbnail_hash
        .as_ref()
        .map(|_| concat_path_leading_slash(api_base_path, &format!("library/thumbnail/{}", entry.uuid)))
}

fn episode_thumbnail_url(episode: &EpisodeMetadata, api_base_path: &str) -> Option<String> {
    episode
        .thumbnail_id
        .as_deref()
        .map(|id| concat_path_leading_slash(api_base_path, &format!("library/thumbnail/{id}")))
}

pub async fn download_library_playlist(_client: &reqwest::Client, app_config: &Arc<AppConfig>, input: &ConfigInput) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    let config = &*app_config.config.load();
    let Some(library_config) = config.library.as_ref() else { return (vec![], vec![]) };
    if !library_config.enabled { return (vec![], vec![]); }
    let api_base_path = concat_path_leading_slash(
        config.web_ui.as_ref().and_then(|w| w.path.as_deref()).unwrap_or(""),
        "api/v1",
    );

    let storage_path = resolve_metadata_storage_path(config.metadata_update.as_ref(), &config.storage_dir)
        .join("library");
    let mut metadata_iter = MetadataAsyncIter::new(&storage_path).await;
    let mut group_movies = PlaylistGroup {
        id: 0,
        title: library_config.playlist.movie_category.clone(),
        channels: vec![],
        xtream_cluster: XtreamCluster::Video,
    };
    let mut group_series = PlaylistGroup {
        id: 0,
        title: library_config.playlist.series_category.clone(),
        channels: vec![],
        xtream_cluster: XtreamCluster::Series,
    };
    while let Some(entry) = metadata_iter.next().await {
        match entry.metadata {
            MediaMetadata::Movie(_) => {
                to_playlist_item(&entry, &input.name, &library_config.playlist.movie_category, &api_base_path, &mut group_movies.channels);
            }
            MediaMetadata::Series(_) => {
                to_playlist_item(&entry, &input.name, &library_config.playlist.series_category, &api_base_path, &mut group_series.channels);
            }
        }
    }

    let mut groups = vec![];
    if !group_movies.channels.is_empty() {
        groups.push(group_movies);
    }
    if !group_series.channels.is_empty() {
        groups.push(group_series);
    }

    (groups, vec![])
}

fn to_playlist_item(
    entry: &MetadataCacheEntry,
    input_name: &Arc<str>,
    group_name: &str,
    api_base_path: &str,
    channels: &mut Vec<PlaylistItem>,
) {
    match &entry.metadata {
        MediaMetadata::Movie(_) => channels.push(build_movie_playlist_item(entry, input_name, group_name, api_base_path)),
        MediaMetadata::Series(_) => {
            if let Some((series_info, episodes)) = build_series_playlist_items(entry, input_name, group_name, api_base_path) {
                channels.push(series_info);
                channels.extend(episodes);
            }
        }
    }
}

fn build_movie_playlist_item(
    entry: &MetadataCacheEntry,
    input_name: &Arc<str>,
    group_name: &str,
    api_base_path: &str,
) -> PlaylistItem {
    let metadata = &entry.metadata;
    let additional_properties = metadata_cache_entry_to_xtream_movie_info(entry, api_base_path);
    let title = metadata.title().intern();
    let group = group_name.intern();

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid(&entry.uuid),
            name: Arc::clone(&title),
            title,
            group,
            logo: metadata
                .poster()
                .or(thumbnail_url(entry, api_base_path).as_deref())
                .unwrap_or("")
                .intern(),
            url: concat_string!("file://", &entry.file_path).into(),
            xtream_cluster: XtreamCluster::Video,
            additional_properties,
            item_type: PlaylistItemType::LocalVideo,
            input_name: Arc::clone(input_name),
            ..PlaylistItemHeader::default()
        },
    }
}

fn build_series_playlist_items(
    entry: &MetadataCacheEntry,
    input_name: &Arc<str>,
    group_name: &str,
    api_base_path: &str,
) -> Option<(PlaylistItem, Vec<PlaylistItem>)> {
    let additional_properties = metadata_cache_entry_to_xtream_series_info(entry, api_base_path)?;
    let series_info = build_series_info_playlist_item(entry, input_name, group_name, api_base_path, &additional_properties);
    let episodes = build_series_episode_playlist_items(entry, input_name, group_name, api_base_path, &additional_properties);
    Some((series_info, episodes))
}

fn build_series_info_playlist_item(
    entry: &MetadataCacheEntry,
    input_name: &Arc<str>,
    group_name: &str,
    api_base_path: &str,
    additional_properties: &StreamProperties,
) -> PlaylistItem {
    let metadata = &entry.metadata;

    PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid(&entry.uuid),
            id: entry.uuid.clone().into(),
            name: metadata.title().intern(),
            group: group_name.intern(),
            title: metadata.title().intern(),
            logo: metadata
                .poster()
                .or(thumbnail_url(entry, api_base_path).as_deref())
                .unwrap_or("")
                .intern(),
            url: concat_string!("file://", &entry.file_path).into(),
            xtream_cluster: XtreamCluster::Series,
            item_type: PlaylistItemType::LocalSeriesInfo,
            input_name: Arc::clone(input_name),
            additional_properties: Some(additional_properties.clone()),
            ..PlaylistItemHeader::default()
        },
    }
}

fn build_series_episode_playlist_items(
    entry: &MetadataCacheEntry,
    input_name: &Arc<str>,
    group_name: &str,
    api_base_path: &str,
    additional_properties: &StreamProperties,
) -> Vec<PlaylistItem> {
    let metadata = &entry.metadata;
    let group_arc: Arc<str> = group_name.intern();

    match additional_properties {
        StreamProperties::Series(series_properties) => series_properties
            .details
            .as_ref()
            .and_then(|details| details.episodes.as_ref())
            .map(|episodes| {
                episodes
                    .iter()
                    .map(|episode| build_series_episode_playlist_item(entry, metadata, input_name, &group_arc, api_base_path, episode))
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn build_series_episode_playlist_item(
    entry: &MetadataCacheEntry,
    metadata: &MediaMetadata,
    input_name: &Arc<str>,
    group_name: &Arc<str>,
    api_base_path: &str,
    episode: &SeriesStreamDetailEpisodeProperties,
) -> PlaylistItem {
    let logo: Arc<str> = if episode.movie_image.is_empty() {
        metadata
            .poster()
            .or(thumbnail_url(entry, api_base_path).as_deref())
            .unwrap_or("")
            .intern()
    } else {
        episode.movie_image.clone()
    };
    let container_extension = Path::new(&*episode.direct_source)
        .extension()
        .and_then(|s| s.to_str())
        .map(ToString::to_string)
        .unwrap_or_default();

    PlaylistItem {
        header: PlaylistItemHeader {
            id: episode.id.to_string().into(),
            parent_code: entry.uuid.clone().into(),
            uuid: generate_local_playlist_uuid(input_name, PlaylistItemType::LocalSeries, &episode.direct_source),
            logo: logo.clone(),
            name: episode.title.clone(),
            group: Arc::clone(group_name),
            title: episode.title.clone(),
            url: episode.direct_source.clone(),
            xtream_cluster: XtreamCluster::Series,
            item_type: PlaylistItemType::LocalSeries,
            category_id: 0,
            input_name: Arc::clone(input_name),
            additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                episode_id: episode.id,
                episode: episode.episode_num,
                season: episode.season,
                added: Some(episode.added.clone()),
                release_date: Some(episode.release_date.clone()),
                series_release_date: None,
                tmdb: episode.tmdb,
                movie_image: logo,
                container_extension: container_extension.intern(),
                audio: None,
                video: None,
            }))),
            ..Default::default()
        },
    }
}

pub fn metadata_cache_entry_to_xtream_movie_info(
    entry: &MetadataCacheEntry,
    api_base_path: &str,
) -> Option<StreamProperties> {
    let movie = match &entry.metadata {
        MediaMetadata::Movie(m) => m,
        MediaMetadata::Series(_) => return None,
    };

    let binding = thumbnail_url(entry, api_base_path);
    let thumbnail_poster = binding.as_deref();
    let poster = movie.poster.as_deref().or(movie.fanart.as_deref()).or(thumbnail_poster);

    let container_extension = Path::new(&entry.file_path)
        .extension()
        .and_then(|s| s.to_str())
        .map(ToString::to_string).unwrap_or_default();

    let actor_names = movie.actors.as_ref().map(|a| a.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ").intern());

    let properties = VideoStreamProperties {
        name: movie.title.clone().into(),
        category_id: 0,
        stream_id: 0,
        stream_icon: poster.unwrap_or("").to_owned().into(),
        direct_source: "".into(),
        custom_sid: None,
        added: entry.file_modified.intern(),
        container_extension: container_extension.intern(),
        rating: movie.rating,
        rating_5based: None,
        stream_type: Some("movie".intern()),
        trailer: movie.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone().into())),
        tmdb: movie.tmdb_id,
        is_adult: 0,
        details: Some(VideoStreamDetailProperties {
            kinopoisk_url: movie.tmdb_id.map(|id| concat_string!("https://www.themoviedb.org/movie/", &id.to_string()).into()),
            o_name: movie.original_title.clone().map(Into::into),
            cover_big: poster.map(Into::into),
            movie_image: poster.map(Into::into),
            release_date: movie.year.map(|y| format!("{y}-01-01").into()),
            episode_run_time: movie.runtime,
            director: movie.directors.as_ref().map(|d| d.join(", ").into()),
            youtube_trailer: movie.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone().into())),
            actors: actor_names.clone(),
            cast: actor_names.clone(),
            genre: movie.genres.as_ref().map(|g| g.join(", ").into()),
            description: movie.plot.clone().map(Into::into),
            plot: movie.plot.clone().map(Into::into),
            age: None,
            mpaa_rating: movie.mpaa.clone().map(Into::into),
            rating_count_kinopoisk: 0,
            country: None,
            backdrop_path: movie
                .fanart
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|f| vec![f.clone().into()])
                .or_else(|| poster.map(|p| vec![p.into()])),
            duration_secs: movie.runtime.map(|r| (r * 60).to_string().into()),
            duration: movie.runtime.map(|r| {
                let h = r / 60;
                let m = r % 60;
                format!("{h:02}:{m:02}:00").into()
            }),

            video: None,
            audio: None,
            bitrate: 0,
            runtime: movie.runtime.map(|r| (r * 60).to_string().into()),
            status: Some("Released".intern()),
        }),
    };

    Some(StreamProperties::Video(Box::new(properties)))
}

#[allow(clippy::too_many_lines)]
pub fn metadata_cache_entry_to_xtream_series_info(
    entry: &MetadataCacheEntry,
    api_base_path: &str,
) -> Option<StreamProperties> {
    let series = match &entry.metadata {
        MediaMetadata::Movie(_) => return None,
        MediaMetadata::Series(m) => m,
    };

    let actor_names: Arc<str> = series.actors.as_ref().map(|a| a.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")).unwrap_or_default().into();
    let release_date = series.year.map(|y| format!("{y}-01-01"));
    let youtube_trailer = series.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone())).unwrap_or_default();

    let mut season_data = HashMap::new();
    series.seasons.as_ref().iter().for_each(|seasons| seasons.iter().for_each(|season_metadata| {
        season_data.insert(season_metadata.season_number, SeriesStreamDetailSeasonProperties {
            name: season_metadata.name.clone().into(),
            season_number: season_metadata.season_number,
            episode_count: 0,
            overview: season_metadata.overview.clone().map(Into::into),
            air_date: season_metadata.air_date.clone().map(Into::into),
            cover: season_metadata.poster_path.clone().map(Into::into),
            cover_tmdb: season_metadata.poster_path.clone().map(Into::into),
            cover_big: None,
            duration: Some(String::from("0").into()),
        });
    }));

    let episodes = series.episodes.as_ref().map(|episodes| {
        episodes.iter().filter(|episode| !episode.file_path.is_empty()).map(|episode| {
            let container_extension = Path::new(&episode.file_path)
                .extension()
                .and_then(|s| s.to_str())
                .map(ToString::to_string)
                .unwrap_or_default();
            let episode_release_date = episode.aired.as_ref().map(ToString::to_string).unwrap_or_default();
            let tmdb_id = (episode.tmdb_id > 0).then_some(episode.tmdb_id);
            let raw_episode_title: Arc<str> = episode.title.clone().into();
            let series_title: Arc<str> = series.title.clone().into();

            let season_entry = season_data.entry(episode.season).or_insert_with(|| {
                SeriesStreamDetailSeasonProperties {
                    name: concat_string!(&series.title, " ", &episode.season.to_string()).into(),
                    season_number: episode.season,
                    episode_count: 0,
                    overview: series.poster.clone().map(Into::into),
                    air_date: episode.aired.clone().map(Into::into),
                    cover: series.poster.clone().map(Into::into),
                    cover_tmdb: None,
                    cover_big: None,
                    duration: None,
                }
            });
            season_entry.episode_count = season_entry.episode_count.saturating_add(1);

            SeriesStreamDetailEpisodeProperties {
                id: tmdb_id.unwrap_or_default(),
                episode_num: episode.episode,
                season: episode.season,
                title: normalize_episode_title(&raw_episode_title, &series_title, episode.season, episode.episode),
                container_extension: container_extension.into(),
                custom_sid: None,
                added: episode.file_modified.to_string().into(),
                direct_source: episode.file_path.clone().into(),
                tmdb: tmdb_id,
                release_date: episode_release_date.clone().into(),
                series_release_date: None,
                plot: episode.plot.clone().map(Into::into),
                crew: Some(Arc::clone(&actor_names)),
                duration_secs: episode.runtime.map_or(0, |r| r * 60),
                duration: episode.runtime
                    .map(|r| format!("{:02}:{:02}:00", r / 60, r % 60))
                    .unwrap_or_default().into(),
                movie_image: episode
                    .thumb
                    .clone()
                    .or_else(|| episode_thumbnail_url(episode, api_base_path))
                    .unwrap_or_default()
                    .into(),
                audio: None,
                video: None,
                bitrate: 0,
                rating: None,
            }
        }).collect::<Vec<_>>()
    });


    let mut seasons = season_data.into_values().collect::<Vec<_>>();
    seasons.sort_by_key(|s| s.season_number);

    let properties = SeriesStreamProperties {
        name: series.title.clone().into(),
        category_id: 0,
        series_id: 0,
        backdrop_path: series
            .fanart
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|f| vec![f.clone().into()])
            .or_else(|| {
                series.poster.as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|p| vec![p.clone().into()])
            }),
        cast: Arc::clone(&actor_names),
        cover: series.poster.clone().unwrap_or_default().into(),
        director: series.directors.as_ref().map(|d| d.join(", ")).unwrap_or_default().into(),
        episode_run_time: None,
        genre: series.genres.as_ref().map(|d| d.join(", ").into()),
        last_modified: Some(series.last_updated.to_string().into()),
        plot: series.plot.clone().map(Into::into),
        rating: series.rating.unwrap_or(0f64),
        rating_5based: 0.0,
        release_date: release_date.map(Into::into),
        youtube_trailer: youtube_trailer.into(),
        tmdb: series.tmdb_id,
        details: Some(SeriesStreamDetailProperties {
            year: series.year,
            seasons: Some(seasons),
            episodes,
        }),
    };

    Some(StreamProperties::Series(Box::new(properties)))
}

#[cfg(test)]
mod tests {
    use super::{metadata_cache_entry_to_xtream_series_info, thumbnail_url};
    use crate::library::{MediaMetadata, MetadataCacheEntry, MovieMetadata, SeriesMetadata, EpisodeMetadata};
    use shared::model::StreamProperties;

    #[test]
    fn thumbnail_url_uses_v1_api_prefix() {
        let mut entry = MetadataCacheEntry::new(
            "/tmp/video.mkv".to_string(),
            123,
            456,
            MediaMetadata::Movie(MovieMetadata::default()),
        );
        entry.uuid = "test-uuid-123".to_string();
        entry.thumbnail_hash = Some("thumb-hash".to_string());

        assert_eq!(
            thumbnail_url(&entry, "/api/v1").as_deref(),
            Some("/api/v1/library/thumbnail/test-uuid-123"),
        );
    }

    #[test]
    fn series_episode_uses_local_thumbnail_when_episode_thumb_missing() {
        let entry = MetadataCacheEntry::new(
            "/tmp/show/S01E01.mkv".to_string(),
            123,
            456,
            MediaMetadata::Series(SeriesMetadata {
                title: "Test Series".to_string(),
                episodes: Some(vec![EpisodeMetadata {
                    title: "Episode 1".to_string(),
                    season: 1,
                    episode: 1,
                    file_path: "/tmp/show/S01E01.mkv".to_string(),
                    thumbnail_id: Some("31fedbc18dca3fa273fba98afda584486ad4f1d8e1ca06740435b97b14f2ec8b".to_string()),
                    ..EpisodeMetadata::default()
                }]),
                ..SeriesMetadata::default()
            }),
        );

        let Some(StreamProperties::Series(series)) = metadata_cache_entry_to_xtream_series_info(&entry, "/api/v1") else {
            panic!("expected series stream properties");
        };
        let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("episodes missing");
        let episode = episodes.first().expect("episode missing");

        assert_eq!(
            episode.movie_image.as_ref(),
            "/api/v1/library/thumbnail/31fedbc18dca3fa273fba98afda584486ad4f1d8e1ca06740435b97b14f2ec8b",
        );
    }
}
