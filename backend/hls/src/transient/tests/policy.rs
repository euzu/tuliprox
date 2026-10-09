use super::*;

#[test]
fn rolling_event_append_requires_current_rewrite_secret_and_refreshes_existing_mapping_validity() {
    let mut state = TransientPassthroughState::default();
    let first_origin = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                            #EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:6,\n0.ts\n#EXTINF:6,\n1.ts\n";
    let first = TransientManifestRewriter::rewrite(
        first_origin,
        "https://origin.example/event/index.m3u8",
        &ProxySessionId("proxy-session".to_string()),
        b"secret",
        0,
        300_000,
    );
    let checkpoint = first.checkpoint.clone();
    let first_resource_id = first.resources[0].id.clone();
    let semantics = parse_manifest_semantics(first_origin);
    let footprint = state
        .commit_rewritten_manifest_with_semantics(first.body, first.resources, 0, Some(12_000), semantics)
        .expect("initial EVENT commits");
    state.record_rolling_event_rewrite_seed(
        first_origin,
        "https://origin.example/event/index.m3u8",
        b"secret",
        checkpoint,
        footprint.estimated_metadata_bytes,
        true,
    );
    let previous_template = state.last_manifest_template().expect("initial template");
    let second_origin = format!("{first_origin}#EXTINF:6,\n2.ts\n");
    assert!(state
        .rolling_event_append_context(&second_origin, "https://origin.example/event/index.m3u8", b"rotated-secret",)
        .is_none());
    let context = state
        .rolling_event_append_context(&second_origin, "https://origin.example/event/index.m3u8", b"secret")
        .expect("safe append is detected");
    let mut appended = TransientManifestRewriter::rewrite_append(
        &second_origin[context.suffix_offset..],
        "https://origin.example/event/index.m3u8",
        &ProxySessionId("proxy-session".to_string()),
        b"secret",
        400_000,
        300_000,
        context.checkpoint,
    );
    let template = TransientPassthroughState::extend_rolling_event_template(&context.template, &appended.body)
        .expect("append remains within limits")
        .expect("append contains a complete media unit");
    let mut body = String::with_capacity(context.rewritten_prefix.len().saturating_add(appended.body.len()));
    body.push_str(&context.rewritten_prefix);
    body.push_str(&appended.body);
    appended.body = body;

    state
        .commit_incremental_rewritten_manifest_with_semantics(
            appended.body,
            appended.resources,
            Arc::clone(&template),
            400_000,
            300_000,
            parse_manifest_semantics(&second_origin),
        )
        .expect("append commits");

    assert_eq!(template.segments().len(), 3);
    assert!(Arc::ptr_eq(&previous_template.segments()[0].uri, &template.segments()[0].uri));
    assert_eq!(state.resources[&first_resource_id].expires_at_ms, 700_000);
    assert_eq!(state.current_manifest_resource_ids().len(), 3);
}

#[test]
fn finalized_key_readiness_uses_object_ttl_after_mapping_ttl() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/archive/key.key",
        b"secret",
        0,
        20,
        Some("key".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource.clone()]);
    replace_with_finalized_manifest(&mut state, &resource_id, "key", 0);
    let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 10, 100) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("new finalized AES key starts a cache fetch")
        }
    };
    assert!(state.mark_object_ready_if_current(&token, "application/octet-stream".to_string(), 16, 15, 110));

    assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 30), Some(110));
    assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 111), None);
}

#[test]
fn failed_transient_metadata_has_finite_ttl_and_a_hard_count_bound() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::new(100);
    let mut oldest_lookup_key = None;
    for index in 0..=MAX_FAILED_TRANSIENT_OBJECT_ENTRIES {
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            format!("http://origin.example.com/live/{index}.ts"),
            b"secret",
            0,
            10_000,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        let resource = state.resources.get(&resource_id).expect("registered resource").clone();
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", index as u64, 10_000) {
            super::super::TransientObjectFetchDecision::Fetch(token) => token,
            super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
                panic!("unique resource starts a cache fill")
            }
        };
        if index == 0 {
            oldest_lookup_key = Some(token.lookup_key.clone());
        }
        assert!(state.mark_object_failed_permanent_if_current(&token, index as u64, None));
    }

    assert_eq!(
        state
            .object_cache
            .values()
            .filter(|entry| matches!(entry.status, TransientObjectCacheStatus::FailedPermanent { .. }))
            .count(),
        MAX_FAILED_TRANSIENT_OBJECT_ENTRIES
    );
    assert!(!state.object_cache.contains_key(&oldest_lookup_key.expect("oldest lookup key")));

    let mut state = TransientPassthroughState::new(100);
    let expiring = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/expiring.ts",
        b"secret",
        1_000,
        10_000,
        Some("ts".to_string()),
    );
    let expiring_id = expiring.id.clone();
    state.upsert_resources([expiring]);
    let expiring = state.resources.get(&expiring_id).expect("registered expiring resource").clone();
    let token = match state.begin_object_fetch(&proxy_session_id, &expiring, "ts", 1_010, 10_000) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("expiring resource starts a cache fill")
        }
    };
    assert!(state.mark_object_failed_retryable_if_current(&token, 1_020, 10));
    assert_eq!(state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms), Some(1_120));
    state.upsert_resources([TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/expiring.ts",
        b"secret",
        1_100,
        20_000,
        Some("ts".to_string()),
    )]);
    assert_eq!(
        state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms),
        Some(1_120),
        "manifest refresh must not make failed metadata immortal"
    );
    let removals = state.take_expired_object_removals_except(1_121, &HashSet::from([expiring_id]), 1);
    assert_eq!(removals.len(), 1);
    assert_eq!(&removals[0].key, token.cache_key());
    assert!(!state.object_cache.contains_key(&token.lookup_key));
}
