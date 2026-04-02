use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, CustomVideoStreamType, EventManager,
        EventMessage, ProviderHandle, SharedStreamManager,
    },
    model::StreamHistoryConfig,
    repository::{ConnectFailureReason, DisconnectQos, DisconnectReason, FailureStage, StreamHistoryRecord},
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
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};
use crate::repository::{recover_pending_files, StreamHistoryWriter};

// Maximum number of deferred cleanup actions buffered before producers must wait/drop.
const CLEANUP_QUEUE_CAPACITY: usize = 4096;
pub(crate) const PROVIDER_END_NOT_SET: u8 = 0;
pub(crate) const PROVIDER_END_CLOSED: u8 = 1; // Provider EOF
pub(crate) const PROVIDER_END_ERROR: u8 = 2; // Provider Err
// Maximum number of adaptive HTTP activity updates waiting to refresh socket expiry state.
const SOCKET_ACTIVITY_QUEUE_CAPACITY: usize = 4096;
// Rebuild the expiry heap when it grows beyond this multiple of the live index size.
const SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR: usize = 2;
// Avoid rebuilding the expiry heap unless it contains at least this many stale entries.
const SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE: usize = 256;
fn notify_capacity(capacity_notify: &Notify) { capacity_notify.notify_waiters(); }

struct CleanupWorkerDeps {
    user_manager: Arc<ActiveUserManager>,
    provider_manager: Arc<ActiveProviderManager>,
    shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    capacity_notify: Arc<Notify>,
    history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
}

pub(crate) enum CleanupEvent {
    ReleaseStream {
        addr: SocketAddr,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    },
    ReleaseConnection { addr: SocketAddr },
    ReleaseProviderHandle { handle: Option<ProviderHandle> },
    ReleaseStreamAndProviderHandle {
        addr: SocketAddr,
        handle: Option<ProviderHandle>,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    },
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

async fn handle_release_connection(deps: &CleanupWorkerDeps, addr: SocketAddr) {
    let removed = deps.user_manager.release_connection(&addr).await;
    for stream_info in &removed.removed_streams {
        deps.event_manager.unregister_meter_client(stream_info.uid).await;
    }
    deps.provider_manager.release_connection(&addr).await;
    deps.shared_stream_manager.release_connection(&addr, true).await;
    if removed.addr_removed && !removed.removed_streams.is_empty() {
        deps.event_manager
            .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(addr)));
    }
    notify_capacity(deps.capacity_notify.as_ref());
}

async fn handle_release_stream(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
) {
    if release_stream_with_disconnect(
        deps,
        addr,
        provider_end_reason,
        reconnect_count,
        provider_error_class,
        provider_http_status,
    )
    .await
    {
        deps.event_manager
            .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(addr)));
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

async fn handle_release_provider_handle(deps: &CleanupWorkerDeps, handle: Option<ProviderHandle>) {
    if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle).await;
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

async fn handle_release_stream_and_provider_handle(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    handle: Option<ProviderHandle>,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
) {
    let provider_released = if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle).await;
        true
    } else {
        false
    };
    let stream_released = release_stream_with_disconnect(
        deps,
        addr,
        provider_end_reason,
        reconnect_count,
        provider_error_class,
        provider_http_status,
    )
    .await;
    if stream_released {
        deps.event_manager
            .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(addr)));
    }
    if provider_released || stream_released {
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

async fn handle_update_detail_and_release_provider(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    video_type: CustomVideoStreamType,
    handle: Option<ProviderHandle>,
) {
    if let Some(stream_info) = deps.user_manager.update_stream_detail(&addr, video_type).await {
        deps.event_manager
            .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
    }
    if let Some(handle) = handle {
        deps.provider_manager.release_handle(&handle).await;
        notify_capacity(deps.capacity_notify.as_ref());
    }
}

async fn handle_update_detail_and_release_provider_connection(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    video_type: CustomVideoStreamType,
) {
    if let Some(stream_info) = deps.user_manager.update_stream_detail(&addr, video_type).await {
        deps.event_manager
            .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
    }
    deps.provider_manager.release_connection(&addr).await;
    deps.shared_stream_manager.release_connection(&addr, false).await;
    notify_capacity(deps.capacity_notify.as_ref());
}

async fn handle_adaptive_session_expired(deps: &CleanupWorkerDeps, stream_info: Box<StreamInfo>) {
    let (bytes_sent, first_byte_latency_ms) = deps.event_manager.read_meter_qos(stream_info.meter_uid).await;
    deps.event_manager.unregister_meter_client(stream_info.uid).await;
    emit_disconnect_record(
        &deps.history_writer,
        &stream_info,
        &DisconnectReason::SessionExpired,
        &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
        None,
        None,
    );
    deps.event_manager.send_event(EventMessage::ActiveUser(
        ActiveUserConnectionChange::Disconnected(stream_info.addr),
    ));
    notify_capacity(deps.capacity_notify.as_ref());
}

async fn release_stream_with_disconnect(
    deps: &CleanupWorkerDeps,
    addr: SocketAddr,
    provider_end_reason: u8,
    reconnect_count: u8,
    provider_error_class: Option<&'static str>,
    provider_http_status: Option<u16>,
) -> bool {
    let Some(stream_info) = deps.user_manager.release_stream(&addr).await else {
        return false;
    };
    let (bytes_sent, first_byte_latency_ms) = deps.event_manager.read_meter_qos(stream_info.meter_uid).await;
    deps.event_manager.unregister_meter_client(stream_info.uid).await;
    let reason = resolve_disconnect_reason(provider_end_reason, &stream_info);
    let provider_reconnect_count = (reconnect_count > 0).then_some(reconnect_count);
    emit_disconnect_record(
        &deps.history_writer,
        &stream_info,
        &reason,
        &DisconnectQos { bytes_sent, first_byte_latency_ms, provider_reconnect_count },
        provider_error_class,
        provider_http_status,
    );
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SocketExpiryEntry {
    expires_at: u64,
    addr: SocketAddr,
}

#[derive(Clone, Copy, Debug)]
enum SocketActivityEvent { Track(SocketAddr) }

pub struct ConnectionManager {
    pub user_manager: Arc<ActiveUserManager>,
    pub provider_manager: Arc<ActiveProviderManager>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    close_socket_signal_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    cleanup_tx: mpsc::Sender<CleanupEvent>,
    socket_activity_tx: mpsc::Sender<SocketActivityEvent>,
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
        let (socket_activity_tx, socket_activity_rx) = mpsc::channel(SOCKET_ACTIVITY_QUEUE_CAPACITY);
        let socket_cleanup_tx = cleanup_tx.clone();
        let capacity_notify = Arc::new(Notify::new());
        let mgr = Self {
            user_manager: Arc::clone(user_manager),
            provider_manager: Arc::clone(provider_manager),
            shared_stream_manager: Arc::clone(shared_stream_manager),
            event_manager: Arc::clone(event_manager),
            close_socket_signal_tx,
            cleanup_tx,
            socket_activity_tx,
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
        Self::spawn_socket_activity_worker(
            socket_activity_rx,
            Arc::clone(user_manager),
            socket_cleanup_tx,
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

    fn spawn_socket_activity_worker(
        mut rx: mpsc::Receiver<SocketActivityEvent>,
        user_manager: Arc<ActiveUserManager>,
        cleanup_tx: mpsc::Sender<CleanupEvent>,
    ) {
        tokio::spawn(async move {
            let mut expiry_queue: BinaryHeap<Reverse<SocketExpiryEntry>> = BinaryHeap::new();
            let mut expiry_index: HashMap<SocketAddr, u64> = HashMap::new();

            loop {
                let next_expiry = expiry_queue.peek().map(|entry| entry.0.expires_at);
                if let Some(expires_at) = next_expiry {
                    let now = shared::utils::current_time_secs();
                    if expires_at <= now {
                        Self::process_due_socket_expiry_entries(
                            &mut expiry_queue,
                            &mut expiry_index,
                            now,
                            &user_manager,
                            &cleanup_tx,
                        )
                        .await;
                        continue;
                    }

                    tokio::select! {
                        maybe_event = rx.recv() => {
                            let Some(event) = maybe_event else {
                                break;
                            };
                            Self::handle_socket_activity_event(event, &mut expiry_queue, &mut expiry_index, &user_manager).await;
                        }
                        () = tokio::time::sleep(Duration::from_secs(expires_at.saturating_sub(now))) => {}
                    }
                } else {
                    let Some(event) = rx.recv().await else {
                        break;
                    };
                    Self::handle_socket_activity_event(event, &mut expiry_queue, &mut expiry_index, &user_manager).await;
                }
            }
        });
    }

    async fn handle_socket_activity_event(
        event: SocketActivityEvent,
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &mut HashMap<SocketAddr, u64>,
        user_manager: &Arc<ActiveUserManager>,
    ) {
        let SocketActivityEvent::Track(addr) = event;

        if let Some(expires_at) = user_manager.socket_expiry_deadline(&addr).await {
            let current = expiry_index.insert(addr, expires_at);
            if current != Some(expires_at) {
                expiry_queue.push(Reverse(SocketExpiryEntry { expires_at, addr }));
                Self::maybe_rebuild_socket_expiry_queue(expiry_queue, expiry_index);
            }
        }
    }

    async fn process_due_socket_expiry_entries(
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &mut HashMap<SocketAddr, u64>,
        now: u64,
        user_manager: &Arc<ActiveUserManager>,
        cleanup_tx: &mpsc::Sender<CleanupEvent>,
    ) {
        while let Some(entry) = expiry_queue.peek().copied() {
            if entry.0.expires_at > now {
                break;
            }

            let Reverse(SocketExpiryEntry { expires_at, addr }) = expiry_queue.pop().unwrap_or(entry);
            let Some(current_expires_at) = expiry_index.get(&addr).copied() else {
                continue;
            };
            if current_expires_at != expires_at {
                continue;
            }

            if let Some(next_expires_at) = user_manager.socket_expiry_deadline(&addr).await {
                if next_expires_at > now {
                    expiry_index.insert(addr, next_expires_at);
                    expiry_queue.push(Reverse(SocketExpiryEntry {
                        expires_at: next_expires_at,
                        addr,
                    }));
                    Self::maybe_rebuild_socket_expiry_queue(expiry_queue, expiry_index);
                    continue;
                }
            } else {
                expiry_index.remove(&addr);
                continue;
            }

            expiry_index.remove(&addr);
            if cleanup_tx.send(CleanupEvent::ReleaseConnection { addr }).await.is_err() {
                debug!("Cleanup channel closed, stopping socket expiry worker");
                break;
            }
        }
    }

    fn maybe_rebuild_socket_expiry_queue(
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &HashMap<SocketAddr, u64>,
    ) {
        let indexed_len = expiry_index.len();
        if indexed_len == 0 {
            expiry_queue.clear();
            return;
        }

        let stale_entries = expiry_queue.len().saturating_sub(indexed_len);
        if expiry_queue.len() <= indexed_len.saturating_mul(SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR)
            || stale_entries < SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE
        {
            return;
        }

        *expiry_queue = expiry_index
            .iter()
            .map(|(addr, expires_at)| Reverse(SocketExpiryEntry {
                expires_at: *expires_at,
                addr: *addr,
            }))
            .collect();
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
        let deps = CleanupWorkerDeps {
            user_manager,
            provider_manager,
            shared_stream_manager,
            event_manager,
            capacity_notify,
            history_writer,
        };
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CleanupEvent::ReleaseConnection { addr } => {
                        handle_release_connection(&deps, addr).await;
                    }
                    CleanupEvent::ReleaseStream {
                        addr,
                        provider_end_reason,
                        reconnect_count,
                        provider_error_class,
                        provider_http_status,
                    } => {
                        handle_release_stream(
                            &deps,
                            addr,
                            provider_end_reason,
                            reconnect_count,
                            provider_error_class,
                            provider_http_status,
                        )
                        .await;
                    }
                    CleanupEvent::ReleaseProviderHandle { handle } => {
                        handle_release_provider_handle(&deps, handle).await;
                    }
                    CleanupEvent::ReleaseStreamAndProviderHandle {
                        addr,
                        handle,
                        provider_end_reason,
                        reconnect_count,
                        provider_error_class,
                        provider_http_status,
                    } => {
                        handle_release_stream_and_provider_handle(
                            &deps,
                            addr,
                            handle,
                            provider_end_reason,
                            reconnect_count,
                            provider_error_class,
                            provider_http_status,
                        )
                        .await;
                    }
                    CleanupEvent::UpdateDetailAndReleaseProvider { addr, video_type, handle } => {
                        handle_update_detail_and_release_provider(&deps, addr, video_type, handle).await;
                    }
                    CleanupEvent::UpdateDetailAndReleaseProviderConnection { addr, video_type } => {
                        handle_update_detail_and_release_provider_connection(&deps, addr, video_type).await;
                    }
                    CleanupEvent::AdaptiveSessionExpired { stream_info } => {
                        handle_adaptive_session_expired(&deps, stream_info).await;
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
            emit_disconnect_record(
                &self.history_writer,
                stream_info,
                &DisconnectReason::ClientClosed,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
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
            emit_disconnect_record(
                &self.history_writer,
                &stream_info,
                &DisconnectReason::ClientClosed,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
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

    pub fn record_connect_failed(&self, info: &StreamInfo, reason: ConnectFailureReason, failure_stage: FailureStage) {
        self.record_connect_failed_with_provider_failure(info, reason, failure_stage, None, None);
    }

    pub fn record_connect_failed_with_provider_failure(
        &self,
        info: &StreamInfo,
        reason: ConnectFailureReason,
        failure_stage: FailureStage,
        provider_http_status: Option<u16>,
        provider_error_class: Option<&str>,
    ) {
        let guard = self.history_writer.load();
        let Some(writer) = guard.as_ref() else { return };
        let attempt_uid = self.next_stream_uid();
        writer.send_record(StreamHistoryRecord::from_connect_failed(
            info,
            reason,
            attempt_uid,
            failure_stage,
        )
        .with_provider_failure(provider_http_status, provider_error_class));
    }

    pub fn capacity_notified(&self) -> Arc<Notify> {
        Arc::clone(&self.capacity_notify)
    }

    /// Emit disconnect records for all still-active streams and flush the history writer.
    /// Call once at graceful shutdown before dropping the `ConnectionManager`.
    pub async fn shutdown(&self) {
        let active_streams = self.user_manager.get_all_active_streams().await;
        for stream_info in active_streams {
            let (bytes_sent, first_byte_latency_ms) = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            emit_disconnect_record(
                &self.history_writer,
                &stream_info,
                &DisconnectReason::Shutdown,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
        }
        if let Some(w) = self.history_writer.load_full() {
            w.shutdown().await;
        }
    }

    pub async fn add_connection(&self, addr: &SocketAddr) { self.user_manager.add_connection(addr).await; }

    async fn track_socket_activity(&self, addr: &SocketAddr) {
        if self.socket_activity_tx.send(SocketActivityEvent::Track(*addr)).await.is_err() {
            debug!("Socket activity channel closed, dropping socket activity event");
        }
    }

    pub async fn touch_http_activity(&self, username: &str, token: &str, addr: &SocketAddr) {
        self.user_manager.touch_http_activity(username, token, addr).await;
        self.track_socket_activity(addr).await;
    }

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

fn emit_disconnect_record(
    writer: &ArcSwapOption<StreamHistoryWriter>,
    info: &StreamInfo,
    reason: &DisconnectReason,
    qos: &DisconnectQos,
    provider_error_class: Option<&str>,
    provider_http_status: Option<u16>,
) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(
        StreamHistoryRecord::from_disconnect(
            info,
            reason.clone(),
            qos,
            resolve_disconnect_failure_stage(info, reason, qos),
        )
        .with_provider_failure(provider_http_status, provider_error_class),
    );
}

fn resolve_disconnect_failure_stage(info: &StreamInfo, reason: &DisconnectReason, qos: &DisconnectQos) -> Option<FailureStage> {
    match reason {
        DisconnectReason::ProviderError | DisconnectReason::ProviderClosed => {
            if !info.channel.shared && qos.first_byte_latency_ms.is_none() {
                Some(FailureStage::FirstByte)
            } else {
                Some(FailureStage::Streaming)
            }
        }
        DisconnectReason::Preempted => Some(FailureStage::Streaming),
        DisconnectReason::SessionExpired => Some(FailureStage::SessionReconnect),
        _ => None,
    }
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
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "".intern(),
            title: title.intern(),
            url: "".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
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

    #[test]
    fn test_provider_error_disconnect_maps_to_streaming_failure_stage() {
        assert_eq!(
            resolve_disconnect_failure_stage(
                &make_stream_info("some_provider", "Some Channel"),
                &DisconnectReason::ProviderError,
                &DisconnectQos { first_byte_latency_ms: Some(150), ..Default::default() },
            ),
            Some(FailureStage::Streaming)
        );
    }

    #[test]
    fn test_session_expired_disconnect_maps_to_session_reconnect_stage() {
        assert_eq!(
            resolve_disconnect_failure_stage(
                &make_stream_info("some_provider", "Some Channel"),
                &DisconnectReason::SessionExpired,
                &DisconnectQos::default(),
            ),
            Some(FailureStage::SessionReconnect)
        );
    }

    #[test]
    fn test_provider_error_without_first_byte_maps_to_first_byte_stage() {
        assert_eq!(
            resolve_disconnect_failure_stage(
                &make_stream_info("some_provider", "Some Channel"),
                &DisconnectReason::ProviderError,
                &DisconnectQos::default(),
            ),
            Some(FailureStage::FirstByte)
        );
    }

    #[test]
    fn test_shared_provider_error_without_first_byte_stays_streaming_stage() {
        let mut info = make_stream_info("some_provider", "Some Channel");
        info.channel.shared = true;
        assert_eq!(
            resolve_disconnect_failure_stage(
                &info,
                &DisconnectReason::ProviderError,
                &DisconnectQos::default(),
            ),
            Some(FailureStage::Streaming)
        );
    }
}
