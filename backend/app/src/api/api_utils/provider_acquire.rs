use super::{
    activate_session_before_stream_open, cleanup_failed_detected_catchup_hls, connection_priority_for_kind,
    create_stream_response_details, detected_catchup_hls_response, get_session_reservation_ttl_secs,
    get_stream_options, is_socket_bound_playback_session, is_stream_share_enabled, mark_response_as_uncompressed,
    no_custom_video_fallback_status, prepare_body_stream, prepare_stream_metering, probe_catchup_payload,
    record_connect_failed_attempt, reentry_suppressed_response, resolve_request_url_for_logging,
    resolve_xtream_vod_provider_url, select_provider_stream_url, should_pin_provider_for_session,
    stream_admission_rejected_response, try_shared_stream_response_if_any, try_unwrap_body, CatchupPayload,
    ConnectFailedAttempt, DetectedCatchupHlsResponseParams, ExactProviderAcquire, PlaybackRequestClass,
    SessionActivationRequest, StreamResponseMode, StreamingAcquireOptions, LEASE_EXPIRY_RECHECK_FLOOR,
};
use crate::{
    api::model::{
        create_active_client_stream, create_channel_unavailable_stream, create_custom_video_stream_response,
        create_provider_connections_exhausted_stream, get_stream_response_with_headers, AppState, BoxedProviderStream,
        CustomVideoStreamType, PlaybackLeaseRef, ProviderAllocation, ProviderStreamCustomReason, ProviderStreamInfo,
        ProviderStreamState, SharedStreamCtx, SharedStreamManager, StreamAdmissionError, StreamingStrategy,
    },
    auth::Fingerprint,
    model::{AppConfig, ConfigInput, ConfigTarget, PlaybackKind, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::{debug, error, log_enabled, trace};
use shared::{
    model::{ConnectFailureReason, FailureStage, PlaylistItemType, StreamChannel, UserConnectionPermission, VirtualId},
    utils::{extract_extension_from_url, sanitize_sensitive_info, Internable},
};
use std::{borrow::Cow, sync::Arc, time::Duration};
use tuliprox_session::ProviderSessionHeaders;

pub(super) fn create_unmapped_provider_stream(app_config: &AppConfig) -> ProviderStreamState {
    ProviderStreamState::Custom {
        response: create_channel_unavailable_stream(app_config, &[], StatusCode::OK),
        reason: ProviderStreamCustomReason::UnmappedProviderUrl,
    }
}

/// Splits a provider open into stream, response info and session headers. An upstream error
/// of a finite HLS resource carries no stream, only its status and forwarded headers.
pub(super) fn split_provider_stream_open(
    open: crate::api::model::ProviderStreamOpen,
) -> (Option<BoxedProviderStream>, ProviderStreamInfo, ProviderSessionHeaders) {
    match open {
        crate::api::model::ProviderStreamOpen::Stream(response) => {
            (Some(response.stream), response.info, response.provider_session_headers)
        }
        crate::api::model::ProviderStreamOpen::UpstreamStatus { status, headers } => {
            (None, Some((headers, status, None, None)), ProviderSessionHeaders::default())
        }
    }
}

/// Acquires a slot on exactly `request.provider`, waiting at most `capacity_wait` for one to
/// free up. Only slot and lease releases of this provider wake the waiter; reserved capacity
/// held by idle leases is retried when the next lease expires.
pub(crate) async fn acquire_exact_provider_handle(
    app_state: &Arc<AppState>,
    request: &ExactProviderAcquire<'_>,
    capacity_wait: Option<Duration>,
) -> Option<crate::api::model::ProviderHandle> {
    let deadline = capacity_wait.map(|wait| tokio::time::Instant::now() + wait);
    let acquire = || {
        app_state.active_provider.acquire_exact_connection_with_lease_for_session_until(
            request.provider,
            request.addr,
            request.allow_grace,
            request.priority,
            request.kind,
            request.lease,
            deadline,
        )
    };
    let Some(deadline) = deadline else {
        return acquire().await;
    };
    let capacity_notify = app_state.active_provider.provider_capacity_notify(request.provider);
    loop {
        let notified = capacity_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if app_state.connection_manager.is_shutting_down() || tokio::time::Instant::now() >= deadline {
            return None;
        }
        if let Some(handle) = acquire().await {
            return Some(handle);
        }
        let earliest_retry = tokio::time::Instant::now() + LEASE_EXPIRY_RECHECK_FLOOR;
        let wake_at = app_state
            .active_provider
            .next_lease_expiry()
            .map_or(deadline, |expiry| expiry.max(earliest_retry).min(deadline));
        tokio::select! {
            () = tokio::time::sleep_until(wake_at) => {},
            () = &mut notified => {},
        }
    }
}

/// Waits for finite resources to release capacity without superseding an active body.
pub(super) async fn acquire_exact_stream_provider_handle(
    app_state: &Arc<AppState>,
    provider: &Arc<str>,
    fingerprint: &Fingerprint,
    options: &StreamingAcquireOptions<'_>,
) -> Option<crate::api::model::ProviderHandle> {
    let request = ExactProviderAcquire {
        provider,
        addr: &fingerprint.addr,
        allow_grace: options.allow_provider_grace,
        priority: options.user_priority,
        kind: options.connection_kind,
        lease: options.session_owner.map(|owner| PlaybackLeaseRef::new(owner, options.playback_kind)),
    };
    acquire_exact_provider_handle(app_state, &request, options.capacity_wait_timeout).await
}

pub(super) async fn acquire_stream_provider_handle(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    fingerprint: &Fingerprint,
    options: &StreamingAcquireOptions<'_>,
) -> Option<tuliprox_session::ManagedProviderHandle> {
    let lease = options.session_owner.map(|owner| PlaybackLeaseRef::new(owner, options.playback_kind));
    let managed = |handle| tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), handle);
    match options.force_provider {
        Some(provider) => {
            // First try to stay on the exact pinned provider account without over-allocating.
            if let Some(handle) = acquire_exact_stream_provider_handle(app_state, provider, fingerprint, options).await
            {
                Some(managed(handle))
            } else if options.allow_forced_provider_fallback {
                debug_if_enabled!(
                    "Pinned provider {} unavailable for {}; falling back to lineup allocation",
                    sanitize_sensitive_info(provider),
                    sanitize_sensitive_info(&fingerprint.addr.to_string())
                );
                app_state
                    .active_provider
                    .acquire_connection_with_lease_for_session_await(
                        &input.name,
                        &fingerprint.addr,
                        options.allow_provider_grace,
                        options.user_priority,
                        options.connection_kind,
                        lease,
                    )
                    .await
                    .map(managed)
            } else {
                debug_if_enabled!(
                    "Pinned provider {} unavailable for {}; strict provider affinity prevents fallback",
                    sanitize_sensitive_info(provider),
                    sanitize_sensitive_info(&fingerprint.addr.to_string())
                );
                None
            }
        }
        None => app_state
            .active_provider
            .acquire_connection_with_lease_for_session_await(
                &input.name,
                &fingerprint.addr,
                options.allow_provider_grace,
                options.user_priority,
                options.connection_kind,
                lease,
            )
            .await
            .map(managed),
    }
}

pub(super) fn allows_provider_pool_failover(item_type: PlaylistItemType) -> bool {
    matches!(
        item_type,
        PlaylistItemType::Video
            | PlaylistItemType::LocalVideo
            | PlaylistItemType::Series
            | PlaylistItemType::SeriesInfo
            | PlaylistItemType::LocalSeries
            | PlaylistItemType::LocalSeriesInfo
    )
}

pub(super) async fn resolve_streaming_strategy_with_provider_handle(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    input: &ConfigInput,
    options: StreamingAcquireOptions<'_>,
    stream_channel: Option<&StreamChannel>,
    preacquired_provider_handle: Option<tuliprox_session::ManagedProviderHandle>,
) -> StreamingStrategy {
    // Recording requests transfer the slot acquired by the worker into this
    // provider-body owner. Normal playback allocates here as before.
    let mut provider_connection_handle = match preacquired_provider_handle {
        Some(handle) => Some(handle),
        None => acquire_stream_provider_handle(app_state, input, fingerprint, &options).await,
    };

    // panel_api provisioning/loading is handled later in the stream creation flow

    let mut release_failed_mapping = false;
    let stream_response_params = if let Some(allocation) =
        provider_connection_handle.as_ref().and_then(|managed| managed.handle()).map(|ph| &ph.allocation)
    {
        match allocation {
            ProviderAllocation::Exhausted => {
                debug!("Provider {} is exhausted. No connections allowed.", input.name);
                let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
                ProviderStreamState::Custom { response: stream, reason: ProviderStreamCustomReason::ProviderExhausted }
            }
            ProviderAllocation::Available(ref provider_cfg) | ProviderAllocation::GracePeriod(ref provider_cfg) => {
                // If a forced/pinned provider was requested but allocation fell back to another account,
                // the session's stream_url still points to the old provider and cannot be accepted as-is;
                // it must be resolved or rewritten for the newly allocated provider account.
                let accept_requested_stream_url = (options.accept_requested_stream_url
                    || input.input_type.is_stalker())
                    && options.force_provider.is_none_or(|forced| forced.as_ref() == provider_cfg.name.as_ref());
                // Keep the URL only when it already targets the selected provider account. Hot reload can leave old
                // alias URLs in persisted playlists until the next processing run.
                let selected_stream = select_provider_stream_url(
                    stream_url,
                    input,
                    provider_cfg,
                    accept_requested_stream_url,
                    &app_state.app_config,
                )
                .await
                .or_else(|| {
                    let url = resolve_xtream_vod_provider_url(stream_url, input, provider_cfg, stream_channel?)?;
                    Some((Arc::clone(&provider_cfg.name), url))
                });
                if let Some((selected_provider_name, url)) = selected_stream {
                    debug_if_enabled!(
                        "provider session: input={} provider_cfg={} user={} allocation={} stream_url={}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&provider_cfg.name),
                        sanitize_sensitive_info(
                            provider_cfg.get_user_info().as_ref().map_or_else(|| "?", |u| u.username.as_str())
                        ),
                        allocation.short_key(),
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, &url).as_ref())
                    );

                    if matches!(allocation, ProviderAllocation::Available(_)) {
                        ProviderStreamState::Available(Some(selected_provider_name.intern()), url.intern())
                    } else {
                        ProviderStreamState::GracePeriod(Some(selected_provider_name.intern()), url.intern())
                    }
                } else {
                    debug_if_enabled!(
                        "provider session rejected: input={} provider_cfg={} allocation={} stream_url={} reason=unmapped_provider_url",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&provider_cfg.name),
                        allocation.short_key(),
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref())
                    );
                    release_failed_mapping = true;
                    create_unmapped_provider_stream(&app_state.app_config)
                }
            }
        }
    } else {
        debug!("Provider {} is exhausted. No connections allowed.", input.name);
        let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
        ProviderStreamState::Custom { response: stream, reason: ProviderStreamCustomReason::ProviderExhausted }
    };

    if release_failed_mapping {
        // The managed owner releases the provider slot synchronously on drop; no
        // task-per-drop and no lossy cleanup message.
        drop(provider_connection_handle.take());
    }

    StreamingStrategy {
        provider_handle: provider_connection_handle,
        provider_stream_state: stream_response_params,
        input_headers: Some(input.headers.clone()),
    }
}

/// # Panics
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn stream_response_with_provider_handle(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    session_token: &str,
    request_class: Option<PlaybackRequestClass>,
    mut stream_channel: StreamChannel,
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
    preacquired_provider_handle: Option<tuliprox_session::ManagedProviderHandle>,
) -> impl IntoResponse + Send {
    let _transition_guard = app_state.active_users.acquire_playback_transition(&user.username, session_token).await;
    let request_log_stream_url = resolve_request_url_for_logging(input, stream_url);
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(request_log_stream_url.as_ref()));
    }

    let virtual_id = stream_channel.virtual_id;
    let item_type = stream_channel.item_type;
    let playback_extension = extract_extension_from_url(stream_url);
    let socket_bound = is_socket_bound_playback_session(item_type, playback_extension);
    let mut connection_permission = connection_permission;
    let mut connection_kind = connection_kind;
    let activation = activate_session_before_stream_open(
        app_state,
        SessionActivationRequest {
            fingerprint,
            input,
            user,
            session_token,
            request_class,
            virtual_id: VirtualId::new(virtual_id),
            item_type,
            stream_url,
            connection_permission,
            connection_kind,
            granted_grace_mode: grace_mode,
            socket_bound,
        },
    )
    .await;
    let grace_mode = activation.grace_mode.or(grace_mode);
    connection_permission = activation.admission.permission();
    connection_kind = activation.admission.kind().unwrap_or(connection_kind);

    let allow_shared_reuse =
        connection_permission != UserConnectionPermission::Exhausted || allow_exhausted_shared_reconnect;

    let share_stream = is_stream_share_enabled(item_type, target);
    let _shared_lock = if share_stream {
        let write_lock = app_state.app_config.file_locks.write_lock_str(stream_url).await;

        if allow_shared_reuse {
            if let Some(value) = try_shared_stream_response_if_any(
                app_state,
                stream_url,
                fingerprint,
                user,
                connection_permission,
                connection_kind,
                stream_channel.clone(),
                session_token,
                req_headers,
                activation.placeholder_transition_version,
            )
            .await
            {
                return value.into_response();
            }
        }
        Some(write_lock)
    } else {
        // Opportunistic cross-target sharing: if another target already runs a shared stream
        // for the same provider URL, subscribe to it instead of opening a separate connection.
        if item_type == PlaylistItemType::Live && allow_shared_reuse {
            if let Some(value) = try_shared_stream_response_if_any(
                app_state,
                stream_url,
                fingerprint,
                user,
                connection_permission,
                connection_kind,
                stream_channel.clone(),
                session_token,
                req_headers,
                activation.placeholder_transition_version,
            )
            .await
            {
                debug_if_enabled!("Opportunistic shared stream reuse for {}", sanitize_sensitive_info(stream_url));
                return value.into_response();
            }
        }
        None
    };

    if connection_permission == UserConnectionPermission::Exhausted {
        app_state
            .active_users
            .release_unbound_session_reservation(
                &user.username,
                session_token,
                activation.placeholder_transition_version,
                activation.placeholder_transition_version.is_some(),
            )
            .await;
        if activation.admission.is_reentry_suppressed() {
            return reentry_suppressed_response();
        }
        record_connect_failed_attempt(ConnectFailedAttempt {
            app_state,
            fingerprint,
            user,
            stream_channel: stream_channel.clone(),
            provider_name: input.name.clone(),
            req_headers,
            reason: ConnectFailureReason::UserConnectionsExhausted,
            failure_stage: FailureStage::Admission,
        });
        return create_custom_video_stream_response(
            &app_state.provider_stream_ctx(),
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        )
        .into_response();
    }

    let stream_options = get_stream_options(&app_state.app_config, StreamResponseMode::Stream);
    let session_state = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
    let pinned_provider = pinned_provider.filter(|provider| {
        item_type.is_live()
            || session_state
                .as_ref()
                .is_some_and(|session| session.media_started.load(std::sync::atomic::Ordering::Acquire))
            || app_state.active_provider.should_reuse_playback_provider(session_token, provider)
    });
    let provider_session_headers =
        session_state.as_ref().and_then(|session| session.provider_session_headers_for(stream_url));
    let mut stream_details = match create_stream_response_details(
        app_state,
        &stream_options,
        stream_url,
        &user.username,
        fingerprint,
        req_headers,
        input,
        &stream_channel,
        item_type,
        if item_type == PlaylistItemType::Catchup {
            crate::api::model::ProviderContentRepresentationMode::Identity
        } else {
            crate::api::model::ProviderContentRepresentationMode::PreserveOrigin
        },
        share_stream,
        connection_permission,
        pinned_provider,
        pinned_provider.is_none() || allows_provider_pool_failover(item_type),
        true,
        VirtualId::new(stream_channel.virtual_id),
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
        false,
        Some(session_token),
        provider_session_headers.as_deref(),
        pinned_provider.is_some(),
        grace_mode.map(|m| matches!(m, crate::api::model::GraceMode::Hold)),
        activation.grace_context.clone(),
        None,
        preacquired_provider_handle,
    )
    .await
    {
        Ok(stream_details) => stream_details,
        Err(err) => {
            app_state
                .active_users
                .release_unbound_session_reservation(
                    &user.username,
                    session_token,
                    activation.placeholder_transition_version,
                    activation.placeholder_transition_version.is_some(),
                )
                .await;
            error!("Failed to stream: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if item_type == PlaylistItemType::Catchup {
        if let Some(provider_stream) = stream_details.stream.take() {
            let probe_deadline = Duration::from_millis(app_state.hls.proxy.origin_manifest_timeout_ms().max(1));
            match probe_catchup_payload(provider_stream, probe_deadline).await {
                Ok(CatchupPayload::Direct(provider_stream)) => stream_details.stream = Some(provider_stream),
                Ok(CatchupPayload::HlsManifest(manifest)) => {
                    return detected_catchup_hls_response(DetectedCatchupHlsResponseParams {
                        app_state,
                        stream_details,
                        manifest,
                        user,
                        target,
                        input,
                        fingerprint,
                        session_token,
                        virtual_id: VirtualId::new(virtual_id),
                        connection_permission,
                        connection_kind,
                        fallback_stream_url: stream_url,
                    })
                    .await;
                }
                Err(err) => {
                    error!("Failed to inspect catch-up payload: {err}");
                    cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token)
                        .await;
                    return StatusCode::BAD_GATEWAY.into_response();
                }
            }
        }
    }

    // When no provider stream is available, still create an ActiveClientStream if a grace period
    // needs to resolve (provider-grace with hold_stream, or user-grace). The grace task will
    // determine the correct mode (UserExhausted / ProviderExhausted / Inner) and serve the
    // appropriate custom video or terminate cleanly.
    let deferred_grace_hold_stream =
        stream_details.has_deferred_provider_open() || connection_permission == UserConnectionPermission::GracePeriod;

    if stream_details.has_stream() || deferred_grace_hold_stream {
        // let content_length = get_stream_content_length(provider_response.as_ref());
        let provider_response = stream_details
            .stream_info
            .as_ref()
            .map(|(h, sc, response_url, cvt)| (h.clone(), *sc, response_url.clone(), *cvt));
        let provider_name = stream_details.provider_name.clone();
        let actual_request_url = stream_details.request_url.clone().unwrap_or_else(|| Arc::<str>::from(stream_url));
        let log_actual_request_url = resolve_request_url_for_logging(input, actual_request_url.as_ref());

        debug_if_enabled!(
            "Provider request mapping: allocated_provider={} actual_request_url={}",
            sanitize_sensitive_info(provider_name.as_deref().unwrap_or("?")),
            sanitize_sensitive_info(log_actual_request_url.as_ref())
        );

        if let Some((headers, status, _response_url, Some(CustomVideoStreamType::Provisioning))) =
            stream_details.stream_info.as_ref()
        {
            debug_if_enabled!("panel_api provisioning response to client: status={} headers={:?}", status, headers);
        }

        // Captured before `stream_details` is moved into `create_active_client_stream`.
        // The pinning rule is centralized in `should_pin_provider_for_session` so it stays
        // testable in isolation and in sync with the call site below.
        let should_pin_provider = should_pin_provider_for_session(&stream_details, app_state, item_type);
        let user_agent_stream_index = stream_details.user_agent_stream_index;
        // Persist this before response construction because some item types skip the later
        // create_user_session path, including placeholder sessions created by ensure_user_session_placeholder.
        if let Some(stream_index) = user_agent_stream_index {
            app_state
                .active_users
                .set_user_agent_stream_index_if_absent(&user.username, session_token, stream_index)
                .await;
        }

        let mut is_stream_shared = share_stream && !stream_details.has_deferred_provider_open();
        if let Some((_header, _status_code, _url, Some(_custom_video))) = stream_details.stream_info.as_ref() {
            if stream_details.stream.is_some() {
                is_stream_shared = false;
            }
        }
        let shared_subscriber_id =
            tuliprox_core::model::SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
        let mut pending_shared_cleanup = if is_stream_shared {
            match SharedStreamManager::reserve_subscriber_cleanup(
                &app_state.connection_manager,
                shared_subscriber_id,
                fingerprint.addr,
            )
            .await
            {
                Ok(cleanup) => Some(cleanup),
                Err(reason) => {
                    app_state
                        .active_users
                        .release_unbound_session_reservation(
                            &user.username,
                            session_token,
                            activation.placeholder_transition_version,
                            activation.placeholder_transition_version.is_some(),
                        )
                        .await;
                    return stream_admission_rejected_response(reason.into(), &user.username);
                }
            }
        } else {
            None
        };
        let mut metering = prepare_stream_metering(
            app_state,
            stream_url,
            is_stream_shared,
            stream_details.stream.is_some(),
            stream_details.has_deferred_provider_open(),
        );
        if let (Some(cleanup), Some(request_id)) = (
            pending_shared_cleanup.as_mut(),
            stream_details
                .provider_handle
                .as_ref()
                .and_then(|managed| managed.handle())
                .and_then(|handle| handle.playback_request_id),
        ) {
            cleanup.set_provider_request_identity(session_token, request_id);
        }
        let provider_handle = if is_stream_shared && !stream_details.has_deferred_provider_open() {
            // Transfer the ManagedProviderHandle to the shared-stream manager.
            // Ownership stays with the managed type across all awaits; the shared-stream
            // manager disarms it only once it holds the write lock and commits to a new
            // shared origin. If the future is cancelled before that point the Drop guard
            // releases the slot.
            stream_details.provider_handle.take()
        } else {
            None
        };

        stream_channel.shared = is_stream_shared;
        if is_stream_shared {
            stream_channel.shared_joined_existing = Some(false);
            stream_channel.shared_stream_id = Some(u64::from(metering.meter_uid));
        } else {
            stream_channel.shared_joined_existing = None;
            stream_channel.shared_stream_id = None;
        }
        if is_stream_shared {
            stream_details.shared_subscriber_id =
                pending_shared_cleanup.as_ref().map(tuliprox_session::PendingSharedSubscriberCleanup::capability);
        }
        // In the no-limits path there may be no placeholder yet. The body needs the
        // session's media flag before its first byte; create that session now.
        let created_media_session = if !is_stream_shared
            && !item_type.is_live()
            && item_type.requires_provider_affinity()
            && app_state.active_users.media_started_flag(&user.username, session_token).await.is_none()
        {
            if let Some(provider) = provider_name.as_deref() {
                app_state
                    .active_users
                    .ensure_user_session_placeholder(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token,
                        virtual_id,
                        provider,
                        stream_url: actual_request_url.as_ref(),
                        addr: &fingerprint.addr,
                        connection_permission,
                        connection_kind: Some(connection_kind),
                        socket_bound,
                    })
                    .await;
                true
            } else {
                false
            }
        } else {
            false
        };
        let stream = match create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user,
            connection_permission,
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
            Ok(stream) => stream,
            Err(error) => {
                if created_media_session {
                    app_state.active_users.terminate_session(&user.username, session_token).await;
                } else {
                    app_state
                        .active_users
                        .release_unbound_session_reservation(
                            &user.username,
                            session_token,
                            activation.placeholder_transition_version,
                            activation.placeholder_transition_version.is_some(),
                        )
                        .await;
                }
                return stream_admission_rejected_response(error, &user.username);
            }
        };
        let stream_resp = if is_stream_shared {
            debug_if_enabled!(
                "Streaming shared stream request from {}",
                sanitize_sensitive_info(log_actual_request_url.as_ref())
            );
            let Some(pending_shared_cleanup) = pending_shared_cleanup else {
                return stream_admission_rejected_response(StreamAdmissionError::RegistrationRejected, &user.username);
            };
            // Shared Stream response
            let shared_headers = provider_response.as_ref().map_or_else(Vec::new, |(h, _, _, _)| h.clone());
            if let Some((broadcast_stream, _shared_provider, _cleanup_capability)) =
                SharedStreamManager::register_shared_stream(
                    SharedStreamCtx {
                        app_config: &app_state.app_config,
                        shared_stream_manager: &app_state.shared_stream_manager,
                        active_provider: &app_state.active_provider,
                        connection_manager: &app_state.connection_manager,
                    },
                    stream_url,
                    stream,
                    &fingerprint.addr,
                    shared_subscriber_id,
                    shared_headers,
                    stream_options.buffer_size,
                    provider_handle,
                    pending_shared_cleanup,
                    connection_priority_for_kind(user, connection_kind),
                    connection_kind,
                )
                .await
            {
                metering.commit_shared_registration();
                let (status_code, header_map) =
                    get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
                let mut response = axum::response::Response::builder().status(status_code);
                for (key, value) in &header_map {
                    response = response.header(key, value);
                }
                let mut response = try_unwrap_body!(response.body(axum::body::Body::from_stream(broadcast_stream)));
                mark_response_as_uncompressed(&mut response);
                response
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        } else {
            // Previously, we always persisted the provider's final request URL into the session.
            // For VOD-like playback that can be the wrong thing to reuse later: a seek or reopen
            // should start from the canonical playback entrypoint, not from a provider-specific
            // redirected target that happened to be used for an earlier request.
            // For Movies/Series/Catchup we therefore keep the canonical request URL in the session.
            // That avoids "session poisoning" where later seeks/resumes inherit a non-canonical URL.
            // For live playback we still keep the redirected URL when available, because staying on
            // the chosen upstream edge/server is often desirable there.
            let session_url: Cow<'_, str> = if matches!(
                item_type,
                PlaylistItemType::Catchup
                    | PlaylistItemType::Video
                    | PlaylistItemType::LocalVideo
                    | PlaylistItemType::Series
                    | PlaylistItemType::LocalSeries
                    | PlaylistItemType::SeriesInfo
                    | PlaylistItemType::LocalSeriesInfo
            ) {
                Cow::Owned(actual_request_url.to_string())
            } else {
                provider_response
                    .as_ref()
                    .and_then(|(_, _, u, _)| u.as_ref())
                    .map_or_else(|| Cow::Owned(actual_request_url.to_string()), |url| Cow::Owned(url.to_string()))
            };
            let log_session_url = resolve_request_url_for_logging(input, session_url.as_ref());
            if log_enabled!(log::Level::Debug) {
                if log_session_url.eq(log_actual_request_url.as_ref()) {
                    debug!(
                        "Streaming stream request from {}",
                        sanitize_sensitive_info(log_actual_request_url.as_ref())
                    );
                } else {
                    debug!(
                        "Streaming stream request for {} from {}",
                        sanitize_sensitive_info(log_actual_request_url.as_ref()),
                        sanitize_sensitive_info(log_session_url.as_ref())
                    );
                }
            }
            let (status_code, header_map) =
                get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
            let mut response = axum::response::Response::builder().status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }

            if let Some(provider) = provider_name {
                if matches!(
                    item_type,
                    PlaylistItemType::LiveHls
                        | PlaylistItemType::LiveDash
                        | PlaylistItemType::Video
                        | PlaylistItemType::Series
                        | PlaylistItemType::SeriesInfo
                        | PlaylistItemType::LocalSeries
                        | PlaylistItemType::LocalSeriesInfo
                        | PlaylistItemType::Catchup
                ) {
                    let _ = app_state
                        .active_users
                        .create_user_session(crate::api::model::CreateUserSessionParams {
                            user,
                            session_token,
                            virtual_id,
                            provider: &provider,
                            stream_url: &session_url,
                            addr: &fingerprint.addr,
                            connection_permission,
                            connection_kind: Some(connection_kind),
                            socket_bound,
                        })
                        .await;
                    if let Some(stream_index) = user_agent_stream_index {
                        app_state
                            .active_users
                            .set_user_agent_stream_index_if_absent(&user.username, session_token, stream_index)
                            .await;
                    }
                    if should_pin_provider {
                        let reservation_ttl_secs = get_session_reservation_ttl_secs(app_state, item_type);
                        if reservation_ttl_secs > 0 {
                            app_state.active_provider.refresh_adaptive_playback_lease(
                                &provider,
                                session_token,
                                PlaybackKind::classify(item_type, playback_extension),
                                reservation_ttl_secs,
                            );
                        }
                    }
                }
            }

            let body_stream = prepare_body_stream(app_state, item_type, stream);
            let mut response = try_unwrap_body!(response.body(body_stream));
            mark_response_as_uncompressed(&mut response);
            response
        };

        return stream_resp.into_response();
    }
    app_state.connection_manager.release_managed_provider_handle(stream_details.provider_handle);
    app_state
        .active_users
        .release_unbound_session_reservation(
            &user.username,
            session_token,
            activation.placeholder_transition_version,
            activation.placeholder_transition_version.is_some(),
        )
        .await;
    if stream_details.custom_reason == Some(ProviderStreamCustomReason::ProviderExhausted) {
        record_connect_failed_attempt(ConnectFailedAttempt {
            app_state,
            fingerprint,
            user,
            stream_channel,
            provider_name: stream_details.provider_name.unwrap_or_else(|| input.name.clone()),
            req_headers,
            reason: ConnectFailureReason::ProviderConnectionsExhausted,
            failure_stage: FailureStage::Admission,
        });
        return create_custom_video_stream_response(
            &app_state.provider_stream_ctx(),
            &fingerprint.addr,
            CustomVideoStreamType::ProviderConnectionsExhausted,
        )
        .into_response();
    }
    no_custom_video_fallback_status(&app_state.app_config).into_response()
}
