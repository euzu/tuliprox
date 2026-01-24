use crate::error::TuliproxError;
use crate::model::MsgKind;
use crate::utils::{is_false, is_blank_optional_string, is_blank_optional_str};

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
        self.bot_token.trim().is_empty()
            && self.chat_ids.is_empty()
            && self.templates.is_empty()
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
}

impl PushoverMessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        is_blank_optional_str(self.url.as_deref())
            && self.token.trim().is_empty()
            && self.user.trim().is_empty()
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
}

impl MessagingConfigDto {
    pub fn is_empty(&self) -> bool {
        self.notify_on.is_empty()
            && (self.telegram.is_none() || self.telegram.as_ref().is_some_and(|c| c.is_empty()))
            && (self.rest.is_none()  || self.rest.as_ref().is_some_and(|c| c.is_empty()))
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
    }

    pub fn prepare(&mut self, _include_computed: bool) -> Result<(), TuliproxError> {
        Ok(())
    }
}