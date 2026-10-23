//! One transaction for repairing an already published segment binding.
use super::{
    HlsAccessLeaseId, HlsAccessLeaseRemovalPreparation, HlsAccessLeaseState, HlsAccessLeaseStore, HlsLeasePlaybackMode,
    HlsMediaLeaseIdentity,
};
use crate::{HlsSession, SegmentCacheStatus, SegmentRevisionGuard, SegmentRevisionState};
use std::sync::{atomic::Ordering, Arc};

#[derive(Debug, Clone)]
pub(crate) struct HlsRevisionRecoveryGuard {
    identity: HlsMediaLeaseIdentity,
    snapshot_generation: u64,
    pub(crate) revision: SegmentRevisionGuard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsRevisionRecoveryOutcome {
    Unchanged,
    ReplacementBound,
    RetryPending,
    ReentryRequired,
}

impl HlsRevisionRecoveryGuard {
    pub(crate) fn completion_identity(&self) -> (u64, u64) {
        (self.identity.lease_issued_at_ms(), self.snapshot_generation)
    }
}

impl HlsAccessLeaseStore {
    pub(crate) fn revision_recovery_guard(
        &self,
        lease_id: &HlsAccessLeaseId,
        seq: u64,
    ) -> Option<HlsRevisionRecoveryGuard> {
        let lease = self.by_lease_id.get(lease_id)?;
        Some(HlsRevisionRecoveryGuard {
            identity: lease.media_identity()?,
            snapshot_generation: lease.manifest_snapshot_generation,
            revision: self.published_segment_revision(lease_id, seq)?,
        })
    }

    fn revision_recovery_is_current(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        expected: &HlsRevisionRecoveryGuard,
        session: &HlsSession,
        now_ms: u64,
    ) -> bool {
        expected.identity.is_live()
            && !session.is_gc_marked_for_removal()
            && self.media_identity_is_current(lease_id, &session.proxy_session_id, expected.identity, now_ms)
            && self.by_lease_id.get(lease_id).is_some_and(|lease| {
                lease.manifest_snapshot_generation == expected.snapshot_generation
                    && self.published_segment_revision(lease_id, expected.revision.revision().key.proxy_seq).as_ref()
                        == Some(&expected.revision)
            })
    }

    /// Caller holds the lease store and session locks in that order. The guard
    /// fences the exact lease incarnation, snapshot and old revision together.
    pub(crate) fn recover_revision_binding(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        expected: &HlsRevisionRecoveryGuard,
        session: &HlsSession,
        now_ms: u64,
    ) -> HlsRevisionRecoveryOutcome {
        use HlsRevisionRecoveryOutcome::{ReentryRequired, ReplacementBound, RetryPending, Unchanged};
        if !self.revision_recovery_is_current(lease_id, expected, session, now_ms)
            || !matches!(expected.revision.revision().state(), SegmentRevisionState::Failed)
        {
            return Unchanged;
        }
        let old = expected.revision.revision();
        let seq = old.key.proxy_seq;
        if old.was_exposed() {
            return ReentryRequired;
        }
        if !old.origin_binding_is_current(session) {
            return ReentryRequired;
        }
        let mode = self.by_lease_id.get(lease_id).and_then(|lease| {
            lease.last_manifest_snapshot.as_ref()?.startup_revisions.as_ref().map(|publication| publication.mode)
        });
        if !mode.is_some_and(|mode| old.is_replaceable_in(mode)) {
            return ReentryRequired;
        }
        let candidate = session.startup.as_ref().and_then(|startup| startup.revisions.get(&seq));
        let replacement = candidate.filter(|candidate| {
            *candidate != &expected.revision
                && candidate.revision().key.proxy_seq == seq
                && candidate.revision().id() > old.id()
                && candidate.revision().key.kind == old.key.kind
                && candidate.revision().origin_binding_is_current(session)
                && candidate.revision().is_complete()
                && session
                    .segments
                    .get(&seq)
                    .is_some_and(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
        });
        let Some(replacement) = replacement else {
            return if session.segments.get(&seq).is_some_and(|entry| {
                matches!(
                    entry.status,
                    SegmentCacheStatus::Discovered
                        | SegmentCacheStatus::Queued { .. }
                        | SegmentCacheStatus::Fetching { .. }
                        | SegmentCacheStatus::FailedRetryable { .. }
                        | SegmentCacheStatus::CapacityDeferred { .. }
                )
            }) {
                RetryPending
            } else {
                ReentryRequired
            };
        };
        if self.availability_evidence_can_advance().is_err() {
            return RetryPending;
        }
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else { return Unchanged };
        let Some(generation) = lease.manifest_snapshot_generation.checked_add(1) else { return RetryPending };
        let Some(bindings) = lease.revision_bindings.as_mut() else { return Unchanged };
        Arc::make_mut(bindings).revisions.insert(seq, replacement.clone());
        lease.manifest_snapshot_generation = generation;
        if let Some(snapshot) = lease.last_manifest_snapshot.as_mut() {
            snapshot.snapshot_generation = generation;
            if let Some(publication) = snapshot.startup_revisions.as_mut() {
                if Arc::make_mut(publication).revisions.contains_key(&seq) {
                    Arc::make_mut(publication).revisions.insert(seq, replacement.clone());
                }
            }
        }
        let _ = self.advance_availability_evidence(&session.proxy_session_id);
        log::debug!(
            "HLS revision recovery: lease={} seq={seq} generation={generation} old={:?} new={:?} prefix={} exposed={} outcome=replacement_bound",
            crate::safe_hls_access_lease_id(lease_id), old.id(), replacement.revision().id(),
            old.prefix_available.load(Ordering::Acquire), old.was_exposed(),
        );
        ReplacementBound
    }

    /// An identity-checked, idempotent live -> ended transition. Removal and all
    /// existing external cleanup happen through the manager's prepared removal.
    pub(crate) fn end_revision_playback_if_current(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        expected: &HlsRevisionRecoveryGuard,
        session: &HlsSession,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseRemovalPreparation> {
        if !self.revision_recovery_is_current(lease_id, expected, session, now_ms)
            || !matches!(expected.revision.revision().state(), SegmentRevisionState::Failed)
            || self.availability_evidence_can_advance().is_err()
        {
            return None;
        }
        let lease = self.by_lease_id.get_mut(lease_id)?;
        if lease.playback_mode != HlsLeasePlaybackMode::Live {
            return None;
        }
        lease.end_playback();
        lease.state = HlsAccessLeaseState::Denied;
        let _ = self.advance_availability_evidence(&session.proxy_session_id);
        log::debug!(
            "HLS revision recovery: lease={} seq={} generation={} revision={:?} prefix={} exposed={} outcome=reentry_required",
            crate::safe_hls_access_lease_id(lease_id), expected.revision.revision().key.proxy_seq,
            expected.snapshot_generation, expected.revision.revision().id(),
            expected.revision.revision().prefix_available.load(Ordering::Acquire), expected.revision.revision().was_exposed(),
        );
        self.prepare_access_lease_removal(lease_id)
    }

    pub(crate) fn revision_bound_lease_ids(&self, session: &crate::ProxySessionId, seq: u64) -> Vec<HlsAccessLeaseId> {
        self.by_lease_id
            .values()
            .filter(|lease| {
                lease.proxy_session_id == *session
                    && lease.playback_mode == HlsLeasePlaybackMode::Live
                    && lease.revision_bindings.as_ref().is_some_and(|bindings| bindings.revisions.contains_key(&seq))
            })
            .map(|lease| lease.lease_id.clone())
            .collect()
    }
}
