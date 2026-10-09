use super::*;

#[test]
fn transient_session_origin_uri_byte_limit_is_enforced() {
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "x".repeat(MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION + 1),
        b"secret",
        0,
        300_000,
        Some("ts".to_string()),
    );

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(
                finalized_manifest_body(&resource.id, "ts"),
                vec![resource],
                1,
                Some(1_000),
                parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
            )
            .expect_err("origin URI byte overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientOriginUriBytes);
    assert!(state.resources.is_empty());
}

#[test]
fn expired_finalized_object_is_refetched_instead_of_served_stale() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/archive/refetch.ts",
        b"secret",
        0,
        10,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 0);
    let resource = state.resolve_current_resource(&resource_id, 5).expect("finalized mapping");
    let first = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 5, 5) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("first cache fill starts")
        }
    };
    assert!(state.mark_object_ready_if_current(
        &first,
        "video/mp2t".to_string(),
        7,
        6,
        transient_object_expires_at(6, 5),
    ));
    let lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");

    assert!(state.ready_object(&lookup_key, TransientResourceKind::Segment, 11).is_some());
    assert!(state.ready_object(&lookup_key, TransientResourceKind::Segment, 12).is_none());
    let resource = state.resolve_current_resource(&resource_id, 12).expect("mapping outlives cached bytes");
    assert!(matches!(
        state.begin_object_fetch(&proxy_session_id, &resource, "ts", 12, 5),
        super::super::TransientObjectFetchDecision::Fetch(_)
    ));
}
