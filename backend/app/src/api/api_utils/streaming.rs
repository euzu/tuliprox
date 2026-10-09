use super::{
    allows_provider_pool_failover, cleanup_forced_reopen_addrs, connection_priority_for_kind,
    get_catchup_session_ttl_secs, get_grace_period_millis, get_hls_session_ttl_secs, get_stream_options,
    get_stream_throttle, hls_resource_failure_status, is_media_server_playback_url, is_media_server_stream_ref_url,
    is_socket_bound_playback_session, is_stream_metrics_enabled, is_throttled_stream, mark_response_as_uncompressed,
    open_media_server_stream_for_input, prepare_stream_metering, re_resolve_stalker_url_singleflight,
    resolve_streaming_strategy_with_provider_handle, session_reacquire_cleanup_addrs,
    should_defer_provider_open_for_grace_hold, should_refresh_stalker_playback, split_provider_stream_open,
    stalker_stream_kind, stream_admission_rejected_response, stream_response_with_provider_handle, try_unwrap_body,
    ConnectFailedAttempt, CurrentSessionGuard, ForceStreamRequestContext, PlaybackRequestClass, RedirectParams,
    ResourceFetchPolicy, StreamMeteringConfig, StreamOptions, StreamResponseMode, HLS_MEDIA_CAPACITY_WAIT,
};
use crate::{
    api::{
        model::{
            create_active_client_stream, create_channel_unavailable_stream, get_custom_stream_response_error_status,
            get_stream_response_with_headers, is_custom_video_stream_enabled, open_provider_stream_with_lifecycle,
            AppState, CustomVideoStreamType, ProviderStreamCustomReason, ProviderStreamFactoryOptions,
            ProviderStreamOpenLifecycle, ProviderStreamState, SharedStreamCtx, SharedStreamManager, StreamDetails,
            StreamError, ThrottledStream, UserSession,
        },
        panel_api::{can_provision_on_exhausted, create_panel_api_provisioning_stream_details},
    },
    auth::Fingerprint,
    model::{AppConfig, ConfigInput, ConfigInputFlags, ConfigTarget, PlaybackKind, ProxyUserCredentials},
    utils::{debug_if_enabled, request},
};
use axum::{
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
};
use futures::StreamExt;
use log::{debug, error, info, log_enabled, warn};
use shared::{
    concat_string,
    defaults::HLS_EXT,
    error::TuliproxError,
    model::{
        ConfigTargetOptions, FailureStage, PlaylistEntry, PlaylistItemType, StreamChannel, StreamInfo,
        UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        extract_extension_from_url, human_readable_kbps, is_sanitize_sensitive_info_enabled, sanitize_sensitive_info,
        trim_slash, Internable,
    },
};
use std::{borrow::Cow, collections::HashMap, sync::Arc, time::Duration};
use tuliprox_session::{ProviderSessionHeaders, SessionProviderHeaders};
use url::Url;

pub(crate) fn resolve_request_url_for_logging<'a>(input: &ConfigInput, stream_url: &'a str) -> Cow<'a, str> {
    if !is_sanitize_sensitive_info_enabled() {
        return Cow::Borrowed(stream_url);
    }
    if is_media_server_playback_url(input, stream_url) {
        return Cow::Borrowed("media-server://<redacted>");
    }

    let provider = input.get_resolve_provider(stream_url);
    if let Ok(url) = Url::parse(stream_url) {
        return Cow::Owned(request::preview_request_target_for_logging(&url, provider.as_ref()));
    }

    input
        .resolve_url(stream_url)
        .ok()
        .and_then(|resolved| {
            Url::parse(resolved.as_ref())
                .ok()
                .map(|url| Cow::Owned(request::preview_request_target_for_logging(&url, provider.as_ref())))
        })
        .unwrap_or(Cow::Borrowed(stream_url))
}

pub(crate) fn record_connect_failed_attempt(attempt: ConnectFailedAttempt<'_>) {
    let user_agent = attempt
        .req_headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let info = StreamInfo::new(shared::model::StreamInfoParams {
        uid: 0,
        meter_uid: 0,
        username: &attempt.user.username,
        addr: &attempt.fingerprint.addr,
        client_ip: &attempt.fingerprint.client_ip,
        provider: attempt.provider_name,
        stream_channel: attempt.stream_channel,
        user_agent,
        country_code: None,
        session_token: None,
    });
    // Resolve target_name from target_id using the stable target config name.
    let target_name =
        attempt.app_state.app_config.get_target_by_id(info.channel.target_id).as_deref().map(|t| (&t.name).intern());
    attempt.app_state.connection_manager.record_connect_failed_with_provider_failure(
        &info,
        attempt.reason,
        attempt.failure_stage,
        None,
        None,
        target_name,
    );
}

pub(super) struct StreamingAcquireOptions<'a> {
    pub(super) force_provider: Option<&'a Arc<str>>,
    pub(super) allow_forced_provider_fallback: bool,
    pub(super) allow_provider_grace: bool,
    pub(super) user_priority: i8,
    pub(super) connection_kind: crate::api::model::ConnectionKind,
    pub(super) session_owner: Option<&'a str>,
    pub(super) playback_kind: PlaybackKind,
    pub(super) accept_requested_stream_url: bool,
    pub(super) capacity_wait_timeout: Option<Duration>,
}

pub(crate) async fn resolve_stream_user_agent_index(
    app_state: &AppState,
    input: &ConfigInput,
    has_provider_handle: bool,
    username: &str,
    session_owner: Option<&str>,
) -> Option<u64> {
    if input.has_flag(ConfigInputFlags::UserAgentStreamIndex) && has_provider_handle {
        if let Some(session_token) = session_owner {
            app_state
                .active_users
                .get_or_assign_user_agent_stream_index(username, session_token)
                .await
                .or_else(|| Some(app_state.active_users.next_user_agent_stream_index()))
        } else {
            Some(app_state.active_users.next_user_agent_stream_index())
        }
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::fn_params_excessive_bools)]
pub(super) async fn create_stream_response_details(
    app_state: &Arc<AppState>,
    stream_options: &StreamOptions,
    stream_url: &str,
    username: &str,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    stream_channel: &StreamChannel,
    item_type: PlaylistItemType,
    content_representation: crate::api::model::ProviderContentRepresentationMode,
    share_stream: bool,
    connection_permission: UserConnectionPermission,
    force_provider: Option<&Arc<str>>,
    allow_forced_provider_fallback: bool,
    allow_provider_grace: bool,
    virtual_id: VirtualId,
    user_priority: i8,
    connection_kind: crate::api::model::ConnectionKind,
    is_reopen: bool,
    session_owner: Option<&str>,
    session_headers: Option<&HashMap<String, String>>,
    accept_requested_stream_url: bool,
    grace_hold_override: Option<bool>,
    grace_resolution_context: Option<crate::api::model::GraceResolutionContext>,
    current_session: Option<CurrentSessionGuard<'_>>,
    preacquired_provider_handle: Option<tuliprox_session::ManagedProviderHandle>,
) -> Result<StreamDetails, TuliproxError> {
    let mut streaming_strategy = resolve_streaming_strategy_with_provider_handle(
        app_state,
        stream_url,
        fingerprint,
        input,
        StreamingAcquireOptions {
            force_provider,
            allow_forced_provider_fallback,
            allow_provider_grace,
            user_priority,
            connection_kind,
            session_owner,
            playback_kind: PlaybackKind::classify(item_type, extract_extension_from_url(stream_url)),
            accept_requested_stream_url,
            capacity_wait_timeout: (stream_options.response_mode == StreamResponseMode::HlsResource)
                .then_some(HLS_MEDIA_CAPACITY_WAIT),
        },
        Some(stream_channel),
        preacquired_provider_handle,
    )
    .await;
    // A capacity wait can outlive the session snapshot: revalidate the account binding and
    // read the cookies valid now, before any upstream request is sent.
    let current_session_headers;
    let session_headers = match current_session {
        Some(guard) => {
            match app_state
                .active_users
                .current_session_provider_headers(guard.username, guard.token, guard.identity, stream_url)
                .await
            {
                SessionProviderHeaders::NoSession => {
                    app_state
                        .connection_manager
                        .release_managed_provider_handle(streaming_strategy.provider_handle.take());
                    return Err(TuliproxError::Errors("playback session ended or switched provider account"));
                }
                SessionProviderHeaders::NoHeaders => current_session_headers = None,
                SessionProviderHeaders::Headers(headers) => current_session_headers = Some(headers),
            }
            current_session_headers.as_ref()
        }
        None => session_headers,
    };
    let user_agent_stream_index = resolve_stream_user_agent_index(
        app_state,
        input,
        streaming_strategy.provider_handle.is_some(),
        username,
        session_owner,
    )
    .await;
    let mut grace_period_options = app_state.get_grace_options();
    grace_period_options.period_millis = get_grace_period_millis(
        connection_permission,
        &streaming_strategy.provider_stream_state,
        grace_period_options.period_millis,
    );
    if let Some(hold) = grace_hold_override {
        grace_period_options.hold_stream = hold;
    }
    let provider_grace_active =
        matches!(streaming_strategy.provider_stream_state, ProviderStreamState::GracePeriod(_, _));

    let guard_provider_name = streaming_strategy
        .provider_handle
        .as_ref()
        .and_then(|managed| managed.handle())
        .and_then(|handle| handle.allocation.get_provider_name());

    if matches!(
        streaming_strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::ProviderExhausted, .. }
    ) && can_provision_on_exhausted(app_state, input)
    {
        if let Some(managed) = streaming_strategy.provider_handle.take() {
            drop(managed);
        }
        debug_if_enabled!(
            "panel_api: provider connections exhausted; sending provisioning stream for input {}",
            sanitize_sensitive_info(&input.name)
        );
        let mut details = create_panel_api_provisioning_stream_details(
            app_state,
            input,
            guard_provider_name.clone().or_else(|| Some(input.name.clone())),
            &grace_period_options,
            fingerprint.addr,
            virtual_id,
        );
        details.content_representation = content_representation;
        return Ok(details);
    }

    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom { response: provider_stream, reason } => {
            let (stream, stream_info) = provider_stream;
            // When allocation is exhausted or no connection was acquired, guard_provider_name is None.
            // Use input.name as fallback so the provider field is never empty.
            let provider_name = guard_provider_name.clone().unwrap_or_else(|| input.name.clone());
            Ok(StreamDetails {
                shared_subscriber_id: None,
                stream,
                stream_info,
                provider_name: Some(provider_name),
                request_url: None,
                session_headers: session_headers.cloned(),
                provider_session_headers: ProviderSessionHeaders::default(),
                user_agent_stream_index,
                grace_period: grace_period_options,
                provider_grace_active: false,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: streaming_strategy.provider_handle.take(),
                content_representation,
                grace_resolution_context,
                custom_reason: Some(reason),
                response_mode: stream_options.response_mode,
                session_registration: None,
            })
        }
        ProviderStreamState::Available(_provider_name, request_url)
        | ProviderStreamState::GracePeriod(_provider_name, request_url) => {
            let mut request_url = request_url;
            debug_if_enabled!(
                "Provider stream selection: allocated_provider={} actual_request_url={}",
                sanitize_sensitive_info(guard_provider_name.as_deref().unwrap_or("?")),
                sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
            );
            let defer_provider_stream_until_grace_check = if should_defer_provider_open_for_grace_hold(
                provider_grace_active,
                grace_period_options.hold_stream,
                item_type,
                is_reopen,
            ) {
                if let Some(provider_name) = guard_provider_name.as_ref() {
                    app_state.active_provider.is_over_limit(provider_name)
                } else {
                    false
                }
            } else {
                false
            };
            let is_fallback_provider = force_provider
                .is_some_and(|forced| guard_provider_name.as_ref().is_some_and(|allocated| allocated != forced));
            let session_headers = if is_fallback_provider { None } else { session_headers };
            let (stream, stream_info, provider_session_headers, reconnect_flag) =
                if defer_provider_stream_until_grace_check {
                    debug_if_enabled!(
                        "Deferring provider stream open until grace check completes for {}",
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
                    );
                    (None, None, ProviderSessionHeaders::default(), None)
                } else if is_media_server_stream_ref_url(request_url.as_ref()) {
                    match open_media_server_stream_for_input(app_state, input, request_url.as_ref(), req_headers).await
                    {
                        Ok((stream, stream_info)) => {
                            (Some(stream), stream_info, ProviderSessionHeaders::default(), None)
                        }
                        Err(err) => {
                            error!("Can't open media-server stream: {err}");
                            (None, None, ProviderSessionHeaders::default(), None)
                        }
                    }
                } else {
                    let parsed_url = Url::parse(&request_url);
                    let request_url_valid = parsed_url.is_ok();
                    let ((mut stream, mut stream_info, mut provider_session_headers), mut reconnect_flag) =
                        if let Ok(url) = parsed_url {
                            let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
                            let disabled_headers = app_state.get_disabled_headers();
                            let mut provider_stream_factory_options =
                                ProviderStreamFactoryOptions::new(&crate::api::model::ProviderStreamFactoryParams {
                                    addr: fingerprint.addr,
                                    item_type,
                                    share_stream,
                                    stream_options,
                                    stream_url: &url,
                                    req_headers,
                                    input_headers: streaming_strategy.input_headers.as_ref(),
                                    session_headers,
                                    disabled_headers: disabled_headers.as_ref(),
                                    default_user_agent: default_user_agent.as_deref(),
                                    username: Some(username),
                                    client_ip: Some(&fingerprint.client_ip),
                                    stream_channel: Some(stream_channel),
                                    connect_failure_stage: Some(FailureStage::ProviderOpen),
                                    content_representation,
                                });

                            if let Some(stream_index) = user_agent_stream_index {
                                provider_stream_factory_options.apply_user_agent_stream_index(stream_index);
                            }

                            let provider_config = input.get_resolve_provider(url.as_ref());
                            provider_stream_factory_options.set_provider(provider_config);
                            provider_stream_factory_options.apply_input_options(input);
                            if input.input_type.is_stalker() {
                                provider_stream_factory_options.require_public_destination();
                            }

                            let reconnect_flag = provider_stream_factory_options.get_reconnect_flag_clone();
                            let lifecycle = streaming_strategy
                                .provider_handle
                                .as_ref()
                                .and_then(ProviderStreamOpenLifecycle::from_managed);
                            let provider_stream = match open_provider_stream_with_lifecycle(
                                &app_state.provider_stream_ctx(),
                                &app_state.http_clients.default.load(),
                                provider_stream_factory_options,
                                lifecycle,
                            )
                            .await
                            {
                                None => (None, None, ProviderSessionHeaders::default()),
                                Some(open) => split_provider_stream_open(open),
                            };
                            (provider_stream, Some(reconnect_flag))
                        } else {
                            ((None, None, ProviderSessionHeaders::default()), None)
                        };
                    let should_refresh_stalker = should_refresh_stalker_playback(
                        input.input_type,
                        request_url_valid,
                        stream_info.as_ref().map(|(_, status, _, _)| *status),
                    );
                    if should_refresh_stalker {
                        let force_stalker_refresh =
                            stream_info.as_ref().is_some_and(|(_, status, _, _)| status.is_client_error());
                        let kind = stalker_stream_kind(stream_channel.cluster, item_type);
                        let resolve_result = re_resolve_stalker_url_singleflight(
                            app_state,
                            input,
                            stream_channel.provider_id,
                            kind,
                            force_stalker_refresh,
                        )
                        .await;
                        match resolve_result {
                            Ok(Some(refreshed_url)) => {
                                if let Ok(url) = Url::parse(&refreshed_url) {
                                    let default_user_agent =
                                        app_state.app_config.config.load().default_user_agent.clone();
                                    let disabled_headers = app_state.get_disabled_headers();
                                    let mut options = ProviderStreamFactoryOptions::new(
                                        &crate::api::model::ProviderStreamFactoryParams {
                                            addr: fingerprint.addr,
                                            item_type,
                                            share_stream,
                                            stream_options,
                                            stream_url: &url,
                                            req_headers,
                                            input_headers: streaming_strategy.input_headers.as_ref(),
                                            session_headers,
                                            disabled_headers: disabled_headers.as_ref(),
                                            default_user_agent: default_user_agent.as_deref(),
                                            username: Some(username),
                                            client_ip: Some(&fingerprint.client_ip),
                                            stream_channel: Some(stream_channel),
                                            connect_failure_stage: Some(FailureStage::ProviderOpen),
                                            content_representation,
                                        },
                                    );
                                    if let Some(stream_index) = user_agent_stream_index {
                                        options.apply_user_agent_stream_index(stream_index);
                                    }
                                    options.set_provider(input.get_resolve_provider(url.as_ref()));
                                    options.apply_input_options(input);
                                    options.require_public_destination();
                                    if let Some(m) = streaming_strategy.provider_handle.as_mut() {
                                        let _ = m.renew_opening_tokens();
                                    }
                                    let retry_reconnect_flag = options.get_reconnect_flag_clone();
                                    let retry_lifecycle = streaming_strategy
                                        .provider_handle
                                        .as_ref()
                                        .and_then(ProviderStreamOpenLifecycle::from_managed);
                                    let retried = open_provider_stream_with_lifecycle(
                                        &app_state.provider_stream_ctx(),
                                        &app_state.http_clients.default.load(),
                                        options,
                                        retry_lifecycle,
                                    )
                                    .await;
                                    if let Some(open) = retried {
                                        (stream, stream_info, provider_session_headers) =
                                            split_provider_stream_open(open);
                                        reconnect_flag = Some(retry_reconnect_flag);
                                        request_url = refreshed_url;
                                    } else {
                                        // Keep the original stream/stream_info: the upstream response
                                        // might still be serveable, and its status is needed for reporting.
                                        debug!("Stalker re-resolve retry could not open a stream, keeping original provider response");
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                warn!(
                                    "Failed to refresh Stalker playback URL: {}",
                                    sanitize_sensitive_info(&err.to_string())
                                );
                            }
                        }
                    }
                    (stream, stream_info, provider_session_headers, reconnect_flag)
                };

            if is_fallback_provider && stream.is_some() {
                if let Some(token) = session_owner {
                    let _ =
                        app_state.active_users.update_session_provider_headers(username, token, &HashMap::new()).await;
                }
            }

            if log_enabled!(log::Level::Debug) {
                if let Some((headers, status_code, response_url, _custom_video_type)) = stream_info.as_ref() {
                    debug!(
                        "Responding stream request {} with status {}, headers {:?}",
                        sanitize_sensitive_info(response_url.as_ref().map_or(stream_url, |s| s.as_str())),
                        status_code,
                        headers
                    );
                }
            }

            // An intentional deferred open must retain its grace allocation until body polling
            // resumes the provider request. Other failed opens release their allocation here.
            let failed_open = stream.is_none()
                || matches!(stream_info.as_ref(), Some((_, _, _, Some(custom))) if *custom != CustomVideoStreamType::Provisioning);
            let provider_handle = if failed_open && !defer_provider_stream_until_grace_check {
                // The managed owner releases the provider slot synchronously on drop;
                // a failed open must not rely on a lossy cleanup message.
                drop(streaming_strategy.provider_handle.take());
                match stream_info.as_ref() {
                    // Finite HLS resources hand routine upstream errors (e.g. live-edge 404) to the client.
                    Some((_, status, _, None)) if stream.is_none() && !status.is_success() => {
                        debug!("Provider answered {status} for {}", sanitize_sensitive_info(&request_url));
                    }
                    _ => error!("Can't open stream {}", sanitize_sensitive_info(&request_url)),
                }
                None
            } else {
                streaming_strategy.provider_handle.take()
            };

            Ok(StreamDetails {
                shared_subscriber_id: None,
                stream,
                stream_info,
                provider_name: guard_provider_name.clone(),
                request_url: Some(request_url.clone()),
                session_headers: session_headers.cloned(),
                provider_session_headers,
                user_agent_stream_index,
                grace_period: grace_period_options,
                provider_grace_active,
                disable_provider_grace: false,
                reconnect_flag,
                provider_handle,
                content_representation,
                grace_resolution_context,
                custom_reason: None,
                response_mode: stream_options.response_mode,
                session_registration: None,
            })
        }
    }
}

impl<P> RedirectParams<'_, P>
where
    P: PlaylistEntry,
{
    pub fn get_query_path(&self, provider_id: u32, url: &str) -> String {
        let extension = self.stream_ext.map_or_else(
            || extract_extension_from_url(url).map_or_else(String::new, ToString::to_string),
            ToString::to_string,
        );

        // if there is an action_path (like for timeshift duration/start), it will be added in front of the stream_id
        if self.action_path.is_empty() {
            concat_string!(&provider_id.to_string(), &extension)
        } else {
            concat_string!(&trim_slash(self.action_path), "/", &provider_id.to_string(), &extension)
        }
    }
}

pub(super) fn prepare_body_stream<S>(
    app_state: &Arc<AppState>,
    item_type: PlaylistItemType,
    stream: S,
) -> axum::body::Body
where
    S: futures::Stream<Item = Result<bytes::Bytes, StreamError>> + Send + 'static,
{
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let body_stream = if is_throttled_stream(item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        axum::body::Body::from_stream(ThrottledStream::new(stream.boxed(), throttle_kbps))
    } else {
        axum::body::Body::from_stream(stream)
    };
    body_stream
}

pub(super) fn is_hop_by_hop_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub(super) fn no_custom_video_fallback_status(app_config: &AppConfig) -> StatusCode {
    // Two reasons we have no custom-video response:
    //   1. Operator disabled `custom_stream_response_enabled`  → return the
    //      configured fallback status (e.g. 502) so reverse proxies handle the
    //      socket consistently.
    //   2. Operator enabled custom-video but the concrete resource is missing
    //      → return `400` so downstream `proxy_intercept_errors on;` (Nginx)
    //      can sever the socket instead of looping on `200 OK`.
    // Collapsing both into the configured status code broke the Nginx-intercept
    // contract that the operator relied on by enabling custom-video in the
    // first place.
    if is_custom_video_stream_enabled(app_config) {
        StatusCode::BAD_REQUEST
    } else {
        get_custom_stream_response_error_status(app_config)
    }
}

pub async fn force_provider_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    stream_channel: StreamChannel,
    ctx: ForceStreamRequestContext<'_>,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> axum::response::Response {
    force_stream_response(
        fingerprint,
        app_state,
        user_session,
        stream_channel,
        ctx,
        grace_mode,
        StreamResponseMode::Stream,
    )
    .await
}

/// Answers a failed finite HLS resource with its status and the forwarded upstream headers.
/// Its provider slot is freed and an entry reservation no body ever used is released; a session
/// that streams elsewhere keeps its reservation (see `release_unbound_session_reservation`).
pub(super) async fn reject_hls_resource(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    stream_details: StreamDetails,
    status: StatusCode,
) -> axum::response::Response {
    let StreamDetails { provider_handle, stream_info, .. } = stream_details;
    app_state.connection_manager.release_managed_provider_handle(provider_handle);
    app_state.active_users.release_unbound_session_reservation(username, session_token, None, false).await;
    let mut response = status.into_response();
    if let Some((headers, _, _, None)) = stream_info {
        for (name, value) in headers {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                response.headers_mut().insert(name, value);
            }
        }
    }
    response
}

#[allow(clippy::too_many_lines)]
pub(super) async fn force_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    mut stream_channel: StreamChannel,
    ctx: ForceStreamRequestContext<'_>,
    grace_mode: Option<crate::api::model::GraceMode>,
    response_mode: StreamResponseMode,
) -> axum::response::Response {
    let hls_resource = response_mode == StreamResponseMode::HlsResource;
    // Finite HLS resources stay pinned to the session account and never rebind or clean up
    // other sockets, so they skip the session transition gate. Holding it across the
    // capacity wait would serialize parallel audio and video rendition requests.
    let _transition_guard = if hls_resource {
        None
    } else {
        Some(app_state.active_users.acquire_playback_transition(&ctx.user.username, &user_session.token).await)
    };
    let stream_options = get_stream_options(&app_state.app_config, response_mode);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;
    let item_type = stream_channel.item_type;

    // Forced reopens must clear stale provider slots before reacquiring. For adaptive HLS/DASH
    // and Catchup sessions we only target old active stream sockets of the same session, never
    // manifest-only session addresses, otherwise the controlling playlist request gets torn down.
    let cleanup_addrs = if hls_resource {
        Vec::new()
    } else if item_type.is_live_adaptive() || item_type == PlaylistItemType::Catchup {
        app_state
            .active_users
            .adaptive_session_stream_cleanup_addrs(&ctx.user.username, &user_session.token, &fingerprint.addr)
            .await
    } else {
        session_reacquire_cleanup_addrs(user_session, &fingerprint.addr)
    };

    if cleanup_addrs.is_empty() {
        debug_if_enabled!(
            "Forced reopen cleanup had no stale targets for item_type={item_type:?} session={} current_addr={}",
            sanitize_sensitive_info(&user_session.token),
            sanitize_sensitive_info(&fingerprint.addr.to_string())
        );
    } else {
        debug_if_enabled!(
            "Forced reopen cleanup releasing {} stale target(s) for item_type={item_type:?} session={} current_addr={}",
            cleanup_addrs.len(),
            sanitize_sensitive_info(&user_session.token),
            sanitize_sensitive_info(&fingerprint.addr.to_string())
        );
        cleanup_forced_reopen_addrs(app_state, &user_session.token, &cleanup_addrs).await;
    }

    // HLS child URLs remain bound to the manifest account before the first media byte.
    // Other streams prefer the account after media starts or while its allocation is active,
    // and can fall back to the lineup when that account is exhausted.
    let preferred_provider = (hls_resource
        || item_type.is_live()
        || user_session.media_started.load(std::sync::atomic::Ordering::Acquire)
        || app_state.active_provider.should_reuse_playback_provider(&user_session.token, &user_session.provider))
    .then_some(&user_session.provider);
    // Child URLs and their signed tokens belong to the account that fetched the playlist.
    let allow_forced_provider_fallback =
        !hls_resource && (!item_type.requires_provider_affinity() || allows_provider_pool_failover(item_type));
    // Never allow provider-side grace for forced seek/session reacquire.
    // Over-allocation here would break provider-side one-connection limits.
    let allow_provider_grace = false;
    let connection_kind = user_session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

    let session_identity = user_session.identity();
    let current_session = hls_resource.then_some(CurrentSessionGuard {
        username: &ctx.user.username,
        token: &user_session.token,
        identity: session_identity,
    });
    // Finite resources read their cookies after the capacity wait instead of from this snapshot.
    let provider_session_headers = preferred_provider
        .filter(|_| !hls_resource)
        .and_then(|_| user_session.provider_session_headers_for(&user_session.stream_url));
    let create_details = create_stream_response_details(
        app_state,
        &stream_options,
        &user_session.stream_url,
        &ctx.user.username,
        fingerprint,
        ctx.req_headers,
        ctx.input,
        &stream_channel,
        item_type,
        ctx.content_representation,
        share_stream,
        connection_permission,
        preferred_provider,
        allow_forced_provider_fallback,
        allow_provider_grace,
        VirtualId::new(stream_channel.virtual_id),
        connection_priority_for_kind(ctx.user, connection_kind),
        connection_kind,
        true,
        Some(user_session.token.as_str()),
        provider_session_headers.as_deref(),
        hls_resource || preferred_provider.is_some(),
        grace_mode.map(|mode| matches!(mode, crate::api::model::GraceMode::Hold)),
        None,
        current_session,
        None,
    );
    // The FORBIDDEN exits below leave the session reservation alone: they only fire once this
    // session identity is gone, and after an account switch the reservation belongs to the new
    // binding. Dropping the cancelled future frees any provider slot it held.
    let details_result = if hls_resource {
        tokio::select! {
            result = create_details => result,
            () = app_state.active_users.wait_for_playback_session_end(
                &ctx.user.username, &user_session.token, session_identity,
            ) => return StatusCode::FORBIDDEN.into_response(),
        }
    } else {
        create_details.await
    };
    let mut stream_details = match details_result {
        Ok(stream_details) => stream_details,
        Err(err) => {
            if hls_resource
                && !app_state
                    .active_users
                    .playback_session_is_current(&ctx.user.username, &user_session.token, session_identity)
                    .await
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            app_state
                .active_users
                .release_unbound_session_reservation(&ctx.user.username, &user_session.token, None, false)
                .await;
            error!("Failed to stream: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if hls_resource {
        if !app_state
            .active_users
            .playback_session_is_current(&ctx.user.username, &user_session.token, session_identity)
            .await
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        stream_details.session_registration = Some(tuliprox_session::PlaybackSessionRegistration {
            identity: session_identity,
            enforce_limits: app_state.app_config.config.load().user_access_control,
            grace_admitted: user_session.permission == UserConnectionPermission::GracePeriod,
        });
        if let Some(status) = hls_resource_failure_status(&stream_details) {
            return reject_hls_resource(app_state, &ctx.user.username, &user_session.token, stream_details, status)
                .await;
        }
    }

    let deferred_grace_hold_stream = stream_details.has_deferred_provider_open();

    if stream_details.has_stream() || deferred_grace_hold_stream {
        let selected_provider = stream_details.provider_name.clone();
        let selected_request_url = stream_details.request_url.clone();
        let selected_provider_headers = stream_details.provider_session_headers.clone();
        let metering = prepare_stream_metering(
            app_state,
            user_session.stream_url.as_ref(),
            false,
            stream_details.stream.is_some(),
            stream_details.has_deferred_provider_open(),
        );
        let provider_response =
            stream_details.stream_info.as_ref().map(|(h, sc, url, cvt)| (h.clone(), *sc, url.clone(), *cvt));
        if ctx.session_reservation_ttl_secs > 0 {
            if let Some(provider_name) = stream_details.provider_name.as_ref() {
                app_state.active_provider.refresh_adaptive_playback_lease(
                    provider_name,
                    &user_session.token,
                    PlaybackKind::classify(item_type, extract_extension_from_url(user_session.stream_url.as_ref())),
                    ctx.session_reservation_ttl_secs,
                );
            }
        }
        if let Some(allocated_provider) = stream_details.provider_name.as_ref() {
            if allocated_provider.as_ref() != user_session.provider.as_ref() {
                let new_stream_url =
                    stream_details.request_url.as_deref().map_or_else(|| user_session.stream_url.clone(), Arc::from);
                app_state
                    .active_users
                    .update_session_provider_binding(
                        &ctx.user.username,
                        &user_session.token,
                        Arc::clone(allocated_provider),
                        new_stream_url,
                    )
                    .await;
            }
            if allocated_provider.as_ref() != user_session.provider.as_ref()
                || !stream_details.provider_session_headers.is_empty()
            {
                let cookie_origin = provider_response
                    .as_ref()
                    .and_then(|(_, _, url, _)| url.as_ref())
                    .map_or(user_session.stream_url.as_ref(), Url::as_str);
                let active_users = &app_state.active_users;
                if hls_resource {
                    // Cookies from the previous account never enter a jar after an account switch.
                    active_users
                        .update_current_session_provider_response_headers_from(
                            &ctx.user.username,
                            &user_session.token,
                            session_identity,
                            &stream_details.provider_session_headers,
                            cookie_origin,
                        )
                        .await;
                } else {
                    active_users
                        .update_session_provider_response_headers_from(
                            &ctx.user.username,
                            &user_session.token,
                            &stream_details.provider_session_headers,
                            cookie_origin,
                        )
                        .await;
                }
            }
        }
        app_state.active_users.update_session_addr(&ctx.user.username, &user_session.token, &fingerprint.addr).await;
        stream_channel.shared = share_stream;
        let socket_bound = user_session.socket_bound;
        let stream = match create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user: ctx.user,
            connection_permission,
            connection_kind: user_session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            fingerprint,
            stream_channel,
            socket_bound,
            session_token: Some(&user_session.token),
            req_headers: ctx.req_headers,
            meter_uid: metering.meter_uid,
            meter_stream: metering.meter_stream,
        })
        .await
        {
            Ok(stream) => stream,
            Err(error) => {
                app_state
                    .active_users
                    .release_unbound_session_reservation(&ctx.user.username, &user_session.token, None, false)
                    .await;
                return stream_admission_rejected_response(error, &ctx.user.username);
            }
        };

        if let Some(provider) = selected_provider.as_deref() {
            let session_url = selected_request_url.as_deref().unwrap_or(user_session.stream_url.as_ref());
            app_state
                .active_users
                .create_user_session(crate::api::model::CreateUserSessionParams {
                    user: ctx.user,
                    session_token: &user_session.token,
                    virtual_id: user_session.virtual_id,
                    provider,
                    stream_url: session_url,
                    addr: &fingerprint.addr,
                    connection_permission,
                    connection_kind: user_session.connection_kind,
                    socket_bound: user_session.socket_bound,
                })
                .await;
            if !selected_provider_headers.is_empty() {
                let cookie_origin =
                    provider_response.as_ref().and_then(|(_, _, url, _)| url.as_ref()).map_or(session_url, Url::as_str);
                app_state
                    .active_users
                    .update_session_provider_response_headers_from(
                        &ctx.user.username,
                        &user_session.token,
                        &selected_provider_headers,
                        cookie_origin,
                    )
                    .await;
            }
        }

        let (status_code, header_map) = get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
        let mut response = axum::response::Response::builder().status(status_code);
        for (key, value) in &header_map {
            response = response.header(key, value);
        }

        let body_stream = prepare_body_stream(app_state, item_type, stream);
        debug_if_enabled!(
            "Streaming provider forced stream request from {}",
            sanitize_sensitive_info(
                resolve_request_url_for_logging(ctx.input, user_session.stream_url.as_ref()).as_ref()
            )
        );
        let mut response = try_unwrap_body!(response.body(body_stream));
        mark_response_as_uncompressed(&mut response);
        return response;
    }

    app_state.connection_manager.release_managed_provider_handle(stream_details.provider_handle);
    app_state
        .active_users
        .release_unbound_session_reservation(&ctx.user.username, &user_session.token, None, false)
        .await;
    if let (Some(stream), _stream_info) =
        create_channel_unavailable_stream(&app_state.app_config, &[], StatusCode::SERVICE_UNAVAILABLE)
    {
        app_state
            .connection_manager
            .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::ChannelUnavailable)
            .await;
        debug!("Streaming custom stream");
        let mut response = try_unwrap_body!(axum::response::Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from_stream(stream)));
        mark_response_as_uncompressed(&mut response);
        response
    } else {
        no_custom_video_fallback_status(&app_state.app_config).into_response()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    session_token: &str,
    request_class: Option<PlaybackRequestClass>,
    stream_channel: StreamChannel,
    stream_url: &str,
    pinned_provider: Option<&Arc<str>>,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    target: &Arc<ConfigTarget>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    allow_exhausted_shared_reconnect: bool,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> axum::response::Response {
    stream_response_with_provider_handle(
        fingerprint,
        app_state,
        session_token,
        request_class,
        stream_channel,
        stream_url,
        pinned_provider,
        req_headers,
        input,
        target,
        user,
        connection_permission,
        connection_kind,
        allow_exhausted_shared_reconnect,
        grace_mode,
        None,
    )
    .await
    .into_response()
}

/// Reconnect window of an HLS playback lease: archive playback keeps the catchup window.
pub(crate) fn get_hls_playback_ttl_secs(app_state: &Arc<AppState>, kind: PlaybackKind) -> u64 {
    if kind == PlaybackKind::Catchup {
        get_catchup_session_ttl_secs(app_state)
    } else {
        get_hls_session_ttl_secs(app_state)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn try_shared_stream_response_if_any(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    connect_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    mut stream_channel: StreamChannel,
    session_token: &str,
    req_headers: &HeaderMap,
    placeholder_transition_version: Option<u64>,
) -> Option<impl IntoResponse> {
    let subscriber_id =
        tuliprox_core::model::SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let shared_subscription = SharedStreamManager::subscribe_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        &fingerprint.addr,
        subscriber_id,
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
    )
    .await;
    let (stream, provider, cleanup_capability) = match shared_subscription {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return None,
        Err(reason) => {
            app_state
                .active_users
                .release_unbound_session_reservation(
                    &user.username,
                    session_token,
                    placeholder_transition_version,
                    placeholder_transition_version.is_some(),
                )
                .await;
            return Some(stream_admission_rejected_response(reason.into(), &user.username));
        }
    };
    debug_if_enabled!("Using shared stream {}", sanitize_sensitive_info(stream_url));
    if let Some(headers) = app_state.shared_stream_manager.get_shared_state_headers(stream_url).await {
        let (status_code, header_map) = get_stream_response_with_headers(Some((headers.clone(), StatusCode::OK)));
        let mut grace_period_options = app_state.get_grace_options();
        if connect_permission != UserConnectionPermission::GracePeriod {
            grace_period_options.period_millis = 0;
        }
        let mut stream_details = StreamDetails::from_stream(stream, grace_period_options);
        stream_details.shared_subscriber_id = Some(cleanup_capability);

        stream_details.provider_name = provider;
        let socket_bound =
            is_socket_bound_playback_session(stream_channel.item_type, extract_extension_from_url(stream_url));
        // A shared origin may have no provider allocation (e.g. a local/unknown owner), in
        // which case `provider` is `None`. The subscriber must still be tracked as a user
        // session, so fall back to a stable placeholder name rather than skipping session
        // creation; `is_over_limit` on an unknown provider is a no-op.
        let provider_name = stream_details.provider_name.as_deref().unwrap_or("shared");
        let _ = app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id: stream_channel.virtual_id,
                provider: provider_name,
                stream_url,
                addr: &fingerprint.addr,
                connection_permission: connect_permission,
                connection_kind: Some(connection_kind),
                socket_bound,
            })
            .await;
        stream_channel.shared = true;
        stream_channel.shared_joined_existing = Some(true);
        // Joining an existing origin reuses its meter identity. When metrics are disabled
        // no meter exists to reserve; reserving one here would leave an ownerless entry that
        // teardown cannot match and remove.
        let (meter_uid, pending_registration) = if is_stream_metrics_enabled(app_state) {
            app_state
                .shared_stream_manager
                .reserve_meter_uid(stream_url, || app_state.connection_manager.next_stream_uid())
        } else {
            (0, None)
        };
        stream_channel.shared_stream_id = Some(u64::from(meter_uid));
        // The pending registration is carried through admission so its drop guard rolls
        // the reservation back if the client stream is never created.
        let mut metering =
            StreamMeteringConfig { meter_uid, meter_stream: false, pending_shared_registration: pending_registration };
        let stream = match create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user,
            connection_permission: connect_permission,
            connection_kind,
            fingerprint,
            stream_channel,
            socket_bound,
            session_token: Some(session_token),
            req_headers,
            meter_uid: metering.meter_uid,
            meter_stream: metering.meter_stream,
        })
        .await
        {
            Ok(stream) => {
                metering.commit_shared_registration();
                stream.boxed()
            }
            Err(error) => {
                app_state
                    .active_users
                    .release_unbound_session_reservation(
                        &user.username,
                        session_token,
                        placeholder_transition_version,
                        placeholder_transition_version.is_some(),
                    )
                    .await;
                return Some(stream_admission_rejected_response(error, &user.username));
            }
        };
        let mut response = axum::response::Response::builder().status(status_code);
        for (key, value) in &header_map {
            response = response.header(key, value);
        }
        let mut response = response.body(axum::body::Body::from_stream(stream)).ok()?;
        mark_response_as_uncompressed(&mut response);
        return Some(response);
    }
    None
}

pub fn is_stream_share_enabled(item_type: PlaylistItemType, target: &ConfigTarget) -> bool {
    (item_type == PlaylistItemType::Live/* || item_type == PlaylistItemType::LiveHls */)
        && target.options.as_ref().is_some_and(ConfigTargetOptions::share_live_mpeg_ts_enabled)
}

pub fn is_hls_stream_share_enabled(target: &ConfigTarget) -> bool {
    target.options.as_ref().is_some_and(ConfigTargetOptions::share_live_hls_enabled)
}

impl ResourceFetchPolicy {
    pub(super) const fn cache_key(self, resource_url: &str) -> Option<&str> {
        match self {
            Self::Public => Some(resource_url),
            Self::NonPublic => None,
        }
    }

    /// Whether an upstream response may be relayed to the client.
    ///
    /// A public destination was chosen by the user's own configuration, so what it answered is what
    /// the client asked for. A resource proxy whose destination is deliberately hidden from the client
    /// must not become a reader for that destination: status, headers and body of an upstream error can
    /// disclose more about the internal service than the link itself.
    pub(super) const fn relays_upstream_response(self) -> bool { matches!(self, Self::Public) }
}

pub fn is_seek_request(cluster: XtreamCluster, req_headers: &HeaderMap) -> bool {
    // seek only for non-live streams
    if cluster == XtreamCluster::Live {
        return false;
    }

    // seek requests contains range header
    let range = req_headers.get("range").and_then(|h| h.to_str().ok()).map(ToString::to_string);

    if let Some(range) = range {
        if range.starts_with("bytes=") {
            return true;
        }
    }
    false
}

pub fn is_seekable_media_request(cluster: XtreamCluster, req_headers: &HeaderMap, extension: Option<&str>) -> bool {
    !extension.is_some_and(|ext| ext.eq_ignore_ascii_case(HLS_EXT)) && is_seek_request(cluster, req_headers)
}

pub(crate) fn should_allow_exhausted_shared_reconnect(
    share_stream: bool,
    user_session: Option<&UserSession>,
    requested_virtual_id: u32,
    requested_stream_url: &str,
) -> bool {
    share_stream
        && user_session.is_some_and(|session| {
            session.permission != UserConnectionPermission::Exhausted
                && session.virtual_id == requested_virtual_id
                && session.stream_url.as_ref() == requested_stream_url
        })
}
