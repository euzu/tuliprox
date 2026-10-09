use super::{
    classify_playback_request, resolve_admission_with_strategies, AdmissionRequest, EvictionReentryGuard,
    PlaybackRequestClass, PlaybackRequestFacts,
};
use crate::{
    api::model::{AppState, PendingProviderReason},
    auth::Fingerprint,
    model::{ConfigInput, ProxyUserCredentials},
};
use shared::{
    model::{PlaylistItemType, UserConnectionPermission, VirtualId},
    utils::current_time_secs,
};
use std::{net::SocketAddr, sync::Arc};

pub(super) struct SessionActivationRequest<'a> {
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) input: &'a ConfigInput,
    pub(super) user: &'a ProxyUserCredentials,
    pub(super) session_token: &'a str,
    pub(super) request_class: Option<PlaybackRequestClass>,
    pub(super) virtual_id: VirtualId,
    pub(super) item_type: PlaylistItemType,
    pub(super) stream_url: &'a str,
    pub(super) connection_permission: UserConnectionPermission,
    pub(super) connection_kind: crate::api::model::ConnectionKind,
    pub(super) granted_grace_mode: Option<crate::api::model::GraceMode>,
    pub(super) socket_bound: bool,
}

pub(super) struct PlaybackActivationResult {
    pub(super) admission: crate::api::model::ConnectionAdmission,
    pub(super) grace_mode: Option<crate::api::model::GraceMode>,
    pub(super) grace_context: Option<crate::api::model::GraceResolutionContext>,
    pub(super) placeholder_transition_version: Option<u64>,
}

/// # Panics
#[allow(clippy::too_many_lines)]
pub(super) async fn activate_session_before_stream_open(
    app_state: &Arc<AppState>,
    request: SessionActivationRequest<'_>,
) -> PlaybackActivationResult {
    let SessionActivationRequest {
        fingerprint,
        input,
        user,
        session_token,
        request_class,
        virtual_id,
        item_type,
        stream_url,
        connection_permission,
        connection_kind,
        granted_grace_mode,
        socket_bound,
    } = request;
    // Classify based on current session state, not the pre-computed value.
    // If caller passes FollowUp, verify the session is still counted under the guard.
    // A stale FollowUp would bypass admission — reclassify to catch this.
    let (effective_request_class, loaded_session) = if let Some(request_class) = request_class {
        if matches!(request_class, PlaybackRequestClass::FollowUp | PlaybackRequestClass::Activate) {
            // Re-read session under the guard to ensure the counted lease is still held or acquired.
            // If it is no longer counted, classify it from the current lifecycle so
            // stale FollowUp requests cannot bypass admission.
            // If it became counted, classify it so stale Activate requests don't double count.
            let current_session =
                app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
            let classified = classify_playback_request(PlaybackRequestFacts {
                existing_session: current_session.as_ref(),
                prepare_only: false,
                terminate: false,
            });
            (classified, Some(current_session))
        } else {
            (request_class, None)
        }
    } else {
        let existing_session = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
        let classified = classify_playback_request(PlaybackRequestFacts {
            existing_session: existing_session.as_ref(),
            prepare_only: false,
            terminate: false,
        });
        (classified, Some(existing_session))
    };
    let limits_enabled = app_state.app_config.config.load().user_access_control
        && (user.max_connections > 0 || user.soft_connections > 0);
    // Prepare: session setup without admission cost. The caller handles the actual activation.
    // FollowUp: already counted, no re-admission needed.
    // GracePeriod: grace already granted, no re-evaluation needed.
    // No limits: skip admission entirely.
    // GracePeriod permission is already resolved — skip admission strategies (re-run
    // would evict the same session again). But we must still materialize the grace
    // lifecycle (PendingProvider / GraceActive) so the session state is consistent.
    if connection_permission == UserConnectionPermission::GracePeriod {
        if loaded_session.as_ref().is_none_or(Option::is_none) {
            app_state
                .active_users
                .ensure_user_session_placeholder(crate::api::model::CreateUserSessionParams {
                    user,
                    session_token,
                    virtual_id: virtual_id.get(),
                    provider: input.name.as_ref(),
                    stream_url,
                    addr: &fingerprint.addr,
                    connection_permission,
                    connection_kind: Some(connection_kind),
                    socket_bound,
                })
                .await;
        }
        // Materialize grace lifecycle under the guard so the session state is consistent.
        let current_session = match loaded_session {
            Some(Some(session)) => Some(session),
            _ => app_state.active_users.get_and_update_user_session(&user.username, session_token).await,
        };
        let (_, resolved_grace) = match current_session.as_ref().map(|s| &s.lifecycle) {
            Some(crate::api::model::PlaybackLifecycle::PendingProvider { .. }) => {
                // Session already in PendingProvider — refresh deadline.
                let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                let _ = app_state
                    .active_users
                    .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                    .await;
                (
                    crate::api::model::PlaybackLifecycle::PendingProvider {
                        data: crate::api::model::PendingProviderState {
                            reason_code: PendingProviderReason::GraceHold,
                            created_at: current_time_secs(),
                            deadline,
                            version: current_session.as_ref().map_or(0, |s| {
                                if let crate::api::model::PlaybackLifecycle::PendingProvider { data } = &s.lifecycle {
                                    data.version
                                } else {
                                    0
                                }
                            }),
                            wake_source: None,
                        },
                    },
                    Some(crate::api::model::GraceMode::Hold),
                )
            }
            Some(crate::api::model::PlaybackLifecycle::GraceActive) => {
                (crate::api::model::PlaybackLifecycle::GraceActive, Some(crate::api::model::GraceMode::Instant))
            }
            _ => {
                let hold_stream = granted_grace_mode.map_or_else(
                    || item_type.is_live() || item_type.is_live_adaptive(),
                    |mode| matches!(mode, crate::api::model::GraceMode::Hold),
                );
                if hold_stream {
                    let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                    let _ = app_state
                        .active_users
                        .mark_pending_provider(
                            &user.username,
                            session_token,
                            PendingProviderReason::GraceHold,
                            deadline,
                        )
                        .await;
                    (
                        crate::api::model::PlaybackLifecycle::PendingProvider {
                            data: crate::api::model::PendingProviderState {
                                reason_code: PendingProviderReason::GraceHold,
                                created_at: current_time_secs(),
                                deadline,
                                version: 1,
                                wake_source: None,
                            },
                        },
                        Some(crate::api::model::GraceMode::Hold),
                    )
                } else {
                    app_state.active_users.mark_grace_active(&user.username, session_token).await;
                    (crate::api::model::PlaybackLifecycle::GraceActive, Some(crate::api::model::GraceMode::Instant))
                }
            }
        };
        return PlaybackActivationResult {
            admission: crate::api::model::ConnectionAdmission::from_permission(
                connection_permission,
                Some(connection_kind),
            ),
            grace_mode: resolved_grace,
            grace_context: None,
            placeholder_transition_version: None,
        };
    }
    // No limits: skip admission entirely. FollowUp / Prepare: no re-admission needed.
    if !limits_enabled
        || effective_request_class == PlaybackRequestClass::FollowUp
        || effective_request_class == PlaybackRequestClass::Prepare
    {
        return PlaybackActivationResult {
            admission: crate::api::model::ConnectionAdmission::from_permission(
                connection_permission,
                Some(connection_kind),
            ),
            grace_mode: None,
            grace_context: None,
            placeholder_transition_version: None,
        };
    }

    let placeholder_transition_version = Some(
        app_state
            .active_users
            .ensure_user_session_placeholder(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id: virtual_id.get(),
                provider: input.name.as_ref(),
                stream_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(connection_kind),
                socket_bound,
            })
            .await,
    );

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some(session_token),
            activate_unbound_session: true,
            eviction_reentry_guard: if socket_bound {
                EvictionReentryGuard::SocketPlayback { virtual_id }
            } else {
                EvictionReentryGuard::Session(session_token)
            },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;
    let grace_context = result.grace_context;

    if admission.permission() == UserConnectionPermission::GracePeriod {
        if matches!(grace_mode, Some(crate::api::model::GraceMode::Hold)) {
            // Hold: session waits for provider slot. Does not count until provider is acquired.
            let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
            let _ = app_state
                .active_users
                .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                .await;
        } else if matches!(grace_mode, Some(crate::api::model::GraceMode::Instant)) {
            // Instant: session is provisionally active immediately. Counts against admission limits
            // until the grace window resolves (success -> Active, failure -> Expired).
            app_state.active_users.mark_grace_active(&user.username, session_token).await;
        }
    }

    PlaybackActivationResult { admission, grace_mode, grace_context, placeholder_transition_version }
}

/// Session and account binding a forced resource request must still belong to after acquiring capacity.
#[derive(Clone, Copy)]
pub(super) struct CurrentSessionGuard<'a> {
    pub(super) username: &'a str,
    pub(super) token: &'a str,
    pub(super) identity: tuliprox_session::SessionIdentity,
}

pub(super) async fn cleanup_forced_reopen_addrs(
    app_state: &Arc<AppState>,
    session_owner: &str,
    cleanup_addrs: &[SocketAddr],
) {
    app_state.active_provider.release_playback_connections_await(session_owner, cleanup_addrs).await;
}
