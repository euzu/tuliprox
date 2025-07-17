use serde::{Deserialize, Serialize};

const  TRAKT_API_KEY: &str = "0183a05ad97098d87287fe46da4ae286f434f32e8e951caad4cc147c947d79a3";
const  TRAKT_API_VERSION: &str = "2";
const  TRAKT_API_URL: &str = "https://api.trakt.tv";
fn default_fuzzy_threshold() -> u8 {
    80
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraktContentType {
    Vod,
    Series,
    Both,
}

impl Default for TraktContentType {
    fn default() -> Self {
        Self::Both
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktApiConfigDto {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub url: String,
}

impl TraktApiConfigDto {
    pub fn prepare(&mut self) {
        let key  =  self.key.trim();
        self.key = String::from(if key.is_empty() { TRAKT_API_KEY } else { key });
        let version = self.version.trim();
        self.version = String::from(if version.is_empty() { TRAKT_API_VERSION } else { version });
        let url = self.url.trim();
        self.url = String::from(if url.is_empty() { TRAKT_API_URL } else { url });
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktListConfigDto {
    pub user: String,
    pub list_slug: String,
    pub category_name: String,
    pub content_type: TraktContentType,
    #[serde(default = "default_fuzzy_threshold")]
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktConfigDto {
    #[serde(default)]
    pub api: TraktApiConfigDto,
    pub lists: Vec<TraktListConfigDto>,
}


impl TraktConfigDto {
    pub fn prepare(&mut self) {
        self.api.prepare();
    }
}
