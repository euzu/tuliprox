use crate::api::api_utils::{create_session_fingerprint, local_stream_response, try_unwrap_body};
use crate::api::api_utils::{
    force_provider_stream_response, get_user_target, get_user_target_by_credentials,
    is_seek_request, redirect, redirect_response, resource_response, separate_number_and_remainder,
    stream_response, try_result_not_found, try_option_bad_request, try_result_bad_request, RedirectParams,
};
use crate::api::endpoints::hls_api::handle_hls_stream_request;
use crate::api::endpoints::xtream_api::{ApiStreamContext, ApiStreamRequest};
use crate::api::model::AppState;
use crate::api::model::UserApiRequest;
use crate::api::model::{create_custom_video_stream_response, CustomVideoStreamType};
use crate::auth::Fingerprint;
use crate::repository::m3u_repository::{m3u_get_item_for_stream_id, m3u_load_rewrite_playlist};
use crate::repository::storage_const;
use crate::utils::debug_if_enabled;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream;
use log::{debug, error};
use shared::model::{FieldGetAccessor, PlaylistEntry, PlaylistItemType, TargetType, UserConnectionPermission, XtreamCluster};
use shared::utils::{concat_path, extract_extension_from_url, sanitize_sensitive_info, HLS_EXT};
use std::sync::Arc;

async fn m3u_api(api_req: &UserApiRequest, app_state: &AppState) -> impl IntoResponse + Send {
    match get_user_target(api_req, app_state) {
        Some((user, target)) => {
            match m3u_load_rewrite_playlist(&app_state.app_config, &target, &user).await {
                Ok(m3u_iter) => {
                    // Convert the iterator into a stream of `Bytes`
                    let content_stream = stream::iter(m3u_iter.map(|line| {
                        Ok::<Bytes, String>(Bytes::from(
                            [line.clone().as_bytes(), b"\n"].concat(),
                        ))
                    }));

                    let mut builder = axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(
                            axum::http::header::CONTENT_TYPE,
                            mime::TEXT_PLAIN_UTF_8.to_string(),
                        );
                    if api_req.content_type == "m3u_plus" {
                        builder = builder.header(
                            "Content-Disposition",
                            "attachment; filename=\"playlist.m3u\"",
                        );
                    }
                    try_unwrap_body!(builder.body(axum::body::Body::from_stream(content_stream)))
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

async fn m3u_api_get(
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    m3u_api(&api_req, &app_state).await
}

async fn m3u_api_post(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_req): axum::extract::Form<UserApiRequest>,
) -> impl IntoResponse + Send {
    m3u_api(&api_req, &app_state).await.into_response()
}

#[allow(clippy::too_many_lines)]
async fn m3u_api_stream(
    fingerprint: &Fingerprint,
    req_headers: &axum::http::HeaderMap,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    stream_req: ApiStreamRequest<'_>,
    // _addr: &std::net::SocketAddr,
) -> impl IntoResponse + Send {
    let (user, target) = try_option_bad_request!(
        get_user_target_by_credentials(
            stream_req.username,
            stream_req.password,
            api_req,
            app_state
        ),
        false,
        format!(
            "Could not find any user for m3u stream {}",
            stream_req.username
        )
    );

    let _guard =  app_state.app_config.file_locks.write_lock_str(&user.username).await;

    if user.permission_denied(app_state) {
        return create_custom_video_stream_response(
            app_state, &fingerprint.addr,
            CustomVideoStreamType::UserAccountExpired,
        ).await
        .into_response();
    }

    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let pli = try_result_not_found!(
        m3u_get_item_for_stream_id(req_virtual_id, app_state, &target).await,
        true,
        format!("Failed to read m3u item for stream id {req_virtual_id}")
    );
    let virtual_id = pli.virtual_id;

    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let input = try_option_bad_request!(
      app_state
      .app_config
      .get_input_by_name(&pli.input_name),
      true,
      format!("Can't find input {} for target {target_name}, stream_id {virtual_id}", pli.input_name)
    );

    if pli.item_type.is_local() {
        let connection_permission = user.connection_permission(app_state).await;
        return local_stream_response(
            fingerprint,
            app_state,
            pli.to_stream_channel(target.id),
            req_headers,
            &input,
            &target,
            &user,
            connection_permission,
            true,
        ).await.into_response();
    }

    let cluster = XtreamCluster::try_from(pli.item_type).unwrap_or(XtreamCluster::Live);
    
    debug_if_enabled!(
        "ID chain for m3u endpoint: request_stream_id={} -> action_stream_id={action_stream_id} -> req_virtual_id={req_virtual_id} -> virtual_id={virtual_id}",
        stream_req.stream_id);
    let session_key = create_session_fingerprint(fingerprint, &user.username, virtual_id);
    let user_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(
                app_state, &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            ).await
            .into_response();
        }

        if app_state
            .active_provider
            .is_over_limit(&session.provider)
            .await
        {
            return create_custom_video_stream_response(
                app_state, &fingerprint.addr,
                CustomVideoStreamType::ProviderConnectionsExhausted,
            ).await
            .into_response();
        }
        if session.virtual_id == virtual_id && is_seek_request(cluster, req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                pli.to_stream_channel(target.id),
                req_headers,
                &input,
                &user,
            )
            .await
            .into_response();
        }
        session.stream_url.clone()
    } else {
        pli.url.clone()
    };

    let connection_permission = user.connection_permission(app_state).await;
    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(
            app_state, &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        ).await
        .into_response();
    }

    let context = ApiStreamContext::try_from(cluster).unwrap_or(ApiStreamContext::Live);

    let redirect_params = RedirectParams {
        item: &pli,
        provider_id: pli.get_provider_id(),
        cluster,
        target_type: TargetType::M3u,
        target: &target,
        input: &input,
        user: &user,
        stream_ext: stream_ext.as_deref(),
        req_context: context,
        action_path: "", // TODO is there timeshift or something like that ?
    };

    if let Some(response) = redirect_response(app_state, &redirect_params).await {
        return response.into_response();
    }

    let extension = stream_ext.unwrap_or_else(|| {
        extract_extension_from_url(&pli.url)
            .map_or_else(String::new, std::string::ToString::to_string)
    });

    let is_hls_request = pli.item_type == PlaylistItemType::LiveHls
        || pli.item_type == PlaylistItemType::LiveDash
        || extension == HLS_EXT;
    // Reverse proxy mode
    if is_hls_request {
        return handle_hls_stream_request(
            fingerprint,
            app_state,
            &user,
            user_session.as_ref(),
            &pli.url,
            pli.virtual_id,
            &input,
            req_headers,
            connection_permission,
        )
        .await
        .into_response();
    }

    stream_response(
        fingerprint,
        app_state,
        &session_key,
        pli.to_stream_channel(target.id),
        &session_url,
        req_headers,
        &input,
        &target,
        &user,
        connection_permission,
    )
    .await
    .into_response()
}

async fn m3u_api_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, stream_id, resource)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Ok(m3u_stream_id) = stream_id.parse::<u32>() else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let Some((user, target)) =
        get_user_target_by_credentials(&username, &password, &api_req, &app_state)
    else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let m3u_item =
        match m3u_get_item_for_stream_id(m3u_stream_id, &app_state, &target).await {
            Ok(item) => item,
            Err(err) => {
                error!(
                    "Failed to get m3u url: {}",
                    sanitize_sensitive_info(err.to_string().as_str())
                );
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        };

    let stream_url = m3u_item.get_field(resource.as_str());
    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(m3u_item.item_type)
                || target.is_force_redirect(m3u_item.item_type)
            {
                debug!(
                    "Redirecting stream request to {}",
                    sanitize_sensitive_info(&url)
                );
                redirect(&url).into_response()
            } else {
                resource_response(&app_state, &url, &req_headers, None)
                    .await
                    .into_response()
            }
        }
    }
}

macro_rules! create_m3u_api_stream {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            fingerprint: Fingerprint,
            req_headers: axum::http::HeaderMap,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(
                String,
                String,
                String,
            )>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            // axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
        ) -> impl IntoResponse + Send {
            m3u_api_stream(
                &fingerprint,
                &req_headers,
                &app_state,
                &api_req,
                ApiStreamRequest::from($context, &username, &password, &stream_id, ""),
            )
            .await
            .into_response()
        }
    };
}

create_m3u_api_stream!(m3u_api_live_stream_alt, ApiStreamContext::LiveAlt);
create_m3u_api_stream!(m3u_api_live_stream, ApiStreamContext::Live);
create_m3u_api_stream!(m3u_api_series_stream, ApiStreamContext::Series);
create_m3u_api_stream!(m3u_api_movie_stream, ApiStreamContext::Movie);

macro_rules! register_m3u_api_stream {
     ($router:expr, [$(($path:expr, $fn_name:ident)),*]) => {{
         $router
       $(
        .route(&format!("/{}/{{username}}/{{password}}/{{stream_id}}", $path), axum::routing::get($fn_name))
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
    router = register_m3u_api_stream!(
        router,
        [
            (storage_const::M3U_STREAM_PATH, m3u_api_live_stream_alt),
            (
                concat_path(storage_const::M3U_STREAM_PATH, "live"),
                m3u_api_live_stream
            ),
            (
                concat_path(storage_const::M3U_STREAM_PATH, "movie"),
                m3u_api_movie_stream
            ),
            (
                concat_path(storage_const::M3U_STREAM_PATH, "series"),
                m3u_api_series_stream
            )
        ]
    );

    router.route(
        &format!(
            "/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}",
            storage_const::M3U_RESOURCE_PATH
        ),
        axum::routing::get(m3u_api_resource),
    )
}
