use crate::{
    api::model::{
        streams::buffered_stream::CHANNEL_SIZE, ActiveProviderManager, AppState, BoxedProviderStream, ProviderHandle,
        StreamError, STREAM_IDLE_TIMEOUT,
    },
    model::Config,
    utils::{debug_if_enabled, trace_if_enabled},
};
use bytes::Bytes;
use futures::{stream::BoxStream, Stream, StreamExt};
use log::{debug, trace, warn};
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    fmt::{Debug, Formatter},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, mpsc::Sender, RwLock},
    time::{sleep, Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

const DEFAULT_SHARED_BUFFER_SIZE_BYTES: usize = 1024 * 1024 * 12; // 12 MB

const YIELD_COUNTER: usize = 64;

///
/// Wraps a `ReceiverStream` as Stream<Item = Result<Bytes, `StreamError`>>
///
struct ReceiverStreamWrapper<S> {
    stream: S,
}

impl<S> Stream for ReceiverStreamWrapper<S>
where
    S: Stream<Item = Bytes> + Unpin,
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

struct BurstBuffer {
    buffer: VecDeque<Arc<Bytes>>,
    buffer_size: usize,
    current_bytes: usize,
}

#[allow(clippy::missing_fields_in_debug)]
impl Debug for BurstBuffer {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BurstBuffer")
            .field("buffer_size", &self.buffer_size)
            .field("current_bytes", &self.current_bytes)
            .finish()
    }
}

impl BurstBuffer {
    pub fn new(buf_size: usize) -> Self {
        Self { buffer: VecDeque::with_capacity(buf_size), buffer_size: buf_size, current_bytes: 0 }
    }

    pub fn snapshot(&self) -> VecDeque<Arc<Bytes>> { self.buffer.iter().cloned().collect::<VecDeque<Arc<Bytes>>>() }

    pub fn push(&mut self, packet: Arc<Bytes>) {
        while self.current_bytes + packet.len() > self.buffer_size {
            if let Some(popped) = self.buffer.pop_front() {
                self.current_bytes -= popped.len();
            } else {
                self.current_bytes = 0;
                break;
            }
        }
        self.current_bytes += packet.len();
        self.buffer.push_back(packet);
    }
}

async fn send_burst_buffer(
    start_buffer: &VecDeque<Arc<Bytes>>,
    client_tx: &Sender<Bytes>,
    cancellation_token: &CancellationToken,
) -> usize {
    let mut sent = 0_usize;
    for buf in start_buffer {
        if cancellation_token.is_cancelled() {
            return sent;
        }
        if let Err(err) = client_tx.send(buf.as_ref().clone()).await {
            debug!("Failed sending burst-buffer chunk to client: {err}");
            return sent; // stop on send error
        }
        sent = sent.saturating_add(1);
    }
    sent
}

/// Represents the state of a shared provider URL.
///
/// - `headers`: The initial connection headers used during the setup of the shared stream.
#[derive(Debug)]
pub struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    provider_guard: Option<ProviderHandle>,
    subscribers: RwLock<HashMap<SubscriberId, CancellationToken>>,
    broadcaster: tokio::sync::broadcast::Sender<Bytes>,
    stop_token: CancellationToken,
    burst_buffer: Arc<RwLock<BurstBuffer>>,
    task_handles: RwLock<Vec<tokio::task::JoinHandle<()>>>,
}

impl SharedStreamState {
    fn new(
        headers: Vec<(String, String)>,
        buf_size: usize,
        provider_guard: Option<ProviderHandle>,
        min_burst_buffer_size: usize,
    ) -> Self {
        let (broadcaster, _) = tokio::sync::broadcast::channel(buf_size);
        // TODO channel size versus byte size,  channels are chunk sized, burst_buffer byte sized
        let burst_buffer_size_in_bytes = min_burst_buffer_size.max(buf_size * 1024 * 12);
        Self {
            headers,
            buf_size,
            provider_guard,
            subscribers: RwLock::new(HashMap::new()),
            broadcaster,
            stop_token: CancellationToken::new(),
            burst_buffer: Arc::new(RwLock::new(BurstBuffer::new(burst_buffer_size_in_bytes))),
            task_handles: RwLock::new(Vec::new()),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn subscribe(
        &self,
        addr: &SocketAddr,
        manager: Arc<SharedStreamManager>,
    ) -> (BoxedProviderStream, Option<Arc<str>>) {
        let (client_tx, client_rx) = mpsc::channel(self.buf_size);
        let mut broadcast_rx = self.broadcaster.subscribe();
        let cancel_token = CancellationToken::new();

        {
            let mut handles = self.task_handles.write().await;
            handles.retain(|h| !h.is_finished());
        }

        {
            let mut subs = self.subscribers.write().await;
            subs.insert(*addr, cancel_token.clone());
            debug_if_enabled!(
                "Shared stream subscriber added {}; total subscribers={}",
                sanitize_sensitive_info(&addr.to_string()),
                subs.len()
            );
        }

        let client_tx_clone = client_tx.clone();
        let burst_buffer = self.burst_buffer.clone();
        let burst_buffer_for_log = Arc::clone(&self.burst_buffer);
        let yield_counter = YIELD_COUNTER;

        // If a client stops streaming (for example presses
        let timeout_duration = Duration::from_secs(300); // 5 minutes
        let mut last_active = Instant::now();
        let mut last_lag_log = Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now);
        let mut consecutive_lag_count: u32 = 0;
        let subscriber_buf_size = self.buf_size;

        let address = *addr;
        let subscriber_started_at = Instant::now();
        let handle = tokio::spawn(async move {
            // initial burst buffer
            let snapshot = {
                let buffer = burst_buffer.read().await;
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

            let mut loop_cnt = 0;
            let mut first_live_chunk_logged = false;
            let mut startup_chunks_sent = 0usize;
            let mut startup_bytes_sent = 0usize;
            let mut startup_stats_logged = false;
            loop {
                tokio::select! {
                    biased;

                        // canceled
                    () = cancel_token.cancelled() => {
                        debug!("Client disconnected from shared stream: {address}");
                        break;
                    }

                        // timeout handling
                    () = sleep(Duration::from_secs(1)) => {
                        if last_active.elapsed() > timeout_duration {
                            debug!("Client timed out due to inactivity: {address}");
                            cancel_token.cancel();
                            break;
                        }
                    }

                    // receive broadcast data
                    result = broadcast_rx.recv() => {
                        match result {
                            Ok(data) => {
                                consecutive_lag_count = 0;
                                // If the client press pause, skip
                                if client_tx_clone.is_closed() {
                                    continue;
                                }

                                let chunk_len = data.len();
                                if let Err(err) = client_tx.send(data).await {
                                    debug!("Shared stream client send error: {address} {err}");
                                    break;
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
                                loop_cnt += 1;
                                last_active = Instant::now();

                                if loop_cnt >= yield_counter {
                                    tokio::task::yield_now().await;
                                    loop_cnt = 0;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                consecutive_lag_count = consecutive_lag_count.saturating_add(1);
                                if last_lag_log.elapsed() > Duration::from_secs(5) {
                                    let buffered_bytes = {
                                        let buffer = burst_buffer_for_log.read().await;
                                        buffer.current_bytes
                                    };
                                    warn!(
                                        "Shared stream client lagged behind {address}. Skipped {skipped} messages \
                                         (buffered {buffered_bytes} bytes, yield counter {yield_counter}, \
                                         consecutive lags={consecutive_lag_count})"
                                    );
                                    last_lag_log = Instant::now();
                                }
                                // Exponential backoff for persistently lagging clients: 50 ms base,
                                // doubling up to 1 600 ms, to provide increasing backpressure without
                                // permanently blocking the subscriber task.
                                let backoff_ms = 50_u64
                                    .saturating_mul(
                                        1_u64.checked_shl(consecutive_lag_count.min(5)).unwrap_or(u64::MAX),
                                    )
                                    .min(1_600);
                                sleep(Duration::from_millis(backoff_ms)).await;
                            }
                            Err(_) => break,
                        }
                    }
                }
            }

            manager.release_connection(&address, false).await;
        });

        self.task_handles.write().await.push(handle);

        let provider = self.provider_guard.as_ref().and_then(|h| h.allocation.get_provider_name());
        (convert_stream(ReceiverStream::new(client_rx).boxed()), provider)
    }

    #[allow(clippy::too_many_lines)]
    fn broadcast<S, E>(&self, stream_url: &str, bytes_stream: S, shared_streams: Arc<SharedStreamManager>)
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let mut source_stream = Box::pin(bytes_stream);
        let streaming_url = stream_url.to_string();
        let sender = self.broadcaster.clone();
        let stop_token = self.stop_token.clone();
        let burst_buffer = self.burst_buffer.clone();
        let broadcast_started_at = Instant::now();

        tokio::spawn(async move {
            let mut counter = 0usize;
            let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
            let idle = sleep(idle_timeout);
            tokio::pin!(idle);
            let mut first_source_chunk_logged = false;
            let mut startup_chunks_seen = 0usize;
            let mut startup_bytes_seen = 0usize;
            let mut startup_stats_logged = false;

            loop {
                tokio::select! {
                   biased;

                   () = stop_token.cancelled() => {
                        debug_if_enabled!(
                            "No shared stream subscribers left. Closing shared provider stream {}",
                            sanitize_sensitive_info(&streaming_url)
                        );
                         break;
                   },

                   () = &mut idle => {
                        debug!("shared stream idle for too long, closing");
                         stop_token.cancel();
                        break;
                   }

                   chunk = source_stream.next() => {
                      idle.as_mut().reset(Instant::now() + idle_timeout);
                      match chunk {
                         Some(Ok(data)) => {
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
                               startup_bytes_seen = startup_bytes_seen.saturating_add(data.len());
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
                           let arc_data = Arc::new(data);
                           {
                             let mut buffer = burst_buffer.write().await;
                             buffer.push(arc_data.clone());
                           }

                             match sender.send(arc_data.as_ref().clone()) {
                             Ok(clients) =>  {
                                 if clients == 0 {
                                    debug_if_enabled!("No shared stream subscribers closing {}", sanitize_sensitive_info(&streaming_url));
                                    break;
                                 }
                                 counter += 1;
                                 if counter >= YIELD_COUNTER {
                                     tokio::task::yield_now().await;
                                     counter = 0;
                                 }
                             }
                             Err(_e) => {
                                    debug_if_enabled!(
                                        "Shared stream send error,no subscribers closing {}",
                                        sanitize_sensitive_info(&streaming_url)
                                    );
                                    break;
                             }
                           }
                         }
                         Some(Err(e)) => {
                             trace!("Shared stream received error: {e:?}");
                             tokio::task::yield_now().await;

                         }
                         None => {
                             debug_if_enabled!(
                                 "Source stream ended. Closing shared provider stream {}",
                                 sanitize_sensitive_info(&streaming_url)
                             );
                             break;
                         }
                     }
                   },
                }
            }
            debug_if_enabled!(
                "Shared stream exhausted. Closing shared provider stream {}",
                sanitize_sensitive_info(&streaming_url)
            );
            shared_streams.unregister(&streaming_url, false).await;
        });
    }
}

#[derive(Debug, Clone, Default)]
struct SharedStreamsRegister {
    by_key: HashMap<String, Arc<SharedStreamState>>,
    key_by_addr: HashMap<SubscriberId, String>,
}

pub struct SharedStreamManager {
    provider_manager: Arc<ActiveProviderManager>,
    shared_streams: RwLock<SharedStreamsRegister>,
}

impl SharedStreamManager {
    pub(crate) fn new(provider_manager: Arc<ActiveProviderManager>) -> Self {
        Self { provider_manager, shared_streams: RwLock::new(SharedStreamsRegister::default()) }
    }

    pub async fn get_shared_state(&self, stream_url: &str) -> Option<Arc<SharedStreamState>> {
        self.shared_streams.read().await.by_key.get(stream_url).map(Arc::clone)
    }

    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.get_shared_state(stream_url).await.map(|s| s.headers.clone())
    }

    async fn unregister(&self, stream_url: &str, send_stop_signal: bool) {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;

            let remove_keys: Vec<SocketAddr> = shared_streams
                .key_by_addr
                .iter()
                .filter_map(|(addr, url)| if url == stream_url { Some(*addr) } else { None })
                .collect();
            for k in remove_keys {
                shared_streams.key_by_addr.remove(&k);
            }

            shared_streams.by_key.remove(stream_url)
        };

        if let Some(shared_state) = shared_state_opt {
            let remaining = shared_state.subscribers.read().await.len();
            debug_if_enabled!(
                "Unregistering shared stream {} (remaining_subscribers={remaining}, send_stop_signal={send_stop_signal})",
                sanitize_sensitive_info(stream_url)
            );

            for handle in shared_state.task_handles.write().await.drain(..) {
                handle.abort();
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

    /// Tears down a shared stream that was preempted via priority eviction.
    /// Stops the broadcast task and removes subscriber mappings, but does NOT
    /// release the provider handle (the preemption path already handles that).
    ///
    /// # Precondition
    /// The caller **must** have already released the provider allocation for this stream
    /// before calling this method.  Failing to do so will leak the provider slot because
    /// `provider_guard` inside the shared state is intentionally not released here.
    pub async fn teardown_preempted_stream(&self, stream_url: &str) {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;

            let remove_keys: Vec<SocketAddr> = shared_streams
                .key_by_addr
                .iter()
                .filter_map(|(addr, url)| if url == stream_url { Some(*addr) } else { None })
                .collect();
            for k in remove_keys {
                shared_streams.key_by_addr.remove(&k);
            }

            shared_streams.by_key.remove(stream_url)
        };

        if let Some(shared_state) = shared_state_opt {
            debug_if_enabled!(
                "Tearing down preempted shared stream {}",
                sanitize_sensitive_info(stream_url)
            );

            for handle in shared_state.task_handles.write().await.drain(..) {
                handle.abort();
            }

            // Cancel stop_token to terminate the broadcast task.
            // Do NOT release provider_guard — the preemption caller already released the allocation.
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

            // Remove the addr → url mapping eagerly so key_by_addr does not leak when
            // other subscribers are still active (unregister only runs when is_empty).
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

            client_stop_signal.cancel();
        }
    }

    async fn subscribe_stream(
        &self,
        stream_url: &str,
        addr: &SocketAddr,
        manager: Arc<SharedStreamManager>,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)> {
        let shared_state_opt = {
            let mut shared_streams = self.shared_streams.write().await;
            if let Some(shared_state) = shared_streams.by_key.get(stream_url).cloned() {
                shared_streams.key_by_addr.insert(*addr, stream_url.to_owned());
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
            Some(shared_state.subscribe(addr, manager).await)
        } else {
            None
        }
    }

    async fn register(&self, addr: &SocketAddr, stream_url: &str, shared_state: Arc<SharedStreamState>) {
        let mut shared_streams = self.shared_streams.write().await;
        shared_streams.by_key.insert(stream_url.to_string(), shared_state);
        shared_streams.key_by_addr.insert(*addr, stream_url.to_string());
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
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let registration_started_at = Instant::now();
        let buf_size = CHANNEL_SIZE.max(buffer_size);
        let config = app_state.app_config.config.load();
        let min_buffer_bytes = resolve_min_burst_buffer_bytes(&config);
        let shared_state = Arc::new(SharedStreamState::new(headers, buf_size, provider_handle, min_buffer_bytes));
        app_state.shared_stream_manager.register(addr, stream_url, Arc::clone(&shared_state)).await;
        app_state.active_provider.make_shared_connection(addr, stream_url).await;
        let subscribed_stream = Self::subscribe_shared_stream(app_state, stream_url, addr, user_priority).await;
        debug_if_enabled!(
            "Shared stream startup register+subscribe completed for {} in {} ms",
            sanitize_sensitive_info(stream_url),
            registration_started_at.elapsed().as_millis()
        );
        if subscribed_stream.is_some() {
            shared_state.broadcast(stream_url, bytes_stream, Arc::clone(&app_state.shared_stream_manager));
            debug_if_enabled!(
                "Created shared provider stream {} (channel_capacity={buf_size}, burst_buffer_min={min_buffer_bytes} bytes)",
                sanitize_sensitive_info(stream_url)
            );
        }
        subscribed_stream
    }

    /// Creates a broadcast notify stream for the given URL if a shared stream exists.
    pub async fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
        addr: &SocketAddr,
        user_priority: i8,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)> {
        let manager = Arc::clone(&app_state.shared_stream_manager);
        if let Some(result) = app_state.shared_stream_manager.subscribe_stream(stream_url, addr, manager).await {
            match app_state.active_provider.add_shared_connection(addr, stream_url, user_priority).await {
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
    use super::{SharedStreamManager, SharedStreamState, CHANNEL_SIZE};
    use crate::{
        api::model::{ActiveProviderManager, EventManager},
        model::{AppConfig, Config, ConfigInput, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType},
        utils::Internable,
    };
    use std::{collections::HashMap, net::SocketAddr, sync::Arc};
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
            ffprobe_available: Arc::default(),
        }
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

        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024));

        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(stream_url.to_string(), Arc::clone(&state));
            reg.key_by_addr.insert(addr_1, stream_url.to_string());
            reg.key_by_addr.insert(addr_2, stream_url.to_string());
        }

        {
            let mut subs = state.subscribers.write().await;
            subs.insert(addr_1, CancellationToken::new());
            subs.insert(addr_2, CancellationToken::new());
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let subs = state.subscribers.read().await;
            assert_eq!(subs.len(), 1, "first release should remove exactly one subscriber");
            assert!(subs.contains_key(&addr_2), "remaining subscriber must stay registered");
        }

        // Duplicate release for the same address should be ignored.
        shared_manager.release_connection(&addr_1, false).await;
        {
            let subs = state.subscribers.read().await;
            assert_eq!(subs.len(), 1, "duplicate release must not modify subscribers");
            assert!(subs.contains_key(&addr_2), "remaining subscriber must stay registered");
        }

        let reg = shared_manager.shared_streams.read().await;
        assert!(reg.by_key.contains_key(stream_url), "shared stream must stay registered while one subscriber remains");
    }

    #[tokio::test]
    async fn test_duplicate_release_connection_is_idempotent_with_single_subscriber() {
        let app_cfg = create_test_app_config();
        let event_manager = Arc::new(EventManager::new());
        let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_manager = Arc::new(SharedStreamManager::new(provider_manager));

        let stream_url = "https://example.invalid/live/single.ts";
        let addr_1: SocketAddr = "127.0.0.1:42001".parse().unwrap_or_else(|_| unreachable!());
        let state = Arc::new(SharedStreamState::new(Vec::new(), CHANNEL_SIZE.max(8), None, 1024));

        {
            let mut reg = shared_manager.shared_streams.write().await;
            reg.by_key.insert(stream_url.to_string(), Arc::clone(&state));
            reg.key_by_addr.insert(addr_1, stream_url.to_string());
        }

        {
            let mut subs = state.subscribers.write().await;
            subs.insert(addr_1, CancellationToken::new());
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url), "stream should be unregistered after last subscriber leaves");
            assert!(!reg.key_by_addr.contains_key(&addr_1), "address mapping should be removed");
        }
        {
            let subs = state.subscribers.read().await;
            assert!(subs.is_empty(), "subscriber list should be empty after release");
        }

        shared_manager.release_connection(&addr_1, false).await;
        {
            let reg = shared_manager.shared_streams.read().await;
            assert!(!reg.by_key.contains_key(stream_url), "duplicate release must keep stream unregistered");
            assert!(!reg.key_by_addr.contains_key(&addr_1), "duplicate release must keep address mapping absent");
        }
        {
            let subs = state.subscribers.read().await;
            assert!(subs.is_empty(), "duplicate release must keep subscribers empty");
        }
    }
}
