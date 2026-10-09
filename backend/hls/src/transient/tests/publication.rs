use super::*;

#[test]
fn finalized_committed_manifest_has_full_window_and_no_deadline() {
    let mut state = TransientPassthroughState::default();

    state.replace_manifest_with_semantics("#EXTM3U\n#EXT-X-ENDLIST\n".to_string(), 100, Some(1_000));

    assert!(state.last_manifest_finalized());
    assert_eq!(state.last_manifest_window_policy(), HlsManifestWindowPolicy::PreserveFullManifest);
    assert_eq!(state.last_manifest_valid_until_ms(), None);
}

#[test]
fn rolling_typed_committed_manifest_preserves_full_window_with_deadline() {
    let mut state = TransientPassthroughState::default();

    state.replace_manifest_with_semantics(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:1,\n/r/resource.ts\n".to_string(),
        100,
        Some(1_000),
    );

    assert!(!state.last_manifest_finalized());
    assert_eq!(state.last_manifest_window_policy(), HlsManifestWindowPolicy::PreserveFullManifest);
    assert_eq!(state.last_manifest_valid_until_ms(), Some(1_100));
}

#[test]
fn transient_manifest_resource_limit_rejects_without_mutating_current_manifest() {
    let mut state = TransientPassthroughState::default();
    let baseline = rewritten_finalized_manifest(1, "baseline");
    let baseline_semantics = parse_manifest_semantics(&baseline.body);
    state
        .commit_rewritten_manifest_with_semantics(baseline.body, baseline.resources, 1, Some(6_000), baseline_semantics)
        .expect("baseline manifest commits");
    let baseline_body = state.last_manifest_body.clone().expect("baseline body");
    let baseline_generation = state.manifest_generation();
    let baseline_resource_count = state.resources.len();
    let oversized = rewritten_finalized_manifest(MAX_TRANSIENT_MANIFEST_RESOURCES + 1, "oversized");
    let oversized_semantics = parse_manifest_semantics(&oversized.body);

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(oversized.body, oversized.resources, 2, None, oversized_semantics)
            .expect_err("resource overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientResources);
    assert_eq!(violation.actual, MAX_TRANSIENT_MANIFEST_RESOURCES + 1);
    assert_eq!(state.manifest_generation(), baseline_generation);
    assert_eq!(state.resources.len(), baseline_resource_count);
    assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("current body remains"), &baseline_body));
}

#[test]
fn retained_finalized_generation_limit_rejects_the_next_generation() {
    let mut state = TransientPassthroughState::default();
    for index in 0..MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS {
        let rewritten = rewritten_finalized_manifest(1, &format!("generation-{index}"));
        let semantics = parse_manifest_semantics(&rewritten.body);
        state
            .commit_rewritten_manifest_with_semantics(
                rewritten.body,
                rewritten.resources,
                u64::try_from(index).expect("test index fits u64"),
                Some(6_000),
                semantics,
            )
            .expect("generation within retention limit commits");
        let generation = state.current_finalized_manifest_generation().expect("finalized generation");
        assert!(state.bind_finalized_manifest_generation(TransientManifestLeaseBinding::new(
            HlsAccessLeaseId(format!("lease-{index}")),
            u64::try_from(index).expect("test index fits u64"),
            generation,
        )));
    }
    let retained_body = state.last_manifest_body.clone().expect("retained current body");
    let retained_generation = state.manifest_generation();
    let overflow = rewritten_finalized_manifest(1, "generation-overflow");
    let semantics = parse_manifest_semantics(&overflow.body);

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(overflow.body, overflow.resources, 100, Some(6_000), semantics)
            .expect_err("additional retained generation is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::FinalizedGenerations);
    assert_eq!(violation.actual, MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS + 1);
    assert_eq!(state.manifest_generation(), retained_generation);
    assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("current body remains"), &retained_body));
}

#[test]
fn byte_identical_finalized_refresh_reuses_generation_and_body_allocation() {
    let mut state = TransientPassthroughState::default();
    let first = rewritten_finalized_manifest(3, "stable");
    let semantics = parse_manifest_semantics(&first.body);
    state
        .commit_rewritten_manifest_with_semantics(first.body, first.resources, 0, Some(18_000), semantics)
        .expect("first finalized manifest commits");
    let generation = state.current_finalized_manifest_generation().expect("finalized generation");
    let generation_highwater = state.manifest_generation();
    let body = state.last_manifest_body.clone().expect("stored body");

    let refreshed = TransientManifestRewriter::rewrite(
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:6,\nstable-0.ts\n#EXTINF:6,\nstable-1.ts\n#EXTINF:6,\nstable-2.ts\n#EXT-X-ENDLIST\n",
        "https://origin.example/archive/index.m3u8",
        &ProxySessionId("proxy-session".to_string()),
        b"secret",
        400_000,
        300_000,
    );
    let refreshed_resource_id = refreshed.resources[0].id.clone();
    state
        .commit_rewritten_manifest_with_semantics(refreshed.body, refreshed.resources, 400_000, Some(18_000), semantics)
        .expect("identical finalized refresh commits");

    assert_eq!(state.current_finalized_manifest_generation(), Some(generation));
    assert_eq!(state.manifest_generation(), generation_highwater);
    assert_eq!(state.finalized_manifest_generation_count(), 1);
    assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("same body allocation"), &body));
    assert_eq!(state.resources[&refreshed_resource_id].expires_at_ms, 700_000);
}

#[test]
fn rolling_lease_membership_retains_only_still_valid_previously_published_resources() {
    let mut state = TransientPassthroughState::default();
    let previous_resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://origin.example/previous.ts",
        b"secret",
        0,
        20,
        Some("ts".to_string()),
    );
    let next_resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://origin.example/next.ts",
        b"secret",
        0,
        40,
        Some("ts".to_string()),
    );
    let previous_resource_id = previous_resource.id.clone();
    let next_resource_id = next_resource.id.clone();
    state.upsert_resources([previous_resource, next_resource]);
    let previous = HlsPublishedTransientResourceIds::from_manifest_body(&format!(
        "/hls/shared/live/proxy/lease/r/{}.ts",
        previous_resource_id.0
    ));
    let next = HlsPublishedTransientResourceIds::from_manifest_body(&format!(
        "/hls/shared/live/proxy/lease/r/{}.ts",
        next_resource_id.0
    ));

    let retained = state.merge_current_published_resource_ids(&previous, next.clone(), 10);
    assert!(retained.contains(&previous_resource_id));
    assert!(retained.contains(&next_resource_id));

    let pruned = state.merge_current_published_resource_ids(&retained, next, 21);
    assert!(!pruned.contains(&previous_resource_id));
    assert!(pruned.contains(&next_resource_id));
}

#[test]
fn finalized_generation_membership_limit_counts_overlapping_sets() {
    let mut state = TransientPassthroughState::default();
    let base = rewritten_finalized_manifest(MAX_TRANSIENT_MANIFEST_RESOURCES, "shared");
    let mut body = base.body;
    let resources = base.resources;
    let generations_within_limit = MAX_TRANSIENT_GENERATION_MEMBERSHIPS / MAX_TRANSIENT_MANIFEST_RESOURCES;
    for index in 0..generations_within_limit {
        let _ = writeln!(body, "# generation-{index}");
        state
            .commit_rewritten_manifest_with_semantics(
                body.clone(),
                if index == 0 { resources.clone() } else { Vec::new() },
                u64::try_from(index).expect("index fits u64"),
                None,
                parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
            )
            .expect("membership count remains within limit");
        let generation = state.current_finalized_manifest_generation().expect("finalized generation");
        assert!(state.bind_finalized_manifest_generation(TransientManifestLeaseBinding::new(
            HlsAccessLeaseId(format!("lease-{index}")),
            u64::try_from(index).expect("index fits u64"),
            generation,
        )));
    }
    body.push_str("# generation-overflow\n");

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(
                body,
                Vec::new(),
                100,
                None,
                parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
            )
            .expect_err("membership overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientGenerationMemberships);
    assert_eq!(violation.actual, MAX_TRANSIENT_GENERATION_MEMBERSHIPS + MAX_TRANSIENT_MANIFEST_RESOURCES);
}

#[test]
fn manifest_refresh_does_not_extend_transient_ready_object_ttl() {
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/seg.ts",
        b"secret",
        10,
        100,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    let key = TransientPassthroughState::transient_object_key(
        &ProxySessionId("proxy-session".to_string()),
        &resource_id,
        "ts",
    );
    state.upsert_resources([resource.clone()]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
    let token = match state.begin_object_fetch(&ProxySessionId("proxy-session".to_string()), &resource, "ts", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("new resource starts a cache fetch")
        }
    };
    assert!(state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 30, 110));
    assert_eq!(state.object_cache.get(&key).expect("object").expires_at_ms, 110);

    let updated = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/seg.ts",
        b"secret",
        100,
        300,
        Some("ts".to_string()),
    );
    state.upsert_resources([updated]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 100);

    let object = state.object_cache.get(&key).expect("object remains");
    assert!(matches!(object.status, TransientObjectCacheStatus::Ready { .. }));
    assert_eq!(object.expires_at_ms, 110);
}

#[test]
fn manifest_refresh_does_not_extend_a_ready_key_revision_and_rotation_reports_the_displaced_object() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let first_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/key.key",
        b"secret",
        10,
        100,
        Some("key".to_string()),
    );
    let resource_id = first_resource.id.clone();
    state.upsert_resources([first_resource.clone()]);
    let first = match state.begin_object_fetch(&proxy_session_id, &first_resource, "key", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("first key revision starts a fetch")
        }
    };
    assert!(state.mark_object_ready_if_current(&first, "application/octet-stream".to_string(), 16, 30, 110,));

    let refreshed_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/key.key",
        b"secret",
        100,
        100,
        Some("key".to_string()),
    );
    assert_eq!(refreshed_resource.id, resource_id);
    state.upsert_resources([refreshed_resource.clone()]);
    let lookup_key =
        TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "key".to_string());
    assert_eq!(state.object_cache.get(&lookup_key).map(|entry| entry.expires_at_ms), Some(110));

    let second = match state.begin_object_fetch(&proxy_session_id, &refreshed_resource, "key", 111, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("expired key revision starts a replacement fetch")
        }
    };
    let displaced = second.superseded_object().expect("replacement owns one bounded displaced object");
    assert_eq!(&displaced.key, first.cache_key());
    assert_eq!(displaced.content_length, 16);
    assert_ne!(second.cache_key(), first.cache_key());
}

#[test]
fn stale_transient_fetch_token_cannot_overwrite_or_fail_a_new_generation() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/key.key",
        b"secret",
        10,
        100,
        Some("key".to_string()),
    );
    state.upsert_resources([resource.clone()]);
    let first = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("first key fetch starts")
        }
    };
    assert!(state.mark_object_failed_retryable_if_current(&first, 21, 0));
    let second = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 22, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("replacement key fetch starts")
        }
    };
    assert_ne!(first.cache_key(), second.cache_key());

    assert!(!state.mark_object_ready_if_current(&first, "application/octet-stream".to_string(), 16, 23, 110,));
    assert!(state.mark_object_ready_if_current(&second, "application/octet-stream".to_string(), 16, 24, 110,));
    assert!(!state.mark_object_failed_retryable_if_current(&first, 25, 0));
    assert!(matches!(
        state.object_cache.get(&second.lookup_key).map(|entry| &entry.status),
        Some(TransientObjectCacheStatus::Ready { content_length: 16, .. })
    ));
}

#[test]
fn finalized_manifest_resource_accepts_late_fetch_commit() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/archive/short.ts",
        b"secret",
        10,
        10,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
    let resource = state.resolve_current_resource(&resource_id, 15).expect("finalized resource resolves");
    let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 15, 100) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("valid finalized resource starts a cache fill")
        }
    };

    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 18);
    assert_eq!(state.manifest_generation(), 1);
    assert!(state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 121));
    assert!(state.object_cache.contains_key(&token.lookup_key));
}

#[test]
fn finalized_prune_keeps_resources_referenced_by_current_manifest() {
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/seg.ts",
        b"secret",
        0,
        10,
        Some("ts".to_string()),
    );
    let resource_id = resource.id.clone();
    state.upsert_resources([resource]);
    replace_with_finalized_manifest(&mut state, &resource_id, "ts", 0);

    state.prune_expired(20);

    assert!(state.resources.contains_key(&resource_id));
}
