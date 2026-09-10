use crate::{
    uses_direct_body_idle_timeout, ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, EventManager,
    ManagedProviderHandle, SharedStreamManager,
};
use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use log::{debug, warn};
use shared::{
    model::{
        ActiveUserConnectionChange, ConnectFailureReason, CustomVideoStreamType, DisconnectReason, EventMessage,
        FailureStage, StreamChannel, StreamInfo, VirtualId,
    },
    utils::sanitize_sensitive_info,
};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, Notify, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{
        DisconnectQos, Fingerprint, PlaybackRequestOutcome, ProviderHandle, SharedSubscriberId, StreamHistoryConfig,
        StreamHistoryRecord,
    },
    utils::debug_if_enabled,
};
use tuliprox_repository::{recover_pending_files, StreamHistoryWriter};

// Maximum number of deferred cleanup actions buffered before producers must wait/drop.
const CLEANUP_QUEUE_CAPACITY: usize = 4096;
// Reserved lane for control work (HLS origin I/O, shutdown) so a saturated body-cleanup
// queue cannot starve the control path that releases provider handles.
const CONTROL_CLEANUP_CAPACITY: usize = 64;
pub const PROVIDER_END_NOT_SET: u8 = 0;
pub const PROVIDER_END_CLOSED: u8 = 1; // Provider EOF
pub const PROVIDER_END_ERROR: u8 = 2; // Provider Err
const PREEMPT_REENTRY_BLOCK_SECS: u64 = 3;
// Bounded wait for a mandatory cleanup admission right before a request registration.
const CLEANUP_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);
// Rebuild the expiry heap when it grows beyond this multiple of the live index size.
const SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR: usize = 2;
// Avoid rebuilding the expiry heap unless it contains at least this many stale entries.
const SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE: usize = 256;
fn notify_capacity(capacity_notify: &Notify) { capacity_notify.notify_waiters(); }

/// Proof that a shared-stream response owns the guaranteed terminal cleanup permit for
/// its subscriber. The constructor is crate-private, so callers outside this crate cannot
/// fabricate the claim; it is only produced when a cleanup permit is actually reserved.
/// Each registration consumes one capability, so a stale capability cannot be duplicated
/// and reused after its owner has released.
#[derive(Debug)]
pub struct SharedCleanupCapability {
    subscriber_id: SharedSubscriberId,
}

impl SharedCleanupCapability {
    pub(crate) fn new(subscriber_id: SharedSubscriberId) -> Self { Self { subscriber_id } }

    pub fn stream_uid(&self) -> u32 { self.subscriber_id.stream_uid() }

    pub fn subscriber_id(&self) -> SharedSubscriberId { self.subscriber_id }
}

struct BackpressureState<T> {
    overflow: VecDeque<T>,
    draining: bool,
}

struct BackpressureSender<T> {
    tx: mpsc::Sender<T>,
    state: Arc<Mutex<BackpressureState<T>>>,
    queue_name: &'static str,
    overflow_capacity: usize,
    dropped_events: AtomicU64,
}

impl<T> BackpressureSender<T>
where
    T: Send + 'static,
{
    fn new(tx: mpsc::Sender<T>, queue_name: &'static str, overflow_capacity: usize) -> Self {
        Self {
            tx,
            state: Arc::new(Mutex::new(BackpressureState { overflow: VecDeque::new(), draining: false })),
            queue_name,
            overflow_capacity,
            dropped_events: AtomicU64::new(0),
        }
    }

    fn enqueue(&self, event: T) {
        let runtime = tokio::runtime::Handle::try_current().ok();
        {
            let mut state = lock_backpressure_state(self.state.as_ref());
            if state.draining {
                if state.overflow.len() >= self.overflow_capacity {
                    let count = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                    if count == 1 || count.is_multiple_of(1024) {
                        warn!(
                            "{} overflow buffer full (capacity={}), {} events dropped total",
                            self.queue_name, self.overflow_capacity, count
                        );
                    }
                    return;
                }
                state.overflow.push_back(event);
                return;
            }

            match self.tx.try_send(event) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                    state.draining = true;
                    if state.overflow.len() >= self.overflow_capacity {
                        let count = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                        if count == 1 || count.is_multiple_of(1024) {
                            warn!(
                                "{} overflow buffer full (capacity={}), {} events dropped total",
                                self.queue_name, self.overflow_capacity, count
                            );
                        }
                        state.draining = false;
                        return;
                    }
                    state.overflow.push_back(event);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_event)) => {
                    debug!("{} channel closed, dropping event", self.queue_name);
                    return;
                }
            }
        }

        let tx = self.tx.clone();
        let state = Arc::clone(&self.state);
        let queue_name = self.queue_name;
        if let Some(handle) = runtime {
            handle.spawn(async move {
                Self::drain_async(&tx, &state, queue_name).await;
            });
        } else {
            thread::spawn(move || Self::drain_blocking(&tx, &state, queue_name));
        }
    }

    async fn drain_async(tx: &mpsc::Sender<T>, state: &Arc<Mutex<BackpressureState<T>>>, queue_name: &'static str) {
        loop {
            let Some(event) = Self::next_event(state) else {
                break;
            };
            if tx.send(event).await.is_err() {
                debug!("{queue_name} channel closed while draining backpressure");
                Self::clear_and_stop(state);
                break;
            }
        }
    }

    fn drain_blocking(tx: &mpsc::Sender<T>, state: &Arc<Mutex<BackpressureState<T>>>, queue_name: &'static str) {
        loop {
            let Some(event) = Self::next_event(state) else {
                break;
            };
            if tx.blocking_send(event).is_err() {
                warn!("{queue_name} channel closed while draining backpressure");
                Self::clear_and_stop(state);
                break;
            }
        }
    }

    fn next_event(state: &Arc<Mutex<BackpressureState<T>>>) -> Option<T> {
        let mut state = lock_backpressure_state(state.as_ref());
        if let Some(event) = state.overflow.pop_front() {
            return Some(event);
        }
        state.draining = false;
        None
    }

    fn clear_and_stop(state: &Arc<Mutex<BackpressureState<T>>>) {
        let mut state = lock_backpressure_state(state.as_ref());
        state.overflow.clear();
        state.draining = false;
    }

    fn dropped_count(&self) -> u64 { self.dropped_events.load(Ordering::Relaxed) }
}

fn lock_backpressure_state<T>(state: &Mutex<BackpressureState<T>>) -> MutexGuard<'_, BackpressureState<T>> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Backpressure queue state was poisoned, continuing with recovered state");
            poisoned.into_inner()
        }
    }
}

#[derive(Clone)]
struct SocketActivityTracker {
    pending: Arc<Mutex<HashSet<SocketActivityEvent>>>,
    notify: Arc<Notify>,
}

impl SocketActivityTracker {
    fn new() -> Self { Self { pending: Arc::new(Mutex::new(HashSet::new())), notify: Arc::new(Notify::new()) } }

    fn track(&self, event: SocketActivityEvent) {
        let mut pending = lock_socket_activity_pending(self.pending.as_ref());
        pending.insert(event);
        drop(pending);
        self.notify.notify_one();
    }

    fn drain(&self) -> Vec<SocketActivityEvent> {
        let mut pending = lock_socket_activity_pending(self.pending.as_ref());
        pending.drain().collect()
    }

    async fn notified(&self) { self.notify.notified().await; }
}

fn lock_socket_activity_pending(
    pending: &Mutex<HashSet<SocketActivityEvent>>,
) -> MutexGuard<'_, HashSet<SocketActivityEvent>> {
    pending.lock().unwrap_or_else(|poisoned| {
        warn!("Socket activity state was poisoned, continuing with recovered state");
        poisoned.into_inner()
    })
}

struct CleanupWorkerDeps {
    user_manager: Arc<ActiveUserManager>,
    provider_manager: Arc<ActiveProviderManager>,
    shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    capacity_notify: Arc<Notify>,
    history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
}

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

async fn release_connection_with_reason(
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

async fn release_connection_parts(
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
                deps.provider_manager.clear_provider_reservation(session_token);
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

async fn handle_update_detail_and_release_provider(
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SocketExpiryEntry {
    expires_at: u64,
    addr: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum SocketActivityEvent {
    HttpActivity { addr: SocketAddr },
    DirectBodyActivity { addr: SocketAddr },
}

impl SocketActivityEvent {
    fn addr(&self) -> SocketAddr {
        match self {
            Self::HttpActivity { addr, .. } | Self::DirectBodyActivity { addr, .. } => *addr,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CloseConnectionSignal {
    WithReason(SocketAddr, DisconnectReason),
}

pub struct ConnectionManager {
    pub user_manager: Arc<ActiveUserManager>,
    pub provider_manager: Arc<ActiveProviderManager>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    close_socket_signal_tx: tokio::sync::broadcast::Sender<CloseConnectionSignal>,
    cleanup_sender: BackpressureSender<CleanupEvent>,
    control_cleanup_tx: mpsc::Sender<CleanupEvent>,
    socket_activity_tracker: SocketActivityTracker,
    capacity_notify: Arc<Notify>,
    stream_uid_counter: AtomicU32,
    history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
    is_shutting_down: AtomicBool,
    admission_gate: RwLock<()>,
    shutdown_token: CancellationToken,
    worker_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

pub struct ConnectionParams<'a> {
    pub meter_uid: u32,
    pub username: &'a str,
    pub max_connections: u32,
    pub soft_connections: u16,
    pub connection_kind: crate::active_provider_manager::ConnectionKind,
    pub priority: i8,
    pub soft_priority: i8,
    pub fingerprint: &'a Fingerprint,
    pub provider: Arc<str>,
    pub stream_channel: &'a StreamChannel,
    pub user_agent: Cow<'a, str>,
    pub session_token: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionHistoryMode {
    EmitConnect,
    RefreshOnly,
}

/// Guaranteed final cleanup for a registered request, transferred into the body.
///
/// The permit was reserved before the registration mutation, so the release is
/// delivered even under cleanup-queue pressure. The body owns this and calls
/// [`OwnedRequestCleanup::finish`] exactly once with the real provider outcome; an
/// un-polled body drops it and gets a conservative release instead of a successful EOF.
pub struct OwnedRequestCleanup {
    addr: SocketAddr,
    request_uid: u32,
    /// Provider request identity captured at acquire, so an automatic rollback can
    /// finish the provider lease even when the user claim was never fully registered.
    provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    /// Playback owner (session token) captured at acquire. The provider finish must not
    /// depend on the user claim still existing, so the owner travels with the cleanup.
    owner: Option<Arc<str>>,
    permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
    finished: bool,
}

impl OwnedRequestCleanup {
    /// Releases the reserved cleanup right without emitting any release.
    fn disarm(&mut self) {
        self.finished = true;
        self.permit = None;
    }

    /// Releases the request claim exactly once with the actual provider outcome.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &mut self,
        request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        provider_end_reason: u8,
        reconnect_count: u8,
        provider_error_class: Option<&'static str>,
        provider_http_status: Option<u16>,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(permit) = self.permit.take() else {
            return;
        };
        permit.send(CleanupEvent::ReleaseStream {
            request_id,
            owner: self.owner.clone(),
            addr: self.addr,
            stream_uid: Some(self.request_uid),
            provider_end_reason,
            reconnect_count,
            provider_error_class,
            provider_http_status,
        });
    }
}

impl Drop for OwnedRequestCleanup {
    fn drop(&mut self) {
        // Conservative fallback for a body that is dropped without an explicit finish:
        // never invent a successful EOF, but still finish the provider request with its
        // captured identity so an abandoned start does not linger until the startup TTL.
        self.finish(self.provider_request_id, PROVIDER_END_NOT_SET, 0, None, None);
    }
}

/// Why a playback request was rejected before a body could be created. Carried on
/// `RegisteredPlaybackRequest` so the caller can surface a non-success HTTP response
/// instead of silently dropping the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRejectionReason {
    CleanupReceiverClosed,
    CleanupAdmissionTimeout,
    RegistrationFailed,
}

impl std::fmt::Display for ConnectionRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CleanupReceiverClosed => write!(f, "cleanup receiver closed"),
            Self::CleanupAdmissionTimeout => write!(f, "cleanup admission timed out"),
            Self::RegistrationFailed => write!(f, "connection registration failed"),
        }
    }
}

impl std::error::Error for ConnectionRejectionReason {}

pub struct RegisteredPlaybackRequest {
    pub request_uid: u32,
    pub display_stream: Option<StreamInfo>,
    cleanup: Option<OwnedRequestCleanup>,
    rejection: Option<ConnectionRejectionReason>,
}

impl std::fmt::Debug for RegisteredPlaybackRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredPlaybackRequest")
            .field("request_uid", &self.request_uid)
            .field("display_stream", &self.display_stream)
            .field("rejection", &self.rejection)
            .finish_non_exhaustive()
    }
}

impl RegisteredPlaybackRequest {
    #[inline]
    pub fn new(request_uid: u32, display_stream: Option<StreamInfo>) -> Self {
        Self { request_uid, display_stream, cleanup: None, rejection: None }
    }

    #[inline]
    fn rejected(request_uid: u32, reason: ConnectionRejectionReason) -> Self {
        Self { request_uid, display_stream: None, cleanup: None, rejection: Some(reason) }
    }

    #[inline]
    fn with_cleanup(
        request_uid: u32,
        display_stream: Option<StreamInfo>,
        cleanup: Option<OwnedRequestCleanup>,
    ) -> Self {
        Self { request_uid, display_stream, cleanup, rejection: None }
    }

    /// Marks the request as taken over by a path without a body (compatibility API or
    /// shared source), disabling rollback on drop.
    #[inline]
    pub fn commit(&mut self) {
        if let Some(mut cleanup) = self.cleanup.take() {
            cleanup.disarm();
        }
    }

    /// Transfers the guaranteed cleanup right into the body. The body owns the returned
    /// value and must finish it exactly once with the real provider outcome.
    #[inline]
    pub fn into_body_cleanup(&mut self) -> Option<OwnedRequestCleanup> { self.cleanup.take() }

    #[inline]
    pub fn display_uid(&self) -> Option<u32> { self.display_stream.as_ref().map(|s| s.uid) }

    #[inline]
    pub fn rejection_reason(&self) -> Option<ConnectionRejectionReason> { self.rejection }

    #[inline]
    pub fn into_display_stream(self) -> Option<StreamInfo> { self.display_stream }
}

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

    fn spawn_socket_activity_worker(
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

    async fn drain_pending_socket_activity(
        activity_tracker: &SocketActivityTracker,
        expiry_queue: &mut BinaryHeap<Reverse<SocketExpiryEntry>>,
        expiry_index: &mut HashMap<SocketAddr, u64>,
        user_manager: &Arc<ActiveUserManager>,
    ) {
        for event in activity_tracker.drain() {
            Self::handle_socket_activity_event(event, expiry_queue, expiry_index, user_manager).await;
        }
    }

    async fn handle_socket_activity_event(
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
            .map(|(addr, expires_at)| Reverse(SocketExpiryEntry { expires_at: *expires_at, addr: *addr }))
            .collect();
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn spawn_cleanup_worker(
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

    pub fn get_close_connection_channel(&self) -> tokio::sync::broadcast::Receiver<CloseConnectionSignal> {
        self.close_socket_signal_tx.subscribe()
    }

    pub async fn kick_connection(&self, addr: &SocketAddr, virtual_id: VirtualId, block_secs: u64) -> bool {
        debug_if_enabled!(
            "User {} kicked for stream with virtual_id {virtual_id} for {block_secs} seconds with addr {}.",
            self.user_manager.get_username_for_addr(addr).await.unwrap_or_default(),
            sanitize_sensitive_info(&addr.to_string())
        );
        self.close_connection_with_reason_and_block(addr, virtual_id, block_secs, DisconnectReason::ClientKicked).await
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
        if let Err(e) = self.close_socket_signal_tx.send(CloseConnectionSignal::WithReason(*addr, reason)) {
            debug_if_enabled!(
                "No active receivers for close signal ({}): {e:?}",
                sanitize_sensitive_info(&addr.to_string())
            );
            return false;
        }
        true
    }

    pub fn close_connection_signal(&self, addr: &SocketAddr) -> bool {
        self.close_connection_with_reason(addr, DisconnectReason::ClientClosed)
    }

    pub async fn block_stream_by_uid(&self, uid: u32, virtual_id: VirtualId, block_secs: u64) {
        self.user_manager.block_user_for_stream_uid(uid, virtual_id, block_secs).await;
    }

    pub fn close_connection_with_reason(&self, addr: &SocketAddr, reason: DisconnectReason) -> bool {
        if let Err(e) = self.close_socket_signal_tx.send(CloseConnectionSignal::WithReason(*addr, reason)) {
            debug_if_enabled!(
                "No active receivers for close signal ({}): {e:?}",
                sanitize_sensitive_info(&addr.to_string())
            );
            return false;
        }
        true
    }

    pub async fn release_connection(&self, addr: &SocketAddr) {
        release_connection_with_reason(self, addr, DisconnectReason::ClientClosed, true).await;
    }

    pub async fn release_connection_with_reason(&self, addr: &SocketAddr, reason: DisconnectReason) {
        release_connection_with_reason(self, addr, reason, true).await;
    }

    pub async fn release_connection_as_kicked(&self, addr: &SocketAddr) {
        let _ = self.close_connection_with_reason(addr, DisconnectReason::ClientKicked);
        release_connection_with_reason(self, addr, DisconnectReason::ClientKicked, true).await;
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
                self.provider_manager.clear_provider_reservation(session_token);
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
        self.update_connection_with_uid_impl(update, history_mode, uid, provider_request_id, false).await
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
        self.update_connection_with_uid_impl(update, history_mode, capability.stream_uid(), provider_request_id, true)
            .await
    }

    async fn update_connection_with_uid_impl(
        &self,
        update: ConnectionParams<'_>,
        history_mode: ConnectionHistoryMode,
        uid: u32,
        provider_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
        cleanup_owned_by_shared_subscriber: bool,
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
            .update_connection(ActiveUserConnectionParams {
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
            })
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

/// Async variant used on hot config reload: recovery I/O and compression run on the blocking
/// pool so a runtime worker is never blocked while streams are being served.
async fn build_history_writer_async(config: Option<&StreamHistoryConfig>) -> Option<Arc<StreamHistoryWriter>> {
    let cfg = config?;
    if !cfg.stream_history_enabled {
        return None;
    }
    let directory = cfg.stream_history_directory.clone();
    match tokio::task::spawn_blocking(move || recover_pending_files(&directory)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("Stream history recovery failed: {e}"),
        Err(join_err) => log::warn!("Stream history recovery task panicked: {join_err}"),
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
    if stream_info.provider.as_ref() == "tuliprox" {
        if let Ok(video_type) = CustomVideoStreamType::from_str(&stream_info.channel.title) {
            match video_type {
                CustomVideoStreamType::LowPriorityPreempted => return DisconnectReason::Preempted,
                CustomVideoStreamType::UserConnectionsExhausted => return DisconnectReason::UserConnectionsExhausted,
                CustomVideoStreamType::ProviderConnectionsExhausted => {
                    return DisconnectReason::ProviderConnectionsExhausted
                }
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

/// Resolves a disconnect reason from the provider-end signal alone, for the path where
/// the user stream claim is already gone and no `StreamInfo` is available to consult.
fn resolve_disconnect_reason_from_provider_end(provider_end_reason: u8) -> DisconnectReason {
    match provider_end_reason {
        PROVIDER_END_CLOSED => DisconnectReason::ProviderClosed,
        PROVIDER_END_ERROR => DisconnectReason::ProviderError,
        _ => DisconnectReason::ClientClosed,
    }
}

/// Maps a disconnect reason onto the provider-lease outcome policy.
///
/// Only a clean client-side end keeps a reconnect-capable lease alive; provider
/// failures, preemption, kicks and timeouts release capacity immediately.
fn playback_outcome_for_reason(reason: DisconnectReason) -> PlaybackRequestOutcome {
    match reason {
        DisconnectReason::ClientClosed
        | DisconnectReason::DayRollover
        | DisconnectReason::Cleanup
        | DisconnectReason::Unknown => PlaybackRequestOutcome::ClientClosed,
        DisconnectReason::Timeout => PlaybackRequestOutcome::TimedOut,
        DisconnectReason::SessionExpired => PlaybackRequestOutcome::SessionExpired,
        DisconnectReason::Shutdown => PlaybackRequestOutcome::ServerShutdown,
        DisconnectReason::ClientKicked => PlaybackRequestOutcome::Kicked,
        DisconnectReason::Preempted => PlaybackRequestOutcome::Preempted,
        DisconnectReason::Provisioning
        | DisconnectReason::UserConnectionsExhausted
        | DisconnectReason::ProviderConnectionsExhausted => PlaybackRequestOutcome::FailedBeforeMedia,
        DisconnectReason::ProviderError
        | DisconnectReason::ProviderClosed
        | DisconnectReason::ServerError
        | DisconnectReason::IntermediateFailures(_) => PlaybackRequestOutcome::ProviderFailed,
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
    reason: DisconnectReason,
    qos: &DisconnectQos,
    provider_error_class: Option<&str>,
    provider_http_status: Option<u16>,
) {
    let guard = writer.load();
    let Some(w) = guard.as_ref() else { return };
    w.send_record(
        StreamHistoryRecord::from_disconnect(info, reason, qos, resolve_disconnect_failure_stage(info, reason, qos))
            .with_provider_failure(provider_http_status, provider_error_class),
    );
}

fn resolve_disconnect_failure_stage(
    info: &StreamInfo,
    reason: DisconnectReason,
    qos: &DisconnectQos,
) -> Option<FailureStage> {
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
    use crate::{ActiveProviderManager, ActiveUserManager, CreateUserSessionParams, EventManager, SharedStreamManager};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{
            ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, ProxyType, StreamChannel, StreamInfo,
            UserConnectionPermission, XtreamCluster,
        },
        utils::Internable,
    };
    use std::{collections::HashMap, net::SocketAddr, sync::Arc};
    use tokio::sync::mpsc;
    use tuliprox_core::{
        model::{AppConfig, Config, ConfigInput, MediaToolCapabilities, ProxyUserCredentials, SourcesConfig},
        utils::FileLockManager,
    };
    use tuliprox_repository::GeoIp;

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
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        };
        StreamInfo::new(shared::model::StreamInfoParams {
            uid: 0,
            meter_uid: 0,
            username: "test",
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: provider.intern(),
            stream_channel: channel,
            user_agent: String::new(),
            country_code: None,
            session_token: None,
        })
    }

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_connection_manager() -> Arc<ConnectionManager> {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
        provider_manager.set_shared_stream_manager(&shared_manager);

        let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));

        Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, &event_manager, None))
    }

    fn create_test_proxy_user(username: &str) -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.password = "password".to_string();
        user.proxy = ProxyType::Reverse(None);
        user.max_connections = 1;
        user
    }

    #[tokio::test]
    async fn enqueue_with_backpressure_delivers_events_after_queue_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = BackpressureSender::new(tx.clone(), "test", 2);
        assert!(tx.send(1_u8).await.is_ok());

        sender.enqueue(2_u8);
        sender.enqueue(3_u8);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(2));
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(3));
    }

    #[tokio::test]
    async fn enqueue_with_backpressure_bounds_overflow_buffer() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = BackpressureSender::new(tx.clone(), "test", 1);
        assert!(tx.send(1_u8).await.is_ok());

        sender.enqueue(2_u8);
        for _ in 0..50 {
            let ready = {
                let state = super::lock_backpressure_state(sender.state.as_ref());
                state.overflow.is_empty() && state.draining
            };
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sender.enqueue(3_u8);
        sender.enqueue(4_u8);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(2));
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(3));
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err());
    }

    #[test]
    fn socket_activity_tracker_coalesces_updates_per_socket() {
        let tracker = SocketActivityTracker::new();
        let addr_one: SocketAddr = "127.0.0.1:3234".parse().unwrap_or_else(|_| unreachable!());
        let addr_two: SocketAddr = "127.0.0.1:3235".parse().unwrap_or_else(|_| unreachable!());

        tracker.track(SocketActivityEvent::HttpActivity { addr: addr_one });
        tracker.track(SocketActivityEvent::HttpActivity { addr: addr_one });
        tracker.track(SocketActivityEvent::DirectBodyActivity { addr: addr_two });

        let pending = tracker.drain();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|event| matches!(
            event,
            SocketActivityEvent::HttpActivity { addr } if *addr == addr_one
        )));
        assert!(pending.iter().any(|event| matches!(
            event,
            SocketActivityEvent::DirectBodyActivity { addr } if *addr == addr_two
        )));
    }

    #[tokio::test]
    async fn touch_http_activity_is_processed_by_socket_activity_worker() {
        let manager = create_test_connection_manager();
        let addr: SocketAddr = "127.0.0.1:3234".parse().unwrap_or_else(|_| unreachable!());
        let user = create_test_proxy_user("user1");

        manager.add_connection(&addr).await;
        let _ = manager
            .user_manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-touch",
                virtual_id: 1,
                provider: "provider_1",
                stream_url: "http://provider-1.example/live.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        manager.touch_http_activity(&user.username, "tok-touch", &addr).await;

        assert!(tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.user_manager.socket_expiry_deadline(&addr).await.is_some()
                    && manager.user_manager.get_username_for_addr(&addr).await.as_deref()
                        == Some(user.username.as_str())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn kick_connection_sends_kick_close_signal() {
        let manager = create_test_connection_manager();
        let mut rx = manager.get_close_connection_channel();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());

        assert!(manager.kick_connection(&addr, shared::model::VirtualId::new(1), 0).await);
        assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked)));
    }

    #[tokio::test]
    async fn release_connection_as_kicked_sends_kick_close_signal() {
        let manager = create_test_connection_manager();
        let mut rx = manager.get_close_connection_channel();
        let addr: SocketAddr = "127.0.0.1:2234".parse().unwrap_or_else(|_| unreachable!());

        manager.release_connection_as_kicked(&addr).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await.ok().and_then(Result::ok),
            Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientKicked))
        );
    }

    #[tokio::test]
    async fn close_connection_signal_sends_generic_close_signal() {
        let manager = create_test_connection_manager();
        let mut rx = manager.get_close_connection_channel();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());

        assert!(manager.close_connection_signal(&addr));
        assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::ClientClosed)));
    }

    #[tokio::test]
    async fn provisioning_close_connection_sends_provisioning_signal() {
        let manager = create_test_connection_manager();
        let mut rx = manager.get_close_connection_channel();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());

        assert!(
            manager
                .close_connection_with_reason_and_block(
                    &addr,
                    shared::model::VirtualId::new(7),
                    0,
                    DisconnectReason::Provisioning
                )
                .await
        );
        assert_eq!(rx.recv().await.ok(), Some(CloseConnectionSignal::WithReason(addr, DisconnectReason::Provisioning)));
    }

    #[tokio::test]
    async fn low_priority_preempted_cleanup_blocks_same_user_stream_reentry() {
        let manager = create_test_connection_manager();
        let user = create_test_proxy_user("preempted-user");
        let addr: SocketAddr = "127.0.0.1:6234".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 409,
            title: "channel-409".intern(),
            ..make_stream_info("provider_1", "channel-409").channel
        };

        manager.add_connection(&addr).await;
        manager
            .user_manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preempted",
                virtual_id: channel.virtual_id,
                provider: "provider_1",
                stream_url: "http://provider-1.example/live/409.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        manager
            .user_manager
            .update_connection(ActiveUserConnectionParams {
                uid: 9001,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 9,
                soft_priority: 9,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("tok-preempted"),
            })
            .await;

        let deps = CleanupWorkerDeps {
            user_manager: Arc::clone(&manager.user_manager),
            provider_manager: Arc::clone(&manager.provider_manager),
            shared_stream_manager: Arc::clone(&manager.shared_stream_manager),
            event_manager: Arc::clone(&manager.event_manager),
            capacity_notify: Arc::clone(&manager.capacity_notify),
            history_writer: Arc::clone(&manager.history_writer),
        };

        handle_update_detail_and_release_provider(&deps, addr, CustomVideoStreamType::LowPriorityPreempted, None, None)
            .await;

        assert!(
            manager
                .user_manager
                .is_user_blocked_for_stream(&user.username, shared::model::VirtualId::new(channel.virtual_id))
                .await,
            "preempted playback should be blocked briefly to prevent immediate reconnect ping-pong"
        );
    }

    #[tokio::test]
    async fn low_priority_preempted_cleanup_blocks_same_user_hls_reentry() {
        let manager = create_test_connection_manager();
        let user = create_test_proxy_user("preempted-hls-user");
        let addr: SocketAddr = "127.0.0.1:6235".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let mut channel = make_stream_info("provider_1", "channel-410").channel;
        channel.virtual_id = 410;
        channel.item_type = PlaylistItemType::LiveHls;
        channel.title = "channel-410.m3u8".intern();
        channel.url = "http://provider-1.example/live/410.m3u8".intern();

        manager.add_connection(&addr).await;
        manager
            .user_manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preempted-hls",
                virtual_id: channel.virtual_id,
                provider: "provider_1",
                stream_url: "http://provider-1.example/live/410.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .user_manager
            .update_connection(ActiveUserConnectionParams {
                uid: 9002,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 9,
                soft_priority: 9,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("tok-preempted-hls"),
            })
            .await;

        let deps = CleanupWorkerDeps {
            user_manager: Arc::clone(&manager.user_manager),
            provider_manager: Arc::clone(&manager.provider_manager),
            shared_stream_manager: Arc::clone(&manager.shared_stream_manager),
            event_manager: Arc::clone(&manager.event_manager),
            capacity_notify: Arc::clone(&manager.capacity_notify),
            history_writer: Arc::clone(&manager.history_writer),
        };

        handle_update_detail_and_release_provider(&deps, addr, CustomVideoStreamType::LowPriorityPreempted, None, None)
            .await;

        assert!(
            manager
                .user_manager
                .is_user_blocked_for_stream(&user.username, shared::model::VirtualId::new(channel.virtual_id))
                .await,
            "preempted HLS playback should be blocked briefly to prevent immediate reconnect ping-pong"
        );
    }

    #[test]
    fn test_client_closed_when_no_provider_end() {
        let info = make_stream_info("some_provider", "Some Channel");
        let reason = resolve_disconnect_reason(PROVIDER_END_NOT_SET, &info);
        assert_eq!(reason, DisconnectReason::ClientClosed);
    }

    #[test]
    fn test_client_kicked_disconnect_has_no_failure_stage() {
        assert_eq!(
            resolve_disconnect_failure_stage(
                &make_stream_info("some_provider", "Some Channel"),
                DisconnectReason::ClientKicked,
                &DisconnectQos::default(),
            ),
            None
        );
    }

    #[test]
    fn test_provisioning_disconnect_has_no_failure_stage() {
        assert_eq!(
            resolve_disconnect_failure_stage(
                &make_stream_info("some_provider", "Some Channel"),
                DisconnectReason::Provisioning,
                &DisconnectQos::default(),
            ),
            None
        );
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
                DisconnectReason::ProviderError,
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
                DisconnectReason::SessionExpired,
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
                DisconnectReason::ProviderError,
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
            resolve_disconnect_failure_stage(&info, DisconnectReason::ProviderError, &DisconnectQos::default(),),
            Some(FailureStage::Streaming)
        );
    }

    #[tokio::test]
    async fn deferred_cleanup_executes_in_background_worker() {
        let conn_manager = create_test_connection_manager();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let ran_clone = Arc::clone(&ran);
        let notify_clone = Arc::clone(&notify);

        conn_manager.send_cleanup(CleanupEvent::Defer(Box::pin(async move {
            ran_clone.store(true, Ordering::SeqCst);
            notify_clone.notify_one();
        })));

        tokio::time::timeout(Duration::from_secs(1), notify.notified()).await.expect("deferred task must execute");
        assert!(ran.load(Ordering::SeqCst));
    }

    /// Dropping a just-registered request without handing it to a body must roll the
    /// claim back, so a cancellation between registration and body construction cannot
    /// leak a user claim.
    #[tokio::test]
    async fn registration_rollback_releases_claim_when_body_never_constructed() {
        let manager = create_test_connection_manager();
        let addr: SocketAddr = "127.0.0.1:56234".parse().unwrap();
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 410,
            title: "channel-410".intern(),
            ..make_stream_info("provider_1", "channel-410").channel
        };

        manager.add_connection(&addr).await;
        let registered = manager
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "rollback-user",
                    max_connections: 1,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &fingerprint,
                    provider: "provider_1".intern(),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: None,
                },
                ConnectionHistoryMode::EmitConnect,
                1,
                None,
            )
            .await;
        assert!(registered.display_stream.is_some(), "registration must succeed");

        // Drop without committing: the rollback owner releases the claim.
        drop(registered);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if manager.user_manager.active_streams().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the rollback must release the claim");
    }

    /// The body cleanup transferred via `into_body_cleanup` releases the claim exactly
    /// once with the real outcome, even under cleanup-queue pressure.
    #[tokio::test]
    async fn body_cleanup_finish_releases_claim_exactly_once() {
        let manager = create_test_connection_manager();
        let addr: SocketAddr = "127.0.0.1:56236".parse().unwrap();
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 412,
            title: "channel-412".intern(),
            ..make_stream_info("provider_1", "channel-412").channel
        };

        manager.add_connection(&addr).await;
        let mut registered = manager
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "body-cleanup-user",
                    max_connections: 1,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &fingerprint,
                    provider: "provider_1".intern(),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: None,
                },
                ConnectionHistoryMode::EmitConnect,
                1,
                None,
            )
            .await;
        assert!(registered.display_stream.is_some(), "registration must succeed");

        let mut cleanup = registered.into_body_cleanup().expect("body cleanup present");
        cleanup.finish(None, PROVIDER_END_CLOSED, 0, None, None);
        cleanup.finish(None, PROVIDER_END_CLOSED, 0, None, None);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if manager.user_manager.active_streams().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the body cleanup must release the claim exactly once");
    }

    /// A saturated cleanup queue must reject registration with an admission-timeout reason
    /// (rather than hanging), so the caller can surface a defined non-success response.
    #[tokio::test(start_paused = true)]
    async fn cleanup_admission_timeout_rejects_registration_with_reason() {
        let manager = create_test_connection_manager();
        // Stall the cleanup worker on a never-resolving defer, then fill the channel so
        // `reserve_owned()` blocks and the bounded admission deadline fires.
        let pending = || CleanupEvent::Defer(Box::pin(std::future::pending::<()>()));
        manager.send_cleanup(pending());
        for _ in 0..CLEANUP_QUEUE_CAPACITY {
            manager.send_cleanup(pending());
        }

        let addr: SocketAddr = "127.0.0.1:56242".parse().unwrap();
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 414,
            title: "channel-414".intern(),
            ..make_stream_info("provider_1", "channel-414").channel
        };

        manager.add_connection(&addr).await;
        let manager_for_task = Arc::clone(&manager);
        let registration = tokio::spawn(async move {
            manager_for_task
                .update_connection_with_uid(
                    ConnectionParams {
                        meter_uid: 0,
                        username: "admission-timeout-user",
                        max_connections: 1,
                        soft_connections: 0,
                        connection_kind: crate::ConnectionKind::Normal,
                        priority: 0,
                        soft_priority: 0,
                        fingerprint: &fingerprint,
                        provider: "provider_1".intern(),
                        stream_channel: &channel,
                        user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                        session_token: None,
                    },
                    ConnectionHistoryMode::EmitConnect,
                    1,
                    None,
                )
                .await
        });

        // Let the task reach the blocked cleanup reservation, then fire the deadline.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(CLEANUP_ADMISSION_TIMEOUT + Duration::from_secs(1)).await;
        let registered = registration.await.expect("registration task must not panic");

        assert!(registered.display_stream.is_none(), "saturated cleanup admission must be rejected");
        assert_eq!(registered.rejection_reason(), Some(ConnectionRejectionReason::CleanupAdmissionTimeout));
    }

    #[tokio::test]
    async fn control_cleanup_lane_precedes_a_ready_body_cleanup_backlog() {
        let manager = create_test_connection_manager();
        let release_blocker = Arc::new(Notify::new());
        let (blocker_started_tx, blocker_started_rx) = tokio::sync::oneshot::channel();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        manager.send_cleanup(CleanupEvent::Defer(Box::pin({
            let release_blocker = Arc::clone(&release_blocker);
            let order = Arc::clone(&order);
            async move {
                let _ = blocker_started_tx.send(());
                release_blocker.notified().await;
                order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('B');
            }
        })));
        blocker_started_rx.await.expect("cleanup blocker must start");

        manager.send_cleanup(CleanupEvent::Defer(Box::pin({
            let order = Arc::clone(&order);
            async move { order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('N') }
        })));
        manager
            .control_cleanup_tx()
            .send(CleanupEvent::Defer(Box::pin({
                let order = Arc::clone(&order);
                async move { order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push('C') }
            })))
            .await
            .expect("control cleanup receiver must be open");

        release_blocker.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if order.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both queued cleanup events must run");

        assert_eq!(
            *order.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!['B', 'C', 'N'],
            "a ready control cleanup must not be starved by the body cleanup backlog"
        );
    }

    /// A joined shared body already holds the terminal cleanup permit. Registration must
    /// reuse that ownership instead of waiting for a second permit from the same full queue.
    #[tokio::test]
    async fn shared_admission_does_not_wait_for_second_permit_it_prevents() {
        let manager = create_test_connection_manager();
        let cleanup_tx = manager.cleanup_tx();
        let available = cleanup_tx.capacity();
        let mut permits = Vec::with_capacity(available);
        for _ in 0..available {
            permits.push(cleanup_tx.clone().try_reserve_owned().expect("cleanup permit"));
        }

        let addr: SocketAddr = "127.0.0.1:56243".parse().expect("test socket");
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 415,
            title: "channel-415".intern(),
            shared: true,
            shared_joined_existing: Some(true),
            ..make_stream_info("provider_1", "channel-415").channel
        };

        manager.add_connection(&addr).await;
        let mut registered = tokio::time::timeout(
            Duration::from_millis(100),
            manager.update_connection_with_uid_using_shared_cleanup(
                ConnectionParams {
                    meter_uid: 0,
                    username: "shared-admission-user",
                    max_connections: 1,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &fingerprint,
                    provider: "provider_1".intern(),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: None,
                },
                ConnectionHistoryMode::EmitConnect,
                SharedCleanupCapability::new(tuliprox_core::model::SharedSubscriberId::from_stream_uid(1)),
                None,
            ),
        )
        .await
        .expect("externally owned cleanup must not reserve another permit");

        assert!(registered.display_stream.is_some(), "shared registration must succeed");
        assert!(registered.into_body_cleanup().is_some(), "the body still tracks its terminal outcome");
        let _ = manager.user_manager.release_stream_request_by_uid(&addr, 1).await;
        drop(permits);
    }

    /// A cleanup whose user row is already gone must still finish the provider request,
    /// because the provider identity travels with the cleanup owner independently.
    #[tokio::test]
    async fn cleanup_finishes_provider_request_when_user_row_is_gone() {
        let manager = create_test_connection_manager();
        let addr: SocketAddr = "127.0.0.1:56240".parse().unwrap();
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 413,
            title: "channel-413".intern(),
            ..make_stream_info("provider_1", "channel-413").channel
        };
        let input_name = "provider_1".intern();
        let owner = "session-owner-g2";

        // Acquire a provider slot with an identified lease.
        let handle = manager
            .provider_manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &addr,
                false,
                0,
                crate::ConnectionKind::Normal,
                Some(owner),
            )
            .expect("acquire provider slot");
        let request_id = handle.playback_request_id.expect("identified request id");
        assert!(manager.provider_manager.provider_lease_usage(&input_name).total() > 0);

        // Register a user claim carrying the same owner and provider request id.
        manager.add_connection(&addr).await;
        let mut registered = manager
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "g2-user",
                    max_connections: 1,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &fingerprint,
                    provider: "provider_1".intern(),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: Some(owner),
                },
                ConnectionHistoryMode::EmitConnect,
                1,
                Some(request_id),
            )
            .await;
        assert!(registered.display_stream.is_some());

        let cleanup = registered.into_body_cleanup().expect("cleanup present");

        // Remove the user row first, so the cleanup worker hits the NotFound path.
        let _ = manager.user_manager.release_stream_request_by_uid(&addr, 1).await;

        drop(cleanup);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if manager.provider_manager.provider_lease_usage(&input_name).total() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider request must finish even without a user row");

        manager.provider_manager.release_handle(&handle);
    }

    /// Shutdown releases every active stream so no claim or provider slot survives.
    #[tokio::test]
    async fn shutdown_releases_active_streams() {
        let manager = create_test_connection_manager();
        let addr: SocketAddr = "127.0.0.1:56235".parse().unwrap();
        let fingerprint = tuliprox_core::model::Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let channel = StreamChannel {
            virtual_id: 411,
            title: "channel-411".intern(),
            ..make_stream_info("provider_1", "channel-411").channel
        };

        manager.add_connection(&addr).await;
        let stream_info = manager
            .update_connection(ConnectionParams {
                meter_uid: 0,
                username: "shutdown-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: None,
            })
            .await;
        assert!(stream_info.is_some(), "registration must succeed");
        assert!(!manager.user_manager.active_streams().await.is_empty());

        manager.shutdown().await;
        assert!(manager.user_manager.active_streams().await.is_empty(), "shutdown must release active streams");
    }

    #[tokio::test]
    async fn shutdown_parallel_requests_on_distinct_sockets_drains_all_claims() {
        let manager = create_test_connection_manager();
        let first_addr: SocketAddr = "127.0.0.1:56241".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:56242".parse().unwrap();
        let first_fingerprint =
            tuliprox_core::model::Fingerprint::new("range-first".to_string(), "127.0.0.1".to_string(), first_addr);
        let second_fingerprint =
            tuliprox_core::model::Fingerprint::new("range-second".to_string(), "127.0.0.1".to_string(), second_addr);
        let mut channel = make_stream_info("provider_1", "parallel-range").channel;
        channel.item_type = PlaylistItemType::Video;
        channel.cluster = XtreamCluster::Video;
        let owner = "shutdown-parallel-range";
        let input_name = "provider_1".intern();
        let handle = manager
            .provider_manager
            .acquire_connection_with_grace_for_session(
                &input_name,
                &first_addr,
                false,
                0,
                crate::ConnectionKind::Normal,
                Some(owner),
            )
            .expect("provider allocation");
        let request_id = handle.playback_request_id.expect("provider request id");

        manager.add_connection(&first_addr).await;
        manager.add_connection(&second_addr).await;
        let mut first = manager
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "shutdown-range-user",
                    max_connections: 2,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &first_fingerprint,
                    provider: Arc::clone(&input_name),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: Some(owner),
                },
                ConnectionHistoryMode::EmitConnect,
                1,
                Some(request_id),
            )
            .await;
        let mut second = manager
            .update_connection_with_uid(
                ConnectionParams {
                    meter_uid: 0,
                    username: "shutdown-range-user",
                    max_connections: 2,
                    soft_connections: 0,
                    connection_kind: crate::ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: &second_fingerprint,
                    provider: Arc::clone(&input_name),
                    stream_channel: &channel,
                    user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                    session_token: Some(owner),
                },
                ConnectionHistoryMode::EmitConnect,
                2,
                None,
            )
            .await;
        assert!(first.display_stream.is_some());
        assert!(second.display_stream.is_some());
        assert_eq!(manager.user_manager.active_streams().await.len(), 1);
        assert_eq!(manager.user_manager.playback_resource_counts().await.0, 2);
        assert_eq!(manager.provider_manager.get_provider_connections_count(), 1);

        let first_cleanup = first.into_body_cleanup().expect("first cleanup");
        let second_cleanup = second.into_body_cleanup().expect("second cleanup");
        manager.shutdown().await;

        assert_eq!(manager.user_manager.playback_resource_counts().await, (0, 0));
        assert_eq!(manager.provider_manager.get_provider_connections_count(), 0);
        assert_eq!(manager.provider_manager.provider_lease_usage(&input_name).total(), 0);

        drop(first_cleanup);
        drop(second_cleanup);
        drop(handle);
    }

    #[tokio::test]
    async fn shutdown_with_all_cleanup_permits_held_completes() {
        let manager = create_test_connection_manager();
        let mut permits = Vec::with_capacity(CLEANUP_QUEUE_CAPACITY);
        for _ in 0..CLEANUP_QUEUE_CAPACITY {
            permits.push(manager.cleanup_tx().reserve_owned().await.expect("cleanup receiver open"));
        }
        assert_eq!(manager.cleanup_tx().capacity(), 0);

        tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
            .await
            .expect("shutdown must not require cleanup queue capacity");
        drop(permits);
    }

    #[tokio::test]
    async fn new_with_capacity_sizes_the_cleanup_queue() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
        provider_manager.set_shared_stream_manager(&shared_manager);
        let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));

        let manager = Arc::new(ConnectionManager::new_with_capacity(
            &user_manager,
            &provider_manager,
            &shared_manager,
            &event_manager,
            None,
            2,
        ));
        assert_eq!(manager.cleanup_tx().capacity(), 2);

        // A capacity of zero is clamped to one so the cleanup queue is always usable.
        let manager = Arc::new(ConnectionManager::new_with_capacity(
            &user_manager,
            &provider_manager,
            &shared_manager,
            &event_manager,
            None,
            0,
        ));
        assert_eq!(manager.cleanup_tx().capacity(), 1);
    }
}
