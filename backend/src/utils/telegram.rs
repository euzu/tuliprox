use crate::model::AppConfig;
use log::{debug, error};
use std::sync::Arc;
use url::Url;

const MAX_MESSAGE_LENGTH: usize = 4000;

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

    // Split message into chunks if it exceeds MAX_MESSAGE_LENGTH
    let chars: Vec<char> = msg.chars().collect();
    let chunks = chars.chunks(MAX_MESSAGE_LENGTH)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<String>>();

    for (i, chunk_text) in chunks.iter().enumerate() {
        let request_json_obj = RequestObj {
            chat_id: instance.chat_id.clone(),
            message_thread_id: instance.message_thread_id.clone(),
            text: chunk_text.clone(),
            parse_mode: options
                .map(|o| get_send_message_parse_mode_str(&o.parse_mode))
                .map(ToString::to_string),
        };

        let result = client
            .post(url.clone())
            .json(&request_json_obj)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match result {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("Message chunk {}/{} sent successfully to {chat_id} telegram api", i + 1, chunks.len());
                } else {
                    match response.json::<TelegramErrorResult>().await {
                        Ok(json) => error!("Message chunk {}/{} wasn't sent to {chat_id} telegram api because of: {}", i + 1, chunks.len(), json.description),
                        Err(_) => error!("Message chunk {}/{} wasn't sent to {chat_id} telegram api. Telegram response could not be parsed!", i + 1, chunks.len()),
                    }
                }
            }
            Err(e) => error!("Message chunk {}/{} wasn't sent to {chat_id} telegram api because of: {e}", i + 1, chunks.len()),
        }

        // Small delay between chunks to be polite to the API
        if i < chunks.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}