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

#[test]
fn account_handoff_retains_cached_transient_bytes_but_blocks_old_origin_mapping() {
    let mut state = TransientPassthroughState::default();
    let proxy = ProxySessionId("account-handoff".to_string());
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://origin.example/old/media.ts",
        b"secret",
        1,
        100,
        Some("ts".to_string()),
    );
    let id = resource.id.clone();
    state.upsert_resources([resource]);
    let resource = state.resolve_current_resource(&id, 2).unwrap();
    let token = match state.begin_object_fetch(&proxy, &resource, "ts", 2, 100) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("uncached media starts one fetch")
        }
    };
    assert!(state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 3, 100));
    state.invalidate_origin_resources();
    let key = TransientPassthroughState::transient_object_key(&proxy, &id, "ts");
    assert!(state.ready_object(&key, TransientResourceKind::Segment, 4).is_some());
    assert!(state.resolve_current_resource(&id, 4).is_some());
    assert!(!state.origin_resource_is_fetchable(&resource, 4));
    state.upsert_resources([resource.clone()]);
    assert!(!state.origin_resource_is_fetchable(&resource, 5));
    assert!(state.ready_object(&key, TransientResourceKind::Segment, 5).is_some());
    state.invalidate_origin_resources();
    state.prune_expired(102);
    assert!(state.invalid_origin_resources.is_empty());
}

#[test]
fn account_handoff_gives_identical_key_uri_a_new_cache_identity_and_retains_the_old_key() {
    let mut state = TransientPassthroughState::default();
    let proxy = ProxySessionId("key-handoff".to_string());
    let body = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"shared.key\"\n#EXTINF:4,\nmedia.ts\n";
    let old = TransientManifestRewriter::rewrite(body, "https://origin.example/index.m3u8", &proxy, b"secret", 1, 100);
    let old_key = old.resources.iter().find(|resource| resource.kind == TransientResourceKind::Key).unwrap().id.clone();
    state.upsert_resources(old.resources);
    let old_key_ref = state.resolve_current_resource(&old_key, 2).unwrap();
    let super::super::TransientObjectFetchDecision::Fetch(token) =
        state.begin_object_fetch(&proxy, &old_key_ref, "key", 2, 100)
    else {
        panic!("old key starts its first cache fill")
    };
    assert!(state.mark_object_ready_if_current(&token, "application/octet-stream".to_string(), 16, 3, 100));
    state.invalidate_origin_resources();
    let new = TransientManifestRewriter::rewrite_with_options(
        body,
        "https://origin.example/index.m3u8",
        &proxy,
        b"secret",
        4,
        100,
        crate::transient_manifest::TransientRewriteOptions {
            origin_account_generation: state.origin_account_generation(),
            ..crate::transient_manifest::TransientRewriteOptions::default()
        },
    );
    let new_key = new.resources.iter().find(|resource| resource.kind == TransientResourceKind::Key).unwrap().id.clone();
    assert_ne!(old_key, new_key);
    assert_ne!(old.body, new.body);
    state.upsert_resources(new.resources);
    let new_key_ref = state.resolve_current_resource(&new_key, 5).unwrap();
    assert_eq!(old_key_ref.resolved_origin_uri, new_key_ref.resolved_origin_uri);
    let old_lookup = TransientPassthroughState::transient_object_key(&proxy, &old_key, "key");
    let new_lookup = TransientPassthroughState::transient_object_key(&proxy, &new_key, "key");
    assert!(state.ready_object(&old_lookup, TransientResourceKind::Key, 5).is_some());
    assert!(state.ready_object(&new_lookup, TransientResourceKind::Key, 5).is_none());
    assert!(!state.origin_resource_is_fetchable(&old_key_ref, 5));
    assert!(state.origin_resource_is_fetchable(&new_key_ref, 5));
    assert!(matches!(
        state.begin_object_fetch(&proxy, &new_key_ref, "key", 5, 100),
        super::super::TransientObjectFetchDecision::Fetch(_)
    ));
}
