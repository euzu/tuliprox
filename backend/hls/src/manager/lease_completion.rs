//! Lease completion belongs to the runtime, not to the HTTP future waiting for it.
use super::{HlsAccessLeaseId, HlsAccessLeaseRemovalPreparation, HlsProxyManager, ProxySessionId};
use crate::{safe_proxy_session_id, sync_ext::MutexExt, HlsTerminalTailProtectionRemoval};
use arc_swap::ArcSwapOption;
use log::debug;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::{watch, Notify};
use tuliprox_session::ActiveUserManager;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct LeaseCompletionKey {
    pub(super) lease_id: HlsAccessLeaseId,
    pub(super) session_id: ProxySessionId,
    pub(super) issued_at_ms: u64,
    pub(super) snapshot_generation: Option<u64>,
}

#[derive(Default)]
pub(super) struct HlsLeaseCompletion {
    users: ArcSwapOption<ActiveUserManager>,
    users_bound: Notify,
    jobs: Mutex<HashMap<LeaseCompletionKey, watch::Receiver<Option<bool>>>>,
}

impl HlsLeaseCompletion {
    fn bind_users(&self, users: &Arc<ActiveUserManager>) {
        self.users.store(Some(Arc::clone(users)));
        self.users_bound.notify_waiters();
    }

    pub(super) async fn users(&self) -> Arc<ActiveUserManager> {
        loop {
            let notified = self.users_bound.notified();
            if let Some(users) = self.users.load_full() {
                return users;
            }
            // Before runtime attachment the lease remains its cleanup ticket;
            // an absent accounting owner must never silently skip the release.
            notified.await;
        }
    }

    pub(super) async fn run(
        self: &Arc<Self>,
        key: LeaseCompletionKey,
        completion: impl Future<Output = bool> + Send + 'static,
    ) -> bool {
        let mut receiver = {
            let mut jobs = self.jobs.lock_unpoisoned();
            if let Some(receiver) = jobs.get(&key) {
                receiver.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                jobs.insert(key.clone(), receiver.clone());
                let owner = Arc::clone(self);
                // Registered and spawned before the first await. Cancelling any
                // waiter cannot cancel or erase the runtime's completion job.
                tokio::spawn(async move {
                    let result = completion.await;
                    owner.jobs.lock_unpoisoned().remove(&key);
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };
        loop {
            if let Some(result) = *receiver.borrow_and_update() {
                return result;
            }
            if receiver.changed().await.is_err() {
                return false;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn pending_count(&self) -> usize { self.jobs.lock_unpoisoned().len() }
}

impl HlsProxyManager {
    /// Attach the server's authoritative user registry before serving leases.
    pub fn bind_lease_user_accounting(&self, users: &Arc<ActiveUserManager>) {
        self.lease_completion.bind_users(users);
    }

    #[cfg(test)]
    pub(crate) fn pending_lease_completions_for_test(&self) -> usize { self.lease_completion.pending_count() }
}

impl super::HlsLeaseRuntime {
    pub(super) async fn remove_prepared_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsAccessLeaseRemovalPreparation,
    ) -> bool {
        let key = LeaseCompletionKey {
            lease_id: lease_id.clone(),
            session_id: preparation.proxy_session_id.clone(),
            issued_at_ms: preparation.issued_at_ms,
            snapshot_generation: None,
        };
        let runtime = self.clone();
        let lease_id = lease_id.clone();
        let preparation = preparation.clone();
        self.lease_completion
            .run(key, async move { runtime.complete_prepared_lease(&lease_id, &preparation).await })
            .await
    }
}

impl super::HlsLeaseRuntime {
    pub(super) async fn complete_prepared_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        preparation: &crate::lease::HlsAccessLeaseRemovalPreparation,
    ) -> bool {
        // Hold the index and lease-store fences through external cleanup. A
        // replacement with the same ID must not acquire QoS/repair ownership
        // between removing the old incarnation and clearing those registries.
        // Reject stale tickets before waiting for runtime attachment.
        if !self.access_leases.read().await.prepare_access_lease_removal(lease_id).is_some_and(|current| {
            current.proxy_session_id == preparation.proxy_session_id && current.issued_at_ms == preparation.issued_at_ms
        }) {
            return false;
        }
        let users = self.lease_completion.users().await;
        let current = self.sessions.hold_current_proxy_session(&preparation.proxy_session_id).await;
        let mut leases = self.access_leases.write().await;
        let Some(lease) = leases.response_snapshot(
            lease_id,
            &preparation.proxy_session_id,
            tuliprox_core::utils::current_time_millis(),
        ) else {
            return false;
        };
        if lease.issued_at_ms != preparation.issued_at_ms {
            return false;
        }
        users.release_session_streams_and_counted_reservation(&lease.username, &lease.user_session_token).await;
        self.terminal_pending.cancel_lease(lease_id);
        self.terminal_commit_retries.cancel_lease(lease_id);
        if let Some(current) = current.as_ref() {
            let session = current.session();
            let mut session = session.write().await;
            if session
                .live_window_demands
                .get(lease_id)
                .is_some_and(|demand| demand.issued_at_ms == preparation.issued_at_ms)
            {
                session.live_window_demands.remove(lease_id);
            }
            let finalized_manifest_released =
                session.transient.release_finalized_manifest_generations(lease_id, preparation.issued_at_ms);
            if finalized_manifest_released {
                debug!(
                    "HLS finalized transient manifest generation released: lease={} proxy_session={} reason=lease_removed",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&preparation.proxy_session_id)
                );
            }
            if let Some(generation) = preparation.terminal_protection_generation {
                let removal = session.remove_terminal_tail_protection_after_lease_end(lease_id, generation);
                match removal {
                    HlsTerminalTailProtectionRemoval::Removed => {
                        debug!(
                            "HLS terminal-tail protection released: lease={} proxy_session={} generation={} reason=lease_removed",
                            super::super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0
                        );
                    }
                    HlsTerminalTailProtectionRemoval::Missing => {}
                    HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual } => {
                        debug!(
                            "HLS stale terminal-tail protection released: lease={} proxy_session={} expected_generation={} actual_generation={} reason=lease_removed",
                            super::super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0,
                            actual.0
                        );
                    }
                }
            } else if session.remove_terminal_tail_protection(lease_id).is_some() {
                debug!(
                    "HLS untracked terminal-tail protection released: lease={} proxy_session={} reason=lease_removed",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&preparation.proxy_session_id)
                );
            }
        }
        self.standalone_custom_access.remove(lease_id);
        self.segment_repair.remove_access_lease_window(lease_id).await;
        self.startup_observability.remove_access_lease(lease_id);
        self.qos.remove_access_lease(lease_id).await;
        // Keep the exact lease present until all release steps have finished.
        leases.remove_access_lease_if_preparation_matches(lease_id, preparation).is_some()
    }
}
