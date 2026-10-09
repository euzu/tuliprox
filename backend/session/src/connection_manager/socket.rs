use super::{
    CleanupEvent, ConnectionManager, SocketActivityTracker, SocketExpiryEntry, SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR,
    SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE,
};
use crate::ActiveUserManager;
use log::debug;
use shared::{
    model::{DisconnectReason, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuliprox_core::utils::debug_if_enabled;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(super) enum SocketActivityEvent {
    HttpActivity { addr: SocketAddr },
    DirectBodyActivity { addr: SocketAddr },
}

impl SocketActivityEvent {
    pub(super) fn addr(&self) -> SocketAddr {
        match self {
            Self::HttpActivity { addr, .. } | Self::DirectBodyActivity { addr, .. } => *addr,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CloseConnectionSignal {
    WithReason(SocketAddr, DisconnectReason),
}

#[derive(Debug)]
pub(super) enum SocketCloseState {
    Open(tokio::sync::oneshot::Sender<DisconnectReason>),
    Closing,
}

impl ConnectionManager {
    pub(super) fn spawn_socket_activity_worker(
        activity_tracker: SocketActivityTracker,
        user_manager: Arc<ActiveUserManager>,
        cleanup_tx: mpsc::Sender<CleanupEvent>,
        shutdown_token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut expiry_queue: BinaryHeap<Reverse<SocketExpiryEntry>> = BinaryHeap::new();
            let mut expiry_index: HashMap<SocketAddr, u64> = HashMap::new();

            loop {
                Self::drain_pending_socket_activity(
                    &activity_tracker,
                    &mut expiry_queue,
                    &mut expiry_index,
                    &user_manager,
                )
                .await;

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
                        biased;
                        () = shutdown_token.cancelled() => break,
                        () = activity_tracker.notified() => {}
                        () = tokio::time::sleep(Duration::from_secs(expires_at.saturating_sub(now))) => {}
                    }
                } else {
                    tokio::select! {
                        () = shutdown_token.cancelled() => break,
                        () = activity_tracker.notified() => {}
                    }
                }
            }
        })
    }

    pub(super) async fn drain_pending_socket_activity(
        activity_tracker: &SocketActivityTracker,
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &mut HashMap<SocketAddr, u64>,
        user_manager: &Arc<ActiveUserManager>,
    ) {
        for event in activity_tracker.drain() {
            Self::handle_socket_activity_event(event, expiry_queue, expiry_index, user_manager).await;
        }
    }

    pub(super) async fn handle_socket_activity_event(
        event: SocketActivityEvent,
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &mut HashMap<SocketAddr, u64>,
        user_manager: &Arc<ActiveUserManager>,
    ) {
        let addr = event.addr();
        if matches!(event, SocketActivityEvent::DirectBodyActivity { .. }) {
            user_manager.touch_socket_activity(&addr).await;
        }

        if let Some(expires_at) = user_manager.socket_expiry_deadline(&addr).await {
            let current = expiry_index.insert(addr, expires_at);
            if current != Some(expires_at) {
                expiry_queue.push(Reverse(SocketExpiryEntry { expires_at, addr }));
                Self::maybe_rebuild_socket_expiry_queue(expiry_queue, expiry_index);
            }
        }
    }

    pub(super) async fn process_due_socket_expiry_entries(
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
                    expiry_queue.push(Reverse(SocketExpiryEntry { expires_at: next_expires_at, addr }));
                    Self::maybe_rebuild_socket_expiry_queue(expiry_queue, expiry_index);
                    continue;
                }
            }

            // Fallthrough to release connection if `None` or `< now`

            expiry_index.remove(&addr);
            debug_if_enabled!(
                "Socket activity deadline expired for {}, releasing connection",
                sanitize_sensitive_info(&addr.to_string())
            );
            if cleanup_tx.send(CleanupEvent::ReleaseConnection { addr }).await.is_err() {
                debug!("Cleanup channel closed, stopping socket expiry worker");
                break;
            }
        }
    }

    pub(super) fn maybe_rebuild_socket_expiry_queue(
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
            .map(|(addr, expires_at)| Reverse(SocketExpiryEntry { expires_at: *expires_at, addr: *addr }))
            .collect();
    }

    pub fn get_close_connection_channel(&self) -> tokio::sync::broadcast::Receiver<CloseConnectionSignal> {
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
        self.release_connection_as_kicked(addr).await
    }

    pub fn register_close_socket(&self, addr: SocketAddr) -> tokio::sync::oneshot::Receiver<DisconnectReason> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut closers) = self.socket_closers.lock() {
            closers.insert(addr, SocketCloseState::Open(tx));
        }
        rx
    }

    pub fn unregister_close_socket(&self, addr: &SocketAddr) {
        if let Ok(mut closers) = self.socket_closers.lock() {
            closers.remove(addr);
        }
    }

    pub async fn close_connection_with_reason_and_block(
        &self,
        addr: &SocketAddr,
        virtual_id: VirtualId,
        block_secs: u64,
        reason: DisconnectReason,
    ) -> bool {
        if block_secs > 0 {
            self.user_manager.block_user_for_stream(addr, virtual_id, block_secs).await;
        }
        self.close_connection_with_reason(addr, reason)
    }

    pub fn close_connection_signal(&self, addr: &SocketAddr) -> bool {
        self.close_connection_with_reason(addr, DisconnectReason::ClientClosed)
    }

    pub async fn block_stream_by_uid(&self, uid: u32, virtual_id: VirtualId, block_secs: u64) {
        self.user_manager.block_user_for_stream_uid(uid, virtual_id, block_secs).await;
    }

    pub fn close_connection_with_reason(&self, addr: &SocketAddr, reason: DisconnectReason) -> bool {
        let target_sent = if let Ok(mut closers) = self.socket_closers.lock() {
            match closers.remove(addr) {
                Some(SocketCloseState::Open(tx)) => {
                    if tx.send(reason).is_ok() {
                        closers.insert(*addr, SocketCloseState::Closing);
                        true
                    } else {
                        false
                    }
                }
                Some(SocketCloseState::Closing) => {
                    closers.insert(*addr, SocketCloseState::Closing);
                    true
                }
                None => false,
            }
        } else {
            false
        };
        let _ = self.close_socket_signal_tx.send(CloseConnectionSignal::WithReason(*addr, reason));
        target_sent
    }
}
