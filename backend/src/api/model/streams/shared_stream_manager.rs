use crate::api::model::app_state::AppState;
use crate::api::model::stream_error::StreamError;
use crate::api::model::streams::provider_stream_factory::STREAM_QUEUE_SIZE;
use crate::utils::debug_if_enabled;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use std::sync::Arc;

use crate::api::model::active_provider_manager::ProviderConnectionGuard;
use crate::api::model::stream::BoxedProviderStream;
use dashmap::DashMap;
use log::{debug, trace};
use shared::utils::sanitize_sensitive_info;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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

fn convert_stream(stream: BoxStream<Bytes>) -> BoxStream<Result<Bytes, StreamError>> {
    ReceiverStreamWrapper { stream }.boxed()
}

type SubscriberId = String;

/// Represents the state of a shared provider URL.
///
/// - `headers`: The initial connection headers used during the setup of the shared stream.
struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    provider_guard: Option<Arc<ProviderConnectionGuard>>,
    subscribers: Arc<DashMap<SubscriberId, tokio::sync::watch::Sender<bool>>>,
    broadcaster: tokio::sync::broadcast::Sender<Bytes>,
    stop_tx: tokio::sync::watch::Sender<bool>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
}

impl Drop for SharedStreamState {
    fn drop(&mut self) {
        if let Some(guard) = self.provider_guard.as_ref() {
            guard.force_release();
        }
    }
}

impl SharedStreamState {
    fn new(headers: Vec<(String, String)>, buf_size: usize,
           provider_guard: Option<Arc<ProviderConnectionGuard>>) -> Self {
        if let Some(guard) = &provider_guard {
            guard.disable_release();
        }
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (broadcaster, _) = tokio::sync::broadcast::channel(buf_size);
        Self {
            headers,
            buf_size,
            provider_guard,
            subscribers: Arc::new(DashMap::new()), //Arc::new(RwLock::new(Vec::new())),
            broadcaster,
            stop_tx,
            stop_rx,
        }
    }

    fn subscribe(&self, addr: &str, manager: Arc<SharedStreamManager>) -> BoxedProviderStream {
        let (client_tx, client_rx) = mpsc::channel(self.buf_size);
        let mut broadcast_rx = self.broadcaster.subscribe();
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        self.subscribers.insert(addr.to_string(), cancel_tx);

        let address = addr.to_string();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = broadcast_rx.recv() => {
                        match result {
                            Ok(data) => {
                                if let Err(err) = client_tx.send(data).await {
                                    debug!("Shared stream client send error: {address} {err}");
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                trace!("Client lagged behind. Skipped {skipped} messages. {address}");
                            }
                            Err(_) => break,
                        }
                    }
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            debug!("Client disconnected from shared stream: {address}");
                            break;
                        }
                    }
                }
            }
            manager.release_connection(&address);
        });
        convert_stream(ReceiverStream::new(client_rx).boxed())
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
        let mut stop_rx = self.stop_rx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                  item = source_stream.next() => {
                     match item {
                        Some(Ok(data)) => {
                            let _ = sender.send(data);
                        }
                        Some(Err(e)) => {
                            trace!("Shared stream received error: {e:?}");
                        }
                        None => {
                            debug_if_enabled!("Source stream ended. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
                            break;
                        }
                    }
                  },
                  _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                           debug_if_enabled!("No shared stream subscribers left. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
                            break;
                        }
                  }
               }
            }
            debug_if_enabled!("Shared stream exhausted. Closing shared provider stream {}", sanitize_sensitive_info(&streaming_url));
            shared_streams.unregister(&streaming_url);
        });
    }
}

type SharedStreamRegister = DashMap<String, SharedStreamState>;
type SharedStreamAddrRegister = DashMap<String, String>;


pub struct SharedStreamManager {
    shared_streams: SharedStreamRegister,
    shared_streams_by_addr: SharedStreamAddrRegister,
}

impl SharedStreamManager {
    pub(crate) fn new() -> Self {
        Self {
            shared_streams: SharedStreamRegister::new(),
            shared_streams_by_addr: SharedStreamAddrRegister::new(),
        }
    }

    pub fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.shared_streams.get(stream_url).map(|s| s.headers.clone())
    }


    fn unregister(&self, stream_url: &str) {
        debug_if_enabled!("Unregistering shared stream {}", sanitize_sensitive_info(stream_url));
        if let Some((_, shared_state)) = self.shared_streams.remove(stream_url) {
            debug_if_enabled!("No active subscribers. Closing shared provider stream {}", sanitize_sensitive_info(stream_url));
            let _ = shared_state.stop_tx.send(true);
            if let Some(guard) = &shared_state.provider_guard {
                guard.force_release();
            }
        }
    }

    pub fn release_connection(&self, addr: &str) {
        let stream_url = self.shared_streams_by_addr.remove(addr).map(|(_key, value)| value);
        if let Some(stream_url) = stream_url {
            debug!("Release shared stream {addr}");
            if let Some(mut entry) = self.shared_streams.get_mut(&stream_url) {
                let shared_state = &mut *entry;
                if let Some((_, tx)) = shared_state.subscribers.remove(addr) {
                    let _ = tx.send(true);
                }
                if shared_state.subscribers.is_empty() {
                    drop(entry); // Release the lock before unregistering
                    self.unregister(&stream_url);
                }
            }
        }
    }

    fn subscribe_stream(&self, stream_url: &str, addr: Option<&str>, manager: Arc<SharedStreamManager>) -> Option<BoxedProviderStream> {
        match self.shared_streams.get(stream_url) {
            None => None,
            Some(stream_state) => {
                debug_if_enabled!("Responding to existing shared client stream {}", sanitize_sensitive_info(stream_url));
                if let Some(address) = addr {
                    self.shared_streams_by_addr.insert(address.to_string(), stream_url.to_string());
                    Some(stream_state.subscribe(address, manager))
                } else {
                    None
                }
            }
        }
    }

    fn register(&self, stream_url: &str, shared_state: SharedStreamState) {
        let _ = self.shared_streams.insert(stream_url.to_string(), shared_state);
    }

    pub(crate) fn subscribe<S, E>(
        app_state: &AppState,
        stream_url: &str,
        bytes_stream: S,
        headers: Vec<(String, String)>,
        buffer_size: usize,
        provider_guard: Option<Arc<ProviderConnectionGuard>>) -> Option<BoxedProviderStream>
    where
        S: Stream<Item=Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let buf_size = STREAM_QUEUE_SIZE.max(buffer_size);
        let shared_state = SharedStreamState::new(headers, buf_size, provider_guard);
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(&app_state.shared_stream_manager));
        app_state.shared_stream_manager.register(stream_url, shared_state);
        debug_if_enabled!("Created shared provider stream {}", sanitize_sensitive_info(stream_url));
        Self::subscribe_shared_stream(app_state, stream_url, None)
    }

    /// Creates a broadcast notify stream for the given URL if a shared stream exists.
    pub fn subscribe_shared_stream(
        app_state: &AppState,
        stream_url: &str,
        addr: Option<&str>,
    ) -> Option<BoxedProviderStream> {
        let manager = Arc::clone(&app_state.shared_stream_manager);
        app_state.shared_stream_manager.subscribe_stream(stream_url, addr, manager)
    }
}