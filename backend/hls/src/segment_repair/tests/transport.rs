use super::*;

#[test]
fn repair_prewarm_candidates_include_only_ready_clear_origin_ts_media() {
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let mut session = HlsSession::new(HlsSessionKey::new(1, "prewarm-candidates"), b"secret", 0);
    let proxy_seq = 10;
    let cache_key = SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts");
    session.segments.insert(
        proxy_seq,
        SegmentEntry {
            origin_key: OriginSegmentKey {
                origin_epoch: 1,
                effective_host_id: 1,
                host_local_sequence: 100,
                host_local_index: 0,
            },
            proxy_seq,
            duration_ms: 6_000,
            proxy_file_ext: "ts".to_string(),
            content_type: "video/mp2t".to_string(),
            cache_key,
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            encryption: None,
            origin_fetch_ref: None,
            status: SegmentCacheStatus::Ready { content_length: 1_024, ready_at_ms: 1 },
            last_rendered_at_ms: Some(1),
            access: Arc::new(CacheAccessState::new()),
        },
    );
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1,
        first_proxy_seq: proxy_seq,
        last_proxy_seq: proxy_seq,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: 6_000,
            uri: "/live/10.ts".to_string().into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 6_000,
        playlist_duration_ms: 6_000,
        last_visible_media_end_ms: 6_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };

    assert_eq!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 1).len(), 1);
    assert!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 0).is_empty());

    let entry = session.segments.get_mut(&proxy_seq).expect("candidate segment");
    entry.proxy_file_ext = "m4s".to_string();
    assert!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 1).is_empty());
    let entry = session.segments.get_mut(&proxy_seq).expect("candidate segment");
    entry.proxy_file_ext = "ts".to_string();
    entry.encryption = Some(HlsSegmentEncryption {
        resource_id: TransientResourceId("key-a".to_string()),
        resource_extension: "key".to_string(),
        iv: None,
        key_format: None,
        key_format_versions: None,
    });
    assert!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 1).is_empty());
    let entry = session.segments.get_mut(&proxy_seq).expect("candidate segment");
    entry.encryption = None;
    entry.status = SegmentCacheStatus::Discovered;
    assert!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 1).is_empty());
    let entry = session.segments.get_mut(&proxy_seq).expect("candidate segment");
    entry.status = SegmentCacheStatus::Ready { content_length: 1_024, ready_at_ms: 1 };
    entry.origin_key.origin_epoch = HLS_PROVISIONING_ORIGIN_EPOCH;
    assert!(ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, 1).is_empty());
}

#[test]
fn hevc_keyframe_nalu_counts_as_vcl_but_does_not_trigger_alone() {
    let warnings = parse_ffmpeg_warnings("[hevc @ 0x1] Skipping invalid undecodable NALU: 21\n");

    assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
    assert_eq!(warnings.invalid_undecodable_nalu_keyframe, 1);
    assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn hevc_invalid_undecodable_nalu_0_to_31_counts_as_vcl_trigger_input() {
    let warnings =
        parse_ffmpeg_warnings("[hevc @ 0x1] missing SPS\n[hevc @ 0x1] Skipping invalid undecodable NALU: 30\n");

    assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
    assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 1);
    assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn parse_probe_detects_hevc_codec_and_extradata() {
    let probe = parse_probe(
        r#"{
                "streams": [
                    {
                        "index": 0,
                        "codec_type": "video",
                        "codec_name": "hevc",
                        "start_time": "1.250000",
                        "extradata_size": 96
                    }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters::default(),
    )
    .expect("probe should parse");

    assert_eq!(detect_video_codec(&probe), RepairVideoCodec::Hevc);
    assert_eq!(probe.primary_video_extradata_size, Some(96));
}

#[test]
fn repair_remux_selection_rejects_without_valid_video() {
    let probe = parse_probe(
        r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "height": 1080 },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6 }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters::default(),
    )
    .expect("probe should parse");

    let err = select_repair_remux_streams(&probe).expect_err("missing width should reject video");

    assert_eq!(err, "no_valid_video_stream");
}

#[test]
fn validation_rejects_remaining_repair_triggers_even_when_level_improves() {
    let raw = parse_probe(
        r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters { codec_parameters_missing: 1, missing_sps: 1, ..WarningCounters::default() },
    )
    .expect("raw probe should parse");
    let fixed = parse_probe(
        r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters { missing_sps: 1, ..WarningCounters::default() },
    )
    .expect("fixed probe should parse");

    let err = validate_repair(
        &repair_config(HlsSegmentRepairMode::High, 1),
        RepairVideoCodec::Hevc,
        &raw,
        &fixed,
        HlsSegmentRepairMode::High,
        &RepairRemuxStreamSelection::preserve_all(&raw),
    )
    .expect_err("remaining medium trigger should fail validation");
    assert_eq!(err, "repair_triggers_remaining");
}

#[tokio::test]
async fn repair_window_selects_first_unique_segments_per_access_lease() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let first = repair_context("lease-a", "000001");
    let second = repair_context("lease-a", "000002");

    assert_eq!(selected_repair_mode(&manager, &first).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(selected_repair_mode(&manager, &first).await, None);
    assert_eq!(selected_repair_mode(&manager, &second).await, None);
}

#[tokio::test]
async fn non_repairable_objects_do_not_consume_repair_window() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let mut partial = repair_context("lease-a", "1");
    partial.complete_object = false;
    let repairable = repair_context("lease-a", "2");

    assert_eq!(selected_repair_mode(&manager, &partial).await, None);
    assert_eq!(selected_repair_mode(&manager, &repairable).await, Some(HlsSegmentRepairMode::Low));
}

#[test]
fn repair_context_excludes_non_finite_origin_ts_objects() {
    let mut context = repair_context("lease-a", "000001");

    context.complete_object = false;
    assert!(!context.is_repairable_ts());

    context.complete_object = true;
    context.encrypted = true;
    assert!(!context.is_repairable_ts());

    context.encrypted = false;
    context.custom_response = true;
    assert!(!context.is_repairable_ts());

    context.custom_response = false;
    context.file_ext = "m4s".to_string();
    assert!(!context.is_repairable_ts());
}
