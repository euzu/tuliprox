use crate::api::api_utils::{force_provider_stream_response, get_stream_alternative_url, is_seek_request};
use crate::api::api_utils::{try_option_bad_request};
use crate::api::model::app_state::AppState;
use crate::api::model::streams::provider_stream::{create_custom_video_stream_response, CustomVideoStreamType};
use crate::model::{ProxyUserCredentials};
use crate::model::ConfigInput;
use shared::model::{PlaylistItemType, UserConnectionPermission, XtreamCluster};
use crate::processing::parser::hls::{get_hls_session_token_and_url_from_token, rewrite_hls, RewriteHlsProps};
use shared::utils::{is_hls_url, replace_url_extension, sanitize_sensitive_info, HLS_EXT};
use crate::utils::request;
use axum::response::IntoResponse;
use log::{debug, error};
use serde::Deserialize;
use std::sync::Arc;
use crate::api::model::active_user_manager::UserSession;
use crate::auth::Fingerprint;

#[derive(Debug, Deserialize)]
struct HlsApiPathParams {
    username: String,
    password: String,
    input_id: u16,
    stream_id: u32,
    token: String,
}

fn hls_response(hls_content: String) -> impl IntoResponse + Send {
    let builder = axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-mpegurl");
    builder.body(hls_content)
        .unwrap()
        .into_response()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api) async fn handle_hls_stream_request(
        fingerprint: &str,
        app_state: &Arc<AppState>,
        user: &ProxyUserCredentials,
        user_session: Option<&UserSession>,
        hls_url: &str,
        virtual_id: u32,
        input: &ConfigInput,
        connection_permission: UserConnectionPermission) -> impl IntoResponse + Send {
    let url = replace_url_extension(hls_url, HLS_EXT);
    let server_info = app_state.app_config.get_user_server_info(user);

    let (request_url, session_token) = match user_session {
        Some(session) => {
            match app_state.active_provider.force_exact_acquire_connection(&session.provider).await.get_provider_config() {
                Some(provider_cfg) => {
                    let stream_url = get_stream_alternative_url(&url, input, &provider_cfg);
                    (stream_url, Some(session.token.to_string()))
                },
                None => (url, None),
            }
        },
        None => {
            match app_state.active_provider.get_next_provider(&input.name).await {
                Some(provider_cfg) => {
                    let stream_url = get_stream_alternative_url(&url, input, &provider_cfg);
                    let user_session_token = format!("{fingerprint}{virtual_id}");
                    let session_token= app_state.active_users.create_user_session(user, &user_session_token, virtual_id, &provider_cfg.name, &stream_url, connection_permission).await;
                    (stream_url, session_token)
                },
                None => (url, None),
            }
        }
    };

    match request::download_text_content(Arc::clone(&app_state.http_client.load()), input, &request_url, None).await {
        Ok((content, response_url)) => {
            let rewrite_hls_props = RewriteHlsProps {
                secret: &app_state.app_config.encrypt_secret,
                base_url: &server_info.get_base_url(),
                content: &content,
                hls_url: response_url,
                virtual_id,
                input_id: input.id,
                user_token: session_token.as_deref(),
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            error!("Failed to download m3u8 {}", sanitize_sensitive_info(err.to_string().as_str()));
            create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::ChannelUnavailable).into_response()
        }
    }
}

async fn hls_api_stream(
    Fingerprint(fingerprint): Fingerprint,
    req_headers: axum::http::HeaderMap,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let (user, target) = try_option_bad_request!(
        app_state.app_config.get_target_for_user(&params.username, &params.password), false,
        format!("Could not find any user for hls stream {}", params.username));
    if user.permission_denied(&app_state) {
        return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserAccountExpired).into_response();
    }

    let target_name = &target.name;
    let virtual_id = params.stream_id;
    let input = try_option_bad_request!(app_state.app_config.get_input_by_id(params.input_id), true, format!("Cant find input for target {target_name}, context {}, stream_id {virtual_id}", XtreamCluster::Live));

    let user_session_token = format!("{fingerprint}{virtual_id}");
    let mut user_session = app_state.active_users.get_user_session(&user.username, &user_session_token).await;

    if let Some(session)  = &mut user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserConnectionsExhausted).into_response();
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::ProviderConnectionsExhausted).into_response();
        }

        let hls_url = match get_hls_session_token_and_url_from_token(&app_state.app_config.encrypt_secret, &params.token) {
            Some((Some(session_token), hls_url)) if session.token.eq(&session_token) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };

        session.stream_url = hls_url;
        if session.virtual_id == virtual_id {
            if is_seek_request(XtreamCluster::Live, &req_headers).await {
                // partial request means we are in reverse proxy mode, seek happened
                return force_provider_stream_response(&app_state, session, PlaylistItemType::LiveHls, &req_headers, &input, &user).await.into_response()
            }
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let connection_permission = user.connection_permission(&app_state).await;
        if connection_permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserConnectionsExhausted).into_response();
        }

        if is_hls_url(&session.stream_url) {
            return handle_hls_stream_request(&fingerprint, &app_state, &user, Some(session), &session.stream_url, virtual_id, &input, connection_permission).await.into_response();
        }

        force_provider_stream_response(&app_state, session, PlaylistItemType::LiveHls, &req_headers, &input, &user).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

pub fn hls_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/hls/{username}/{password}/{input_id}/{stream_id}/{token}", axum::routing::get(hls_api_stream))
    //cfg.service(web::resource("/hls/{token}/{stream}").route(web::get().to(xtream_player_api_hls_stream)));
    //cfg.service(web::resource("/play/{token}/{type}").route(web::get().to(xtream_player_api_play_stream)));
}
