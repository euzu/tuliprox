use crate::model::MsgKind;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelegramMessagingConfigDto {
    pub bot_token: String,
    pub chat_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RestMessagingConfigDto {
    pub url: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PushoverMessagingConfigDto {
    pub url: Option<String>,
    pub token: String,
    pub user: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MessagingConfigDto {
    #[serde(default)]
    pub notify_on: Vec<MsgKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<RestMessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover: Option<PushoverMessagingConfigDto>,

}