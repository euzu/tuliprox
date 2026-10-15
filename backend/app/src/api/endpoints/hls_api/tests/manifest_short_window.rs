use super::{support::TestEncodedManifestOrigin, *};
use shared::model::{HlsManifestRecoveryBurstLevel, HlsStartupConfigDto, HlsStartupMode};
use std::sync::atomic::AtomicU64;
use tuliprox_hls::api::HlsFreshManifestRequiredReason;

struct ShortWindowFixture {
    _temp_dir: tempfile::TempDir,
    origin: TestSegmentOrigin,
    app_state: Arc<AppState>,
    input: ConfigInput,
    request_url: String,
    head: Arc<AtomicU64>,
    count: Arc<AtomicUsize>,
    proxy_session_id: ProxySessionId,
}

enum ShortWindowResources {
    Plain,
    EncryptedWithMap,
    FirstReady { media_base_url: String },
}

async fn short_window_fixture(count: usize) -> ShortWindowFixture {
    short_window_resource_fixture(count, ShortWindowResources::Plain).await
}

async fn short_window_resource_fixture(count: usize, resources: ShortWindowResources) -> ShortWindowFixture {
    let startup_mode = match &resources {
        ShortWindowResources::Plain | ShortWindowResources::EncryptedWithMap => HlsStartupMode::Conservative,
        ShortWindowResources::FirstReady { .. } => HlsStartupMode::FirstReady,
    };
    let head = Arc::new(AtomicU64::new(100));
    let count = Arc::new(AtomicUsize::new(count));
    let (origin_head, origin_count) = (Arc::clone(&head), Arc::clone(&count));
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        let body = if path_has_extension(path, "m3u8") {
            let head = origin_head.load(Ordering::SeqCst);
            let mut body =
                format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
            let extension = match &resources {
                ShortWindowResources::Plain | ShortWindowResources::FirstReady { .. } => "ts",
                ShortWindowResources::EncryptedWithMap => {
                    body.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x01010101010101010101010101010101\n");
                    "m4s"
                }
            };
            for offset in 0..origin_count.load(Ordering::SeqCst) {
                let segment_url = match &resources {
                    ShortWindowResources::FirstReady { media_base_url } => {
                        format!("{media_base_url}/segment-{}.ts", head + u64::try_from(offset).unwrap())
                    }
                    ShortWindowResources::Plain | ShortWindowResources::EncryptedWithMap => {
                        format!("segment-{}.{extension}", head + u64::try_from(offset).unwrap())
                    }
                };
                writeln!(body, "#EXTINF:2.5,\n{segment_url}").unwrap();
            }
            Arc::from(body.into_bytes())
        } else if path_has_extension(path, "mp4") {
            Arc::from(&b"short-window-map"[..])
        } else if path_has_extension(path, "bin") {
            Arc::from(AES_TEST_KEY_BYTES)
        } else {
            match &resources {
                ShortWindowResources::Plain => Arc::from(&b"short-window-media"[..]),
                ShortWindowResources::FirstReady { .. } => unreachable!("media uses the gated segment origin"),
                ShortWindowResources::EncryptedWithMap => Arc::from(encrypt_test_aes128_cbc_pkcs7(
                    b"short-window-media", AES_TEST_KEY_BYTES, [1; 16])),
            }
        };
        TestBinaryOriginResponse::new(StatusCode::OK, body)
    }))
    .await;
    let temp_dir = tempfile::tempdir().unwrap();
    let input = ConfigInput {
        id: 1,
        name: Arc::from("short-window-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state = test_app_state_with_hls_proxy_and_inputs(
        Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)),
        vec![Arc::new(input.clone())],
    );
    let mut cache = HlsCacheConfigDto {
        cache_path: Some(temp_dir.path().to_string_lossy().to_string()),
        startup: HlsStartupConfigDto { mode: startup_mode, ..HlsStartupConfigDto::default() },
        ..HlsCacheConfigDto::default()
    };
    cache.manifest_recovery_burst.level = HlsManifestRecoveryBurstLevel::Friendly;
    let config = Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(cache),
            ..Default::default()
        })),
        ..Config::default()
    };
    app_state.app_config.config.store(Arc::new(config));
    app_state.hls.proxy.update_config(&app_state.app_config).await;
    configure_default_test_server(&app_state);
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/channel/index.m3u8", origin.base_url);
    cache_test_m3u_hls_item(&app_state, &target, test_m3u_hls_item(&input, 12345, "short-channel", &request_url)).await;
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let proxy_session_id = crate::api::model::build_proxy_session_id(&key, &app_state.get_encrypt_secret());
    ShortWindowFixture { _temp_dir: temp_dir, origin, app_state, input, request_url, head, count, proxy_session_id }
}

/// Keeps the first media fetch pending until all lease entries have been checked.
async fn gated_segment_origin(gate: Arc<tokio::sync::Semaphore>) -> TestEncodedManifestOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let origin_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let requests = Arc::clone(&origin_requests);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let mut buffer = [0_u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.lock().await.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let _permit = gate.acquire().await;
                let body = [0x47; 8192];
                let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
        }
    });
    TestEncodedManifestOrigin { base_url: format!("http://{addr}"), requests, task }
}

async fn wait_for_shared_first_ready_refresh(
    fixture: &ShortWindowFixture,
    session: &HlsSessionHandle,
    previous_requests: usize,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.origin.manifest_request_count() > previous_requests
                && !session.read().await.origin_refresh.in_flight
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("additional leases complete one shared freshness request");
    assert_eq!(fixture.origin.manifest_request_count(), previous_requests + 1);
}

async fn assert_first_ready_lease_media(
    fixture: &ShortWindowFixture,
    leases: &mut tokio::task::JoinSet<String>,
    minimum_media_sequence: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(result) = leases.join_next().await {
            let body = result.unwrap();
            assert!(manifest_media_sequence(&body) >= minimum_media_sequence);
            assert_eq!(media_uri_count(&body), 1);
            for uri in body.lines().filter(|line| !line.starts_with('#') && !line.is_empty()) {
                let response = get_response(Arc::clone(&fixture.app_state), uri, None).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response_body(response).await.as_ref(), &[0x47; 8192]);
            }
        }
    })
    .await
    .expect("media publication releases every waiting lease");
}

#[tokio::test]
async fn first_ready_leases_share_qualification_while_first_segment_is_loading() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let media = gated_segment_origin(Arc::clone(&gate)).await;
    let fixture = Arc::new(
        short_window_resource_fixture(1, ShortWindowResources::FirstReady { media_base_url: media.base_url.clone() })
            .await,
    );
    let mut leases = tokio::task::JoinSet::new();
    let first = Arc::clone(&fixture);
    leases.spawn(async move { open_short_lease(&first, "first-ready-0").await });
    let session = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !media.requests.lock().await.is_empty() {
                if let Some(session) =
                    fixture.app_state.hls.proxy.sessions().get_by_proxy_session_id(&fixture.proxy_session_id).await
                {
                    if !session.read().await.origin_refresh.in_flight {
                        break session;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("qualified origin has started its first segment fetch");
    let generation = {
        let session = session.read().await;
        assert_eq!(session.startup_mode(), HlsStartupMode::FirstReady);
        assert!(session.last_rendered_manifest.is_none());
        assert_eq!(session.origin_control.acceptance_generation.0, 1);
        session.origin_control.acceptance_generation
    };
    assert_eq!(fixture.origin.manifest_request_count(), 3);
    for lease in 1..20 {
        let fixture = Arc::clone(&fixture);
        leases.spawn(async move { open_short_lease(&fixture, &format!("first-ready-{lease}")).await });
    }
    wait_for_shared_first_ready_refresh(&fixture, &session, 3).await;
    assert!(session.read().await.last_rendered_manifest.is_none());
    assert_eq!(session.read().await.origin_control.acceptance_generation, generation);
    gate.close();
    assert_first_ready_lease_media(&fixture, &mut leases, 0).await;
    assert_eq!(session.read().await.origin_control.acceptance_generation, generation);
}

#[tokio::test]
async fn first_ready_revalidation_keeps_its_qualification_while_new_media_is_loading() {
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let media = gated_segment_origin(Arc::clone(&gate)).await;
    let fixture = Arc::new(
        short_window_resource_fixture(1, ShortWindowResources::FirstReady { media_base_url: media.base_url.clone() })
            .await,
    );
    let previous_body = open_short_lease(&fixture, "revalidation-baseline").await;
    let session =
        fixture.app_state.hls.proxy.sessions().get_by_proxy_session_id(&fixture.proxy_session_id).await.unwrap();
    let previous_commit = {
        let mut session = session.write().await;
        assert_eq!(session.startup_mode(), HlsStartupMode::FirstReady);
        assert_eq!(session.origin_control.acceptance_generation.0, 1);
        session.last_rendered_manifest.as_mut().unwrap().valid_until_ms = 0;
        session.activity.last_authorized_media_at_ms = None;
        session.startup.as_mut().unwrap().revisions.clear();
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Expired;
        }
        session.last_normal_manifest_commit_identity().unwrap()
    };
    assert_eq!(fixture.origin.manifest_request_count(), 3);
    assert_eq!(gate.forget_permits(1), 1);
    assert_eq!(
        hls_manifest_commit_requirement(
            &session,
            tuliprox_hls::api::HlsSessionStoreOutcome::Reused,
            None,
            true,
            super::super::current_time_millis(),
        )
        .await,
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        }
    );
    fixture.head.store(101, Ordering::SeqCst);
    let mut leases = tokio::task::JoinSet::new();
    let first = Arc::clone(&fixture);
    leases.spawn(async move { open_short_lease(&first, "revalidation-waiting-0").await });
    let revalidation = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if media.requests.lock().await.len() >= 2 {
                let state = session.read().await;
                if !state.origin_refresh.in_flight && state.origin_seq_highwater == Some(101) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let media_requests = media.requests.lock().await.len();
    {
        let session = session.read().await;
        assert!(
            revalidation.is_ok(),
            "revalidation requests={} generation={} highwater={:?} media_requests={media_requests} in_flight={} commit={:?}",
            fixture.origin.manifest_request_count(),
            session.origin_control.acceptance_generation.0,
            session.origin_seq_highwater,
            session.origin_refresh.in_flight,
            session.last_normal_manifest_commit_identity(),
        );
    }
    assert_eq!(fixture.origin.manifest_request_count(), 5);
    {
        let session = session.read().await;
        assert_eq!(session.startup_mode(), HlsStartupMode::FirstReady);
        assert_eq!(session.origin_control.acceptance_generation.0, 2);
        assert_eq!(session.last_normal_manifest_commit_identity(), Some(previous_commit));
    }
    assert_eq!(
        hls_manifest_commit_requirement(
            &session,
            tuliprox_hls::api::HlsSessionStoreOutcome::Reused,
            None,
            true,
            super::super::current_time_millis(),
        )
        .await,
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        }
    );
    for lease in 1..20 {
        let fixture = Arc::clone(&fixture);
        leases.spawn(async move { open_short_lease(&fixture, &format!("revalidation-waiting-{lease}")).await });
    }
    wait_for_shared_first_ready_refresh(&fixture, &session, 5).await;
    assert_eq!(session.read().await.origin_control.acceptance_generation.0, 2);
    assert_eq!(session.read().await.last_normal_manifest_commit_identity(), Some(previous_commit));
    gate.close();
    assert_first_ready_lease_media(&fixture, &mut leases, manifest_media_sequence(&previous_body) + 1).await;
    assert_eq!(session.read().await.origin_control.acceptance_generation.0, 2);
    assert!(session.read().await.last_normal_manifest_commit_identity().unwrap() > previous_commit);
}

async fn open_short_lease(fixture: &ShortWindowFixture, lease: &str) -> String {
    let access_lease_id = HlsAccessLeaseId(lease.to_string());
    let access = test_hls_access_context(fixture.proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&fixture.app_state, &fixture.proxy_session_id, &access_lease_id).await;
    let response = super::super::try_hls_cache_canonical_manifest_response(
        &fixture.app_state,
        &test_fingerprint(),
        &access,
        &fixture.proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: &fixture.request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(&fixture.request_url),
            input: &fixture.input,
            origin_source: super::super::build_hls_origin_source(&fixture.input, "12345"),
        },
        HeaderMap::new(),
        None,
        "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("shared HLS response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    assert!(!body.contains("#EXT-X-ENDLIST"), "short origin must remain live: {body}");
    assert!(body.contains("/hls/shared/live/"));
    body
}

async fn assert_short_media(fixture: &ShortWindowFixture, body: &str) {
    for uri in body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let response = get_response(Arc::clone(&fixture.app_state), uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), b"short-window-media");
    }
}

#[tokio::test]
async fn one_segment_origin_publishes_live_manifest_and_segment() {
    let fixture = short_window_fixture(1).await;
    let body = open_short_lease(&fixture, "short-one").await;
    assert_eq!(media_uri_count(&body), 1);
    assert_short_media(&fixture, &body).await;
    assert_eq!(fixture.origin.manifest_request_count(), 3);
}

#[tokio::test]
async fn two_segment_origin_publishes_live_manifest_and_segments() {
    let fixture = short_window_fixture(2).await;
    let body = open_short_lease(&fixture, "short-two").await;
    assert_eq!(media_uri_count(&body), 2);
    assert_short_media(&fixture, &body).await;
    assert_eq!(fixture.origin.manifest_request_count(), 3);
}

#[tokio::test]
async fn twenty_actual_leases_share_the_same_short_origin_qualification() {
    let mut counts = Vec::new();
    for leases in [1, 20] {
        let fixture = Arc::new(short_window_fixture(1).await);
        let mut tasks = tokio::task::JoinSet::new();
        for lease in 0..leases {
            let fixture = Arc::clone(&fixture);
            tasks.spawn(async move { open_short_lease(&fixture, &format!("short-shared-{lease}")).await });
        }
        while let Some(result) = tasks.join_next().await {
            let body = result.unwrap();
            assert_eq!(media_uri_count(&body), 1);
            assert_short_media(&fixture, &body).await;
        }
        counts.push(fixture.origin.manifest_request_count());
        let before_new_lease = fixture.origin.manifest_request_count();
        open_short_lease(&fixture, "later-short-lease").await;
        assert_eq!(fixture.origin.manifest_request_count(), before_new_lease);
    }
    assert_eq!(counts, vec![3, 3]);
}

#[tokio::test]
async fn ten_short_refresh_cycles_keep_serving_live_media_and_immutable_old_routes() {
    assert_ten_short_refresh_cycles([3, 1, 3, 1, 3, 1, 3, 1, 3, 1]).await;
}

#[tokio::test]
async fn continuously_advancing_one_segment_origin_serves_ten_refresh_cycles() {
    assert_ten_short_refresh_cycles([1; 10]).await;
}

async fn assert_ten_short_refresh_cycles(counts: [usize; 10]) {
    let fixture = short_window_fixture(1).await;
    let initial = open_short_lease(&fixture, "short-reloads").await;
    assert_short_media(&fixture, &initial).await;
    let uri = format!("/hls/shared/live/{}/short-reloads/manifest.m3u8", fixture.proxy_session_id.0);
    let session =
        fixture.app_state.hls.proxy.sessions().get_by_proxy_session_id(&fixture.proxy_session_id).await.unwrap();
    let mut previous = initial.clone();
    let mut previous_last = session.read().await.last_rendered_manifest.as_ref().unwrap().last_proxy_seq;
    for cycle in 1..=10 {
        fixture.head.store(100 + cycle * 4, Ordering::SeqCst);
        fixture.count.store(counts[usize::try_from(cycle - 1).unwrap()], Ordering::SeqCst);
        {
            let mut session = session.write().await;
            session.origin_refresh.next_fetch_allowed_at_ms = 0;
        }
        let mut current = None;
        for _ in 0..100 {
            let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            assert!(!body.contains("#EXT-X-ENDLIST"));
            if body != previous {
                current = Some(body);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let body = current.expect("normal refresh advances the short window");
        assert!(manifest_media_sequence(&body) >= manifest_media_sequence(&previous));
        previous = body.clone();
        assert_short_media(&fixture, &body).await;
        let lease = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(
                &HlsAccessLeaseId("short-reloads".to_string()),
                &fixture.proxy_session_id,
                super::super::current_time_millis(),
            )
            .await
            .unwrap();
        let snapshot = lease.last_manifest_snapshot.unwrap();
        assert!(snapshot.last_proxy_seq > previous_last);
        previous_last = snapshot.last_proxy_seq;
        assert_eq!(snapshot.playlist_duration_ms, u64::try_from(media_uri_count(&body)).unwrap() * 2_500);
    }
    assert_eq!(fixture.origin.manifest_request_count(), 13);
    assert_short_media(&fixture, &initial).await;
    let invalid = uri.replace("short-reloads", "missing-lease");
    assert_ne!(get_response(Arc::clone(&fixture.app_state), &invalid, None).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn short_manifests_keep_map_key_and_segment_routes_on_later_publications() {
    let fixture = short_window_resource_fixture(2, ShortWindowResources::EncryptedWithMap).await;
    let initial = open_short_lease(&fixture, "short-resources").await;
    let session =
        fixture.app_state.hls.proxy.sessions().get_by_proxy_session_id(&fixture.proxy_session_id).await.unwrap();
    for uri in initial.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let response = get_response(Arc::clone(&fixture.app_state), uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        response_body(response).await;
    }
    let mut bodies = vec![initial];
    fixture.head.store(102, Ordering::SeqCst);
    session.write().await.origin_refresh.next_fetch_allowed_at_ms = 0;
    let uri = format!("/hls/shared/live/{}/short-resources/manifest.m3u8", fixture.proxy_session_id.0);
    for _ in 0..100 {
        let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
        if body != bodies[0] {
            bodies.push(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(bodies.len(), 2, "a second short publication must advance");
    for body in bodies {
        assert!(!body.contains("#EXT-X-ENDLIST"));
        for (tag, expected) in [("#EXT-X-MAP:", &b"short-window-map"[..]), ("#EXT-X-KEY:", AES_TEST_KEY_BYTES)] {
            let uri = body
                .lines()
                .find(|line| line.starts_with(tag))
                .unwrap()
                .split_once("URI=\"")
                .unwrap()
                .1
                .split_once('"')
                .unwrap()
                .0;
            let response = get_response(Arc::clone(&fixture.app_state), uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response_body(response).await.as_ref(), expected);
        }
        for uri in body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let response = get_response(Arc::clone(&fixture.app_state), uri, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response_body(response).await.as_ref(),
                encrypt_test_aes128_cbc_pkcs7(b"short-window-media", AES_TEST_KEY_BYTES, [1; 16])
            );
        }
    }
    assert_eq!(fixture.origin.manifest_request_count(), 4);
}
