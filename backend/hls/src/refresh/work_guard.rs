//! Cancellation owns the same work counter and refresh token as normal completion.
use super::OriginRefreshRequest;
use crate::{manifest_fetch::HlsManifestAttemptContext, HlsSessionHandle};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

pub struct RefreshWorkGuard {
    counter: Option<Arc<AtomicUsize>>,
    session: HlsSessionHandle,
    identity: Option<Arc<()>>,
    attempt: Option<Arc<HlsManifestAttemptContext>>,
    generation: Option<u64>,
    now_ms: u64,
    armed: AtomicBool,
    counter_finished: AtomicBool,
    origin_io: Option<crate::HlsOriginIoContext>,
}

impl RefreshWorkGuard {
    pub(super) fn new(request: &OriginRefreshRequest, session: &crate::HlsSession) -> Self {
        let generation = request.origin_io.as_ref().and_then(|io| io.started_generation);
        let counter =
            if generation.is_some() { Some(Arc::clone(&session.activity.active_origin_work_count)) } else { None };
        Self {
            counter,
            session: Arc::clone(&request.session),
            identity: request.refresh_identity.clone(),
            attempt: request.attempt.clone(),
            generation,
            now_ms: request.hls_proxy.now_ms(),
            armed: AtomicBool::new(true),
            counter_finished: AtomicBool::new(false),
            origin_io: request.origin_io.clone(),
        }
    }

    pub(super) fn disarm(&self) { self.armed.store(false, Ordering::Release); }
    pub(super) fn finish_counter(&self) { self.counter_finished.store(true, Ordering::Release); }
}

impl Drop for RefreshWorkGuard {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        drop(self.origin_io.as_ref().and_then(crate::HlsOriginIoContext::take_preacquired_provider_handle));
        if let Some(counter) = self.counter.as_ref().filter(|_| !self.counter_finished.load(Ordering::Acquire)) {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
        let Some(identity) = self.identity.clone() else { return };
        let session = Arc::clone(&self.session);
        let attempt = self.attempt.clone();
        let generation = self.generation;
        let now_ms = self.now_ms;
        // The provider I/O guard releases its own handle independently of this lock.
        tokio::spawn(async move {
            let mut session = session.write().await;
            if session.origin_refresh.refresh_identity.as_ref().is_some_and(|current| Arc::ptr_eq(&identity, current)) {
                if generation.is_none_or(|generation| generation == session.activity.origin_work_generation) {
                    if let Some(attempt) = attempt {
                        attempt.finish(true);
                    }
                }
                session.origin_refresh.mark_invalidated(now_ms);
            }
        });
    }
}
