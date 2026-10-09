use super::{
    playlist_content_live, playlist_content_series, playlist_content_vod, playlist_epg, playlist_episode_item,
    playlist_filter_preview, playlist_recording_stream, playlist_resolve_url, playlist_resource, playlist_series_info,
    playlist_update, playlist_update_status, playlist_webplayer_stream,
};
use crate::api::{
    auth_middleware::permission_layer,
    endpoints::{epg_grid_api, xmltv_api::stream_epg_api},
    model::AppState,
};
use axum::Router;
use shared::model::permission::Permission;
use std::sync::Arc;

pub fn v1_api_playlist_register_protected(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist/update/status", axum::routing::get(playlist_update_status))
        .route("/playlist/epg", axum::routing::post(playlist_epg))
        .route("/playlist/epg/stream", axum::routing::post(stream_epg_api))
        .route("/playlist/epg/groups", axum::routing::post(epg_grid_api::playlist_epg_groups))
        .route("/playlist/epg/grid", axum::routing::post(epg_grid_api::playlist_epg_grid))
        .route("/playlist/live", axum::routing::post(playlist_content_live))
        .route("/playlist/vod", axum::routing::post(playlist_content_vod))
        .route("/playlist/series", axum::routing::post(playlist_content_series))
        .route("/playlist/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/playlist/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
        .route("/playlist/filter/preview", axum::routing::post(playlist_filter_preview))
}

pub fn v1_api_playlist_register_public(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/resource/{resource}", axum::routing::get(playlist_resource))
        .route(
            "/playlist/webplayer/{token}/{target_id}/{cluster}/{stream_id}",
            axum::routing::get(playlist_webplayer_stream),
        )
        .route("/playlist/recording/{token}/{cluster}/{virtual_id}", axum::routing::get(playlist_recording_stream))
}

pub fn v1_api_playlist_register_with_permissions(
    router: Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
) -> axum::Router<Arc<AppState>> {
    let read_routes = Router::new()
        .route("/update/status", axum::routing::get(playlist_update_status))
        .route("/live", axum::routing::post(playlist_content_live))
        .route("/vod", axum::routing::post(playlist_content_vod))
        .route("/series", axum::routing::post(playlist_content_series))
        .route("/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
        .route("/filter/preview", axum::routing::post(playlist_filter_preview))
        .layer(permission_layer!(app_state, Permission::PlaylistRead));

    let write_routes = Router::new()
        .route("/update", axum::routing::post(playlist_update))
        .layer(permission_layer!(app_state, Permission::PlaylistWrite));

    let epg_routes = Router::new()
        .route("/epg", axum::routing::post(playlist_epg))
        .route("/epg/stream", axum::routing::post(stream_epg_api))
        .route("/epg/groups", axum::routing::post(epg_grid_api::playlist_epg_groups))
        .route("/epg/grid", axum::routing::post(epg_grid_api::playlist_epg_grid))
        .layer(permission_layer!(app_state, Permission::EpgRead));

    router.nest("/playlist", read_routes.merge(write_routes).merge(epg_routes))
}
