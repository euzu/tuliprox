use crate::{
    defaults::{default_critical_percent, default_repeat_interval_secs, default_warn_percent, is_false},
    error::TuliproxError,
    utils::{is_blank_optional_str, is_blank_optional_string},
};

/// Placeholder substituted for a channel secret on the way out to a client.
///
/// A client that sends it back unchanged means "keep what is stored"; see
/// [`MessagingConfigDto::restore_redacted_secrets`].
pub const REDACTED_SECRET: &str = "********";

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

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelegramMessagingConfigDto {
    pub bot_token: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chat_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub markdown: bool,
    /// Template per event, keyed by event id wire name.
    ///
    /// Legacy `MsgKind` names (`info`, `stats`, `disk_alert`, ...) and
    /// canonical dotted ids (`recording.completed`) are both accepted;
    /// `MessagingConfig::prepare` resolves them and warns on a key that
    /// matches no known event.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    /// Per-channel routing. Inherits the global `notify_on` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
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
    /// HMAC-SHA256 signing secret.
    ///
    /// When set, each request carries `X-Tuliprox-Timestamp` and
    /// `X-Tuliprox-Signature: sha256=<hex>` over `timestamp.body`, so the
    /// receiving endpoint can verify the sender.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub signing_secret: Option<String>,
    /// Template per event, keyed by event id wire name.
    ///
    /// Legacy `MsgKind` names (`info`, `stats`, `disk_alert`, ...) and
    /// canonical dotted ids (`recording.completed`) are both accepted;
    /// `MessagingConfig::prepare` resolves them and warns on a key that
    /// matches no known event.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    /// Per-channel routing. Inherits the global `notify_on` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
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
    /// Template per event, keyed by event id wire name.
    ///
    /// Legacy `MsgKind` names (`info`, `stats`, `disk_alert`, ...) and
    /// canonical dotted ids (`recording.completed`) are both accepted;
    /// `MessagingConfig::prepare` resolves them and warns on a key that
    /// matches no known event.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    /// Per-channel routing. Inherits the global `notify_on` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl DiscordMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty() && self.templates.is_empty()
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PushoverMessagingConfigDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub url: Option<String>,
    pub token: String,
    pub user: String,
    /// Template per event, keyed by event id wire name.
    ///
    /// Pushover previously had no template support at all, so every
    /// notification took the built-in text - which for watch changes and
    /// playlist stats was a raw `serde_json` dump pushed to a phone.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    /// Per-channel routing. Inherits the global `notify_on` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl PushoverMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        is_blank_optional_str(self.url.as_deref())
            && self.token.trim().is_empty()
            && self.user.trim().is_empty()
            && self.templates.is_empty()
    }
}

/// [ntfy](https://ntfy.sh) - self-hosted push, no account, no bot token.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NtfyMessagingConfigDto {
    /// Server base URL, e.g. `https://ntfy.sh`.
    pub url: String,
    /// Topic to publish to.
    pub topic: String,
    /// Bearer token for a protected topic.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl NtfyMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty() && self.topic.trim().is_empty()
    }
}

/// [Gotify](https://gotify.net) - same audience as ntfy, same shape.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GotifyMessagingConfigDto {
    /// Server base URL.
    pub url: String,
    /// Application token.
    pub token: String,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl GotifyMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty() && self.token.trim().is_empty()
    }
}

/// Slack incoming webhook.
///
/// Not a Discord clone: Block Kit differs enough from Discord embeds that
/// reusing the Discord payload shape produces bad output.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SlackMessagingConfigDto {
    pub url: String,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl SlackMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.url.trim().is_empty()
    }
}

/// Run a local program, with the event JSON on stdin.
///
/// The escape hatch that means nobody has to wait for a channel to be
/// added upstream. This runs arbitrary code as the tuliprox process user,
/// so it is opt-in and never configured by default.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandMessagingConfigDto {
    /// Program to run. Not passed through a shell, so no quoting rules and
    /// no shell injection.
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Kill the child after this many seconds. Defaults to 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChannelRoutingDto>,
}

impl CommandMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.program.trim().is_empty()
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
    /// Which events to notify on.
    ///
    /// Glob patterns over event ids: `*`, `recording.*`,
    /// `provider.*.expired`, `recording.completed`, and a leading `!` to
    /// exclude. Legacy `MsgKind` names (`info`, `stats`, `disk_alert`, ...)
    /// are still accepted and resolve to their canonical ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<RestMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover: Option<PushoverMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntfy: Option<NtfyMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gotify: Option<GotifyMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_alert: Option<DiskAlertConfigDto>,
}

impl MessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.notify_on.is_empty()
            && (self.disk_alert.is_none() || self.disk_alert.as_ref().is_some_and(DiskAlertConfigDto::is_empty))
            && (self.telegram.is_none() || self.telegram.as_ref().is_some_and(TelegramMessagingConfigDto::is_empty))
            && (self.rest.is_none() || self.rest.as_ref().is_some_and(RestMessagingConfigDto::is_empty))
            && (self.pushover.is_none() || self.pushover.as_ref().is_some_and(PushoverMessagingConfigDto::is_empty))
            && (self.discord.is_none() || self.discord.as_ref().is_some_and(DiscordMessagingConfigDto::is_empty))
            && (self.ntfy.is_none() || self.ntfy.as_ref().is_some_and(NtfyMessagingConfigDto::is_empty))
            && (self.gotify.is_none() || self.gotify.as_ref().is_some_and(GotifyMessagingConfigDto::is_empty))
            && (self.slack.is_none() || self.slack.as_ref().is_some_and(SlackMessagingConfigDto::is_empty))
            && (self.command.is_none() || self.command.as_ref().is_some_and(CommandMessagingConfigDto::is_empty))
    }

    /// Replace channel secrets with [`REDACTED_SECRET`].
    ///
    /// The config GET returns `config.yml` in full to any client with
    /// `ConfigRead`, which included the Telegram bot token, the Pushover
    /// token and user key, and any `Authorization` header configured on the
    /// REST channel.
    pub fn redact_secrets(&mut self) {
        if let Some(telegram) = self.telegram.as_mut() {
            redact(&mut telegram.bot_token);
        }
        if let Some(pushover) = self.pushover.as_mut() {
            redact(&mut pushover.token);
            redact(&mut pushover.user);
        }
        if let Some(ntfy) = self.ntfy.as_mut() {
            if let Some(token) = ntfy.token.as_mut() {
                redact(token);
            }
        }
        if let Some(gotify) = self.gotify.as_mut() {
            redact(&mut gotify.token);
        }
        if let Some(rest) = self.rest.as_mut() {
            if let Some(secret) = rest.signing_secret.as_mut() {
                redact(secret);
            }
            for header in &mut rest.headers {
                // `Name: value` - keep the name, mask the value.
                if let Some((name, _)) = header.split_once(':') {
                    if is_sensitive_header(name) {
                        *header = format!("{name}: {REDACTED_SECRET}");
                    }
                }
            }
        }
    }

    /// Put back any secret the client returned still redacted.
    ///
    /// Without this a round-trip through the UI would overwrite the stored
    /// token with the mask, silently breaking the channel.
    pub fn restore_redacted_secrets(&mut self, current: &Self) {
        if let (Some(incoming), Some(stored)) = (self.telegram.as_mut(), current.telegram.as_ref()) {
            restore(&mut incoming.bot_token, &stored.bot_token);
        }
        if let (Some(incoming), Some(stored)) = (self.pushover.as_mut(), current.pushover.as_ref()) {
            restore(&mut incoming.token, &stored.token);
            restore(&mut incoming.user, &stored.user);
        }
        if let (Some(incoming), Some(stored)) = (self.ntfy.as_mut(), current.ntfy.as_ref()) {
            if let (Some(token), Some(original)) = (incoming.token.as_mut(), stored.token.as_ref()) {
                restore(token, original);
            }
        }
        if let (Some(incoming), Some(stored)) = (self.gotify.as_mut(), current.gotify.as_ref()) {
            restore(&mut incoming.token, &stored.token);
        }
        if let (Some(incoming), Some(stored)) = (self.rest.as_mut(), current.rest.as_ref()) {
            if let (Some(secret), Some(original)) = (incoming.signing_secret.as_mut(), stored.signing_secret.as_ref()) {
                restore(secret, original);
            }
            for header in &mut incoming.headers {
                let Some((name, value)) = header.split_once(':') else { continue };
                if value.trim() != REDACTED_SECRET {
                    continue;
                }
                let name = name.to_string();
                if let Some(original) =
                    stored.headers.iter().find(|h| h.split_once(':').is_some_and(|(n, _)| n.trim() == name.trim()))
                {
                    *header = original.clone();
                }
            }
        }
    }

    pub fn clean(&mut self) {
        if self.telegram.as_ref().is_some_and(TelegramMessagingConfigDto::is_empty) {
            self.telegram = None;
        }
        if self.rest.as_ref().is_some_and(RestMessagingConfigDto::is_empty) {
            self.rest = None;
        }
        if self.pushover.as_ref().is_some_and(PushoverMessagingConfigDto::is_empty) {
            self.pushover = None;
        }
        if self.discord.as_ref().is_some_and(DiscordMessagingConfigDto::is_empty) {
            self.discord = None;
        }
        if self.ntfy.as_ref().is_some_and(NtfyMessagingConfigDto::is_empty) {
            self.ntfy = None;
        }
        if self.gotify.as_ref().is_some_and(GotifyMessagingConfigDto::is_empty) {
            self.gotify = None;
        }
        if self.slack.as_ref().is_some_and(SlackMessagingConfigDto::is_empty) {
            self.slack = None;
        }
        if self.command.as_ref().is_some_and(CommandMessagingConfigDto::is_empty) {
            self.command = None;
        }
        if self.disk_alert.as_ref().is_some_and(DiskAlertConfigDto::is_empty) {
            self.disk_alert = None;
        }
    }

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
    fn normalize_notify_on(&mut self) {
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

fn redact(value: &mut String) {
    if !value.trim().is_empty() {
        *value = REDACTED_SECRET.to_string();
    }
}

fn restore(incoming: &mut String, stored: &str) {
    if incoming.trim() == REDACTED_SECRET {
        *incoming = stored.to_string();
    }
}

/// Header names whose value is a credential.
fn is_sensitive_header(name: &str) -> bool {
    const SENSITIVE: &[&str] = &["authorization", "x-api-key", "x-auth-token", "proxy-authorization", "cookie"];
    let name = name.trim();
    SENSITIVE.iter().any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_preserves_disk_alert_with_default_values() {
        let mut messaging =
            MessagingConfigDto { disk_alert: Some(DiskAlertConfigDto::default()), ..Default::default() };
        messaging.clean();
        assert_eq!(messaging.disk_alert, None);
    }

    #[test]
    fn clean_preserves_disk_alert_with_non_default_values() {
        let custom = DiskAlertConfigDto { warn_percent: 70.0, critical_percent: 90.0, repeat_interval_secs: 600 };
        let mut messaging = MessagingConfigDto { disk_alert: Some(custom.clone()), ..Default::default() };
        messaging.clean();
        assert_eq!(messaging.disk_alert, Some(custom));
    }

    #[test]
    fn clean_strips_empty_subconfigs() {
        let mut messaging = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto::default()),
            rest: Some(RestMessagingConfigDto::default()),
            pushover: Some(PushoverMessagingConfigDto::default()),
            discord: Some(DiscordMessagingConfigDto::default()),
            ..Default::default()
        };
        messaging.clean();
        assert!(messaging.telegram.is_none());
        assert!(messaging.rest.is_none());
        assert!(messaging.pushover.is_none());
        assert!(messaging.discord.is_none());
    }

    #[test]
    fn prepare_rewrites_legacy_notify_on_names_to_canonical_ids() {
        let mut messaging = MessagingConfigDto {
            notify_on: vec!["disk_alert".to_string(), "recording_completed".to_string()],
            ..Default::default()
        };
        messaging.prepare(false).expect("prepare");
        assert_eq!(messaging.notify_on, vec!["system.disk.alert", "recording.completed"]);
    }

    #[test]
    fn prepare_leaves_glob_patterns_untouched() {
        let mut messaging = MessagingConfigDto {
            notify_on: vec!["recording.*".to_string(), "!system.info".to_string(), "*".to_string()],
            ..Default::default()
        };
        messaging.prepare(false).expect("prepare");
        assert_eq!(messaging.notify_on, vec!["recording.*", "!system.info", "*"]);
    }

    #[test]
    fn redaction_masks_every_channel_secret() {
        let mut messaging = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto { bot_token: "real-token".to_string(), ..Default::default() }),
            pushover: Some(PushoverMessagingConfigDto {
                token: "real-app".to_string(),
                user: "real-user".to_string(),
                ..Default::default()
            }),
            rest: Some(RestMessagingConfigDto {
                url: "https://example.test".to_string(),
                headers: vec!["Authorization: Bearer secret".to_string(), "X-Trace: keep-me".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        messaging.redact_secrets();
        assert_eq!(messaging.telegram.as_ref().expect("telegram").bot_token, REDACTED_SECRET);
        assert_eq!(messaging.pushover.as_ref().expect("pushover").token, REDACTED_SECRET);
        assert_eq!(messaging.pushover.as_ref().expect("pushover").user, REDACTED_SECRET);
        let headers = &messaging.rest.as_ref().expect("rest").headers;
        assert!(headers.contains(&format!("Authorization: {REDACTED_SECRET}")), "auth header not masked: {headers:?}");
        // A non-credential header keeps its value; masking everything would
        // make the config unreadable for no gain.
        assert!(headers.contains(&"X-Trace: keep-me".to_string()), "harmless header was masked: {headers:?}");
    }

    #[test]
    fn an_empty_secret_is_not_replaced_by_a_mask() {
        // Otherwise an unconfigured channel would look configured.
        let mut messaging =
            MessagingConfigDto { telegram: Some(TelegramMessagingConfigDto::default()), ..Default::default() };
        messaging.redact_secrets();
        assert_eq!(messaging.telegram.as_ref().expect("telegram").bot_token, "");
    }

    #[test]
    fn a_returned_mask_restores_the_stored_secret() {
        // Without this the UI round-trip would overwrite the real token with
        // the mask and silently break the channel.
        let stored = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto { bot_token: "real-token".to_string(), ..Default::default() }),
            ..Default::default()
        };
        let mut incoming = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto { bot_token: REDACTED_SECRET.to_string(), ..Default::default() }),
            ..Default::default()
        };
        incoming.restore_redacted_secrets(&stored);
        assert_eq!(incoming.telegram.as_ref().expect("telegram").bot_token, "real-token");
    }

    #[test]
    fn a_genuinely_changed_secret_is_written_through() {
        let stored = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto { bot_token: "old".to_string(), ..Default::default() }),
            ..Default::default()
        };
        let mut incoming = MessagingConfigDto {
            telegram: Some(TelegramMessagingConfigDto { bot_token: "new".to_string(), ..Default::default() }),
            ..Default::default()
        };
        incoming.restore_redacted_secrets(&stored);
        assert_eq!(incoming.telegram.as_ref().expect("telegram").bot_token, "new");
    }

    #[test]
    fn a_returned_masked_header_restores_the_stored_one() {
        let stored = MessagingConfigDto {
            rest: Some(RestMessagingConfigDto {
                url: "https://example.test".to_string(),
                headers: vec!["Authorization: Bearer secret".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut incoming = MessagingConfigDto {
            rest: Some(RestMessagingConfigDto {
                url: "https://example.test".to_string(),
                headers: vec![format!("Authorization: {REDACTED_SECRET}")],
                ..Default::default()
            }),
            ..Default::default()
        };
        incoming.restore_redacted_secrets(&stored);
        assert_eq!(incoming.rest.as_ref().expect("rest").headers, vec!["Authorization: Bearer secret".to_string()]);
    }

    #[test]
    fn is_empty_default_messaging() {
        assert!(MessagingConfigDto::default().is_empty());
    }

    #[test]
    fn is_empty_with_only_notify_on_is_not_empty() {
        let messaging = MessagingConfigDto { notify_on: vec!["disk_alert".to_string()], ..Default::default() };
        assert!(!messaging.is_empty());
    }
}
