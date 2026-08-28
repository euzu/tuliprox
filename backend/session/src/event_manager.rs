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
use shared::model::{EventKind, EventKindMask, EventMessage, EventSink, StreamMeterEntry};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;

/// Buffer depth used when no configuration is available - tests, and the
/// composition root before the config is resolved.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;

/// How many recent events the bus keeps for inspection.
///
/// Small and fixed: this is a debugging aid answering "did that event
/// actually fire", not an audit log.
pub const RECENT_EVENT_CAPACITY: usize = 256;

/// How long a coalescable nudge suppresses an identical one.
///
/// Short enough that a user action always produces a visible refresh, long
/// enough to absorb the back-to-back pairs the recording routes emit.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// What became of one `emit`.
///
/// `send_event` used to return a `bool`, which conflated "nothing was
/// listening" with "this was deliberately suppressed" and told a caller
/// nothing either way - so ~80 call sites discarded it. Modelled on the
/// messaging crate's `Delivery`, which drew the same distinction for
/// notification channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitOutcome {
    /// Buffered for every current subscriber.
    Delivered { receivers: usize },
    /// Published with nothing subscribed. Normal - the Web UI is usually
    /// closed - and not an error.
    NoSubscribers,
    /// An identical coalescable nudge is already in flight; see
    /// [`EventKind::is_coalescable`].
    Coalesced,
}

impl EmitOutcome {
    /// Did this reach at least one subscriber?
    #[must_use]
    pub const fn is_delivered(self) -> bool {
        matches!(self, Self::Delivered { .. })
    }
}

/// What the bus has carried, and what it has dropped.
///
/// `send_event` returned a `bool` that ~80 call sites discarded with
/// `let _ =`, so an event nobody received and an event nobody was listening
/// for were equally invisible. Counting here rather than at the call sites
/// means one place to read, and the plugin system's promised drop counters
/// have somewhere to come from.
#[derive(Debug, Default)]
pub struct EventBusStats {
    emitted: [AtomicU64; EventKind::ALL.len()],
    no_subscribers: AtomicU64,
    lagged: AtomicU64,
    coalesced: AtomicU64,
}

impl EventBusStats {
    fn record_emit(&self, kind: EventKind, delivered: bool) {
        self.emitted[kind as usize].fetch_add(1, Ordering::Relaxed);
        if !delivered {
            self.no_subscribers.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_coalesced(&self, _kind: EventKind) {
        self.coalesced.fetch_add(1, Ordering::Relaxed);
    }

    /// Nudges suppressed because an identical one had just gone out.
    #[must_use]
    pub fn coalesced(&self) -> u64 {
        self.coalesced.load(Ordering::Relaxed)
    }

    /// A subscriber reporting the gap it was told about.
    ///
    /// Subscribers call this from their `Lagged` arm; the send side cannot
    /// see a lag, because a broadcast channel drops for the slow receiver
    /// and not for the sender.
    pub fn record_lag(&self, skipped: u64) {
        self.lagged.fetch_add(skipped, Ordering::Relaxed);
    }

    /// Events published, per kind, since start.
    #[must_use]
    pub fn emitted(&self) -> Vec<(EventKind, u64)> {
        EventKind::ALL.into_iter().map(|kind| (kind, self.emitted[kind as usize].load(Ordering::Relaxed))).collect()
    }

    /// Events published while nothing was subscribed. Not a fault - the Web
    /// UI is usually closed - but a non-zero count beside "I never saw that
    /// notification" is the answer.
    #[must_use]
    pub fn no_subscribers(&self) -> u64 {
        self.no_subscribers.load(Ordering::Relaxed)
    }

    /// Events a subscriber was told it had missed. Non-zero means the buffer
    /// is too shallow for the emit rate, or a subscriber is too slow.
    #[must_use]
    pub fn lagged(&self) -> u64 {
        self.lagged.load(Ordering::Relaxed)
    }

    /// Total across all kinds.
    #[must_use]
    pub fn total_emitted(&self) -> u64 {
        self.emitted.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
}

/// Take a bus lock, ignoring poisoning.
///
/// Every critical section here is a map or deque operation that cannot
/// panic, so a poisoned lock can only mean a panic elsewhere in the process.
/// Refusing to publish events after that would turn one unrelated panic into
/// a silently dead event bus.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct EventManager {
    channel_tx: tokio::sync::broadcast::Sender<EventMessage>,
    meters: StreamMeterRegistry,
    stats: Arc<EventBusStats>,
    /// Last time each coalescable kind was published. A blocking mutex
    /// rather than an async one: `emit` is synchronous by contract, and the
    /// critical section is a map lookup with no await inside it.
    last_nudge: Mutex<HashMap<EventKind, Instant>>,
    /// The newest message of each latched kind, for subscribers that connect
    /// after it was published.
    latched: Mutex<HashMap<EventKind, EventMessage>>,
    /// The last [`RECENT_EVENT_CAPACITY`] events, newest last.
    recent: Mutex<VecDeque<RecordedEvent>>,
    started: Instant,
}

/// One entry in the bus's recent-event ring.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub kind: EventKind,
    /// Milliseconds since the process started. Monotonic, so it stays
    /// ordered across a wall-clock adjustment.
    pub uptime_millis: u64,
    pub outcome: EmitOutcome,
}

impl EventManager {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_CHANNEL_CAPACITY)
    }

    /// A bus buffering `capacity` events per subscriber.
    ///
    /// Capacity 10 - the old fixed value - was routinely outrun: a playlist
    /// refresh emits progress ticks in a loop, and any subscriber that
    /// awaits I/O per event falls behind within one target. Both the
    /// notification bridge's `Lagged` arm and the websocket's resync path
    /// exist because of it.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (channel_tx, _channel_rx) = tokio::sync::broadcast::channel(capacity.max(1));
        Self {
            channel_tx,
            meters: StreamMeterRegistry::new(),
            stats: Arc::new(EventBusStats::default()),
            last_nudge: Mutex::new(HashMap::new()),
            latched: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_EVENT_CAPACITY)),
            started: Instant::now(),
        }
    }

    /// Counters for what this bus has carried.
    #[must_use]
    pub fn stats(&self) -> &Arc<EventBusStats> {
        &self.stats
    }

    /// The metering subsystem.
    #[must_use]
    pub fn meters(&self) -> &StreamMeterRegistry {
        &self.meters
    }

    pub fn get_event_channel(&self) -> tokio::sync::broadcast::Receiver<EventMessage> {
        self.channel_tx.subscribe()
    }

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

    /// Publish.
    ///
    /// Never blocks and never fails from the emitter's point of view: every
    /// outcome, including "suppressed" and "nobody listening", is a normal
    /// result the caller may ignore.
    pub fn send_event(&self, event: EventMessage) -> EmitOutcome {
        let kind = event.kind();

        if kind.is_coalescable() && !self.admit_nudge(kind) {
            self.stats.record_coalesced(kind);
            // Recorded, not skipped: "it fired but was suppressed" is
            // precisely what the recent-event ring is consulted to find out.
            self.record_recent(kind, EmitOutcome::Coalesced);
            return EmitOutcome::Coalesced;
        }

        if kind.is_latched() {
            lock(&self.latched).insert(kind, event.clone());
        }

        let outcome = match self.channel_tx.send(event) {
            Ok(receivers) => EmitOutcome::Delivered { receivers },
            Err(err) => {
                trace!("Failed to send event: {err}");
                EmitOutcome::NoSubscribers
            }
        };
        self.stats.record_emit(kind, outcome.is_delivered());
        self.record_recent(kind, outcome);
        outcome
    }

    fn record_recent(&self, kind: EventKind, outcome: EmitOutcome) {
        let uptime_millis = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut recent = lock(&self.recent);
        if recent.len() == RECENT_EVENT_CAPACITY {
            recent.pop_front();
        }
        recent.push_back(RecordedEvent { kind, uptime_millis, outcome });
    }

    /// The newest message of each latched kind.
    ///
    /// A session that connects between two `SystemInfo` samples used to show
    /// empty panels until the next one arrived, and the websocket's
    /// resync-on-lag path re-requested a status snapshot for the same
    /// reason. Both are answered by handing over what the bus already has.
    #[must_use]
    pub fn snapshot(&self) -> Vec<EventMessage> {
        lock(&self.latched).values().cloned().collect()
    }

    /// The last [`RECENT_EVENT_CAPACITY`] events, oldest first.
    ///
    /// "Why did my notification not fire?" is otherwise unanswerable without
    /// a debug build: this shows whether the event was published at all, and
    /// whether anything was subscribed when it was.
    #[must_use]
    pub fn recent_events(&self) -> Vec<RecordedEvent> {
        lock(&self.recent).iter().cloned().collect()
    }

    /// Should this nudge go out, or has an identical one just gone?
    ///
    /// Records the send time on admission, so a steady stream of nudges is
    /// throttled to one per window rather than one per burst.
    fn admit_nudge(&self, kind: EventKind) -> bool {
        let now = Instant::now();
        let mut last = lock(&self.last_nudge);
        match last.get(&kind) {
            Some(previous) if now.duration_since(*previous) < COALESCE_WINDOW => false,
            _ => {
                last.insert(kind, now);
                true
            }
        }
    }

    #[must_use]
    pub fn has_event_receivers(&self) -> bool {
        self.channel_tx.receiver_count() > 0
    }

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
    pub fn has_meter_event_receivers(&self) -> bool {
        self.meters.has_receivers()
    }

    pub fn stream_meter_subscriber_connected(&self) {
        self.meters.subscriber_connected();
    }

    pub fn stream_meter_subscriber_disconnected(&self) {
        self.meters.subscriber_disconnected();
    }

    #[must_use]
    pub fn has_stream_meter_subscribers(&self) -> bool {
        self.meters.has_subscribers()
    }

    pub async fn register_meter(&self, meter: Arc<StreamMeterHandle>) {
        self.meters.register_meter(meter).await;
    }

    pub async fn unregister_meter(&self, meter_uid: u32) {
        self.meters.unregister_meter(meter_uid).await;
    }

    pub async fn flush_and_unregister_meter(&self, meter_uid: u32) {
        self.meters.flush_and_unregister_meter(meter_uid).await;
    }

    pub async fn register_meter_client(&self, client_uid: u32, meter_uid: u32) {
        self.meters.register_meter_client(client_uid, meter_uid).await;
    }

    pub async fn unregister_meter_client(&self, client_uid: u32) {
        self.meters.unregister_meter_client(client_uid).await;
    }

    pub async fn read_meter_qos(&self, meter_uid: u32) -> Option<MeterQos> {
        self.meters.read_qos(meter_uid).await
    }

    pub fn send_meter_batch(&self, entries: Vec<StreamMeterEntry>) {
        self.meters.send_batch(entries);
    }
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
    pub fn mask(&self) -> EventKindMask {
        self.mask
    }
}

impl EventManager {
    /// Stop background work, flushing what is pending.
    ///
    /// The event channel needs nothing: closing the sender is what tells
    /// subscribers to stop, and `Drop` does that. The meter registry does -
    /// see [`StreamMeterRegistry::shutdown`].
    pub async fn shutdown(&self) {
        self.meters.shutdown().await;
    }
}

impl EventSink for EventManager {
    fn emit(&self, event: EventMessage) {
        self.send_event(event);
    }
}

impl Default for EventManager {
    fn default() -> Self {
        Self::new()
    }
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

#[cfg(test)]
mod bus_stats_tests {
    use super::{EventManager, DEFAULT_EVENT_CHANNEL_CAPACITY};
    use shared::model::{ConfigType, EventKind, EventMessage};

    #[test]
    fn emitting_with_no_subscribers_is_counted_not_silent() {
        let manager = EventManager::new();
        manager.send_event(EventMessage::RecordingChanged);
        manager.send_event(EventMessage::ConfigChange(ConfigType::Config));

        let stats = manager.stats();
        assert_eq!(stats.total_emitted(), 2);
        assert_eq!(stats.no_subscribers(), 2, "nothing was listening, and that is now visible");
    }

    #[tokio::test]
    async fn a_delivered_event_does_not_count_as_dropped() {
        let manager = EventManager::new();
        let _rx = manager.get_event_channel();
        manager.send_event(EventMessage::RecordingChanged);

        assert_eq!(manager.stats().no_subscribers(), 0);
        let per_kind = manager.stats().emitted();
        let recording = per_kind.iter().find(|(kind, _)| *kind == EventKind::RecordingChanged).map(|(_, n)| *n);
        assert_eq!(recording, Some(1));
    }

    #[tokio::test]
    async fn capacity_is_deep_enough_that_a_refresh_burst_does_not_lag_a_subscriber() {
        // The old fixed capacity was 10; a playlist refresh emits progress
        // ticks in a loop and outran it within one target.
        let manager = EventManager::new();
        let mut rx = manager.get_event_channel();
        for _ in 0..DEFAULT_EVENT_CHANNEL_CAPACITY {
            manager.send_event(EventMessage::RecordingChanged);
        }
        assert!(rx.try_recv().is_ok(), "a full buffer must not have already dropped the oldest event");
    }

    #[test]
    fn zero_capacity_is_clamped_rather_than_panicking() {
        let manager = EventManager::with_capacity(0);
        manager.send_event(EventMessage::RecordingChanged);
        assert_eq!(manager.stats().total_emitted(), 1);
    }
}

#[cfg(test)]
mod coalescing_tests {
    use super::{EmitOutcome, EventManager, COALESCE_WINDOW};
    use shared::model::{EventKind, EventMessage, PlaylistUpdateProgressEvent};

    #[tokio::test(start_paused = true)]
    async fn a_back_to_back_recording_nudge_is_suppressed() {
        // Deleting a recording emits `RecordingChanged` and then
        // `RecordingRulesChanged`; a bulk delete emits one pair per item.
        // Every one of them makes the Web UI re-fetch the same snapshot.
        let manager = EventManager::new();
        let _rx = manager.get_event_channel();

        assert!(manager.send_event(EventMessage::RecordingChanged).is_delivered());
        assert_eq!(manager.send_event(EventMessage::RecordingChanged), EmitOutcome::Coalesced);
        assert_eq!(manager.send_event(EventMessage::RecordingChanged), EmitOutcome::Coalesced);

        assert_eq!(manager.stats().coalesced(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_two_recording_nudges_do_not_suppress_each_other() {
        let manager = EventManager::new();
        let _rx = manager.get_event_channel();

        assert!(manager.send_event(EventMessage::RecordingChanged).is_delivered());
        assert!(
            manager.send_event(EventMessage::RecordingRulesChanged).is_delivered(),
            "different kinds are different news"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_nudge_after_the_window_gets_through() {
        let manager = EventManager::new();
        let _rx = manager.get_event_channel();

        assert!(manager.send_event(EventMessage::RecordingChanged).is_delivered());
        tokio::time::advance(COALESCE_WINDOW + std::time::Duration::from_millis(1)).await;
        assert!(
            manager.send_event(EventMessage::RecordingChanged).is_delivered(),
            "a later user action must always produce a visible refresh"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn events_carrying_a_payload_are_never_coalesced() {
        let manager = EventManager::new();
        let _rx = manager.get_event_channel();

        for _ in 0..5 {
            let outcome = manager.send_event(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: "t".to_string(),
                message: "m".to_string(),
            }));
            assert!(outcome.is_delivered(), "dropping a progress tick would lose the message it carried");
        }
        assert_eq!(manager.stats().coalesced(), 0);
    }

    #[test]
    fn only_payload_free_nudges_are_coalescable() {
        for kind in EventKind::ALL {
            let expected = matches!(kind, EventKind::RecordingChanged | EventKind::RecordingRulesChanged);
            assert_eq!(kind.is_coalescable(), expected, "{kind:?}");
        }
    }
}

#[cfg(test)]
mod snapshot_and_ring_tests {
    use super::{EmitOutcome, EventManager, RECENT_EVENT_CAPACITY};
    use shared::model::{ConfigType, EventKind, EventMessage, SystemInfo};
    use std::sync::Arc;

    fn system_info() -> Arc<SystemInfo> {
        Arc::new(SystemInfo {
            cpu_usage: 1.0,
            memory_usage: 2,
            memory_total: 3,
            net_rx_bytes_per_sec: 0.0,
            net_tx_bytes_per_sec: 0.0,
            net_rx_bytes_total: 0,
            net_tx_bytes_total: 0,
            disk_total_bytes: 0,
            disk_free_bytes: 0,
        })
    }

    #[test]
    fn a_late_subscriber_can_be_handed_the_latest_state() {
        // The Web UI used to show empty panels until the next three-second
        // sample arrived.
        let manager = EventManager::new();
        manager.send_event(EventMessage::SystemInfoUpdate(system_info()));
        manager.send_event(EventMessage::ActiveProvider("p".into(), 4));

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().any(|event| event.kind() == EventKind::SystemInfoUpdate));
        assert!(snapshot.iter().any(|event| matches!(event, EventMessage::ActiveProvider(_, 4))));
    }

    #[test]
    fn only_the_newest_message_of_a_latched_kind_is_kept() {
        let manager = EventManager::new();
        manager.send_event(EventMessage::ActiveProvider("p".into(), 1));
        manager.send_event(EventMessage::ActiveProvider("p".into(), 9));

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.len(), 1, "a latched kind is state, not a log");
        assert!(matches!(snapshot[0], EventMessage::ActiveProvider(_, 9)));
    }

    #[test]
    fn occurrences_are_never_latched() {
        // Replaying "a playlist update finished" to a session that was not
        // there when it happened would be a lie.
        let manager = EventManager::new();
        manager.send_event(EventMessage::ConfigChange(ConfigType::Config));
        manager.send_event(EventMessage::RecordingChanged);

        assert!(manager.snapshot().is_empty());
    }

    #[test]
    fn the_recent_ring_records_outcomes_and_stays_bounded() {
        let manager = EventManager::new();
        for _ in 0..(RECENT_EVENT_CAPACITY + 10) {
            manager.send_event(EventMessage::ConfigChange(ConfigType::Config));
        }

        let recent = manager.recent_events();
        assert_eq!(recent.len(), RECENT_EVENT_CAPACITY, "the ring must not grow for the process lifetime");
        assert!(recent.iter().all(|entry| entry.kind == EventKind::ConfigChange));
        assert!(
            recent.iter().all(|entry| entry.outcome == EmitOutcome::NoSubscribers),
            "nothing was subscribed, and the ring says so"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_ring_shows_a_suppressed_nudge_as_coalesced() {
        let manager = EventManager::new();
        manager.send_event(EventMessage::RecordingChanged);
        manager.send_event(EventMessage::RecordingChanged);

        let recent = manager.recent_events();
        assert_eq!(recent.len(), 2, "a suppressed event is still worth showing - that is the question being asked");
        assert_eq!(recent[1].outcome, EmitOutcome::Coalesced);
    }

    #[test]
    fn only_state_describing_kinds_are_latched() {
        for kind in EventKind::ALL {
            let expected =
                matches!(kind, EventKind::SystemInfoUpdate | EventKind::DownloadsUpdate | EventKind::ActiveProvider);
            assert_eq!(kind.is_latched(), expected, "{kind:?}");
        }
    }
}
