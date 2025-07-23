use std::collections::HashMap;
use std::borrow::BorrowMut;
use crate::create_tuliprox_error_result;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::DEFAULT_USER_AGENT;

pub const DEFAULT_VIDEO_EXTENSIONS: [&str; 6] = ["mkv", "avi", "mp4", "mpeg", "divx", "mov"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoDownloadConfigDto {
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default)]
    pub organize_into_directories: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_pattern: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoConfigDto {
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<VideoDownloadConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<String>,
}


impl VideoConfigDto {
    /// # Panics
    ///
    /// Will panic if default `RegEx` gets invalid
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.extensions.is_empty() {
            self.extensions = DEFAULT_VIDEO_EXTENSIONS
                .iter()
                .map(|&arg| arg.to_string())
                .collect();
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
                         return create_tuliprox_error_result!(TuliproxErrorKind::Info, "cant parse regex: {episode_pattern} {err}");
                    }
                }
            }
        }
        Ok(())
    }
}