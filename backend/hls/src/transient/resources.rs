use super::{
    identity::TransientResourceValidityScope, HlsAccessLeaseId, HlsPublishedTransientResourceIds,
    TransientObjectCacheKey, TransientObjectCacheStatus, TransientObjectUnavailableState, TransientPassthroughState,
    TransientResourceId, TransientResourceRef, TransientResourceRevision,
};
use std::{collections::HashSet, sync::Arc};

impl TransientPassthroughState {
    pub fn upsert_resources<I>(&mut self, resources: I)
    where
        I: IntoIterator<Item = TransientResourceRef>,
    {
        for mut resource in resources {
            let preserve_revision = self
                .resources
                .get(&resource.id)
                .filter(|existing| existing.has_same_cache_identity(&resource))
                .map(|existing| existing.revision);
            resource.revision = if let Some(revision) = preserve_revision {
                revision
            } else {
                let revision = TransientResourceRevision(self.next_resource_revision);
                self.next_resource_revision = self.next_resource_revision.saturating_add(1);
                revision
            };
            match self.resources.get_mut(&resource.id) {
                Some(existing) => existing.refresh_from(resource),
                None => {
                    let _previous = self.resources.insert(resource.id.clone(), resource);
                }
            }
        }
    }

    pub(super) fn resource_is_valid_at_for(
        &self,
        resource: &TransientResourceRef,
        now_ms: u64,
        scope: TransientResourceValidityScope<'_>,
    ) -> bool {
        match scope {
            TransientResourceValidityScope::PublishedSession => {
                resource.is_valid_at(now_ms) || self.protected_finalized_resource_refcounts.contains_key(&resource.id)
            }
            TransientResourceValidityScope::AccessLease { lease_id, lease_issued_at_ms, published_resource_ids } => {
                (published_resource_ids.contains(&resource.id) && resource.is_valid_at(now_ms))
                    || self.finalized_manifest_lease_bindings.iter().any(|binding| {
                        binding.lease_id == *lease_id
                            && binding.lease_issued_at_ms == lease_issued_at_ms
                            && self
                                .finalized_manifest_generations
                                .get(&binding.manifest_generation)
                                .is_some_and(|generation| generation.resource_ids.contains(&resource.id))
                    })
            }
        }
    }

    pub(crate) fn resource_is_valid_at(&self, resource: &TransientResourceRef, now_ms: u64) -> bool {
        self.resource_is_valid_at_for(resource, now_ms, TransientResourceValidityScope::PublishedSession)
    }

    pub(crate) fn resource_matches_current(&self, resource: &TransientResourceRef, now_ms: u64) -> bool {
        self.resources.get(&resource.id).is_some_and(|current| {
            Arc::ptr_eq(&current.access, &resource.access)
                && current.has_same_cache_identity(resource)
                && self.resource_is_valid_at(current, now_ms)
        })
    }

    pub fn resolve_current_resource(
        &self,
        resource_id: &TransientResourceId,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.resources.get(resource_id).filter(|resource| self.resource_is_valid_at(resource, now_ms)).cloned()
    }

    pub(crate) fn resolve_resource_for_lease(
        &self,
        resource_id: &TransientResourceId,
        lease_id: &HlsAccessLeaseId,
        lease_issued_at_ms: u64,
        published_resource_ids: &HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.resources
            .get(resource_id)
            .filter(|resource| {
                self.resource_is_valid_at_for(
                    resource,
                    now_ms,
                    TransientResourceValidityScope::AccessLease {
                        lease_id,
                        lease_issued_at_ms,
                        published_resource_ids,
                    },
                )
            })
            .cloned()
    }

    pub fn get_valid_resource(
        &mut self,
        resource_id: &TransientResourceId,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.prune_expired(now_ms);
        self.resolve_current_resource(resource_id, now_ms)
    }

    pub fn prune_expired(&mut self, now_ms: u64) { self.prune_expired_except(now_ms, &HashSet::new()); }

    pub fn prune_expired_except(&mut self, now_ms: u64, protected: &HashSet<TransientResourceId>) {
        let expired_resource_ids = self
            .resources
            .iter()
            .filter(|(id, resource)| {
                !protected.contains(*id)
                    && !self.resource_is_valid_at(resource, now_ms)
                    && resource.active_readers() == 0
            })
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        self.resources.retain(|id, _| !expired_resource_ids.contains(id));
    }

    pub fn active_resource_readers(&self) -> u32 {
        self.resources.values().map(TransientResourceRef::active_readers).sum()
    }

    pub fn object_unavailable_state(
        &self,
        key: &TransientObjectCacheKey,
        now_ms: u64,
    ) -> TransientObjectUnavailableState {
        let Some(entry) = self.object_cache.get(key) else {
            return TransientObjectUnavailableState::Missing;
        };
        if entry.expires_at_ms < now_ms || !self.binding_is_current(&entry.binding, now_ms) {
            return TransientObjectUnavailableState::Missing;
        }
        match entry.status {
            TransientObjectCacheStatus::Fetching { .. } => TransientObjectUnavailableState::Fetching,
            TransientObjectCacheStatus::FailedRetryable { retry_after_ms, .. } => {
                TransientObjectUnavailableState::FailedRetryable { retry_after_ms }
            }
            TransientObjectCacheStatus::FailedPermanent { .. } => TransientObjectUnavailableState::FailedPermanent,
            TransientObjectCacheStatus::Ready { .. } => TransientObjectUnavailableState::Missing,
        }
    }
}
