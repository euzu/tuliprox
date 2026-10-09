use super::*;

#[tokio::test]
async fn terminal_base_media_evidence_scans_complete_cache_file() {
    let mut file = tempfile::NamedTempFile::new().expect("temporary cache file");
    std::io::Write::write_all(&mut file, TERMINAL_ASSET_BYTES).expect("write valid TS");
    let source_size = file.as_file().metadata().expect("cache metadata").len();
    let terminal_asset = asset();
    let probe = HlsTerminalMediaProbe {
        segment_path: file.path().to_path_buf(),
        source_size,
        expected_duration_ticks_90khz: terminal_asset.duration_ticks_90khz(),
        encryption: HlsTerminalTrackEncryption::Clear,
    };

    let evidence = terminal_base_media_evidence(probe).await;
    let expected_signature = terminal_asset.track_signature().clone();

    assert_eq!(evidence.track_resolution.signature(), Some(&expected_signature));
    assert_eq!(evidence.track_resolution.reason_code(), "found");
    assert_eq!(evidence.timestamp_profile, terminal_asset.timestamp_profile());
    assert!(matches!(evidence.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
}

#[tokio::test]
async fn terminal_base_timestamp_profile_uses_exact_pinned_last_safe_segment() {
    let temp_dir = tempfile::tempdir().expect("terminal timing cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let terminal_asset = asset();
    let renderer = terminal_asset.renderer();
    let first_bytes = renderer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("first cached segment");
    let last_offset = 9_000_000;
    let last_bytes = renderer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: last_offset,
            continuity_seed: 0,
            logical_segment_index: 1,
        })
        .expect("last cached segment");
    let expected_profile = TransportStreamBuffer::new(last_bytes.to_vec())
        .finite_hls_timestamp_profile()
        .expect("last segment timestamp profile");
    let (session, base, last_cache_key, last_proxy_seq) =
        terminal_timing_session(&cache, &terminal_asset, &first_bytes, &last_bytes).await;

    let evidence = prepare_terminal_base_evidence(&session, &cache, &base, 3).await;
    let timing = evidence.timing().expect("exact last-base timing evidence");

    assert_eq!(timing.base.proxy_seq, last_proxy_seq);
    assert_eq!(timing.base.origin_epoch, 7);
    assert_eq!(timing.base.cache_key, last_cache_key);
    assert_eq!(timing.profile, expected_profile);
    assert_ne!(
        timing.profile,
        TransportStreamBuffer::new(first_bytes.to_vec())
            .finite_hls_timestamp_profile()
            .expect("first segment timestamp profile")
    );
}

#[tokio::test]
async fn terminal_base_preserves_invalid_iv_and_missing_key_evidence() {
    let invalid_iv = unresolved_aes_terminal_base_evidence("not-an-iv").await;
    assert!(invalid_iv.track_base().is_some());
    assert_eq!(invalid_iv.track_resolution(), Some(&HlsTrackEvidenceResolution::InvalidIv));
    assert_eq!(invalid_iv.track_evidence_reason_code(), "invalid-iv");

    let missing_key = unresolved_aes_terminal_base_evidence("0x00000000000000000000000000000384").await;
    assert!(missing_key.track_base().is_some());
    assert_eq!(missing_key.track_resolution(), Some(&HlsTrackEvidenceResolution::KeyUnavailable));
    assert_eq!(missing_key.track_evidence_reason_code(), "key-unavailable");
}

#[test]
fn terminal_tail_plan_serves_preanchored_bytes_without_request_time_rendering() {
    let asset = asset();
    let renderer = asset.renderer();
    let first_input = build_input(4, manifest(), Arc::clone(&asset));
    let shared_bundle = Arc::clone(&first_input.anchored_bundle);
    let mut second_input = build_input(5, manifest(), asset);
    second_input.anchored_bundle = Arc::clone(&shared_bundle);
    let plan = build_terminal_tail_plan(first_input).expect("compatible tail");
    let second_plan = build_terminal_tail_plan(second_input).expect("second compatible tail");
    let render_count_after_plan_commit = renderer.finite_hls_render_count();
    let finalize_count_after_plan_commit = renderer.finite_hls_finalize_count();
    assert!(Arc::ptr_eq(&plan.anchored_bundle, &second_plan.anchored_bundle));
    let first_path = HlsTerminalSegmentPath { generation: plan.generation, index: 0 };
    let second_path = HlsTerminalSegmentPath { generation: plan.generation, index: 1 };

    let first = plan.segment_bytes(first_path).expect("first prepared segment exists");
    assert_eq!(plan.segment_content_length(first_path), u64::try_from(first.len()).ok());
    let first_again = plan.segment_bytes(first_path).expect("same prepared segment exists");
    assert_eq!(first, first_again);
    assert_eq!(first.as_ptr(), first_again.as_ptr(), "Bytes clones share the prepared allocation");
    assert_ne!(plan.segment_bytes(second_path), Some(first));
    assert_eq!(
        plan.segment_bytes(HlsTerminalSegmentPath {
            generation: HlsTerminalTailGeneration(plan.generation.0.saturating_add(1)),
            index: 0,
        }),
        None
    );
    assert_eq!(
        plan.segment_bytes(HlsTerminalSegmentPath { generation: plan.generation, index: plan.segment_count }),
        None
    );
    assert_eq!(renderer.finite_hls_render_count(), render_count_after_plan_commit);
    assert_eq!(renderer.finite_hls_finalize_count(), finalize_count_after_plan_commit);
}

#[test]
fn hls_terminal_response_rejects_bundle_for_a_different_target_duration() {
    let asset = asset();
    let mut input = build_input(4, manifest(), Arc::clone(&asset));
    input.anchored_bundle = HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, 13_000);

    assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::AssetRevisionMismatch));
}

#[test]
fn terminal_plan_rejects_unbound_and_mixed_lease_routes() {
    let mut unbound = manifest();
    for segment in Arc::make_mut(&mut unbound.visible_segments) {
        segment.uri = format!("/live/{}.ts", segment.proxy_seq).into();
    }
    assert_eq!(
        build_terminal_tail_plan(build_input(4, unbound, asset())),
        Err(HlsTerminalTailCompatibility::InvalidLeaseRoute)
    );

    let mut mixed = manifest();
    Arc::make_mut(&mut mixed.visible_segments)[1].uri = Arc::from("/iptv/hls/shared/live/session/other-lease/194.ts");
    assert_eq!(
        build_terminal_tail_plan(build_input(4, mixed, asset())),
        Err(HlsTerminalTailCompatibility::InvalidLeaseRoute)
    );
}

#[test]
fn active_fmp4_map_rejects_mpeg_ts_splice() {
    let mut base = manifest();
    base.container = HlsMediaContainer::FragmentedMp4;
    base.active_map = Some(HlsMapSignature { fingerprint: [2; 32], container: HlsMediaContainer::FragmentedMp4 });
    let asset = asset();

    assert_eq!(
        compatibility(&base, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback
    );
}

#[test]
fn mapless_non_ts_container_rejects_mpeg_ts_splice() {
    let mut base = manifest();
    base.container = HlsMediaContainer::Unknown;
    base.active_map = None;
    let asset = asset();

    assert_eq!(
        compatibility(&base, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::ContainerMismatch
    );
}

#[test]
fn terminal_plan_keeps_only_contiguous_ready_protectable_suffix() {
    let asset = asset();
    let base = manifest();
    let mut input = build_input(1, base, Arc::clone(&asset));
    Arc::make_mut(&mut input.base_availability)[0].media_state = HlsTerminalBaseMediaState::NotReady;

    let plan = build_terminal_tail_plan(input).expect("safe suffix");

    assert_eq!(plan.base_manifest.first_proxy_seq, 194);
    assert_eq!(plan.base_manifest.last_proxy_seq, 194);
    assert_eq!(plan.protected_base_proxy_seqs.as_ref(), &[194]);
    assert_eq!(plan.base_manifest.visible_segments.len(), 1);
}

#[test]
fn unsafe_last_advertised_segment_rejects_terminal_plan() {
    let mut input = build_input(1, manifest(), asset());
    Arc::make_mut(&mut input.base_availability)[1].protection = HlsTerminalBaseProtection::Unavailable;

    assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
}

#[test]
fn unsafe_transport_evidence_cannot_commit_or_expose_any_runtime_custom_tail() {
    let unsafe_reason = super::super::super::HlsTsSpliceIncompatibility::IncompletePes {
        pid: 0x101,
        packet_index: 27,
        declared_bytes: Some(512),
        observed_bytes: 384,
    };
    for reason in [
        HlsRuntimeCustomTailReason::ChannelUnavailable,
        HlsRuntimeCustomTailReason::LowPriorityPreempted,
        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
        HlsRuntimeCustomTailReason::UserAccountExpired,
        HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
    ] {
        let asset = asset();
        let mut input = build_input(1, manifest(), Arc::clone(&asset));
        input.expected_asset =
            HlsRuntimeCustomTailAssetIdentity { reason, media: HlsTerminalAssetIdentity::from_asset(&asset) };
        input.base_splice_evidence = Some(HlsTsSpliceEvidence::Incompatible(unsafe_reason));

        assert_eq!(
            build_terminal_tail_plan(input),
            Err(HlsTerminalTailCompatibility::SpliceTransportFailure(unsafe_reason)),
            "reason {reason} must use the common exact splice gate"
        );
    }
}

#[test]
fn terminal_plan_rejects_missing_or_topologically_different_exact_evidence() {
    let asset = asset();
    let mut missing = build_input(1, manifest(), Arc::clone(&asset));
    missing.terminal_splice_evidence = None;
    assert_eq!(build_terminal_tail_plan(missing), Err(HlsTerminalTailCompatibility::MissingSpliceEvidence));

    let mut different = build_input(2, manifest(), asset);
    different.terminal_splice_evidence =
        Some(HlsTsSpliceEvidence::compatible_for_test(HlsTsTrackSignature::from_stream_types(Arc::<[u8]>::from([
            0x1B,
        ]))));
    assert_eq!(build_terminal_tail_plan(different), Err(HlsTerminalTailCompatibility::SpliceTopologyMismatch));
}

#[test]
fn terminal_splice_diagnostic_preserves_typed_boundary_fields() {
    let compatibility = HlsTerminalTailCompatibility::SpliceTransportFailure(
        super::super::super::HlsTsSpliceIncompatibility::ContinuityFailure {
            pid: 0x102,
            packet_index: 44,
            expected: 9,
            actual: 12,
        },
    );

    assert_eq!(
        HlsTerminalSpliceDiagnostic::from_compatibility(compatibility),
        Some(HlsTerminalSpliceDiagnostic {
            result: "continuity-failure",
            pid: Some(0x102),
            packet_index: Some(44),
            expected_cc: Some(9),
            actual_cc: Some(12),
            declared_pes_bytes: None,
            observed_pes_bytes: None,
        })
    );
    assert_eq!(
        HlsTerminalSpliceDiagnostic::from_compatibility(HlsTerminalTailCompatibility::SpliceTopologyMismatch)
            .map(|diagnostic| diagnostic.result),
        Some("topology-mismatch")
    );
}

#[test]
fn encrypted_base_without_ready_key_rejects_terminal_plan() {
    let mut base = manifest();
    let encryption = resettable_aes128_encryption();
    base.active_encryption = Some(encryption.clone());
    Arc::make_mut(&mut base.visible_segments)[1].encryption = Some(encryption);
    let mut input = build_input(1, base, asset());
    Arc::make_mut(&mut input.base_availability)[1].required_key_ready = false;

    assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::MissingSafeBase));
}

#[test]
fn unknown_or_different_track_layout_rejects_splice() {
    let asset = asset();
    assert_eq!(compatibility(&manifest(), &asset, None), HlsTerminalTailCompatibility::MissingTrackSignature);
    let different = HlsTsTrackSignature::from_stream_types(Arc::<[u8]>::from([0x1B]));
    assert_eq!(compatibility(&manifest(), &asset, Some(&different)), HlsTerminalTailCompatibility::TrackLayoutMismatch);
}
