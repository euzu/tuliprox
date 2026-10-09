use super::{
    assets::{parse_encryption_method, HlsTerminalBaseScope, ParsedEncryptionMethod},
    plan::encryption_reset_required,
    HlsAccessLeaseId, HlsEncryptionSignature, HlsLeaseManifestSnapshot, HlsTerminalAssetIdentity,
    HlsTerminalBaseMediaState, HlsTerminalBaseProtection, HlsTerminalBaseSegmentAvailability,
    HlsTerminalTailGeneration, HlsTerminalTailManifestRenderInput, HlsTerminalTailPlan, HlsTerminalTailRouteBinding,
    ProxySessionId, TransientResourceFile, HLS_SHARED_LIVE_ROUTE_MARKER,
};
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Write as _,
    sync::Arc,
};
use tuliprox_core::utils::{format_hls_duration_ms, hls_target_duration_secs};

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

pub(super) fn valid_quoted_attribute(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character != '"' && !character.is_control())
}

pub(super) fn valid_iv(value: &str) -> bool { super::super::hls_aes128_cbc_iv(Some(value), 0).is_ok() }

pub(super) fn valid_key_format_versions(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit() || byte == b'/')
}

pub(super) fn safe_terminal_base(
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

pub(super) fn terminal_tail_route_binding(manifest: &HlsLeaseManifestSnapshot) -> Option<HlsTerminalTailRouteBinding> {
    let first = manifest.visible_segments.first()?;
    let first_uri = manifest.materialize_uri(&first.uri);
    let binding = terminal_tail_route_binding_from_uri(&first_uri)?;
    manifest
        .visible_segments
        .iter()
        .all(|segment| {
            terminal_tail_route_binding_from_uri(&manifest.materialize_uri(&segment.uri)).as_ref() == Some(&binding)
        })
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

pub(super) fn terminal_key_resource_file(encryption: &HlsEncryptionSignature) -> Option<TransientResourceFile> {
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

pub(super) fn render_terminal_tail_manifest_body(
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
    render_active_base_encryption(&mut body, input.base_manifest)?;
    for segment in input.base_manifest.visible_segments.iter() {
        if segment.discontinuity_before {
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        writeln!(body, "#EXTINF:{},", format_hls_duration_ms(segment.duration_ms))
            .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
        writeln!(body, "{}", input.base_manifest.materialize_uri(&segment.uri))
            .map_err(|_| HlsTerminalTailRenderError::Formatting)?;
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
    manifest: &HlsLeaseManifestSnapshot,
) -> Result<(), HlsTerminalTailRenderError> {
    let encryption = manifest.active_encryption.as_deref();
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
            let key_uri = manifest.materialize_uri(key_uri);
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
