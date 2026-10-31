//! Lifecycle events to `on-event` (plan §7 *Event queue*, §14): the bounded
//! queue between the app's bus subscriber (`plugin_event_bridge.rs`) and the
//! consumer task that fans each event out to the plugins subscribed to it.
//!
//! Emitters never wait on a plugin. The bridge offers each bus event through
//! [`PluginSystem::offer_event`], which only ever `try_send`s: a full queue
//! drops the event and counts it. The consumer runs each subscribed plugin in
//! a fresh instance per event, under the normal epoch and memory limits. A
//! failure is logged and counted against that plugin's circuit breaker, and it
//! never reaches the emitter or another plugin.
//!
//! The bus is one-in-one-out except for coalescing: `recording.changed` and
//! `recording.rules.changed` are coalesced within a 250 ms window
//! (`EventKind::is_coalescable`, `COALESCE_WINDOW`), so a plugin subscribed to
//! them receives fewer events than were emitted, by design.

use crate::{
    dispatch::HookDispatcher,
    host_fns,
    registry::{LoadedPlugin, Registry},
    runtime::{call_with_deadline, CallError, Runtime},
    system::PluginSystem,
};
use shared::model::{EventKind, EventKindMask, PluginHook};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, PoisonError,
};
use tokio::sync::mpsc;
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::event_hooks::Event;

/// Events between the bus subscriber and the consumer. A full queue drops.
const EVENT_QUEUE_CAPACITY: usize = 256;

/// One bus event, as the consumer hands it to `on-event`.
struct QueuedEvent {
    kind: EventKind,
    /// [`shared::model::EventMessage::payload`], serialised.
    payload: String,
    /// Unix seconds, taken when the bridge received the event.
    timestamp: i64,
}

/// The queue and its consumer. Built once with the [`PluginSystem`].
pub(crate) struct EventQueue {
    sender: mpsc::Sender<QueuedEvent>,
    /// Taken by the first offer, which spawns the consumer over it. Nothing is
    /// spawned while no event has been offered.
    receiver: Mutex<Option<mpsc::Receiver<QueuedEvent>>>,
    dropped: AtomicU64,
}

impl EventQueue {
    pub(crate) fn new(_config: &PluginsConfig) -> Self { Self::with_capacity(EVENT_QUEUE_CAPACITY) }

    fn with_capacity(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self { sender, receiver: Mutex::new(Some(receiver)), dropped: AtomicU64::new(0) }
    }

    /// The receiver, handed out once. `Some` only for the first caller.
    fn take_receiver(&self) -> Option<mpsc::Receiver<QueuedEvent>> {
        self.receiver.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Queue `event` without waiting. `false` when it was dropped.
    fn push(&self, event: QueuedEvent) -> bool {
        match self.sender.try_send(event) {
            Ok(()) => true,
            Err(err) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("plugin event queue did not take an event, dropped: {err}");
                false
            }
        }
    }

    /// Events dropped since load: the queue was full, or the consumer is gone.
    pub(crate) fn dropped(&self) -> u64 { self.dropped.load(Ordering::Relaxed) }
}

/// The union of `masks`: the one mask the event bridge subscribes with.
fn union_masks(masks: impl IntoIterator<Item = EventKindMask>) -> EventKindMask {
    masks.into_iter().fold(EventKindMask::NONE, EventKindMask::union)
}

impl PluginSystem {
    /// The kinds the loaded plugins that export `on-event` subscribe to: the
    /// union of their manifest `events:`. Empty when none does, and the bridge
    /// then holds no bus subscription at all.
    pub fn event_mask(&self) -> EventKindMask {
        union_masks(
            self.dispatcher()
                .runnable(PluginHook::OnEvent)
                .filter_map(|(plugin, _)| plugin.manifest.as_ref().map(|manifest| manifest.events)),
        )
    }

    /// Queue one bus event for the `on-event` plugins. Never blocks: a full
    /// queue drops the event and counts it ([`PluginSystem::dropped_events`]).
    /// The first call starts the consumer task, so it must run inside tokio.
    pub fn offer_event(&self, kind: EventKind, payload: String, timestamp: i64) {
        let events = self.events();
        if let Some(receiver) = events.take_receiver() {
            tokio::spawn(consume(receiver, Arc::clone(self.registry()), Arc::clone(self.runtime())));
        }
        events.push(QueuedEvent { kind, payload, timestamp });
    }

    /// Events [`PluginSystem::offer_event`] dropped because the queue was full.
    pub fn dropped_events(&self) -> u64 { self.events().dropped() }
}

/// The consumer: one registry snapshot per event, so a reload mid-fan-out
/// never mixes two plugin sets for one event. It ends when the queue's sender
/// goes, with the [`PluginSystem`] that owns it.
async fn consume(mut receiver: mpsc::Receiver<QueuedEvent>, registry: Arc<Registry>, runtime: Arc<Runtime>) {
    while let Some(event) = receiver.recv().await {
        let dispatcher = HookDispatcher::new(&runtime, registry.snapshot());
        deliver(&dispatcher, &event).await;
    }
}

/// Fan one event out to every `on-event` plugin whose manifest subscribes to
/// its kind, in run order.
async fn deliver(dispatcher: &HookDispatcher<'_>, event: &QueuedEvent) {
    for (plugin, loaded) in dispatcher.runnable(PluginHook::OnEvent) {
        if !plugin.manifest.as_ref().is_some_and(|manifest| manifest.events.contains(event.kind)) {
            continue;
        }
        let outcome = call_one(dispatcher, loaded, event).await;
        plugin.report(outcome.as_ref().copied());
        match outcome {
            Ok(()) => {}
            Err(CallError::Budget { .. }) => {
                log::debug!("[plugin:{}] on-event skipped, no budget: {}", plugin.id, event.kind.as_wire_name());
            }
            Err(err) => {
                log::warn!("[plugin:{}] on-event `{}` failed: {err}", plugin.id, event.kind.as_wire_name());
            }
        }
    }
}

/// One plugin, one event, in a fresh instance.
async fn call_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    event: &QueuedEvent,
) -> Result<(), CallError> {
    let indices =
        loaded.event_hooks.as_ref().ok_or_else(|| CallError::Instantiate("does not export event-hooks".to_string()))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let wit = Event {
        kind: event.kind.as_wire_name().to_string(),
        payload: event.payload.clone(),
        timestamp: event.timestamp,
    };
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_on_event(live.armed(), &wit)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(kinds: &[EventKind]) -> EventKindMask {
        kinds.iter().fold(EventKindMask::new(), |mask, kind| mask.with(*kind))
    }

    fn event(kind: EventKind) -> QueuedEvent { QueuedEvent { kind, payload: "{}".to_string(), timestamp: 0 } }

    #[test]
    fn mask_is_the_union_of_every_subscribing_plugin() {
        let first = mask(&[EventKind::PlaylistUpdate, EventKind::StreamProbeFailed]);
        let second = mask(&[EventKind::UserCreated, EventKind::PlaylistUpdate]);
        let union = union_masks([first, second]);
        assert!(union.contains(EventKind::PlaylistUpdate));
        assert!(union.contains(EventKind::StreamProbeFailed), "a kind only the first plugin asked for");
        assert!(union.contains(EventKind::UserCreated), "a kind only the second plugin asked for");
        assert!(!union.contains(EventKind::ActiveUser), "a kind nobody asked for");
    }

    #[test]
    fn no_subscribing_plugin_means_an_empty_mask() {
        assert!(union_masks(std::iter::empty()).is_empty());
        assert!(union_masks([EventKindMask::NONE]).is_empty());
    }

    #[test]
    fn a_full_queue_drops_and_counts_without_blocking() {
        let queue = EventQueue::with_capacity(2);
        let accepted = (0..5).filter(|_| queue.push(event(EventKind::PlaylistUpdate))).count();
        assert_eq!(accepted, 2);
        assert_eq!(queue.dropped(), 3);
    }

    #[test]
    fn the_receiver_is_handed_out_once() {
        let queue = EventQueue::with_capacity(1);
        assert!(queue.take_receiver().is_some());
        assert!(queue.take_receiver().is_none());
    }
}
