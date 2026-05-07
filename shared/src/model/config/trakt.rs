use crate::{
    error::TuliproxError,
    utils::{
        default_as_true, default_trakt_fuzzy_threshold, is_false, is_true, DEFAULT_USER_AGENT, TRAKT_API_KEY,
        TRAKT_API_URL, TRAKT_API_VERSION,
    },
};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

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
        write!(
            f,
            "{}",
            match self {
                TraktContentType::Vod => "Vod",
                TraktContentType::Series => "Series",
                TraktContentType::Both => "Both",
            }
        )
    }
}

impl FromStr for TraktContentType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "vod" => Ok(TraktContentType::Vod),
            "series" => Ok(TraktContentType::Series),
            "both" => Ok(TraktContentType::Both),
            _ => Err(TuliproxError::Config(format!("Invalid TraktContentType: {}", s))),
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub tmdb_only: bool,
    #[serde(default = "default_trakt_fuzzy_threshold")]
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraktChartKind {
    #[default]
    #[serde(alias = "movie", alias = "vod")]
    Movies,
    #[serde(alias = "show", alias = "series", alias = "tvshows")]
    Shows,
}

impl TraktChartKind {
    pub const fn content_type(self) -> TraktContentType {
        match self {
            Self::Movies => TraktContentType::Vod,
            Self::Shows => TraktContentType::Series,
        }
    }
}

impl fmt::Display for TraktChartKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Movies => "movies",
            Self::Shows => "shows",
        })
    }
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraktChartType {
    #[default]
    Trending,
    Popular,
}

impl fmt::Display for TraktChartType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trending => "trending",
            Self::Popular => "popular",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TraktChartConfigDto {
    pub kind: TraktChartKind,
    pub chart: TraktChartType,
    pub category_name: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tmdb_only: bool,
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
            tmdb_only: false,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lists: Vec<TraktListConfigDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub charts: Vec<TraktChartConfigDto>,
}

impl Default for TraktConfigDto {
    fn default() -> Self {
        Self { enabled: true, api: TraktApiConfigDto::default(), lists: Vec::new(), charts: Vec::new() }
    }
}

impl TraktConfigDto {
    pub fn prepare(&mut self) { self.api.prepare(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trakt_config_accepts_charts_without_user_lists() {
        let config = serde_json::from_str::<TraktConfigDto>(
            r#"{"charts":[{"kind":"movies","chart":"trending","category_name":"Trending Movies","tmdb_only":true}]}"#,
        )
        .expect("charts-only Trakt config should deserialize");

        assert!(config.lists.is_empty());
        assert_eq!(config.charts.len(), 1);
        assert_eq!(config.charts[0].kind, TraktChartKind::Movies);
        assert_eq!(config.charts[0].kind.content_type(), TraktContentType::Vod);
        assert_eq!(config.charts[0].chart, TraktChartType::Trending);
        assert_eq!(config.charts[0].fuzzy_match_threshold, default_trakt_fuzzy_threshold());
    }

    #[test]
    fn trakt_chart_kind_accepts_show_aliases() {
        let config = serde_json::from_str::<TraktConfigDto>(
            r#"{"charts":[{"kind":"series","chart":"popular","category_name":"Popular Shows"}]}"#,
        )
        .expect("series alias should deserialize as show charts");

        assert_eq!(config.charts[0].kind, TraktChartKind::Shows);
        assert_eq!(config.charts[0].kind.content_type(), TraktContentType::Series);
        assert_eq!(config.charts[0].chart, TraktChartType::Popular);
    }
}
