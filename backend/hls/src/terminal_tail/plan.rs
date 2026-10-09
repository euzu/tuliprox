#[cfg(any(test, feature = "test-support"))]
use super::HlsTerminalBaseTrackIdentity;
use super::{
    assets::{parse_encryption_method, HlsTerminalBaseScope, ParsedEncryptionMethod},
    diagnostics::log_terminal_splice_compatibility,
    render::{
        render_terminal_tail_manifest_body, safe_terminal_base, terminal_key_resource_file,
        terminal_tail_route_binding, valid_iv, valid_key_format_versions, valid_quoted_attribute,
    },
    terminal_asset_fits_target_duration, HlsAccessLeaseId, HlsAnchoredTerminalBundle, HlsAnchoredTerminalSegment,
    HlsEncryptionSignature, HlsLeaseManifestSnapshot, HlsManifestDeliveryMode, HlsMediaContainer,
    HlsPreparedTerminalBundleKey, HlsRuntimeCustomTailAssetIdentity, HlsTerminalAssetIdentity,
    HlsTerminalBaseSegmentAvailability, HlsTerminalBaseTimingEvidence, HlsTerminalKeyBinding, HlsTerminalMediaAsset,
    HlsTerminalSegmentPath, HlsTerminalTailGeneration, HlsTerminalTailManifestRenderInput, HlsTerminalTailPlan,
    HlsTerminalTailRouteBinding, HlsTsSpliceEvidence, HlsTsTrackSignature, ProxySessionId, TransientResourceFile,
    HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
use bytes::Bytes;
use std::sync::Arc;

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
        let prepared = super::super::prepared_terminal_bundle::build_prepared_terminal_bundle(asset, key)
            .expect("relative terminal test bundle");
        let profile = asset.timestamp_profile().expect("terminal test asset timing profile");
        let anchor = tuliprox_mpegts::transport_stream_buffer::HlsTsSpliceAnchor::between(profile, profile)
            .expect("terminal test splice anchor");
        super::super::prepared_terminal_bundle::anchor_prepared_terminal_bundle(asset, &prepared, anchor)
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
                cache_key: super::super::SegmentCacheKey::new(
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
    pub fn media_preparation_key(&self) -> super::super::recovery_timing::HlsTerminalMediaPreparationKey {
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

    pub(super) fn prepared_segment(&self, path: HlsTerminalSegmentPath) -> Option<&HlsAnchoredTerminalSegment> {
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
    SpliceTransportFailure(super::super::HlsTsSpliceIncompatibility),
    SpliceTopologyMismatch,
    MissingTimestampAnchor,
    InvalidTimestampTransition,
    TransientPassthroughUnsupported,
    ProtectionCapacityExceeded,
    InvalidLeaseRoute,
    ManifestRenderFailed,
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
        match super::super::evaluate_mpeg_ts_splice_boundary(base, terminal) {
            Ok(()) => {}
            Err(super::super::HlsTsSpliceBoundaryIncompatibility::Media(reason)) => {
                return HlsTerminalTailCompatibility::SpliceTransportFailure(reason);
            }
            Err(super::super::HlsTsSpliceBoundaryIncompatibility::TopologyMismatch) => {
                return HlsTerminalTailCompatibility::SpliceTopologyMismatch;
            }
        }
    }
    if encryption_reset_required(manifest.active_encryption.as_deref()).is_err() {
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

pub(super) fn encryption_reset_required(
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
    let append_key_method_none = encryption_reset_required(input.base_manifest.active_encryption.as_deref())?;
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
