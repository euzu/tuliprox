use crate::{
    model::{info_doc_utils::InfoDocUtils, PlaylistEntry, XtreamSeriesInfo, XtreamSeriesInfoDoc},
    utils::{
        arc_str_none_default_on_null, arc_str_null_is_none_serde, arc_str_option_null_if_empty_serde,
        arc_str_option_serde, deserialize_as_option_arc_str, deserialize_as_string_array,
        deserialize_json_as_opt_string, deserialize_number_from_string, deserialize_number_from_string_or_zero,
        serialize_json_as_opt_string, Internable, CONSTANTS,
    },
};
use log::warn;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn format_episode_code(season: u32, episode: u32) -> Option<String> {
    match (season, episode) {
        (0, 0) => None,
        (season, 0) => Some(format!("S{season:02}")),
        (0, episode) => Some(format!("E{episode:02}")),
        (season, episode) => Some(format!("S{season:02}E{episode:02}")),
    }
}

fn title_contains_episode_code(title: &str) -> bool { CONSTANTS.re_episode_code.is_match(title) }

pub(super) fn first_series_episode_value<T, F>(series: &SeriesStreamProperties, selector: F) -> Option<T>
where
    F: FnOnce(&SeriesStreamDetailEpisodeProperties) -> Option<T>,
{
    series
        .details
        .as_ref()
        .and_then(|details| details.episodes.as_ref())
        .and_then(|episodes| episodes.first())
        .and_then(selector)
}

pub fn normalize_episode_title(raw_title: &Arc<str>, series_name: &Arc<str>, season: u32, episode: u32) -> Arc<str> {
    let title = raw_title.trim();
    let series_name = series_name.trim();
    let Some(code) = format_episode_code(season, episode) else {
        return if title.is_empty() { series_name.intern() } else { title.intern() };
    };

    if title.is_empty() || (!series_name.is_empty() && title.eq_ignore_ascii_case(series_name)) {
        return code.into();
    }

    if title_contains_episode_code(title) {
        return title.intern();
    }

    format!("{code} - {title}").into()
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct SeriesStreamDetailEpisodeProperties {
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_num: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub title: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_null_is_none_serde::deserialize")]
    pub container_extension: Arc<str>,
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
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
    pub series_release_date: Option<Arc<str>>, // Global series release date
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

impl SeriesStreamDetailProperties {
    /// Builds a `SeriesStreamDetailProperties`. Empty `seasons` is normalized to
    /// `None` so downstream code can short-circuit; `episodes` is kept as-is.
    pub fn new(
        year: Option<u32>,
        seasons: Vec<SeriesStreamDetailSeasonProperties>,
        episodes: Option<Vec<SeriesStreamDetailEpisodeProperties>>,
    ) -> Self {
        Self { year, seasons: if seasons.is_empty() { None } else { Some(seasons) }, episodes }
    }
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
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
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub series_release_date: Option<Arc<str>>, // Global series release date
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub plot: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub movie_image: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_null_is_none_serde::deserialize")]
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

pub(super) fn parse_season_field(s: &str) -> Option<(u32, String)> {
    let mut parts = s.split('_');

    let (prefix, suffix) = (parts.next()?, parts.next()?);
    if prefix != "nfo" || suffix != "s" {
        return None;
    }

    let season: u32 = parts.next()?.parse().ok()?;

    let kind = join_parts(parts, '_');
    if kind.is_empty() {
        return None;
    }

    Some((season, kind))
}

pub(super) fn parse_season_episode_field(s: &str) -> Option<(u32, u32, String)> {
    let mut parts = s.split('_');

    let (prefix, ep) = (parts.next()?, parts.next()?);
    if prefix != "nfo" || ep != "ep" {
        return None;
    }

    let season: u32 = parts.next()?.parse().ok()?;
    let episode: u32 = parts.next()?.parse().ok()?;

    let kind = join_parts(parts, '_');
    if kind.is_empty() {
        return None;
    }

    Some((season, episode, kind))
}

fn join_parts<'a>(parts: impl Iterator<Item = &'a str>, separator: char) -> String {
    let mut result = String::new();
    for part in parts {
        if !result.is_empty() {
            result.push(separator);
        }
        result.push_str(part);
    }
    result
}

impl SeriesStreamProperties {
    pub(super) fn from_info_base(info: &XtreamSeriesInfo, series_id: u32) -> SeriesStreamProperties {
        SeriesStreamProperties {
            name: info.info.name.clone(),
            category_id: info.info.category_id,
            series_id,
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
                    let mut seasons: Vec<SeriesStreamDetailSeasonProperties> = list
                        .iter()
                        .map(|s| SeriesStreamDetailSeasonProperties {
                            name: s.name.clone(),
                            season_number: s.season_number,
                            episode_count: s.episode_count,
                            overview: Some(s.overview.clone()),
                            air_date: Some(s.air_date.clone()),
                            cover: Some(s.cover.clone()),
                            cover_tmdb: Some(s.cover_tmdb.clone()),
                            cover_big: Some(s.cover_big.clone()),
                            duration: Some(s.duration.clone()),
                        })
                        .collect();
                    seasons.sort_by_key(|season| season.season_number);
                    seasons
                }),
                episodes: info.episodes.as_ref().map(|list| {
                    let mut episodes: Vec<SeriesStreamDetailEpisodeProperties> = list
                        .iter()
                        .map(|e| SeriesStreamDetailEpisodeProperties {
                            id: e.id,
                            episode_num: e.episode_num,
                            season: e.season,
                            title: normalize_episode_title(&e.title, &info.info.name, e.season, e.episode_num),
                            container_extension: e.container_extension.clone(),
                            custom_sid: e.custom_sid.clone(),
                            added: e.added.clone(),
                            direct_source: e.direct_source.clone(),
                            tmdb: info.info.tmdb,
                            release_date: e.info.as_ref().map(|i| i.air_date.clone()).unwrap_or_default(),
                            series_release_date: None,
                            plot: e.info.as_ref().and_then(|i| i.plot.clone()),
                            crew: e.info.as_ref().map(|i| i.crew.clone()),
                            duration_secs: e.info.as_ref().map(|i| i.duration_secs).unwrap_or_default(),
                            duration: e.info.as_ref().map(|i| i.duration.clone()).unwrap_or_default(),
                            movie_image: e.info.as_ref().map(|i| i.movie_image.clone()).unwrap_or_default(),
                            bitrate: e.info.as_ref().map(|i| i.bitrate).unwrap_or_default(),
                            rating: e.info.as_ref().map(|i| i.rating),
                            video: e.info.as_ref().map(|i| i.video.clone()).unwrap_or_default(),
                            audio: e.info.as_ref().map(|i| i.audio.clone()).unwrap_or_default(),
                        })
                        .collect();
                    episodes.sort_by_key(|episode| (episode.season, episode.episode_num));
                    episodes
                }),
            }),
        }
    }

    pub fn from_info<P>(info: &XtreamSeriesInfo, pli: &P) -> SeriesStreamProperties
    where
        P: PlaylistEntry,
    {
        Self::from_info_base(info, pli.get_virtual_id().get())
    }

    pub fn from_info_without_existing(info: &XtreamSeriesInfo, series_id: u32) -> SeriesStreamProperties {
        Self::from_info_base(info, series_id)
    }

    pub fn from_info_doc(info: &XtreamSeriesInfoDoc, series_id: u32) -> SeriesStreamProperties {
        let tmdb = info.info.tmdb.parse::<u32>().ok();
        SeriesStreamProperties {
            name: info.info.name.clone(),
            category_id: info.info.category_id.parse::<u32>().unwrap_or_else(|_| {
                warn!("Failed to parse category_id {}", info.info.category_id);
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
                warn!("Failed to parse rating {}", info.info.rating);
                0.0
            }),
            rating_5based: info.info.rating_5based.parse::<f64>().unwrap_or_else(|_| {
                warn!("Failed to parse rating_5based {}", info.info.rating_5based);
                0.0
            }),
            release_date: Some(info.info.release_date.clone()),
            youtube_trailer: info.info.youtube_trailer.clone(),
            tmdb,
            details: Some(SeriesStreamDetailProperties {
                year: InfoDocUtils::extract_year_from_release_date(&info.info.release_date),
                seasons: {
                    let mut seasons: Vec<SeriesStreamDetailSeasonProperties> = info
                        .seasons
                        .iter()
                        .map(|s| SeriesStreamDetailSeasonProperties {
                            name: s.name.clone(),
                            season_number: s.season_number,
                            episode_count: s.episode_count.parse::<u32>().unwrap_or_else(|_| {
                                warn!("Failed to parse episode_count {}", s.episode_count);
                                0
                            }),
                            overview: s.overview.clone(),
                            air_date: s.air_date.clone(),
                            cover: s.cover.clone(),
                            cover_tmdb: s.cover_tmdb.clone(),
                            cover_big: s.cover_big.clone(),
                            duration: s.duration.clone(),
                        })
                        .collect();
                    seasons.sort_by_key(|season| season.season_number);
                    Some(seasons)
                },
                episodes: {
                    let mut episodes: Vec<SeriesStreamDetailEpisodeProperties> = info
                        .episodes
                        .iter()
                        .flat_map(|(_, list)| list.iter())
                        .map(|e| SeriesStreamDetailEpisodeProperties {
                            id: e.id.parse::<u32>().unwrap_or_else(|_| {
                                warn!("Failed to parse episode id {}", e.id);
                                0
                            }),
                            episode_num: e.episode_num,
                            season: e.season,
                            title: normalize_episode_title(&e.title, &info.info.name, e.season, e.episode_num),
                            container_extension: e.container_extension.clone(),
                            custom_sid: e.custom_sid.clone(),
                            added: e.added.clone(),
                            direct_source: e.direct_source.clone(),
                            tmdb,
                            release_date: e.info.air_date.clone(),
                            series_release_date: None,
                            plot: e.info.plot.clone(),
                            crew: e.info.crew.clone(),
                            duration_secs: e.info.duration_secs,
                            duration: e.info.duration.clone(),
                            movie_image: e.info.movie_image.clone(),
                            bitrate: e.info.bitrate,
                            rating: Some(e.info.rating),
                            video: None,
                            audio: None,
                        })
                        .collect();
                    episodes.sort_by_key(|episode| (episode.season, episode.episode_num));
                    Some(episodes)
                },
            }),
        }
    }
}

impl EpisodeStreamProperties {
    pub fn from_series(
        series: &SeriesStreamProperties,
        episode: &SeriesStreamDetailEpisodeProperties,
    ) -> EpisodeStreamProperties {
        EpisodeStreamProperties {
            episode_id: episode.id,
            episode: episode.episode_num,
            season: episode.season,
            added: Some(episode.added.clone()),
            release_date: Some(episode.release_date.clone()),
            // Inherit global release date from the Series object if available
            series_release_date: series.release_date.clone(),
            plot: episode.plot.clone(),
            tmdb: episode.tmdb.or(series.tmdb),
            movie_image: episode.movie_image.clone(),
            container_extension: episode.container_extension.clone(),
            video: episode.video.clone(),
            audio: episode.audio.clone(),
        }
    }
}

pub(super) fn non_empty_arc(s: &Arc<str>) -> Option<Arc<str>> {
    if s.is_empty() {
        None
    } else {
        Some(Arc::clone(s))
    }
}
