use crate::{
    api::{
        api_utils::{internal_server_error, json_or_bin_response, try_unwrap_body},
        endpoints::{
            download_api, extract_accept_header::ExtractAcceptHeader, library_api::library_api_register,
            rbac_api::rbac_api_register, recording_media_api::recording_media_api_register,
            user_api::user_api_register, v1_api_config::v1_api_config_register,
            v1_api_config::v1_api_config_register_with_permissions, v1_api_playlist::{
                v1_api_playlist_register_public,
                v1_api_playlist_register_protected,
                v1_api_playlist_register_with_permissions,
            },
            v1_api_user::{v1_api_user_register, v1_api_user_register_with_permissions},
        },
        model::AppState,
    },
    processing::geoip::{update_geoip_db, GeoIpUpdateError},
    utils::ip_checker::get_ips,
    VERSION,
};
use axum::response::IntoResponse;
use crate::auth::permission_layer;
use shared::{
    model::permission::Permission,
    model::{IpCheckDto, StatusCheck},
    utils::concat_path_leading_slash,
};
use std::{collections::BTreeMap, sync::Arc};
use crate::api::endpoints::rbac_api::rbac_api_register_unprotected;
use crate::api::endpoints::recording_api::recording_api_register;

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

    let system_write = axum::routing::Router::new()
        .route("/geoip/update", axum::routing::get(geoip_update));

    let download_read = axum::routing::Router::new()
        .route("/file/download/info", axum::routing::get(download_api::download_file_info));

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
            .merge(recording_api_register(axum::routing::Router::new()))
            .merge(recording_media_api_register(axum::routing::Router::new()));
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
            .merge(recording_api_register(axum::routing::Router::new()))
            .merge(recording_media_api_register(axum::routing::Router::new()));
    }

    let config = app_state.app_config.config.load();
    let mut base_router = axum::routing::Router::new();

    if config.web_ui.as_ref().is_none_or(|c| c.user_ui_enabled) {
        base_router = base_router.merge(user_api_register(app_state, web_ui_path));
    }

    let api_prefix = concat_path_leading_slash(web_ui_path, API_V1_PATH);
    base_router
        .nest(&api_prefix, public_router)
        .nest(&api_prefix, router)
}

#[cfg(test)]
mod tests {
    use super::create_status_check;
    use crate::{
        api::model::{create_test_app_state, ConnectionKind, ConnectionParams},
        auth::Fingerprint,
        model::Config,
    };
    use shared::{
        model::{PlaylistItemType, StreamChannel, XtreamCluster},
        utils::Internable,
    };
    use std::{borrow::Cow, net::SocketAddr};

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
        assert_eq!(
            clean
                .active_provider_connections
                .unwrap_or_default()
                .values()
                .sum::<usize>(),
            0
        );
    }
}
