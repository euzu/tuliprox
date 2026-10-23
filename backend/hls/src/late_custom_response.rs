//! Legacy standalone lease-plan shape. Published live leases reject this plan;
//! its constructors exist only for regression tests. Unpublished standalone
//! responses remain owned by `runtime_custom_tail`.

#[cfg(test)]
use crate::{
    hls_ctx::HlsCtx,
    lease::{HlsAccessLeaseId, HlsTerminalTailPreparation},
    prepared_terminal_bundle::{prepared_terminal_bundle_key, HlsPreparedTerminalBundle},
    runtime_custom_tail::{
        await_prepared_standalone_bundle, snapshot_hls_runtime_custom_tail_asset, standalone_custom_asset_fingerprint,
        HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason, HlsStandaloneCustomPlan,
        HlsStandaloneCustomPlanKey,
    },
    terminal_tail::{HlsLeasePlaybackMode, HlsTerminalTailCompatibility, HlsTerminalTailGeneration},
    HlsAccessLeaseState, ProxySessionId,
};
#[cfg(not(test))]
use crate::{
    lease::HlsAccessLeaseId,
    runtime_custom_tail::{standalone_custom_asset_fingerprint, HlsStandaloneCustomPlan, HlsStandaloneCustomPlanKey},
    terminal_tail::HlsTerminalTailGeneration,
    ProxySessionId,
};
use std::fmt;
#[cfg(test)]
use std::{fmt::Write, sync::Arc};
#[cfg(test)]
use tuliprox_core::utils::{format_hls_duration_ms, hls_target_duration_secs};

/// Immutable, lease-bound custom playlist that does not consume a live splice base.
#[derive(Clone)]
pub struct HlsLateCustomResponsePlan {
    pub generation: HlsTerminalTailGeneration,
    pub(crate) manifest_snapshot_generation: u64,
    pub(crate) key: HlsStandaloneCustomPlanKey,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    standalone: HlsStandaloneCustomPlan,
}

impl HlsLateCustomResponsePlan {
    pub fn matches_route(&self, proxy_session_id: &ProxySessionId, lease_id: &HlsAccessLeaseId) -> bool {
        self.proxy_session_id == *proxy_session_id && self.lease_id == *lease_id
    }

    pub fn manifest_body(&self) -> &str { &self.standalone.manifest_body }

    pub(crate) fn segment_bytes(
        &self,
        fingerprint: &str,
        index: u16,
    ) -> Result<bytes::Bytes, crate::runtime_custom_tail::HlsStandaloneCustomSegmentError> {
        use crate::runtime_custom_tail::HlsStandaloneCustomSegmentError;
        if fingerprint != standalone_custom_asset_fingerprint(self.key.asset) {
            return Err(HlsStandaloneCustomSegmentError::StaleAssetFingerprint);
        }
        self.standalone.segment_bytes(index).ok_or(HlsStandaloneCustomSegmentError::InvalidIndex)
    }
}

impl fmt::Debug for HlsLateCustomResponsePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HlsLateCustomResponsePlan")
            .field("generation", &self.generation)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl PartialEq for HlsLateCustomResponsePlan {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.manifest_snapshot_generation == other.manifest_snapshot_generation
            && self.key == other.key
            && self.proxy_session_id == other.proxy_session_id
            && self.lease_id == other.lease_id
            && self.standalone.manifest_body == other.standalone.manifest_body
    }
}

impl Eq for HlsLateCustomResponsePlan {}

#[cfg(test)]
pub(crate) enum HlsLateCustomResponseError {
    Superseded,
    Unavailable(HlsTerminalTailCompatibility),
}

#[cfg(test)]
pub(crate) async fn prepare_late_custom_response(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    now_ms: u64,
) -> Result<(Arc<HlsLateCustomResponsePlan>, HlsRuntimeCustomTailAssetIdentity), HlsLateCustomResponseError> {
    let lease = ctx
        .hls_proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, now_ms)
        .await
        .ok_or(HlsLateCustomResponseError::Superseded)?;
    if !matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
        || lease.playback_mode != HlsLeasePlaybackMode::Live
        || lease.issued_at_ms != preparation.lease_issued_at_ms
        || lease.admission_generation != preparation.expected_admission_generation
        || lease.playback_cursor.cursor_generation != preparation.cursor_generation
        || lease.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
            != Some(preparation.manifest_snapshot_generation)
    {
        return Err(HlsLateCustomResponseError::Superseded);
    }
    let snapshot = snapshot_hls_runtime_custom_tail_asset(ctx, HlsRuntimeCustomTailReason::ChannelUnavailable)
        .map_err(|error| HlsLateCustomResponseError::Unavailable(error.terminal_compatibility()))?;
    let expected_asset = HlsRuntimeCustomTailAssetIdentity::from_asset(&snapshot);
    let bundle_key = prepared_terminal_bundle_key(
        &snapshot.asset,
        preparation.manifest_snapshot.target_duration_ms,
        crate::terminal_tail::HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let bundle = await_prepared_standalone_bundle(ctx, Arc::clone(&snapshot.asset), bundle_key)
        .await
        .map_err(HlsLateCustomResponseError::Unavailable)?;
    let first_proxy_seq = lease
        .published_media_sequence_highwater
        .unwrap_or(preparation.manifest_snapshot.last_proxy_seq)
        .checked_add(1)
        .ok_or(HlsLateCustomResponseError::Unavailable(HlsTerminalTailCompatibility::ManifestRenderFailed))?;
    let discontinuity_sequence =
        lease.published_discontinuity_sequence_highwater.max(preparation.manifest_snapshot.discontinuity_sequence);
    let path_prefix = preparation
        .manifest_snapshot
        .visible_segments
        .first()
        .and_then(|segment| {
            let uri = preparation.manifest_snapshot.materialize_uri(&segment.uri);
            uri.find("/hls/shared/live/").map(|offset| uri[..offset].to_owned())
        })
        .ok_or(HlsLateCustomResponseError::Unavailable(HlsTerminalTailCompatibility::InvalidLeaseRoute))?;
    let manifest_body = render_late_manifest(first_proxy_seq, discontinuity_sequence, &path_prefix, lease_id, &bundle)?;
    let key = HlsStandaloneCustomPlanKey {
        reason: snapshot.reason,
        asset: expected_asset.media,
        segment_count: bundle.key.segment_count,
    };
    let standalone = HlsStandaloneCustomPlan { key, manifest_body: Arc::from(manifest_body), prepared_bundle: bundle };
    Ok((
        Arc::new(HlsLateCustomResponsePlan {
            generation: HlsTerminalTailGeneration(preparation.decision_generation),
            manifest_snapshot_generation: preparation.manifest_snapshot_generation,
            key,
            proxy_session_id: proxy_session_id.clone(),
            lease_id: lease_id.clone(),
            standalone,
        }),
        expected_asset,
    ))
}

// Keeping the layout separate makes the publication boundary testable without a live origin or cache.
#[cfg(test)]
fn render_late_manifest(
    first_proxy_seq: u64,
    discontinuity_sequence: u64,
    path_prefix: &str,
    lease_id: &HlsAccessLeaseId,
    bundle: &HlsPreparedTerminalBundle,
) -> Result<String, HlsLateCustomResponseError> {
    first_proxy_seq
        .checked_add(u64::from(bundle.key.segment_count).saturating_sub(1))
        .ok_or(HlsLateCustomResponseError::Unavailable(HlsTerminalTailCompatibility::ManifestRenderFailed))?;
    let fingerprint = standalone_custom_asset_fingerprint(bundle.key.asset);
    let extinf = format_hls_duration_ms(bundle.source_asset_duration_ms);
    let mut manifest = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{first_proxy_seq}\n#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}\n#EXT-X-KEY:METHOD=NONE\n#EXT-X-DISCONTINUITY\n", hls_target_duration_secs(bundle.key.target_duration_ms));
    for index in 0..bundle.key.segment_count {
        let _ = writeln!(manifest, "#EXTINF:{extinf},\n{path_prefix}/cvs/hls/{}/{fingerprint}/{index}.ts", lease_id.0);
    }
    manifest.push_str("#EXT-X-ENDLIST\n");
    Ok(manifest)
}

#[cfg(test)]
mod tests;
