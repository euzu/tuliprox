use crate::utils::{arc_str_serde, arc_str_option_serde, arc_str_vec_serde, Internable};
use crate::model::info_doc_utils::InfoDocUtils;
use crate::model::{
    LiveStreamProperties, SeriesStreamProperties, StreamProperties, VideoStreamProperties,
    XtreamCluster, XtreamEmptyDoc, XtreamInfoDocument, XtreamMappingOptions, XtreamPlaylistItem,
    XtreamSeriesInfoData, XtreamSeriesInfoDoc, XtreamVideoInfoData,
    XtreamVideoInfoDoc, XtreamVideoMovieData,
};
use std::sync::Arc;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_as_live() -> Arc<str> { "live".intern() }

fn default_as_movie() -> Arc<str> {
    "movie".intern()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamLiveDoc {
    pub num: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub stream_type: Arc<str>,
    pub stream_id: u32,
    #[serde(with = "arc_str_serde")]
    pub stream_icon: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub epg_channel_id: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub added: Arc<str>,
    pub is_adult: i32,
    #[serde(with = "arc_str_serde")]
    pub category_id: Arc<str>,
    pub category_ids: Vec<u32>,
    #[serde(default, with = "arc_str_option_serde")]
    pub custom_sid: Option<Arc<str>>,
    pub tv_archive: i32,
    #[serde(with = "arc_str_serde")]
    pub direct_source: Arc<str>,
    pub tv_archive_duration: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoDoc {
    pub num: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub stream_type: Arc<str>,
    pub stream_id: u32,
    #[serde(with = "arc_str_serde")]
    pub stream_icon: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rating: Arc<str>,
    pub rating_5based: f64,
    #[serde(with = "arc_str_serde")]
    pub tmdb: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub trailer: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub added: Arc<str>,
    pub is_adult: i32,
    #[serde(with = "arc_str_serde")]
    pub category_id: Arc<str>,
    pub category_ids: Vec<u32>,
    #[serde(with = "arc_str_serde")]
    pub container_extension: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub direct_source: Arc<str>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamSeriesDoc {
    pub num: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub series_id: u32,
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
    pub youtube_trailer: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub tmdb: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub episode_runtime: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub category_id: Arc<str>,
    pub category_ids: Vec<u32>,
}

impl XtreamPlaylistItem {
    pub fn to_info_document(&self, options: &XtreamMappingOptions) -> XtreamInfoDocument {
        if self.has_details() {
            if let Some(doc) = self.additional_properties.as_ref() {
                return doc.to_info_document(options, self.item_type, self.virtual_id, self.category_id);
            }
        }
        let resource_url = options.get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id);
        self.to_info_document_no_props(resource_url)
    }

    fn to_info_document_no_props(&self, resource_url: Option<String>) -> XtreamInfoDocument {
        let stream_icon = self.get_stream_icon(resource_url);
        let empty_str = "".intern();
        match self.xtream_cluster {
            XtreamCluster::Live => XtreamInfoDocument::Empty(XtreamEmptyDoc {}),
            XtreamCluster::Video => {
                XtreamInfoDocument::Video(XtreamVideoInfoDoc {
                    info: XtreamVideoInfoData {
                        kinopoisk_url: Arc::clone(&empty_str),
                        tmdb_id: Arc::clone(&empty_str),
                        name: Arc::clone(&self.title),
                        o_name: Arc::clone(&self.name),
                        cover_big: Arc::clone(&stream_icon),
                        movie_image: Arc::clone(&stream_icon),
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
                        duration: Arc::clone(&empty_str),
                        video: Value::Array(Vec::new()),
                        audio: Value::Array(Vec::new()),
                        bitrate: 0,
                        rating: Arc::clone(&empty_str),
                        runtime: Arc::clone(&empty_str),
                        status: "Released".intern(),
                    },
                    movie_data: XtreamVideoMovieData {
                        stream_id: self.virtual_id,
                        name: Arc::clone(&self.name),
                        added: Arc::clone(&empty_str),
                        category_id: self.category_id.intern(),
                        category_ids: vec![self.category_id],
                        container_extension: Arc::clone(&empty_str),
                        custom_sid: None,
                        direct_source: Arc::clone(&empty_str),
                    },
                })
            }
            XtreamCluster::Series => {
                XtreamInfoDocument::Series(XtreamSeriesInfoDoc {
                    seasons: Vec::new(),
                    info: XtreamSeriesInfoData {
                        name: Arc::clone(&self.title),
                        cover: Arc::clone(&stream_icon),
                        plot: Arc::clone(&empty_str),
                        cast: Arc::clone(&empty_str),
                        director: Arc::clone(&empty_str),
                        genre: Arc::clone(&empty_str),
                        release_date_alternate: Arc::clone(&empty_str),
                        release_date: Arc::clone(&empty_str),
                        last_modified: Arc::clone(&empty_str),
                        rating: Arc::clone(&empty_str),
                        rating_5based: Arc::clone(&empty_str),
                        backdrop_path: if stream_icon.is_empty() {vec![] } else { vec![Arc::clone(&stream_icon)] },
                        tmdb: Arc::clone(&empty_str),
                        youtube_trailer: Arc::clone(&empty_str),
                        episode_run_time: empty_str,
                        category_id: self.category_id.intern(),
                        category_ids: vec![self.category_id],
                    },
                    episodes: IndexMap::new(),
                })
            }
        }
    }

    pub fn to_document(&self, options: &XtreamMappingOptions) -> XtreamDocument {
        if let Some(props) = self.additional_properties.as_ref() {
            match props {
                StreamProperties::Live(live) => self.live_to_document(options, live),
                StreamProperties::Video(video) => self.video_to_document(options, video),
                StreamProperties::Series(series) => self.series_to_document(options, series),
                StreamProperties::Episode(_episode) => XtreamDocument::Episode(XtreamEmptyDoc::default()),
            }
        } else {
            let resource_url = options.get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id);
            self.to_document_no_props(resource_url)
        }
    }

    fn series_to_document(&self, options: &XtreamMappingOptions, series: &SeriesStreamProperties) -> XtreamDocument {
        let resource_url = options.get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id);
        let empty_str = "".intern();
        XtreamDocument::Series(XtreamSeriesDoc {
            num: self.channel_no,
            name: self.title.clone(),
            series_id: self.virtual_id,
            cover: InfoDocUtils::make_resource_url(resource_url.as_deref(), &series.cover, "cover").intern(),
            plot: series.plot.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            cast: series.cast.clone(),
            director: series.director.clone(),
            genre: series.genre.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            release_date: series.release_date.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            release_date_alternate: series.release_date.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            last_modified: series.last_modified.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            rating: InfoDocUtils::limited(series.rating).intern(),
            rating_5based: InfoDocUtils::limited(series.rating_5based).intern(),
            backdrop_path: series.backdrop_path.as_ref().map_or_else(
                || {
                    let res_url = InfoDocUtils::make_resource_url(resource_url.as_deref(), &series.cover, "cover");
                    if res_url.is_empty() { vec![] } else { vec![res_url.intern()] }
                },
                |b| b.iter().enumerate().map(|(idx, p)|
                    InfoDocUtils::make_bdpath_resource_url(resource_url.as_deref(), p, idx, "").intern()
                ).collect()),
            youtube_trailer: series.youtube_trailer.clone(),
            tmdb: series.tmdb.map(|v| v.intern()).unwrap_or_else(|| Arc::clone(&empty_str)),
            episode_runtime: series.episode_run_time.clone().unwrap_or(empty_str),
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
        })
    }

    fn video_to_document(&self, options: &XtreamMappingOptions, video: &VideoStreamProperties) -> XtreamDocument {
        let resource_url = options.get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id);
        let stream_icon = self.get_stream_icon(resource_url);
        let empty_str = "".intern();
        XtreamDocument::Video(XtreamVideoDoc {
            num: self.channel_no,
            name: self.title.clone(),
            stream_type: video.stream_type.clone().unwrap_or_else(default_as_movie),
            stream_id: self.virtual_id,
            stream_icon,
            rating: video.rating.map(|v| InfoDocUtils::limited(v).intern()).unwrap_or_else(|| Arc::clone(&empty_str)),
            rating_5based: video.rating_5based.unwrap_or_default(),
            tmdb: video.tmdb.map(|v| v.intern()).unwrap_or_else(|| Arc::clone(&empty_str)),
            trailer: video.trailer.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            added: video.added.clone(),
            is_adult: video.is_adult,
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
            container_extension: video.container_extension.clone(),
            custom_sid: video.custom_sid.clone(),
            direct_source: if options.skip_video_direct_source { empty_str } else { video.direct_source.clone() },
        })
    }

    fn live_to_document(&self, options: &XtreamMappingOptions, live: &LiveStreamProperties) -> XtreamDocument {
        let resource_url = options.get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id);
        let stream_icon = self.get_stream_icon(resource_url);
        let empty_str = "".intern();
        XtreamDocument::Live(XtreamLiveDoc {
            num: self.channel_no,
            name: self.title.clone(),
            stream_type: live.stream_type.clone().unwrap_or_else(default_as_live),
            stream_id: self.virtual_id,
            stream_icon,
            epg_channel_id: self.epg_channel_id.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            added: live.added.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            is_adult: live.is_adult,
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
            custom_sid: live.custom_sid.clone(),
            tv_archive: live.tv_archive.unwrap_or_default(),
            direct_source: if options.skip_live_direct_source { empty_str } else { live.direct_source.clone() },
            tv_archive_duration: live.tv_archive_duration.unwrap_or_default(),
        })
    }

    fn to_document_no_props(&self, resource_url: Option<String>) -> XtreamDocument {
        let empty_str = "".intern();
        let zero_str = "0".intern();
        let stream_icon = self.get_stream_icon(resource_url);
        match self.xtream_cluster {
            XtreamCluster::Live => {
                XtreamDocument::Live(XtreamLiveDoc {
                    num: self.channel_no,
                    name: self.title.clone(),
                    stream_type: default_as_live(),
                    stream_id: self.virtual_id,
                    stream_icon,
                    epg_channel_id: self.epg_channel_id.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
                    added: Arc::clone(&empty_str),
                    is_adult: 0,
                    category_id: self.category_id.intern(),
                    category_ids: vec![self.category_id],
                    custom_sid: None,
                    tv_archive: 0,
                    direct_source: Arc::clone(&empty_str),
                    tv_archive_duration: 0,
                })
            }
            XtreamCluster::Video => {
                XtreamDocument::Video(XtreamVideoDoc {
                    num: self.channel_no,
                    name: self.title.clone(),
                    stream_type: default_as_movie(),
                    stream_id: self.virtual_id,
                    stream_icon,
                    rating: Arc::clone(&zero_str),
                    rating_5based: 0.0,
                    tmdb: Arc::clone(&empty_str),
                    trailer: Arc::clone(&empty_str),
                    added: Arc::clone(&empty_str),
                    is_adult: 0,
                    category_id: self.category_id.intern(),
                    category_ids: vec![self.category_id],
                    container_extension: Arc::clone(&empty_str),
                    custom_sid: None,
                    direct_source: Arc::clone(&empty_str),
                })
            }
            XtreamCluster::Series => {
                XtreamDocument::Series(XtreamSeriesDoc {
                    num: self.channel_no,
                    name: self.title.clone(),
                    series_id: self.virtual_id,
                    cover: stream_icon.clone(),
                    plot: Arc::clone(&empty_str),
                    cast: Arc::clone(&empty_str),
                    director: Arc::clone(&empty_str),
                    genre: Arc::clone(&empty_str),
                    release_date: Arc::clone(&empty_str),
                    release_date_alternate: Arc::clone(&empty_str),
                    last_modified: Arc::clone(&empty_str),
                    rating: Arc::clone(&zero_str),
                    rating_5based: Arc::clone(&zero_str),
                    backdrop_path: if stream_icon.is_empty() { vec![] } else { vec![Arc::clone(&stream_icon)] },
                    youtube_trailer: Arc::clone(&empty_str),
                    tmdb: empty_str,
                    episode_runtime: zero_str,
                    category_id: self.category_id.intern(),
                    category_ids: vec![self.category_id],
                })
            }
        }
    }

    fn get_stream_icon(&self, resource_url: Option<String>) -> Arc<str> {
        if !self.logo.is_empty() {
            InfoDocUtils::make_resource_url(resource_url.as_deref(), &self.logo, "logo").intern()
        } else if !self.logo_small.is_empty() {
            InfoDocUtils::make_resource_url(resource_url.as_deref(), &self.logo_small, "logo_small").intern()
        } else {
            "".intern()
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum XtreamDocument {
    Live(XtreamLiveDoc),
    Video(XtreamVideoDoc),
    Series(XtreamSeriesDoc),
    Episode(XtreamEmptyDoc),
}

