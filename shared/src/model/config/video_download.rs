use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        default_supported_video_extensions, is_blank_optional_str, is_blank_optional_string,
        is_blank_or_default_download_dir, is_default_supported_video_extensions, is_false, DEFAULT_DOWNLOAD_DIR,
        DEFAULT_USER_AGENT,
    },
};
use std::{borrow::BorrowMut, collections::HashMap};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoDownloadConfigDto {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "is_blank_or_default_download_dir")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub organize_into_directories: bool,
    // TODO use ptt
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub episode_pattern: Option<String>,
}

impl VideoDownloadConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.organize_into_directories
            && self.headers.is_empty()
            && is_blank_optional_str(self.directory.as_deref())
            && is_blank_optional_str(self.episode_pattern.as_deref())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoConfigDto {
    #[serde(
        default = "default_supported_video_extensions",
        skip_serializing_if = "is_default_supported_video_extensions"
    )]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<VideoDownloadConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub web_search: Option<String>,
}

impl VideoConfigDto {
    pub fn is_empty(&self) -> bool {
        (self.extensions.is_empty() || is_default_supported_video_extensions(&self.extensions))
            && is_blank_optional_str(self.web_search.as_deref())
            && (self.download.is_none() || self.download.as_ref().is_some_and(|d| d.is_empty()))
    }

    pub fn clean(&mut self) {
        if self.download.as_ref().is_some_and(|d| d.is_empty()) {
            self.download = None;
        }
    }

    /// # Panics
    ///
    /// Will panic if default `RegEx` gets invalid
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.extensions.is_empty() {
            self.extensions = default_supported_video_extensions();
        }
        match &mut self.download {
            None => {}
            Some(downl) => {
                if is_blank_or_default_download_dir(&downl.directory) {
                    downl.directory = Some(DEFAULT_DOWNLOAD_DIR.to_string());
                } else if let Some(directory) = downl.directory.as_ref() {
                    downl.directory = Some(directory.trim().to_string());
                }

                if downl.headers.is_empty() {
                    downl.headers.borrow_mut().insert("Accept".to_string(), "video/*".to_string());
                    downl.headers.borrow_mut().insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());
                }

                if let Some(episode_pattern) = &downl.episode_pattern {
                    if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(episode_pattern) {
                        return info_err_res!("can't parse regex: {episode_pattern} {err}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_sets_default_download_dir_when_missing() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                headers: HashMap::new(),
                directory: None,
                organize_into_directories: false,
                episode_pattern: None,
            }),
            web_search: None,
        };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.directory.as_deref(), Some(DEFAULT_DOWNLOAD_DIR));
    }

    #[test]
    fn prepare_keeps_custom_download_dir() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                headers: HashMap::new(),
                directory: Some("custom-downloads".to_string()),
                organize_into_directories: false,
                episode_pattern: None,
            }),
            web_search: None,
        };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.directory.as_deref(), Some("custom-downloads"));
    }

    #[test]
    fn serializing_skips_default_download_dir() {
        let download = VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: Some(DEFAULT_DOWNLOAD_DIR.to_string()),
            organize_into_directories: false,
            episode_pattern: None,
        };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(
            !serialized.contains("\"directory\""),
            "expected no directory field for default value, got: {serialized}"
        );
    }

    #[test]
    fn serializing_keeps_custom_download_dir() {
        let download = VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: Some("custom-downloads".to_string()),
            organize_into_directories: false,
            episode_pattern: None,
        };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"directory\""), "expected directory field for custom value, got: {serialized}");
    }
}
