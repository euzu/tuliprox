use crate::{
    api::{
        endpoints::{download_api::download_queue_snapshot, v1_api::create_status_check},
        model::{AppState, EventMessage},
    },
    auth::verify_token,
};
use axum::{
    extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use log::{error, trace};
use shared::{
    model::{
        Permission, ProtocolHandler, ProtocolHandlerMemory, ProtocolMessage, UserCommand, UserRole, WsCloseCode,
        PERM_ALL, PROTOCOL_VERSION, ROLE_ADMIN,
    },
    utils::{concat_path_leading_slash},
    defaults::{default_kick_secs},
};
use std::{fmt, io, sync::Arc};

#[derive(Debug)]
enum WebSocketApiError {
    Transport(axum::Error),
    Protocol(io::Error),
    ProtocolVersionMismatch,
    EventSend { context: &'static str, source: axum::Error },
}

impl fmt::Display for WebSocketApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "{err}"),
            Self::Protocol(err) => write!(f, "{err}"),
            Self::ProtocolVersionMismatch => f.write_str("Protocol version mismatch"),
            Self::EventSend { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for WebSocketApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(err) => Some(err),
            Self::Protocol(err) => Some(err),
            Self::ProtocolVersionMismatch => None,
            Self::EventSend { source, .. } => Some(source),
        }
    }
}

impl From<axum::Error> for WebSocketApiError {
    fn from(value: axum::Error) -> Self { Self::Transport(value) }
}

impl From<io::Error> for WebSocketApiError {
    fn from(value: io::Error) -> Self { Self::Protocol(value) }
}

// WebSocket upgrade handler
async fn websocket_handler(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    trace!("Websocket connected");
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, false))
}

// WebSocket upgrade handler
async fn websocket_handler_auth(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    trace!("Websocket connected");
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, true))
}

pub fn ws_api_register(web_auth_enabled: bool, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    if web_auth_enabled {
        axum::Router::new()
            .route(&concat_path_leading_slash(web_ui_path, "ws"), axum::routing::get(websocket_handler_auth))
    } else {
        axum::Router::new().route(&concat_path_leading_slash(web_ui_path, "ws"), axum::routing::get(websocket_handler))
    }
}

#[inline]
fn set_websocket_auth(mem: &mut ProtocolHandlerMemory, auth_token: String, claims: &shared::model::Claims) {
    mem.permissions = claims.permissions;
    mem.role = if claims.roles.iter().any(|role| role == ROLE_ADMIN) {
        UserRole::Admin
    } else {
        UserRole::User
    };
    mem.token = Some(auth_token);
}

#[inline]
fn websocket_requires_system_read(auth_required: bool, mem: &ProtocolHandlerMemory) -> bool {
    !auth_required || mem.permissions.contains(Permission::SystemRead)
}

#[inline]
fn websocket_requires_download_read(auth_required: bool, mem: &ProtocolHandlerMemory) -> bool {
    !auth_required || mem.permissions.contains(Permission::DownloadRead)
}

fn websocket_can_receive_runtime_events(mem: &ProtocolHandlerMemory, event: &EventMessage) -> bool {
    match event {
        EventMessage::DownloadsUpdate(_) | EventMessage::DownloadsDeltaUpdate(_) => {
            mem.permissions.contains(Permission::DownloadRead)
        }
        EventMessage::PlaylistUpdateProgress(_) | EventMessage::PlaylistUpdate(_) => {
            mem.permissions.contains(Permission::PlaylistWrite)
        }
        EventMessage::LibraryScanProgress(_) => mem.permissions.contains(Permission::LibraryWrite),
        _ => mem.permissions.contains(Permission::SystemRead),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MainEventReceiveErrorAction {
    Continue,
    ResyncStatus,
    Terminate,
}

fn main_event_receive_error_action(
    handler: &ProtocolHandler,
    error: &tokio::sync::broadcast::error::RecvError,
) -> MainEventReceiveErrorAction {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(_) if matches!(handler, ProtocolHandler::Default(mem) if mem.permissions.contains(Permission::SystemRead)) => {
            MainEventReceiveErrorAction::ResyncStatus
        }
        tokio::sync::broadcast::error::RecvError::Lagged(_) => MainEventReceiveErrorAction::Continue,
        tokio::sync::broadcast::error::RecvError::Closed => MainEventReceiveErrorAction::Terminate,
    }
}

fn get_secret_key(app_state: &AppState, auth: bool) -> Option<Vec<u8>> {
    if !auth {
        return None;
    }

    app_state.app_config.config.load().web_ui.as_ref().and_then(|c| c.auth.as_ref()).map(|c| {
        let secret_key: &[u8] = c.secret.as_ref();
        secret_key.to_vec()
    })
}

async fn handle_handshake(msg: Message, socket: &mut WebSocket, version: u8) -> Result<(), WebSocketApiError> {
    if let Message::Binary(bytes) = msg {
        if bytes.len() == 1 {
            let client_version = bytes[0];
            if client_version == version {
                socket.send(Message::binary(bytes)).await?;
                return Ok(());
            }
            error!("Protocol Version mismatch: server={version}, client={client_version}");
        }
    }

    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: WsCloseCode::Protocol.code(),
            reason: "Unsupported protocol".into(),
        })))
        .await;

    Err(WebSocketApiError::ProtocolVersionMismatch)
}

async fn handle_protocol_message(
    msg: Message,
    mem: &mut ProtocolHandlerMemory,
    app_state: &Arc<AppState>,
    auth_required: bool,
    secret_key: Option<&Vec<u8>>,
) -> Option<ProtocolMessage> {
    if let Message::Binary(bytes) = msg {
        match ProtocolMessage::from_bytes(bytes) {
            Ok(ProtocolMessage::Auth(auth_token)) => {
                if !auth_required {
                    mem.permissions = PERM_ALL;
                    mem.role = UserRole::Admin;
                    mem.token = Some(auth_token);
                    return Some(ProtocolMessage::Authorized);
                }

                let Some(secret_key) = secret_key else {
                    return Some(ProtocolMessage::Unauthorized);
                };

                let Some(token_data) = verify_token(&auth_token, secret_key.as_slice()) else {
                    return Some(ProtocolMessage::Unauthorized);
                };

                set_websocket_auth(mem, auth_token, &token_data.claims);
                Some(ProtocolMessage::Authorized)
            }
            Ok(ProtocolMessage::StatusRequest(auth_token)) => {
                if auth_required {
                    let Some(secret_key) = secret_key else {
                        return Some(ProtocolMessage::Unauthorized);
                    };

                    let Some(token_data) = verify_token(&auth_token, secret_key.as_slice()) else {
                        return Some(ProtocolMessage::Unauthorized);
                    };

                    if !token_data.claims.permissions.contains(Permission::SystemRead) {
                        return Some(ProtocolMessage::Unauthorized);
                    }

                    set_websocket_auth(mem, auth_token, &token_data.claims);
                }

                let status = create_status_check(app_state).await;
                Some(ProtocolMessage::StatusResponse(status))
            }
            Ok(ProtocolMessage::UserAction(cmd)) => {
                if websocket_requires_system_read(auth_required, mem) {
                    if !auth_required || mem.token.is_some() {
                        Some(ProtocolMessage::UserActionResponse(handle_user_action(app_state, cmd).await))
                    } else {
                        Some(ProtocolMessage::UserActionResponse(false))
                    }
                } else {
                    Some(ProtocolMessage::UserActionResponse(false))
                }
            }
            Ok(ProtocolMessage::DownloadsRequest) => {
                if websocket_requires_download_read(auth_required, mem) && (!auth_required || mem.token.is_some()) {
                    Some(ProtocolMessage::DownloadsResponse(download_queue_snapshot(&app_state.downloads).await))
                } else {
                    Some(ProtocolMessage::Unauthorized)
                }
            }
            Ok(ProtocolMessage::StreamMeterSubscribe) => {
                handle_stream_meter_subscribe(mem, app_state, auth_required);
                None
            }
            Ok(ProtocolMessage::StreamMeterUnsubscribe) => {
                handle_stream_meter_unsubscribe(mem, app_state);
                None
            }
            Ok(ProtocolMessage::ActiveProviderCountRequest(auth_token)) => {
                handle_active_provider_count_request(auth_token, mem, app_state, auth_required, secret_key).await
            }
            Ok(_) => {
                trace!("Unexpected protocol message after handshake");
                None
            }
            Err(e) => {
                error!("Invalid websocket message: {e}");
                Some(ProtocolMessage::Error(format!("Invalid websocket message: {e}")))
            }
        }
    } else {
        None
    }
}

fn handle_stream_meter_subscribe(mem: &mut ProtocolHandlerMemory, app_state: &Arc<AppState>, auth_required: bool) {
    if websocket_requires_system_read(auth_required, mem) && (!auth_required || mem.token.is_some()) && !mem.stream_meter_subscribed {
        mem.stream_meter_subscribed = true;
        app_state.event_manager.stream_meter_subscriber_connected();
    }
}

fn handle_stream_meter_unsubscribe(mem: &mut ProtocolHandlerMemory, app_state: &Arc<AppState>) {
    if mem.stream_meter_subscribed {
        mem.stream_meter_subscribed = false;
        app_state.event_manager.stream_meter_subscriber_disconnected();
    }
}

async fn handle_active_provider_count_request(
    auth_token: String,
    mem: &mut ProtocolHandlerMemory,
    app_state: &Arc<AppState>,
    auth_required: bool,
    secret_key: Option<&Vec<u8>>,
) -> Option<ProtocolMessage> {
    if auth_required {
        let Some(secret_key) = secret_key else {
            return Some(ProtocolMessage::Unauthorized);
        };
        let Some(token_data) = verify_token(&auth_token, secret_key.as_slice()) else {
            return Some(ProtocolMessage::Unauthorized);
        };
        if token_data.claims.permissions.contains(Permission::SystemRead) {
            set_websocket_auth(mem, auth_token, &token_data.claims);
            let connections = app_state.active_provider.get_provider_connections_count().await;
            Some(ProtocolMessage::ActiveProviderCountResponse(connections))
        } else {
            Some(ProtocolMessage::Unauthorized)
        }
    } else {
        mem.permissions = PERM_ALL;
        mem.role = UserRole::Admin;
        mem.token = Some(auth_token);
        let connections = app_state.active_provider.get_provider_connections_count().await;
        Some(ProtocolMessage::ActiveProviderCountResponse(connections))
    }
}

async fn handle_incoming_message(
    result: Result<Message, axum::Error>,
    socket: &mut WebSocket,
    handler: &mut ProtocolHandler,
    app_state: &Arc<AppState>,
    auth_required: bool,
    secret_key: Option<&Vec<u8>>,
) -> Result<(), WebSocketApiError> {
    let msg = result?;

    match handler {
        ProtocolHandler::Version(version) => {
            handle_handshake(msg, socket, *version).await?;
            let mut mem = ProtocolHandlerMemory::default();
            if !auth_required {
                mem.permissions = PERM_ALL;
                mem.role = UserRole::Admin;
            }
            *handler = ProtocolHandler::Default(mem);
            Ok(())
        }
        ProtocolHandler::Default(mem) => {
            let msg = handle_protocol_message(msg, mem, app_state, auth_required, secret_key).await;
            match msg {
                None => Ok(()),
                Some(protocol_msg) => {
                    let bytes = match protocol_msg.to_bytes() {
                        Ok(bytes) => bytes,
                        Err(err) => ProtocolMessage::Error(err.to_string()).to_bytes()?,
                    };
                    Ok(socket.send(Message::Binary(bytes)).await?)
                }
            }
        }
    }
}

async fn handle_event_message(
    socket: &mut WebSocket,
    event: EventMessage,
    handler: &ProtocolHandler,
) -> Result<(), WebSocketApiError> {
    match handler {
        ProtocolHandler::Version(_) => {}
        ProtocolHandler::Default(mem) => {
            if websocket_can_receive_runtime_events(mem, &event) {
                match event {
                    EventMessage::ServerError(error) => {
                        send_event_response(socket, ProtocolMessage::ServerError(error), "Server Error event").await?;
                    }
                    EventMessage::ActiveUser(event) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::ActiveUserResponse(event),
                            "Active user connection change event",
                        )
                        .await?;
                    }
                    EventMessage::ActiveProvider(provider, connections) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::ActiveProviderResponse(provider, connections),
                            "Provider connection change event",
                        )
                        .await?;
                    }
                    EventMessage::ConfigChange(config) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::ConfigChangeResponse(config),
                            "Configuration files change event",
                        )
                        .await?;
                    }
                    EventMessage::PlaylistUpdate(state) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::PlaylistUpdateResponse(state),
                            "Playlist update event",
                        )
                        .await?;
                    }
                    EventMessage::PlaylistUpdateProgress(progress) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::PlaylistUpdateProgressResponse(progress),
                            "Playlist update progress event",
                        )
                        .await?;
                    }
                    EventMessage::SystemInfoUpdate(system_info) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::SystemInfoResponse(system_info),
                            "System info event",
                        )
                        .await?;
                    }
                    EventMessage::LibraryScanProgress(progress) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::LibraryScanProgressResponse(progress),
                            "Library scan progress event",
                        )
                        .await?;
                    }
                    EventMessage::DownloadsUpdate(downloads) => {
                        send_event_response(socket, ProtocolMessage::DownloadsResponse(downloads), "Downloads event")
                            .await?;
                    }
                    EventMessage::DownloadsDeltaUpdate(delta) => {
                        send_event_response(
                            socket,
                            ProtocolMessage::DownloadsDeltaResponse(delta),
                            "Downloads delta event",
                        )
                        .await?;
                    }
                    EventMessage::InputMetadataUpdatesCompleted(_)
                    | EventMessage::InputMetadataUpdatesStarted(_) => {
                        // Internal events or already handled above
                    }
                }
            }
        }
    }
    Ok(())
}

async fn send_event_response(
    socket: &mut WebSocket,
    message: ProtocolMessage,
    context: &'static str,
) -> Result<(), WebSocketApiError> {
    let msg = message.to_bytes()?;
    socket
        .send(Message::Binary(msg))
        .await
        .map_err(|source| WebSocketApiError::EventSend { context, source })
}

// WebSocket communication logic
async fn handle_socket(mut socket: WebSocket, app_state: Arc<AppState>, auth_required: bool) {
    let secret_key = get_secret_key(&app_state, auth_required);

    let mut event_rx = app_state.event_manager.get_event_channel();
    let mut meter_event_rx = app_state.event_manager.get_meter_channel();
    let mut handler = ProtocolHandler::Version(PROTOCOL_VERSION);

    loop {
        tokio::select! {
            maybe_msg = socket.recv() => {
                if let Some(msg) = maybe_msg {
                    if let Err(e) = handle_incoming_message(msg, &mut socket, &mut handler, &app_state, auth_required, secret_key.as_ref()).await {
                        trace!("WebSocket message handling error: {e}");
                        break;
                    }
                } else {
                    break;
                }
            }

            event_result = event_rx.recv() => {
                match event_result {
                    Ok(event) => {
                        if let Err(e) = handle_event_message(&mut socket, event, &handler).await {
                            trace!("Failed to send ws event: {e}");
                            break;
                        }
                    }
                    Err(error) => {
                        if let tokio::sync::broadcast::error::RecvError::Lagged(skipped) = &error {
                            trace!("Main websocket event receiver lagged by {skipped} messages");
                        }
                        match main_event_receive_error_action(&handler, &error) {
                            MainEventReceiveErrorAction::Continue => {}
                            MainEventReceiveErrorAction::ResyncStatus => {
                                // Drop retained pre-snapshot deltas so they cannot be replayed after the authoritative state.
                                event_rx = app_state.event_manager.get_event_channel();
                                let status = create_status_check(&app_state).await;
                                if let Err(e) = send_event_response(
                                    &mut socket,
                                    ProtocolMessage::StatusResponse(status),
                                    "Status resync after lagged main websocket event receiver",
                                )
                                .await
                                {
                                    trace!("Failed to send ws status resync: {e}");
                                    break;
                                }
                            }
                            MainEventReceiveErrorAction::Terminate => break,
                        }
                    }
                }
            }

            meter_result = meter_event_rx.recv() => {
                match meter_result {
                    Ok(entries) => {
                        if let ProtocolHandler::Default(mem) = &handler {
                            if mem.stream_meter_subscribed {
                                let msg = ProtocolMessage::StreamMeterBatchResponse(entries).to_bytes();
                                match msg {
                                    Ok(msg) => {
                                        if let Err(e) = socket.send(Message::Binary(msg)).await {
                                            trace!("Failed to send ws meter event: {e}");
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        trace!("Failed to encode ws meter event: {e}");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        trace!("Meter websocket event receiver lagged by {skipped} messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    if let ProtocolHandler::Default(mem) = &handler {
        if mem.stream_meter_subscribed {
            app_state.event_manager.stream_meter_subscriber_disconnected();
        }
    }
}

async fn handle_user_action(app_state: &Arc<AppState>, cmd: UserCommand) -> bool {
    match cmd {
        UserCommand::Kick(addr, virtual_id, _secs) => {
            // secs could be later used for different kick configurations. Currently, we only have 1.
            let kick_secs =
                app_state.app_config.config.load().web_ui.as_ref().map_or_else(default_kick_secs, |wc| wc.kick_secs);
            app_state.connection_manager.kick_connection(&addr, virtual_id, kick_secs).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{main_event_receive_error_action, websocket_can_receive_runtime_events, MainEventReceiveErrorAction};
    use crate::api::model::EventMessage;
    use shared::model::{
        DownloadsDelta, DownloadsResponse, FileDownloadDto, LibraryScanProgressEvent, LibraryScanSummary,
        LibraryScanSummaryStatus, Permission, PlaylistUpdateProgressEvent, ProtocolHandler, ProtocolHandlerMemory,
        TaskKindDto, TaskPriorityDto, TransferStatusDto, UserRole, PROTOCOL_VERSION,
    };
    use tokio::sync::broadcast::error::RecvError;

    #[test]
    fn lagged_main_event_receiver_resyncs_authorized_system_reader() {
        let handler = ProtocolHandler::Default(ProtocolHandlerMemory {
            token: Some("token".to_string()),
            permissions: Permission::SystemRead.into(),
            role: UserRole::User,
            ..ProtocolHandlerMemory::default()
        });

        assert_eq!(
            main_event_receive_error_action(&handler, &RecvError::Lagged(3)),
            MainEventReceiveErrorAction::ResyncStatus
        );
    }

    #[test]
    fn closed_main_event_receiver_terminates() {
        let handler = ProtocolHandler::Default(ProtocolHandlerMemory {
            permissions: Permission::SystemRead.into(),
            ..ProtocolHandlerMemory::default()
        });

        assert_eq!(
            main_event_receive_error_action(&handler, &RecvError::Closed),
            MainEventReceiveErrorAction::Terminate
        );
    }

    #[test]
    fn lagged_main_event_receiver_does_not_resync_before_handshake_or_authorization() {
        let version_handler = ProtocolHandler::Version(PROTOCOL_VERSION);
        let unauthorized_handler = ProtocolHandler::Default(ProtocolHandlerMemory::default());

        assert_eq!(
            main_event_receive_error_action(&version_handler, &RecvError::Lagged(1)),
            MainEventReceiveErrorAction::Continue
        );
        assert_eq!(
            main_event_receive_error_action(&unauthorized_handler, &RecvError::Lagged(1)),
            MainEventReceiveErrorAction::Continue
        );
    }

    #[test]
    fn test_websocket_runtime_events_allowed_for_admin() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::SystemRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::Admin;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::ServerError("err".to_string())
        ));
    }

    #[test]
    fn test_websocket_runtime_events_allowed_for_system_read_user() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::SystemRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::ServerError("err".to_string())
        ));
    }

    #[test]
    fn test_websocket_runtime_events_denied_without_system_read() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::ConfigRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(!websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::ServerError("err".to_string())
        ));
    }

    #[test]
    fn test_websocket_runtime_events_denied_for_default_permissions() {
        let mem = ProtocolHandlerMemory {
            role: UserRole::User,
            ..ProtocolHandlerMemory::default()
        };

        assert!(!websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::ServerError("err".to_string())
        ));
    }

    #[test]
    fn test_websocket_playlist_progress_allowed_for_playlist_write_without_system_read() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::PlaylistWrite.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: "target".to_string(),
                message: "step".to_string(),
            })
        ));
    }

    #[test]
    fn test_websocket_library_progress_allowed_for_library_write_without_system_read() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::LibraryWrite.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::LibraryScanProgress(LibraryScanProgressEvent {
                summary: LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Success,
                    message: "done".to_string(),
                    result: None,
                },
            })
        ));
    }

    #[test]
    fn test_websocket_download_updates_allowed_for_download_read_user() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::DownloadRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::DownloadsUpdate(DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: Vec::new(),
            })
        ));
    }

    #[test]
    fn test_websocket_download_updates_denied_without_download_read() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::SystemRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(!websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::DownloadsUpdate(DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: Vec::new(),
            })
        ));
    }

    #[test]
    fn test_websocket_download_delta_updates_allowed_for_download_read_user() {
        let mut mem = ProtocolHandlerMemory {
            permissions: Permission::DownloadRead.into(),
            ..ProtocolHandlerMemory::default()
        };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActivePatched(FileDownloadDto {
                id: "id".to_string(),
                title: "file.ts".to_string(),
                kind: TaskKindDto::Download,
                priority: TaskPriorityDto::Background,
                status: TransferStatusDto::Running,
                retry_attempts: 0,
                downloaded_bytes: 1,
                total_bytes: Some(2),
                next_retry_at: None,
                scheduled_start_at: None,
                duration_secs: None,
                error: None,
            }))
        ));
    }
}
