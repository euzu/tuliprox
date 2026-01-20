use crate::model::DEFAULT_USER_AGENT;
use crate::utils::{default_as_true, default_trakt_fuzzy_threshold, is_true,
                   TRAKT_API_KEY, TRAKT_API_URL, TRAKT_API_VERSION};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraktContentType {
    Vod,
    Series,
    #[default]
    Both,
}

impl fmt::Display for TraktContentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            TraktContentType::Vod => "Vod",
            TraktContentType::Series => "Series",
            TraktContentType::Both => "Both",
        })
    }
}

impl FromStr for TraktContentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "vod" => Ok(TraktContentType::Vod),
            "series" => Ok(TraktContentType::Series),
            "both" => Ok(TraktContentType::Both),
            _ => Err(format!("Invalid TraktContentType: {}", s)),
        }
    }
}


#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktApiConfigDto {
    #[serde(default, alias = "key")]
    pub api_key: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub user_agent: String,
}

impl TraktApiConfigDto {
    pub fn prepare(&mut self) {
        let key = self.api_key.trim();
        self.api_key = String::from(if key.is_empty() { TRAKT_API_KEY } else { key });
        let version = self.version.trim();
        self.version = String::from(if version.is_empty() { TRAKT_API_VERSION } else { version });
        let url = self.url.trim();
        self.url = String::from(if url.is_empty() { TRAKT_API_URL } else { url });
        let user_agent = self.user_agent.trim();
        self.user_agent = String::from(if user_agent.is_empty() { DEFAULT_USER_AGENT } else { user_agent });
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktListConfigDto {
    pub user: String,
    pub list_slug: String,
    pub category_name: String,
    pub content_type: TraktContentType,
    #[serde(default = "default_trakt_fuzzy_threshold")]
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

impl Default for TraktListConfigDto {
    fn default() -> Self {
        TraktListConfigDto {
            user: String::new(),
            list_slug: String::new(),
            category_name: String::new(),
            content_type: TraktContentType::default(),
            fuzzy_match_threshold: default_trakt_fuzzy_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api: TraktApiConfigDto,
    pub lists: Vec<TraktListConfigDto>,
}

impl Default for TraktConfigDto {
    fn default() -> Self {
        Self {
            enabled: true,
            api: TraktApiConfigDto::default(),
            lists: Vec::new(),
        }
    }
}

impl TraktConfigDto {
    pub fn prepare(&mut self) {
        self.api.prepare();
    }
}
