use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, CustomVideoStreamType, EventManager,
        EventMessage, ProviderHandle, SharedStreamManager,
    },
    model::StreamHistoryConfig,
    repository::{DisconnectQos, DisconnectReason, StreamHistoryRecord},
    auth::Fingerprint,
    utils::debug_if_enabled,
};
use arc_swap::ArcSwapOption;
use log::{debug, warn};
use shared::{
    model::{ActiveUserConnectionChange, StreamChannel, StreamInfo, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    borrow::Cow,
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, Notify};
use crate::repository::{recover_pending_files, StreamHistoryWriter};

const CLEANUP_QUEUE_CAPACITY: usize = 4096;
pub(crate) const PROVIDER_END_NOT_SET: u8 = 0;
pub(crate) const PROVIDER_END_CLOSED: u8 = 1;  // Provider EOF
pub(crate) const PROVIDER_END_ERROR: u8 = 2;    // Provider Err
fn notify_capacity(capacity_notify: &Notify) { capacity_notify.notify_waiters(); }

pub(crate) enum CleanupEvent {
    ReleaseStream { addr: SocketAddr, provider_end_reason: u8, reconnect_count: u8 },
    ReleaseProviderHandle { handle: Option<ProviderHandle> },
    ReleaseStreamAndProviderHandle { addr: SocketAddr, handle: Option<ProviderHandle>, provider_end_reason: u8, reconnect_count: u8 },
    UpdateDetailAndReleaseProvider {
        addr: SocketAddr,
        video_type: CustomVideoStreamType,
        handle: Option<ProviderHandle>,
    },
    UpdateDetailAndReleaseProviderConnection {
        addr: SocketAddr,
        video_type: CustomVideoStreamType,
    },
    AdaptiveSessionExpired {
        stream_info: Box<StreamInfo>,
    },
}

pub struct ConnectionManager {
    pub user_manager: Arc<ActiveUserManager>,
    pub provider_manager: Arc<ActiveProviderManager>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    close_socket_signal_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    cleanup_tx: mpsc::Sender<CleanupEvent>,
    capacity_notify: Arc<Notify>,
    stream_uid_counter: AtomicU32,
    history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
}

pub struct ConnectionParams<'a> {
    pub meter_uid: u32,
    pub username: &'a str,
    pub max_connections: u32,
    pub fingerprint: &'a Fingerprint,
    pub provider: &'a str,
    pub stream_channel: &'a StreamChannel,
    pub user_agent: Cow<'a, str>,
    pub session_token: Option<&'a str>,
}

impl ConnectionManager {
    pub fn new(
        user_manager: &Arc<ActiveUserManager>,
        provider_manager: &Arc<ActiveProviderManager>,
        shared_stream_manager: &Arc<SharedStreamManager>,
        event_manager: &Arc<EventManager>,
        history_config: Option<&StreamHistoryConfig>,
    ) -> Self {
        let history_writer = Arc::new(ArcSwapOption::new(build_history_writer(history_config)));
        let (close_socket_signal_tx, _) = tokio::sync::broadcast::channel(256);
        let (cleanup_tx, cleanup_rx) = mpsc::channel(CLEANUP_QUEUE_CAPACITY);
        user_manager.set_cleanup_sender(cleanup_tx.clone());
        let capacity_notify = Arc::new(Notify::new());
        let mgr = Self {
            user_manager: Arc::clone(user_manager),
            provider_manager: Arc::clone(provider_manager),
            shared_stream_manager: Arc::clone(shared_stream_manager),
            event_manager: Arc::clone(event_manager),
            close_socket_signal_tx,
            cleanup_tx,
            capacity_notify: Arc::clone(&capacity_notify),
            stream_uid_counter: AtomicU32::new(1),
            history_writer: Arc::clone(&history_writer),
        };

        Self::spawn_cleanup_worker(
            cleanup_rx,
            Arc::clone(user_manager),
            Arc::clone(provider_manager),
            Arc::clone(shared_stream_manager),
            Arc::clone(event_manager),
            Arc::clone(&capacity_notify),
            history_writer,
        );

        mgr
    }

    /// Reload the history writer on config change. Shuts down the old writer first so
    /// `recover_pending_files` in `build_history_writer` does not collide with an active writer.
    pub async fn reload_history_writer(&self, config: Option<&StreamHistoryConfig>) {
        let old_writer = self.history_writer.swap(None);
        if let Some(w) = old_writer {
            w.shutdown().await;
        }
        let new_writer = build_history_writer(config);
        self.history_writer.store(new_writer);
    }

    fn spawn_cleanup_worker(
        mut rx: mpsc::Receiver<CleanupEvent>,
        user_manager: Arc<ActiveUserManager>,
        provider_manager: Arc<ActiveProviderManager>,
        shared_stream_manager: Arc<SharedStreamManager>,
        event_manager: Arc<EventManager>,
        capacity_notify: Arc<Notify>,
        history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
    ) {
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CleanupEvent::ReleaseStream { addr, provider_end_reason, reconnect_count } => {
                        if let Some(stream_info) = user_manager.release_stream(&addr).await {
                            let (bytes_sent, first_byte_latency_ms) = event_manager.read_meter_qos(stream_info.meter_uid).await;
                            event_manager.unregister_meter_client(stream_info.uid).await;
                            let reason = resolve_disconnect_reason(provider_end_reason, &stream_info);
                            let rc = if reconnect_count > 0 { Some(reconnect_count) } else { None };
                            emit_disconnect_record(&history_writer, &stream_info, reason, &DisconnectQos { bytes_sent, first_byte_latency_ms, provider_reconnect_count: rc });
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Disconnected(addr),
                            ));
                            notify_capacity(capacity_notify.as_ref());
                        }
                    }
                    CleanupEvent::ReleaseProviderHandle { handle } => {
                        if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                            notify_capacity(capacity_notify.as_ref());
                        }
                    }
                    CleanupEvent::ReleaseStreamAndProviderHandle { addr, handle, provider_end_reason, reconnect_count } => {
                        // Release provider handle first to avoid a race window where the user
                        // connection count drops (making capacity appear available) before the
                        // provider slot is actually freed.
                        let provider_released = if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                            true
                        } else {
                            false
                        };
                        let released_stream = user_manager.release_stream(&addr).await;
                        let stream_released = released_stream.is_some();
                        if let Some(stream_info) = released_stream {
                            let (bytes_sent, first_byte_latency_ms) = event_manager.read_meter_qos(stream_info.meter_uid).await;
                            event_manager.unregister_meter_client(stream_info.uid).await;
                            let reason = resolve_disconnect_reason(provider_end_reason, &stream_info);
                            let rc = if reconnect_count > 0 { Some(reconnect_count) } else { None };
                            emit_disconnect_record(&history_writer, &stream_info, reason, &DisconnectQos { bytes_sent, first_byte_latency_ms, provider_reconnect_count: rc });
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Disconnected(addr),
                            ));
                        }
                        if provider_released || stream_released {
                            notify_capacity(capacity_notify.as_ref());
                        }
                    }
                    CleanupEvent::UpdateDetailAndReleaseProvider { addr, video_type, handle } => {
                        if let Some(stream_info) = user_manager.update_stream_detail(&addr, video_type).await {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Updated(stream_info),
                            ));
                        }
                        if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                            notify_capacity(capacity_notify.as_ref());
                        }
                    }
                    CleanupEvent::UpdateDetailAndReleaseProviderConnection { addr, video_type } => {
                        if let Some(stream_info) = user_manager.update_stream_detail(&addr, video_type).await {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Updated(stream_info),
                            ));
                        }
                        provider_manager.release_connection(&addr).await;
                        shared_stream_manager.release_connection(&addr, false).await;
                        notify_capacity(capacity_notify.as_ref());
                    }
                    CleanupEvent::AdaptiveSessionExpired { stream_info } => {
                        let (bytes_sent, first_byte_latency_ms) = event_manager.read_meter_qos(stream_info.meter_uid).await;
                        event_manager.unregister_meter_client(stream_info.uid).await;
                        emit_disconnect_record(&history_writer, &stream_info, DisconnectReason::SessionExpired, &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() });
                        event_manager.send_event(EventMessage::ActiveUser(
                            ActiveUserConnectionChange::Disconnected(stream_info.addr),
                        ));
                        notify_capacity(capacity_notify.as_ref());
                    }
                }
            }
            debug!("Cleanup worker exiting");
        });
    }

    pub(crate) fn send_cleanup(&self, event: CleanupEvent) {
        match self.cleanup_tx.try_send(event) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_event)) => {
                warn!("Cleanup queue full, dropping event");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_event)) => {
                debug!("Cleanup channel closed, dropping cleanup event");
            }
        }
    }

    pub fn get_close_connection_channel(&self) -> tokio::sync::broadcast::Receiver<SocketAddr> {
        self.close_socket_signal_tx.subscribe()
    }

    pub async fn kick_connection(&self, addr: &SocketAddr, virtual_id: VirtualId, block_secs: u64) -> bool {
        debug_if_enabled!(
            "User {} kicked for stream with virtual_id {virtual_id} for {block_secs} seconds with addr {}.",
            self.user_manager.get_username_for_addr(addr).await.unwrap_or_default(),
            sanitize_sensitive_info(&addr.to_string())
        );
        if block_secs > 0 {
            self.user_manager.block_user_for_stream(addr, virtual_id, block_secs).await;
        }
        if let Err(e) = self.close_socket_signal_tx.send(*addr) {
            debug_if_enabled!(
                "No active receivers for close signal ({}): {e:?}",
                sanitize_sensitive_info(&addr.to_string())
            );
            return false;
        }
        true
    }

    pub async fn release_connection(&self, addr: &SocketAddr) {
        let removed = self.user_manager.release_connection(addr).await;
        for stream_info in &removed.removed_streams {
            let (bytes_sent, first_byte_latency_ms) = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            self.event_manager.unregister_meter_client(stream_info.uid).await;
            emit_disconnect_record(&self.history_writer, stream_info, DisconnectReason::ClientClosed, &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() });
        }
        self.provider_manager.release_connection(addr).await;
        self.shared_stream_manager.release_connection(addr, true).await;
        if removed.addr_removed && !removed.removed_streams.is_empty() {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
        }
        notify_capacity(self.capacity_notify.as_ref());
    }

    pub async fn release_provider_connection(&self, addr: &SocketAddr) {
        self.provider_manager.release_connection(addr).await;
        self.shared_stream_manager.release_connection(addr, false).await;
        notify_capacity(self.capacity_notify.as_ref());
    }

    pub async fn release_stream(&self, addr: &SocketAddr) {
        if let Some(stream_info) = self.user_manager.release_stream(addr).await {
            let (bytes_sent, first_byte_latency_ms) = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            self.event_manager.unregister_meter_client(stream_info.uid).await;
            emit_disconnect_record(&self.history_writer, &stream_info, DisconnectReason::ClientClosed, &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() });
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
            notify_capacity(self.capacity_notify.as_ref());
        }
    }

    pub async fn release_provider_handle(&self, provider_handle: Option<ProviderHandle>) {
        if let Some(handle) = provider_handle {
            self.provider_manager.release_handle(&handle).await;
            notify_capacity(self.capacity_notify.as_ref());
        }
    }

    pub fn next_stream_uid(&self) -> u32 {
        self.stream_uid_counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let next = current.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap_or(1)
    }

    pub fn capacity_notified(&self) -> Arc<Notify> {
        Arc::clone(&self.capacity_notify)
    }

    /// Flush and finalize the stream history writer. Call once at graceful shutdown.
    pub async fn shutdown(&self) {
        if let Some(w) = self.history_writer.load_full() {
            w.shutdown().await;
        }
    }

    pub async fn add_connection(&self, addr: &SocketAddr) { self.user_manager.add_connection(addr).await; }

    pub async fn update_connection(&self, update: ConnectionParams<'_>) {
        let uid = self.next_stream_uid();
        let username = update.username;
        let fingerprint = update.fingerprint;
        if let Some(stream_info) = self
            .user_manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: update.meter_uid,
                username,
                max_connections: update.max_connections,
                fingerprint,
                provider: update.provider,
                stream_channel: update.stream_channel,
                user_agent: update.user_agent,
                session_token: update.session_token,
            })
            .await
        {
            self.event_manager
                .register_meter_client(stream_info.uid, stream_info.meter_uid)
                .await;
            emit_connect_record(&self.history_writer, &stream_info);
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        } else {
            warn!("Failed to register connection for user {username} at {}; disconnecting client", fingerprint.addr);
            let _ = self.kick_connection(&fingerprint.addr, 0, 0).await;
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
}

/// Build a new `StreamHistoryWriter` from the given config, running file recovery first.
/// Returns `None` if history is disabled or no config is provided.
fn build_history_writer(config: Option<&StreamHistoryConfig>) -> Option<Arc<StreamHistoryWriter>> {
    let cfg = config?;
    if !cfg.stream_history_enabled {
        return None;
    }
    if let Err(e) = recover_pending_files(&cfg.stream_history_directory) {
        log::warn!("Stream history recovery failed: {e}");
    }
    Some(Arc::new(StreamHistoryWriter::new(cfg)))
}

/// Determine the disconnect reason from the provider-end signal and the stream's current state.
///
/// Priority: If `update_stream_detail` switched the stream to custom-video mode
/// (`provider == "tuliprox"`), the video type takes precedence. The `provider_end_reason`
/// `AtomicU8` disambiguates `ChannelUnavailable` into `ProviderClosed` (EOF) vs `ProviderError` (Err).
///
/// SAFETY: The `channel.title` strings (`channel_unavailable`, `low_priority_preempted`, etc.)
/// are wire-format identifiers shared with Serialize/Deserialize and the REST API.
/// If they ever change, update `CustomVideoStreamType::fmt`/`from_str` and this function together.
fn resolve_disconnect_reason(provider_end_reason: u8, stream_info: &StreamInfo) -> DisconnectReason {
    if stream_info.provider == "tuliprox" {
        if let Ok(video_type) = CustomVideoStreamType::from_str(&stream_info.channel.title) {
            match video_type {
                CustomVideoStreamType::LowPriorityPreempted => return DisconnectReason::Preempted,
                CustomVideoStreamType::UserConnectionsExhausted => return DisconnectReason::UserConnectionsExhausted,
                CustomVideoStreamType::ProviderConnectionsExhausted => return DisconnectReason::ProviderConnectionsExhausted,
                CustomVideoStreamType::ChannelUnavailable => {
                    return match provider_end_reason {
                        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
                        _ => DisconnectReason::ProviderError,
                    };
                }
                _ => {}
            }
        }
    }

    match provider_end_reason {
        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
        PROVIDER_END_ERROR => DisconnectReason::ProviderError,
        _ => DisconnectReason::ClientClosed,
    }
}

fn emit_connect_record(writer: &ArcSwapOption<StreamHistoryWriter>, info: &StreamInfo) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(StreamHistoryRecord::from_connect(info));
}

fn emit_disconnect_record(writer: &ArcSwapOption<StreamHistoryWriter>, info: &StreamInfo, reason: DisconnectReason, qos: &DisconnectQos) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(StreamHistoryRecord::from_disconnect(info, reason, qos));
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PlaylistItemType, StreamChannel, StreamInfo, XtreamCluster};
    use shared::utils::Internable;
    use std::net::SocketAddr;

    fn make_stream_info(provider: &str, title: &str) -> StreamInfo {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
        let channel = StreamChannel {
            target_id: 1,
            virtual_id: 1,
            provider_id: 1,
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "".intern(),
            title: title.intern(),
            url: "".intern(),
            shared: false,
            technical: None,
        };
        StreamInfo::new(0, 0, "test", &addr, "127.0.0.1", provider, channel, String::new(), None, None)
    }

    #[test]
    fn test_client_closed_when_no_provider_end() {
        let info = make_stream_info("some_provider", "Some Channel");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::ClientClosed);
    }

    #[test]
    fn test_provider_closed_on_eof() {
        let info = make_stream_info("some_provider", "Some Channel");
        let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
        assert_eq!(reason, DisconnectReason::ProviderClosed);
    }

    #[test]
    fn test_provider_error_on_err() {
        let info = make_stream_info("some_provider", "Some Channel");
        let reason = resolve_disconnect_reason(PROVIDER_END_ERROR, &info);
        assert_eq!(reason, DisconnectReason::ProviderError);
    }

    #[test]
    fn test_preempted_from_custom_video_detail() {
        let info = make_stream_info("tuliprox", "low_priority_preempted");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::Preempted);
    }

    #[test]
    fn test_channel_unavailable_with_eof_maps_to_provider_closed() {
        let info = make_stream_info("tuliprox", "channel_unavailable");
        let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
        assert_eq!(reason, DisconnectReason::ProviderClosed);
    }

    #[test]
    fn test_channel_unavailable_with_err_maps_to_provider_error() {
        let info = make_stream_info("tuliprox", "channel_unavailable");
        let reason = resolve_disconnect_reason(PROVIDER_END_ERROR, &info);
        assert_eq!(reason, DisconnectReason::ProviderError);
    }

    #[test]
    fn test_channel_unavailable_without_atomic_maps_to_provider_error() {
        let info = make_stream_info("tuliprox", "channel_unavailable");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::ProviderError);
    }

    #[test]
    fn test_user_exhausted_custom_video_maps_to_user_connections_exhausted() {
        let info = make_stream_info("tuliprox", "user_connections_exhausted");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::UserConnectionsExhausted);
    }

    #[test]
    fn test_provider_exhausted_custom_video_maps_to_provider_connections_exhausted() {
        let info = make_stream_info("tuliprox", "provider_connections_exhausted");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::ProviderConnectionsExhausted);
    }

    #[test]
    fn test_unknown_tuliprox_title_falls_through_to_atomic() {
        let info = make_stream_info("tuliprox", "some_unknown_video_type");
        let reason = resolve_disconnect_reason(PROVIDER_END_CLOSED, &info);
        assert_eq!(reason, DisconnectReason::ProviderClosed);
    }
}
