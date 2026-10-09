use super::{
    PendingJoinGuard, PendingSharedMeterRegistration, PendingSharedSubscriberCleanup, ReceiverStreamWrapper,
    SharedStreamState, DEFAULT_SHARED_BUFFER_SIZE_BYTES,
};
use crate::{CleanupEvent, SharedCleanupCapability};
use bytes::Bytes;
use futures::Stream;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tuliprox_core::model::{Config, SharedSubscriberId, StreamError};

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

impl PendingJoinGuard {
    pub(super) fn new(state: &Arc<SharedStreamState>) -> Self {
        state.increment_pending_joins();
        Self { state: Arc::clone(state), armed: true }
    }

    pub(super) fn commit(mut self) {
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
    pub(super) fn new(
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

    pub(super) fn take_cleanup(
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

impl<S> ReceiverStreamWrapper<S> {
    pub(super) fn release(&mut self) {
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

pub(super) fn resolve_min_burst_buffer_bytes(config: &Config) -> usize {
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
