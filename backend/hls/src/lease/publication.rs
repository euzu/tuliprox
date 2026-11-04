use super::{
    commit_lease_revision_bindings, lease_state_allows_use, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsAvailabilityEvidenceAdvanceError, HlsAvailabilityEvidenceGeneration,
    HlsLeaseManifestPublicationGuard, HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason,
    HlsLeaseManifestSnapshot, HlsLeasePlaybackMode, HlsLeaseStartupAdmissionState, HlsPublishedTransientResourceIds,
    ProxySessionId,
};

impl HlsAccessLease {
    pub const fn entry_generation(&self) -> Option<u64> { self.entry_generation }

    pub fn has_current_entry_manifest(&self) -> bool {
        self.entry_generation.is_some() && self.admitted_entry_generation == self.entry_generation
    }

    pub fn uses_entry_view(&self) -> bool {
        matches!(self.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle)
            || (self.state == HlsAccessLeaseState::Activated
                && self.last_manifest_snapshot.is_some()
                && !self.has_current_entry_manifest())
    }

    pub fn published_transient_resource_ids(&self) -> &HlsPublishedTransientResourceIds {
        &self.published_transient_resource_ids
    }

    fn manifest_publication_guard(&self) -> Option<HlsLeaseManifestPublicationGuard> {
        let entry_generation = self.entry_generation?;
        (lease_state_allows_use(self.state)
            && self.playback_mode == HlsLeasePlaybackMode::Live
            && !self.manifest_start_attempt.as_ref().is_some_and(|attempt| attempt.failed()))
        .then_some(HlsLeaseManifestPublicationGuard {
            issued_at_ms: self.issued_at_ms,
            admission_generation: self.admission_generation,
            state: self.state,
            entry_generation,
            strip_policy: None,
            admits_live_entry: true,
            source: None,
        })
    }
}

impl HlsAccessLeaseStore {
    pub(crate) fn bound_entry_wait(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        entry_generation: u64,
        requested_deadline_ms: u64,
    ) -> Option<u64> {
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if lease.proxy_session_id != *proxy_session_id
            || lease.entry_generation != Some(entry_generation)
            || !lease.uses_entry_view()
        {
            return None;
        }
        let deadline =
            lease.entry_wait_deadline_ms.map_or(requested_deadline_ms, |current| current.min(requested_deadline_ms));
        lease.entry_wait_deadline_ms = Some(deadline);
        Some(deadline)
    }

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
        if lease.state != expected.state || lease.entry_generation != Some(expected.entry_generation) {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::EntryChanged);
        }
        if lease.playback_mode != HlsLeasePlaybackMode::Live {
            return HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseNotLive);
        }
        if lease
            .last_manifest_snapshot
            .as_ref()
            .is_some_and(|current| manifest_source_is_regressive(current, &snapshot, expected.strip_policy.is_some()))
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
        lease.published_media_sequence_highwater = Some(
            lease
                .published_media_sequence_highwater
                .map_or(snapshot.last_proxy_seq, |previous| previous.max(snapshot.last_proxy_seq)),
        );
        let last_discontinuity_sequence = snapshot.discontinuity_sequence.saturating_add(
            u64::try_from(snapshot.visible_segments.iter().filter(|segment| segment.discontinuity_before).count())
                .unwrap_or(u64::MAX),
        );
        lease.published_discontinuity_sequence_highwater =
            lease.published_discontinuity_sequence_highwater.max(last_discontinuity_sequence);
        let finalized_manifest_generation = snapshot.finalized_transient_manifest_generation;
        lease.last_manifest_snapshot = Some(snapshot);
        lease.published_transient_resource_ids = published_transient_resource_ids;
        if let Some(manifest_generation) = finalized_manifest_generation {
            lease.published_finalized_manifest_generations.insert(manifest_generation);
        }
        lease.startup_admission = HlsLeaseStartupAdmissionState::Admitted;
        if expected.admits_live_entry {
            lease.admitted_entry_generation = Some(expected.entry_generation);
        }
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

fn manifest_source_is_regressive(
    current: &HlsLeaseManifestSnapshot,
    candidate: &HlsLeaseManifestSnapshot,
    entry: bool,
) -> bool {
    candidate.source_commit_identity.commit_generation() < current.source_commit_identity.commit_generation()
        || ((entry
            || candidate.startup_revisions.as_ref().is_some_and(|publication| publication.window_plan.is_some()))
            && candidate.first_proxy_seq < current.first_proxy_seq)
}
