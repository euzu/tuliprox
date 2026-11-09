use super::{
    create_active_hls_user_session, encrypt_test_aes128_cbc_pkcs7, get_response,
    manifest_entry_freshness::{canonical_response_for_lease, enable_hls_cache_with_startup_mode},
    manifest_media_sequence, path_has_extension, response_body, spawn_test_binary_origin, test_app_state_with_inputs,
    ConfigInput, TestBinaryOriginResponse, AES_TEST_KEY_BYTES,
};
use axum::http::StatusCode;
use shared::model::{HlsStartupMode, InputType};
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn trace_fixture() {
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool { true }
        fn log(&self, record: &log::Record<'_>) {
            eprintln!("{} {}", record.target(), record.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: Logger = Logger;
    if std::env::var_os("TULIPROX_RESTART_TRACE").is_some() && log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}

fn media(marker: u8) -> Vec<u8> {
    let mut bytes =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts")).to_vec();
    bytes.extend_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    bytes.extend_from_slice(&[marker; 184]);
    bytes
}

async fn enable_unstripped_cache(app: &Arc<crate::api::model::AppState>, mode: HlsStartupMode) {
    enable_hls_cache_with_startup_mode(app, mode).await;
    let mut config = app.app_config.config.load_full().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().strip =
        crate::model::StripConfig { mode: shared::model::HlsStripMode::Segments, value: 0 };
    app.app_config.config.store(Arc::new(config));
    app.hls.proxy.update_config(&app.app_config).await;
}

/// Runs an origin restart in `mode`; `reuse_urls` keeps the segment namespace, `next_count` sets the new segment count.
async fn restarted_origin_case(mode: HlsStartupMode, reuse_urls: bool, next_count: u64) {
    let phase = Arc::new(AtomicU8::new(0));
    let origin_phase = Arc::clone(&phase);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        let phase = origin_phase.load(Ordering::Acquire);
        let body: Arc<[u8]> = if path_has_extension(path, "m3u8") {
            let (namespace, count) = if phase == 0 {
                ("0123456789abcdef", 5)
            } else if reuse_urls {
                ("0123456789abcdef", next_count)
            } else {
                ("fedcba9876543210", next_count)
            };
            let mut body = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:4\n");
            for seq in 0..count {
                writeln!(body, "#EXTINF:4,\n/{namespace}/{seq}.ts").unwrap();
            }
            Arc::from(body.into_bytes())
        } else {
            Arc::from(media(if reuse_urls { phase } else { u8::from(path.contains("fedcba9876543210")) }))
        };
        TestBinaryOriginResponse::new(StatusCode::OK, body)
    }))
    .await;
    let input = ConfigInput {
        id: 1,
        name: "restart-input".into(),
        input_type: InputType::M3u,
        url: format!("{}/input.m3u", origin.base_url),
        username: Some("user".into()),
        password: Some("pass".into()),
        max_connections: 1,
        enabled: true,
        ..Default::default()
    };
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_unstripped_cache(&app, mode).await;
    create_active_hls_user_session(&app).await;
    let url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let warm = canonical_response_for_lease(&app, &input, &url, "warm").await;
    assert_eq!(warm.status(), StatusCode::OK);
    let warm_body = String::from_utf8(response_body(warm).await.to_vec()).unwrap();
    for uri in warm_body.lines().filter(|line| line.starts_with("/hls/") && !line.contains("/r/")) {
        let response = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), media(0));
    }
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    let previous =
        session.read().await.startup.as_ref().unwrap().revisions[&manifest_media_sequence(&warm_body)].clone();
    let old_metadata = previous
        .revision()
        .wait_complete(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .unwrap();
    let old_bytes = tokio::fs::read(&old_metadata.path).await.unwrap();
    // Segment completion activity is committed by spawned tasks. On a busy or
    // single-core runtime they can land after the backdating write and reset
    // the activity to "now", so repeat the write until it sticks.
    let expired = super::super::current_time_millis().saturating_sub(300_000);
    let mut backdated = false;
    for _ in 0..50 {
        {
            let mut session = session.write().await;
            session.activity.last_authorized_media_at_ms = Some(expired);
            session.origin_refresh.last_success_at_ms = Some(expired);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let session = session.read().await;
        if session.activity.last_authorized_media_at_ms == Some(expired)
            && session.origin_refresh.last_success_at_ms == Some(expired)
        {
            backdated = true;
            break;
        }
    }
    assert!(backdated, "session activity stays backdated");
    phase.store(1, Ordering::Release);
    let response = canonical_response_for_lease(&app, &input, &url, "restart").await;
    assert_eq!(response.status(), StatusCode::OK, "{mode:?}");
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    assert!(!body.contains("#EXT-X-ENDLIST"));
    let first = manifest_media_sequence(&body);
    assert!(first >= 5, "new lease uses the new epoch");
    let session = session.read().await;
    assert_eq!(session.origin_epoch, 1);
    assert_eq!(session.origin_seq_highwater, Some(next_count - 1));
    let current = session.startup.as_ref().unwrap().revisions[&first].clone();
    assert_ne!(current, previous);
    drop(session);
    assert_eq!(
        tokio::fs::read(current.revision().wait_complete(tokio::time::Instant::now()).await.unwrap().path)
            .await
            .unwrap(),
        media(1)
    );
    assert_eq!(tokio::fs::read(old_metadata.path).await.unwrap(), old_bytes);
}

#[tokio::test]
async fn new_lease_receives_restarted_origin_without_mutating_the_previous_revision() {
    trace_fixture();
    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        for reuse_urls in [false, true] {
            for next_count in [4, 5] {
                restarted_origin_case(mode, reuse_urls, next_count).await;
            }
        }
    }
}

#[tokio::test]
async fn encrypted_query_rotation_verifies_raw_media_and_key_without_restarting() {
    trace_fixture();

    let phase = Arc::new(AtomicU8::new(0));
    let key_requests = Arc::new(AtomicUsize::new(0));
    let query_requests = Arc::new(AtomicUsize::new(0));
    let ciphertext: Arc<[u8]> = Arc::from(encrypt_test_aes128_cbc_pkcs7(&media(1), AES_TEST_KEY_BYTES, [0; 16]));
    let origin_phase = Arc::clone(&phase);
    let keys = Arc::clone(&key_requests);
    let queries = Arc::clone(&query_requests);
    let encrypted = Arc::clone(&ciphertext);
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        let body = if path_has_extension(path, "m3u8") {
            let mut body = String::from("#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"/key.bin\",IV=0x00000000000000000000000000000000\n");
            for seq in 0..6 {
                writeln!(body, "#EXTINF:4,\n/{seq}.ts?synthetic={}", origin_phase.load(Ordering::Acquire)).unwrap();
            }
            Arc::from(body.into_bytes())
        } else if path == "/key.bin" {
            keys.fetch_add(1, Ordering::AcqRel);
            Arc::from(AES_TEST_KEY_BYTES)
        } else {
            if path.ends_with("?synthetic=1") {
                queries.fetch_add(1, Ordering::AcqRel);
            }
            Arc::clone(&encrypted)
        };
        TestBinaryOriginResponse::new(StatusCode::OK, body)
    }))
    .await;
    let input = ConfigInput {
        id: 1,
        name: "encrypted-query-input".into(),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        enabled: true,
        max_connections: 1,
        ..Default::default()
    };
    let app = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_unstripped_cache(&app, HlsStartupMode::Conservative).await;
    create_active_hls_user_session(&app).await;
    let url = format!("{}/index.m3u8", origin.base_url);
    let response = canonical_response_for_lease(&app, &input, &url, "warm").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    let uris = body.lines().filter(|line| line.starts_with("/hls/") && !line.contains("/r/")).collect::<Vec<_>>();
    assert_eq!(uris.len(), 6);
    for uri in &uris {
        let response = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), ciphertext.as_ref());
    }
    let key = super::super::build_hls_origin_source(&input, "12345").session_key();
    let session = app.hls.proxy.sessions().get_by_key(&key).await.unwrap();
    let (progress, cache_keys) = {
        let session = session.read().await;
        (
            session.origin_control.progress_generation,
            session.segments.values().map(|entry| entry.cache_key.clone()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(key_requests.load(Ordering::Acquire), 1);
    phase.store(1, Ordering::Release);
    for _ in 0..2 {
        let commits = {
            let mut session = session.write().await;
            session.origin_refresh.next_fetch_allowed_at_ms = 0;
            session.origin_refresh.last_success_at_ms =
                Some(super::super::current_time_millis().saturating_sub(10_000));
            session.origin_manifest_commits()
        };
        let response = canonical_response_for_lease(&app, &input, &url, "warm").await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response_body(response).await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while session.read().await.origin_manifest_commits() == commits {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("query verification commits");
        let session = session.read().await;
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.origin_control.progress_generation, progress);
        assert_eq!(session.segments.values().map(|entry| entry.cache_key.clone()).collect::<Vec<_>>(), cache_keys);
        assert!(session.segments.values().all(|entry| entry
            .origin_fetch_ref
            .as_ref()
            .unwrap()
            .resolved_origin_url
            .ends_with("?synthetic=1")));
    }
    assert_eq!(query_requests.load(Ordering::Acquire), 6, "unchanged polls reuse the completed proof");
    assert_eq!(key_requests.load(Ordering::Acquire), 2, "one fresh check of the shared key");
    for uri in uris {
        let response = get_response(Arc::clone(&app), uri, None).await;
        assert_eq!(response_body(response).await.as_ref(), ciphertext.as_ref());
    }
}
