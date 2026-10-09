use super::{
    super::media_reserve::{HlsLeaseManifestSegment, HlsManifestCommitIdentity, HlsManifestDeliveryMode},
    *,
};
use bytes::Bytes;
use tuliprox_core::utils::format_hls_duration_ms;
use tuliprox_mpegts::transport_stream_buffer::{HlsFiniteTsRenderSpec, TransportStreamBuffer};

const TERMINAL_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

fn manifest() -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
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
    let anchored_bundle = HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, base_manifest.target_duration_ms);
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

fn resettable_aes128_encryption() -> Arc<HlsEncryptionSignature> {
    Arc::new(HlsEncryptionSignature {
        method: "AES-128".into(),
        key_uri: Some("/iptv/hls/shared/live/session/lease/r/aes-key.key".into()),
        iv: Some("0x000000000000000000000000000000C2".into()),
        key_format: Some("identity".into()),
        key_format_versions: Some("1".into()),
        can_reset_to_clear: true,
    })
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
    cache.write_bytes_and_commit(&cache_key, TERMINAL_ASSET_BYTES).await.expect("terminal evidence segment commits");
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
        uri: format!("/iptv/hls/shared/live/{}/lease/{proxy_seq}.ts", session.proxy_session_id.0).into(),
        discontinuity_before: false,
        map_ref_ready: true,
        encryption: Some(Arc::new(encryption.clone())),
    }]);
    base.active_encryption = Some(Arc::new(encryption));
    base.playlist_duration_ms = 4_000;
    base.last_visible_media_end_ms = 4_000;
    let session = Arc::new(tokio::sync::RwLock::new(session));

    prepare_terminal_base_evidence(&session, &cache, &base, 3).await
}

async fn terminal_timing_session(
    cache: &HlsSegmentCache,
    terminal_asset: &HlsTerminalMediaAsset,
    first_bytes: &[u8],
    last_bytes: &[u8],
) -> (Arc<tokio::sync::RwLock<super::super::HlsSession>>, HlsLeaseManifestSnapshot, super::super::SegmentCacheKey, u64)
{
    let mut session =
        super::super::HlsSession::new(super::super::HlsSessionKey::new(1, "terminal-timing-evidence"), b"secret", 1);
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
        uri: format!("/iptv/hls/shared/live/{}/lease/{proxy_seq}.ts", proxy_session_id.0).into(),
        discontinuity_before: false,
        map_ref_ready: true,
        encryption: None,
    }));
    base.playlist_duration_ms = terminal_asset.duration_ms().saturating_mul(2);
    base.last_visible_media_end_ms = base.playlist_duration_ms;
    (Arc::new(tokio::sync::RwLock::new(session)), base, last_cache_key, last_proxy_seq)
}

mod policy;
mod publication;
mod storage;
mod terminal;
mod transport;
