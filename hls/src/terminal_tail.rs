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
use bytes::Bytes;
use log::debug;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::AsyncReadExt;
use tuliprox_core::utils::{format_hls_duration_ms, hls_target_duration_secs};
use tuliprox_mpegts::transport_stream_buffer::{HlsTsTimestampProfile, TransportStreamBuffer};
use zeroize::Zeroizing;

pub const HLS_TERMINAL_TAIL_SEGMENT_COUNT: u16 = 12;
const AES_128_BLOCK_BYTES: usize = 16;
const HLS_SHARED_LIVE_ROUTE_MARKER: &str = "/hls/shared/live/";

struct HlsTerminalKeyMaterial {
    bytes: Zeroizing<[u8; AES_128_BLOCK_BYTES]>,
}

impl HlsTerminalKeyMaterial {
    fn from_slice(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; AES_128_BLOCK_BYTES] = bytes.try_into().ok()?;
        Some(Self { bytes: Zeroizing::new(bytes) })
    }

    fn as_bytes(&self) -> &[u8; AES_128_BLOCK_BYTES] { &self.bytes }
}

impl std::fmt::Debug for HlsTerminalKeyMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HlsTerminalKeyMaterial(<redacted>)")
    }
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

impl HlsTerminalKeyBinding {
    fn new(
        proxy_session_id: ProxySessionId,
        resource_id: TransientResourceId,
        route_extension: String,
        source_cache_key: TransientObjectCacheKey,
        content_type: String,
        bytes: &[u8],
    ) -> Option<Self> {
        Some(Self {
            proxy_session_id,
            resource_id,
            route_extension: Arc::from(route_extension),
            source_cache_key,
            content_type: Arc::from(content_type),
            material: Arc::new(HlsTerminalKeyMaterial::from_slice(bytes)?),
        })
    }

    fn matches_resource(&self, resource_file: &TransientResourceFile) -> bool {
        self.resource_id == resource_file.resource_id && self.route_extension.as_ref() == resource_file.extension
    }

    fn matches_route(&self, proxy_session_id: &ProxySessionId) -> bool { self.proxy_session_id == *proxy_session_id }

    pub fn content_type(&self) -> &str { &self.content_type }

    pub fn bytes(&self) -> Bytes { Bytes::copy_from_slice(self.material.as_bytes()) }

    #[cfg(any(test, feature = "test-support"))]
    pub fn source_cache_key(&self) -> &TransientObjectCacheKey { &self.source_cache_key }

    #[cfg(any(test, feature = "test-support"))]
    pub fn resource_id(&self) -> &TransientResourceId { &self.resource_id }
}

impl std::fmt::Debug for HlsTerminalKeyBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsTerminalKeyBinding")
            .field("proxy_session_id", &"<redacted>")
            .field("resource_id", &self.resource_id)
            .field("route_extension", &self.route_extension)
            .field("source_cache_key", &self.source_cache_key)
            .field("content_type", &self.content_type)
            .field("material", &"<redacted>")
            .finish()
    }
}

impl PartialEq for HlsTerminalKeyBinding {
    fn eq(&self, other: &Self) -> bool {
        self.proxy_session_id == other.proxy_session_id
            && self.resource_id == other.resource_id
            && self.route_extension == other.route_extension
            && self.source_cache_key == other.source_cache_key
            && self.content_type == other.content_type
            && self.material.as_bytes() == other.material.as_bytes()
    }
}

impl Eq for HlsTerminalKeyBinding {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HlsTerminalTailGeneration(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsMediaContainer {
    MpegTs,
    FragmentedMp4,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsMapSignature {
    pub fingerprint: [u8; 32],
    pub container: HlsMediaContainer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsEncryptionSignature {
    pub method: String,
    pub key_uri: Option<String>,
    pub iv: Option<String>,
    pub key_format: Option<String>,
    pub key_format_versions: Option<String>,
    pub can_reset_to_clear: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedEncryptionMethod<'a> {
    None,
    Aes128,
    Unsupported(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsTerminalBaseScope {
    SafeSuffix,
    LastSafeSegment,
}

fn parse_encryption_method(method: &str) -> ParsedEncryptionMethod<'_> {
    if method.eq_ignore_ascii_case("NONE") {
        ParsedEncryptionMethod::None
    } else if method.eq_ignore_ascii_case("AES-128") {
        ParsedEncryptionMethod::Aes128
    } else {
        ParsedEncryptionMethod::Unsupported(method)
    }
}

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

impl PartialEq for HlsTerminalMediaAsset {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
            && self.container() == other.container()
            && self.track_signature() == other.track_signature()
            && self.duration_ms() == other.duration_ms()
            && self.duration_ticks_90khz() == other.duration_ticks_90khz()
            && self.timestamp_profile() == other.timestamp_profile()
            && self.content_type() == other.content_type()
    }
}

impl Eq for HlsTerminalMediaAsset {}

impl HlsTerminalMediaAsset {
    fn identity(&self) -> HlsTerminalAssetIdentity {
        HlsTerminalAssetIdentity { revision: self.validated.revision, fingerprint: self.validated.fingerprint }
    }

    pub fn container(&self) -> HlsMediaContainer { self.validated.container }

    pub fn track_signature(&self) -> &HlsTsTrackSignature { &self.validated.track_signature }

    pub fn duration_ms(&self) -> u64 { self.validated.duration_ms }

    pub fn duration_ticks_90khz(&self) -> u64 { self.validated.duration_ticks_90khz }

    pub fn timestamp_profile(&self) -> Option<HlsTsTimestampProfile> { self.validated.timestamp_profile }

    pub fn content_type(&self) -> &'static str { self.validated.content_type }

    pub(super) fn renderer(&self) -> Arc<TransportStreamBuffer> { Arc::clone(&self.validated.renderer) }
}

/// Captures and validates one immutable revision of the configured terminal TS asset.
pub fn snapshot_terminal_media_asset(
    buffer: &TransportStreamBuffer,
) -> Result<Arc<HlsTerminalMediaAsset>, HlsTerminalTailCompatibility> {
    let (Some(duration_ms), Some(duration_ticks_90khz), Some(track_signature)) =
        (buffer.duration_ms(), buffer.duration_ticks_90khz(), buffer.finite_hls_track_signature())
    else {
        return Err(HlsTerminalTailCompatibility::InvalidAsset);
    };
    let bytes = buffer.clone_bytes();
    if bytes.is_empty() {
        return Err(HlsTerminalTailCompatibility::InvalidAsset);
    }
    let fingerprint = buffer.finite_hls_asset_fingerprint();
    let identity = terminal_asset_identity_from_fingerprint(fingerprint);
    Ok(Arc::new(HlsTerminalMediaAsset {
        validated: Arc::new(HlsValidatedTerminalMediaAsset {
            revision: identity.revision,
            fingerprint: identity.fingerprint,
            container: HlsMediaContainer::MpegTs,
            track_signature,
            duration_ms,
            duration_ticks_90khz,
            timestamp_profile: buffer.finite_hls_timestamp_profile(),
            content_type: "video/mp2t",
            renderer: Arc::new(buffer.clone()),
        }),
    }))
}

/// Reads only cached validation metadata from the configured buffer. This is
/// suitable for the final commit CAS and never parses, hashes, or clones media.
pub fn terminal_media_asset_identity(buffer: &TransportStreamBuffer) -> Option<HlsTerminalAssetIdentity> {
    if buffer.as_bytes().is_empty()
        || buffer.duration_ms().is_none()
        || buffer.duration_ticks_90khz().is_none()
        || !buffer.has_finite_hls_track_signature()
    {
        return None;
    }
    Some(terminal_asset_identity_from_fingerprint(buffer.finite_hls_asset_fingerprint()))
}

fn terminal_asset_identity_from_fingerprint(fingerprint: [u8; 32]) -> HlsTerminalAssetIdentity {
    let mut revision_bytes = [0_u8; std::mem::size_of::<u64>()];
    let revision_len = revision_bytes.len();
    revision_bytes.copy_from_slice(&fingerprint[..revision_len]);
    HlsTerminalAssetIdentity { revision: u64::from_be_bytes(revision_bytes), fingerprint }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HlsTerminalAssetIdentity {
    pub revision: u64,
    pub fingerprint: [u8; 32],
}

impl HlsTerminalAssetIdentity {
    pub fn from_asset(asset: &HlsTerminalMediaAsset) -> Self { asset.identity() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalBaseMediaState {
    Ready,
    NotReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalBaseProtection {
    Protectable,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTerminalBaseSegmentAvailability {
    pub proxy_seq: u64,
    pub media_state: HlsTerminalBaseMediaState,
    pub required_map_ready: bool,
    pub required_key_ready: bool,
    pub protection: HlsTerminalBaseProtection,
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

impl HlsTerminalBaseEvidence {
    pub fn availability(&self) -> Arc<[HlsTerminalBaseSegmentAvailability]> { Arc::clone(&self.availability) }

    pub fn track_signature(&self) -> Option<HlsTsTrackSignature> {
        self.track_resolution.as_ref().and_then(HlsTrackEvidenceResolution::signature).cloned()
    }

    pub fn track_resolution(&self) -> Option<&HlsTrackEvidenceResolution> { self.track_resolution.as_ref() }

    pub fn splice_evidence(&self) -> Option<&HlsTsSpliceEvidence> { self.splice_evidence.as_ref() }

    pub fn track_evidence_reason_code(&self) -> &'static str {
        self.track_resolution.as_ref().map_or("base-not-ready", HlsTrackEvidenceResolution::reason_code)
    }

    pub fn track_base(&self) -> Option<&HlsTerminalBaseTrackIdentity> { self.track_base.as_ref() }

    pub fn timing(&self) -> Option<&HlsTerminalBaseTimingEvidence> { self.timing.as_ref() }

    pub fn key_bindings(&self) -> Arc<[HlsTerminalKeyBinding]> { Arc::clone(&self.key_bindings) }

    /// Makes the intended guard lifetime explicit at the endpoint commit boundary.
    #[cfg(any(test, feature = "test-support"))]
    pub fn release(self) { drop(self.read_protection) }

    /// Transfers the READY-object reader pins to an autonomous terminal commit.
    ///
    /// The returned guard must stay alive until the lease/session transaction
    /// either installs durable terminal-tail protection or reaches a terminal
    /// rejection. This closes the request-cancellation and `LockBusy` gap.
    pub fn into_commit_guard(self) -> HlsTerminalCommitMediaGuard {
        HlsTerminalCommitMediaGuard { read_protection: self.read_protection }
    }
}

/// Opaque ownership of the cache reader pins required by a pending terminal commit.
pub struct HlsTerminalCommitMediaGuard {
    read_protection: HlsTerminalBaseReadProtection,
}

#[cfg(any(test, feature = "test-support"))]
impl HlsTerminalCommitMediaGuard {
    pub fn empty_for_test() -> Self { Self { read_protection: HlsTerminalBaseReadProtection { accesses: Vec::new() } } }
}

impl std::fmt::Debug for HlsTerminalCommitMediaGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsTerminalCommitMediaGuard")
            .field("pinned_objects", &self.read_protection.accesses.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTerminalBaseTrackIdentity {
    pub proxy_seq: u64,
    pub origin_epoch: u64,
    pub cache_key: super::SegmentCacheKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsTerminalBaseTimingEvidence {
    pub base: HlsTerminalBaseTrackIdentity,
    pub profile: HlsTsTimestampProfile,
}

struct HlsTerminalBaseReadProtection {
    accesses: Vec<Arc<CacheAccessState>>,
}

impl Drop for HlsTerminalBaseReadProtection {
    fn drop(&mut self) {
        for access in &self.accesses {
            access.reader_finished();
        }
    }
}

/// Reads cache metadata and the last READY base segment without holding a session lock across I/O.
pub async fn prepare_terminal_base_evidence(
    session: &HlsSessionHandle,
    segment_cache: &HlsSegmentCache,
    manifest: &HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsTerminalBaseEvidence {
    let preparation = {
        let session = session.read().await;
        pin_terminal_base_evidence(&session, manifest, now_ms)
    };
    resolve_terminal_base_evidence(segment_cache, manifest, preparation).await
}

/// Pins the exact READY media and key objects represented by one frozen lease
/// manifest. Callers can perform this while holding the same session read lock
/// used to select the lease, closing the selection-to-GC race without I/O.
pub fn pin_terminal_base_evidence(
    session: &super::HlsSession,
    manifest: &HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsTerminalBaseEvidencePreparation {
    snapshot_terminal_base_probes(session, manifest, now_ms)
}

/// Resolves already pinned base objects and inspects the exact terminal track
/// basis. No session lock is held while cache metadata and bytes are read.
pub async fn resolve_terminal_base_evidence(
    segment_cache: &HlsSegmentCache,
    manifest: &HlsLeaseManifestSnapshot,
    preparation: HlsTerminalBaseEvidencePreparation,
) -> HlsTerminalBaseEvidence {
    let resolved = resolve_terminal_base_probes(segment_cache, manifest, preparation.probes).await;
    let media_evidence = match resolved.last_media_probe {
        Some(probe) => Some(terminal_base_media_evidence(probe).await),
        None => None,
    };
    let track_resolution = match media_evidence.as_ref() {
        Some(evidence) => Some(evidence.track_resolution.clone()),
        None => resolved.last_track_preprobe_resolution,
    };
    let splice_evidence = media_evidence.as_ref().map(|evidence| evidence.splice_evidence.clone());
    if track_resolution.as_ref().and_then(HlsTrackEvidenceResolution::signature).is_none() {
        debug!(
            "HLS terminal base track evidence unavailable: reason={}",
            track_resolution.as_ref().map_or("base-not-ready", HlsTrackEvidenceResolution::reason_code)
        );
    }
    HlsTerminalBaseEvidence {
        availability: resolved.availability.into(),
        track_resolution,
        splice_evidence,
        timing: media_evidence
            .and_then(|evidence| evidence.timestamp_profile)
            .zip(resolved.track_base.clone())
            .map(|(profile, base)| HlsTerminalBaseTimingEvidence { base, profile }),
        track_base: resolved.track_base,
        key_bindings: resolved.key_bindings.into(),
        read_protection: preparation.read_protection,
    }
}

pub struct HlsTerminalBaseEvidencePreparation {
    probes: Vec<HlsTerminalBaseSegmentProbe>,
    read_protection: HlsTerminalBaseReadProtection,
}

fn snapshot_terminal_base_probes(
    session: &super::HlsSession,
    manifest: &HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsTerminalBaseEvidencePreparation {
    let mut accesses = Vec::with_capacity(manifest.visible_segments.len());
    let mut pinned_key_objects = HashSet::new();
    let probes = manifest
        .visible_segments
        .iter()
        .map(|segment| {
            let entry = session.segments.get(&segment.proxy_seq);
            let media_ready = entry.is_some_and(|entry| matches!(&entry.status, SegmentCacheStatus::Ready { .. }));
            let required_map_ready = entry.is_some_and(|entry| {
                entry.map_ref.is_none_or(|map_id| {
                    session
                        .maps
                        .get(&map_id)
                        .is_some_and(|map| matches!(&map.status, super::map::MapCacheStatus::Ready { .. }))
                })
            });
            let key_evidence = terminal_base_key_evidence(session, entry, segment.encryption.as_ref());
            if let HlsTerminalBaseKeyEvidence::Aes128 { cache_key, object_access, resource_access, .. } = &key_evidence
            {
                if pinned_key_objects.insert(cache_key.clone()) {
                    object_access.reader_started(now_ms);
                    resource_access.reader_started(now_ms);
                    accesses.push(Arc::clone(object_access));
                    accesses.push(Arc::clone(resource_access));
                }
            }
            let cache_key = media_ready.then(|| entry.map(|entry| entry.cache_key.clone())).flatten();
            if let Some(access) = media_ready.then(|| entry.map(|entry| Arc::clone(&entry.access))).flatten() {
                // GC takes the session write lock before selecting removals, so acquiring
                // the pin under this read lock cannot race behind an already queued removal.
                access.reader_started(now_ms);
                accesses.push(access);
            }
            HlsTerminalBaseSegmentProbe {
                proxy_seq: segment.proxy_seq,
                duration_ms: segment.duration_ms,
                media_ready,
                required_map_ready,
                cache_key,
                origin_epoch: media_ready.then(|| entry.map(|entry| entry.origin_key.origin_epoch)).flatten(),
                key_evidence,
            }
        })
        .collect();
    HlsTerminalBaseEvidencePreparation { probes, read_protection: HlsTerminalBaseReadProtection { accesses } }
}

struct HlsResolvedTerminalBaseEvidence {
    availability: Vec<HlsTerminalBaseSegmentAvailability>,
    key_bindings: Vec<HlsTerminalKeyBinding>,
    last_media_probe: Option<HlsTerminalMediaProbe>,
    last_track_preprobe_resolution: Option<HlsTrackEvidenceResolution>,
    track_base: Option<HlsTerminalBaseTrackIdentity>,
}

async fn resolve_terminal_base_probes(
    segment_cache: &HlsSegmentCache,
    manifest: &HlsLeaseManifestSnapshot,
    probes: Vec<HlsTerminalBaseSegmentProbe>,
) -> HlsResolvedTerminalBaseEvidence {
    let mut availability = Vec::with_capacity(probes.len());
    let mut key_bindings = Vec::new();
    let mut key_binding_cache: HashMap<TransientObjectCacheKey, Option<HlsTerminalKeyBinding>> = HashMap::new();
    let mut last_media_probe = None;
    let mut last_track_preprobe_resolution = None;
    let mut track_base = None;
    for probe in probes {
        let metadata = if let Some(cache_key) = probe.cache_key.as_ref() {
            segment_cache.metadata(cache_key).await.ok().flatten()
        } else {
            None
        };
        let protectable = metadata.is_some();
        let (required_key_ready, track_encryption, key_failure) = match probe.key_evidence {
            HlsTerminalBaseKeyEvidence::Clear => (true, Some(HlsTerminalTrackEncryption::Clear), None),
            HlsTerminalBaseKeyEvidence::Aes128 {
                proxy_session_id,
                cache_key,
                resource_id,
                route_extension,
                content_type,
                iv,
                ..
            } => {
                let binding = match key_binding_cache.entry(cache_key.clone()) {
                    Entry::Occupied(entry) => entry.get().clone(),
                    Entry::Vacant(entry) => {
                        let key_metadata = segment_cache.metadata(&cache_key).await.ok().flatten();
                        let binding = if let Some(metadata) =
                            key_metadata.filter(|metadata| metadata.size == AES_128_BLOCK_BYTES as u64)
                        {
                            let bytes = read_bounded_file(&metadata.path, AES_128_BLOCK_BYTES as u64).await;
                            bytes.as_deref().and_then(|bytes| {
                                HlsTerminalKeyBinding::new(
                                    proxy_session_id,
                                    resource_id,
                                    route_extension,
                                    cache_key,
                                    content_type,
                                    bytes,
                                )
                            })
                        } else {
                            None
                        };
                        if let Some(binding) = &binding {
                            key_bindings.push(binding.clone());
                        }
                        entry.insert(binding).clone()
                    }
                };
                let ready = binding.is_some();
                let encryption = binding.as_ref().map(|binding| HlsTerminalTrackEncryption::Aes128 {
                    key_material: Arc::clone(&binding.material),
                    iv,
                });
                let failure = (!ready).then_some(HlsTrackEvidenceResolution::KeyUnavailable);
                (ready, encryption, failure)
            }
            HlsTerminalBaseKeyEvidence::Unavailable { resolution } => (false, None, Some(resolution)),
        };
        if probe.proxy_seq == manifest.last_proxy_seq && probe.media_ready && probe.required_map_ready {
            if let (Some(cache_key), Some(origin_epoch)) = (probe.cache_key.as_ref(), probe.origin_epoch) {
                track_base = Some(HlsTerminalBaseTrackIdentity {
                    proxy_seq: probe.proxy_seq,
                    origin_epoch,
                    cache_key: cache_key.clone(),
                });
                if required_key_ready {
                    if let (Some(metadata), Some(encryption)) = (metadata.as_ref(), track_encryption) {
                        last_media_probe = Some(HlsTerminalMediaProbe {
                            segment_path: metadata.path.clone(),
                            source_size: metadata.size,
                            expected_duration_ticks_90khz: probe.duration_ms.saturating_mul(90),
                            encryption,
                        });
                    }
                } else {
                    last_track_preprobe_resolution = key_failure;
                }
            }
        }
        availability.push(HlsTerminalBaseSegmentAvailability {
            proxy_seq: probe.proxy_seq,
            media_state: terminal_base_media_state(probe.media_ready, protectable),
            required_map_ready: probe.required_map_ready,
            required_key_ready,
            protection: terminal_base_protection(protectable),
        });
    }
    HlsResolvedTerminalBaseEvidence {
        availability,
        key_bindings,
        last_media_probe,
        last_track_preprobe_resolution,
        track_base,
    }
}

const fn terminal_base_media_state(media_ready: bool, protectable: bool) -> HlsTerminalBaseMediaState {
    if media_ready && protectable {
        HlsTerminalBaseMediaState::Ready
    } else {
        HlsTerminalBaseMediaState::NotReady
    }
}

const fn terminal_base_protection(protectable: bool) -> HlsTerminalBaseProtection {
    if protectable {
        HlsTerminalBaseProtection::Protectable
    } else {
        HlsTerminalBaseProtection::Unavailable
    }
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

enum HlsTerminalBaseKeyEvidence {
    Clear,
    Aes128 {
        proxy_session_id: ProxySessionId,
        cache_key: TransientObjectCacheKey,
        resource_id: TransientResourceId,
        route_extension: String,
        content_type: String,
        iv: [u8; AES_128_BLOCK_BYTES],
        object_access: Arc<CacheAccessState>,
        resource_access: Arc<CacheAccessState>,
    },
    Unavailable {
        resolution: HlsTrackEvidenceResolution,
    },
}

impl HlsTerminalBaseKeyEvidence {
    fn unavailable(resolution: HlsTrackEvidenceResolution) -> Self { Self::Unavailable { resolution } }
}

fn terminal_base_key_evidence(
    session: &super::HlsSession,
    timeline_entry: Option<&SegmentEntry>,
    encryption: Option<&HlsEncryptionSignature>,
) -> HlsTerminalBaseKeyEvidence {
    let Some(encryption) = encryption else {
        return if timeline_entry.is_none_or(|entry| entry.encryption.is_none()) {
            HlsTerminalBaseKeyEvidence::Clear
        } else {
            HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::IncompleteEvidence)
        };
    };
    if parse_encryption_method(&encryption.method) != ParsedEncryptionMethod::Aes128 {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::UnsupportedProtection(
            super::HlsTsProtectionReason::UnsupportedEncryption,
        ));
    }
    let Some(resource_file) = encryption
        .key_uri
        .as_deref()
        .and_then(|uri| uri.split(['?', '#']).next())
        .and_then(|path| path.rsplit('/').next())
        .and_then(TransientResourceFile::parse)
    else {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    };
    let Some(timeline_entry) = timeline_entry else {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    };
    let Some(timeline_encryption) = timeline_entry.encryption.as_ref() else {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    };
    if timeline_encryption.resource_id != resource_file.resource_id
        || timeline_encryption.resource_extension != resource_file.extension
        || timeline_encryption.iv != encryption.iv
        || timeline_encryption.key_format != encryption.key_format
        || timeline_encryption.key_format_versions != encryption.key_format_versions
    {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    }
    let iv = match super::hls_aes128_cbc_iv(encryption.iv.as_deref(), timeline_entry.origin_key.host_local_sequence) {
        Ok(iv) => iv,
        Err(error) => {
            return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::from(Err(error)));
        }
    };
    let Some(resource) = session.transient.resources.get(&resource_file.resource_id) else {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    };
    if resource.kind != TransientResourceKind::Key
        || resource.file_ext_hint.as_deref() != Some(resource_file.extension.as_str())
    {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    }
    let key = TransientPassthroughState::transient_object_key(
        &session.proxy_session_id,
        &resource_file.resource_id,
        resource_file.extension.clone(),
    );
    let Some(entry) = session.transient.object_cache.get(&key) else {
        return HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable);
    };
    // TTL controls admission to new live manifests, not whether exact READY key
    // bytes referenced by this frozen lease snapshot may be pinned for terminal
    // evidence. The reader pins acquired below close the GC race without any
    // post-terminal origin fetch.
    let ready = matches!(entry.status, super::TransientObjectCacheStatus::Ready { .. });
    if ready {
        HlsTerminalBaseKeyEvidence::Aes128 {
            proxy_session_id: session.proxy_session_id.clone(),
            cache_key: entry.key.clone(),
            resource_id: resource_file.resource_id,
            route_extension: resource_file.extension,
            content_type: entry.content_type.clone(),
            iv,
            object_access: Arc::clone(&entry.access),
            resource_access: Arc::clone(&resource.access),
        }
    } else {
        HlsTerminalBaseKeyEvidence::unavailable(HlsTrackEvidenceResolution::KeyUnavailable)
    }
}

enum HlsTerminalTrackEncryption {
    Clear,
    Aes128 { key_material: Arc<HlsTerminalKeyMaterial>, iv: [u8; AES_128_BLOCK_BYTES] },
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

async fn terminal_base_media_evidence(probe: HlsTerminalMediaProbe) -> HlsTerminalBaseMediaEvidence {
    let file = match tokio::fs::File::open(&probe.segment_path).await {
        Ok(file) => file,
        Err(error) => {
            return HlsTerminalBaseMediaEvidence {
                track_resolution: HlsTrackEvidenceResolution::Io(error.kind()),
                timestamp_profile: None,
                splice_evidence: HlsTsSpliceEvidence::Incompatible(
                    super::HlsTsSpliceIncompatibility::TopologyUnavailable,
                ),
            };
        }
    };
    let budget = HlsTsProbeBudget {
        max_bytes: probe.source_size.saturating_add(1),
        max_packets: probe.source_size.saturating_add(187).saturating_div(188).saturating_add(1),
        ..HlsTsProbeBudget::default()
    };
    let outcome = match probe.encryption {
        HlsTerminalTrackEncryption::Clear => {
            super::inspect_mpeg_ts_media_evidence_async(
                file,
                HlsTsProbeProtection::Clear,
                budget,
                probe.expected_duration_ticks_90khz,
            )
            .await
        }
        HlsTerminalTrackEncryption::Aes128 { key_material, iv } => {
            super::inspect_mpeg_ts_media_evidence_async(
                file,
                HlsTsProbeProtection::Aes128Cbc { key: key_material.as_bytes(), iv },
                budget,
                probe.expected_duration_ticks_90khz,
            )
            .await
        }
    };
    match outcome {
        Ok(evidence) => {
            let super::HlsTsMediaEvidence { track_outcome, timestamp_profile, splice_evidence } = evidence;
            HlsTerminalBaseMediaEvidence {
                track_resolution: HlsTrackEvidenceResolution::from(Ok(track_outcome)),
                timestamp_profile,
                splice_evidence,
            }
        }
        Err(error) => HlsTerminalBaseMediaEvidence {
            track_resolution: HlsTrackEvidenceResolution::from(Err(error)),
            timestamp_profile: None,
            splice_evidence: HlsTsSpliceEvidence::Incompatible(super::HlsTsSpliceIncompatibility::TopologyUnavailable),
        },
    }
}

async fn read_bounded_file(path: &Path, max_bytes: u64) -> Option<Zeroizing<Vec<u8>>> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut limited = file.take(max_bytes.saturating_add(1));
    let mut bytes = Zeroizing::new(Vec::new());
    limited.read_to_end(&mut bytes).await.ok()?;
    (u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= max_bytes).then_some(bytes)
}

#[derive(Clone)]
pub struct HlsTerminalTailBuildInput {
    pub generation: HlsTerminalTailGeneration,
    pub created_at_ms: u64,
    pub base_manifest: HlsLeaseManifestSnapshot,
    pub base_availability: Arc<[HlsTerminalBaseSegmentAvailability]>,
    pub base_track_signature: Option<HlsTsTrackSignature>,
    pub base_splice_evidence: Option<HlsTsSpliceEvidence>,
    pub terminal_splice_evidence: Option<HlsTsSpliceEvidence>,
    pub base_timing: Option<HlsTerminalBaseTimingEvidence>,
    pub base_key_bindings: Arc<[HlsTerminalKeyBinding]>,
    pub expected_asset: HlsRuntimeCustomTailAssetIdentity,
    pub asset: Arc<HlsTerminalMediaAsset>,
    pub anchored_bundle: Arc<HlsAnchoredTerminalBundle>,
}

#[cfg(any(test, feature = "test-support"))]
impl HlsTerminalTailBuildInput {
    /// # Panics
    ///
    /// Panics if the asset cannot produce a bundle, timing profile or splice
    /// anchor. Test-support only: a fixture that cannot be built is a bug in
    /// the test, not a runtime condition.
    pub fn anchored_bundle_for_test(
        asset: &HlsTerminalMediaAsset,
        target_duration_ms: u64,
    ) -> Arc<HlsAnchoredTerminalBundle> {
        let key = HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity::from_asset(asset),
            target_duration_ms,
            segment_count: HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        };
        let prepared = super::prepared_terminal_bundle::build_prepared_terminal_bundle(asset, key)
            .expect("relative terminal test bundle");
        let profile = asset.timestamp_profile().expect("terminal test asset timing profile");
        let anchor = tuliprox_mpegts::transport_stream_buffer::HlsTsSpliceAnchor::between(profile, profile)
            .expect("terminal test splice anchor");
        super::prepared_terminal_bundle::anchor_prepared_terminal_bundle(asset, &prepared, anchor)
            .expect("anchored terminal test bundle")
    }

    /// # Panics
    ///
    /// Panics if the manifest does not describe a usable base track.
    /// Test-support only.
    pub fn base_timing_for_test(
        asset: &HlsTerminalMediaAsset,
        manifest: &HlsLeaseManifestSnapshot,
    ) -> HlsTerminalBaseTimingEvidence {
        HlsTerminalBaseTimingEvidence {
            base: HlsTerminalBaseTrackIdentity {
                proxy_seq: manifest.last_proxy_seq,
                origin_epoch: 1,
                cache_key: super::SegmentCacheKey::new(
                    ProxySessionId("terminal-timing-test".to_string()),
                    manifest.last_proxy_seq,
                    "ts",
                ),
            },
            profile: asset.timestamp_profile().expect("terminal test asset timing profile"),
        }
    }

    pub fn compatible_splice_evidence_for_test(asset: &HlsTerminalMediaAsset) -> HlsTsSpliceEvidence {
        HlsTsSpliceEvidence::compatible_for_test(asset.track_signature().clone())
    }
}

#[derive(Clone, Copy)]
pub struct HlsTerminalTailCompatibilityInput<'a> {
    pub manifest: &'a HlsLeaseManifestSnapshot,
    pub base_track_signature: Option<&'a HlsTsTrackSignature>,
    pub boundary_evidence: HlsTerminalTailBoundaryEvidence<'a>,
    pub expected_asset: HlsTerminalAssetIdentity,
    pub asset: &'a HlsTerminalMediaAsset,
}

#[derive(Clone, Copy)]
pub enum HlsTerminalTailBoundaryEvidence<'a> {
    StructuralOnly,
    Exact { base: Option<&'a HlsTsSpliceEvidence>, terminal: Option<&'a HlsTsSpliceEvidence> },
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

impl std::fmt::Debug for HlsTerminalTailPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsTerminalTailPlan")
            .field("generation", &self.generation)
            .field("reason", &self.reason)
            .field("created_at_ms", &self.created_at_ms)
            .field("base_manifest_generation", &self.base_manifest.snapshot_generation)
            .field("base_segment_count", &self.base_manifest.visible_segments.len())
            .field("protected_base_segment_count", &self.protected_base_proxy_seqs.len())
            .field("terminal_segment_count", &self.segment_count)
            .field("segment_duration_ms", &self.segment_duration_ms)
            .field("append_key_method_none", &self.append_key_method_none)
            .field("key_binding_count", &self.key_bindings.len())
            .field("manifest_byte_len", &self.manifest_body.len())
            .field("segment_content_type", &self.segment_content_type)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsTerminalTailRouteBinding {
    public_path_prefix: Arc<str>,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
}

impl PartialEq for HlsTerminalTailPlan {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.created_at_ms == other.created_at_ms
            && self.base_manifest == other.base_manifest
            && self.protected_base_proxy_seqs == other.protected_base_proxy_seqs
            && self.reason == other.reason
            && self.asset_identity == other.asset_identity
            && self.segment_count == other.segment_count
            && self.segment_duration_ms == other.segment_duration_ms
            && self.append_key_method_none == other.append_key_method_none
            && self.key_bindings == other.key_bindings
            && self.route_binding == other.route_binding
            && self.manifest_body == other.manifest_body
            && self.anchored_bundle.prepared_key == other.anchored_bundle.prepared_key
            && self.anchored_bundle.splice_anchor == other.anchored_bundle.splice_anchor
            && self.segment_content_type == other.segment_content_type
    }
}

impl Eq for HlsTerminalTailPlan {}

impl HlsTerminalTailPlan {
    pub fn media_preparation_key(&self) -> super::recovery_timing::HlsTerminalMediaPreparationKey {
        self.anchored_bundle.prepared_key
    }

    pub fn matches_route(&self, proxy_session_id: &ProxySessionId, lease_id: &HlsAccessLeaseId) -> bool {
        self.route_binding.proxy_session_id == *proxy_session_id && self.route_binding.lease_id == *lease_id
    }

    pub fn segment_content_length(&self, path: HlsTerminalSegmentPath) -> Option<u64> {
        self.prepared_segment(path).and_then(|segment| u64::try_from(segment.bytes.len()).ok())
    }

    pub fn segment_content_type(&self) -> &'static str { self.segment_content_type }

    /// Clones one immutable pre-rendered segment only for this plan generation.
    pub fn segment_bytes(&self, path: HlsTerminalSegmentPath) -> Option<Bytes> {
        self.prepared_segment(path).map(|segment| segment.bytes.clone())
    }

    fn prepared_segment(&self, path: HlsTerminalSegmentPath) -> Option<&HlsAnchoredTerminalSegment> {
        if path.generation != self.generation || path.index >= self.segment_count {
            return None;
        }
        let segment = self.anchored_bundle.segments.get(usize::from(path.index))?;
        (segment.index == path.index).then_some(segment)
    }

    pub fn terminal_key_binding(
        &self,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        resource_file: &TransientResourceFile,
    ) -> Option<HlsTerminalKeyBinding> {
        self.matches_route(proxy_session_id, lease_id)
            .then_some(())
            .and_then(|()| {
                self.key_bindings
                    .iter()
                    .find(|binding| binding.matches_route(proxy_session_id) && binding.matches_resource(resource_file))
            })
            .cloned()
    }

    pub fn key_bindings(&self) -> Arc<[HlsTerminalKeyBinding]> { Arc::clone(&self.key_bindings) }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HlsLeasePlaybackMode {
    #[default]
    Live,
    TerminalTail(Arc<HlsTerminalTailPlan>),
    TerminalUnavailable {
        decision_generation: u64,
        reason: HlsTerminalTailCompatibility,
    },
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalTailCompatibility {
    Compatible,
    MissingAsset,
    TerminalMediaNotReady,
    InvalidAsset,
    AssetRevisionMismatch,
    MissingSafeBase,
    TargetDurationExceeded { asset_ms: u64, target_ms: u64 },
    ActiveMapRequiresCompatibleFallback,
    UnsupportedEncryptionTransition,
    ContainerMismatch,
    MissingTrackSignature,
    TrackLayoutMismatch,
    MissingSpliceEvidence,
    SpliceTransportFailure(super::HlsTsSpliceIncompatibility),
    SpliceTopologyMismatch,
    MissingTimestampAnchor,
    InvalidTimestampTransition,
    TransientPassthroughUnsupported,
    ProtectionCapacityExceeded,
    InvalidLeaseRoute,
    ManifestRenderFailed,
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

impl HlsTerminalSpliceDiagnostic {
    fn from_compatibility(compatibility: HlsTerminalTailCompatibility) -> Option<Self> {
        let empty = |result| Self {
            result,
            pid: None,
            packet_index: None,
            expected_cc: None,
            actual_cc: None,
            declared_pes_bytes: None,
            observed_pes_bytes: None,
        };
        match compatibility {
            HlsTerminalTailCompatibility::Compatible => Some(empty("compatible")),
            HlsTerminalTailCompatibility::MissingTrackSignature
            | HlsTerminalTailCompatibility::TrackLayoutMismatch
            | HlsTerminalTailCompatibility::MissingSpliceEvidence
            | HlsTerminalTailCompatibility::SpliceTopologyMismatch => Some(empty("topology-mismatch")),
            HlsTerminalTailCompatibility::SpliceTransportFailure(reason) => {
                let mut diagnostic = empty(reason.result_code());
                match reason {
                    super::HlsTsSpliceIncompatibility::InvalidPacket { packet_index }
                    | super::HlsTsSpliceIncompatibility::TransportError { packet_index, .. } => {
                        diagnostic.packet_index = Some(packet_index);
                        if let super::HlsTsSpliceIncompatibility::TransportError { pid, .. } = reason {
                            diagnostic.pid = Some(pid);
                        }
                    }
                    super::HlsTsSpliceIncompatibility::ContinuityFailure { pid, packet_index, expected, actual } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                        diagnostic.expected_cc = Some(expected);
                        diagnostic.actual_cc = Some(actual);
                    }
                    super::HlsTsSpliceIncompatibility::IncompletePes {
                        pid,
                        packet_index,
                        declared_bytes,
                        observed_bytes,
                    } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                        diagnostic.declared_pes_bytes = declared_bytes;
                        diagnostic.observed_pes_bytes = Some(observed_bytes);
                    }
                    super::HlsTsSpliceIncompatibility::InvalidPes { pid, packet_index } => {
                        diagnostic.pid = Some(pid);
                        diagnostic.packet_index = Some(packet_index);
                    }
                    super::HlsTsSpliceIncompatibility::InspectionBudgetExhausted
                    | super::HlsTsSpliceIncompatibility::TopologyUnavailable => {}
                }
                Some(diagnostic)
            }
            HlsTerminalTailCompatibility::MissingAsset
            | HlsTerminalTailCompatibility::TerminalMediaNotReady
            | HlsTerminalTailCompatibility::InvalidAsset
            | HlsTerminalTailCompatibility::AssetRevisionMismatch
            | HlsTerminalTailCompatibility::MissingSafeBase
            | HlsTerminalTailCompatibility::TargetDurationExceeded { .. }
            | HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback
            | HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
            | HlsTerminalTailCompatibility::ContainerMismatch
            | HlsTerminalTailCompatibility::MissingTimestampAnchor
            | HlsTerminalTailCompatibility::InvalidTimestampTransition
            | HlsTerminalTailCompatibility::TransientPassthroughUnsupported
            | HlsTerminalTailCompatibility::ProtectionCapacityExceeded
            | HlsTerminalTailCompatibility::InvalidLeaseRoute
            | HlsTerminalTailCompatibility::ManifestRenderFailed => None,
        }
    }
}

fn format_optional_diagnostic<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn log_terminal_splice_compatibility(base_proxy_seq: u64, compatibility: HlsTerminalTailCompatibility) {
    let Some(diagnostic) = HlsTerminalSpliceDiagnostic::from_compatibility(compatibility) else {
        return;
    };
    debug!(
        "HLS terminal TS splice eligibility: base_proxy_seq={} result={} pid={} packet_index={} \
         expected_cc={} actual_cc={} declared_pes_bytes={} observed_pes_bytes={}",
        base_proxy_seq,
        diagnostic.result,
        format_optional_diagnostic(diagnostic.pid),
        format_optional_diagnostic(diagnostic.packet_index),
        format_optional_diagnostic(diagnostic.expected_cc),
        format_optional_diagnostic(diagnostic.actual_cc),
        format_optional_diagnostic(diagnostic.declared_pes_bytes),
        format_optional_diagnostic(diagnostic.observed_pes_bytes),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsTerminalSegmentPath {
    pub generation: HlsTerminalTailGeneration,
    pub index: u16,
}

impl HlsTerminalSegmentPath {
    pub fn parse(generation: &str, terminal_file: &str) -> Option<Self> {
        let index = terminal_file.strip_suffix(".ts")?;
        Some(Self {
            generation: HlsTerminalTailGeneration(parse_canonical_decimal(generation)?),
            index: parse_canonical_decimal(index)?,
        })
    }
}

fn parse_canonical_decimal<T>(value: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HlsTerminalTailRenderError {
    #[error("terminal tail has no safe base segments")]
    MissingSafeBase,
    #[error("terminal tail asset identity changed")]
    AssetIdentityChanged,
    #[error("terminal tail has an invalid encryption state")]
    InvalidEncryptionState,
    #[error("terminal tail formatting failed")]
    Formatting,
    #[error("terminal tail route does not match its frozen lease binding")]
    RouteBindingMismatch,
}

pub fn evaluate_terminal_tail_compatibility(
    input: HlsTerminalTailCompatibilityInput<'_>,
) -> HlsTerminalTailCompatibility {
    let manifest = input.manifest;
    let asset = input.asset;
    if manifest.delivery_mode == HlsManifestDeliveryMode::TransientPassthrough {
        return HlsTerminalTailCompatibility::TransientPassthroughUnsupported;
    }
    if asset.duration_ms() == 0 || asset.duration_ticks_90khz() == 0 {
        return HlsTerminalTailCompatibility::InvalidAsset;
    }
    if input.expected_asset != HlsTerminalAssetIdentity::from_asset(asset) {
        return HlsTerminalTailCompatibility::AssetRevisionMismatch;
    }
    if asset.content_type() != "video/mp2t" {
        return HlsTerminalTailCompatibility::InvalidAsset;
    }
    if manifest.active_map.is_some() {
        return HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback;
    }
    if manifest.container != HlsMediaContainer::MpegTs || asset.container() != HlsMediaContainer::MpegTs {
        return HlsTerminalTailCompatibility::ContainerMismatch;
    }
    let Some(base_track_signature) = input.base_track_signature else {
        return HlsTerminalTailCompatibility::MissingTrackSignature;
    };
    if base_track_signature != asset.track_signature() {
        return HlsTerminalTailCompatibility::TrackLayoutMismatch;
    }
    if let HlsTerminalTailBoundaryEvidence::Exact { base, terminal } = input.boundary_evidence {
        let (Some(base), Some(terminal)) = (base, terminal) else {
            return HlsTerminalTailCompatibility::MissingSpliceEvidence;
        };
        match super::evaluate_mpeg_ts_splice_boundary(base, terminal) {
            Ok(()) => {}
            Err(super::HlsTsSpliceBoundaryIncompatibility::Media(reason)) => {
                return HlsTerminalTailCompatibility::SpliceTransportFailure(reason);
            }
            Err(super::HlsTsSpliceBoundaryIncompatibility::TopologyMismatch) => {
                return HlsTerminalTailCompatibility::SpliceTopologyMismatch;
            }
        }
    }
    if encryption_reset_required(manifest.active_encryption.as_ref()).is_err() {
        return HlsTerminalTailCompatibility::UnsupportedEncryptionTransition;
    }
    if !terminal_asset_fits_target_duration(asset.duration_ms(), manifest.target_duration_ms) {
        return HlsTerminalTailCompatibility::TargetDurationExceeded {
            asset_ms: asset.duration_ms(),
            target_ms: manifest.target_duration_ms,
        };
    }
    HlsTerminalTailCompatibility::Compatible
}

fn encryption_reset_required(
    encryption: Option<&HlsEncryptionSignature>,
) -> Result<bool, HlsTerminalTailCompatibility> {
    let Some(encryption) = encryption else {
        return Ok(false);
    };
    match parse_encryption_method(&encryption.method) {
        ParsedEncryptionMethod::None => Ok(false),
        ParsedEncryptionMethod::Aes128
            if encryption.can_reset_to_clear
                && encryption.key_format.as_deref().is_none_or(|format| format.eq_ignore_ascii_case("identity"))
                && encryption.key_uri.as_deref().is_some_and(valid_quoted_attribute)
                && encryption.iv.as_deref().is_none_or(valid_iv)
                && encryption.key_format_versions.as_deref().is_none_or(valid_key_format_versions)
                && (encryption.key_format_versions.is_none()
                    || encryption
                        .key_format
                        .as_deref()
                        .is_some_and(|format| format.eq_ignore_ascii_case("identity"))) =>
        {
            Ok(true)
        }
        ParsedEncryptionMethod::Aes128 | ParsedEncryptionMethod::Unsupported(_) => {
            Err(HlsTerminalTailCompatibility::UnsupportedEncryptionTransition)
        }
    }
}

fn valid_quoted_attribute(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character != '"' && !character.is_control())
}

fn valid_iv(value: &str) -> bool { super::hls_aes128_cbc_iv(Some(value), 0).is_ok() }

fn valid_key_format_versions(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit() || byte == b'/')
}

fn safe_terminal_base(
    mut manifest: HlsLeaseManifestSnapshot,
    availability: &[HlsTerminalBaseSegmentAvailability],
    scope: HlsTerminalBaseScope,
) -> Option<(HlsLeaseManifestSnapshot, Arc<[u64]>)> {
    let mut availability_by_seq = HashMap::<u64, Option<&HlsTerminalBaseSegmentAvailability>>::new();
    for state in availability {
        match availability_by_seq.entry(state.proxy_seq) {
            Entry::Vacant(entry) => {
                entry.insert(Some(state));
            }
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    let mut start = manifest.visible_segments.len();
    let mut expected_next = None;
    for (index, segment) in manifest.visible_segments.iter().enumerate().rev() {
        if expected_next.is_some_and(|next| segment.proxy_seq.checked_add(1) != Some(next)) {
            break;
        }
        let state = availability_by_seq.get(&segment.proxy_seq).copied().flatten();
        let safe = state.is_some_and(|state| {
            state.media_state == HlsTerminalBaseMediaState::Ready
                && state.required_map_ready
                && state.required_key_ready
                && state.protection == HlsTerminalBaseProtection::Protectable
                && segment.map_ref_ready
                && segment.encryption == manifest.active_encryption
        });
        if !safe {
            break;
        }
        start = index;
        expected_next = Some(segment.proxy_seq);
    }
    if start == manifest.visible_segments.len() {
        return None;
    }
    if scope == HlsTerminalBaseScope::LastSafeSegment {
        start = manifest.visible_segments.len().saturating_sub(1);
    }

    let mut selected = manifest.visible_segments[start..].to_vec();
    let removed_discontinuities =
        manifest.visible_segments[..start].iter().filter(|segment| segment.discontinuity_before).count();
    if start > 0 && selected.first().is_some_and(|segment| segment.discontinuity_before) {
        if let Some(first) = selected.first_mut() {
            first.discontinuity_before = false;
        }
        manifest.discontinuity_sequence = manifest.discontinuity_sequence.saturating_add(1);
    }
    manifest.discontinuity_sequence =
        manifest.discontinuity_sequence.saturating_add(u64::try_from(removed_discontinuities).unwrap_or(u64::MAX));
    let first_proxy_seq = selected.first()?.proxy_seq;
    let last_proxy_seq = selected.last()?.proxy_seq;
    let playlist_duration_ms =
        selected.iter().fold(0u64, |duration, segment| duration.saturating_add(segment.duration_ms));
    let protected = Arc::from(selected.iter().map(|segment| segment.proxy_seq).collect::<Vec<_>>());
    manifest.first_proxy_seq = first_proxy_seq;
    manifest.last_proxy_seq = last_proxy_seq;
    manifest.playlist_duration_ms = playlist_duration_ms;
    manifest.last_visible_media_end_ms = playlist_duration_ms;
    manifest.visible_segments = Arc::from(selected);
    Some((manifest, protected))
}

fn terminal_tail_route_binding(manifest: &HlsLeaseManifestSnapshot) -> Option<HlsTerminalTailRouteBinding> {
    let first = manifest.visible_segments.first()?;
    let binding = terminal_tail_route_binding_from_uri(&first.uri)?;
    manifest
        .visible_segments
        .iter()
        .all(|segment| terminal_tail_route_binding_from_uri(&segment.uri).as_ref() == Some(&binding))
        .then_some(binding)
}

fn terminal_tail_route_binding_from_uri(uri: &str) -> Option<HlsTerminalTailRouteBinding> {
    let path = uri.split(['?', '#']).next()?;
    let marker_offset = path.find(HLS_SHARED_LIVE_ROUTE_MARKER)?;
    let public_path_prefix = path.get(..marker_offset)?;
    if !public_path_prefix.is_empty() && (!public_path_prefix.starts_with('/') || public_path_prefix.ends_with('/')) {
        return None;
    }
    let route = path.get(marker_offset.saturating_add(HLS_SHARED_LIVE_ROUTE_MARKER.len())..)?;
    let mut components = route.split('/');
    let proxy_session_id = components.next()?;
    let lease_id = components.next()?;
    let media_file = components.next()?;
    if !valid_terminal_route_component(proxy_session_id)
        || !valid_terminal_route_component(lease_id)
        || media_file.is_empty()
        || components.next().is_some()
        || public_path_prefix.chars().any(char::is_control)
        || media_file.chars().any(char::is_control)
    {
        return None;
    }
    Some(HlsTerminalTailRouteBinding {
        public_path_prefix: Arc::from(public_path_prefix),
        proxy_session_id: ProxySessionId(proxy_session_id.to_string()),
        lease_id: HlsAccessLeaseId(lease_id.to_string()),
    })
}

fn valid_terminal_route_component(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn build_terminal_tail_plan(
    input: HlsTerminalTailBuildInput,
) -> Result<HlsTerminalTailPlan, HlsTerminalTailCompatibility> {
    let compatibility = evaluate_terminal_tail_compatibility(HlsTerminalTailCompatibilityInput {
        manifest: &input.base_manifest,
        base_track_signature: input.base_track_signature.as_ref(),
        boundary_evidence: HlsTerminalTailBoundaryEvidence::Exact {
            base: input.base_splice_evidence.as_ref(),
            terminal: input.terminal_splice_evidence.as_ref(),
        },
        expected_asset: input.expected_asset.media,
        asset: &input.asset,
    });
    log_terminal_splice_compatibility(input.base_manifest.last_proxy_seq, compatibility);
    if compatibility != HlsTerminalTailCompatibility::Compatible {
        return Err(compatibility);
    }
    let expected_bundle_key = HlsPreparedTerminalBundleKey {
        asset: input.expected_asset.media,
        target_duration_ms: input.base_manifest.target_duration_ms,
        segment_count: HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    };
    if !input.anchored_bundle.matches_key_and_shape(expected_bundle_key, input.asset.duration_ticks_90khz()) {
        return Err(HlsTerminalTailCompatibility::AssetRevisionMismatch);
    }
    let Some(base_timing) = input.base_timing.as_ref() else {
        return Err(HlsTerminalTailCompatibility::MissingTimestampAnchor);
    };
    let Some(asset_profile) = input.asset.timestamp_profile() else {
        return Err(HlsTerminalTailCompatibility::InvalidTimestampTransition);
    };
    let Some(expected_anchor) =
        tuliprox_mpegts::transport_stream_buffer::HlsTsSpliceAnchor::between(base_timing.profile, asset_profile)
    else {
        return Err(HlsTerminalTailCompatibility::InvalidTimestampTransition);
    };
    if base_timing.base.proxy_seq != input.base_manifest.last_proxy_seq
        || input.anchored_bundle.splice_anchor != expected_anchor
    {
        return Err(HlsTerminalTailCompatibility::InvalidTimestampTransition);
    }
    let append_key_method_none = encryption_reset_required(input.base_manifest.active_encryption.as_ref())?;
    let Some((base_manifest, protected_base_proxy_seqs)) = safe_terminal_base(
        input.base_manifest,
        &input.base_availability,
        if append_key_method_none { HlsTerminalBaseScope::LastSafeSegment } else { HlsTerminalBaseScope::SafeSuffix },
    ) else {
        return Err(HlsTerminalTailCompatibility::MissingSafeBase);
    };
    let Some(route_binding) = terminal_tail_route_binding(&base_manifest) else {
        return Err(HlsTerminalTailCompatibility::InvalidLeaseRoute);
    };
    let key_bindings = terminal_key_bindings(&base_manifest, &input.base_key_bindings, &route_binding)?;
    let segment_count = input.anchored_bundle.prepared_key.segment_count;
    let segment_duration_ms = input.asset.duration_ms();
    let segment_content_type = input.asset.content_type();
    let manifest_body = render_terminal_tail_manifest_body(&HlsTerminalTailManifestRenderInput {
        generation: input.generation,
        base_manifest: &base_manifest,
        protected_base_proxy_seqs: &protected_base_proxy_seqs,
        asset: &input.asset,
        asset_identity: input.expected_asset.media,
        segment_count,
        segment_duration_ms,
        append_key_method_none,
        route_binding: &route_binding,
    })
    .map(Arc::from)
    .map_err(|_| HlsTerminalTailCompatibility::ManifestRenderFailed)?;
    Ok(HlsTerminalTailPlan {
        generation: input.generation,
        created_at_ms: input.created_at_ms,
        segment_count,
        segment_duration_ms,
        append_key_method_none,
        key_bindings,
        base_manifest,
        protected_base_proxy_seqs,
        reason: input.expected_asset.reason,
        asset_identity: input.expected_asset,
        route_binding,
        manifest_body,
        anchored_bundle: input.anchored_bundle,
        segment_content_type,
    })
}

fn terminal_key_bindings(
    manifest: &HlsLeaseManifestSnapshot,
    evidence: &[HlsTerminalKeyBinding],
    route_binding: &HlsTerminalTailRouteBinding,
) -> Result<Arc<[HlsTerminalKeyBinding]>, HlsTerminalTailCompatibility> {
    let mut selected = Vec::new();
    for encryption in manifest.visible_segments.iter().filter_map(|segment| segment.encryption.as_ref()) {
        match parse_encryption_method(&encryption.method) {
            ParsedEncryptionMethod::None => continue,
            ParsedEncryptionMethod::Aes128 => {}
            ParsedEncryptionMethod::Unsupported(_) => {
                return Err(HlsTerminalTailCompatibility::UnsupportedEncryptionTransition);
            }
        }
        let Some(resource_file) = terminal_key_resource_file(encryption) else {
            return Err(HlsTerminalTailCompatibility::MissingSafeBase);
        };
        let mut matches = evidence.iter().filter(|binding| {
            binding.matches_route(&route_binding.proxy_session_id) && binding.matches_resource(&resource_file)
        });
        let Some(binding) = matches.next().cloned() else {
            return Err(HlsTerminalTailCompatibility::MissingSafeBase);
        };
        if matches.any(|candidate| candidate != &binding) {
            return Err(HlsTerminalTailCompatibility::MissingSafeBase);
        }
        if !selected.contains(&binding) {
            selected.push(binding);
        }
    }
    selected.sort_by(|left, right| {
        (&left.resource_id.0, left.route_extension.as_ref())
            .cmp(&(&right.resource_id.0, right.route_extension.as_ref()))
    });
    Ok(selected.into())
}

fn terminal_key_resource_file(encryption: &HlsEncryptionSignature) -> Option<TransientResourceFile> {
    encryption.key_uri.as_deref()?.split(['?', '#']).next()?.rsplit('/').next().and_then(TransientResourceFile::parse)
}

pub fn terminal_tail_manifest_body<'a>(
    plan: &'a HlsTerminalTailPlan,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) -> Result<&'a str, HlsTerminalTailRenderError> {
    if !plan.matches_route(proxy_session_id, lease_id) {
        return Err(HlsTerminalTailRenderError::RouteBindingMismatch);
    }
    Ok(&plan.manifest_body)
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

fn render_terminal_tail_manifest_body(
    input: &HlsTerminalTailManifestRenderInput<'_>,
) -> Result<String, HlsTerminalTailRenderError> {
    if input.asset_identity != HlsTerminalAssetIdentity::from_asset(input.asset) {
        return Err(HlsTerminalTailRenderError::AssetIdentityChanged);
    }
    let Some(first) = input.base_manifest.visible_segments.first() else {
        return Err(HlsTerminalTailRenderError::MissingSafeBase);
    };
    if input.protected_base_proxy_seqs != input.base_manifest.visible_proxy_seqs().collect::<Vec<_>>().as_slice() {
        return Err(HlsTerminalTailRenderError::MissingSafeBase);
    }
    let mut body = String::new();
    let version = input.base_manifest.active_encryption.as_ref().map_or(3, |encryption| {
        if encryption.key_format.is_some() || encryption.key_format_versions.is_some() {
            5
        } else {
            3
        }
    });
    writeln!(body, "#EXTM3U\n#EXT-X-VERSION:{version}").map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    writeln!(body, "#EXT-X-TARGETDURATION:{}", hls_target_duration_secs(input.base_manifest.target_duration_ms))
        .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    writeln!(body, "#EXT-X-MEDIA-SEQUENCE:{}", first.proxy_seq).map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    writeln!(body, "#EXT-X-DISCONTINUITY-SEQUENCE:{}", input.base_manifest.discontinuity_sequence)
        .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    render_active_base_encryption(&mut body, input.base_manifest.active_encryption.as_ref())?;
    for segment in input.base_manifest.visible_segments.iter() {
        if segment.discontinuity_before {
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        writeln!(body, "#EXTINF:{},", format_hls_duration_ms(segment.duration_ms))
            .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
        writeln!(body, "{}", segment.uri).map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    }
    if input.append_key_method_none {
        body.push_str("#EXT-X-KEY:METHOD=NONE\n");
    }
    body.push_str("#EXT-X-DISCONTINUITY\n");
    for index in 0..input.segment_count {
        writeln!(body, "#EXTINF:{},", format_hls_duration_ms(input.segment_duration_ms))
            .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
        writeln!(
            body,
            "{}/hls/shared/live/{}/{}/terminal/{}/{}.ts",
            input.route_binding.public_path_prefix,
            input.route_binding.proxy_session_id.0,
            input.route_binding.lease_id.0,
            input.generation.0,
            index
        )
        .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(body)
}

fn render_active_base_encryption(
    body: &mut String,
    encryption: Option<&HlsEncryptionSignature>,
) -> Result<(), HlsTerminalTailRenderError> {
    let Some(encryption) = encryption else {
        return Ok(());
    };
    match parse_encryption_method(&encryption.method) {
        ParsedEncryptionMethod::None => Ok(()),
        ParsedEncryptionMethod::Aes128 => {
            encryption_reset_required(Some(encryption))
                .map_err(|_| HlsTerminalTailRenderError::InvalidEncryptionState)?;
            let Some(key_uri) = encryption.key_uri.as_deref() else {
                return Err(HlsTerminalTailRenderError::InvalidEncryptionState);
            };
            write!(body, "#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"")
                .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
            if let Some(iv) = encryption.iv.as_deref() {
                write!(body, ",IV={iv}").map_err(|_| HlsTerminalTailRenderError::Formatting)?;
            }
            if encryption.key_format.as_deref().is_some_and(|format| format.eq_ignore_ascii_case("identity")) {
                body.push_str(",KEYFORMAT=\"identity\"");
            }
            if let Some(versions) = encryption.key_format_versions.as_deref() {
                write!(body, ",KEYFORMATVERSIONS=\"{versions}\"")
                    .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
            }
            body.push('\n');
            Ok(())
        }
        ParsedEncryptionMethod::Unsupported(_) => Err(HlsTerminalTailRenderError::InvalidEncryptionState),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::media_reserve::{HlsLeaseManifestSegment, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker},
        *,
    };
    use tuliprox_mpegts::transport_stream_buffer::{HlsFiniteTsRenderSpec, TransportStreamBuffer};

    const TERMINAL_ASSET_BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../test/fixtures/hls/channel_unavailable.ts"));

    fn manifest() -> HlsLeaseManifestSnapshot {
        HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
            snapshot_generation: 1,
            delivered_at_ms: 10,
            first_proxy_seq: 193,
            last_proxy_seq: 194,
            visible_segments: Arc::from([
                HlsLeaseManifestSegment {
                    proxy_seq: 193,
                    duration_ms: 9_940,
                    uri: "/iptv/hls/shared/live/session/lease/193.ts".into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
                HlsLeaseManifestSegment {
                    proxy_seq: 194,
                    duration_ms: 10_380,
                    uri: "/iptv/hls/shared/live/session/lease/194.ts".into(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
            ]),
            discontinuity_sequence: 7,
            target_duration_ms: 12_000,
            playlist_duration_ms: 20_320,
            last_visible_media_end_ms: 20_320,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        }
    }

    fn asset() -> Arc<HlsTerminalMediaAsset> {
        let bytes = Bytes::from_static(TERMINAL_ASSET_BYTES);
        let buffer = TransportStreamBuffer::new(bytes.to_vec());
        snapshot_terminal_media_asset(&buffer).expect("valid terminal asset")
    }

    fn availability(base: &HlsLeaseManifestSnapshot) -> Arc<[HlsTerminalBaseSegmentAvailability]> {
        Arc::from(
            base.visible_proxy_seqs()
                .map(|proxy_seq| HlsTerminalBaseSegmentAvailability {
                    proxy_seq,
                    media_state: HlsTerminalBaseMediaState::Ready,
                    required_map_ready: true,
                    required_key_ready: true,
                    protection: HlsTerminalBaseProtection::Protectable,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn build_input(
        generation: u64,
        base_manifest: HlsLeaseManifestSnapshot,
        asset: Arc<HlsTerminalMediaAsset>,
    ) -> HlsTerminalTailBuildInput {
        let anchored_bundle =
            HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, base_manifest.target_duration_ms);
        let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
        let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
        let terminal_splice_evidence = base_splice_evidence.clone();
        HlsTerminalTailBuildInput {
            generation: HlsTerminalTailGeneration(generation),
            created_at_ms: 20,
            base_availability: availability(&base_manifest),
            base_track_signature: Some(asset.track_signature().clone()),
            base_splice_evidence,
            terminal_splice_evidence,
            base_timing,
            base_key_bindings: Arc::from([]),
            expected_asset: HlsRuntimeCustomTailAssetIdentity {
                reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
                media: HlsTerminalAssetIdentity::from_asset(&asset),
            },
            base_manifest,
            asset,
            anchored_bundle,
        }
    }

    fn compatibility(
        base: &HlsLeaseManifestSnapshot,
        asset: &HlsTerminalMediaAsset,
        base_track_signature: Option<&HlsTsTrackSignature>,
    ) -> HlsTerminalTailCompatibility {
        evaluate_terminal_tail_compatibility(HlsTerminalTailCompatibilityInput {
            manifest: base,
            base_track_signature,
            boundary_evidence: HlsTerminalTailBoundaryEvidence::StructuralOnly,
            expected_asset: HlsTerminalAssetIdentity::from_asset(asset),
            asset,
        })
    }

    fn resettable_aes128_encryption() -> HlsEncryptionSignature {
        HlsEncryptionSignature {
            method: "AES-128".into(),
            key_uri: Some("/iptv/hls/shared/live/session/lease/r/aes-key.key".into()),
            iv: Some("0x000000000000000000000000000000C2".into()),
            key_format: Some("identity".into()),
            key_format_versions: Some("1".into()),
            can_reset_to_clear: true,
        }
    }

    fn aes128_key_binding() -> HlsTerminalKeyBinding {
        HlsTerminalKeyBinding::new(
            ProxySessionId("session".into()),
            TransientResourceId("aes-key".into()),
            "key".into(),
            TransientObjectCacheKey::new(
                ProxySessionId("session".into()),
                TransientResourceId("aes-key".into()),
                "key.fill-0000000000000001",
            ),
            "application/octet-stream".into(),
            b"0123456789abcdef",
        )
        .expect("valid AES-128 binding")
    }

    async fn unresolved_aes_terminal_base_evidence(iv: &str) -> HlsTerminalBaseEvidence {
        let temp_dir = tempfile::tempdir().expect("terminal evidence cache tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let mut session =
            super::super::HlsSession::new(super::super::HlsSessionKey::new(1, "terminal-evidence"), b"secret", 1);
        let proxy_seq = 194;
        let cache_key = super::super::SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts");
        cache
            .write_bytes_and_commit(&cache_key, TERMINAL_ASSET_BYTES)
            .await
            .expect("terminal evidence segment commits");
        let timeline_encryption = super::super::HlsSegmentEncryption {
            resource_id: TransientResourceId("aes-key".into()),
            resource_extension: "key".into(),
            iv: Some(iv.into()),
            key_format: Some("identity".into()),
            key_format_versions: Some("1".into()),
        };
        session.segments.insert(
            proxy_seq,
            SegmentEntry {
                origin_key: super::super::OriginSegmentKey {
                    origin_epoch: 3,
                    effective_host_id: 7,
                    host_local_sequence: 900,
                    host_local_index: 0,
                },
                proxy_seq,
                duration_ms: 4_000,
                proxy_file_ext: "ts".into(),
                content_type: "video/mp2t".into(),
                cache_key,
                discontinuity_before: false,
                program_date_time: None,
                daterange_tags_before: Vec::new(),
                origin_byte_range: None,
                map_ref: None,
                encryption: Some(timeline_encryption),
                origin_fetch_ref: None,
                status: SegmentCacheStatus::Ready {
                    content_length: u64::try_from(TERMINAL_ASSET_BYTES.len()).unwrap_or(u64::MAX),
                    ready_at_ms: 2,
                },
                last_rendered_at_ms: Some(2),
                access: Arc::new(CacheAccessState::new()),
            },
        );
        let encryption = HlsEncryptionSignature {
            method: "AES-128".into(),
            key_uri: Some("/iptv/hls/shared/live/session/lease/r/aes-key.key".into()),
            iv: Some(iv.into()),
            key_format: Some("identity".into()),
            key_format_versions: Some("1".into()),
            can_reset_to_clear: true,
        };
        let mut base = manifest();
        base.first_proxy_seq = proxy_seq;
        base.last_proxy_seq = proxy_seq;
        base.visible_segments = Arc::from([HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: 4_000,
            uri: format!("/iptv/hls/shared/live/{}/lease/{proxy_seq}.ts", session.proxy_session_id.0),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: Some(encryption.clone()),
        }]);
        base.active_encryption = Some(encryption);
        base.playlist_duration_ms = 4_000;
        base.last_visible_media_end_ms = 4_000;
        let session = Arc::new(tokio::sync::RwLock::new(session));

        prepare_terminal_base_evidence(&session, &cache, &base, 3).await
    }

    #[tokio::test]
    async fn terminal_base_media_evidence_scans_complete_cache_file() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary cache file");
        std::io::Write::write_all(&mut file, TERMINAL_ASSET_BYTES).expect("write valid TS");
        let source_size = file.as_file().metadata().expect("cache metadata").len();
        let terminal_asset = asset();
        let probe = HlsTerminalMediaProbe {
            segment_path: file.path().to_path_buf(),
            source_size,
            expected_duration_ticks_90khz: terminal_asset.duration_ticks_90khz(),
            encryption: HlsTerminalTrackEncryption::Clear,
        };

        let evidence = terminal_base_media_evidence(probe).await;
        let expected_signature = terminal_asset.track_signature().clone();

        assert_eq!(evidence.track_resolution.signature(), Some(&expected_signature));
        assert_eq!(evidence.track_resolution.reason_code(), "found");
        assert_eq!(evidence.timestamp_profile, terminal_asset.timestamp_profile());
        assert!(matches!(evidence.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
    }

    async fn terminal_timing_session(
        cache: &HlsSegmentCache,
        terminal_asset: &HlsTerminalMediaAsset,
        first_bytes: &[u8],
        last_bytes: &[u8],
    ) -> (
        Arc<tokio::sync::RwLock<super::super::HlsSession>>,
        HlsLeaseManifestSnapshot,
        super::super::SegmentCacheKey,
        u64,
    ) {
        let mut session = super::super::HlsSession::new(
            super::super::HlsSessionKey::new(1, "terminal-timing-evidence"),
            b"secret",
            1,
        );
        let proxy_session_id = session.proxy_session_id.clone();
        let first_proxy_seq = 193;
        let last_proxy_seq = 194;
        let first_cache_key = super::super::SegmentCacheKey::new(proxy_session_id.clone(), first_proxy_seq, "ts");
        let last_cache_key = super::super::SegmentCacheKey::new(proxy_session_id.clone(), last_proxy_seq, "ts");
        cache.write_bytes_and_commit(&first_cache_key, first_bytes).await.expect("first segment commits");
        cache.write_bytes_and_commit(&last_cache_key, last_bytes).await.expect("last segment commits");
        for (proxy_seq, cache_key, content_length) in [
            (first_proxy_seq, first_cache_key, first_bytes.len()),
            (last_proxy_seq, last_cache_key.clone(), last_bytes.len()),
        ] {
            session.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: super::super::OriginSegmentKey {
                        origin_epoch: 7,
                        effective_host_id: 3,
                        host_local_sequence: proxy_seq,
                        host_local_index: 0,
                    },
                    proxy_seq,
                    duration_ms: terminal_asset.duration_ms(),
                    proxy_file_ext: "ts".into(),
                    content_type: "video/mp2t".into(),
                    cache_key,
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready {
                        content_length: u64::try_from(content_length).unwrap_or(u64::MAX),
                        ready_at_ms: 2,
                    },
                    last_rendered_at_ms: Some(2),
                    access: Arc::new(CacheAccessState::new()),
                },
            );
        }
        let mut base = manifest();
        base.first_proxy_seq = first_proxy_seq;
        base.last_proxy_seq = last_proxy_seq;
        base.visible_segments = Arc::from([first_proxy_seq, last_proxy_seq].map(|proxy_seq| HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: terminal_asset.duration_ms(),
            uri: format!("/iptv/hls/shared/live/{}/lease/{proxy_seq}.ts", proxy_session_id.0),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }));
        base.playlist_duration_ms = terminal_asset.duration_ms().saturating_mul(2);
        base.last_visible_media_end_ms = base.playlist_duration_ms;
        (Arc::new(tokio::sync::RwLock::new(session)), base, last_cache_key, last_proxy_seq)
    }

    #[tokio::test]
    async fn terminal_base_timestamp_profile_uses_exact_pinned_last_safe_segment() {
        let temp_dir = tempfile::tempdir().expect("terminal timing cache tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let terminal_asset = asset();
        let renderer = terminal_asset.renderer();
        let first_bytes = renderer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: 0,
                continuity_seed: 0,
                logical_segment_index: 0,
            })
            .expect("first cached segment");
        let last_offset = 9_000_000;
        let last_bytes = renderer
            .render_finite_hls_segment(HlsFiniteTsRenderSpec {
                timestamp_offset_ticks_90khz: last_offset,
                continuity_seed: 0,
                logical_segment_index: 1,
            })
            .expect("last cached segment");
        let expected_profile = TransportStreamBuffer::new(last_bytes.to_vec())
            .finite_hls_timestamp_profile()
            .expect("last segment timestamp profile");
        let (session, base, last_cache_key, last_proxy_seq) =
            terminal_timing_session(&cache, &terminal_asset, &first_bytes, &last_bytes).await;

        let evidence = prepare_terminal_base_evidence(&session, &cache, &base, 3).await;
        let timing = evidence.timing().expect("exact last-base timing evidence");

        assert_eq!(timing.base.proxy_seq, last_proxy_seq);
        assert_eq!(timing.base.origin_epoch, 7);
        assert_eq!(timing.base.cache_key, last_cache_key);
        assert_eq!(timing.profile, expected_profile);
        assert_ne!(
            timing.profile,
            TransportStreamBuffer::new(first_bytes.to_vec())
                .finite_hls_timestamp_profile()
                .expect("first segment timestamp profile")
        );
    }

    #[tokio::test]
    async fn terminal_base_preserves_invalid_iv_and_missing_key_evidence() {
        let invalid_iv = unresolved_aes_terminal_base_evidence("not-an-iv").await;
        assert!(invalid_iv.track_base().is_some());
        assert_eq!(invalid_iv.track_resolution(), Some(&HlsTrackEvidenceResolution::InvalidIv));
        assert_eq!(invalid_iv.track_evidence_reason_code(), "invalid-iv");

        let missing_key = unresolved_aes_terminal_base_evidence("0x00000000000000000000000000000384").await;
        assert!(missing_key.track_base().is_some());
        assert_eq!(missing_key.track_resolution(), Some(&HlsTrackEvidenceResolution::KeyUnavailable));
        assert_eq!(missing_key.track_evidence_reason_code(), "key-unavailable");
    }

    #[test]
    fn hls_terminal_response_manifest_keeps_live_tail_and_appends_finite_endlist_tail() {
        let asset = asset();
        let plan = build_terminal_tail_plan(build_input(4, manifest(), Arc::clone(&asset))).expect("compatible tail");

        let first =
            terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
                .expect("render tail");
        let second =
            terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
                .expect("render tail again");

        assert_eq!(first, second);
        assert_eq!(first.matches("#EXTM3U").count(), 1);
        assert!(first.contains("/iptv/hls/shared/live/session/lease/193.ts"));
        assert_eq!(
            first.matches("/iptv/hls/shared/live/session/lease/terminal/4/").count(),
            usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
        );
        assert_eq!(first.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert!(first.ends_with("#EXT-X-ENDLIST\n"));
        assert!(first.contains("#EXT-X-TARGETDURATION:12\n"));
        assert_eq!(plan.base_manifest.target_duration_ms, 12_000);
        assert_eq!(plan.segment_duration_ms, asset.duration_ms());
        let terminal_media = first
            .rsplit_once("#EXT-X-DISCONTINUITY\n")
            .map(|(_, terminal_media)| terminal_media)
            .expect("terminal discontinuity");
        assert_eq!(
            terminal_media.matches(&format!("#EXTINF:{},", format_hls_duration_ms(asset.duration_ms()))).count(),
            usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
        );
        assert_eq!(
            terminal_tail_manifest_body(
                &plan,
                &ProxySessionId("session".into()),
                &HlsAccessLeaseId("other-lease".into()),
            ),
            Err(HlsTerminalTailRenderError::RouteBindingMismatch)
        );
    }

    #[test]
    fn terminal_tail_plan_serves_preanchored_bytes_without_request_time_rendering() {
        let asset = asset();
        let renderer = asset.renderer();
        let first_input = build_input(4, manifest(), Arc::clone(&asset));
        let shared_bundle = Arc::clone(&first_input.anchored_bundle);
        let mut second_input = build_input(5, manifest(), asset);
        second_input.anchored_bundle = Arc::clone(&shared_bundle);
        let plan = build_terminal_tail_plan(first_input).expect("compatible tail");
        let second_plan = build_terminal_tail_plan(second_input).expect("second compatible tail");
        let render_count_after_plan_commit = renderer.finite_hls_render_count();
        let finalize_count_after_plan_commit = renderer.finite_hls_finalize_count();
        assert!(Arc::ptr_eq(&plan.anchored_bundle, &second_plan.anchored_bundle));
        let first_path = HlsTerminalSegmentPath { generation: plan.generation, index: 0 };
        let second_path = HlsTerminalSegmentPath { generation: plan.generation, index: 1 };

        let first = plan.segment_bytes(first_path).expect("first prepared segment exists");
        assert_eq!(plan.segment_content_length(first_path), u64::try_from(first.len()).ok());
        let first_again = plan.segment_bytes(first_path).expect("same prepared segment exists");
        assert_eq!(first, first_again);
        assert_eq!(first.as_ptr(), first_again.as_ptr(), "Bytes clones share the prepared allocation");
        assert_ne!(plan.segment_bytes(second_path), Some(first));
        assert_eq!(
            plan.segment_bytes(HlsTerminalSegmentPath {
                generation: HlsTerminalTailGeneration(plan.generation.0.saturating_add(1)),
                index: 0,
            }),
            None
        );
        assert_eq!(
            plan.segment_bytes(HlsTerminalSegmentPath { generation: plan.generation, index: plan.segment_count }),
            None
        );
        assert_eq!(renderer.finite_hls_render_count(), render_count_after_plan_commit);
        assert_eq!(renderer.finite_hls_finalize_count(), finalize_count_after_plan_commit);
    }

    #[test]
    fn hls_terminal_response_path_parser_rejects_noncanonical_generation_and_file() {
        assert_eq!(
            HlsTerminalSegmentPath::parse("17", "2.ts"),
            Some(HlsTerminalSegmentPath { generation: HlsTerminalTailGeneration(17), index: 2 })
        );
        for (generation, terminal_file) in [
            ("", "0.ts"),
            ("01", "0.ts"),
            ("+1", "0.ts"),
            ("one", "0.ts"),
            ("1", ""),
            ("1", "00.ts"),
            ("1", "+1.ts"),
            ("1", "1.m4s"),
            ("1", "65536.ts"),
        ] {
            assert_eq!(HlsTerminalSegmentPath::parse(generation, terminal_file), None);
        }
    }

    #[test]
    fn hls_terminal_response_rejects_bundle_for_a_different_target_duration() {
        let asset = asset();
        let mut input = build_input(4, manifest(), Arc::clone(&asset));
        input.anchored_bundle = HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, 13_000);

        assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::AssetRevisionMismatch));
    }

    #[test]
    fn terminal_plan_rejects_unbound_and_mixed_lease_routes() {
        let mut unbound = manifest();
        for segment in Arc::make_mut(&mut unbound.visible_segments) {
            segment.uri = format!("/live/{}.ts", segment.proxy_seq);
        }
        assert_eq!(
            build_terminal_tail_plan(build_input(4, unbound, asset())),
            Err(HlsTerminalTailCompatibility::InvalidLeaseRoute)
        );

        let mut mixed = manifest();
        Arc::make_mut(&mut mixed.visible_segments)[1].uri =
            "/iptv/hls/shared/live/session/other-lease/194.ts".to_string();
        assert_eq!(
            build_terminal_tail_plan(build_input(4, mixed, asset())),
            Err(HlsTerminalTailCompatibility::InvalidLeaseRoute)
        );
    }

    #[test]
    fn encrypted_base_announces_clear_key_transition() {
        let mut base = manifest();
        let encryption = resettable_aes128_encryption();
        base.active_encryption = Some(encryption.clone());
        Arc::make_mut(&mut base.visible_segments)[1].encryption = Some(encryption);
        let mut input = build_input(1, base, asset());
        input.base_key_bindings = Arc::from([aes128_key_binding()]);
        let plan = build_terminal_tail_plan(input).expect("clear transition");
        let resource_file = TransientResourceFile::parse("aes-key.key").expect("terminal key route");
        let binding = plan
            .terminal_key_binding(&ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()), &resource_file)
            .expect("exact lease route resolves frozen key binding");
        assert_eq!(binding.bytes().as_ref(), b"0123456789abcdef");
        assert!(plan
            .terminal_key_binding(
                &ProxySessionId("session".into()),
                &HlsAccessLeaseId("other-lease".into()),
                &resource_file,
            )
            .is_none());
        assert!(plan
            .terminal_key_binding(
                &ProxySessionId("session".into()),
                &HlsAccessLeaseId("lease".into()),
                &TransientResourceFile::parse("aes-key.bin").expect("different extension route"),
            )
            .is_none());
        let rendered =
            terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
                .expect("render");

        assert!(
            !rendered.contains("/iptv/hls/shared/live/session/lease/193.ts"),
            "only the segment covered by the active key is safe"
        );
        let key = rendered.find("#EXT-X-KEY:METHOD=AES-128").expect("base key tag");
        let base_segment = rendered.find("/iptv/hls/shared/live/session/lease/194.ts").expect("encrypted base segment");
        let reset = rendered.find("#EXT-X-KEY:METHOD=NONE").expect("clear reset");
        assert!(key < base_segment && base_segment < reset);
        assert!(rendered.contains(
            "URI=\"/iptv/hls/shared/live/session/lease/r/aes-key.key\",IV=0x000000000000000000000000000000C2"
        ));
        assert!(rendered.contains("#EXT-X-VERSION:5\n"));
        assert!(rendered.contains("#EXT-X-KEY:METHOD=NONE\n#EXT-X-DISCONTINUITY\n"));
    }

    #[test]
    fn method_none_boundary_keeps_only_homogeneous_clear_suffix() {
        let mut base = manifest();
        Arc::make_mut(&mut base.visible_segments)[0].encryption = Some(resettable_aes128_encryption());

        let plan = build_terminal_tail_plan(build_input(2, base, asset())).expect("clear suffix");
        let rendered =
            terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
                .expect("render");

        assert_eq!(plan.protected_base_proxy_seqs.as_ref(), &[194]);
        assert!(!rendered.contains("/iptv/hls/shared/live/session/lease/193.ts"));
        assert!(!rendered.contains("#EXT-X-KEY:METHOD=AES-128"));
        assert!(!rendered.contains("#EXT-X-KEY:METHOD=NONE"));
    }

    #[test]
    fn active_fmp4_map_rejects_mpeg_ts_splice() {
        let mut base = manifest();
        base.container = HlsMediaContainer::FragmentedMp4;
        base.active_map = Some(HlsMapSignature { fingerprint: [2; 32], container: HlsMediaContainer::FragmentedMp4 });
        let asset = asset();

        assert_eq!(
            compatibility(&base, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback
        );
    }

    #[test]
    fn mapless_non_ts_container_rejects_mpeg_ts_splice() {
        let mut base = manifest();
        base.container = HlsMediaContainer::Unknown;
        base.active_map = None;
        let asset = asset();

        assert_eq!(
            compatibility(&base, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::ContainerMismatch
        );
    }

    #[test]
    fn transient_origin_backed_manifest_is_explicitly_incompatible() {
        let asset = asset();
        let mut base = manifest();
        base.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;

        assert_eq!(
            compatibility(&base, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::TransientPassthroughUnsupported
        );
    }

    #[test]
    fn asset_rounded_duration_must_not_exceed_target_duration() {
        let asset = asset();
        let mut short_target = manifest();
        short_target.target_duration_ms = 9_000;

        assert_eq!(
            compatibility(&short_target, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::TargetDurationExceeded { asset_ms: asset.duration_ms(), target_ms: 9_000 }
        );
    }

    #[test]
    fn terminal_plan_keeps_only_contiguous_ready_protectable_suffix() {
        let asset = asset();
        let base = manifest();
        let mut input = build_input(1, base, Arc::clone(&asset));
        Arc::make_mut(&mut input.base_availability)[0].media_state = HlsTerminalBaseMediaState::NotReady;

        let plan = build_terminal_tail_plan(input).expect("safe suffix");

        assert_eq!(plan.base_manifest.first_proxy_seq, 194);
        assert_eq!(plan.base_manifest.last_proxy_seq, 194);
        assert_eq!(plan.protected_base_proxy_seqs.as_ref(), &[194]);
        assert_eq!(plan.base_manifest.visible_segments.len(), 1);
    }

    #[test]
    fn unsafe_last_advertised_segment_rejects_terminal_plan() {
        let mut input = build_input(1, manifest(), asset());
        Arc::make_mut(&mut input.base_availability)[1].protection = HlsTerminalBaseProtection::Unavailable;

        assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
    }

    #[test]
    fn unsafe_transport_evidence_cannot_commit_or_expose_any_runtime_custom_tail() {
        let unsafe_reason = super::super::HlsTsSpliceIncompatibility::IncompletePes {
            pid: 0x101,
            packet_index: 27,
            declared_bytes: Some(512),
            observed_bytes: 384,
        };
        for reason in [
            HlsRuntimeCustomTailReason::ChannelUnavailable,
            HlsRuntimeCustomTailReason::LowPriorityPreempted,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            HlsRuntimeCustomTailReason::UserAccountExpired,
            HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
        ] {
            let asset = asset();
            let mut input = build_input(1, manifest(), Arc::clone(&asset));
            input.expected_asset =
                HlsRuntimeCustomTailAssetIdentity { reason, media: HlsTerminalAssetIdentity::from_asset(&asset) };
            input.base_splice_evidence = Some(HlsTsSpliceEvidence::Incompatible(unsafe_reason));

            assert_eq!(
                build_terminal_tail_plan(input),
                Err(HlsTerminalTailCompatibility::SpliceTransportFailure(unsafe_reason)),
                "reason {reason} must use the common exact splice gate"
            );
        }
    }

    #[test]
    fn terminal_plan_rejects_missing_or_topologically_different_exact_evidence() {
        let asset = asset();
        let mut missing = build_input(1, manifest(), Arc::clone(&asset));
        missing.terminal_splice_evidence = None;
        assert_eq!(build_terminal_tail_plan(missing), Err(HlsTerminalTailCompatibility::MissingSpliceEvidence));

        let mut different = build_input(2, manifest(), asset);
        different.terminal_splice_evidence = Some(HlsTsSpliceEvidence::compatible_for_test(
            HlsTsTrackSignature::from_stream_types(Arc::<[u8]>::from([0x1B])),
        ));
        assert_eq!(build_terminal_tail_plan(different), Err(HlsTerminalTailCompatibility::SpliceTopologyMismatch));
    }

    #[test]
    fn terminal_splice_diagnostic_preserves_typed_boundary_fields() {
        let compatibility = HlsTerminalTailCompatibility::SpliceTransportFailure(
            super::super::HlsTsSpliceIncompatibility::ContinuityFailure {
                pid: 0x102,
                packet_index: 44,
                expected: 9,
                actual: 12,
            },
        );

        assert_eq!(
            HlsTerminalSpliceDiagnostic::from_compatibility(compatibility),
            Some(HlsTerminalSpliceDiagnostic {
                result: "continuity-failure",
                pid: Some(0x102),
                packet_index: Some(44),
                expected_cc: Some(9),
                actual_cc: Some(12),
                declared_pes_bytes: None,
                observed_pes_bytes: None,
            })
        );
        assert_eq!(
            HlsTerminalSpliceDiagnostic::from_compatibility(HlsTerminalTailCompatibility::SpliceTopologyMismatch)
                .map(|diagnostic| diagnostic.result),
            Some("topology-mismatch")
        );
    }

    #[test]
    fn encrypted_base_without_ready_key_rejects_terminal_plan() {
        let mut base = manifest();
        let encryption = resettable_aes128_encryption();
        base.active_encryption = Some(encryption.clone());
        Arc::make_mut(&mut base.visible_segments)[1].encryption = Some(encryption);
        let mut input = build_input(1, base, asset());
        Arc::make_mut(&mut input.base_availability)[1].required_key_ready = false;

        assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
    }

    #[test]
    fn duplicate_readiness_evidence_is_not_accepted_as_safe_base() {
        let mut input = build_input(1, manifest(), asset());
        let duplicate = input.base_availability[1];
        let mut states = input.base_availability.to_vec();
        states.push(duplicate);
        input.base_availability = Arc::from(states);

        assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
    }

    #[test]
    fn unknown_or_different_track_layout_rejects_splice() {
        let asset = asset();
        assert_eq!(compatibility(&manifest(), &asset, None), HlsTerminalTailCompatibility::MissingTrackSignature);
        let different = HlsTsTrackSignature::from_stream_types(Arc::<[u8]>::from([0x1B]));
        assert_eq!(
            compatibility(&manifest(), &asset, Some(&different)),
            HlsTerminalTailCompatibility::TrackLayoutMismatch
        );
    }

    #[test]
    fn unsupported_key_format_cannot_reset_to_clear_asset() {
        let mut base = manifest();
        base.active_encryption = Some(HlsEncryptionSignature {
            method: "SAMPLE-AES".into(),
            key_uri: Some("/keys/drm.key".into()),
            iv: None,
            key_format: Some("com.example.drm".into()),
            key_format_versions: None,
            can_reset_to_clear: true,
        });
        let asset = asset();

        assert_eq!(
            compatibility(&base, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
        );
    }

    #[test]
    fn unsafe_key_uri_or_iv_cannot_be_rendered_into_terminal_manifest() {
        let asset = asset();
        let mut unsafe_uri = manifest();
        let mut encryption = resettable_aes128_encryption();
        encryption.key_uri = Some("/key\"\n#EXT-X-ENDLIST".into());
        unsafe_uri.active_encryption = Some(encryption);
        assert_eq!(
            compatibility(&unsafe_uri, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
        );

        let mut unsafe_iv = manifest();
        let mut encryption = resettable_aes128_encryption();
        encryption.iv = Some("not-hex".into());
        unsafe_iv.active_encryption = Some(encryption);
        assert_eq!(
            compatibility(&unsafe_iv, &asset, Some(asset.track_signature())),
            HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
        );
    }

    #[test]
    fn stale_asset_revision_cannot_build_terminal_plan() {
        let asset = asset();
        let mut input = build_input(1, manifest(), asset);
        input.expected_asset.media.revision = input.expected_asset.media.revision.saturating_add(1);

        assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::AssetRevisionMismatch));
    }
}
