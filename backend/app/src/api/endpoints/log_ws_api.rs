use crate::{
    api::model::AppState,
    auth::TokenVerifier,
    utils::{get_log_history, subscribe_logs},
};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use log::{trace, warn};
use serde::Deserialize;
use shared::{
    model::{LogLevel, LogWsMessage, Permission, WsCloseCode},
    utils::concat_path_leading_slash,
};
use std::sync::Arc;

#[derive(Debug, Deserialize, Default)]
pub struct LogWsQuery {
    pub token: Option<String>,
    pub min_level: Option<String>,
}

/// The verifier this socket authenticates against.
///
/// This used to hand a bare `Vec<u8>` secret around, which is precisely why
/// the WebSocket paths could not check the token's issuer.
fn get_token_verifier(app_state: &AppState, auth_required: bool) -> Option<TokenVerifier> {
    if !auth_required {
        return None;
    }
    app_state.app_config.config.load().web_ui.as_ref().and_then(|c| c.auth.as_ref()).map(TokenVerifier::from_config)
}

fn check_token_auth(token: &str, verifier: Option<&TokenVerifier>) -> bool {
    verifier.and_then(|verifier| verifier.verify(token)).is_some_and(|token_data| {
        token_data.claims.permissions.contains(Permission::SystemRead) || token_data.claims.is_admin()
    })
}

async fn wait_for_socket_auth(socket: &mut WebSocket, verifier: Option<&TokenVerifier>) -> bool {
    let auth_timeout = tokio::time::sleep(tokio::time::Duration::from_secs(10));
    tokio::pin!(auth_timeout);

    loop {
        tokio::select! {
            () = &mut auth_timeout => {
                let _ = socket.send(Message::Close(Some(CloseFrame {
                    code: WsCloseCode::Protocol.code(),
                    reason: "Auth timeout".into(),
                }))).await;
                return false;
            }
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else {
                    return false;
                };
                match msg {
                    Message::Text(text) => {
                        if let Ok(LogWsMessage::Auth(token)) = serde_json::from_str::<LogWsMessage>(&text) {
                            if check_token_auth(&token, verifier) {
                                let auth_ok = serde_json::to_string(&LogWsMessage::Authorized).unwrap_or_default();
                                let _ = socket.send(Message::Text(auth_ok.into())).await;
                                return true;
                            }
                            let auth_fail = serde_json::to_string(&LogWsMessage::Unauthorized).unwrap_or_default();
                            let _ = socket.send(Message::Text(auth_fail.into())).await;
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: 1008, // Policy Violation
                                reason: "Unauthorized".into(),
                            }))).await;
                            return false;
                        }
                    }
                    Message::Ping(p) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Message::Close(_) => return false,
                    _ => {}
                }
            }
        }
    }
}

async fn handle_socket(mut socket: WebSocket, app_state: Arc<AppState>, auth_required: bool, query: LogWsQuery) {
    let verifier = get_token_verifier(&app_state, auth_required);
    let mut is_authorized = !auth_required;
    let mut min_level: Option<LogLevel> = query.min_level.as_deref().and_then(|s| s.parse().ok());

    if !is_authorized {
        if let Some(token) = query.token.as_deref() {
            if check_token_auth(token, verifier.as_ref()) {
                is_authorized = true;
            }
        }
    }

    if !is_authorized && !wait_for_socket_auth(&mut socket, verifier.as_ref()).await {
        return;
    }

    // Send history
    let history = get_log_history();
    let filtered_history: Vec<_> =
        history.into_iter().filter(|e| min_level.is_none_or(|lvl| e.level.matches(lvl))).collect();

    if let Ok(history_json) = serde_json::to_string(&LogWsMessage::History(filtered_history)) {
        if socket.send(Message::Text(history_json.into())).await.is_err() {
            return;
        }
    }

    let mut rx = subscribe_logs();

    loop {
        tokio::select! {
            broadcast_res = rx.recv() => {
                match broadcast_res {
                    Ok(entry) => {
                        if min_level.is_none_or(|lvl| entry.level.matches(lvl)) {
                            if let Ok(entry_json) = serde_json::to_string(&LogWsMessage::Entry(entry)) {
                                if socket.send(Message::Text(entry_json.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("Log stream client lagged, dropped {skipped} messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                let Some(msg) = msg else {
                    break;
                };
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(LogWsMessage::Filter { min_level: new_level }) = serde_json::from_str::<LogWsMessage>(&text) {
                            min_level = new_level;
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn log_websocket_handler(
    Query(query): Query<LogWsQuery>,
    State(app_state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    trace!("Log Websocket connected (no auth)");
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, false, query))
}

async fn log_websocket_handler_auth(
    Query(query): Query<LogWsQuery>,
    State(app_state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    trace!("Log Websocket connected (auth required)");
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, true, query))
}

pub fn log_ws_api_register(web_auth_enabled: bool, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    let path = concat_path_leading_slash(web_ui_path, "ws/logs");
    if web_auth_enabled {
        axum::Router::new().route(&path, axum::routing::get(log_websocket_handler_auth))
    } else {
        axum::Router::new().route(&path, axum::routing::get(log_websocket_handler))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{create_jwt_admin, create_jwt_web_user},
        model::WebAuthConfig,
    };
    use shared::model::permission::PermissionSet;

    #[test]
    fn test_log_ws_query_parsing() {
        let query: LogWsQuery = serde_html_form::from_str("token=secret123&min_level=warn").unwrap();
        assert_eq!(query.token.as_deref(), Some("secret123"));
        assert_eq!(query.min_level.as_deref(), Some("warn"));
    }

    #[test]
    fn test_check_token_auth_permissions() {
        let auth_config = WebAuthConfig {
            enabled: true,
            issuer: "tuliprox".to_string(),
            secret: "01234567890123456789012345678901".to_string(),
            token_ttl_mins: 60,
            userfile: Some("user.txt".to_string()),
            groupfile: None,
            t_users: None,
            t_groups: None,
        };
        let verifier = TokenVerifier::from_config(&auth_config);

        let admin_token = create_jwt_admin(&auth_config, "admin", 0).unwrap();
        assert!(check_token_auth(&admin_token, Some(&verifier)));

        let mut perms = PermissionSet::new();
        perms.set(Permission::SystemRead);
        let user_token = create_jwt_web_user(&auth_config, "user", perms, 0).unwrap();
        assert!(check_token_auth(&user_token, Some(&verifier)));

        // Missing system read
        let empty_perms = PermissionSet::new();
        let token_no_perm = create_jwt_web_user(&auth_config, "user2", empty_perms, 0).unwrap();
        assert!(!check_token_auth(&token_no_perm, Some(&verifier)));

        // Invalid secret
        let wrong_secret = TokenVerifier::new(&b"wrongsecretwrongsecretwrongsecret"[..], "tuliprox");
        assert!(!check_token_auth(&admin_token, Some(&wrong_secret)));

        // Right secret, wrong issuer: the token must not verify.
        let wrong_issuer = TokenVerifier::new(auth_config.secret.as_bytes(), "somebody-else");
        assert!(!check_token_auth(&admin_token, Some(&wrong_issuer)));
    }
}
