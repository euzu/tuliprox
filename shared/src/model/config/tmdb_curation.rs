use super::curation::prepare_selector_category;
use crate::{
    defaults::{default_as_true, is_true},
    error::TuliproxError,
    model::XtreamCluster,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// TMDB application Read Access Token. Serialization is lossless; diagnostics are not.
#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbCurationApiConfigDto {
    #[serde(default)]
    pub access_token: String,
}

impl fmt::Debug for TmdbCurationApiConfigDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmdbCurationApiConfigDto").field("access_token", &"[redacted]").finish()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TmdbTrendingKind {
    Movie,
    Tv,
}

impl TmdbTrendingKind {
    pub const fn xtream_cluster(self) -> XtreamCluster {
        match self {
            Self::Movie => XtreamCluster::Video,
            Self::Tv => XtreamCluster::Series,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TmdbTrendingTimeWindow {
    Day,
    Week,
}

/// The complete requested subset, not a promise to exhaust the origin ranking.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TmdbTrendingScope {
    FirstPage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbTrendingConfigDto {
    pub kind: TmdbTrendingKind,
    pub time_window: TmdbTrendingTimeWindow,
    pub scope: TmdbTrendingScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub create_xtream_category: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbCurationConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api: TmdbCurationApiConfigDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trending: Vec<TmdbTrendingConfigDto>,
}

impl Default for TmdbCurationConfigDto {
    fn default() -> Self { Self { enabled: true, api: TmdbCurationApiConfigDto::default(), trending: Vec::new() } }
}

impl TmdbCurationConfigDto {
    pub(super) fn prepare(&mut self, curation_enabled: bool) -> Result<(), TuliproxError> {
        self.api.access_token = self.api.access_token.trim().to_string();
        for selector in &mut self.trending {
            prepare_selector_category(
                &mut selector.category_name,
                selector.create_xtream_category,
                curation_enabled && self.enabled,
                "TMDB trending",
            )?;
        }
        Ok(())
    }
}
