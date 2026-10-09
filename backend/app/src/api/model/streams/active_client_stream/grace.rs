use super::{store_stream_mode, ActiveClientStreamState, GracePeriodParams, GraceProvisioningInfo, StreamMode};
use crate::{
    api::{
        model::{AppState, CustomVideoStreamType, PendingProviderWakeSource, StreamDetails},
        panel_api::{can_provision_on_exhausted, find_input_by_provider_name, run_panel_api_provisioning_probe},
    },
    utils::debug_if_enabled,
};
use log::{error, info};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

impl ActiveClientStreamState {
    pub(super) fn stop_grace_task(&mut self) {
        if let Some(task) = self.grace_task_handle.take() {
            task.abort();
        }
        if let Some(token) = self.provisioning_stop_signal.take() {
            token.cancel();
        }
    }

    pub(super) fn clear_finished_grace_task(&mut self) {
        if self.grace_task_handle.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
            self.grace_task_handle = None;
            // If the task finished but the flag is still GRACE_PENDING (e.g. the task
            // panicked or was cancelled before it could update the flag), reset the flag
            // to INNER_STREAM so the client stream is not hung indefinitely.
            if let Some(flag) = &self.send_custom_stream_flag {
                let _ = flag.compare_exchange(
                    StreamMode::GracePending as u8,
                    StreamMode::Inner as u8,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        }
    }
}

pub(super) fn resolve_grace_period_provisioning(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
) -> Option<GraceProvisioningInfo> {
    if stream_details.disable_provider_grace || !stream_details.provider_grace_active {
        return None;
    }
    let provider_name = stream_details.provider_name.as_deref();
    let input = provider_name.and_then(|name| find_input_by_provider_name(app_state.as_ref(), name))?;
    if !can_provision_on_exhausted(app_state, &input) {
        return None;
    }

    let stop_signal = CancellationToken::new();
    Some(GraceProvisioningInfo { input, stop_signal })
}

#[allow(clippy::too_many_lines)]
pub(super) fn stream_grace_period(
    request: GracePeriodParams,
) -> (Option<Arc<AtomicU8>>, Option<tokio::task::JoinHandle<()>>) {
    let GracePeriodParams {
        app_state,
        stream_details,
        user_grace_period,
        user,
        fingerprint,
        virtual_id,
        session_token,
        provisioning_info,
        waker,
        hold_stream,
        capacity_notify,
        pending_provider_version,
        grace_active_version,
        grace_resolution_context,
        grace_kind,
        socket_bound,
        shared_subscriber_id,
        ..
    } = request;
    let grace_period = stream_details.grace_period;
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let connection_manager = Arc::clone(&app_state.connection_manager);

    let provider_grace_check = if stream_details.provider_grace_active
        && stream_details.provider_name.is_some()
        && !stream_details.disable_provider_grace
    {
        stream_details.provider_name.clone()
    } else {
        None
    };

    let user_max_connections = user.max_connections;
    let user_grace_check = if user_grace_period && user_max_connections > 0 {
        let user_name = user.username.clone();
        Some((user_name, user_max_connections))
    } else {
        None
    };

    if provider_grace_check.is_some() || user_grace_check.is_some() {
        let stream_strategy_flag =
            Arc::new(AtomicU8::new(if hold_stream { StreamMode::GracePending as u8 } else { StreamMode::Inner as u8 }));
        let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
        let grace_period_millis = grace_period.period_millis;

        let user_manager = Arc::clone(&active_users);
        let provider_manager = Arc::clone(&active_provider);
        let connection_manager = Arc::clone(&connection_manager);
        let reconnect_flag = stream_details.reconnect_flag.clone();
        let fingerprint = fingerprint.clone();
        let app_state = Arc::clone(&app_state);
        let pending_username = user.username.clone();
        // Clone owned copies of fields borrowed from `request` so the spawn sees owned values.
        let grace_resolution_context = grace_resolution_context.clone();
        // Safety timeout: if async operations inside the grace task stall, force the flag
        // out of GRACE_PENDING so the client stream is not hung indefinitely.
        // Allow grace_period_millis for the intentional delay plus a 10-second buffer
        // for the async connection checks that follow.
        let grace_task_timeout = tokio::time::Duration::from_millis(grace_period_millis.saturating_add(10_000));
        // Clone handles for use in the timeout fallback, in case the inner async block is cancelled.
        let flag_for_fallback = Arc::clone(&stream_strategy_flag_copy);
        let waker_for_fallback = waker.clone();
        let session_token_timeout = session_token.clone();
        let active_users_timeout = Arc::clone(&active_users);
        let pending_username_timeout = pending_username.clone();
        let grace_task_handle = tokio::spawn(async move {
            let timed_out = tokio::time::timeout(grace_task_timeout, async move {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(grace_period_millis);
                let mut pending_wake_source = PendingProviderWakeSource::Activated;
                loop {
                    let capacity_wait = capacity_notify.notified();
                    tokio::pin!(capacity_wait);

                    let user_ok = match &user_grace_check {
                        Some((username, max_connections)) => {
                            user_manager.user_connections(username).await <= *max_connections
                        }
                        None => true,
                    };
                    let provider_ok = match &provider_grace_check {
                        Some(provider_name) => !provider_manager.is_over_limit(provider_name),
                        None => true,
                    };
                    if user_ok && provider_ok {
                        break;
                    }

                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {
                            pending_wake_source = PendingProviderWakeSource::Timeout;
                            break;
                        }
                        () = &mut capacity_wait => {
                            pending_wake_source = PendingProviderWakeSource::CapacityNotify;
                        }
                    }
                }

                let mut updated = false;
                if let Some((username, max_connections)) = user_grace_check {
                    let active_connections = user_manager.user_connections(&username).await;
                    if active_connections > max_connections {
                        // User-grace failed. Evaluate remaining strategies before final deny.
                        if let Some(ref ctx) = grace_resolution_context {
                            let eviction_guard = if socket_bound {
                                tuliprox_session::admission::EvictionReentryGuard::SocketPlayback { virtual_id }
                            } else {
                                tuliprox_session::admission::EvictionReentryGuard::Session(
                                    // Defensive fallback: an empty token will not match any real session.
                                    session_token.as_deref().unwrap_or_default(),
                                )
                            };
                            let remaining_result =
                                tuliprox_session::admission::evaluate_remaining_strategies_after_grace(
                                    &app_state.admission_ctx(),
                                    tuliprox_session::admission::AdmissionRequest {
                                        username: &username,
                                        max_connections,
                                        soft_connections: user.soft_connections,
                                        client_ip: &fingerprint.client_ip,
                                        request_addr: &fingerprint.addr,
                                        use_session_admission: true,
                                        session_token: session_token.as_deref(),
                                        activate_unbound_session: true,
                                        eviction_reentry_guard: eviction_guard,
                                    },
                                    ctx,
                                    grace_kind,
                                )
                                .await;
                            match remaining_result.admission.permission() {
                                shared::model::UserConnectionPermission::Allowed
                                | shared::model::UserConnectionPermission::GracePeriod => {
                                    // Remaining strategy succeeded — proceed to Inner.
                                    store_stream_mode(&stream_strategy_flag_copy, StreamMode::Inner);
                                    // updated stays false
                                }
                                shared::model::UserConnectionPermission::Exhausted => {
                                    let suppressed = remaining_result.admission.is_reentry_suppressed();
                                    if suppressed {
                                        // Every remaining eviction candidate is reentry-protected,
                                        // so this is a suppressed retry of a recently evicted
                                        // playback. End the body quietly: a visible
                                        // connections-exhausted video would be misleading because
                                        // the request was declined by the reentry guard, not by a
                                        // real connection limit.
                                        store_stream_mode(&stream_strategy_flag_copy, StreamMode::ReentrySuppressed);
                                        info!(
                                            "Suppressing reentry retry for recently evicted playback of user {username}"
                                        );
                                    } else {
                                        // Remaining strategies exhausted — final UserExhausted.
                                        store_stream_mode(&stream_strategy_flag_copy, StreamMode::UserExhausted);
                                        connection_manager
                                            .update_stream_detail(
                                                &fingerprint.addr,
                                                CustomVideoStreamType::UserConnectionsExhausted,
                                            )
                                            .await;
                                        info!("User connections exhausted for active clients: {username}");
                                    }
                                    if let Some(id) = shared_subscriber_id {
                                        connection_manager.shared_stream_manager.release_subscriber(id).await;
                                    }
                                    updated = true;
                                }
                            }
                        } else {
                            // No grace context — immediate UserExhausted.
                            store_stream_mode(&stream_strategy_flag_copy, StreamMode::UserExhausted);
                            connection_manager
                                .update_stream_detail(
                                    &fingerprint.addr,
                                    CustomVideoStreamType::UserConnectionsExhausted,
                                )
                                .await;
                            if let Some(id) = shared_subscriber_id {
                                connection_manager.shared_stream_manager.release_subscriber(id).await;
                            }
                            info!("User connections exhausted for active clients: {username}");
                            updated = true;
                        }
                    }
                }

                if !updated {
                    if let Some(provider_name) = provider_grace_check {
                        if provider_manager.is_over_limit(&provider_name) {
                            if let Some(provisioning_info) = provisioning_info {
                                store_stream_mode(&stream_strategy_flag_copy, StreamMode::Provisioning);
                                connection_manager
                                    .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::Provisioning)
                                    .await;
                                debug_if_enabled!(
                                    "Provider grace period exhausted; provisioning for active clients: {provider_name}"
                                );
                                let app_state = Arc::clone(&app_state);
                                let input_name = Arc::clone(&provisioning_info.input.name);
                                let stop_signal = provisioning_info.stop_signal;
                                let addr = fingerprint.addr;
                                tokio::spawn(async move {
                                    if let Err(err) = run_panel_api_provisioning_probe(
                                        app_state,
                                        input_name,
                                        stop_signal,
                                        addr,
                                        virtual_id,
                                    )
                                    .await
                                    {
                                        error!("Error running Probe: {err:?}");
                                    }
                                });
                            } else {
                                store_stream_mode(&stream_strategy_flag_copy, StreamMode::ProviderExhausted);
                                connection_manager
                                    .update_stream_detail(
                                        &fingerprint.addr,
                                        CustomVideoStreamType::ProviderConnectionsExhausted,
                                    )
                                    .await;
                                // Release the shared stream subscription to stop the subscriber loop
                                if let Some(id) = shared_subscriber_id {
                                    connection_manager.shared_stream_manager.release_subscriber(id).await;
                                }
                                info!("Provider connections exhausted for active clients: {provider_name}");
                            }
                            updated = true;
                        }
                    }
                }

                if !updated {
                    store_stream_mode(&stream_strategy_flag_copy, StreamMode::Inner);
                }

                // Resolve session lifecycle transitions.
                // PendingProvider (Hold): activate on success, expire on failure.
                if hold_stream {
                    if let (Some(token), Some(version)) = (session_token.as_deref(), pending_provider_version) {
                        let _transition_guard =
                            active_users.acquire_playback_transition(&pending_username, token).await;
                        if updated {
                            active_users
                                .expire_pending_provider(&pending_username, token, version, pending_wake_source)
                                .await;
                        } else {
                            active_users
                                .activate_pending_provider(&pending_username, token, version, pending_wake_source)
                                .await;
                        }
                    }
                }

                // GraceActive (Instant): activate on success, expire on failure.
                // grace_active_version is Some when the session is in `GraceActive` lifecycle.
                if let (Some(token), Some(version)) = (session_token.as_deref(), grace_active_version) {
                    let _transition_guard = active_users.acquire_playback_transition(&pending_username, token).await;
                    if updated {
                        active_users.expire_grace_active(&pending_username, token, version).await;
                    } else {
                        active_users.activate_grace_active(&pending_username, token, version).await;
                    }
                }

                if updated {
                    if let Some(flag) = reconnect_flag {
                        flag.cancel();
                    }
                }

                if let Some(w) = waker.as_ref() {
                    w.wake();
                }
            })
            .await;

            if timed_out.is_err() {
                // Grace task exceeded its budget without updating the flag — reset GRACE_PENDING
                // to INNER_STREAM so the client stream is not hung indefinitely.
                // Also resolve any session lifecycle to prevent inconsistent state:
                // a PendingProvider / GraceActive session that was never resolved would cause
                // the next admission attempt to incorrectly skip re-evaluation.
                if let (Some(token), Some(version)) = (session_token_timeout.as_deref(), pending_provider_version) {
                    let _transition_guard =
                        active_users_timeout.acquire_playback_transition(&pending_username_timeout, token).await;
                    active_users_timeout
                        .expire_pending_provider(
                            &pending_username_timeout,
                            token,
                            version,
                            PendingProviderWakeSource::Timeout,
                        )
                        .await;
                }
                if let (Some(token), Some(version)) = (session_token_timeout.as_deref(), grace_active_version) {
                    let _transition_guard =
                        active_users_timeout.acquire_playback_transition(&pending_username_timeout, token).await;
                    active_users_timeout.expire_grace_active(&pending_username_timeout, token, version).await;
                }
                error!("Grace period task timed out; resetting stream flag to prevent client hang");
                let _ = flag_for_fallback.compare_exchange(
                    StreamMode::GracePending as u8,
                    StreamMode::Inner as u8,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                if let Some(w) = waker_for_fallback.as_ref() {
                    w.wake();
                }
            }
        });
        return (Some(stream_strategy_flag), Some(grace_task_handle));
    }
    (None, None)
}
