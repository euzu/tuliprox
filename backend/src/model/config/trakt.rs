use shared::model::{PlaylistItem, TraktApiConfigDto, TraktConfigDto, TraktContentType, TraktListConfigDto};
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
            fuzzy_match_threshold: dto.fuzzy_match_threshold
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
            fuzzy_match_threshold: instance.fuzzy_match_threshold
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktConfig {
    pub enabled: bool,
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
}

macros::from_impl!(TraktConfig);
impl From<&TraktConfigDto>  for TraktConfig {
    fn from(dto: &TraktConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfig::from(&dto.api),
            lists: dto.lists.iter().map(Into::into).collect(),
        }
    }
}
impl From<&TraktConfig>  for TraktConfigDto {
    fn from(dto: &TraktConfig) -> Self {
        Self {
            enabled: dto.enabled,
            api: TraktApiConfigDto::from(&dto.api),
            lists: dto.lists.iter().map(TraktListConfigDto::from).collect(),
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
