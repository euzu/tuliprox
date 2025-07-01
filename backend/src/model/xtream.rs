use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget, XtreamTargetOutput};
use shared::model::{xtream_const, PlaylistItem,XtreamPlaylistItem};
use shared::utils::{deserialize_as_option_rc_string, deserialize_as_rc_string, deserialize_as_string_array, deserialize_number_from_string,
                    opt_string_or_number_u32, string_default_on_null, string_or_number_f64, string_or_number_u32, get_non_empty_str};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::iter::FromIterator;
use shared::model::{ClusterFlags, PlaylistEntry, XtreamCluster};

#[derive(Deserialize, Default)]
pub struct XtreamCategory {
    #[serde(deserialize_with = "deserialize_as_rc_string")]
    pub category_id: String,
    #[serde(deserialize_with = "deserialize_as_rc_string")]
    pub category_name: String,
    //pub parent_id: i32,
    #[serde(default)]
    pub channels: Vec<PlaylistItem>,
}

impl XtreamCategory {
    pub fn add(&mut self, item: PlaylistItem) {
        self.channels.push(item);
    }
}

#[derive(Serialize, Deserialize)]
pub struct XtreamStream {
    #[serde(default, deserialize_with = "deserialize_as_rc_string")]
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_as_rc_string")]
    pub category_id: String,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub stream_id: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub series_id: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_as_rc_string")]
    pub stream_icon: String,
    #[serde(default, deserialize_with = "deserialize_as_rc_string")]
    pub direct_source: String,

    // optional attributes
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub custom_sid: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_string_array")]
    pub backdrop_path: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub added: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub cast: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub container_extension: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub cover: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub director: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub episode_run_time: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub genre: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub last_modified: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub plot: Option<String>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub rating: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub rating_5based: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub stream_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub year: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub trailer: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub youtube_trailer: Option<String>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub epg_channel_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive_duration: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_as_option_rc_string")]
    pub tmdb: Option<String>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub is_adult: Option<i32>,

}

macro_rules! add_str_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
        $vec.insert(String::from($prop_name), Value::String($prop.to_string()));
    }
}
macro_rules! add_rc_str_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
       $prop.as_ref().map(|v| $vec.insert(String::from($prop_name), Value::String(v.to_string())));
    }
}

macro_rules! add_opt_i64_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
       $prop.as_ref().map(|v| $vec.insert(String::from($prop_name), Value::Number(serde_json::value::Number::from(i64::from(*v)))));
    }
}

macro_rules! add_opt_f64_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
       $prop.as_ref().map(|v| $vec.insert(String::from($prop_name), Value::Number(serde_json::value::Number::from_f64(f64::from(*v)).unwrap())));
    }
}

macro_rules! add_f64_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
       $vec.insert(String::from($prop_name), Value::Number(serde_json::value::Number::from_f64(f64::from($prop)).unwrap()));
    }
}

macro_rules! add_i64_property_if_exists {
    ($vec:expr, $prop:expr, $prop_name:expr) => {
       $vec.insert(String::from($prop_name), Value::Number(serde_json::value::Number::from(i64::from($prop))));
    }
}

macro_rules! add_to_doc_str_property_if_not_exists {
    ($document:expr, $prop_name:expr, $prop_value:expr) => {
          match $document.get($prop_name) {
            None => {
                $document.insert(String::from($prop_name), $prop_value);
            }
            Some(value) => { if Value::is_null(value) {
                $document.insert(String::from($prop_name), $prop_value);
            }}
          }
    }
}


impl XtreamStream {
    pub fn get_stream_id(&self) -> u32 {
        self.stream_id.unwrap_or_else(|| self.series_id.unwrap_or(0))
    }

    pub fn get_additional_properties(&self) -> Option<Value> {
        let mut result = Map::new();
        if let Some(bdpath) = self.backdrop_path.as_ref() {
            if !bdpath.is_empty() {
                result.insert(String::from(xtream_const::XC_PROP_BACKDROP_PATH), Value::Array(Vec::from([Value::String(String::from(bdpath.first()?))])));
            }
        }
        add_rc_str_property_if_exists!(result, self.tmdb, "tmdb");
        add_rc_str_property_if_exists!(result, self.added, "added");
        add_rc_str_property_if_exists!(result, self.cast, "cast");
        add_rc_str_property_if_exists!(result, self.container_extension, "container_extension");
        add_rc_str_property_if_exists!(result, self.cover, "cover");
        add_rc_str_property_if_exists!(result, self.director, "director");
        add_rc_str_property_if_exists!(result, self.episode_run_time, "episode_run_time");
        add_rc_str_property_if_exists!(result, self.genre, "genre");
        add_rc_str_property_if_exists!(result, self.last_modified, "last_modified");
        add_rc_str_property_if_exists!(result, self.plot, "plot");
        add_opt_f64_property_if_exists!(result, self.rating, "rating");
        add_opt_f64_property_if_exists!(result, self.rating_5based, "rating_5based");
        add_rc_str_property_if_exists!(result, self.release_date, "release_date");
        add_rc_str_property_if_exists!(result, self.stream_type, "stream_type");
        add_rc_str_property_if_exists!(result, self.title, "title");
        add_rc_str_property_if_exists!(result, self.year, "year");
        add_rc_str_property_if_exists!(result, self.trailer, "trailer");
        add_rc_str_property_if_exists!(result, self.youtube_trailer, "youtube_trailer");
        add_rc_str_property_if_exists!(result, self.epg_channel_id, "epg_channel_id");
        add_opt_i64_property_if_exists!(result, self.tv_archive, "tv_archive");
        add_opt_i64_property_if_exists!(result, self.tv_archive_duration, "tv_archive_duration");
        add_opt_i64_property_if_exists!(result, self.is_adult, "is_adult");
        if result.is_empty() { None } else { Some(Value::Object(result)) }
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfoSeason {
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub air_date: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub episode_count: u32,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub id: u32,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub name: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub overview: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub season_number: u32,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub vote_average: f64,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub cover: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub cover_big: String,

}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(non_snake_case)]
pub struct XtreamSeriesInfoInfo {
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub(crate) name: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    cover: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    plot: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    cast: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    director: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    genre: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    release_date: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    releaseDate: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    releasedate: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    last_modified: String,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    rating: f64,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    rating_5based: f64,
    #[serde(default, deserialize_with = "deserialize_as_string_array")]
    pub backdrop_path: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_default_on_null")]
    trailer: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    youtube_trailer: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    episode_run_time: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    category_id: u32,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct XtreamSeriesInfoEpisodeInfo {
    #[serde(default, deserialize_with = "opt_string_or_number_u32")]
    pub tmdb_id: Option<u32>,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub release_date: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub releaseDate: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub releasedate: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub plot: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub duration_secs: u32,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub duration: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub movie_image: String,
    #[serde(default)]
    pub video: Value,
    #[serde(default)]
    pub audio: Value,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub bitrate: u32,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub rating: f64,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub season: u32,
}

// Used for serde_json deserialization, can not be used with bincode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfoEpisode {
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub id: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub episode_num: u32,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub title: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub container_extension: String,
    #[serde(default)]
    pub info: Option<XtreamSeriesInfoEpisodeInfo>,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub custom_sid: String,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub added: String,
    #[serde(default, deserialize_with = "string_or_number_u32")]
    pub season: u32,
    #[serde(default, deserialize_with = "string_default_on_null")]
    pub direct_source: String,
}

impl XtreamSeriesInfoEpisode {
    pub fn get_id(&self) -> u32 {
        self.id.parse::<u32>().unwrap_or(0)
    }
}

//bincode does not support deserialize_with. We use this struct for db
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesEpisode {
    pub id: u32,
    pub episode_num: u32,
    pub title: String,
    pub container_extension: String,
    pub custom_sid: String,
    pub added: String,
    pub season: u32,
    pub tmdb_id: u32,
    pub direct_source: String,
}

impl XtreamSeriesEpisode {
    pub fn from(info_episode: &XtreamSeriesInfoEpisode) -> Self {
        Self {
            id: info_episode.get_id(),
            episode_num: info_episode.episode_num,
            title: info_episode.title.to_string(),
            container_extension: info_episode.container_extension.to_string(),
            custom_sid: info_episode.custom_sid.to_string(),
            added: info_episode.added.to_string(),
            season: info_episode.season,
            tmdb_id: info_episode.info.as_ref().and_then(|info| info.tmdb_id).unwrap_or(0),
            direct_source: info_episode.direct_source.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfo {
    #[serde(default)]
    pub seasons: Option<Vec<XtreamSeriesInfoSeason>>,
    #[serde(default)]
    pub info: Option<XtreamSeriesInfoInfo>,
    #[serde(default)]
    pub episodes: Option<HashMap<String, Vec<XtreamSeriesInfoEpisode>>>,
}

impl XtreamSeriesInfoEpisode {
    pub fn get_additional_properties(&self, series_info: &XtreamSeriesInfo) -> Option<Value> {
        let mut result = Map::new();
        let info = series_info.info.as_ref();
        let bdpath = info.and_then(|i| i.backdrop_path.as_ref());
        let bdpath_is_set = bdpath.as_ref().is_some_and(|bdpath| !bdpath.is_empty());
        if bdpath_is_set {
            result.insert(String::from("backdrop_path"), Value::Array(Vec::from([Value::String(String::from(bdpath?.first()?))])));
        }
        add_str_property_if_exists!(result, info.map_or("", |i| i.name.as_str()), "series_name");
        add_str_property_if_exists!(result, info.map_or("", |i| get_non_empty_str(i.release_date.as_str(), i.releaseDate.as_str(), i.releasedate.as_str())), "series_release_date");
        add_str_property_if_exists!(result, self.added.as_str(), "added");
        add_str_property_if_exists!(result, info.map_or("", |i| i.cast.as_str()), "cast");
        add_str_property_if_exists!(result, self.container_extension.as_str(), "container_extension");
        add_str_property_if_exists!(result, self.info.as_ref().map_or("", |info| info.movie_image.as_str()), "cover");
        add_str_property_if_exists!(result, info.map_or("", |i| i.director.as_str()), "director");
        add_str_property_if_exists!(result, info.map_or("", |i| i.episode_run_time.as_str()), "episode_run_time");
        add_str_property_if_exists!(result, info.map_or("", |i| i.last_modified.as_str()), "last_modified");
        add_str_property_if_exists!(result, self.info.as_ref().map_or("", |info| info.plot.as_str()), "plot");
        add_f64_property_if_exists!(result, info.map_or(0_f64, |i| i.rating), "rating");
        add_f64_property_if_exists!(result, info.map_or(0_f64, |i| i.rating_5based), "rating_5based");
        add_str_property_if_exists!(result, self.info.as_ref().map_or("", |info| get_non_empty_str(info.release_date.as_str(), info.releaseDate.as_str(), info.releasedate.as_str())), "release_date");
        add_str_property_if_exists!(result, self.title, "title");
        add_i64_property_if_exists!(result, self.season, "season");
        add_i64_property_if_exists!(result, self.episode_num, "episode");
        add_opt_i64_property_if_exists!(result, self.info.as_ref().and_then(|info| info.tmdb_id), "tmdb_id");
        if result.is_empty() { None } else { Some(Value::Object(result)) }
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct XtreamMappingOptions {
    pub skip_live_direct_source: bool,
    pub skip_video_direct_source: bool,
    pub skip_series_direct_source: bool,
    pub rewrite_resource_url: bool,
    pub force_redirect: Option<ClusterFlags>,
}

impl XtreamMappingOptions {
    pub fn from_target_options(target: &ConfigTarget, target_output: &XtreamTargetOutput, cfg: &AppConfig) -> Self {
        Self {
            skip_live_direct_source: target_output.skip_live_direct_source,
            skip_video_direct_source: target_output.skip_video_direct_source,
            skip_series_direct_source: target_output.skip_series_direct_source,
            rewrite_resource_url: cfg.is_reverse_proxy_resource_rewrite_enabled(),
            force_redirect: target.options.as_ref().and_then(|o| o.force_redirect),
        }
    }
}

pub fn normalize_release_date(document: &mut serde_json::Map<String, Value>) {
    // Find the first non-empty release date key
    let date_value = document.get("release_date")
        .or_else(|| document.get("releaseDate"))
        .or_else(|| document.get("releasedate"))
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) 
        .cloned();

    // Remove unused keys (optional)
    document.remove("releaseDate");
    document.remove("releasedate");

    // Insert the normalized release date or null if not found
    if let Some(date) = date_value {
        document.insert("release_date".to_string(), date);
    } else {
        document.insert("release_date".to_string(), Value::Null);
    }
}

fn append_mandatory_fields(document: &mut serde_json::Map<String, Value>, fields: &[&str]) {
    for &field in fields {
        if !document.contains_key(field) {
            document.insert(field.to_string(), Value::Null);
        }
    }
}

fn append_prepared_series_properties(add_props: Option<&Map<String, Value>>, document: &mut Map<String, Value>) {
    if let Some(props) = add_props {
        match props.get("rating") {
            Some(value) => {
                document.insert("rating".to_string(), match value {
                    Value::Number(val) => Value::String(format!("{:.0}", val.as_f64().unwrap())),
                    Value::String(val) => Value::String(val.to_string()),
                    _ => Value::String("0".to_string()),
                });
            }
            None => {
                document.insert("rating".to_string(), Value::String("0".to_string()));
            }
        }
    }
}

fn make_bdpath_resource_url(resource_url: &str, bd_path: &str, index: usize, field_prefix: &str) -> String {
    if bd_path.starts_with("http") {
        format!("{resource_url}/{field_prefix}{}_{index}", xtream_const::XC_PROP_BACKDROP_PATH)
    } else {
        bd_path.to_string()
    }
}

pub fn xtream_playlistitem_to_document(pli: &XtreamPlaylistItem, url: &str, options: &XtreamMappingOptions, user: &ProxyUserCredentials) -> serde_json::Value {
    let stream_id_value = Value::Number(serde_json::Number::from(pli.virtual_id));

    let is_reverse = user.proxy.is_reverse(pli.item_type) && !options.force_redirect.as_ref().is_some_and(|o| o.has_cluster(pli.item_type));
    let (resource_url, logo, logo_small) = if is_reverse && options.rewrite_resource_url {
        let resource_url = format!("{url}/resource/{}/{}/{}/{}", pli.xtream_cluster.as_stream_type(), user.username, user.password, pli.get_virtual_id());
        let logo_url = if pli.logo.is_empty() { String::new() } else { format!("{resource_url}/logo") };
        let logo_small_url = if pli.logo_small.is_empty() { String::new() } else { format!("{resource_url}/logo_small") };
        (Some(resource_url), logo_url, logo_small_url)
    } else {
        (None, pli.logo.clone(), pli.logo_small.clone())
    };
    let mut document = serde_json::Map::from_iter([
        ("category_id".to_string(), Value::String(format!("{}", &pli.category_id))),
        ("category_ids".to_string(), Value::Array(Vec::from([Value::Number(serde_json::Number::from(pli.category_id))]))),
        ("name".to_string(), Value::String(pli.name.clone())),
        ("num".to_string(), Value::Number(serde_json::Number::from(pli.channel_no))),
        ("title".to_string(), Value::String(pli.title.clone())),
        ("stream_icon".to_string(), Value::String(logo)),
    ]);

    match pli.xtream_cluster {
        XtreamCluster::Live => {
            document.insert("stream_id".to_string(), stream_id_value);
            if options.skip_live_direct_source {
                document.insert("direct_source".to_string(), Value::String(String::new()));
            } else {
                document.insert("direct_source".to_string(), Value::String(pli.url.clone()));
            }
            document.insert("thumbnail".to_string(), Value::String(logo_small));
            document.insert("custom_sid".to_string(), Value::String(String::new()));
            document.insert("epg_channel_id".to_string(), pli.epg_channel_id.as_ref().map_or(Value::Null, |epg_id| Value::String(epg_id.clone())));
        }
        XtreamCluster::Video => {
            document.insert("stream_id".to_string(), stream_id_value);
            if options.skip_video_direct_source {
                document.insert("direct_source".to_string(), Value::String(String::new()));
            } else {
                document.insert("direct_source".to_string(), Value::String(pli.url.clone()));
            }
            document.insert("custom_sid".to_string(), Value::String(String::new()));
        }
        XtreamCluster::Series => {
            document.insert("series_id".to_string(), stream_id_value);
        }
    }

    let props = pli.additional_properties.as_ref().and_then(|add_props| serde_json::from_str::<Map<String, Value>>(add_props).ok());

    if let Some(ref add_props) = props {
        for (field_name, field_value) in add_props {
            if !document.contains_key(field_name) {
                document.insert(field_name.to_string(), field_value.to_owned());
            }
        }
    }

    match pli.xtream_cluster {
        XtreamCluster::Live => {
            append_mandatory_fields(&mut document, xtream_const::LIVE_STREAM_FIELDS);
            add_to_doc_str_property_if_not_exists!(document, "stream_type", Value::String(String::from("live")));
            add_to_doc_str_property_if_not_exists!(document, "added", Value::String(chrono::Utc::now().timestamp().to_string()));
        }
        XtreamCluster::Video => {
            append_mandatory_fields(&mut document, xtream_const::VIDEO_STREAM_FIELDS);
            add_to_doc_str_property_if_not_exists!(document, "stream_type", Value::String(String::from("movie")));
            add_to_doc_str_property_if_not_exists!(document, "added", Value::String(chrono::Utc::now().timestamp().to_string()));
        }
        XtreamCluster::Series => {
            append_prepared_series_properties(props.as_ref(), &mut document);
            append_mandatory_fields(&mut document, xtream_const::SERIES_STREAM_FIELDS);
        }
    }

    rewrite_doc_urls(resource_url.as_ref(), &mut document, xtream_const::XTREAM_VOD_REWRITE_URL_PROPS, "");

    Value::Object(document)
}

pub fn rewrite_doc_urls(resource_url: Option<&String>, document: &mut Map<String, Value>, fields: &[&str], field_prefix: &str) {
    if let Some(rewrite_url) = resource_url {
        if let Some(bdpath) = document.get(xtream_const::XC_PROP_BACKDROP_PATH) {
            match bdpath {
                Value::String(bd_path) => {
                    document.insert(xtream_const::XC_PROP_BACKDROP_PATH.to_string(), Value::Array(vec![Value::String(make_bdpath_resource_url(rewrite_url.as_str(), bd_path, 0, field_prefix))]));
                }
                Value::Array(bd_path) => {
                    document.insert(xtream_const::XC_PROP_BACKDROP_PATH.to_string(), Value::Array(
                        bd_path.iter()
                            .filter_map(|val| val.as_str())
                            .enumerate()
                            .map(|(index, value)| Value::String(make_bdpath_resource_url(rewrite_url.as_str(), value, index, field_prefix)))
                            .collect()));
                }
                _ => {}
            }
        }
        for &field in fields {
            if let Some(Value::String(value)) = document.get(field) {
                if value.starts_with("http") {
                    document.insert(field.to_string(), Value::String(format!("{rewrite_url}/{field_prefix}{field}")));
                }
            }
        }
    }
}


#[derive(Deserialize, Serialize)]
pub struct PlaylistXtreamCategory {
    #[serde(alias = "category_id")]
    pub id: String,
    #[serde(alias = "category_name")]
    pub name: String,
}