use crate::model::macros;
use chrono_tz::Tz;
use regex::Regex;
use shared::{
    defaults::DEFAULT_DOWNLOAD_DIR,
    model::{
        default_recording_notification_backoff_initial_secs, default_recording_notification_backoff_max_secs,
        default_recording_notification_max_attempts, default_recording_notification_outbox_buffer, RecordingConfigDto,
        RecordingContainerFormat, RecordingDiskConfigDto, RecordingNotificationConfigDto, RecordingQuotaConfigDto,
        RecordingRetentionConfigDto, VideoConfigDto, VideoDownloadConfigDto,
    },
};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct VideoDownloadConfig {
    pub headers: HashMap<String, String>,
    pub directory: String,
    pub organize_into_directories: bool,
    pub episode_pattern: Option<Arc<Regex>>,
    pub download_priority: i8,
    pub recording_priority: i8,
    pub reserve_slots_for_users: u8,
    pub max_background_per_provider: u8,
    pub retry_backoff_initial_secs: u64,
    pub retry_backoff_multiplier: f64,
    pub retry_backoff_max_secs: u64,
    pub retry_backoff_jitter_percent: u8,
    pub retry_max_attempts: u8,
    pub recording: Option<RecordingConfig>,
}

macros::from_impl!(VideoDownloadConfig);
impl From<&VideoDownloadConfigDto> for VideoDownloadConfig {
    fn from(dto: &VideoDownloadConfigDto) -> Self {
        Self {
            headers: dto.headers.clone(),
            directory: dto.directory.as_ref().map_or_else(|| DEFAULT_DOWNLOAD_DIR.to_string(), ToString::to_string),
            organize_into_directories: dto.organize_into_directories,
            episode_pattern: dto.episode_pattern.as_ref().and_then(|s| {
                shared::model::REGEX_CACHE
                    .get_or_compile(s)
                    .map_err(|e| log::warn!("Invalid episode_pattern regex '{s}': {e}"))
                    .ok()
            }),
            download_priority: dto.download_priority,
            recording_priority: dto.recording_priority,
            reserve_slots_for_users: dto.reserve_slots_for_users,
            max_background_per_provider: dto.max_background_per_provider,
            retry_backoff_initial_secs: dto.retry_backoff_initial_secs.max(1),
            retry_backoff_multiplier: dto.retry_backoff_multiplier.max(1.0),
            retry_backoff_max_secs: dto.retry_backoff_max_secs.max(dto.retry_backoff_initial_secs.max(1)),
            retry_backoff_jitter_percent: dto.retry_backoff_jitter_percent.min(95),
            retry_max_attempts: dto.retry_max_attempts.max(1),
            recording: dto.recording.as_ref().map(Into::into),
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
            download_priority: instance.download_priority,
            recording_priority: instance.recording_priority,
            reserve_slots_for_users: instance.reserve_slots_for_users,
            max_background_per_provider: instance.max_background_per_provider,
            retry_backoff_initial_secs: instance.retry_backoff_initial_secs,
            retry_backoff_multiplier: instance.retry_backoff_multiplier,
            retry_backoff_max_secs: instance.retry_backoff_max_secs,
            retry_backoff_jitter_percent: instance.retry_backoff_jitter_percent,
            retry_max_attempts: instance.retry_max_attempts,
            recording: instance.recording.as_ref().map(Into::into),
        }
    }
}

/// Backend domain type for DVR recording configuration.
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub enabled: bool,
    pub container_format: RecordingContainerFormat,
    pub directory: String,
    pub timezone: Tz,
    pub filename_template: String,
    pub default_pre_roll_secs: u64,
    pub max_pre_roll_secs: u64,
    pub default_post_roll_secs: u64,
    pub max_post_roll_secs: u64,
    pub retention: Option<RecordingRetentionConfig>,
    pub disk: Option<RecordingDiskConfig>,
    pub quota: Option<RecordingQuotaConfig>,
    pub notifications: RecordingNotificationConfig,
    pub fallback_bytes_per_minute: u64,
}

#[derive(Debug, Clone)]
pub struct RecordingRetentionConfig {
    pub keep_last_per_channel: Option<u32>,
    pub delete_after_days: Option<u32>,
    pub sweep_interval_secs: u64,
}

impl Default for RecordingRetentionConfig {
    fn default() -> Self {
        Self::from(&RecordingRetentionConfigDto::default())
    }
}

/// Runtime notification-delivery knobs. Always present: an absent
/// `notifications:` block means "use the documented defaults", not
/// "deliver nothing".
#[derive(Debug, Clone)]
pub struct RecordingNotificationConfig {
    pub outbox_buffer: usize,
    pub max_attempts: u32,
    pub backoff_initial_secs: u64,
    pub backoff_max_secs: u64,
}

impl Default for RecordingNotificationConfig {
    fn default() -> Self {
        Self::from(&RecordingNotificationConfigDto::default())
    }
}

impl RecordingNotificationConfig {
    /// True when every field still equals the documented default —
    /// `RecordingConfigDto::is_empty` and `VideoConfigDto::clean`
    /// use the same check to omit a defaulted notifications block.
    pub fn is_empty(&self) -> bool {
        self.outbox_buffer == default_recording_notification_outbox_buffer()
            && self.max_attempts == default_recording_notification_max_attempts()
            && self.backoff_initial_secs == default_recording_notification_backoff_initial_secs()
            && self.backoff_max_secs == default_recording_notification_backoff_max_secs()
    }
}

macros::from_impl!(RecordingNotificationConfig);
impl From<&RecordingNotificationConfigDto> for RecordingNotificationConfig {
    fn from(dto: &RecordingNotificationConfigDto) -> Self {
        Self {
            // A zero-capacity channel would make every enqueue block the
            // recorder; clamp to at least one slot.
            outbox_buffer: dto.outbox_buffer.max(1),
            max_attempts: dto.max_attempts.max(1),
            backoff_initial_secs: dto.backoff_initial_secs.max(1),
            backoff_max_secs: dto.backoff_max_secs.max(dto.backoff_initial_secs.max(1)),
        }
    }
}

impl From<&RecordingNotificationConfig> for RecordingNotificationConfigDto {
    fn from(instance: &RecordingNotificationConfig) -> Self {
        Self {
            outbox_buffer: instance.outbox_buffer,
            max_attempts: instance.max_attempts,
            backoff_initial_secs: instance.backoff_initial_secs,
            backoff_max_secs: instance.backoff_max_secs,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecordingDiskConfig {
    pub high_water_percent: Option<u8>,
    pub low_water_percent: Option<u8>,
    pub cleanup_interval_secs: Option<u64>,
    pub safety_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RecordingQuotaConfig {
    pub default_private_bytes: Option<u64>,
    pub per_user_bytes: HashMap<String, u64>,
    pub shared_bytes: Option<u64>,
}

macros::from_impl!(RecordingConfig);
impl From<&RecordingConfigDto> for RecordingConfig {
    fn from(dto: &RecordingConfigDto) -> Self {
        let timezone = dto
            .timezone
            .as_deref()
            .and_then(|s| s.parse::<Tz>().ok())
            .unwrap_or_else(|| "UTC".parse::<Tz>().expect("UTC must parse"));
        Self {
            enabled: dto.enabled,
            container_format: dto.container_format,
            directory: dto.directory.clone().unwrap_or_default(),
            timezone,
            filename_template: dto.filename_template.clone().unwrap_or_default(),
            default_pre_roll_secs: dto.default_pre_roll_secs.unwrap_or(0),
            max_pre_roll_secs: dto.max_pre_roll_secs,
            default_post_roll_secs: dto.default_post_roll_secs.unwrap_or(0),
            max_post_roll_secs: dto.max_post_roll_secs,
            retention: dto.retention.as_ref().map(Into::into),
            disk: dto.disk.as_ref().map(Into::into),
            quota: dto.quota.as_ref().map(Into::into),
            notifications: dto.notifications.as_ref().map(Into::into).unwrap_or_default(),
            fallback_bytes_per_minute: dto.fallback_bytes_per_minute,
        }
    }
}

impl From<&RecordingConfig> for RecordingConfigDto {
    fn from(instance: &RecordingConfig) -> Self {
        Self {
            enabled: instance.enabled,
            container_format: instance.container_format,
            directory: Some(instance.directory.clone()),
            timezone: Some(instance.timezone.name().to_string()),
            filename_template: Some(instance.filename_template.clone()),
            default_pre_roll_secs: if instance.default_pre_roll_secs == 0 {
                None
            } else {
                Some(instance.default_pre_roll_secs)
            },
            max_pre_roll_secs: instance.max_pre_roll_secs,
            default_post_roll_secs: if instance.default_post_roll_secs == 0 {
                None
            } else {
                Some(instance.default_post_roll_secs)
            },
            max_post_roll_secs: instance.max_post_roll_secs,
            retention: instance.retention.as_ref().map(Into::into),
            disk: instance.disk.as_ref().map(Into::into),
            quota: instance.quota.as_ref().map(Into::into),
            notifications: if instance.notifications.is_empty() {
                None
            } else {
                Some((&instance.notifications).into())
            },
            fallback_bytes_per_minute: instance.fallback_bytes_per_minute,
        }
    }
}

macros::from_impl!(RecordingRetentionConfig);
impl From<&RecordingRetentionConfigDto> for RecordingRetentionConfig {
    fn from(dto: &RecordingRetentionConfigDto) -> Self {
        Self {
            keep_last_per_channel: dto.keep_last_per_channel,
            delete_after_days: dto.delete_after_days,
            // A zero interval would spin the sweep loop; fall back to the
            // documented default instead of busy-looping.
            sweep_interval_secs: if dto.sweep_interval_secs == 0 {
                shared::model::default_recording_retention_sweep_interval_secs()
            } else {
                dto.sweep_interval_secs
            },
        }
    }
}

impl From<&RecordingRetentionConfig> for RecordingRetentionConfigDto {
    fn from(instance: &RecordingRetentionConfig) -> Self {
        Self {
            keep_last_per_channel: instance.keep_last_per_channel,
            delete_after_days: instance.delete_after_days,
            sweep_interval_secs: instance.sweep_interval_secs,
        }
    }
}

macros::from_impl!(RecordingDiskConfig);
impl From<&RecordingDiskConfigDto> for RecordingDiskConfig {
    fn from(dto: &RecordingDiskConfigDto) -> Self {
        Self {
            high_water_percent: dto.high_water_percent,
            low_water_percent: dto.low_water_percent,
            cleanup_interval_secs: dto.cleanup_interval_secs,
            safety_bytes: dto.safety_bytes,
        }
    }
}

impl From<&RecordingDiskConfig> for RecordingDiskConfigDto {
    fn from(instance: &RecordingDiskConfig) -> Self {
        Self {
            high_water_percent: instance.high_water_percent,
            low_water_percent: instance.low_water_percent,
            cleanup_interval_secs: instance.cleanup_interval_secs,
            safety_bytes: instance.safety_bytes,
        }
    }
}

macros::from_impl!(RecordingQuotaConfig);
impl From<&RecordingQuotaConfigDto> for RecordingQuotaConfig {
    fn from(dto: &RecordingQuotaConfigDto) -> Self {
        Self {
            default_private_bytes: dto.default_private_bytes,
            per_user_bytes: dto.per_user_bytes.clone(),
            shared_bytes: dto.shared_bytes,
        }
    }
}

impl From<&RecordingQuotaConfig> for RecordingQuotaConfigDto {
    fn from(instance: &RecordingQuotaConfig) -> Self {
        Self {
            default_private_bytes: instance.default_private_bytes,
            per_user_bytes: instance.per_user_bytes.clone(),
            shared_bytes: instance.shared_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub extensions: Vec<String>,
    pub download: Option<VideoDownloadConfig>,
    pub web_search: Option<String>,
}

impl VideoConfig {
    pub fn prepare(&mut self) {}
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
