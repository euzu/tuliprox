//! Shared, bounded retirement for regular GC and projected-capacity reclamation.
use super::{GarbageCollectionReport, HlsGarbageCollector, HlsSessionHandle};
use crate::{sync_ext::MutexExt, SegmentRevision, SegmentRevisionStore};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Weak},
};

struct RetirementBatch {
    store: Arc<SegmentRevisionStore>,
    pending: VecDeque<Arc<SegmentRevision>>,
}

impl Drop for RetirementBatch {
    fn drop(&mut self) {
        // Cancellation or a failed delete preserves retry ownership, including
        // all later entries in the batch which have not reached the filesystem.
        for revision in self.pending.drain(..) {
            let _ = self.store.requeue_retired(revision);
        }
    }
}

impl HlsGarbageCollector {
    pub(crate) fn install_revision_store(&self, store: &Arc<SegmentRevisionStore>) {
        *self.revision_store.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(store));
    }

    /// Caller already owns the GC gate. Never calls admission or reacquires GC.
    #[allow(
        clippy::too_many_lines,
        reason = "One bounded lease/session snapshot followed by cancellation-safe filesystem retirement."
    )]
    pub(super) async fn cleanup_revisions(
        &self,
        sessions: &[HlsSessionHandle],
        report: &mut GarbageCollectionReport,
    ) -> io::Result<()> {
        self.cache.retry_abandoned_write_deletions().await;
        let before = (
            report.revision_deletions_planned,
            report.revision_deletions_succeeded,
            report.revision_deletions_failed,
            report.revision_bytes_reclaimed,
        );
        let mut stores: Vec<_> = self
            .revision_store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .into_iter()
            .collect();
        let leases = self
            .access_leases
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        for handle in sessions {
            let mut lease_guard = match &leases {
                Some(leases) => {
                    if let Ok(guard) = leases.try_write() {
                        Some(guard)
                    } else {
                        report.revision_lock_conflicts += 1;
                        continue;
                    }
                }
                None => None,
            };
            let Ok(mut session) = handle.try_write() else {
                report.revision_lock_conflicts += 1;
                continue;
            };
            let id = session.proxy_session_id.clone();
            if let Some(leases) = &mut lease_guard {
                session.live_window_demands.retain(|lease_id, demand| {
                    leases.response_snapshot(lease_id, &id, tuliprox_core::utils::current_time_millis()).is_some_and(
                        |lease| {
                            lease.issued_at_ms == demand.issued_at_ms
                                && matches!(
                                    lease.state,
                                    crate::HlsAccessLeaseState::Pending
                                        | crate::HlsAccessLeaseState::Activated
                                        | crate::HlsAccessLeaseState::Idle
                                )
                        },
                    )
                });
            }
            let floor = session.segments.first_key_value().map_or(u64::MAX, |(seq, _)| *seq);
            let crate::HlsSession { startup, segments, .. } = &mut *session;
            if let Some(startup) = startup {
                startup.revisions.retain(|seq, _| segments.contains_key(seq));
                if !stores.iter().any(|store| Arc::ptr_eq(store, &startup.store)) {
                    stores.push(Arc::clone(&startup.store));
                }
            }
            if let Some(leases) = &mut lease_guard {
                leases.prune_revision_bindings(&id, floor);
            }
        }
        let mut remaining = super::MAX_CACHE_DELETE_RETRIES_PER_RUN;
        for store in stores {
            if remaining == 0 {
                break;
            }
            let pending = store.retire_unpinned_limit(remaining)?;
            let mut batch = RetirementBatch { store, pending: pending.into() };
            let attempts = batch.pending.len();
            for _ in 0..attempts {
                let Some(revision) = batch.pending.front().cloned() else { break };
                report.revision_deletions_planned += 1;
                remaining -= 1;
                revision.file_pin.lock_unpoisoned().take();
                let size = self.cache.metadata(&revision.key).await?.map_or(0, |metadata| metadata.size);
                match self.cache.delete_if_inactive(&revision.key).await {
                    Ok(()) => {
                        batch.pending.pop_front();
                        report.revision_deletions_succeeded += 1;
                        report.revision_bytes_reclaimed = report.revision_bytes_reclaimed.saturating_add(size);
                    }
                    Err(error) => {
                        report.revision_deletions_failed += 1;
                        log::warn!(
                            "HLS revision delete deferred: proxy_session={} proxy_seq={} error_kind={:?}",
                            crate::safe_proxy_session_id(crate::HlsCacheObjectKey::proxy_session_id(&revision.key)),
                            revision.key.proxy_seq,
                            error.kind()
                        );
                        // Put failed entries at the end of this bounded batch;
                        // its Drop requeues them after successful work finishes.
                        batch.pending.rotate_left(1);
                    }
                }
            }
        }
        self.metrics.record_revision_gc(
            report.revision_deletions_planned - before.0,
            report.revision_deletions_succeeded - before.1,
            report.revision_deletions_failed - before.2,
            report.revision_bytes_reclaimed - before.3,
        );
        log::debug!(
            "HLS revision GC: planned={} succeeded={} failed={} reclaimed_bytes={} lock_conflicts={}",
            report.revision_deletions_planned,
            report.revision_deletions_succeeded,
            report.revision_deletions_failed,
            report.revision_bytes_reclaimed,
            report.revision_lock_conflicts
        );
        Ok(())
    }
}
