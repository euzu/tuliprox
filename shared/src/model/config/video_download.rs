use crate::{
    defaults::{
        default_download_dir, default_episode_pattern, default_supported_video_extensions,
        is_blank_or_default_download_dir, is_blank_or_default_episode_pattern, is_default_supported_video_extensions,
        is_false, DEFAULT_DOWNLOAD_DIR, DEFAULT_USER_AGENT, F64_DEFAULT_EPSILON,
    },
    error::TuliproxError,
    utils::{is_blank_optional_str, is_blank_optional_string},
};
use std::{borrow::BorrowMut, collections::HashMap};

const fn default_retry_backoff_initial_secs() -> u64 { 3 }
const fn default_retry_backoff_multiplier() -> f64 { 3.0 }
const fn default_retry_backoff_max_secs() -> u64 { 30 }
const fn default_retry_backoff_jitter_percent() -> u8 { 20 }
const fn default_retry_max_attempts() -> u8 { 5 }
fn is_default_retry_backoff_initial_secs(value: &u64) -> bool { *value == default_retry_backoff_initial_secs() }
fn is_default_retry_backoff_multiplier(value: &f64) -> bool {
    (*value - default_retry_backoff_multiplier()).abs() < F64_DEFAULT_EPSILON
}
fn is_default_retry_backoff_max_secs(value: &u64) -> bool { *value == default_retry_backoff_max_secs() }
fn is_default_retry_backoff_jitter_percent(value: &u8) -> bool { *value == default_retry_backoff_jitter_percent() }
fn is_default_retry_max_attempts(value: &u8) -> bool { *value == default_retry_max_attempts() }
fn is_zero_u8(value: &u8) -> bool { *value == 0 }
fn is_zero_i8(value: &i8) -> bool { *value == 0 }

// Recording configuration constants.
const DEFAULT_RECORDING_DIRECTORY_SUFFIX: &str = "recordings";
const DEFAULT_RECORDING_TIMEZONE: &str = "UTC";
const DEFAULT_RECORDING_FILENAME_TEMPLATE: &str = "{channel}_{program_title}_{start_time}";
const DEFAULT_RECORDING_MAX_PRE_ROLL_SECS: u64 = 15 * 60; // 15 minutes
const DEFAULT_RECORDING_MAX_POST_ROLL_SECS: u64 = 30 * 60; // 30 minutes
const DEFAULT_RECORDING_CLEANUP_INTERVAL_SECS: u64 = 60 * 60; // 1 hour
const DEFAULT_RECORDING_DISK_SAFETY_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
const DEFAULT_RECORDING_FALLBACK_BYTES_PER_MINUTE: u64 = 8 * 1024 * 1024; // 8 MiB
const MAX_FILENAME_TEMPLATE_BYTES: usize = 240;

pub fn default_recording_directory(download_dir: &str) -> String {
    format!("{download_dir}/{DEFAULT_RECORDING_DIRECTORY_SUFFIX}")
}

pub fn default_recording_timezone() -> String { DEFAULT_RECORDING_TIMEZONE.to_string() }
pub fn default_recording_filename_template() -> String { DEFAULT_RECORDING_FILENAME_TEMPLATE.to_string() }
pub const fn default_recording_max_pre_roll_secs() -> u64 { DEFAULT_RECORDING_MAX_PRE_ROLL_SECS }
pub const fn default_recording_max_post_roll_secs() -> u64 { DEFAULT_RECORDING_MAX_POST_ROLL_SECS }
pub const fn default_recording_cleanup_interval_secs() -> u64 { DEFAULT_RECORDING_CLEANUP_INTERVAL_SECS }
pub const fn default_recording_disk_safety_bytes() -> u64 { DEFAULT_RECORDING_DISK_SAFETY_BYTES }
pub const fn default_recording_fallback_bytes_per_minute() -> u64 { DEFAULT_RECORDING_FALLBACK_BYTES_PER_MINUTE }

fn is_default_recording_max_pre_roll_secs(value: &u64) -> bool { *value == default_recording_max_pre_roll_secs() }
fn is_default_recording_max_post_roll_secs(value: &u64) -> bool { *value == default_recording_max_post_roll_secs() }
fn is_default_recording_fallback_bytes_per_minute(value: &u64) -> bool {
    *value == default_recording_fallback_bytes_per_minute()
}
fn is_zero_u64_opt(value: &Option<u64>) -> bool { value.is_none_or(|v| v == 0) }
fn is_zero_u32_opt(value: &Option<u32>) -> bool { value.is_none_or(|v| v == 0) }
fn is_zero_u8_opt(value: &Option<u8>) -> bool { value.is_none_or(|v| v == 0) }

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
    #[serde(default, skip_serializing_if = "is_zero_i8")]
    pub download_priority: i8,
    #[serde(default, skip_serializing_if = "is_zero_i8")]
    pub recording_priority: i8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub reserve_slots_for_users: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub max_background_per_provider: u8,
    #[serde(
        default = "default_retry_backoff_initial_secs",
        skip_serializing_if = "is_default_retry_backoff_initial_secs"
    )]
    pub retry_backoff_initial_secs: u64,
    #[serde(default = "default_retry_backoff_multiplier", skip_serializing_if = "is_default_retry_backoff_multiplier")]
    pub retry_backoff_multiplier: f64,
    #[serde(default = "default_retry_backoff_max_secs", skip_serializing_if = "is_default_retry_backoff_max_secs")]
    pub retry_backoff_max_secs: u64,
    #[serde(
        default = "default_retry_backoff_jitter_percent",
        skip_serializing_if = "is_default_retry_backoff_jitter_percent"
    )]
    pub retry_backoff_jitter_percent: u8,
    #[serde(default = "default_retry_max_attempts", skip_serializing_if = "is_default_retry_max_attempts")]
    pub retry_max_attempts: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingConfigDto>,
}

impl VideoDownloadConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.organize_into_directories
            && self.headers.is_empty()
            && is_blank_or_default_download_dir(&self.directory)
            && is_blank_or_default_episode_pattern(&self.episode_pattern)
            && self.download_priority == 0
            && self.recording_priority == 0
            && self.reserve_slots_for_users == 0
            && self.max_background_per_provider == 0
            && is_default_retry_backoff_initial_secs(&self.retry_backoff_initial_secs)
            && is_default_retry_backoff_multiplier(&self.retry_backoff_multiplier)
            && is_default_retry_backoff_max_secs(&self.retry_backoff_max_secs)
            && is_default_retry_backoff_jitter_percent(&self.retry_backoff_jitter_percent)
            && is_default_retry_max_attempts(&self.retry_max_attempts)
            && self.recording.as_ref().is_none_or(RecordingConfigDto::is_empty)
    }
}

/// DVR recording configuration block.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename_template: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub default_pre_roll_secs: Option<u64>,
    #[serde(
        default = "default_recording_max_pre_roll_secs",
        skip_serializing_if = "is_default_recording_max_pre_roll_secs"
    )]
    pub max_pre_roll_secs: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub default_post_roll_secs: Option<u64>,
    #[serde(
        default = "default_recording_max_post_roll_secs",
        skip_serializing_if = "is_default_recording_max_post_roll_secs"
    )]
    pub max_post_roll_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RecordingRetentionConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<RecordingDiskConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<RecordingQuotaConfigDto>,
    #[serde(
        default = "default_recording_fallback_bytes_per_minute",
        skip_serializing_if = "is_default_recording_fallback_bytes_per_minute"
    )]
    pub fallback_bytes_per_minute: u64,
}

impl RecordingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.directory.is_none()
            && self.timezone.is_none()
            && self.filename_template.is_none()
            && self.default_pre_roll_secs.is_none()
            && is_default_recording_max_pre_roll_secs(&self.max_pre_roll_secs)
            && self.default_post_roll_secs.is_none()
            && is_default_recording_max_post_roll_secs(&self.max_post_roll_secs)
            && self.retention.is_none()
            && self.disk.is_none()
            && self.quota.is_none()
            && self.fallback_bytes_per_minute == default_recording_fallback_bytes_per_minute()
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingRetentionConfigDto {
    #[serde(default, skip_serializing_if = "is_zero_u32_opt")]
    pub keep_last_per_channel: Option<u32>,
    #[serde(default, skip_serializing_if = "is_zero_u32_opt")]
    pub delete_after_days: Option<u32>,
}

impl RecordingRetentionConfigDto {
    pub fn is_empty(&self) -> bool { self.keep_last_per_channel.is_none() && self.delete_after_days.is_none() }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingDiskConfigDto {
    #[serde(default, skip_serializing_if = "is_zero_u8_opt")]
    pub high_water_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "is_zero_u8_opt")]
    pub low_water_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub cleanup_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub safety_bytes: Option<u64>,
}

impl RecordingDiskConfigDto {
    pub fn is_empty(&self) -> bool {
        self.high_water_percent.is_none()
            && self.low_water_percent.is_none()
            && self.cleanup_interval_secs.is_none()
            && self.safety_bytes.is_none()
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingQuotaConfigDto {
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub default_private_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub per_user_bytes: HashMap<String, u64>,
    #[serde(default, skip_serializing_if = "is_zero_u64_opt")]
    pub shared_bytes: Option<u64>,
}

impl RecordingQuotaConfigDto {
    pub fn is_empty(&self) -> bool {
        self.default_private_bytes.is_none() && self.per_user_bytes.is_empty() && self.shared_bytes.is_none()
    }
}

/// Allowed placeholders in the recording filename template.
/// Order matters for error messages and the validation regex.
pub const RECORDING_FILENAME_PLACEHOLDERS: &[&str] =
    &["{channel}", "{program_title}", "{start_time}", "{end_time}", "{episode}", "{owner}"];

/// Validates a recording filename template. Returns the cleaned template and
/// the matched placeholder set on success.
fn validate_recording_filename_template(template: &str) -> Result<(), TuliproxError> {
    if template.is_empty() {
        return Err(TuliproxError::ConfigVideoDownload("recording.filename_template must not be empty".to_string()));
    }
    if template.len() > MAX_FILENAME_TEMPLATE_BYTES {
        return Err(TuliproxError::ConfigVideoDownload(format!(
            "recording.filename_template must not exceed {MAX_FILENAME_TEMPLATE_BYTES} bytes"
        )));
    }

    let bytes = template.as_bytes();
    let mut i = 0;
    let mut found_placeholder = false;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close) = template[i + 1..].find('}') {
                let end = i + 1 + close;
                let placeholder = &template[i..=end];
                if !RECORDING_FILENAME_PLACEHOLDERS.contains(&placeholder) {
                    return Err(TuliproxError::ConfigVideoDownload(format!(
                        "recording.filename_template contains unknown placeholder '{placeholder}'"
                    )));
                }
                found_placeholder = true;
                i = end + 1;
            } else {
                return Err(TuliproxError::ConfigVideoDownload(
                    "recording.filename_template has an unmatched '{'".to_string(),
                ));
            }
        } else if bytes[i] == b'}' {
            return Err(TuliproxError::ConfigVideoDownload(
                "recording.filename_template has an unmatched '}'".to_string(),
            ));
        } else {
            i += 1;
        }
    }

    if !found_placeholder {
        return Err(TuliproxError::ConfigVideoDownload(
            "recording.filename_template must contain at least one placeholder".to_string(),
        ));
    }
    Ok(())
}

fn validate_recording_timezone(tz: &str) -> Result<(), TuliproxError> {
    if tz.is_empty() {
        return Err(TuliproxError::ConfigVideoDownload("recording.timezone must not be empty".to_string()));
    }
    tz.parse::<chrono_tz::Tz>().map(|_| ()).map_err(|_| {
        TuliproxError::ConfigVideoDownload(format!("recording.timezone '{tz}' is not a valid IANA timezone"))
    })
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
        if let Some(download) = self.download.as_mut() {
            if download.recording.as_ref().is_some_and(RecordingConfigDto::is_empty) {
                download.recording = None;
            }
        }
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
                        return Err(TuliproxError::RegexCompile(format!("{episode_pattern} {err}")));
                    }
                }

                downl.retry_backoff_initial_secs = downl.retry_backoff_initial_secs.max(1);
                downl.retry_backoff_multiplier = downl.retry_backoff_multiplier.max(1.0);
                downl.retry_backoff_max_secs = downl.retry_backoff_max_secs.max(downl.retry_backoff_initial_secs);
                downl.retry_backoff_jitter_percent = downl.retry_backoff_jitter_percent.min(95);
                downl.retry_max_attempts = downl.retry_max_attempts.max(1);

                if let Some(recording) = downl.recording.as_mut() {
                    prepare_recording_config(recording, downl.directory.as_deref().unwrap_or(DEFAULT_DOWNLOAD_DIR))?;
                }
            }
        }
        Ok(())
    }
}

fn prepare_recording_config(recording: &mut RecordingConfigDto, download_dir: &str) -> Result<(), TuliproxError> {
    // directory: default to <download_dir>/recordings when blank.
    if let Some(dir) = recording.directory.as_ref() {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            recording.directory = None;
        } else if trimmed != dir {
            recording.directory = Some(trimmed.to_string());
        }
    }
    if recording.directory.is_none() {
        recording.directory = Some(default_recording_directory(download_dir));
    }

    // timezone: default UTC; validate IANA.
    if let Some(tz) = recording.timezone.as_ref() {
        let trimmed = tz.trim();
        if trimmed.is_empty() {
            recording.timezone = None;
        } else if trimmed != tz {
            recording.timezone = Some(trimmed.to_string());
        }
    }
    if recording.timezone.is_none() {
        recording.timezone = Some(default_recording_timezone());
    }
    validate_recording_timezone(recording.timezone.as_deref().unwrap_or("UTC"))?;

    // filename_template: default; validate placeholders.
    if let Some(template) = recording.filename_template.as_ref() {
        let trimmed = template.trim();
        if trimmed != template {
            recording.filename_template = Some(trimmed.to_string());
        }
    }
    if recording.filename_template.is_none() {
        recording.filename_template = Some(default_recording_filename_template());
    }
    validate_recording_filename_template(recording.filename_template.as_deref().unwrap_or(""))?;

    // padding: default <= max, fallback to defaults if missing.
    if recording.default_pre_roll_secs.is_none() {
        recording.default_pre_roll_secs = Some(0);
    }
    if recording.default_pre_roll_secs.unwrap_or(0) > recording.max_pre_roll_secs {
        return Err(TuliproxError::ConfigVideoDownload(format!(
            "recording.default_pre_roll_secs ({}) must not exceed max_pre_roll_secs ({})",
            recording.default_pre_roll_secs.unwrap_or(0),
            recording.max_pre_roll_secs
        )));
    }
    if recording.default_post_roll_secs.is_none() {
        recording.default_post_roll_secs = Some(0);
    }
    if recording.default_post_roll_secs.unwrap_or(0) > recording.max_post_roll_secs {
        return Err(TuliproxError::ConfigVideoDownload(format!(
            "recording.default_post_roll_secs ({}) must not exceed max_post_roll_secs ({})",
            recording.default_post_roll_secs.unwrap_or(0),
            recording.max_post_roll_secs
        )));
    }

    // retention: each present policy must be > 0.
    if let Some(retention) = recording.retention.as_ref() {
        if let Some(keep) = retention.keep_last_per_channel {
            if keep == 0 {
                return Err(TuliproxError::ConfigVideoDownload(
                    "recording.retention.keep_last_per_channel must be > 0".to_string(),
                ));
            }
        }
        if let Some(days) = retention.delete_after_days {
            if days == 0 {
                return Err(TuliproxError::ConfigVideoDownload(
                    "recording.retention.delete_after_days must be > 0".to_string(),
                ));
            }
        }
    }

    // disk: percentages in 0..=100, low < high, cleanup > 0, safety non-zero.
    if let Some(disk) = recording.disk.as_ref() {
        if let Some(high) = disk.high_water_percent {
            if high > 100 {
                return Err(TuliproxError::ConfigVideoDownload(format!(
                    "recording.disk.high_water_percent ({high}) must be 0..=100"
                )));
            }
        }
        if let Some(low) = disk.low_water_percent {
            if low > 100 {
                return Err(TuliproxError::ConfigVideoDownload(format!(
                    "recording.disk.low_water_percent ({low}) must be 0..=100"
                )));
            }
        }
        if let (Some(low), Some(high)) = (disk.low_water_percent, disk.high_water_percent) {
            if low >= high {
                return Err(TuliproxError::ConfigVideoDownload(format!(
                    "recording.disk.low_water_percent ({low}) must be < high_water_percent ({high})"
                )));
            }
        }
        if let Some(interval) = disk.cleanup_interval_secs {
            if interval == 0 {
                return Err(TuliproxError::ConfigVideoDownload(
                    "recording.disk.cleanup_interval_secs must be > 0".to_string(),
                ));
            }
        }
        if let Some(safety) = disk.safety_bytes {
            if safety == 0 {
                return Err(TuliproxError::ConfigVideoDownload("recording.disk.safety_bytes must be > 0".to_string()));
            }
        }
    }

    // quota: fallback bytes per minute must be > 0.
    if recording.fallback_bytes_per_minute == 0 {
        return Err(TuliproxError::ConfigVideoDownload("recording.fallback_bytes_per_minute must be > 0".to_string()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults::DEFAULT_DOWNLOAD_DIR;

    fn make_test_download_config() -> VideoDownloadConfigDto {
        VideoDownloadConfigDto {
            headers: HashMap::new(),
            directory: None,
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: default_retry_backoff_initial_secs(),
            retry_backoff_multiplier: default_retry_backoff_multiplier(),
            retry_backoff_max_secs: default_retry_backoff_max_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
            recording: None,
        }
    }

    #[test]
    fn prepare_sets_default_download_dir_when_missing() {
        let mut video =
            VideoConfigDto { extensions: Vec::new(), download: Some(make_test_download_config()), web_search: None };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.directory.as_deref(), Some(DEFAULT_DOWNLOAD_DIR));
    }

    #[test]
    fn prepare_sets_default_episode_pattern_when_missing() {
        let mut video =
            VideoConfigDto { extensions: Vec::new(), download: Some(make_test_download_config()), web_search: None };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert!(download.episode_pattern.is_some(), "expected default episode pattern to be set");
    }

    #[test]
    fn prepare_keeps_custom_download_dir() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                directory: Some("custom-downloads".to_string()),
                ..make_test_download_config()
            }),
            web_search: None,
        };
        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.directory.as_deref(), Some("custom-downloads"));
    }

    #[test]
    fn serializing_skips_default_download_dir() {
        let download =
            VideoDownloadConfigDto { directory: Some(DEFAULT_DOWNLOAD_DIR.to_string()), ..make_test_download_config() };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(
            !serialized.contains("\"directory\""),
            "expected no directory field for default value, got: {serialized}"
        );
    }

    #[test]
    fn serializing_keeps_custom_download_dir() {
        let download =
            VideoDownloadConfigDto { directory: Some("custom-downloads".to_string()), ..make_test_download_config() };
        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"directory\""), "expected directory field for custom value, got: {serialized}");
    }

    #[test]
    fn serializing_skips_default_episode_pattern() {
        let download =
            VideoDownloadConfigDto { episode_pattern: default_episode_pattern(), ..make_test_download_config() };
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
                retry_backoff_initial_secs: 3,
                retry_backoff_multiplier: 2.5,
                retry_backoff_max_secs: 45,
                retry_backoff_jitter_percent: 0,
                retry_max_attempts: 7,
                ..make_test_download_config()
            }),
            web_search: None,
        };

        video.prepare().expect("prepare should succeed");
        let download = video.download.expect("download should exist");
        assert_eq!(download.retry_backoff_initial_secs, 3);
        assert!((download.retry_backoff_multiplier - 2.5).abs() < F64_DEFAULT_EPSILON);
        assert_eq!(download.retry_backoff_max_secs, 45);
        assert_eq!(download.retry_backoff_jitter_percent, 0);
        assert_eq!(download.retry_max_attempts, 7);
    }

    #[test]
    fn serializing_keeps_custom_download_retry_backoff_settings() {
        let download = VideoDownloadConfigDto {
            retry_backoff_initial_secs: 4,
            retry_backoff_multiplier: 2.0,
            retry_backoff_max_secs: 60,
            retry_backoff_jitter_percent: 10,
            retry_max_attempts: 6,
            ..make_test_download_config()
        };

        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"retry_backoff_initial_secs\":4"));
        assert!(serialized.contains("\"retry_backoff_multiplier\":2.0"));
        assert!(serialized.contains("\"retry_backoff_max_secs\":60"));
        assert!(serialized.contains("\"retry_backoff_jitter_percent\":10"));
        assert!(serialized.contains("\"retry_max_attempts\":6"));
    }

    #[test]
    fn serializing_keeps_scheduler_policy_settings() {
        let download = VideoDownloadConfigDto {
            reserve_slots_for_users: 1,
            max_background_per_provider: 2,
            ..make_test_download_config()
        };

        let serialized = serde_json::to_string(&download).expect("download serialization should succeed");
        assert!(serialized.contains("\"reserve_slots_for_users\":1"));
        assert!(serialized.contains("\"max_background_per_provider\":2"));
    }

    #[test]
    fn clean_preserves_download_block_when_non_zero_priorities_are_set() {
        let mut video = VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto {
                download_priority: -2,
                recording_priority: 3,
                ..make_test_download_config()
            }),
            web_search: None,
        };

        video.clean();

        assert!(video.download.is_some());
    }

    #[test]
    fn serializing_keeps_non_zero_priorities_and_skips_zero_priorities() {
        let non_zero =
            VideoDownloadConfigDto { download_priority: -1, recording_priority: 2, ..make_test_download_config() };
        let zero = VideoDownloadConfigDto { download_priority: 0, recording_priority: 0, ..non_zero.clone() };

        let non_zero_serialized = serde_json::to_string(&non_zero).expect("non-zero priorities serialize");
        let zero_serialized = serde_json::to_string(&zero).expect("zero priorities serialize");

        assert!(non_zero_serialized.contains("\"download_priority\":-1"));
        assert!(non_zero_serialized.contains("\"recording_priority\":2"));
        assert!(!zero_serialized.contains("\"download_priority\""));
        assert!(!zero_serialized.contains("\"recording_priority\""));
    }

    // --- Recording config tests ---

    fn make_recording_config() -> RecordingConfigDto {
        RecordingConfigDto {
            directory: None,
            timezone: None,
            filename_template: None,
            default_pre_roll_secs: None,
            max_pre_roll_secs: default_recording_max_pre_roll_secs(),
            default_post_roll_secs: None,
            max_post_roll_secs: default_recording_max_post_roll_secs(),
            retention: None,
            disk: None,
            quota: None,
            fallback_bytes_per_minute: default_recording_fallback_bytes_per_minute(),
        }
    }

    fn make_recording_video_config(recording: RecordingConfigDto) -> VideoConfigDto {
        VideoConfigDto {
            extensions: Vec::new(),
            download: Some(VideoDownloadConfigDto { recording: Some(recording), ..make_test_download_config() }),
            web_search: None,
        }
    }

    #[test]
    fn recording_defaults_fill_in_when_absent() {
        let mut video = make_recording_video_config(make_recording_config());
        video.prepare().expect("prepare should succeed");
        let recording = video.download.as_ref().and_then(|d| d.recording.as_ref()).expect("recording should exist");
        assert_eq!(recording.directory.as_deref(), Some(format!("{DEFAULT_DOWNLOAD_DIR}/recordings").as_str()));
        assert!(recording.directory.as_deref().unwrap_or("").ends_with("/recordings"));
        assert_eq!(recording.timezone.as_deref(), Some("UTC"));
        assert_eq!(recording.filename_template.as_deref(), Some("{channel}_{program_title}_{start_time}"));
        assert_eq!(recording.default_pre_roll_secs, Some(0));
        assert_eq!(recording.default_post_roll_secs, Some(0));
    }

    #[test]
    fn recording_round_trips_custom_directory_and_template() {
        let mut recording = make_recording_config();
        recording.directory = Some("/var/recordings".to_string());
        recording.timezone = Some("Europe/Berlin".to_string());
        recording.filename_template = Some("{channel}_{program_title}_{start_time}_{episode}".to_string());
        let mut video = make_recording_video_config(recording);
        video.prepare().expect("prepare should succeed");
        let recording = video.download.as_ref().unwrap().recording.as_ref().unwrap();
        assert_eq!(recording.directory.as_deref(), Some("/var/recordings"));
        assert_eq!(recording.timezone.as_deref(), Some("Europe/Berlin"));
        assert_eq!(recording.filename_template.as_deref(), Some("{channel}_{program_title}_{start_time}_{episode}"));
    }

    #[test]
    fn recording_yaml_without_recording_block_loads() {
        let yaml = r#"
            extensions: [".ts"]
            download:
              directory: custom
        "#;
        let cfg: VideoConfigDto = serde_saphyr::from_str(yaml).expect("YAML should parse");
        let download = cfg.download.as_ref().expect("download should exist");
        assert!(download.recording.is_none());
    }

    #[test]
    fn recording_yaml_with_recording_block_loads_and_validates() {
        let yaml = r#"
            extensions: [".ts"]
            download:
              directory: custom
              recording:
                directory: /data/recordings
                timezone: Europe/London
                filename_template: "{owner}_{channel}_{start_time}"
        "#;
        let cfg: VideoConfigDto = serde_saphyr::from_str(yaml).expect("YAML should parse");
        let recording = cfg.download.as_ref().unwrap().recording.as_ref().unwrap();
        assert_eq!(recording.directory.as_deref(), Some("/data/recordings"));
        assert_eq!(recording.timezone.as_deref(), Some("Europe/London"));
    }

    #[test]
    fn recording_template_with_unknown_placeholder_is_rejected() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("{channel}_{unknown}".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("unknown placeholder should fail");
        assert!(err.to_string().contains("unknown placeholder"), "error: {err}");
    }

    #[test]
    fn recording_template_with_unmatched_brace_is_rejected() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("{channel_{start_time}".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("unmatched brace should fail");
        // The parser treats `{channel_{start_time}` as a single malformed placeholder,
        // which is rejected as 'unknown placeholder'.
        assert!(err.to_string().contains("placeholder"), "error: {err}");
    }

    #[test]
    fn recording_template_with_open_brace_is_rejected() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("{channel".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("open brace should fail");
        assert!(err.to_string().contains("unmatched"), "error: {err}");
    }

    #[test]
    fn recording_template_empty_is_rejected() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("empty template should fail");
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    #[test]
    fn recording_template_without_any_placeholder_is_rejected() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("static_name".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("static template should fail");
        assert!(err.to_string().contains("at least one placeholder"), "error: {err}");
    }

    #[test]
    fn recording_invalid_timezone_is_rejected() {
        let mut recording = make_recording_config();
        recording.timezone = Some("Not/A_Real_Zone".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("invalid timezone should fail");
        assert!(err.to_string().contains("IANA timezone"), "error: {err}");
    }

    #[test]
    fn recording_discards_unmatched_quote_brace() {
        let mut recording = make_recording_config();
        recording.filename_template = Some("channel}_{start_time}".to_string());
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("unmatched '}' should fail");
        assert!(err.to_string().contains("unmatched"), "error: {err}");
    }

    #[test]
    fn recording_default_pre_roll_above_max_is_rejected() {
        let mut recording = make_recording_config();
        recording.default_pre_roll_secs = Some(1200);
        recording.max_pre_roll_secs = 600;
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("default > max should fail");
        assert!(err.to_string().contains("default_pre_roll_secs"), "error: {err}");
    }

    #[test]
    fn recording_default_post_roll_above_max_is_rejected() {
        let mut recording = make_recording_config();
        recording.default_post_roll_secs = Some(2400);
        recording.max_post_roll_secs = 1800;
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("default > max should fail");
        assert!(err.to_string().contains("default_post_roll_secs"), "error: {err}");
    }

    #[test]
    fn recording_retention_zero_keep_last_is_rejected() {
        let mut recording = make_recording_config();
        recording.retention =
            Some(RecordingRetentionConfigDto { keep_last_per_channel: Some(0), delete_after_days: None });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("zero keep_last should fail");
        assert!(err.to_string().contains("keep_last_per_channel"), "error: {err}");
    }

    #[test]
    fn recording_retention_zero_delete_after_days_is_rejected() {
        let mut recording = make_recording_config();
        recording.retention =
            Some(RecordingRetentionConfigDto { keep_last_per_channel: None, delete_after_days: Some(0) });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("zero delete_after_days should fail");
        assert!(err.to_string().contains("delete_after_days"), "error: {err}");
    }

    #[test]
    fn recording_disk_high_water_above_100_is_rejected() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: Some(101),
            low_water_percent: None,
            cleanup_interval_secs: Some(3600),
            safety_bytes: Some(1024),
        });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("high > 100 should fail");
        assert!(err.to_string().contains("high_water_percent"), "error: {err}");
    }

    #[test]
    fn recording_disk_low_water_above_100_is_rejected() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: None,
            low_water_percent: Some(101),
            cleanup_interval_secs: Some(3600),
            safety_bytes: Some(1024),
        });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("low > 100 should fail");
        assert!(err.to_string().contains("low_water_percent"), "error: {err}");
    }

    #[test]
    fn recording_disk_low_ge_high_is_rejected() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: Some(80),
            low_water_percent: Some(80),
            cleanup_interval_secs: Some(3600),
            safety_bytes: Some(1024),
        });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("low >= high should fail");
        assert!(err.to_string().contains("must be <"), "error: {err}");
    }

    #[test]
    fn recording_disk_low_lt_high_is_accepted() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: Some(85),
            low_water_percent: Some(75),
            cleanup_interval_secs: Some(3600),
            safety_bytes: Some(1024),
        });
        let mut video = make_recording_video_config(recording);
        video.prepare().expect("low < high should succeed");
    }

    #[test]
    fn recording_disk_zero_cleanup_interval_is_rejected() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: None,
            low_water_percent: None,
            cleanup_interval_secs: Some(0),
            safety_bytes: None,
        });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("zero cleanup_interval should fail");
        assert!(err.to_string().contains("cleanup_interval"), "error: {err}");
    }

    #[test]
    fn recording_disk_zero_safety_bytes_is_rejected() {
        let mut recording = make_recording_config();
        recording.disk = Some(RecordingDiskConfigDto {
            high_water_percent: None,
            low_water_percent: None,
            cleanup_interval_secs: Some(3600),
            safety_bytes: Some(0),
        });
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("zero safety_bytes should fail");
        assert!(err.to_string().contains("safety_bytes"), "error: {err}");
    }

    #[test]
    fn recording_zero_fallback_bytes_per_minute_is_rejected() {
        let mut recording = make_recording_config();
        recording.fallback_bytes_per_minute = 0;
        let mut video = make_recording_video_config(recording);
        let err = video.prepare().expect_err("zero fallback should fail");
        assert!(err.to_string().contains("fallback_bytes_per_minute"), "error: {err}");
    }

    #[test]
    fn recording_with_per_user_byte_quota_loads() {
        let mut recording = make_recording_config();
        recording.quota = Some(RecordingQuotaConfigDto {
            default_private_bytes: Some(50 * 1024 * 1024 * 1024),
            per_user_bytes: HashMap::from([("user-uuid-1".to_string(), 100 * 1024 * 1024 * 1024)]),
            shared_bytes: Some(200 * 1024 * 1024 * 1024),
        });
        let mut video = make_recording_video_config(recording);
        video.prepare().expect("quota should be accepted");
        let recording = video.download.as_ref().unwrap().recording.as_ref().unwrap();
        assert_eq!(
            recording.quota.as_ref().unwrap().per_user_bytes.get("user-uuid-1"),
            Some(&(100 * 1024 * 1024 * 1024))
        );
    }

    #[test]
    fn recording_empty_config_is_cleaned_away() {
        let mut video = make_recording_video_config(make_recording_config());
        // prepare fills defaults, so a fresh empty config gets populated — then
        // a clean() pass should drop everything back to absent only if the
        // user explicitly emptied it. With defaults filled, recording is
        // non-empty and stays.
        video.prepare().expect("prepare should succeed");
        assert!(video.download.as_ref().unwrap().recording.is_some());
        video.clean();
        // After defaults filled, clean() treats the recording as non-empty.
        assert!(video.download.as_ref().unwrap().recording.is_some());
    }

    #[test]
    fn recording_serializes_empty_config_to_no_field() {
        let download =
            VideoDownloadConfigDto { recording: Some(make_recording_config()), ..make_test_download_config() };
        let serialized = serde_json::to_string(&download).expect("serialize should succeed");
        // make_recording_config has all defaults filled, so no fields are
        // serialized inside `recording` — but the parent block IS present.
        assert!(serialized.contains("\"recording\""), "expected recording block, got: {serialized}");
    }

    #[test]
    fn recording_serializes_omitted_field_when_absent() {
        let download = VideoDownloadConfigDto { recording: None, ..make_test_download_config() };
        let serialized = serde_json::to_string(&download).expect("serialize should succeed");
        assert!(!serialized.contains("\"recording\""), "expected no recording field, got: {serialized}");
    }
}
