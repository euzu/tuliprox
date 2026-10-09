use super::StreamProperties;
use crate::{
    model::{PlaylistEntry, XtreamVideoInfo},
    utils::{
        arc_str_none_default_on_null, arc_str_null_is_none_serde, arc_str_option_null_if_empty_serde,
        arc_str_option_serde, deserialize_as_option_arc_str, deserialize_as_string_array,
        deserialize_json_as_opt_string, deserialize_number_from_string, deserialize_number_from_string_or_zero,
        serialize_json_as_opt_string, Internable,
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
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

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
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
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub added: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_null_is_none_serde::deserialize")]
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

impl VideoStreamProperties {
    pub(super) fn from_info_base(info: &XtreamVideoInfo) -> VideoStreamProperties {
        VideoStreamProperties {
            name: info.info.name.clone(),
            category_id: info.movie_data.category_id,
            stream_id: info.movie_data.stream_id,
            stream_icon: info.info.movie_image.as_ref().map_or_else(|| "".intern(), Clone::clone),
            direct_source: info.movie_data.direct_source.clone(),
            custom_sid: info.movie_data.custom_sid.clone(),
            added: info.movie_data.added.clone(),
            container_extension: info.movie_data.container_extension.clone(),
            rating: None,        // from PlaylistItem
            rating_5based: None, // from PlaylistItem
            stream_type: Some("movie".intern()),
            trailer: info.info.youtube_trailer.clone(),
            tmdb: info.info.tmdb_id.parse::<u32>().ok(),
            is_adult: 0, // from PlaylistItem
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
        }
    }

    pub fn from_info<P>(info: &XtreamVideoInfo, pli: &P) -> VideoStreamProperties
    where
        P: PlaylistEntry,
    {
        let mut props = VideoStreamProperties::from_info_base(info);

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
        }

        props
    }

    pub fn from_info_without_existing(info: &XtreamVideoInfo) -> VideoStreamProperties {
        VideoStreamProperties::from_info_base(info)
    }
}
