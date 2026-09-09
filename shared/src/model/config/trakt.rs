use crate::{
    defaults::{
        default_as_true, default_trakt_fuzzy_threshold, is_false, is_true, DEFAULT_USER_AGENT, TRAKT_API_URL,
        TRAKT_API_VERSION,
    },
    error::TuliproxError,
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

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TraktCatalogSelection {
    #[default]
    Full,
    Curated,
}

impl TraktCatalogSelection {
    pub const fn is_full(&self) -> bool { matches!(self, Self::Full) }

    pub const fn is_curated(self) -> bool { matches!(self, Self::Curated) }
}

impl TraktApiConfigDto {
    pub fn prepare(&mut self) {
        self.api_key = self.api_key.trim().to_string();
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub create_xtream_category: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub create_xtream_category: bool,
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
            category_name: None,
            create_xtream_category: true,
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
            category_name: None,
            create_xtream_category: true,
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
    #[serde(default, skip_serializing_if = "TraktCatalogSelection::is_full")]
    pub catalog_selection: TraktCatalogSelection,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub include_xtream_base_categories: bool,
    #[serde(default)]
    pub api: TraktApiConfigDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lists: Vec<TraktListConfigDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub charts: Vec<TraktChartConfigDto>,
}

impl Default for TraktConfigDto {
    fn default() -> Self {
        Self {
            enabled: true,
            catalog_selection: TraktCatalogSelection::Full,
            include_xtream_base_categories: true,
            api: TraktApiConfigDto::default(),
            lists: Vec::new(),
            charts: Vec::new(),
        }
    }
}

fn prepare_selector_category(
    category_name: &mut Option<String>,
    create_xtream_category: bool,
    validate: bool,
    selector_name: &str,
) -> Result<(), TuliproxError> {
    *category_name = category_name.take().map(|name| name.trim().to_string()).filter(|name| !name.is_empty());
    if validate && create_xtream_category && category_name.is_none() {
        return Err(TuliproxError::Config(format!(
            "Trakt {selector_name} category_name is required when create_xtream_category is true"
        )));
    }
    Ok(())
}

impl TraktConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.api.prepare();
        for selector in &mut self.lists {
            prepare_selector_category(
                &mut selector.category_name,
                selector.create_xtream_category,
                self.enabled,
                "list",
            )?;
        }
        for selector in &mut self.charts {
            prepare_selector_category(
                &mut selector.category_name,
                selector.create_xtream_category,
                self.enabled,
                "chart",
            )?;
        }
        if self.enabled
            && self.lists.is_empty()
            && self.charts.is_empty()
            && (self.catalog_selection != TraktCatalogSelection::Full || !self.include_xtream_base_categories)
        {
            return Err(TuliproxError::Config(
                "Enabled Trakt curation with non-default policy requires at least one list or chart".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trakt_api_prepare_keeps_blank_client_id_absent() {
        for client_id in ["", " \t\r\n "] {
            let mut config = TraktApiConfigDto { api_key: client_id.to_string(), ..TraktApiConfigDto::default() };

            config.prepare();

            assert!(config.api_key.is_empty(), "blank Client ID should remain absent");
        }
    }

    #[test]
    fn trakt_api_prepare_trims_supplied_client_id() {
        let mut config =
            TraktApiConfigDto { api_key: "  user-supplied-client-id  ".to_string(), ..TraktApiConfigDto::default() };

        config.prepare();

        assert_eq!(config.api_key, "user-supplied-client-id");
    }

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

        assert_eq!(config.catalog_selection, TraktCatalogSelection::Full);
        assert!(config.include_xtream_base_categories);
        assert!(config.lists.is_empty());
        assert_eq!(config.charts.len(), 1);
        assert_eq!(config.charts[0].kind, TraktChartKind::Movies);
        assert_eq!(config.charts[0].kind.content_type(), TraktContentType::Vod);
        assert_eq!(config.charts[0].chart, TraktChartType::Trending);
        assert_eq!(config.charts[0].category_name.as_deref(), Some("Trending Movies"));
        assert!(config.charts[0].create_xtream_category);
        assert_eq!(config.charts[0].fuzzy_match_threshold, default_trakt_fuzzy_threshold());
    }

    #[test]
    fn selector_category_is_optional_and_projection_switches_are_independent() {
        let mut config = serde_json::from_str::<TraktConfigDto>(
            r#"{"catalog_selection":"curated","include_xtream_base_categories":false,"lists":[{"user":"alice","list_slug":"watchlist","content_type":"both","create_xtream_category":false}]}"#,
        )
        .expect("selection-only Trakt selector should deserialize");

        config.prepare().expect("selection-only selector should prepare");
        assert_eq!(config.catalog_selection, TraktCatalogSelection::Curated);
        assert!(!config.include_xtream_base_categories);
        assert_eq!(config.lists.len(), 1);
        assert!(!config.lists[0].create_xtream_category);
        assert!(config.lists[0].category_name.is_none());
    }

    #[test]
    fn category_name_is_conditionally_required_during_preparation() {
        let mut required = serde_json::from_str::<TraktConfigDto>(
            r#"{"lists":[{"user":"alice","list_slug":"watchlist","content_type":"both"}]}"#,
        )
        .expect("selector DTO");
        assert!(required.prepare().is_err());

        required.lists[0].create_xtream_category = false;
        assert!(required.prepare().is_ok());

        let mut chart = serde_json::from_str::<TraktConfigDto>(r#"{"charts":[{"kind":"movies","chart":"trending"}]}"#)
            .expect("chart selector DTO");
        assert!(chart.prepare().is_err());
        chart.charts[0].create_xtream_category = false;
        assert!(chart.prepare().is_ok());
    }

    #[test]
    fn disabled_config_can_retain_incomplete_selector_edits() {
        let mut config = serde_json::from_str::<TraktConfigDto>(
            r#"{"enabled":false,"lists":[{"user":"alice","list_slug":"watchlist","content_type":"both"}]}"#,
        )
        .expect("disabled selector DTO");

        assert!(config.prepare().is_ok());
        assert!(config.lists[0].category_name.is_none());
        assert!(config.lists[0].create_xtream_category);
    }

    #[test]
    fn source_less_non_default_policy_is_rejected_only_when_enabled() {
        let mut enabled =
            TraktConfigDto { catalog_selection: TraktCatalogSelection::Curated, ..TraktConfigDto::default() };
        assert!(enabled.prepare().is_err());

        enabled.enabled = false;
        assert!(enabled.prepare().is_ok());
    }

    #[test]
    fn old_yaml_shape_keeps_case_a_defaults_without_serializing_new_fields() {
        let config = serde_json::from_str::<TraktConfigDto>(
            r#"{"charts":[{"kind":"movies","chart":"trending","category_name":"Trending"}]}"#,
        )
        .expect("old config shape");
        let serialized = serde_json::to_value(&config).expect("serialize compatible config");

        assert_eq!(config.catalog_selection, TraktCatalogSelection::Full);
        assert!(config.include_xtream_base_categories);
        assert!(config.charts[0].create_xtream_category);
        assert!(serialized.get("catalog_selection").is_none());
        assert!(serialized.get("include_xtream_base_categories").is_none());
        assert!(serialized["charts"][0].get("create_xtream_category").is_none());
    }

    #[test]
    fn yaml_accepts_explicit_a_through_d_policy_controls() {
        let cases = [
            ("A", "full", true, true),
            ("B", "curated", true, true),
            ("C", "curated", true, false),
            ("D", "curated", false, true),
        ];

        for (case, selection, include_base, create_category) in cases {
            let yaml = format!(
                "catalog_selection: {selection}\ninclude_xtream_base_categories: {include_base}\nlists:\n  - user: alice\n    list_slug: watchlist\n    category_name: Watchlist\n    create_xtream_category: {create_category}\n    content_type: vod\n"
            );
            let mut config = serde_saphyr::from_str::<TraktConfigDto>(&yaml).expect("A-D YAML should deserialize");
            config.prepare().expect("A-D YAML should prepare");

            assert_eq!(config.catalog_selection.to_string(), selection, "case {case}");
            assert_eq!(config.include_xtream_base_categories, include_base, "case {case}");
            assert_eq!(config.lists[0].create_xtream_category, create_category, "case {case}");
        }
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
