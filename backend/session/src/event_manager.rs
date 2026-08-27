//! The in-process event bus.
//!
//! A `broadcast::Sender<EventMessage>` and the subscription helpers around
//! it. Stream metering used to live in this struct too - a second broadcast
//! channel, a registry, a subscriber counter and a background sampler, none
//! of which any event subscriber ever touched. That is
//! [`StreamMeterRegistry`] now; `EventManager` owns one so the composition
//! root still constructs a single handle.

use crate::{
    meter_registry::{MeterQos, StreamMeterRegistry},
    StreamMeterHandle,
};
use log::trace;
use shared::model::{EventKindMask, EventMessage, EventSink, StreamMeterEntry, SystemInfo};
use std::sync::Arc;

pub struct EventManager {
    channel_tx: tokio::sync::broadcast::Sender<EventMessage>,
    meters: StreamMeterRegistry,
}

impl EventManager {
    #[must_use]
    pub fn new() -> Self {
        let (channel_tx, _channel_rx) = tokio::sync::broadcast::channel(10);
        Self { channel_tx, meters: StreamMeterRegistry::new() }
    }

    /// The metering subsystem.
    #[must_use]
    pub fn meters(&self) -> &StreamMeterRegistry { &self.meters }

    pub fn get_event_channel(&self) -> tokio::sync::broadcast::Receiver<EventMessage> { self.channel_tx.subscribe() }

    /// Subscribe to `mask` only.
    ///
    /// The filtering still happens receiver-side - a broadcast channel has
    /// one buffer for everyone, so there is nowhere else to put it - but it
    /// happens before the subscriber's own work, and a subscriber that wants
    /// two kinds no longer wakes for the other twelve. `Lagged` is still
    /// reported: a gap the subscriber did not want to know about is still a
    /// gap in the ones it did.
    pub fn subscribe_filtered(&self, mask: EventKindMask) -> FilteredEventReceiver {
        FilteredEventReceiver { inner: self.channel_tx.subscribe(), mask }
    }

    pub fn send_event(&self, event: EventMessage) -> bool {
        if let Err(err) = self.channel_tx.send(event) {
            trace!("Failed to send event: {err}");
            false
        } else {
            true
        }
    }

    pub fn send_provider_event(&self, provider: &Arc<str>, connection_count: usize) {
        if !self.send_event(EventMessage::ActiveProvider(Arc::clone(provider), connection_count)) {
            trace!("Failed to send connection change: {provider}: {connection_count}");
        }
    }

    pub fn send_system_info(&self, system_info: Arc<SystemInfo>) {
        if !self.send_event(EventMessage::SystemInfoUpdate(system_info)) {
            trace!("Failed to send system info");
        }
    }

    #[must_use]
    pub fn has_event_receivers(&self) -> bool { self.channel_tx.receiver_count() > 0 }

    // --- metering, forwarded ------------------------------------------------
    //
    // Kept as inherent methods because the call sites reach the bus through
    // `AppState.event_manager` and a stream has no reason to know that
    // metering is a separate component. `meters()` is there for anything that
    // wants the registry itself.

    #[must_use]
    pub fn get_meter_channel(&self) -> tokio::sync::broadcast::Receiver<Vec<StreamMeterEntry>> {
        self.meters.subscribe()
    }

    #[must_use]
    pub fn has_meter_event_receivers(&self) -> bool { self.meters.has_receivers() }

    pub fn stream_meter_subscriber_connected(&self) { self.meters.subscriber_connected(); }

    pub fn stream_meter_subscriber_disconnected(&self) { self.meters.subscriber_disconnected(); }

    #[must_use]
    pub fn has_stream_meter_subscribers(&self) -> bool { self.meters.has_subscribers() }

    pub async fn register_meter(&self, meter: Arc<StreamMeterHandle>) { self.meters.register_meter(meter).await; }

    pub async fn unregister_meter(&self, meter_uid: u32) { self.meters.unregister_meter(meter_uid).await; }

    pub async fn flush_and_unregister_meter(&self, meter_uid: u32) {
        self.meters.flush_and_unregister_meter(meter_uid).await;
    }

    pub async fn register_meter_client(&self, client_uid: u32, meter_uid: u32) {
        self.meters.register_meter_client(client_uid, meter_uid).await;
    }

    pub async fn unregister_meter_client(&self, client_uid: u32) {
        self.meters.unregister_meter_client(client_uid).await;
    }

    pub async fn read_meter_qos(&self, meter_uid: u32) -> Option<MeterQos> { self.meters.read_qos(meter_uid).await }

    pub fn send_meter_batch(&self, entries: Vec<StreamMeterEntry>) { self.meters.send_batch(entries); }
}

/// An [`EventMessage`] subscription narrowed to some [`EventKindMask`].
///
/// A concrete type rather than a `Stream` adaptor: the call site is a
/// `tokio::select!` arm awaiting `recv()`, which is what this offers, and a
/// boxed stream would put a virtual call on every event.
pub struct FilteredEventReceiver {
    inner: tokio::sync::broadcast::Receiver<EventMessage>,
    mask: EventKindMask,
}

impl FilteredEventReceiver {
    /// The next event this subscription asked for.
    ///
    /// Errors pass through unchanged, so `Lagged` and `Closed` are handled
    /// exactly as they are on a raw receiver.
    pub async fn recv(&mut self) -> Result<EventMessage, tokio::sync::broadcast::error::RecvError> {
        loop {
            let event = self.inner.recv().await?;
            if self.mask.contains(event.kind()) {
                return Ok(event);
            }
        }
    }

    /// The mask this subscription was opened with.
    #[must_use]
    pub fn mask(&self) -> EventKindMask { self.mask }
}

impl EventSink for EventManager {
    fn emit(&self, event: EventMessage) { self.send_event(event); }
}

impl Default for EventManager {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::EventManager;
    use crate::StreamMeterHandle;
    use std::sync::Arc;
    use tokio::time::{advance, Duration};

    #[tokio::test(start_paused = true)]
    async fn stream_meter_batch_expands_to_client_uids() {
        let manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(7, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(41, 7).await;
        manager.register_meter_client(42, 7).await;
        let mut meter_events = manager.get_meter_channel();
        manager.stream_meter_subscriber_connected();
        meter.record_bytes(15_728_640);

        advance(Duration::from_secs(3)).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 7);
        assert_eq!(entries[0].uids, vec![41, 42]);
        assert_eq!(entries[0].rate_kbps, 5120);
        assert_eq!(entries[0].total_kb, 15_360);
    }

    #[tokio::test(start_paused = true)]
    async fn late_stream_meter_subscribe_samples_already_running_stream() {
        let manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(9, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(77, 9).await;

        meter.record_bytes(3_145_728);

        let mut meter_events = manager.get_meter_channel();
        advance(Duration::from_secs(3)).await;
        assert!(meter_events.try_recv().is_err(), "meter batches must stay idle without stream-meter subscribers");

        manager.stream_meter_subscriber_connected();
        advance(Duration::from_secs(3)).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 9);
        assert_eq!(entries[0].uids, vec![77]);
        assert_eq!(entries[0].rate_kbps, 1024);
        assert_eq!(entries[0].total_kb, 3072);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_meter_batches_do_not_pollute_main_event_channel() {
        let manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(5, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(11, 5).await;
        let mut main_events = manager.get_event_channel();
        let mut meter_events = manager.get_meter_channel();
        manager.stream_meter_subscriber_connected();

        meter.record_bytes(3_145_728);
        advance(Duration::from_secs(3)).await;

        assert!(main_events.try_recv().is_err(), "meter batches must not occupy the main event channel");
        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 5);
        assert_eq!(entries[0].uids, vec![11]);
    }

    #[tokio::test]
    async fn flush_and_unregister_meter_sends_final_totals_before_removal() {
        let manager = Arc::new(EventManager::new());
        let meter = Arc::new(StreamMeterHandle::new(12, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(91, 12).await;
        manager.stream_meter_subscriber_connected();
        let mut meter_events = manager.get_meter_channel();

        meter.record_bytes(2048);
        manager.flush_and_unregister_meter(12).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 12);
        assert_eq!(entries[0].uids, vec![91]);
        assert_eq!(entries[0].total_kb, 2);
    }

    #[tokio::test]
    async fn reassigning_meter_client_emits_old_meter_totals_for_single_client_streams() {
        let manager = Arc::new(EventManager::new());
        let old_meter = Arc::new(StreamMeterHandle::new(21, Arc::downgrade(&manager)));
        let new_meter = Arc::new(StreamMeterHandle::new(22, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&old_meter)).await;
        manager.register_meter(Arc::clone(&new_meter)).await;
        manager.register_meter_client(91, 21).await;
        manager.stream_meter_subscriber_connected();
        let mut meter_events = manager.get_meter_channel();

        old_meter.record_bytes(3072);
        manager.register_meter_client(91, 22).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meter_uid, 21);
        assert_eq!(entries[0].uids, vec![91]);
        assert_eq!(entries[0].total_kb, 3);
    }

    #[tokio::test]
    async fn reassigning_meter_client_does_not_emit_old_meter_totals_for_shared_streams() {
        let manager = Arc::new(EventManager::new());
        let old_meter = Arc::new(StreamMeterHandle::new(31, Arc::downgrade(&manager)));
        let new_meter = Arc::new(StreamMeterHandle::new(32, Arc::downgrade(&manager)));
        manager.register_meter(Arc::clone(&old_meter)).await;
        manager.register_meter(Arc::clone(&new_meter)).await;
        manager.register_meter_client(91, 31).await;
        manager.register_meter_client(92, 31).await;
        manager.stream_meter_subscriber_connected();
        let mut meter_events = manager.get_meter_channel();

        old_meter.record_bytes(4096);
        manager.register_meter_client(91, 32).await;

        assert!(meter_events.try_recv().is_err(), "shared meters must not emit carried totals during reassignment");
    }
}
