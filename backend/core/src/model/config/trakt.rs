use crate::model::macros;
use shared::model::{
    TraktApiConfigDto, TraktChartConfigDto, TraktChartKind, TraktChartType, TraktConfigDto, TraktContentType,
    TraktListConfigDto,
};

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
pub struct TraktConfig {
    pub enabled: bool,
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
    pub charts: Vec<TraktChartConfig>,
}

macros::from_impl!(TraktConfig);
impl From<&TraktConfigDto> for TraktConfig {
    fn from(dto: &TraktConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfig::from(&dto.api),
            lists: dto.lists.iter().map(Into::into).collect(),
            charts: dto.charts.iter().map(Into::into).collect(),
        }
    }
}
impl From<&TraktConfig> for TraktConfigDto {
    fn from(dto: &TraktConfig) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfigDto::from(&dto.api),
            lists: dto.lists.iter().map(TraktListConfigDto::from).collect(),
            charts: dto.charts.iter().map(TraktChartConfigDto::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_trakt_config_round_trips_through_the_compatible_dto() {
        let dto = TraktConfigDto {
            enabled: true,
            api: TraktApiConfigDto {
                api_key: "client-id".to_string(),
                version: "2".to_string(),
                url: "https://api.trakt.tv".to_string(),
                user_agent: "agent".to_string(),
            },
            lists: vec![TraktListConfigDto {
                user: "alice".to_string(),
                list_slug: "watchlist".to_string(),
                category_name: "Watchlist".to_string(),
                content_type: TraktContentType::Vod,
                tmdb_only: false,
                fuzzy_match_threshold: 80,
            }],
            charts: vec![TraktChartConfigDto {
                kind: TraktChartKind::Shows,
                chart: TraktChartType::Popular,
                category_name: "Popular Shows".to_string(),
                tmdb_only: true,
                fuzzy_match_threshold: 90,
            }],
        };

        let resolved = TraktConfig::from(&dto);

        assert_eq!(TraktConfigDto::from(&resolved), dto);
    }
}
