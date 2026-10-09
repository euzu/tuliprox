use super::{
    CacheAccessState, HlsAccessLeaseId, HlsPublishedTransientResourceIds, HlsTransientManifestTemplate,
    RetainedFinalizedManifestGeneration, TransientManifestGeneration, TransientManifestIdentity,
    TransientManifestLeaseBinding, TransientObjectCacheEntry, TransientObjectCacheKey, TransientPassthroughState,
    TransientResourceId, TransientResourceRef, ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES,
    ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES,
};
use std::{collections::HashSet, sync::Arc};
use tuliprox_parser::hls::origin_manifest::HlsManifestWindowPolicy;

impl TransientPassthroughState {
    #[cfg(test)]
    pub(crate) const fn manifest_generation(&self) -> u64 { self.manifest_generation }

    pub const fn current_finalized_manifest_generation(&self) -> Option<TransientManifestGeneration> {
        self.current_finalized_manifest_generation
    }

    pub const fn last_manifest_commit_identity(
        &self,
    ) -> Option<super::super::media_reserve::HlsManifestCommitIdentity> {
        self.last_manifest_commit_identity
    }

    pub(crate) fn record_manifest_commit_identity(
        &mut self,
        identity: super::super::media_reserve::HlsManifestCommitIdentity,
    ) {
        self.last_manifest_commit_identity = Some(identity);
    }

    pub(crate) const fn last_manifest_finalized(&self) -> bool { self.last_manifest_validity.is_finalized() }

    pub fn last_manifest_template(&self) -> Option<Arc<HlsTransientManifestTemplate>> {
        self.last_manifest_template.clone()
    }

    pub fn last_manifest_published_resource_ids(&self) -> HlsPublishedTransientResourceIds {
        HlsPublishedTransientResourceIds::from_shared(Arc::clone(&self.last_manifest_resource_ids))
    }

    pub(crate) fn merge_current_published_resource_ids(
        &self,
        previous: &HlsPublishedTransientResourceIds,
        next: HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> HlsPublishedTransientResourceIds {
        if Arc::ptr_eq(&previous.0, &next.0)
            || previous.0.iter().all(|resource_id| {
                next.contains(resource_id)
                    || !self
                        .resources
                        .get(resource_id)
                        .is_some_and(|resource| self.resource_is_valid_at(resource, now_ms))
            })
        {
            return next;
        }
        let mut merged = next.0.as_ref().clone();
        merged.extend(
            previous
                .0
                .iter()
                .filter(|resource_id| {
                    self.resources.get(*resource_id).is_some_and(|resource| self.resource_is_valid_at(resource, now_ms))
                })
                .cloned(),
        );
        HlsPublishedTransientResourceIds(Arc::new(merged))
    }

    pub const fn last_manifest_window_policy(&self) -> HlsManifestWindowPolicy {
        self.last_manifest_validity.window_policy()
    }

    pub(crate) const fn last_manifest_valid_until_ms(&self) -> Option<u64> {
        self.last_manifest_validity.valid_until_ms()
    }

    #[cfg(test)]
    pub(crate) fn current_manifest_resource_ids(&self) -> &HashSet<TransientResourceId> {
        self.last_manifest_resource_ids.as_ref()
    }

    pub(crate) fn has_finalized_manifest_generation(&self, generation: TransientManifestGeneration) -> bool {
        self.finalized_manifest_generations.contains_key(&generation)
    }

    #[cfg(test)]
    pub(crate) fn finalized_manifest_generation_count(&self) -> usize { self.finalized_manifest_generations.len() }

    #[cfg(test)]
    pub(crate) fn finalized_manifest_lease_binding_count(&self) -> usize {
        self.finalized_manifest_lease_bindings.len()
    }

    pub(crate) fn bind_finalized_manifest_generation(&mut self, binding: TransientManifestLeaseBinding) -> bool {
        if !self.finalized_manifest_generations.contains_key(&binding.manifest_generation) {
            return false;
        }
        self.finalized_manifest_lease_bindings.insert(binding);
        self.prune_unreferenced_finalized_manifest_generations();
        true
    }

    pub(crate) fn release_finalized_manifest_generations(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        lease_issued_at_ms: u64,
    ) -> bool {
        let previous_len = self.finalized_manifest_lease_bindings.len();
        self.finalized_manifest_lease_bindings
            .retain(|binding| binding.lease_id != *lease_id || binding.lease_issued_at_ms != lease_issued_at_ms);
        let released = self.finalized_manifest_lease_bindings.len() != previous_len;
        if !released {
            return false;
        }
        self.prune_unreferenced_finalized_manifest_generations();
        true
    }

    pub(crate) fn reconcile_finalized_manifest_lease_bindings(&mut self, bindings: &[TransientManifestLeaseBinding]) {
        self.finalized_manifest_lease_bindings = bindings
            .iter()
            .filter(|binding| self.finalized_manifest_generations.contains_key(&binding.manifest_generation))
            .cloned()
            .collect();
        self.prune_unreferenced_finalized_manifest_generations();
    }

    pub(super) fn prune_unreferenced_finalized_manifest_generations(&mut self) {
        let current_generation = self.current_finalized_manifest_generation();
        let referenced_generations = self
            .finalized_manifest_lease_bindings
            .iter()
            .map(|binding| binding.manifest_generation)
            .collect::<HashSet<_>>();
        let removed = self
            .finalized_manifest_generations
            .extract_if(|generation, _| {
                Some(*generation) != current_generation && !referenced_generations.contains(generation)
            })
            .map(|(_, generation)| generation)
            .collect::<Vec<_>>();
        for generation in removed {
            self.decrement_protected_resource_refcounts(&generation.resource_ids);
        }
    }

    pub(super) fn finalized_generation_for_identity(
        &self,
        identity: TransientManifestIdentity,
    ) -> Option<TransientManifestGeneration> {
        let generation = self.current_finalized_manifest_generation?;
        self.finalized_manifest_generations
            .get(&generation)
            .is_some_and(|retained| retained.identity == identity)
            .then_some(generation)
    }

    pub(super) fn generation_is_lease_referenced(&self, generation: TransientManifestGeneration) -> bool {
        self.finalized_manifest_lease_bindings.iter().any(|binding| binding.manifest_generation == generation)
    }

    pub(super) fn insert_finalized_manifest_generation(
        &mut self,
        generation: TransientManifestGeneration,
        retained: RetainedFinalizedManifestGeneration,
    ) {
        let resource_ids = Arc::clone(&retained.resource_ids);
        if let Some(previous) = self.finalized_manifest_generations.insert(generation, retained) {
            self.decrement_protected_resource_refcounts(&previous.resource_ids);
        }
        for resource_id in resource_ids.iter() {
            let refcount = self.protected_finalized_resource_refcounts.entry(resource_id.clone()).or_default();
            *refcount = refcount.saturating_add(1);
        }
    }

    pub(super) fn decrement_protected_resource_refcounts(&mut self, resource_ids: &HashSet<TransientResourceId>) {
        for resource_id in resource_ids {
            let remove = self.protected_finalized_resource_refcounts.get_mut(resource_id).is_some_and(|refcount| {
                *refcount = refcount.saturating_sub(1);
                *refcount == 0
            });
            if remove {
                self.protected_finalized_resource_refcounts.remove(resource_id);
            }
        }
    }
}

pub(super) fn estimated_transient_resource_entry_bytes(
    resource_id: &TransientResourceId,
    resource: &TransientResourceRef,
) -> usize {
    std::mem::size_of::<(TransientResourceId, TransientResourceRef)>()
        .saturating_add(resource_id.0.len())
        .saturating_add(resource.id.0.len())
        .saturating_add(resource.resolved_origin_uri.len())
        .saturating_add(resource.content_type_hint.as_ref().map_or(0, String::len))
        .saturating_add(resource.file_ext_hint.as_ref().map_or(0, String::len))
        .saturating_add(std::mem::size_of::<CacheAccessState>())
        .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES)
}

pub(super) fn estimated_transient_resource_id_set_bytes(resource_ids: &HashSet<TransientResourceId>) -> usize {
    resource_ids.iter().fold(0_usize, |total, resource_id| {
        total.saturating_add(
            std::mem::size_of::<TransientResourceId>()
                .saturating_add(resource_id.0.len())
                .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
        )
    })
}

pub(super) fn estimated_transient_object_entry_bytes(entry: &TransientObjectCacheEntry) -> usize {
    std::mem::size_of::<(TransientObjectCacheKey, TransientObjectCacheEntry)>()
        .saturating_add(entry.key.stable_value().len())
        .saturating_add(entry.content_type.len())
        .saturating_add(entry.binding.resource_id.0.len())
        .saturating_add(entry.binding.resolved_origin_uri.len())
        .saturating_add(entry.binding.file_extension.len())
        .saturating_add(std::mem::size_of::<CacheAccessState>())
        .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES)
}
