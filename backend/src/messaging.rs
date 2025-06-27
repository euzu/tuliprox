use std::sync::Arc;
use crate::model::{MessagingConfig};
use log::{debug, error};
use reqwest::{header};
use shared::model::MsgKind;

fn is_enabled(kind: MsgKind, cfg: &MessagingConfig) -> bool {
    cfg.notify_on.contains(&kind)
}

fn send_http_post_request(client: &Arc<reqwest::Client>, msg: &str, messaging: &MessagingConfig) {
    if let Some(rest) = &messaging.rest {
        let url = rest.url.clone();
        let data = msg.to_owned();
        let the_client = Arc::clone(client);
        tokio::spawn(async move {
            match the_client
                .post(&url)
                .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(data)
                .send()
                .await
            {
                Ok(_) => debug!("Text message sent successfully to rest api"),
                Err(e) => error!("Text message wasn't sent to rest api because of: {e}"),
            }
        });
    }
}

fn send_telegram_message(msg: &str, messaging: &MessagingConfig) {
    // TODO use proxy settings
    if let Some(telegram) = &messaging.telegram {
        for chat_id in &telegram.chat_ids {
            let bot = rustelebot::create_instance(&telegram.bot_token, chat_id);
            match rustelebot::send_message(&bot, msg, None) {
                Ok(()) => debug!("Text message sent successfully to {chat_id}"),
                Err(e) => error!("Text message wasn't sent to {chat_id} because of: {e}")
            }
        }
    }
}

fn send_pushover_message(client: &Arc<reqwest::Client>, msg: &str, messaging: &MessagingConfig) {
    if let Some(pushover) = &messaging.pushover {
        let encoded_message: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", pushover.token.as_str())
            .append_pair("user", pushover.user.as_str())
            .append_pair("message", msg)
            .finish();
        let the_client = Arc::clone(client);
        let pushover_url = pushover.url.to_string();
        tokio::spawn(async move {
            match the_client
                .post(pushover_url)
                .header(header::CONTENT_TYPE, mime::APPLICATION_WWW_FORM_URLENCODED.to_string())
                .body(encoded_message)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        debug!("Text message sent successfully to PUSHOVER, status code {}", response.status());
                    } else {
                        error!("Failed to send text message to PUSHOVER, status code {}", response.status());
                    }
                },
                Err(e) => error!("Text message wasn't sent to PUSHOVER api because of: {e}"),
            }
        });
    }
}

pub fn send_message(client: &Arc<reqwest::Client>, kind: &MsgKind, cfg: Option<&MessagingConfig>, msg: &str) {
    if let Some(messaging) = cfg {
        if is_enabled(*kind, messaging) {
            send_telegram_message(msg, messaging);
            send_http_post_request(client, msg, messaging);
            send_pushover_message(client, msg, messaging);
        }
    }
}
