use super::{
    safe_proxy_session_id, CacheDeletionBatch, CacheDeletionQueueState, GarbageCollectionReport, HlsSegmentCache,
    HlsSwitchCacheCleanupReservation, MapCacheKey, PendingCacheObjectDeletion, SegmentCacheKey,
    TransientObjectCacheKey, MAX_PENDING_CACHE_DELETIONS, SWITCH_CACHE_CLEANUP_HEADROOM,
};
use log::{debug, info, warn};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex as StdMutex},
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SegmentCacheDeletionReason {
    Duration,
    SessionSize,
    GlobalSize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum CacheObjectDeletion {
    Segment { key: SegmentCacheKey, reason: SegmentCacheDeletionReason },
    Map(MapCacheKey),
    TransientObject { key: TransientObjectCacheKey, content_length: u64 },
    UncommittedSwitchSegment(SegmentCacheKey),
    UncommittedSwitchMap(MapCacheKey),
}

impl CacheObjectDeletion {
    pub(super) async fn delete_from(&self, cache: &HlsSegmentCache) -> io::Result<()> {
        match self {
            Self::Segment { key, .. } | Self::UncommittedSwitchSegment(key) => cache.delete_if_inactive(key).await,
            Self::Map(key) | Self::UncommittedSwitchMap(key) => cache.delete_if_inactive(key).await,
            Self::TransientObject { key, .. } => cache.delete_if_inactive(key).await,
        }
    }

    pub(super) fn record_success(&self, report: &mut GarbageCollectionReport) {
        report.cache_object_deletions_succeeded = report.cache_object_deletions_succeeded.saturating_add(1);
        match self {
            Self::Segment { reason: SegmentCacheDeletionReason::Duration, .. } => {
                report.segments_deleted_duration = report.segments_deleted_duration.saturating_add(1);
            }
            Self::Segment { reason: SegmentCacheDeletionReason::SessionSize, .. } => {
                report.segments_deleted_size_session = report.segments_deleted_size_session.saturating_add(1);
            }
            Self::Segment { reason: SegmentCacheDeletionReason::GlobalSize, .. } => {
                report.segments_deleted_size_global = report.segments_deleted_size_global.saturating_add(1);
            }
            Self::Map(_) => {
                report.maps_deleted = report.maps_deleted.saturating_add(1);
            }
            Self::TransientObject { content_length, .. } => {
                report.transient_objects_deleted = report.transient_objects_deleted.saturating_add(1);
                report.transient_object_bytes_deleted =
                    report.transient_object_bytes_deleted.saturating_add(*content_length);
            }
            Self::UncommittedSwitchSegment(_) | Self::UncommittedSwitchMap(_) => {}
        }
    }

    pub(super) fn log_success(&self) {
        match self {
            Self::Segment { key, .. } => {
                info!(
                    "Segment '{:06}' removed: session={} source=normal",
                    key.proxy_seq(),
                    safe_proxy_session_id(key.proxy_session_id()),
                );
            }
            Self::UncommittedSwitchSegment(_) | Self::UncommittedSwitchMap(_) => {
                debug!("HLS uncommitted switch cache object removed");
            }
            Self::Map(_) | Self::TransientObject { .. } => {}
        }
    }

    pub(super) fn log_deferred(&self, attempts: u16, error_kind: Option<io::ErrorKind>) {
        let object_kind = match self {
            Self::Segment { .. } => "segment",
            Self::Map(_) => "map",
            Self::TransientObject { .. } => "transient-object",
            Self::UncommittedSwitchSegment(_) => "uncommitted-switch-segment",
            Self::UncommittedSwitchMap(_) => "uncommitted-switch-map",
        };
        warn!(
            "HLS cache object deletion deferred: object_kind={object_kind} attempts={attempts} error_kind={error_kind:?}"
        );
    }
}

pub(super) struct CacheDeletionQueueGeneration;

impl Default for CacheDeletionQueueState {
    fn default() -> Self {
        Self { pending: VecDeque::new(), reserved_slots: 0, generation: Arc::new(CacheDeletionQueueGeneration) }
    }
}

impl HlsSwitchCacheCleanupReservation {
    pub(super) fn reserve(
        queue: Arc<StdMutex<CacheDeletionQueueState>>,
        deletion: CacheObjectDeletion,
    ) -> Option<Self> {
        let queue_generation = {
            let mut state = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.pending.len().saturating_add(state.reserved_slots) >= MAX_PENDING_CACHE_DELETIONS {
                return None;
            }
            state.reserved_slots = state.reserved_slots.saturating_add(1);
            Arc::clone(&state.generation)
        };
        Some(Self { queue, queue_generation, deletion: Some(deletion), slot_reserved: true })
    }

    pub fn disarm(&mut self) {
        self.deletion = None;
        self.release_slot();
    }

    pub(super) fn release_slot(&mut self) {
        if !self.slot_reserved {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::ptr_eq(&state.generation, &self.queue_generation) {
            state.reserved_slots = state.reserved_slots.saturating_sub(1);
        }
        self.slot_reserved = false;
    }
}

impl std::fmt::Debug for HlsSwitchCacheCleanupReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsSwitchCacheCleanupReservation")
            .field("armed", &self.deletion.is_some())
            .field("slot_reserved", &self.slot_reserved)
            .finish_non_exhaustive()
    }
}

impl Drop for HlsSwitchCacheCleanupReservation {
    fn drop(&mut self) {
        if !self.slot_reserved {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::ptr_eq(&state.generation, &self.queue_generation) {
            state.reserved_slots = state.reserved_slots.saturating_sub(1);
            if let Some(deletion) = self.deletion.take() {
                state.pending.push_back(PendingCacheObjectDeletion { deletion, attempts: 0 });
            }
        }
        self.slot_reserved = false;
    }
}

impl CacheDeletionBatch {
    pub(super) fn reserve(queue: Arc<StdMutex<CacheDeletionQueueState>>) -> Self {
        let reserved_slots = {
            let mut state = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let batch_limit = MAX_PENDING_CACHE_DELETIONS.saturating_sub(SWITCH_CACHE_CLEANUP_HEADROOM);
            let available = batch_limit.saturating_sub(state.pending.len()).saturating_sub(state.reserved_slots);
            state.reserved_slots = state.reserved_slots.saturating_add(available);
            available
        };
        Self { queue, reserved_slots, deletions: Vec::new() }
    }

    pub(super) fn has_capacity(&self) -> bool { self.deletions.len() < self.reserved_slots }

    pub(super) fn remaining_capacity(&self) -> usize { self.reserved_slots.saturating_sub(self.deletions.len()) }

    pub(super) fn push(&mut self, deletion: CacheObjectDeletion) { self.deletions.push(deletion); }

    pub(super) fn persist_pending(&mut self, report: &mut GarbageCollectionReport) {
        let planned = self.deletions.len();
        if planned == 0 {
            return;
        }
        {
            let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reserved_slots = state.reserved_slots.saturating_sub(planned);
            state
                .pending
                .extend(self.deletions.drain(..).map(|deletion| PendingCacheObjectDeletion { deletion, attempts: 0 }));
        }
        self.reserved_slots = self.reserved_slots.saturating_sub(planned);
        report.cache_object_deletions_planned = report.cache_object_deletions_planned.saturating_add(planned);
    }

    pub(super) fn persist(mut self, report: &mut GarbageCollectionReport) { self.persist_pending(report); }

    pub(super) async fn execute_prioritized(
        mut self,
        cache: &HlsSegmentCache,
        report: &mut GarbageCollectionReport,
        attempt_budget: &mut usize,
    ) {
        while *attempt_budget > 0 {
            let Some(deletion) = self.deletions.first().cloned() else {
                break;
            };
            *attempt_budget = attempt_budget.saturating_sub(1);
            let result = deletion.delete_from(cache).await;
            let completed = self.deletions.remove(0);
            self.release_reserved_slot();
            report.cache_object_deletions_planned = report.cache_object_deletions_planned.saturating_add(1);
            match result {
                Ok(()) => {
                    completed.record_success(report);
                    completed.log_success();
                }
                Err(error) => {
                    completed.log_deferred(1, Some(error.kind()));
                    self.persist_failed(completed);
                }
            }
        }
        self.persist_pending(report);
    }

    pub(super) fn release_reserved_slot(&mut self) {
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved_slots = state.reserved_slots.saturating_sub(1);
        self.reserved_slots = self.reserved_slots.saturating_sub(1);
    }

    pub(super) fn persist_failed(&mut self, deletion: CacheObjectDeletion) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .push_back(PendingCacheObjectDeletion { deletion, attempts: 1 });
    }
}

impl Drop for CacheDeletionBatch {
    fn drop(&mut self) {
        if self.reserved_slots == 0 && self.deletions.is_empty() {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved_slots = state.reserved_slots.saturating_sub(self.reserved_slots);
        state
            .pending
            .extend(self.deletions.drain(..).map(|deletion| PendingCacheObjectDeletion { deletion, attempts: 0 }));
        self.reserved_slots = 0;
    }
}
