use crate::api::model::AppState;
use crate::api::model::{ActiveProviderManager, ProviderHandle, StreamError};
use crate::model::Config;
use crate::utils::debug_if_enabled;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::api::model::streams::buffered_stream::CHANNEL_SIZE;
use crate::api::model::BoxedProviderStream;
use crate::utils::trace_if_enabled;
use log::{debug, trace, warn};
use shared::utils::sanitize_sensitive_info;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration, Instant};
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
        Self {
            buffer: VecDeque::with_capacity(buf_size),
            buffer_size: buf_size,
            current_bytes: 0,
        }
    }

    pub fn snapshot(&self) -> VecDeque<Arc<Bytes>> {
        self.buffer.iter().cloned().collect::<VecDeque<Arc<Bytes>>>()
    }

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
    cancellation_token: &CancellationToken) {
    for buf in start_buffer {
        if cancellation_token.is_cancelled() { return; }
        if let Err(err) = client_tx.send(buf.as_ref().clone()).await {
            warn!("Error sending burst-buffer chunk to client: {err}");
            return; // stop on send error
        }
    }
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
    fn new(headers: Vec<(String, String)>, buf_size: usize,
           provider_guard: Option<ProviderHandle>, min_burst_buffer_size: usize) -> Self {
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

    async fn subscribe(&self, addr: &SocketAddr, manager: Arc<SharedStreamManager>) -> (BoxedProviderStream, Option<Arc<str>>) {
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
            debug_if_enabled!("Shared stream subscriber added {}; total subscribers={}",
                sanitize_sensitive_info(&addr.to_string()), subs.len());
        }

        let client_tx_clone = client_tx.clone();
        let burst_buffer = self.burst_buffer.clone();
        let burst_buffer_for_log = Arc::clone(&self.burst_buffer);
        let yield_counter = YIELD_COUNTER;

        // If a client stops streaming (for example presses
        let timeout_duration = Duration::from_secs(300); // 5 minutes
        let mut last_active = Instant::now();
        let mut last_lag_log = Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now);

        let address = *addr;
        let handle = tokio::spawn(async move {
            // initial burst buffer
            let snapshot = {
                let buffer = burst_buffer.read().await;
                buffer.snapshot()
            };
            send_burst_buffer(&snapshot, &client_tx_clone, &cancel_token).await;

            let mut loop_cnt = 0;
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
                            // If the client press pause, skip
                            if client_tx_clone.is_closed() {
                                continue;
                            }

                            if let Err(err) = client_tx.send(data).await {
                                debug!("Shared stream client send error: {address} {err}");
                                break;
                            }
                            loop_cnt += 1;
                            last_active = Instant::now();

                            if loop_cnt >= yield_counter {
                                tokio::task::yield_now().await;
                                loop_cnt = 0;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            if last_lag_log.elapsed() > Duration::from_secs(5) {
                                let buffered_bytes = {
                                    let buffer = burst_buffer_for_log.read().await;
                                    buffer.current_bytes
                                };
                                warn!("Shared stream client lagged behind {address}. Skipped {skipped} messages (buffered {buffered_bytes} bytes, yield counter {yield_counter})");
                                last_lag_log = Instant::now();
                            }
                            tokio::task::yield_now().await;
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

    fn broadcast<S, E>(
        &self,
        stream_url: &str,
        bytes_stream: S,
        shared_streams: Arc<SharedStreamManager>,
    )
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let mut source_stream = Box::pin(bytes_stream);
        let streaming_url = stream_url.to_string();
        let sender = self.broadcaster.clone();
        let stop_token = self.stop_token.clone();
        let burst_buffer = self.burst_buffer.clone();

        tokio::spawn(async move {
            let mut counter = 0usize;
            loop {
                tokio::select! {
                  biased;

                  () = stop_token.cancelled() => {
                       debug_if_enabled!("No shared stream subscribers left. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
                        break;
                  },

                  item = source_stream.next() => {
                     match item {
                        Some(Ok(data)) => {
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
                                   debug_if_enabled!("Shared stream send error,no subscribers closing {}", sanitize_sensitive_info(&streaming_url));
                                   break;
                            }
                          }
                        }
                        Some(Err(e)) => {
                            trace!("Shared stream received error: {e:?}");
                            tokio::task::yield_now().await;

                        }
                        None => {
                            debug_if_enabled!("Source stream ended. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
                            break;
                        }
                    }
                  },
               }
            }
            debug_if_enabled!("Shared stream exhausted. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
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
        Self {
            provider_manager,
            shared_streams: RwLock::new(SharedStreamsRegister::default()),
        }
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

            let remove_keys: Vec<SocketAddr> = shared_streams.key_by_addr
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
            debug_if_enabled!("Unregistering shared stream {} (remaining_subscribers={remaining}, send_stop_signal={send_stop_signal})",
            sanitize_sensitive_info(stream_url));

            for handle in shared_state.task_handles.write().await.drain(..) {
                handle.abort();
            }

            if let Some(provider_handle) = &shared_state.provider_guard {
                self.provider_manager.release_handle(provider_handle).await;
            }

            if send_stop_signal || remaining == 0 {
                trace_if_enabled!("Sending shared stream stop signal {}", sanitize_sensitive_info(stream_url));
                shared_state.stop_token.cancel();
            }
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

            debug_if_enabled!("Shared stream subscriber removed {}; remaining subscribers={remaining}", sanitize_sensitive_info(&addr.to_string()));

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

            if let Some(client_stop_signal) = tx {
                client_stop_signal.cancel();
            }
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
            debug_if_enabled!("Responding to existing shared client stream {} {}",
                sanitize_sensitive_info(&addr.to_string()), sanitize_sensitive_info(stream_url));
            Some(shared_state.subscribe(addr, manager).await)
        } else {
            None
        }
    }


    async fn register(&self, addr: &SocketAddr, stream_url: &str, shared_state: Arc<SharedStreamState>) {
        let mut shared_streams = self.shared_streams.write().await;
        shared_streams.by_key.insert(stream_url.to_string(), shared_state);
        shared_streams.key_by_addr.insert(*addr, stream_url.to_string());
        debug_if_enabled!("Registered shared stream {} for initial subscriber {}",
            sanitize_sensitive_info(stream_url), sanitize_sensitive_info(&addr.to_string()));
    }

    pub(crate) async fn register_shared_stream<S, E>(
        app_state: &AppState,
        stream_url: &str,
        bytes_stream: S,
        addr: &SocketAddr,
        headers: Vec<(String, String)>,
        buffer_size: usize,
        provider_handle: Option<ProviderHandle>) -> Option<(BoxedProviderStream, Option<Arc<str>>)>
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let buf_size = CHANNEL_SIZE.max(buffer_size);
        let config = app_state.app_config.config.load();
        let min_buffer_bytes = resolve_min_burst_buffer_bytes(&config);
        let shared_state = Arc::new(SharedStreamState::new(headers, buf_size, provider_handle, min_buffer_bytes));
        app_state.shared_stream_manager.register(addr, stream_url, Arc::clone(&shared_state)).await;
        app_state.active_provider.make_shared_connection(addr, stream_url).await;
        let subscribed_stream = Self::subscribe_shared_stream(app_state, stream_url, addr).await;
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(&app_state.shared_stream_manager));
        debug_if_enabled!("Created shared provider stream {} (channel_capacity={buf_size}, burst_buffer_min={min_buffer_bytes} bytes)",
            sanitize_sensitive_info(stream_url));
        subscribed_stream
    }

    /// Creates a broadcast notify stream for the given URL if a shared stream exists.
    pub async fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
        addr: &SocketAddr,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>)> {
        let manager = Arc::clone(&app_state.shared_stream_manager);
        if let Some(result) = app_state.shared_stream_manager.subscribe_stream(stream_url, addr, manager).await {
            app_state.active_provider.add_shared_connection(addr, stream_url).await;
            Some(result)
        } else {
            None
        }
    }
}
