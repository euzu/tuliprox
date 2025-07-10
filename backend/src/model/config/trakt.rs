use shared::model::{PlaylistItem, TraktApiConfigDto, TraktConfigDto, TraktContentType, TraktListConfigDto};
use crate::model::config::trakt_api::TraktMatchItem;
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct TraktApiConfig {
    pub key: String,
    pub version: String,
    pub url: String,
}

macros::from_impl!(TraktApiConfig);
impl From<&TraktApiConfigDto> for TraktApiConfig {
    fn from(dto: &TraktApiConfigDto) -> Self {
        Self {
            key: dto.key.to_string(),
            version: dto.version.to_string(),
            url: dto.url.to_string(),
        }
    }
}

impl From<&TraktApiConfig> for TraktApiConfigDto {
    fn from(instance: &TraktApiConfig) -> Self {
        Self {
            key: instance.key.to_string(),
            version: instance.version.to_string(),
            url: instance.url.to_string(),
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
            user: dto.user.to_string(),
            list_slug: dto.list_slug.to_string(),
            category_name: dto.category_name.to_string(),
            content_type: dto.content_type,
            fuzzy_match_threshold: dto.fuzzy_match_threshold
        }
    }
}

impl From<&TraktListConfig> for TraktListConfigDto {
    fn from(instance: &TraktListConfig) -> Self {
        Self {
            user: instance.user.to_string(),
            list_slug: instance.list_slug.to_string(),
            category_name: instance.category_name.to_string(),
            content_type: instance.content_type,
            fuzzy_match_threshold: instance.fuzzy_match_threshold
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraktConfig {
    pub api: TraktApiConfig,
    pub lists: Vec<TraktListConfig>,
}

macros::from_impl!(TraktConfig);
impl From<&TraktConfigDto>  for TraktConfig {
    fn from(dto: &TraktConfigDto) -> Self {
        Self {
            api: TraktApiConfig::from(&dto.api),
            lists: dto.lists.iter().map(Into::into).collect(),
        }
    }
}
impl From<&TraktConfig>  for TraktConfigDto {
    fn from(dto: &TraktConfig) -> Self {
        Self {
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
