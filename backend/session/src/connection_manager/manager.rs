use super::{
    backpressure::notify_capacity,
    cleanup::release_connection_with_reason,
    history::{build_history_writer, build_history_writer_async, emit_disconnect_record},
    BackpressureSender, ConnectionManager, SocketActivityTracker, CLEANUP_QUEUE_CAPACITY, CONTROL_CLEANUP_CAPACITY,
};
use crate::{ActiveProviderManager, ActiveUserManager, EventManager, ManagedProviderHandle, SharedStreamManager};
use arc_swap::ArcSwapOption;
use shared::model::{
    ActiveUserConnectionChange, ConnectFailureReason, DisconnectReason, EventMessage, FailureStage, StreamInfo,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, Notify, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{DisconnectQos, ProviderHandle, StreamHistoryConfig, StreamHistoryRecord};
use tuliprox_repository::StreamHistoryWriter;

impl ConnectionManager {
    pub fn new(
        user_manager: &Arc<ActiveUserManager>,
        provider_manager: &Arc<ActiveProviderManager>,
        shared_stream_manager: &Arc<SharedStreamManager>,
        event_manager: &Arc<EventManager>,
        history_config: Option<&StreamHistoryConfig>,
    ) -> Self {
        Self::new_with_capacity(
            user_manager,
            provider_manager,
            shared_stream_manager,
            event_manager,
            history_config,
            CLEANUP_QUEUE_CAPACITY,
        )
    }

    /// Same as [`ConnectionManager::new`], but with an explicit cleanup-queue capacity
    /// derived from configuration instead of the hard-coded default.
    pub fn new_with_capacity(
        user_manager: &Arc<ActiveUserManager>,
        provider_manager: &Arc<ActiveProviderManager>,
        shared_stream_manager: &Arc<SharedStreamManager>,
        event_manager: &Arc<EventManager>,
        history_config: Option<&StreamHistoryConfig>,
        cleanup_capacity: usize,
    ) -> Self {
        let cleanup_capacity = cleanup_capacity.max(1);
        let history_writer = Arc::new(ArcSwapOption::new(build_history_writer(history_config)));
        let (close_socket_signal_tx, _) = tokio::sync::broadcast::channel(256);
        let (cleanup_tx, cleanup_rx) = mpsc::channel(cleanup_capacity);
        let (control_cleanup_tx, control_cleanup_rx) = mpsc::channel(CONTROL_CLEANUP_CAPACITY);
        user_manager.set_cleanup_sender(cleanup_tx.clone());
        user_manager.set_provider_manager(Arc::clone(provider_manager));
        let socket_cleanup_tx = cleanup_tx.clone();
        let socket_activity_tracker = SocketActivityTracker::new();
        let capacity_notify = Arc::new(Notify::new());
        let shutdown_token = CancellationToken::new();
        let mgr = Self {
            user_manager: Arc::clone(user_manager),
            provider_manager: Arc::clone(provider_manager),
            shared_stream_manager: Arc::clone(shared_stream_manager),
            event_manager: Arc::clone(event_manager),
            close_socket_signal_tx,
            socket_closers: std::sync::Mutex::new(HashMap::new()),
            cleanup_sender: BackpressureSender::new(cleanup_tx, "cleanup", cleanup_capacity),
            control_cleanup_tx: control_cleanup_tx.clone(),
            socket_activity_tracker: socket_activity_tracker.clone(),
            capacity_notify: Arc::clone(&capacity_notify),
            stream_uid_counter: AtomicU32::new(1),
            history_writer: Arc::clone(&history_writer),
            is_shutting_down: AtomicBool::new(false),
            admission_gate: RwLock::new(()),
            shutdown_token: shutdown_token.clone(),
            worker_handles: std::sync::Mutex::new(Vec::new()),
        };

        let cleanup_handle = Self::spawn_cleanup_worker(
            cleanup_rx,
            control_cleanup_rx,
            Arc::clone(user_manager),
            Arc::clone(provider_manager),
            Arc::clone(shared_stream_manager),
            Arc::clone(event_manager),
            Arc::clone(&capacity_notify),
            history_writer,
            shutdown_token.clone(),
        );
        let socket_handle = Self::spawn_socket_activity_worker(
            socket_activity_tracker,
            Arc::clone(user_manager),
            socket_cleanup_tx,
            shutdown_token,
        );
        mgr.worker_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([cleanup_handle, socket_handle]);

        mgr
    }

    /// Reload the history writer on config change. Shuts down the old writer first so
    /// `recover_pending_files` does not collide with an active writer. Recovery runs on the
    /// blocking pool instead of a runtime worker so large pending-file sets cannot stall
    /// live stream heartbeats.
    pub async fn reload_history_writer(&self, config: Option<&StreamHistoryConfig>) {
        let old_writer = self.history_writer.swap(None);
        if let Some(w) = old_writer {
            w.shutdown().await;
        }
        let new_writer = build_history_writer_async(config).await;
        self.history_writer.store(new_writer);
    }

    /// Returns a reference to the history writer.
    pub fn history_writer(&self) -> &Arc<ArcSwapOption<StreamHistoryWriter>> { &self.history_writer }

    pub async fn release_connection(&self, addr: &SocketAddr) {
        release_connection_with_reason(self, addr, DisconnectReason::ClientClosed, true).await;
    }

    pub async fn release_connection_with_reason(&self, addr: &SocketAddr, reason: DisconnectReason) {
        release_connection_with_reason(self, addr, reason, true).await;
    }

    pub async fn release_connection_as_kicked(&self, addr: &SocketAddr) -> bool {
        self.release_user_sessions_only(addr).await;
        let closed = self.close_connection_with_reason(addr, DisconnectReason::ClientKicked);
        if !closed {
            self.release_provider_deferred(addr).await;
        }
        closed
    }

    /// Releases the provider connection for `addr` after `tcp_close_notify` is notified.
    /// This defers the provider-slot release until after the TCP connection is fully closed,
    /// preventing a race where a new request acquires the same provider slot while the old
    /// connection is still draining buffered data (which can take 10+ seconds on a live stream).
    /// The `close_connection_with_reason` call on the `ConnectionManager` must have already been
    /// called before invoking this method.
    pub async fn release_provider_deferred(&self, addr: &SocketAddr) {
        let addr_owned = *addr;
        self.provider_manager.release_connection(&addr_owned);
        self.shared_stream_manager.release_connection(&addr_owned, true).await;
        notify_capacity(self.capacity_notify.as_ref());
    }

    /// Cleans up user/session/stream state for a forced close without releasing the provider slot.
    /// The provider release is deferred to `release_provider_deferred` which waits for the TCP
    /// connection to close, preventing a race where a new request acquires the same provider
    /// slot while the old connection is still draining buffered data.
    /// Note: `release_connection_as_kicked` (called internally) already handles divergence checking.
    pub async fn release_user_sessions_only(&self, addr: &SocketAddr) {
        let removed = self.user_manager.release_connection_as_kicked(addr).await;
        // Mirrors the kicked-specific steps from `release_connection_parts`.
        // Provider release and capacity notification are deferred via `release_provider_deferred`.
        for stream_info in &removed.removed_streams {
            if let Some(session_token) = stream_info.session_token.as_deref() {
                self.provider_manager.terminate_identified_playback_owner(session_token);
            }
        }
        for username in &removed.disconnected_users {
            self.user_manager.terminate_sessions_for_addr(username, addr).await;
        }
        for stream_info in &removed.removed_streams {
            let qos = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            let bytes_sent = qos.map(|qos| qos.bytes_total);
            let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
            self.event_manager.unregister_meter_client(stream_info.uid).await;
            emit_disconnect_record(
                &self.history_writer,
                stream_info,
                DisconnectReason::ClientKicked,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
        }
        if removed.addr_removed && !removed.removed_streams.is_empty() {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
        }
    }

    pub async fn release_provider_connection(&self, addr: &SocketAddr) {
        self.provider_manager.release_connection(addr);
        self.shared_stream_manager.release_connection(addr, false).await;
        notify_capacity(self.capacity_notify.as_ref());
    }

    pub async fn release_stream(&self, addr: &SocketAddr) {
        if let Some(stream_info) = self.user_manager.release_stream(addr).await {
            let qos = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            let bytes_sent = qos.map(|qos| qos.bytes_total);
            let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
            self.event_manager.unregister_meter_client(stream_info.uid).await;
            emit_disconnect_record(
                &self.history_writer,
                &stream_info,
                DisconnectReason::ClientClosed,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::DisconnectedStream {
                addr: stream_info.addr,
                uid: stream_info.uid,
            }));
            notify_capacity(self.capacity_notify.as_ref());
        }
    }

    pub fn release_provider_handle(&self, provider_handle: Option<ProviderHandle>) {
        if let Some(handle) = provider_handle {
            self.provider_manager.release_handle(&handle);
            notify_capacity(self.capacity_notify.as_ref());
        }
    }

    /// Releases a managed provider owner by value: its drop performs the synchronous
    /// slot release, then the capacity waiters are notified.
    pub fn release_managed_provider_handle(&self, provider_handle: Option<ManagedProviderHandle>) {
        if provider_handle.is_some() {
            drop(provider_handle);
            notify_capacity(self.capacity_notify.as_ref());
        }
    }

    pub fn next_stream_uid(&self) -> u32 {
        self.stream_uid_counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let next = current.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap_or(1)
    }

    pub fn record_connect_failed_with_provider_failure(
        &self,
        info: &StreamInfo,
        reason: ConnectFailureReason,
        failure_stage: FailureStage,
        provider_http_status: Option<u16>,
        provider_error_class: Option<&str>,
        target_name: Option<Arc<str>>,
    ) {
        let guard = self.history_writer.load();
        let Some(writer) = guard.as_ref() else { return };
        let attempt_uid = self.next_stream_uid();
        writer.send_record(
            StreamHistoryRecord::from_connect_failed(info, reason, attempt_uid, failure_stage, target_name)
                .with_provider_failure(provider_http_status, provider_error_class),
        );
    }

    pub fn capacity_notified(&self) -> Arc<Notify> { Arc::clone(&self.capacity_notify) }

    #[inline]
    pub fn is_shutting_down(&self) -> bool { self.is_shutting_down.load(Ordering::Acquire) }

    pub(crate) async fn begin_admission(&self) -> Option<RwLockReadGuard<'_, ()>> {
        if self.is_shutting_down() {
            return None;
        }
        let guard = self.admission_gate.read().await;
        (!self.is_shutting_down()).then_some(guard)
    }

    /// Emit disconnect records for all still-active streams, unregister shared streams,
    /// drain queued cleanups, and flush the history writer.
    /// Call once at graceful shutdown before dropping the `ConnectionManager`.
    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::Release);
        let _admission_closed = self.admission_gate.write().await;
        self.shared_stream_manager.shutdown().await;
        let active_streams = self.user_manager.drain_for_shutdown().await;
        for stream_info in &active_streams {
            let qos = self.event_manager.read_meter_qos(stream_info.meter_uid).await;
            let bytes_sent = qos.map(|qos| qos.bytes_total);
            let first_byte_latency_ms = qos.and_then(|qos| qos.first_byte_latency_ms);
            emit_disconnect_record(
                &self.history_writer,
                stream_info,
                DisconnectReason::Shutdown,
                &DisconnectQos { bytes_sent, first_byte_latency_ms, ..Default::default() },
                None,
                None,
            );
            self.event_manager.unregister_meter_client(stream_info.uid).await;
        }
        // This terminal transition does not depend on queue capacity or body-held
        // cleanup permits. Later guard drops are idempotent against the emptied indices.
        self.provider_manager.shutdown();
        if let Some(w) = self.history_writer.load_full() {
            w.shutdown().await;
        }
        // Stop and join the cleanup and socket workers so no background task outlives
        // the manager. Active claims were released directly above; later guard drops are
        // idempotent against the emptied indices.
        self.shutdown_token.cancel();
        let handles =
            std::mem::take(&mut *self.worker_handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for handle in handles {
            let _ = handle.await;
        }
    }
}
