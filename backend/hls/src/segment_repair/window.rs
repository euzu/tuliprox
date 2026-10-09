use super::{
    is_hls_provisioning_segment, safe_hls_access_lease_id, HlsAccessLeaseId, HlsAccessLeaseStore,
    HlsLeaseManifestSnapshot, HlsLogIdentity, HlsRepairPrewarmGuard, HlsRepairWindowCandidateKey, HlsSession,
    ProxySessionId, SegmentCacheKey, SegmentCacheStatus,
};
use shared::model::HlsSegmentRepairMode;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum HlsSegmentRepairSource {
    Normal,
    Transient,
}

impl HlsSegmentRepairSource {
    pub(crate) const fn as_log_value(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Transient => "transient",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum HlsRepairRenderedObjectId {
    Normal { proxy_seq: u64 },
    Transient { resource_id: String },
}

#[derive(Debug, Clone)]
pub struct HlsSegmentRepairObjectContext {
    pub source: HlsSegmentRepairSource,
    pub log_identity: HlsLogIdentity,
    pub proxy_session_id: ProxySessionId,
    pub hls_access_lease_id: Option<HlsAccessLeaseId>,
    pub rendered_object_id: HlsRepairRenderedObjectId,
    pub resource_id: String,
    pub file_ext: String,
    /// Concrete origin fetch URI retained for diagnostics and postprocess metadata.
    ///
    /// This may include a provider mirror or redirect/CDN host. It must not be used as HLS session identity, account
    /// binding, provider-failover state, repair object identity, or repair-window candidate identity.
    pub origin_fetch_uri_for_diagnostics: String,
    pub media_sequence: Option<u64>,
    pub discontinuity_sequence: Option<u64>,
    pub complete_object: bool,
    pub encrypted: bool,
    pub custom_response: bool,
}

pub fn ready_segment_repair_prewarm_candidates(
    session: &HlsSession,
    lease_id: &HlsAccessLeaseId,
    snapshot: &HlsLeaseManifestSnapshot,
    candidate_limit: usize,
) -> Vec<(SegmentCacheKey, HlsSegmentRepairObjectContext)> {
    let identity = HlsLogIdentity::from_session(session);
    snapshot
        .visible_segments
        .iter()
        .take(candidate_limit)
        .filter_map(|visible| {
            let entry = session.segments.get(&visible.proxy_seq)?;
            if entry.proxy_seq != visible.proxy_seq
                || is_hls_provisioning_segment(entry)
                || entry.proxy_file_ext != "ts"
                || entry.origin_byte_range.is_some()
                || entry.encryption.is_some()
                || !matches!(entry.status, SegmentCacheStatus::Ready { .. })
            {
                return None;
            }
            Some((
                entry.cache_key.clone(),
                HlsSegmentRepairObjectContext {
                    source: HlsSegmentRepairSource::Normal,
                    log_identity: identity.clone(),
                    proxy_session_id: session.proxy_session_id.clone(),
                    hls_access_lease_id: Some(lease_id.clone()),
                    rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: visible.proxy_seq },
                    resource_id: format!("{:06}", visible.proxy_seq),
                    file_ext: entry.proxy_file_ext.clone(),
                    origin_fetch_uri_for_diagnostics: entry
                        .origin_fetch_ref
                        .as_ref()
                        .map(|fetch_ref| fetch_ref.resolved_origin_url.clone())
                        .unwrap_or_default(),
                    media_sequence: Some(entry.origin_key.host_local_sequence),
                    discontinuity_sequence: Some(session.discontinuity_sequence),
                    complete_object: true,
                    encrypted: false,
                    custom_response: false,
                },
            ))
        })
        .collect()
}

impl HlsRepairPrewarmGuard {
    pub fn new(
        access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
        lease_id: HlsAccessLeaseId,
        proxy_session_id: ProxySessionId,
        issued_at_ms: u64,
        snapshot_generation: u64,
    ) -> Self {
        Self { access_leases, lease_id, proxy_session_id, issued_at_ms, snapshot_generation }
    }

    pub(super) async fn is_current(&self) -> bool {
        self.access_leases.read().await.repair_prewarm_is_current(
            &self.lease_id,
            &self.proxy_session_id,
            self.issued_at_ms,
            self.snapshot_generation,
        )
    }
}

impl HlsSegmentRepairObjectContext {
    pub(crate) fn is_repairable_ts(&self) -> bool {
        self.file_ext.eq_ignore_ascii_case("ts") && self.complete_object && !self.encrypted && !self.custom_response
    }

    pub(super) fn repair_skip_reason(&self) -> Option<&'static str> {
        if self.is_repairable_ts() {
            return None;
        }
        if !self.file_ext.eq_ignore_ascii_case("ts") {
            return Some("not-ts");
        }
        if !self.complete_object {
            return Some("partial-object");
        }
        if self.encrypted {
            return Some("encrypted");
        }
        if self.custom_response {
            return Some("custom-response");
        }
        None
    }

    pub(super) fn log_identity_fields(&self) -> String {
        format!(
            "session={} proxy_session={} lease={}",
            self.log_identity.session(),
            self.log_identity.proxy_session(),
            self.hls_access_lease_id.as_ref().map_or_else(|| "<none>".to_string(), safe_hls_access_lease_id)
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum RepairCandidateSelection {
    Selected(HlsSegmentRepairMode, HlsRepairWindowCandidateKey),
    Skipped(&'static str),
    AlreadyChecked(HlsSegmentRepairMode, HlsRepairWindowCandidateKey),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum RepairStatus {
    Clean,
    Fixed,
    PolicyLimited,
    Unsupported,
    Timeout,
    RemuxFailed,
    ValidationFailed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum RepairVideoCodec {
    H264,
    Hevc,
    Unsupported,
}

impl RepairVideoCodec {
    pub(super) const fn as_log_value(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Unsupported => "unsupported",
        }
    }
}
