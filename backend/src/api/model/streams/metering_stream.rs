use crate::api::model::{BoxedProviderStream, EventManager, StreamError};
use bytes::Bytes;
use futures::{
    stream::Stream,
    task::{Context, Poll},
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Shared per-source meter state. The stream hot path only performs relaxed byte counting.
#[derive(Debug)]
pub struct StreamMeterHandle {
    meter_uid: u32,
    bytes_total: AtomicU64,
    bytes_window: AtomicU64,
}

impl StreamMeterHandle {
    pub fn new(meter_uid: u32) -> Self {
        Self {
            meter_uid,
            bytes_total: AtomicU64::new(0),
            bytes_window: AtomicU64::new(0),
        }
    }

    pub fn meter_uid(&self) -> u32 { self.meter_uid }

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
                event_manager.unregister_meter(meter_uid).await;
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

    fn boxed_stream(chunks: usize) -> BoxedProviderStream {
        stream::iter((0..chunks).map(|_| Ok::<Bytes, StreamError>(Bytes::from_static(b"abc")))).boxed()
    }

    #[tokio::test]
    async fn metering_stream_counts_bytes_without_snapshot_logic() {
        let meter = Arc::new(StreamMeterHandle::new(11));
        let event_manager = Arc::new(EventManager::new());
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
}
