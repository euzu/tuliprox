//! One metadata-only window plan for accelerated, untagged live playlists.
use crate::{
    media_reserve::HlsManifestCommitIdentity, HlsAccessLease, HlsAccessLeaseState, HlsSession, HlsSessionMode,
};
use tuliprox_core::model::StripConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsLiveWindowView {
    Entry,
    Active { first_published: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsLiveWindowPlan {
    pub visible: Vec<u64>,
    pub hidden_tail: Vec<u64>,
    pub effective_strip_segments: usize,
    pub origin_commits: u64,
    pub origin_work_generation: u64,
    pub source_commit_identity: Option<HlsManifestCommitIdentity>,
    origin_sequences: Vec<u64>,
    strip: StripConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsLiveWindowPublication {
    pub plan: HlsLiveWindowPlan,
    pub body: std::sync::Arc<str>,
}

#[derive(Debug, Clone)]
pub(crate) struct HlsLiveWindowDemand {
    pub(crate) issued_at_ms: u64,
    pub(crate) view: HlsLiveWindowView,
    pub(crate) published: Vec<u64>,
}

impl HlsLiveWindowView {
    pub fn for_lease(lease: &HlsAccessLease) -> Self {
        if lease.state == HlsAccessLeaseState::Activated {
            if let Some(snapshot) = &lease.last_manifest_snapshot {
                return Self::Active { first_published: snapshot.first_proxy_seq };
            }
        }
        Self::Entry
    }
}

impl HlsLiveWindowPlan {
    pub fn is_current(&self, session: &HlsSession) -> bool {
        session.uses_fast_start_live_window()
            && self.origin_commits == session.origin_manifest_commits()
            && self.origin_work_generation == session.activity.origin_work_generation
            && self.source_commit_identity == session.live_window_source_identity()
            && self.origin_sequences.as_slice() == session.live_origin_sequences()
            && self.strip == session.live_window_strip
    }
}

impl HlsSession {
    pub(crate) fn live_origin_sequences(&self) -> &[u64] {
        self.origin_window_proxy_seqs.as_deref().unwrap_or_default()
    }

    pub(crate) fn live_window_source_identity(&self) -> Option<HlsManifestCommitIdentity> {
        if self
            .live_window_origin_work_generation
            .is_some_and(|generation| generation != self.activity.origin_work_generation)
        {
            return None;
        }
        self.live_window_origin_commit_identity.or_else(|| self.last_normal_manifest_commit_identity())
    }

    pub fn uses_fast_start_live_window(&self) -> bool {
        self.startup.is_some()
            && matches!(self.mode, HlsSessionMode::NormalCacheTimeline)
            && self.origin_source.archive_reference.is_none()
            && (self.origin_window_proxy_seqs.is_some() || self.segments.is_empty())
            && self.live_origin_sequences().iter().all(|seq| {
                self.segments
                    .get(seq)
                    .is_none_or(|entry| entry.is_fast_start_eligible() && !crate::is_hls_provisioning_segment(entry))
            })
    }

    /// Entry selection uses only origin metadata. Active slides may retain cached
    /// predecessors from this view to preserve three target durations.
    pub fn plan_live_window(&self, view: HlsLiveWindowView) -> Option<HlsLiveWindowPlan> {
        if !self.uses_fast_start_live_window()
            || self.live_origin_sequences().is_empty()
            || self
                .live_window_origin_work_generation
                .is_some_and(|generation| generation != self.activity.origin_work_generation)
        {
            return None;
        }
        let origin = self.live_origin_sequences();
        let (visible, hidden_tail, effective_strip_segments) = match view {
            HlsLiveWindowView::Entry => {
                let strip = crate::initial_strip::initial_hls_strip_segments_for_durations(
                    &self.live_window_strip,
                    &self.origin_window_durations_ms,
                );
                let end = origin.len().saturating_sub(strip);
                (origin[end.saturating_sub(3)..end].to_vec(), origin[end..].to_vec(), strip)
            }
            HlsLiveWindowView::Active { first_published } => {
                let available = origin.iter().copied().filter(|seq| *seq >= first_published).collect::<Vec<_>>();
                let mut visible = available[available.len().saturating_sub(6)..].to_vec();
                crate::renderer::retain_published_predecessors_from(self, &mut visible, first_published);
                (visible, Vec::new(), 0)
            }
        };
        if visible.is_empty() {
            return None;
        }
        Some(HlsLiveWindowPlan {
            visible,
            hidden_tail,
            effective_strip_segments,
            origin_commits: self.origin_manifest_commits(),
            origin_work_generation: self.activity.origin_work_generation,
            source_commit_identity: self.live_window_source_identity(),
            origin_sequences: origin.to_vec(),
            strip: self.live_window_strip.clone(),
        })
    }

    pub fn plan_live_window_for_lease(&mut self, lease: &HlsAccessLease) -> Option<HlsLiveWindowPlan> {
        let view = HlsLiveWindowView::for_lease(lease);
        let plan = self.plan_live_window(view)?;
        self.live_window_demands.insert(
            lease.lease_id.clone(),
            HlsLiveWindowDemand {
                issued_at_ms: lease.issued_at_ms,
                view,
                published: lease
                    .last_manifest_snapshot
                    .as_ref()
                    .map_or_else(Vec::new, |snapshot| snapshot.visible_proxy_seqs().collect()),
            },
        );
        Some(plan)
    }

    pub(crate) fn required_live_windows(&self) -> Vec<HlsLiveWindowPlan> {
        if self.live_window_demands.is_empty() {
            return self.plan_live_window(HlsLiveWindowView::Entry).into_iter().collect();
        }
        self.live_window_demands.values().filter_map(|demand| self.plan_live_window(demand.view)).collect()
    }

    pub(crate) fn live_window_heads(&self) -> Vec<u64> {
        self.required_live_windows().iter().filter_map(|plan| plan.visible.first().copied()).collect()
    }

    pub(crate) fn record_live_window_publication(
        &mut self,
        lease_id: &crate::HlsAccessLeaseId,
        snapshot: &crate::media_reserve::HlsLeaseManifestSnapshot,
    ) {
        if let Some(demand) = self.live_window_demands.get_mut(lease_id) {
            demand.published = snapshot.visible_proxy_seqs().collect();
        }
        self.longest_rendered_playlist_duration_ms =
            self.longest_rendered_playlist_duration_ms.max(snapshot.playlist_duration_ms);
        for seq in snapshot.visible_proxy_seqs() {
            if let Some(entry) = self.segments.get_mut(&seq) {
                entry.last_rendered_at_ms = Some(snapshot.delivered_at_ms);
                if let Some(identity) = entry.media_resource_identity() {
                    self.published_resource_history.record(identity, seq);
                }
                if self.published_live_origin_baseline.is_none() {
                    self.published_live_origin_baseline = Some(crate::session::HlsPublishedLiveOriginBaseline {
                        evidence_proxy_seq: seq,
                        origin_epoch: entry.origin_key.origin_epoch,
                        rendered_at_ms: snapshot.delivered_at_ms,
                    });
                }
            }
        }
    }
}
