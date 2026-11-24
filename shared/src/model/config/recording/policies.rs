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
use crate::model::ByteSize;
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
    // The `*_bytes` aliases keep configs written before the keys accepted human-readable sizes loading.
    #[serde(default, alias = "default_private_bytes", skip_serializing_if = "Option::is_none")]
    pub default_private: Option<ByteSize>,
    #[serde(default, alias = "per_user_bytes", skip_serializing_if = "HashMap::is_empty")]
    pub per_user: HashMap<String, ByteSize>,
    #[serde(default, alias = "shared_bytes", skip_serializing_if = "Option::is_none")]
    pub shared: Option<ByteSize>,
}

impl RecordingQuotaConfigDto {
    pub fn is_empty(&self) -> bool {
        self.default_private.is_none() && self.per_user.is_empty() && self.shared.is_none()
    }

    pub fn validate(&self) -> Result<(), crate::error::TuliproxError> {
        let check = |key: &str, quota: &ByteSize| {
            quota.parse_positive(key).map(drop).map_err(crate::error::TuliproxError::ConfigRecording)
        };
        if let Some(quota) = &self.default_private {
            check("video.recording.quota.default_private", quota)?;
        }
        for (user, quota) in &self.per_user {
            crate::model::UserId::validate(user).map_err(|_| {
                crate::error::TuliproxError::ConfigRecording(format!(
                    "video.recording.quota.per_user key '{user}' must be a user id such as api:alice or web:alice"
                ))
            })?;
            check(&format!("video.recording.quota.per_user[\"{user}\"]"), quota)?;
        }
        if let Some(quota) = &self.shared {
            check("video.recording.quota.shared", quota)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_quota_config_dto_accepts_legacy_bytes_keys() {
        let yaml = r#"
default_private_bytes: 53687091200
per_user_bytes:
  "api:alice": 1024
shared_bytes: 536870912000
"#;
        let dto: RecordingQuotaConfigDto = crate::utils::yaml_from_str(yaml).expect("deserialize legacy quota yaml");
        assert!(dto.validate().is_ok());
        let bytes = |size: Option<&ByteSize>| size.and_then(|size| size.parse_bytes().ok().map(|bytes| bytes.0));
        assert_eq!(bytes(dto.default_private.as_ref()), Some(53_687_091_200));
        assert_eq!(bytes(dto.per_user.get("api:alice")), Some(1024));
        assert_eq!(bytes(dto.shared.as_ref()), Some(536_870_912_000));
    }

    #[test]
    fn recording_quota_config_dto_yaml_resolves_exact_bytes() {
        let yaml = r#"
default_private: 20GB
per_user:
  "api:alice": 1TB
shared: 1024
"#;
        let dto: RecordingQuotaConfigDto = crate::utils::yaml_from_str(yaml).expect("deserialize valid quota yaml");
        assert!(dto.validate().is_ok());

        assert_eq!(
            dto.default_private.as_ref().unwrap().parse_bytes().expect("parse default_private").get(),
            21_474_836_480
        );
        assert_eq!(
            dto.per_user.get("api:alice").unwrap().parse_bytes().expect("parse alice quota").get(),
            1_099_511_627_776
        );
        assert_eq!(dto.shared.as_ref().unwrap().parse_bytes().expect("parse shared").get(), 1024);
    }

    #[test]
    fn recording_quota_config_dto_rejects_per_user_keys_without_namespace() {
        for key in ["alice", "", "api:../x"] {
            let dto = RecordingQuotaConfigDto {
                per_user: [(key.to_string(), ByteSize::new("10GB"))].into(),
                ..Default::default()
            };
            assert_eq!(
                dto.validate().map_err(|err| err.to_string()).err(),
                Some(format!(
                    "config recording error: video.recording.quota.per_user key '{key}' must be a user id such as \
                     api:alice or web:alice"
                )),
                "key {key:?}"
            );
        }
        for key in ["api:alice", "web:bob"] {
            let dto = RecordingQuotaConfigDto {
                per_user: [(key.to_string(), ByteSize::new("10GB"))].into(),
                ..Default::default()
            };
            assert!(dto.validate().is_ok(), "key {key:?}");
        }
    }

    #[test]
    fn recording_quota_config_dto_rejects_invalid_and_zero_values() {
        for bad in ["0", "0GB", "abc"] {
            let dto_private =
                RecordingQuotaConfigDto { default_private: Some(ByteSize::new(bad)), ..Default::default() };
            let err = dto_private.validate().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("video.recording.quota.default_private") && msg.contains(bad),
                "expected key name and value in error for default_private {bad}, got: {msg}"
            );

            let dto_shared = RecordingQuotaConfigDto { shared: Some(ByteSize::new(bad)), ..Default::default() };
            let err = dto_shared.validate().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("video.recording.quota.shared") && msg.contains(bad),
                "expected key name and value in error for shared {bad}, got: {msg}"
            );

            let dto_user = RecordingQuotaConfigDto {
                per_user: [("api:alice".to_string(), ByteSize::new(bad))].into(),
                ..Default::default()
            };
            let err = dto_user.validate().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("video.recording.quota.per_user[\"api:alice\"]") && msg.contains(bad),
                "expected key name and value in error for per_user {bad}, got: {msg}"
            );
        }
    }
}
