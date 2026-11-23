use crate::{
    api::{
        auth_middleware::{api_user_context, authenticate, can_api_user_access, ApiUserAccess, ApiUserContext},
        model::AppState,
    },
    auth::{AuthBearer, AuthError, Fingerprint},
    model::ProxyUserCredentials,
};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use shared::model::{
    EventKind, EventKindMask, EventMessage, PlayerClientMessage, PlayerEvent, PlayerRecordingLevel,
    RecordingVisibility, UserId,
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

/// Per-user active WebSocket socket limits.
///
/// Encapsulating the per-user socket count in a module-level static avoids adding fields to
/// `AppState` or mixing lightweight event-stream counters into unrelated streaming managers.
/// Entries are removed as soon as their count reaches zero to guarantee bounded memory.
static USER_SOCKET_COUNTS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

const MAX_SOCKETS_PER_USER: usize = 4;
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const POLICY_VIOLATION_CODE: u16 = 1008;

/// RAII guard that tracks an open event stream socket for a specific user.
#[derive(Debug)]
struct UserSocketGuard {
    username: String,
}

impl Drop for UserSocketGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = USER_SOCKET_COUNTS.lock() {
            if let Some(count) = map.get_mut(self.username.as_str()) {
                if *count <= 1 {
                    map.remove(self.username.as_str());
                } else {
                    *count -= 1;
                }
            }
        }
    }
}

/// Attempts to reserve an open socket slot for the given username.
fn acquire_user_socket(username: &str) -> Option<UserSocketGuard> {
    let mut map = USER_SOCKET_COUNTS.lock().ok()?;
    if let Some(count) = map.get_mut(username) {
        if *count >= MAX_SOCKETS_PER_USER {
            return None;
        }
        *count += 1;
        Some(UserSocketGuard { username: username.to_string() })
    } else {
        let username_owned = username.to_string();
        map.insert(username_owned.clone(), 1);
        Some(UserSocketGuard { username: username_owned })
    }
}

/// WebSocket upgrade handler for the player user event stream (`GET /api/v1/me/events`).
pub async fn get_me_events(
    State(app_state): State<Arc<AppState>>,
    fingerprint: Fingerprint,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // 1. Check for Authorization: Bearer <token> header at upgrade time.
    if let Ok(AuthBearer(token)) = AuthBearer::from_headers(&headers) {
        let Ok(claims) = authenticate(&app_state, &token).await else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let ctx = match api_user_context(&app_state, claims, &fingerprint) {
            Ok(ctx) => ctx,
            Err(AuthError::Forbidden) => return StatusCode::FORBIDDEN.into_response(),
            Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
        };

        if !ctx.user.player_features.event_stream {
            return StatusCode::FORBIDDEN.into_response();
        }

        let Some(guard) = acquire_user_socket(&ctx.claims.username) else {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        };

        return ws.on_upgrade(move |socket| handle_socket(socket, app_state, Some(ctx), Some(guard), fingerprint));
    }

    // 2. Without header: accept upgrade and wait for in-band auth message.
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, None, None, fingerprint))
}

/// Main socket loop handling in-band authentication, event forwarding, and ping/pong.
#[allow(clippy::too_many_lines)]
async fn handle_socket(
    mut socket: WebSocket,
    app_state: Arc<AppState>,
    initial_ctx: Option<ApiUserContext>,
    initial_guard: Option<UserSocketGuard>,
    fingerprint: Fingerprint,
) {
    let (ctx, _guard) = if let (Some(c), Some(g)) = (initial_ctx, initial_guard) {
        (c, g)
    } else {
        let auth_timeout = tokio::time::sleep(AUTH_TIMEOUT);
        tokio::pin!(auth_timeout);
        tokio::select! {
            () = &mut auth_timeout => {
                let _ = socket.send(Message::Close(Some(CloseFrame {
                    code: POLICY_VIOLATION_CODE,
                    reason: "Auth timeout".into(),
                }))).await;
                return;
            }
            msg = socket.recv() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: POLICY_VIOLATION_CODE,
                        reason: "Expected text frame".into(),
                    }))).await;
                    return;
                };
                let Ok(PlayerClientMessage::Auth { token }) = serde_json::from_str::<PlayerClientMessage>(&text) else {
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: POLICY_VIOLATION_CODE,
                        reason: "Invalid auth message".into(),
                    }))).await;
                    return;
                };
                let Ok(claims) = authenticate(&app_state, &token).await else {
                    send_error_and_close(&mut socket, "unauthorized", "Unauthorized").await;
                    return;
                };
                let ctx = match api_user_context(&app_state, claims, &fingerprint) {
                    Ok(ctx) => ctx,
                    Err(AuthError::Forbidden) => {
                        send_error_and_close(&mut socket, "forbidden", "Forbidden").await;
                        return;
                    }
                    Err(_) => {
                        send_error_and_close(&mut socket, "unauthorized", "Unauthorized").await;
                        return;
                    }
                };
                if !ctx.user.player_features.event_stream {
                    send_error_and_close(&mut socket, "feature_disabled", "Feature disabled").await;
                    return;
                }
                let Some(guard) = acquire_user_socket(&ctx.claims.username) else {
                    send_error_and_close(&mut socket, "too_many_connections", "Too many connections").await;
                    return;
                };
                (ctx, guard)
            }
        }
    };

    let connected = PlayerEvent::Connected { server_time: chrono::Utc::now().timestamp() };
    let Ok(connected_json) = serde_json::to_string(&connected) else {
        return;
    };
    if !send_ws_message(&mut socket, Message::Text(connected_json.into())).await {
        return;
    }

    let user_id = UserId::api(&ctx.claims.username);

    let mask = EventKindMask::new()
        .with(EventKind::StreamEnded)
        .with(EventKind::FailoverGroupMemberSwitched)
        .with(EventKind::RecordingCompleted)
        .with(EventKind::RecordingFailed)
        .with(EventKind::ConnectionDenied);

    let mut event_rx = app_state.event_manager.subscribe_filtered(mask);
    let mut ping_interval = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                if !send_ws_message(&mut socket, Message::Ping(vec![].into())).await {
                    break;
                }
            }
            client_msg = socket.recv() => {
                let Some(Ok(msg)) = client_msg else {
                    break;
                };
                match msg {
                    Message::Ping(payload) => {
                        if !send_ws_message(&mut socket, Message::Pong(payload)).await {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            event = event_rx.recv() => {
                let event = match event {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let Ok(resync) = serde_json::to_string(&PlayerEvent::ResyncRequired) else { break };
                        if !send_ws_message(&mut socket, Message::Text(resync.into())).await {
                            break;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                if !event_might_belong_to_user(&event, &ctx.claims.username, &user_id) {
                    continue;
                }

                let live_user = app_state.app_config.get_user_credentials(&ctx.claims.username);
                if let Some(reason) = live_user_close_reason(&app_state, live_user.as_deref()) {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: POLICY_VIOLATION_CODE,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
                let Some(live_user) = live_user else {
                    break;
                };

                if let Some(player_event) = filter_event_for_user(&app_state, &live_user, event).await {
                    let Ok(json) = serde_json::to_string(&player_event) else {
                        continue;
                    };
                    if !send_ws_message(&mut socket, Message::Text(json.into())).await {
                        break;
                    }
                }
            }
        }
    }
}

fn live_user_close_reason(app_state: &AppState, live_user: Option<&ProxyUserCredentials>) -> Option<&'static str> {
    let Some(live_user) = live_user else {
        return Some("User deleted");
    };
    if live_user.permission_denied(&app_state.app_config) {
        return Some("Permission denied");
    }
    if !can_api_user_access(app_state.app_config.config.load().web_ui.as_ref(), live_user, ApiUserAccess::PlayerApi) {
        return Some("Player API disabled");
    }
    if !live_user.player_features.event_stream {
        return Some("Feature disabled");
    }
    None
}

async fn send_ws_message(socket: &mut WebSocket, msg: Message) -> bool {
    let res = tokio::time::timeout(SEND_TIMEOUT, socket.send(msg)).await;
    !matches!(res, Err(_) | Ok(Err(_)))
}

async fn send_error_and_close(socket: &mut WebSocket, code: &str, reason: &'static str) {
    let err_msg = serde_json::to_string(&PlayerEvent::Error { code: code.to_string() }).unwrap_or_default();
    let _ = socket.send(Message::Text(err_msg.into())).await;
    let _ = socket.send(Message::Close(Some(CloseFrame { code: POLICY_VIOLATION_CODE, reason: reason.into() }))).await;
}

/// Cheap pre-filter to drop broadcast events that cannot belong to the connected user
/// before querying live user credentials and active stream allocations.
fn event_might_belong_to_user(event: &EventMessage, username: &str, user_id: &UserId) -> bool {
    match event {
        EventMessage::StreamEnded { username: ev_username, .. } => ev_username.as_ref() == username,
        EventMessage::ConnectionDenied(denied) => denied.username.as_ref() == username,
        EventMessage::FailoverGroupMemberSwitched { .. } => true,
        EventMessage::RecordingFinished { metadata, .. } => {
            if metadata.visibility == RecordingVisibility::Private {
                metadata.owner_id() == user_id
            } else {
                true
            }
        }
        _ => false,
    }
}

/// Evaluates whether an event should be delivered to the connected user and maps it to a `PlayerEvent`.
async fn filter_event_for_user(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    event: EventMessage,
) -> Option<PlayerEvent> {
    match event {
        EventMessage::StreamEnded { username, uid, virtual_id, reason } => {
            if username.as_ref() == user.username.as_str() {
                Some(PlayerEvent::StreamEnded { uid, virtual_id, reason })
            } else {
                None
            }
        }
        EventMessage::FailoverGroupMemberSwitched { target_id, group_vid, from_member_vid, to_member_vid, .. } => {
            if !user.player_features.source_switch {
                return None;
            }
            let watching = app_state
                .active_users
                .user_has_stream(&user.username, |s| {
                    s.channel.target_id == target_id
                        && (s.channel.virtual_id == group_vid || s.channel.group_virtual_id == Some(group_vid))
                })
                .await;
            if watching {
                Some(PlayerEvent::FailoverMemberSwitched {
                    group_virtual_id: group_vid,
                    from_member_vid,
                    to_member_vid,
                })
            } else {
                None
            }
        }
        EventMessage::RecordingFinished { id, state, metadata } => {
            if user.player_features.recording == PlayerRecordingLevel::None {
                return None;
            }
            let live_claims = tuliprox_core::model::api_user_claims(user);
            let subject_id = live_claims.subject_id.as_ref()?;
            let visible = crate::api::endpoints::recording_api::check_recording_visibility_with_context(
                app_state,
                &metadata,
                &live_claims,
                subject_id,
                None,
            )
            .await;
            if visible {
                Some(PlayerEvent::RecordingFinished { id, state })
            } else {
                None
            }
        }
        EventMessage::ConnectionDenied(denied) => {
            if denied.username.as_ref() == user.username.as_str() {
                Some(PlayerEvent::ConnectionDenied { max_connections: denied.max_connections })
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use crate::{
        api::model::{create_test_app_state, ConnectionKind, ConnectionParams},
        auth::{create_jwt_api_user, Fingerprint},
        model::{ApiProxyConfig, Config, ConfigSource, ConfigTarget, SourcesConfig},
    };
    use futures::{SinkExt, StreamExt};
    use shared::model::{
        ApiProxyConfigDto, ConnectionDenied, FailoverGroupsTargetConfig, M3uPlaylistItem, PlayerFeaturesDto,
        PlaylistItemType, ProxyUserCredentialsDto, RecordingFinishedState, RecordingMetadata, RecordingOwner,
        RecordingSource, RecordingVisibility, StreamChannel, StreamEndedReason, SwitchReason, TargetUserDto, UserId,
        WebAuthConfigDto, WebUiConfigDto, XtreamCluster,
    };
    use std::net::SocketAddr;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, Message as TungsteniteMessage},
    };

    struct TestEnv {
        _dir: TempDir,
        app_state: Arc<AppState>,
    }

    impl TestEnv {
        fn create_user(
            &self,
            username: &str,
            event_stream: bool,
            source_switch: bool,
            recording: PlayerRecordingLevel,
        ) -> String {
            let cred_dto = ProxyUserCredentialsDto {
                username: username.to_string(),
                password: "password".to_string(),
                ui_enabled: true,
                max_connections: 5,
                player_features: Some(PlayerFeaturesDto {
                    event_stream: Some(event_stream),
                    source_switch: Some(source_switch),
                    recording: Some(recording),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut user = ProxyUserCredentials::from(&cred_dto);
            user.resolve_plan(&HashMap::new());

            let mut api_proxy =
                self.app_state.app_config.api_proxy.load_full().as_ref().map(|p| (**p).clone()).unwrap_or_default();
            if let Some(target_user) = api_proxy.user.first_mut() {
                target_user.credentials.retain(|c| c.username != username);
                target_user.credentials.push(Arc::new(user));
            }
            self.app_state.app_config.set_api_proxy(api_proxy).unwrap();

            let web_auth =
                self.app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
            create_jwt_api_user(&web_auth, username, UserId::api(username)).unwrap()
        }

        fn update_user(&self, username: &str, event_stream: bool, source_switch: bool) {
            let mut api_proxy =
                self.app_state.app_config.api_proxy.load_full().as_ref().map(|p| (**p).clone()).unwrap_or_default();
            if let Some(target_user) = api_proxy.user.first_mut() {
                if let Some(pos) = target_user.credentials.iter().position(|c| c.username == username) {
                    let old = &target_user.credentials[pos];
                    let mut cred_dto = ProxyUserCredentialsDto::from(old.as_ref());
                    cred_dto.player_features = Some(PlayerFeaturesDto {
                        event_stream: Some(event_stream),
                        source_switch: Some(source_switch),
                        ..Default::default()
                    });
                    let mut updated = ProxyUserCredentials::from(&cred_dto);
                    updated.resolve_plan(&HashMap::new());
                    target_user.credentials[pos] = Arc::new(updated);
                }
            }
            self.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }

        fn update_user_recording(&self, username: &str, recording: PlayerRecordingLevel) {
            let mut api_proxy =
                self.app_state.app_config.api_proxy.load_full().as_ref().map(|p| (**p).clone()).unwrap_or_default();
            if let Some(target_user) = api_proxy.user.first_mut() {
                if let Some(pos) = target_user.credentials.iter().position(|c| c.username == username) {
                    let old = &target_user.credentials[pos];
                    let mut cred_dto = ProxyUserCredentialsDto::from(old.as_ref());
                    if let Some(features) = &mut cred_dto.player_features {
                        features.recording = Some(recording);
                    }
                    let mut updated = ProxyUserCredentials::from(&cred_dto);
                    updated.resolve_plan(&HashMap::new());
                    target_user.credentials[pos] = Arc::new(updated);
                }
            }
            self.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }

        fn update_user_api_enabled(&self, username: &str, api_enabled: Option<bool>) {
            let mut api_proxy =
                self.app_state.app_config.api_proxy.load_full().as_ref().map(|p| (**p).clone()).unwrap_or_default();
            if let Some(target_user) = api_proxy.user.first_mut() {
                if let Some(pos) = target_user.credentials.iter().position(|c| c.username == username) {
                    let old = &target_user.credentials[pos];
                    let mut cred_dto = ProxyUserCredentialsDto::from(old.as_ref());
                    if let Some(features) = &mut cred_dto.player_features {
                        features.api_enabled = api_enabled;
                    } else {
                        cred_dto.player_features = Some(PlayerFeaturesDto { api_enabled, ..Default::default() });
                    }
                    let mut updated = ProxyUserCredentials::from(&cred_dto);
                    updated.resolve_plan(&HashMap::new());
                    target_user.credentials[pos] = Arc::new(updated);
                }
            }
            self.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }
    }

    fn setup_env() -> TestEnv {
        let dir = TempDir::new().unwrap();
        let web_auth_dto = WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret".to_string(),
            ..Default::default()
        };
        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            auth: Some(web_auth_dto.clone()),
            user_ui_enabled: true,
            ..Default::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().to_string(),
            web_ui: Some((&web_ui_dto).into()),
            ..Default::default()
        };
        let app_state = create_test_app_state(config);

        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: Some(shared::model::ConfigTargetOptions {
                failover_groups: Some(FailoverGroupsTargetConfig { enabled: true, ..Default::default() }),
                share_live_streams: shared::model::ConfigTargetShareLiveStreams { hls: true, mpeg_ts: true },
                ..Default::default()
            }),
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput::from(
                &shared::model::M3uTargetOutputDto::default(),
            ))],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });

        let sources = SourcesConfig {
            inputs: Vec::new(),
            sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![Arc::clone(&target)] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let proxy_dto = ApiProxyConfigDto {
            user: vec![TargetUserDto { target: "main".to_string(), credentials: Vec::new() }],
            ..Default::default()
        };
        let api_proxy = ApiProxyConfig::from(&proxy_dto);
        app_state.app_config.set_api_proxy(api_proxy).unwrap();

        TestEnv { _dir: dir, app_state }
    }

    async fn start_test_server(app_state: Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let router =
            crate::api::endpoints::me_player_api::me_player_api_register("").with_state(Arc::clone(&app_state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn test_auth_via_header_receives_connected() {
        let env = setup_env();
        let token = env.create_user("alice_hdr", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());

        let (mut socket, response) = connect_async(req).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::SWITCHING_PROTOCOLS);

        let frame = socket.next().await.unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert!(matches!(event, PlayerEvent::Connected { .. }));
        } else {
            panic!("expected text frame");
        }

        socket.close(None).await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn test_auth_via_first_message_receives_connected() {
        let env = setup_env();
        let token = env.create_user("alice_msg", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let (mut socket, response) = connect_async(url).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::SWITCHING_PROTOCOLS);

        let auth_msg = serde_json::to_string(&PlayerClientMessage::Auth { token }).unwrap();
        socket.send(TungsteniteMessage::Text(auth_msg.into())).await.unwrap();

        let frame = socket.next().await.unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert!(matches!(event, PlayerEvent::Connected { .. }));
        } else {
            panic!("expected text frame");
        }

        socket.close(None).await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn test_gate_403_header_rejected_before_upgrade() {
        let env = setup_env();
        let token = env.create_user("eve_dis_hdr", false, false, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());

        let res = connect_async(req).await;
        match res {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
            }
            _ => panic!("expected 403 Forbidden rejection"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_gate_error_frame_first_message() {
        let env = setup_env();
        let token = env.create_user("eve_dis_msg", false, false, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let (mut socket, _) = connect_async(url).await.unwrap();

        let auth_msg = serde_json::to_string(&PlayerClientMessage::Auth { token }).unwrap();
        socket.send(TungsteniteMessage::Text(auth_msg.into())).await.unwrap();

        let frame = socket.next().await.unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(event, PlayerEvent::Error { code: "feature_disabled".to_string() });
        } else {
            panic!("expected error text frame");
        }

        let close = socket.next().await.unwrap().unwrap();
        assert!(matches!(close, TungsteniteMessage::Close(_)));

        handle.abort();
    }

    #[tokio::test]
    async fn test_fifth_socket_refused_header() {
        let env = setup_env();
        let token = env.create_user("alice_5th_hdr", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let mut sockets = Vec::new();
        for _ in 0..4 {
            let url = format!("ws://{addr}/api/v1/me/events");
            let mut req = url.into_client_request().unwrap();
            req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
            let (mut s, _) = connect_async(req).await.unwrap();
            let _ = s.next().await.unwrap().unwrap(); // Connected greeting
            sockets.push(s);
        }

        // 5th socket
        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        match connect_async(req).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
            }
            _ => panic!("expected 429 Too Many Requests"),
        }

        // Drop one socket and verify 5th can now connect
        let mut first = sockets.pop().unwrap();
        first.close(None).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut connected_socket = None;
        while tokio::time::Instant::now() < deadline {
            let url = format!("ws://{addr}/api/v1/me/events");
            let mut req = url.into_client_request().unwrap();
            req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
            match connect_async(req).await {
                Ok((socket, _)) => {
                    connected_socket = Some(socket);
                    break;
                }
                Err(tokio_tungstenite::tungstenite::Error::Http(resp))
                    if resp.status() == axum::http::StatusCode::TOO_MANY_REQUESTS =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("unexpected error during reconnect polling: {e}"),
            }
        }
        let mut new_socket = connected_socket.expect("timed out waiting for socket slot to free up");
        let greeting = new_socket.next().await.unwrap().unwrap();
        assert!(matches!(greeting, TungsteniteMessage::Text(_)));

        handle.abort();
    }

    #[tokio::test]
    async fn test_fifth_socket_refused_first_message() {
        let env = setup_env();
        let token = env.create_user("alice_5th_msg", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let mut sockets = Vec::new();
        for _ in 0..4 {
            let url = format!("ws://{addr}/api/v1/me/events");
            let mut req = url.into_client_request().unwrap();
            req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
            let (mut s, _) = connect_async(req).await.unwrap();
            let _ = s.next().await.unwrap().unwrap(); // Connected greeting
            sockets.push(s);
        }

        // 5th socket connects without header
        let url = format!("ws://{addr}/api/v1/me/events");
        let (mut fifth_socket, _) = connect_async(url).await.unwrap();
        let auth_msg = serde_json::to_string(&PlayerClientMessage::Auth { token }).unwrap();
        fifth_socket.send(TungsteniteMessage::Text(auth_msg.into())).await.unwrap();

        let frame = fifth_socket.next().await.unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(event, PlayerEvent::Error { code: "too_many_connections".to_string() });
        } else {
            panic!("expected error text frame");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_events_delivered_only_to_owner() {
        let env = setup_env();
        let token_alice = env.create_user("alice_own", true, true, PlayerRecordingLevel::Read);
        let token_bob = env.create_user("bob_own", true, false, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        // Connect Alice
        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req_a = url.into_client_request().unwrap();
        req_a.headers_mut().insert("Authorization", format!("Bearer {token_alice}").parse().unwrap());
        let (mut socket_alice, _) = connect_async(req_a).await.unwrap();
        let _ = socket_alice.next().await.unwrap().unwrap(); // Connected

        // Connect Bob
        let url_b = format!("ws://{addr}/api/v1/me/events");
        let mut req_b = url_b.into_client_request().unwrap();
        req_b.headers_mut().insert("Authorization", format!("Bearer {token_bob}").parse().unwrap());
        let (mut socket_bob, _) = connect_async(req_b).await.unwrap();
        let _ = socket_bob.next().await.unwrap().unwrap(); // Connected

        // Emit stream ended for Alice
        env.app_state.event_manager.send_event(EventMessage::StreamEnded {
            username: Arc::from("alice_own"),
            uid: 42,
            virtual_id: 100,
            reason: StreamEndedReason::Kicked,
        });

        // Alice receives stream_ended
        let frame_a =
            tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame_a {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(event, PlayerEvent::StreamEnded { uid: 42, virtual_id: 100, reason: StreamEndedReason::Kicked });
        } else {
            panic!("expected text frame for Alice");
        }

        // Bob receives nothing
        let frame_b = tokio::time::timeout(Duration::from_millis(100), socket_bob.next()).await;
        assert!(frame_b.is_err(), "Bob should not receive Alice's stream_ended event");

        // Emit connection denied for Alice
        env.app_state.event_manager.send_event(EventMessage::ConnectionDenied(ConnectionDenied {
            username: Arc::from("alice_own"),
            client_ip: Arc::from("127.0.0.1"),
            max_connections: 2,
            soft_connections: 0,
        }));

        let frame_a2 =
            tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame_a2 {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(event, PlayerEvent::ConnectionDenied { max_connections: 2 });
        } else {
            panic!("expected text frame for Alice");
        }

        let bob_denied_frame = tokio::time::timeout(Duration::from_millis(100), socket_bob.next()).await;
        assert!(bob_denied_frame.is_err(), "Bob should not receive Alice's connection_denied event");

        // Emit recording finished for Alice
        let alice_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("alice_own")),
            RecordingVisibility::Private,
            RecordingSource::new("main", "42", "input-a").with_cluster(XtreamCluster::Video),
            "Film".to_string(),
        );
        env.app_state.event_manager.send_event(EventMessage::RecordingFinished {
            id: "rec-alice-1".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(alice_meta),
        });

        let frame_a3 =
            tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame_a3 {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(
                event,
                PlayerEvent::RecordingFinished {
                    id: "rec-alice-1".to_string(),
                    state: RecordingFinishedState::Completed,
                }
            );
        } else {
            panic!("expected recording_finished text frame for Alice");
        }

        let bob_rec_frame = tokio::time::timeout(Duration::from_millis(100), socket_bob.next()).await;
        assert!(bob_rec_frame.is_err(), "Bob should not receive Alice's private recording_finished event");

        handle.abort();
    }

    async fn register_stream(
        app_state: &AppState,
        meter_uid: u32,
        username: &str,
        virtual_id: u32,
        group_virtual_id: Option<u32>,
    ) {
        let addr: SocketAddr = format!("127.0.0.1:{}", 8000 + meter_uid).parse().unwrap();
        let fp = Fingerprint::new("fp".into(), "127.0.0.1".into(), addr);
        let channel = StreamChannel {
            target_id: 1,
            virtual_id,
            group_virtual_id,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test Stream".into(),
            url: "http://example.com/stream.ts".into(),
            shared: false,
            ..Default::default()
        };
        let connection = ConnectionParams {
            meter_uid,
            username,
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp,
            provider: "test-prov".into(),
            stream_channel: &channel,
            user_agent: "test-ua".into(),
            session_token: Some("token-test"),
        };
        let _ = app_state.connection_manager.update_connection(connection).await;
    }

    #[tokio::test]
    async fn test_failover_event_requires_active_stream_and_source_switch() {
        let env = setup_env();
        let token_alice = env.create_user("alice_failover", true, true, PlayerRecordingLevel::None);
        let token_bob = env.create_user("bob_failover", true, false, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        // Register active stream on group 200 for Alice
        register_stream(&env.app_state, 10, "alice_failover", 200, Some(200)).await;

        // Connect Alice
        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req_a = url.clone().into_client_request().unwrap();
        req_a.headers_mut().insert("Authorization", format!("Bearer {token_alice}").parse().unwrap());
        let (mut socket_alice, _) = connect_async(req_a).await.unwrap();
        let _ = socket_alice.next().await.unwrap().unwrap(); // Connected

        // Connect Bob (Bob does not watch group 200, and has source_switch: false)
        let mut req_b = url.into_client_request().unwrap();
        req_b.headers_mut().insert("Authorization", format!("Bearer {token_bob}").parse().unwrap());
        let (mut socket_bob, _) = connect_async(req_b).await.unwrap();
        let _ = socket_bob.next().await.unwrap().unwrap(); // Connected

        // Emit failover for group 200
        env.app_state.event_manager.send_event(EventMessage::FailoverGroupMemberSwitched {
            target_id: 1,
            group_vid: 200,
            from_member_vid: 1001,
            to_member_vid: 1002,
            reason: SwitchReason::Manual,
        });

        // Alice receives failover
        let frame_a =
            tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Text(text) = frame_a {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(
                event,
                PlayerEvent::FailoverMemberSwitched {
                    group_virtual_id: 200,
                    from_member_vid: 1001,
                    to_member_vid: 1002,
                }
            );
        } else {
            panic!("expected failover text frame for Alice");
        }

        // Bob receives nothing
        let frame_b = tokio::time::timeout(Duration::from_millis(100), socket_bob.next()).await;
        assert!(frame_b.is_err(), "Bob should not receive failover event");

        handle.abort();
    }

    #[tokio::test]
    async fn test_revoked_feature_stops_delivery() {
        let env = setup_env();
        let token_alice = env.create_user("alice_revoked", true, true, PlayerRecordingLevel::Read);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        // Register active stream on group 300 for Alice
        register_stream(&env.app_state, 11, "alice_revoked", 300, Some(300)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req_a = url.into_client_request().unwrap();
        req_a.headers_mut().insert("Authorization", format!("Bearer {token_alice}").parse().unwrap());
        let (mut socket_alice, _) = connect_async(req_a).await.unwrap();
        let _ = socket_alice.next().await.unwrap().unwrap(); // Connected

        // First failover succeeds
        env.app_state.event_manager.send_event(EventMessage::FailoverGroupMemberSwitched {
            target_id: 1,
            group_vid: 300,
            from_member_vid: 2001,
            to_member_vid: 2002,
            reason: SwitchReason::Manual,
        });
        let frame_a =
            tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(frame_a, TungsteniteMessage::Text(_)));

        // Revoke source_switch in live config
        env.update_user("alice_revoked", true, false);

        // Next failover event is suppressed
        env.app_state.event_manager.send_event(EventMessage::FailoverGroupMemberSwitched {
            target_id: 1,
            group_vid: 300,
            from_member_vid: 2002,
            to_member_vid: 2003,
            reason: SwitchReason::Manual,
        });
        let suppressed = tokio::time::timeout(Duration::from_millis(100), socket_alice.next()).await;
        assert!(suppressed.is_err(), "revoked source_switch must stop failover events");

        // Now revoke event_stream completely
        env.update_user("alice_revoked", false, false);

        // Next event triggers live re-eval and closes socket
        env.app_state.event_manager.send_event(EventMessage::StreamEnded {
            username: Arc::from("alice_revoked"),
            uid: 11,
            virtual_id: 300,
            reason: StreamEndedReason::Kicked,
        });
        let closed = tokio::time::timeout(Duration::from_millis(500), socket_alice.next()).await.unwrap();
        match closed {
            None | Some(Ok(TungsteniteMessage::Close(None))) => {}
            Some(Ok(TungsteniteMessage::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), POLICY_VIOLATION_CODE);
            }
            other => panic!("expected close frame or EOF, got {other:?}"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_lag_sends_resync_required() {
        let env = setup_env();
        let token = env.create_user("alice_lag", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req_a = url.into_client_request().unwrap();
        req_a.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = connect_async(req_a).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        // Flood event bus beyond broadcast channel capacity to trigger lag
        for i in 0..1500 {
            env.app_state.event_manager.send_event(EventMessage::StreamEnded {
                username: Arc::from("alice_lag"),
                uid: i,
                virtual_id: 100,
                reason: StreamEndedReason::Preempted,
            });
        }

        // Drain messages until resync_required appears
        let mut saw_resync = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(Ok(TungsteniteMessage::Text(text)))) =
                tokio::time::timeout(Duration::from_millis(200), socket.next()).await
            {
                if let Ok(event) = serde_json::from_str::<PlayerEvent>(&text) {
                    if matches!(event, PlayerEvent::ResyncRequired) {
                        saw_resync = true;
                        break;
                    }
                }
            }
        }
        assert!(saw_resync, "must receive ResyncRequired on broadcast lag");

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_timeout_without_auth() {
        let env = setup_env();
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let (mut socket, response) = connect_async(url).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::SWITCHING_PROTOCOLS);

        tokio::time::advance(Duration::from_secs(6)).await;

        let frame = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("server must close after 5s auth timeout")
            .unwrap()
            .unwrap();
        if let TungsteniteMessage::Close(Some(close)) = frame {
            assert_eq!(u16::from(close.code), POLICY_VIOLATION_CODE);
            assert_eq!(close.reason, "Auth timeout");
        } else {
            panic!("expected close frame with policy violation code");
        }

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_ping_server_30s() {
        let env = setup_env();
        let token = env.create_user("alice_ping_srv", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());

        let (mut socket, _) = connect_async(req).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        tokio::time::advance(Duration::from_secs(31)).await;

        let frame = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("server ping should arrive")
            .unwrap()
            .unwrap();
        assert!(matches!(frame, TungsteniteMessage::Ping(_)));

        socket.close(None).await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn test_ping_pong() {
        let env = setup_env();
        let token = env.create_user("alice_pong", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());

        let (mut socket, _) = connect_async(req).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        // Direct ping from client to verify pong
        socket.send(TungsteniteMessage::Ping(bytes::Bytes::from_static(b"hello"))).await.unwrap();
        let pong = tokio::time::timeout(Duration::from_millis(500), socket.next()).await.unwrap().unwrap().unwrap();
        if let TungsteniteMessage::Pong(payload) = pong {
            assert_eq!(&payload[..], b"hello");
        } else {
            panic!("expected pong frame");
        }

        socket.close(None).await.unwrap();
        handle.abort();
    }

    #[test]
    fn test_cheap_event_filter_variants() {
        let alice_id = UserId::api("alice");
        let bob_id = UserId::api("bob");
        let stream_ended_alice = EventMessage::StreamEnded {
            username: Arc::from("alice"),
            uid: 1,
            virtual_id: 10,
            reason: StreamEndedReason::Kicked,
        };
        assert!(event_might_belong_to_user(&stream_ended_alice, "alice", &alice_id));
        assert!(!event_might_belong_to_user(&stream_ended_alice, "bob", &bob_id));

        let denied_alice = EventMessage::ConnectionDenied(ConnectionDenied {
            username: Arc::from("alice"),
            client_ip: Arc::from("127.0.0.1"),
            max_connections: 1,
            soft_connections: 0,
        });
        assert!(event_might_belong_to_user(&denied_alice, "alice", &alice_id));
        assert!(!event_might_belong_to_user(&denied_alice, "bob", &bob_id));

        let failover = EventMessage::FailoverGroupMemberSwitched {
            target_id: 1,
            group_vid: 100,
            from_member_vid: 101,
            to_member_vid: 102,
            reason: SwitchReason::Stall,
        };
        assert!(event_might_belong_to_user(&failover, "alice", &alice_id));
        assert!(event_might_belong_to_user(&failover, "bob", &bob_id));

        let meta_alice = RecordingMetadata::new_live(
            RecordingOwner::User(UserId::api("alice")),
            RecordingVisibility::Private,
            RecordingSource::new("target-1", "v-1", "input-a"),
            1000,
            2000,
            0,
            0,
        );
        let rec_finished_alice = EventMessage::RecordingFinished {
            id: "rec-1".into(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(meta_alice),
        };
        assert!(event_might_belong_to_user(&rec_finished_alice, "alice", &alice_id));
        assert!(!event_might_belong_to_user(&rec_finished_alice, "bob", &bob_id));

        assert!(!event_might_belong_to_user(&EventMessage::RecordingRulesChanged, "alice", &alice_id));
    }

    #[tokio::test]
    async fn test_user_upgraded_to_read_receives_shared_recording_event() {
        let env = setup_env();

        let storage = tuliprox_repository::ensure_target_storage_path(&env.app_state.app_config.config.load(), "main")
            .await
            .unwrap();
        let path = tuliprox_repository::m3u_get_file_path_for_db(&storage);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut tree = tuliprox_repository::BPlusTree::new();
        let item = shared::model::PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                virtual_id: shared::model::VirtualId::new(42),
                input_name: "input-a".into(),
                name: "Film".into(),
                title: "Film".into(),
                group: "Movies".into(),
                url: "http://upstream.example/film.mp4".into(),
                item_type: shared::model::PlaylistItemType::Video,
                xtream_cluster: shared::model::XtreamCluster::Video,
                ..Default::default()
            },
        };
        tree.insert(item.header.virtual_id, M3uPlaylistItem::from(&item));
        tree.store(&path).unwrap();

        let token = env.create_user("charlie_upg", true, false, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = connect_async(req).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        // Upgrade user to Read while connected
        env.update_user_recording("charlie_upg", PlayerRecordingLevel::Read);

        // Emit shared recording event
        let shared_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("other_user")),
            RecordingVisibility::Shared,
            RecordingSource::new("main", "42", "input-a").with_cluster(XtreamCluster::Video),
            "Film".to_string(),
        );
        env.app_state.event_manager.send_event(EventMessage::RecordingFinished {
            id: "rec-shared-1".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(shared_meta),
        });

        let frame = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("timed out waiting for shared recording event")
            .unwrap()
            .unwrap();
        if let TungsteniteMessage::Text(text) = frame {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(
                event,
                PlayerEvent::RecordingFinished {
                    id: "rec-shared-1".to_string(),
                    state: RecordingFinishedState::Completed,
                }
            );
        } else {
            panic!("expected recording_finished text frame");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_token_without_subject_receives_recording_event() {
        let env = setup_env();

        let storage = tuliprox_repository::ensure_target_storage_path(&env.app_state.app_config.config.load(), "main")
            .await
            .unwrap();
        let path = tuliprox_repository::m3u_get_file_path_for_db(&storage);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut tree = tuliprox_repository::BPlusTree::new();
        let item = shared::model::PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                virtual_id: shared::model::VirtualId::new(42),
                input_name: "input-a".into(),
                name: "Film".into(),
                title: "Film".into(),
                group: "Movies".into(),
                url: "http://upstream.example/film.mp4".into(),
                item_type: shared::model::PlaylistItemType::Video,
                xtream_cluster: shared::model::XtreamCluster::Video,
                ..Default::default()
            },
        };
        tree.insert(item.header.virtual_id, M3uPlaylistItem::from(&item));
        tree.store(&path).unwrap();

        let _ = env.create_user("no_sub_user", true, false, PlayerRecordingLevel::Read);
        let user = env.app_state.app_config.get_user_credentials("no_sub_user").unwrap();
        let shared_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("other_user")),
            RecordingVisibility::Shared,
            RecordingSource::new("main", "42", "input-a").with_cluster(XtreamCluster::Video),
            "Film".to_string(),
        );
        let event = EventMessage::RecordingFinished {
            id: "rec-no-sub-1".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(shared_meta),
        };

        let filtered = filter_event_for_user(&env.app_state, &user, event).await;
        assert_eq!(
            filtered,
            Some(PlayerEvent::RecordingFinished {
                id: "rec-no-sub-1".to_string(),
                state: RecordingFinishedState::Completed,
            })
        );
    }

    #[tokio::test]
    async fn test_live_user_close_reason_all_cases() {
        let env = setup_env();

        // 1. User deleted (None) -> "User deleted"
        assert_eq!(live_user_close_reason(&env.app_state, None), Some("User deleted"));

        // 2. Permission denied (permission_denied returns true) -> "Permission denied"
        let denied_dto = ProxyUserCredentialsDto {
            username: "denied".to_string(),
            password: "p".to_string(),
            plan: Some("unknown_plan".to_string()),
            ..Default::default()
        };
        let mut denied_user = ProxyUserCredentials::from(&denied_dto);
        denied_user.resolve_plan(&HashMap::new());
        assert_eq!(live_user_close_reason(&env.app_state, Some(&denied_user)), Some("Permission denied"));

        // 3. Player API disabled (can_api_user_access returns false) -> "Player API disabled"
        let papi_dto = ProxyUserCredentialsDto {
            username: "papi_off".to_string(),
            password: "p".to_string(),
            ui_enabled: false,
            player_features: Some(PlayerFeaturesDto {
                api_enabled: Some(false),
                event_stream: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut papi_disabled_user = ProxyUserCredentials::from(&papi_dto);
        papi_disabled_user.resolve_plan(&HashMap::new());
        assert_eq!(live_user_close_reason(&env.app_state, Some(&papi_disabled_user)), Some("Player API disabled"));

        // 4. Feature disabled (event_stream: false) -> "Feature disabled"
        let feat_dto = ProxyUserCredentialsDto {
            username: "feature_off".to_string(),
            password: "p".to_string(),
            ui_enabled: true,
            player_features: Some(PlayerFeaturesDto { event_stream: Some(false), ..Default::default() }),
            ..Default::default()
        };
        let mut feature_off_user = ProxyUserCredentials::from(&feat_dto);
        feature_off_user.resolve_plan(&HashMap::new());
        assert_eq!(live_user_close_reason(&env.app_state, Some(&feature_off_user)), Some("Feature disabled"));

        // 5. Allowed -> None
        let ok_dto = ProxyUserCredentialsDto {
            username: "ok_user".to_string(),
            password: "p".to_string(),
            ui_enabled: true,
            player_features: Some(PlayerFeaturesDto { event_stream: Some(true), ..Default::default() }),
            ..Default::default()
        };
        let mut ok_user = ProxyUserCredentials::from(&ok_dto);
        ok_user.resolve_plan(&HashMap::new());
        assert_eq!(live_user_close_reason(&env.app_state, Some(&ok_user)), None);
    }

    #[tokio::test]
    async fn test_reader_level_user_recording_event_isolation_and_flip() {
        let env = setup_env();

        let storage = tuliprox_repository::ensure_target_storage_path(&env.app_state.app_config.config.load(), "main")
            .await
            .unwrap();
        let path = tuliprox_repository::m3u_get_file_path_for_db(&storage);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut tree = tuliprox_repository::BPlusTree::new();
        let item = shared::model::PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                virtual_id: shared::model::VirtualId::new(42),
                input_name: "input-a".into(),
                name: "Allowed Film".into(),
                title: "Allowed Film".into(),
                group: "Movies".into(),
                url: "http://upstream.example/allowed.mp4".into(),
                item_type: shared::model::PlaylistItemType::Video,
                xtream_cluster: shared::model::XtreamCluster::Video,
                ..Default::default()
            },
        };
        tree.insert(item.header.virtual_id, M3uPlaylistItem::from(&item));
        tree.store(&path).unwrap();

        let token_bob = env.create_user("bob_reader", true, false, PlayerRecordingLevel::Read);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token_bob}").parse().unwrap());
        let (mut socket, _) = connect_async(req).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        // 1. Shared recording from another target ("other_target") -> Bob must NOT receive it
        let foreign_target_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("alice")),
            RecordingVisibility::Shared,
            RecordingSource::new("other_target", "42", "input-a").with_cluster(XtreamCluster::Video),
            "Foreign Film".to_string(),
        );
        env.app_state.event_manager.send_event(EventMessage::RecordingFinished {
            id: "rec-foreign-target".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(foreign_target_meta),
        });

        let frame_foreign = tokio::time::timeout(Duration::from_millis(100), socket.next()).await;
        assert!(frame_foreign.is_err(), "Bob must not receive shared recording from another target");

        // 2. Shared recording on "main" target but unindexed channel (vid 9999) -> Bob must NOT receive it
        let foreign_channel_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("alice")),
            RecordingVisibility::Shared,
            RecordingSource::new("main", "9999", "input-a").with_cluster(XtreamCluster::Video),
            "Unknown Film".to_string(),
        );
        env.app_state.event_manager.send_event(EventMessage::RecordingFinished {
            id: "rec-unknown-channel".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(foreign_channel_meta),
        });

        let frame_channel = tokio::time::timeout(Duration::from_millis(100), socket.next()).await;
        assert!(frame_channel.is_err(), "Bob must not receive shared recording for channel outside target/filter");

        // 3. Flip to valid target and indexed channel (vid 42) -> Bob DOES receive it
        let valid_meta = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api("alice")),
            RecordingVisibility::Shared,
            RecordingSource::new("main", "42", "input-a").with_cluster(XtreamCluster::Video),
            "Allowed Film".to_string(),
        );
        env.app_state.event_manager.send_event(EventMessage::RecordingFinished {
            id: "rec-valid-main".to_string(),
            state: RecordingFinishedState::Completed,
            metadata: Arc::new(valid_meta),
        });

        let frame_valid = tokio::time::timeout(Duration::from_millis(500), socket.next())
            .await
            .expect("timed out waiting for valid shared recording event after flip")
            .unwrap()
            .unwrap();
        if let TungsteniteMessage::Text(text) = frame_valid {
            let event: PlayerEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(
                event,
                PlayerEvent::RecordingFinished {
                    id: "rec-valid-main".to_string(),
                    state: RecordingFinishedState::Completed,
                }
            );
        } else {
            panic!("expected recording_finished text frame");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_revoked_player_api_closes_socket() {
        let env = setup_env();
        let token = env.create_user("alice_papi", true, true, PlayerRecordingLevel::None);
        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = connect_async(req).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap(); // Connected greeting

        // Flip api_enabled to Some(false) via config
        env.update_user_api_enabled("alice_papi", Some(false));

        // Emit an event for the user
        env.app_state.event_manager.send_event(EventMessage::StreamEnded {
            username: Arc::from("alice_papi"),
            uid: 77,
            virtual_id: 100,
            reason: StreamEndedReason::Kicked,
        });

        let closed = tokio::time::timeout(Duration::from_millis(500), socket.next()).await.unwrap();
        match closed {
            Some(Ok(TungsteniteMessage::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), POLICY_VIOLATION_CODE);
                assert_eq!(frame.reason.as_str(), "Player API disabled");
            }
            other => panic!("expected close frame with 'Player API disabled', got {other:?}"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_me_events_websocket_enforces_network_access() {
        use shared::model::NetworkAccessDto;

        let env = setup_env();
        let token = env.create_user("net_alice", true, true, PlayerRecordingLevel::None);
        // Set network_access for net_alice restricting to 10.0.0.0/8
        {
            let mut api_proxy = (*env.app_state.app_config.api_proxy.load_full().unwrap()).clone();
            for target_user in &mut api_proxy.user {
                for cred in &mut target_user.credentials {
                    if cred.username == "net_alice" {
                        Arc::make_mut(cred).network_access =
                            Some(tuliprox_core::model::NetworkAccess::from(&NetworkAccessDto {
                                allowed_countries: None,
                                allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
                            }));
                    }
                }
            }
            env.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }

        let (addr, handle) = start_test_server(Arc::clone(&env.app_state)).await;

        // 1. Header upgrade with denied IP (192.168.1.1): rejected with 403 Forbidden
        let url = format!("ws://{addr}/api/v1/me/events");
        let mut req_denied = url.clone().into_client_request().unwrap();
        req_denied.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        req_denied.headers_mut().insert("x-forwarded-for", "192.168.1.1".parse().unwrap());

        let res_denied = connect_async(req_denied).await;
        match res_denied {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
            }
            other => panic!("expected 403 Forbidden rejection for denied IP, got {other:?}"),
        }

        // 2. Header upgrade with allowed IP (10.0.0.1): succeeds and receives Connected event
        let mut req_allowed = url.clone().into_client_request().unwrap();
        req_allowed.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        req_allowed.headers_mut().insert("x-forwarded-for", "10.0.0.1".parse().unwrap());

        let (mut ws_stream, resp_allowed) = connect_async(req_allowed).await.expect("connect allowed");
        assert_eq!(resp_allowed.status(), reqwest::StatusCode::SWITCHING_PROTOCOLS);
        let msg = ws_stream.next().await.expect("connected msg").expect("valid msg");
        let text = msg.to_text().unwrap();
        let event: PlayerEvent = serde_json::from_str(text).unwrap();
        assert!(matches!(event, PlayerEvent::Connected { .. }));
        let _ = ws_stream.close(None).await;

        // 3. In-band auth frame with denied IP (192.168.1.1): upgrade succeeds without header, auth frame denied
        let mut req_inband = url.into_client_request().unwrap();
        req_inband.headers_mut().insert("x-forwarded-for", "192.168.1.1".parse().unwrap());
        let (mut ws_inband, _) = connect_async(req_inband).await.expect("connect inband");
        let auth_payload = serde_json::to_string(&PlayerClientMessage::Auth { token }).unwrap();
        ws_inband.send(tokio_tungstenite::tungstenite::Message::Text(auth_payload.into())).await.unwrap();
        let err_msg = ws_inband.next().await.expect("close/error msg").expect("msg");
        let err_text = err_msg.to_text().unwrap();
        let err_event: PlayerEvent = serde_json::from_str(err_text).unwrap();
        assert!(matches!(err_event, PlayerEvent::Error { code, .. } if code == "forbidden"));

        handle.abort();
    }
}
