use super::*;

#[test]
fn storage_full_transient_cache_failure_is_permanent() {
    let error = HlsOriginResourceFetchError::cache_commit(&io::Error::from_raw_os_error(28));

    assert!(matches!(
        hls_transient_object_fetch_failure(&error),
        HlsTransientObjectFetchFailure::Permanent { status: None }
    ));
}

#[tokio::test]
async fn raw_ttl_resource_requires_requesting_lease_manifest_membership_for_full_and_range() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "rolling-membership"), b"rewrite-secret", 0);
    let proxy_session_id = session.proxy_session_id.clone();
    let authorized_lease_id = HlsAccessLeaseId("lease-a".to_string());
    let unpublished_lease_id = HlsAccessLeaseId("lease-b".to_string());
    let segment = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "http://origin.example.com/live/segment.ts",
        b"rewrite-secret",
        0,
        300_000,
        Some("ts".to_string()),
    );
    let key = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/key.bin",
        b"rewrite-secret",
        0,
        300_000,
        Some("key".to_string()),
    );
    let resource_files = [
        TransientResourceFile { resource_id: segment.id.clone(), extension: "ts".to_string() },
        TransientResourceFile { resource_id: key.id.clone(), extension: "key".to_string() },
    ];
    let manifest_body = format!(
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/{}/lease-a/r/{}.key\"\n\
             #EXTINF:6,\n/hls/shared/live/{}/lease-a/r/{}.ts\n",
        proxy_session_id.0, key.id.0, proxy_session_id.0, segment.id.0
    );
    let authorized_resources = HlsPublishedTransientResourceIds::from_manifest_body(&manifest_body);
    let unpublished_resources = HlsPublishedTransientResourceIds::default();
    session.transient.upsert_resources([segment, key]);
    let session = Arc::new(RwLock::new(session));
    let ranges = [None, Some(HeaderValue::from_static("bytes=0-")), Some(HeaderValue::from_static("bytes=1000-"))];

    for resource_file in &resource_files {
        for range in &ranges {
            let rejected = resolve_hls_transient_object_cache_action(
                &session,
                &proxy_session_id,
                HlsTransientResourceLeaseContext {
                    access_lease_id: &unpublished_lease_id,
                    lease_issued_at_ms: 0,
                    published_resource_ids: &unpublished_resources,
                },
                resource_file,
                range.as_ref(),
                1,
                300_000,
            )
            .await;
            assert!(matches!(rejected, Err(StatusCode::NOT_FOUND)));

            let accepted = resolve_hls_transient_object_cache_action(
                &session,
                &proxy_session_id,
                HlsTransientResourceLeaseContext {
                    access_lease_id: &authorized_lease_id,
                    lease_issued_at_ms: 0,
                    published_resource_ids: &authorized_resources,
                },
                resource_file,
                range.as_ref(),
                1,
                300_000,
            )
            .await;
            assert!(accepted.is_ok());
        }
    }
}

#[tokio::test]
async fn finalized_resource_validity_is_identical_for_full_and_range_requests() {
    let (session, proxy_session_id, access_lease_id, resource_file, published_resource_ids) =
        finalized_resource_resolution_fixture();
    let full = resolve_hls_transient_object_cache_action(
        &session,
        &proxy_session_id,
        HlsTransientResourceLeaseContext {
            access_lease_id: &access_lease_id,
            lease_issued_at_ms: 0,
            published_resource_ids: &published_resource_ids,
        },
        &resource_file,
        None,
        301_520,
        300_000,
    )
    .await
    .expect("full request resolves finalized mapping");
    assert!(matches!(full.action, HlsTransientObjectCacheAction::FetchAndCache(_)));

    let zero_range = HeaderValue::from_static("bytes=0-");
    let from_zero = resolve_hls_transient_object_cache_action(
        &session,
        &proxy_session_id,
        HlsTransientResourceLeaseContext {
            access_lease_id: &access_lease_id,
            lease_issued_at_ms: 0,
            published_resource_ids: &published_resource_ids,
        },
        &resource_file,
        Some(&zero_range),
        301_520,
        300_000,
    )
    .await
    .expect("zero range resolves finalized mapping");
    assert!(matches!(from_zero.action, HlsTransientObjectCacheAction::WaitForFetch(_)));

    let offset_range = HeaderValue::from_static("bytes=1000-");
    let from_offset = resolve_hls_transient_object_cache_action(
        &session,
        &proxy_session_id,
        HlsTransientResourceLeaseContext {
            access_lease_id: &access_lease_id,
            lease_issued_at_ms: 0,
            published_resource_ids: &published_resource_ids,
        },
        &resource_file,
        Some(&offset_range),
        301_520,
        300_000,
    )
    .await
    .expect("offset range resolves finalized mapping");
    assert!(matches!(from_offset.action, HlsTransientObjectCacheAction::PassthroughNoCache));
}

#[tokio::test]
async fn removed_finalized_mapping_is_rejected_for_full_and_range_requests() {
    let (session, proxy_session_id, access_lease_id, resource_file, published_resource_ids) =
        finalized_resource_resolution_fixture();
    {
        let mut session = session.write().await;
        session.transient.replace_manifest_with_semantics(
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-ENDLIST\n".to_string(),
            1,
            None,
        );
        assert!(session.transient.release_finalized_manifest_generations(&access_lease_id, 0));
    }
    let ranges = [None, Some(HeaderValue::from_static("bytes=0-")), Some(HeaderValue::from_static("bytes=1000-"))];

    for range in &ranges {
        let result = resolve_hls_transient_object_cache_action(
            &session,
            &proxy_session_id,
            HlsTransientResourceLeaseContext {
                access_lease_id: &access_lease_id,
                lease_issued_at_ms: 0,
                published_resource_ids: &published_resource_ids,
            },
            &resource_file,
            range.as_ref(),
            301_520,
            300_000,
        )
        .await;
        assert!(matches!(result, Err(StatusCode::NOT_FOUND)));
    }
}

#[tokio::test]
async fn cacheable_transient_rejects_identity_partial_response_before_commit_and_cleans_guard() {
    let origin = spawn_test_origin(
        "206 Partial Content",
        vec![("Content-Type", "video/mp2t"), ("Content-Range", "bytes 0-3/10")],
        b"part".to_vec(),
    )
    .await;
    let fixture = TestTransientCacheFixture::new(format!("{}/partial.ts", origin.base_url)).await;
    let dropped_guards = Arc::new(AtomicUsize::new(0));
    let dropped_guards_for_prepare = Arc::clone(&dropped_guards);

    let result =
        fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), move |_| {
            let dropped_guards = Arc::clone(&dropped_guards_for_prepare);
            async move { Ok(DropCounter(dropped_guards)) }.boxed()
        })
        .await;

    assert!(matches!(result, Err(HlsOriginResourceFetchError::UnexpectedByteRangeStatus)));
    assert_eq!(dropped_guards.load(Ordering::Relaxed), 1);
    assert!(fixture.segment_cache.metadata(&fixture.cache_key).await.expect("cache metadata reads").is_none());
    assert!(!fixture.segment_cache.has_active_temp_files());
    assert_eq!(std::fs::read_dir(fixture.temp_dir.path()).expect("cache root reads").count(), 0);
    let session = fixture.session.read().await;
    let entry = session.transient.object_cache.get(&fixture.lookup_key).expect("fetching cache entry remains");
    assert!(!matches!(entry.status, TransientObjectCacheStatus::Ready { .. }));
    drop(session);

    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_ascii_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(!request.contains("\r\nrange:"));
}

#[tokio::test]
async fn stale_resource_revision_commit_deletes_the_physical_fill_after_controlled_mapping_replacement() {
    let origin = spawn_test_origin("200 OK", vec![("Content-Type", "video/mp2t")], b"stale-body".to_vec()).await;
    let fixture = TestTransientCacheFixture::new(format!("{}/stale.ts", origin.base_url)).await;
    {
        let mut session = fixture.session.write().await;
        let replacement_now_ms = tuliprox_core::utils::current_time_millis();
        let mut replacement = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://replacement.example.com/live/replacement.ts",
            b"rewrite-secret",
            replacement_now_ms,
            60_000,
            Some("ts".to_string()),
        );
        replacement.id = fixture.resource.id.clone();
        session.transient.upsert_resources([replacement]);
    }

    let result = fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), |_| {
        async { Ok(()) }.boxed()
    })
    .await;

    assert!(
        matches!(result, Err(HlsOriginResourceFetchError::Superseded)),
        "the superseded resource revision aborts without a retryable timeout"
    );
    assert!(fixture.segment_cache.metadata(&fixture.cache_key).await.expect("cache metadata reads").is_none());
    assert!(!fixture.segment_cache.has_active_temp_files());
    assert!(!fixture.session.read().await.transient.object_cache.contains_key(&fixture.lookup_key));
    assert_eq!(origin.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn cacheable_transient_without_range_decodes_to_identity_before_cache_and_range_reads() {
    let ciphertext =
        vec![0x00, 0xff, 0x92, 0x10, 0x7e, 0x33, 0xc4, 0x81, 0xa8, 0x09, 0x5d, 0xe2, 0x44, 0x17, 0xb0, 0x6f];
    assert!(std::str::from_utf8(&ciphertext).is_err());
    let encoded = zstd_encode(&ciphertext).await;
    assert_ne!(encoded, ciphertext);
    let origin = spawn_zstd_origin(encoded).await;
    let fixture = TestTransientCacheFixture::new(format!("{}/cipher.ts", origin.base_url)).await;

    fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), |_| {
        async { Ok(()) }.boxed()
    })
    .await
    .expect("encoded transient cache fetch succeeds");

    let metadata = fixture
        .segment_cache
        .metadata(&fixture.cache_key)
        .await
        .expect("cache metadata reads")
        .expect("decoded object is cached");
    assert_eq!(metadata.size, ciphertext.len() as u64);
    assert_eq!(tokio::fs::read(&metadata.path).await.expect("cache body reads"), ciphertext);
    let mut range = Vec::new();
    fixture
        .segment_cache
        .open_range(&fixture.cache_key, 5)
        .await
        .expect("decoded cache range opens")
        .take(4)
        .read_to_end(&mut range)
        .await
        .expect("decoded cache range reads");
    assert_eq!(range, ciphertext[5..9]);

    let session = fixture.session.read().await;
    let entry = session.transient.object_cache.get(&fixture.lookup_key).expect("transient cache entry");
    assert!(matches!(
        entry.status,
        TransientObjectCacheStatus::Ready { content_length, .. }
            if content_length == ciphertext.len() as u64
    ));
    assert_eq!(entry.content_type, "application/octet-stream");
    drop(session);

    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_ascii_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(!request.contains("\r\nrange:"));
}
