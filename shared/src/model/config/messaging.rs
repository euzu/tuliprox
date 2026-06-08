use crate::{
    error::TuliproxError,
    model::MsgKind,
    utils::{
        default_critical_percent, default_repeat_interval_secs, default_warn_percent, is_blank_optional_str,
        is_blank_optional_string, is_false,
    },
};

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelegramMessagingConfigDto {
    pub bot_token: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chat_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub markdown: bool,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<MsgKind, String>,
}

impl TelegramMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.bot_token.trim().is_empty() && self.chat_ids.is_empty() && self.templates.is_empty()
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RestMessagingConfigDto {
    pub url: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<MsgKind, String>,
}

impl RestMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty()
            && is_blank_optional_str(self.method.as_deref())
            && self.headers.is_empty()
            && self.templates.is_empty()
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiscordMessagingConfigDto {
    pub url: String,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<MsgKind, String>,
}

impl DiscordMessagingConfigDto {
    pub fn is_empty(&self) -> bool { self.url.trim().is_empty() && self.templates.is_empty() }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PushoverMessagingConfigDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub url: Option<String>,
    pub token: String,
    pub user: String,
}

impl PushoverMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        is_blank_optional_str(self.url.as_deref()) && self.token.trim().is_empty() && self.user.trim().is_empty()
    }
}

/// Configuration for threshold-driven disk-space alerts.
///
/// Set `messaging.disk_alert` to enable notifications whenever the filesystem
/// hosting the process working directory crosses `warn_percent` or
/// `critical_percent`. The same alert is re-sent after `repeat_interval_secs`
/// while the disk stays in the same alert state.
///
/// All fields are optional and the alert is disabled when the whole block is
/// absent. Defaults match the values used at runtime when fields are unset.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiskAlertConfigDto {
    /// Threshold (0-100, exclusive) below which the disk is considered normal.
    /// Defaults to `80.0`.
    #[serde(default = "default_warn_percent")]
    pub warn_percent: f64,
    /// Threshold (0-100, inclusive) at which the disk is considered full.
    /// Must be `> warn_percent`. Defaults to `95.0`.
    #[serde(default = "default_critical_percent")]
    pub critical_percent: f64,
    /// Re-arm interval in seconds. While the disk stays in the same alert
    /// state, the alert is re-sent after this many seconds. Defaults to
    /// `3600` (1h).
    #[serde(default = "default_repeat_interval_secs")]
    pub repeat_interval_secs: u64,
}

impl DiskAlertConfigDto {
    pub fn is_empty(&self) -> bool {
        self.warn_percent == default_warn_percent()
            && self.critical_percent == default_critical_percent()
            && self.repeat_interval_secs == default_repeat_interval_secs()
    }
}

impl Default for DiskAlertConfigDto {
    fn default() -> Self {
        Self {
            warn_percent: default_warn_percent(),
            critical_percent: default_critical_percent(),
            repeat_interval_secs: default_repeat_interval_secs(),
        }
    }
}

impl DiskAlertConfigDto {
    /// Sanity-check the thresholds. The caller surfaces the error to the user.
    pub fn prepare(&self) -> Result<(), TuliproxError> {
        if !(0.0..=100.0).contains(&self.warn_percent) {
            return Err(TuliproxError::Config(format!(
                "messaging.disk_alert.warn_percent must be in [0, 100], got {}",
                self.warn_percent
            )));
        }
        if !(0.0..=100.0).contains(&self.critical_percent) {
            return Err(TuliproxError::Config(format!(
                "messaging.disk_alert.critical_percent must be in [0, 100], got {}",
                self.critical_percent
            )));
        }
        if self.critical_percent <= self.warn_percent {
            return Err(TuliproxError::Config(format!(
                "messaging.disk_alert.critical_percent ({}) must be > warn_percent ({})",
                self.critical_percent, self.warn_percent
            )));
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MessagingConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_on: Vec<MsgKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<RestMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover: Option<PushoverMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_alert: Option<DiskAlertConfigDto>,
}

impl MessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.notify_on.is_empty()
            && (self.disk_alert.is_none() || self.disk_alert.as_ref().is_some_and(|c| c.is_empty()))
            && (self.telegram.is_none() || self.telegram.as_ref().is_some_and(|c| c.is_empty()))
            && (self.rest.is_none() || self.rest.as_ref().is_some_and(|c| c.is_empty()))
            && (self.pushover.is_none() || self.pushover.as_ref().is_some_and(|c| c.is_empty()))
            && (self.discord.is_none() || self.discord.as_ref().is_some_and(|c| c.is_empty()))
    }

    pub fn clean(&mut self) {
        if self.telegram.as_ref().is_some_and(|c| c.is_empty()) {
            self.telegram = None;
        }
        if self.rest.as_ref().is_some_and(|c| c.is_empty()) {
            self.rest = None;
        }
        if self.pushover.as_ref().is_some_and(|c| c.is_empty()) {
            self.pushover = None;
        }
        if self.discord.as_ref().is_some_and(|c| c.is_empty()) {
            self.discord = None;
        }
        if self.disk_alert.as_ref().is_some_and(|c| c.is_empty()) {
            self.disk_alert = None;
        }
    }

    pub fn prepare(&mut self, _include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(disk) = &self.disk_alert {
            disk.prepare()?;
        }
        Ok(())
    }
}
