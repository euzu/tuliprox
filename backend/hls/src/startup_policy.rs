use super::{HlsSession, ProgressiveBudgetManager, SegmentEntry, SegmentRevisionGuard, SegmentRevisionStore};
use shared::model::HlsStartupMode;
use std::{collections::BTreeMap, sync::Arc};
use tuliprox_core::model::HlsStartupConfig;

/// Successors after the live head that fast start publishes and scans for missing segments.
pub(crate) const STARTUP_WINDOW_SUCCESSORS: u64 = 5;

#[derive(Debug)]
pub struct HlsSessionStartup {
    pub config: HlsStartupConfig,
    pub first_data_timeout_ms: u64,
    pub worker: std::sync::Weak<super::HlsSegmentWorkerPool>,
    pub access_leases: Arc<tokio::sync::RwLock<super::HlsAccessLeaseStore>>,
    pub revisions: BTreeMap<u64, SegmentRevisionGuard>,
    pub policy_fixed: bool,
    pub store: Arc<SegmentRevisionStore>,
    pub budget: Arc<ProgressiveBudgetManager>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsManifestRevisions {
    pub mode: HlsStartupMode,
    pub retained_start_seq: u64,
    pub revisions: BTreeMap<u64, SegmentRevisionGuard>,
}

impl HlsSession {
    pub fn startup_mode(&self) -> HlsStartupMode {
        self.startup.as_ref().map_or(HlsStartupMode::Conservative, |startup| startup.config.mode)
    }

    pub(crate) fn reconcile_startup_eligibility(&mut self) {
        if self.startup.is_none() {
            return;
        }
        let eligible = self.origin_source.archive_reference.is_none()
            && !self.segments.is_empty()
            && self.segments.values().all(SegmentEntry::is_fast_start_eligible);
        if !eligible && self.published_live_origin_baseline.is_none() {
            self.startup = None;
        }
    }
}

impl SegmentEntry {
    /// Fast start serves only clear, whole MPEG-TS objects without an init map.
    pub(crate) fn is_fast_start_eligible(&self) -> bool {
        self.encryption.is_none()
            && self.map_ref.is_none()
            && self.origin_byte_range.is_none()
            && self.proxy_file_ext.eq_ignore_ascii_case("ts")
    }
}
