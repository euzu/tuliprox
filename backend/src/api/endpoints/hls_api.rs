use crate::{
    api::{
        api_utils::{
            create_session_fingerprint, force_provider_stream_response, get_headers_from_request,
            get_hls_session_ttl_secs,
            admission_failure_response, get_stream_alternative_url, is_seek_request, local_stream_response, try_option_bad_request,
            try_unwrap_body, HeaderFilter,
        },
        model::{
            AppState, CustomVideoStreamType, ProviderAllocation, UserSession,
        },
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigInputFlags, ConfigTarget, InputSource, ProxyUserCredentials},
    processing::parser::hls::{get_hls_session_token_and_url_from_token, rewrite_hls, RewriteHlsProps},
    repository::{m3u_get_item_for_stream_id, xtream_get_item_for_stream_id},
    utils::{debug_if_enabled, request, request::is_file_url},
};
use axum::{http::HeaderMap, response::IntoResponse};
use log::{debug, error};
use serde::Deserialize;
use shared::{
    model::{PlaylistItemType, StreamChannel, TargetType, UserConnectionPermission, XtreamCluster},
    utils::{is_hls_url, replace_url_extension, sanitize_sensitive_info, Internable, CUSTOM_VIDEO_PREFIX, HLS_EXT},
};
use std::sync::Arc;
use url::Url;

const PLAYLIST_TEMPLATE: &str = r"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.0,
{url}
#EXT-X-ENDLIST
";
const MAX_MANUAL_REDIRECTS: usize = 10;

#[derive(Debug, Deserialize)]
struct HlsApiPathParams {
    username: String,
    password: String,
    input_id: u16,
    stream_id: u32,
    token: String,
}

fn hls_response(hls_content: String) -> impl IntoResponse + Send {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/x-mpegurl")
        .body(hls_content))
}

fn normalize_xtream_live_hls_url(hls_url: &str, input: &ConfigInput) -> String {
    if !input.input_type.is_xtream() || !input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix) {
        return hls_url.to_string();
    }

    let (Some(username), Some(password)) = (input.username.as_deref(), input.password.as_deref()) else {
        return hls_url.to_string();
    };

    let Ok(mut parsed) = Url::parse(hls_url) else {
        return hls_url.to_string();
    };
    let Some(segments) = parsed.path_segments() else {
        return hls_url.to_string();
    };

    let parts: Vec<&str> = segments.collect();
    if parts.len() >= 3 && parts[0] == username && parts[1] == password {
        parsed.set_path(&format!("/live/{}", parts.join("/")));
        return parsed.to_string();
    }

    hls_url.to_string()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn handle_hls_stream_request(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    user_session: Option<&UserSession>,
    hls_url: &str,
    virtual_id: u32,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
) -> impl IntoResponse + Send {
    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
    if normalized_hls_url != hls_url {
        debug_if_enabled!(
            "Normalized xtream hls url from {} to {}",
            sanitize_sensitive_info(hls_url),
            sanitize_sensitive_info(&normalized_hls_url)
        );
    }
    let url = replace_url_extension(&normalized_hls_url, HLS_EXT);
    let server_info = app_state.app_config.get_user_server_info(user);

    let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
    let (request_url, session_token, provider_handle) = if let Some(session) = user_session {
        let provider_handle = if let Some(handle) = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &fingerprint.addr,
                false,
                user.priority,
                Some(session.token.as_str()),
            )
            .await
        {
            Some(handle)
        } else {
            debug_if_enabled!(
                "HLS pinned provider {} unavailable for {}; falling back to lineup allocation",
                sanitize_sensitive_info(&session.provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            app_state.active_provider.acquire_connection_with_grace(&input.name, &fingerprint.addr, false, user.priority).await
        };

        match provider_handle.as_ref().map(|handle| &handle.allocation) {
            Some(ProviderAllocation::Exhausted) => (url, None, provider_handle),
            Some(ProviderAllocation::Available(cfg) | ProviderAllocation::GracePeriod(cfg)) => {
                let stream_url = get_stream_alternative_url(&url, input, cfg);
                let session_token = app_state
                    .active_users
                    .create_user_session(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token: &session.token,
                        virtual_id,
                        provider: &cfg.name,
                        stream_url: &stream_url,
                        addr: &fingerprint.addr,
                        connection_permission,
                    })
                    .await;
                app_state
                    .active_provider
                    .refresh_provider_reservation(&cfg.name, &session_token, hls_session_ttl_secs)
                    .await;
                (stream_url, Some(session_token), provider_handle)
            }
            None => (url, None, None),
        }
    } else {
        let user_session_token = create_session_fingerprint(fingerprint, &user.username, virtual_id);
        match app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &fingerprint.addr,
                false,
                user.priority,
                Some(&user_session_token),
            )
            .await
        {
            Some(provider_handle) => match provider_handle.allocation.get_provider_config() {
                Some(provider_cfg) => {
                    let stream_url = get_stream_alternative_url(&url, input, &provider_cfg);
                    debug_if_enabled!(
                            "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} provider={} stream_url={}",
                            sanitize_sensitive_info(&user.username),
                            provider_cfg.name,
                            sanitize_sensitive_info(&stream_url)
                        );
                    let session_token = app_state
                        .active_users
                        .create_user_session(crate::api::model::CreateUserSessionParams {
                            user,
                            session_token: &user_session_token,
                            virtual_id,
                            provider: &provider_cfg.name,
                            stream_url: &stream_url,
                            addr: &fingerprint.addr,
                            connection_permission,
                        })
                        .await;
                    app_state
                        .active_provider
                        .refresh_provider_reservation(&provider_cfg.name, &session_token, hls_session_ttl_secs)
                        .await;
                    (stream_url, Some(session_token), Some(provider_handle))
                }
                None => (url, None, Some(provider_handle)),
            },
            None => (url, None, None),
        }
    };

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle).await;

    // Don't forward Range on playlist fetch; segments use original headers in provider path
    let filter_header: HeaderFilter = Some(Box::new(|name: &str| !name.eq_ignore_ascii_case("range")));
    let forwarded = get_headers_from_request(req_headers, &filter_header);
    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers =
        request::get_request_headers(None, Some(&forwarded), disabled_headers.as_ref(), default_user_agent.as_deref());
    let input_source = InputSource::from(input).with_url(request_url);
    let use_manual_redirects = app_state.should_use_manual_redirects();
    let download_result = if use_manual_redirects {
        request::download_text_content_with_manual_redirects(
            &app_state.app_config,
            &app_state.http_client_no_redirect.load(),
            &input_source,
            Some(&headers),
            None,
            false,
            MAX_MANUAL_REDIRECTS,
        )
        .await
    } else {
        request::download_text_content(
            &app_state.app_config,
            &app_state.http_client.load(),
            &input_source,
            Some(&headers),
            None,
            false,
        )
        .await
    };
    match download_result {
        Ok((content, response_url)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
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

            let custom_stream_response = app_state.app_config.custom_stream_response.load();
            if custom_stream_response.as_ref().and_then(|c| c.channel_unavailable.as_ref()).is_some() {
                let url = format!(
                    "{}/{CUSTOM_VIDEO_PREFIX}/{}/{}/{}.ts",
                    &server_info.get_base_url(),
                    user.username,
                    user.password,
                    CustomVideoStreamType::ChannelUnavailable
                );

                let playlist = PLAYLIST_TEMPLATE.replace("{url}", &url);
                hls_response(playlist).into_response()
            } else {
                axum::http::StatusCode::NOT_FOUND.into_response()
            }
        }
    }
}

async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, None).await {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    m3u_get_item_for_stream_id(virtual_id, app_state, target).await.ok().map(|pli| pli.to_stream_channel(target_id))
}

async fn resolve_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    hls_url: &str,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    let mut channel = match get_stream_channel(app_state, target, virtual_id).await {
        Some(mut channel) => {
            channel.url = Arc::from(hls_url);
            channel
        }
        None => StreamChannel {
            target_id: target.id,
            virtual_id,
            provider_id: 0,
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: unknown.clone(),
            title: unknown,
            url: Arc::from(hls_url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
        },
    };

    channel.item_type = PlaylistItemType::LiveHls;
    channel
}

#[allow(clippy::too_many_lines)]
async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: axum::http::HeaderMap,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let (user, target) = try_option_bad_request!(
        app_state.app_config.get_target_for_user(&params.username, &params.password),
        false,
        format!("Could not find any user for hls stream {}", params.username)
    );
    let target_name = &target.name;
    let virtual_id = params.stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(params.input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", params.input_id)
    );

    if user.permission_denied(&app_state) {
        let denied_channel = resolve_stream_channel(
            &app_state,
            &target,
            &input,
            virtual_id,
            &Arc::from(String::new()),
        )
        .await;
        return admission_failure_response(
            &app_state,
            &fingerprint,
            &user,
            denied_channel,
            input.name.as_ref(),
            &req_headers,
            crate::repository::ConnectFailureReason::UserAccountExpired,
        );
    }

    debug_if_enabled!("ID chain for hls endpoint: request_stream_id={} -> virtual_id={virtual_id}", params.stream_id);
    let encrypt_secret = app_state.get_encrypt_secret();
    let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, &params.token) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let lookup_session_token = decoded_hls_token
        .0
        .clone()
        .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, virtual_id));
    let mut user_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, &lookup_session_token)
        .await;

    if let Some(session) = &mut user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &decoded_hls_token.1).await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                &session.provider,
                &req_headers,
                crate::repository::ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &decoded_hls_token.1).await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                &session.provider,
                &req_headers,
                crate::repository::ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }

        let hls_url = match decoded_hls_token {
            (Some(session_token), hls_url) if session.token.eq(&session_token) => hls_url,
            (None, hls_url) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let hls_url = hls_url.intern();
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state
                .connection_manager
                .touch_http_activity(&user.username, &session.token, &fingerprint.addr)
                .await;
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url).await;
            if is_seek_request(stream_channel.cluster, &req_headers).await {
                // partial request means we are in reverse proxy mode, seek happened
                return force_provider_stream_response(
                    &fingerprint,
                    &app_state,
                    session,
                    stream_channel,
                    crate::api::api_utils::ForceStreamRequestContext {
                        req_headers: &req_headers,
                        input: &input,
                        user: &user,
                        session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                    },
                )
                .await
                .into_response();
            }
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let connection_permission = if user.max_connections > 0 && app_state.app_config.config.load().user_access_control {
            app_state
                .active_users
                .connection_permission_for_session(&user.username, user.max_connections, &session.token)
                .await
        } else {
            UserConnectionPermission::Allowed
        };
        if connection_permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &session.stream_url).await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                input.name.as_ref(),
                &req_headers,
                crate::repository::ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if is_hls_url(&session.stream_url) {
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                Some(session),
                &session.stream_url,
                virtual_id,
                &input,
                &req_headers,
                connection_permission,
            )
            .await
            .into_response();
        }

        if is_file_url(&session.stream_url) {
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url).await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                Some(&session.token),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url).await;
        force_provider_stream_response(
            &fingerprint,
            &app_state,
            session,
            stream_channel,
            crate::api::api_utils::ForceStreamRequestContext {
                req_headers: &req_headers,
                input: &input,
                user: &user,
                session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
            },
        )
            .await
            .into_response()
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
