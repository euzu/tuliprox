use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, CustomVideoStreamType, EventManager, EventMessage, ProviderHandle,
        SharedStreamManager,
    },
    auth::Fingerprint,
    utils::debug_if_enabled,
};
use log::{debug, warn};
use shared::{
    model::{ActiveUserConnectionChange, StreamChannel, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tokio::sync::{mpsc, Notify};

pub(crate) enum CleanupEvent {
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

pub struct ConnectionManager {
    pub user_manager: Arc<ActiveUserManager>,
    pub provider_manager: Arc<ActiveProviderManager>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    close_socket_signal_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    cleanup_tx: mpsc::UnboundedSender<CleanupEvent>,
    capacity_notify: Arc<Notify>,
}

impl ConnectionManager {
    pub fn new(
        user_manager: &Arc<ActiveUserManager>,
        provider_manager: &Arc<ActiveProviderManager>,
        shared_stream_manager: &Arc<SharedStreamManager>,
        event_manager: &Arc<EventManager>,
    ) -> Self {
        let (close_socket_signal_tx, _) = tokio::sync::broadcast::channel(256);
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        let capacity_notify = Arc::new(Notify::new());

        let mgr = Self {
            user_manager: Arc::clone(user_manager),
            provider_manager: Arc::clone(provider_manager),
            shared_stream_manager: Arc::clone(shared_stream_manager),
            event_manager: Arc::clone(event_manager),
            close_socket_signal_tx,
            cleanup_tx,
            capacity_notify: Arc::clone(&capacity_notify),
        };

        Self::spawn_cleanup_worker(
            cleanup_rx,
            Arc::clone(user_manager),
            Arc::clone(provider_manager),
            Arc::clone(shared_stream_manager),
            Arc::clone(event_manager),
            capacity_notify,
        );

        mgr
    }

    fn spawn_cleanup_worker(
        mut rx: mpsc::UnboundedReceiver<CleanupEvent>,
        user_manager: Arc<ActiveUserManager>,
        provider_manager: Arc<ActiveProviderManager>,
        shared_stream_manager: Arc<SharedStreamManager>,
        event_manager: Arc<EventManager>,
        capacity_notify: Arc<Notify>,
    ) {
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CleanupEvent::ReleaseStream { addr } => {
                        if user_manager.release_stream(&addr).await {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Disconnected(addr),
                            ));
                            capacity_notify.notify_waiters();
                        }
                    }
                    CleanupEvent::ReleaseProviderHandle { handle } => {
                        if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                            capacity_notify.notify_waiters();
                        }
                    }
                    CleanupEvent::ReleaseStreamAndProviderHandle { addr, handle } => {
                        // Release provider handle first to avoid a race window where the user
                        // connection count drops (making capacity appear available) before the
                        // provider slot is actually freed.
                        if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                        }
                        if user_manager.release_stream(&addr).await {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Disconnected(addr),
                            ));
                        }
                        capacity_notify.notify_waiters();
                    }
                    CleanupEvent::UpdateDetailAndReleaseProvider { addr, video_type, handle } => {
                        if let Some(stream_info) = user_manager.update_stream_detail(&addr, video_type).await {
                            event_manager.send_event(EventMessage::ActiveUser(
                                ActiveUserConnectionChange::Updated(stream_info),
                            ));
                        }
                        if let Some(h) = handle {
                            provider_manager.release_handle(&h).await;
                            capacity_notify.notify_waiters();
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
                        capacity_notify.notify_waiters();
                    }
                }
            }
            debug!("Cleanup worker exiting");
        });
    }

    pub(crate) fn send_cleanup(&self, event: CleanupEvent) {
        if let Err(e) = self.cleanup_tx.send(event) {
            debug!("Cleanup channel closed, dropping event: {e}");
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
        self.provider_manager.release_connection(addr).await;
        self.shared_stream_manager.release_connection(addr, true).await;
        if removed {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
        }
        self.capacity_notify.notify_waiters();
    }

    pub async fn release_provider_connection(&self, addr: &SocketAddr) {
        self.provider_manager.release_connection(addr).await;
        self.shared_stream_manager.release_connection(addr, false).await;
        self.capacity_notify.notify_waiters();
    }

    pub async fn release_stream(&self, addr: &SocketAddr) {
        if self.user_manager.release_stream(addr).await {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(*addr)));
            self.capacity_notify.notify_waiters();
        }
    }

    pub async fn release_provider_handle(&self, provider_handle: Option<ProviderHandle>) {
        if let Some(handle) = provider_handle {
            self.provider_manager.release_handle(&handle).await;
            self.capacity_notify.notify_waiters();
        }
    }

    pub fn capacity_notified(&self) -> Arc<Notify> {
        Arc::clone(&self.capacity_notify)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_connection(&self, addr: &SocketAddr) { self.user_manager.add_connection(addr).await; }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_connection(
        &self,
        username: &str,
        max_connections: u32,
        fingerprint: &Fingerprint,
        provider: &str,
        stream_channel: StreamChannel,
        user_agent: Cow<'_, str>,
        session_token: Option<&str>,
    ) {
        if let Some(stream_info) = self
            .user_manager
            .update_connection(
                username,
                max_connections,
                fingerprint,
                provider,
                stream_channel,
                user_agent,
                session_token,
            )
            .await
        {
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
