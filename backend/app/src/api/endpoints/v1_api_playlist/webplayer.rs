use super::build_webplayer_recording_url;
use crate::{
    api::{
        api_utils::{
            create_api_proxy_user,
            failover_group::{handle_stream_failure_for_group, resolve_playback_source},
            separate_number_and_remainder, try_option_bad_request, try_result_bad_request,
        },
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
use shared::model::{PlaylistItemType, TargetType, VirtualId, XtreamCluster};
use std::{str::FromStr, sync::Arc};
use tuliprox_repository::target_stream_item::{classify_virtual_id, member_to_m3u_item, ResolvedStreamTarget};

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

    let (action_stream_id, stream_ext) = separate_number_and_remainder(&stream_id);
    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let resolved_target =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, VirtualId::new(req_virtual_id)).await;
    let pli =
        match m3u_get_item_for_stream_id(req_virtual_id, &app_state.app_config, &app_state.playlists, &target).await {
            Ok(pli) => pli,
            Err(err) => match &resolved_target {
                ResolvedStreamTarget::Group(group) => member_to_m3u_item(group, 0),
                ResolvedStreamTarget::Member { group, index } => member_to_m3u_item(group, *index),
                ResolvedStreamTarget::Item => {
                    debug!("Failed to read m3u item for stream id {req_virtual_id}: {err}");
                    return axum::http::StatusCode::BAD_REQUEST.into_response();
                }
            },
        };
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}", pli.input_name, target.name)
    );
    let is_live = ctxt != ApiStreamContext::Timeshift && pli.item_type != PlaylistItemType::Catchup;
    let source = resolve_playback_source(&resolved_target, is_live, &input, &pli.t_stream_url, None);
    let user = Arc::new(create_api_proxy_user(&app_state));

    let outcome = m3u_api_stream_loaded(
        user.clone(),
        target,
        &fingerprint,
        &req_headers,
        &app_state,
        pli.clone(),
        input.clone(),
        stream_ext,
        None,
        None,
        Some(source),
        Some(resolved_target.clone()),
    )
    .await;

    if !outcome.is_success() {
        let session_vid = resolved_target.group().map_or(pli.virtual_id.get(), |g| g.group_vid.get());
        let session_key = crate::api::endpoints::m3u_api::m3u_stream_session_key(
            &fingerprint,
            &user.username,
            session_vid,
            &pli,
            stream_ext,
            None,
        );
        handle_stream_failure_for_group(
            &app_state,
            &user.username,
            &session_key,
            outcome.failure_class,
            resolved_target.group().map(|g| &**g),
            pli.virtual_id,
        )
        .await;
    }

    outcome.into_response()
}
