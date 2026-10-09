use super::{
    clear_hls_provisioning_handoff_consumer, duration_to_millis_saturating,
    effective_hls_url_failover_provider_for_fetch_url, hls_access_lease_timing_for_session, hls_access_lease_ttl_ms,
    hls_availability_reevaluation_registration_failure_response, hls_cache_configured,
    hls_canonical_owner_registration, hls_canonical_owner_request_deadline_ms, hls_canonical_retry_after_response,
    hls_canonical_status_response, hls_effective_origin_acquire_policy,
    hls_origin_account_reservation_ttl_secs_for_session, hls_pending_bootstrap_window_ms,
    hls_provider_connections_exhausted_manifest_resolution, hls_runtime_or_standalone_custom_tail_response,
    hls_unpublished_lease_channel_unavailable_response, join_hls_canonical_manifest_owner,
    latest_shared_hls_manifest_rendered_at_ms, mark_hls_authorized_manifest_access,
    maybe_mark_hls_provisioning_handoff_for_canonical_manifest, prepare_hls_origin_runtime,
    trigger_hls_canonical_manifest_refresh, try_hls_cached_manifest_response, HlsCacheManifestOrigin,
    HlsCanonicalOwnerHandoffContext, HlsCanonicalOwnerRegistration, HlsManifestRefreshOrdering,
    HlsOriginRuntimeAcquireError, HlsOriginRuntimeNoAccountReason, HlsOriginWorkKind, HlsProviderExhaustedResolution,
    HlsRuntimeBandwidthLearningContext, PreparedHlsOriginRuntime,
};
use crate::{api::model::AppState, auth::Fingerprint, model::ConfigInputFlags, utils::request};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::debug;
use shared::utils::sanitize_sensitive_info;
use std::{sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    build_proxy_session_id, hls_cached_manifest_options_for_requirement, hls_manifest_acceptance_directive_for_session,
    hls_manifest_commit_requirement, register_hls_availability_reevaluation, safe_hls_access_lease_id,
    safe_proxy_session_id, safe_session_key, HlsAccessContext, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
    HlsAccessLeaseState, HlsAccessLeaseTouch, HlsAccountBindingProtection, HlsCachedManifestOptions,
    HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome, HlsManifestCommitRequirement,
    HlsOriginIoContext, HlsOriginWorkClass, HlsPostRefreshRuntime, HlsRuntimeCustomTailReason, HlsSessionHandle,
    HlsSessionStoreOutcome, LiveHlsOriginEntry, OriginRefreshRequest, ProxySessionId, RetryPolicy,
};

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn prepare_hls_canonical_manifest_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    context: &HlsAccessContext,
    origin: &HlsCacheManifestOrigin<'_>,
    path_proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    fingerprint: &Fingerprint,
    server_path: Option<&str>,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, Box<axum::response::Response>> {
    let mut allow_grace_hold = true;
    loop {
        let origin_policy = hls_effective_origin_acquire_policy(session).await;
        match prepare_hls_origin_runtime(
            app_state,
            session,
            origin.input,
            origin.raw_request_url,
            origin.session_entry_url.as_str(),
            path_proxy_session_id,
            fingerprint,
            origin_policy.connection_kind,
            origin_policy.priority,
            HlsOriginWorkKind::Manifest,
            HlsOriginWorkClass::ManifestInteractive,
            now_ms,
        )
        .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: HlsOriginRuntimeNoAccountReason::OriginBindingPreempted,
            }) => {
                return Err(Box::new(
                    hls_runtime_or_standalone_custom_tail_response(
                        app_state,
                        session,
                        path_proxy_session_id,
                        access_lease_id,
                        HlsRuntimeCustomTailReason::LowPriorityPreempted,
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                    .await,
                ));
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted,
            }) => {
                let strip = app_state.hls.proxy.strip();
                match hls_provider_connections_exhausted_manifest_resolution(
                    app_state,
                    session,
                    &context.username,
                    origin.input,
                    context.virtual_id,
                    access_lease_id,
                    access_lease_state,
                    &strip,
                    server_path,
                    allow_grace_hold,
                )
                .await
                {
                    HlsProviderExhaustedResolution::RetryAcquire => {
                        allow_grace_hold = false;
                    }
                    HlsProviderExhaustedResolution::Response(response) => return Err(Box::new(response)),
                }
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(Box::new(hls_canonical_status_response(status)))
            }
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn apply_hls_user_agent_stream_index(
    session: &HlsSessionHandle,
    headers: &mut HeaderMap,
    enabled: bool,
    active_users: &crate::api::model::ActiveUserManager,
) {
    if !enabled {
        return;
    }
    let stream_index = {
        let mut session = session.write().await;
        if session.user_agent_stream_index.is_none() {
            session.user_agent_stream_index = Some(active_users.next_user_agent_stream_index());
        }
        session.user_agent_stream_index
    };
    if let Some(stream_index) = stream_index {
        request::append_user_agent_stream_index(headers, stream_index);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn try_hls_cache_canonical_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    context: &HlsAccessContext,
    path_proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    origin: HlsCacheManifestOrigin<'_>,
    mut headers: HeaderMap,
    server_path: Option<&str>,
    _original_hls_entry_path: &str,
    refresh_ordering: HlsManifestRefreshOrdering,
) -> Option<axum::response::Response> {
    if !hls_cache_configured(app_state) {
        return None;
    }
    if origin.origin_source.input_id != context.input_id || origin.origin_source.stream_ref != context.stream_ref {
        return Some(StatusCode::NOT_FOUND.into_response());
    }

    let session_key = origin.origin_source.session_key();
    let expected_proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    if &expected_proxy_session_id != path_proxy_session_id {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    let now_ms = current_time_millis();
    let rewrite_secret = app_state.get_encrypt_secret();
    let (session, session_outcome) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            origin.origin_source.clone(),
            &rewrite_secret,
            now_ms,
        )
        .await;
    if access_lease_state == HlsAccessLeaseState::Activated {
        let timing = hls_access_lease_timing_for_session(app_state, &session).await;
        match app_state
            .hls
            .proxy
            .touch_manifest_access_lease(
                access_lease_id,
                path_proxy_session_id,
                now_ms,
                Some(timing),
                None,
                hls_access_lease_ttl_ms(app_state),
            )
            .await
        {
            HlsAccessLeaseTouch::Touched { .. } => {}
            HlsAccessLeaseTouch::Denied => {
                return Some(
                    hls_runtime_or_standalone_custom_tail_response(
                        app_state,
                        &session,
                        path_proxy_session_id,
                        access_lease_id,
                        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                        StatusCode::FORBIDDEN,
                    )
                    .await,
                );
            }
            HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
                return Some(StatusCode::NOT_FOUND.into_response());
            }
        }
    }
    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            path_proxy_session_id,
            now_ms,
        )
        .await;
    let prepared_origin = match prepare_hls_canonical_manifest_origin_runtime(
        app_state,
        &session,
        context,
        &origin,
        path_proxy_session_id,
        access_lease_id,
        access_lease_state,
        fingerprint,
        server_path,
        now_ms,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return Some(*response),
    };
    apply_hls_user_agent_stream_index(
        &session,
        &mut headers,
        origin.input.has_flag(ConfigInputFlags::UserAgentStreamIndex),
        &app_state.active_users,
    )
    .await;
    let url_failover_provider = effective_hls_url_failover_provider_for_fetch_url(
        &prepared_origin.fetch_url,
        prepared_origin.url_failover_provider.clone(),
        origin.session_entry_url.url_failover_provider(),
    );
    let origin_entry = LiveHlsOriginEntry::parse_with_provider_configs(
        &prepared_origin.fetch_url,
        url_failover_provider,
        prepared_origin.runtime_provider_config.clone(),
    )?;
    {
        let mut session_guard = session.write().await;
        if session_guard.is_gc_marked_for_removal() {
            return Some(hls_canonical_retry_after_response());
        }
        if prepared_origin.origin_account_binding_to_store.is_some() {
            session_guard.replace_origin_account_binding(prepared_origin.origin_account_binding_to_store);
        }
    }
    mark_hls_authorized_manifest_access(app_state, &session, now_ms).await;
    let selected_account = session.read().await.origin_account_binding.as_ref().map_or_else(
        || "<none>".to_string(),
        |binding| sanitize_sensitive_info(binding.account_name.as_ref()).to_string(),
    );
    debug!(
        "HLS origin account selected: proxy_session={} account={}",
        safe_proxy_session_id(path_proxy_session_id),
        selected_account
    );
    let reservation_ttl_secs = hls_origin_account_reservation_ttl_secs_for_session(&session).await;
    let previous_manifest_rendered_at_ms = latest_shared_hls_manifest_rendered_at_ms(&session).await;
    let handoff_previous_rendered_at_ms = maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
        app_state,
        &session,
        origin.input,
        context.virtual_id,
        access_lease_id,
        now_ms,
    )
    .await;
    let manifest_commit_requirement =
        hls_manifest_commit_requirement(&session, session_outcome, handoff_previous_rendered_at_ms, now_ms).await;
    let hls_ctx = app_state.hls_ctx();
    let acceptance_evaluation =
        hls_manifest_acceptance_directive_for_session(&hls_ctx, &session, path_proxy_session_id).await;
    let (acceptance_directive, availability_reevaluation_owner_key) = match acceptance_evaluation {
        HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => (directive, None),
        HlsManifestAcceptanceEvaluationOutcome::StateContention { owner_key } => {
            (HlsManifestAcceptanceDirective::none(), Some(owner_key))
        }
        HlsManifestAcceptanceEvaluationOutcome::SessionSuperseded => {
            app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle);
            return Some(hls_canonical_retry_after_response());
        }
    };
    let manifest_boundary_rendered_at_ms = handoff_previous_rendered_at_ms.unwrap_or(previous_manifest_rendered_at_ms);
    let wait_timeout =
        hls_manifest_wait_timeout_for_requirement(app_state, &session, manifest_commit_requirement).await;
    let cached_manifest_options = hls_cached_manifest_options_for_requirement(
        wait_timeout,
        manifest_commit_requirement,
        manifest_boundary_rendered_at_ms,
    );
    let bandwidth_learning = match context.known_bitrate_bps {
        Some(_) => HlsRuntimeBandwidthLearningContext::Disabled,
        None => HlsRuntimeBandwidthLearningContext::Eligible(origin.input),
    };

    let origin_policy = hls_effective_origin_acquire_policy(&session).await;
    let origin_provider_session_headers = session.read().await.origin_provider_session_headers.clone();
    let mut preacquired_provider_handle = prepared_origin.preacquired_origin_account_handle;
    let mut origin_io = HlsOriginIoContext {
        ctx: hls_ctx.clone(),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::ManifestInteractive.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    };
    if availability_reevaluation_owner_key.is_none() {
        if let Some(provider_handle) = preacquired_provider_handle.take() {
            origin_io = origin_io.with_preacquired_provider_handle(provider_handle);
        }
    }

    let refresh_request = OriginRefreshRequest {
        app_config: Arc::clone(&app_state.app_config),
        session: Arc::clone(&session),
        origin_entry,
        headers,
        origin_provider_session_headers,
        client: app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: app_state.http_clients.no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        segment_cache: Arc::clone(app_state.hls.proxy.segment_cache()),
        hls_proxy: Arc::clone(&app_state.hls.proxy),
        segment_repair: Arc::clone(app_state.hls.proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls.proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls.proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls.proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: app_state.hls.proxy.manifest_recovery_burst(),
        strip: app_state.hls.proxy.strip().clone(),
        retry_policy: RetryPolicy::default(),
        reverse_proxy_rewrite_secret: rewrite_secret.to_vec(),
        transient_resource_ttl_ms: app_state.hls.proxy.transient_resource_ttl_ms(),
        manifest_commit_requirement,
        fresh_manifest_requirement_generation: None,
        acceptance_directive,
        access_lease_id: Some(access_lease_id.clone()),
        disabled_headers: app_state.get_disabled_headers(),
        now_ms,
        origin_io: Some(origin_io),
        post_refresh_runtime: Some(HlsPostRefreshRuntime { ctx: hls_ctx.downgrade() }),
    };
    let refresh_ordering = if session_outcome == HlsSessionStoreOutcome::Reused {
        refresh_ordering
    } else {
        HlsManifestRefreshOrdering::Background
    };
    if let Some(owner_key) = availability_reevaluation_owner_key {
        app_state.connection_manager.release_provider_handle(preacquired_provider_handle);
        touch_initial_manifest_access_lease_window(
            app_state,
            access_lease_id,
            path_proxy_session_id,
            access_lease_state,
            wait_timeout,
            now_ms,
        )
        .await;
        let owner_wait_lease = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(access_lease_id, path_proxy_session_id, current_time_millis())
            .await;
        let expected_lease_issued_at_ms = owner_wait_lease.as_ref().map(|lease| lease.issued_at_ms);
        let request_deadline_ms = owner_wait_lease
            .as_ref()
            .map_or(now_ms, |lease| hls_canonical_owner_request_deadline_ms(lease, wait_timeout, now_ms));
        let safe_session = {
            let session = session.read().await;
            safe_session_key(&session.key)
        };
        let registration =
            register_hls_availability_reevaluation(hls_ctx, Arc::clone(&session), owner_key, refresh_request);
        return Some(match hls_canonical_owner_registration(registration) {
            HlsCanonicalOwnerRegistration::Join(registration) => {
                let strip = app_state.hls.proxy.strip();
                join_hls_canonical_manifest_owner(
                    HlsCanonicalOwnerHandoffContext {
                        app_state,
                        proxy_session_id: path_proxy_session_id,
                        access_lease_id,
                        expected_lease_issued_at_ms,
                        strip: &strip,
                        server_path,
                        manifest_commit_requirement,
                        manifest_boundary_rendered_at_ms,
                        bandwidth_learning,
                        request_deadline_ms,
                        safe_session,
                    },
                    registration,
                )
                .await
            }
            HlsCanonicalOwnerRegistration::FailClosed(failure) => {
                hls_availability_reevaluation_registration_failure_response(failure)
            }
        });
    }
    if handoff_previous_rendered_at_ms.is_some() {
        touch_initial_manifest_access_lease_window(
            app_state,
            access_lease_id,
            path_proxy_session_id,
            access_lease_state,
            wait_timeout,
            now_ms,
        )
        .await;
        if let Some(response) = trigger_hls_canonical_manifest_refresh(
            app_state,
            &session,
            path_proxy_session_id,
            access_lease_id,
            refresh_request,
            refresh_ordering,
        )
        .await
        {
            return Some(response);
        }
        let strip = app_state.hls.proxy.strip();
        if let Some(response) = try_hls_cached_manifest_response(
            app_state,
            &session,
            access_lease_id,
            access_lease_state,
            &strip,
            server_path,
            cached_manifest_options,
            bandwidth_learning,
        )
        .await
        {
            clear_hls_provisioning_handoff_consumer(app_state, origin.input, context.virtual_id, current_time_millis());
            return Some(response);
        }
        return Some(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    match session_outcome {
        HlsSessionStoreOutcome::Created => {
            touch_initial_manifest_access_lease_window(
                app_state,
                access_lease_id,
                path_proxy_session_id,
                access_lease_state,
                wait_timeout,
                now_ms,
            )
            .await;
            if let Some(response) = trigger_hls_canonical_manifest_refresh(
                app_state,
                &session,
                path_proxy_session_id,
                access_lease_id,
                refresh_request,
                refresh_ordering,
            )
            .await
            {
                return Some(response);
            }
            let strip = app_state.hls.proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                cached_manifest_options,
                bandwidth_learning,
            )
            .await
            {
                return Some(response);
            }
        }
        HlsSessionStoreOutcome::Reused => {
            if let Some(response) = trigger_hls_canonical_manifest_refresh(
                app_state,
                &session,
                path_proxy_session_id,
                access_lease_id,
                refresh_request,
                refresh_ordering,
            )
            .await
            {
                if refresh_ordering == HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation
                    && response.status() == StatusCode::SERVICE_UNAVAILABLE
                {
                    let strip = app_state.hls.proxy.strip();
                    if let Some(live_response) = try_hls_cached_manifest_response(
                        app_state,
                        &session,
                        access_lease_id,
                        access_lease_state,
                        &strip,
                        server_path,
                        HlsCachedManifestOptions::initial(Duration::ZERO),
                        bandwidth_learning,
                    )
                    .await
                    .filter(|candidate| candidate.status() == StatusCode::OK)
                    {
                        return Some(live_response);
                    }
                }
                return Some(response);
            }
            touch_initial_manifest_access_lease_window(
                app_state,
                access_lease_id,
                path_proxy_session_id,
                access_lease_state,
                wait_timeout,
                now_ms,
            )
            .await;
            let strip = app_state.hls.proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                cached_manifest_options,
                bandwidth_learning,
            )
            .await
            {
                return Some(response);
            }
        }
    }

    Some(hls_unpublished_lease_channel_unavailable_response(app_state, path_proxy_session_id, access_lease_id).await)
}

pub(in crate::api::endpoints::hls_api) fn hls_initial_manifest_decision_wait_timeout(
    app_state: &Arc<AppState>,
) -> Duration {
    Duration::from_secs(app_state.hls.proxy.initial_manifest_wait_timeout_secs())
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_wait_timeout_for_requirement(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    requirement: HlsManifestCommitRequirement,
) -> Duration {
    match requirement {
        HlsManifestCommitRequirement::FreshCommitRequired { .. } => {
            hls_initial_manifest_decision_wait_timeout(app_state)
        }
        HlsManifestCommitRequirement::CommittedManifestAllowed => {
            hls_initial_manifest_wait_timeout(app_state, session).await
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn touch_initial_manifest_access_lease_window(
    app_state: &Arc<AppState>,
    access_lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    access_lease_state: HlsAccessLeaseState,
    wait_timeout: Duration,
    now_ms: u64,
) {
    if wait_timeout.is_zero() || access_lease_state != HlsAccessLeaseState::Pending {
        return;
    }
    let wait_timeout_ms = duration_to_millis_saturating(wait_timeout);
    let deadline_ms = now_ms.saturating_add(wait_timeout_ms.max(hls_pending_bootstrap_window_ms(app_state)));
    let touch = app_state
        .hls
        .proxy
        .touch_manifest_access_lease(
            access_lease_id,
            proxy_session_id,
            now_ms,
            None,
            Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms }),
            hls_access_lease_ttl_ms(app_state),
        )
        .await;
    let failure = match touch {
        HlsAccessLeaseTouch::Touched { .. } => return,
        HlsAccessLeaseTouch::Expired => "expired",
        HlsAccessLeaseTouch::Denied => "denied",
        HlsAccessLeaseTouch::UnknownLease => "unknown-lease",
        HlsAccessLeaseTouch::SessionMismatch => "session-mismatch",
    };
    debug!(
        "HLS initial manifest lease window not extended: lease={} proxy_session={} outcome={failure}",
        safe_hls_access_lease_id(access_lease_id),
        safe_proxy_session_id(proxy_session_id)
    );
}

pub(in crate::api::endpoints::hls_api) async fn hls_initial_manifest_wait_timeout(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> Duration {
    let session = session.read().await;
    if matches!(
        session.account_binding_protection(current_time_millis()),
        HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired
    ) {
        hls_initial_manifest_decision_wait_timeout(app_state)
    } else {
        Duration::ZERO
    }
}
