use crate::model::info_doc_utils::InfoDocUtils;
use crate::model::{PlaylistEntry, XtreamSeriesInfo, XtreamSeriesInfoDoc, XtreamVideoInfo};
use crate::utils::{arc_str_default_on_null, arc_str_none_default_on_null, arc_str_option_serde,
                   deserialize_as_option_arc_str, deserialize_as_string_array,
                   deserialize_json_as_opt_string, deserialize_number_from_string,
                   deserialize_number_from_string_or_zero, serialize_json_as_opt_string,
                   serialize_option_string_as_null_if_empty, Internable};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use log::{warn};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LiveStreamProperties {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub stream_id: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub stream_icon: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub direct_source: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str", serialize_with = "serialize_option_string_as_null_if_empty")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub added: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub stream_type: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive_duration: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub is_adult: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VideoStreamDetailProperties {
    #[serde(default, with = "arc_str_option_serde")]
    pub kinopoisk_url: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub o_name: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub cover_big: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub movie_image: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub release_date: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub episode_run_time: Option<u32>,
    #[serde(default, with = "arc_str_option_serde")]
    pub youtube_trailer: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub director: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub actors: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub cast: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub description: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub plot: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub age: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub mpaa_rating: Option<Arc<str>>,
    #[serde(default)]
    pub rating_count_kinopoisk: u32,
    #[serde(default, with = "arc_str_option_serde")]
    pub country: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub genre: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_string_array")]
    pub backdrop_path: Option<Vec<Arc<str>>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub duration_secs: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub duration: Option<Arc<str>>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub video: Option<Arc<str>>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub audio: Option<Arc<str>>,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default, with = "arc_str_option_serde")]
    pub runtime: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub status: Option<Arc<str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VideoStreamProperties {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub stream_id: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub stream_icon: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub direct_source: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str", serialize_with = "serialize_option_string_as_null_if_empty")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub added: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_default_on_null")]
    pub container_extension: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub rating: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub rating_5based: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub stream_type: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub trailer: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub is_adult: i32,
    #[serde(default)]
    pub details: Option<VideoStreamDetailProperties>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SeriesStreamDetailSeasonProperties {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season_number: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_count: u32,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub overview: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub air_date: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub cover: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub cover_tmdb: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub cover_big: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub duration: Option<Arc<str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SeriesStreamDetailEpisodeProperties {
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_num: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub title: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_default_on_null")]
    pub container_extension: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str", serialize_with = "serialize_option_string_as_null_if_empty")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub added: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub direct_source: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub release_date: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub plot: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub crew: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub duration_secs: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub duration: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub movie_image: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub bitrate: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub rating: Option<f64>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub video: Option<Arc<str>>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub audio: Option<Arc<str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SeriesStreamDetailProperties {
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub year: Option<u32>,
    #[serde(default)]
    pub seasons: Option<Vec<SeriesStreamDetailSeasonProperties>>,
    #[serde(default)]
    pub episodes: Option<Vec<SeriesStreamDetailEpisodeProperties>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SeriesStreamProperties {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub series_id: u32,
    #[serde(default, deserialize_with = "deserialize_as_string_array")]
    pub backdrop_path: Option<Vec<Arc<str>>>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub cast: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub cover: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub director: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub episode_run_time: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub genre: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub last_modified: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub plot: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub rating: f64,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub rating_5based: f64,
    #[serde(default, with = "arc_str_option_serde")]
    pub release_date: Option<Arc<str>>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub youtube_trailer: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default)]
    pub details: Option<SeriesStreamDetailProperties>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EpisodeStreamProperties {
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season: u32,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub added: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub release_date: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub movie_image: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_default_on_null")]
    pub container_extension: Arc<str>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub video: Option<Arc<str>>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub audio: Option<Arc<str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum StreamProperties {
    Live(LiveStreamProperties),
    Video(Box<VideoStreamProperties>),
    Series(Box<SeriesStreamProperties>),
    Episode(EpisodeStreamProperties),
}

impl StreamProperties {
    pub fn has_details(&self) -> bool {
        match self {
            StreamProperties::Video(video) => video.details.is_some(),
            StreamProperties::Series(series) => series.details.is_some(),
            StreamProperties::Live(_)
            | StreamProperties::Episode(_) => false,
        }
    }

    pub fn prepare(&mut self) {
        match self {
            StreamProperties::Live(live) => {
                live.epg_channel_id = live.epg_channel_id.as_ref()
                    .filter(|epg_id| !epg_id.trim().is_empty())
                    .map(|epg_id| epg_id.to_lowercase().intern())
                    .or(live.epg_channel_id.clone());
            }
            StreamProperties::Video(_) => {}
            StreamProperties::Series(_) => {}
            StreamProperties::Episode(_) => {}
        }
    }

    pub fn get_category_id(&self) -> u32 {
        match self {
            StreamProperties::Live(live) => live.category_id,
            StreamProperties::Video(video) => video.category_id,
            StreamProperties::Series(series) => series.category_id,
            StreamProperties::Episode(_episode) => 0,
        }
    }

    pub fn get_stream_id(&self) -> u32 {
        match self {
            StreamProperties::Live(live) => live.stream_id,
            StreamProperties::Video(video) => video.stream_id,
            StreamProperties::Series(series) => series.series_id,
            StreamProperties::Episode(episode) => episode.episode_id,
        }
    }

    pub fn get_stream_icon(&self) -> Arc<str> {
        match self {
            StreamProperties::Live(live) => Arc::clone(&live.stream_icon),
            StreamProperties::Video(video) => Arc::clone(&video.stream_icon),
            StreamProperties::Series(series) => Arc::clone(&series.cover),
            StreamProperties::Episode(episode) => Arc::clone(&episode.movie_image),
        }
    }

    pub fn get_name(&self) -> Arc<str> {
        match self {
            StreamProperties::Live(live) => Arc::clone(&live.name),
            StreamProperties::Video(video) => Arc::clone(&video.name),
            StreamProperties::Series(series) => Arc::clone(&series.name),
            StreamProperties::Episode(_episode) => "".intern(),
        }
    }

    pub fn get_epg_channel_id(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(live) => live.epg_channel_id.clone(),
            StreamProperties::Video(_) => None,
            StreamProperties::Series(_) => None,
            StreamProperties::Episode(_) => None,
        }
    }

    pub fn get_tmdb_id(&self) -> Option<u32> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => video.tmdb,
            StreamProperties::Series(series) => series.tmdb,
            StreamProperties::Episode(episode) => episode.tmdb,
        }
    }

    pub fn get_release_date(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => video.details.as_ref().and_then(|d| d.release_date.as_ref().map(Arc::clone)),
            StreamProperties::Series(series) => series.release_date.as_ref().map(Arc::clone),
            StreamProperties::Episode(episode) => episode.release_date.as_ref().map(Arc::clone),
        }
    }

    pub fn get_added(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.added),
            StreamProperties::Series(series) => series.details.as_ref()
                .and_then(|d| d.episodes.as_ref())
                .and_then(|e| e.first().and_then(|e| non_empty_arc(&e.added))),
            StreamProperties::Episode(episode) => episode.added.as_ref().map(Arc::clone),
        }
    }

    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.container_extension),
            StreamProperties::Series(series) => series.details.as_ref()
                .and_then(|d| d.episodes.as_ref())
                .and_then(|e| e.first().and_then(|e| non_empty_arc(&e.container_extension))),
            StreamProperties::Episode(episode) => non_empty_arc(&episode.container_extension),
        }
    }

    pub fn get_direct_source(&self) -> Option<Arc<str>> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => non_empty_arc(&video.direct_source),
            StreamProperties::Series(series) => series.details.as_ref()
                .and_then(|d| d.episodes.as_ref())
                .and_then(|e| e.first().and_then(|e| non_empty_arc(&e.direct_source))),
            StreamProperties::Episode(_episode) => None,
        }
    }

    pub fn get_season(&self) -> Option<u32> {
        match self {
            StreamProperties::Live(_)
            | StreamProperties::Video(_)
            | StreamProperties::Series(_) => None,
            StreamProperties::Episode(episode) => Some(episode.season),
        }
    }

    pub fn get_episode(&self) -> Option<u32> {
        match self {
            StreamProperties::Live(_)
            | StreamProperties::Video(_)
            | StreamProperties::Series(_) => None,
            StreamProperties::Episode(episode) => Some(episode.episode),
        }
    }

    pub fn get_last_modified(&self) -> Option<u64> {
        match self {
            StreamProperties::Live(_) => None,
            StreamProperties::Video(video) => video.added.parse::<u64>().ok(),
            StreamProperties::Series(series) => series.last_modified.as_ref().and_then(|v| v.parse::<u64>().ok()),
            StreamProperties::Episode(episode) => episode.added.as_ref().and_then(|v| v.parse::<u64>().ok()),
        }
    }

    pub fn resolve_resource_url(&self, field: &str) -> Option<Arc<str>> {
        if field.starts_with("backdrop_path") {
            if let StreamProperties::Series(series) = self {
                if let Some(backdrop) = series.backdrop_path.as_ref() {
                    if let Some(url) = backdrop.first() {
                        return Some(Arc::clone(url));
                    }
                }
            }
            return None;
        } else if field.starts_with("nfo_backdrop_path") {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(backdrop) = details.backdrop_path.as_ref() {
                        if let Some(url) = backdrop.first() {
                            return Some(Arc::clone(url));
                        }
                    }
                }
            }
            return None;
        }

        if field == "cover" {
            if let StreamProperties::Series(series) = self {
                return Some(Arc::clone(&series.cover));
            }
            return None;
        }
        if field == "logo" || field == "logo_small" {
            return match self {
                StreamProperties::Live(live) => {
                    Some(Arc::clone(&live.stream_icon))
                }
                StreamProperties::Video(video) => {
                    Some(Arc::clone(&video.stream_icon))
                }
                StreamProperties::Series(series) => {
                    Some(Arc::clone(&series.cover))
                }
                StreamProperties::Episode(episode) => {
                    Some(Arc::clone(&episode.movie_image))
                }
            };
        }
        if field == "movie_image" {
            if let StreamProperties::Episode(episode) = self {
                return Some(Arc::clone(&episode.movie_image));
            }
            return None;
        }
        if field == "nfo_cover_big" {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(cover_big) = details.cover_big.as_ref() {
                        return Some(Arc::clone(cover_big));
                    }
                }
            }
        }

        if field == "nfo_movie_image" {
            if let StreamProperties::Video(video) = self {
                if let Some(details) = video.details.as_ref() {
                    if let Some(movie_image) = details.movie_image.as_ref() {
                        return Some(Arc::clone(movie_image));
                    }
                }
            }
            return None;
        }

        if field.starts_with("nfo_s_") {
            if let Some((season_num, field)) = parse_season_field(field) {
                if let StreamProperties::Series(series) = self {
                    if let Some(details) = series.details.as_ref() {
                        if let Some(seasons) = details.seasons.as_ref() {
                            for season in seasons {
                                if season.season_number == season_num {
                                    if field == "cover" {
                                        return season.cover.as_ref().map(Arc::clone);
                                    }
                                    if field == "cover_tmdb" {
                                        return season.cover_tmdb.as_ref().map(Arc::clone);
                                    }
                                    if field == "cover_big" {
                                        return season.cover_big.as_ref().map(Arc::clone);
                                    }
                                    if field == "overview" {
                                        return season.overview.as_ref().map(Arc::clone);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if field.starts_with("nfo_ep_") {
            if let Some((season, episode_num, field)) = parse_season_episode_field(field) {
                if let StreamProperties::Series(series) = self {
                    if let Some(details) = series.details.as_ref() {
                        if let Some(episodes) = details.episodes.as_ref() {
                            for episode in episodes {
                                if episode.season == season && episode_num == episode.episode_num && field == "movie_image" {
                                    return Some(Arc::clone(&episode.movie_image));
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

fn parse_season_field(s: &str) -> Option<(u32, String)> {
    let mut parts = s.split('_');

    let (prefix, suffix) = (parts.next()?, parts.next()?);
    if prefix != "nfo" || suffix != "s" {
        return None;
    }

    let season: u32 = parts.next()?.parse().ok()?;

    let kind = parts.collect::<Vec<_>>().join("_");
    if kind.is_empty() {
        return None;
    }

    Some((season, kind))
}

fn parse_season_episode_field(s: &str) -> Option<(u32, u32, String)> {
    let mut parts = s.split('_');

    let (prefix, ep) = (parts.next()?, parts.next()?);
    if prefix != "nfo" || ep != "ep" {
        return None;
    }

    let season: u32 = parts.next()?.parse().ok()?;
    let episode: u32 = parts.next()?.parse().ok()?;

    let kind = parts.collect::<Vec<_>>().join("_");
    if kind.is_empty() {
        return None;
    }

    Some((season, episode, kind))
}


impl VideoStreamProperties {
    pub fn from_info<P>(info: &XtreamVideoInfo, pli: &P) -> VideoStreamProperties
    where
        P: PlaylistEntry,
    {
        let mut props = VideoStreamProperties {
            name: info.info.name.clone(),
            category_id: info.movie_data.category_id,
            stream_id: info.movie_data.stream_id,
            stream_icon: info.info.movie_image.as_ref().map_or_else(|| "".intern(), Clone::clone),
            direct_source: info.movie_data.direct_source.clone(),
            custom_sid: info.movie_data.custom_sid.clone(),
            added: info.movie_data.added.clone(),
            container_extension: info.movie_data.container_extension.clone(),
            rating: None, // from PlaylistItem
            rating_5based: None, // from PlaylistItem
            stream_type: None, // from PlaylistItem
            trailer: info.info.youtube_trailer.clone(),
            tmdb: info.info.tmdb_id.parse::<u32>().ok(),
            is_adult: 0,  // from PlaylistItem
            details: Some(VideoStreamDetailProperties {
                kinopoisk_url: info.info.kinopoisk_url.clone(),
                o_name: info.info.o_name.clone(),
                cover_big: info.info.cover_big.clone(),
                movie_image: info.info.movie_image.clone(),
                release_date: info.info.releasedate.clone(),
                episode_run_time: info.info.episode_run_time,
                youtube_trailer: info.info.youtube_trailer.clone(),
                director: info.info.director.clone(),
                actors: info.info.actors.clone(),
                cast: info.info.cast.clone(),
                description: info.info.description.clone(),
                plot: info.info.plot.clone(),
                age: info.info.age.clone(),
                mpaa_rating: info.info.mpaa_rating.clone(),
                rating_count_kinopoisk: info.info.rating_count_kinopoisk,
                country: info.info.country.clone(),
                genre: info.info.genre.clone(),
                backdrop_path: info.info.backdrop_path.clone(),
                duration_secs: info.info.duration_secs.clone(),
                duration: info.info.duration.clone(),
                video: info.info.video.clone(),
                audio: info.info.audio.clone(),
                bitrate: info.info.bitrate,
                runtime: info.info.runtime.clone(),
                status: info.info.status.clone(),
            }),
        };

        if let Some(StreamProperties::Video(video)) = pli.get_additional_properties() {
            props.rating = video.rating;
            props.rating_5based = video.rating_5based;
            props.stream_type = video.stream_type.clone();
            if props.tmdb.is_none() {
                props.tmdb = video.tmdb;
            }
            if props.trailer.is_none() {
                props.trailer = video.trailer.clone();
            }
            props.is_adult = video.is_adult;
        } else {
            props.stream_type = Some("movie".intern());
        }

        props
    }
}

impl SeriesStreamProperties {
    pub fn from_info<P>(info: &XtreamSeriesInfo, pli: &P) -> SeriesStreamProperties
    where
        P: PlaylistEntry,
    {
        SeriesStreamProperties {
            name: info.info.name.clone(),
            category_id: info.info.category_id,
            series_id: pli.get_virtual_id(),
            backdrop_path: info.info.backdrop_path.clone(),
            cast: info.info.cast.clone(),
            cover: info.info.cover.clone(),
            director: info.info.director.clone(),
            episode_run_time: Some(info.info.episode_run_time.clone()),
            genre: Some(info.info.genre.clone()),
            last_modified: Some(info.info.last_modified.clone()),
            plot: Some(info.info.plot.clone()),
            rating: info.info.rating,
            rating_5based: info.info.rating_5based,
            release_date: Some(info.info.release_date.clone()),
            youtube_trailer: info.info.youtube_trailer.clone(),
            tmdb: info.info.tmdb,
            details: Some(SeriesStreamDetailProperties {
                year: InfoDocUtils::extract_year_from_release_date(&info.info.release_date),
                seasons: info.seasons.as_ref().map(|list| {
                    let mut seasons: Vec<SeriesStreamDetailSeasonProperties> = list.iter().map(|s| {
                        SeriesStreamDetailSeasonProperties {
                            name: s.name.clone(),
                            season_number: s.season_number,
                            episode_count: s.episode_count,
                            overview: Some(s.overview.clone()),
                            air_date: Some(s.air_date.clone()),
                            cover: Some(s.cover.clone()),
                            cover_tmdb: Some(s.cover_tmdb.clone()),
                            cover_big: Some(s.cover_big.clone()),
                            duration: Some(s.duration.clone()),
                        }
                    }).collect();
                    seasons.sort_by_key(|season| season.season_number);
                    seasons
                }),
                episodes: info.episodes.as_ref().map(|list| {
                    let mut episodes: Vec<SeriesStreamDetailEpisodeProperties> = list.iter().map(|e| {
                        SeriesStreamDetailEpisodeProperties {
                            id: e.id,
                            episode_num: e.episode_num,
                            season: e.season,
                            title: e.title.clone(),
                            container_extension: e.container_extension.clone(),
                            custom_sid: e.custom_sid.clone(),
                            added: e.added.clone(),
                            direct_source: e.direct_source.clone(),
                            tmdb: info.info.tmdb,
                            release_date: e.info.as_ref().map(|i| i.air_date.clone()).unwrap_or_default(),
                            plot: None,
                            crew: e.info.as_ref().map(|i| i.crew.clone()),
                            duration_secs: e.info.as_ref().map(|i| i.duration_secs).unwrap_or_default(),
                            duration: e.info.as_ref().map(|i| i.duration.clone()).unwrap_or_default(),
                            movie_image: e.info.as_ref().map(|i| i.movie_image.clone()).unwrap_or_default(),
                            bitrate: e.info.as_ref().map(|i| i.bitrate).unwrap_or_default(),
                            rating: e.info.as_ref().map(|i| i.rating),
                            video: e.info.as_ref().map(|i| i.video.clone()).unwrap_or_default(),
                            audio: e.info.as_ref().map(|i| i.audio.clone()).unwrap_or_default(),
                        }
                    }).collect();
                    episodes.sort_by_key(|episode| (episode.season, episode.episode_num));
                    episodes
                }),
            }),
        }
    }

    pub fn from_info_doc(info: &XtreamSeriesInfoDoc, series_id: u32) -> SeriesStreamProperties {
        let tmdb = info.info.tmdb.parse::<u32>().ok();
        SeriesStreamProperties {
            name: info.info.name.clone(),
            category_id: info.info.category_id.parse::<u32>().unwrap_or_else(|_| {
                warn!("Failed to parse category_id {}", &info.info.category_id);
                0
            }),
            series_id,
            backdrop_path: Some(info.info.backdrop_path.clone()),
            cast: info.info.cast.clone(),
            cover: info.info.cover.clone(),
            director: info.info.director.clone(),
            episode_run_time: Some(info.info.episode_run_time.clone()),
            genre: Some(info.info.genre.clone()),
            last_modified: Some(info.info.last_modified.clone()),
            plot: Some(info.info.plot.clone()),
            rating: info.info.rating.parse::<f64>().unwrap_or_else(|_| {
                warn!("Failed to parse rating {}", &info.info.rating);
                0.0
            }),
            rating_5based: info.info.rating_5based.parse::<f64>().unwrap_or_else(|_| {
                warn!("Failed to parse rating_5based {}", &info.info.rating_5based);
                0.0
            }),
            release_date: Some(info.info.release_date.clone()),
            youtube_trailer: info.info.youtube_trailer.clone(),
            tmdb,
            details: Some(SeriesStreamDetailProperties {
                year: InfoDocUtils::extract_year_from_release_date(&info.info.release_date),
                seasons: {
                    let mut seasons: Vec<SeriesStreamDetailSeasonProperties> = info.seasons.iter().map(|s|
                        SeriesStreamDetailSeasonProperties {
                            name: s.name.clone(),
                            season_number: s.season_number,
                            episode_count: s.episode_count.parse::<u32>().unwrap_or_else(|_| {
                                warn!("Failed to parse episode_count {}", &s.episode_count);
                                0
                            }),
                            overview: s.overview.clone(),
                            air_date: s.air_date.clone(),
                            cover: s.cover.clone(),
                            cover_tmdb: s.cover_tmdb.clone(),
                            cover_big: s.cover_big.clone(),
                            duration: s.duration.clone(),
                        }).collect();
                    seasons.sort_by_key(|season| season.season_number);
                    Some(seasons)
                },
                episodes: {
                    let mut episodes: Vec<SeriesStreamDetailEpisodeProperties> = info.episodes.iter().flat_map(|(_, list)| list.iter()).map(|e|
                        SeriesStreamDetailEpisodeProperties {
                            id: e.id.parse::<u32>().unwrap_or_else(|_| {
                                warn!("Failed to parse episode id {}", &e.id);
                                0
                            }),
                            episode_num: e.episode_num,
                            season: e.season,
                            title: e.title.clone(),
                            container_extension: e.container_extension.clone(),
                            custom_sid: e.custom_sid.clone(),
                            added: e.added.clone(),
                            direct_source: e.direct_source.clone(),
                            tmdb,
                            release_date: e.info.air_date.clone(),
                            plot: None,
                            crew: e.info.crew.clone(),
                            duration_secs: e.info.duration_secs,
                            duration: e.info.duration.clone(),
                            movie_image: e.info.movie_image.clone(),
                            bitrate: e.info.bitrate,
                            rating: Some(e.info.rating),
                            video: None,
                            audio: None,
                        }).collect();
                    episodes.sort_by_key(|episode| (episode.season, episode.episode_num));
                    Some(episodes)
                },
            }),
        }
    }
}

impl EpisodeStreamProperties {
    pub fn from_series(series: &SeriesStreamProperties, episode: &SeriesStreamDetailEpisodeProperties) -> EpisodeStreamProperties {
        EpisodeStreamProperties {
            episode_id: episode.id,
            episode: episode.episode_num,
            season: episode.season,
            added: Some(episode.added.clone()),
            release_date: Some(episode.release_date.clone()),
            tmdb: episode.tmdb.or(series.tmdb),
            movie_image: episode.movie_image.clone(),
            container_extension: episode.container_extension.clone(),
            video: episode.video.clone(),
            audio: episode.audio.clone(),
        }
    }
}

fn non_empty_arc(s: &Arc<str>) -> Option<Arc<str>> {
    if s.is_empty() { None } else { Some(Arc::clone(s)) }
}