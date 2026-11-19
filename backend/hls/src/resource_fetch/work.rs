use super::HlsOriginResourceFetchError;
use crate::{HlsBoundAccountAcquireErrorKind, HlsSessionHandle};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::time::Instant;
use tuliprox_core::model::PlaybackKind;
use tuliprox_session::{ActiveProviderManager, ProviderWorkReservation};

/// One fixed resource budget, shared by attempts and (for direct streams) body.
/// The timer is a backstop even if a consumer retains a guard without polling.
pub struct HlsOriginRetryWork {
    deadline: Instant,
    generation: u64,
    count: Arc<AtomicUsize>,
    active: AtomicBool,
    provider: Mutex<Option<ProviderWorkReservation>>,
    timer: Mutex<Option<tokio::task::AbortHandle>>,
    protects_provider: bool,
}

impl std::fmt::Debug for HlsOriginRetryWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsOriginRetryWork")
            .field("deadline", &self.deadline)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl HlsOriginRetryWork {
    pub async fn for_transient(
        session: &HlsSessionHandle,
        manager: &Arc<ActiveProviderManager>,
        policy: &crate::SegmentFetchPolicy,
        kind: crate::TransientResourceKind,
    ) -> Option<Arc<Self>> {
        Self::for_resource(
            session,
            Some(manager),
            policy,
            crate::transient_fetcher::hls_transient_resource_fetch_kind(kind),
            crate::HlsResourceFetchSource::Transient,
        )
        .await
    }

    pub(crate) async fn for_resource(
        session: &HlsSessionHandle,
        manager: Option<&Arc<ActiveProviderManager>>,
        policy: &crate::SegmentFetchPolicy,
        kind: crate::HlsResourceFetchKind,
        source: crate::HlsResourceFetchSource,
    ) -> Option<Arc<Self>> {
        let target = session.read().await.target_duration;
        let budget = std::time::Duration::from_millis(policy.resource_retry_budget_ms(kind, source, target));
        let now = Instant::now();
        Self::begin(session, manager, now.checked_add(budget).unwrap_or(now)).await
    }

    pub(crate) async fn begin(
        session: &HlsSessionHandle,
        manager: Option<&Arc<ActiveProviderManager>>,
        deadline: Instant,
    ) -> Option<Arc<Self>> {
        let mut state = session.write().await;
        let protects_provider = match state.origin_playback_kind() {
            PlaybackKind::LiveHls => true,
            PlaybackKind::Catchup => false,
            _ => return None,
        };
        let provider = manager.filter(|_| protects_provider).and_then(|manager| {
            let binding = state.origin_account_binding.as_ref()?;
            manager.reserve_hls_origin_work(
                &binding.session_owner,
                &binding.account_name,
                binding.provider_binding_tag?,
                deadline,
            )
        });
        let generation = state.start_origin_work();
        let work = Arc::new(Self {
            deadline,
            generation,
            count: Arc::clone(&state.activity.active_origin_work_count),
            active: AtomicBool::new(true),
            provider: Mutex::new(provider),
            timer: Mutex::new(None),
            protects_provider,
        });
        let weak = Arc::downgrade(&work);
        let timer = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(work) = weak.upgrade() {
                work.finish();
            }
        });
        *work.timer.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(timer.abort_handle());
        Some(work)
    }

    pub(crate) fn deadline(&self) -> Instant { self.deadline }

    pub(crate) async fn protect(
        &self,
        manager: &Arc<ActiveProviderManager>,
        session: &HlsSessionHandle,
        expected_generation: u64,
        require_allocation: bool,
    ) -> Result<(), HlsOriginResourceFetchError> {
        let state = session.read().await;
        if expected_generation != self.generation || state.activity.origin_work_generation != self.generation {
            return Err(HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::StoreRace));
        }
        let mut provider = self.provider.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.active.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(HlsOriginResourceFetchError::Timeout);
        }
        // Catchup keeps the existing per-attempt I/O allocation contract.
        // Its bounded work counter only prevents lifecycle detachment during preparation.
        if !self.protects_provider {
            return Ok(());
        }
        // Renew the right first: a lapsed sibling's right ends with the operation that
        // granted it, so only a refused renewal turns that lapse into a failure.
        let lapsed = provider.as_ref().is_some_and(|right| !right.is_current());
        let binding = state.origin_account_binding.as_ref();
        let renewed = binding.and_then(|binding| {
            binding.provider_binding_tag.and_then(|tag| {
                manager.reserve_hls_origin_work(&binding.session_owner, &binding.account_name, tag, self.deadline)
            })
        });
        if let Some(right) = renewed {
            *provider = Some(right);
        } else if lapsed {
            return Err(HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::StoreRace));
        }
        // A missing binding keeps the per-attempt allocation contract.
        if binding.is_some() && require_allocation && provider.is_none() {
            log::debug!(
                "HLS origin work refused after admission: proxy_session={} reason={}",
                crate::safe_proxy_session_id(&state.proxy_session_id),
                if binding.is_some_and(|binding| binding.provider_binding_tag.is_some()) {
                    "work_right_denied"
                } else {
                    "missing_binding_tag"
                }
            );
            return Err(HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Unavailable));
        }
        Ok(())
    }

    fn finish(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.provider.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            self.count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for HlsOriginRetryWork {
    fn drop(&mut self) {
        self.finish();
        if let Some(timer) = self.timer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            timer.abort();
        }
    }
}
