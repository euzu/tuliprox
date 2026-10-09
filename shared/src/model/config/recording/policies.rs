use super::{
    default_recording_notification_backoff_initial_secs, default_recording_notification_backoff_max_secs,
    default_recording_notification_max_attempts, default_recording_notification_outbox_buffer,
    default_recording_retention_sweep_interval_secs,
    defaults::{
        is_default_recording_notification_backoff_initial_secs, is_default_recording_notification_backoff_max_secs,
        is_default_recording_notification_max_attempts, is_default_recording_notification_outbox_buffer,
        is_default_recording_retention_sweep_interval_secs, is_zero_u32_opt, is_zero_u64_opt, is_zero_u8_opt,
    },
};
use std::collections::HashMap;

/// Container the recorder muxes into. MPEG-TS is the default; operators
/// recording H.265 or AAC-only channels may want a container that suits them
/// better.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingContainerFormat {
    /// MPEG-TS. Robust against truncation, so a recording killed
    /// mid-stream still plays — which is why it is the default.
    #[default]
    Mpegts,
    Matroska,
    Mp4,
}

impl RecordingContainerFormat {
    /// The `-f` argument for the muxer.
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Self::Mpegts => "mpegts",
            Self::Matroska => "matroska",
            Self::Mp4 => "mp4",
        }
    }

    /// The extension recordings in this container get, without the dot.
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Mpegts => "ts",
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingRetentionConfigDto {
    #[serde(default, skip_serializing_if = "is_zero_u32_opt")]
    pub keep_last_per_channel: Option<u32>,
    #[serde(default, skip_serializing_if = "is_zero_u32_opt")]
    pub delete_after_days: Option<u32>,
    /// How often the age/count sweep runs. Independent of
    /// `disk.cleanup_interval_secs`, which paces the watermark check.
    #[serde(
        default = "default_recording_retention_sweep_interval_secs",
        skip_serializing_if = "is_default_recording_retention_sweep_interval_secs"
    )]
    pub sweep_interval_secs: u64,
}

impl Default for RecordingRetentionConfigDto {
    fn default() -> Self {
        Self {
            keep_last_per_channel: None,
            delete_after_days: None,
            sweep_interval_secs: default_recording_retention_sweep_interval_secs(),
        }
    }
}

impl RecordingRetentionConfigDto {
    pub fn is_empty(&self) -> bool {
        self.keep_last_per_channel.is_none()
            && self.delete_after_days.is_none()
            && is_default_recording_retention_sweep_interval_secs(&self.sweep_interval_secs)
    }
}

/// Lifecycle-notification delivery. The outbox worker owns these; the
/// recorder itself never blocks on a notification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordingNotificationConfigDto {
    /// Bounded in-memory queue between the recorder and the outbox
    /// worker. A full channel drops the newest entry rather than
    /// stalling a recording.
    #[serde(
        default = "default_recording_notification_outbox_buffer",
        skip_serializing_if = "is_default_recording_notification_outbox_buffer"
    )]
    pub outbox_buffer: usize,
    /// Delivery attempts before an entry is dead-lettered.
    #[serde(
        default = "default_recording_notification_max_attempts",
        skip_serializing_if = "is_default_recording_notification_max_attempts"
    )]
    pub max_attempts: u32,
    #[serde(
        default = "default_recording_notification_backoff_initial_secs",
        skip_serializing_if = "is_default_recording_notification_backoff_initial_secs"
    )]
    pub backoff_initial_secs: u64,
    #[serde(
        default = "default_recording_notification_backoff_max_secs",
        skip_serializing_if = "is_default_recording_notification_backoff_max_secs"
    )]
    pub backoff_max_secs: u64,
}

impl Default for RecordingNotificationConfigDto {
    fn default() -> Self {
        Self {
            outbox_buffer: default_recording_notification_outbox_buffer(),
            max_attempts: default_recording_notification_max_attempts(),
            backoff_initial_secs: default_recording_notification_backoff_initial_secs(),
            backoff_max_secs: default_recording_notification_backoff_max_secs(),
        }
    }
}

impl RecordingNotificationConfigDto {
    pub fn is_empty(&self) -> bool { *self == Self::default() }
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
