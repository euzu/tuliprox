use super::*;

#[tokio::test]
async fn failed_repair_output_adoption_removes_the_unowned_file() {
    let cache_root = tempfile::tempdir().expect("cache root");
    let outside = tempfile::tempdir().expect("outside root");
    let output = outside.path().join("fixed.ts");
    tokio::fs::write(&output, b"fixed").await.expect("write output");
    let cache = HlsSegmentCache::with_cache_path(cache_root.path());

    assert!(adopt_repair_output(&cache, output.clone(), 5).await.is_err());
    assert!(!output.exists());
}

#[test]
fn repair_remux_selection_drops_invalid_audio_side_stream() {
    let probe = parse_probe(
        r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080 },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6 },
                    { "index": 2, "codec_type": "audio", "codec_name": "ac3", "channels": 0 }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters::default(),
    )
    .expect("probe should parse");

    let selection = select_repair_remux_streams(&probe).expect("valid video should allow remux");

    assert_eq!(selection.mapped_streams, vec![0, 1]);
    assert_eq!(selection.dropped_streams.len(), 1);
    assert_eq!(selection.dropped_streams[0].index, 2);
    assert_eq!(selection.dropped_streams[0].reason, "invalid-audio-parameters");
}

#[test]
fn repair_validation_allows_configured_stream_drop() {
    let raw = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080, "start_time": "0.000000" },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6, "start_time": "0.000000" },
                    { "index": 2, "codec_type": "audio", "codec_name": "ac3", "channels": 0, "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters { codec_parameters_missing: 1, ..WarningCounters::default() },
        )
        .expect("raw probe should parse");
    let fixed = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080, "start_time": "0.000000" },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6, "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters::default(),
        )
        .expect("fixed probe should parse");
    let selection = select_repair_remux_streams(&raw).expect("raw should select valid streams");

    assert!(validate_repair(
        &repair_config(HlsSegmentRepairMode::Low, 1),
        RepairVideoCodec::Hevc,
        &raw,
        &fixed,
        HlsSegmentRepairMode::Low,
        &selection
    )
    .is_ok());
}

#[test]
fn hevc_validation_accepts_when_repair_triggers_are_removed() {
    let raw = parse_probe(
        r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
        WarningCounters {
            pps_id_out_of_range: 1,
            invalid_undecodable_nalu_non_metadata: 2,
            invalid_undecodable_nalu_metadata: 1,
            dolby_vision_rpu: 1,
            ..WarningCounters::default()
        },
    )
    .expect("raw probe should parse");
    let fixed = parse_probe(
        r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1010" }
            }"#,
        WarningCounters { invalid_undecodable_nalu_metadata: 1, dolby_vision_rpu: 1, ..WarningCounters::default() },
    )
    .expect("fixed probe should parse");

    assert!(validate_repair(
        &repair_config(HlsSegmentRepairMode::Medium, 1),
        RepairVideoCodec::Hevc,
        &raw,
        &fixed,
        HlsSegmentRepairMode::Medium,
        &RepairRemuxStreamSelection::preserve_all(&raw)
    )
    .is_ok());
}

#[tokio::test]
async fn remove_access_lease_window_clears_checked_candidates() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let lease_id = HlsAccessLeaseId("lease-a".to_string());

    manager.start_access_lease_window(lease_id.clone()).await;
    assert_eq!(
        selected_repair_mode(&manager, &repair_context("lease-a", "000001")).await,
        Some(HlsSegmentRepairMode::Low)
    );
    assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

    manager.remove_access_lease_window(&lease_id).await;

    assert_eq!(manager.windows.read().await.checked_candidates.len(), 0);
}

#[tokio::test]
async fn remove_proxy_session_state_clears_checked_candidates() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.start_access_lease_window(lease_id.clone()).await;
    let context = repair_context("lease-a", "000001");

    assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

    manager.remove_proxy_session_state(&proxy_session_id, &[lease_id]).await;

    assert_eq!(manager.windows.read().await.checked_candidates.len(), 0);
}

#[tokio::test]
async fn remove_proxy_session_state_clears_object_metadata() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let context = repair_context("lease-a", "000001");
    manager
        .record_object_metadata(
            repair_object_metadata_key(&context, HlsSegmentRepairMode::Low),
            HlsRepairObjectMetadata {
                committed_sha256: "hash".to_string(),
                raw_sha256: Some("raw".to_string()),
                status: RepairStatus::Clean,
                raw_size: 1,
                final_size: 1,
                validation_reason: None,
            },
        )
        .await;

    assert_eq!(manager.stats().await.object_metadata, 1);

    manager.remove_proxy_session_state(&proxy_session_id, &[lease_id]).await;

    assert_eq!(manager.stats().await.object_metadata, 0);
}

#[tokio::test]
async fn repair_lock_cleanup_keeps_waited_lock_and_removes_unused_lock() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let identity = RepairIdentity {
        raw_sha256: "a".repeat(64),
        repair_mode: HlsSegmentRepairMode::Low,
        command_version: 1,
        ffmpeg_version: "test".to_string(),
    };
    let lock = manager.lock_for_identity(identity.clone()).await;
    let waiter = Arc::clone(&lock);

    manager.remove_lock_if_unused(&identity, &lock).await;
    assert_eq!(manager.stats().await.locks, 1);

    drop(waiter);
    manager.remove_lock_if_unused(&identity, &lock).await;
    assert_eq!(manager.stats().await.locks, 0);
}
