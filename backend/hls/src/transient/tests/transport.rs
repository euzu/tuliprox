use super::*;

#[test]
fn extract_transient_resource_ids_rejects_noncanonical_resource_files() {
    let body = concat!(
        "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.exe\n",
        "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.ts/extra\n",
        "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.tar.gz\n",
    );

    assert!(extract_transient_resource_ids(body).is_empty());
}

#[test]
fn estimated_transient_metadata_limit_counts_owned_hint_bytes() {
    let mut state = TransientPassthroughState::default();
    let mut retained = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://origin.example/retained.ts".to_string(),
        b"secret",
        0,
        300_000,
        Some("ts".to_string()),
    );
    retained.content_type_hint = Some("x".repeat(MAX_ESTIMATED_TRANSIENT_METADATA_BYTES));
    state.upsert_resources([retained]);
    let candidate = rewritten_finalized_manifest(1, "candidate");
    let semantics = parse_manifest_semantics(&candidate.body);

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(candidate.body, candidate.resources, 1, Some(6_000), semantics)
            .expect_err("estimated metadata overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientEstimatedMetadataBytes);
    assert!(violation.actual > MAX_ESTIMATED_TRANSIENT_METADATA_BYTES);
}

#[test]
fn aes_key_readiness_requires_exactly_sixteen_bytes_and_honors_the_inclusive_ttl_boundary() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    for invalid_size in [15, 17] {
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            format!("http://origin.example.com/live/key-{invalid_size}.key"),
            b"secret",
            10,
            100,
            Some("key".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource.clone()]);
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
            super::super::TransientObjectFetchDecision::Fetch(token) => token,
            super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
                panic!("new AES key starts a cache fetch")
            }
        };
        assert!(state.mark_object_ready_if_current(
            &token,
            "application/octet-stream".to_string(),
            invalid_size,
            30,
            110,
        ));
        assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 30), None);
    }

    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/valid.key",
        b"secret",
        10,
        100,
        Some("key".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource.clone()]);
    let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("new AES key starts a cache fetch")
        }
    };
    assert!(state.mark_object_ready_if_current(&token, "application/octet-stream".to_string(), 16, 30, 110,));

    assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 110), Some(110));
    assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 111), None);
}

#[tokio::test]
async fn controlled_resource_mapping_replacement_invalidates_the_fetch_token_and_wakes_its_waiter() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let original = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin-a.example.com/live/segment.ts",
        b"secret",
        10,
        100,
        Some("ts".to_string()),
    );
    let resource_id = original.id.clone();
    state.upsert_resources([original]);
    let original = state.resources.get(&resource_id).expect("registered original mapping").clone();
    let token = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("new resource starts a cache fetch")
        }
    };
    let notifier = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 21, 50) {
        super::super::TransientObjectFetchDecision::Wait(notifier) => notifier,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Fetch(_) => {
            panic!("concurrent request waits for the owned fill")
        }
    };
    let waiter = notifier.notified();
    tokio::pin!(waiter);
    assert!(matches!(futures::poll!(&mut waiter), Poll::Pending));

    let mut replacement = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin-b.example.com/live/segment.ts",
        b"secret",
        22,
        100,
        Some("ts".to_string()),
    );
    replacement.id = resource_id;
    state.upsert_resources([replacement]);

    assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 23, 122));
    assert!(matches!(futures::poll!(&mut waiter), Poll::Ready(())));
    assert!(!state.object_cache.contains_key(&token.lookup_key));
    let replacement = state.resources.get(&token.binding.resource_id).expect("replacement mapping").clone();
    let replacement_token = match state.begin_object_fetch(&proxy_session_id, &replacement, "ts", 24, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("replacement mapping owns a new cache fill")
        }
    };
    assert_ne!(token.cache_key(), replacement_token.cache_key());
    assert_ne!(token.binding.revision, replacement_token.binding.revision);
}

#[test]
fn expired_resource_validity_rejects_a_late_fetch_commit() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/short.ts",
        b"secret",
        10,
        10,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource]);
    state.replace_manifest_with_semantics(
        format!("#EXTM3U\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.ts\n", resource_id.0),
        10,
        None,
    );
    let resource = state.resources.get(&resource_id).expect("registered resource").clone();
    let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 15, 100) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("valid resource starts a cache fill")
        }
    };

    assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 115));
    assert!(!state.object_cache.contains_key(&token.lookup_key));
}

#[test]
fn finalized_resource_replacement_still_rejects_stale_fetch_commit() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let original = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/archive/original.ts",
        b"secret",
        10,
        10,
        Some("ts".to_string()),
    );
    let resource_id = original.id.clone();
    state.upsert_resources([original]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
    let original = state.resolve_current_resource(&resource_id, 15).expect("original mapping");
    let token = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 15, 100) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("original finalized resource starts a cache fill")
        }
    };
    let mut replacement = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://replacement.example.com/archive/replacement.ts",
        b"secret",
        16,
        100,
        Some("ts".to_string()),
    );
    replacement.id = resource_id.clone();
    state.upsert_resources([replacement]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 16);

    assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 121));
    assert!(!state.object_cache.contains_key(&token.lookup_key));
}
