use super::{
    cache::HlsSegmentCache,
    ids::ProxySessionId,
    lease::HlsAccessLeaseId,
    media_reserve::{HlsLeaseManifestSnapshot, HlsManifestDeliveryMode},
    prepared_terminal_bundle::{
        terminal_asset_fits_target_duration, HlsAnchoredTerminalBundle, HlsAnchoredTerminalSegment,
        HlsPreparedTerminalBundleKey,
    },
    runtime_custom_tail::{HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason},
    session_store::HlsSessionHandle,
    timeline::{CacheAccessState, SegmentCacheStatus, SegmentEntry},
    transient::TransientResourceId,
    HlsTrackEvidenceResolution, HlsTsProbeBudget, HlsTsProbeProtection, HlsTsSpliceEvidence, HlsTsTrackSignature,
    TransientObjectCacheKey, TransientPassthroughState, TransientResourceFile, TransientResourceKind,
};
use std::{path::PathBuf, sync::Arc};
use tuliprox_mpegts::transport_stream_buffer::{HlsTsTimestampProfile, TransportStreamBuffer};
use zeroize::Zeroizing;

pub const HLS_TERMINAL_TAIL_SEGMENT_COUNT: u16 = 12;

const AES_128_BLOCK_BYTES: usize = 16;

const HLS_SHARED_LIVE_ROUTE_MARKER: &str = "/hls/shared/live/";

struct HlsTerminalKeyMaterial {
    bytes: Zeroizing<[u8; AES_128_BLOCK_BYTES]>,
}

/// Immutable lease-bound evidence for one exact READY AES-128 key revision.
///
/// The physical cache key proves which committed object supplied the frozen
/// bytes. Terminal serving uses only the frozen bytes and never resolves the
/// session's current transient mapping or performs origin I/O.
#[derive(Clone)]
pub struct HlsTerminalKeyBinding {
    proxy_session_id: ProxySessionId,
    resource_id: TransientResourceId,
    route_extension: Arc<str>,
    source_cache_key: TransientObjectCacheKey,
    content_type: Arc<str>,
    material: Arc<HlsTerminalKeyMaterial>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HlsTerminalTailGeneration(pub u64);

#[derive(Debug, Clone)]
pub struct HlsTerminalMediaAsset {
    validated: Arc<HlsValidatedTerminalMediaAsset>,
}

#[derive(Debug)]
struct HlsValidatedTerminalMediaAsset {
    revision: u64,
    fingerprint: [u8; 32],
    container: HlsMediaContainer,
    track_signature: HlsTsTrackSignature,
    duration_ms: u64,
    duration_ticks_90khz: u64,
    timestamp_profile: Option<HlsTsTimestampProfile>,
    content_type: &'static str,
    renderer: Arc<TransportStreamBuffer>,
}

/// Immutable READY/cache evidence retained while a terminal plan is built and committed.
///
/// The private reader pins close the gap between the session snapshot and publishing
/// lease-specific GC protection for the resulting terminal plan.
pub struct HlsTerminalBaseEvidence {
    availability: Arc<[HlsTerminalBaseSegmentAvailability]>,
    track_resolution: Option<HlsTrackEvidenceResolution>,
    splice_evidence: Option<HlsTsSpliceEvidence>,
    track_base: Option<HlsTerminalBaseTrackIdentity>,
    timing: Option<HlsTerminalBaseTimingEvidence>,
    key_bindings: Arc<[HlsTerminalKeyBinding]>,
    read_protection: HlsTerminalBaseReadProtection,
}

/// Opaque ownership of the cache reader pins required by a pending terminal commit.
pub struct HlsTerminalCommitMediaGuard {
    read_protection: HlsTerminalBaseReadProtection,
}

struct HlsTerminalBaseReadProtection {
    accesses: Vec<Arc<CacheAccessState>>,
}

pub struct HlsTerminalBaseEvidencePreparation {
    probes: Vec<HlsTerminalBaseSegmentProbe>,
    read_protection: HlsTerminalBaseReadProtection,
}

struct HlsResolvedTerminalBaseEvidence {
    availability: Vec<HlsTerminalBaseSegmentAvailability>,
    key_bindings: Vec<HlsTerminalKeyBinding>,
    last_media_probe: Option<HlsTerminalMediaProbe>,
    last_track_preprobe_resolution: Option<HlsTrackEvidenceResolution>,
    track_base: Option<HlsTerminalBaseTrackIdentity>,
}

struct HlsTerminalBaseSegmentProbe {
    proxy_seq: u64,
    duration_ms: u64,
    media_ready: bool,
    required_map_ready: bool,
    cache_key: Option<super::SegmentCacheKey>,
    origin_epoch: Option<u64>,
    key_evidence: HlsTerminalBaseKeyEvidence,
}

struct HlsTerminalMediaProbe {
    segment_path: PathBuf,
    source_size: u64,
    expected_duration_ticks_90khz: u64,
    encryption: HlsTerminalTrackEncryption,
}

struct HlsTerminalBaseMediaEvidence {
    track_resolution: HlsTrackEvidenceResolution,
    timestamp_profile: Option<HlsTsTimestampProfile>,
    splice_evidence: HlsTsSpliceEvidence,
}

#[derive(Clone)]
pub struct HlsTerminalTailPlan {
    pub generation: HlsTerminalTailGeneration,
    pub created_at_ms: u64,
    pub base_manifest: HlsLeaseManifestSnapshot,
    pub protected_base_proxy_seqs: Arc<[u64]>,
    pub reason: HlsRuntimeCustomTailReason,
    pub asset_identity: HlsRuntimeCustomTailAssetIdentity,
    pub segment_count: u16,
    pub segment_duration_ms: u64,
    pub append_key_method_none: bool,
    key_bindings: Arc<[HlsTerminalKeyBinding]>,
    route_binding: HlsTerminalTailRouteBinding,
    manifest_body: Arc<str>,
    anchored_bundle: Arc<HlsAnchoredTerminalBundle>,
    segment_content_type: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsTerminalTailRouteBinding {
    public_path_prefix: Arc<str>,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsTerminalSpliceDiagnostic {
    result: &'static str,
    pid: Option<u16>,
    packet_index: Option<u64>,
    expected_cc: Option<u8>,
    actual_cc: Option<u8>,
    declared_pes_bytes: Option<u16>,
    observed_pes_bytes: Option<u64>,
}

struct HlsTerminalTailManifestRenderInput<'a> {
    generation: HlsTerminalTailGeneration,
    base_manifest: &'a HlsLeaseManifestSnapshot,
    protected_base_proxy_seqs: &'a [u64],
    asset: &'a HlsTerminalMediaAsset,
    asset_identity: HlsTerminalAssetIdentity,
    segment_count: u16,
    segment_duration_ms: u64,
    append_key_method_none: bool,
    route_binding: &'a HlsTerminalTailRouteBinding,
}

#[cfg(test)]
mod tests;

mod assets;
mod diagnostics;
mod evidence;
mod plan;
mod render;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use assets::{
    snapshot_terminal_media_asset, terminal_media_asset_identity, HlsEncryptionSignature, HlsMapSignature,
    HlsMediaContainer, HlsTerminalAssetIdentity,
};
#[cfg(test)]
use evidence::terminal_base_media_evidence;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use evidence::{
    pin_terminal_base_evidence, prepare_terminal_base_evidence, resolve_terminal_base_evidence,
    HlsTerminalBaseMediaState, HlsTerminalBaseProtection, HlsTerminalBaseSegmentAvailability,
    HlsTerminalBaseTimingEvidence, HlsTerminalBaseTrackIdentity,
};
use evidence::{HlsTerminalBaseKeyEvidence, HlsTerminalTrackEncryption};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use plan::{
    build_terminal_tail_plan, evaluate_terminal_tail_compatibility, HlsLeasePlaybackMode,
    HlsTerminalTailBoundaryEvidence, HlsTerminalTailBuildInput, HlsTerminalTailCompatibility,
    HlsTerminalTailCompatibilityInput,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use render::{terminal_tail_manifest_body, HlsTerminalSegmentPath, HlsTerminalTailRenderError};
