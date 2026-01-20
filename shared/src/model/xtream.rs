use crate::utils::{deserialize_as_string_array,
                   deserialize_number_from_string, deserialize_number_from_string_or_zero,
                   deserialize_json_as_opt_string, serialize_json_as_opt_string, arc_str_option_serde, arc_str_serde};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoInfoMovieData {
    #[serde(default, with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub stream_id: u32,
    #[serde(default, with = "arc_str_serde")]
    pub direct_source: Arc<str>,
    #[serde(default, serialize_with = "arc_str_option_serde::serialize_null_if_empty", deserialize_with = "arc_str_option_serde::deserialize")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, with = "arc_str_serde")]
    pub added: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub container_extension: Arc<str>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoInfoInfo {
    #[serde(default, with = "arc_str_option_serde")]
    pub kinopoisk_url: Option<Arc<str>>,
    #[serde(default, with = "arc_str_serde")]
    pub tmdb_id: Arc<str>, // is in get_vod_streams
    #[serde(default, with = "arc_str_serde")]
    pub name: Arc<str>, // is in get_vod_streams
    #[serde(default, with = "arc_str_option_serde")]
    pub o_name: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub cover_big: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub movie_image: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub releasedate: Option<Arc<str>>,
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
    #[serde(default, serialize_with = "serialize_json_as_opt_string", deserialize_with = "deserialize_json_as_opt_string")]
    pub video: Option<Arc<str>>,
    #[serde(default, serialize_with = "serialize_json_as_opt_string", deserialize_with = "deserialize_json_as_opt_string")]
    pub audio: Option<Arc<str>>,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default, with = "arc_str_option_serde")]
    pub runtime: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub status: Option<Arc<str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct XtreamVideoInfo {
    pub info: XtreamVideoInfoInfo,
    pub movie_data: XtreamVideoInfoMovieData,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfoSeason {
    #[serde(default, with = "arc_str_serde")]
    pub air_date: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_count: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub id: u32,
    #[serde(default, with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub overview: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season_number: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub vote_average: f64,
    #[serde(default, with = "arc_str_serde")]
    pub cover: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub cover_big: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub cover_tmdb: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub duration: Arc<str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(non_snake_case)]
pub struct XtreamSeriesInfoInfo {
    #[serde(default, with = "arc_str_serde")]
    pub(crate) name: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub cover: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub plot: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub cast: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub director: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub genre: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub release_date: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub last_modified: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub rating: f64,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub rating_5based: f64,
    #[serde(default, deserialize_with = "deserialize_as_string_array")]
    pub backdrop_path: Option<Vec<Arc<str>>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tmdb: Option<u32>,
    #[serde(default, with = "arc_str_serde")]
    pub youtube_trailer: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub episode_run_time: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
}

#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct XtreamSeriesInfoEpisodeInfo {
    #[serde(default, with = "arc_str_serde")]
    pub air_date: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub crew: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub rating: f64,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub id: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub duration_secs: u32,
    #[serde(default, with = "arc_str_serde")]
    pub duration: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub movie_image: Arc<str>,
    #[serde(default, serialize_with = "serialize_json_as_opt_string", deserialize_with = "deserialize_json_as_opt_string")]
    pub video: Option<Arc<str>>,
    #[serde(default, serialize_with = "serialize_json_as_opt_string", deserialize_with = "deserialize_json_as_opt_string")]
    pub audio: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub bitrate: u32,
}

// Used for serde_json deserialization, cannot be used with bincode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfoEpisode {
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub episode_num: u32,
    #[serde(default, with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(default, with = "arc_str_serde")]
    pub container_extension: Arc<str>,
    #[serde(default)]
    pub info: Option<XtreamSeriesInfoEpisodeInfo>,
    #[serde(default, serialize_with = "arc_str_option_serde::serialize_null_if_empty", deserialize_with = "arc_str_option_serde::deserialize")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, with = "arc_str_serde")]
    pub added: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub season: u32,
    #[serde(default, with = "arc_str_serde")]
    pub direct_source: Arc<str>,
}

impl XtreamSeriesInfoEpisode {
    pub fn get_id(&self) -> u32 {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamSeriesInfo {
    #[serde(default)]
    pub seasons: Option<Vec<XtreamSeriesInfoSeason>>,
    #[serde(default)]
    pub info: XtreamSeriesInfoInfo,
    #[serde(
        default,
        serialize_with = "serialize_episodes",
        deserialize_with = "deserialize_episodes"
    )]
    pub episodes: Option<Vec<XtreamSeriesInfoEpisode>>,
}


// sometimes episodes are a map with season as key, sometimes an array
fn deserialize_episodes<'de, D>(deserializer: D) -> Result<Option<Vec<XtreamSeriesInfoEpisode>>, D::Error>
where
    D: Deserializer<'de>,
{
    // read as generic value
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(None),
        Value::Array(array) => {
            if array.is_empty() {
                Ok(None)
            } else {
                let mut result = Vec::new();
                for inner in array {
                    if let Some(inner_array) = inner.as_array() {
                        // Nested array case: [[ep1, ep2], [ep3]]
                        for item in inner_array {
                            let ep: XtreamSeriesInfoEpisode = serde_json::from_value(item.clone())
                                .map_err(serde::de::Error::custom)?;
                            result.push(ep);
                        }
                    } else if inner.is_object() {
                        // Flat array case: [ep1, ep2, ep3]
                        let ep: XtreamSeriesInfoEpisode = serde_json::from_value(inner.clone())
                            .map_err(serde::de::Error::custom)?;
                        result.push(ep);
                    }
                }
                Ok(if result.is_empty() { None } else { Some(result) })
            }
        }
        Value::Object(object) => {
            if object.is_empty() {
                Ok(None)
            } else {
                let mut result = Vec::new();
                for (_key, val) in object {
                    if let Some(inner_array) = val.as_array() {
                        for item in inner_array {
                            let ep: XtreamSeriesInfoEpisode = serde_json::from_value(item.clone())
                                .map_err(serde::de::Error::custom)?;
                            result.push(ep);
                        }
                    }
                }
                Ok(Some(result))
            }
        }
        _ => Err(serde::de::Error::custom("Invalid format for episodes")),
    }
}

#[allow(clippy::ref_option)]
fn serialize_episodes<S>(
    episodes: &Option<Vec<XtreamSeriesInfoEpisode>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match episodes {
        None => {
            let map = serializer.serialize_map(Some(0))?;
            map.end()
        }
        Some(list) => {
            if list.is_empty() {
                let map = serializer.serialize_map(Some(0))?;
                return map.end();
            }
            let mut seasons: BTreeMap<String, Vec<&XtreamSeriesInfoEpisode>> = BTreeMap::new();
            for ep in list {
                seasons.entry(ep.season.to_string()).or_default().push(ep);
            }

            seasons.serialize(serializer)
        }
    }
}