use crate::{
    api::{
        api_utils::{internal_server_error, json_or_bin_response, try_unwrap_body},
        endpoints::{
            download_api,
            extract_accept_header::ExtractAcceptHeader,
            library_api::library_api_register,
            rbac_api::{rbac_api_register, rbac_api_register_unprotected},
            recording_api::{recording_api_register, recording_availability_register, recording_enabled_layer},
            recording_media_api::recording_media_api_register,
            user_api::user_api_register,
            v1_api_config::{v1_api_config_register, v1_api_config_register_with_permissions},
            v1_api_playlist::{
                v1_api_playlist_register_protected, v1_api_playlist_register_public,
                v1_api_playlist_register_with_permissions,
            },
            v1_api_user::{v1_api_user_register, v1_api_user_register_with_permissions},
        },
        model::AppState,
    },
    auth::permission_layer,
    processing::geoip::{update_geoip_db, GeoIpUpdateError},
    utils::ip_checker::get_ips,
    VERSION,
};
use axum::response::IntoResponse;
use shared::{
    model::{permission::Permission, IpCheckDto, StatusCheck},
    utils::concat_path_leading_slash,
};
use std::{collections::BTreeMap, sync::Arc};

pub const API_V1_PATH: &str = "api/v1";

async fn create_ipinfo_check(app_state: &Arc<AppState>) -> Option<(Option<String>, Option<String>)> {
    let config = app_state.app_config.config.load();
    if let Some(ipcheck) = config.ipcheck.as_ref() {
        if let Ok(check) = get_ips(&app_state.http_client.load(), ipcheck).await {
            return Some(check);
        }
    }
    None
}

pub async fn create_status_check(app_state: &Arc<AppState>) -> StatusCheck {
    let cache = match app_state.cache.load().as_ref().as_ref() {
        None => None,
        Some(lock) => Some(lock.read().await.get_size_text()),
    };
    let (active_users, active_user_connections, active_user_streams) = {
        let active_user = &app_state.active_users;
        let (user_count, connection_count) = active_user.active_users_and_connections().await;
        (user_count, connection_count, active_user.panel_streams().await)
    };

    let active_provider_connections =
        app_state.active_provider.active_connections().await.map(|c| c.into_iter().collect::<BTreeMap<_, _>>());

    StatusCheck {
        status: "ok".to_string(),
        version: VERSION.to_string(),
        build_time: crate::api::api_utils::get_build_time(),
        server_time: crate::api::api_utils::get_server_time(),
        uptime_secs: crate::api::api_utils::get_uptime_secs(),
        active_users,
        active_user_connections,
        active_provider_connections,
        active_user_streams,
        cache,
    }
}
async fn status(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> axum::response::Response {
    let status = create_status_check(&app_state).await;
    match serde_json::to_string_pretty(&status) {
        Ok(pretty_json) => try_unwrap_body!(axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
            .body(pretty_json)),
        Err(_) => axum::Json(status).into_response(),
    }
}

async fn streams(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    let streams = app_state.active_users.panel_streams().await;
    json_or_bin_response(accept.as_deref(), &streams).into_response()
}

async fn geoip_update(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    match update_geoip_db(&app_state).await {
        Ok(()) => axum::http::StatusCode::OK.into_response(),
        Err(GeoIpUpdateError::Disabled | GeoIpUpdateError::DownloadFailed(_)) => {
            axum::http::StatusCode::BAD_REQUEST.into_response()
        }
        Err(err) => {
            log::error!("GeoIp update failed: {err}");
            internal_server_error!()
        }
    }
}

async fn ipinfo(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> axum::response::Response {
    if let Some((ipv4, ipv6)) = create_ipinfo_check(&app_state).await {
        let ipcheck = IpCheckDto { ipv4, ipv6 };
        return match serde_json::to_string(&ipcheck) {
            Ok(json) => try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(json)),
            Err(_) => axum::Json(ipcheck).into_response(),
        };
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

pub fn v1_api_register(
    web_auth_enabled: bool,
    app_state: &Arc<AppState>,
    web_ui_path: &str,
) -> axum::Router<Arc<AppState>> {
    let public_router = v1_api_playlist_register_public(axum::Router::new());

    let system_read = axum::routing::Router::new()
        .route("/status", axum::routing::get(status))
        .route("/streams", axum::routing::get(streams))
        .route("/ipinfo", axum::routing::get(ipinfo))
        .route("/stream-history", axum::routing::get(super::stream_history_api::stream_history_page_query))
        .route("/stream-history/summary", axum::routing::get(super::stream_history_api::stream_history_summary_query))
        .route("/qos-snapshots", axum::routing::get(super::stream_history_api::qos_snapshot_query))
        .route(
            "/qos-snapshots/{stream_identity_key}",
            axum::routing::get(super::stream_history_api::qos_snapshot_detail_query),
        );

    let system_write = axum::routing::Router::new().route("/geoip/update", axum::routing::get(geoip_update));

    let download_read =
        axum::routing::Router::new().route("/file/download/info", axum::routing::get(download_api::download_file_info));

    let download_write = axum::routing::Router::new()
        .route("/file/download", axum::routing::post(download_api::queue_download_file))
        .route("/file/record", axum::routing::post(download_api::queue_recording_file))
        .route("/file/download/pause", axum::routing::post(download_api::pause_download))
        .route("/file/download/resume", axum::routing::post(download_api::resume_download))
        .route("/file/download/cancel", axum::routing::post(download_api::cancel_download))
        .route("/file/download/remove", axum::routing::post(download_api::remove_download))
        .route("/file/download/retry", axum::routing::post(download_api::retry_download));

    let mut router = axum::routing::Router::new();

    if web_auth_enabled {
        router = router
            .merge(system_read.layer(permission_layer!(app_state, Permission::SystemRead)))
            .merge(system_write.layer(permission_layer!(app_state, Permission::SystemWrite)))
            .merge(download_read.layer(permission_layer!(app_state, Permission::DownloadRead)))
            .merge(download_write.layer(permission_layer!(app_state, Permission::DownloadWrite)))
            .merge(v1_api_config_register_with_permissions(app_state))
            .merge(v1_api_user_register_with_permissions(axum::routing::Router::new(), app_state))
            .merge(v1_api_playlist_register_with_permissions(axum::routing::Router::new(), app_state))
            .merge(library_api_register(axum::routing::Router::new(), Some(app_state)))
            .merge(rbac_api_register(Arc::clone(app_state)))
            .merge(recording_availability_register(axum::routing::Router::new()))
            // `recording.enabled: false` turns the DVR off end to end:
            // the supervisors idle and the routes answer
            // `501 recording_disabled` instead of queueing work nothing
            // will run.
            .merge(
                recording_api_register(axum::routing::Router::new())
                    .merge(recording_media_api_register(axum::routing::Router::new()))
                    .layer(recording_enabled_layer!(app_state)),
            );
    } else {
        router = router
            .merge(system_read)
            .merge(system_write)
            .merge(download_read)
            .merge(download_write)
            .merge(v1_api_config_register(axum::routing::Router::new()))
            .merge(v1_api_user_register(axum::routing::Router::new()))
            .merge(v1_api_playlist_register_protected(axum::routing::Router::new()))
            .merge(library_api_register(axum::routing::Router::new(), None))
            .merge(rbac_api_register_unprotected(Arc::clone(app_state)))
            .merge(recording_availability_register(axum::routing::Router::new()))
            .merge(
                recording_api_register(axum::routing::Router::new())
                    .merge(recording_media_api_register(axum::routing::Router::new()))
                    .layer(recording_enabled_layer!(app_state)),
            );
    }

    let config = app_state.app_config.config.load();
    let mut base_router = axum::routing::Router::new();

    if config.web_ui.as_ref().is_none_or(|c| c.user_ui_enabled) {
        base_router = base_router.merge(user_api_register(app_state, web_ui_path));
    }

    let api_prefix = concat_path_leading_slash(web_ui_path, API_V1_PATH);
    base_router.nest(&api_prefix, public_router).nest(&api_prefix, router)
}

#[cfg(test)]
mod tests {
    use super::{create_status_check, v1_api_register};
    use crate::{
        api::model::{create_test_app_state, ConnectionKind, ConnectionParams},
        auth::{create_jwt_web_user, Fingerprint},
        model::Config,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use shared::{
        model::{Permission, PlaylistItemType, StreamChannel, WebAuthConfigDto, WebUiConfigDto, XtreamCluster},
        utils::Internable,
    };
    use std::{borrow::Cow, net::SocketAddr};
    use tower::ServiceExt;

    /// A config whose DVR block is present and explicitly on or off.
    /// Built through the DTO so the `enabled` flag travels the same
    /// deserialize → domain path it does in production.
    fn config_with_recording_enabled(enabled: bool) -> Config {
        let recording = shared::model::RecordingConfigDto { enabled, ..Default::default() };
        let download = shared::model::VideoDownloadConfigDto { recording: Some(recording), ..Default::default() };
        let video = shared::model::VideoConfigDto { download: Some(download), ..Default::default() };
        Config { video: Some((&video).into()), ..Config::default() }
    }

    fn config_with_recording_and_web_auth(recording_enabled: bool) -> Config {
        let mut config = config_with_recording_enabled(recording_enabled);
        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: "test-secret".to_string(),
                ..WebAuthConfigDto::default()
            }),
            ..WebUiConfigDto::default()
        };
        config.web_ui = Some((&web_ui).into());
        config
    }

    #[tokio::test]
    async fn recording_routes_answer_not_implemented_when_the_dvr_is_disabled() {
        // `recording.enabled: false` has to be visible at the API edge,
        // not just in the schedulers: a client that keeps calling would
        // otherwise queue recordings nothing will ever run. 501 also
        // distinguishes "switched off here" from 403 and 404.
        let app_state = create_test_app_state(config_with_recording_enabled(false));
        let router = v1_api_register(false, &app_state, "").with_state(app_state);

        let response = router
            .oneshot(
                Request::builder().method("GET").uri("/api/v1/recording/tasks").body(Body::empty()).expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn recording_routes_are_reachable_when_the_dvr_is_enabled() {
        // The mirror of the test above: the gate must not be a blanket
        // block. An explicitly enabled DVR reaches the handler, which
        // then rejects the unauthenticated call on its own terms —
        // anything other than 501 proves the layer let the request past.
        let app_state = create_test_app_state(config_with_recording_enabled(true));
        let router = v1_api_register(false, &app_state, "").with_state(app_state);

        let response = router
            .oneshot(
                Request::builder().method("GET").uri("/api/v1/recording/tasks").body(Body::empty()).expect("request"),
            )
            .await
            .expect("response");

        assert_ne!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn no_auth_router_exposes_recording_routes() {
        let app_state = create_test_app_state(config_with_recording_enabled(true));
        let router = v1_api_register(false, &app_state, "").with_state(app_state);

        for path in ["/api/v1/recording/tasks", "/api/v1/library/recording/playback/missing"] {
            let response = router
                .clone()
                .oneshot(Request::builder().method("OPTIONS").uri(path).body(Body::empty()).expect("request"))
                .await
                .expect("response");

            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "missing route: {path}");
        }
    }

    #[tokio::test]
    async fn recording_availability_authenticates_before_reporting_disabled() -> Result<(), Box<dyn std::error::Error>>
    {
        let app_state = create_test_app_state(Config::default());
        let router = v1_api_register(true, &app_state, "").with_state(app_state);

        let response = router
            .oneshot(Request::builder().method("GET").uri("/api/v1/recording/availability").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn recording_availability_reports_disabled_after_authenticated_recording_claim(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with_recording_and_web_auth(false);
        let Some(web_auth) = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()) else {
            return Err("missing test web auth".into());
        };
        let token = create_jwt_web_user(web_auth, "alice", Permission::RecordingRead.into(), 0)?;
        let app_state = create_test_app_state(config);
        let router = v1_api_register(true, &app_state, "").with_state(app_state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/recording/availability")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), br#"{"error":"recording_disabled"}"#);
        Ok(())
    }

    #[tokio::test]
    async fn recording_availability_reports_disabled_for_no_auth_token() -> Result<(), Box<dyn std::error::Error>> {
        let app_state = create_test_app_state(config_with_recording_enabled(false));
        let router = v1_api_register(false, &app_state, "").with_state(app_state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/recording/availability")
                    .header("Authorization", format!("Bearer {}", shared::model::TOKEN_NO_AUTH))
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), br#"{"error":"recording_disabled"}"#);
        Ok(())
    }

    #[tokio::test]
    async fn status_snapshot_removes_released_direct_series_stream() {
        let app_state = create_test_app_state(Config::default());
        let addr: SocketAddr = "127.0.0.1:55070".parse().expect("test address");
        let fingerprint = Fingerprint::new("status-series".to_string(), "127.0.0.1".to_string(), addr);
        let channel = StreamChannel {
            target_id: 1,
            virtual_id: 70,
            provider_id: 1,
            input_name: "input".intern(),
            item_type: PlaylistItemType::Series,
            cluster: XtreamCluster::Series,
            group: "Series".intern(),
            title: "Episode".intern(),
            url: "http://provider.example/series/70.mkv".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        };
        app_state.connection_manager.add_connection(&addr).await;
        let registered = app_state
            .connection_manager
            .update_connection(ConnectionParams {
                meter_uid: 0,
                username: "status-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider".intern(),
                stream_channel: &channel,
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("direct Series stream should register");

        let active = create_status_check(&app_state).await;
        assert_eq!(active.active_users, 1);
        assert_eq!(active.active_user_connections, 1);
        assert_eq!(active.active_user_streams.len(), 1);
        assert_eq!(active.active_user_streams[0].uid, registered.uid);

        app_state
            .active_users
            .release_stream_by_uid(&addr, registered.uid)
            .await
            .expect("registered stream should release");
        let clean = create_status_check(&app_state).await;
        assert_eq!(clean.active_users, 0);
        assert_eq!(clean.active_user_connections, 0);
        assert!(clean.active_user_streams.is_empty());
        assert_eq!(clean.active_provider_connections.unwrap_or_default().values().sum::<usize>(), 0);
    }
}
