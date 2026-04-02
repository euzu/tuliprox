use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        default_download_dir, default_episode_pattern, default_supported_video_extensions, is_blank_optional_str,
        is_blank_optional_string, is_blank_or_default_download_dir, is_blank_or_default_episode_pattern,
        is_default_supported_video_extensions, is_false, DEFAULT_USER_AGENT,
    },
};
use std::{borrow::BorrowMut, collections::HashMap};

const fn default_retry_backoff_step_1_secs() -> u64 { 3 }
const fn default_retry_backoff_step_2_secs() -> u64 { 10 }
const fn default_retry_backoff_step_3_secs() -> u64 { 30 }
const fn default_retry_backoff_jitter_percent() -> u8 { 20 }
const fn default_retry_max_attempts() -> u8 { 5 }
fn is_default_retry_backoff_step_1_secs(value: &u64) -> bool { *value == default_retry_backoff_step_1_secs() }
fn is_default_retry_backoff_step_2_secs(value: &u64) -> bool { *value == default_retry_backoff_step_2_secs() }
fn is_default_retry_backoff_step_3_secs(value: &u64) -> bool { *value == default_retry_backoff_step_3_secs() }
fn is_default_retry_backoff_jitter_percent(value: &u8) -> bool { *value == default_retry_backoff_jitter_percent() }
fn is_default_retry_max_attempts(value: &u8) -> bool { *value == default_retry_max_attempts() }
fn is_zero_u8(value: &u8) -> bool { *value == 0 }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoDownloadConfigDto {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_download_dir", skip_serializing_if = "is_blank_or_default_download_dir")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub organize_into_directories: bool,
    #[serde(default = "default_episode_pattern", skip_serializing_if = "is_blank_or_default_episode_pattern")]
    pub episode_pattern: Option<String>,
    #[serde(default)]
    pub download_priority: i8,
    #[serde(default)]
    pub recording_priority: i8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub reserve_slots_for_users: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub max_background_per_provider: u8,
    #[serde(
        default = "default_retry_backoff_step_1_secs",
        skip_serializing_if = "is_default_retry_backoff_step_1_secs"
    )]
    pub retry_backoff_step_1_secs: u64,
    #[serde(
        default = "default_retry_backoff_step_2_secs",
        skip_serializing_if = "is_default_retry_backoff_step_2_secs"
    )]
    pub retry_backoff_step_2_secs: u64,
    #[serde(
        default = "default_retry_backoff_step_3_secs",
        skip_serializing_if = "is_default_retry_backoff_step_3_secs"
    )]
    pub retry_backoff_step_3_secs: u64,
    #[serde(
        default = "default_retry_backoff_jitter_percent",
        skip_serializing_if = "is_default_retry_backoff_jitter_percent"
    )]
    pub retry_backoff_jitter_percent: u8,
    #[serde(default = "default_retry_max_attempts", skip_serializing_if = "is_default_retry_max_attempts")]
    pub retry_max_attempts: u8,
}

impl VideoDownloadConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.organize_into_directories
            && self.headers.is_empty()
            && is_blank_or_default_download_dir(&self.directory)
            && is_blank_or_default_episode_pattern(&self.episode_pattern)
            && self.reserve_slots_for_users == 0
            && self.max_background_per_provider == 0
            && is_default_retry_backoff_step_1_secs(&self.retry_backoff_step_1_secs)
            && is_default_retry_backoff_step_2_secs(&self.retry_backoff_step_2_secs)
            && is_default_retry_backoff_step_3_secs(&self.retry_backoff_step_3_secs)
            && is_default_retry_backoff_jitter_percent(&self.retry_backoff_jitter_percent)
            && is_default_retry_max_attempts(&self.retry_max_attempts)
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
                    downl.directory = default_download_dir();
                } else if let Some(directory) = downl.directory.as_ref() {
                    downl.directory = Some(directory.trim().to_string());
                }

                if downl.headers.is_empty() {
                    downl.headers.borrow_mut().insert("Accept".to_string(), "video/*".to_string());
                    downl.headers.borrow_mut().insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());
                }

                if is_blank_or_default_episode_pattern(&downl.episode_pattern) {
                    downl.episode_pattern = default_episode_pattern();
                } else if let Some(episode_pattern) = downl.episode_pattern.as_ref() {
                    downl.episode_pattern = Some(episode_pattern.trim().to_string());
                }

                if let Some(episode_pattern) = &downl.episode_pattern {
                    if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(episode_pattern) {
                        return info_err_res!("can't parse regex: {episode_pattern} {err}");
                    }
                }

                downl.retry_backoff_step_1_secs = downl.retry_backoff_step_1_secs.max(1);
                downl.retry_backoff_step_2_secs = downl.retry_backoff_step_2_secs.max(1);
                downl.retry_backoff_step_3_secs = downl.retry_backoff_step_3_secs.max(1);
                downl.retry_backoff_jitter_percent = downl.retry_backoff_jitter_percent.min(95);
                downl.retry_max_attempts = downl.retry_max_attempts.max(1);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::DEFAULT_DOWNLOAD_DIR;

    #[test]
    fn prepare_sets_default_download_dir_when_missing() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                headers: HashMap::new(),
                directory: None,
                organize_into_directories: false,
                episode_pattern: None,
                download_priority: 0,
                recording_priority: 0,
                reserve_slots_for_users: 0,
                max_background_per_provider: 0,
                retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
                retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
                retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
                retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
                retry_max_attempts: default_retry_max_attempts(),
            }),
            web_search: None,
        };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.directory.as_deref(), Some(DEFAULT_DOWNLOAD_DIR));
    }

    #[test]
    fn prepare_sets_default_episode_pattern_when_missing() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                headers: HashMap::new(),
                directory: None,
                organize_into_directories: false,
                episode_pattern: None,
                download_priority: 0,
                recording_priority: 0,
                reserve_slots_for_users: 0,
                max_background_per_provider: 0,
                retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
                retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
                retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
                retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
                retry_max_attempts: default_retry_max_attempts(),
            }),
            web_search: None,
        };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert!(download.episode_pattern.is_some(), "expected default episode pattern to be set");
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
                download_priority: 0,
                recording_priority: 0,
                reserve_slots_for_users: 0,
                max_background_per_provider: 0,
                retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
                retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
                retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
                retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
                retry_max_attempts: default_retry_max_attempts(),
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
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
            retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
            retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
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
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
            retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
            retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
        };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"directory\""), "expected directory field for custom value, got: {serialized}");
    }

    #[test]
    fn serializing_skips_default_episode_pattern() {
        let download = VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: None,
            organize_into_directories: false,
            episode_pattern: default_episode_pattern(),
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
            retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
            retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
        };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(
            !serialized.contains("\"episode_pattern\""),
            "expected no episode_pattern field for default value, got: {serialized}"
        );
    }

    #[test]
    fn prepare_preserves_download_retry_backoff_settings() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                headers: HashMap::new(),
                directory: None,
                organize_into_directories: false,
                episode_pattern: None,
                download_priority: 0,
                recording_priority: 0,
                reserve_slots_for_users: 0,
                max_background_per_provider: 0,
                retry_backoff_step_1_secs: 3,
                retry_backoff_step_2_secs: 12,
                retry_backoff_step_3_secs: 45,
                retry_backoff_jitter_percent: 0,
                retry_max_attempts: 7,
            }),
            web_search: None,
        };

        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.retry_backoff_step_1_secs, 3);
        assert_eq!(download.retry_backoff_step_2_secs, 12);
        assert_eq!(download.retry_backoff_step_3_secs, 45);
        assert_eq!(download.retry_backoff_jitter_percent, 0);
        assert_eq!(download.retry_max_attempts, 7);
    }

    #[test]
    fn serializing_keeps_custom_download_retry_backoff_settings() {
        let download = VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: None,
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_step_1_secs: 4,
            retry_backoff_step_2_secs: 15,
            retry_backoff_step_3_secs: 60,
            retry_backoff_jitter_percent: 10,
            retry_max_attempts: 6,
        };

        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"retry_backoff_step_1_secs\":4"));
        assert!(serialized.contains("\"retry_backoff_step_2_secs\":15"));
        assert!(serialized.contains("\"retry_backoff_step_3_secs\":60"));
        assert!(serialized.contains("\"retry_backoff_jitter_percent\":10"));
        assert!(serialized.contains("\"retry_max_attempts\":6"));
    }

    #[test]
    fn serializing_keeps_scheduler_policy_settings() {
        let download = VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: None,
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 1,
            max_background_per_provider: 2,
            retry_backoff_step_1_secs: default_retry_backoff_step_1_secs(),
            retry_backoff_step_2_secs: default_retry_backoff_step_2_secs(),
            retry_backoff_step_3_secs: default_retry_backoff_step_3_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
        };

        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"reserve_slots_for_users\":1"));
        assert!(serialized.contains("\"max_background_per_provider\":2"));
    }
}
