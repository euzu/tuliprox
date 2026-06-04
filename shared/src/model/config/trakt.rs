use crate::utils::{
    default_as_true, default_trakt_fuzzy_threshold, is_false, is_true, DEFAULT_USER_AGENT, TRAKT_API_KEY,
    TRAKT_API_URL, TRAKT_API_VERSION,
};
use serde::{Deserialize, Serialize};
use strum_macros::{Display, EnumString};

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "lowercase")]
pub enum TraktContentType {
    #[strum(serialize = "vod")]
    Vod,
    #[strum(serialize = "series")]
    Series,
    #[default]
    #[strum(serialize = "both")]
    Both,
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

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TraktChartKind {
    #[default]
    #[serde(alias = "movie", alias = "vod")]
    // `to_string` defines the canonical emitted value; `serialize` adds accepted parse aliases.
    #[strum(to_string = "movies", serialize = "movies", serialize = "movie", serialize = "vod")]
    Movies,
    #[serde(alias = "show", alias = "series", alias = "tvshows")]
    #[strum(to_string = "shows", serialize = "shows", serialize = "show", serialize = "series", serialize = "tvshows")]
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

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TraktChartType {
    #[default]
    Trending,
    Popular,
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

impl Default for TraktChartConfigDto {
    fn default() -> Self {
        Self {
            kind: TraktChartKind::default(),
            chart: TraktChartType::default(),
            category_name: String::new(),
            tmdb_only: false,
            fuzzy_match_threshold: default_trakt_fuzzy_threshold(),
        }
    }
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
    fn trakt_content_type_parsing_and_display_remain_stable() {
        assert_eq!("vod".parse::<TraktContentType>().ok(), Some(TraktContentType::Vod));
        assert_eq!("series".parse::<TraktContentType>().ok(), Some(TraktContentType::Series));
        assert_eq!("both".parse::<TraktContentType>().ok(), Some(TraktContentType::Both));
        assert!("SERIES".parse::<TraktContentType>().is_err(), "should not accept SERIES");
        assert_eq!(TraktContentType::Vod.to_string(), "vod");
        assert_eq!(TraktContentType::Series.to_string(), "series");
        assert_eq!(TraktContentType::Both.to_string(), "both");
    }

    #[test]
    fn trakt_chart_kind_parsing_aliases_and_display_remain_stable() {
        assert_eq!("movies".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Movies));
        assert_eq!("movie".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Movies));
        assert_eq!("VOD".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Movies));
        assert_eq!("shows".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Shows));
        assert_eq!("series".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Shows));
        assert_eq!("tvshows".parse::<TraktChartKind>().ok(), Some(TraktChartKind::Shows));
        assert_eq!(TraktChartKind::Movies.to_string(), "movies");
        assert_eq!(TraktChartKind::Shows.to_string(), "shows");
    }

    #[test]
    fn trakt_chart_type_parsing_and_display_remain_stable() {
        assert_eq!("trending".parse::<TraktChartType>().ok(), Some(TraktChartType::Trending));
        assert_eq!("POPULAR".parse::<TraktChartType>().ok(), Some(TraktChartType::Popular));
        assert_eq!(TraktChartType::Trending.to_string(), "trending");
        assert_eq!(TraktChartType::Popular.to_string(), "popular");
    }

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
