use crate::{
    defaults::{
        default_episode_pattern, default_recording_dir, default_supported_video_extensions,
        is_blank_or_default_episode_pattern, is_blank_or_default_recording_dir, is_default_supported_video_extensions,
        is_false, DEFAULT_USER_AGENT, F64_DEFAULT_EPSILON,
    },
    error::TuliproxError,
    utils::{is_blank_optional_str, is_blank_optional_string},
};
use std::collections::HashMap;

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
const DEFAULT_RECORDING_RETENTION_SWEEP_INTERVAL_SECS: u64 = 60 * 60; // 1 hour
const DEFAULT_RECORDING_NOTIFICATION_OUTBOX_BUFFER: usize = 1024;
const DEFAULT_RECORDING_NOTIFICATION_MAX_ATTEMPTS: u32 = 6;
const DEFAULT_RECORDING_NOTIFICATION_BACKOFF_INITIAL_SECS: u64 = 5;
const DEFAULT_RECORDING_NOTIFICATION_BACKOFF_MAX_SECS: u64 = 900; // 15 minutes
const MAX_FILENAME_TEMPLATE_BYTES: usize = 240;

/// Container the recorder muxes into. Recordings used to be hard-coded
/// to MPEG-TS regardless of the source codecs; operators recording
/// H.265 or AAC-only channels want a container that can hold them.
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
pub const fn default_recording_retention_sweep_interval_secs() -> u64 {
    DEFAULT_RECORDING_RETENTION_SWEEP_INTERVAL_SECS
}
pub const fn default_recording_notification_outbox_buffer() -> usize { DEFAULT_RECORDING_NOTIFICATION_OUTBOX_BUFFER }
pub const fn default_recording_notification_max_attempts() -> u32 { DEFAULT_RECORDING_NOTIFICATION_MAX_ATTEMPTS }
pub const fn default_recording_notification_backoff_initial_secs() -> u64 {
    DEFAULT_RECORDING_NOTIFICATION_BACKOFF_INITIAL_SECS
}
pub const fn default_recording_notification_backoff_max_secs() -> u64 {
    DEFAULT_RECORDING_NOTIFICATION_BACKOFF_MAX_SECS
}
pub const fn default_recording_enabled() -> bool { true }

fn is_default_recording_fallback_bytes_per_minute(value: &u64) -> bool {
    *value == default_recording_fallback_bytes_per_minute()
}
fn is_default_recording_retention_sweep_interval_secs(value: &u64) -> bool {
    *value == default_recording_retention_sweep_interval_secs()
}
fn is_default_recording_notification_outbox_buffer(value: &usize) -> bool {
    *value == default_recording_notification_outbox_buffer()
}
fn is_default_recording_notification_max_attempts(value: &u32) -> bool {
    *value == default_recording_notification_max_attempts()
}
fn is_default_recording_notification_backoff_initial_secs(value: &u64) -> bool {
    *value == default_recording_notification_backoff_initial_secs()
}
fn is_default_recording_notification_backoff_max_secs(value: &u64) -> bool {
    *value == default_recording_notification_backoff_max_secs()
}
fn is_recording_enabled(value: &bool) -> bool { *value }
fn is_default_recording_container_format(value: &RecordingContainerFormat) -> bool {
    *value == RecordingContainerFormat::default()
}
// `skip_serializing_if` predicates that distinguish "field absent" from
// "field present with a zero value". An explicit `Some(0)` is a real
// configuration choice and must round-trip; only `None` means "absent".
fn is_zero_u64_opt(value: &Option<u64>) -> bool { value.is_none() }
fn is_zero_u32_opt(value: &Option<u32>) -> bool { value.is_none() }
fn is_zero_u8_opt(value: &Option<u8>) -> bool { value.is_none() }

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

/// Allowed placeholders in the recording filename template.
/// Order matters for error messages and the validation regex.
pub const RECORDING_FILENAME_PLACEHOLDERS: &[&str] =
    &["{channel}", "{program_title}", "{start_time}", "{end_time}", "{episode}", "{owner}"];

/// Validates a recording filename template. Returns the cleaned template and
/// the matched placeholder set on success.
fn validate_recording_filename_template(template: &str) -> Result<(), TuliproxError> {
    if template.is_empty() {
        return Err(TuliproxError::ConfigRecording("recording.filename_template must not be empty".to_string()));
    }
    if template.len() > MAX_FILENAME_TEMPLATE_BYTES {
        return Err(TuliproxError::ConfigRecording(format!(
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
                    return Err(TuliproxError::ConfigRecording(format!(
                        "recording.filename_template contains unknown placeholder '{placeholder}'"
                    )));
                }
                found_placeholder = true;
                i = end + 1;
            } else {
                return Err(TuliproxError::ConfigRecording(
                    "recording.filename_template has an unmatched '{'".to_string(),
                ));
            }
        } else if bytes[i] == b'}' {
            return Err(TuliproxError::ConfigRecording("recording.filename_template has an unmatched '}'".to_string()));
        } else {
            i += 1;
        }
    }

    if !found_placeholder {
        return Err(TuliproxError::ConfigRecording(
            "recording.filename_template must contain at least one placeholder".to_string(),
        ));
    }
    Ok(())
}

fn validate_recording_timezone(tz: &str) -> Result<(), TuliproxError> {
    if tz.is_empty() {
        return Err(TuliproxError::ConfigRecording("recording.timezone must not be empty".to_string()));
    }
    tz.parse::<chrono_tz::Tz>()
        .map(|_| ())
        .map_err(|_| TuliproxError::ConfigRecording(format!("recording.timezone '{tz}' is not a valid IANA timezone")))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoConfigDto {
    #[serde(
        default = "default_supported_video_extensions",
        skip_serializing_if = "is_default_supported_video_extensions"
    )]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub web_search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingConfigDto>,
}

impl VideoConfigDto {
    pub fn is_empty(&self) -> bool {
        (self.extensions.is_empty() || is_default_supported_video_extensions(&self.extensions))
            && is_blank_optional_str(self.web_search.as_deref())
            && self.recording.as_ref().is_none_or(RecordingConfigDto::is_empty)
    }

    pub fn clean(&mut self) {
        if let Some(recording) = self.recording.as_mut() {
            recording.clean();
        }
        if self.recording.as_ref().is_some_and(RecordingConfigDto::is_empty) {
            self.recording = None;
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.extensions.is_empty() {
            self.extensions = default_supported_video_extensions();
        }
        if let Some(recording) = self.recording.as_mut() {
            prepare_recording_config(recording)?;
        }
        Ok(())
    }
}

pub(crate) fn prepare_recording_config(recording: &mut RecordingConfigDto) -> Result<(), TuliproxError> {
    if recording.headers.is_empty() {
        recording.headers.insert("Accept".to_string(), "video/*".to_string());
        recording.headers.insert("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string());
    }
    if is_blank_or_default_episode_pattern(&recording.episode_pattern) {
        recording.episode_pattern = default_episode_pattern();
    } else if let Some(pattern) = recording.episode_pattern.as_ref() {
        recording.episode_pattern = Some(pattern.trim().to_string());
    }
    if let Some(pattern) = recording.episode_pattern.as_ref() {
        crate::model::REGEX_CACHE
            .get_or_compile(pattern)
            .map_err(|err| TuliproxError::RegexCompile(format!("{pattern} {err}")))?;
    }
    recording.retry_backoff_initial_secs = recording.retry_backoff_initial_secs.max(1);
    recording.retry_backoff_multiplier = recording.retry_backoff_multiplier.max(1.0);
    recording.retry_backoff_max_secs = recording.retry_backoff_max_secs.max(recording.retry_backoff_initial_secs);
    recording.retry_backoff_jitter_percent = recording.retry_backoff_jitter_percent.min(95);
    recording.retry_max_attempts = recording.retry_max_attempts.max(1);

    // Directory is independent of video extensions and download settings.
    if let Some(dir) = recording.directory.as_ref() {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            recording.directory = None;
        } else if trimmed != dir {
            recording.directory = Some(trimmed.to_string());
        }
    }
    if recording.directory.is_none() {
        recording.directory = default_recording_dir();
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
    if let Some(tz) = recording.timezone.as_ref() {
        if tz.parse::<chrono_tz::Tz>().is_err() {
            // Surface a warning before the strict validator below turns
            // this into a hard error, so the operator sees both signals.
            log::warn!("recording.timezone '{tz}' is not a valid IANA timezone");
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
        return Err(TuliproxError::ConfigRecording(format!(
            "recording.default_pre_roll_secs ({}) must not exceed max_pre_roll_secs ({})",
            recording.default_pre_roll_secs.unwrap_or(0),
            recording.max_pre_roll_secs
        )));
    }
    if recording.default_post_roll_secs.is_none() {
        recording.default_post_roll_secs = Some(0);
    }
    if recording.default_post_roll_secs.unwrap_or(0) > recording.max_post_roll_secs {
        return Err(TuliproxError::ConfigRecording(format!(
            "recording.default_post_roll_secs ({}) must not exceed max_post_roll_secs ({})",
            recording.default_post_roll_secs.unwrap_or(0),
            recording.max_post_roll_secs
        )));
    }

    // retention: each present policy must be > 0.
    if let Some(retention) = recording.retention.as_ref() {
        if let Some(keep) = retention.keep_last_per_channel {
            if keep == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.retention.keep_last_per_channel must be > 0".to_string(),
                ));
            }
        }
        if let Some(days) = retention.delete_after_days {
            if days == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.retention.delete_after_days must be > 0".to_string(),
                ));
            }
        }
        if retention.sweep_interval_secs == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.retention.sweep_interval_secs must be > 0".to_string(),
            ));
        }
        if retention.keep_last_per_channel.is_none() && retention.delete_after_days.is_none() {
            // A retention block that expresses no policy reads as
            // "retention is configured" while nothing is ever deleted.
            log::warn!(
                "recording.retention has neither keep_last_per_channel nor delete_after_days; \
                 no recording will ever be deleted by policy"
            );
        }
    }

    // notifications: an outbox that cannot hold or retry anything would
    // silently drop every lifecycle notification.
    if let Some(notifications) = recording.notifications.as_ref() {
        if notifications.outbox_buffer == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.notifications.outbox_buffer must be > 0".to_string(),
            ));
        }
        if notifications.max_attempts == 0 {
            return Err(TuliproxError::ConfigRecording("recording.notifications.max_attempts must be > 0".to_string()));
        }
        if notifications.backoff_initial_secs == 0 {
            return Err(TuliproxError::ConfigRecording(
                "recording.notifications.backoff_initial_secs must be > 0".to_string(),
            ));
        }
        if notifications.backoff_max_secs < notifications.backoff_initial_secs {
            return Err(TuliproxError::ConfigRecording(format!(
                "recording.notifications.backoff_max_secs ({}) must be >= backoff_initial_secs ({})",
                notifications.backoff_max_secs, notifications.backoff_initial_secs
            )));
        }
    }

    // disk: percentages in 0..=100, low < high, cleanup > 0, safety non-zero.
    if let Some(disk) = recording.disk.as_ref() {
        if let Some(high) = disk.high_water_percent {
            if high > 100 {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.high_water_percent ({high}) must be 0..=100"
                )));
            }
        }
        if let Some(low) = disk.low_water_percent {
            if low > 100 {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.low_water_percent ({low}) must be 0..=100"
                )));
            }
        }
        if let (Some(low), Some(high)) = (disk.low_water_percent, disk.high_water_percent) {
            if low >= high {
                return Err(TuliproxError::ConfigRecording(format!(
                    "recording.disk.low_water_percent ({low}) must be < high_water_percent ({high})"
                )));
            }
        }
        if let Some(interval) = disk.cleanup_interval_secs {
            if interval == 0 {
                return Err(TuliproxError::ConfigRecording(
                    "recording.disk.cleanup_interval_secs must be > 0".to_string(),
                ));
            }
        }
        if let Some(safety) = disk.safety_bytes {
            if safety == 0 {
                return Err(TuliproxError::ConfigRecording("recording.disk.safety_bytes must be > 0".to_string()));
            }
        }
    }

    // quota: fallback bytes per minute must be > 0.
    if recording.fallback_bytes_per_minute == 0 {
        return Err(TuliproxError::ConfigRecording("recording.fallback_bytes_per_minute must be > 0".to_string()));
    }

    // Nothing bounds recording disk use unless at least one of the three
    // policies is set. Worth a warning, not an error: a dedicated
    // filesystem is a legitimate reason to run without any of them.
    let has_policy_retention = recording
        .retention
        .as_ref()
        .is_some_and(|retention| retention.keep_last_per_channel.is_some() || retention.delete_after_days.is_some());
    let has_watermarks = recording
        .disk
        .as_ref()
        .is_some_and(|disk| disk.high_water_percent.is_some() && disk.low_water_percent.is_some());
    let has_quota = recording.quota.as_ref().is_some_and(|quota| {
        quota.default_private_bytes.is_some() || quota.shared_bytes.is_some() || !quota.per_user_bytes.is_empty()
    });
    if recording.enabled && !has_policy_retention && !has_watermarks && !has_quota {
        log::warn!(
            "recording is enabled with no retention, no disk watermarks, and no quota; \
             recording disk usage is unbounded"
        );
    }

    Ok(())
}
