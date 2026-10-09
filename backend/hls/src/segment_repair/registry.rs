use super::{
    ffmpeg_identity_version, window::RepairCandidateSelection, HlsAccessLeaseId, HlsRepairObjectMetadataKey,
    HlsRepairWindow, HlsRepairWindowCandidateKey, HlsRepairWindowRegistry, HlsSegmentRepairObjectContext,
    ProxySessionId, COMMAND_VERSION, REPAIR_CANDIDATE_MAX_ENTRIES,
};
use shared::model::HlsSegmentRepairMode;
use std::collections::HashSet;
use tuliprox_core::model::HlsSegmentRepairConfig;

impl HlsRepairWindowRegistry {
    pub(super) fn start_window(&mut self, lease_id: HlsAccessLeaseId, config: &HlsSegmentRepairConfig) {
        if config.max_level == HlsSegmentRepairMode::Off || config.apply_to_first_segments == 0 {
            self.windows.remove(&lease_id);
            return;
        }
        let generation = self
            .generations
            .entry(lease_id.clone())
            .and_modify(|generation| *generation = generation.saturating_add(1))
            .or_insert(1);
        self.windows.insert(
            lease_id,
            HlsRepairWindow {
                mode: config.max_level,
                activation_generation: *generation,
                remaining_segments: config.apply_to_first_segments,
                seen_candidates: HashSet::new(),
            },
        );
    }

    pub(super) fn ensure_window(&mut self, lease_id: HlsAccessLeaseId, config: &HlsSegmentRepairConfig) -> bool {
        if self.windows.contains_key(&lease_id) {
            return false;
        }
        self.start_window(lease_id, config);
        true
    }

    pub(super) fn try_select_candidate(&mut self, context: &HlsSegmentRepairObjectContext) -> RepairCandidateSelection {
        if let Some(reason) = context.repair_skip_reason() {
            return RepairCandidateSelection::Skipped(reason);
        }
        let Some(lease_id) = context.hls_access_lease_id.as_ref() else {
            return RepairCandidateSelection::Skipped("missing-lease");
        };
        let Some((activation_generation, mode)) =
            self.windows.get(lease_id).map(|window| (window.activation_generation, window.mode))
        else {
            return RepairCandidateSelection::Skipped("no-window");
        };
        let candidate_key = HlsRepairWindowCandidateKey {
            proxy_session_id: context.proxy_session_id.clone(),
            hls_access_lease_id: lease_id.clone(),
            activation_generation,
            object_id: context.rendered_object_id.clone(),
            file_ext: context.file_ext.clone(),
        };
        let Some(window) = self.windows.get(lease_id) else {
            return RepairCandidateSelection::Skipped("no-window");
        };
        if window.seen_candidates.contains(&candidate_key) {
            return RepairCandidateSelection::AlreadyChecked(mode, candidate_key);
        }
        if window.remaining_segments == 0 {
            return RepairCandidateSelection::Skipped("window-exhausted");
        }
        if !self.remember_candidate(candidate_key.clone()) {
            return RepairCandidateSelection::Skipped("candidate-not-admitted");
        }
        let Some(window) = self.windows.get_mut(lease_id) else {
            return RepairCandidateSelection::Skipped("no-window");
        };
        window.seen_candidates.insert(candidate_key.clone());
        window.remaining_segments = window.remaining_segments.saturating_sub(1);
        RepairCandidateSelection::Selected(window.mode, candidate_key)
    }

    pub(super) fn candidate_is_current(&self, candidate: &HlsRepairWindowCandidateKey) -> bool {
        self.windows.get(&candidate.hls_access_lease_id).is_some_and(|window| {
            window.activation_generation == candidate.activation_generation
                && window.seen_candidates.contains(candidate)
        })
    }

    pub(super) fn remove_access_lease(&mut self, lease_id: &HlsAccessLeaseId) {
        self.windows.remove(lease_id);
        self.generations.remove(lease_id);
        self.checked_candidates.retain(|key| key.hls_access_lease_id != *lease_id);
        self.checked_candidate_order.retain(|key| key.hls_access_lease_id != *lease_id);
    }

    pub(super) fn remove_proxy_session(&mut self, proxy_session_id: &ProxySessionId, lease_ids: &[HlsAccessLeaseId]) {
        for lease_id in lease_ids {
            self.windows.remove(lease_id);
            self.generations.remove(lease_id);
        }
        self.checked_candidates.retain(|key| key.proxy_session_id != *proxy_session_id);
        self.checked_candidate_order.retain(|key| key.proxy_session_id != *proxy_session_id);
    }

    pub(super) fn clear(&mut self) {
        self.windows.clear();
        self.generations.clear();
        self.checked_candidates.clear();
        self.checked_candidate_order.clear();
    }

    pub(super) fn stats(&self) -> HlsSegmentRepairStats {
        HlsSegmentRepairStats {
            windows: self.windows.len(),
            generations: self.generations.len(),
            checked_candidates: self.checked_candidates.len(),
            ..HlsSegmentRepairStats::default()
        }
    }

    pub(super) fn remember_candidate(&mut self, key: HlsRepairWindowCandidateKey) -> bool {
        if !self.checked_candidates.insert(key.clone()) {
            return false;
        }
        self.checked_candidate_order.push_back(key);
        self.prune_checked_candidates();
        true
    }

    pub(super) fn prune_checked_candidates(&mut self) {
        while self.checked_candidates.len() > REPAIR_CANDIDATE_MAX_ENTRIES {
            let Some(oldest) = self.checked_candidate_order.pop_front() else {
                return;
            };
            self.checked_candidates.remove(&oldest);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct HlsSegmentRepairStats {
    pub windows: usize,
    pub generations: usize,
    pub checked_candidates: usize,
    pub metadata: usize,
    pub object_metadata: usize,
    pub locks: usize,
    pub watchdog_metadata: usize,
    pub watchdog_locks: usize,
}

pub(super) fn repair_object_metadata_key(
    context: &HlsSegmentRepairObjectContext,
    repair_mode: HlsSegmentRepairMode,
) -> HlsRepairObjectMetadataKey {
    HlsRepairObjectMetadataKey {
        proxy_session_id: context.proxy_session_id.clone(),
        rendered_object_id: context.rendered_object_id.clone(),
        file_ext: context.file_ext.to_ascii_lowercase(),
        repair_mode,
        command_version: COMMAND_VERSION,
        ffmpeg_version: ffmpeg_identity_version(),
    }
}
