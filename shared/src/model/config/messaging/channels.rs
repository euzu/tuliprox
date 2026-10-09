use super::ChannelRoutingDto;
use crate::{
    defaults::is_false,
    utils::{is_blank_optional_str, is_blank_optional_string},
};

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
    pub fn is_empty(&self) -> bool { self.url.trim().is_empty() && self.templates.is_empty() }
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
    pub fn is_empty(&self) -> bool { self.url.trim().is_empty() && self.topic.trim().is_empty() }
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
    pub fn is_empty(&self) -> bool { self.url.trim().is_empty() && self.token.trim().is_empty() }
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
    pub fn is_empty(&self) -> bool { self.url.trim().is_empty() }
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
    pub fn is_empty(&self) -> bool { self.program.trim().is_empty() }
}
