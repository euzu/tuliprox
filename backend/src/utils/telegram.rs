use std::sync::Arc;
use log::{debug, error};
use url::Url;
use crate::model::AppConfig;

/// Requests will be sent according to bot instance.
#[derive(Clone)]
pub struct BotInstance {
    pub bot_token: String,
    pub chat_id: String,
    pub message_thread_id: Option<String>,
}

/// Telegram's error result.
#[derive(Debug, serde::Deserialize)]
struct TelegramErrorResult {
    #[allow(unused)]
    pub ok: bool,
    #[allow(unused)]
    pub error_code: i32,
    pub description: String,
}

/// Parse mode for `sendMessage` API
pub enum SendMessageParseMode {
    MarkdownV2,
    HTML,
}

/// Options which can be used with `sendMessage` API
pub struct SendMessageOption {
    pub parse_mode: SendMessageParseMode,
}

fn get_send_message_parse_mode_str(mode: &SendMessageParseMode) -> &'static str {
    match mode {
        SendMessageParseMode::MarkdownV2 => "MarkdownV2",
        SendMessageParseMode::HTML => "HTML",
    }
}

#[derive(Debug, serde::Serialize)]
struct RequestObj {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
}

/// Create an instance to interact with APIs.
pub fn telegram_create_instance(bot_token: &str, chat_id: &str) -> BotInstance {
    // chat-id:thread-id
    let mut parts = chat_id.splitn(2, ':');
    let chat_id_part = parts.next().unwrap_or_default();
    let thread_id_part = parts.next().map(ToString::to_string);

    BotInstance {
        bot_token: bot_token.to_string(),
        chat_id: chat_id_part.to_string(),
        message_thread_id: thread_id_part,
    }
}

pub async fn telegram_send_message(
    _app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    instance: &BotInstance,
    msg: &str,
    options: Option<&SendMessageOption>,
) {
    let chat_id = instance.chat_id.clone();
    let raw_url_str = format!("https://api.telegram.org/bot{}/sendMessage", instance.bot_token);
    let url = match Url::parse(&raw_url_str) {
        Ok(url) => url,
        Err(e) => {
            error!("Message wasn't sent to {chat_id} telegram api because of: {e}");
            return;
        }
    };

    let request_json_obj = RequestObj {
        chat_id: instance.chat_id.clone(),
        message_thread_id: instance.message_thread_id.clone(),
        text: msg.to_string(),
        parse_mode: options
            .map(|o| get_send_message_parse_mode_str(&o.parse_mode))
            .map(ToString::to_string),
    };

    let result = client
    .post(url)
    .json(&request_json_obj)
    .send()
    .await;

    match result {
        Ok(response) => {
            if response.status().is_success() {
                debug!("Message sent successfully to {chat_id} telegram api");
            } else {
                match response.json::<TelegramErrorResult>().await {
                    Ok(json) => error!("Message wasn't sent to {chat_id} telegram api because of: {}", json.description),
                    Err(_) => error!("Message wasn't sent to {chat_id} telegram api. Telegram response could not be parsed!"),
                }
            }
        },
        Err(e) => error!("Message wasn't sent to {chat_id} telegram api because of: {e}"),
    }
}


