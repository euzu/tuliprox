use shared::model::{
    PlaylistItem, TraktApiConfigDto, TraktChartConfigDto, TraktChartKind, TraktChartType, TraktConfigDto,
    TraktContentType, TraktListConfigDto,
};
use crate::model::config::trakt_api::TraktMatchItem;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct TraktApiConfig {
    pub api_key: String,
    pub version: String,
    pub url: String,
    pub user_agent: String,
}

macros::from_impl!(TraktApiConfig);
impl From<&TraktApiConfigDto> for TraktApiConfig {
    fn from(dto: &TraktApiConfigDto) -> Self {
        Self {
            api_key: dto.api_key.clone(),
            version: dto.version.clone(),
            url: dto.url.clone(),
            user_agent: dto.user_agent.clone(),
        }
    }
}

impl From<&TraktApiConfig> for TraktApiConfigDto {
    fn from(instance: &TraktApiConfig) -> Self {
        Self {
            api_key: instance.api_key.clone(),
            version: instance.version.clone(),
            url: instance.url.clone(),
            user_agent: instance.user_agent.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktListConfig {
    pub user: String,
    pub list_slug: String,
    pub category_name: String,
    pub content_type: TraktContentType,
    pub tmdb_only: bool,
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

macros::from_impl!(TraktListConfig);
impl From<&TraktListConfigDto> for TraktListConfig {
    fn from(dto: &TraktListConfigDto) -> Self {
        Self {
            user: dto.user.clone(),
            list_slug: dto.list_slug.clone(),
            category_name: dto.category_name.clone(),
            content_type: dto.content_type,
            tmdb_only: dto.tmdb_only,
            fuzzy_match_threshold: dto.fuzzy_match_threshold,
        }
    }
}

impl From<&TraktListConfig> for TraktListConfigDto {
    fn from(instance: &TraktListConfig) -> Self {
        Self {
            user: instance.user.clone(),
            list_slug: instance.list_slug.clone(),
            category_name: instance.category_name.clone(),
            content_type: instance.content_type,
            tmdb_only: instance.tmdb_only,
            fuzzy_match_threshold: instance.fuzzy_match_threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktChartConfig {
    pub kind: TraktChartKind,
    pub chart: TraktChartType,
    pub category_name: String,
    pub tmdb_only: bool,
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

macros::from_impl!(TraktChartConfig);
impl From<&TraktChartConfigDto> for TraktChartConfig {
    fn from(dto: &TraktChartConfigDto) -> Self {
        Self {
            kind: dto.kind,
            chart: dto.chart,
            category_name: dto.category_name.clone(),
            tmdb_only: dto.tmdb_only,
            fuzzy_match_threshold: dto.fuzzy_match_threshold,
        }
    }
}

impl From<&TraktChartConfig> for TraktChartConfigDto {
    fn from(instance: &TraktChartConfig) -> Self {
        Self {
            kind: instance.kind,
            chart: instance.chart,
            category_name: instance.category_name.clone(),
            tmdb_only: instance.tmdb_only,
            fuzzy_match_threshold: instance.fuzzy_match_threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktCategoryConfig {
    pub category_name: String,
    pub content_type: TraktContentType,
    pub tmdb_only: bool,
    pub fuzzy_match_threshold: u8, // Percentage (0-100)
}

impl From<&TraktListConfig> for TraktCategoryConfig {
    fn from(config: &TraktListConfig) -> Self {
        Self {
            category_name: config.category_name.clone(),
            content_type: config.content_type,
            tmdb_only: config.tmdb_only,
            fuzzy_match_threshold: config.fuzzy_match_threshold,
        }
    }
}

impl From<&TraktChartConfig> for TraktCategoryConfig {
    fn from(config: &TraktChartConfig) -> Self {
        Self {
            category_name: config.category_name.clone(),
            content_type: config.kind.content_type(),
            tmdb_only: config.tmdb_only,
            fuzzy_match_threshold: config.fuzzy_match_threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktConfig {
    pub enabled: bool,
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
    pub charts: Vec<TraktChartConfig>,
}

macros::from_impl!(TraktConfig);
impl From<&TraktConfigDto>  for TraktConfig {
    fn from(dto: &TraktConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfig::from(&dto.api),
            lists: dto.lists.iter().map(Into::into).collect(),
            charts: dto.charts.iter().map(Into::into).collect(),
        }
    }
}
impl From<&TraktConfig>  for TraktConfigDto {
    fn from(dto: &TraktConfig) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfigDto::from(&dto.api),
            lists: dto.lists.iter().map(TraktListConfigDto::from).collect(),
            charts: dto.charts.iter().map(TraktChartConfigDto::from).collect(),
        }
    }
}

// Matching results
#[derive(Debug, Clone)]
pub struct TraktMatchResult<'a> {
    pub playlist_item: &'a PlaylistItem,
    pub trakt_item: &'a TraktMatchItem<'a>,
    pub match_score: f64,
    // pub match_type: MatchType,
}
