use crate::defaults::{
    default_episode_pattern, default_recording_dir, is_blank_or_default_episode_pattern,
    is_blank_or_default_recording_dir, is_false,
};
use std::collections::HashMap;

/// DVR recording configuration block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingConfigDto {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub organize_into_directories: bool,
    #[serde(default = "default_episode_pattern", skip_serializing_if = "is_blank_or_default_episode_pattern")]
    pub episode_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_i8")]
    pub priority: i8,
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
    /// Master switch for the whole DVR feature. `false` stops the
    /// supervisors, so nothing is materialized, swept, or notified.
    #[serde(default = "default_recording_enabled", skip_serializing_if = "is_recording_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_recording_container_format")]
    pub container_format: RecordingContainerFormat,
    #[serde(default = "default_recording_dir", skip_serializing_if = "is_blank_or_default_recording_dir")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<RecordingNotificationConfigDto>,
    #[serde(
        default = "default_recording_fallback_bytes_per_minute",
        skip_serializing_if = "is_default_recording_fallback_bytes_per_minute"
    )]
    pub fallback_bytes_per_minute: u64,
}

/// Hand-written so `Default` agrees with the serde defaults. A derived
/// `Default` would produce `enabled: false` and zero padding limits —
/// i.e. a silently disabled DVR — which is the opposite of what an
/// absent `recording:` block means.
impl Default for RecordingConfigDto {
    fn default() -> Self {
        Self {
            headers: HashMap::new(),
            organize_into_directories: false,
            episode_pattern: default_episode_pattern(),
            priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: default_retry_backoff_initial_secs(),
            retry_backoff_multiplier: default_retry_backoff_multiplier(),
            retry_backoff_max_secs: default_retry_backoff_max_secs(),
            retry_backoff_jitter_percent: default_retry_backoff_jitter_percent(),
            retry_max_attempts: default_retry_max_attempts(),
            enabled: default_recording_enabled(),
            container_format: RecordingContainerFormat::default(),
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
            notifications: None,
            fallback_bytes_per_minute: default_recording_fallback_bytes_per_minute(),
        }
    }
}

impl RecordingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
            && !self.organize_into_directories
            && is_blank_or_default_episode_pattern(&self.episode_pattern)
            && self.priority == 0
            && self.reserve_slots_for_users == 0
            && self.max_background_per_provider == 0
            && is_default_retry_backoff_initial_secs(&self.retry_backoff_initial_secs)
            && is_default_retry_backoff_multiplier(&self.retry_backoff_multiplier)
            && is_default_retry_backoff_max_secs(&self.retry_backoff_max_secs)
            && is_default_retry_backoff_jitter_percent(&self.retry_backoff_jitter_percent)
            && is_default_retry_max_attempts(&self.retry_max_attempts)
            && self.enabled == default_recording_enabled()
            && is_default_recording_container_format(&self.container_format)
            && self.notifications.is_none()
            && self.directory.is_none()
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

    pub fn clean(&mut self) {
        self.retention = self.retention.take().filter(|value| !value.is_empty());
        self.disk = self.disk.take().filter(|value| !value.is_empty());
        self.quota = self.quota.take().filter(|value| !value.is_empty());
        self.notifications = self.notifications.take().filter(|value| !value.is_empty());
    }
}

mod defaults;
mod policies;
mod prepare;
mod video;
pub use defaults::{
    default_recording_cleanup_interval_secs, default_recording_directory, default_recording_disk_safety_bytes,
    default_recording_enabled, default_recording_fallback_bytes_per_minute, default_recording_filename_template,
    default_recording_max_post_roll_secs, default_recording_max_pre_roll_secs,
    default_recording_notification_backoff_initial_secs, default_recording_notification_backoff_max_secs,
    default_recording_notification_max_attempts, default_recording_notification_outbox_buffer,
    default_recording_retention_sweep_interval_secs, default_recording_timezone,
};
use defaults::{
    default_retry_backoff_initial_secs, default_retry_backoff_jitter_percent, default_retry_backoff_max_secs,
    default_retry_backoff_multiplier, default_retry_max_attempts, is_default_recording_container_format,
    is_default_recording_fallback_bytes_per_minute, is_default_recording_max_post_roll_secs,
    is_default_recording_max_pre_roll_secs, is_default_retry_backoff_initial_secs,
    is_default_retry_backoff_jitter_percent, is_default_retry_backoff_max_secs, is_default_retry_backoff_multiplier,
    is_default_retry_max_attempts, is_recording_enabled, is_zero_i8, is_zero_u64_opt, is_zero_u8,
};
pub use policies::{
    RecordingContainerFormat, RecordingDiskConfigDto, RecordingNotificationConfigDto, RecordingQuotaConfigDto,
    RecordingRetentionConfigDto,
};
pub(crate) use prepare::prepare_recording_config;
pub use prepare::RECORDING_FILENAME_PLACEHOLDERS;
pub use video::VideoConfigDto;
