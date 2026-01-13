use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;
use shared::model::{VideoConfigDto, VideoDownloadConfigDto};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct VideoDownloadConfig {
    pub headers: HashMap<String, String>,
    pub directory: String,
    pub organize_into_directories: bool,
    pub episode_pattern: Option<Arc<Regex>>,
}

macros::from_impl!(VideoDownloadConfig);
impl From<&VideoDownloadConfigDto> for VideoDownloadConfig {
    fn from(dto: &VideoDownloadConfigDto) -> Self {
        Self {
            headers: dto.headers.clone(),
            directory: dto.directory.as_ref().map_or_else(|| "downloads".to_string(), ToString::to_string),
            organize_into_directories: dto.organize_into_directories,
            episode_pattern: dto.episode_pattern.as_ref().and_then(|s| shared::model::REGEX_CACHE.get_or_compile(s)
                                    .map_err(|e| log::warn!("Invalid episode_pattern regex '{s}': {e}"))
                                    .ok()),
        }
    }
}

impl From<&VideoDownloadConfig> for VideoDownloadConfigDto {
    fn from(instance: &VideoDownloadConfig) -> Self {
        Self {
            headers: instance.headers.clone(),
            directory: Some(instance.directory.clone()),
            organize_into_directories: instance.organize_into_directories,
            episode_pattern: instance.episode_pattern.as_ref().map(std::string::ToString::to_string),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub extensions: Vec<String>,
    pub download: Option<VideoDownloadConfig>,
    pub web_search: Option<String>,
}
macros::from_impl!(VideoConfig);
impl From<&VideoConfigDto> for VideoConfig {
    fn from(dto: &VideoConfigDto) -> Self {
        Self {
            extensions: dto.extensions.clone(),
            download: dto.download.as_ref().map(Into::into),
            web_search: dto.web_search.clone(),
        }
    }
}

impl From<&VideoConfig> for VideoConfigDto {
    fn from(instance: &VideoConfig) -> Self {
        Self {
            extensions: instance.extensions.clone(),
            download: instance.download.as_ref().map(Into::into),
            web_search: instance.web_search.clone(),
        }
    }
}