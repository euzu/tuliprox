use super::{
    ensure_manifest_limit, extend_hls_transient_manifest_template, extract_transient_resource_ids,
    generations::{
        estimated_transient_object_entry_bytes, estimated_transient_resource_entry_bytes,
        estimated_transient_resource_id_set_bytes,
    },
    identity::{complete_manifest_append_boundary, transient_manifest_identity, CommittedTransientManifestValidity},
    parse_hls_transient_manifest_template, HlsManifestLimitKind, HlsManifestLimitViolation,
    HlsTransientManifestCommitError, HlsTransientManifestFootprint, HlsTransientManifestTemplate,
    RetainedFinalizedManifestGeneration, RollingEventAppendContext, RollingEventRewriteSeed,
    TransientManifestFootprintCandidate, TransientManifestGeneration, TransientManifestIdentity,
    TransientManifestLeaseBinding, TransientManifestReplacement, TransientObjectCacheKey, TransientPassthroughState,
    TransientResourceId, TransientResourceRef, ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES,
    ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES, MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
    MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS, MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
    MAX_TRANSIENT_MANIFEST_RESOURCES, MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
    MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION, MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
};
use crate::transient_manifest::TransientRewriteCheckpoint;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::Notify;
use tuliprox_parser::hls::origin_manifest::{HlsManifestLifecycle, HlsManifestWindowPolicy, ParsedManifestSemantics};

impl TransientPassthroughState {
    pub fn new(resource_ttl_ms: u64) -> Self {
        Self {
            resources: HashMap::new(),
            object_cache: HashMap::new(),
            object_fetch_notifiers: HashMap::new(),
            next_resource_revision: 1,
            next_object_fetch_generation: 0,
            manifest_generation: 0,
            last_manifest_commit_identity: None,
            current_finalized_manifest_generation: None,
            last_manifest_validity: CommittedTransientManifestValidity::Rolling {
                valid_until_ms: None,
                window_policy: HlsManifestWindowPolicy::ApplyLiveWindow,
            },
            last_manifest_resource_ids: Arc::new(HashSet::new()),
            last_manifest_template: None,
            rolling_event_rewrite_seed: None,
            finalized_manifest_generations: HashMap::new(),
            finalized_manifest_lease_bindings: HashSet::new(),
            protected_finalized_resource_refcounts: HashMap::new(),
            last_manifest_body: None,
            last_manifest_rendered_at_ms: None,
            last_manifest_playlist_duration_ms: None,
            resource_ttl_ms,
        }
    }

    pub fn set_resource_ttl_ms(&mut self, resource_ttl_ms: u64) { self.resource_ttl_ms = resource_ttl_ms; }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_manifest_with_semantics(
        &mut self,
        body: String,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
    ) {
        let semantics = tuliprox_parser::hls::origin_manifest::parse_manifest_semantics(&body);
        if self.replace_manifest_common(body, rendered_at_ms, playlist_duration_ms, semantics).is_ok() {
            self.last_manifest_commit_identity =
                Some(super::super::media_reserve::HlsManifestCommitIdentity::committed(
                    self.manifest_generation,
                    rendered_at_ms,
                ));
        }
    }

    pub(crate) fn commit_rewritten_manifest_with_semantics(
        &mut self,
        body: String,
        resources: Vec<TransientResourceRef>,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
        semantics: ParsedManifestSemantics,
    ) -> Result<HlsTransientManifestFootprint, HlsTransientManifestCommitError> {
        let manifest_resource_ids = Arc::new(extract_transient_resource_ids(&body));
        let manifest_identity = transient_manifest_identity(&body, &manifest_resource_ids);
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        self.ensure_manifest_generation_available(reusable_finalized_generation)?;
        let resource_count = resources.len().max(manifest_resource_ids.len());
        Self::validate_manifest_shape(body.len(), resource_count)?;
        let template = if let Some(generation) = reusable_finalized_generation {
            self.finalized_manifest_generations.get(&generation).and_then(|retained| retained.template.clone())
        } else {
            parse_hls_transient_manifest_template(&body)?
        }
        .ok_or(HlsTransientManifestCommitError::SnapshotUnavailable)?;
        let footprint = self.validate_manifest_footprint(&TransientManifestFootprintCandidate {
            rewritten_bytes: body.len(),
            resource_count,
            resources: &resources,
            lifecycle: semantics.lifecycle(),
            resource_ids: &manifest_resource_ids,
            reusable_finalized_generation,
            template: Some(&template),
        })?;
        self.upsert_resources(resources);
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template: Some(template),
        })?;
        Ok(footprint)
    }

    pub(crate) fn commit_incremental_rewritten_manifest_with_semantics(
        &mut self,
        body: String,
        appended_resources: Vec<TransientResourceRef>,
        template: Arc<HlsTransientManifestTemplate>,
        rendered_at_ms: u64,
        resource_ttl_ms: u64,
        semantics: ParsedManifestSemantics,
    ) -> Result<HlsTransientManifestFootprint, HlsManifestLimitViolation> {
        self.ensure_manifest_generation_available(None)?;
        let mut manifest_resource_ids = std::mem::take(&mut self.last_manifest_resource_ids);
        let mut inserted_resource_ids = Vec::new();
        for resource in &appended_resources {
            if Arc::make_mut(&mut manifest_resource_ids).insert(resource.id.clone()) {
                inserted_resource_ids.push(resource.id.clone());
            }
        }
        let manifest_identity = if semantics.lifecycle().is_finalized() {
            transient_manifest_identity(&body, &manifest_resource_ids)
        } else {
            TransientManifestIdentity::default()
        };
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        let footprint = Self::validate_manifest_shape(body.len(), manifest_resource_ids.len()).and_then(|()| {
            self.validate_manifest_footprint(&TransientManifestFootprintCandidate {
                rewritten_bytes: body.len(),
                resource_count: manifest_resource_ids.len(),
                resources: &appended_resources,
                lifecycle: semantics.lifecycle(),
                resource_ids: &manifest_resource_ids,
                reusable_finalized_generation,
                template: Some(&template),
            })
        });
        let footprint = match footprint {
            Ok(footprint) => footprint,
            Err(violation) => {
                let resource_ids = Arc::make_mut(&mut manifest_resource_ids);
                for resource_id in inserted_resource_ids {
                    resource_ids.remove(&resource_id);
                }
                self.last_manifest_resource_ids = manifest_resource_ids;
                return Err(violation);
            }
        };
        self.upsert_resources(appended_resources);
        let expires_at_ms = rendered_at_ms.saturating_add(resource_ttl_ms);
        for resource_id in manifest_resource_ids.iter() {
            if let Some(resource) = self.resources.get_mut(resource_id) {
                resource.expires_at_ms = expires_at_ms;
            }
        }
        let playlist_duration_ms = Some(template.playlist_duration_ms());
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template: Some(template),
        })?;
        Ok(footprint)
    }

    pub(crate) fn rolling_event_append_context(
        &self,
        origin_body: &str,
        final_manifest_url: &str,
        rewrite_secret: &[u8],
    ) -> Option<RollingEventAppendContext> {
        let seed = self.rolling_event_rewrite_seed.as_ref()?;
        if seed.final_manifest_url_hash != *blake3::hash(final_manifest_url.as_bytes()).as_bytes()
            || seed.rewrite_secret_hash != *blake3::hash(rewrite_secret).as_bytes()
            || !complete_manifest_append_boundary(&seed.origin_body)
            || origin_body.len() <= seed.origin_body.len()
            || !origin_body.starts_with(seed.origin_body.as_ref())
        {
            return None;
        }
        Some(RollingEventAppendContext {
            suffix_offset: seed.origin_body.len(),
            rewritten_prefix: Arc::clone(&seed.rewritten_body),
            template: Arc::clone(&seed.template),
            checkpoint: seed.checkpoint.clone(),
        })
    }

    pub(crate) fn extend_rolling_event_template(
        previous: &Arc<HlsTransientManifestTemplate>,
        rewritten_suffix: &str,
    ) -> Result<Option<Arc<HlsTransientManifestTemplate>>, HlsManifestLimitViolation> {
        extend_hls_transient_manifest_template(previous, rewritten_suffix)
    }

    pub(crate) fn record_rolling_event_rewrite_seed(
        &mut self,
        origin_body: &str,
        final_manifest_url: &str,
        rewrite_secret: &[u8],
        checkpoint: TransientRewriteCheckpoint,
        retained_metadata_bytes: usize,
        eligible: bool,
    ) {
        let retained_with_origin_body = retained_metadata_bytes
            .saturating_add(origin_body.len())
            .saturating_add(std::mem::size_of::<RollingEventRewriteSeed>())
            .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES);
        if !eligible || retained_with_origin_body > MAX_ESTIMATED_TRANSIENT_METADATA_BYTES {
            self.rolling_event_rewrite_seed = None;
            return;
        }
        let (Some(rewritten_body), Some(template)) = (&self.last_manifest_body, &self.last_manifest_template) else {
            self.rolling_event_rewrite_seed = None;
            return;
        };
        self.rolling_event_rewrite_seed = Some(RollingEventRewriteSeed {
            origin_body: Arc::from(origin_body),
            final_manifest_url_hash: *blake3::hash(final_manifest_url.as_bytes()).as_bytes(),
            rewrite_secret_hash: *blake3::hash(rewrite_secret).as_bytes(),
            rewritten_body: Arc::clone(rewritten_body),
            template: Arc::clone(template),
            checkpoint,
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn replace_manifest_common(
        &mut self,
        body: String,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
        semantics: ParsedManifestSemantics,
    ) -> Result<(), HlsManifestLimitViolation> {
        let manifest_resource_ids = Arc::new(extract_transient_resource_ids(&body));
        let manifest_identity = transient_manifest_identity(&body, &manifest_resource_ids);
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        let template = parse_hls_transient_manifest_template(&body).ok().flatten();
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template,
        })
    }

    pub(super) fn replace_manifest_common_with_resource_ids(
        &mut self,
        replacement: TransientManifestReplacement,
    ) -> Result<(), HlsManifestLimitViolation> {
        let TransientManifestReplacement {
            body,
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids,
            identity,
            reusable_finalized_generation,
            template,
        } = replacement;
        self.ensure_manifest_generation_available(reusable_finalized_generation)?;
        let manifest_validity =
            CommittedTransientManifestValidity::from_semantics(semantics, rendered_at_ms, playlist_duration_ms);
        let manifest_generation = if let Some(generation) = reusable_finalized_generation {
            generation
        } else {
            self.manifest_generation = self.manifest_generation.saturating_add(1);
            TransientManifestGeneration(self.manifest_generation)
        };
        if reusable_finalized_generation.is_none() {
            self.last_manifest_resource_ids = Arc::clone(&resource_ids);
            self.last_manifest_template.clone_from(&template);
        }
        self.last_manifest_validity = manifest_validity;
        if manifest_validity.is_finalized() {
            self.current_finalized_manifest_generation = Some(manifest_generation);
            if reusable_finalized_generation.is_none() {
                self.insert_finalized_manifest_generation(
                    manifest_generation,
                    RetainedFinalizedManifestGeneration { identity, resource_ids: Arc::clone(&resource_ids), template },
                );
            }
        } else {
            self.current_finalized_manifest_generation = None;
        }
        self.prune_unreferenced_finalized_manifest_generations();
        if reusable_finalized_generation.is_none() {
            self.last_manifest_body = Some(body);
        }
        self.last_manifest_rendered_at_ms = Some(rendered_at_ms);
        self.last_manifest_playlist_duration_ms = playlist_duration_ms;
        Ok(())
    }

    pub(super) fn validate_manifest_footprint(
        &self,
        candidate: &TransientManifestFootprintCandidate<'_>,
    ) -> Result<HlsTransientManifestFootprint, HlsManifestLimitViolation> {
        let origin_uri_bytes = self.prospective_origin_uri_bytes(candidate.resources);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientOriginUriBytes,
            origin_uri_bytes,
            MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
        )?;
        let resource_entries = self.prospective_resource_entry_count(candidate.resources);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientResourceEntries,
            resource_entries,
            MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION,
        )?;
        let prospective_generation_sets = self.prospective_finalized_generation_sets(
            candidate.lifecycle,
            candidate.resource_ids,
            candidate.reusable_finalized_generation,
        );
        let retained_finalized_generations = prospective_generation_sets.len();
        ensure_manifest_limit(
            HlsManifestLimitKind::FinalizedGenerations,
            retained_finalized_generations,
            MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS,
        )?;
        let finalized_generation_memberships = prospective_generation_sets
            .iter()
            .fold(0_usize, |total, resource_ids| total.saturating_add(resource_ids.len()));
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientGenerationMemberships,
            finalized_generation_memberships,
            MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
        )?;
        let estimated_metadata_bytes =
            self.prospective_estimated_metadata_bytes(candidate, &prospective_generation_sets);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientEstimatedMetadataBytes,
            estimated_metadata_bytes,
            MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
        )?;
        Ok(HlsTransientManifestFootprint {
            resources: candidate.resource_count,
            resource_entries,
            rewritten_bytes: candidate.rewritten_bytes,
            origin_uri_bytes,
            retained_finalized_generations,
            finalized_generation_memberships,
            estimated_metadata_bytes,
        })
    }

    pub(super) fn validate_manifest_shape(
        rewritten_bytes: usize,
        resource_count: usize,
    ) -> Result<(), HlsManifestLimitViolation> {
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientResources,
            resource_count,
            MAX_TRANSIENT_MANIFEST_RESOURCES,
        )?;
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientRewrittenBytes,
            rewritten_bytes,
            MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
        )
    }

    pub(super) fn ensure_manifest_generation_available(
        &self,
        reusable_generation: Option<TransientManifestGeneration>,
    ) -> Result<(), HlsManifestLimitViolation> {
        if reusable_generation.is_none() && self.manifest_generation == u64::MAX {
            return Err(HlsManifestLimitViolation::new(
                HlsManifestLimitKind::ManifestCommitGeneration,
                usize::MAX,
                usize::MAX - 1,
            ));
        }
        Ok(())
    }

    pub(super) fn prospective_origin_uri_bytes(&self, resources: &[TransientResourceRef]) -> usize {
        let incoming_uri_bytes = resources
            .iter()
            .map(|resource| (&resource.id, resource.resolved_origin_uri.len()))
            .collect::<HashMap<_, _>>();
        let retained_uri_bytes = self
            .resources
            .iter()
            .filter(|(resource_id, _)| !incoming_uri_bytes.contains_key(resource_id))
            .fold(0_usize, |total, (_, resource)| total.saturating_add(resource.resolved_origin_uri.len()));
        incoming_uri_bytes.values().fold(retained_uri_bytes, |total, uri_bytes| total.saturating_add(*uri_bytes))
    }

    pub(super) fn prospective_resource_entry_count(&self, resources: &[TransientResourceRef]) -> usize {
        let incoming_ids = resources.iter().map(|resource| &resource.id).collect::<HashSet<_>>();
        incoming_ids.iter().fold(self.resources.len(), |count, resource_id| {
            count.saturating_add(usize::from(!self.resources.contains_key(*resource_id)))
        })
    }

    pub(super) fn prospective_finalized_generation_sets<'a>(
        &'a self,
        lifecycle: HlsManifestLifecycle,
        candidate_resource_ids: &'a HashSet<TransientResourceId>,
        reusable_generation: Option<TransientManifestGeneration>,
    ) -> Vec<&'a HashSet<TransientResourceId>> {
        let mut sets = self
            .finalized_manifest_generations
            .iter()
            .filter(|(generation, _)| {
                Some(**generation) == reusable_generation || self.generation_is_lease_referenced(**generation)
            })
            .map(|(_, generation)| generation.resource_ids.as_ref())
            .collect::<Vec<_>>();
        if lifecycle.is_finalized() && reusable_generation.is_none() {
            sets.push(candidate_resource_ids);
        }
        sets
    }

    pub(super) fn prospective_estimated_metadata_bytes(
        &self,
        candidate: &TransientManifestFootprintCandidate<'_>,
        generation_sets: &[&HashSet<TransientResourceId>],
    ) -> usize {
        let incoming = candidate.resources.iter().map(|resource| (&resource.id, resource)).collect::<HashMap<_, _>>();
        let retained_resource_bytes = self
            .resources
            .iter()
            .filter(|(resource_id, _)| !incoming.contains_key(resource_id))
            .fold(0_usize, |total, (resource_id, resource)| {
                total.saturating_add(estimated_transient_resource_entry_bytes(resource_id, resource))
            });
        let resource_bytes = incoming.values().fold(retained_resource_bytes, |total, resource| {
            total.saturating_add(estimated_transient_resource_entry_bytes(&resource.id, resource))
        });
        let generation_bytes = generation_sets.iter().fold(0_usize, |total, resource_ids| {
            total
                .saturating_add(std::mem::size_of::<RetainedFinalizedManifestGeneration>())
                .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
                .saturating_add(estimated_transient_resource_id_set_bytes(resource_ids))
        });
        let rolling_manifest_resource_bytes = if candidate.lifecycle.is_finalized() {
            0
        } else {
            estimated_transient_resource_id_set_bytes(candidate.resource_ids)
        };
        let retained_template_bytes = self
            .finalized_manifest_generations
            .iter()
            .filter(|(generation, _)| {
                Some(**generation) == candidate.reusable_finalized_generation
                    || self.generation_is_lease_referenced(**generation)
            })
            .filter_map(|(_, generation)| generation.template.as_ref())
            .fold(0_usize, |total, template| total.saturating_add(template.estimated_metadata_bytes()));
        let candidate_template_bytes =
            if candidate.lifecycle.is_finalized() && candidate.reusable_finalized_generation.is_some() {
                0
            } else {
                candidate.template.map_or(0, HlsTransientManifestTemplate::estimated_metadata_bytes)
            };
        let mut protected_ids = HashSet::new();
        for resource_ids in generation_sets {
            protected_ids.extend(resource_ids.iter());
        }
        let protected_index_bytes = protected_ids.iter().fold(0_usize, |total, resource_id| {
            total.saturating_add(
                std::mem::size_of::<(TransientResourceId, u16)>()
                    .saturating_add(resource_id.0.len())
                    .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
            )
        });
        let binding_bytes = self.finalized_manifest_lease_bindings.iter().fold(0_usize, |total, binding| {
            total.saturating_add(
                std::mem::size_of::<TransientManifestLeaseBinding>()
                    .saturating_add(binding.lease_id.0.len())
                    .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
            )
        });
        let object_bytes = self
            .object_cache
            .values()
            .fold(0_usize, |total, entry| total.saturating_add(estimated_transient_object_entry_bytes(entry)));
        candidate
            .rewritten_bytes
            .saturating_add(resource_bytes)
            .saturating_add(generation_bytes)
            .saturating_add(rolling_manifest_resource_bytes)
            .saturating_add(retained_template_bytes)
            .saturating_add(candidate_template_bytes)
            .saturating_add(protected_index_bytes)
            .saturating_add(binding_bytes)
            .saturating_add(object_bytes)
            .saturating_add(
                self.object_fetch_notifiers.len().saturating_mul(
                    std::mem::size_of::<(TransientObjectCacheKey, Arc<Notify>)>()
                        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
                ),
            )
    }
}
