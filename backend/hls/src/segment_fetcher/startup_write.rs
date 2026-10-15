//! Failure/cancellation closure for one startup write, keyed by its peak owner.
use super::{segment_fetch_attempt_matches, SegmentFetchContext, SegmentFetchSnapshot};
use crate::{sync_ext::MutexExt, HlsRevisionDiskReservation, SegmentRevisionGuard, SegmentRevisionState};
use std::sync::Arc;

/// Rolls back only this attempt and keeps immutable completed revisions alive.
pub(super) struct StartupWrite {
    context: SegmentFetchContext,
    snapshot: SegmentFetchSnapshot,
    budget: Arc<HlsRevisionDiskReservation>,
    revision: SegmentRevisionGuard,
    armed: bool,
    observed: Option<Arc<tokio::sync::Mutex<super::startup_fill::HlsObservedFill>>>,
}

impl StartupWrite {
    pub(super) fn new(
        context: &SegmentFetchContext,
        snapshot: &SegmentFetchSnapshot,
        budget: Arc<HlsRevisionDiskReservation>,
        revision: SegmentRevisionGuard,
    ) -> Self {
        Self { context: context.clone(), snapshot: snapshot.clone(), budget, revision, armed: true, observed: None }
    }

    pub(super) fn track_fill(&mut self, fill: &Arc<tokio::sync::Mutex<super::startup_fill::HlsObservedFill>>) {
        self.observed = Some(Arc::clone(fill));
    }

    pub(super) fn finish(&mut self) { self.armed = false; }

    pub(super) async fn abort(&mut self) {
        cleanup(&self.context, &self.snapshot, &self.budget, &self.revision, self.observed.as_ref()).await;
        self.armed = false;
    }
}

impl Drop for StartupWrite {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let context = self.context.clone();
        let snapshot = self.snapshot.clone();
        let budget = Arc::clone(&self.budget);
        let revision = self.revision.clone();
        let observed = self.observed.clone();
        revision.revision().fail();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                cleanup(&context, &snapshot, &budget, &revision, observed.as_ref()).await;
                let mut session = context.session.write().await;
                session.active_segment_fetches = session.active_segment_fetches.saturating_sub(1);
                if session
                    .segments
                    .get(&snapshot.proxy_seq)
                    .is_some_and(|entry| segment_fetch_attempt_matches(entry, &snapshot))
                {
                    if let Some(entry) = session.segments.get_mut(&snapshot.proxy_seq) {
                        entry.status = crate::SegmentCacheStatus::Discovered;
                    }
                    if let Some(notifier) = session.segment_fetch_notifiers.remove(&snapshot.proxy_seq) {
                        notifier.notify_waiters();
                    }
                }
            });
        }
    }
}

async fn cleanup(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    budget: &HlsRevisionDiskReservation,
    revision: &SegmentRevisionGuard,
    observed: Option<&Arc<tokio::sync::Mutex<super::startup_fill::HlsObservedFill>>>,
) {
    if let Some(observed) = observed {
        use tokio::io::AsyncWriteExt;
        let mut fill = observed.lock().await;
        if let Some(mut file) = fill.file.take() {
            let _ = file.flush().await;
        }
    }
    revision.revision().fail();
    if let Err(error) = context.segment_cache.delete_write_owned(&snapshot.cache_key, budget).await {
        log::warn!(
            "HLS startup partial commit cleanup deferred: proxy_seq={} error_kind={:?}",
            snapshot.proxy_seq,
            error.kind()
        );
    }
    // Complete revisions have immutable, independently pinned bytes. Failed
    // spools are unique to this revision and cannot be confused with a retry.
    if matches!(revision.revision().state(), SegmentRevisionState::Failed) {
        revision.revision().file_pin.lock_unpoisoned().take();
        if let Err(error) = context.segment_cache.delete_write_owned(&revision.revision().key, budget).await {
            log::warn!(
                "HLS startup failed revision cleanup deferred: proxy_seq={} error_kind={:?}",
                snapshot.proxy_seq,
                error.kind()
            );
        }
    }
}
