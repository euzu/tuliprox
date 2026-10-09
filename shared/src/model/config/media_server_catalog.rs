use crate::{
    defaults::{
        default_as_true, default_media_server_catalog_page_size, default_media_server_catalog_request_delay_ms,
        is_default_media_server_catalog_page_size, is_default_media_server_catalog_request_delay_ms, is_false, is_true,
    },
    error::TuliproxError,
    utils::{deserialize_as_option_string, get_trimmed_string, is_blank_optional_string, is_non_blank_optional_string},
};
use std::sync::Arc;

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MediaServerCatalogRefreshMode {
    #[default]
    Manual,
    Scheduled,
}

pub fn is_default_media_server_catalog_refresh_mode(value: &MediaServerCatalogRefreshMode) -> bool {
    *value == MediaServerCatalogRefreshMode::default()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerCatalogConfigDto {
    #[serde(default, skip_serializing_if = "is_default_media_server_catalog_refresh_mode")]
    pub refresh_mode: MediaServerCatalogRefreshMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub refresh_on_startup: bool,
    #[serde(
        default = "default_media_server_catalog_page_size",
        skip_serializing_if = "is_default_media_server_catalog_page_size"
    )]
    pub page_size: u16,
    #[serde(
        default = "default_media_server_catalog_request_delay_ms",
        skip_serializing_if = "is_default_media_server_catalog_request_delay_ms"
    )]
    pub request_delay_ms: u64,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub include_media_sources: bool,
    #[serde(default, alias = "include_file_paths", skip_serializing_if = "is_false")]
    pub include_paths: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_user_state: bool,
}

impl Default for MediaServerCatalogConfigDto {
    fn default() -> Self {
        Self {
            refresh_mode: MediaServerCatalogRefreshMode::default(),
            refresh_on_startup: false,
            page_size: default_media_server_catalog_page_size(),
            request_delay_ms: default_media_server_catalog_request_delay_ms(),
            include_media_sources: default_as_true(),
            include_paths: false,
            include_user_state: false,
        }
    }
}

impl MediaServerCatalogConfigDto {
    pub fn is_default(&self) -> bool { self == &Self::default() }

    pub fn prepare(&self, input_name: &Arc<str>) -> Result<(), TuliproxError> {
        if self.page_size == 0 {
            return Err(TuliproxError::ConfigInput(format!(
                "media server catalog page_size must be greater than zero (input: {input_name})"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaServerPlaybackInfoPolicy {
    #[default]
    OnDemand,
    Disabled,
}

pub fn is_default_media_server_playback_info_policy(value: &MediaServerPlaybackInfoPolicy) -> bool {
    *value == MediaServerPlaybackInfoPolicy::default()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerPlaybackConfigDto {
    #[serde(default, skip_serializing_if = "is_default_media_server_playback_info_policy")]
    pub playback_info_policy: MediaServerPlaybackInfoPolicy,
    #[serde(default, skip_serializing_if = "is_false")]
    pub preflight_streams: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub direct_play_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_transcode: bool,
}

impl Default for MediaServerPlaybackConfigDto {
    fn default() -> Self {
        Self {
            playback_info_policy: MediaServerPlaybackInfoPolicy::default(),
            preflight_streams: false,
            direct_play_only: default_as_true(),
            allow_transcode: false,
        }
    }
}

impl MediaServerPlaybackConfigDto {
    pub fn is_default(&self) -> bool { self == &Self::default() }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaServerImagePolicy {
    #[default]
    ProxyOnDemand,
    Disabled,
}

pub fn is_default_media_server_image_policy(value: &MediaServerImagePolicy) -> bool {
    *value == MediaServerImagePolicy::default()
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaServerLibraryKind {
    Movies,
    #[serde(alias = "shows", alias = "series")]
    TvShows,
    Unsupported,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerLibrarySelectorDetailsDto {
    #[serde(
        default,
        deserialize_with = "deserialize_as_option_string",
        skip_serializing_if = "is_blank_optional_string"
    )]
    pub id: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_as_option_string",
        skip_serializing_if = "is_blank_optional_string"
    )]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MediaServerLibraryKind>,
}

impl MediaServerLibrarySelectorDetailsDto {
    fn prepare(&mut self) {
        self.id = get_trimmed_string(self.id.as_deref());
        self.key = get_trimmed_string(self.key.as_deref());
        self.name = get_trimmed_string(self.name.as_deref());
    }

    fn is_empty(&self) -> bool {
        self.id.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.key.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.name.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.kind.is_none()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MediaServerLibrarySelector {
    Name(String),
    Detailed(MediaServerLibrarySelectorDetailsDto),
}

impl MediaServerLibrarySelector {
    fn prepare(&mut self) {
        match self {
            Self::Name(name) => *name = name.trim().to_string(),
            Self::Detailed(details) => details.prepare(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Name(name) => name.trim().is_empty(),
            Self::Detailed(details) => details.is_empty(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerInputConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<MediaServerLibrarySelector>,
    #[serde(default, skip_serializing_if = "MediaServerCatalogConfigDto::is_default")]
    pub catalog: MediaServerCatalogConfigDto,
    #[serde(default, skip_serializing_if = "MediaServerPlaybackConfigDto::is_default")]
    pub playback: MediaServerPlaybackConfigDto,
    #[serde(default, skip_serializing_if = "is_default_media_server_image_policy")]
    pub image_policy: MediaServerImagePolicy,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub account_token: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub server_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub server_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub prefer_https: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_relay: bool,
}

impl Default for MediaServerInputConfigDto {
    fn default() -> Self {
        Self {
            libraries: Vec::new(),
            catalog: MediaServerCatalogConfigDto::default(),
            playback: MediaServerPlaybackConfigDto::default(),
            image_policy: MediaServerImagePolicy::default(),
            token: None,
            api_key: None,
            user_id: None,
            account_token: None,
            server_id: None,
            server_name: None,
            prefer_https: default_as_true(),
            allow_relay: false,
        }
    }
}

impl MediaServerInputConfigDto {
    pub fn normalize(&mut self) {
        self.token = get_trimmed_string(self.token.as_deref());
        self.api_key = get_trimmed_string(self.api_key.as_deref());
        self.user_id = get_trimmed_string(self.user_id.as_deref());
        self.account_token = get_trimmed_string(self.account_token.as_deref());
        self.server_id = get_trimmed_string(self.server_id.as_deref());
        self.server_name = get_trimmed_string(self.server_name.as_deref());

        for library in &mut self.libraries {
            library.prepare();
        }
    }

    pub fn prepare(&mut self, input_name: &Arc<str>) -> Result<(), TuliproxError> {
        self.normalize();
        self.catalog.prepare(input_name)?;

        if self.libraries.iter().any(MediaServerLibrarySelector::is_empty) {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server library selectors must not be empty (input: {input_name})"
            )));
        }
        Ok(())
    }

    pub fn has_any_emby_jellyfin_auth(&self) -> bool {
        is_non_blank_optional_string(&self.token) || is_non_blank_optional_string(&self.api_key)
    }

    pub fn has_any_plex_token(&self) -> bool {
        is_non_blank_optional_string(&self.account_token) || is_non_blank_optional_string(&self.token)
    }

    pub fn has_plex_server_selector(&self) -> bool {
        is_non_blank_optional_string(&self.server_id) || is_non_blank_optional_string(&self.server_name)
    }
}

#[cfg(test)]
mod tests;
