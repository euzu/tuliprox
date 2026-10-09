use super::{
    AcquireProviderParams, ActiveProviderManager, ActiveProviderManagerCore, CapacityReleases, Connections,
    ProviderCapacityNotifier,
};
use crate::provider_leases::ProviderLeaseTable;
use ::shared::utils::sanitize_sensitive_info;
use log::error;
use std::sync::{Arc, RwLockReadGuard, RwLockWriteGuard};
use tuliprox_core::{model::PlaybackSelectionReason, utils::debug_if_enabled};

/// Write access to a capacity table that wakes the waiters of freed providers after release.
///
/// Fields drop in declaration order: `lock` stages the freed providers while it still holds the
/// lock and then releases it; only afterwards does `_flush` wake the waiters.
pub(super) struct CapacityWriteGuard<'a, T: CapacityReleases> {
    lock: StagingWriteGuard<'a, T>,
    _flush: FlushCapacityWaiters<'a>,
}

struct StagingWriteGuard<'a, T: CapacityReleases> {
    guard: RwLockWriteGuard<'a, T>,
    notifier: &'a ProviderCapacityNotifier,
}

impl<T: CapacityReleases> Drop for StagingWriteGuard<'_, T> {
    fn drop(&mut self) { self.notifier.stage(self.guard.take_released_providers()); }
}

struct FlushCapacityWaiters<'a>(&'a ProviderCapacityNotifier);

impl Drop for FlushCapacityWaiters<'_> {
    fn drop(&mut self) { self.0.flush(); }
}

impl<'a, T: CapacityReleases> CapacityWriteGuard<'a, T> {
    pub(super) fn new(guard: RwLockWriteGuard<'a, T>, notifier: &'a ProviderCapacityNotifier) -> Self {
        Self { lock: StagingWriteGuard { guard, notifier }, _flush: FlushCapacityWaiters(notifier) }
    }
}

impl<T: CapacityReleases> std::ops::Deref for CapacityWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target { &self.lock.guard }
}

impl<T: CapacityReleases> std::ops::DerefMut for CapacityWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.lock.guard }
}

impl ActiveProviderManagerCore {
    /// Notification fired whenever `provider` frees a slot or ends a lease.
    pub fn provider_capacity_notify(&self, provider: &Arc<str>) -> Arc<tokio::sync::Notify> {
        self.capacity.subscribe(provider)
    }

    /// Serialises capacity transitions (acquire/release/reclassify) so a reclassify
    /// cannot race an acquire or release.
    ///
    /// Lock-order invariant: this lock must be acquired **before** the connections
    /// lock (`read_connections`/`write_connections`). No path may hold the
    /// connections lock and then call this; doing so can deadlock.
    pub(super) fn lock_capacity_transition(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.capacity_transition.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider capacity transition lock");
                poisoned.into_inner()
            }
        }
    }

    pub(super) fn read_connections(&self) -> RwLockReadGuard<'_, Connections> {
        match self.connections.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider connection lock");
                poisoned.into_inner()
            }
        }
    }

    pub(super) fn write_connections(&self) -> CapacityWriteGuard<'_, Connections> {
        let guard = match self.connections.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned active-provider connection lock");
                poisoned.into_inner()
            }
        };
        CapacityWriteGuard::new(guard, &self.capacity)
    }

    pub(super) fn write_leases(&self) -> CapacityWriteGuard<'_, ProviderLeaseTable> {
        let guard = match self.leases.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned provider-lease lock");
                poisoned.into_inner()
            }
        };
        CapacityWriteGuard::new(guard, &self.capacity)
    }

    pub(super) fn read_leases(&self) -> RwLockReadGuard<'_, ProviderLeaseTable> {
        match self.leases.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Recovering poisoned provider-lease lock");
                poisoned.into_inner()
            }
        }
    }
}

impl ActiveProviderManager {
    pub(super) fn reservation_capacity_usage(
        &self,
        provider_name: &Arc<str>,
        session_owner: Option<&str>,
    ) -> Option<(usize, usize, usize)> {
        let (current_connections, max_connections) = self.providers.provider_capacity(provider_name)?;
        if max_connections == 0 {
            return Some((current_connections, max_connections, 0));
        }
        // Without any foreign reserving lease the owner analysis cannot change the result.
        if !self.read_leases().has_foreign_reserved_lease(provider_name, session_owner) {
            return Some((current_connections, max_connections, 0));
        }
        let counted_owners = self.active_reservation_owners(provider_name);
        let mut leases = self.write_leases();
        Self::prune_expired_leases(&mut leases);
        let idle_foreign_reservations = leases.foreign_reserved_slots(provider_name, session_owner, &counted_owners);

        Some((current_connections, max_connections, idle_foreign_reservations))
    }

    /// Returns true when an already-counted candidate allocation consumed a slot kept for an idle reservation.
    pub(super) fn exceeds_reserved_capacity(&self, provider_name: &Arc<str>, session_owner: Option<&str>) -> bool {
        let Some((current_connections, max_connections, idle_foreign_reservations)) =
            self.reservation_capacity_usage(provider_name, session_owner)
        else {
            return true;
        };
        max_connections > 0
            && idle_foreign_reservations > 0
            && current_connections.saturating_add(idle_foreign_reservations) > max_connections
    }

    /// Explains a reservation skip with the authoritative slot breakdown instead of
    /// only the transport socket, so the decision can be audited from the logs.
    pub(super) fn log_reserved_capacity_skip(&self, provider_name: &Arc<str>, params: &AcquireProviderParams<'_>) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let Some((current_connections, max_connections, foreign_reserved)) =
            self.reservation_capacity_usage(provider_name, params.session_owner())
        else {
            debug_if_enabled!(
                "Skipping reserved provider {} (reason={}, capacity=unknown, peer_addr={}, request_id={}, playback_kind={})",
                sanitize_sensitive_info(provider_name),
                PlaybackSelectionReason::ReservedCapacity,
                sanitize_sensitive_info(&params.addr.to_string()),
                params.lease.map_or_else(String::new, |lease| lease.request_id.to_string()),
                params.lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string())
            );
            return;
        };
        let usage = self.provider_lease_usage(provider_name);
        debug_if_enabled!(
            "Skipping reserved provider {} (reason={}, current={}, max={}, foreign_reserved={}, active_slots={}, starting_slots={}, idle_slots={}, peer_addr={}, request_id={}, playback_kind={})",
            sanitize_sensitive_info(provider_name),
            PlaybackSelectionReason::ReservedCapacity,
            current_connections,
            max_connections,
            foreign_reserved,
            usage.active,
            usage.starting,
            usage.idle,
            sanitize_sensitive_info(&params.addr.to_string()),
            params.lease.map_or_else(String::new, |lease| lease.request_id.to_string()),
            params.lease.map_or_else(|| "-".to_string(), |lease| lease.kind.to_string())
        );
    }

    pub fn is_over_limit(&self, provider_name: &Arc<str>) -> bool { self.providers.is_over_limit(provider_name) }
}
