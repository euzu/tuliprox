use super::build_webplayer_recording_url;
use crate::{
    api::{
        api_utils::{create_api_proxy_user, try_option_bad_request, try_result_bad_request},
        endpoints::{
            m3u_api::m3u_api_stream_loaded,
            xtream_api::{xtream_player_api_stream_with_token, ApiStreamContext, ApiStreamRequest},
        },
        model::AppState,
    },
    auth::verify_access_token,
    repository::m3u_get_item_for_stream_id,
};
use axum::response::IntoResponse;
use log::{debug, error};
use shared::model::{TargetType, XtreamCluster};
use std::{str::FromStr, sync::Arc};

pub(super) fn build_playlist_webplayer_url(
    base_url: &str,
    access_token: &str,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> String {
    format!(
        "{base_url}/api/v1/playlist/webplayer/{access_token}/{}/{}/{}",
        target_id,
        cluster.as_stream_type(),
        virtual_id
    )
}

pub(super) fn playlist_webplayer(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> impl axum::response::IntoResponse + Send {
    let Some(url) = build_webplayer_recording_url(&app_state.app_config, target_id, virtual_id, cluster) else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    url.into_response()
}

pub(super) async fn playlist_webplayer_stream(
    fingerprint: crate::auth::Fingerprint,
    axum::extract::Path((token, target_id, cluster, stream_id)): axum::extract::Path<(String, u16, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: axum::http::HeaderMap,
) -> impl IntoResponse + Send {
    if !verify_access_token(&token, &app_state.app_config.access_token_secret, crate::auth::scope::INTERNAL_PLAYER) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let ctxt = try_result_bad_request!(ApiStreamContext::from_str(cluster.as_str()));
    let Some(target) = app_state.app_config.get_target_by_id(target_id) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    if target.has_output(TargetType::Xtream) {
        return xtream_player_api_stream_with_token(
            &fingerprint,
            &req_headers,
            &app_state,
            target_id,
            ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, ""),
        )
        .await
        .into_response();
    }

    if !target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let req_virtual_id: u32 = try_result_bad_request!(stream_id.trim().parse());
    let pli = try_result_bad_request!(
        m3u_get_item_for_stream_id(req_virtual_id, &app_state.app_config, &app_state.playlists, &target).await,
        true,
        format!("Failed to read m3u item for stream id {req_virtual_id}")
    );
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}", pli.input_name, target.name)
    );
    let user = Arc::new(create_api_proxy_user(&app_state));

    m3u_api_stream_loaded(user, target, &fingerprint, &req_headers, &app_state, pli, input, None, None, None)
        .await
        .into_response()
}
