use super::{
    history::emit_connect_record, socket::SocketActivityEvent, ConnectionHistoryMode, ConnectionManager,
    ConnectionParams, ConnectionRejectionReason, OwnedRequestCleanup, RegisteredPlaybackRequest,
    SharedCleanupCapability, CLEANUP_ADMISSION_TIMEOUT,
};
use crate::{uses_direct_body_idle_timeout, ActiveUserConnectionParams};
use log::warn;
use shared::{
    model::{ActiveUserConnectionChange, CustomVideoStreamType, EventMessage, StreamInfo},
    utils::sanitize_sensitive_info,
};
use std::{net::SocketAddr, sync::Arc};
use tuliprox_core::utils::debug_if_enabled;

impl ConnectionManager {
    pub async fn add_connection(&self, addr: &SocketAddr) { self.user_manager.add_connection(addr).await; }

    pub async fn touch_http_activity(&self, username: &str, token: &str, addr: &SocketAddr) {
        self.user_manager.touch_http_activity(username, token, addr).await;
        self.socket_activity_tracker.track(SocketActivityEvent::HttpActivity { addr: *addr });
    }

    pub fn touch_direct_body_activity(&self, addr: &SocketAddr) {
        self.socket_activity_tracker.track(SocketActivityEvent::DirectBodyActivity { addr: *addr });
    }

    pub async fn update_connection(&self, update: ConnectionParams<'_>) -> Option<StreamInfo> {
        self.update_connection_with_history_mode(update, ConnectionHistoryMode::EmitConnect).await
    }

    pub async fn update_connection_with_history_mode(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
    ) -> Option<StreamInfo> {
        let uid = self.next_stream_uid();
        let mut registered = self.update_connection_with_uid(update, history_mode, uid, None).await;
        // This compatibility API has no body to hand the claim to; the caller owns the
        // registered stream, so disarm the rollback before returning its metadata.
        registered.commit();
        registered.into_display_stream()
    }

    pub async fn update_connection_with_uid(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
        uid: u32,
        provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    ) -> RegisteredPlaybackRequest {
        self.update_connection_with_uid_and_session_registration(update, history_mode, uid, provider_request_id, None)
            .await
    }

    pub async fn update_connection_with_uid_and_session_registration(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
        uid: u32,
        provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        session_registration: Option<&crate::PlaybackSessionRegistration>,
    ) -> RegisteredPlaybackRequest {
        self.update_connection_with_uid_impl(
            update,
            history_mode,
            uid,
            provider_request_id,
            false,
            session_registration,
        )
        .await
    }

    /// Registers a request whose enclosing shared-subscriber body already owns the
    /// guaranteed terminal cleanup permit for this exact stream UID.
    pub async fn update_connection_with_uid_using_shared_cleanup(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
        capability: SharedCleanupCapability,
        provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    ) -> RegisteredPlaybackRequest {
        self.update_connection_with_uid_impl(
            update,
            history_mode,
            capability.stream_uid(),
            provider_request_id,
            true,
            None,
        )
        .await
    }

    pub(super) async fn update_connection_with_uid_impl(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
        uid: u32,
        provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        cleanup_owned_by_shared_subscriber: bool,
        session_registration: Option<&crate::PlaybackSessionRegistration>,
    ) -> RegisteredPlaybackRequest {
        let username = update.username;
        let fingerprint = update.fingerprint;
        let track_direct_body_activity = uses_direct_body_idle_timeout(update.stream_channel);
        let Some(_admission) = self.begin_admission().await else {
            warn!("Connection manager is shutting down; rejecting connection registration for user {username}");
            return RegisteredPlaybackRequest::rejected(uid, ConnectionRejectionReason::CleanupReceiverClosed);
        };
        // Admission: reserve a guaranteed cleanup right before any registration mutation,
        // bounded so a saturated or stalled cleanup queue cannot hang request setup.
        let rollback_permit = if cleanup_owned_by_shared_subscriber {
            None
        } else {
            match tokio::time::timeout(CLEANUP_ADMISSION_TIMEOUT, self.cleanup_tx().reserve_owned()).await {
                Ok(Ok(permit)) => Some(permit),
                Ok(Err(_)) => {
                    warn!("Cleanup receiver closed; rejecting connection registration for user {username}");
                    return RegisteredPlaybackRequest::rejected(uid, ConnectionRejectionReason::CleanupReceiverClosed);
                }
                Err(_) => {
                    warn!("Cleanup admission timed out; rejecting connection registration for user {username}");
                    return RegisteredPlaybackRequest::rejected(
                        uid,
                        ConnectionRejectionReason::CleanupAdmissionTimeout,
                    );
                }
            }
        };
        // The cleanup owner is armed before the first mutation, so a cancellation during
        // `update_connection`, meter registration or any later await still releases the claim.
        let mut cleanup = OwnedRequestCleanup {
            addr: fingerprint.addr,
            request_uid: uid,
            provider_request_id,
            owner: update.session_token.map(Arc::<str>::from),
            permit: rollback_permit,
            finished: false,
        };
        if let Some(stream_info) = self
            .user_manager
            .update_connection_with_session_registration(
                ActiveUserConnectionParams {
                    uid,
                    meter_uid: update.meter_uid,
                    username,
                    max_connections: update.max_connections,
                    soft_connections: update.soft_connections,
                    connection_kind: update.connection_kind,
                    priority: update.priority,
                    soft_priority: update.soft_priority,
                    fingerprint,
                    provider: update.provider,
                    stream_channel: update.stream_channel,
                    user_agent: update.user_agent,
                    session_token: update.session_token,
                },
                session_registration,
            )
            .await
        {
            self.event_manager.register_meter_client(stream_info.uid, stream_info.meter_uid).await;
            if history_mode == ConnectionHistoryMode::EmitConnect {
                emit_connect_record(&self.history_writer, &stream_info);
            }
            if track_direct_body_activity {
                debug_if_enabled!(
                    "Direct body stream registered for socket expiry: {}",
                    sanitize_sensitive_info(&fingerprint.addr.to_string())
                );
                self.touch_direct_body_activity(&fingerprint.addr);
            }
            self.event_manager
                .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info.clone())));
            RegisteredPlaybackRequest::with_cleanup(uid, Some(stream_info), Some(cleanup))
        } else {
            // Registration failed: nothing was inserted, so the cleanup must not fire.
            cleanup.disarm();
            warn!("Failed to register connection for user {username} at {}; disconnecting client", fingerprint.addr);
            RegisteredPlaybackRequest::rejected(uid, ConnectionRejectionReason::RegistrationFailed)
        }
    }

    // pub fn send_active_user_stats(&self, user_count: usize, user_connection_count: usize) {
    //     self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(user_count, user_connection_count)));
    // }

    pub async fn update_stream_detail(&self, addr: &SocketAddr, video_type: CustomVideoStreamType) {
        if let Some(stream_info) = self.user_manager.update_stream_detail(addr, video_type).await {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        }
    }

    pub async fn update_stream_detail_by_uid(&self, uid: u32, video_type: CustomVideoStreamType) {
        if let Some(stream_info) = self.user_manager.update_stream_detail_by_uid(uid, video_type).await {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        }
    }
}
