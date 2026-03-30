use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, CustomVideoStreamType, EventManager,
        EventMessage, ProviderHandle, SharedStreamManager,
    },
    auth::Fingerprint,
    utils::debug_if_enabled,
};
use log::{debug, warn};
use shared::{
    model::{ActiveUserConnectionChange, StreamChannel, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

// Maximum number of deferred cleanup actions buffered before producers must wait/drop.
const CLEANUP_QUEUE_CAPACITY: usize = 4096;
// Maximum number of adaptive HTTP activity updates waiting to refresh socket expiry state.
const SOCKET_ACTIVITY_QUEUE_CAPACITY: usize = 4096;
// Rebuild the expiry heap when it grows beyond this multiple of the live index size.
const SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR: usize = 2;
// Avoid rebuilding the expiry heap unless it contains at least this many stale entries.
const SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE: usize = 256;
fn notify_capacity(capacity_notify: &Notify) { capacity_notify.notify_waiters(); }

pub(crate) enum CleanupEvent {
    ReleaseConnection { addr: SocketAddr },
    ReleaseStream { addr: SocketAddr },
    ReleaseProviderHandle { handle: Option<ProviderHandle> },
    ReleaseStreamAndProviderHandle { addr: SocketAddr, handle: Option<ProviderHandle> },
    UpdateDetailAndReleaseProvider {
        addr: SocketAddr,
        video_type: CustomVideoStreamType,
        handle: Option<ProviderHandle>,
    },
    UpdateDetailAndReleaseProviderConnection {
        addr: SocketAddr,
        video_type: CustomVideoStreamType,
    },
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
    ) -> Self {
        let (close_socket_signal_tx, _) = tokio::sync::broadcast::channel(256);
        let (cleanup_tx, cleanup_rx) = mpsc::channel(CLEANUP_QUEUE_CAPACITY);
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
        };

        Self::spawn_cleanup_worker(
            cleanup_rx,
            Arc::clone(user_manager),
            Arc::clone(provider_manager),
            Arc::clone(shared_stream_manager),
            Arc::clone(event_manager),
            Arc::clone(&capacity_notify),
        );
        Self::spawn_socket_activity_worker(
            socket_activity_rx,
            Arc::clone(user_manager),
            socket_cleanup_tx,
        );

        mgr
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
    ) {
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CleanupEvent::ReleaseConnection { addr } => {
                        let removed = user_manager.release_connection(&addr).await;
                        for stream_info in &removed.removed_streams {
                            event_manager.unregister_meter_client(stream_info.uid).await;
                        }
                        provider_manager.release_connection(&addr).await;
                        shared_stream_manager.release_connection(&addr, true).await;
                        if removed.addr_removed && !removed.removed_streams.is_empty() {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Disconnected(addr),
                            ));
                        }
                        notify_capacity(capacity_notify.as_ref());
                    }
                    CleanupEvent::ReleaseStream { addr } => {
                        if let Some(stream_info) = user_manager.release_stream(&addr).await {
                            event_manager.unregister_meter_client(stream_info.uid).await;
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
                    CleanupEvent::ReleaseStreamAndProviderHandle { addr, handle } => {
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
                            event_manager.unregister_meter_client(stream_info.uid).await;
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
            self.event_manager.unregister_meter_client(stream_info.uid).await;
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
            self.event_manager.unregister_meter_client(stream_info.uid).await;
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
