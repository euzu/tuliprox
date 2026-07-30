use super::terminal_tail::{HlsTerminalAssetIdentity, HlsTerminalMediaAsset};
use crate::api::model::{
    HlsFiniteTsDiscontinuityMode, HlsFiniteTsFinalizeSpec, HlsFiniteTsRenderError, HlsFiniteTsRenderSpec,
    HlsTsSpliceAnchor,
};
use bytes::Bytes;
use lru::LruCache;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::Notify;

const HLS_PREPARED_TERMINAL_BUNDLE_CACHE_CAPACITY: usize = 32;
const HLS_PREPARED_TERMINAL_BUNDLE_MAX_IN_FLIGHT: usize = 4;
const HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;

/// Exact identity of one immutable prepared terminal-media representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HlsPreparedTerminalBundleKey {
    pub asset: HlsTerminalAssetIdentity,
    pub target_duration_ms: u64,
    pub segment_count: u16,
}

#[derive(Clone)]
pub(crate) struct HlsPreparedTerminalSegment {
    pub index: u16,
    pub timestamp_offset_ticks_90khz: u64,
    pub bytes: Bytes,
}

impl std::fmt::Debug for HlsPreparedTerminalSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsPreparedTerminalSegment")
            .field("index", &self.index)
            .field("timestamp_offset_ticks_90khz", &self.timestamp_offset_ticks_90khz)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

/// Immutable finite terminal-media sequence shared by all matching leases.
#[derive(Clone)]
pub(crate) struct HlsPreparedTerminalBundle {
    pub key: HlsPreparedTerminalBundleKey,
    pub source_asset_duration_ms: u64,
    pub source_asset_duration_ticks_90khz: u64,
    pub segments: Arc<[HlsPreparedTerminalSegment]>,
}

impl std::fmt::Debug for HlsPreparedTerminalBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let byte_len = self
            .segments
            .iter()
            .fold(0_usize, |total, segment| total.saturating_add(segment.bytes.len()));
        formatter
            .debug_struct("HlsPreparedTerminalBundle")
            .field("key", &self.key)
            .field("source_asset_duration_ms", &self.source_asset_duration_ms)
            .field("source_asset_duration_ticks_90khz", &self.source_asset_duration_ticks_90khz)
            .field("segment_count", &self.segments.len())
            .field("byte_len", &byte_len)
            .finish()
    }
}

impl HlsPreparedTerminalBundle {
    pub(crate) fn matches_key_and_shape(&self, key: HlsPreparedTerminalBundleKey) -> bool {
        self.key == key
            && self.segments.len() == usize::from(key.segment_count)
            && self.segments.iter().enumerate().all(|(position, segment)| {
                usize::from(segment.index) == position
                    && !segment.bytes.is_empty()
                    && terminal_timestamp_offset_ticks_90khz(
                        segment.index,
                        self.source_asset_duration_ticks_90khz,
                    )
                        .is_ok_and(|expected| expected == segment.timestamp_offset_ticks_90khz)
            })
    }
}

#[derive(Clone)]
pub(crate) struct HlsAnchoredTerminalSegment {
    pub index: u16,
    pub total_timestamp_offset_ticks_90khz: u64,
    pub bytes: Bytes,
}

impl std::fmt::Debug for HlsAnchoredTerminalSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsAnchoredTerminalSegment")
            .field("index", &self.index)
            .field("total_timestamp_offset_ticks_90khz", &self.total_timestamp_offset_ticks_90khz)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct HlsAnchoredTerminalBundle {
    pub prepared_key: HlsPreparedTerminalBundleKey,
    pub splice_anchor: HlsTsSpliceAnchor,
    pub segments: Arc<[HlsAnchoredTerminalSegment]>,
}

impl std::fmt::Debug for HlsAnchoredTerminalBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsAnchoredTerminalBundle")
            .field("prepared_key", &self.prepared_key)
            .field("splice_anchor", &self.splice_anchor)
            .field("segment_count", &self.segments.len())
            .finish_non_exhaustive()
    }
}

impl HlsAnchoredTerminalBundle {
    pub(crate) fn matches_key_and_shape(
        &self,
        key: HlsPreparedTerminalBundleKey,
        source_asset_duration_ticks_90khz: u64,
    ) -> bool {
        self.prepared_key == key
            && self.segments.len() == usize::from(key.segment_count)
            && self.segments.iter().enumerate().all(|(position, segment)| {
                usize::from(segment.index) == position
                    && !segment.bytes.is_empty()
                    && segment.bytes.len().is_multiple_of(188)
                    && terminal_timestamp_offset_ticks_90khz(
                        segment.index,
                        source_asset_duration_ticks_90khz,
                    )
                    .map(|offset| {
                        add_timestamp_offsets_90khz(offset, self.splice_anchor.timestamp_delta_ticks)
                    })
                    .is_ok_and(|expected| expected == segment.total_timestamp_offset_ticks_90khz)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsPreparedTerminalBundleIncompatibility {
    EmptySegmentSet,
    ZeroTargetDuration,
    TargetDurationExceeded { asset_ms: u64, target_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsPreparedTerminalBundleBuildError {
    Incompatible(HlsPreparedTerminalBundleIncompatibility),
    AssetIdentityMismatch,
    TimestampOffsetOverflow {
        index: u16,
        source_asset_duration_ticks_90khz: u64,
    },
    FiniteSegmentRender(HlsFiniteTsRenderError),
    PublishedBundleKeyMismatch,
    PublishedBundleShapeMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsPreparedTerminalBundleFailure {
    Build(HlsPreparedTerminalBundleBuildError),
    WorkerJoin,
    RuntimeUnavailable,
    PreparationCapacityExceeded,
    ByteCapacityExceeded { required_bytes: u64, capacity_bytes: u64 },
    BundleSizeOverflow,
    GenerationExhausted,
}

/// Observable state for one exact prepared-bundle key.
#[derive(Debug, Clone)]
#[must_use]
pub(crate) enum HlsPreparedTerminalBundleState {
    Ready { bundle: Arc<HlsPreparedTerminalBundle> },
    Preparing { key: HlsPreparedTerminalBundleKey },
    Failed { key: HlsPreparedTerminalBundleKey, reason: HlsPreparedTerminalBundleFailure },
    Incompatible {
        key: HlsPreparedTerminalBundleKey,
        reason: HlsPreparedTerminalBundleIncompatibility,
    },
}

impl PartialEq for HlsPreparedTerminalBundleState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ready { bundle: left }, Self::Ready { bundle: right }) => left.key == right.key,
            (Self::Preparing { key: left }, Self::Preparing { key: right }) => left == right,
            (
                Self::Failed { key: left_key, reason: left_reason },
                Self::Failed { key: right_key, reason: right_reason },
            ) => left_key == right_key && left_reason == right_reason,
            (
                Self::Incompatible { key: left_key, reason: left_reason },
                Self::Incompatible { key: right_key, reason: right_reason },
            ) => left_key == right_key && left_reason == right_reason,
            (
                Self::Ready { .. } | Self::Preparing { .. } | Self::Failed { .. } | Self::Incompatible { .. },
                _,
            ) => false,
        }
    }
}

impl Eq for HlsPreparedTerminalBundleState {}

/// Terminal result published by the exact preparation flight captured in a
/// completion ticket. A later flight for the same bundle key cannot satisfy an
/// older ticket.
#[derive(Debug, Clone)]
#[must_use]
pub(crate) enum HlsPreparedTerminalBundleCompletion {
    Ready { bundle: Arc<HlsPreparedTerminalBundle> },
    Failed { key: HlsPreparedTerminalBundleKey, reason: HlsPreparedTerminalBundleFailure },
    Incompatible { key: HlsPreparedTerminalBundleKey, reason: HlsPreparedTerminalBundleIncompatibility },
    FlightReplaced { key: HlsPreparedTerminalBundleKey, generation: u64 },
}

impl HlsPreparedTerminalBundleCompletion {
    fn from_entry(key: HlsPreparedTerminalBundleKey, entry: &HlsPreparedTerminalBundleCacheEntry) -> Self {
        match entry {
            HlsPreparedTerminalBundleCacheEntry::Ready(bundle) => Self::Ready { bundle: Arc::clone(bundle) },
            HlsPreparedTerminalBundleCacheEntry::Failed(reason) => Self::Failed { key, reason: *reason },
            HlsPreparedTerminalBundleCacheEntry::Incompatible(reason) => Self::Incompatible { key, reason: *reason },
        }
    }

    #[cfg(test)]
    fn into_state(self) -> Option<HlsPreparedTerminalBundleState> {
        match self {
            Self::Ready { bundle } => Some(HlsPreparedTerminalBundleState::Ready { bundle }),
            Self::Failed { key, reason } => Some(HlsPreparedTerminalBundleState::Failed { key, reason }),
            Self::Incompatible { key, reason } => Some(HlsPreparedTerminalBundleState::Incompatible { key, reason }),
            Self::FlightReplaced { .. } => None,
        }
    }
}

type HlsPreparedTerminalBundleBuilder = dyn Fn(
        &HlsTerminalMediaAsset,
        HlsPreparedTerminalBundleKey,
    ) -> Result<Arc<HlsPreparedTerminalBundle>, HlsPreparedTerminalBundleBuildError>
    + Send
    + Sync;

#[derive(Clone)]
enum HlsPreparedTerminalBundleCacheEntry {
    Ready(Arc<HlsPreparedTerminalBundle>),
    Failed(HlsPreparedTerminalBundleFailure),
    Incompatible(HlsPreparedTerminalBundleIncompatibility),
}

impl HlsPreparedTerminalBundleCacheEntry {
    fn state(&self, key: HlsPreparedTerminalBundleKey) -> HlsPreparedTerminalBundleState {
        match self {
            Self::Ready(bundle) => HlsPreparedTerminalBundleState::Ready { bundle: Arc::clone(bundle) },
            Self::Failed(reason) => HlsPreparedTerminalBundleState::Failed { key, reason: *reason },
            Self::Incompatible(reason) => HlsPreparedTerminalBundleState::Incompatible { key, reason: *reason },
        }
    }
}

struct HlsPreparedTerminalBundleFlight {
    generation: u64,
    completion: Arc<HlsPreparedTerminalBundleFlightCompletion>,
    estimated_bytes: u64,
}

struct HlsPreparedTerminalBundleFlightCompletion {
    result: Mutex<Option<HlsPreparedTerminalBundleCompletion>>,
    completed: Notify,
}

impl HlsPreparedTerminalBundleFlightCompletion {
    fn new() -> Self { Self { result: Mutex::new(None), completed: Notify::new() } }

    fn publish(&self, completion: HlsPreparedTerminalBundleCompletion) {
        let published = {
            let mut result = self.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if result.is_some() {
                false
            } else {
                *result = Some(completion);
                true
            }
        };
        if published {
            self.completed.notify_waiters();
        }
    }

    async fn wait(&self) -> HlsPreparedTerminalBundleCompletion {
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            let _registered = notified.as_mut().enable();
            let result = self.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            if let Some(result) = result {
                return result;
            }
            notified.await;
        }
    }
}

/// Generation-bound wait handle for one already existing exact-key flight.
/// Creating or waiting on a ticket never starts terminal-bundle preparation.
#[derive(Clone)]
pub(crate) struct HlsPreparedTerminalBundleCompletionTicket {
    key: HlsPreparedTerminalBundleKey,
    generation: u64,
    completion: Arc<HlsPreparedTerminalBundleFlightCompletion>,
}

impl std::fmt::Debug for HlsPreparedTerminalBundleCompletionTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsPreparedTerminalBundleCompletionTicket")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl HlsPreparedTerminalBundleCompletionTicket {
    pub(crate) async fn wait(self) -> HlsPreparedTerminalBundleCompletion { self.completion.wait().await }
}

#[cfg(test)]
pub(crate) struct HlsPreparedTerminalBundleCompletionPublisher {
    completion: Arc<HlsPreparedTerminalBundleFlightCompletion>,
}

#[cfg(test)]
impl HlsPreparedTerminalBundleCompletionPublisher {
    pub(crate) fn publish(self, result: HlsPreparedTerminalBundleCompletion) { self.completion.publish(result) }
}

#[cfg(test)]
pub(crate) fn prepared_terminal_bundle_completion_channel_for_test(
    key: HlsPreparedTerminalBundleKey,
) -> (
    HlsPreparedTerminalBundleCompletionTicket,
    HlsPreparedTerminalBundleCompletionPublisher,
) {
    let completion = Arc::new(HlsPreparedTerminalBundleFlightCompletion::new());
    (
        HlsPreparedTerminalBundleCompletionTicket { key, generation: 1, completion: Arc::clone(&completion) },
        HlsPreparedTerminalBundleCompletionPublisher { completion },
    )
}

/// Atomic observation of one exact prepared-bundle key.
///
/// A flight ticket and the settled cache state are selected while holding the
/// same cache mutex, so callers cannot miss a same-key flight between separate
/// ticket and state reads.
#[derive(Debug, Clone)]
#[must_use]
pub(crate) enum HlsPreparedTerminalBundleObservation {
    Flight(HlsPreparedTerminalBundleCompletionTicket),
    Settled(HlsPreparedTerminalBundleState),
    Missing,
}

struct HlsPreparedTerminalBundleCacheInner {
    entries: LruCache<HlsPreparedTerminalBundleKey, HlsPreparedTerminalBundleCacheEntry>,
    flights: HashMap<HlsPreparedTerminalBundleKey, HlsPreparedTerminalBundleFlight>,
    resident_bytes: u64,
    next_generation: u64,
}

/// Bounded, process-local cache and singleflight coordinator for terminal media.
///
/// Cache entries are immutable `Arc` values. A terminal plan may therefore keep
/// serving an evicted bundle. At most four independently keyed preparations own
/// an async task and one corresponding blocking writer job at the same time.
pub(crate) struct HlsPreparedTerminalBundleCache {
    inner: Mutex<HlsPreparedTerminalBundleCacheInner>,
    max_in_flight: usize,
    max_resident_bytes: u64,
    builder: Arc<HlsPreparedTerminalBundleBuilder>,
}

impl Default for HlsPreparedTerminalBundleCache {
    fn default() -> Self { Self::new() }
}

impl HlsPreparedTerminalBundleCache {
    pub(crate) fn new() -> Self {
        Self::with_limits_and_builder(
            HLS_PREPARED_TERMINAL_BUNDLE_CACHE_CAPACITY,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_IN_FLIGHT,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            Arc::new(build_prepared_terminal_bundle),
        )
    }

    fn with_limits_and_builder(
        cache_capacity: usize,
        max_in_flight: usize,
        max_resident_bytes: u64,
        builder: Arc<HlsPreparedTerminalBundleBuilder>,
    ) -> Self {
        let cache_capacity = NonZeroUsize::new(cache_capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(HlsPreparedTerminalBundleCacheInner {
                entries: LruCache::new(cache_capacity),
                flights: HashMap::new(),
                resident_bytes: 0,
                next_generation: 0,
            }),
            max_in_flight,
            max_resident_bytes,
            builder,
        }
    }

    /// Starts at most one owned preparation task for the exact key and returns
    /// immediately. The task retains the cache until it has generation-safely
    /// published its result, so cancellation of the caller cannot orphan a
    /// `Preparing` entry.
    pub(crate) fn start_preparation(
        self: &Arc<Self>,
        asset: Arc<HlsTerminalMediaAsset>,
        target_duration_ms: u64,
        segment_count: u16,
    ) -> HlsPreparedTerminalBundleState {
        let key = prepared_terminal_bundle_key(&asset, target_duration_ms, segment_count);
        let incompatibility = prepared_terminal_bundle_incompatibility(&asset, key);
        let estimated_bytes = estimated_bundle_bytes(&asset, segment_count);
        let runtime = tokio::runtime::Handle::try_current().ok();
        {
            let mut inner = self.lock_inner();
            if let Some(entry) = inner.entries.get(&key) {
                return entry.state(key);
            }
            if inner.flights.contains_key(&key) {
                return HlsPreparedTerminalBundleState::Preparing { key };
            }
            if let Some(reason) = incompatibility {
                insert_cache_entry(
                    &mut inner,
                    key,
                    HlsPreparedTerminalBundleCacheEntry::Incompatible(reason),
                    self.max_resident_bytes,
                );
                return HlsPreparedTerminalBundleState::Incompatible { key, reason };
            }
            let Some(estimated_bytes) = estimated_bytes else {
                return HlsPreparedTerminalBundleState::Failed {
                    key,
                    reason: HlsPreparedTerminalBundleFailure::BundleSizeOverflow,
                };
            };
            let Some(runtime) = runtime else {
                return HlsPreparedTerminalBundleState::Failed {
                    key,
                    reason: HlsPreparedTerminalBundleFailure::RuntimeUnavailable,
                };
            };
            if inner.flights.len() >= self.max_in_flight {
                return HlsPreparedTerminalBundleState::Failed {
                    key,
                    reason: HlsPreparedTerminalBundleFailure::PreparationCapacityExceeded,
                };
            }
            let Some(generation) = inner.next_generation.checked_add(1) else {
                return HlsPreparedTerminalBundleState::Failed {
                    key,
                    reason: HlsPreparedTerminalBundleFailure::GenerationExhausted,
                };
            };
            if estimated_bytes > self.max_resident_bytes
                || !reserve_resident_bytes(&mut inner, estimated_bytes, self.max_resident_bytes)
            {
                return HlsPreparedTerminalBundleState::Failed {
                    key,
                    reason: HlsPreparedTerminalBundleFailure::ByteCapacityExceeded {
                        required_bytes: estimated_bytes,
                        capacity_bytes: self.max_resident_bytes,
                    },
                };
            }
            inner.next_generation = generation;
            let completion = Arc::new(HlsPreparedTerminalBundleFlightCompletion::new());
            let previous_flight = inner.flights.insert(
                key,
                HlsPreparedTerminalBundleFlight { generation, completion: Arc::clone(&completion), estimated_bytes },
            );
            if let Some(previous_flight) = previous_flight.as_ref() {
                inner.resident_bytes = inner.resident_bytes.saturating_sub(previous_flight.estimated_bytes);
            }
            inner.resident_bytes = inner.resident_bytes.saturating_add(estimated_bytes);
            drop(inner);
            self.spawn_preparation(&runtime, asset, key, generation, previous_flight);
        }
        HlsPreparedTerminalBundleState::Preparing { key }
    }

    fn spawn_preparation(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        asset: Arc<HlsTerminalMediaAsset>,
        key: HlsPreparedTerminalBundleKey,
        generation: u64,
        previous_flight: Option<HlsPreparedTerminalBundleFlight>,
    ) {
        if let Some(previous_flight) = previous_flight {
            previous_flight.completion.publish(HlsPreparedTerminalBundleCompletion::FlightReplaced {
                key,
                generation: previous_flight.generation,
            });
        }
        let cache = Arc::clone(self);
        let builder = Arc::clone(&self.builder);
        drop(runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || builder(asset.as_ref(), key)).await;
            let entry = match result {
                Ok(Ok(bundle)) if bundle.matches_key_and_shape(key) => {
                    HlsPreparedTerminalBundleCacheEntry::Ready(bundle)
                }
                Ok(Ok(bundle)) if bundle.key != key => HlsPreparedTerminalBundleCacheEntry::Failed(
                    HlsPreparedTerminalBundleFailure::Build(
                        HlsPreparedTerminalBundleBuildError::PublishedBundleKeyMismatch,
                    ),
                ),
                Ok(Ok(_)) => HlsPreparedTerminalBundleCacheEntry::Failed(HlsPreparedTerminalBundleFailure::Build(
                    HlsPreparedTerminalBundleBuildError::PublishedBundleShapeMismatch,
                )),
                Ok(Err(HlsPreparedTerminalBundleBuildError::Incompatible(reason))) => {
                    HlsPreparedTerminalBundleCacheEntry::Incompatible(reason)
                }
                Ok(Err(error)) => {
                    HlsPreparedTerminalBundleCacheEntry::Failed(HlsPreparedTerminalBundleFailure::Build(error))
                }
                Err(_) => HlsPreparedTerminalBundleCacheEntry::Failed(HlsPreparedTerminalBundleFailure::WorkerJoin),
            };
            cache.publish_if_current(key, generation, entry);
        }));
    }

    pub(crate) fn state(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        let mut inner = self.lock_inner();
        if let Some(entry) = inner.entries.get(&key) {
            return Some(entry.state(key));
        }
        inner.flights.contains_key(&key).then_some(HlsPreparedTerminalBundleState::Preparing { key })
    }

    /// Captures the current exact-key preparation flight without creating one.
    /// The returned ticket remains bound to that flight generation even if the
    /// cache entry is later evicted and another flight reuses the same key.
    #[cfg(test)]
    pub(crate) fn completion_ticket(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleCompletionTicket> {
        let inner = self.lock_inner();
        let flight = inner.flights.get(&key)?;
        Some(HlsPreparedTerminalBundleCompletionTicket {
            key,
            generation: flight.generation,
            completion: Arc::clone(&flight.completion),
        })
    }

    #[cfg(test)]
    pub(crate) fn install_controlled_flight_for_test(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleCompletionPublisher> {
        let mut inner = self.lock_inner();
        if inner.entries.peek(&key).is_some() || inner.flights.contains_key(&key) {
            return None;
        }
        let generation = inner.next_generation.checked_add(1)?;
        inner.next_generation = generation;
        let completion = Arc::new(HlsPreparedTerminalBundleFlightCompletion::new());
        inner.flights.insert(
            key,
            HlsPreparedTerminalBundleFlight {
                generation,
                completion: Arc::clone(&completion),
                estimated_bytes: 0,
            },
        );
        Some(HlsPreparedTerminalBundleCompletionPublisher { completion })
    }

    pub(crate) fn observe_exact(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> HlsPreparedTerminalBundleObservation {
        let mut inner = self.lock_inner();
        if let Some(flight) = inner.flights.get(&key) {
            return HlsPreparedTerminalBundleObservation::Flight(HlsPreparedTerminalBundleCompletionTicket {
                key,
                generation: flight.generation,
                completion: Arc::clone(&flight.completion),
            });
        }
        match inner.entries.get(&key) {
            Some(entry) => HlsPreparedTerminalBundleObservation::Settled(entry.state(key)),
            None => HlsPreparedTerminalBundleObservation::Missing,
        }
    }

    fn publish_if_current(
        &self,
        key: HlsPreparedTerminalBundleKey,
        generation: u64,
        mut entry: HlsPreparedTerminalBundleCacheEntry,
    ) {
        let (completion, result) = {
            let mut inner = self.lock_inner();
            let Some(flight) = inner.flights.get(&key) else {
                return;
            };
            if flight.generation != generation {
                return;
            }
            let completion = Arc::clone(&flight.completion);
            let estimated_bytes = flight.estimated_bytes;
            let _removed_flight = inner.flights.remove(&key);
            inner.resident_bytes = inner.resident_bytes.saturating_sub(estimated_bytes);
            let actual_bytes = cache_entry_bytes(&entry);
            if actual_bytes > self.max_resident_bytes
                || !reserve_resident_bytes(&mut inner, actual_bytes, self.max_resident_bytes)
            {
                entry = HlsPreparedTerminalBundleCacheEntry::Failed(
                    HlsPreparedTerminalBundleFailure::ByteCapacityExceeded {
                        required_bytes: actual_bytes,
                        capacity_bytes: self.max_resident_bytes,
                    },
                );
            }
            let result = HlsPreparedTerminalBundleCompletion::from_entry(key, &entry);
            insert_cache_entry(&mut inner, key, entry, self.max_resident_bytes);
            (completion, result)
        };
        completion.publish(result);
    }

    fn lock_inner(&self) -> MutexGuard<'_, HlsPreparedTerminalBundleCacheInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_completion(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        loop {
            let ticket = {
                let mut inner = self.lock_inner();
                if let Some(entry) = inner.entries.get(&key) {
                    return Some(entry.state(key));
                }
                let flight = inner.flights.get(&key)?;
                HlsPreparedTerminalBundleCompletionTicket {
                    key,
                    generation: flight.generation,
                    completion: Arc::clone(&flight.completion),
                }
            };
            if let Some(state) = ticket.wait().await.into_state() {
                return Some(state);
            }
        }
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize { self.lock_inner().entries.len() }

    #[cfg(test)]
    fn in_flight_count(&self) -> usize { self.lock_inner().flights.len() }

    #[cfg(test)]
    fn resident_bytes(&self) -> u64 { self.lock_inner().resident_bytes }
}

fn estimated_bundle_bytes(asset: &HlsTerminalMediaAsset, segment_count: u16) -> Option<u64> {
    u64::try_from(asset.renderer().as_bytes().len())
        .ok()?
        .checked_mul(u64::from(segment_count))
}

fn cache_entry_bytes(entry: &HlsPreparedTerminalBundleCacheEntry) -> u64 {
    match entry {
        HlsPreparedTerminalBundleCacheEntry::Ready(bundle) => bundle.segments.iter().fold(0_u64, |total, segment| {
            total.saturating_add(u64::try_from(segment.bytes.len()).unwrap_or(u64::MAX))
        }),
        HlsPreparedTerminalBundleCacheEntry::Failed(_) | HlsPreparedTerminalBundleCacheEntry::Incompatible(_) => 0,
    }
}

fn evict_lru_entry(inner: &mut HlsPreparedTerminalBundleCacheInner) -> bool {
    let Some((_key, entry)) = inner.entries.pop_lru() else {
        return false;
    };
    inner.resident_bytes = inner.resident_bytes.saturating_sub(cache_entry_bytes(&entry));
    true
}

fn reserve_resident_bytes(
    inner: &mut HlsPreparedTerminalBundleCacheInner,
    additional_bytes: u64,
    max_resident_bytes: u64,
) -> bool {
    while inner.resident_bytes.saturating_add(additional_bytes) > max_resident_bytes {
        if !evict_lru_entry(inner) {
            return false;
        }
    }
    true
}

fn insert_cache_entry(
    inner: &mut HlsPreparedTerminalBundleCacheInner,
    key: HlsPreparedTerminalBundleKey,
    entry: HlsPreparedTerminalBundleCacheEntry,
    max_resident_bytes: u64,
) {
    if let Some(previous) = inner.entries.pop(&key) {
        inner.resident_bytes = inner.resident_bytes.saturating_sub(cache_entry_bytes(&previous));
    }
    let entry_bytes = cache_entry_bytes(&entry);
    while inner.entries.len() >= inner.entries.cap().get()
        || inner.resident_bytes.saturating_add(entry_bytes) > max_resident_bytes
    {
        if !evict_lru_entry(inner) {
            return;
        }
    }
    inner.resident_bytes = inner.resident_bytes.saturating_add(entry_bytes);
    let _replaced = inner.entries.put(key, entry);
}

pub(crate) fn prepared_terminal_bundle_key(
    asset: &HlsTerminalMediaAsset,
    target_duration_ms: u64,
    segment_count: u16,
) -> HlsPreparedTerminalBundleKey {
    HlsPreparedTerminalBundleKey {
        asset: HlsTerminalAssetIdentity::from_asset(asset),
        target_duration_ms,
        segment_count,
    }
}

/// Central target-duration compatibility rule shared by preparation and tail
/// admission. HLS compares the rounded EXTINF duration with TARGETDURATION.
pub(crate) const fn terminal_asset_fits_target_duration(asset_duration_ms: u64, target_duration_ms: u64) -> bool {
    asset_duration_ms.saturating_add(500) / 1_000 <= target_duration_ms / 1_000
}

fn prepared_terminal_bundle_incompatibility(
    asset: &HlsTerminalMediaAsset,
    key: HlsPreparedTerminalBundleKey,
) -> Option<HlsPreparedTerminalBundleIncompatibility> {
    if key.segment_count == 0 {
        return Some(HlsPreparedTerminalBundleIncompatibility::EmptySegmentSet);
    }
    if key.target_duration_ms == 0 {
        return Some(HlsPreparedTerminalBundleIncompatibility::ZeroTargetDuration);
    }
    if !terminal_asset_fits_target_duration(asset.duration_ms(), key.target_duration_ms) {
        return Some(HlsPreparedTerminalBundleIncompatibility::TargetDurationExceeded {
            asset_ms: asset.duration_ms(),
            target_ms: key.target_duration_ms,
        });
    }
    None
}

fn terminal_timestamp_offset_ticks_90khz(
    index: u16,
    source_asset_duration_ticks_90khz: u64,
) -> Result<u64, HlsPreparedTerminalBundleBuildError> {
    let ticks = u128::from(index)
        .checked_mul(u128::from(source_asset_duration_ticks_90khz))
        .and_then(|value| u64::try_from(value).ok());
    ticks.ok_or(HlsPreparedTerminalBundleBuildError::TimestampOffsetOverflow {
        index,
        source_asset_duration_ticks_90khz,
    })
}

fn add_timestamp_offsets_90khz(relative: u64, anchor: u64) -> u64 {
    relative.wrapping_add(anchor) & ((1_u64 << 33) - 1)
}

pub(crate) fn build_prepared_terminal_bundle(
    asset: &HlsTerminalMediaAsset,
    key: HlsPreparedTerminalBundleKey,
) -> Result<Arc<HlsPreparedTerminalBundle>, HlsPreparedTerminalBundleBuildError> {
    if key.asset != HlsTerminalAssetIdentity::from_asset(asset) {
        return Err(HlsPreparedTerminalBundleBuildError::AssetIdentityMismatch);
    }
    if let Some(reason) = prepared_terminal_bundle_incompatibility(asset, key) {
        return Err(HlsPreparedTerminalBundleBuildError::Incompatible(reason));
    }
    let renderer = asset.renderer();
    let source_asset_duration_ticks_90khz = asset.duration_ticks_90khz();
    let mut segments = Vec::with_capacity(usize::from(key.segment_count));
    for index in 0..key.segment_count {
        let timestamp_offset_ticks_90khz =
            terminal_timestamp_offset_ticks_90khz(index, source_asset_duration_ticks_90khz)?;
        let bytes = renderer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz,
                continuity_seed: 0,
                logical_segment_index: index,
            })
            .map_err(HlsPreparedTerminalBundleBuildError::FiniteSegmentRender)?;
        segments.push(HlsPreparedTerminalSegment { index, timestamp_offset_ticks_90khz, bytes });
    }
    Ok(Arc::new(HlsPreparedTerminalBundle {
        key,
        source_asset_duration_ms: asset.duration_ms(),
        source_asset_duration_ticks_90khz,
        segments: Arc::from(segments),
    }))
}

pub(crate) fn anchor_prepared_terminal_bundle(
    asset: &HlsTerminalMediaAsset,
    prepared: &HlsPreparedTerminalBundle,
    anchor: HlsTsSpliceAnchor,
) -> Result<Arc<HlsAnchoredTerminalBundle>, HlsPreparedTerminalBundleBuildError> {
    if prepared.key.asset != HlsTerminalAssetIdentity::from_asset(asset)
        || prepared.source_asset_duration_ms != asset.duration_ms()
        || prepared.source_asset_duration_ticks_90khz != asset.duration_ticks_90khz()
        || !prepared.matches_key_and_shape(prepared.key)
    {
        return Err(HlsPreparedTerminalBundleBuildError::AssetIdentityMismatch);
    }
    let renderer = asset.renderer();
    let mut segments = Vec::with_capacity(prepared.segments.len());
    for relative in prepared.segments.iter() {
        let discontinuity = if relative.index == 0 {
            HlsFiniteTsDiscontinuityMode::FirstPacketPerPid
        } else {
            HlsFiniteTsDiscontinuityMode::None
        };
        let bytes = renderer
            .finalize_prepared_finite_hls_segment(
                &relative.bytes,
                HlsFiniteTsFinalizeSpec {
                    additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                    discontinuity,
                },
            )
            .map_err(HlsPreparedTerminalBundleBuildError::FiniteSegmentRender)?;
        let total_timestamp_offset_ticks_90khz = add_timestamp_offsets_90khz(
            relative.timestamp_offset_ticks_90khz,
            anchor.timestamp_delta_ticks,
        );
        segments.push(HlsAnchoredTerminalSegment {
            index: relative.index,
            total_timestamp_offset_ticks_90khz,
            bytes,
        });
    }
    let anchored = Arc::new(HlsAnchoredTerminalBundle {
        prepared_key: prepared.key,
        splice_anchor: anchor,
        segments: Arc::from(segments),
    });
    if !anchored.matches_key_and_shape(prepared.key, prepared.source_asset_duration_ticks_90khz) {
        return Err(HlsPreparedTerminalBundleBuildError::PublishedBundleShapeMismatch);
    }
    Ok(anchored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::{snapshot_terminal_media_asset, TransportStreamBuffer};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar,
    };

    const TERMINAL_ASSET_BYTES: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/channel_unavailable.ts"));
    const TARGET_DURATION_MS: u64 = 12_000;
    const SEGMENT_COUNT: u16 = super::super::terminal_tail::HLS_TERMINAL_TAIL_SEGMENT_COUNT;

    fn asset() -> Arc<HlsTerminalMediaAsset> {
        let renderer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        snapshot_terminal_media_asset(&renderer).expect("terminal test asset")
    }

    fn ready_bundle(state: HlsPreparedTerminalBundleState) -> Arc<HlsPreparedTerminalBundle> {
        match state {
            HlsPreparedTerminalBundleState::Ready { bundle } => bundle,
            other => panic!("expected ready terminal bundle, got {other:?}"),
        }
    }

    async fn prepare(
        cache: &Arc<HlsPreparedTerminalBundleCache>,
        asset: Arc<HlsTerminalMediaAsset>,
        target_duration_ms: u64,
    ) -> Arc<HlsPreparedTerminalBundle> {
        let key = prepared_terminal_bundle_key(&asset, target_duration_ms, SEGMENT_COUNT);
        let _started = cache.start_preparation(asset, target_duration_ms, SEGMENT_COUNT);
        ready_bundle(cache.wait_for_completion(key).await.expect("terminal preparation state"))
    }

    #[test]
    fn hls_prepared_terminal_bundle_contains_twelve_small_segments_with_exact_asset_offsets() {
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let bundle = build_prepared_terminal_bundle(&asset, key).expect("prepared terminal bundle");

        assert_eq!(SEGMENT_COUNT, 12);
        assert_eq!(bundle.segments.len(), usize::from(SEGMENT_COUNT));
        assert_eq!(bundle.source_asset_duration_ms, asset.duration_ms());
        assert_eq!(bundle.source_asset_duration_ticks_90khz, asset.duration_ticks_90khz());
        assert_eq!(bundle.segments[0].timestamp_offset_ticks_90khz, 0);
        assert_eq!(
            bundle.segments[1].timestamp_offset_ticks_90khz,
            bundle.source_asset_duration_ticks_90khz
        );
        let last = bundle.segments.last().expect("last prepared terminal segment");
        assert_eq!(
            last.timestamp_offset_ticks_90khz,
            u64::from(last.index) * bundle.source_asset_duration_ticks_90khz
        );
        assert!(bundle.segments.iter().all(|segment| !segment.bytes.is_empty() && segment.bytes.len() % 188 == 0));
    }

    #[test]
    fn hls_prepared_terminal_bundle_uses_exact_asset_duration_as_stride() {
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let bundle = build_prepared_terminal_bundle(&asset, key).expect("prepared terminal bundle");

        assert_eq!(bundle.source_asset_duration_ticks_90khz, 902_400);
        assert_eq!(bundle.source_asset_duration_ms, 10_027);
        assert_eq!(bundle.segments[0].timestamp_offset_ticks_90khz, 0);
        assert_eq!(bundle.segments[1].timestamp_offset_ticks_90khz, 902_400);
        assert_eq!(bundle.segments[2].timestamp_offset_ticks_90khz, 1_804_800);
        assert_ne!(bundle.segments[1].timestamp_offset_ticks_90khz, TARGET_DURATION_MS * 90);
    }

    #[test]
    fn target_duration_twelve_seconds_does_not_create_terminal_timestamp_gap() {
        let asset = asset();
        let asset_profile = asset.timestamp_profile().expect("terminal asset timestamp profile");
        let live_profile = crate::api::model::HlsTsTimestampProfile {
            first_clock_90khz: 1_002_716_400,
            last_clock_90khz: 1_003_618_800,
            span_ticks_90khz: 902_400,
            observed_pts_or_dts: true,
            observed_pcr: true,
        };
        let anchor =
            crate::api::model::HlsTsSpliceAnchor::between(live_profile, asset_profile).expect("splice anchor");
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let prepared = build_prepared_terminal_bundle(&asset, key).expect("relative terminal bundle");
        let anchored =
            anchor_prepared_terminal_bundle(&asset, &prepared, anchor).expect("anchored terminal bundle");
        let segment_zero = TransportStreamBuffer::new(anchored.segments[0].bytes.to_vec())
            .finite_hls_timestamp_profile()
            .expect("segment zero profile");
        let segment_one = TransportStreamBuffer::new(anchored.segments[1].bytes.to_vec())
            .finite_hls_timestamp_profile()
            .expect("segment one profile");
        let actual_stride = segment_one
            .first_clock_90khz
            .wrapping_add(1_u64 << 33)
            .wrapping_sub(segment_zero.first_clock_90khz)
            % (1_u64 << 33);

        assert_eq!(actual_stride, asset.duration_ticks_90khz());
        assert_ne!(actual_stride, TARGET_DURATION_MS * 90);
    }

    #[test]
    fn hls_prepared_terminal_bundle_key_separates_asset_revision_fingerprint_and_target_duration() {
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, 10_000, SEGMENT_COUNT);
        let different_target = HlsPreparedTerminalBundleKey { target_duration_ms: 12_000, ..key };
        let different_revision = HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity { revision: key.asset.revision.saturating_add(1), ..key.asset },
            ..key
        };
        let mut fingerprint = key.asset.fingerprint;
        fingerprint[0] ^= 0xFF;
        let different_fingerprint = HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity { fingerprint, ..key.asset },
            ..key
        };

        assert_ne!(key, different_target);
        assert_ne!(key, different_revision);
        assert_ne!(key, different_fingerprint);

        let mut revised_bytes = TERMINAL_ASSET_BYTES.to_vec();
        let last = revised_bytes.last_mut().expect("terminal fixture is non-empty");
        *last ^= 0x01;
        let revised_renderer = TransportStreamBuffer::new(revised_bytes);
        let revised_asset = snapshot_terminal_media_asset(&revised_renderer).expect("revised terminal test asset");
        let revised_key = prepared_terminal_bundle_key(&revised_asset, key.target_duration_ms, key.segment_count);
        assert_ne!(revised_key.asset, key.asset);
        assert_ne!(revised_key, key);
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_same_key_reuses_the_same_arc() {
        let cache = Arc::new(HlsPreparedTerminalBundleCache::new());
        let asset = asset();
        let first = prepare(&cache, Arc::clone(&asset), TARGET_DURATION_MS).await;
        let second = ready_bundle(cache.start_preparation(asset, TARGET_DURATION_MS, SEGMENT_COUNT));

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn hls_prepared_terminal_bundle_completion_ticket_does_not_start_a_missing_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let calls = Arc::clone(&calls);
            Arc::new(move |asset, key| {
                calls.fetch_add(1, Ordering::SeqCst);
                build_prepared_terminal_bundle(asset, key)
            })
        };
        let cache = HlsPreparedTerminalBundleCache::with_limits_and_builder(
            4,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        );
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);

        assert!(cache.completion_ticket(key).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(cache.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_atomic_observation_captures_flight_started_after_missing() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move |asset, key| {
                entered.notify_one();
                let (lock, changed) = &*release;
                let mut released = lock.lock().expect("atomic-observation release lock");
                while !*released {
                    released = changed.wait(released).expect("atomic-observation release wait");
                }
                build_prepared_terminal_bundle(asset, key)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            4,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);

        assert!(matches!(cache.observe_exact(key), HlsPreparedTerminalBundleObservation::Missing));

        let entered_wait = Arc::clone(&entered).notified_owned();
        assert_eq!(
            cache.start_preparation(asset, key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        entered_wait.await;
        let HlsPreparedTerminalBundleObservation::Flight(ticket) = cache.observe_exact(key) else {
            panic!("exact observation must capture the current same-key flight");
        };
        assert_eq!(ticket.key, key);

        {
            let (lock, changed) = &*release;
            *lock.lock().expect("atomic-observation release lock") = true;
            changed.notify_all();
        }
        let HlsPreparedTerminalBundleCompletion::Ready { bundle: completed } = ticket.wait().await else {
            panic!("observed exact-key flight must complete ready");
        };
        let HlsPreparedTerminalBundleObservation::Settled(HlsPreparedTerminalBundleState::Ready { bundle }) =
            cache.observe_exact(key)
        else {
            panic!("completed exact-key flight must be observed as settled");
        };
        assert!(Arc::ptr_eq(&bundle, &completed));
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_completion_ticket_observes_ready_after_pre_wait_completion() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move |asset, key| {
                entered.notify_one();
                let (lock, changed) = &*release;
                let mut released = lock.lock().expect("completion-ticket release lock");
                while !*released {
                    released = changed.wait(released).expect("completion-ticket release wait");
                }
                build_prepared_terminal_bundle(asset, key)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            4,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let entered_wait = Arc::clone(&entered).notified_owned();

        assert_eq!(
            cache.start_preparation(asset, key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        entered_wait.await;
        let ticket = cache.completion_ticket(key).expect("ticket for the current exact-key flight");
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("completion-ticket release lock") = true;
            changed.notify_all();
        }
        let cached = ready_bundle(cache.wait_for_completion(key).await.expect("published ready state"));

        let completion = ticket.wait().await;

        let HlsPreparedTerminalBundleCompletion::Ready { bundle } = completion else {
            panic!("expected ready flight completion, got {completion:?}");
        };
        assert!(Arc::ptr_eq(&bundle, &cached));
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_completion_ticket_reports_failed_flight() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move |_asset, _key| {
                entered.notify_one();
                let (lock, changed) = &*release;
                let mut released = lock.lock().expect("failed-flight release lock");
                while !*released {
                    released = changed.wait(released).expect("failed-flight release wait");
                }
                Err(HlsPreparedTerminalBundleBuildError::AssetIdentityMismatch)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            4,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let entered_wait = Arc::clone(&entered).notified_owned();

        assert_eq!(
            cache.start_preparation(asset, key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        entered_wait.await;
        let ticket = cache.completion_ticket(key).expect("ticket for failing exact-key flight");
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("failed-flight release lock") = true;
            changed.notify_all();
        }

        assert!(matches!(
            ticket.wait().await,
            HlsPreparedTerminalBundleCompletion::Failed {
                key: failed_key,
                reason: HlsPreparedTerminalBundleFailure::Build(
                    HlsPreparedTerminalBundleBuildError::AssetIdentityMismatch
                ),
            } if failed_key == key
        ));
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_completion_ticket_stays_bound_across_same_key_flight_replacement() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let released_builds = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let calls = Arc::clone(&calls);
            let released_builds = Arc::clone(&released_builds);
            Arc::new(move |asset, key| {
                let call_index = calls.fetch_add(1, Ordering::SeqCst);
                assert!(entered_tx.send(call_index).is_ok(), "replacement test receiver must remain live");
                let (lock, changed) = &*released_builds;
                let mut released = lock.lock().expect("replacement release lock");
                while *released <= call_index {
                    released = changed.wait(released).expect("replacement release wait");
                }
                drop(released);
                let mut bundle = build_prepared_terminal_bundle(asset, key)?;
                Arc::make_mut(&mut bundle).source_asset_duration_ms =
                    u64::try_from(call_index).unwrap_or(u64::MAX).saturating_add(1);
                Ok(bundle)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            1,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);

        assert_eq!(
            cache.start_preparation(Arc::clone(&asset), key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        assert_eq!(entered_rx.recv().await, Some(0));
        let first_ticket = cache.completion_ticket(key).expect("first-generation ticket");
        {
            let (lock, changed) = &*released_builds;
            *lock.lock().expect("replacement release lock") = 1;
            changed.notify_all();
        }
        let HlsPreparedTerminalBundleCompletion::Ready { bundle: first_bundle } = first_ticket.clone().wait().await
        else {
            panic!("first flight must complete ready");
        };

        let eviction_key =
            prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS.saturating_add(1_000), SEGMENT_COUNT);
        assert_eq!(
            cache.start_preparation(Arc::clone(&asset), eviction_key.target_duration_ms, eviction_key.segment_count,),
            HlsPreparedTerminalBundleState::Preparing { key: eviction_key }
        );
        assert_eq!(entered_rx.recv().await, Some(1));
        let eviction_ticket = cache.completion_ticket(eviction_key).expect("eviction-flight ticket");
        {
            let (lock, changed) = &*released_builds;
            *lock.lock().expect("replacement release lock") = 2;
            changed.notify_all();
        }
        assert!(matches!(eviction_ticket.wait().await, HlsPreparedTerminalBundleCompletion::Ready { .. }));
        assert!(cache.state(key).is_none(), "capacity-one cache must evict the first completed key");

        assert_eq!(
            cache.start_preparation(asset, key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        assert_eq!(entered_rx.recv().await, Some(2));
        let second_ticket = cache.completion_ticket(key).expect("replacement-generation ticket");
        assert_ne!(first_ticket.generation, second_ticket.generation);

        let HlsPreparedTerminalBundleCompletion::Ready { bundle: retained_first_bundle } = first_ticket.wait().await
        else {
            panic!("old ticket must retain its original completion");
        };
        assert_eq!(retained_first_bundle.source_asset_duration_ms, 1);
        assert!(Arc::ptr_eq(&retained_first_bundle, &first_bundle));

        {
            let (lock, changed) = &*released_builds;
            *lock.lock().expect("replacement release lock") = 3;
            changed.notify_all();
        }
        let HlsPreparedTerminalBundleCompletion::Ready { bundle: second_bundle } = second_ticket.wait().await else {
            panic!("replacement flight must complete ready");
        };
        assert_eq!(second_bundle.source_asset_duration_ms, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_singleflight_has_one_owner_for_the_same_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let calls = Arc::clone(&calls);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move |asset, key| {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                let (lock, changed) = &*release;
                let mut released = lock.lock().expect("singleflight release lock");
                while !*released {
                    released = changed.wait(released).expect("singleflight release wait");
                }
                build_prepared_terminal_bundle(asset, key)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            4,
            2,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);
        let entered_wait = Arc::clone(&entered).notified_owned();

        assert_eq!(
            cache.start_preparation(Arc::clone(&asset), TARGET_DURATION_MS, SEGMENT_COUNT),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        entered_wait.await;
        assert_eq!(
            cache.start_preparation(asset, TARGET_DURATION_MS, SEGMENT_COUNT),
            HlsPreparedTerminalBundleState::Preparing { key }
        );
        assert_eq!(cache.in_flight_count(), 1);
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("singleflight release lock") = true;
            changed.notify_all();
        }
        let first = ready_bundle(cache.wait_for_completion(key).await.expect("singleflight completion"));
        let second = ready_bundle(cache.state(key).expect("cached terminal bundle"));

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_limits_in_flight_owner_tasks() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let builder: Arc<HlsPreparedTerminalBundleBuilder> = {
            let release = Arc::clone(&release);
            Arc::new(move |asset, key| {
                let _entered = entered_tx.send(());
                let (lock, changed) = &*release;
                let mut released = lock.lock().expect("owner-task release lock");
                while !*released {
                    released = changed.wait(released).expect("owner-task release wait");
                }
                build_prepared_terminal_bundle(asset, key)
            })
        };
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            8,
            4,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            builder,
        ));
        let asset = asset();
        let targets = [10_000, 11_000, 12_000, 13_000];
        let keys = targets.map(|target_duration_ms| {
            let key = prepared_terminal_bundle_key(&asset, target_duration_ms, SEGMENT_COUNT);
            assert_eq!(
                cache.start_preparation(Arc::clone(&asset), target_duration_ms, SEGMENT_COUNT),
                HlsPreparedTerminalBundleState::Preparing { key }
            );
            key
        });
        for _ in targets {
            entered_rx.recv().await.expect("owner task enters builder");
        }
        let rejected_key = prepared_terminal_bundle_key(&asset, 14_000, SEGMENT_COUNT);

        assert_eq!(cache.in_flight_count(), 4);
        assert_eq!(
            cache.start_preparation(asset, rejected_key.target_duration_ms, rejected_key.segment_count),
            HlsPreparedTerminalBundleState::Failed {
                key: rejected_key,
                reason: HlsPreparedTerminalBundleFailure::PreparationCapacityExceeded,
            }
        );
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("owner-task release lock") = true;
            changed.notify_all();
        }
        for key in keys {
            assert!(matches!(
                cache.wait_for_completion(key).await,
                Some(HlsPreparedTerminalBundleState::Ready { .. })
            ));
        }
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_cache_is_bounded_and_retained_arc_survives_eviction() {
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            2,
            1,
            HLS_PREPARED_TERMINAL_BUNDLE_MAX_RESIDENT_BYTES,
            Arc::new(build_prepared_terminal_bundle),
        ));
        let asset = asset();
        let first_key = prepared_terminal_bundle_key(&asset, 10_000, SEGMENT_COUNT);
        let first = prepare(&cache, Arc::clone(&asset), 10_000).await;
        let _second = prepare(&cache, Arc::clone(&asset), 11_000).await;
        let _third = prepare(&cache, asset, 12_000).await;

        assert_eq!(cache.entry_count(), 2);
        assert!(cache.state(first_key).is_none());
        assert_eq!(first.key, first_key);
        assert_eq!(first.segments.len(), usize::from(SEGMENT_COUNT));
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_cache_evicts_by_byte_weight_before_starting_a_new_flight() {
        let asset = asset();
        let byte_budget = estimated_bundle_bytes(&asset, SEGMENT_COUNT).expect("bundle byte budget");
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            32,
            1,
            byte_budget,
            Arc::new(build_prepared_terminal_bundle),
        ));
        let first_key = prepared_terminal_bundle_key(&asset, 10_000, SEGMENT_COUNT);
        let _retained = prepare(&cache, Arc::clone(&asset), 10_000).await;
        let second_key = prepared_terminal_bundle_key(&asset, 12_000, SEGMENT_COUNT);
        let _second = prepare(&cache, asset, 12_000).await;

        assert!(cache.state(first_key).is_none());
        assert!(matches!(cache.state(second_key), Some(HlsPreparedTerminalBundleState::Ready { .. })));
        assert!(cache.resident_bytes() <= byte_budget);
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_incompatible_target_is_typed_without_starting_work() {
        let cache = Arc::new(HlsPreparedTerminalBundleCache::new());
        let asset = asset();
        let key = prepared_terminal_bundle_key(&asset, 9_000, SEGMENT_COUNT);

        assert_eq!(
            cache.start_preparation(Arc::clone(&asset), key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Incompatible {
                key,
                reason: HlsPreparedTerminalBundleIncompatibility::TargetDurationExceeded {
                    asset_ms: asset.duration_ms(),
                    target_ms: key.target_duration_ms,
                },
            }
        );
        assert_eq!(cache.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn hls_prepared_terminal_bundle_rejects_a_single_bundle_above_the_byte_budget() {
        let asset = asset();
        let required_bytes = estimated_bundle_bytes(&asset, SEGMENT_COUNT).expect("bounded bundle estimate");
        let capacity_bytes = required_bytes.saturating_sub(1);
        let cache = Arc::new(HlsPreparedTerminalBundleCache::with_limits_and_builder(
            32,
            4,
            capacity_bytes,
            Arc::new(build_prepared_terminal_bundle),
        ));
        let key = prepared_terminal_bundle_key(&asset, TARGET_DURATION_MS, SEGMENT_COUNT);

        assert_eq!(
            cache.start_preparation(asset, key.target_duration_ms, key.segment_count),
            HlsPreparedTerminalBundleState::Failed {
                key,
                reason: HlsPreparedTerminalBundleFailure::ByteCapacityExceeded {
                    required_bytes,
                    capacity_bytes,
                },
            }
        );
        assert_eq!(cache.in_flight_count(), 0);
        assert_eq!(cache.resident_bytes(), 0);
    }

    #[test]
    fn hls_prepared_terminal_bundle_reports_checked_timestamp_overflow() {
        assert_eq!(
            terminal_timestamp_offset_ticks_90khz(2, u64::MAX),
            Err(HlsPreparedTerminalBundleBuildError::TimestampOffsetOverflow {
                index: 2,
                source_asset_duration_ticks_90khz: u64::MAX,
            })
        );
    }
}
