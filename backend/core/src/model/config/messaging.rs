use crate::model::macros;
use shared::model::{
    notification::{EventId, EventSubscription, QuietHours, Severity},
    ChannelRoutingDto, DiskAlertConfigDto, MessagingConfigDto,
};

/// Runtime configuration for the threshold-driven disk-space alert.
#[derive(Debug, Clone)]
pub struct DiskAlertConfig {
    /// Disk usage at or above this percentage triggers the warning state.
    pub warn_percent: f64,
    /// Disk usage at or above this percentage triggers the critical state.
    pub critical_percent: f64,
    /// Re-send the alert after this many seconds if the disk is still in
    /// the same alert state.
    pub repeat_interval_secs: u64,
}

impl Default for DiskAlertConfig {
    fn default() -> Self {
        Self {
            warn_percent: DiskAlertConfigDto::default().warn_percent,
            critical_percent: DiskAlertConfigDto::default().critical_percent,
            repeat_interval_secs: DiskAlertConfigDto::default().repeat_interval_secs,
        }
    }
}

macros::from_impl!(DiskAlertConfig);
impl From<&DiskAlertConfigDto> for DiskAlertConfig {
    fn from(dto: &DiskAlertConfigDto) -> Self {
        Self {
            warn_percent: dto.warn_percent,
            critical_percent: dto.critical_percent,
            repeat_interval_secs: dto.repeat_interval_secs,
        }
    }
}

impl From<&DiskAlertConfig> for DiskAlertConfigDto {
    fn from(instance: &DiskAlertConfig) -> Self {
        Self {
            warn_percent: instance.warn_percent,
            critical_percent: instance.critical_percent,
            repeat_interval_secs: instance.repeat_interval_secs,
        }
    }
}

/// Per-channel routing, resolved at config load.
#[derive(Debug, Clone, Default)]
pub struct ChannelRouting {
    /// Overrides the global subscription when non-empty.
    pub subscription: Option<EventSubscription>,
    pub min_severity: Option<Severity>,
    pub quiet_hours: Option<QuietHours>,
    pub max_per_hour: Option<u32>,
    pub dedup_window: Option<std::time::Duration>,
}

impl ChannelRouting {
    /// Does this channel accept `event` at `severity`?
    ///
    /// Quiet hours are deliberately *not* checked here: they defer delivery
    /// rather than reject it, so they belong to the outbox schedule, not to
    /// the routing decision.
    #[must_use]
    pub fn accepts(&self, event: EventId, severity: Severity) -> bool {
        if self.min_severity.is_some_and(|min| severity < min) {
            return false;
        }
        match &self.subscription {
            Some(subscription) => subscription.matches(event),
            None => true,
        }
    }
}

impl From<&ChannelRoutingDto> for ChannelRouting {
    fn from(dto: &ChannelRoutingDto) -> Self {
        Self {
            subscription: if dto.notify_on.is_empty() {
                None
            } else {
                Some(EventSubscription::parse(dto.notify_on.iter()))
            },
            min_severity: dto.min_severity.as_deref().and_then(Severity::from_wire),
            quiet_hours: dto.quiet_hours.as_deref().and_then(QuietHours::parse),
            max_per_hour: dto.max_per_hour,
            dedup_window: dto.dedup_window_secs.map(std::time::Duration::from_secs),
        }
    }
}

impl From<&ChannelRouting> for ChannelRoutingDto {
    fn from(instance: &ChannelRouting) -> Self {
        Self {
            notify_on: instance
                .subscription
                .as_ref()
                .map(|s| s.patterns().iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
            min_severity: instance.min_severity.map(|s| s.wire_name().to_string()),
            quiet_hours: None,
            max_per_hour: instance.max_per_hour,
            dedup_window_secs: instance.dedup_window.map(|d| d.as_secs()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessagingConfig {
    /// Raw `notify_on` entries as written in config. Glob patterns and
    /// legacy `MsgKind` names are both accepted.
    pub notify_on: Vec<String>,
    /// Optional disk-space alert config. When `None`, no alert is fired.
    pub disk_alert: Option<DiskAlertConfig>,
    /// `notify_on` parsed once at load. Matching runs per notification, so
    /// re-parsing the patterns each time would be wasteful.
    subscription: EventSubscription,
}

impl MessagingConfig {
    /// The parsed `notify_on` subscription.
    #[must_use]
    pub fn subscription(&self) -> &EventSubscription { &self.subscription }
}

macros::from_impl!(MessagingConfig);
impl From<&MessagingConfigDto> for MessagingConfig {
    fn from(dto: &MessagingConfigDto) -> Self {
        Self {
            notify_on: dto.notify_on.clone(),
            subscription: EventSubscription::parse(dto.notify_on.iter()),
            disk_alert: dto.disk_alert.as_ref().map(Into::into),
        }
    }
}

impl From<&MessagingConfig> for MessagingConfigDto {
    fn from(instance: &MessagingConfig) -> Self {
        Self { notify_on: instance.notify_on.clone(), disk_alert: instance.disk_alert.as_ref().map(Into::into) }
    }
}
