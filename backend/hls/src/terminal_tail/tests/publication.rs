use super::*;

#[test]
fn hls_terminal_response_manifest_keeps_live_tail_and_appends_finite_endlist_tail() {
    let asset = asset();
    let plan = build_terminal_tail_plan(build_input(4, manifest(), Arc::clone(&asset))).expect("compatible tail");

    let first =
        terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
            .expect("render tail");
    let second =
        terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
            .expect("render tail again");

    assert_eq!(first, second);
    assert_eq!(first.matches("#EXTM3U").count(), 1);
    assert!(first.contains("/iptv/hls/shared/live/session/lease/193.ts"));
    assert_eq!(
        first.matches("/iptv/hls/shared/live/session/lease/terminal/4/").count(),
        usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
    );
    assert_eq!(first.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(first.ends_with("#EXT-X-ENDLIST\n"));
    assert!(first.contains("#EXT-X-TARGETDURATION:12\n"));
    assert_eq!(plan.base_manifest.target_duration_ms, 12_000);
    assert_eq!(plan.segment_duration_ms, asset.duration_ms());
    let terminal_media = first
        .rsplit_once("#EXT-X-DISCONTINUITY\n")
        .map(|(_, terminal_media)| terminal_media)
        .expect("terminal discontinuity");
    assert_eq!(
        terminal_media.matches(&format!("#EXTINF:{},", format_hls_duration_ms(asset.duration_ms()))).count(),
        usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT)
    );
    assert_eq!(
        terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("other-lease".into()),),
        Err(HlsTerminalTailRenderError::RouteBindingMismatch)
    );
}

#[test]
fn hls_terminal_response_path_parser_rejects_noncanonical_generation_and_file() {
    assert_eq!(
        HlsTerminalSegmentPath::parse("17", "2.ts"),
        Some(HlsTerminalSegmentPath { generation: HlsTerminalTailGeneration(17), index: 2 })
    );
    for (generation, terminal_file) in [
        ("", "0.ts"),
        ("01", "0.ts"),
        ("+1", "0.ts"),
        ("one", "0.ts"),
        ("1", ""),
        ("1", "00.ts"),
        ("1", "+1.ts"),
        ("1", "1.m4s"),
        ("1", "65536.ts"),
    ] {
        assert_eq!(HlsTerminalSegmentPath::parse(generation, terminal_file), None);
    }
}

#[test]
fn transient_origin_backed_manifest_is_explicitly_incompatible() {
    let asset = asset();
    let mut base = manifest();
    base.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;

    assert_eq!(
        compatibility(&base, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::TransientPassthroughUnsupported
    );
}

#[test]
fn unsafe_key_uri_or_iv_cannot_be_rendered_into_terminal_manifest() {
    let asset = asset();
    let mut unsafe_uri = manifest();
    let mut encryption = resettable_aes128_encryption();
    Arc::make_mut(&mut encryption).key_uri = Some("/key\"\n#EXT-X-ENDLIST".into());
    unsafe_uri.active_encryption = Some(encryption);
    assert_eq!(
        compatibility(&unsafe_uri, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
    );

    let mut unsafe_iv = manifest();
    let mut encryption = resettable_aes128_encryption();
    Arc::make_mut(&mut encryption).iv = Some("not-hex".into());
    unsafe_iv.active_encryption = Some(encryption);
    assert_eq!(
        compatibility(&unsafe_iv, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
    );
}

#[test]
fn stale_asset_revision_cannot_build_terminal_plan() {
    let asset = asset();
    let mut input = build_input(1, manifest(), asset);
    input.expected_asset.media.revision = input.expected_asset.media.revision.saturating_add(1);

    assert_eq!(build_terminal_tail_plan(input), Err(HlsTerminalTailCompatibility::AssetRevisionMismatch));
}
