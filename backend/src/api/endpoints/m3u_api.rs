use crate::{
    api::{
        api_utils::{
            create_catchup_session_key, create_session_fingerprint, force_provider_stream_response,
            get_session_reservation_ttl_secs, get_user_target, get_user_target_by_credentials, is_seek_request,
            is_stream_share_enabled, local_stream_response, redirect, redirect_response, resource_response,
            admission_failure_response, separate_number_and_remainder, should_allow_exhausted_shared_reconnect, stream_response,
            try_option_bad_request, try_option_forbidden, try_result_bad_request, try_result_not_found,
            try_unwrap_body, RedirectParams,
        },
        endpoints::{
            hls_api::handle_hls_stream_request,
            xtream_api::{ApiStreamContext, ApiStreamRequest},
        },
        model::{AppState, UserApiRequestQueryOrBody, UserApiRequest},
    },
    auth::Fingerprint,
    repository::{m3u_get_item_for_stream_id, m3u_load_rewrite_playlist, storage_const},
    utils::debug_if_enabled,
};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::StreamExt;
use log::{debug, error};
use shared::{
    model::{FieldGetAccessor, PlaylistEntry, PlaylistItemType, TargetType, UserConnectionPermission, XtreamCluster},
    utils::{concat_path, extract_extension_from_url, sanitize_sensitive_info, HLS_EXT},
};
use std::sync::Arc;

async fn m3u_api(api_req: &UserApiRequest, app_state: &AppState) -> impl IntoResponse + Send {
    api_req.log_sanitized("m3u_api");
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target(api_req, app_state),
        auth_status,
        false,
        format!("Could not find any user for m3u api {}", api_req.username)
    );

    match m3u_load_rewrite_playlist(&app_state.app_config, &target, &user).await {
        Ok(m3u_iter) => {
            // Convert the stream into a stream of `Bytes`
            let content_stream = m3u_iter.map(|mut line| {
                line.push('\n');
                Ok::<Bytes, String>(Bytes::from(line))
            });

            let mut builder = axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::TEXT_PLAIN_UTF_8.to_string());
            if api_req.content_type == "m3u_plus" {
                builder = builder.header("Content-Disposition", "attachment; filename=\"playlist.m3u\"");
            }
            try_unwrap_body!(builder.body(axum::body::Body::from_stream(content_stream)))
        }
        Err(err) => {
            error!("{}", sanitize_sensitive_info(err.to_string().as_str()));
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
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
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
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
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target_by_credentials(stream_req.username, stream_req.password, api_req, app_state),
        auth_status,
        false,
        format!("Could not find any user for m3u stream {}", stream_req.username)
    );

    let _guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

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
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}", pli.input_name)
    );

    if user.permission_denied(app_state) {
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            pli.to_stream_channel(target.id),
            pli.input_name.as_ref(),
            req_headers,
            crate::repository::ConnectFailureReason::UserAccountExpired,
        );
    }

    if pli.item_type.is_local() {
        let admission = app_state
            .get_connection_admission(&user.username, user.max_connections, user.soft_connections)
            .await;
        let playback_session_token = create_session_fingerprint(fingerprint, &user.username, virtual_id);
        return local_stream_response(
            fingerprint,
            app_state,
            pli.to_stream_channel(target.id),
            req_headers,
            &input,
            &target,
            &user,
            admission.permission,
            admission.kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            Some(playback_session_token.as_str()),
            true,
        )
        .await
        .into_response();
    }

    let cluster = XtreamCluster::try_from(pli.item_type).unwrap_or(XtreamCluster::Live);

    debug_if_enabled!(
        "ID chain for m3u endpoint: request_stream_id={} -> action_stream_id={action_stream_id} -> req_virtual_id={req_virtual_id} -> virtual_id={virtual_id}",
        stream_req.stream_id);
    let session_key = if pli.item_type == PlaylistItemType::Catchup {
        create_catchup_session_key(fingerprint, &user.username, virtual_id)
    } else {
        create_session_fingerprint(fingerprint, &user.username, virtual_id)
    };
    let user_session = app_state.active_users.get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                &session.provider,
                req_headers,
                crate::repository::ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                &session.provider,
                req_headers,
                crate::repository::ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }
        if session.virtual_id == virtual_id && is_seek_request(cluster, req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                pli.to_stream_channel(target.id),
                crate::api::api_utils::ForceStreamRequestContext {
                    req_headers,
                    input: &input,
                    user: &user,
                    session_reservation_ttl_secs: get_session_reservation_ttl_secs(app_state, pli.item_type),
                },
            )
            .await
            .into_response();
        }
        session.stream_url.clone()
    } else {
        pli.url.clone()
    };

    let connection_admission = if (user.max_connections > 0 || user.soft_connections > 0)
        && app_state.app_config.config.load().user_access_control
    {
        app_state
            .get_connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                &session_key,
            )
            .await
    } else {
        crate::api::model::ConnectionAdmission {
            permission: UserConnectionPermission::Allowed,
            kind: user_session.as_ref().and_then(|session| session.connection_kind),
        }
    };
    let connection_permission = connection_admission.permission;
    let connection_kind = connection_admission
        .kind
        .or(user_session.as_ref().and_then(|session| session.connection_kind))
        .unwrap_or(crate::api::model::ConnectionKind::Normal);
    let allow_exhausted_shared_reconnect = should_allow_exhausted_shared_reconnect(
        is_stream_share_enabled(pli.item_type, &target),
        user_session.as_ref(),
        virtual_id,
        session_url.as_ref(),
    );
    if connection_permission == UserConnectionPermission::Exhausted && !allow_exhausted_shared_reconnect {
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            pli.to_stream_channel(target.id),
            input.name.as_ref(),
            req_headers,
            crate::repository::ConnectFailureReason::UserConnectionsExhausted,
        );
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

    let extension = stream_ext.unwrap_or_else(|| extract_extension_from_url(&pli.url).unwrap_or_default());

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
            connection_kind,
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
        connection_kind,
        allow_exhausted_shared_reconnect,
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
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target_by_credentials(&username, &password, &api_req, &app_state),
        auth_status,
        false,
        format!("Could not find any user for m3u resource {username}")
    );
    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let m3u_item = match m3u_get_item_for_stream_id(m3u_stream_id, &app_state, &target).await {
        Ok(item) => item,
        Err(err) => {
            error!("Failed to get m3u url: {}", sanitize_sensitive_info(err.to_string().as_str()));
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    };

    let stream_url = m3u_item.get_field(resource.as_str());
    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(m3u_item.item_type) || target.is_force_redirect(m3u_item.item_type) {
                debug!("Redirecting stream request to {}", sanitize_sensitive_info(&url));
                redirect(&url).into_response()
            } else {
                resource_response(&app_state, &url, &req_headers, None).await.into_response()
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
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(String, String, String)>,
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
            .route(&format!("/{}", $path), axum::routing::get(m3u_api_get).post(m3u_api_post))
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
            (concat_path(storage_const::M3U_STREAM_PATH, "live"), m3u_api_live_stream),
            (concat_path(storage_const::M3U_STREAM_PATH, "movie"), m3u_api_movie_stream),
            (concat_path(storage_const::M3U_STREAM_PATH, "series"), m3u_api_series_stream)
        ]
    );

    router.route(
        &format!("/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}", storage_const::M3U_RESOURCE_PATH),
        axum::routing::get(m3u_api_resource),
    )
}

#[cfg(test)]
mod tests {
    use crate::api::model::UserApiRequest;

    #[test]
    fn post_query_only_request_prefers_query_when_form_is_missing() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            content_type: String::from("m3u_plus"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.password, "query-pass");
        assert_eq!(api_req.content_type, "m3u_plus");
    }

    #[test]
    fn post_request_prefers_query_over_form() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            content_type: String::from("query-type"),
            ..UserApiRequest::default()
        };
        let form_req = UserApiRequest {
            username: String::from("form-user"),
            content_type: String::from("form-type"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.content_type, "query-type");
    }
}
