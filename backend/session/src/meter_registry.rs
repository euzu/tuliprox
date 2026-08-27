//! Per-stream throughput metering: the registry, its sampler, and the batch
//! channel the Web UI reads.
//!
//! This used to live inside `EventManager`, which meant one struct owned two
//! unrelated things: a pub/sub bus for fourteen event kinds, and a metering
//! subsystem with its own broadcast channel, its own subscriber counter, its
//! own background sampler task and three mutually-dependent maps. Nothing
//! outside metering ever touched the second half.

use crate::{MeterReading, StreamMeterHandle};
use log::trace;
use shared::model::StreamMeterEntry;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub const STREAM_METER_INTERVAL: Duration = Duration::from_secs(3);
const STREAM_METER_INTERVAL_SECS: u64 = STREAM_METER_INTERVAL.as_secs();

/// What is known about one meter uid.
///
/// The two halves arrive independently and in either order - a client can be
/// assigned to a meter before the stream that owns it has registered - which
/// is why the handle is optional rather than the slot being created with one.
/// Previously this was three maps (`meters`, `meter_to_clients`,
/// `client_to_meter`) whose agreement was re-established by hand in four
/// separate methods.
#[derive(Debug, Default)]
struct MeterSlot {
    handle: Option<Arc<StreamMeterHandle>>,
    clients: Vec<u32>,
}

impl MeterSlot {
    /// A slot with neither a meter nor a viewer is not tracking anything and
    /// must not be left behind, or the map grows for the process lifetime.
    fn is_vacant(&self) -> bool { self.handle.is_none() && self.clients.is_empty() }
}

#[derive(Debug, Default)]
struct MeterSlots {
    by_meter: HashMap<u32, MeterSlot>,
    client_to_meter: HashMap<u32, u32>,
}

impl MeterSlots {
    /// Detach `client_uid` from whatever meter holds it, returning that meter.
    ///
    /// The one place the client index and the slot list are kept in step, so
    /// that "remove from the vec, drop the slot if it emptied, drop the index
    /// entry" cannot be spelled four subtly different ways.
    fn detach_client(&mut self, client_uid: u32) -> Option<u32> {
        let meter_uid = self.client_to_meter.remove(&client_uid)?;
        if let Some(slot) = self.by_meter.get_mut(&meter_uid) {
            slot.clients.retain(|uid| *uid != client_uid);
            if slot.is_vacant() {
                self.by_meter.remove(&meter_uid);
            }
        }
        Some(meter_uid)
    }

    /// Drop a meter and every client index pointing at it.
    fn remove_meter(&mut self, meter_uid: u32) -> Option<MeterSlot> {
        let slot = self.by_meter.remove(&meter_uid)?;
        for client_uid in &slot.clients {
            self.client_to_meter.remove(client_uid);
        }
        Some(slot)
    }
}

/// The live meters, who is watching each of them, and the sampler that turns
/// that into a batch every [`STREAM_METER_INTERVAL`].
pub struct StreamMeterRegistry {
    channel_tx: tokio::sync::broadcast::Sender<Vec<StreamMeterEntry>>,
    slots: Arc<RwLock<MeterSlots>>,
    subscriber_count: Arc<AtomicUsize>,
    sampler_cancel: CancellationToken,
}

impl StreamMeterRegistry {
    #[must_use]
    pub fn new() -> Self {
        let (channel_tx, _rx) = tokio::sync::broadcast::channel(10);
        let slots = Arc::new(RwLock::new(MeterSlots::default()));
        let subscriber_count = Arc::new(AtomicUsize::new(0));
        let sampler_cancel = CancellationToken::new();

        Self::spawn_sampler(
            channel_tx.clone(),
            Arc::clone(&slots),
            Arc::clone(&subscriber_count),
            sampler_cancel.clone(),
        );

        Self { channel_tx, slots, subscriber_count, sampler_cancel }
    }

    fn spawn_sampler(
        channel_tx: tokio::sync::broadcast::Sender<Vec<StreamMeterEntry>>,
        slots: Arc<RwLock<MeterSlots>>,
        subscriber_count: Arc<AtomicUsize>,
        cancel_token: CancellationToken,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            // Constructed outside a runtime - a unit test, or the composition
            // root before the runtime starts. Metering is inert rather than
            // broken, but silence here reads as "metering does not work", so
            // say so once.
            log::debug!("Stream meter sampler not started: no tokio runtime on the constructing thread");
            return;
        }
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(STREAM_METER_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => break,
                    _ = interval.tick() => {}
                }

                if subscriber_count.load(Ordering::Relaxed) == 0 || channel_tx.receiver_count() == 0 {
                    continue;
                }

                let entries = sample_entries(&slots).await;
                if !entries.is_empty() && channel_tx.send(entries).is_err() {
                    trace!("Failed to send stream meter batch");
                }
            }
        });
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Vec<StreamMeterEntry>> { self.channel_tx.subscribe() }

    #[must_use]
    pub fn has_receivers(&self) -> bool { self.channel_tx.receiver_count() > 0 }

    pub fn subscriber_connected(&self) { self.subscriber_count.fetch_add(1, Ordering::Relaxed); }

    pub fn subscriber_disconnected(&self) {
        let _ = self.subscriber_count.try_update(Ordering::AcqRel, Ordering::Relaxed, |count| count.checked_sub(1));
    }

    #[must_use]
    pub fn has_subscribers(&self) -> bool { self.subscriber_count.load(Ordering::Relaxed) > 0 }

    pub fn send_batch(&self, entries: Vec<StreamMeterEntry>) {
        if !entries.is_empty() {
            let _ = self.channel_tx.send(entries);
        }
    }

    pub async fn register_meter(&self, meter: Arc<StreamMeterHandle>) {
        let meter_uid = meter.meter_uid();
        if meter_uid == 0 {
            return;
        }
        self.slots.write().await.by_meter.entry(meter_uid).or_default().handle = Some(meter);
    }

    pub async fn unregister_meter(&self, meter_uid: u32) {
        if meter_uid == 0 {
            return;
        }
        self.slots.write().await.remove_meter(meter_uid);
    }

    /// Remove a meter, emitting its final totals first.
    ///
    /// A stream that ends between two sampler ticks would otherwise have its
    /// last bytes never reported.
    pub async fn flush_and_unregister_meter(&self, meter_uid: u32) {
        if meter_uid == 0 {
            return;
        }

        let final_entry = {
            let mut slots = self.slots.write().await;
            slots
                .remove_meter(meter_uid)
                .and_then(|slot| slot.handle.map(|handle| (handle.snapshot(), slot.clients)))
                .and_then(|(reading, clients)| build_meter_entry(reading, clients))
        };

        if let Some(entry) = final_entry {
            self.send_batch(vec![entry]);
        }
    }

    pub async fn register_meter_client(&self, client_uid: u32, meter_uid: u32) {
        if client_uid == 0 || meter_uid == 0 {
            return;
        }

        let carried_entry = {
            let mut slots = self.slots.write().await;

            // Moving a client off a meter it was the only viewer of ends that
            // meter's reportable life: emit its totals before the move, or
            // they are lost. A meter with other viewers keeps reporting, and
            // its totals are not this client's to carry.
            let carried = slots.client_to_meter.get(&client_uid).copied().filter(|old| *old != meter_uid).and_then(
                |old_meter_uid| {
                    let slot = slots.by_meter.get(&old_meter_uid)?;
                    if slot.clients.len() != 1 || slot.clients[0] != client_uid {
                        return None;
                    }
                    let reading = slot.handle.as_ref()?.snapshot();
                    build_meter_entry(reading, vec![client_uid])
                },
            );

            slots.detach_client(client_uid);
            slots.client_to_meter.insert(client_uid, meter_uid);
            let slot = slots.by_meter.entry(meter_uid).or_default();
            if !slot.clients.contains(&client_uid) {
                slot.clients.push(client_uid);
            }

            carried
        };

        if let Some(entry) = carried_entry {
            self.send_batch(vec![entry]);
        }
    }

    pub async fn unregister_meter_client(&self, client_uid: u32) {
        if client_uid == 0 {
            return;
        }
        self.slots.write().await.detach_client(client_uid);
    }

    /// Read a meter's `QoS` counters without removing it.
    ///
    /// `None` for a meter shared by several clients: the totals are
    /// meter-wide, so attributing them to one session would overstate it.
    pub async fn read_qos(&self, meter_uid: u32) -> Option<MeterQos> {
        if meter_uid == 0 {
            return None;
        }
        let slots = self.slots.read().await;
        let slot = slots.by_meter.get(&meter_uid)?;
        if slot.clients.len() > 1 {
            return None;
        }
        let handle = slot.handle.as_ref()?;
        Some(MeterQos { bytes_total: handle.bytes_total(), first_byte_latency_ms: handle.first_byte_latency_ms() })
    }

    /// Stop the sampler. Idempotent.
    pub fn shutdown(&self) { self.sampler_cancel.cancel(); }
}

/// What one meter can say about the session it served.
///
/// Was `(Option<u64>, Option<u64>)`, where "both `None`" doubled as "this
/// meter is shared, ask no further" - a convention that lived only in a doc
/// comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterQos {
    pub bytes_total: u64,
    /// `None` when the stream never delivered a first byte.
    pub first_byte_latency_ms: Option<u64>,
}

impl Default for StreamMeterRegistry {
    fn default() -> Self { Self::new() }
}

impl Drop for StreamMeterRegistry {
    fn drop(&mut self) { self.sampler_cancel.cancel(); }
}

async fn sample_entries(slots: &RwLock<MeterSlots>) -> Vec<StreamMeterEntry> {
    let samples = {
        let slots = slots.read().await;
        if slots.by_meter.is_empty() {
            return Vec::new();
        }
        slots
            .by_meter
            .values()
            .filter(|slot| !slot.clients.is_empty())
            .filter_map(|slot| slot.handle.as_ref().map(|handle| (handle.snapshot(), slot.clients.clone())))
            .collect::<Vec<_>>()
    };

    samples.into_iter().filter_map(|(reading, clients)| build_meter_entry(reading, clients)).collect()
}

fn build_meter_entry(reading: MeterReading, uids: Vec<u32>) -> Option<StreamMeterEntry> {
    if uids.is_empty() {
        return None;
    }

    let rate_kbps_u64 = reading.bytes_window / 1024 / STREAM_METER_INTERVAL_SECS;
    let rate_kbps = u32::try_from(rate_kbps_u64).unwrap_or(u32::MAX);
    let total_kb = u32::try_from(reading.bytes_total / 1024).unwrap_or(u32::MAX);

    Some(StreamMeterEntry { meter_uid: reading.meter_uid, uids, rate_kbps, total_kb })
}
