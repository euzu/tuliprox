use crate::{
    api::{
        endpoints::{download_api::download_queue_snapshot, v1_api::create_status_check},
        model::{AppState, EventMessage},
    },
    auth::{validate_token_claims, TokenVerifier},
};
use axum::{
    extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use log::{error, trace};
use shared::{
    defaults::default_kick_secs,
    model::{
        Claims, Permission, ProtocolHandler, ProtocolHandlerMemory, ProtocolMessage, RoleSet, UserCommand, UserId,
        UserRole, WsCloseCode, CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, PROTOCOL_VERSION, TOKEN_NO_AUTH,
    },
    utils::concat_path_leading_slash,
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
fn set_websocket_auth(mem: &mut ProtocolHandlerMemory, auth_token: String, claims: &Claims) -> bool {
    if validate_token_claims(claims).is_err() {
        return false;
    }
    mem.permissions = claims.permissions;
    mem.role = if claims.is_admin() { UserRole::Admin } else { UserRole::User };
    mem.subject_id = claims.subject_id.as_ref().map(|u| u.0.clone());
    mem.token = Some(auth_token);
    true
}

fn set_no_auth_websocket_identity(mem: &mut ProtocolHandlerMemory, auth_token: Option<String>) {
    mem.permissions = PERM_ALL;
    mem.role = UserRole::Admin;
    mem.subject_id = Some(UserId::BUILTIN_ADMIN_NAMESPACE.to_string());
    mem.token = auth_token;
}

fn websocket_claims(mem: &ProtocolHandlerMemory) -> Option<Claims> {
    let subject_id = mem.subject_id.as_ref().map(|subject| UserId::from(subject.clone()))?;
    Some(Claims {
        username: "__ws__".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: i64::MAX,
        roles: if mem.role == UserRole::Admin { RoleSet::ADMIN } else { RoleSet::new() },
        permissions: mem.permissions,
        pwd_version: 0,
        subject_id: Some(subject_id),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    })
}

#[inline]
fn websocket_requires_system_read(auth_required: bool, mem: &ProtocolHandlerMemory) -> bool {
    !auth_required || mem.permissions.contains(Permission::SystemRead)
}

#[inline]
fn websocket_requires_download_read(auth_required: bool, mem: &ProtocolHandlerMemory) -> bool {
    !auth_required || mem.permissions.contains(Permission::DownloadRead)
}

/// Which permission an event needs is a fact about the event, not about the
/// websocket: it does not change with the transport carrying it. The table
/// lives on `EventKind`, so a new variant cannot reach a session that should
/// not see it just because this file was not updated.
fn websocket_can_receive_runtime_events(mem: &ProtocolHandlerMemory, event: &EventMessage) -> bool {
    mem.permissions.contains(event.required_permission())
}

/// The payload itself, cloned only if this session is not the last holder.
///
/// The bus keeps the large payloads behind `Arc` so that subscribers who do
/// not serialize them - the notification bridge, say - pay a refcount bump
/// instead of a deep copy. Only here, at the wire boundary, is the value
/// itself needed.
fn unwrap_or_clone<T: Clone>(value: Arc<T>) -> T { Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone()) }

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

/// The verifier this socket authenticates against.
///
/// This used to hand a bare `Vec<u8>` secret around, which is precisely why
/// the WebSocket paths could not check the token's issuer.
fn get_token_verifier(app_state: &AppState, auth: bool) -> Option<TokenVerifier> {
    if !auth {
        return None;
    }

    app_state.app_config.config.load().web_ui.as_ref().and_then(|c| c.auth.as_ref()).map(TokenVerifier::from_config)
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
    verifier: Option<&TokenVerifier>,
) -> Option<ProtocolMessage> {
    if let Message::Binary(bytes) = msg {
        match ProtocolMessage::from_bytes(bytes) {
            Ok(ProtocolMessage::Auth(auth_token)) => {
                if !auth_required {
                    set_no_auth_websocket_identity(mem, Some(TOKEN_NO_AUTH.to_string()));
                    return Some(ProtocolMessage::Authorized);
                }

                let Some(verifier) = verifier else {
                    return Some(ProtocolMessage::Unauthorized);
                };

                let Some(token_data) = verifier.verify(&auth_token) else {
                    return Some(ProtocolMessage::Unauthorized);
                };

                if set_websocket_auth(mem, auth_token, &token_data.claims) {
                    Some(ProtocolMessage::Authorized)
                } else {
                    Some(ProtocolMessage::Unauthorized)
                }
            }
            Ok(ProtocolMessage::StatusRequest(auth_token)) => {
                if auth_required {
                    let Some(verifier) = verifier else {
                        return Some(ProtocolMessage::Unauthorized);
                    };

                    let Some(token_data) = verifier.verify(&auth_token) else {
                        return Some(ProtocolMessage::Unauthorized);
                    };

                    if !token_data.claims.permissions.contains(Permission::SystemRead) {
                        return Some(ProtocolMessage::Unauthorized);
                    }

                    if !set_websocket_auth(mem, auth_token, &token_data.claims) {
                        return Some(ProtocolMessage::Unauthorized);
                    }
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
            Ok(ProtocolMessage::RecordingSnapshotRequest) => {
                if let Some(claims) = websocket_claims(mem) {
                    Some(recording_frame_for_session(app_state, &claims).await)
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
                handle_active_provider_count_request(auth_token, mem, app_state, auth_required, verifier).await
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
    if websocket_requires_system_read(auth_required, mem)
        && (!auth_required || mem.token.is_some())
        && !mem.stream_meter_subscribed
    {
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
    verifier: Option<&TokenVerifier>,
) -> Option<ProtocolMessage> {
    if auth_required {
        let Some(verifier) = verifier else {
            return Some(ProtocolMessage::Unauthorized);
        };
        let Some(token_data) = verifier.verify(&auth_token) else {
            return Some(ProtocolMessage::Unauthorized);
        };
        if token_data.claims.permissions.contains(Permission::SystemRead)
            && set_websocket_auth(mem, auth_token, &token_data.claims)
        {
            let connections = app_state.active_provider.get_provider_connections_count().await;
            Some(ProtocolMessage::ActiveProviderCountResponse(connections))
        } else {
            Some(ProtocolMessage::Unauthorized)
        }
    } else {
        set_no_auth_websocket_identity(mem, Some(TOKEN_NO_AUTH.to_string()));
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
    verifier: Option<&TokenVerifier>,
) -> Result<(), WebSocketApiError> {
    let msg = result?;

    match handler {
        ProtocolHandler::Version(version) => {
            handle_handshake(msg, socket, *version).await?;
            let mut mem = ProtocolHandlerMemory::default();
            if !auth_required {
                set_no_auth_websocket_identity(&mut mem, Some(TOKEN_NO_AUTH.to_string()));
            }
            *handler = ProtocolHandler::Default(mem);
            Ok(())
        }
        ProtocolHandler::Default(mem) => {
            let msg = handle_protocol_message(msg, mem, app_state, auth_required, verifier).await;
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

/// The frame to send a session that asked for recordings.
///
/// Three outcomes, and the distinction is the point:
///
/// - the DVR is switched off, or the token is too old to trust →
///   `RecordingWsError { code }`, so the client can act. Both used to
///   come back as an empty task list, indistinguishable from "you have
///   no recordings";
/// - the principal has no `recording.read` → an empty snapshot, which is
///   the honest answer and needs no error;
/// - otherwise → the session-filtered snapshot.
async fn recording_frame_for_session(app_state: &AppState, claims: &Claims) -> ProtocolMessage {
    use crate::api::model::recording::recording_ws::{recording_view_denial, RecordingViewDenial};

    // `recording.enabled: false` gates the REST routes and the
    // schedulers; the socket has to agree, or a client would keep
    // receiving live recording data while every REST call answered
    // `501 recording_disabled`.
    if !crate::api::model::recording::recording_supervisor::recording_enabled(&app_state.app_config) {
        return ProtocolMessage::RecordingWsError { code: "recording_disabled".to_string() };
    }
    if let Some(RecordingViewDenial::TokenRefreshRequired) = recording_view_denial(claims) {
        return ProtocolMessage::RecordingWsError {
            code: RecordingViewDenial::TokenRefreshRequired.code().to_string(),
        };
    }
    let (revision, tasks) =
        crate::api::model::recording::recording_ws::recording_snapshot(&app_state.downloads, claims).await;
    ProtocolMessage::RecordingSnapshotResponse { revision, tasks }
}

async fn send_recording_snapshot_event(
    app_state: &AppState,
    socket: &mut WebSocket,
    mem: &ProtocolHandlerMemory,
) -> Result<(), WebSocketApiError> {
    if let Some(claims) = websocket_claims(mem) {
        let frame = recording_frame_for_session(app_state, &claims).await;
        send_event_response(socket, frame, "Recording snapshot event").await?;
    }
    Ok(())
}

/// The wire frame an event becomes, with the label used if sending it fails.
///
/// A pure function so the whole taxonomy can be checked at once: the test
/// below asserts every `EventKind` either maps here or is on the short list
/// of kinds handled another way. The mapping used to be a hundred lines of
/// `match` nested three deep inside the socket loop, where a kind that
/// reached no arm was indistinguishable from one deliberately ignored.
fn to_protocol_message(event: EventMessage) -> Option<(ProtocolMessage, &'static str)> {
    Some(match event {
        EventMessage::ServerError(error) => (ProtocolMessage::ServerError(error), "Server Error event"),
        EventMessage::ActiveUser(event) => {
            (ProtocolMessage::ActiveUserResponse(event), "Active user connection change event")
        }
        EventMessage::ActiveProvider(provider, connections) => {
            (ProtocolMessage::ActiveProviderResponse(provider, connections), "Provider connection change event")
        }
        EventMessage::ConfigChange(config) => {
            (ProtocolMessage::ConfigChangeResponse(config), "Configuration files change event")
        }
        // The wire carries the outcome only. The run summary rides along on
        // the bus for notifications and plugins; the Web UI re-fetches its
        // own view rather than reading statistics off this frame.
        EventMessage::PlaylistUpdate(summary) => {
            (ProtocolMessage::PlaylistUpdateResponse(summary.state), "Playlist update event")
        }
        EventMessage::PlaylistUpdateProgress(progress) => {
            (ProtocolMessage::PlaylistUpdateProgressResponse(progress), "Playlist update progress event")
        }
        EventMessage::SystemInfoUpdate(system_info) => {
            (ProtocolMessage::SystemInfoResponse(unwrap_or_clone(system_info)), "System info event")
        }
        EventMessage::LibraryScanProgress(progress) => {
            (ProtocolMessage::LibraryScanProgressResponse(progress), "Library scan progress event")
        }
        EventMessage::DownloadsUpdate(downloads) => {
            (ProtocolMessage::DownloadsResponse(unwrap_or_clone(downloads)), "Downloads event")
        }
        EventMessage::DownloadsDeltaUpdate(delta) => {
            (ProtocolMessage::DownloadsDeltaResponse(delta), "Downloads delta event")
        }
        // The rule repository is per-process and `list_recording_rules`
        // enforces the session filter server-side, so this is a bare nudge:
        // the frontend re-fetches.
        EventMessage::RecordingRulesChanged => (ProtocolMessage::RecordingRulesChanged, "Recording rules changed"),

        // Handled by `handle_event_message`, not translatable here:
        // `RecordingChanged` needs a per-session snapshot re-fetch, and the
        // metadata events are internal.
        //
        // The lifecycle events are notification-side only: they reach
        // operators through the messaging pipeline and plugins through the
        // bus, and adding them to the wire would need a `ProtocolMessage`
        // variant and frontend handling that nothing asks for yet.
        EventMessage::RecordingChanged
        | EventMessage::ServerLifecycle(_)
        | EventMessage::InputMetadataUpdatesCompleted(_)
        | EventMessage::InputMetadataUpdatesStarted(_)
        | EventMessage::InputMetadataUpdatesFailed(_)
        | EventMessage::DiskAlert(_)
        | EventMessage::ConfigReloadFailed(_)
        | EventMessage::PlaylistWatchChanged(_)
        | EventMessage::PlaylistGroupsChanged(_)
        | EventMessage::PlaylistWatchDisabled(_)
        | EventMessage::PlaylistWatchUnmatched(_)
        | EventMessage::RecordingLifecycle(_)
        | EventMessage::ProviderAccount(_)
        | EventMessage::ProviderFetchFailed(_)
        | EventMessage::ProviderPoolExhausted(_)
        | EventMessage::ProviderPriorityFallback(_)
        // The panel already knows it just saved a user - it made the
        // request - and nothing in the Web UI subscribes to probe
        // failures yet. Both are on the bus for operators and plugins.
        | EventMessage::UserLifecycle(_)
        | EventMessage::ConnectionDenied(_)
        | EventMessage::StreamProbeFailed(_)
        // A notification that could not be delivered is an operator concern,
        // not a Web UI one, and there is no panel that renders it.
        | EventMessage::NotificationDeadLettered(_)
        | EventMessage::ScheduledTaskFailed(_)
        // Auth decisions reach notification channels and plugins, not the
        // Web UI socket: there is no panel that renders them, and pushing
        // every sign-in to every connected admin is noise.
        | EventMessage::AuthAudit(_) => return None,
    })
}

async fn handle_event_message(
    app_state: &Arc<AppState>,
    socket: &mut WebSocket,
    event: EventMessage,
    handler: &ProtocolHandler,
) -> Result<(), WebSocketApiError> {
    let ProtocolHandler::Default(mem) = handler else {
        return Ok(());
    };
    if !websocket_can_receive_runtime_events(mem, &event) {
        return Ok(());
    }

    // Re-fetch the per-session filtered snapshot so the visibility contract
    // is enforced by `recording_ws` rather than by this socket.
    if matches!(event, EventMessage::RecordingChanged) {
        return send_recording_snapshot_event(app_state, socket, mem).await;
    }

    if let Some((message, context)) = to_protocol_message(event) {
        send_event_response(socket, message, context).await?;
    }
    Ok(())
}

async fn send_event_response(
    socket: &mut WebSocket,
    message: ProtocolMessage,
    context: &'static str,
) -> Result<(), WebSocketApiError> {
    let msg = message.to_bytes()?;
    socket.send(Message::Binary(msg)).await.map_err(|source| WebSocketApiError::EventSend { context, source })
}

// WebSocket communication logic
async fn handle_socket(mut socket: WebSocket, app_state: Arc<AppState>, auth_required: bool) {
    let verifier = get_token_verifier(&app_state, auth_required);

    let mut event_rx = app_state.event_manager.get_event_channel();
    let mut meter_event_rx = app_state.event_manager.get_meter_channel();
    let mut handler = ProtocolHandler::Version(PROTOCOL_VERSION);

    loop {
        tokio::select! {
            maybe_msg = socket.recv() => {
                if let Some(msg) = maybe_msg {
                    if let Err(e) = handle_incoming_message(msg, &mut socket, &mut handler, &app_state, auth_required, verifier.as_ref()).await {
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
                        if let Err(e) = handle_event_message(&app_state, &mut socket, event, &handler).await {
                            trace!("Failed to send ws event: {e}");
                            break;
                        }
                    }
                    Err(error) => {
                        if let tokio::sync::broadcast::error::RecvError::Lagged(skipped) = &error {
                            app_state.event_manager.stats().record_lag(*skipped);
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
    use super::{
        main_event_receive_error_action, set_no_auth_websocket_identity, set_websocket_auth, to_protocol_message,
        websocket_can_receive_runtime_events, websocket_claims, MainEventReceiveErrorAction,
    };
    use crate::api::model::EventMessage;
    use shared::model::{
        Claims, ConfigReloadFailure, DiskAlert, DiskAlertLevel, DownloadsDelta, DownloadsResponse, FileDownloadDto,
        LibraryScanProgressEvent, LibraryScanSummary, LibraryScanSummaryStatus, MetadataUpdateFailure, MsgKind,
        Permission, PlaylistGroupsChanged, PlaylistUpdateProgressEvent, ProtocolHandler, ProtocolHandlerMemory,
        ProviderAccountEvent, ProviderAccountState, ProviderFailureKind, ProviderFetchFailure, ProviderPoolExhausted,
        ProviderPriorityFallback, RecordingLifecycleMessage, RoleSet, TaskKindDto, TaskPriorityDto, TransferStatusDto,
        UserId, UserRole, WatchChanges, WatchDisabled, WatchDisabledReason, WatchUnmatched,
        CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, PROTOCOL_VERSION, TOKEN_NO_AUTH,
    };
    use std::sync::Arc;
    use tokio::sync::broadcast::error::RecvError;

    /// Every kind either becomes a wire frame or is on the short list of
    /// kinds handled another way. Without this, a variant added later
    /// silently reaches no arm - which is exactly what the old nested match
    /// could not distinguish from a deliberate omission.
    #[test]
    fn every_event_kind_is_either_wire_mapped_or_deliberately_not() {
        use shared::model::EventKind;

        // Two reasons a kind produces no frame, kept apart because they are
        // not the same fact - the same distinction `to_notification` draws
        // between its own two `None` arms.

        // Reachable over the wire, just not through this pure function:
        // `RecordingChanged` needs a per-session snapshot re-fetch, and the
        // metadata events are internal.
        const HANDLED_ELSEWHERE: [EventKind; 3] = [
            EventKind::RecordingChanged,
            EventKind::InputMetadataUpdatesCompleted,
            EventKind::InputMetadataUpdatesStarted,
        ];

        // Never on the wire: these reach operators through the messaging
        // pipeline and plugins through the bus. Putting one on the wire
        // would need a `ProtocolMessage` variant and frontend handling that
        // nothing asks for.
        //
        // This list did not exist until now, so the assert below compared
        // every notification-only kind against `expected = true` and the
        // test failed on `DiskAlert` - it has been red since the lifecycle
        // events joined the bus.
        const NOT_ON_THE_WIRE: [EventKind; 25] = [
            EventKind::ConnectionDenied,
            EventKind::ScheduledTaskFailed,
            EventKind::ProviderFetchFailed,
            EventKind::ProviderPoolExhausted,
            EventKind::ProviderPriorityFallback,
            EventKind::PlaylistGroupsChanged,
            EventKind::PlaylistWatchDisabled,
            EventKind::PlaylistWatchUnmatched,
            EventKind::NotificationDeadLettered,
            EventKind::ServerStarted,
            EventKind::ServerShutdown,
            EventKind::InputMetadataUpdatesFailed,
            EventKind::DiskAlert,
            EventKind::ConfigReloadFailed,
            EventKind::PlaylistWatchChanged,
            EventKind::RecordingStarted,
            EventKind::RecordingCompleted,
            EventKind::RecordingFailed,
            EventKind::ProviderAccountStatus,
            EventKind::ProviderAccountExpiring,
            EventKind::ProviderAccountExpired,
            EventKind::UserCreated,
            EventKind::UserUpdated,
            EventKind::UserDeleted,
            EventKind::StreamProbeFailed,
        ];

        for (event, kind) in sample_event_of_every_kind() {
            let mapped = to_protocol_message(event).is_some();
            let expected = !HANDLED_ELSEWHERE.contains(&kind) && !NOT_ON_THE_WIRE.contains(&kind);
            assert_eq!(mapped, expected, "{kind:?}: wire-mapped={mapped}, expected={expected}");
        }
    }

    fn provider_fetch_failure() -> ProviderFetchFailure {
        ProviderFetchFailure {
            input: "i".into(),
            provider: "m3u".into(),
            kind: ProviderFailureKind::Transient,
            error_count: 1,
            message: None,
            retryable: true,
            needs_operator: false,
            partial: false,
        }
    }

    /// One `EventMessage` per `EventKind`; the length assert means a new
    /// variant cannot slip past the test above.
    ///
    /// Long by construction, and it has to stay one list for that assert to
    /// mean anything.
    #[allow(clippy::too_many_lines)]
    fn sample_event_of_every_kind() -> Vec<(EventMessage, shared::model::EventKind)> {
        use shared::model::{
            ActiveUserConnectionChange, ConfigType, ConnectionDenied, EventKind, NotificationDeadLetter,
            PlaylistUpdateState, PlaylistUpdateSummary, ScheduledTaskFailure, ServerLifecycleEvent, StreamProbeFailure,
            StreamProbeFailureReason, SystemInfo, UserLifecycleEvent, UserLifecycleState,
        };

        fn user_lifecycle(state: UserLifecycleState) -> EventMessage {
            EventMessage::UserLifecycle(UserLifecycleEvent::new("u".into(), "t".into(), state))
        }

        let downloads = DownloadsResponse { queue: Vec::new(), finished: Vec::new(), active: Vec::new() };
        let samples = vec![
            EventMessage::ServerError("x".to_string()),
            EventMessage::ServerLifecycle(ServerLifecycleEvent::started("1".into(), "h:1".into())),
            EventMessage::ServerLifecycle(ServerLifecycleEvent::shutting_down("1".into(), "SIGTERM".into())),
            EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(0, 0)),
            EventMessage::ActiveProvider("p".into(), 1),
            EventMessage::ConfigChange(ConfigType::Config),
            EventMessage::PlaylistUpdate(PlaylistUpdateSummary::state_only(PlaylistUpdateState::Success)),
            EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: String::new(),
                message: String::new(),
            }),
            EventMessage::SystemInfoUpdate(Arc::new(SystemInfo {
                cpu_usage: 0.0,
                memory_usage: 0,
                memory_total: 0,
                net_rx_bytes_per_sec: 0.0,
                net_tx_bytes_per_sec: 0.0,
                net_rx_bytes_total: 0,
                net_tx_bytes_total: 0,
                disk_total_bytes: 0,
                disk_free_bytes: 0,
            })),
            EventMessage::LibraryScanProgress(LibraryScanProgressEvent {
                summary: LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Success,
                    message: String::new(),
                    result: None,
                },
            }),
            EventMessage::LibraryScanProgress(LibraryScanProgressEvent {
                summary: LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Error,
                    message: String::new(),
                    result: None,
                },
            }),
            EventMessage::DownloadsUpdate(Arc::new(downloads)),
            EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActiveCleared),
            EventMessage::RecordingChanged,
            EventMessage::RecordingRulesChanged,
            EventMessage::InputMetadataUpdatesCompleted("a".into()),
            EventMessage::InputMetadataUpdatesStarted("a".into()),
            EventMessage::InputMetadataUpdatesFailed(MetadataUpdateFailure::new("a".into(), 1, false, None)),
            EventMessage::DiskAlert(DiskAlert {
                level: DiskAlertLevel::Warn,
                total_bytes: 100,
                free_bytes: 5,
                used_bytes: 95,
                percent: 95.0,
            }),
            EventMessage::ConfigReloadFailed(ConfigReloadFailure {
                paths: "config.yml".to_string(),
                error: "boom".to_string(),
            }),
            EventMessage::PlaylistWatchChanged(WatchChanges::new(
                "t".to_string(),
                "g".to_string(),
                Vec::new(),
                Vec::new(),
            )),
            EventMessage::PlaylistGroupsChanged(PlaylistGroupsChanged::new("t".to_string(), Vec::new(), Vec::new())),
            EventMessage::PlaylistWatchDisabled(WatchDisabled::new(
                "t".to_string(),
                WatchDisabledReason::InvalidPatterns,
            )),
            EventMessage::PlaylistWatchUnmatched(WatchUnmatched::new("t".to_string(), vec!["x".to_string()], 0)),
            recording_lifecycle(MsgKind::RecordingStarted),
            recording_lifecycle(MsgKind::RecordingCompleted),
            recording_lifecycle(MsgKind::RecordingFailed),
            EventMessage::ProviderFetchFailed(provider_fetch_failure()),
            EventMessage::ProviderPoolExhausted(ProviderPoolExhausted::new("i".into(), Vec::new())),
            EventMessage::ProviderPriorityFallback(ProviderPriorityFallback::new(
                "i".into(),
                "p".into(),
                1,
                2,
                Some(0),
            )),
            provider_account(ProviderAccountState::StatusChanged),
            provider_account(ProviderAccountState::Expiring),
            provider_account(ProviderAccountState::Expired),
            user_lifecycle(UserLifecycleState::Created),
            user_lifecycle(UserLifecycleState::Updated),
            user_lifecycle(UserLifecycleState::Deleted),
            EventMessage::ScheduledTaskFailed(ScheduledTaskFailure::new(
                shared::model::ScheduleTaskType::GeoIpUpdate,
                "boom".to_string(),
            )),
            EventMessage::NotificationDeadLettered(NotificationDeadLetter::new(
                shared::model::notification::registry::SYSTEM_ERROR,
                3,
                vec!["telegram".to_string()],
                0,
            )),
            EventMessage::ConnectionDenied(ConnectionDenied::new("u".into(), "1.2.3.4".into(), 1, 0)),
            EventMessage::StreamProbeFailed(StreamProbeFailure::new(
                "input".into(),
                "1".into(),
                "http://example.test/s".into(),
                StreamProbeFailureReason::Unreachable,
            )),
        ];
        assert_eq!(samples.len(), EventKind::ALL.len(), "add the new variant to this list");
        samples
            .into_iter()
            .map(|event| {
                let kind = event.kind();
                (event, kind)
            })
            .collect()
    }

    #[test]
    fn no_auth_websocket_identity_is_builtin_admin() {
        let mut mem = ProtocolHandlerMemory::default();

        set_no_auth_websocket_identity(&mut mem, Some(TOKEN_NO_AUTH.to_string()));
        let claims = websocket_claims(&mem).expect("claims");

        assert_eq!(mem.subject_id.as_deref(), Some("builtin:admin"));
        assert_eq!(mem.role, UserRole::Admin);
        assert_eq!(mem.permissions, PERM_ALL);
        assert_eq!(claims.subject_id, Some(UserId::builtin_admin()));
        assert_eq!(claims.roles, RoleSet::ADMIN);
        assert_eq!(claims.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
    }

    fn recording_claims(subject_id: Option<UserId>, permission_schema_version: u16) -> Claims {
        Claims {
            username: "alice".to_string(),
            iss: "test".to_string(),
            iat: 1,
            exp: i64::MAX,
            roles: RoleSet::ADMIN,
            permissions: Permission::RecordingRead.into(),
            pwd_version: 0,
            subject_id,
            permission_schema_version,
        }
    }

    #[test]
    fn websocket_auth_rejects_stale_permission_schema() {
        let mut mem = ProtocolHandlerMemory::default();
        let claims = recording_claims(Some(UserId::from("web:alice")), CURRENT_PERMISSION_SCHEMA_VERSION - 1);

        assert!(!set_websocket_auth(&mut mem, "token".to_string(), &claims));
        assert!(mem.token.is_none());
        assert!(mem.subject_id.is_none());
    }

    #[test]
    fn websocket_auth_rejects_missing_subject() {
        let mut mem = ProtocolHandlerMemory::default();
        let claims = recording_claims(None, CURRENT_PERMISSION_SCHEMA_VERSION);

        assert!(!set_websocket_auth(&mut mem, "token".to_string(), &claims));
        assert!(mem.token.is_none());
        assert!(mem.subject_id.is_none());
    }

    #[test]
    fn current_websocket_auth_preserves_recording_authorization_context() {
        let mut mem = ProtocolHandlerMemory::default();
        let subject_id = UserId::from("web:alice");
        let claims = recording_claims(Some(subject_id.clone()), CURRENT_PERMISSION_SCHEMA_VERSION);

        assert!(set_websocket_auth(&mut mem, "token".to_string(), &claims));
        let Some(snapshot_claims) = websocket_claims(&mem) else {
            unreachable!("authenticated websocket must expose recording claims");
        };

        assert_eq!(snapshot_claims.subject_id, Some(subject_id));
        assert_eq!(snapshot_claims.permissions, claims.permissions);
        assert_eq!(snapshot_claims.roles, claims.roles);
        assert_eq!(snapshot_claims.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
    }

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
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::SystemRead.into(), ..ProtocolHandlerMemory::default() };
        mem.role = UserRole::Admin;

        assert!(websocket_can_receive_runtime_events(&mem, &EventMessage::ServerError("err".to_string())));
    }

    #[test]
    fn test_websocket_runtime_events_allowed_for_system_read_user() {
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::SystemRead.into(), ..ProtocolHandlerMemory::default() };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(&mem, &EventMessage::ServerError("err".to_string())));
    }

    #[test]
    fn test_websocket_runtime_events_denied_without_system_read() {
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::ConfigRead.into(), ..ProtocolHandlerMemory::default() };
        mem.role = UserRole::User;

        assert!(!websocket_can_receive_runtime_events(&mem, &EventMessage::ServerError("err".to_string())));
    }

    #[test]
    fn test_websocket_runtime_events_denied_for_default_permissions() {
        let mem = ProtocolHandlerMemory { role: UserRole::User, ..ProtocolHandlerMemory::default() };

        assert!(!websocket_can_receive_runtime_events(&mem, &EventMessage::ServerError("err".to_string())));
    }

    #[test]
    fn test_websocket_playlist_progress_allowed_for_playlist_write_without_system_read() {
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::PlaylistWrite.into(), ..ProtocolHandlerMemory::default() };
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
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::LibraryWrite.into(), ..ProtocolHandlerMemory::default() };
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
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::DownloadRead.into(), ..ProtocolHandlerMemory::default() };
        mem.role = UserRole::User;

        assert!(websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::DownloadsUpdate(Arc::new(DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: Vec::new(),
            }))
        ));
    }

    #[test]
    fn test_websocket_download_updates_denied_without_download_read() {
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::SystemRead.into(), ..ProtocolHandlerMemory::default() };
        mem.role = UserRole::User;

        assert!(!websocket_can_receive_runtime_events(
            &mem,
            &EventMessage::DownloadsUpdate(Arc::new(DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: Vec::new(),
            }))
        ));
    }

    #[test]
    fn test_websocket_download_delta_updates_allowed_for_download_read_user() {
        let mut mem =
            ProtocolHandlerMemory { permissions: Permission::DownloadRead.into(), ..ProtocolHandlerMemory::default() };
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
                recording: None,
            }))
        ));
    }

    fn recording_lifecycle(event: MsgKind) -> EventMessage {
        EventMessage::RecordingLifecycle(RecordingLifecycleMessage {
            event,
            programme_title: None,
            channel: None,
            effective_start: None,
            effective_end: None,
            visibility: None,
            output_filename: None,
            failure_reason: None,
        })
    }

    fn provider_account(state: ProviderAccountState) -> EventMessage {
        EventMessage::ProviderAccount(ProviderAccountEvent {
            state,
            username: "u".to_string(),
            provider: "p".to_string(),
            status: None,
            expires_at: None,
            message: "m".to_string(),
        })
    }
}
