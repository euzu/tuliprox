use crate::api::model::{BoxedProviderStream, EventManager, StreamError};
use bytes::Bytes;
use futures::{
    stream::Stream,
    task::{Context, Poll},
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
};

/// Shared per-source meter state. The stream hot path only performs relaxed byte counting.
#[derive(Debug)]
pub struct StreamMeterHandle {
    meter_uid: u32,
    bytes_total: AtomicU64,
    bytes_window: AtomicU64,
    attached: AtomicBool,
    event_manager: Weak<EventManager>,
}

impl StreamMeterHandle {
    pub fn new(meter_uid: u32, event_manager: Weak<EventManager>) -> Self {
        Self {
            meter_uid,
            bytes_total: AtomicU64::new(0),
            bytes_window: AtomicU64::new(0),
            attached: AtomicBool::new(false),
            event_manager,
        }
    }

    pub fn meter_uid(&self) -> u32 { self.meter_uid }

    pub fn mark_attached(&self) { self.attached.store(true, Ordering::Release); }

    pub fn record_bytes(&self, len: u64) {
        self.bytes_total.fetch_add(len, Ordering::Relaxed);
        self.bytes_window.fetch_add(len, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MeterReading {
        MeterReading {
            meter_uid: self.meter_uid,
            bytes_total: self.bytes_total.load(Ordering::Relaxed),
            bytes_window: self.bytes_window.swap(0, Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterReading {
    pub meter_uid: u32,
    pub bytes_total: u64,
    pub bytes_window: u64,
}

/// Thin stream wrapper that only counts bytes in a shared meter handle.
pub struct MeteringStream {
    inner: BoxedProviderStream,
    meter: Arc<StreamMeterHandle>,
    event_manager: Arc<EventManager>,
}

impl MeteringStream {
    pub fn new(inner: BoxedProviderStream, meter: Arc<StreamMeterHandle>, event_manager: Arc<EventManager>) -> Self {
        meter.mark_attached();
        Self { inner, meter, event_manager }
    }
}

impl Stream for MeteringStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.meter.record_bytes(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            other => other,
        }
    }
}

impl Drop for MeteringStream {
    fn drop(&mut self) {
        let event_manager = Arc::clone(&self.event_manager);
        let meter_uid = self.meter.meter_uid();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                event_manager.flush_and_unregister_meter(meter_uid).await;
            });
        }
    }
}

impl Drop for StreamMeterHandle {
    fn drop(&mut self) {
        if self.attached.load(Ordering::Acquire) || self.meter_uid == 0 {
            return;
        }

        let Some(event_manager) = self.event_manager.upgrade() else {
            return;
        };

        let meter_uid = self.meter_uid;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                event_manager.flush_and_unregister_meter(meter_uid).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MeterReading, MeteringStream, StreamMeterHandle};
    use crate::api::model::{BoxedProviderStream, EventManager, StreamError};
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use std::sync::Arc;
    use tokio::{
        task::yield_now,
        time::{advance, Duration},
    };

    fn boxed_stream(chunks: usize) -> BoxedProviderStream {
        stream::iter((0..chunks).map(|_| Ok::<Bytes, StreamError>(Bytes::from_static(b"abc")))).boxed()
    }

    #[tokio::test]
    async fn metering_stream_counts_bytes_without_snapshot_logic() {
        let event_manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(11, Arc::downgrade(&event_manager)));
        let mut stream = MeteringStream::new(boxed_stream(4), Arc::clone(&meter), event_manager);

        while stream.next().await.is_some() {}

        let reading = meter.snapshot();
        assert_eq!(
            reading,
            MeterReading {
                meter_uid: 11,
                bytes_total: 12,
                bytes_window: 12,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unattached_meter_handle_unregisters_on_drop() {
        let event_manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(17, Arc::downgrade(&event_manager)));
        event_manager.register_meter(Arc::clone(&meter)).await;
        event_manager.register_meter_client(71, 17).await;
        event_manager.stream_meter_subscriber_connected();
        let mut meter_events = event_manager.get_meter_channel();
        meter.record_bytes(1024);
        drop(meter);
        yield_now().await;
        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 17);
        assert_eq!(entries[0].uids, vec![71]);
        assert_eq!(entries[0].total_kb, 1);
        advance(Duration::from_secs(3)).await;
        assert!(meter_events.try_recv().is_err(), "meter must be unregistered after final flush");
    }
}
