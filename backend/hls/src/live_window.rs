//! One metadata-only window plan for accelerated, untagged live playlists.
use crate::{
    media_reserve::HlsManifestCommitIdentity, HlsAccessLease, HlsAccessLeaseState, HlsSession, HlsSessionMode,
    SegmentCacheStatus,
};
use tuliprox_core::model::StripConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsLiveWindowView {
    Entry,
    /// `last_published` is the last sequence this viewer received; `None` disables tail truncation.
    Active {
        first_published: u64,
        last_published: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsLiveWindowPlan {
    pub visible: Vec<u64>,
    pub hidden_tail: Vec<u64>,
    /// Active-view sequences after the visible end (`available[end..]`) that are demanded but not renderable yet.
    pub pending_tail: Vec<u64>,
    pub effective_strip_segments: usize,
    pub origin_commits: u64,
    pub origin_work_generation: u64,
    pub source_commit_identity: Option<HlsManifestCommitIdentity>,
    origin_sequences: Vec<u64>,
    strip: StripConfig,
    view: HlsLiveWindowView,
    recovered_startup: bool,
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
        if lease.state == HlsAccessLeaseState::Activated && !lease.uses_entry_view() {
            if let Some(snapshot) = &lease.last_manifest_snapshot {
                return Self::Active {
                    first_published: snapshot.first_proxy_seq,
                    last_published: Some(snapshot.last_proxy_seq),
                };
            }
        }
        Self::Entry
    }
}

impl HlsLiveWindowPlan {
    pub fn is_current(&self, session: &HlsSession) -> bool {
        session.plan_live_window(self.view).as_ref() == Some(self)
    }

    pub fn is_startup_recovery(&self) -> bool { self.recovered_startup }

    pub(crate) fn is_ready(&self, session: &HlsSession) -> bool {
        self.visible.iter().all(|seq| {
            session.segments.get(seq).is_some_and(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
        })
    }
}

impl HlsSession {
    #[cfg(any(test, feature = "test-support"))]
    pub fn qualify_short_entry_window_for_test(&mut self) {
        self.origin_control.qualification = crate::short_manifest::HlsOriginQualification::Qualified {
            basis: crate::short_manifest::HlsOriginQualificationBasis::capture(self),
            generation: self.origin_control.acceptance_generation,
            windows: crate::short_manifest::HlsQualifiedOriginWindows::ShortAllowed,
            publication_boundary: self.last_normal_manifest_commit_identity(),
        };
    }

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

    /// Entry selection follows origin metadata unless a selected fetch failed.
    /// Recovery may consume the stripped reserve, but never crosses a failed
    /// object or changes an active lease's published window.
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
        let mut recovered_startup = false;
        let mut pending_tail = Vec::new();
        let (visible, hidden_tail, effective_strip_segments) = match view {
            HlsLiveWindowView::Entry => {
                let strip = crate::initial_strip::initial_hls_strip_segments_for_durations(
                    &self.live_window_strip,
                    &self.origin_window_durations_ms,
                );
                let end = origin.len().saturating_sub(strip);
                let start = end.saturating_sub(crate::renderer::MIN_VISIBLE_SEGMENTS);
                let selected = &origin[start..end];
                // The retry still owns the tail. A new viewer can start on the complete
                // contiguous prefix while the same object is retried for existing viewers.
                if selected.len() == crate::renderer::MIN_VISIBLE_SEGMENTS
                    && selected.last().and_then(|seq| self.segments.get(seq)).is_some_and(|entry| {
                        matches!(entry.status, SegmentCacheStatus::Fetching { gateway_timeout: true, .. })
                    })
                    && selected[..selected.len() - 1].iter().all(|seq| {
                        self.segments
                            .get(seq)
                            .is_some_and(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
                    })
                {
                    recovered_startup = true;
                    (origin[start..end - 1].to_vec(), origin[end - 1..].to_vec(), strip + 1)
                } else if selected.iter().any(|seq| self.live_startup_segment_failed(*seq)) {
                    recovered_startup = true;
                    let runs = origin[start..].split(|seq| self.live_startup_segment_failed(*seq));
                    let mut selected = &origin[0..0];
                    for run in runs {
                        if run.len() >= crate::renderer::MIN_VISIBLE_SEGMENTS {
                            selected = &run[..crate::renderer::MIN_VISIBLE_SEGMENTS];
                            break;
                        }
                        if run.len() > selected.len() {
                            selected = run;
                        }
                    }
                    let last = selected.last()?;
                    let selected_end = origin.iter().position(|seq| seq == last)? + 1;
                    (selected.to_vec(), origin[selected_end..].to_vec(), origin.len() - selected_end)
                } else {
                    (origin[start..end].to_vec(), origin[end..].to_vec(), strip)
                }
            }
            HlsLiveWindowView::Active { first_published, last_published } => {
                let available = origin.iter().copied().filter(|seq| *seq >= first_published).collect::<Vec<_>>();
                // A slow origin tail must not freeze the live window while earlier media is ready.
                // Trailing unrenderable sequences stay demanded but are not published yet. Never
                // drop an already published sequence and never skip a hole inside the window.
                let mut end = available.len();
                while let Some(&seq) = end.checked_sub(1).and_then(|index| available.get(index)) {
                    if last_published.is_some_and(|last| seq <= last)
                        || self.segments.get(&seq).is_some_and(|entry| crate::renderer::is_renderable(entry, self))
                    {
                        break;
                    }
                    end -= 1;
                }
                // Truncate only when the ready prefix publishes new media; otherwise the
                // previous publication stays authoritative (retained window fallback).
                if !last_published.is_some_and(|last| {
                    end.checked_sub(1).and_then(|index| available.get(index)).is_some_and(|seq| *seq > last)
                }) {
                    end = available.len();
                }
                let mut visible = available[end.saturating_sub(6)..end].to_vec();
                crate::renderer::retain_published_predecessors_from(self, &mut visible, first_published);
                pending_tail = available[end..].to_vec();
                (visible, Vec::new(), 0)
            }
        };
        if visible.is_empty() {
            return None;
        }
        Some(HlsLiveWindowPlan {
            visible,
            hidden_tail,
            pending_tail,
            effective_strip_segments,
            origin_commits: self.origin_manifest_commits(),
            origin_work_generation: self.activity.origin_work_generation,
            source_commit_identity: self.live_window_source_identity(),
            origin_sequences: origin.to_vec(),
            strip: self.live_window_strip.clone(),
            view,
            recovered_startup,
        })
    }

    pub(crate) fn live_startup_segment_failed(&self, seq: u64) -> bool {
        self.segments.get(&seq).is_none_or(|entry| {
            matches!(
                entry.status,
                SegmentCacheStatus::FailedPermanent { .. }
                    | SegmentCacheStatus::FailedRetryable { .. }
                    | SegmentCacheStatus::Expired
            )
        })
    }

    pub fn live_startup_segments_exhausted(&self) -> bool {
        self.uses_fast_start_live_window()
            && !self.live_origin_sequences().is_empty()
            && self.start_segments_exhausted()
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
        issued_at_ms: u64,
        snapshot: &crate::media_reserve::HlsLeaseManifestSnapshot,
    ) {
        if snapshot
            .startup_revisions
            .as_ref()
            .and_then(|publication| publication.window_plan.as_ref())
            .is_some_and(|window| window.plan.is_startup_recovery())
            && snapshot.visible_segments.len() < crate::renderer::MIN_VISIBLE_SEGMENTS
        {
            let visible = snapshot.visible_proxy_seqs().collect::<Vec<_>>();
            if self.last_warned_short_startup_window.as_ref() != Some(&visible) {
                crate::short_manifest::log_short_window_warning(self, visible.len(), "startup window recovered");
                self.last_warned_short_startup_window = Some(visible);
            }
        }
        let demand = self.live_window_demands.entry(lease_id.clone()).or_insert_with(|| HlsLiveWindowDemand {
            issued_at_ms,
            view: HlsLiveWindowView::Entry,
            published: Vec::new(),
        });
        demand.issued_at_ms = issued_at_ms;
        demand.published = snapshot.visible_proxy_seqs().collect();
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
