use crate::{
    model::{
        info_doc_utils::InfoDocUtils, PlaylistItemType, StreamProperties, VideoStreamProperties, VirtualId,
        XtreamCluster, XtreamMappingFlags, XtreamMappingOptions,
    },
    utils::{
        arc_str_null_is_none_serde, arc_str_option_null_if_empty_serde, arc_str_serde, arc_str_vec_serde, Internable,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

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
    #[serde(with = "arc_str_null_is_none_serde")]
    pub container_extension: Arc<str>,
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub direct_source: Arc<str>,
}

impl Default for XtreamVideoInfoDoc {
    fn default() -> Self {
        let empty_str = "".intern();
        Self {
            info: XtreamVideoInfoData {
                kinopoisk_url: Arc::clone(&empty_str),
                tmdb_id: Arc::clone(&empty_str),
                name: Arc::clone(&empty_str),
                o_name: Arc::clone(&empty_str),
                cover_big: Arc::clone(&empty_str),
                movie_image: Arc::clone(&empty_str),
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
                backdrop_path: vec![],
                duration_secs: "0".intern(),
                duration: Arc::clone(&empty_str),
                video: Value::Array(Vec::new()),
                audio: Value::Array(Vec::new()),
                bitrate: 0,
                rating: Arc::clone(&empty_str),
                runtime: Arc::clone(&empty_str),
                status: Arc::clone(&empty_str),
            },
            movie_data: XtreamVideoMovieData {
                stream_id: 0,
                name: Arc::clone(&empty_str),
                added: Arc::clone(&empty_str),
                category_id: Arc::clone(&empty_str),
                category_ids: vec![],
                container_extension: Arc::clone(&empty_str),
                custom_sid: None,
                direct_source: empty_str,
            },
        }
    }
}

impl StreamProperties {
    pub(super) fn video_to_info_document(
        &self,
        options: &XtreamMappingOptions,
        video: &VideoStreamProperties,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        category_id: u32,
    ) -> XtreamVideoInfoDoc {
        let stream_icon = options
            .get_resource_url(XtreamCluster::Video, item_type, virtual_id, &self.get_stream_icon(), "logo")
            .intern();
        let empty_str = "".intern();
        let zero_str = "0".intern();

        let info = if let Some(details) = video.details.as_ref() {
            XtreamVideoInfoData {
                kinopoisk_url: details.kinopoisk_url.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                tmdb_id: video.tmdb.unwrap_or_default().to_string().intern(),
                name: Arc::clone(&video.name),
                o_name: details.o_name.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                cover_big: options
                    .get_resource_url(
                        XtreamCluster::Video,
                        item_type,
                        virtual_id,
                        details.cover_big.as_ref().map_or("", Arc::as_ref),
                        "nfo_cover_big",
                    )
                    .intern(),
                movie_image: options
                    .get_resource_url(
                        XtreamCluster::Video,
                        item_type,
                        virtual_id,
                        details.movie_image.as_ref().map_or("", Arc::as_ref),
                        "nfo_movie_image",
                    )
                    .intern(),
                release_date: details.release_date.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                episode_run_time: details.episode_run_time.unwrap_or_default(),
                youtube_trailer: details.youtube_trailer.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                director: details.director.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                actors: details.actors.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                cast: details.cast.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                description: details.description.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                plot: details.plot.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                age: details.age.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                mpaa_rating: details.mpaa_rating.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                rating_count_kinopoisk: details.rating_count_kinopoisk,
                country: details.country.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                genre: details.genre.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                backdrop_path: details.backdrop_path.as_deref().map_or_else(Vec::new, |b| {
                    b.iter()
                        .enumerate()
                        .map(|(idx, p)| {
                            options
                                .get_bd_path_resource_url(XtreamCluster::Video, item_type, virtual_id, p, "nfo_", idx)
                                .intern()
                        })
                        .collect()
                }),
                duration_secs: details.duration_secs.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                duration: details.duration.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                video: InfoDocUtils::build_value(details.video.as_ref().map(Arc::as_ref)),
                audio: InfoDocUtils::build_value(details.audio.as_ref().map(Arc::as_ref)),
                bitrate: details.bitrate,
                rating: InfoDocUtils::limited(video.rating.unwrap_or_default()).intern(),
                runtime: details.runtime.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
                status: details.status.as_ref().map_or_else(|| Arc::clone(&empty_str), Arc::clone),
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
                duration_secs: Arc::clone(&zero_str),
                duration: Arc::clone(&zero_str),
                video: Value::Array(Vec::new()),
                audio: Value::Array(Vec::new()),
                bitrate: 0,
                rating: InfoDocUtils::limited(video.rating.unwrap_or_default()).intern(),
                runtime: zero_str,
                status: "Released".intern(),
            }
        };

        XtreamVideoInfoDoc {
            info,
            movie_data: XtreamVideoMovieData {
                stream_id: virtual_id.get(),
                name: Arc::clone(&video.name),
                added: Arc::clone(&video.added),
                category_id: category_id.to_string().intern(),
                category_ids: vec![category_id],
                container_extension: Arc::clone(&video.container_extension),
                custom_sid: video.custom_sid.as_ref().map(Arc::clone),
                direct_source: if options.flags.contains(XtreamMappingFlags::SkipVideoDirectSource) {
                    Arc::clone(&empty_str)
                } else {
                    Arc::clone(&video.direct_source)
                },
            },
        }
    }
}
