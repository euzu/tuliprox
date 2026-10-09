use super::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
    ProxySessionId,
};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub(super) struct HlsLeaseRevisionBindings {
    pub(super) revisions: std::collections::BTreeMap<u64, super::super::SegmentRevisionGuard>,
    pub(super) retired_through: Option<u64>,
}

/// Upper bound of revisions one lease keeps bound, independent of the retained window.
const MAX_LEASE_REVISION_BINDINGS: usize = 1024;

impl HlsLeaseRevisionBindings {
    /// Pops the oldest bindings while `retire(seq, current_len)` holds and remembers the newest retired sequence.
    fn retire_while(&mut self, mut retire: impl FnMut(u64, usize) -> bool) {
        loop {
            let len = self.revisions.len();
            let Some(entry) = self.revisions.first_entry() else { break };
            if !retire(*entry.key(), len) {
                break;
            }
            self.retired_through = Some(entry.remove_entry().0);
        }
    }
}

impl HlsAccessLeaseStore {
    pub fn prune_revision_bindings(&mut self, session: &ProxySessionId, first_retained_seq: u64) {
        for lease in self.by_lease_id.values_mut().filter(|lease| lease.proxy_session_id == *session) {
            let Some(bindings) = &mut lease.revision_bindings else { continue };
            if bindings.revisions.first_key_value().is_some_and(|(seq, _)| *seq < first_retained_seq) {
                Arc::make_mut(bindings).retire_while(|seq, _| seq < first_retained_seq);
            }
        }
    }
    pub fn published_segment_revision(
        &self,
        lease_id: &HlsAccessLeaseId,
        seq: u64,
    ) -> Option<super::super::SegmentRevisionGuard> {
        self.by_lease_id.get(lease_id)?.revision_bindings.as_ref()?.revisions.get(&seq).cloned()
    }
    pub fn try_claim_progressive_startup(&mut self, lease_id: &HlsAccessLeaseId, seq: u64, now_ms: u64) -> bool {
        let Some(lease) = self.by_lease_id.get_mut(lease_id) else {
            return false;
        };
        if lease.playback_mode != HlsLeasePlaybackMode::Live
            || lease.progressive_startup_claimed
            || lease.validity_due_at_ms() <= now_ms
            || lease.last_manifest_snapshot.as_ref().is_none_or(|snapshot| snapshot.first_proxy_seq != seq)
        {
            return false;
        }
        lease.progressive_startup_claimed = true;
        true
    }
}

pub(super) fn commit_lease_revision_bindings(lease: &mut HlsAccessLease, snapshot: &HlsLeaseManifestSnapshot) -> bool {
    if let Some(publication) = &snapshot.startup_revisions {
        let current = lease.revision_bindings.as_ref();
        if publication.revisions.iter().any(|(seq, revision)| {
            !revision.revision().is_publishable()
                || current.is_some_and(|current| {
                    current.retired_through.is_some_and(|retired| *seq <= retired)
                        || current.revisions.get(seq).is_some_and(|bound| bound != revision)
                })
        }) {
            return false;
        }
        let bindings = lease.revision_bindings.get_or_insert_with(|| Arc::new(HlsLeaseRevisionBindings::default()));
        let bindings = Arc::make_mut(bindings);
        bindings.revisions.extend(publication.revisions.iter().map(|(seq, revision)| (*seq, revision.clone())));
        bindings.retire_while(|seq, len| len > MAX_LEASE_REVISION_BINDINGS || seq < publication.retained_start_seq);
    }
    true
}
