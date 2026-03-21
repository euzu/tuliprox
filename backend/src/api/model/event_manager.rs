use crate::api::model::streams::{MeterReading, StreamMeterHandle};
use log::trace;
use shared::model::{ActiveUserConnectionChange, ConfigType, LibraryScanSummary, PlaylistUpdateState, StreamMeterEntry, SystemInfo};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::RwLock;

const STREAM_METER_INTERVAL: Duration = Duration::from_secs(3);
const STREAM_METER_INTERVAL_SECS: u64 = 3;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EventMessage {
    ServerError(String),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize),
    ConfigChange(ConfigType),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(String, String),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanSummary),
    InputMetadataUpdatesCompleted(Arc<str>),
    InputMetadataUpdatesStarted(Arc<str>),
}

pub struct EventManager {
    channel_tx: tokio::sync::broadcast::Sender<EventMessage>,
    meter_channel_tx: tokio::sync::broadcast::Sender<Vec<StreamMeterEntry>>,
    meter_registry: Arc<RwLock<MeterRegistry>>,
    stream_meter_subscriber_count: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct MeterRegistry {
    meters: HashMap<u32, Arc<StreamMeterHandle>>,
    meter_to_clients: HashMap<u32, Vec<u32>>,
    client_to_meter: HashMap<u32, u32>,
}

impl EventManager {
    pub fn new() -> Self {
        let (channel_tx, _channel_rx) = tokio::sync::broadcast::channel(10);
        let (meter_channel_tx, _meter_channel_rx) = tokio::sync::broadcast::channel(10);
        let meter_registry = Arc::new(RwLock::new(MeterRegistry::default()));
        let stream_meter_subscriber_count = Arc::new(AtomicUsize::new(0));

        Self::spawn_meter_sampler(
            meter_channel_tx.clone(),
            Arc::clone(&meter_registry),
            Arc::clone(&stream_meter_subscriber_count),
        );

        Self {
            channel_tx,
            meter_channel_tx,
            meter_registry,
            stream_meter_subscriber_count,
        }
    }

    fn spawn_meter_sampler(
        meter_channel_tx: tokio::sync::broadcast::Sender<Vec<StreamMeterEntry>>,
        meter_registry: Arc<RwLock<MeterRegistry>>,
        stream_meter_subscriber_count: Arc<AtomicUsize>,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(STREAM_METER_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if stream_meter_subscriber_count.load(Ordering::Relaxed) == 0 {
                    continue;
                }

                if meter_channel_tx.receiver_count() == 0 {
                    continue;
                }

                let entries = sample_meter_entries(&meter_registry).await;
                if !entries.is_empty() && meter_channel_tx.send(entries).is_err() {
                    trace!("Failed to send stream meter batch");
                }
            }
        });
    }

    pub fn get_event_channel(&self) -> tokio::sync::broadcast::Receiver<EventMessage> { self.channel_tx.subscribe() }

    pub fn get_meter_channel(&self) -> tokio::sync::broadcast::Receiver<Vec<StreamMeterEntry>> {
        self.meter_channel_tx.subscribe()
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

    pub fn send_system_info(&self, system_info: SystemInfo) {
        if !self.send_event(EventMessage::SystemInfoUpdate(system_info)) {
            trace!("Failed to send system info");
        }
    }

    pub fn has_event_receivers(&self) -> bool { self.channel_tx.receiver_count() > 0 }

    pub fn has_meter_event_receivers(&self) -> bool { self.meter_channel_tx.receiver_count() > 0 }

    pub fn stream_meter_subscriber_connected(&self) {
        self.stream_meter_subscriber_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stream_meter_subscriber_disconnected(&self) {
        let _ = self
            .stream_meter_subscriber_count
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| count.checked_sub(1));
    }

    pub fn has_stream_meter_subscribers(&self) -> bool {
        self.stream_meter_subscriber_count.load(Ordering::Relaxed) > 0
    }

    pub async fn register_meter(&self, meter: Arc<StreamMeterHandle>) {
        let meter_uid = meter.meter_uid();
        if meter_uid == 0 {
            return;
        }

        self.meter_registry.write().await.meters.insert(meter_uid, meter);
    }

    pub async fn unregister_meter(&self, meter_uid: u32) {
        if meter_uid == 0 {
            return;
        }

        let mut registry = self.meter_registry.write().await;
        registry.meters.remove(&meter_uid);
        if let Some(client_uids) = registry.meter_to_clients.remove(&meter_uid) {
            for client_uid in client_uids {
                registry.client_to_meter.remove(&client_uid);
            }
        }
    }

    pub async fn register_meter_client(&self, client_uid: u32, meter_uid: u32) {
        if client_uid == 0 || meter_uid == 0 {
            return;
        }

        let mut registry = self.meter_registry.write().await;
        if let Some(old_meter_uid) = registry.client_to_meter.insert(client_uid, meter_uid) {
            if let Some(client_uids) = registry.meter_to_clients.get_mut(&old_meter_uid) {
                client_uids.retain(|uid| *uid != client_uid);
                if client_uids.is_empty() {
                    registry.meter_to_clients.remove(&old_meter_uid);
                }
            }
        }

        let client_uids = registry.meter_to_clients.entry(meter_uid).or_default();
        if !client_uids.contains(&client_uid) {
            client_uids.push(client_uid);
        }
    }

    pub async fn unregister_meter_client(&self, client_uid: u32) {
        if client_uid == 0 {
            return;
        }

        let mut registry = self.meter_registry.write().await;
        if let Some(meter_uid) = registry.client_to_meter.remove(&client_uid) {
            if let Some(client_uids) = registry.meter_to_clients.get_mut(&meter_uid) {
                client_uids.retain(|uid| *uid != client_uid);
                if client_uids.is_empty() {
                    registry.meter_to_clients.remove(&meter_uid);
                }
            }
        }
    }

    pub fn send_meter_batch(&self, entries: Vec<shared::model::StreamMeterEntry>) {
        if !entries.is_empty() {
            let _ = self.meter_channel_tx.send(entries);
        }
    }
}

async fn sample_meter_entries(meter_registry: &RwLock<MeterRegistry>) -> Vec<StreamMeterEntry> {
    let (has_subscribers, samples) = {
        let registry = meter_registry.read().await;
        if registry.meters.is_empty() || registry.meter_to_clients.is_empty() {
            return Vec::new();
        }

        let samples = registry
            .meters
            .iter()
            .filter_map(|(meter_uid, meter)| {
                registry.meter_to_clients.get(meter_uid).filter(|uids| !uids.is_empty()).map(|uids| {
                    let reading = meter.snapshot();
                    (*meter_uid, reading, uids.clone())
                })
            })
            .collect::<Vec<_>>();
        (!samples.is_empty(), samples)
    };

    if !has_subscribers {
        return Vec::new();
    }

    samples
        .into_iter()
        .filter_map(|(_meter_uid, reading, uids)| build_meter_entry(reading, uids))
        .collect()
}

fn build_meter_entry(reading: MeterReading, uids: Vec<u32>) -> Option<StreamMeterEntry> {
    if uids.is_empty() {
        return None;
    }

    let rate_kbps_u64 = reading.bytes_window / 1024 / STREAM_METER_INTERVAL_SECS;
    let rate_kbps = u32::try_from(rate_kbps_u64).unwrap_or(u32::MAX);
    let total_kb = u32::try_from(reading.bytes_total / 1024).unwrap_or(u32::MAX);

    Some(StreamMeterEntry {
        uids,
        rate_kbps,
        total_kb,
    })
}

impl Default for EventManager {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::EventManager;
    use crate::api::model::StreamMeterHandle;
    use std::sync::Arc;
    use tokio::time::{advance, Duration};

    #[tokio::test(start_paused = true)]
    async fn stream_meter_batch_expands_to_client_uids() {
        let manager = EventManager::new();
        let meter = Arc::new(StreamMeterHandle::new(7));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(41, 7).await;
        manager.register_meter_client(42, 7).await;
        manager.stream_meter_subscriber_connected();

        let mut meter_events = manager.get_meter_channel();
        meter.record_bytes(15_728_640);

        advance(Duration::from_secs(3)).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uids, vec![41, 42]);
        assert_eq!(entries[0].rate_kbps, 5120);
        assert_eq!(entries[0].total_kb, 15_360);
    }

    #[tokio::test(start_paused = true)]
    async fn late_stream_meter_subscribe_samples_already_running_stream() {
        let manager = EventManager::new();
        let meter = Arc::new(StreamMeterHandle::new(9));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(77, 9).await;

        meter.record_bytes(3_145_728);

        let mut meter_events = manager.get_meter_channel();
        advance(Duration::from_secs(3)).await;
        assert!(
            meter_events.try_recv().is_err(),
            "meter batches must stay idle without stream-meter subscribers"
        );

        manager.stream_meter_subscriber_connected();
        advance(Duration::from_secs(3)).await;

        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uids, vec![77]);
        assert_eq!(entries[0].rate_kbps, 1024);
        assert_eq!(entries[0].total_kb, 3072);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_meter_batches_do_not_pollute_main_event_channel() {
        let manager = EventManager::new();
        let meter = Arc::new(StreamMeterHandle::new(5));
        manager.register_meter(Arc::clone(&meter)).await;
        manager.register_meter_client(11, 5).await;
        manager.stream_meter_subscriber_connected();

        let mut main_events = manager.get_event_channel();
        let mut meter_events = manager.get_meter_channel();

        meter.record_bytes(3_145_728);
        advance(Duration::from_secs(3)).await;

        assert!(main_events.try_recv().is_err(), "meter batches must not occupy the main event channel");
        let entries = meter_events.recv().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uids, vec![11]);
    }
}
