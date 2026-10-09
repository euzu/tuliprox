use super::{
    backpressure::notify_capacity,
    history::{
        emit_disconnect_record, playback_outcome_for_reason, resolve_disconnect_reason,
        resolve_disconnect_reason_from_provider_end,
    },
    CleanupWorkerDeps, ConnectionManager, PREEMPT_REENTRY_BLOCK_SECS, PROVIDER_END_NOT_SET,
};
use crate::{ActiveProviderManager, ActiveUserManager, EventManager, SharedStreamManager};
use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use log::debug;
use shared::{
    model::{ActiveUserConnectionChange, CustomVideoStreamType, DisconnectReason, EventMessage, StreamInfo},
    utils::sanitize_sensitive_info,
};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{DisconnectQos, ProviderHandle, SharedSubscriberId},
    utils::debug_if_enabled,
};
use tuliprox_repository::StreamHistoryWriter;

pub enum CleanupEvent {
    ReleaseSharedSubscriber {
        addr: SocketAddr,
        subscriber_id: SharedSubscriberId,
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        owner: Option<Arc<str>>,
    },
    ReleaseStream {
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        /// Playback owner (session token) captured at acquire, so the provider request
        /// can be finished independently of whether the user claim still exists.
        owner: Option<Arc<str>>,
        addr: SocketAddr,
        stream_uid: Option<u32>,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    },
    ReleaseConnection {
        addr: SocketAddr,
    },
    ReleaseProviderHandle {
        handle: Option<ProviderHandle>,
    },
    ReleaseStreamAndProviderHandle {
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        addr: SocketAddr,
        stream_uid: Option<u32>,
        handle: Option<ProviderHandle>,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    },
    UpdateDetailAndReleaseProvider {
        addr: SocketAddr,
        stream_uid: Option<u32>,
        video_type: CustomVideoStreamType,
        handle: Option<ProviderHandle>,
    },
    AdaptiveSessionExpired {
        stream_info: Box<StreamInfo>,
    },
    /// Confirms that real provider media reached the client for a playback lease.
    /// Only a confirmed lease may reserve provider capacity against other playbacks.
    ConfirmPlaybackLease {
        owner: Arc<str>,
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    },
    /// Runs a deferred asynchronous cleanup task in the managed cleanup worker.
    Defer(BoxFuture<'static, ()>),
}

async fn handle_release_connection(deps: &CleanupWorkerDeps, addr: SocketAddr) {
    release_connection_parts(deps, &addr, DisconnectReason::Cleanup, true).await;
}

pub(super) async fn release_connection_with_reason(
    connection_manager: &ConnectionManager,
    addr: &SocketAddr,
    reason: DisconnectReason,
    send_shared_stop_signal: bool,
) {
    let deps = CleanupWorkerDeps {
        user_manager: Arc::clone(&connection_manager.user_manager),
        provider_manager: Arc::clone(&connection_manager.provider_manager),
        shared_stream_manager: Arc::clone(&connection_manager.shared_stream_manager),
        event_manager: Arc::clone(&connection_manager.event_manager),
        capacity_notify: Arc::clone(&connection_manager.capacity_notify),
        history_writer: Arc::clone(&connection_manager.history_writer),
    };
    release_connection_parts(&deps, addr, reason, send_shared_stop_signal).await;
}

pub(super) async fn release_connection_parts(
    deps: &CleanupWorkerDeps,
    addr: &SocketAddr,
    reason: DisconnectReason,
    send_shared_stop_signal: bool,
) {
    let removed = if matches!(reason, DisconnectReason::ClientKicked) {
        deps.user_manager.release_connection_as_kicked(addr).await
    } else {
        deps.user_manager.release_connection(addr).await
    };
    if matches!(reason, DisconnectReason::ClientKicked) {
        for stream_info in &removed.removed_streams {
            if let Some(session_token) = stream_info.session_token.as_deref() {
                deps.provider_manager.terminate_identified_playback_owner(session_token);
            }
        }
        // Explicitly terminate all sessions for the kicked addr. This expires them
        // immediately rather than leaving them for TTL-based GC cleanup.
        for username in &removed.disconnected_users {
            deps.user_manager.terminate_sessions_for_addr(username, addr).await;
        }
    }
    for stream_info in &removed.removed_streams {
        let qos = deps.event_manager.read_meter_qos(stream_info.meter_uid).await;
        let bytes_sent = qos.map(|qos| qos.bytes_total);
        let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
        deps.event_manager.unregister_meter_client(stream_info.uid).await;
        emit_disconnect_record(
            &deps.history_writer,
            stream_info,
            reason,
            &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
            None,
            None,
        );
    }
    deps.provider_manager.release_connection(addr);
    deps.shared_stream_manager.release_connection(addr, send_shared_stop_signal).await;
    if removed.addr_removed && !removed.removed_streams.is_empty() {
        deps.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
    }
    notify_capacity(deps.capacity_notify.as_ref());
}

#[allow(clippy::too_many_arguments)]
async fn handle_release_stream(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    stream_uid: Option<u32>,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
) {
    if let Some(stream_info) = release_stream_with_disconnect(
        deps,
        addr,
        stream_uid,
        provider_end_reason,
        reconnect_count,
        provider_error_class,
        provider_http_status,
        request_id,
        owner,
    )
    .await
    {
        deps.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::DisconnectedStream {
            addr: stream_info.addr,
            uid: stream_info.uid,
        }));
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

fn handle_release_provider_handle(deps: &CleanupWorkerDeps, handle: Option<ProviderHandle>) {
    if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle);
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_release_stream_and_provider_handle(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    stream_uid: Option<u32>,
    handle: Option<ProviderHandle>,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
) {
    let provider_released = if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle);
        true
    } else {
        false
    };
    let stream_released = release_stream_with_disconnect(
        deps,
        addr,
        stream_uid,
        provider_end_reason,
        reconnect_count,
        provider_error_class,
        provider_http_status,
        request_id,
        None,
    )
    .await;
    if let Some(stream_info) = stream_released.as_ref() {
        deps.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::DisconnectedStream {
            addr: stream_info.addr,
            uid: stream_info.uid,
        }));
    }
    if provider_released || stream_released.is_some() {
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

pub(super) async fn handle_update_detail_and_release_provider(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    video_type: CustomVideoStreamType,
    handle: Option<ProviderHandle>,
    stream_uid: Option<u32>,
) {
    let stream_info = if let Some(uid) = stream_uid {
        deps.user_manager.update_stream_detail_by_uid(uid, video_type).await
    } else {
        deps.user_manager.update_stream_detail(&addr, video_type).await
    };
    if let Some(stream_info) = stream_info {
        if matches!(video_type, CustomVideoStreamType::LowPriorityPreempted) {
            deps.user_manager
                .block_user_for_stream_uid(
                    stream_info.uid,
                    shared::model::VirtualId::new(stream_info.channel.virtual_id),
                    PREEMPT_REENTRY_BLOCK_SECS,
                )
                .await;
        }
        deps.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
    }
    if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle);
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

async fn handle_adaptive_session_expired(deps: &CleanupWorkerDeps, stream_info: Box<StreamInfo>) {
    let qos = deps.event_manager.read_meter_qos(stream_info.meter_uid).await;
    let bytes_sent = qos.map(|qos| qos.bytes_total);
    let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
    deps.event_manager.unregister_meter_client(stream_info.uid).await;
    emit_disconnect_record(
        &deps.history_writer,
        &stream_info,
        DisconnectReason::SessionExpired,
        &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
        None,
        None,
    );
    deps.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::DisconnectedStream {
        addr: stream_info.addr,
        uid: stream_info.uid,
    }));
    notify_capacity(deps.capacity_notify.as_ref());
}

#[allow(clippy::too_many_arguments)]
async fn release_stream_with_disconnect(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    stream_uid: Option<u32>,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
) -> Option<StreamInfo> {
    let detach = if let Some(stream_uid) = stream_uid {
        deps.user_manager.release_stream_request_by_uid(&addr, stream_uid).await
    } else {
        crate::StreamRequestDetach::NotFound
    };
    match detach {
        crate::StreamRequestDetach::NotFound => {
            debug_if_enabled!(
                "Stream release skipped: no active stream for {} uid={:?}",
                sanitize_sensitive_info(&addr.to_string()),
                stream_uid
            );
            // The provider request must still be finished even when the user claim is
            // already gone: the identity travels with the cleanup event.
            if let (Some(owner), Some(request_id)) = (owner.as_deref(), request_id) {
                let reason = resolve_disconnect_reason_from_provider_end(provider_end_reason);
                deps.provider_manager.finish_identified_playback_request(
                    owner,
                    request_id,
                    playback_outcome_for_reason(reason),
                );
                notify_capacity(deps.capacity_notify.as_ref());
            }
            None
        }
        crate::StreamRequestDetach::Retained(stream_info) | crate::StreamRequestDetach::Preserved(stream_info) => {
            let reason = resolve_disconnect_reason(provider_end_reason, &stream_info);
            if let (Some(session_token), Some(request_id)) =
                (owner.as_deref().or(stream_info.session_token.as_deref()), request_id)
            {
                deps.provider_manager.finish_identified_playback_request(
                    session_token,
                    request_id,
                    playback_outcome_for_reason(reason),
                );
            }
            notify_capacity(deps.capacity_notify.as_ref());
            None
        }
        crate::StreamRequestDetach::Removed(stream_info) => {
            let qos = deps.event_manager.read_meter_qos(stream_info.meter_uid).await;
            let bytes_sent = qos.map(|qos| qos.bytes_total);
            let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
            deps.event_manager.unregister_meter_client(stream_info.uid).await;
            let reason = resolve_disconnect_reason(provider_end_reason, &stream_info);
            if let (Some(session_token), Some(request_id)) =
                (owner.as_deref().or(stream_info.session_token.as_deref()), request_id)
            {
                deps.provider_manager.finish_identified_playback_request(
                    session_token,
                    request_id,
                    playback_outcome_for_reason(reason),
                );
            }
            let provider_reconnect_count = (reconnect_count > 0).then_some(reconnect_count);
            emit_disconnect_record(
                &deps.history_writer,
                &stream_info,
                reason,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, provider_reconnect_count },
                provider_error_class,
                provider_http_status,
            );
            Some(stream_info)
        }
    }
}

impl ConnectionManager {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) fn spawn_cleanup_worker(
        mut rx: mpsc::Receiver<CleanupEvent>,
        mut control_rx: mpsc::Receiver<CleanupEvent>,
        user_manager: Arc<ActiveUserManager>,
        provider_manager: Arc<ActiveProviderManager>,
        shared_stream_manager: Arc<SharedStreamManager>,
        event_manager: Arc<EventManager>,
        capacity_notify: Arc<Notify>,
        history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
        shutdown_token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let deps = CleanupWorkerDeps {
            user_manager,
            provider_manager,
            shared_stream_manager,
            event_manager,
            capacity_notify,
            history_writer,
        };
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    () = shutdown_token.cancelled() => break,
                    control_event = control_rx.recv() => control_event,
                    event = rx.recv() => event,
                };
                let Some(event) = event else { break };
                match event {
                    CleanupEvent::ReleaseSharedSubscriber { addr, subscriber_id, request_id, owner } => {
                        deps.shared_stream_manager.release_subscriber(subscriber_id).await;
                        handle_release_stream(
                            &deps,
                            addr,
                            Some(subscriber_id.stream_uid()),
                            PROVIDER_END_NOT_SET,
                            0,
                            None,
                            None,
                            request_id,
                            owner,
                        )
                        .await;
                        notify_capacity(deps.capacity_notify.as_ref());
                    }
                    CleanupEvent::ReleaseConnection { addr } => {
                        handle_release_connection(&deps, addr).await;
                    }
                    CleanupEvent::ReleaseStream {
                        request_id,
                        owner,
                        addr,
                        stream_uid,
                        provider_end_reason,
                        reconnect_count,
                        provider_error_class,
                        provider_http_status,
                    } => {
                        handle_release_stream(
                            &deps,
                            addr,
                            stream_uid,
                            provider_end_reason,
                            reconnect_count,
                            provider_error_class,
                            provider_http_status,
                            request_id,
                            owner,
                        )
                        .await;
                    }
                    CleanupEvent::ReleaseProviderHandle { handle } => {
                        handle_release_provider_handle(&deps, handle);
                    }
                    CleanupEvent::ReleaseStreamAndProviderHandle {
                        request_id,
                        addr,
                        stream_uid,
                        handle,
                        provider_end_reason,
                        reconnect_count,
                        provider_error_class,
                        provider_http_status,
                    } => {
                        handle_release_stream_and_provider_handle(
                            &deps,
                            addr,
                            stream_uid,
                            handle,
                            provider_end_reason,
                            reconnect_count,
                            provider_error_class,
                            provider_http_status,
                            request_id,
                        )
                        .await;
                    }
                    CleanupEvent::UpdateDetailAndReleaseProvider { addr, stream_uid, video_type, handle } => {
                        handle_update_detail_and_release_provider(&deps, addr, video_type, handle, stream_uid).await;
                    }
                    CleanupEvent::AdaptiveSessionExpired { stream_info } => {
                        handle_adaptive_session_expired(&deps, stream_info).await;
                    }
                    CleanupEvent::ConfirmPlaybackLease { owner, request_id } => {
                        if let Some(request_id) = request_id {
                            deps.provider_manager.confirm_identified_playback_activity(&owner, request_id);
                        } else {
                            deps.provider_manager.confirm_playback_activity(&owner);
                        }
                    }
                    CleanupEvent::Defer(future) => {
                        future.await;
                    }
                }
            }
            debug!("Cleanup worker exiting");
        })
    }

    pub fn send_cleanup(&self, event: CleanupEvent) { self.cleanup_sender.enqueue(event); }

    pub fn cleanup_tx(&self) -> mpsc::Sender<CleanupEvent> { self.cleanup_sender.tx.clone() }

    /// Reserved control lane for cleanup permits that must never be starved by a
    /// saturated body-cleanup queue (HLS origin I/O, shutdown).
    pub fn control_cleanup_tx(&self) -> mpsc::Sender<CleanupEvent> { self.control_cleanup_tx.clone() }

    pub fn dropped_cleanup_events(&self) -> u64 {
        self.cleanup_sender.dropped_count() + self.user_manager.dropped_cleanup_events.load(Ordering::Relaxed)
    }
}
