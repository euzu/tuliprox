use crate::{
    api::{
        api_utils::{
            admission_failure_response, connection_priority_for_kind,
            create_api_proxy_user, create_playback_session_fingerprint, create_session_fingerprint, force_provider_stream_response, get_headers_from_request,
            get_hls_session_ttl_secs, get_stream_alternative_url, is_seek_request, local_stream_response,
            try_option_bad_request, try_unwrap_body,
            HeaderFilter,
        },
        model::{
            AppState, CustomVideoStreamType, ProviderAllocation, UserSession,
        },
    },
    auth::{create_access_token, Fingerprint},
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
use std::{collections::HashMap, sync::Arc};
use url::Url;
use shared::model::ConnectFailureReason;
use crate::auth::check_network_access_only;

const PLAYLIST_TEMPLATE: &str = r"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:10.0,
{url}
#EXT-X-ENDLIST
";
const MAX_MANUAL_REDIRECTS: usize = 10;

fn is_m3u_catchup_session_token(session_token: &str) -> bool {
    session_token.starts_with("m3u-catchup|") || session_token.starts_with("catchup|")
}

fn query_flag_is_archive(key: &str) -> bool {
    key.eq_ignore_ascii_case("utc")
}

fn query_flag_marks_start_context(key: &str) -> bool {
    key.eq_ignore_ascii_case("end") || key.eq_ignore_ascii_case("duration") || key.eq_ignore_ascii_case("lutc")
}

pub(in crate::api) fn m3u_archive_epg_reference_ts(stream_url: &str) -> Option<i64> {
    let parsed = Url::parse(stream_url).ok()?;
    let path = parsed.path();
    if let Some(rest) = path.split("/archive-").nth(1) {
        let start = rest.split('-').next()?;
        if let Ok(ts) = start.parse::<i64>() {
            return Some(ts);
        }
    }
    if let Some(rest) = path.split("/timeshift_abs-").nth(1) {
        let start = rest.trim_end_matches(".ts").trim_end_matches(".m3u8");
        if let Ok(ts) = start.parse::<i64>() {
            return Some(ts);
        }
    }
    let mut start_ts = None;
    let mut has_start_context = false;
    for (key, value) in parsed.query_pairs() {
        if query_flag_is_archive(&key) {
            if let Ok(ts) = value.parse::<i64>() {
                return Some(ts);
            }
        } else if key.eq_ignore_ascii_case("start") {
            start_ts = value.parse::<i64>().ok();
        } else if query_flag_marks_start_context(&key) {
            has_start_context = true;
        }
    }

    has_start_context.then_some(start_ts).flatten()
}

#[derive(Debug, Deserialize)]
struct HlsApiPathParams {
    username: String,
    password: String,
    target_id: u16,
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

fn extract_hls_provider_session_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let cookies = headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next().map(str::trim))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    let mut session_headers = HashMap::new();
    if !cookies.is_empty() {
        session_headers.insert(String::from("cookie"), cookies.join("; "));
    }
    session_headers
}

async fn release_prepared_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    addr: &std::net::SocketAddr,
) {
    let _transition_guard = app_state
        .active_users
        .acquire_playback_transition(username, session_token)
        .await;
    app_state
        .active_users
        .release_unbound_session_reservation(username, session_token, None, false)
        .await;
    app_state
        .active_users
        .clear_unbound_session_addr(username, session_token, addr)
        .await;
}

async fn terminate_failed_hls_manifest_session(app_state: &Arc<AppState>, username: &str, session_token: &str) {
    let _transition_guard = app_state
        .active_users
        .acquire_playback_transition(username, session_token)
        .await;
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token).await;
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
    target_id: u16,
    user_session: Option<&UserSession>,
    hls_url: &str,
    archive_reference: Option<i64>,
    virtual_id: u32,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
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
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
    let (request_url, session_token, provider_handle) = if let Some(session) = user_session {
        let pinned_provider = if session.provider.is_empty() { &input.name } else { &session.provider };
        let provider_handle = if let Some(handle) = app_state
            .active_provider
            .acquire_exact_connection_with_grace_for_session(
                pinned_provider,
                &fingerprint.addr,
                false,
                connection_priority_for_kind(user, session.connection_kind.unwrap_or(connection_kind)),
                session.connection_kind.unwrap_or(connection_kind),
                Some(session.token.as_str()),
            )
            .await
        {
            Some(handle)
        } else {
            debug_if_enabled!(
                "HLS pinned provider {} unavailable for {}; aborting allocation to prevent mid-session migration",
                sanitize_sensitive_info(pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            None
        };

        if provider_handle.is_none() {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }

        match provider_handle.as_ref().map(|handle| &handle.allocation) {
            Some(ProviderAllocation::Exhausted) => (url, None, provider_handle),
            Some(ProviderAllocation::Available(cfg) | ProviderAllocation::GracePeriod(cfg)) => {
                let Some(stream_url) = get_stream_alternative_url(&url, input, cfg) else {
                    app_state.connection_manager.release_provider_handle(provider_handle).await;
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
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
                        connection_kind: session.connection_kind,
                        socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
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
        let manifest_item_type = if archive_reference.is_some() {
            PlaylistItemType::Catchup
        } else {
            PlaylistItemType::LiveHls
        };
        let user_session_token =
            create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, manifest_item_type, None);
        match app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &fingerprint.addr,
                false,
                connection_priority_for_kind(user, connection_kind),
                connection_kind,
                Some(&user_session_token),
            )
            .await
        {
            Some(provider_handle) => match provider_handle.allocation.get_provider_config() {
                Some(provider_cfg) => {
                    let Some(stream_url) = get_stream_alternative_url(&url, input, &provider_cfg) else {
                        app_state
                            .connection_manager
                            .release_provider_handle(Some(provider_handle))
                            .await;
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    };
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
                            connection_kind: Some(connection_kind),
                            socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
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
        request::download_text_content_with_manual_redirects_and_headers(
            &app_state.app_config,
            &app_state.http_client_no_redirect.load(),
            &input_source,
            Some(&headers),
            false,
            MAX_MANUAL_REDIRECTS,
        )
        .await
    } else {
        request::download_text_content_with_headers(
            &app_state.app_config,
            &app_state.http_client.load(),
            &input_source,
            Some(&headers),
            false,
        )
        .await
    };
    match download_result {
        Ok((content, response_url, response_headers)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id,
                virtual_id,
                input_id: input.id,
                user_token: session_token.as_deref(),
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            if let Some(session_token) = session_token.as_deref() {
                let session_headers = extract_hls_provider_session_headers(&response_headers);
                if !session_headers.is_empty() {
                    app_state
                        .active_users
                        .update_session_provider_headers(&user.username, session_token, &session_headers)
                        .await;
                }
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr).await;
            }
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            error!("Failed to download m3u8 {}", sanitize_sensitive_info(&err.to_string()));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(app_state, &user.username, session_token).await;
            }

            let custom_stream_response = app_state.app_config.custom_stream_response.load();
            if custom_stream_response.as_ref().and_then(|c| c.channel_unavailable.as_ref()).is_some() {
                let custom_stream_token = create_access_token(&app_state.app_config.access_token_secret, 30);
                let url = format!(
                    "{}/{CUSTOM_VIDEO_PREFIX}/{}/{}/{}.ts?token={custom_stream_token}",
                    server_info.get_base_url(),
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
    archive_reference: Option<i64>,
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
            epg_channel_id: None,
            epg_reference_ts: None,
        },
    };

    if archive_reference.is_some() {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.epg_reference_ts = None;
    }
    channel
}

#[allow(clippy::too_many_lines)]
async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let api_proxy_user = create_api_proxy_user(&app_state);
    let (user, target) = if params.username == api_proxy_user.username && params.password == api_proxy_user.password {
        let Some(target) = app_state.app_config.get_target_by_id(params.target_id) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        (Arc::new(api_proxy_user), target)
    } else {
        let Some((user, target)) = app_state.app_config.get_target_for_user(&params.username, &params.password) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        if target.id != params.target_id {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        (user, target)
    };
    hls_api_stream_resolved(
        fingerprint,
        req_headers,
        app_state,
        user,
        target,
        params.input_id,
        params.stream_id,
        params.token,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn hls_api_stream_resolved(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input_id: u16,
    stream_id: u32,
    token: String,
) -> axum::response::Response {
    // Network access check only - permission check is done later with full stream info
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let target_name = &target.name;
    let virtual_id = stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", input_id)
    );

    if user.permission_denied(&app_state) {
        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, "", None).await;
        return admission_failure_response(
            &app_state,
            &fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            &req_headers,
            ConnectFailureReason::UserAccountExpired,
        );
    }

    debug_if_enabled!("ID chain for hls endpoint: request_stream_id={stream_id} -> virtual_id={virtual_id}");
    let encrypt_secret = app_state.get_encrypt_secret();
    let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, &token) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let lookup_session_token = decoded_hls_token
        .0
        .clone()
        .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, virtual_id, false));
    let mut user_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, &lookup_session_token)
        .await;

    if let Some(session) = &mut user_session {
        let decoded_archive_reference = m3u_archive_epg_reference_ts(&decoded_hls_token.1);
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &decoded_hls_token.1, decoded_archive_reference)
                    .await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &decoded_hls_token.1, decoded_archive_reference)
                    .await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }

        let hls_url = match decoded_hls_token {
            (Some(session_token), hls_url) if session.token.eq(&session_token) => hls_url,
            (None, hls_url) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let hls_url = hls_url.intern();
        let archive_reference = m3u_archive_epg_reference_ts(&hls_url);
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state
                .connection_manager
                .touch_http_activity(&user.username, &session.token, &fingerprint.addr)
                .await;
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
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
                    None,
                )
                .await
                .into_response();
            }
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let (connection_admission, grace_mode, request_class) =
            crate::api::api_utils::resolve_playback_request_admission(
                &app_state,
                &user,
                &fingerprint,
                if is_m3u_catchup_session_token(&session.token) || archive_reference.is_some() {
                    PlaylistItemType::Catchup
                } else {
                    PlaylistItemType::LiveHls
                },
                Some(session),
                &session.token,
                true,
                crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
                // HLS playlist requests (.m3u8) are explicit Prepare: they set up session metadata
                // but do not consume an admission slot. Segment and other media requests use Activate.
                is_hls_url(&hls_url),
                false,
            )
            .await;
        let connection_permission = connection_admission.permission;
        let connection_kind = connection_admission
            .kind
            .or(session.connection_kind)
            .unwrap_or(crate::api::model::ConnectionKind::Normal);
        session.permission = connection_permission;
        session.connection_kind = Some(connection_kind);
        if connection_permission == UserConnectionPermission::Exhausted {
            let provider = if session.provider.is_empty() {
                input.name.clone()
            } else {
                session.provider.clone()
            };
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &session.stream_url, archive_reference).await;
            return admission_failure_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                provider,
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if is_hls_url(&session.stream_url) {
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                target.id,
                Some(session),
                &session.stream_url,
                archive_reference,
                virtual_id,
                &input,
                &req_headers,
                connection_permission,
                connection_kind,
            )
            .await
            .into_response();
        }

        if is_file_url(&session.stream_url) {
            let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                connection_kind,
                Some(&session.token),
                Some(request_class),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
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
            grace_mode,
        )
            .await
            .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

pub fn hls_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/hls/{username}/{password}/{target_id}/{input_id}/{stream_id}/{token}", axum::routing::get(hls_api_stream))
    //cfg.service(web::resource("/hls/{token}/{stream}").route(web::get().to(xtream_player_api_hls_stream)));
    //cfg.service(web::resource("/play/{token}/{type}").route(web::get().to(xtream_player_api_play_stream)));
}

#[cfg(test)]
mod tests {
    use super::{extract_hls_provider_session_headers, m3u_archive_epg_reference_ts};
    use axum::http::HeaderMap;

    #[test]
    fn archive_epg_reference_supports_query_and_path_formats() {
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?utc=1700000000&lutc=1700003600"),
            Some(1_700_000_000)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/archive-1700003600-1700007200.m3u8"),
            Some(1_700_003_600)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/timeshift_abs-1700007200.ts"),
            Some(1_700_007_200)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000&end=1700003600"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn archive_epg_reference_rejects_plain_start_queries() {
        assert_eq!(m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000"), None);
    }

    #[test]
    fn extract_hls_provider_session_headers_converts_set_cookie_to_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "sid=abc; Path=/; HttpOnly".parse().expect("valid cookie"));
        headers.append("set-cookie", "pref=1; Secure".parse().expect("valid cookie"));

        let session_headers = extract_hls_provider_session_headers(&headers);

        assert_eq!(session_headers.get("cookie").map(String::as_str), Some("sid=abc; pref=1"));
    }
}
