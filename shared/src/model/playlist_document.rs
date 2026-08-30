use crate::{
    model::{
        info_doc_utils::InfoDocUtils, LiveStreamProperties, SeriesStreamProperties, StreamProperties,
        VideoStreamProperties, XtreamCluster, XtreamEmptyDoc, XtreamInfoDocument, XtreamMappingFlags,
        XtreamMappingOptions, XtreamPlaylistItem, XtreamSeriesInfoData, XtreamSeriesInfoDoc, XtreamVideoInfoData,
        XtreamVideoInfoDoc, XtreamVideoMovieData,
    },
    utils::{
        arc_str_null_is_none_serde, arc_str_option_null_if_empty_serde, arc_str_serde, arc_str_vec_serde,
        extract_extension_from_url, Internable,
    },
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

fn default_as_live() -> Arc<str> { "live".intern() }

fn default_as_movie() -> Arc<str> { "movie".intern() }

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
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
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
    #[serde(with = "arc_str_null_is_none_serde")]
    pub container_extension: Arc<str>,
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
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
                let mut document = doc.to_info_document(options, self.item_type, self.virtual_id, self.category_id);
                // `StreamProperties` has no URL to fall back to, so the blank is filled here.
                if let XtreamInfoDocument::Video(ref mut video) = document {
                    video.movie_data.container_extension =
                        self.container_extension_or_url_fallback(&video.movie_data.container_extension);
                }
                return document;
            }
        }
        self.to_info_document_no_props(options)
    }

    /// Fills a blank `container_extension` from the extension carried by the item URL.
    ///
    /// Providers sometimes omit `container_extension` from `get_vod_streams`, and
    /// `arc_str_default_on_null` collapses a missing or null value to an empty string.
    /// Clients build the playback URL as `<stream_id>.<container_extension>`, so an empty
    /// value leaves them with nothing to append and some render the blank as a literal
    /// `null`. The provider URL still carries the extension, so it is the fallback here,
    /// the same one `create_vod_info_from_item` already applies to its info document.
    fn container_extension_or_url_fallback(&self, container_extension: &Arc<str>) -> Arc<str> {
        if !container_extension.is_empty() {
            return Arc::clone(container_extension);
        }
        // `extract_extension_from_url` yields the extension with its leading dot, while
        // `container_extension` holds it without one.
        extract_extension_from_url(&self.url)
            .map_or_else(|| Arc::clone(container_extension), |ext| ext.trim_start_matches('.').intern())
    }

    fn to_info_document_no_props(&self, options: &XtreamMappingOptions) -> XtreamInfoDocument {
        let empty_str = "".intern();
        match self.xtream_cluster {
            XtreamCluster::Live => XtreamInfoDocument::Empty(XtreamEmptyDoc {}),
            XtreamCluster::Video => {
                let stream_icon = self.get_stream_icon(options);
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
                        stream_id: self.virtual_id.get(),
                        name: Arc::clone(&self.name),
                        added: Arc::clone(&empty_str),
                        category_id: self.category_id.intern(),
                        category_ids: vec![self.category_id],
                        container_extension: self.container_extension_or_url_fallback(&empty_str),
                        custom_sid: None,
                        direct_source: Arc::clone(&empty_str),
                    },
                })
            }
            XtreamCluster::Series => {
                let stream_icon = self.get_stream_icon(options);
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
                        backdrop_path: if stream_icon.is_empty() { vec![] } else { vec![Arc::clone(&stream_icon)] },
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
            self.to_document_no_props(options)
        }
    }

    // Clippy's method-path suggestion for `.intern()` names the private
    // `utils::string_interner` module and does not compile; closures are kept.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn series_to_document(&self, options: &XtreamMappingOptions, series: &SeriesStreamProperties) -> XtreamDocument {
        let empty_str = "".intern();
        XtreamDocument::Series(XtreamSeriesDoc {
            num: self.channel_no,
            name: self.title.clone(),
            series_id: self.virtual_id.get(),
            cover: self.get_stream_resource(options, &series.cover, "cover"),
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
                    let res_url = self.get_stream_resource(options, &series.cover, "cover");
                    if res_url.is_empty() {
                        vec![]
                    } else {
                        vec![res_url]
                    }
                },
                |b| {
                    b.iter()
                        .enumerate()
                        .map(|(idx, p)| {
                            options
                                .get_bd_path_resource_url(
                                    XtreamCluster::Series,
                                    self.item_type,
                                    self.virtual_id,
                                    p,
                                    "",
                                    idx,
                                )
                                .intern()
                        })
                        .collect()
                },
            ),
            youtube_trailer: series.youtube_trailer.clone(),
            tmdb: series.tmdb.map_or_else(|| Arc::clone(&empty_str), |v| v.intern()),
            episode_runtime: series.episode_run_time.clone().unwrap_or(empty_str),
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
        })
    }

    // Clippy's method-path suggestion for `.intern()` names the private
    // `utils::string_interner` module and does not compile; closures are kept.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn video_to_document(&self, options: &XtreamMappingOptions, video: &VideoStreamProperties) -> XtreamDocument {
        let stream_icon = self.get_stream_icon(options);
        let empty_str = "".intern();
        XtreamDocument::Video(XtreamVideoDoc {
            num: self.channel_no,
            name: self.title.clone(),
            stream_type: video.stream_type.clone().unwrap_or_else(default_as_movie),
            stream_id: self.virtual_id.get(),
            stream_icon,
            rating: video.rating.map_or_else(|| Arc::clone(&empty_str), |v| InfoDocUtils::limited(v).intern()),
            rating_5based: video.rating_5based.unwrap_or_default(),
            tmdb: video.tmdb.map_or_else(|| Arc::clone(&empty_str), |v| v.intern()),
            trailer: video.trailer.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            added: video.added.clone(),
            is_adult: video.is_adult,
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
            container_extension: self.container_extension_or_url_fallback(&video.container_extension),
            custom_sid: video.custom_sid.clone(),
            direct_source: if options.flags.contains(XtreamMappingFlags::SkipVideoDirectSource) {
                empty_str
            } else {
                video.direct_source.clone()
            },
        })
    }

    fn live_to_document(&self, options: &XtreamMappingOptions, live: &LiveStreamProperties) -> XtreamDocument {
        let stream_icon = self.get_stream_icon(options);
        let empty_str = "".intern();
        XtreamDocument::Live(XtreamLiveDoc {
            num: self.channel_no,
            name: self.title.clone(),
            stream_type: live.stream_type.clone().unwrap_or_else(default_as_live),
            stream_id: self.virtual_id.get(),
            stream_icon,
            epg_channel_id: self.epg_channel_id.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            added: live.added.clone().unwrap_or_else(|| Arc::clone(&empty_str)),
            is_adult: live.is_adult,
            category_id: self.category_id.intern(),
            category_ids: vec![self.category_id],
            custom_sid: live.custom_sid.clone(),
            tv_archive: live.tv_archive.unwrap_or_default(),
            direct_source: if options.flags.contains(XtreamMappingFlags::SkipLiveDirectSource) {
                empty_str
            } else {
                live.direct_source.clone()
            },
            tv_archive_duration: live.tv_archive_duration.unwrap_or_default(),
        })
    }

    fn to_document_no_props(&self, options: &XtreamMappingOptions) -> XtreamDocument {
        let empty_str = "".intern();
        let zero_str = "0".intern();
        let stream_icon = self.get_stream_icon(options);
        match self.xtream_cluster {
            XtreamCluster::Live => XtreamDocument::Live(XtreamLiveDoc {
                num: self.channel_no,
                name: self.title.clone(),
                stream_type: default_as_live(),
                stream_id: self.virtual_id.get(),
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
            }),
            XtreamCluster::Video => XtreamDocument::Video(XtreamVideoDoc {
                num: self.channel_no,
                name: self.title.clone(),
                stream_type: default_as_movie(),
                stream_id: self.virtual_id.get(),
                stream_icon,
                rating: Arc::clone(&zero_str),
                rating_5based: 0.0,
                tmdb: Arc::clone(&empty_str),
                trailer: Arc::clone(&empty_str),
                added: Arc::clone(&empty_str),
                is_adult: 0,
                category_id: self.category_id.intern(),
                category_ids: vec![self.category_id],
                container_extension: self.container_extension_or_url_fallback(&empty_str),
                custom_sid: None,
                direct_source: Arc::clone(&empty_str),
            }),
            XtreamCluster::Series => XtreamDocument::Series(XtreamSeriesDoc {
                num: self.channel_no,
                name: self.title.clone(),
                series_id: self.virtual_id.get(),
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
            }),
        }
    }

    fn get_stream_icon(&self, options: &XtreamMappingOptions) -> Arc<str> {
        if !self.logo.is_empty() {
            self.get_stream_resource(options, &self.logo, "logo")
        } else if !self.logo_small.is_empty() {
            self.get_stream_resource(options, &self.logo_small, "logo_small")
        } else {
            "".intern()
        }
    }

    fn get_stream_resource(
        &self,
        options: &XtreamMappingOptions,
        resource_url: &str,
        resource_field: &str,
    ) -> Arc<str> {
        options
            .get_resource_url(self.xtream_cluster, self.item_type, self.virtual_id, resource_url, resource_field)
            .intern()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        PlaylistItemType, PlaylistItemTypeSet, VideoStreamDetailProperties, VideoStreamProperties, VirtualId,
    };

    fn sample_options() -> XtreamMappingOptions {
        XtreamMappingOptions {
            flags: XtreamMappingFlags::RewriteResourceUrl.into(),
            force_redirect: None,
            reverse_item_types: PlaylistItemTypeSet::empty(),
            resource_proxy_item_types: PlaylistItemTypeSet::empty(),
            username: "user".to_string(),
            password: "pass".to_string(),
            base_url: "http://proxy.example".to_string(),
            web_ui_request: false,
            encrypt_secret: [0u8; 16],
        }
    }

    fn video_item(url: &str, properties: Option<StreamProperties>) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: VirtualId::new(4711),
            provider_id: 813_563,
            name: "Test".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "Test".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: Arc::<str>::from(url),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Video,
            additional_properties: properties,
            item_type: PlaylistItemType::Video,
            category_id: 0,
            input_name: "provider".intern(),
            channel_no: 1,
            source_ordinal: 0,
            input_stream_id: "813563".intern(),
            upstream_user_agent: None,
        }
    }

    fn video_properties(container_extension: &str, with_details: bool) -> StreamProperties {
        StreamProperties::Video(Box::new(VideoStreamProperties {
            name: "Test".intern(),
            stream_id: 813_563,
            container_extension: container_extension.intern(),
            details: with_details.then(VideoStreamDetailProperties::default),
            ..VideoStreamProperties::default()
        }))
    }

    fn video_doc_extension(item: &XtreamPlaylistItem) -> Arc<str> {
        let XtreamDocument::Video(doc) = item.to_document(&sample_options()) else {
            panic!("expected a video document");
        };
        doc.container_extension
    }

    fn video_info_doc_extension(item: &XtreamPlaylistItem) -> Arc<str> {
        let XtreamInfoDocument::Video(doc) = item.to_info_document(&sample_options()) else {
            panic!("expected a video info document");
        };
        doc.movie_data.container_extension
    }

    #[test]
    fn blank_container_extension_falls_back_to_the_url_extension() {
        let item = video_item("http://provider.example/movie/u/p/813563.mkv", Some(video_properties("", false)));
        assert_eq!(video_doc_extension(&item).as_ref(), "mkv");
    }

    #[test]
    fn provider_container_extension_wins_over_the_url_extension() {
        let item = video_item("http://provider.example/movie/u/p/813563.mkv", Some(video_properties("mp4", false)));
        assert_eq!(video_doc_extension(&item).as_ref(), "mp4");
    }

    #[test]
    fn blank_container_extension_stays_blank_without_a_url_extension() {
        let item = video_item("http://provider.example/movie/u/p/813563", Some(video_properties("", false)));
        assert_eq!(video_doc_extension(&item).as_ref(), "");
    }

    #[test]
    fn url_fallback_applies_when_the_item_carries_no_properties() {
        let item = video_item("http://provider.example/movie/u/p/813563.mkv", None);
        assert_eq!(video_doc_extension(&item).as_ref(), "mkv");
    }

    #[test]
    fn url_fallback_applies_to_the_resolved_info_document() {
        let item = video_item("http://provider.example/movie/u/p/813563.mkv", Some(video_properties("", true)));
        assert!(item.has_details(), "the resolved info branch needs details");
        assert_eq!(video_info_doc_extension(&item).as_ref(), "mkv");
    }

    #[test]
    fn url_fallback_applies_to_the_info_document_without_properties() {
        let item = video_item("http://provider.example/movie/u/p/813563.mkv", None);
        assert_eq!(video_info_doc_extension(&item).as_ref(), "mkv");
    }
}
