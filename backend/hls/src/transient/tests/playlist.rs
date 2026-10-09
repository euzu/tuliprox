use super::*;

#[test]
fn extract_transient_resource_ids_ignores_comment_and_foreign_uri_surfaces() {
    let body = concat!(
        "# /r/fake.ts\n",
        "https://origin.example.com/r/fake.ts\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"https://origin.example.com/r/fake.key\"\n",
    );

    assert!(extract_transient_resource_ids(body).is_empty());
}

#[test]
fn extract_transient_resource_ids_ignores_query_values() {
    let body = concat!(
        "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.key?token=/r/fake.ts\"\n",
        "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.ts?redirect=/r/foreign.ts\n",
    );

    let resource_ids = extract_transient_resource_ids(body);

    assert_eq!(resource_ids.len(), 1);
    assert!(resource_ids.contains(&TransientResourceId("AbCdEfGhIjKlMnOp".to_string())));
}

#[test]
fn extract_transient_resource_ids_keeps_segment_key_and_map_routes() {
    let body = concat!(
        "#EXTM3U\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy/__hls_access_lease_id__/r/Key0000000000Id.key\"\n",
        "#EXT-X-MAP:URI=\"/hls/shared/live/proxy/__hls_access_lease_id__/r/Map0000000000Id.mp4\"\n",
        "#EXTINF:1.0,\n",
        "/hls/shared/live/proxy/lease/r/Seg0000000000Id.ts\n",
    );

    let resource_ids = extract_transient_resource_ids(body);

    assert_eq!(resource_ids.len(), 3);
    assert!(resource_ids.contains(&TransientResourceId("Key0000000000Id".to_string())));
    assert!(resource_ids.contains(&TransientResourceId("Map0000000000Id".to_string())));
    assert!(resource_ids.contains(&TransientResourceId("Seg0000000000Id".to_string())));
}

#[test]
fn transient_session_resource_entry_limit_is_enforced() {
    let mut state = TransientPassthroughState::default();
    for index in 0..MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION {
        state.upsert_resources([TransientResourceRef::new(
            TransientResourceKind::Segment,
            format!("https://origin.example/{index}.ts"),
            b"secret",
            0,
            300_000,
            Some("ts".to_string()),
        )]);
    }
    let incoming = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://other-origin.example/overflow.ts".to_string(),
        b"secret",
        0,
        300_000,
        Some("ts".to_string()),
    );

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(
                finalized_manifest_body(&incoming.id, "ts"),
                vec![incoming],
                1,
                Some(1_000),
                parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
            )
            .expect_err("resource entry overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientResourceEntries);
    assert_eq!(violation.actual, MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION + 1);
}

#[test]
fn transient_resource_id_is_stable_and_opaque() {
    let first = build_transient_resource_id("http://origin.example.com/live/key.bin", b"secret");
    let second = build_transient_resource_id("http://origin.example.com/live/key.bin", b"secret");
    let other = build_transient_resource_id("http://origin.example.com/live/seg.ts", b"secret");

    assert_eq!(first, second);
    assert_ne!(first, other);
    assert_eq!(first.0.len(), 16);
    assert!(!first.0.contains("origin"));
}

#[test]
fn transient_state_updates_existing_resource_ttl() {
    let mut state = TransientPassthroughState::default();
    let mut resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/seg.ts",
        b"secret",
        10,
        100,
        Some("ts".to_string()),
    );
    resource.encrypted_media = true;
    let resource_id = resource.id.clone();
    state.upsert_resources([resource]);
    let updated = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/seg.ts",
        b"secret",
        20,
        200,
        Some("ts".to_string()),
    );
    state.upsert_resources([updated]);

    assert_eq!(state.resources.len(), 1);
    assert!(state.resources.get(&resource_id).expect("resource").encrypted_media);
    assert_eq!(state.get_valid_resource(&resource_id, 150).expect("resource remains valid").expires_at_ms, 220);
    assert!(state.get_valid_resource(&resource_id, 221).is_none());
}

#[test]
fn transient_resource_refresh_does_not_extend_an_inflight_fill_deadline() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/inflight.ts",
        b"secret",
        10,
        100,
        Some("ts".to_string()),
    );
    state.upsert_resources([resource.clone()]);
    let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 20, 50) {
        super::super::TransientObjectFetchDecision::Fetch(token) => token,
        super::super::TransientObjectFetchDecision::Ready | super::super::TransientObjectFetchDecision::Wait(_) => {
            panic!("new resource starts a cache fetch")
        }
    };

    state.upsert_resources([TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/inflight.ts",
        b"secret",
        100,
        300,
        Some("ts".to_string()),
    )]);

    assert_eq!(state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms), Some(70));
}

#[test]
fn transient_media_resource_extensions_use_video_mp4_content_type() {
    for extension in ["mp4", "m4s", "m4v"] {
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            format!("http://origin.example.com/live/seg.{extension}"),
            b"secret",
            10,
            100,
            Some(extension.to_string()),
        );

        assert_eq!(resource.content_type_hint.as_deref(), Some("video/mp4"));
    }
}

#[test]
fn finalized_catchup_has_no_resource_boundary_at_cache_duration() {
    let proxy_session_id = ProxySessionId("proxy-session".to_string());
    let mut origin_body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
    let durations_ms = (0..46)
        .map(|index| match index {
            0..44 => 6_700,
            44 => 6_720,
            _ => 6_000,
        })
        .collect::<Vec<_>>();
    for (index, duration_ms) in durations_ms.iter().enumerate() {
        writeln!(origin_body, "#EXTINF:{}.{:03},\n{index}.ts", duration_ms / 1_000, duration_ms % 1_000)
            .expect("synthetic manifest renders");
    }
    origin_body.push_str("#EXT-X-ENDLIST\n");
    assert_eq!(durations_ms.iter().take(45).sum::<u64>(), 301_520);
    let lifecycle = parse_manifest_semantics(&origin_body).lifecycle();
    assert_eq!(lifecycle, HlsManifestLifecycle::Finalized);
    let rewritten = TransientManifestRewriter::rewrite(
        &origin_body,
        "http://origin.example.com/archive/index.m3u8",
        &proxy_session_id,
        b"secret",
        0,
        300_000,
    );
    let before_boundary = rewritten
        .resources
        .iter()
        .find(|resource| resource.resolved_origin_uri.ends_with("/44.ts"))
        .expect("segment before boundary")
        .id
        .clone();
    let after_boundary = rewritten
        .resources
        .iter()
        .find(|resource| resource.resolved_origin_uri.ends_with("/45.ts"))
        .expect("segment after boundary")
        .id
        .clone();
    let mut state = TransientPassthroughState::default();
    state.upsert_resources(rewritten.resources);
    state.replace_manifest_with_semantics(rewritten.body, 0, Some(durations_ms.iter().sum()));

    assert!(state.resolve_current_resource(&before_boundary, 301_520).is_some());
    let after = state
        .resolve_current_resource(&after_boundary, 301_520)
        .expect("segment after cache-duration boundary remains resolvable");
    assert!(matches!(
        state.begin_object_fetch(&proxy_session_id, &after, "ts", 301_520, 300_000),
        super::super::TransientObjectFetchDecision::Fetch(_)
    ));
}
