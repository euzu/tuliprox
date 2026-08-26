use serde::{Deserialize, Serialize};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct PttMetadata {
    pub title: String,
    pub seasons: Vec<u32>,
    pub episodes: Vec<u32>,
    pub languages: Vec<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub tmdb: Option<u32>,
    #[serde(default)]
    pub tvdb: Option<u32>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub codec: Option<String>,
    pub audio: Vec<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    pub networks: Vec<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extended: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hardcoded: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proper: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repack: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retail: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remastered: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unrated: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncensored: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub documentary: bool,
    #[serde(default)]
    pub episode_code: Option<String>,
    #[serde(default)]
    pub date: Option<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adult: bool,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub bit_depth: Option<String>,
    #[serde(default)]
    pub hdr: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<i32>,
    #[serde(default)]
    pub edition: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trash: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upscaled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub convert: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub commentary: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subbed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dubbed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_3d: bool,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ppv: bool,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub bitrate: Option<String>,
    #[serde(default)]
    pub extension: Option<String>,
}
