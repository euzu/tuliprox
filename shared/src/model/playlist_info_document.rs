use crate::utils::{arc_str_serde, arc_str_option_serde, arc_str_vec_serde, Internable};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use serde_json::Value;
use crate::concat_string;
use crate::model::{PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailSeasonProperties,
                   SeriesStreamProperties, StreamProperties, VideoStreamProperties, VirtualId, XtreamCluster, XtreamMappingOptions};
use crate::model::info_doc_utils::InfoDocUtils;

#[inline]
fn build_season_episode_field(season: u32, episode: u32, field: &str) -> String {
    concat_string!("nfo_ep_", &season.to_string(), "_", &episode.to_string(), "_", field)
}

#[inline]
fn build_season_field(season: u32, field: &str) -> String {
    concat_string!("nfo_s_", &season.to_string(), "_", field)
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum XtreamInfoDocument {
    Video(XtreamVideoInfoDoc),
    Series(XtreamSeriesInfoDoc),
    Empty(XtreamEmptyDoc),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct XtreamEmptyDoc {}

impl Default for XtreamInfoDocument {
    fn default() -> Self {
        Self::Empty(XtreamEmptyDoc {})
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoInfoDoc {
    pub info: XtreamVideoInfoData,
    pub movie_data: XtreamVideoMovieData,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoInfoData {
    #[serde(with = "arc_str_serde")]
    pub kinopoisk_url: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub tmdb_id: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub o_name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub cover_big: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub movie_image: Arc<str>,
    #[serde(rename = "releasedate", with = "arc_str_serde")]
    pub release_date: Arc<str>,
    pub episode_run_time: u32,
    #[serde(with = "arc_str_serde")]
    pub youtube_trailer: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub director: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub actors: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub cast: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub description: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub plot: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub age: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub mpaa_rating: Arc<str>,
    pub rating_count_kinopoisk: u32,
    #[serde(with = "arc_str_serde")]
    pub country: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub genre: Arc<str>,
    #[serde(with = "arc_str_vec_serde")]
    pub backdrop_path: Vec<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub duration_secs: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub duration: Arc<str>,
    pub video: Value,
    pub audio: Value,
    pub bitrate: u32,
    #[serde(with = "arc_str_serde")]
    pub rating: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub runtime: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub status: Arc<str>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoMovieData {
    pub stream_id: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub added: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub category_id: Arc<str>,
    pub category_ids: Vec<u32>,
    #[serde(with = "arc_str_serde")]
    pub container_extension: Arc<str>,
    #[serde(default, serialize_with = "arc_str_option_serde::serialize_null_if_empty", deserialize_with = "arc_str_option_serde::deserialize")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub direct_source: Arc<str>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesInfoDoc {
    #[serde(default)]
    pub seasons: Vec<XtreamSeriesSeasonInfoDoc>,
    pub info: XtreamSeriesInfoData,
    pub episodes: HashMap<String, Vec<XtreamSeriesEpisodeInfoDoc>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesSeasonInfoDoc {
    #[serde(default, with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub episode_count: Arc<str>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub overview: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub air_date: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub cover: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub cover_tmdb: Option<Arc<str>>,
    #[serde(default)]
    pub season_number: u32,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub cover_big: Option<Arc<str>>,
    #[serde(default, rename = "releaseDate", with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub release_date: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub duration: Option<Arc<str>>,
}


#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesInfoData {
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub cover: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub plot: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub cast: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub director: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub genre: Arc<str>,
    #[serde(rename = "releaseDate", with = "arc_str_serde")]
    pub release_date_alternate: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub release_date: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub last_modified: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rating: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rating_5based: Arc<str>,
    #[serde(with = "arc_str_vec_serde")]
    pub backdrop_path: Vec<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub tmdb: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub youtube_trailer: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub episode_run_time: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub category_id: Arc<str>,
    pub category_ids: Vec<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesEpisodeInfoDoc {
    #[serde(with = "arc_str_serde")]
    pub id: Arc<str>,
    pub episode_num: u32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub container_extension: Arc<str>,
    pub info: XtreamSeriesEpisodeInfoData,
    #[serde(default, serialize_with = "arc_str_option_serde::serialize_null_if_empty", deserialize_with = "arc_str_option_serde::deserialize")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub added: Arc<str>,
    pub season: u32,
    #[serde(with = "arc_str_serde")]
    pub direct_source: Arc<str>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesEpisodeInfoData {
    #[serde(with = "arc_str_serde")]
    pub air_date: Arc<str>,
    #[serde(default, with = "arc_str_option_serde", skip_serializing_if = "Option::is_none")]
    pub crew: Option<Arc<str>>,
    pub rating: f64,
    #[serde(rename = "id")]
    pub tmdb_id: u32,
    #[serde(with = "arc_str_serde")]
    pub movie_image: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub duration: Arc<str>,
    pub duration_secs: u32,
    pub video: Value,
    pub audio: Value,
    pub bitrate: u32,
}

impl StreamProperties {
    pub fn to_info_document(&self, options: &XtreamMappingOptions, item_type: PlaylistItemType,
                            virtual_id: VirtualId, category_id: u32) -> XtreamInfoDocument {
        match self {
            StreamProperties::Live(_live) => {
                // Live streams don't expose info documents through the Xtream API.
                XtreamInfoDocument::Empty(XtreamEmptyDoc {})
            }
            StreamProperties::Video(video) => {
                XtreamInfoDocument::Video(self.video_to_info_document(options, video, item_type, virtual_id, category_id))
            }
            StreamProperties::Series(series) => {
                XtreamInfoDocument::Series(self.series_to_info_document(options, series, item_type, virtual_id, category_id))
            }
            StreamProperties::Episode(_episode) => {
                // Episode streams don't expose info documents through the Xtream API.
                XtreamInfoDocument::Empty(XtreamEmptyDoc {})
            }
        }
    }

    fn series_to_info_document(&self, options: &XtreamMappingOptions, series: &SeriesStreamProperties,
                               item_type: PlaylistItemType, virtual_id: VirtualId, category_id: u32) -> XtreamSeriesInfoDoc {
        let resource_url = options.get_resource_url(XtreamCluster::Series, item_type, virtual_id);
        XtreamSeriesInfoDoc {
            seasons: if let Some(seasons) = series.details.as_ref().and_then(|d| d.seasons.as_ref()) {
                self.series_seasons_to_info_document(resource_url.as_deref(), seasons)
            } else {
                Vec::new()
            },
            info: XtreamSeriesInfoData {
                name: Arc::clone(&series.name),
                cover: InfoDocUtils::make_resource_url(resource_url.as_deref(), series.cover.as_ref(), "cover").intern(),
                plot: series.plot.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                cast: Arc::clone(&series.cast),
                director: Arc::clone(&series.director),
                genre: series.genre.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                release_date: series.release_date.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                release_date_alternate: series.release_date.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                last_modified: series.last_modified.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                rating: InfoDocUtils::limited(series.rating).intern(),
                rating_5based: InfoDocUtils::limited(series.rating_5based).intern(),
                backdrop_path: series.backdrop_path.as_ref().map_or_else(Vec::new, |b| b.iter().enumerate().map(|(idx, p)|
                    InfoDocUtils::make_bdpath_resource_url(resource_url.as_deref(), p, idx, "").intern()
                ).collect()),
                tmdb: series.tmdb.unwrap_or_default().to_string().intern(),
                youtube_trailer: Arc::clone(&series.youtube_trailer),
                episode_run_time: series.episode_run_time.as_ref().map(Arc::clone).unwrap_or_else(|| "".intern()),
                category_id: category_id.to_string().intern(),
                category_ids: vec![category_id],
            },
            episodes: if let Some(episodes) = series.details.as_ref().and_then(|d| d.episodes.as_ref()) {
                self.series_episodes_to_info_document(options, resource_url.as_deref(), episodes)
            } else {
                HashMap::new()
            }
        }
    }

    fn video_to_info_document(&self, options: &XtreamMappingOptions, video: &VideoStreamProperties,
                              item_type: PlaylistItemType, virtual_id: VirtualId, category_id: u32) -> XtreamVideoInfoDoc {
        let resource_url = options.get_resource_url(XtreamCluster::Video, item_type, virtual_id);
        let stream_icon = InfoDocUtils::make_resource_url(resource_url.as_deref(), &self.get_stream_icon(), "logo").intern();
        let empty_str = "".intern();

        let info = if let Some(details) = video.details.as_ref() {
            XtreamVideoInfoData {
                kinopoisk_url: details.kinopoisk_url.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                tmdb_id: video.tmdb.unwrap_or_default().to_string().intern(),
                name: Arc::clone(&video.name),
                o_name: details.o_name.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                cover_big: InfoDocUtils::make_resource_url(resource_url.as_deref(), details.cover_big.as_ref().map(Arc::as_ref).unwrap_or(""), "nfo_cover_big").intern(),
                movie_image: InfoDocUtils::make_resource_url(resource_url.as_deref(), details.cover_big.as_ref().map(Arc::as_ref).unwrap_or(""), "nfo_movie_image").intern(),
                release_date: details.release_date.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                episode_run_time: details.episode_run_time.unwrap_or_default(),
                youtube_trailer: details.youtube_trailer.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                director: details.director.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                actors: details.actors.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                cast: details.cast.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                description: details.description.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                plot: details.plot.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                age: details.age.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                mpaa_rating: details.mpaa_rating.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                rating_count_kinopoisk: details.rating_count_kinopoisk,
                country: details.country.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                genre: details.genre.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                backdrop_path: details.backdrop_path.as_deref().map_or_else(Vec::new, |b| b.iter().enumerate().map(|(idx, p)|
                    InfoDocUtils::make_bdpath_resource_url(resource_url.as_deref(), p, idx, "nfo_").intern()
                ).collect()),
                duration_secs: details.duration_secs.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                duration: details.duration.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                video: InfoDocUtils::build_value(details.video.as_ref().map(Arc::as_ref)),
                audio: InfoDocUtils::build_value(details.audio.as_ref().map(Arc::as_ref)),
                bitrate: details.bitrate,
                rating: InfoDocUtils::limited(video.rating.unwrap_or_default()).intern(),
                runtime: details.runtime.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
                status: details.status.as_ref().map(Arc::clone).unwrap_or_else(|| Arc::clone(&empty_str)),
            }
        } else {
            XtreamVideoInfoData {
                kinopoisk_url: Arc::clone(&empty_str),
                tmdb_id: video.tmdb.unwrap_or_default().to_string().intern(),
                name: Arc::clone(&video.name),
                o_name: Arc::clone(&video.name),
                cover_big: stream_icon.clone(),
                movie_image: stream_icon.clone(),
                release_date: Arc::clone(&empty_str),
                episode_run_time: 0,
                youtube_trailer: Arc::clone(&empty_str),
                director: Arc::clone(&empty_str),
                actors: Arc::clone(&empty_str),
                cast: Arc::clone(&empty_str),
                description: Arc::clone(&empty_str),
                plot: Arc::clone(&empty_str),
                age: Arc::clone(&empty_str),
                mpaa_rating: Arc::clone(&empty_str),
                rating_count_kinopoisk: 0,
                country: Arc::clone(&empty_str),
                genre: Arc::clone(&empty_str),
                backdrop_path: vec![Arc::clone(&stream_icon)],
                duration_secs: "0".intern(),
                duration: "0".intern(),
                video: Value::Array(Vec::new()),
                audio: Value::Array(Vec::new()),
                bitrate: 0,
                rating: InfoDocUtils::limited(video.rating.unwrap_or_default()).intern(),
                runtime: "0".intern(),
                status: "Released".intern(),
            }
        };

        XtreamVideoInfoDoc {
            info,
            movie_data: XtreamVideoMovieData {
                stream_id: virtual_id,
                name: Arc::clone(&video.name),
                added: Arc::clone(&video.added),
                category_id: category_id.to_string().intern(),
                category_ids: vec![category_id],
                container_extension: Arc::clone(&video.container_extension),
                custom_sid: video.custom_sid.as_ref().map(Arc::clone),
                direct_source: if options.skip_video_direct_source { Arc::clone(&empty_str) } else { Arc::clone(&video.direct_source) },
            }
        }
    }

    fn series_seasons_to_info_document(&self,
                                        resource_url: Option<&str>,
                                        seasons: &[SeriesStreamDetailSeasonProperties]) -> Vec<XtreamSeriesSeasonInfoDoc> {
        seasons.iter().map(|season|
            XtreamSeriesSeasonInfoDoc {
                name: Arc::clone(&season.name),
                season_number: season.season_number,
                episode_count: season.episode_count.intern(),
                overview: season.overview.as_ref().map(|v| if v.starts_with("http") {
                    InfoDocUtils::make_resource_url(resource_url, v, &build_season_field(season.season_number, "overview")).intern()
                } else {
                    Arc::clone(v)
                }),
                air_date: season.air_date.as_ref().map(Arc::clone),
                cover: season.cover.as_ref().map(|v| InfoDocUtils::make_resource_url(resource_url, v, &build_season_field(season.season_number, "cover")).intern()),
                cover_tmdb: season.cover_tmdb.as_ref().map(|v| InfoDocUtils::make_resource_url(resource_url, v, &build_season_field(season.season_number, "cover_tmdb")).intern()),
                cover_big: season.cover_big.as_ref().map(|v| InfoDocUtils::make_resource_url(resource_url, v, &build_season_field(season.season_number, "cover_big")).intern()),
                release_date: season.air_date.as_ref().map(Arc::clone),
                duration: season.duration.as_ref().map(Arc::clone),
            }
            ).collect()
    }

    fn series_episodes_to_info_document(&self, options: &XtreamMappingOptions,
                                        resource_url: Option<&str>,
                                        episodes: &[SeriesStreamDetailEpisodeProperties]) -> HashMap<String, Vec<XtreamSeriesEpisodeInfoDoc>> {
        let empty_str = "".intern();
        let mut map: HashMap<u32, Vec<XtreamSeriesEpisodeInfoDoc>> = HashMap::new();
        for ep in episodes {
            let doc = XtreamSeriesEpisodeInfoDoc {
                id: ep.id.intern(),
                episode_num: ep.episode_num,
                title: Arc::clone(&ep.title),
                container_extension: Arc::clone(&ep.container_extension),
                info: XtreamSeriesEpisodeInfoData {
                    tmdb_id: ep.tmdb.unwrap_or_default(),
                    air_date: Arc::clone(&ep.release_date),
                    crew: ep.crew.as_ref().map(Arc::clone),
                    rating: ep.rating.unwrap_or_default(),
                    movie_image: InfoDocUtils::make_resource_url(resource_url, &ep.movie_image, &build_season_episode_field(ep.season, ep.episode_num, "movie_image")).intern(),
                    duration: Arc::clone(&ep.duration),
                    duration_secs: ep.duration_secs,
                    video: InfoDocUtils::build_value(ep.video.as_ref().map(Arc::as_ref)),
                    audio: InfoDocUtils::build_value(ep.audio.as_ref().map(Arc::as_ref)),
                    bitrate: ep.bitrate,
                },
                custom_sid: ep.custom_sid.as_ref().map(Arc::clone),
                added: Arc::clone(&ep.added),
                season: ep.season,
                direct_source: if options.skip_series_direct_source { Arc::clone(&empty_str) } else { Arc::clone(&ep.direct_source) },
            };
            map.entry(ep.season).or_default().push(doc);
        }

        map.into_iter()
            .map(|(season, episodes)| (season.to_string(), episodes))
            .collect()
    }
}


impl Default for XtreamVideoInfoDoc {
    fn default() -> Self {
        Self {
            info: XtreamVideoInfoData {
                kinopoisk_url: "".intern(),
                tmdb_id: "".intern(),
                name: "".intern(),
                o_name: "".intern(),
                cover_big: "".intern(),
                movie_image: "".intern(),
                release_date: "".intern(),
                episode_run_time: 0,
                youtube_trailer: "".intern(),
                director: "".intern(),
                actors: "".intern(),
                cast: "".intern(),
                description: "".intern(),
                plot: "".intern(),
                age: "".intern(),
                mpaa_rating: "".intern(),
                rating_count_kinopoisk: 0,
                country: "".intern(),
                genre: "".intern(),
                backdrop_path: vec![],
                duration_secs: "".intern(),
                duration: "".intern(),
                video: Value::Array(Vec::new()),
                audio: Value::Array(Vec::new()),
                bitrate: 0,
                rating: "".intern(),
                runtime: "".intern(),
                status: "".intern(),
            },
            movie_data: XtreamVideoMovieData {
                stream_id: 0,
                name: "".intern(),
                added: "".intern(),
                category_id: "".intern(),
                category_ids: vec![],
                container_extension: "".intern(),
                custom_sid: None,
                direct_source: "".intern(),
            },
        }
    }
}