use crate::model::{macros, TraktApiConfig, TraktChartConfig, TraktConfig, TraktListConfig};
use shared::model::{
    CurationCatalogSelection, CurationConfigDto, TmdbCurationApiConfigDto, TmdbCurationConfigDto,
    TmdbTrendingConfigDto, TmdbTrendingKind, TmdbTrendingScope, TmdbTrendingTimeWindow, TraktSourceConfigDto,
};
use std::fmt;

#[derive(Debug, Clone)]
pub struct TraktSourceConfig {
    pub enabled: bool,
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
    pub charts: Vec<TraktChartConfig>,
}

macros::from_impl!(TraktSourceConfig);
impl From<&TraktSourceConfigDto> for TraktSourceConfig {
    fn from(dto: &TraktSourceConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            api: (&dto.api).into(),
            lists: dto.lists.iter().map(Into::into).collect(),
            charts: dto.charts.iter().map(Into::into).collect(),
        }
    }
}

impl From<&TraktSourceConfig> for TraktSourceConfigDto {
    fn from(config: &TraktSourceConfig) -> Self {
        Self {
            enabled: config.enabled,
            api: (&config.api).into(),
            lists: config.lists.iter().map(Into::into).collect(),
            charts: config.charts.iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone)]
pub struct TmdbCurationApiConfig {
    pub access_token: String,
}

impl fmt::Debug for TmdbCurationApiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmdbCurationApiConfig").field("access_token", &"[redacted]").finish()
    }
}

macros::from_impl!(TmdbCurationApiConfig);
impl From<&TmdbCurationApiConfigDto> for TmdbCurationApiConfig {
    fn from(dto: &TmdbCurationApiConfigDto) -> Self { Self { access_token: dto.access_token.clone() } }
}

impl From<&TmdbCurationApiConfig> for TmdbCurationApiConfigDto {
    fn from(config: &TmdbCurationApiConfig) -> Self { Self { access_token: config.access_token.clone() } }
}

#[derive(Debug, Clone)]
pub struct TmdbTrendingConfig {
    pub kind: TmdbTrendingKind,
    pub time_window: TmdbTrendingTimeWindow,
    pub scope: TmdbTrendingScope,
    pub category_name: Option<String>,
    pub create_xtream_category: bool,
}

macros::from_impl!(TmdbTrendingConfig);
impl From<&TmdbTrendingConfigDto> for TmdbTrendingConfig {
    fn from(dto: &TmdbTrendingConfigDto) -> Self {
        Self {
            kind: dto.kind,
            time_window: dto.time_window,
            scope: dto.scope,
            category_name: dto.category_name.clone(),
            create_xtream_category: dto.create_xtream_category,
        }
    }
}

impl From<&TmdbTrendingConfig> for TmdbTrendingConfigDto {
    fn from(config: &TmdbTrendingConfig) -> Self {
        Self {
            kind: config.kind,
            time_window: config.time_window,
            scope: config.scope,
            category_name: config.category_name.clone(),
            create_xtream_category: config.create_xtream_category,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TmdbCurationConfig {
    pub enabled: bool,
    pub api: TmdbCurationApiConfig,
    pub trending: Vec<TmdbTrendingConfig>,
}

macros::from_impl!(TmdbCurationConfig);
impl From<&TmdbCurationConfigDto> for TmdbCurationConfig {
    fn from(dto: &TmdbCurationConfigDto) -> Self {
        Self { enabled: dto.enabled, api: (&dto.api).into(), trending: dto.trending.iter().map(Into::into).collect() }
    }
}

impl From<&TmdbCurationConfig> for TmdbCurationConfigDto {
    fn from(config: &TmdbCurationConfig) -> Self {
        Self {
            enabled: config.enabled,
            api: (&config.api).into(),
            trending: config.trending.iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CurationConfig {
    pub enabled: bool,
    pub catalog_selection: CurationCatalogSelection,
    pub include_xtream_base_categories: bool,
    pub trakt: Option<TraktSourceConfig>,
    pub tmdb: Option<TmdbCurationConfig>,
}

macros::from_impl!(CurationConfig);
impl From<&CurationConfigDto> for CurationConfig {
    fn from(dto: &CurationConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            catalog_selection: dto.catalog_selection,
            include_xtream_base_categories: dto.include_xtream_base_categories,
            trakt: dto.trakt.as_ref().map(Into::into),
            tmdb: dto.tmdb.as_ref().map(Into::into),
        }
    }
}

impl From<&CurationConfig> for CurationConfigDto {
    fn from(config: &CurationConfig) -> Self {
        Self {
            enabled: config.enabled,
            catalog_selection: config.catalog_selection,
            include_xtream_base_categories: config.include_xtream_base_categories,
            trakt: config.trakt.as_ref().map(Into::into),
            tmdb: config.tmdb.as_ref().map(Into::into),
        }
    }
}

/// Execution-only normalization. Keep the original declaration when saving configuration.
impl From<&TraktConfig> for CurationConfig {
    fn from(legacy: &TraktConfig) -> Self {
        Self {
            enabled: legacy.enabled,
            catalog_selection: legacy.catalog_selection,
            include_xtream_base_categories: legacy.include_xtream_base_categories,
            trakt: Some(TraktSourceConfig {
                enabled: true,
                api: legacy.api.clone(),
                lists: legacy.lists.clone(),
                charts: legacy.charts.clone(),
            }),
            tmdb: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        ConfigTargetDto, TargetOutputDto, TraktApiConfigDto, TraktChartConfigDto, TraktChartKind, TraktChartType,
        TraktConfigDto, TraktContentType, TraktListConfigDto, XtreamTargetOutputDto,
    };

    fn legacy_config() -> TraktConfigDto {
        TraktConfigDto {
            api: TraktApiConfigDto {
                api_key: "client-id".to_string(),
                version: "2".to_string(),
                url: "https://api.trakt.tv".to_string(),
                user_agent: "custom-agent".to_string(),
            },
            lists: vec![TraktListConfigDto {
                user: "alice".to_string(),
                list_slug: "watchlist".to_string(),
                content_type: TraktContentType::Both,
                category_name: Some("Saved list label".to_string()),
                create_xtream_category: false,
                tmdb_only: false,
                fuzzy_match_threshold: 83,
            }],
            charts: vec![TraktChartConfigDto {
                kind: TraktChartKind::Shows,
                chart: TraktChartType::Popular,
                category_name: Some("Shows".to_string()),
                create_xtream_category: true,
                tmdb_only: true,
                fuzzy_match_threshold: 91,
            }],
            ..TraktConfigDto::default()
        }
    }

    #[test]
    fn curation_legacy_normalization_preserves_all_policy_axes_and_source_settings() {
        for enabled in [false, true] {
            for selection in [CurationCatalogSelection::Full, CurationCatalogSelection::Curated] {
                for include_base in [false, true] {
                    let mut legacy = legacy_config();
                    legacy.enabled = enabled;
                    legacy.catalog_selection = selection;
                    legacy.include_xtream_base_categories = include_base;
                    let mut canonical = CurationConfigDto {
                        enabled,
                        catalog_selection: selection,
                        include_xtream_base_categories: include_base,
                        trakt: Some(TraktSourceConfigDto {
                            enabled: true,
                            api: legacy.api.clone(),
                            lists: legacy.lists.clone(),
                            charts: legacy.charts.clone(),
                        }),
                        tmdb: None,
                    };
                    legacy.prepare().unwrap();
                    canonical.prepare(&[TargetOutputDto::Xtream(XtreamTargetOutputDto::default())]).unwrap();
                    let resolved_legacy = TraktConfig::from(&legacy);
                    let normalized = CurationConfig::from(&resolved_legacy);
                    assert_eq!(CurationConfigDto::from(&normalized), canonical);
                    assert_eq!(
                        TraktConfigDto::from(&resolved_legacy),
                        legacy,
                        "normalization does not rewrite ownership"
                    );
                }
            }
        }
    }

    #[test]
    fn curation_legacy_source_less_disabled_policy_is_not_lost_on_normalization() {
        for policy in [
            TraktConfigDto {
                enabled: false,
                catalog_selection: CurationCatalogSelection::Curated,
                ..TraktConfigDto::default()
            },
            TraktConfigDto { enabled: false, include_xtream_base_categories: false, ..TraktConfigDto::default() },
            TraktConfigDto::default(),
        ] {
            let normalized = CurationConfig::from(&TraktConfig::from(&policy));
            assert_eq!(normalized.enabled, policy.enabled);
            assert_eq!(normalized.catalog_selection, policy.catalog_selection);
            assert_eq!(normalized.include_xtream_base_categories, policy.include_xtream_base_categories);
            let source = normalized.trakt.unwrap();
            assert!(source.lists.is_empty());
            assert!(source.charts.is_empty());
            assert!(source.enabled, "legacy whole-block switch belongs to the target policy");
        }
    }

    #[test]
    fn curation_round_trip_preserves_source_switches_credentials_and_selection_only_labels() {
        let value = serde_json::json!({
            "enabled": false,
            "catalog_selection": "curated",
            "include_xtream_base_categories": false,
            "trakt": {
                "enabled": false,
                "api": {"api_key": "trakt-client", "url": "https://api.trakt.tv"},
                "charts": [{"kind": "movies", "chart": "trending", "category_name": "Trakt", "tmdb_only": true}]
            },
            "tmdb": {
                "api": {"access_token": "private-test-token"},
                "trending": [
                    {"kind": "movie", "time_window": "day", "scope": "first_page", "category_name": "Saved label", "create_xtream_category": false},
                    {"kind": "tv", "time_window": "week", "scope": "first_page", "category_name": "Shows"}
                ]
            }
        });
        let dto: CurationConfigDto = serde_json::from_value(value).unwrap();
        let resolved = CurationConfig::from(&dto);
        assert_eq!(CurationConfigDto::from(&resolved), dto);
        assert!(!format!("{resolved:?}").contains("private-test-token"));
        assert!(!format!("{dto:?}").contains("private-test-token"));
    }

    #[test]
    fn curation_round_trip_preserves_absent_sources_and_empty_blocks() {
        for value in
            [serde_json::json!({}), serde_json::json!({"trakt": {}}), serde_json::json!({"tmdb": {"enabled": false}})]
        {
            let dto: CurationConfigDto = serde_json::from_value(value).unwrap();
            assert_eq!(CurationConfigDto::from(&CurationConfig::from(&dto)), dto);
        }
    }

    #[test]
    fn curation_legacy_target_output_round_trip_keeps_its_original_declaration() {
        let mut dto = ConfigTargetDto {
            name: "discovery".to_string(),
            output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto {
                trakt: Some(legacy_config()),
                ..XtreamTargetOutputDto::default()
            })],
            ..ConfigTargetDto::default()
        };
        dto.prepare(1, None, None).unwrap();
        let resolved = crate::model::ConfigTarget::from(&dto);
        let outputs: Vec<TargetOutputDto> = resolved.output.iter().map(Into::into).collect();
        assert_eq!(outputs, dto.output);
        let serialized = serde_json::to_value(&outputs).unwrap();
        assert!(serialized[0].get("trakt").is_some());
        assert!(serialized[0].get("curation").is_none());
    }
}
