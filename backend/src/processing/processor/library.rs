use crate::library::{MediaMetadata, MetadataAsyncIter, MetadataCacheEntry};
use crate::model::{AppConfig, ConfigInput};
use shared::concat_string;
use shared::error::TuliproxError;
use shared::model::UUIDType;
use shared::model::{EpisodeStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties, SeriesStreamProperties, StreamProperties, VideoStreamDetailProperties, VideoStreamProperties, XtreamCluster};
use shared::utils::{generate_playlist_uuid, Internable};
use std::collections::HashMap;
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
    while let Some(entry) = metadata_iter.next().await {
        match entry.metadata {
            MediaMetadata::Movie(_) => {
                to_playlist_item(&entry, &input.name, &library_config.playlist.movie_category, &mut group_movies.channels);
            }
            MediaMetadata::Series(_) => {
                to_playlist_item(&entry, &input.name, &library_config.playlist.series_category, &mut group_series.channels);
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


fn to_playlist_item(entry: &MetadataCacheEntry, input_name: &Arc<str>, group_name: &str, channels: &mut Vec<PlaylistItem>) {
    let metadata = &entry.metadata;

    match metadata {
        MediaMetadata::Movie(_) => {
            let additional_properties = metadata_cache_entry_to_xtream_movie_info(entry);
            let title = metadata.title().intern();
            let group = group_name.intern();
            channels.push(PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: UUIDType::from_valid_uuid(&entry.uuid),
                    name: Arc::clone(&title),
                    title,
                    group,
                    logo: metadata.poster().unwrap_or("").intern(),
                    url: concat_string!("file://", &entry.file_path).into(),
                    xtream_cluster: XtreamCluster::Video,
                    additional_properties,
                    item_type: PlaylistItemType::LocalVideo,
                    input_name: Arc::clone(input_name),
                    ..PlaylistItemHeader::default()
                }
            });
        }
        MediaMetadata::Series(_series) => {
            if let Some(additional_properties) = metadata_cache_entry_to_xtream_series_info(entry) {
                let mut episodes = vec![];
                let group_arc: Arc<str> = group_name.intern();
                if let StreamProperties::Series(series_properties) = &additional_properties {
                    if let Some(details_props) = series_properties.details.as_ref() {
                        if let Some(prop_episodes) = details_props.episodes.as_ref() {
                            for episode in prop_episodes {
                                let logo: Arc<str> = if episode.movie_image.is_empty() { metadata.poster().unwrap_or("").intern() } else { episode.movie_image.clone() };
                                let container_extension = Path::new(&*episode.direct_source)
                                    .extension()
                                    .and_then(|s| s.to_str())
                                    .map(ToString::to_string).unwrap_or_default();
                                episodes.push(PlaylistItem {
                                    header: PlaylistItemHeader {
                                        id: episode.id.to_string().into(),
                                        // we use parent_code for local series to find the parent series info and straighten the virtual_ids
                                        parent_code: entry.uuid.clone().into(),
                                        uuid: generate_playlist_uuid(input_name, &episode.id.to_string(), PlaylistItemType::LocalSeries, &episode.direct_source),
                                        logo: logo.clone(),
                                        name: episode.title.clone(),
                                        group: Arc::clone(&group_arc),
                                        title: episode.title.clone(),
                                        url: episode.direct_source.clone(),
                                        xtream_cluster: XtreamCluster::Series,
                                        item_type: PlaylistItemType::LocalSeries,
                                        category_id: 0,
                                        input_name: Arc::clone(input_name),
                                        additional_properties: Some(StreamProperties::Episode(EpisodeStreamProperties {
                                            episode_id: episode.id,
                                            episode: episode.episode_num,
                                            season: episode.season,
                                            added: Some(episode.added.clone()),
                                            release_date: Some(episode.release_date.clone()),
                                            tmdb: episode.tmdb,
                                            movie_image: logo,
                                            container_extension: container_extension.intern(),
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
                        id: entry.uuid.clone().into(),
                        name: metadata.title().intern(),
                        group: group_name.intern(),
                        title: metadata.title().intern(),
                        logo: metadata.poster().unwrap_or("").intern(),
                        url: concat_string!("file://", &entry.file_path).into(),
                        xtream_cluster: XtreamCluster::Series,
                        item_type: PlaylistItemType::LocalSeriesInfo,
                        input_name: Arc::clone(input_name),
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

    let actor_names = movie.actors.as_ref().map(|a| a.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ").intern());

    let properties = VideoStreamProperties {
        name: movie.title.clone().into(),
        category_id: 0,
        stream_id: 0,
        stream_icon: movie.poster.as_deref().or(movie.fanart.as_deref()).unwrap_or("").to_owned().into(),
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
            cover_big: movie.poster.clone().map(Into::into),
            movie_image: movie.poster.clone().map(Into::into),
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
                .or_else(|| {
                    movie.poster
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .map(|p| vec![p.clone().into()])
                }),
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
                title: episode.title.clone().into(),
                container_extension: container_extension.into(),
                custom_sid: None,
                added: episode.file_modified.to_string().into(),
                direct_source: episode.file_path.clone().into(),
                tmdb: tmdb_id,
                release_date: episode_release_date.clone().into(),
                plot: episode.plot.clone().map(Into::into),
                crew: Some(Arc::clone(&actor_names)),
                duration_secs: episode.runtime.map_or(0, |r| r * 60),
                duration: episode.runtime
                    .map(|r| format!("{:02}:{:02}:00", r / 60, r % 60))
                    .unwrap_or_default().into(),
                movie_image: episode.thumb.clone().unwrap_or_default().into(),
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