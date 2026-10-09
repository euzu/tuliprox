use super::*;

#[tokio::test]
async fn update_config_applies_to_new_access_lease_windows() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Medium, 2));
    let mut updated = repair_config(HlsSegmentRepairMode::Medium, 3);
    updated.max_parallel_repairs = 2;
    manager.update_config(updated);

    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;

    for resource_id in ["1", "2", "3"] {
        assert!(
            manager.try_select_candidate(&repair_context("lease-a", resource_id)).await.is_some(),
            "resource {resource_id} should be inside updated repair window"
        );
    }
    assert!(
        manager.try_select_candidate(&repair_context("lease-a", "4")).await.is_none(),
        "fourth resource should be outside updated repair window"
    );
}

#[test]
fn warning_parser_expands_repeated_messages() {
    let warnings = parse_ffmpeg_warnings(
        "non-existing SPS 0 referenced in buffering period\nLast message repeated 2 times\nno frame!\n",
    );

    assert_eq!(warnings.missing_sps, 3);
    assert_eq!(warnings.no_frame, 1);
}

#[test]
fn mmco_warning_alone_does_not_trigger_repair() {
    let warnings = WarningCounters { mmco_unref_short_failure: 20, ..WarningCounters::default() };

    assert!(!should_repair(RepairVideoCodec::H264, &warnings));
}

#[test]
fn critical_warning_triggers_repair() {
    let warnings = WarningCounters { missing_sps: 1, ..WarningCounters::default() };

    assert!(should_repair(RepairVideoCodec::H264, &warnings));
}

#[test]
fn hevc_pps_warning_triggers_repair() {
    let warnings = parse_ffmpeg_warnings("[hevc @ 0x1] PPS id out of range: 0\n");

    assert_eq!(warnings.pps_id_out_of_range, 1);
    assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn hevc_invalid_slice_nalus_with_parameter_issue_trigger_repair() {
    let warnings = parse_ffmpeg_warnings(
            "[hevc @ 0x1] missing SPS\n[hevc @ 0x1] Skipping invalid undecodable NALU: 0\n[hevc @ 0x1] Skipping invalid undecodable NALU: 1\n",
        );

    assert_eq!(warnings.invalid_undecodable_nalu_total, 2);
    assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 2);
    assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn hevc_metadata_and_dolby_warnings_do_not_trigger_repair_alone() {
    let warnings = parse_ffmpeg_warnings(
            "[hevc @ 0x1] Skipping invalid undecodable NALU: 39\nMultiple Dolby Vision RPUs found in one AU. Skipping previous.\nAudio/Video desynchronisation detected!\n",
        );

    assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
    assert_eq!(warnings.invalid_undecodable_nalu_metadata, 1);
    assert_eq!(warnings.dolby_vision_rpu, 1);
    assert_eq!(warnings.av_desync, 1);
    assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn hevc_repeated_messages_expand_counters_without_triggering_alone() {
    let warnings =
        parse_ffmpeg_warnings("[hevc @ 0x1] Skipping invalid undecodable NALU: 0\nLast message repeated 2 times\n");

    assert_eq!(warnings.invalid_undecodable_nalu_total, 3);
    assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 3);
    assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
}

#[test]
fn warning_parser_matches_trigger_patterns_case_insensitively() {
    let warnings = parse_ffmpeg_warnings(
        "NON-EXISTING SPS 0 referenced\ninvalid nal unit 1\ncould not find codec parameters for stream 0\n",
    );

    assert_eq!(warnings.missing_sps, 1);
    assert_eq!(warnings.invalid_nal, 1);
    assert_eq!(warnings.codec_parameters_missing, 1);
    assert!(should_repair(RepairVideoCodec::H264, &warnings));
}

#[test]
fn unsupported_codec_never_triggers_repair() {
    let warnings = WarningCounters { missing_sps: 1, pps_id_out_of_range: 1, ..WarningCounters::default() };

    assert!(!should_repair(RepairVideoCodec::Unsupported, &warnings));
}

#[test]
fn configured_max_below_required_skips_repair() {
    assert_eq!(
        HlsSegmentRepairMode::Low.execution_plan(HlsSegmentRepairMode::High),
        shared::model::HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired
    );
    assert_eq!(
        HlsSegmentRepairMode::Medium.execution_plan(HlsSegmentRepairMode::Low),
        shared::model::HlsSegmentRepairExecutionPlan::Repair(HlsSegmentRepairMode::Low)
    );
    assert_eq!(
        HlsSegmentRepairMode::Off.execution_plan(HlsSegmentRepairMode::High),
        shared::model::HlsSegmentRepairExecutionPlan::SkipNoTrigger
    );
}

#[tokio::test]
async fn background_candidate_without_access_lease_is_ignored_before_window_check() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let mut context = repair_context("lease-a", "000001");
    context.hls_access_lease_id = None;

    assert_eq!(selected_repair_mode(&manager, &context).await, None);

    let stats = manager.stats().await;
    assert_eq!(stats.windows, 1);
    assert_eq!(stats.checked_candidates, 0);
    assert_eq!(stats.object_metadata, 0);
}

#[tokio::test]
async fn repair_window_is_separate_per_access_lease() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Medium, 1));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;

    assert_eq!(
        selected_repair_mode(&manager, &repair_context("lease-a", "000001")).await,
        Some(HlsSegmentRepairMode::Medium)
    );
    assert_eq!(
        selected_repair_mode(&manager, &repair_context("lease-b", "000001")).await,
        Some(HlsSegmentRepairMode::Medium)
    );
}

#[tokio::test]
async fn known_normal_object_consumes_new_lease_window_without_rescan() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let cache_key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 1, "ts");
    let metadata = cache.write_bytes_and_commit(&cache_key, b"cached-normal-bytes").await.expect("commit");
    let committed_sha256 = sha256_file(&metadata.path).await.expect("hash");
    let previous_context = repair_context("lease-a", "1");
    manager
        .record_object_metadata(
            repair_object_metadata_key(&previous_context, HlsSegmentRepairMode::Low),
            HlsRepairObjectMetadata {
                committed_sha256,
                raw_sha256: Some("previous-raw".to_string()),
                status: RepairStatus::Clean,
                raw_size: metadata.size,
                final_size: metadata.size,
                validation_reason: None,
            },
        )
        .await;

    manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;

    assert!(manager
        .repair_ready_cache_hit(&cache, &cache_key, repair_context("lease-b", "1"))
        .await
        .expect("repair cache hit")
        .is_none());
    assert_eq!(manager.stats().await.metadata, 0);
    assert_eq!(selected_repair_mode(&manager, &repair_context("lease-b", "2")).await, None);
}

#[tokio::test]
async fn known_transient_object_consumes_new_lease_window_without_rescan() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let cache_key = TransientObjectCacheKey::new(
        ProxySessionId("proxy-session".to_string()),
        TransientResourceId("resource-a".to_string()),
        "ts",
    );
    let metadata = cache.write_bytes_and_commit(&cache_key, b"cached-transient-bytes").await.expect("commit");
    let committed_sha256 = sha256_file(&metadata.path).await.expect("hash");
    let mut previous_context = repair_context("lease-a", "1");
    previous_context.source = HlsSegmentRepairSource::Transient;
    previous_context.rendered_object_id =
        HlsRepairRenderedObjectId::Transient { resource_id: "resource-a".to_string() };
    previous_context.resource_id = "resource-a".to_string();
    manager
        .record_object_metadata(
            repair_object_metadata_key(&previous_context, HlsSegmentRepairMode::Low),
            HlsRepairObjectMetadata {
                committed_sha256,
                raw_sha256: Some("previous-raw".to_string()),
                status: RepairStatus::Clean,
                raw_size: metadata.size,
                final_size: metadata.size,
                validation_reason: None,
            },
        )
        .await;

    manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;
    let mut current_context = previous_context.clone();
    current_context.hls_access_lease_id = Some(HlsAccessLeaseId("lease-b".to_string()));

    assert!(manager
        .repair_ready_cache_hit(&cache, &cache_key, current_context)
        .await
        .expect("repair cache hit")
        .is_none());
    assert_eq!(manager.stats().await.metadata, 0);
    let mut second_context = repair_context("lease-b", "2");
    second_context.source = HlsSegmentRepairSource::Transient;
    second_context.rendered_object_id = HlsRepairRenderedObjectId::Transient { resource_id: "resource-b".to_string() };
    second_context.resource_id = "resource-b".to_string();
    assert_eq!(selected_repair_mode(&manager, &second_context).await, None);
}

#[tokio::test]
async fn object_metadata_hash_mismatch_does_not_skip_repair_evaluation() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let context = repair_context("lease-a", "1");
    let object_key = repair_object_metadata_key(&context, HlsSegmentRepairMode::Low);
    manager
        .record_object_metadata(
            object_key.clone(),
            HlsRepairObjectMetadata {
                committed_sha256: "old-hash".to_string(),
                raw_sha256: Some("old-raw".to_string()),
                status: RepairStatus::Clean,
                raw_size: 1,
                final_size: 1,
                validation_reason: None,
            },
        )
        .await;

    assert!(!manager.object_metadata_matches(&object_key, "new-hash").await);
}

#[tokio::test]
async fn repair_disabled_does_not_track_candidates() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Off, 1));
    let context = repair_context("lease-a", "000001");

    assert_eq!(selected_repair_mode(&manager, &context).await, None);

    let registry = manager.windows.read().await;
    assert_eq!(registry.checked_candidates.len(), 0);
}

#[tokio::test]
async fn transient_commit_and_cache_hit_consume_repair_window_once() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let mut commit_context = repair_context("lease-a", "1");
    commit_context.source = HlsSegmentRepairSource::Transient;
    commit_context.rendered_object_id = HlsRepairRenderedObjectId::Transient { resource_id: "resource-a".to_string() };
    commit_context.resource_id = "resource-a".to_string();
    commit_context.origin_fetch_uri_for_diagnostics = "http://origin.example/live/resource-a.ts".to_string();
    commit_context.media_sequence = None;
    commit_context.discontinuity_sequence = None;

    let mut cache_hit_context = commit_context.clone();
    cache_hit_context.origin_fetch_uri_for_diagnostics = "resource-a".to_string();

    let mut second_context = commit_context.clone();
    second_context.rendered_object_id = HlsRepairRenderedObjectId::Transient { resource_id: "resource-b".to_string() };
    second_context.resource_id = "resource-b".to_string();
    second_context.origin_fetch_uri_for_diagnostics = "resource-b".to_string();

    assert_eq!(selected_repair_mode(&manager, &commit_context).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(selected_repair_mode(&manager, &cache_hit_context).await, None);
    assert_eq!(selected_repair_mode(&manager, &second_context).await, Some(HlsSegmentRepairMode::Low));
}

#[tokio::test]
async fn normal_commit_and_cache_hit_consume_repair_window_once() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
    manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
    let commit_context = repair_context("lease-a", "1");
    let mut cache_hit_context = commit_context.clone();
    cache_hit_context.origin_fetch_uri_for_diagnostics = "http://redirect.example/other-path.ts".to_string();
    cache_hit_context.media_sequence = Some(99);
    cache_hit_context.discontinuity_sequence = Some(7);
    let second_context = repair_context("lease-a", "2");

    assert_eq!(selected_repair_mode(&manager, &commit_context).await, Some(HlsSegmentRepairMode::Low));
    assert_eq!(selected_repair_mode(&manager, &cache_hit_context).await, None);
    assert_eq!(selected_repair_mode(&manager, &second_context).await, Some(HlsSegmentRepairMode::Low));
}

#[tokio::test]
async fn ensuring_pending_repair_window_does_not_reset_consumed_candidate_budget() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.ensure_access_lease_window(lease_id.clone()).await;

    assert_eq!(selected_repair_mode(&manager, &repair_context("lease-a", "1")).await, Some(HlsSegmentRepairMode::Low));
    manager.ensure_access_lease_window(lease_id).await;

    assert_eq!(selected_repair_mode(&manager, &repair_context("lease-a", "2")).await, None);
    assert_eq!(manager.stats().await.generations, 1);
}

#[tokio::test]
async fn prewarmed_ready_object_reuses_metadata_on_later_demand() {
    const CLEAN_TS: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let manager = Arc::new(HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1)));
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let cache_key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 1, "ts");
    cache.write_bytes_and_commit(&cache_key, CLEAN_TS).await.expect("cache fixture");
    manager.ensure_access_lease_window(lease_id).await;
    let context = repair_context("lease-a", "1");

    manager.spawn_ready_cache_prewarm_inner(Arc::clone(&cache), vec![(cache_key.clone(), context.clone())], None).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if manager.stats().await.object_metadata == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prewarm completes");
    let after_prewarm = manager.stats().await;
    assert_eq!(after_prewarm.object_metadata, 1);

    assert!(manager
        .repair_ready_cache_hit(&cache, &cache_key, context)
        .await
        .expect("demand reuses prewarm")
        .is_none());
    assert_eq!(manager.stats().await, after_prewarm);
}

#[tokio::test]
async fn repair_metadata_is_bounded() {
    let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));

    for index in 0..REPAIR_METADATA_MAX_ENTRIES + 5 {
        manager
            .record_metadata(
                RepairIdentity {
                    raw_sha256: format!("{index:064x}"),
                    repair_mode: HlsSegmentRepairMode::Low,
                    command_version: 1,
                    ffmpeg_version: "test".to_string(),
                },
                RepairStatus::Clean,
                1,
                1,
                None,
            )
            .await;
    }

    assert_eq!(manager.stats().await.metadata, REPAIR_METADATA_MAX_ENTRIES);
}
