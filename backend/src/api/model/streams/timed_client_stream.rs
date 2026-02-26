use crate::{
    api::model::{stream_error::StreamError, AppState, BoxedProviderStream},
    utils::debug_if_enabled,
};
use bytes::Bytes;
use futures::Stream;
use shared::{
    model::VirtualId,
    utils::{default_kick_secs, sanitize_sensitive_info},
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

pub struct TimedClientStream {
    inner: BoxedProviderStream,
    deadline: Instant,
    app_state: Arc<AppState>,
    addr: SocketAddr,
    virtual_id: VirtualId,
}

impl TimedClientStream {
    pub(crate) fn new(
        app_state: &Arc<AppState>,
        inner: BoxedProviderStream,
        duration: u32,
        addr: SocketAddr,
        virtual_id: VirtualId,
    ) -> Self {
        let deadline = Instant::now() + Duration::from_secs(u64::from(duration));
        Self { inner, deadline, app_state: Arc::clone(app_state), addr, virtual_id }
    }
}
impl Stream for TimedClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        if Instant::now() >= self.deadline {
            let kick_secs = self
                .app_state
                .app_config
                .config
                .load()
                .web_ui
                .as_ref()
                .map_or_else(default_kick_secs, |wc| wc.kick_secs);
            let connection_manager = Arc::clone(&self.app_state.connection_manager);
            let addr = self.addr;
            let virtual_id = self.virtual_id;
            debug_if_enabled!(
                "TimedClient stream exceeds time limit. Closing stream with virtual_id {virtual_id} for addr: {}",
                sanitize_sensitive_info(&addr.to_string())
            );
            tokio::spawn(async move {
                connection_manager.kick_connection(&addr, virtual_id, kick_secs).await;
            });
            return Poll::Ready(None);
        }
        Pin::as_mut(&mut self.inner).poll_next(cx)
    }
}
