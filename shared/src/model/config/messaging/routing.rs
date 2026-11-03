use super::MessagingConfigDto;
use crate::{
    defaults::{default_critical_percent, default_repeat_interval_secs, default_warn_percent},
    error::TuliproxError,
    utils::{is_blank_optional_str, is_blank_optional_string},
};

/// Per-channel routing.
///
/// Without this every enabled event went to every configured channel, so
/// "critical to Pushover, everything to Discord" was not expressible.
/// Every field is optional and an omitted block inherits the global
/// `notify_on`.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChannelRoutingDto {
    /// Overrides the global `notify_on` for this channel. Same glob
    /// grammar. Empty means inherit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_on: Vec<String>,
    /// Drop anything below this severity: `info`, `warn`, `error`,
    /// `critical`.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub min_severity: Option<String>,
    /// `HH:MM-HH:MM` local time. Notifications inside the window are
    /// deferred by the outbox, never dropped - an overnight outage must not
    /// be silently invisible.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub quiet_hours: Option<String>,
    /// Circuit breaker. On reaching it the channel sends one "suppressing
    /// further notifications" message and then goes quiet for the rest of
    /// the hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<u32>,
    /// Suppress a repeated `dedup_key` for this many seconds. Generalizes
    /// the disk alert's `repeat_interval_secs`, which was only available to
    /// disk alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_window_secs: Option<u64>,
}

impl ChannelRoutingDto {
    pub fn is_empty(&self) -> bool {
        self.notify_on.is_empty()
            && is_blank_optional_str(self.min_severity.as_deref())
            && is_blank_optional_str(self.quiet_hours.as_deref())
            && self.max_per_hour.is_none()
            && self.dedup_window_secs.is_none()
    }

    /// Reject values that would otherwise fail silently at send time.
    pub fn prepare(&self) -> Result<(), TuliproxError> {
        if let Some(severity) = self.min_severity.as_deref().filter(|s| !s.trim().is_empty()) {
            if crate::model::notification::Severity::from_wire(severity).is_none() {
                return Err(TuliproxError::Config(format!(
                    "messaging min_severity must be one of info, warn, error, critical; got `{severity}`"
                )));
            }
        }
        if let Some(window) = self.quiet_hours.as_deref().filter(|s| !s.trim().is_empty()) {
            if crate::model::notification::QuietHours::parse(window).is_none() {
                return Err(TuliproxError::Config(format!(
                    "messaging quiet_hours must look like `23:00-07:00`; got `{window}`"
                )));
            }
        }
        Ok(())
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

impl MessagingConfigDto {
    pub fn prepare(&mut self, _include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(disk) = &self.disk_alert {
            disk.prepare()?;
        }
        for routing in [
            self.telegram.as_ref().and_then(|c| c.routing.as_ref()),
            self.rest.as_ref().and_then(|c| c.routing.as_ref()),
            self.discord.as_ref().and_then(|c| c.routing.as_ref()),
            self.pushover.as_ref().and_then(|c| c.routing.as_ref()),
            self.ntfy.as_ref().and_then(|c| c.routing.as_ref()),
            self.gotify.as_ref().and_then(|c| c.routing.as_ref()),
            self.slack.as_ref().and_then(|c| c.routing.as_ref()),
            self.command.as_ref().and_then(|c| c.routing.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            routing.prepare()?;
        }
        self.normalize_notify_on();
        Ok(())
    }

    /// Rewrite legacy `MsgKind` names to their canonical event ids.
    ///
    /// Keeps the UI's selection state consistent with the registry-driven
    /// option list, and migrates an old config the next time it is saved.
    /// Glob patterns are left exactly as written.
    pub(super) fn normalize_notify_on(&mut self) {
        for entry in &mut self.notify_on {
            if entry.contains('*') || entry.starts_with('!') {
                continue;
            }
            if let Some(id) = crate::model::notification::EventId::from_wire(entry) {
                *entry = id.as_str().to_string();
            }
        }
    }
}
