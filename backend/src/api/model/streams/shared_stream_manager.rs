use crate::{
    api::model::{
        streams::buffered_stream::CHANNEL_SIZE, ActiveProviderManager, AppState, BoxedProviderStream, ProviderHandle,
        ConnectionManager, StreamError, STREAM_IDLE_TIMEOUT,
    },
    model::Config,
    utils::{debug_if_enabled, trace_if_enabled},
};
use bytes::Bytes;
use futures::{stream::BoxStream, Stream, StreamExt};
use log::{debug, warn};
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    fmt::{Debug, Formatter},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, mpsc::Sender, Mutex, Notify, RwLock},
    time::{sleep, Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

const DEFAULT_SHARED_BUFFER_SIZE_BYTES: usize = 1024 * 1024 * 32;
const YIELD_COUNTER: usize = 64;
const MIN_BURST_BUFFER_CHUNKS: usize = 2;
const MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES: usize = 188;
const SHARED_BURST_BYTES_PER_BUFFER_SLOT: usize = 12 * 1024;
const DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS: u64 = 300;

struct ReceiverStreamWrapper<S> {
    stream: S,
}

impl<S> Stream for ReceiverStreamWrapper<S>
where
    S: Stream<Item=Bytes> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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

fn convert_stream(stream: BoxStream<Bytes>) -> BoxStream<Result<Bytes, StreamError>> {
    ReceiverStreamWrapper { stream }.boxed()
}

type SubscriberId = SocketAddr;

#[derive(Clone, Debug)]
struct SharedSubscriber {
    id: u64,
    cancel_token: CancellationToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SharedSubscriberOwner {
    id: u64,
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

    pub fn read_from_into(&self, next_sequence: u64, chunks: &mut Vec<Bytes>) -> BurstRead {
        chunks.clear();
        let earliest_sequence = self.buffer.front().map_or(self.next_sequence, |chunk| chunk.sequence);
        let start_sequence = next_sequence.max(earliest_sequence);
        let skipped = start_sequence.saturating_sub(next_sequence);
        let start_index = self.start_index_for_sequence(start_sequence);
        chunks.extend(self.buffer.range(start_index..).map(|chunk| chunk.bytes.clone()));

        BurstRead { next_sequence: self.next_sequence, skipped }
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

async fn send_burst_buffer(
    start_buffer: &[Bytes],
    client_tx: &Sender<Bytes>,
    cancellation_token: &CancellationToken,
) -> usize {
    let mut sent = 0_usize;
    for buf in start_buffer {
        if cancellation_token.is_cancelled() {
            return sent;
        }
        if !send_client_chunk(client_tx, buf.clone(), cancellation_token).await {
            debug!("Failed sending burst-buffer chunk to client");
            return sent;
        }
        sent = sent.saturating_add(1);
    }
    sent
}

async fn send_client_chunk(
    client_tx: &Sender<Bytes>,
    data: Bytes,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }

    let data = match client_tx.try_send(data) {
        Ok(()) => return true,
        Err(mpsc::error::TrySendError::Closed(_)) => return false,
        Err(mpsc::error::TrySendError::Full(data)) => data,
    };

    tokio::select! {
        biased;

        () = cancellation_token.cancelled() => false,
        result = client_tx.send(data) => result.is_ok(),
    }
}

#[derive(Debug)]
pub struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    provider_guard: Option<ProviderHandle>,
    low_priority_preempted: Option<crate::api::model::TransportStreamBuffer>,
    preempted_token: CancellationToken,
    subscribers: RwLock<HashMap<SubscriberId, SharedSubscriber>>,
    next_subscriber_id: AtomicU64,
    stop_token: CancellationToken,
    burst_buffer: Arc<Mutex<BurstBuffer>>,
    live_notification: Arc<Notify>,
    task_handles: RwLock<Vec<tokio::task::JoinHandle<()>>>,
    subscriber_idle_timeout_secs: u64,
}

impl SharedStreamState {
    fn new(
        headers: Vec<(String, String)>,
        buf_size: usize,
        provider_guard: Option<ProviderHandle>,
        min_burst_buffer_size: usize,
        low_priority_preempted: Option<crate::api::model::TransportStreamBuffer>,
    ) -> Self {
        let base_channel_capacity = buf_size.max(CHANNEL_SIZE);
        let burst_buffer_size_in_bytes = min_burst_buffer_size
            .max(base_channel_capacity.saturating_mul(SHARED_BURST_BYTES_PER_BUFFER_SLOT));
        Self {
            headers,
            buf_size: base_channel_capacity,
            provider_guard,
            low_priority_preempted,
            preempted_token: CancellationToken::new(),
            subscribers: RwLock::new(HashMap::new()),
            next_subscriber_id: AtomicU64::new(1),
            stop_token: CancellationToken::new(),
            burst_buffer: Arc::new(Mutex::new(BurstBuffer::new(burst_buffer_size_in_bytes))),
            live_notification: Arc::new(Notify::new()),
            task_handles: RwLock::new(Vec::new()),
            subscriber_idle_timeout_secs: DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS,
        }
    }

    fn with_subscriber_idle_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.subscriber_idle_timeout_secs = secs;
        }
        self
    }

    async fn register_subscriber(&self, addr: &SocketAddr, cancel_token: CancellationToken) -> SharedSubscriberOwner {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        let subscriber = SharedSubscriber {
            id,
            cancel_token,
        };
        let previous = {
            let mut subs = self.subscribers.write().await;
            let previous = subs.insert(*addr, subscriber);
            debug_if_enabled!(
                "Shared stream subscriber added {}; total subscribers={}",
                sanitize_sensitive_info(&addr.to_string()),
                subs.len()
            );
            previous
        };

        if let Some(previous_subscriber) = previous.as_ref() {
            previous_subscriber.cancel_token.cancel();
        }

        SharedSubscriberOwner { id }
    }

    async fn remove_subscriber_if_owner(&self, addr: &SocketAddr, owner: SharedSubscriberOwner) -> bool {
        let mut subs = self.subscribers.write().await;
        if subs.get(addr).is_none_or(|subscriber| subscriber.id != owner.id) {
            return false;
        }
        subs.remove(addr);
        true
    }

    async fn cancel_subscribers(&self) {
        let subscribers = self.subscribers.read().await;
        for subscriber in subscribers.values() {
            subscriber.cancel_token.cancel();
        }
    }

    async fn has_no_subscribers(&self) -> bool { self.subscribers.read().await.is_empty() }

    async fn cleanup_subscriber(
        state: &Arc<SharedStreamState>,
        manager: &SharedStreamManager,
        connection_manager: &ConnectionManager,
        addr: &SocketAddr,
        owner: SharedSubscriberOwner,
    ) {
        if !state.remove_subscriber_if_owner(addr, owner).await {
            return;
        }
        let stream_url = manager.forget_subscriber_addr(addr).await;
        let is_empty = state.has_no_subscribers().await;
        connection_manager.release_stream(addr).await;
        connection_manager.release_provider_connection(addr).await;
        if is_empty {
            if let Some(stream_url) = stream_url.as_ref() {
                manager.unregister(stream_url, false).await;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn subscribe(
        self: &Arc<Self>,
        addr: &SocketAddr,
        manager: Arc<SharedStreamManager>,
        connection_manager: Arc<ConnectionManager>,
    ) -> (BoxedProviderStream, Option<Arc<str>>) {
        let (client_tx, client_rx) = mpsc::channel(self.buf_size);
        let cancel_token = CancellationToken::new();

        {
            let mut handles = self.task_handles.write().await;
            handles.retain(|h| !h.is_finished());
        }

        let owner = self.register_subscriber(addr, cancel_token.clone()).await;

        let client_tx_clone = client_tx.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let burst_buffer_for_log = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        let timeout_duration = Duration::from_secs(self.subscriber_idle_timeout_secs);
        let idle_check_interval = Duration::from_secs(1);
        let mut last_active = Instant::now();
        let mut last_lag_log = Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now);
        let mut consecutive_lag_count: u32 = 0;
        let subscriber_buf_size = self.buf_size;
        let preempted_token = self.preempted_token.clone();
        let low_priority_preempted = self.low_priority_preempted.clone();
        let address = *addr;
        let subscriber_started_at = Instant::now();
        let state = Arc::clone(self);

        let handle = tokio::spawn(async move {
            let (snapshot, mut next_sequence) = {
                let buffer = burst_buffer.lock().await;
                buffer.snapshot()
            };
            let sent_burst_chunks = send_burst_buffer(&snapshot, &client_tx_clone, &cancel_token).await;
            if sent_burst_chunks > 0 {
                debug_if_enabled!(
                    "Shared stream subscriber {} replayed {sent_burst_chunks} burst chunks after {} ms",
                    sanitize_sensitive_info(&address.to_string()),
                    subscriber_started_at.elapsed().as_millis()
                );
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
                    buffer.read_from_into(next_sequence, &mut read_chunks)
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
                        if !send_client_chunk(&client_tx, data, &cancel_token).await {
                            debug!("Shared stream client send error: {address}");
                            Self::cleanup_subscriber(&state, &manager, &connection_manager, &address, owner).await;
                            return;
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
                                if !send_client_chunk(&client_tx, chunk, &cancel_token).await {
                                    debug!(
                                        "Shared stream fallback send error for {}",
                                        sanitize_sensitive_info(&address.to_string())
                                    );
                                    break;
                                }
                            }
                        }
                        break;
                    }
                }
            }

            Self::cleanup_subscriber(&state, &manager, &connection_manager, &address, owner).await;
        });

        self.task_handles.write().await.push(handle);

        let provider = self.provider_guard.as_ref().and_then(|h| h.allocation.get_provider_name());
        (convert_stream(ReceiverStream::new(client_rx).boxed()), provider)
    }

    #[allow(clippy::too_many_lines)]
    fn broadcast<S, E>(
        self: &Arc<Self>,
        stream_url: &str,
        bytes_stream: S,
        shared_streams: Arc<SharedStreamManager>,
    )
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let streaming_url = stream_url.to_string();
        let stop_token = self.stop_token.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        let broadcast_started_at = Instant::now();

        tokio::spawn(async move {
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
            shared_streams.unregister(&streaming_url, false).await;
        });
    }
}

#[derive(Debug, Clone, Default)]
struct SharedStreamsRegister {
    by_key: HashMap<Arc<str>, Arc<SharedStreamState>>,
    key_by_addr: HashMap<SubscriberId, Arc<str>>,
}

pub struct SharedStreamManager {
    provider_manager: Arc<ActiveProviderManager>,
    shared_streams: RwLock<SharedStreamsRegister>,
    meter_uids: RwLock<HashMap<String, u32>>,
}

impl SharedStreamManager {
    pub(crate) fn new(provider_manager: Arc<ActiveProviderManager>) -> Self {
        Self {
            provider_manager,
            shared_streams: RwLock::new(SharedStreamsRegister::default()),
            meter_uids: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_shared_state(&self, stream_url: &str) -> Option<Arc<SharedStreamState>> {
        self.shared_streams.read().await.by_key.get(stream_url).map(Arc::clone)
    }

    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.get_shared_state(stream_url).await.map(|s| s.headers.clone())
    }

    pub async fn get_or_register_meter_uid(&self, stream_url: &str, uid_factory: impl FnOnce() -> u32) -> u32 {
        let mut uids = self.meter_uids.write().await;
        *uids.entry(stream_url.to_string()).or_insert_with(uid_factory)
    }

    async fn forget_subscriber_addr(&self, addr: &SocketAddr) -> Option<Arc<str>> {
        let mut shared_streams = self.shared_streams.write().await;
        shared_streams.key_by_addr.remove(addr)
    }

    async fn unregister(&self, stream_url: &str, send_stop_signal: bool) {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;

            let remove_keys: Vec<SocketAddr> = shared_streams
                .key_by_addr
                .iter()
                .filter_map(|(addr, url)| if url.as_ref() == stream_url { Some(*addr) } else { None })
                .collect();
            for k in remove_keys {
                shared_streams.key_by_addr.remove(&k);
            }

            shared_streams.by_key.remove(stream_url)
        };

        self.meter_uids.write().await.remove(stream_url);

        if let Some(shared_state) = shared_state_opt {
            let remaining = shared_state.subscribers.read().await.len();
            debug_if_enabled!(
                "Unregistering shared stream {} (remaining_subscribers={remaining}, send_stop_signal={send_stop_signal})",
                sanitize_sensitive_info(stream_url)
            );

            if remaining > 0 && !send_stop_signal {
                shared_state.cancel_subscribers().await;
            } else {
                for handle in shared_state.task_handles.write().await.drain(..) {
                    handle.abort();
                }
            }

            if let Some(provider_handle) = &shared_state.provider_guard {
                self.provider_manager.release_handle(provider_handle).await;
            }

            if send_stop_signal || remaining == 0 {
                trace_if_enabled!(
                    "Sending shared stream stop signal {}",
                    sanitize_sensitive_info(stream_url)
                );
                shared_state.stop_token.cancel();
            }
        }
    }

    pub async fn teardown_preempted_stream(&self, stream_url: &str) {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;

            let remove_keys: Vec<SocketAddr> = shared_streams
                .key_by_addr
                .iter()
                .filter_map(|(addr, url)| if url.as_ref() == stream_url { Some(*addr) } else { None })
                .collect();
            for k in remove_keys {
                shared_streams.key_by_addr.remove(&k);
            }

            shared_streams.by_key.remove(stream_url)
        };

        self.meter_uids.write().await.remove(stream_url);

        if let Some(shared_state) = shared_state_opt {
            debug_if_enabled!(
                "Tearing down preempted shared stream {}",
                sanitize_sensitive_info(stream_url)
            );

            shared_state.preempted_token.cancel();
            shared_state.stop_token.cancel();
        }
    }

    pub async fn release_connection(&self, addr: &SocketAddr, send_stop_signal: bool) {
        let (stream_url, shared_state) = {
            let shared_streams = self.shared_streams.read().await;
            if let Some(stream_url) = shared_streams.key_by_addr.get(addr) {
                (Some(stream_url.clone()), shared_streams.by_key.get(stream_url).cloned())
            } else {
                (None, None)
            }
        };

        if let Some(state) = shared_state {
            let (tx, is_empty, remaining) = {
                let mut subs = state.subscribers.write().await;
                let tx = subs.remove(addr);
                let is_empty = subs.is_empty();
                (tx, is_empty, subs.len())
            };

            let Some(client_stop_signal) = tx else {
                trace_if_enabled!(
                    "Ignoring duplicate shared stream release for {} (already removed)",
                    sanitize_sensitive_info(&addr.to_string())
                );
                return;
            };

            {
                let mut shared_streams = self.shared_streams.write().await;
                shared_streams.key_by_addr.remove(addr);
            }

            debug_if_enabled!(
                "Shared stream subscriber removed {}; remaining subscribers={remaining}",
                sanitize_sensitive_info(&addr.to_string())
            );

            if is_empty {
                if let Some(url) = stream_url.as_ref() {
                    debug_if_enabled!(
                        "No subscribers remain for {} after removing {}",
                        sanitize_sensitive_info(url),
                        sanitize_sensitive_info(&addr.to_string())
                    );
                    self.unregister(url, send_stop_signal).await;
                }
            }

            client_stop_signal.cancel_token.cancel();
        }
    }

    async fn subscribe_stream(
        &self,
        stream_url: &str,
        addr: &SocketAddr,
        manager: Arc<SharedStreamManager>,
        connection_manager: Arc<ConnectionManager>,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)> {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;
            if let Some((stream_key, shared_state)) = shared_streams
                .by_key
                .get_key_value(stream_url)
                .map(|(stream_key, shared_state)| (Arc::clone(stream_key), Arc::clone(shared_state)))
            {
                shared_streams.key_by_addr.insert(*addr, stream_key);
                Some(shared_state)
            } else {
                None
            }
        };

        if let Some(shared_state) = shared_state_opt {
            debug_if_enabled!(
                "Responding to existing shared client stream {} {}",
                sanitize_sensitive_info(&addr.to_string()),
                sanitize_sensitive_info(stream_url)
            );
            Some(shared_state.subscribe(addr, manager, connection_manager).await)
        } else {
            None
        }
    }

    async fn register(&self, addr: &SocketAddr, stream_url: &str, shared_state: Arc<SharedStreamState>) {
        let mut shared_streams = self.shared_streams.write().await;
        let stream_key: Arc<str> = Arc::from(stream_url);
        shared_streams.by_key.insert(Arc::clone(&stream_key), shared_state);
        shared_streams.key_by_addr.insert(*addr, stream_key);
        debug_if_enabled!(
            "Registered shared stream {} for initial subscriber {}",
            sanitize_sensitive_info(stream_url),
            sanitize_sensitive_info(&addr.to_string())
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn register_shared_stream<S, E>(
        app_state: &AppState,
        stream_url: &str,
        bytes_stream: S,
        addr: &SocketAddr,
        headers: Vec<(String, String)>,
        buffer_size: usize,
        provider_handle: Option<ProviderHandle>,
        user_priority: i8,
        connection_kind: crate::api::model::active_provider_manager::ConnectionKind,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)>
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let registration_started_at = Instant::now();
        let buf_size = CHANNEL_SIZE.max(buffer_size);
        let config = app_state.app_config.config.load();
        let min_buffer_bytes = resolve_min_burst_buffer_bytes(&config);
        let low_priority_preempted = app_state
            .app_config
            .custom_stream_response
            .load()
            .as_ref()
            .and_then(|c| c.low_priority_preempted.clone());
        let shared_state = Arc::new(
            SharedStreamState::new(
                headers,
                buf_size,
                provider_handle,
                min_buffer_bytes,
                low_priority_preempted,
            )
            .with_subscriber_idle_timeout_secs(
                config
                    .reverse_proxy
                    .as_ref()
                    .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
                    .map_or(DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS, |stream| stream.shared_subscriber_idle_timeout_secs),
            ),
        );
        app_state.shared_stream_manager.register(addr, stream_url, Arc::clone(&shared_state)).await;
        app_state.active_provider.make_shared_connection(addr, stream_url).await;
        let subscribed_stream = Self::subscribe_shared_stream(app_state, stream_url, addr, user_priority, connection_kind).await;
        debug_if_enabled!(
            "Shared stream startup register+subscribe completed for {} in {} ms",
            sanitize_sensitive_info(stream_url),
            registration_started_at.elapsed().as_millis()
        );
        if subscribed_stream.is_some() {
            shared_state.broadcast(
                stream_url,
                bytes_stream,
                Arc::clone(&app_state.shared_stream_manager),
            );
            debug_if_enabled!(
                "Created shared provider stream {} (channel_capacity={buf_size}, burst_buffer_min={min_buffer_bytes} bytes)",
                sanitize_sensitive_info(stream_url)
            );
        } else {
            warn!(
                "Shared stream subscribe failed for {}; broadcaster will not start",
                sanitize_sensitive_info(stream_url)
            );
        }
        subscribed_stream
    }

    pub async fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
        addr: &SocketAddr,
        user_priority: i8,
        connection_kind: crate::api::model::active_provider_manager::ConnectionKind,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)> {
        let manager = Arc::clone(&app_state.shared_stream_manager);
        let connection_manager = Arc::clone(&app_state.connection_manager);
        if let Some(result) = app_state
            .shared_stream_manager
            .subscribe_stream(stream_url, addr, manager, connection_manager)
            .await
        {
            match app_state
                .active_provider
                .add_shared_connection(
                    addr,
                    stream_url,
                    user_priority,
                    connection_kind,
                )
                .await
            {
                Ok(()) => Some(result),
                Err(err) => {
                    warn!(
                        "Rolling back shared stream subscriber {} for {}: {}",
                        sanitize_sensitive_info(&addr.to_string()),
                        sanitize_sensitive_info(stream_url),
                        sanitize_sensitive_info(&err)
                    );
                    app_state.shared_stream_manager.release_connection(addr, true).await;
                    None
                }
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        send_client_chunk, BurstBuffer, SharedStreamManager, SharedStreamState, CHANNEL_SIZE,
        MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES,
    };
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, ConnectionKind, ConnectionManager,
            EventManager,
        },
        auth::Fingerprint,
        model::{AppConfig, Config, ConfigInput, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use bytes::Bytes;
    use futures::StreamExt;
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, XtreamCluster},
        utils::Internable,
    };
    use std::borrow::Cow;
    use std::{collections::HashMap, net::SocketAddr, sync::Arc};
    use tokio::{sync::mpsc, time::{timeout, Duration}};
    use tokio_stream::wrappers::ReceiverStream;
    use tokio_util::sync::CancellationToken;

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
        let connection_manager = Arc::new(ConnectionManager::new(
            &user_manager,
            &provider_manager,
            &shared_manager,
            event_manager,
            None,
        ));
        (provider_manager, user_manager, shared_manager, connection_manager)
    }

    #[allow(clippy::too_many_arguments)]
    async fn register_active_shared_test_stream(
        provider_manager: &Arc<ActiveProviderManager>,
        user_manager: &Arc<ActiveUserManager>,
        shared_manager: &Arc<SharedStreamManager>,
        connection_manager: &Arc<ConnectionManager>,
        state: Arc<SharedStreamState>,
        stream_url: &str,
        addr: SocketAddr,
        uid: u32,
    ) {
        let input_name = "provider_1".intern();
        let channel = create_test_stream_channel(stream_url);
        let fingerprint = Fingerprint::new("client-key".to_string(), "127.0.0.1".to_string(), addr);

        provider_manager
            .acquire_connection(&input_name, &addr, 0, ConnectionKind::Normal)
            .await
            .unwrap_or_else(|| panic!("provider allocation expected"));
        provider_manager.make_shared_connection(&addr, stream_url).await;
        shared_manager.register(&addr, stream_url, state).await;

        connection_manager.add_connection(&addr).await;
        let stream_info = user_manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: input_name,
                stream_channel: &channel,
                user_agent: Cow::Borrowed("test"),
                session_token: None,
            })
            .await
            .unwrap_or_else(|| panic!("active user stream expected"));
        assert_eq!(stream_info.addr, addr);
    }

    #[tokio::test]
    async fn replacing_subscriber_token_cancels_previous_subscriber() {
        let state = SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None);
        let addr: SocketAddr = "127.0.0.1:41003".parse().unwrap_or_else(|_| unreachable!());
        let old_token = CancellationToken::new();
        let new_token = CancellationToken::new();

        let old_owner = state.register_subscriber(&addr, old_token.clone()).await;
        assert_eq!(old_owner.id, 1);

        let new_owner = state.register_subscriber(&addr, new_token).await;
        assert_eq!(new_owner.id, 2);
        assert!(old_token.is_cancelled(), "replaced subscriber token must be cancelled");
    }

    #[tokio::test]
    async fn send_client_chunk_returns_when_cancelled_while_queue_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send(Bytes::from_static(b"queued"))
            .await
            .unwrap_or_else(|_| panic!("initial send should fill queue"));
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let result = timeout(
            Duration::from_secs(1),
            send_client_chunk(&tx, Bytes::from_static(b"blocked"), &cancel_token),
        )
        .await;

        assert!(result.is_ok(), "cancelled send must not wait for channel capacity");
        assert!(!result.unwrap_or_else(|_| unreachable!()));
    }

    #[tokio::test]
    async fn cleanup_subscriber_releases_user_stream_and_provider_subscriber() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let (provider_manager, user_manager, shared_manager, connection_manager) =
            create_test_connection_manager(&app_cfg, &event_manager);

        let stream_url = "https://example.invalid/live/shared.ts";
        let addr: SocketAddr = "127.0.0.1:41004".parse().unwrap_or_else(|_| unreachable!());
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));
        let owner = state.register_subscriber(&addr, CancellationToken::new()).await;
        register_active_shared_test_stream(
            &provider_manager,
            &user_manager,
            &shared_manager,
            &connection_manager,
            Arc::clone(&state),
            stream_url,
            addr,
            1,
        )
        .await;
        assert_eq!(user_manager.active_streams().await.len(), 1);
        assert_eq!(provider_manager.get_provider_connections_count().await, 1);

        SharedStreamState::cleanup_subscriber(&state, &shared_manager, &connection_manager, &addr, owner).await;

        assert!(user_manager.active_streams().await.is_empty());
        assert_eq!(provider_manager.get_provider_connections_count().await, 0);
        let register = shared_manager.shared_streams.read().await;
        assert!(!register.by_key.contains_key(stream_url));
        assert!(!register.key_by_addr.contains_key(&addr));
    }

    #[tokio::test]
    async fn stale_replaced_subscriber_cannot_cleanup_current_subscriber() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let (provider_manager, user_manager, shared_manager, connection_manager) =
            create_test_connection_manager(&app_cfg, &event_manager);

        let stream_url = "https://example.invalid/live/replaced.ts";
        let addr: SocketAddr = "127.0.0.1:41005".parse().unwrap_or_else(|_| unreachable!());
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));
        let stale_owner = state.register_subscriber(&addr, CancellationToken::new()).await;
        let current_owner = state.register_subscriber(&addr, CancellationToken::new()).await;
        assert_ne!(stale_owner, current_owner);

        register_active_shared_test_stream(
            &provider_manager,
            &user_manager,
            &shared_manager,
            &connection_manager,
            Arc::clone(&state),
            stream_url,
            addr,
            2,
        )
        .await;

        SharedStreamState::cleanup_subscriber(&state, &shared_manager, &connection_manager, &addr, stale_owner).await;

        assert_eq!(user_manager.active_streams().await.len(), 1);
        assert_eq!(provider_manager.get_provider_connections_count().await, 1);
        let register = shared_manager.shared_streams.read().await;
        assert!(register.by_key.contains_key(stream_url));
        assert!(register.key_by_addr.contains_key(&addr));
    }

    #[tokio::test]
    async fn unregister_source_end_cancels_remaining_subscriber_tasks_without_aborting() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

        let stream_url = "https://example.invalid/live/source-ended.ts";
        let addr: SocketAddr = "127.0.0.1:41006".parse().unwrap_or_else(|_| unreachable!());
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024, None));
        let cancel_token = CancellationToken::new();
        state.register_subscriber(&addr, cancel_token.clone()).await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut handles = state.task_handles.write().await;
            handles.push(tokio::spawn(async move {
                cancel_token.cancelled().await;
                let _ = tx.send(());
            }));
        }
        shared_manager.register(&addr, stream_url, Arc::clone(&state)).await;

        shared_manager.unregister(stream_url, false).await;

        assert!(
            timeout(Duration::from_secs(1), rx).await.is_ok(),
            "source-end unregister must cancel subscriber tasks instead of aborting them"
        );
    }

    #[tokio::test]
    async fn test_duplicate_release_connection_is_idempotent_with_remaining_subscribers() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

        let stream_url = "https://example.invalid/live/stream.ts";
        let addr_1: SocketAddr = "127.0.0.1:41001".parse().unwrap_or_else(|_| unreachable!());
        let addr_2: SocketAddr = "127.0.0.1:41002".parse().unwrap_or_else(|_| unreachable!());

        let state = Arc::new(SharedStreamState::new(
            Vec::new(),
            CHANNEL_SIZE.max(8),
            None,
            1024,
            None,
        ));

        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
            reg.key_by_addr.insert(addr_1, Arc::from(stream_url));
            reg.key_by_addr.insert(addr_2, Arc::from(stream_url));
        }

        state.register_subscriber(&addr_1, CancellationToken::new()).await;
        state.register_subscriber(&addr_2, CancellationToken::new()).await;

        shared_manager.release_connection(&addr_1, false).await;
        {
            let subs = state.subscribers.read().await;
            assert_eq!(subs.len(), 1);
            assert!(subs.contains_key(&addr_2));
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let subs = state.subscribers.read().await;
            assert_eq!(subs.len(), 1);
            assert!(subs.contains_key(&addr_2));
        }

        let reg = shared_manager.shared_streams.read().await;
        assert!(reg.by_key.contains_key(stream_url));
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
        let read = buffer.read_from_into(0, &mut chunks);

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
        let read = buffer.read_from_into(0, &mut chunks);

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
        let read = buffer.read_from_into(0, &mut chunks);

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
        let read = buffer.read_from_into(0, &mut chunks);

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
        let _read = buffer.read_from_into(0, &mut chunks);
        let Some(read_chunk) = chunks.first() else {
            panic!("expected one buffered chunk");
        };

        assert_eq!(read_chunk.as_ptr(), ptr);
    }

    #[tokio::test]
    async fn test_duplicate_release_connection_is_idempotent_with_single_subscriber() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

        let stream_url = "https://example.invalid/live/single.ts";
        let addr_1: SocketAddr = "127.0.0.1:42001".parse().unwrap_or_else(|_| unreachable!());
        let state = Arc::new(SharedStreamState::new(
            Vec::new(),
            CHANNEL_SIZE.max(8),
            None,
            1024,
            None,
        ));

        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(Arc::from(stream_url), Arc::clone(&state));
            reg.key_by_addr.insert(addr_1, Arc::from(stream_url));
        }

        state.register_subscriber(&addr_1, CancellationToken::new()).await;

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url));
            assert!(!reg.key_by_addr.contains_key(&addr_1));
        }
        {
            let subs = state.subscribers.read().await;
            assert!(subs.is_empty());
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url));
            assert!(!reg.key_by_addr.contains_key(&addr_1));
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
        let connection_manager = Arc::new(ConnectionManager::new(
            &user_manager,
            &provider_manager,
            &shared_manager,
            &event_manager,
            None,
        ));

        let addr: SocketAddr = "127.0.0.1:43001".parse().unwrap_or_else(|_| unreachable!());
        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;

        let low_priority_fallback = crate::api::model::TransportStreamBuffer::new(ts_packet);
        let state = Arc::new(SharedStreamState::new(
            Vec::new(),
            CHANNEL_SIZE.max(8),
            None,
            1024,
            Some(low_priority_fallback),
        ));

        let (mut stream, _provider) = state
            .subscribe(&addr, Arc::clone(&shared_manager), connection_manager)
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
        state.broadcast(
            stream_url,
            ReceiverStream::new(rx),
            Arc::clone(&shared_manager),
        );

        assert!(
            timeout(Duration::from_millis(100), events.recv()).await.is_err(),
            "broadcast startup must not emit runtime events"
        );

        // Pushing data must also not emit a per-chunk health event.
        tx.send(Ok(Bytes::from_static(b"payload-1")))
            .await
            .unwrap_or_else(|_| panic!("send chunk should succeed"));
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
}
