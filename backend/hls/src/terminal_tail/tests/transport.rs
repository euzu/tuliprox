use super::*;

#[test]
fn encrypted_base_announces_clear_key_transition() {
    let mut base = manifest();
    let encryption = resettable_aes128_encryption();
    base.active_encryption = Some(encryption.clone());
    Arc::make_mut(&mut base.visible_segments)[1].encryption = Some(encryption);
    let mut input = build_input(1, base, asset());
    input.base_key_bindings = Arc::from([aes128_key_binding()]);
    let plan = build_terminal_tail_plan(input).expect("clear transition");
    let resource_file = TransientResourceFile::parse("aes-key.key").expect("terminal key route");
    let binding = plan
        .terminal_key_binding(&ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()), &resource_file)
        .expect("exact lease route resolves frozen key binding");
    assert_eq!(binding.bytes().as_ref(), b"0123456789abcdef");
    assert!(plan
        .terminal_key_binding(
            &ProxySessionId("session".into()),
            &HlsAccessLeaseId("other-lease".into()),
            &resource_file,
        )
        .is_none());
    assert!(plan
        .terminal_key_binding(
            &ProxySessionId("session".into()),
            &HlsAccessLeaseId("lease".into()),
            &TransientResourceFile::parse("aes-key.bin").expect("different extension route"),
        )
        .is_none());
    let rendered =
        terminal_tail_manifest_body(&plan, &ProxySessionId("session".into()), &HlsAccessLeaseId("lease".into()))
            .expect("render");

    assert!(
        !rendered.contains("/iptv/hls/shared/live/session/lease/193.ts"),
        "only the segment covered by the active key is safe"
    );
    let key = rendered.find("#EXT-X-KEY:METHOD=AES-128").expect("base key tag");
    let base_segment = rendered.find("/iptv/hls/shared/live/session/lease/194.ts").expect("encrypted base segment");
    let reset = rendered.find("#EXT-X-KEY:METHOD=NONE").expect("clear reset");
    assert!(key < base_segment && base_segment < reset);
    assert!(rendered
        .contains("URI=\"/iptv/hls/shared/live/session/lease/r/aes-key.key\",IV=0x000000000000000000000000000000C2"));
    assert!(rendered.contains("#EXT-X-VERSION:5\n"));
    assert!(rendered.contains("#EXT-X-KEY:METHOD=NONE\n#EXT-X-DISCONTINUITY\n"));
}
