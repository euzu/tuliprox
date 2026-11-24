use crate::{ActiveProviderManager, ActiveUserManager, CleanupEvent};
use bytes::Bytes;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{oneshot, Mutex, Notify, RwLock},
    time::{Duration, Sleep},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{AllocationId, ProviderHandle, SharedSubscriberId};

const DEFAULT_SHARED_BUFFER_SIZE_BYTES: usize = 1024 * 1024 * 32;

const YIELD_COUNTER: usize = 64;

const MIN_BURST_BUFFER_CHUNKS: usize = 2;

const MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES: usize = 188;

const SHARED_BURST_BYTES_PER_BUFFER_SLOT: usize = 12 * 1024;

const DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS: u64 = 300;

const SHARED_CLEANUP_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

// Upper bound on chunks pulled into a subscriber's live batch per read. The burst ring
// is byte-bounded already; this caps how many of its entries are cloned into the
// per-subscriber vector at once so a long backlog cannot balloon that vector.
const MAX_SUBSCRIBER_LIVE_BATCH_CHUNKS: usize = 64;

/// Maximum lookback duration for random access point selection relative to the join target.
/// When the newest RAP before target is older than this limit, join snapshot falls back to time-based selection.
const MAX_RAP_LOOKBACK: Duration = Duration::from_secs(10);

pub struct PendingSharedSubscriberCleanup {
    permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
    subscriber_id: SharedSubscriberId,
    addr: SocketAddr,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
}

pub struct PendingSharedMeterRegistration {
    manager: Arc<SharedStreamManager>,
    stream_url: Arc<str>,
    meter_uid: u32,
    armed: bool,
}

/// Tracks a shared-stream meter reservation. `owner` is `None` while the meter is
/// only reserved; it becomes `Some(allocation_id)` once a live shared origin adopts it.
/// Pending rollbacks may only remove an entry that no origin has adopted yet.
struct SharedMeterEntry {
    uid: u32,
    owner: Option<AllocationId>,
}

/// RAII guard for a join that committed its registry entry but has not yet registered
/// its subscriber. It decrements the origin's pending-join counter exactly once, either
/// on successful registration (`commit`) or when the join future is dropped.
struct PendingJoinGuard {
    state: Arc<SharedStreamState>,
    armed: bool,
}

struct ReceiverStreamWrapper<S> {
    stream: S,
    start: Option<oneshot::Sender<()>>,
    subscriber_id: SharedSubscriberId,
    addr: SocketAddr,
    /// Guaranteed cleanup right reserved before subscriber registration, so the
    /// release below is never dropped by cleanup-queue pressure.
    permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
    deadline: Option<Pin<Box<Sleep>>>,
    released: bool,
}

#[derive(Clone, Debug)]
pub(super) struct SharedSubscriber {
    pub(super) addr: SocketAddr,
    pub(super) cancel_token: CancellationToken,
    pub(super) username: Arc<str>,
    pub(super) session_token: Arc<str>,
    pub(super) free_join: bool,
}

impl SharedSubscriber {
    /// A paid holder keeps the stream: a counted viewer or a recording (F10). Free joiners (E7) do not.
    pub(super) fn is_paid_holder(&self) -> bool { !self.free_join }
}

struct BufferedChunk {
    sequence: u64,
    start: u64,
    arrived: tokio::time::Instant,
    bytes: Bytes,
}

struct RandomAccessPoint {
    sequence: u64,
    offset: u32,
    arrived: tokio::time::Instant,
}

struct BurstBuffer {
    buffer: VecDeque<BufferedChunk>,
    buffer_size: usize,
    max_chunks: usize,
    current_bytes: usize,
    next_sequence: u64,
    next_start: u64,
    raps: VecDeque<RandomAccessPoint>,
}

struct BurstRead {
    next_sequence: u64,
    skipped: u64,
}

/// A queue entry that releases its byte-budget permit when the client consumes (drops) it.
struct BudgetedChunk {
    bytes: Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    /// The provider allocation feeding this stream; a group stream replaces it on a member switch.
    provider_guard: std::sync::Mutex<Option<ProviderHandle>>,
    low_priority_preempted: Option<tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer>,
    preempted_token: CancellationToken,
    subscribers: RwLock<HashMap<SubscriberId, SharedSubscriber>>,
    stop_token: CancellationToken,
    burst_buffer: Arc<Mutex<BurstBuffer>>,
    live_notification: Arc<Notify>,
    task_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    subscriber_idle_timeout_secs: u64,
    subscriber_max_duration: Option<Duration>,
    burst_duration: Duration,
    /// Number of joins that committed their registry entry but have not yet registered
    /// their subscriber in `subscribers`. Teardown must not remove an origin while this is
    /// non-zero, otherwise a join in flight would land on a stopped state.
    pending_joins: AtomicUsize,
}

#[derive(Debug, Clone, Default)]
struct SharedStreamsRegister {
    by_key: HashMap<Arc<str>, Arc<SharedStreamState>>,
    key_by_subscriber: HashMap<SubscriberId, Arc<str>>,
    free_join_owner_by_subscriber: HashMap<SubscriberId, Arc<str>>,
    free_joins_by_user: HashMap<Arc<str>, u32>,
    live_free_joins: Arc<AtomicUsize>,
}

impl SharedStreamsRegister {
    fn try_reserve_free_join(&mut self, subscriber_id: SubscriberId, username: &str, max: u32) -> bool {
        let current = self.free_joins_by_user.get(username).copied().unwrap_or(0);
        if current >= max {
            return false;
        }
        self.record_free_join(subscriber_id, username);
        true
    }

    fn record_free_join(&mut self, subscriber_id: SubscriberId, username: &str) {
        let user_key: Arc<str> = Arc::from(username);
        *self.free_joins_by_user.entry(Arc::clone(&user_key)).or_default() += 1;
        if self.free_join_owner_by_subscriber.insert(subscriber_id, user_key).is_none() {
            self.live_free_joins.fetch_add(1, Ordering::Release);
        }
    }

    pub(super) fn remove_stream(&mut self, stream_url: &str) -> Option<Arc<SharedStreamState>> {
        let removed_subs: Vec<SubscriberId> = self
            .key_by_subscriber
            .iter()
            .filter(|(_, url)| url.as_ref() == stream_url)
            .map(|(sub_id, _)| *sub_id)
            .collect();
        for sub_id in removed_subs {
            self.remove_free_join_for_subscriber(sub_id);
        }
        self.key_by_subscriber.retain(|_, url| url.as_ref() != stream_url);
        self.by_key.remove(stream_url)
    }

    fn remove_free_join_for_subscriber(&mut self, subscriber_id: SubscriberId) {
        if let Some(user_key) = self.free_join_owner_by_subscriber.remove(&subscriber_id) {
            if let Some(count) = self.free_joins_by_user.get_mut(&user_key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.free_joins_by_user.remove(&user_key);
                }
            }
            self.live_free_joins.fetch_sub(1, Ordering::Release);
        }
    }
}

pub struct SharedStreamManager {
    provider_manager: Arc<ActiveProviderManager>,
    shared_streams: RwLock<SharedStreamsRegister>,
    meter_uids: std::sync::Mutex<HashMap<Arc<str>, SharedMeterEntry>>,
    pub(super) user_manager: tokio::sync::OnceCell<std::sync::Weak<ActiveUserManager>>,
    live_free_joins: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) test_preflight_barrier: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

#[cfg(test)]
mod burst_tests;
#[cfg(test)]
mod tests;

mod broadcast;
mod buffer;
mod manager;
mod reader;
mod subscription;
use buffer::SubscriberId;
pub use manager::SharedStreamCtx;
