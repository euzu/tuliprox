use super::*;

#[test]
fn method_none_boundary_keeps_only_homogeneous_clear_suffix() {
    let mut base = manifest();
    Arc::make_mut(&mut base.visible_segments)[0].encryption = Some(resettable_aes128_encryption());

    let plan = build_terminal_tail_plan(build_input(2, base, asset())).expect("clear suffix");
    let rendered =
        terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
            .expect("render");

    assert_eq!(plan.protected_base_proxy_seqs.as_ref(), &[194]);
    assert!(!rendered.contains("/iptv/hls/shared/live/session/lease/193.ts"));
    assert!(!rendered.contains("#EXT-X-KEY:METHOD=AES-128"));
    assert!(!rendered.contains("#EXT-X-KEY:METHOD=NONE"));
}

#[test]
fn asset_rounded_duration_must_not_exceed_target_duration() {
    let asset = asset();
    let mut short_target = manifest();
    short_target.target_duration_ms = 9_000;

    assert_eq!(
        compatibility(&short_target, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::TargetDurationExceeded { asset_ms: asset.duration_ms(), target_ms: 9_000 }
    );
}

#[test]
fn unsupported_key_format_cannot_reset_to_clear_asset() {
    let mut base = manifest();
    base.active_encryption = Some(Arc::new(HlsEncryptionSignature {
        method: "SAMPLE-AES".into(),
        key_uri: Some("/keys/drm.key".into()),
        iv: None,
        key_format: Some("com.example.drm".into()),
        key_format_versions: None,
        can_reset_to_clear: true,
    }));
    let asset = asset();

    assert_eq!(
        compatibility(&base, &asset, Some(asset.track_signature())),
        HlsTerminalTailCompatibility::UnsupportedEncryptionTransition
    );
}
