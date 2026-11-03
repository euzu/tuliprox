/// Placeholder substituted for a channel secret on the way out to a client.
///
/// A client that sends it back unchanged means "keep what is stored"; see
/// [`MessagingConfigDto::restore_redacted_secrets`].
pub const REDACTED_SECRET: &str = "********";

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
}

#[cfg(test)]
mod tests;

mod channels;
mod routing;
mod secrets;
pub use channels::{
    CommandMessagingConfigDto, DiscordMessagingConfigDto, GotifyMessagingConfigDto, NtfyMessagingConfigDto,
    PushoverMessagingConfigDto, RestMessagingConfigDto, SlackMessagingConfigDto, TelegramMessagingConfigDto,
};
pub use routing::{ChannelRoutingDto, DiskAlertConfigDto};
