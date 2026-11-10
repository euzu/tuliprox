use super::{AcquireProviderParams, ActiveProviderManager, ConnectionKind, OwnerProviderPin, PlaybackLeaseRef};
pub use shared::model::CapacityTier;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tuliprox_core::model::{AllocationId, ProviderAllocation, ProviderHandle};

impl ActiveProviderManager {
    pub(super) fn probe_connection_candidate(
        &self,
        provider_or_input_name: &Arc<str>,
        allow_grace: bool,
        session_owner: Option<&str>,
    ) -> ProviderAllocation {
        if let Some(owner) = session_owner {
            match self.owner_provider_pin(provider_or_input_name, owner) {
                Some(OwnerProviderPin::Reserved(reserved_provider)) => {
                    let allocation =
                        self.providers.probe_exact_connection_with_grace_override(&reserved_provider, allow_grace);
                    if matches!(allocation, ProviderAllocation::Exhausted)
                        || self.exceeds_reserved_capacity(&reserved_provider, Some(owner))
                    {
                        return ProviderAllocation::Exhausted;
                    }
                    return allocation;
                }
                Some(OwnerProviderPin::Preferred(affinity_provider)) => {
                    let allocation =
                        self.providers.probe_exact_connection_with_grace_override(&affinity_provider, false);
                    if !matches!(allocation, ProviderAllocation::Exhausted)
                        && !self.exceeds_reserved_capacity(&affinity_provider, Some(owner))
                    {
                        return allocation;
                    }
                }
                None => {}
            }
        }

        let candidate_count = self.providers.provider_names_for_input(provider_or_input_name).len();
        let attempts = candidate_count.max(1);
        let mut skipped_reserved = HashSet::new();
        for _ in 0..attempts {
            let allocation = self.providers.probe_connection_with_grace_override_excluding(
                provider_or_input_name,
                allow_grace,
                &skipped_reserved,
            );
            if matches!(allocation, ProviderAllocation::Exhausted) {
                break;
            }
            if let Some(provider_name) = allocation.get_provider_name() {
                if self.exceeds_reserved_capacity(&provider_name, session_owner) {
                    skipped_reserved.insert(provider_name);
                    if skipped_reserved.len() >= attempts {
                        break;
                    }
                    continue;
                }
            }
            return allocation;
        }

        ProviderAllocation::Exhausted
    }

    pub fn capacity_probe(
        &self,
        input_name: &Arc<str>,
        priority: i8,
        kind: ConnectionKind,
        session_owner: Option<&str>,
        own_releasing: &[AllocationId],
    ) -> CapacityTier {
        let free_probe = self.probe_connection_candidate(input_name, false, session_owner);
        if matches!(free_probe, ProviderAllocation::Available(_)) {
            return CapacityTier::Free;
        }

        if !own_releasing.is_empty() {
            let connections = self.read_connections();
            let has_own_release = own_releasing.iter().any(|alloc_id| {
                if let Some(info) = connections.single.get(alloc_id) {
                    if let Some(prov_name) = info.allocation.get_provider_name() {
                        if self.providers.is_provider_for_input(&prov_name, input_name) || &prov_name == input_name {
                            return true;
                        }
                    }
                }
                if let Some(key) = connections.shared.shared_by_allocation_id.get(alloc_id) {
                    if let Some(shared) = connections.shared.by_key.get(key) {
                        if let Some(prov_name) = shared.allocation.get_provider_name() {
                            if self.providers.is_provider_for_input(&prov_name, input_name) || &prov_name == input_name
                            {
                                return true;
                            }
                        }
                    }
                }
                false
            });
            if has_own_release {
                return CapacityTier::OwnRelease;
            }
        }

        let grace_probe = self.probe_connection_candidate(input_name, true, session_owner);
        if matches!(grace_probe, ProviderAllocation::Available(_) | ProviderAllocation::GracePeriod(_)) {
            return CapacityTier::Grace;
        }

        let reserved_providers = self.reserved_provider_names_for_other(input_name, session_owner);
        let has_preempt_candidate = {
            let connections = self.read_connections();
            self.select_preemption_candidate(&connections, input_name, priority, kind, &reserved_providers).is_some()
        };
        if has_preempt_candidate {
            return CapacityTier::Preempt;
        }

        CapacityTier::Exhausted
    }

    pub fn try_acquire_connection_no_preempt(
        &self,
        input_name: &Arc<str>,
        allow_grace: bool,
        addr: &SocketAddr,
        priority: i8,
        kind: ConnectionKind,
        lease: Option<PlaybackLeaseRef<'_>>,
    ) -> Option<ProviderHandle> {
        let params = AcquireProviderParams { addr, priority, kind, lease };
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let _transition = self.lock_capacity_transition();
        if self.is_shutting_down.load(Ordering::Acquire) {
            return None;
        }
        self.acquire_connection_inner_no_preempt(input_name, allow_grace, &params)
    }
}
