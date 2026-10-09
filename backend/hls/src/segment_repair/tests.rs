use super::{
    super::{
        media_reserve::{
            HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
        },
        terminal_tail::HlsMediaContainer,
    },
    parse_ffmpeg_warnings,
    probe::{adopt_repair_output, detect_video_codec, parse_probe, select_repair_remux_streams, validate_repair},
    ready_segment_repair_prewarm_candidates,
    registry::repair_object_metadata_key,
    sha256_file,
    window::{RepairStatus, RepairVideoCodec},
    HlsRepairObjectMetadata, HlsRepairRenderedObjectId, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, RepairIdentity, RepairRemuxStreamSelection, WarningCounters, REPAIR_METADATA_MAX_ENTRIES,
};
use crate::{
    timeline::HLS_PROVISIONING_ORIGIN_EPOCH, CacheAccessState, HlsAccessLeaseId, HlsSegmentCache, HlsSegmentEncryption,
    HlsSession, HlsSessionKey, OriginSegmentKey, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
    TransientObjectCacheKey, TransientResourceId,
};
use shared::model::HlsSegmentRepairMode;
use std::{sync::Arc, time::Duration};
use tuliprox_core::model::HlsSegmentRepairConfig;

fn repair_config(mode: HlsSegmentRepairMode, apply_to_first_segments: u8) -> HlsSegmentRepairConfig {
    HlsSegmentRepairConfig { max_level: mode, apply_to_first_segments, max_parallel_repairs: 1, ..Default::default() }
}

fn should_repair(codec: RepairVideoCodec, warnings: &WarningCounters) -> bool {
    super::diagnostics::decide_repair(codec, warnings).required_level != HlsSegmentRepairMode::Off
}

fn repair_context(lease_id: &str, resource_id: &str) -> HlsSegmentRepairObjectContext {
    HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Normal,
        log_identity: super::HlsLogIdentity::for_test("input:1|hls|stream-a", "proxy-session"),
        proxy_session_id: ProxySessionId("proxy-session".to_string()),
        hls_access_lease_id: Some(HlsAccessLeaseId(lease_id.to_string())),
        rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: resource_id.parse().unwrap_or(1) },
        resource_id: resource_id.to_string(),
        file_ext: "ts".to_string(),
        origin_fetch_uri_for_diagnostics: format!("http://origin.example/{resource_id}.ts"),
        media_sequence: Some(1),
        discontinuity_sequence: Some(0),
        complete_object: true,
        encrypted: false,
        custom_response: false,
    }
}

async fn selected_repair_mode(
    manager: &HlsSegmentRepairManager,
    context: &HlsSegmentRepairObjectContext,
) -> Option<HlsSegmentRepairMode> {
    manager.try_select_candidate(context).await.map(|(mode, _, _)| mode)
}

mod lifecycle;
mod policy;
mod publication;
mod storage;
mod streaming;
mod terminal;
mod transport;
