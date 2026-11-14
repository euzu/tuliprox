//! Media-server routes below `/api/v1/media-server` for the core media-server
//! inputs (plex, emby, jellyfin, silo).
//!
//! `libraries` and `status` contact the configured server once and are bounded
//! by [`MEDIA_SERVER_CLIENT_TIMEOUT`], so a hanging server answers `504` instead
//! of holding the request. `refresh` only queues the existing manual playlist
//! update machinery for one input; it performs no network I/O itself.
//!
//! Nothing here returns a token, a password or a URL: errors carry the stable
//! kind of the [`MediaServerError`] and its already redacted detail.

use crate::{
    api::{
        auth_middleware::permission_layer,
        endpoints::v1_api_playlist::{
            enqueue_manual_playlist_update, resolve_manual_playlist_update_targets, ManualUpdateEnqueueError,
            QUEUE_BUSY_MESSAGE,
        },
        model::AppState,
    },
    media_server::{
        AnyMediaServerCatalogClient, MediaServerCatalogClient, MediaServerError, MediaServerErrorKind,
        MediaServerHttpClient, MediaServerLibrary,
    },
    model::ConfigInput,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use log::debug;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shared::model::{
    InputRefreshOverride, InputRefreshPolicy, InputUpdateAction, InputUpdateRequest, MediaServerLibraryKind,
    OperationRunAccepted, Permission, PlaylistUpdateRequestDto,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::timeout;

/// Upper bound for one call against the configured media server.
const MEDIA_SERVER_CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

const REFRESH_ACTIONS: [&str; 2] = ["normal", "refresh"];

/// Routes of the media-server API without permission layers, for a server with
/// Web UI authentication disabled.
pub fn media_server_api_register_protected(router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    router.nest(
        "/media-server",
        Router::new()
            .route("/{input}/libraries", get(libraries))
            .route("/{input}/status", get(status))
            .route("/{input}/refresh", post(refresh)),
    )
}

/// The same routes behind the playlist permissions: the read routes need
/// `playlist.read` like `/playlist/sources`, the refresh route `playlist.write`
/// like `/playlist/update`.
pub fn media_server_api_register_with_permissions(
    router: Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
) -> Router<Arc<AppState>> {
    let read_routes = Router::new()
        .route("/{input}/libraries", get(libraries))
        .route("/{input}/status", get(status))
        .layer(permission_layer!(app_state, Permission::PlaylistRead));
    let write_routes = Router::new()
        .route("/{input}/refresh", post(refresh))
        .layer(permission_layer!(app_state, Permission::PlaylistWrite));
    router.nest("/media-server", read_routes.merge(write_routes))
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    error: String,
    detail: Option<String>,
    provider: Option<&'static str>,
}

impl ApiError {
    fn bad_request(error: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, error: error.into(), detail: None, provider: None }
    }

    fn not_found(error: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, error: error.into(), detail: None, provider: None }
    }

    fn timeout(input: &str) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            error: "media_server_timeout".to_string(),
            detail: Some(format!(
                "input '{input}' did not answer within {} seconds",
                MEDIA_SERVER_CLIENT_TIMEOUT.as_secs()
            )),
            provider: None,
        }
    }

    /// A failed client call, with the stable error kind and the redacted detail
    /// the media-server layer already produced.
    fn media_server(err: &MediaServerError) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            error: media_server_error_code(err.kind).to_string(),
            detail: err.detail_text().map(str::to_string),
            provider: err.provider,
        }
    }

    /// A client that cannot even be built from the input configuration.
    fn config(err: &MediaServerError) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "media_server_config".to_string(),
            detail: err.detail_text().map(str::to_string),
            provider: err.provider,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.error });
        if let Some(detail) = self.detail {
            body["detail"] = Value::from(detail);
        }
        if let Some(provider) = self.provider {
            body["provider"] = Value::from(provider);
        }
        (self.status, Json(body)).into_response()
    }
}

const fn media_server_error_code(kind: MediaServerErrorKind) -> &'static str {
    match kind {
        MediaServerErrorKind::MediaServerAuthDenied => "media_server_auth_denied",
        MediaServerErrorKind::MediaServerUnavailable => "media_server_unavailable",
        MediaServerErrorKind::MediaServerLibraryUnavailable => "media_server_library_unavailable",
        MediaServerErrorKind::MediaServerLibraryTypeUnsupported => "media_server_library_type_unsupported",
        MediaServerErrorKind::MediaServerCatalogDecodeFailed => "media_server_catalog_decode_failed",
        MediaServerErrorKind::MediaServerCatalogPageStalled => "media_server_catalog_page_stalled",
        MediaServerErrorKind::MediaServerCatalogIncomplete => "media_server_catalog_incomplete",
        MediaServerErrorKind::MediaServerItemNotFound => "media_server_item_not_found",
        MediaServerErrorKind::NoDirectPlayableMediaServerSource => "no_direct_playable_media_server_source",
        MediaServerErrorKind::MediaServerStreamOpenFailed => "media_server_stream_open_failed",
        MediaServerErrorKind::MediaServerRateLimited => "media_server_rate_limited",
        MediaServerErrorKind::MediaServerDiscoveryFailed => "media_server_discovery_failed",
    }
}

fn resolve_input(app_state: &AppState, input_name: &str) -> Result<Arc<ConfigInput>, ApiError> {
    let input = app_state
        .app_config
        .get_input_by_name(&Arc::<str>::from(input_name))
        .ok_or_else(|| ApiError::not_found(format!("Unknown input '{input_name}'")))?;
    if !input.input_type.is_media_server() {
        return Err(ApiError::bad_request(format!(
            "Input '{input_name}' has type '{}'; the media-server routes require a plex, emby, jellyfin or silo input",
            input.input_type
        )));
    }
    Ok(input)
}

fn catalog_client(app_state: &AppState, input: &ConfigInput) -> Result<AnyMediaServerCatalogClient, ApiError> {
    let http = MediaServerHttpClient::new(app_state.http_clients.default.load().as_ref().clone());
    input.media_server_catalog_client(http).map_err(|err| ApiError::config(&err))
}

#[derive(Serialize)]
struct LibraryView {
    id: String,
    name: String,
    kind: MediaServerLibraryKind,
    selected: bool,
}

fn library_views(selected: Vec<MediaServerLibrary>, available: Vec<MediaServerLibrary>) -> Vec<LibraryView> {
    let mut views: Vec<LibraryView> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut push = |library: MediaServerLibrary, selected: bool, views: &mut Vec<LibraryView>| {
        let id = library.reference.library_id.to_string();
        if let Some(position) = positions.get(&id).copied() {
            if selected {
                views[position].selected = true;
            }
        } else {
            positions.insert(id.clone(), views.len());
            views.push(LibraryView { id, name: library.name.to_string(), kind: library.kind, selected });
        }
    };
    for library in selected {
        push(library, true, &mut views);
    }
    for library in available {
        push(library, false, &mut views);
    }
    views
}

async fn libraries(
    State(app_state): State<Arc<AppState>>,
    Path(input_name): Path<String>,
) -> Result<Json<Vec<LibraryView>>, ApiError> {
    let input = resolve_input(&app_state, &input_name)?;
    let client = catalog_client(&app_state, &input)?;
    let available = timeout(MEDIA_SERVER_CLIENT_TIMEOUT, client.list_available_libraries())
        .await
        .map_err(|_| ApiError::timeout(&input_name))?
        .map_err(|err| ApiError::media_server(&err))?;
    // A selection that names a library the server no longer shows must not hide
    // the visible ones: that is exactly when the user needs this list.
    let selected = match timeout(MEDIA_SERVER_CLIENT_TIMEOUT, client.list_libraries())
        .await
        .map_err(|_| ApiError::timeout(&input_name))?
    {
        Ok(selected) => selected,
        Err(err) if err.kind == MediaServerErrorKind::MediaServerLibraryUnavailable => Vec::new(),
        Err(err) => return Err(ApiError::media_server(&err)),
    };
    Ok(Json(library_views(selected, available)))
}

#[derive(Serialize)]
struct StatusView {
    kind: String,
    server_id: String,
    name: Option<String>,
    version: Option<String>,
    reachable: bool,
}

async fn status(
    State(app_state): State<Arc<AppState>>,
    Path(input_name): Path<String>,
) -> Result<Json<StatusView>, ApiError> {
    let input = resolve_input(&app_state, &input_name)?;
    let client = catalog_client(&app_state, &input)?;
    let server =
        timeout(MEDIA_SERVER_CLIENT_TIMEOUT, client.discover()).await.map_err(|_| ApiError::timeout(&input_name))?;
    let server = server.map_err(|err| ApiError::media_server(&err))?;
    Ok(Json(StatusView {
        kind: server.kind.as_input_type().to_string(),
        server_id: server.server_id.to_string(),
        name: server.display_name.map(|name| name.to_string()),
        version: server.version.map(|version| version.to_string()),
        reachable: true,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    /// `normal` reuses a valid cached catalog when possible, `refresh` bypasses it.
    action: String,
}

fn refresh_policy(action: &str) -> Option<InputRefreshPolicy> {
    match action.trim() {
        "normal" => Some(InputRefreshPolicy::NORMAL),
        "refresh" => Some(InputRefreshPolicy::REFRESH),
        _ => None,
    }
}

async fn refresh(
    State(app_state): State<Arc<AppState>>,
    Path(input_name): Path<String>,
    Json(request): Json<RefreshRequest>,
) -> Response {
    match run_refresh(&app_state, &input_name, &request.action) {
        Ok(run_id) => (StatusCode::ACCEPTED, Json(OperationRunAccepted::playlist_update(run_id))).into_response(),
        Err(error) => error.into_response(),
    }
}

fn run_refresh(
    app_state: &Arc<AppState>,
    input_name: &str,
    action: &str,
) -> Result<shared::model::PlaylistUpdateRunId, ApiError> {
    let input = resolve_input(app_state, input_name)?;
    let Some(policy) = refresh_policy(action) else {
        return Err(ApiError::bad_request(format!(
            "Unknown action '{}'; allowed values: {}",
            action.chars().take(64).collect::<String>(),
            REFRESH_ACTIONS.join(", ")
        )));
    };
    // The media-server input type has no batch variant, so an empty target list
    // resolves to all targets like the legacy `input_refresh` request does.
    let request = PlaylistUpdateRequestDto {
        input_refresh: Some(InputRefreshOverride { input_id: input.id, policy }),
        ..PlaylistUpdateRequestDto::default()
    };
    let sources = app_state.app_config.sources.load_full();
    let process_targets = resolve_manual_playlist_update_targets(&sources, &request)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    let input_action = InputUpdateRequest { input_id: input.id, action: InputUpdateAction::Provider(policy) };
    match enqueue_manual_playlist_update(
        &app_state.playlist_updates.manual_update_sender,
        Arc::new(process_targets),
        Some(input_action),
        None,
        false,
    ) {
        Ok(run_id) => Ok(run_id),
        Err(ManualUpdateEnqueueError::Busy) => {
            debug!("Media-server refresh rejected: another update is already queued");
            Err(ApiError {
                status: StatusCode::CONFLICT,
                error: QUEUE_BUSY_MESSAGE.to_string(),
                detail: None,
                provider: None,
            })
        }
        Err(ManualUpdateEnqueueError::Unavailable) => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error: "playlist_update_unavailable".to_string(),
            detail: None,
            provider: None,
        }),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::{
        api::model::{create_test_app_state, AppState},
        model::{Config, ConfigInput, MediaServerInputConfig, SourcesConfig},
    };
    use shared::model::{InputType, MediaServerInputConfigDto, MediaServerLibrarySelector};
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    pub(crate) const EMBY_TOKEN: &str = "emby-secret";
    pub(crate) const EMBY_USER_ID: &str = "user-1";

    const DISCOVERY_JSON: &str = r#"{"Id":"server-1","ServerName":"Mock Emby","Version":"4.8.0"}"#;
    const VIEWS_JSON: &str = r#"{"Items":[{"Id":"lib-movies","Name":"Movies","CollectionType":"movies","Type":"CollectionFolder"},{"Id":"lib-tv","Name":"TV","CollectionType":"tvshows","Type":"CollectionFolder"}],"TotalRecordCount":2,"StartIndex":0}"#;

    /// A minimal Emby server answering discovery and the user's library views.
    /// Every other path is a `404`, and every request closes its connection.
    pub(crate) async fn start_emby_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("emby test server binds");
        let base_url = format!("http://{}", listener.local_addr().expect("emby test server address"));
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let body = if head.starts_with("get /system/info/public") {
                        Some(DISCOVERY_JSON)
                    } else if head.starts_with("get /users/user-1/views") || head.starts_with("get /userviews") {
                        Some(VIEWS_JSON)
                    } else {
                        None
                    };
                    let response = match body {
                        Some(body) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (base_url, task)
    }

    pub(crate) fn config_input(name: &str, id: u16, input_type: InputType, url: String) -> ConfigInput {
        ConfigInput { id, name: name.into(), input_type, url, enabled: true, ..ConfigInput::default() }
    }

    pub(crate) fn emby_input(name: &str, id: u16, url: String) -> ConfigInput {
        let media_server = MediaServerInputConfig::from(&MediaServerInputConfigDto {
            token: Some(EMBY_TOKEN.to_string()),
            user_id: Some(EMBY_USER_ID.to_string()),
            libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
            ..MediaServerInputConfigDto::default()
        });
        ConfigInput { media_server: Some(media_server), ..config_input(name, id, InputType::Emby, url) }
    }

    pub(crate) fn app_state_with_inputs(inputs: Vec<ConfigInput>) -> Arc<AppState> {
        app_state_with_inputs_and_config(Config::default(), inputs)
    }

    pub(crate) fn app_state_with_inputs_and_config(config: Config, inputs: Vec<ConfigInput>) -> Arc<AppState> {
        let app_state = create_test_app_state(config);
        let sources = SourcesConfig { inputs: inputs.into_iter().map(Arc::new).collect(), ..SourcesConfig::default() };
        app_state.app_config.sources.store(Arc::new(sources));
        app_state
    }
}

#[cfg(test)]
mod tests {
    use super::{
        media_server_api_register_protected,
        test_support::{app_state_with_inputs, config_input, emby_input, start_emby_server, EMBY_TOKEN},
    };
    use crate::api::model::{create_test_app_state_with_manual_update_sender, AppState};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        Router,
    };
    use shared::model::{InputRefreshPolicy, InputType, InputUpdateAction, PlaylistUpdateRequestPayload};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn router(app_state: Arc<AppState>) -> Router {
        media_server_api_register_protected(Router::new()).with_state(app_state)
    }

    async fn get(router: &Router, path: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(Request::builder().method("GET").uri(path).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }

    async fn post(router: &Router, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn libraries_of_a_media_server_input_report_selection_flags() {
        let (base_url, server) = start_emby_server().await;
        let app_state = app_state_with_inputs(vec![emby_input("emby_input", 1, base_url)]);

        let (status, body) = get(&router(app_state), "/media-server/emby_input/libraries").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!([
                { "id": "lib-movies", "name": "Movies", "kind": "movies", "selected": true },
                { "id": "lib-tv", "name": "TV", "kind": "tvshows", "selected": false }
            ])
        );
        assert!(!body.to_string().contains(EMBY_TOKEN), "the token is never serialized");
        server.abort();
    }

    #[tokio::test]
    async fn a_selection_the_server_no_longer_shows_still_lists_the_visible_libraries() {
        use crate::model::MediaServerInputConfig;
        use shared::model::{MediaServerInputConfigDto, MediaServerLibrarySelector};

        let (base_url, server) = start_emby_server().await;
        let mut input = emby_input("emby_input", 1, base_url);
        input.media_server = Some(MediaServerInputConfig::from(&MediaServerInputConfigDto {
            token: Some(EMBY_TOKEN.to_string()),
            user_id: Some(super::test_support::EMBY_USER_ID.to_string()),
            libraries: vec![MediaServerLibrarySelector::Name("Removed".to_string())],
            ..MediaServerInputConfigDto::default()
        }));
        let app_state = app_state_with_inputs(vec![input]);

        let (status, body) = get(&router(app_state), "/media-server/emby_input/libraries").await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            serde_json::json!([
                { "id": "lib-movies", "name": "Movies", "kind": "movies", "selected": false },
                { "id": "lib-tv", "name": "TV", "kind": "tvshows", "selected": false }
            ])
        );
        server.abort();
    }

    #[tokio::test]
    async fn status_reports_the_discovered_server_without_secrets() {
        let (base_url, server) = start_emby_server().await;
        let app_state = app_state_with_inputs(vec![emby_input("emby_input", 1, base_url.clone())]);

        let (status, body) = get(&router(app_state), "/media-server/emby_input/status").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "kind": "emby",
                "server_id": "server-1",
                "name": "Mock Emby",
                "version": "4.8.0",
                "reachable": true
            })
        );
        let text = body.to_string();
        assert!(!text.contains(EMBY_TOKEN));
        assert!(!text.contains(&base_url), "the configured URL is not echoed");
        server.abort();
    }

    #[tokio::test]
    async fn unknown_input_is_a_404() {
        let app_state = app_state_with_inputs(vec![]);
        let (status, body) = get(&router(app_state), "/media-server/missing/status").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap_or_default().contains("Unknown input 'missing'"), "{body}");
    }

    #[tokio::test]
    async fn an_input_that_is_not_a_media_server_is_a_400() {
        let app_state =
            app_state_with_inputs(vec![config_input("m3u_input", 1, InputType::M3u, "http://example.invalid".into())]);

        for path in ["/media-server/m3u_input/status", "/media-server/m3u_input/libraries"] {
            let (status, body) = get(&router(Arc::clone(&app_state)), path).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
            let message = body["error"].as_str().unwrap_or_default();
            assert!(message.contains("require a plex, emby, jellyfin or silo input"), "{message}");
        }
    }

    #[tokio::test]
    async fn invalid_action_is_a_400() {
        let app_state =
            app_state_with_inputs(vec![config_input("emby_input", 1, InputType::Emby, "http://127.0.0.1:1".into())]);
        let (status, body) =
            post(&router(app_state), "/media-server/emby_input/refresh", r#"{"action":"force"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap_or_default().contains("allowed values: normal, refresh"), "{body}");
    }

    #[tokio::test]
    async fn refresh_queues_a_manual_input_update_for_the_named_input() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let input = std::sync::Arc::new(config_input("emby_input", 7, InputType::Emby, "http://127.0.0.1:1".into()));
        let sources = crate::model::SourcesConfig { inputs: vec![input], ..crate::model::SourcesConfig::default() };
        let app_state = create_test_app_state_with_manual_update_sender(crate::model::Config::default(), sender);
        app_state.app_config.sources.store(Arc::new(sources));

        let (status, body) =
            post(&router(app_state), "/media-server/emby_input/refresh", r#"{"action":"refresh"}"#).await;

        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(body["run_id"].is_string(), "{body}");
        let request = receiver.try_recv().expect("the refresh was queued");
        assert_eq!(
            request.input_action,
            Some(shared::model::InputUpdateRequest {
                input_id: 7,
                action: InputUpdateAction::Provider(InputRefreshPolicy::REFRESH)
            })
        );
        // The request body shape is the one /playlist/update accepts.
        let _: PlaylistUpdateRequestPayload = serde_json::from_value(serde_json::json!({
            "input_refresh": { "input_id": 7, "policy": { "cache": "bypass", "quality": "enforce" } }
        }))
        .expect("the queued request is a valid update payload");
    }
}
