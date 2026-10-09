use super::{
    HlsTerminalKeyBinding, HlsTerminalKeyMaterial, HlsTerminalMediaAsset, HlsTerminalTailCompatibility,
    HlsTsTrackSignature, HlsValidatedTerminalMediaAsset, ProxySessionId, TransientObjectCacheKey,
    TransientResourceFile, TransientResourceId, AES_128_BLOCK_BYTES,
};
use bytes::Bytes;
use std::sync::Arc;
use tuliprox_mpegts::transport_stream_buffer::{HlsTsTimestampProfile, TransportStreamBuffer};
use zeroize::Zeroizing;

impl HlsTerminalKeyMaterial {
    pub(super) fn from_slice(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; AES_128_BLOCK_BYTES] = bytes.try_into().ok()?;
        Some(Self { bytes: Zeroizing::new(bytes) })
    }

    pub(super) fn as_bytes(&self) -> &[u8; AES_128_BLOCK_BYTES] { &self.bytes }
}

impl std::fmt::Debug for HlsTerminalKeyMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HlsTerminalKeyMaterial(<redacted>)")
    }
}

impl HlsTerminalKeyBinding {
    pub(super) fn new(
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

    pub(super) fn matches_resource(&self, resource_file: &TransientResourceFile) -> bool {
        self.resource_id == resource_file.resource_id && self.route_extension.as_ref() == resource_file.extension
    }

    pub(super) fn matches_route(&self, proxy_session_id: &ProxySessionId) -> bool {
        self.proxy_session_id == *proxy_session_id
    }

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
pub(super) enum ParsedEncryptionMethod<'a> {
    None,
    Aes128,
    Unsupported(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsTerminalBaseScope {
    SafeSuffix,
    LastSafeSegment,
}

pub(super) fn parse_encryption_method(method: &str) -> ParsedEncryptionMethod<'_> {
    if method.eq_ignore_ascii_case("NONE") {
        ParsedEncryptionMethod::None
    } else if method.eq_ignore_ascii_case("AES-128") {
        ParsedEncryptionMethod::Aes128
    } else {
        ParsedEncryptionMethod::Unsupported(method)
    }
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
    pub(super) fn identity(&self) -> HlsTerminalAssetIdentity {
        HlsTerminalAssetIdentity { revision: self.validated.revision, fingerprint: self.validated.fingerprint }
    }

    pub fn container(&self) -> HlsMediaContainer { self.validated.container }

    pub fn track_signature(&self) -> &HlsTsTrackSignature { &self.validated.track_signature }

    pub fn duration_ms(&self) -> u64 { self.validated.duration_ms }

    pub fn duration_ticks_90khz(&self) -> u64 { self.validated.duration_ticks_90khz }

    pub fn timestamp_profile(&self) -> Option<HlsTsTimestampProfile> { self.validated.timestamp_profile }

    pub fn content_type(&self) -> &'static str { self.validated.content_type }

    pub(crate) fn renderer(&self) -> Arc<TransportStreamBuffer> { Arc::clone(&self.validated.renderer) }
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
