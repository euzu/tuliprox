use crate::{
    streams::buffered_stream::CHANNEL_SIZE, ActiveProviderManager, BoxedProviderStream, CleanupEvent,
    ConnectionManager, ConnectionRejectionReason, ManagedProviderHandle, SharedCleanupCapability,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::{debug, warn};
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    fmt::{Debug, Formatter},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, mpsc::Sender, oneshot, Mutex, Notify, RwLock, Semaphore},
    time::{sleep, timeout, Duration, Instant, Sleep},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{AllocationId, AppConfig, Config, ProviderHandle, SharedSubscriberId, StreamError},
    utils::{debug_if_enabled, network::request::STREAM_IDLE_TIMEOUT, trace_if_enabled},
};

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

impl PendingSharedMeterRegistration {
    pub fn commit(mut self) { self.armed = false; }
}

impl Drop for PendingSharedMeterRegistration {
    fn drop(&mut self) {
        if self.armed {
            self.manager.remove_meter_uid_if_pending(&self.stream_url, self.meter_uid);
        }
    }
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

impl PendingJoinGuard {
    fn new(state: &Arc<SharedStreamState>) -> Self {
        state.increment_pending_joins();
        Self { state: Arc::clone(state), armed: true }
    }

    fn commit(mut self) {
        self.armed = false;
        self.state.decrement_pending_joins();
    }
}

impl Drop for PendingJoinGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.decrement_pending_joins();
        }
    }
}

impl PendingSharedSubscriberCleanup {
    fn new(
        permit: tokio::sync::mpsc::OwnedPermit<CleanupEvent>,
        subscriber_id: SharedSubscriberId,
        addr: SocketAddr,
    ) -> Self {
        Self { permit: Some(permit), subscriber_id, addr, request_id: None, owner: None }
    }

    pub fn set_provider_request_identity(&mut self, owner: &str, request_id: tuliprox_core::model::PlaybackRequestId) {
        self.owner = Some(Arc::from(owner));
        self.request_id = Some(request_id);
    }

    /// Produces the cleanup-ownership proof for this subscriber. The permit held by
    /// this guard is the source of the capability.
    pub fn capability(&self) -> SharedCleanupCapability { SharedCleanupCapability::new(self.subscriber_id) }

    fn take_cleanup(
        &mut self,
    ) -> (
        Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
        Option<tuliprox_core::model::PlaybackRequestId>,
        Option<Arc<str>>,
    ) {
        (self.permit.take(), self.request_id.take(), self.owner.take())
    }

    pub fn disarm(&mut self) { self.permit = None; }
}

impl Drop for PendingSharedSubscriberCleanup {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            permit.send(CleanupEvent::ReleaseSharedSubscriber {
                addr: self.addr,
                subscriber_id: self.subscriber_id,
                request_id: self.request_id,
                owner: self.owner.take(),
            });
        }
    }
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

impl<S> ReceiverStreamWrapper<S> {
    fn release(&mut self) {
        if !self.released {
            self.released = true;
            if let Some(permit) = self.permit.take() {
                permit.send(CleanupEvent::ReleaseSharedSubscriber {
                    addr: self.addr,
                    subscriber_id: self.subscriber_id,
                    request_id: self.request_id,
                    owner: self.owner.take(),
                });
            }
        }
    }
}

impl<S> Stream for ReceiverStreamWrapper<S>
where
    S: Stream<Item = Bytes> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.deadline.as_mut().is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready()) {
            self.release();
            return Poll::Ready(None);
        }
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn resolve_min_burst_buffer_bytes(config: &Config) -> usize {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|rp| rp.stream.as_ref())
        .and_then(|stream| usize::try_from(stream.shared_burst_buffer_mb.saturating_mul(1024 * 1024)).ok())
        .unwrap_or(DEFAULT_SHARED_BUFFER_SIZE_BYTES)
        .max(1)
}

impl<S> Drop for ReceiverStreamWrapper<S> {
    fn drop(&mut self) { self.release(); }
}

type SubscriberId = SharedSubscriberId;

#[derive(Clone, Debug)]
struct SharedSubscriber {
    addr: SocketAddr,
    cancel_token: CancellationToken,
}

struct BufferedChunk {
    sequence: u64,
    bytes: Bytes,
}

struct BurstBuffer {
    buffer: VecDeque<BufferedChunk>,
    buffer_size: usize,
    max_chunks: usize,
    current_bytes: usize,
    next_sequence: u64,
}

struct BurstRead {
    next_sequence: u64,
    skipped: u64,
}

#[allow(clippy::missing_fields_in_debug)]
impl Debug for BurstBuffer {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BurstBuffer")
            .field("buffer_size", &self.buffer_size)
            .field("max_chunks", &self.max_chunks)
            .field("current_bytes", &self.current_bytes)
            .finish()
    }
}

impl BurstBuffer {
    pub fn new(buf_size: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            buffer_size: buf_size,
            max_chunks: Self::max_chunks_for_buffer_size(buf_size),
            current_bytes: 0,
            next_sequence: 0,
        }
    }

    pub fn snapshot(&self) -> (Vec<Bytes>, u64) {
        (self.buffer.iter().map(|chunk| chunk.bytes.clone()).collect::<Vec<Bytes>>(), self.next_sequence)
    }

    pub fn read_from_into(&self, next_sequence: u64, chunks: &mut Vec<Bytes>, max_chunks: usize) -> BurstRead {
        chunks.clear();
        let earliest_sequence = self.buffer.front().map_or(self.next_sequence, |chunk| chunk.sequence);
        let start_sequence = next_sequence.max(earliest_sequence);
        let skipped = start_sequence.saturating_sub(next_sequence);
        let start_index = self.start_index_for_sequence(start_sequence);
        let mut read_next_sequence = start_sequence;
        for chunk in self.buffer.range(start_index..).take(max_chunks) {
            chunks.push(chunk.bytes.clone());
            read_next_sequence = chunk.sequence.saturating_add(1);
        }

        BurstRead { next_sequence: read_next_sequence, skipped }
    }

    pub fn push(&mut self, packet: Bytes) {
        let packet_len = packet.len();
        while !self.buffer.is_empty()
            && (self.buffer.len() >= self.max_chunks
                || self.current_bytes.saturating_add(packet_len) > self.buffer_size)
        {
            if let Some(popped) = self.buffer.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(popped.bytes.len());
            }
        }
        self.current_bytes = self.current_bytes.saturating_add(packet_len);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.buffer.push_back(BufferedChunk { sequence, bytes: packet });
    }

    fn start_index_for_sequence(&self, sequence: u64) -> usize {
        let mut left = 0_usize;
        let mut right = self.buffer.len();

        while left < right {
            let mid = left + ((right - left) / 2);
            let mid_sequence = self.buffer.get(mid).map_or(u64::MAX, |chunk| chunk.sequence);
            if mid_sequence < sequence {
                left = mid.saturating_add(1);
            } else {
                right = mid;
            }
        }

        left
    }

    fn max_chunks_for_buffer_size(buffer_size: usize) -> usize {
        buffer_size.div_ceil(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES).max(MIN_BURST_BUFFER_CHUNKS)
    }
}

/// Terminal outcome of trying to deliver one chunk to a subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
    Sent,
    Cancelled,
    Closed,
    TimedOut,
}

/// A queue entry that releases its byte-budget permit when the client consumes (drops) it.
struct BudgetedChunk {
    bytes: Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

async fn reserve_byte_permit(
    byte_budget: &Arc<Semaphore>,
    len: usize,
    cancellation_token: &CancellationToken,
    progress_deadline: Instant,
) -> Result<tokio::sync::OwnedSemaphorePermit, SendOutcome> {
    let permits = u32::try_from(len).unwrap_or(u32::MAX);
    tokio::select! {
        biased;
        () = cancellation_token.cancelled() => Err(SendOutcome::Cancelled),
        result = tokio::time::timeout_at(progress_deadline, byte_budget.clone().acquire_many_owned(permits)) => {
            match result {
                Ok(Ok(permit)) => Ok(permit),
                Ok(Err(_)) => Err(SendOutcome::Closed),
                Err(_) => Err(SendOutcome::TimedOut),
            }
        }
    }
}

async fn send_burst_buffer(
    start_buffer: &[Bytes],
    client_tx: &Sender<BudgetedChunk>,
    cancellation_token: &CancellationToken,
    idle_timeout: Duration,
    byte_budget: &Arc<Semaphore>,
) -> Result<usize, SendOutcome> {
    let mut sent = 0_usize;
    let mut last_progress = Instant::now();
    for buf in start_buffer {
        let deadline = last_progress + idle_timeout;
        match send_client_chunk(client_tx, buf.clone(), cancellation_token, deadline, byte_budget).await {
            SendOutcome::Sent => {
                sent = sent.saturating_add(1);
                last_progress = Instant::now();
            }
            outcome => return Err(outcome),
        }
    }
    Ok(sent)
}

async fn send_client_chunk(
    client_tx: &Sender<BudgetedChunk>,
    data: Bytes,
    cancellation_token: &CancellationToken,
    progress_deadline: Instant,
    byte_budget: &Arc<Semaphore>,
) -> SendOutcome {
    if cancellation_token.is_cancelled() {
        return SendOutcome::Cancelled;
    }

    let permit = match reserve_byte_permit(byte_budget, data.len(), cancellation_token, progress_deadline).await {
        Ok(permit) => permit,
        Err(outcome) => return outcome,
    };

    let chunk = BudgetedChunk { bytes: data, _permit: permit };
    match client_tx.try_send(chunk) {
        Ok(()) => SendOutcome::Sent,
        Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        Err(mpsc::error::TrySendError::Full(chunk)) => tokio::select! {
            biased;
            () = cancellation_token.cancelled() => SendOutcome::Cancelled,
            result = tokio::time::timeout_at(progress_deadline, client_tx.send(chunk)) => match result {
                Ok(Ok(())) => SendOutcome::Sent,
                Ok(Err(_)) => SendOutcome::Closed,
                Err(_) => SendOutcome::TimedOut,
            },
        },
    }
}

#[derive(Debug)]
pub struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    provider_guard: Option<ProviderHandle>,
    low_priority_preempted: Option<tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer>,
    preempted_token: CancellationToken,
    subscribers: RwLock<HashMap<SubscriberId, SharedSubscriber>>,
    stop_token: CancellationToken,
    burst_buffer: Arc<Mutex<BurstBuffer>>,
    live_notification: Arc<Notify>,
    task_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    subscriber_idle_timeout_secs: u64,
    subscriber_max_duration: Option<Duration>,
    /// Number of joins that committed their registry entry but have not yet registered
    /// their subscriber in `subscribers`. Teardown must not remove an origin while this is
    /// non-zero, otherwise a join in flight would land on a stopped state.
    pending_joins: AtomicUsize,
}

impl SharedStreamState {
    fn new(
        headers: Vec<(String, String)>,
        buf_size: usize,
        provider_guard: Option<ProviderHandle>,
        min_burst_buffer_size: usize,
        low_priority_preempted: Option<tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer>,
    ) -> Self {
        let base_channel_capacity = buf_size.max(CHANNEL_SIZE);
        let burst_buffer_size_in_bytes =
            min_burst_buffer_size.max(base_channel_capacity.saturating_mul(SHARED_BURST_BYTES_PER_BUFFER_SLOT));
        Self {
            headers,
            buf_size: base_channel_capacity,
            provider_guard,
            low_priority_preempted,
            preempted_token: CancellationToken::new(),
            subscribers: RwLock::new(HashMap::new()),
            stop_token: CancellationToken::new(),
            burst_buffer: Arc::new(Mutex::new(BurstBuffer::new(burst_buffer_size_in_bytes))),
            live_notification: Arc::new(Notify::new()),
            task_handles: std::sync::Mutex::new(Vec::new()),
            subscriber_idle_timeout_secs: DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS,
            subscriber_max_duration: None,
            pending_joins: AtomicUsize::new(0),
        }
    }

    fn with_subscriber_idle_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.subscriber_idle_timeout_secs = secs;
        }
        self
    }

    fn increment_pending_joins(&self) { self.pending_joins.fetch_add(1, Ordering::AcqRel); }

    fn decrement_pending_joins(&self) { self.pending_joins.fetch_sub(1, Ordering::AcqRel); }

    fn has_pending_joins(&self) -> bool { self.pending_joins.load(Ordering::Acquire) > 0 }

    fn lock_task_handles(&self) -> std::sync::MutexGuard<'_, Vec<tokio::task::JoinHandle<()>>> {
        self.task_handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn register_subscriber(&self, id: SubscriberId, addr: &SocketAddr, cancel_token: CancellationToken) {
        self.subscribers.write().await.insert(id, SharedSubscriber { addr: *addr, cancel_token });
    }

    async fn cancel_subscribers(&self) {
        for subscriber in self.subscribers.read().await.values() {
            subscriber.cancel_token.cancel();
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn subscribe(
        self: &Arc<Self>,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        connection_manager: Arc<ConnectionManager>,
        mut pending_cleanup: PendingSharedSubscriberCleanup,
        pending_join: PendingJoinGuard,
    ) -> (BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability) {
        let (client_tx, client_rx) = mpsc::channel::<BudgetedChunk>(self.buf_size);
        let cancel_token = CancellationToken::new();
        let queue_byte_budget = self.buf_size.saturating_mul(SHARED_BURST_BYTES_PER_BUFFER_SLOT);
        let byte_budget = Arc::new(Semaphore::new(queue_byte_budget));

        {
            let mut handles = self.lock_task_handles();
            handles.retain(|h| !h.is_finished());
        }

        self.register_subscriber(subscriber_id, addr, cancel_token.clone()).await;
        pending_join.commit();
        let (start_tx, start_rx) = oneshot::channel();
        let cleanup_manager = Arc::clone(&connection_manager);

        let client_tx_clone = client_tx.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let burst_buffer_for_log = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        let timeout_duration = Duration::from_secs(self.subscriber_idle_timeout_secs);
        let idle_check_interval = Duration::from_secs(1);
        let mut last_lag_log = Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now);
        let mut consecutive_lag_count: u32 = 0;
        let subscriber_buf_size = self.buf_size;
        let preempted_token = self.preempted_token.clone();
        let low_priority_preempted = self.low_priority_preempted.clone();
        let address = *addr;
        let subscriber_started_at = Instant::now();

        let handle = tokio::spawn(async move {
            // Wait for the response to be polled, but respect cancellation/shutdown so a
            // body that is never polled cannot keep the forwarder task alive indefinitely.
            tokio::select! {
                () = cancel_token.cancelled() => return,
                started = start_rx => {
                    if started.is_err() {
                        return;
                    }
                }
            }
            // Send-progress budget begins only once the body is actually polled.
            let mut last_active = Instant::now();
            let (snapshot, mut next_sequence) = {
                let buffer = burst_buffer.lock().await;
                buffer.snapshot()
            };
            match send_burst_buffer(&snapshot, &client_tx_clone, &cancel_token, timeout_duration, &byte_budget).await {
                Ok(sent_burst_chunks) => {
                    drop(snapshot);
                    if sent_burst_chunks > 0 {
                        // The replay renewed its own internal progress time; carry that
                        // forward so the first live send does not immediately expire.
                        last_active = Instant::now();
                        debug_if_enabled!(
                            "Shared stream subscriber {} replayed {sent_burst_chunks} burst chunks after {} ms",
                            sanitize_sensitive_info(&address.to_string()),
                            subscriber_started_at.elapsed().as_millis()
                        );
                    }
                }
                Err(outcome) => {
                    drop(snapshot);
                    debug!(
                        "Shared stream subscriber {} burst replay failed ({outcome:?}); terminating",
                        sanitize_sensitive_info(&address.to_string())
                    );
                    cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                        addr: address,
                        subscriber_id,
                        request_id: None,
                        owner: None,
                    });
                    return;
                }
            }

            let mut first_live_chunk_logged = false;
            let mut startup_chunks_sent = 0_usize;
            let mut startup_bytes_sent = 0_usize;
            let mut startup_stats_logged = false;
            let mut read_chunks = Vec::with_capacity(subscriber_buf_size.min(64));
            let idle_check = sleep(idle_check_interval);
            tokio::pin!(idle_check);

            loop {
                // Pre-create the notified future before locking the buffer to avoid
                // a race where notify_waiters() fires between lock release and await.
                let notified_fut = live_notification.notified();

                let read = {
                    let buffer = burst_buffer.lock().await;
                    buffer.read_from_into(next_sequence, &mut read_chunks, MAX_SUBSCRIBER_LIVE_BATCH_CHUNKS)
                };
                next_sequence = read.next_sequence;
                if read.skipped > 0 {
                    consecutive_lag_count = consecutive_lag_count.saturating_add(1);
                    if last_lag_log.elapsed() > Duration::from_secs(5) {
                        let buffered_bytes = {
                            let buffer = burst_buffer_for_log.lock().await;
                            buffer.current_bytes
                        };
                        warn!(
                            "Shared stream client lagged behind {address}. Skipped {} messages \
                             (buffered {buffered_bytes} bytes, consecutive lags={consecutive_lag_count})",
                            read.skipped
                        );
                        last_lag_log = Instant::now();
                    }
                } else if !read_chunks.is_empty() {
                    consecutive_lag_count = 0;
                }

                trace_if_enabled!(
                    "shared_stream.subscribe: read {} chunks (next_seq={}, skipped={}) for {}",
                    read_chunks.len(),
                    read.next_sequence,
                    read.skipped,
                    sanitize_sensitive_info(&address.to_string())
                );

                if !read_chunks.is_empty() {
                    for data in read_chunks.drain(..) {
                        let chunk_len = data.len();
                        match send_client_chunk(
                            &client_tx,
                            data,
                            &cancel_token,
                            last_active + timeout_duration,
                            &byte_budget,
                        )
                        .await
                        {
                            SendOutcome::Sent => {}
                            outcome => {
                                debug!("Shared stream client send error ({outcome:?}): {address}");
                                cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                                    addr: address,
                                    subscriber_id,
                                    request_id: None,
                                    owner: None,
                                });
                                return;
                            }
                        }
                        if !first_live_chunk_logged {
                            debug_if_enabled!(
                                "Shared stream subscriber {} received first live chunk after {} ms",
                                sanitize_sensitive_info(&address.to_string()),
                                subscriber_started_at.elapsed().as_millis()
                            );
                            first_live_chunk_logged = true;
                        }
                        if !startup_stats_logged {
                            startup_chunks_sent = startup_chunks_sent.saturating_add(1);
                            startup_bytes_sent = startup_bytes_sent.saturating_add(chunk_len);
                            if subscriber_started_at.elapsed() >= Duration::from_secs(5) {
                                debug_if_enabled!(
                                    "Shared stream subscriber {} startup throughput: chunks={} bytes={} over {} ms (queue_used={}/{})",
                                    sanitize_sensitive_info(&address.to_string()),
                                    startup_chunks_sent,
                                    startup_bytes_sent,
                                    subscriber_started_at.elapsed().as_millis(),
                                    subscriber_buf_size.saturating_sub(client_tx_clone.capacity()),
                                    subscriber_buf_size
                                );
                                startup_stats_logged = true;
                            }
                        }
                        last_active = Instant::now();
                    }
                    continue;
                }

                tokio::select! {
                    biased;

                    () = cancel_token.cancelled() => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: cancel_received for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                        break;
                    }

                    () = &mut idle_check => {
                        if last_active.elapsed() > timeout_duration {
                            trace_if_enabled!(
                                "shared_stream.subscribe: idle_check_fired (inactivity>={}s) for {}",
                                timeout_duration.as_secs(),
                                sanitize_sensitive_info(&address.to_string())
                            );
                            cancel_token.cancel();
                            break;
                        }
                        idle_check.as_mut().reset(Instant::now() + idle_check_interval);
                    }

                    () = notified_fut => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: empty_buffer_waiting waker for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                    }

                    () = preempted_token.cancelled() => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: preempted for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                        if let Some(mut fallback) = low_priority_preempted {
                            debug_if_enabled!(
                                "Shared stream subscriber {} switching to low_priority_preempted fallback",
                                sanitize_sensitive_info(&address.to_string())
                            );
                            while let Some(chunk) = fallback.next_chunk() {
                                match send_client_chunk(&client_tx, chunk, &cancel_token, last_active + timeout_duration, &byte_budget).await {
                                    SendOutcome::Sent => last_active = Instant::now(),
                                    outcome => {
                                        debug!(
                                            "Shared stream fallback send error ({outcome:?}) for {}",
                                            sanitize_sensitive_info(&address.to_string())
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }

            cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                addr: address,
                subscriber_id,
                request_id: None,
                owner: None,
            });
        });

        self.lock_task_handles().push(handle);

        let provider = self.provider_guard.as_ref().and_then(|h| h.allocation.get_provider_name());
        let (permit, request_id, owner) = pending_cleanup.take_cleanup();
        (
            ReceiverStreamWrapper {
                stream: ReceiverStream::new(client_rx).map(|chunk| chunk.bytes),
                start: Some(start_tx),
                subscriber_id,
                addr: *addr,
                permit,
                request_id,
                owner,
                deadline: self.subscriber_max_duration.map(|duration| Box::pin(sleep(duration))),
                released: false,
            }
            .boxed(),
            provider,
            SharedCleanupCapability::new(subscriber_id),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn broadcast<S, E>(self: &Arc<Self>, stream_url: &str, bytes_stream: S, shared_streams: Arc<SharedStreamManager>)
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let streaming_url = stream_url.to_string();
        let origin_state = Arc::clone(self);
        let stop_token = self.stop_token.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        let broadcast_started_at = Instant::now();

        let broadcast_handle = tokio::spawn(async move {
            let mut source_stream = std::pin::pin!(bytes_stream);
            let mut counter = 0_usize;
            let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
            let idle = sleep(idle_timeout);
            tokio::pin!(idle);
            let mut first_source_chunk_logged = false;
            let mut startup_chunks_seen = 0_usize;
            let mut startup_bytes_seen = 0_usize;
            let mut startup_stats_logged = false;
            // Track the time of the most recent upstream push so the broadcast can
            // detect a stalled source before the global idle timeout fires. This is
            // the diagnostic signal for H1 (broadcast stall with stale burst replay).
            let mut last_push_at: Option<Instant> = None;
            let mut idle_warning_emitted = false;
            let idle_warn_threshold = idle_timeout / 2;

            loop {
                tokio::select! {
                    biased;

                    () = stop_token.cancelled() => {
                        debug_if_enabled!(
                            "No shared stream subscribers left. Closing shared provider stream {}",
                            sanitize_sensitive_info(&streaming_url)
                        );
                        break;
                    }

                    () = &mut idle => {
                        debug_if_enabled!(
                            "Shared stream source idle timeout after {}s for {}",
                            STREAM_IDLE_TIMEOUT,
                            sanitize_sensitive_info(&streaming_url)
                        );
                        stop_token.cancel();
                        break;
                    }

                    chunk = source_stream.next() => {
                        // Only successful chunks count as liveness; resetting on Err would let an
                        // error-spinning source dodge the idle timeout forever
                        if matches!(chunk, Some(Ok(_))) {
                            idle.as_mut().reset(Instant::now() + idle_timeout);
                        }
                        match chunk {
                            Some(Ok(data)) => {
                                let chunk_len = data.len();
                                let push_seq = {
                                    let mut buffer = burst_buffer.lock().await;
                                    let seq = buffer.next_sequence;
                                    buffer.push(data);
                                    seq
                                };
                                live_notification.notify_waiters();
                                last_push_at = Some(Instant::now());
                                idle_warning_emitted = false;
                                trace_if_enabled!(
                                    "shared_stream.broadcast: push seq={} len={} url={}",
                                    push_seq,
                                    chunk_len,
                                    sanitize_sensitive_info(&streaming_url)
                                );

                                if !first_source_chunk_logged {
                                    debug_if_enabled!(
                                        "Shared stream source produced first chunk for {} after {} ms",
                                        sanitize_sensitive_info(&streaming_url),
                                        broadcast_started_at.elapsed().as_millis()
                                    );
                                    first_source_chunk_logged = true;
                                }
                                if !startup_stats_logged {
                                    startup_chunks_seen = startup_chunks_seen.saturating_add(1);
                                    startup_bytes_seen = startup_bytes_seen.saturating_add(chunk_len);
                                    if broadcast_started_at.elapsed() >= Duration::from_secs(5) {
                                        debug_if_enabled!(
                                            "Shared stream source startup throughput for {}: chunks={} bytes={} over {} ms",
                                            sanitize_sensitive_info(&streaming_url),
                                            startup_chunks_seen,
                                            startup_bytes_seen,
                                            broadcast_started_at.elapsed().as_millis()
                                        );
                                        startup_stats_logged = true;
                                    }
                                }

                                counter = counter.saturating_add(1);
                                if counter >= YIELD_COUNTER {
                                    tokio::task::yield_now().await;
                                    counter = 0;
                                }
                            }
                            Some(Err(e)) => {
                                trace_if_enabled!(
                                    "Shared stream source error for {}: {:?}",
                                    sanitize_sensitive_info(&streaming_url),
                                    e
                                );
                                tokio::task::yield_now().await;
                            }
                            None => {
                                debug_if_enabled!(
                                    "Shared stream source stream ended for {}",
                                    sanitize_sensitive_info(&streaming_url)
                                );
                                break;
                            }
                        }
                    }
                }

                // Edge-triggered stall warning: fires once when the broadcast has
                // not seen an upstream push for STREAM_IDLE_TIMEOUT/2 seconds, then
                // resets on the next successful push. Operators correlate this
                // warning with the "stuck in resending same buffer" symptom (H1).
                if let Some(last) = last_push_at {
                    let stalled_for = last.elapsed();
                    if stalled_for >= idle_warn_threshold && !idle_warning_emitted {
                        warn!(
                            "shared_stream.broadcast: no upstream bytes for {}s on url={}; \
                             source may be stalled. Subscribers will see cached burst until {}s timeout.",
                            stalled_for.as_secs(),
                            sanitize_sensitive_info(&streaming_url),
                            STREAM_IDLE_TIMEOUT
                        );
                        idle_warning_emitted = true;
                    }
                }
            }

            debug_if_enabled!(
                "Shared stream exiting for {} (last_push_age_secs={})",
                sanitize_sensitive_info(&streaming_url),
                last_push_at.map_or(0, |t| t.elapsed().as_secs())
            );
            shared_streams.unregister(&streaming_url, &origin_state).await;
        });

        // Keep the broadcast handle so shutdown can join it instead of leaving a detached task.
        // Registration is guaranteed: a short-lived synchronous lock avoids the try_write
        // failure that would otherwise drop the handle under contention.
        self.lock_task_handles().push(broadcast_handle);
    }
}

#[derive(Debug, Clone, Default)]
struct SharedStreamsRegister {
    by_key: HashMap<Arc<str>, Arc<SharedStreamState>>,
    key_by_subscriber: HashMap<SubscriberId, Arc<str>>,
}

pub struct SharedStreamManager {
    provider_manager: Arc<ActiveProviderManager>,
    shared_streams: RwLock<SharedStreamsRegister>,
    meter_uids: std::sync::Mutex<HashMap<Arc<str>, SharedMeterEntry>>,
    #[cfg(test)]
    pub(crate) test_preflight_barrier: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

/// The four state handles the shared-stream paths need.
///
/// These functions used to take the whole `AppState` and reach into four of its
/// fields. Keeping this slice explicit avoids coupling the session crate to the
/// API server state; the composition root supplies the required handles.
#[derive(Clone, Copy)]
pub struct SharedStreamCtx<'a> {
    pub app_config: &'a Arc<AppConfig>,
    pub shared_stream_manager: &'a Arc<SharedStreamManager>,
    pub active_provider: &'a Arc<ActiveProviderManager>,
    pub connection_manager: &'a Arc<ConnectionManager>,
}

impl SharedStreamManager {
    pub async fn reserve_subscriber_cleanup(
        connection_manager: &ConnectionManager,
        subscriber_id: SharedSubscriberId,
        addr: SocketAddr,
    ) -> Result<PendingSharedSubscriberCleanup, ConnectionRejectionReason> {
        if connection_manager.is_shutting_down() {
            warn!("Shared stream cleanup rejected during shutdown for subscriber {subscriber_id}");
            return Err(ConnectionRejectionReason::CleanupReceiverClosed);
        }
        match timeout(SHARED_CLEANUP_ADMISSION_TIMEOUT, connection_manager.cleanup_tx().reserve_owned()).await {
            Ok(Ok(permit)) => Ok(PendingSharedSubscriberCleanup::new(permit, subscriber_id, addr)),
            Ok(Err(_)) => {
                warn!("Shared stream cleanup receiver closed; rejecting subscriber {subscriber_id}");
                Err(ConnectionRejectionReason::CleanupReceiverClosed)
            }
            Err(_) => {
                warn!("Shared stream cleanup admission timed out; rejecting subscriber {subscriber_id}");
                Err(ConnectionRejectionReason::CleanupAdmissionTimeout)
            }
        }
    }

    pub fn new(provider_manager: Arc<ActiveProviderManager>) -> Self {
        Self {
            provider_manager,
            shared_streams: RwLock::new(SharedStreamsRegister::default()),
            meter_uids: std::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_preflight_barrier: std::sync::Mutex::new(None),
        }
    }

    pub async fn get_shared_state(&self, stream_url: &str) -> Option<Arc<SharedStreamState>> {
        self.shared_streams.read().await.by_key.get(stream_url).map(Arc::clone)
    }

    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.get_shared_state(stream_url).await.map(|s| s.headers.clone())
    }

    pub async fn resource_counts(&self) -> (usize, usize) {
        let register = self.shared_streams.read().await;
        (register.by_key.len(), register.key_by_subscriber.len())
    }

    fn lock_meter_uids(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, SharedMeterEntry>> {
        self.meter_uids.lock().unwrap_or_else(|poisoned| {
            warn!("Recovering poisoned shared-stream meter registry");
            poisoned.into_inner()
        })
    }

    pub fn reserve_meter_uid(
        self: &Arc<Self>,
        stream_url: &str,
        uid_factory: impl FnOnce() -> u32,
    ) -> (u32, Option<PendingSharedMeterRegistration>) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get(stream_url) {
            return (entry.uid, None);
        }
        let stream_url: Arc<str> = Arc::from(stream_url);
        let meter_uid = uid_factory();
        uids.insert(Arc::clone(&stream_url), SharedMeterEntry { uid: meter_uid, owner: None });
        drop(uids);
        (
            meter_uid,
            Some(PendingSharedMeterRegistration { manager: Arc::clone(self), stream_url, meter_uid, armed: true }),
        )
    }

    /// Binds the reserved meter entry to the origin that just committed its shared
    /// origin. Called only while the registry write lock is held and only after the URL
    /// was confirmed absent from `by_key`, so the previous origin (if any) is already
    /// torn down and this new origin is the sole owner. Overwriting the owner is what
    /// keeps a successor's meter from being removed by a stale predecessor teardown.
    pub fn adopt_meter_uid(&self, stream_url: &str, allocation_id: AllocationId) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get_mut(stream_url) {
            entry.owner = Some(allocation_id);
        }
    }

    fn remove_meter_uid_if_pending(&self, stream_url: &str, meter_uid: u32) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get(stream_url) {
            if entry.uid == meter_uid && entry.owner.is_none() {
                uids.remove(stream_url);
            }
        }
    }

    pub fn meter_count(&self) -> usize { self.lock_meter_uids().len() }

    fn remove_meter_uid_if_owned_by(&self, stream_url: &str, allocation_id: Option<AllocationId>) {
        let mut uids = self.lock_meter_uids();
        let remove = uids.get(stream_url).is_some_and(|entry| match allocation_id {
            Some(id) => entry.owner == Some(id),
            None => entry.owner.is_none(),
        });
        if remove {
            uids.remove(stream_url);
        }
    }

    async fn finish_unregister(&self, stream_url: &str, state: &SharedStreamState) {
        self.remove_meter_uid_if_owned_by(stream_url, state.provider_guard.as_ref().map(|handle| handle.allocation_id));
        state.cancel_subscribers().await;
        state.stop_token.cancel();
        if let Some(handle) = &state.provider_guard {
            self.provider_manager.release_handle(handle);
        }
    }

    async fn unregister(&self, stream_url: &str, expected: &Arc<SharedStreamState>) {
        let mut register = self.shared_streams.write().await;
        let state = {
            if !register.by_key.get(stream_url).is_some_and(|state| Arc::ptr_eq(state, expected)) {
                return;
            }
            register.key_by_subscriber.retain(|_, url| url.as_ref() != stream_url);
            register.by_key.remove(stream_url)
        };
        if let Some(state) = state {
            self.finish_unregister(stream_url, &state).await;
        }
    }

    pub async fn teardown_preempted_stream(&self, stream_url: &str, allocation_id: u64) {
        let mut register = self.shared_streams.write().await;
        if !register.by_key.get(stream_url).is_some_and(|state| {
            state.provider_guard.as_ref().is_some_and(|handle| handle.allocation_id == allocation_id)
        }) {
            return;
        }
        let state = {
            register.key_by_subscriber.retain(|_, url| url.as_ref() != stream_url);
            register.by_key.remove(stream_url)
        };
        self.remove_meter_uid_if_owned_by(stream_url, Some(allocation_id));
        if let Some(state) = state {
            state.preempted_token.cancel();
            state.stop_token.cancel();
        }
    }

    /// Releases all subscribers on a closed transport. Playback cleanup uses the id variant.
    pub async fn release_connection(&self, addr: &SocketAddr, _send_stop_signal: bool) {
        let states: Vec<Arc<SharedStreamState>> = {
            let register = self.shared_streams.read().await;
            register.by_key.values().cloned().collect()
        };
        let mut ids = Vec::new();
        for state in states {
            ids.extend(
                state
                    .subscribers
                    .read()
                    .await
                    .iter()
                    .filter_map(|(id, subscriber)| (subscriber.addr == *addr).then_some(*id)),
            );
        }
        for id in ids {
            self.release_subscriber(id).await;
        }
    }

    pub async fn release_subscriber(&self, subscriber_id: SubscriberId) {
        let mut register = self.shared_streams.write().await;
        let stopped = {
            // Registry then subscribers is the shared-stream lock order. Joining and
            // removing the final subscriber must not race an origin replacement.
            if let Some(url) = register.key_by_subscriber.remove(&subscriber_id) {
                if let Some(state) = register.by_key.get(&url).cloned() {
                    let mut subscribers = state.subscribers.write().await;
                    if let Some(subscriber) = subscribers.remove(&subscriber_id) {
                        subscriber.cancel_token.cancel();
                    }
                    if subscribers.is_empty() && !state.has_pending_joins() {
                        register.by_key.remove(&url);
                        drop(subscribers);
                        Some((url, state))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(register);
        self.provider_manager.release_shared_connection(subscriber_id);
        if let Some((url, state)) = stopped {
            self.finish_unregister(&url, &state).await;
        }
    }

    pub async fn shutdown(&self) {
        let stopped = {
            let mut register = self.shared_streams.write().await;
            register.key_by_subscriber.clear();
            register.by_key.drain().collect::<Vec<_>>()
        };
        for (url, state) in stopped {
            self.finish_unregister(&url, &state).await;
            // Join the broadcast and forwarder tasks for this origin so teardown is
            // complete before the manager (and its runtime) is dropped. Cancellation has
            // already fired, so these tasks only finish their terminal cleanup.
            let handles = std::mem::take(&mut *state.lock_task_handles());
            for handle in handles {
                let _ = handle.await;
            }
        }
        self.lock_meter_uids().clear();
    }

    async fn subscribe_stream(
        &self,
        stream_url: &str,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        connection_manager: Arc<ConnectionManager>,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Result<Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>, ConnectionRejectionReason>
    {
        let Some(_admission) = connection_manager.begin_admission().await else {
            return Err(ConnectionRejectionReason::CleanupReceiverClosed);
        };
        {
            let register = self.shared_streams.read().await;
            if !register.by_key.contains_key(stream_url) {
                return Ok(None);
            }
        }
        #[cfg(test)]
        {
            let barrier = self.test_preflight_barrier.lock().map(|g| g.clone()).unwrap_or(None);
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
        }
        let mut pending_cleanup = Self::reserve_subscriber_cleanup(&connection_manager, subscriber_id, *addr).await?;
        // Keep the global registry lock only for the lookup and the key_by_subscriber
        // commit. The origin's own subscriber/task locks are taken during subscribe, so a
        // slow origin must not block joins on other URLs.
        let (state, pending_join) = {
            let mut register = self.shared_streams.write().await;
            let Some((key, state)) =
                register.by_key.get_key_value(stream_url).map(|(key, state)| (Arc::clone(key), Arc::clone(state)))
            else {
                pending_cleanup.disarm();
                return Ok(None);
            };
            if let Err(err) = self.provider_manager.add_shared_connection(
                addr,
                subscriber_id,
                stream_url,
                user_priority,
                connection_kind,
            ) {
                warn!("Failed joining shared stream: {}", sanitize_sensitive_info(&err));
                pending_cleanup.disarm();
                return Ok(None);
            }
            register.key_by_subscriber.insert(subscriber_id, key);
            let pending_join = PendingJoinGuard::new(&state);
            (state, pending_join)
        };
        Ok(Some(
            state.subscribe(addr, subscriber_id, Arc::clone(&connection_manager), pending_cleanup, pending_join).await,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_shared_stream<S, E>(
        ctx: SharedStreamCtx<'_>,
        stream_url: &str,
        bytes_stream: S,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        headers: Vec<(String, String)>,
        buffer_size: usize,
        mut provider_handle: Option<ManagedProviderHandle>,
        pending_cleanup: PendingSharedSubscriberCleanup,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let _admission = ctx.connection_manager.begin_admission().await?;
        let registration_started_at = Instant::now();
        let buf_size = CHANNEL_SIZE.max(buffer_size);
        let config = ctx.app_config.config.load();
        let min_buffer_bytes = resolve_min_burst_buffer_bytes(&config);
        let low_priority_preempted =
            ctx.app_config.custom_stream_response.load().as_ref().and_then(|c| c.low_priority_preempted.clone());
        let mut register = ctx.shared_stream_manager.shared_streams.write().await;
        if let Some((key, existing)) =
            register.by_key.get_key_value(stream_url).map(|(key, state)| (Arc::clone(key), Arc::clone(state)))
        {
            drop(provider_handle.take());
            if ctx
                .active_provider
                .add_shared_connection(addr, subscriber_id, stream_url, user_priority, connection_kind)
                .is_err()
            {
                return None;
            }
            register.key_by_subscriber.insert(subscriber_id, key);
            let pending_join = PendingJoinGuard::new(&existing);
            drop(register);
            let response = existing
                .subscribe(addr, subscriber_id, Arc::clone(ctx.connection_manager), pending_cleanup, pending_join)
                .await;
            return Some(response);
        }
        let handle = provider_handle.as_ref().and_then(|managed| managed.handle())?;
        if !ctx.active_provider.make_shared_connection(handle, stream_url, subscriber_id) {
            drop(register);
            drop(provider_handle.take());
            return None;
        }
        let allocation_id = handle.allocation_id;
        let raw_handle = provider_handle.and_then(|mut managed| managed.disarm())?;
        let mut shared_state =
            SharedStreamState::new(headers, buf_size, Some(raw_handle), min_buffer_bytes, low_priority_preempted)
                .with_subscriber_idle_timeout_secs(
                    config
                        .reverse_proxy
                        .as_ref()
                        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
                        .map_or(DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS, |stream| {
                            stream.shared_subscriber_idle_timeout_secs
                        }),
                );
        shared_state.subscriber_max_duration =
            config.sleep_timer_mins.filter(|mins| *mins > 0).map(|mins| Duration::from_secs(u64::from(mins) * 60));
        let shared_state = Arc::new(shared_state);
        let stream_key: Arc<str> = Arc::from(stream_url);
        register.by_key.insert(Arc::clone(&stream_key), Arc::clone(&shared_state));
        register.key_by_subscriber.insert(subscriber_id, stream_key);
        ctx.shared_stream_manager.adopt_meter_uid(stream_url, allocation_id);
        let pending_join = PendingJoinGuard::new(&shared_state);
        drop(register);
        let subscribed_stream = shared_state
            .subscribe(addr, subscriber_id, Arc::clone(ctx.connection_manager), pending_cleanup, pending_join)
            .await;
        debug_if_enabled!(
            "Shared stream startup register+subscribe completed for {} in {} ms",
            sanitize_sensitive_info(stream_url),
            registration_started_at.elapsed().as_millis()
        );
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(ctx.shared_stream_manager));
        Some(subscribed_stream)
    }

    pub async fn subscribe_shared_stream(
        ctx: SharedStreamCtx<'_>,
        stream_url: &str,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Result<Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>, ConnectionRejectionReason>
    {
        ctx.shared_stream_manager
            .subscribe_stream(
                stream_url,
                addr,
                subscriber_id,
                Arc::clone(ctx.connection_manager),
                user_priority,
                connection_kind,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        send_client_chunk, BudgetedChunk, BurstBuffer, SendOutcome, SharedStreamManager, SharedStreamState,
        CHANNEL_SIZE, MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES, SHARED_CLEANUP_ADMISSION_TIMEOUT,
    };
    use crate::{
        ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, ConnectionKind, ConnectionManager,
        ConnectionRejectionReason, EventManager, ManagedProviderHandle,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use bytes::Bytes;
    use futures::StreamExt;
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, XtreamCluster},
        utils::Internable,
    };
    use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
    use tokio::{
        sync::{mpsc, Semaphore},
        time::{timeout, Duration, Instant},
    };
    use tokio_stream::wrappers::ReceiverStream;
    use tokio_util::sync::CancellationToken;
    use tuliprox_core::{
        model::{
            AppConfig, Config, ConfigInput, Fingerprint, MediaToolCapabilities, SharedSubscriberId, SourcesConfig,
        },
        utils::FileLockManager,
    };

    fn create_test_app_config() -> AppConfig { create_test_app_config_with_conns(1) }

    fn create_test_app_config_with_conns(max_connections: u16) -> AppConfig {
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
            max_connections,
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

    fn create_test_stream_channel(url: &str) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id: 1,
            provider_id: 1,
            input_name: "provider_1".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "group".intern(),
            title: "title".intern(),
            url: url.intern(),
            shared: true,
            shared_joined_existing: Some(false),
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        }
    }

    fn create_test_connection_manager(
        app_cfg: &AppConfig,
        event_manager: &Arc<EventManager>,
    ) -> (Arc<ActiveProviderManager>, Arc<ActiveUserManager>, Arc<SharedStreamManager>, Arc<ConnectionManager>) {
        let provider_manager = Arc::new(ActiveProviderManager::new(app_cfg, event_manager));
        let geoip = Arc::new(ArcSwapOption::default());
        let user_manager = Arc::new(ActiveUserManager::new(&Config::default(), &geoip, event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
        let connection_manager =
            Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, event_manager, None));
        (provider_manager, user_manager, shared_manager, connection_manager)
    }

    async fn register_user(
        users: &ActiveUserManager,
        addr: SocketAddr,
        uid: u32,
        username: &str,
        stream_url: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fingerprint = Fingerprint::new(username.to_string(), username.to_string(), addr);
        let channel = create_test_stream_channel(stream_url);
        let stream = users
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username,
                max_connections: 5,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &channel,
                user_agent: Cow::Borrowed("test"),
                session_token: None,
            })
            .await
            .ok_or("user stream missing")?;
        assert_eq!(stream.uid, uid);
        Ok(())
    }

    #[tokio::test]
    async fn subscribers_on_same_socket_do_not_replace_each_other() -> Result<(), Box<dyn std::error::Error>> {
        let state = SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None);
        let addr = "127.0.0.1:41003".parse()?;
        let first = CancellationToken::new();
        let second = CancellationToken::new();
        state.register_subscriber(SharedSubscriberId::from_stream_uid(1), &addr, first.clone()).await;
        state.register_subscriber(SharedSubscriberId::from_stream_uid(2), &addr, second.clone()).await;
        assert_eq!(state.subscribers.read().await.len(), 2);
        assert!(!first.is_cancelled());
        assert!(!second.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn send_client_chunk_returns_when_cancelled_while_queue_is_full() -> Result<(), Box<dyn std::error::Error>> {
        let (tx, _rx) = mpsc::channel::<BudgetedChunk>(1);
        let byte_budget = Arc::new(Semaphore::new(1024));
        let queued_permit = byte_budget.clone().acquire_many_owned(6).await?;
        tx.send(BudgetedChunk { bytes: Bytes::from_static(b"queued"), _permit: queued_permit }).await?;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = timeout(
            Duration::from_secs(1),
            send_client_chunk(
                &tx,
                Bytes::from_static(b"blocked"),
                &cancel,
                Instant::now() + Duration::from_secs(1),
                &byte_budget,
            ),
        )
        .await?;
        assert_eq!(outcome, SendOutcome::Cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn send_client_chunk_respects_progress_deadline_while_queue_is_full() -> Result<(), Box<dyn std::error::Error>>
    {
        let (tx, _rx) = mpsc::channel::<BudgetedChunk>(1);
        let byte_budget = Arc::new(Semaphore::new(1024));
        let queued_permit = byte_budget.clone().acquire_many_owned(6).await?;
        tx.send(BudgetedChunk { bytes: Bytes::from_static(b"queued"), _permit: queued_permit }).await?;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = timeout(
            Duration::from_secs(1),
            send_client_chunk(&tx, Bytes::from_static(b"blocked"), &cancel, deadline, &byte_budget),
        )
        .await?;
        assert_eq!(outcome, SendOutcome::TimedOut);
        Ok(())
    }

    #[tokio::test]
    async fn send_client_chunk_respects_byte_budget() -> Result<(), Box<dyn std::error::Error>> {
        let (tx, _rx) = mpsc::channel::<BudgetedChunk>(16);
        // Byte budget of 4 bytes cannot accommodate a 6-byte chunk, so the acquire must
        // time out rather than exceed the budget.
        let byte_budget = Arc::new(Semaphore::new(4));
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = timeout(
            Duration::from_secs(1),
            send_client_chunk(&tx, Bytes::from_static(b"123456"), &cancel, deadline, &byte_budget),
        )
        .await?;
        assert_eq!(outcome, SendOutcome::TimedOut);
        Ok(())
    }

    #[tokio::test]
    async fn shared_clients_on_same_socket_keep_independent_streams_and_capacity(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41004".parse()?;
        let first_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let second_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/shared.ts";
        register_user(&users, addr, first_id.stream_uid(), "first", url).await?;
        let allocation = ManagedProviderHandle::new(
            Arc::clone(&providers),
            providers
                .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
                .ok_or("provider allocation missing")?,
        );
        let ctx = super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        };
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, first_id, addr)
            .await
            .map_err(|_| "shared cleanup admission failed")?;
        let (mut first, _, _) = SharedStreamManager::register_shared_stream(
            ctx,
            url,
            ReceiverStream::new(rx),
            &addr,
            first_id,
            Vec::new(),
            8,
            Some(allocation),
            pending_cleanup,
            0,
            ConnectionKind::Normal,
        )
        .await
        .ok_or("first subscription missing")?;
        let (mut second, _, _) =
            SharedStreamManager::subscribe_shared_stream(ctx, url, &addr, second_id, 0, ConnectionKind::Normal)
                .await
                .map_err(|_| "second shared cleanup admission failed")?
                .ok_or("second subscription missing")?;
        register_user(&users, addr, second_id.stream_uid(), "second", url).await?;

        tx.send(Ok(Bytes::from_static(b"first chunk"))).await?;
        assert_eq!(
            timeout(Duration::from_secs(2), first.next()).await?.ok_or("first ended")??,
            Bytes::from_static(b"first chunk")
        );
        assert_eq!(
            timeout(Duration::from_secs(2), second.next()).await?.ok_or("second ended")??,
            Bytes::from_static(b"first chunk")
        );
        assert_eq!(users.active_streams().await.len(), 2);
        assert_eq!(providers.get_provider_connections_count(), 1);

        drop(first);
        timeout(Duration::from_secs(2), async {
            while users.active_streams().await.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // A delayed duplicate cleanup cannot remove the surviving subscriber.
        connections.send_cleanup(crate::CleanupEvent::ReleaseSharedSubscriber {
            addr,
            subscriber_id: first_id,
            request_id: None,
            owner: None,
        });
        tx.send(Ok(Bytes::from_static(b"second chunk"))).await?;
        assert_eq!(
            timeout(Duration::from_secs(2), second.next()).await?.ok_or("survivor ended")??,
            Bytes::from_static(b"second chunk")
        );
        assert_eq!(providers.get_provider_connections_count(), 1);
        assert_eq!(users.active_streams().await.first().map(|stream| stream.uid), Some(second_id.stream_uid()));

        drop(second);
        timeout(Duration::from_secs(2), async {
            while !users.active_streams().await.is_empty() || providers.get_provider_connections_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(manager.get_shared_state(url).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn dropping_unpolled_shared_response_releases_its_subscription() -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41005".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/unpolled.ts";
        register_user(&users, addr, id.stream_uid(), "first", url).await?;
        let allocation = ManagedProviderHandle::new(
            Arc::clone(&providers),
            providers
                .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
                .ok_or("provider allocation missing")?,
        );
        let pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr)
            .await
            .map_err(|_| "shared cleanup admission failed")?;
        let response = SharedStreamManager::register_shared_stream(
            super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            },
            url,
            futures::stream::pending::<Result<Bytes, std::io::Error>>(),
            &addr,
            id,
            Vec::new(),
            8,
            Some(allocation),
            pending_cleanup,
            0,
            ConnectionKind::Normal,
        )
        .await
        .ok_or("subscription missing")?;
        drop(response);
        timeout(Duration::from_secs(2), async {
            while !users.active_streams().await.is_empty() || providers.get_provider_connections_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(manager.shared_streams.read().await.key_by_subscriber.is_empty());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_cleanup_queue_rejects_shared_subscriber_before_registration(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let (_, _, _, connections) = create_test_connection_manager(&app_cfg, &events);
        let cleanup_tx = connections.cleanup_tx();
        let available = cleanup_tx.capacity();
        let mut permits = Vec::with_capacity(available);
        for _ in 0..available {
            permits.push(cleanup_tx.clone().try_reserve_owned()?);
        }

        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        let addr = "127.0.0.1:41008".parse()?;
        let subscriber_id = SharedSubscriberId::from_stream_uid(88);
        let subscribe_connections = Arc::clone(&connections);
        let subscribe = tokio::spawn(async move {
            SharedStreamManager::reserve_subscriber_cleanup(&subscribe_connections, subscriber_id, addr).await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(SHARED_CLEANUP_ADMISSION_TIMEOUT + Duration::from_millis(1)).await;
        assert!(subscribe.await?.is_err(), "saturated cleanup admission must reject the subscriber");
        assert!(state.subscribers.read().await.is_empty(), "rejected admission must not register a subscriber");
        assert!(state.lock_task_handles().is_empty(), "rejected admission must not spawn a forwarding task");

        drop(permits);
        Ok(())
    }

    #[tokio::test]
    async fn shared_timeout_ends_only_its_subscriber() -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let (_, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41007".parse()?;
        let first = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let second = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url: Arc<str> = Arc::from("https://example.invalid/timeout.ts");
        register_user(&users, addr, first.stream_uid(), "same-user", &url).await?;
        register_user(&users, addr, second.stream_uid(), "same-user", &url).await?;
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        let surviving_token = CancellationToken::new();
        state.register_subscriber(first, &addr, CancellationToken::new()).await;
        state.register_subscriber(second, &addr, surviving_token.clone()).await;
        {
            let mut register = manager.shared_streams.write().await;
            register.by_key.insert(Arc::clone(&url), state);
            register.key_by_subscriber.insert(first, Arc::clone(&url));
            register.key_by_subscriber.insert(second, Arc::clone(&url));
        }
        let permit = connections.cleanup_tx().reserve_owned().await.ok();
        let mut response = super::ReceiverStreamWrapper {
            stream: futures::stream::pending::<Bytes>(),
            start: None,
            subscriber_id: first,
            addr,
            permit,
            request_id: None,
            owner: None,
            deadline: Some(Box::pin(tokio::time::sleep(Duration::ZERO))),
            released: false,
        };
        assert!(timeout(Duration::from_secs(2), response.next()).await?.is_none());
        timeout(Duration::from_secs(2), async {
            while users.active_streams().await.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(users.active_streams().await.first().map(|stream| stream.uid), Some(second.stream_uid()));
        assert!(!surviving_token.is_cancelled());
        assert!(manager.get_shared_state(&url).await.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn old_origin_cannot_unregister_replacement() {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let (_, _, manager, _) = create_test_connection_manager(&app_cfg, &events);
        let url = "https://example.invalid/live/replaced.ts";
        let old = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        let current = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        manager.shared_streams.write().await.by_key.insert(Arc::from(url), Arc::clone(&current));
        manager.unregister(url, &old).await;
        assert!(manager.get_shared_state(url).await.is_some_and(|state| Arc::ptr_eq(&state, &current)));
        assert!(!current.stop_token.is_cancelled());
    }

    #[tokio::test]
    async fn duplicate_subscriber_release_preserves_other_channel_on_same_socket(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let (_, _, manager, _) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41006".parse()?;
        let first = SharedSubscriberId::from_stream_uid(1);
        let second = SharedSubscriberId::from_stream_uid(2);
        let first_url: Arc<str> = Arc::from("https://example.invalid/first.ts");
        let second_url: Arc<str> = Arc::from("https://example.invalid/second.ts");
        let state_a = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        let state_b = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        let surviving_token = CancellationToken::new();
        state_a.register_subscriber(first, &addr, CancellationToken::new()).await;
        state_b.register_subscriber(second, &addr, surviving_token.clone()).await;
        {
            let mut register = manager.shared_streams.write().await;
            register.by_key.insert(Arc::clone(&first_url), state_a);
            register.by_key.insert(Arc::clone(&second_url), state_b);
            register.key_by_subscriber.insert(first, Arc::clone(&first_url));
            register.key_by_subscriber.insert(second, Arc::clone(&second_url));
        }
        manager.release_subscriber(first).await;
        manager.release_subscriber(first).await;
        assert!(manager.get_shared_state(&first_url).await.is_none());
        assert!(manager.get_shared_state(&second_url).await.is_some());
        assert!(!surviving_token.is_cancelled());
        manager.release_connection(&addr, true).await;
        assert!(surviving_token.is_cancelled());
        Ok(())
    }

    #[test]
    fn test_shared_state_channel_capacity_does_not_scale_with_burst_buffer_bytes() {
        let min_burst_buffer_size = 12 * 1024 * 1024;
        let state = SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, min_burst_buffer_size, None);

        assert_eq!(state.buf_size, CHANNEL_SIZE);
    }

    #[test]
    fn test_burst_buffer_eviction_is_byte_bounded() {
        let mut buffer = BurstBuffer::new(10);
        buffer.push(Bytes::from_static(b"12345"));
        buffer.push(Bytes::from_static(b"67890"));
        buffer.push(Bytes::from_static(b"abcde"));

        let mut chunks = Vec::new();
        let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

        assert_eq!(buffer.current_bytes, 10);
        assert_eq!(read.skipped, 1);
        assert_eq!(read.next_sequence, 3);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_burst_buffer_eviction_is_chunk_bounded_for_small_packets() {
        let max_chunks = 3;
        let mut buffer = BurstBuffer::new(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
        for _ in 0..max_chunks.saturating_add(1) {
            buffer.push(Bytes::from_static(b"x"));
        }

        let mut chunks = Vec::new();
        let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

        assert_eq!(buffer.buffer.len(), max_chunks);
        assert_eq!(buffer.current_bytes, max_chunks);
        assert_eq!(read.skipped, 1);
        assert_eq!(chunks.len(), max_chunks);
    }

    #[test]
    fn test_burst_buffer_chunk_bound_preserves_ts_sized_byte_capacity() {
        let max_chunks = 4;
        let mut buffer = BurstBuffer::new(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
        for _ in 0..max_chunks.saturating_add(1) {
            buffer.push(Bytes::from(vec![0_u8; MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES]));
        }

        let mut chunks = Vec::new();
        let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

        assert_eq!(buffer.buffer.len(), max_chunks);
        assert_eq!(buffer.current_bytes, MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES * max_chunks);
        assert_eq!(read.skipped, 1);
        assert_eq!(chunks.len(), max_chunks);
    }

    #[test]
    fn test_burst_buffer_keeps_oversized_packet_as_single_latest_chunk() {
        let mut buffer = BurstBuffer::new(4);
        buffer.push(Bytes::from_static(b"12345678"));

        let mut chunks = Vec::new();
        let read = buffer.read_from_into(0, &mut chunks, usize::MAX);

        assert_eq!(buffer.buffer.len(), 1);
        assert_eq!(buffer.current_bytes, 8);
        assert_eq!(read.next_sequence, 1);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_burst_buffer_reads_clone_bytes_without_copying_payload() {
        let mut buffer = BurstBuffer::new(1024);
        let chunk = Bytes::from(vec![1_u8, 2, 3, 4]);
        let ptr = chunk.as_ptr();
        buffer.push(chunk);

        let mut chunks = Vec::new();
        let _read = buffer.read_from_into(0, &mut chunks, usize::MAX);
        let Some(read_chunk) = chunks.first() else {
            panic!("expected one buffered chunk");
        };

        assert_eq!(read_chunk.as_ptr(), ptr);
    }

    #[test]
    fn test_burst_buffer_live_batch_is_chunk_bounded() {
        let mut buffer = BurstBuffer::new(4096);
        for _ in 0..10 {
            buffer.push(Bytes::from_static(b"x"));
        }

        let mut chunks = Vec::new();
        let first = buffer.read_from_into(0, &mut chunks, 4);
        assert_eq!(chunks.len(), 4);
        assert_eq!(first.next_sequence, 4);
        assert_eq!(first.skipped, 0);

        let second = buffer.read_from_into(first.next_sequence, &mut chunks, 4);
        assert_eq!(chunks.len(), 4);
        assert_eq!(second.next_sequence, 8);

        let third = buffer.read_from_into(second.next_sequence, &mut chunks, 4);
        assert_eq!(chunks.len(), 2);
        assert_eq!(third.next_sequence, 10);
    }

    #[tokio::test]
    async fn test_duplicate_release_connection_is_idempotent_with_single_subscriber() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

        let stream_url = "https://example.invalid/live/single.ts";
        let addr_1: SocketAddr = "127.0.0.1:42001".parse().unwrap_or_else(|_| unreachable!());
        let id = SharedSubscriberId::from_stream_uid(1);
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));

        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
            reg.key_by_subscriber.insert(id, Arc::from(stream_url));
        }

        state.register_subscriber(id, &addr_1, CancellationToken::new()).await;

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url));
            assert!(!reg.key_by_subscriber.contains_key(&id));
        }
        {
            let subs = state.subscribers.read().await;
            assert!(subs.is_empty());
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url));
            assert!(!reg.key_by_subscriber.contains_key(&id));
        }
        {
            let subs = state.subscribers.read().await;
            assert!(subs.is_empty());
        }
    }

    #[tokio::test]
    async fn test_preempted_shared_subscriber_switches_to_low_priority_fallback() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let geoip = Arc::new(ArcSwapOption::default());
        let user_manager = Arc::new(ActiveUserManager::new(&Config::default(), &geoip, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
        let connection_manager =
            Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, &event_manager, None));

        let addr: SocketAddr = "127.0.0.1:43001".parse().unwrap_or_else(|_| unreachable!());
        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;

        let low_priority_fallback = tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer::new(ts_packet);
        let state =
            Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, Some(low_priority_fallback)));

        let subscriber_id = SharedSubscriberId::from_stream_uid(1);
        let Ok(pending_cleanup) =
            SharedStreamManager::reserve_subscriber_cleanup(&connection_manager, subscriber_id, addr).await
        else {
            panic!("shared subscriber admission failed");
        };
        let (mut stream, _provider, _capability) = state
            .subscribe(&addr, subscriber_id, connection_manager, pending_cleanup, super::PendingJoinGuard::new(&state))
            .await;

        state.preempted_token.cancel();
        drop(state);

        let first = timeout(Duration::from_secs(2), stream.next()).await;
        let Ok(maybe_chunk) = first else { panic!("timed out waiting for fallback chunk") };
        let chunk = match maybe_chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(err)) => panic!("fallback stream returned error: {err}"),
            None => panic!("fallback stream ended unexpectedly"),
        };
        assert!(!chunk.is_empty(), "fallback chunk must contain MPEG-TS bytes");
    }

    #[tokio::test(start_paused = true)]
    async fn shared_subscription_admission_has_deadline_and_no_unprotected_fallback(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let (_, _, _, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr: SocketAddr = "127.0.0.1:43002".parse()?;
        let id = SharedSubscriberId::from_stream_uid(1);

        // Saturate the cleanup queue so the admission await blocks; the shared admission
        // deadline must reject the subscriber instead of hanging or falling back.
        let pending = || crate::CleanupEvent::Defer(Box::pin(std::future::pending::<()>()));
        connections.send_cleanup(pending());
        for _ in 0..4096 {
            connections.send_cleanup(pending());
        }

        let admission = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move { SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await }
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(super::SHARED_CLEANUP_ADMISSION_TIMEOUT + Duration::from_secs(1)).await;
        let result = admission.await?;
        assert!(result.is_err(), "saturated shared subscription admission must be rejected, not fall back");
        Ok(())
    }

    #[tokio::test]
    async fn broadcast_does_not_emit_shared_stream_health_events() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));
        let mut events = event_manager.get_event_channel();
        let stream_url = "https://user:pass@example.invalid/live/health.ts";
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));
        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
        }

        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        state.broadcast(stream_url, ReceiverStream::new(rx), Arc::clone(&shared_manager));

        assert!(
            timeout(Duration::from_millis(100), events.recv()).await.is_err(),
            "broadcast startup must not emit runtime events"
        );

        // Pushing data must also not emit a per-chunk health event.
        tx.send(Ok(Bytes::from_static(b"payload-1"))).await.unwrap_or_else(|_| panic!("send chunk should succeed"));
        assert!(
            timeout(Duration::from_millis(100), events.recv()).await.is_err(),
            "pushing a chunk must not emit runtime events"
        );

        drop(tx);
        let ended = timeout(Duration::from_secs(2), async {
            loop {
                if shared_manager.get_shared_state(stream_url).await.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(ended.is_ok(), "broadcast must exit after source end");
        assert!(
            timeout(Duration::from_millis(100), events.recv()).await.is_err(),
            "broadcast shutdown must not emit runtime events"
        );
    }

    #[tokio::test]
    async fn shared_origin_abort_before_user_registration_releases_physical_and_logical_request(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, users, _manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41010".parse()?;
        let owner = "shared-owner-1";
        let handle = providers
            .acquire_connection_with_lease_for_session(
                &"provider_1".intern(),
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                    owner,
                    tuliprox_core::model::PlaybackKind::LiveTs,
                )),
            )
            .ok_or("provider allocation missing")?;
        let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
        let managed = ManagedProviderHandle::new(Arc::clone(&providers), handle);
        let subscriber_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, subscriber_id, addr)
            .await
            .map_err(|err| format!("shared cleanup admission failed: {err}"))?;
        pending_cleanup.set_provider_request_identity(owner, request_id);
        assert_eq!(providers.get_provider_connections_count(), 1);
        assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).starting, 1);

        drop(managed);
        drop(pending_cleanup);

        timeout(Duration::from_secs(2), async {
            while providers.provider_lease_usage(&"provider_1".intern()).total() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(providers.get_provider_connections_count(), 0);
        assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
        assert!(users.active_streams().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn shared_origin_registration_rejected_releases_preacquired_handle() -> Result<(), Box<dyn std::error::Error>>
    {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41011".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/rejected_preacquired.ts";
        let owner = "rejected-preacquired-owner";
        let handle = providers
            .acquire_connection_with_lease_for_session(
                &"provider_1".intern(),
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                    owner,
                    tuliprox_core::model::PlaybackKind::LiveTs,
                )),
            )
            .ok_or("provider allocation missing")?;
        let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
        let managed = ManagedProviderHandle::new(Arc::clone(&providers), handle);
        assert_eq!(providers.get_provider_connections_count(), 1);

        let dummy_state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        manager.shared_streams.write().await.by_key.insert(Arc::from(url), dummy_state);

        let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
        pending_cleanup.set_provider_request_identity(owner, request_id);
        let res = SharedStreamManager::register_shared_stream(
            super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            },
            url,
            futures::stream::pending::<Result<Bytes, std::io::Error>>(),
            &addr,
            id,
            Vec::new(),
            8,
            Some(managed),
            pending_cleanup,
            0,
            ConnectionKind::Normal,
        )
        .await;

        assert!(res.is_none());
        timeout(Duration::from_secs(2), async {
            while providers.get_provider_connections_count() != 0
                || providers.provider_lease_usage(&"provider_1".intern()).total() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(providers.get_provider_connections_count(), 0);
        assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn shared_origin_abort_waiting_for_registry_lock_releases_handle() -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41012".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/abort_waiting.ts";
        let owner = "abort-waiting-owner";

        let lock_guard = manager.shared_streams.write().await;

        let handle = providers
            .acquire_connection_with_lease_for_session(
                &"provider_1".intern(),
                &addr,
                false,
                0,
                ConnectionKind::Normal,
                Some(crate::active_provider_manager::PlaybackLeaseRef::new(
                    owner,
                    tuliprox_core::model::PlaybackKind::LiveTs,
                )),
            )
            .ok_or("provider allocation missing")?;
        let request_id = handle.playback_request_id.ok_or("provider request identity missing")?;
        let managed = ManagedProviderHandle::new(Arc::clone(&providers), handle);
        assert_eq!(providers.get_provider_connections_count(), 1);

        let mut pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
        pending_cleanup.set_provider_request_identity(owner, request_id);

        let register_task = tokio::spawn({
            let app_cfg = Arc::clone(&app_cfg);
            let manager = Arc::clone(&manager);
            let providers = Arc::clone(&providers);
            let connections = Arc::clone(&connections);
            async move {
                let ctx = super::SharedStreamCtx {
                    app_config: &app_cfg,
                    shared_stream_manager: &manager,
                    active_provider: &providers,
                    connection_manager: &connections,
                };
                SharedStreamManager::register_shared_stream(
                    ctx,
                    url,
                    futures::stream::pending::<Result<Bytes, std::io::Error>>(),
                    &addr,
                    id,
                    Vec::new(),
                    8,
                    Some(managed),
                    pending_cleanup,
                    0,
                    ConnectionKind::Normal,
                )
                .await
            }
        });

        tokio::task::yield_now().await;

        register_task.abort();
        let _ = register_task.await;

        drop(lock_guard);

        timeout(Duration::from_secs(2), async {
            while providers.get_provider_connections_count() != 0
                || providers.provider_lease_usage(&"provider_1".intern()).total() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(providers.get_provider_connections_count(), 0);
        assert_eq!(providers.provider_lease_usage(&"provider_1".intern()).total(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_shared_origin_creation_releases_only_losing_allocation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config_with_conns(2));
        let events = Arc::new(EventManager::new());
        let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr_1 = "127.0.0.1:41013".parse()?;
        let addr_2 = "127.0.0.1:41014".parse()?;
        let id_1 = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let id_2 = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/concurrent_origin.ts";

        let handle_1 = providers
            .acquire_connection(&"provider_1".intern(), &addr_1, 0, ConnectionKind::Normal)
            .ok_or("first provider allocation missing")?;
        let managed_1 = ManagedProviderHandle::new(Arc::clone(&providers), handle_1);
        let pending_1 = SharedStreamManager::reserve_subscriber_cleanup(&connections, id_1, addr_1).await?;
        let (first_stream, _, _) = SharedStreamManager::register_shared_stream(
            super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            },
            url,
            futures::stream::pending::<Result<Bytes, std::io::Error>>(),
            &addr_1,
            id_1,
            Vec::new(),
            8,
            Some(managed_1),
            pending_1,
            0,
            ConnectionKind::Normal,
        )
        .await
        .ok_or("first origin registration failed")?;

        assert_eq!(providers.get_provider_connections_count(), 1);

        let handle_2 = providers
            .acquire_connection(&"provider_1".intern(), &addr_2, 0, ConnectionKind::Normal)
            .ok_or("second provider allocation missing")?;
        let managed_2 = ManagedProviderHandle::new(Arc::clone(&providers), handle_2);
        assert_eq!(providers.get_provider_connections_count(), 2);

        let pending_2 = SharedStreamManager::reserve_subscriber_cleanup(&connections, id_2, addr_2).await?;
        let (second_stream, _, _) = SharedStreamManager::register_shared_stream(
            super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            },
            url,
            futures::stream::pending::<Result<Bytes, std::io::Error>>(),
            &addr_2,
            id_2,
            Vec::new(),
            8,
            Some(managed_2),
            pending_2,
            0,
            ConnectionKind::Normal,
        )
        .await
        .ok_or("second subscriber join failed")?;

        assert_eq!(providers.get_provider_connections_count(), 1);

        drop(first_stream);
        drop(second_stream);
        timeout(Duration::from_secs(2), async {
            while providers.get_provider_connections_count() != 0 || !users.active_streams().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(providers.get_provider_connections_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_rejects_waiting_admission_and_drains_owned_cleanup() -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (_providers, _users, _manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41015".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let initial_capacity = connections.cleanup_tx().capacity();
        let held_cleanup = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
        let admission = connections.begin_admission().await.ok_or("admission unexpectedly closed")?;

        let shutdown = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move { connections.shutdown().await }
        });
        timeout(Duration::from_secs(2), async {
            while !connections.is_shutting_down() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(connections.is_shutting_down());

        let res = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await;
        assert!(
            matches!(res, Err(ConnectionRejectionReason::CleanupReceiverClosed)),
            "waiting admission must be rejected after shutdown"
        );
        assert!(!shutdown.is_finished(), "shutdown must wait for an admitted registration");

        drop(held_cleanup);
        drop(admission);
        timeout(Duration::from_secs(2), shutdown).await??;
        assert_eq!(connections.cleanup_tx().capacity(), initial_capacity);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_with_unpolled_shared_body_leaves_no_requests_or_subscribers(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41016".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/unpolled_shutdown.ts";
        register_user(&users, addr, id.stream_uid(), "shutdown_user", url).await?;

        let handle = providers
            .acquire_connection(&"provider_1".intern(), &addr, 0, ConnectionKind::Normal)
            .ok_or("provider allocation missing")?;
        let managed = ManagedProviderHandle::new(Arc::clone(&providers), handle);
        let pending = SharedStreamManager::reserve_subscriber_cleanup(&connections, id, addr).await?;
        let (unpolled_body, _, _) = SharedStreamManager::register_shared_stream(
            super::SharedStreamCtx {
                app_config: &app_cfg,
                shared_stream_manager: &manager,
                active_provider: &providers,
                connection_manager: &connections,
            },
            url,
            futures::stream::pending::<Result<Bytes, std::io::Error>>(),
            &addr,
            id,
            Vec::new(),
            8,
            Some(managed),
            pending,
            0,
            ConnectionKind::Normal,
        )
        .await
        .ok_or("register failed")?;

        assert_eq!(providers.get_provider_connections_count(), 1);
        assert_eq!(users.active_streams().await.len(), 1);

        connections.shutdown().await;

        assert_eq!(providers.get_provider_connections_count(), 0);
        assert!(users.active_streams().await.is_empty());
        assert!(manager.get_shared_state(url).await.is_none());

        drop(unpolled_body);
        Ok(())
    }

    #[tokio::test]
    async fn shared_cold_miss_does_not_consume_cleanup_capacity_or_emit_cleanup(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41017".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let initial_capacity = connections.cleanup_tx().capacity();

        let ctx = super::SharedStreamCtx {
            app_config: &app_cfg,
            shared_stream_manager: &manager,
            active_provider: &providers,
            connection_manager: &connections,
        };

        let res = SharedStreamManager::subscribe_shared_stream(
            ctx,
            "https://example.invalid/live/non_existent.ts",
            &addr,
            id,
            0,
            ConnectionKind::Normal,
        )
        .await?;

        assert!(res.is_none());
        assert_eq!(
            connections.cleanup_tx().capacity(),
            initial_capacity,
            "cold miss must not consume cleanup capacity"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shared_state_removed_between_preflight_and_commit_leaves_no_claim(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41018".parse()?;
        let id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/removed_between.ts";

        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        manager.shared_streams.write().await.by_key.insert(Arc::from(url), state);

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        if let Ok(mut lock) = manager.test_preflight_barrier.lock() {
            *lock = Some(Arc::clone(&barrier));
        }

        let initial_capacity = connections.cleanup_tx().capacity();

        let sub_task = tokio::spawn({
            let app_cfg = Arc::clone(&app_cfg);
            let manager = Arc::clone(&manager);
            let providers = Arc::clone(&providers);
            let connections = Arc::clone(&connections);
            async move {
                let ctx = super::SharedStreamCtx {
                    app_config: &app_cfg,
                    shared_stream_manager: &manager,
                    active_provider: &providers,
                    connection_manager: &connections,
                };
                SharedStreamManager::subscribe_shared_stream(ctx, url, &addr, id, 0, ConnectionKind::Normal).await
            }
        });

        barrier.wait().await;

        {
            let mut reg = manager.shared_streams.write().await;
            reg.by_key.remove(url);
        }

        let res = sub_task.await??;
        assert!(res.is_none());
        assert_eq!(connections.cleanup_tx().capacity(), initial_capacity);
        assert!(manager.get_shared_state(url).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn pending_join_keeps_origin_alive_when_last_subscriber_leaves() -> Result<(), Box<dyn std::error::Error>> {
        let app_cfg = Arc::new(create_test_app_config());
        let events = Arc::new(EventManager::new());
        let (_providers, _users, manager, connections) = create_test_connection_manager(&app_cfg, &events);
        let addr = "127.0.0.1:41019".parse()?;
        let first_id = SharedSubscriberId::from_stream_uid(connections.next_stream_uid());
        let url = "https://example.invalid/live/pending_join.ts";

        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE, None, 1024, None));
        state.register_subscriber(first_id, &addr, CancellationToken::new()).await;
        {
            let mut reg = manager.shared_streams.write().await;
            reg.by_key.insert(Arc::from(url), Arc::clone(&state));
            reg.key_by_subscriber.insert(first_id, Arc::from(url));
        }

        // Simulate a join that committed its registry entry but has not yet registered
        // its subscriber in the origin.
        let pending_join = super::PendingJoinGuard::new(&state);
        assert!(state.has_pending_joins());

        // The last (and only) subscriber leaves; the origin must not be torn down because
        // the pending join is about to land on it.
        manager.release_subscriber(first_id).await;
        assert!(manager.get_shared_state(url).await.is_some(), "origin must survive a pending join");

        // The join completes; the origin has no subscribers and no pending joins.
        pending_join.commit();
        assert!(!state.has_pending_joins());
        assert!(state.subscribers.read().await.is_empty(), "origin has no subscribers after release");
        Ok(())
    }

    #[test]
    fn uncommitted_shared_meter_registration_is_rolled_back() {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
        let manager = Arc::new(SharedStreamManager::new(providers));
        let url = "https://example.invalid/live/meter-rollback.ts";

        let (meter_uid, pending) = manager.reserve_meter_uid(url, || 41);
        assert_eq!(meter_uid, 41);
        assert_eq!(manager.meter_count(), 1);

        drop(pending);
        assert_eq!(manager.meter_count(), 0);
    }

    #[test]
    fn pending_meter_rollback_does_not_remove_adopted_entry() {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
        let manager = Arc::new(SharedStreamManager::new(providers));
        let url = "https://example.invalid/live/meter-replacement.ts";

        // A reserved the meter; B then commits a shared origin and adopts it.
        let (_, stale) = manager.reserve_meter_uid(url, || 51);
        manager.adopt_meter_uid(url, 999);

        // A aborts: its pending rollback must not remove the adopted entry.
        drop(stale);
        assert_eq!(manager.meter_count(), 1);
        assert_eq!(manager.lock_meter_uids().get(url).map(|entry| entry.uid), Some(51));
        assert_eq!(manager.lock_meter_uids().get(url).and_then(|entry| entry.owner), Some(999));
    }

    #[test]
    fn successor_adoption_overwrites_stale_owner_and_survives_old_teardown() {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
        let manager = Arc::new(SharedStreamManager::new(providers));
        let url = "https://example.invalid/live/meter-successor.ts";

        let (uid, _guard) = manager.reserve_meter_uid(url, || 71);
        manager.adopt_meter_uid(url, 100);
        // A successor origin commits the same URL and takes over the meter.
        manager.adopt_meter_uid(url, 200);
        assert_eq!(manager.lock_meter_uids().get(url).and_then(|entry| entry.owner), Some(200));

        // A stale teardown by the predecessor must not remove the successor's entry.
        manager.remove_meter_uid_if_owned_by(url, Some(100));
        assert_eq!(manager.meter_count(), 1);
        assert_eq!(manager.lock_meter_uids().get(url).map(|entry| entry.uid), Some(uid));
        _guard.expect("reservation").commit();
    }

    #[tokio::test]
    async fn shutdown_clears_shared_meter_registrations() {
        let app_cfg = create_test_app_config();
        let events = Arc::new(EventManager::new());
        let providers = Arc::new(ActiveProviderManager::new(&app_cfg, &events));
        let manager = Arc::new(SharedStreamManager::new(providers));

        let (_, pending) = manager.reserve_meter_uid("https://example.invalid/live/meter-shutdown.ts", || 61);
        pending.expect("new registration").commit();
        assert_eq!(manager.meter_count(), 1);

        manager.shutdown().await;
        assert_eq!(manager.meter_count(), 0);
    }
}
