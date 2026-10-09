use super::{
    assets::{parse_encryption_method, ParsedEncryptionMethod},
    CacheAccessState, HlsEncryptionSignature, HlsLeaseManifestSnapshot, HlsResolvedTerminalBaseEvidence,
    HlsSegmentCache, HlsSessionHandle, HlsTerminalBaseEvidence, HlsTerminalBaseEvidencePreparation,
    HlsTerminalBaseMediaEvidence, HlsTerminalBaseReadProtection, HlsTerminalBaseSegmentProbe,
    HlsTerminalCommitMediaGuard, HlsTerminalKeyBinding, HlsTerminalKeyMaterial, HlsTerminalMediaProbe,
    HlsTrackEvidenceResolution, HlsTsProbeBudget, HlsTsProbeProtection, HlsTsSpliceEvidence, HlsTsTrackSignature,
    ProxySessionId, SegmentCacheStatus, SegmentEntry, TransientObjectCacheKey, TransientPassthroughState,
    TransientResourceFile, TransientResourceId, TransientResourceKind, AES_128_BLOCK_BYTES,
};
use log::debug;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    path::Path,
    sync::Arc,
};
use tokio::io::AsyncReadExt;
use tuliprox_mpegts::transport_stream_buffer::HlsTsTimestampProfile;
use zeroize::Zeroizing;

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
    pub cache_key: super::super::SegmentCacheKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsTerminalBaseTimingEvidence {
    pub base: HlsTerminalBaseTrackIdentity,
    pub profile: HlsTsTimestampProfile,
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
    session: &super::super::HlsSession,
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

fn snapshot_terminal_base_probes(
    session: &super::super::HlsSession,
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
                        .is_some_and(|map| matches!(&map.status, super::super::map::MapCacheStatus::Ready { .. }))
                })
            });
            let key_evidence = terminal_base_key_evidence(session, entry, segment.encryption.as_deref());
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

pub(super) enum HlsTerminalBaseKeyEvidence {
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
    pub(super) fn unavailable(resolution: HlsTrackEvidenceResolution) -> Self { Self::Unavailable { resolution } }
}

fn terminal_base_key_evidence(
    session: &super::super::HlsSession,
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
            super::super::HlsTsProtectionReason::UnsupportedEncryption,
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
    let iv = match super::super::hls_aes128_cbc_iv(
        encryption.iv.as_deref(),
        timeline_entry.origin_key.host_local_sequence,
    ) {
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
    let ready = matches!(entry.status, super::super::TransientObjectCacheStatus::Ready { .. });
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

pub(super) enum HlsTerminalTrackEncryption {
    Clear,
    Aes128 { key_material: Arc<HlsTerminalKeyMaterial>, iv: [u8; AES_128_BLOCK_BYTES] },
}

pub(super) async fn terminal_base_media_evidence(probe: HlsTerminalMediaProbe) -> HlsTerminalBaseMediaEvidence {
    let file = match tokio::fs::File::open(&probe.segment_path).await {
        Ok(file) => file,
        Err(error) => {
            return HlsTerminalBaseMediaEvidence {
                track_resolution: HlsTrackEvidenceResolution::Io(error.kind()),
                timestamp_profile: None,
                splice_evidence: HlsTsSpliceEvidence::Incompatible(
                    super::super::HlsTsSpliceIncompatibility::TopologyUnavailable,
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
            super::super::inspect_mpeg_ts_media_evidence_async(
                file,
                HlsTsProbeProtection::Clear,
                budget,
                probe.expected_duration_ticks_90khz,
            )
            .await
        }
        HlsTerminalTrackEncryption::Aes128 { key_material, iv } => {
            super::super::inspect_mpeg_ts_media_evidence_async(
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
            let super::super::HlsTsMediaEvidence { track_outcome, timestamp_profile, splice_evidence } = evidence;
            HlsTerminalBaseMediaEvidence {
                track_resolution: HlsTrackEvidenceResolution::from(Ok(track_outcome)),
                timestamp_profile,
                splice_evidence,
            }
        }
        Err(error) => HlsTerminalBaseMediaEvidence {
            track_resolution: HlsTrackEvidenceResolution::from(Err(error)),
            timestamp_profile: None,
            splice_evidence: HlsTsSpliceEvidence::Incompatible(
                super::super::HlsTsSpliceIncompatibility::TopologyUnavailable,
            ),
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
