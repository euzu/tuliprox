use crate::library::{MediaMetadata, MetadataAsyncIter, MetadataCacheEntry};
use crate::model::{AppConfig, ConfigInput};
use shared::error::TuliproxError;
use shared::model::{EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties, StreamProperties, UUIDType, VideoStreamDetailProperties, VideoStreamProperties, XtreamCluster};
use shared::utils::{generate_playlist_uuid, StringInterner};
use std::path::Path;
use std::sync::Arc;

pub async fn download_library_playlist(_client: &reqwest::Client, app_config: &Arc<AppConfig>, input: &ConfigInput) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    let config = &*app_config.config.load();
    let Some(library_config) = config.library.as_ref() else { return (vec![], vec![]) };
    if !library_config.enabled { return (vec![], vec![]); }

    let storage_path = std::path::PathBuf::from(&library_config.metadata.path);
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
    let mut interner = StringInterner::new();
    while let Some(entry) = metadata_iter.next().await {
        match entry.metadata {
            MediaMetadata::Movie(_) => {
                to_playlist_item(&mut interner, &entry, &input.name, &library_config.playlist.movie_category, &mut group_movies.channels);
            }
            MediaMetadata::Series(_) => {
                to_playlist_item(&mut interner, &entry, &input.name, &library_config.playlist.series_category, &mut group_series.channels);
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


fn to_playlist_item(interner: &mut StringInterner, entry: &MetadataCacheEntry, input_name: &str, group_name: &str, channels: &mut Vec<PlaylistItem>) {
    let metadata = &entry.metadata;

    match metadata {
        MediaMetadata::Movie(_) => {
            let additional_properties = metadata_cache_entry_to_xtream_movie_info(entry);
            channels.push(PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: UUIDType::from_valid_uuid(&entry.uuid),
                    name: metadata.title().to_string(),
                    group: interner.intern(group_name),
                    title: metadata.title().to_string(),
                    logo: metadata.poster().map_or_else(String::new, ToString::to_string),
                    url: format!("file://{}", entry.file_path),
                    xtream_cluster: XtreamCluster::Video,
                    additional_properties,
                    item_type: PlaylistItemType::LocalVideo,
                    input_name: interner.intern(input_name),
                    ..PlaylistItemHeader::default()
                }
            });
        }
        MediaMetadata::Series(_series) => {
            if let Some(additional_properties) = metadata_cache_entry_to_xtream_series_info(entry) {
                let mut episodes = vec![];
                if let StreamProperties::Series(series_properties) = &additional_properties {
                    if let Some(details_props) = series_properties.details.as_ref() {
                        if let Some(prop_episodes) = details_props.episodes.as_ref() {
                            for episode in prop_episodes {
                                let logo = if episode.movie_image.is_empty() { metadata.poster().map_or_else(String::new, ToString::to_string) } else { episode.movie_image.clone() };
                                let container_extension = Path::new(&episode.direct_source)
                                    .extension()
                                    .and_then(|s| s.to_str())
                                    .map(ToString::to_string).unwrap_or_default();
                                episodes.push(PlaylistItem {
                                    header: PlaylistItemHeader {
                                        id: episode.id.to_string(),
                                        // we use parent_code for local series to find the parent series info and straighten the virtual_ids
                                        parent_code: entry.uuid.clone(),
                                        uuid: generate_playlist_uuid(input_name, &episode.id.to_string(), PlaylistItemType::LocalSeries, &episode.direct_source),
                                        logo: logo.clone(),
                                        name: episode.title.clone(),
                                        group: interner.intern(group_name),
                                        title: episode.title.clone(),
                                        url: episode.direct_source.clone(),
                                        xtream_cluster: XtreamCluster::Series,
                                        item_type: PlaylistItemType::LocalSeries,
                                        category_id: 0,
                                        input_name: interner.intern(input_name),
                                        additional_properties: Some(StreamProperties::Episode(EpisodeStreamProperties {
                                            episode_id: episode.id,
                                            episode: episode.episode_num,
                                            season: episode.season,
                                            added: Some(episode.added.clone()),
                                            release_date: Some(episode.release_date.clone()),
                                            tmdb: episode.tmdb,
                                            movie_image: logo,
                                            container_extension,
                                            audio: None,
                                            video: None,
                                        })),
                                        ..Default::default()
                                    }
                                });
                            }
                        }
                    }
                }

                let series_info = PlaylistItem {
                    header: PlaylistItemHeader {
                        uuid: UUIDType::from_valid_uuid(&entry.uuid),
                        id: entry.uuid.clone(),
                        name: metadata.title().to_string(),
                        group: interner.intern(group_name),
                        title: metadata.title().to_string(),
                        logo: metadata.poster().map_or_else(String::new, ToString::to_string),
                        url: format!("file://{}", entry.file_path),
                        xtream_cluster: XtreamCluster::Series,
                        item_type: PlaylistItemType::LocalSeriesInfo,
                        input_name: interner.intern(input_name),
                        additional_properties: Some(additional_properties),
                        ..PlaylistItemHeader::default()
                    }
                };
                channels.push(series_info);
                channels.extend(episodes);
            }
        }
    }
}

pub fn metadata_cache_entry_to_xtream_movie_info(
    entry: &MetadataCacheEntry,
) -> Option<StreamProperties> {
    let movie = match &entry.metadata {
        MediaMetadata::Movie(m) => m,
        MediaMetadata::Series(_) => return None,
    };

    let container_extension = Path::new(&entry.file_path)
        .extension()
        .and_then(|s| s.to_str())
        .map(ToString::to_string).unwrap_or_default();

    let actor_names = movie.actors.as_ref().map(|a| a.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", "));

    let properties = VideoStreamProperties {
        name: movie.title.clone(),
        category_id: 0,
        stream_id: 0,
        stream_icon: movie.poster.as_deref().or(movie.fanart.as_deref()).unwrap_or("").to_owned(),
        direct_source: String::new(),
        custom_sid: None,
        added: entry.file_modified.to_string(),
        container_extension,
        rating: movie.rating,
        rating_5based: None,
        stream_type: Some("movie".to_string()),
        trailer: movie.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone())),
        tmdb: movie.tmdb_id,
        is_adult: 0,
        details: Some(VideoStreamDetailProperties {
            kinopoisk_url: movie.tmdb_id.map(|id| format!("https://www.themoviedb.org/movie/{id}")),
            o_name: movie.original_title.clone(),
            cover_big: movie.poster.clone(),
            movie_image: movie.poster.clone(),
            release_date: movie.year.map(|y| format!("{y}-01-01")),
            episode_run_time: movie.runtime,
            director: movie.directors.as_ref().map(|d| d.join(", ")),
            youtube_trailer: movie.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone())),
            actors: actor_names.clone(),
            cast: actor_names,
            genre: movie.genres.as_ref().map(|g| g.join(", ")),
            description: movie.plot.clone(),
            plot: movie.plot.clone(),
            age: None,
            mpaa_rating: movie.mpaa.clone(),
            rating_count_kinopoisk: 0,
            country: None,
            backdrop_path: movie
                .fanart
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|f| vec![f.clone()])
                .or_else(|| {
                    movie.poster
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .map(|p| vec![p.clone()])
                }),
            duration_secs: movie.runtime.map(|r| (r * 60).to_string()),
            duration: movie.runtime.map(|r| {
                let h = r / 60;
                let m = r % 60;
                format!("{h:02}:{m:02}:00")
            }),

            video: None,
            audio: None,
            bitrate: 0,
            runtime: movie.runtime.map(|r| (r * 60).to_string()),
            status: Some("Released".to_string()),
        }),
    };

    Some(StreamProperties::Video(Box::new(properties)))
}

pub fn metadata_cache_entry_to_xtream_series_info(
    entry: &MetadataCacheEntry,
) -> Option<StreamProperties> {
    let series = match &entry.metadata {
        MediaMetadata::Movie(_) => return None,
        MediaMetadata::Series(m) => m,
    };

    let actor_names = series.actors.as_ref().map(|a| a.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let release_date = series.year.map(|y| format!("{y}-01-01"));
    let youtube_trailer = series.videos.as_ref().and_then(|v| v.iter().find(|video| video.site.eq_ignore_ascii_case("youtube")).map(|video| video.key.clone())).unwrap_or_default();

    let episodes = series.episodes.as_ref().map(|episodes| {
        episodes.iter().filter(|episode| !episode.file_path.is_empty()).map(|episode| {
            let container_extension = Path::new(&episode.file_path)
                .extension()
                .and_then(|s| s.to_str())
                .map(ToString::to_string)
                .unwrap_or_default();
            let episode_release_date = episode.aired.as_ref().map(ToString::to_string).unwrap_or_default();
            let tmdb_id = (episode.tmdb_id > 0).then_some(episode.tmdb_id);

            SeriesStreamDetailEpisodeProperties {
                id: tmdb_id.unwrap_or_default(),
                episode_num: episode.episode,
                season: episode.season,
                title: episode.title.clone(),
                container_extension,
                custom_sid: None,
                added: episode.file_modified.to_string(),
                direct_source: episode.file_path.clone(),
                tmdb: tmdb_id,
                release_date: episode_release_date.clone(),
                plot: episode.plot.clone(),
                duration_secs: episode.runtime.map_or(0, |r| r * 60),
                duration: episode.runtime
                    .map(|r| format!("{:02}:{:02}:00", r / 60, r % 60))
                    .unwrap_or_default(),
                movie_image: episode.thumb.clone().unwrap_or_default(),
                audio: None,
                video: None,
                bitrate: 0,
                rating: None,
            }
        }).collect::<Vec<_>>()
    });

    let properties = SeriesStreamProperties {
        name: series.title.clone(),
        category_id: 0,
        series_id: 0,
        backdrop_path: series
            .fanart
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|f| vec![f.clone()])
            .or_else(|| {
                series.poster.as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|p| vec![p.clone()])
            }),
        cast: actor_names,
        cover: series.poster.clone().unwrap_or_default(),
        director: series.directors.as_ref().map(|d| d.join(", ")).unwrap_or_default(),
        episode_run_time: None,
        genre: series.genres.as_ref().map(|d| d.join(", ")),
        last_modified: Some(series.last_updated.to_string()),
        plot: series.plot.clone(),
        rating: series.rating.unwrap_or(0f64),
        rating_5based: 0.0,
        release_date,
        youtube_trailer,
        tmdb: series.tmdb_id,
        details: Some(SeriesStreamDetailProperties {
            year: series.year,
            // seasons are not delivered through xtream get_series_info.
            episodes,
        }),
    };

    Some(StreamProperties::Series(Box::new(properties)))
}