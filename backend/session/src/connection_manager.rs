use crate::{ActiveProviderManager, ActiveUserManager, EventManager, SharedStreamManager};
use arc_swap::ArcSwapOption;
use shared::model::StreamInfo;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::SharedSubscriberId;
use tuliprox_repository::StreamHistoryWriter;

// Maximum number of deferred cleanup actions buffered before producers must wait/drop.
const CLEANUP_QUEUE_CAPACITY: usize = 4096;

// Reserved lane for control work (HLS origin I/O, shutdown) so a saturated body-cleanup
// queue cannot starve the control path that releases provider handles.
const CONTROL_CLEANUP_CAPACITY: usize = 64;

pub const PROVIDER_END_NOT_SET: u8 = 0;

pub const PROVIDER_END_CLOSED: u8 = 1;

// Provider EOF
pub const PROVIDER_END_ERROR: u8 = 2;

// Provider Err
pub const PROVIDER_END_PREEMPTED: u8 = 3;

// Preempted by higher priority
const PREEMPT_REENTRY_BLOCK_SECS: u64 = 3;

// Bounded wait for a mandatory cleanup admission right before a request registration.
const CLEANUP_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

// Rebuild the expiry heap when it grows beyond this multiple of the live index size.
const SOCKET_EXPIRY_QUEUE_REBUILD_FACTOR: usize = 2;

// Avoid rebuilding the expiry heap unless it contains at least this many stale entries.
const SOCKET_EXPIRY_QUEUE_REBUILD_MIN_STALE: usize = 256;

/// Proof that a shared-stream response owns the guaranteed terminal cleanup permit for
/// its subscriber. The constructor is crate-private, so callers outside this crate cannot
/// fabricate the claim; it is only produced when a cleanup permit is actually reserved.
/// Each registration consumes one capability, so a stale capability cannot be duplicated
/// and reused after its owner has released.
#[derive(Debug)]
pub struct SharedCleanupCapability {
    subscriber_id: SharedSubscriberId,
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

#[derive(Clone)]
struct SocketActivityTracker {
    pending: Arc<Mutex<HashSet<SocketActivityEvent>>>,
    notify: Arc<Notify>,
}

struct CleanupWorkerDeps {
    user_manager: Arc<ActiveUserManager>,
    provider_manager: Arc<ActiveProviderManager>,
    shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    capacity_notify: Arc<Notify>,
    history_writer: Arc<ArcSwapOption<StreamHistoryWriter>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SocketExpiryEntry {
    expires_at: u64,
    addr: SocketAddr,
}

pub struct ConnectionManager {
    pub user_manager: Arc<ActiveUserManager>,
    pub provider_manager: Arc<ActiveProviderManager>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    event_manager: Arc<EventManager>,
    close_socket_signal_tx: tokio::sync::broadcast::Sender<CloseConnectionSignal>,
    socket_closers: std::sync::Mutex<HashMap<SocketAddr, SocketCloseState>>,
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

pub struct RegisteredPlaybackRequest {
    pub request_uid: u32,
    pub display_stream: Option<StreamInfo>,
    cleanup: Option<OwnedRequestCleanup>,
    rejection: Option<ConnectionRejectionReason>,
}

#[cfg(test)]
mod tests;

mod backpressure;
mod cleanup;
mod history;
mod manager;
mod registration;
mod requests;
mod socket;
#[cfg(test)]
use backpressure::lock_backpressure_state;
#[cfg(test)]
use cleanup::handle_update_detail_and_release_provider;
pub use cleanup::CleanupEvent;
#[cfg(test)]
use history::resolve_disconnect_failure_stage;
pub use requests::{ConnectionHistoryMode, ConnectionParams, ConnectionRejectionReason};
pub use socket::CloseConnectionSignal;
use socket::{SocketActivityEvent, SocketCloseState};
