use super::{
    commit_lease_revision_bindings, lease_state_allows_use, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsAvailabilityEvidenceAdvanceError, HlsAvailabilityEvidenceGeneration,
    HlsLeaseManifestPublicationGuard, HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason,
    HlsLeaseManifestSnapshot, HlsLeasePlaybackMode, HlsLeaseStartupAdmissionState, HlsPublishedTransientResourceIds,
    ProxySessionId,
};

impl HlsAccessLease {
    pub fn published_transient_resource_ids(&self) -> &HlsPublishedTransientResourceIds {
        &self.published_transient_resource_ids
    }

    fn manifest_publication_guard(&self) -> Option<HlsLeaseManifestPublicationGuard> {
        (lease_state_allows_use(self.state) && self.playback_mode == HlsLeasePlaybackMode::Live).then_some(
            HlsLeaseManifestPublicationGuard {
                issued_at_ms: self.issued_at_ms,
                admission_generation: self.admission_generation,
            },
        )
    }
}

impl HlsAccessLeaseStore {
    pub fn availability_evidence_generation(
        &self,
        proxy_session_id: &ProxySessionId,
    ) -> HlsAvailabilityEvidenceGeneration {
        self.availability_generation_by_proxy_session
            .get(proxy_session_id)
            .copied()
            .unwrap_or(HlsAvailabilityEvidenceGeneration::NONE)
    }

    pub(super) fn availability_evidence_can_advance(&self) -> Result<(), HlsAvailabilityEvidenceAdvanceError> {
        self.last_availability_evidence_generation
            .0
            .checked_add(1)
            .map(|_| ())
            .ok_or(HlsAvailabilityEvidenceAdvanceError::Exhausted)
    }

    pub(super) fn advance_availability_evidence(
        &mut self,
        proxy_session_id: &ProxySessionId,
    ) -> Result<HlsAvailabilityEvidenceGeneration, HlsAvailabilityEvidenceAdvanceError> {
        let generation = self
            .last_availability_evidence_generation
            .0
            .checked_add(1)
            .map(HlsAvailabilityEvidenceGeneration)
            .ok_or(HlsAvailabilityEvidenceAdvanceError::Exhausted)?;
        self.last_availability_evidence_generation = generation;
        self.availability_generation_by_proxy_session.insert(proxy_session_id.clone(), generation);
        Ok(generation)
    }

    pub fn prepare_manifest_publication(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsLeaseManifestPublicationGuard> {
        let lease = self.by_lease_id.get(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id {
            return None;
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return None;
        }
        self.by_lease_id.get(lease_id)?.manifest_publication_guard()
    }

    pub(crate) fn published_transient_resource_ids(
        &self,
        lease_id: &HlsAccessLeaseId,
    ) -> Option<&HlsPublishedTransientResourceIds> {
        self.by_lease_id.get(lease_id).map(HlsAccessLease::published_transient_resource_ids)
    }

    pub(crate) fn commit_manifest_publication_with_resources(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        mut snapshot: HlsLeaseManifestSnapshot,
        published_transient_resource_ids: HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        if lease.proxy_session_id != *proxy_session_id {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SessionMismatch,
            );
        }
        if self.refresh_access_lease_validity(lease_id, now_ms).is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        if lease.state == HlsAccessLeaseState::Expired {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired);
        }
        if !lease_state_allows_use(lease.state) {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::LeaseUnavailable,
            );
        }
        if lease.issued_at_ms != expected.issued_at_ms {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::LeaseIncarnationChanged,
            );
        }
        if lease.admission_generation != expected.admission_generation {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged,
            );
        }
        if lease.playback_mode != HlsLeasePlaybackMode::Live {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseNotLive);
        }
        if lease
            .last_manifest_snapshot
            .as_ref()
            .is_some_and(|current| manifest_source_is_regressive(current, &snapshot))
        {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SourceRegressive,
            );
        }
        if self.availability_evidence_can_advance().is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::UnknownLease);
        };
        let next_generation = lease.manifest_snapshot_generation.saturating_add(1);
        if next_generation == lease.manifest_snapshot_generation {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        if !commit_lease_revision_bindings(lease, &snapshot) {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::RevisionConflict,
            );
        }
        lease.manifest_snapshot_generation = next_generation;
        snapshot.snapshot_generation = next_generation;
        let finalized_manifest_generation = snapshot.finalized_transient_manifest_generation;
        lease.last_manifest_snapshot = Some(snapshot);
        lease.published_transient_resource_ids = published_transient_resource_ids;
        if let Some(manifest_generation) = finalized_manifest_generation {
            lease.published_finalized_manifest_generations.insert(manifest_generation);
        }
        lease.startup_admission = HlsLeaseStartupAdmissionState::Admitted;
        if self.advance_availability_evidence(proxy_session_id).is_err() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::SnapshotGenerationExhausted,
            );
        }
        HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: next_generation }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn commit_manifest_publication(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        self.commit_manifest_publication_with_resources(
            lease_id,
            proxy_session_id,
            expected,
            snapshot,
            HlsPublishedTransientResourceIds::default(),
            now_ms,
        )
    }
}

fn manifest_source_is_regressive(current: &HlsLeaseManifestSnapshot, candidate: &HlsLeaseManifestSnapshot) -> bool {
    candidate.source_commit_identity.commit_generation() < current.source_commit_identity.commit_generation()
}
