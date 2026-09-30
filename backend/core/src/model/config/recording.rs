use crate::model::macros;
use chrono_tz::Tz;
use regex::Regex;
use shared::model::{
    default_recording_notification_backoff_initial_secs, default_recording_notification_backoff_max_secs,
    default_recording_notification_max_attempts, default_recording_notification_outbox_buffer, RecordingConfigDto,
    RecordingContainerFormat, RecordingDiskConfigDto, RecordingNotificationConfigDto, RecordingQuotaConfigDto,
    RecordingRetentionConfigDto, VideoConfigDto,
};
use std::{collections::HashMap, sync::Arc};

/// Backend domain type for DVR recording configuration.
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub headers: HashMap<String, String>,
    pub organize_into_directories: bool,
    pub episode_pattern: Option<Arc<Regex>>,
    pub priority: i8,
    pub reserve_slots_for_users: u8,
    pub max_background_per_provider: u8,
    pub retry_backoff_initial_secs: u64,
    pub retry_backoff_multiplier: f64,
    pub retry_backoff_max_secs: u64,
    pub retry_backoff_jitter_percent: u8,
    pub retry_max_attempts: u8,
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
pub struct VideoConfig {
    pub extensions: Vec<String>,
    pub web_search: Option<String>,
    pub recording: Option<RecordingConfig>,
}

macros::from_impl!(VideoConfig);
/// Why a recording-directory change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingRootChange {
    Allowed,
    /// The root moved while recordings exist under the old one.
    RefusedWithExistingRecordings {
        existing: usize,
    },
}

impl std::fmt::Display for RecordingRootChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allowed => f.write_str("allowed"),
            Self::RefusedWithExistingRecordings { existing } => write!(
                f,
                "the recording directory cannot be changed while {existing} recording(s) exist under the current \
                 one; their stored paths resolve against the configured root and would all stop resolving"
            ),
        }
    }
}

/// Whether a recording root may be replaced.
///
/// A recording stores its path relative to the root, and playback resolves it
/// against whichever root is configured now. Moving the root out from under
/// existing recordings makes every one resolve to a path holding no file: the
/// library still lists them and every one of them 404s.
pub fn recording_root_change(current: &str, incoming: &str, existing_recordings: usize) -> RecordingRootChange {
    if current == incoming || existing_recordings == 0 {
        return RecordingRootChange::Allowed;
    }
    RecordingRootChange::RefusedWithExistingRecordings { existing: existing_recordings }
}

/// What a changed recording setting does to a server that is already running.
///
/// The classification is a claim about where the value is read, not a
/// preference. A setting is only `LiveApplied` if every consumer loads it
/// from the current configuration at the moment it is used; one consumer
/// holding a startup copy makes it `NewWorkOnly` at best.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingReloadEffect {
    /// Re-read at each use, so a reload reaches work already in flight.
    LiveApplied,
    /// Read when a recording is admitted. Recordings already admitted keep
    /// the value they were admitted under; the persisted record is
    /// authoritative for them from then on.
    NewWorkOnly,
    /// Consumed once during startup and not re-readable afterwards.
    RestartRequired,
}

/// Every recording setting, flattened across the nested config blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingField {
    Headers,
    OrganizeIntoDirectories,
    EpisodePattern,
    Priority,
    ReserveSlotsForUsers,
    MaxBackgroundPerProvider,
    RetryBackoffInitialSecs,
    RetryBackoffMultiplier,
    RetryBackoffMaxSecs,
    RetryBackoffJitterPercent,
    RetryMaxAttempts,
    Enabled,
    ContainerFormat,
    Directory,
    Timezone,
    FilenameTemplate,
    DefaultPreRollSecs,
    MaxPreRollSecs,
    DefaultPostRollSecs,
    MaxPostRollSecs,
    RetentionKeepLastPerChannel,
    RetentionDeleteAfterDays,
    RetentionSweepIntervalSecs,
    DiskHighWaterPercent,
    DiskLowWaterPercent,
    DiskCleanupIntervalSecs,
    DiskSafetyBytes,
    QuotaDefaultPrivateBytes,
    QuotaPerUserBytes,
    QuotaSharedBytes,
    NotificationOutboxBuffer,
    NotificationMaxAttempts,
    NotificationBackoffInitialSecs,
    NotificationBackoffMaxSecs,
    FallbackBytesPerMinute,
}

impl RecordingField {
    /// Every field, so a test can prove the matrix has no gaps.
    pub const ALL: [Self; 35] = [
        Self::Headers,
        Self::OrganizeIntoDirectories,
        Self::EpisodePattern,
        Self::Priority,
        Self::ReserveSlotsForUsers,
        Self::MaxBackgroundPerProvider,
        Self::RetryBackoffInitialSecs,
        Self::RetryBackoffMultiplier,
        Self::RetryBackoffMaxSecs,
        Self::RetryBackoffJitterPercent,
        Self::RetryMaxAttempts,
        Self::Enabled,
        Self::ContainerFormat,
        Self::Directory,
        Self::Timezone,
        Self::FilenameTemplate,
        Self::DefaultPreRollSecs,
        Self::MaxPreRollSecs,
        Self::DefaultPostRollSecs,
        Self::MaxPostRollSecs,
        Self::RetentionKeepLastPerChannel,
        Self::RetentionDeleteAfterDays,
        Self::RetentionSweepIntervalSecs,
        Self::DiskHighWaterPercent,
        Self::DiskLowWaterPercent,
        Self::DiskCleanupIntervalSecs,
        Self::DiskSafetyBytes,
        Self::QuotaDefaultPrivateBytes,
        Self::QuotaPerUserBytes,
        Self::QuotaSharedBytes,
        Self::NotificationOutboxBuffer,
        Self::NotificationMaxAttempts,
        Self::NotificationBackoffInitialSecs,
        Self::NotificationBackoffMaxSecs,
        Self::FallbackBytesPerMinute,
    ];

    pub fn effect(self) -> RecordingReloadEffect {
        match self {
            // The outbox channel is sized once, at `mpsc::channel(outbox_buffer)`.
            Self::NotificationOutboxBuffer => RecordingReloadEffect::RestartRequired,

            // Read while building the path and the request for a new
            // recording. Once admitted, the stored path and window are what
            // matter, so changing these cannot disturb existing work.
            Self::Headers
            | Self::OrganizeIntoDirectories
            | Self::EpisodePattern
            | Self::Priority
            | Self::ContainerFormat
            | Self::Directory
            | Self::Timezone
            | Self::FilenameTemplate
            | Self::DefaultPreRollSecs
            | Self::MaxPreRollSecs
            | Self::DefaultPostRollSecs
            | Self::MaxPostRollSecs
            | Self::FallbackBytesPerMinute => RecordingReloadEffect::NewWorkOnly,

            // Everything else is loaded from the live configuration at the
            // point of use: the capacity check on each acquire attempt, the
            // retry decision, the retention and disk sweeps, and quota
            // admission.
            _ => RecordingReloadEffect::LiveApplied,
        }
    }
}

/// The result of offering a replacement recording configuration.
///
/// A reload is one decision over the whole block. There is deliberately no
/// variant that applies some fields and rejects others: a half-applied
/// recording configuration is not a state an operator can reason about, and
/// the refusal reasons all concern the configuration as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingReloadOutcome {
    Unchanged,
    Applied {
        changed: Vec<RecordingField>,
        /// Changed fields that will not take effect until a restart. They
        /// are still stored, so the restart picks them up.
        restart_required: Vec<RecordingField>,
    },
    /// Nothing is applied. The previous configuration stands in full.
    Refused(RecordingRootChange),
}

fn changed_recording_fields(current: &RecordingConfig, incoming: &RecordingConfig) -> Vec<RecordingField> {
    RecordingField::ALL
        .into_iter()
        .filter(|field| match field {
            RecordingField::Headers => current.headers != incoming.headers,
            RecordingField::OrganizeIntoDirectories => {
                current.organize_into_directories != incoming.organize_into_directories
            }
            // `Regex` has no equality; the pattern it was built from does.
            RecordingField::EpisodePattern => {
                current.episode_pattern.as_ref().map(|re| re.as_str())
                    != incoming.episode_pattern.as_ref().map(|re| re.as_str())
            }
            RecordingField::Priority => current.priority != incoming.priority,
            RecordingField::ReserveSlotsForUsers => current.reserve_slots_for_users != incoming.reserve_slots_for_users,
            RecordingField::MaxBackgroundPerProvider => {
                current.max_background_per_provider != incoming.max_background_per_provider
            }
            RecordingField::RetryBackoffInitialSecs => {
                current.retry_backoff_initial_secs != incoming.retry_backoff_initial_secs
            }
            RecordingField::RetryBackoffMultiplier => {
                (current.retry_backoff_multiplier - incoming.retry_backoff_multiplier).abs() > f64::EPSILON
            }
            RecordingField::RetryBackoffMaxSecs => current.retry_backoff_max_secs != incoming.retry_backoff_max_secs,
            RecordingField::RetryBackoffJitterPercent => {
                current.retry_backoff_jitter_percent != incoming.retry_backoff_jitter_percent
            }
            RecordingField::RetryMaxAttempts => current.retry_max_attempts != incoming.retry_max_attempts,
            RecordingField::Enabled => current.enabled != incoming.enabled,
            RecordingField::ContainerFormat => current.container_format != incoming.container_format,
            RecordingField::Directory => current.directory != incoming.directory,
            RecordingField::Timezone => current.timezone != incoming.timezone,
            RecordingField::FilenameTemplate => current.filename_template != incoming.filename_template,
            RecordingField::DefaultPreRollSecs => current.default_pre_roll_secs != incoming.default_pre_roll_secs,
            RecordingField::MaxPreRollSecs => current.max_pre_roll_secs != incoming.max_pre_roll_secs,
            RecordingField::DefaultPostRollSecs => current.default_post_roll_secs != incoming.default_post_roll_secs,
            RecordingField::MaxPostRollSecs => current.max_post_roll_secs != incoming.max_post_roll_secs,
            RecordingField::RetentionKeepLastPerChannel => {
                current.retention.as_ref().and_then(|r| r.keep_last_per_channel)
                    != incoming.retention.as_ref().and_then(|r| r.keep_last_per_channel)
            }
            RecordingField::RetentionDeleteAfterDays => {
                current.retention.as_ref().and_then(|r| r.delete_after_days)
                    != incoming.retention.as_ref().and_then(|r| r.delete_after_days)
            }
            RecordingField::RetentionSweepIntervalSecs => {
                current.retention.as_ref().map(|r| r.sweep_interval_secs)
                    != incoming.retention.as_ref().map(|r| r.sweep_interval_secs)
            }
            RecordingField::DiskHighWaterPercent => {
                current.disk.as_ref().and_then(|d| d.high_water_percent)
                    != incoming.disk.as_ref().and_then(|d| d.high_water_percent)
            }
            RecordingField::DiskLowWaterPercent => {
                current.disk.as_ref().and_then(|d| d.low_water_percent)
                    != incoming.disk.as_ref().and_then(|d| d.low_water_percent)
            }
            RecordingField::DiskCleanupIntervalSecs => {
                current.disk.as_ref().and_then(|d| d.cleanup_interval_secs)
                    != incoming.disk.as_ref().and_then(|d| d.cleanup_interval_secs)
            }
            RecordingField::DiskSafetyBytes => {
                current.disk.as_ref().and_then(|d| d.safety_bytes)
                    != incoming.disk.as_ref().and_then(|d| d.safety_bytes)
            }
            RecordingField::QuotaDefaultPrivateBytes => {
                current.quota.as_ref().and_then(|q| q.default_private_bytes)
                    != incoming.quota.as_ref().and_then(|q| q.default_private_bytes)
            }
            RecordingField::QuotaPerUserBytes => {
                current.quota.as_ref().map(|q| &q.per_user_bytes) != incoming.quota.as_ref().map(|q| &q.per_user_bytes)
            }
            RecordingField::QuotaSharedBytes => {
                current.quota.as_ref().and_then(|q| q.shared_bytes)
                    != incoming.quota.as_ref().and_then(|q| q.shared_bytes)
            }
            RecordingField::NotificationOutboxBuffer => {
                current.notifications.outbox_buffer != incoming.notifications.outbox_buffer
            }
            RecordingField::NotificationMaxAttempts => {
                current.notifications.max_attempts != incoming.notifications.max_attempts
            }
            RecordingField::NotificationBackoffInitialSecs => {
                current.notifications.backoff_initial_secs != incoming.notifications.backoff_initial_secs
            }
            RecordingField::NotificationBackoffMaxSecs => {
                current.notifications.backoff_max_secs != incoming.notifications.backoff_max_secs
            }
            RecordingField::FallbackBytesPerMinute => {
                current.fallback_bytes_per_minute != incoming.fallback_bytes_per_minute
            }
        })
        .collect()
}

/// Decide what a replacement recording configuration does, as one atomic
/// choice over the whole block.
pub fn recording_reload_outcome(
    current: &RecordingConfig,
    incoming: &RecordingConfig,
    existing_recordings: usize,
) -> RecordingReloadOutcome {
    let changed = changed_recording_fields(current, incoming);
    if changed.is_empty() {
        return RecordingReloadOutcome::Unchanged;
    }
    // The root refusal rejects the entire replacement rather than every
    // field except the directory: applying the rest would leave the
    // operator with a configuration they never asked for.
    if changed.contains(&RecordingField::Directory) {
        let root = recording_root_change(&current.directory, &incoming.directory, existing_recordings);
        if root != RecordingRootChange::Allowed {
            return RecordingReloadOutcome::Refused(root);
        }
    }
    let restart_required =
        changed.iter().copied().filter(|field| field.effect() == RecordingReloadEffect::RestartRequired).collect();
    RecordingReloadOutcome::Applied { changed, restart_required }
}

impl From<&VideoConfigDto> for VideoConfig {
    fn from(dto: &VideoConfigDto) -> Self {
        Self {
            extensions: dto.extensions.clone(),
            web_search: dto.web_search.clone(),
            recording: dto.recording.as_ref().map(Into::into),
        }
    }
}

impl From<&VideoConfig> for VideoConfigDto {
    fn from(instance: &VideoConfig) -> Self {
        Self {
            extensions: instance.extensions.clone(),
            web_search: instance.web_search.clone(),
            recording: instance.recording.as_ref().map(RecordingConfigDto::from),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordingRetentionConfig {
    pub keep_last_per_channel: Option<u32>,
    pub delete_after_days: Option<u32>,
    pub sweep_interval_secs: u64,
}

impl Default for RecordingRetentionConfig {
    fn default() -> Self { Self::from(&RecordingRetentionConfigDto::default()) }
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
    fn default() -> Self { Self::from(&RecordingNotificationConfigDto::default()) }
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
            headers: dto.headers.clone(),
            organize_into_directories: dto.organize_into_directories,
            episode_pattern: dto.episode_pattern.as_ref().and_then(|pattern| {
                shared::model::REGEX_CACHE
                    .get_or_compile(pattern)
                    .map_err(|error| log::warn!("Invalid episode_pattern regex '{pattern}': {error}"))
                    .ok()
            }),
            priority: dto.priority,
            reserve_slots_for_users: dto.reserve_slots_for_users,
            max_background_per_provider: dto.max_background_per_provider,
            retry_backoff_initial_secs: dto.retry_backoff_initial_secs.max(1),
            retry_backoff_multiplier: dto.retry_backoff_multiplier.max(1.0),
            retry_backoff_max_secs: dto.retry_backoff_max_secs.max(dto.retry_backoff_initial_secs.max(1)),
            retry_backoff_jitter_percent: dto.retry_backoff_jitter_percent.min(95),
            retry_max_attempts: dto.retry_max_attempts.max(1),
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
            headers: instance.headers.clone(),
            organize_into_directories: instance.organize_into_directories,
            episode_pattern: instance.episode_pattern.as_ref().map(std::string::ToString::to_string),
            priority: instance.priority,
            reserve_slots_for_users: instance.reserve_slots_for_users,
            max_background_per_provider: instance.max_background_per_provider,
            retry_backoff_initial_secs: instance.retry_backoff_initial_secs,
            retry_backoff_multiplier: instance.retry_backoff_multiplier,
            retry_backoff_max_secs: instance.retry_backoff_max_secs,
            retry_backoff_jitter_percent: instance.retry_backoff_jitter_percent,
            retry_max_attempts: instance.retry_max_attempts,
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

#[cfg(test)]
mod recording_root_tests {
    use super::{recording_root_change, RecordingRootChange};

    #[test]
    fn moving_the_root_with_recordings_present_is_refused() {
        // Stored paths resolve against the configured root. Moving it leaves
        // the library listing every recording and every one of them missing.
        assert_eq!(
            recording_root_change("/srv/recordings", "/mnt/new", 3),
            RecordingRootChange::RefusedWithExistingRecordings { existing: 3 }
        );
    }

    #[test]
    fn moving_the_root_of_an_empty_library_is_allowed() {
        // Nothing points at the old root, so nothing breaks.
        assert_eq!(recording_root_change("/srv/recordings", "/mnt/new", 0), RecordingRootChange::Allowed);
    }

    #[test]
    fn leaving_the_root_alone_is_always_allowed() {
        // Every other recording setting stays hot-reloadable.
        assert_eq!(recording_root_change("/srv/recordings", "/srv/recordings", 12), RecordingRootChange::Allowed);
    }

    #[test]
    fn the_refusal_says_how_many_recordings_are_in_the_way() {
        let refusal = RecordingRootChange::RefusedWithExistingRecordings { existing: 7 };
        assert!(refusal.to_string().contains('7'), "an operator has to know what is blocking it");
    }
}

#[cfg(test)]
mod recording_reload_tests {
    use super::{
        recording_reload_outcome, RecordingConfig, RecordingDiskConfig, RecordingField, RecordingQuotaConfig,
        RecordingReloadEffect, RecordingReloadOutcome, RecordingRetentionConfig, RecordingRootChange,
    };
    use shared::model::RecordingConfigDto;

    fn base() -> RecordingConfig {
        let mut cfg = RecordingConfig::from(&RecordingConfigDto::default());
        cfg.directory = "/srv/recordings".to_string();
        cfg.retention = Some(RecordingRetentionConfig {
            keep_last_per_channel: Some(3),
            delete_after_days: Some(30),
            sweep_interval_secs: 900,
        });
        cfg.disk = Some(RecordingDiskConfig {
            high_water_percent: Some(90),
            low_water_percent: Some(70),
            cleanup_interval_secs: Some(600),
            safety_bytes: Some(1024),
        });
        cfg.quota = Some(RecordingQuotaConfig {
            default_private_bytes: Some(1_000),
            per_user_bytes: std::collections::HashMap::new(),
            shared_bytes: Some(2_000),
        });
        cfg
    }

    /// Mutate exactly one field, so a diff can be attributed to it.
    fn mutate(cfg: &mut RecordingConfig, field: RecordingField) {
        match field {
            RecordingField::Headers => {
                cfg.headers.insert("X-Test".to_string(), "1".to_string());
            }
            RecordingField::OrganizeIntoDirectories => {
                cfg.organize_into_directories = !cfg.organize_into_directories;
            }
            RecordingField::EpisodePattern => {
                cfg.episode_pattern = Some(std::sync::Arc::new(regex::Regex::new("S(?<s>[0-9]+)").expect("regex")));
            }
            RecordingField::Priority => cfg.priority = cfg.priority.wrapping_add(1),
            RecordingField::ReserveSlotsForUsers => cfg.reserve_slots_for_users += 1,
            RecordingField::MaxBackgroundPerProvider => cfg.max_background_per_provider += 1,
            RecordingField::RetryBackoffInitialSecs => cfg.retry_backoff_initial_secs += 1,
            RecordingField::RetryBackoffMultiplier => cfg.retry_backoff_multiplier += 1.0,
            RecordingField::RetryBackoffMaxSecs => cfg.retry_backoff_max_secs += 1,
            RecordingField::RetryBackoffJitterPercent => cfg.retry_backoff_jitter_percent += 1,
            RecordingField::RetryMaxAttempts => cfg.retry_max_attempts += 1,
            RecordingField::Enabled => cfg.enabled = !cfg.enabled,
            RecordingField::ContainerFormat => {
                cfg.container_format = match cfg.container_format {
                    shared::model::RecordingContainerFormat::Matroska => {
                        shared::model::RecordingContainerFormat::Mpegts
                    }
                    _ => shared::model::RecordingContainerFormat::Matroska,
                };
            }
            RecordingField::Directory => cfg.directory = "/mnt/elsewhere".to_string(),
            RecordingField::Timezone => cfg.timezone = chrono_tz::Tz::Australia__Perth,
            RecordingField::FilenameTemplate => cfg.filename_template = "{title}-changed".to_string(),
            RecordingField::DefaultPreRollSecs => cfg.default_pre_roll_secs += 1,
            RecordingField::MaxPreRollSecs => cfg.max_pre_roll_secs += 1,
            RecordingField::DefaultPostRollSecs => cfg.default_post_roll_secs += 1,
            RecordingField::MaxPostRollSecs => cfg.max_post_roll_secs += 1,
            RecordingField::RetentionKeepLastPerChannel => {
                cfg.retention.as_mut().expect("retention").keep_last_per_channel = Some(9);
            }
            RecordingField::RetentionDeleteAfterDays => {
                cfg.retention.as_mut().expect("retention").delete_after_days = Some(9);
            }
            RecordingField::RetentionSweepIntervalSecs => {
                cfg.retention.as_mut().expect("retention").sweep_interval_secs += 1;
            }
            RecordingField::DiskHighWaterPercent => {
                cfg.disk.as_mut().expect("disk").high_water_percent = Some(95);
            }
            RecordingField::DiskLowWaterPercent => {
                cfg.disk.as_mut().expect("disk").low_water_percent = Some(60);
            }
            RecordingField::DiskCleanupIntervalSecs => {
                cfg.disk.as_mut().expect("disk").cleanup_interval_secs = Some(1);
            }
            RecordingField::DiskSafetyBytes => {
                cfg.disk.as_mut().expect("disk").safety_bytes = Some(2048);
            }
            RecordingField::QuotaDefaultPrivateBytes => {
                cfg.quota.as_mut().expect("quota").default_private_bytes = Some(5);
            }
            RecordingField::QuotaPerUserBytes => {
                cfg.quota.as_mut().expect("quota").per_user_bytes.insert("alice".to_string(), 5);
            }
            RecordingField::QuotaSharedBytes => {
                cfg.quota.as_mut().expect("quota").shared_bytes = Some(5);
            }
            RecordingField::NotificationOutboxBuffer => cfg.notifications.outbox_buffer += 1,
            RecordingField::NotificationMaxAttempts => cfg.notifications.max_attempts += 1,
            RecordingField::NotificationBackoffInitialSecs => cfg.notifications.backoff_initial_secs += 1,
            RecordingField::NotificationBackoffMaxSecs => cfg.notifications.backoff_max_secs += 1,
            RecordingField::FallbackBytesPerMinute => cfg.fallback_bytes_per_minute += 1,
        }
    }

    #[test]
    fn every_field_is_detected_on_its_own_and_never_drags_another_along() {
        // The diff is what the reload decision is built from. A field that
        // reports no change is a setting an operator can never alter, and a
        // field that reports its neighbours makes the decision wrong.
        for field in RecordingField::ALL {
            let current = base();
            let mut incoming = base();
            mutate(&mut incoming, field);
            match recording_reload_outcome(&current, &incoming, 0) {
                RecordingReloadOutcome::Applied { changed, .. } => {
                    assert_eq!(changed, vec![field], "{field:?} must be the only field reported");
                }
                other => panic!("{field:?} should apply cleanly against an empty library, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_identical_replacement_changes_nothing() {
        assert_eq!(recording_reload_outcome(&base(), &base(), 12), RecordingReloadOutcome::Unchanged);
    }

    #[test]
    fn every_field_has_exactly_one_classification() {
        // `effect` is a total function over `ALL`; this is what stops a new
        // setting being added without anyone deciding when it takes effect.
        for field in RecordingField::ALL {
            let effect = field.effect();
            assert!(
                matches!(
                    effect,
                    RecordingReloadEffect::LiveApplied
                        | RecordingReloadEffect::NewWorkOnly
                        | RecordingReloadEffect::RestartRequired
                ),
                "{field:?}"
            );
        }
        assert_eq!(RecordingField::ALL.len(), 35, "a new setting needs a classification, not a default");
    }

    #[test]
    fn only_the_outbox_buffer_needs_a_restart() {
        // It sizes an mpsc channel once, at startup. Everything else is
        // re-read either per use or per admission.
        let restart: Vec<_> = RecordingField::ALL
            .into_iter()
            .filter(|field| field.effect() == RecordingReloadEffect::RestartRequired)
            .collect();
        assert_eq!(restart, vec![RecordingField::NotificationOutboxBuffer]);
    }

    #[test]
    fn the_settings_a_running_transfer_consults_are_live() {
        // These are read inside the worker's capacity and retry loops and by
        // the retention sweeps, so a reload has to reach work already in
        // flight. If one of these ever becomes a startup copy this assertion
        // is the thing that should fail.
        for field in [
            RecordingField::MaxBackgroundPerProvider,
            RecordingField::ReserveSlotsForUsers,
            RecordingField::RetryMaxAttempts,
            RecordingField::RetentionSweepIntervalSecs,
            RecordingField::DiskLowWaterPercent,
            RecordingField::QuotaSharedBytes,
            RecordingField::Enabled,
        ] {
            assert_eq!(field.effect(), RecordingReloadEffect::LiveApplied, "{field:?}");
        }
    }

    #[test]
    fn the_settings_baked_into_a_recording_at_admission_do_not_disturb_it() {
        // Path, window and container are fixed when the recording is
        // admitted; the persisted record is authoritative afterwards.
        for field in [
            RecordingField::Directory,
            RecordingField::FilenameTemplate,
            RecordingField::OrganizeIntoDirectories,
            RecordingField::ContainerFormat,
            RecordingField::MaxPreRollSecs,
            RecordingField::Headers,
        ] {
            assert_eq!(field.effect(), RecordingReloadEffect::NewWorkOnly, "{field:?}");
        }
    }

    #[test]
    fn a_refused_root_rejects_the_whole_replacement_not_just_the_directory() {
        // Applying the other twenty settings and silently keeping the old
        // directory would leave the operator running a configuration they
        // never wrote.
        let current = base();
        let mut incoming = base();
        mutate(&mut incoming, RecordingField::Directory);
        mutate(&mut incoming, RecordingField::RetryMaxAttempts);
        mutate(&mut incoming, RecordingField::DiskLowWaterPercent);

        assert_eq!(
            recording_reload_outcome(&current, &incoming, 4),
            RecordingReloadOutcome::Refused(RecordingRootChange::RefusedWithExistingRecordings { existing: 4 }),
            "one refused field refuses the reload"
        );
    }

    #[test]
    fn a_many_field_reload_is_one_decision() {
        let current = base();
        let mut incoming = base();
        mutate(&mut incoming, RecordingField::Enabled);
        mutate(&mut incoming, RecordingField::NotificationOutboxBuffer);
        mutate(&mut incoming, RecordingField::QuotaSharedBytes);

        match recording_reload_outcome(&current, &incoming, 7) {
            RecordingReloadOutcome::Applied { changed, restart_required } => {
                assert_eq!(changed.len(), 3, "{changed:?}");
                // The restart-bound field is still applied and stored; it is
                // reported so the operator knows it is pending, not dropped.
                assert_eq!(restart_required, vec![RecordingField::NotificationOutboxBuffer]);
            }
            other => panic!("expected an applied reload, got {other:?}"),
        }
    }

    #[test]
    fn changing_the_root_of_an_empty_library_is_ordinary() {
        let current = base();
        let mut incoming = base();
        mutate(&mut incoming, RecordingField::Directory);
        match recording_reload_outcome(&current, &incoming, 0) {
            RecordingReloadOutcome::Applied { changed, restart_required } => {
                assert_eq!(changed, vec![RecordingField::Directory]);
                assert!(restart_required.is_empty(), "nothing points at the old root");
            }
            other => panic!("expected an applied reload, got {other:?}"),
        }
    }
}
