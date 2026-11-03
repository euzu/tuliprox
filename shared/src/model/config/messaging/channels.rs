use super::ChannelRoutingDto;
use crate::defaults::is_false;

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
