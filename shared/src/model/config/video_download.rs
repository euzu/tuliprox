use std::collections::HashMap;
use std::borrow::BorrowMut;
use crate::info_err_res;
use crate::error::{TuliproxError};
use crate::model::DEFAULT_USER_AGENT;
use crate::utils::{is_false, is_blank_optional_string, is_blank_optional_str, default_supported_video_extensions, is_default_supported_video_extensions};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoDownloadConfigDto {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
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
    #[serde(default = "default_supported_video_extensions", skip_serializing_if = "is_default_supported_video_extensions")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<VideoDownloadConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub web_search: Option<String>,
}

impl VideoConfigDto {
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty() && is_blank_optional_str(self.web_search.as_deref())
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
                if downl.headers.is_empty() {
                    downl.headers.borrow_mut().insert("Accept".to_string(), "video/*".to_string());
                    downl.headers.borrow_mut().insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());
                }

                if let Some(episode_pattern) = &downl.episode_pattern {
                    if let Err(err) = regex::Regex::new(episode_pattern) {
                         return info_err_res!("cant parse regex: {episode_pattern} {err}");
                    }
                }
            }
        }
        Ok(())
    }
}