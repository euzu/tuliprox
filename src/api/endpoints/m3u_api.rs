use crate::api::api_utils::{get_user_target, get_user_target_by_credentials, is_seek_request, redirect, redirect_response, resource_response, force_provider_stream_response, separate_number_and_remainder, stream_response, try_option_bad_request, try_result_bad_request, RedirectParams, read_session_token};
use crate::api::endpoints::hls_api::handle_hls_stream_request;
use crate::api::model::app_state::AppState;
use crate::api::model::request::UserApiRequest;
use crate::model::{UserConnectionPermission};
use crate::model::{TargetType};
use crate::model::{FieldGetAccessor, PlaylistEntry, PlaylistItemType, XtreamCluster};
use crate::repository::m3u_repository::{m3u_get_item_for_stream_id, m3u_load_rewrite_playlist};
use crate::utils::request::{extract_extension_from_url, sanitize_sensitive_info};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream;
use log::{debug, error};
use std::sync::Arc;
use axum::http::StatusCode;
use crate::api::endpoints::xtream_api::XtreamApiStreamContext;
use crate::api::model::streams::provider_stream::{create_custom_video_stream_response, CustomVideoStreamType};
use crate::repository::storage_const;
use crate::utils::{HLS_EXT};

async fn m3u_api(
    api_req: &UserApiRequest,
    app_state: &AppState,
) -> impl axum::response::IntoResponse + Send {
    match get_user_target(api_req, app_state) {
        Some((user, target)) => {
            match m3u_load_rewrite_playlist(&app_state.config, target, &user).await {
                Ok(m3u_iter) => {
                    // Convert the iterator into a stream of `Bytes`
                    let content_stream = stream::iter(m3u_iter.map(|line| Ok::<Bytes, String>(Bytes::from([line.to_string().as_bytes(), b"\n"].concat()))));

                    let mut builder = axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, mime::TEXT_PLAIN_UTF_8.to_string());
                    if api_req.content_type == "m3u_plus" {
                        builder = builder.header("Content-Disposition", "attachment; filename=\"playlist.m3u\"");
                    }
                    builder.body(axum::body::Body::from_stream(content_stream)).unwrap().into_response()
                }
                Err(err) => {
                    error!("{}", sanitize_sensitive_info(err.to_string().as_str()));
                    axum::http::StatusCode::NO_CONTENT.into_response()
                }
            }
        }
        None => axum::http::StatusCode::BAD_REQUEST.into_response(),
    }
}


async fn m3u_api_get(axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
                     axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    m3u_api(&api_req, &app_state).await
}

async fn m3u_api_post(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_req): axum::extract::Form<UserApiRequest>,
) -> impl axum::response::IntoResponse + Send {
    m3u_api(&api_req, &app_state).await.into_response()
}

async fn m3u_api_stream(
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, stream_id)): axum::extract::Path<(String, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let (user, target) = try_option_bad_request!(get_user_target_by_credentials(&username, &password, &api_req, &app_state), false, format!("Could not find any user {username}"));
    if user.permission_denied(&app_state) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let target_name = &target.name;
    if !target.has_output(&TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(&stream_id);
    let virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let pli = try_result_bad_request!(m3u_get_item_for_stream_id(virtual_id, &app_state.config, target).await, true, format!("Failed to read m3u item for stream id {}", virtual_id));
    let input = try_option_bad_request!(app_state.config.get_input_by_name(pli.input_name.as_str()), true, format!("Cant find input for target {target_name}, stream_id {virtual_id}"));

    let cluster = XtreamCluster::try_from(pli.item_type).unwrap_or(XtreamCluster::Live);

    let user_session = match read_session_token(&req_headers) {
        None => None,
        Some(token) => app_state.active_users.get_user_session(&user.username, token).await,
    };

    if let Some(session)  = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(&app_state.config, &CustomVideoStreamType::UserConnectionsExhausted).into_response();
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            return create_custom_video_stream_response(&app_state.config, &CustomVideoStreamType::ProviderConnectionsExhausted).into_response();
        }
        if session.virtual_id == virtual_id  && is_seek_request(cluster, &req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(&app_state, session, pli.item_type, &req_headers, input, &user).await.into_response()
        }
    }

    let connection_permission = user.connection_permission(&app_state).await;
    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(&app_state.config, &CustomVideoStreamType::UserConnectionsExhausted).into_response();
    }

    let context = XtreamApiStreamContext::try_from(cluster).unwrap_or(XtreamApiStreamContext::Live);

    let redirect_params = RedirectParams {
        item: &pli,
        provider_id: pli.get_provider_id(),
        cluster,
        target_type: TargetType::Xtream,
        target,
        input,
        user: &user,
        stream_ext: stream_ext.as_deref(),
        req_context: context,
        action_path: "" // TODO is there timeshoft or something like that ?
    };

    if let Some(response) = redirect_response(&app_state, &redirect_params).await {
        return response.into_response();
    }

    let extension = stream_ext.unwrap_or_else(
        || extract_extension_from_url(&pli.url).map_or_else(String::new, std::string::ToString::to_string));

    let is_hls_request = pli.item_type == PlaylistItemType::LiveHls || pli.item_type == PlaylistItemType::LiveDash || extension == HLS_EXT;
    // Reverse proxy mode
    if is_hls_request {
        return handle_hls_stream_request(&app_state, &user, user_session.as_ref(), &pli.url, pli.virtual_id, input, connection_permission).await.into_response();
    }

    stream_response(&app_state, pli.virtual_id, pli.item_type, pli.url.as_str(), &req_headers, input, target, &user, connection_permission).await.into_response()
}

async fn m3u_api_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, stream_id, resource)): axum::extract::Path<(String, String, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let Ok(m3u_stream_id) = stream_id.parse::<u32>() else { return axum::http::StatusCode::BAD_REQUEST.into_response() };
    let Some((user, target)) = get_user_target_by_credentials(&username, &password, &api_req, &app_state)
    else { return StatusCode::BAD_REQUEST.into_response() };
    if user.permission_denied(&app_state) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let target_name = &target.name;
    if !target.has_output(&TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return StatusCode::BAD_REQUEST.into_response();
    }
    let m3u_item = match m3u_get_item_for_stream_id(m3u_stream_id, &app_state.config, target).await {
        Ok(item) => item,
        Err(err) => {
            error!("Failed to get m3u url: {}", sanitize_sensitive_info(err.to_string().as_str()));
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };

    let stream_url = m3u_item.get_field(resource.as_str());
    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(m3u_item.item_type)  || target.is_force_redirect(m3u_item.item_type) {
                debug!("Redirecting stream request to {}", sanitize_sensitive_info(&url));
                redirect(&url).into_response()
            } else {
                resource_response(&app_state, &url, &req_headers, None).await.into_response()
            }
        }
    }
}

macro_rules! register_m3u_stream_routes {
    ($router:expr, [$($path:expr),*]) => {{
        $router
        $(
        .route(&format!("/{}/{}/{{username}}/{{password}}/{{stream_id}}", storage_const::M3U_STREAM_PATH, $path), axum::routing::get(m3u_api_stream))
            // $cfg.service(web::resource(format!("/{M3U_STREAM_PATH}/{}/{{username}}/{{password}}/{{stream_id}}", $path)).route(web::get().to(m3u_api_stream)));
        )*
    }};
}

macro_rules! register_m3u_api_routes {
    ($router:expr, [$($path:expr),*]) => {{
        $router
        $(
            .route(&format!("/{}", $path), axum::routing::get(m3u_api_get))
            .route(&format!("/{}", $path), axum::routing::post(m3u_api_post))
            // $cfg.service(web::resource(format!("/{}", $path)).route(web::get().to(m3u_api_get)).route(web::post().to(m3u_api_post)));
        )*
    }};
}

pub fn m3u_api_register() -> axum::Router<Arc<AppState>> {
    let mut router = axum::Router::new();
    router = register_m3u_api_routes!(router, ["get.php", "apiget", "m3u"]);
    register_m3u_stream_routes!(router, ["live", "movie", "series"])
    .route(&format!("/{}/{{username}}/{{password}}/{{stream_id}}", storage_const::M3U_STREAM_PATH), axum::routing::get(m3u_api_stream))
    .route(&format!("/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}", storage_const::M3U_RESOURCE_PATH), axum::routing::get(m3u_api_resource))
}