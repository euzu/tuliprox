use crate::{
    api::model::{BoxedProviderStream, ConnectionManager},
    model::{AppConfig, StreamError},
    utils::debug_if_enabled,
};
use bytes::Bytes;
use futures::Stream;
use shared::{
    defaults::default_kick_secs,
    model::{DisconnectReason, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{future::Future, net::SocketAddr, pin::Pin, sync::Arc, task::Poll, time::Duration};
use tokio::time::{sleep_until, Instant, Sleep};

enum TimeoutAction {
    Kick {
        app_config: Arc<AppConfig>,
        connection_manager: Arc<ConnectionManager>,
        addr: SocketAddr,
        virtual_id: VirtualId,
    },
    Stop,
}

pub struct TimedClientStream {
    inner: BoxedProviderStream,
    /// `Sleep` future pinned in-place.  Polling it registers a waker with the
    /// Tokio runtime so this task is woken at the deadline regardless of whether
    /// the inner stream is producing data — unlike a plain `Instant` comparison,
    /// which is only evaluated when `poll_next` happens to be called.
    deadline: Pin<Box<Sleep>>,
    timeout_action: TimeoutAction,
}

impl TimedClientStream {
    pub(crate) fn new(
        app_config: &Arc<AppConfig>,
        connection_manager: &Arc<ConnectionManager>,
        inner: BoxedProviderStream,
        duration: u32,
        addr: SocketAddr,
        virtual_id: VirtualId,
    ) -> Self {
        let deadline = Box::pin(sleep_until(Instant::now() + Duration::from_secs(u64::from(duration))));
        Self {
            inner,
            deadline,
            timeout_action: TimeoutAction::Kick {
                app_config: Arc::clone(app_config),
                connection_manager: Arc::clone(connection_manager),
                addr,
                virtual_id,
            },
        }
    }

    pub(crate) fn new_without_kick(inner: BoxedProviderStream, duration: u32) -> Self {
        let deadline = Box::pin(sleep_until(Instant::now() + Duration::from_secs(u64::from(duration))));
        Self { inner, deadline, timeout_action: TimeoutAction::Stop }
    }
}

impl Stream for TimedClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        // Poll the sleep future first.  This registers a waker so the executor
        // wakes this task exactly when the deadline fires — even if the upstream
        // provider is stalled and emitting no data.
        if self.deadline.as_mut().poll(cx).is_ready() {
            if let TimeoutAction::Kick { app_config, connection_manager, addr, virtual_id } = &self.timeout_action {
                let kick_secs =
                    app_config.config.load().web_ui.as_ref().map_or_else(default_kick_secs, |wc| wc.kick_secs);
                let connection_manager = Arc::clone(connection_manager);
                let addr = *addr;
                let virtual_id = *virtual_id;
                debug_if_enabled!(
                    "TimedClient stream exceeds time limit. Closing stream with virtual_id {virtual_id} for addr: {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
                tokio::spawn(async move {
                    let _ = connection_manager
                        .close_connection_with_reason_and_block(&addr, virtual_id, kick_secs, DisconnectReason::Timeout)
                        .await;
                });
            }
            return Poll::Ready(None);
        }
        Pin::as_mut(&mut self.inner).poll_next(cx)
    }
}
