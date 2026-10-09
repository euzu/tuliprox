use super::{
    commit_fetched_manifest, fetch_and_commit_manifest_with_policy, fetch_hls_origin_manifest_request,
    fetched_manifest, hls_manifest_redirect_host, manifest_body, no_delay_policy,
    refresh_from_live_hls_entrypoint_with_retries, request_header_value, retry_after_delay_ms, spawn_test_origin,
    test_app_config, test_origin_refresh_request, test_recovery_timing_policy, test_segment_repair_manager,
    test_session, three_segment_manifest_body, HlsManifestCommitError, HlsManifestCommitRequirement,
    HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest, LiveHlsOriginEntry, OriginRefreshRequest,
};
use crate::{
    manifest_fetch::fetched_effective_manifest_host, refresh::maybe_trigger_origin_refresh,
    HlsManifestAcceptanceDirective, HlsMapWorkerPool, HlsOriginAccountBinding, HlsProxyManager, HlsSegmentCache,
    HlsSegmentWorkerPool, HlsSession, HlsSessionKey, HlsSessionMode, TransientPassthroughReason,
};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use shared::model::HlsStripMode;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tuliprox_core::model::{HlsManifestRecoveryBurstConfig, ReverseProxyDisabledHeaderConfig, StripConfig};
use url::Url;

#[test]
fn retry_after_header_is_parsed_as_milliseconds() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
    assert_eq!(retry_after_delay_ms(&headers), Some(3_000));
}

#[tokio::test]
async fn refresh_stores_headers_after_hls_origin_policy() {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(|_path| {
        (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
    }))
    .await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
    headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
    headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
    headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("blocked"));
    headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));
    headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers,
        origin_provider_session_headers: HeaderMap::new(),
        disabled_headers: Some(ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: true,
            cloudflare_header: true,
            custom_header: Vec::new(),
        }),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.last_rendered_manifest.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let session = session.read().await;
    assert!(!session.origin_request_headers.contains_key(header::AUTHORIZATION));
    assert!(!session.origin_request_headers.contains_key(header::COOKIE));
    assert!(!session.origin_request_headers.contains_key("proxy-authorization"));
    assert!(!session.origin_request_headers.contains_key(header::HOST));
    assert!(!session.origin_request_headers.contains_key("x-blocked"));
    assert!(!session.origin_request_headers.contains_key("cf-ray"));
    assert_eq!(session.origin_request_headers.get(header::ACCEPT_LANGUAGE).expect("language"), "de");
}

#[test]
fn origin_account_binding_change_clears_provider_session_headers() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "1"), b"secret", 100);
    session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=abc"));
    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-a"),
        &session.proxy_session_id.clone(),
        100,
    )));
    assert!(session.origin_provider_session_headers.is_empty());

    session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=next"));
    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-a"),
        &session.proxy_session_id.clone(),
        200,
    )));
    assert!(!session.origin_provider_session_headers.is_empty());

    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-b"),
        &session.proxy_session_id.clone(),
        300,
    )));
    assert!(session.origin_provider_session_headers.is_empty());
}

#[test]
fn transient_commit_accepts_plausible_same_redirect_host_rollover_and_resets_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(100);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[test]
fn transient_commit_with_different_redirect_host_is_held_as_candidate() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
    assert_eq!(session.origin_seq_highwater, Some(758));
    assert!(session.transient.last_manifest_body.is_none());
}

#[test]
fn provider_failover_mirror_without_redirect_uses_resolved_host_signal() {
    let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    fetched.redirect_host = None;
    fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
    fetched.provider_url_index = Some(1);

    assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("mirror.example.com"));
}

#[test]
fn provider_failover_with_redirect_uses_redirect_host_as_manifest_host_signal() {
    let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    fetched.redirect_host = Some("redirect.example.com".to_string());
    fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
    fetched.provider_url_index = Some(1);

    assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("redirect.example.com"));
}

#[test]
fn manifest_redirect_host_is_only_set_for_actual_redirect_host_switch() {
    let resolved = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("resolved url");
    let same_target = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("same url");
    let redirected = Url::parse("http://cdn.example.net/live/play/12345.m3u8").expect("redirect url");

    assert_eq!(hls_manifest_redirect_host(&resolved, &same_target), None);
    assert_eq!(hls_manifest_redirect_host(&resolved, &redirected).as_deref(), Some("cdn.example.net"));
}

#[tokio::test]
async fn shared_initial_manifest_automatic_cross_origin_redirect_keeps_identity_and_scrubs_credentials() {
    let target = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let target_url = format!("{}/final/manifest.m3u8", target.base_url);
    let expected_target_url = target_url.clone();
    let redirect =
        spawn_test_origin(Arc::new(move |_path| (302, vec![("Location", target_url.clone())], String::new()))).await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", redirect.base_url)).expect("entry url");
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer origin-secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=origin-secret"));
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers,
        client: reqwest::Client::builder().no_proxy().build().expect("client"),
        no_redirect_client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("automatic redirect should return the decoded manifest");

    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.attempts, 1);
    assert_eq!(fetched.final_manifest_url, expected_target_url);
    let redirect_requests = redirect.raw_requests.lock().await;
    let target_requests = target.raw_requests.lock().await;
    assert_eq!(redirect_requests.len(), 1);
    assert_eq!(target_requests.len(), 1);
    assert_eq!(request_header_value(&redirect_requests[0], "accept-encoding"), Some("identity"));
    assert_eq!(request_header_value(&target_requests[0], "accept-encoding"), Some("identity"));
    assert_eq!(request_header_value(&redirect_requests[0], "authorization"), Some("Bearer origin-secret"));
    assert_eq!(request_header_value(&redirect_requests[0], "cookie"), Some("sid=origin-secret"));
    assert!(request_header_value(&target_requests[0], "authorization").is_none());
    assert!(request_header_value(&target_requests[0], "cookie").is_none());
}

#[tokio::test]
async fn shared_initial_manifest_budget_covers_status_decoder_redirect_and_success() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let target_hits_for_handler = Arc::clone(&target_hits);
    let server = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
        }
        if path == "/live/play/once/12345" {
            return match target_hits_for_handler.fetch_add(1, Ordering::SeqCst) {
                0 => (500, Vec::new(), "temporary".to_string()),
                1 => (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string()),
                _ => (200, Vec::new(), manifest_body()),
            };
        }
        (404, Vec::new(), String::new())
    }))
    .await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client"),
        use_manual_redirects: true,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("third logical attempt should succeed");

    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.attempts, 3);
    assert_eq!(
        *server.requests.lock().await,
        vec![
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
        ]
    );
    assert!(server
        .raw_requests
        .lock()
        .await
        .iter()
        .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
}

#[tokio::test]
async fn manifest_retry_starts_at_entrypoint_after_redirect_failure() {
    let redirect_hits = Arc::new(AtomicUsize::new(0));
    let redirect_hits_for_handler = Arc::clone(&redirect_hits);
    let server = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
        }
        if path == "/live/play/once/12345" {
            let hit = redirect_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < 2 {
                return (500, Vec::new(), "fail".to_string());
            }
            return (200, Vec::new(), manifest_body());
        }
        (404, Vec::new(), String::new())
    }))
    .await;
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let no_redirect_client =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");

    let fetched = refresh_from_live_hls_entrypoint_with_retries(
        &entry,
        &HeaderMap::new(),
        &reqwest::Client::new(),
        &no_redirect_client,
        true,
        2_000,
        &no_delay_policy(),
    )
    .await
    .expect("refresh eventually succeeds");

    assert_eq!(fetched.attempts, 3);
    assert_eq!(
        *server.requests.lock().await,
        vec![
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345"
        ]
    );
}

#[tokio::test]
async fn successful_redirected_baseline_binds_the_original_request_entry() {
    let target = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;
    let target_url = format!("{}/redirected/final.m3u8", target.base_url);
    let target_url_for_handler = target_url.clone();
    let entry = spawn_test_origin(Arc::new(move |_path| {
        (302, vec![("Location", target_url_for_handler.clone())], String::new())
    }))
    .await;
    let request_url = format!("{}/live/user/pass/12345.m3u8?token=fixed", entry.base_url);
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse(&request_url).expect("redirecting entry URL");

    let committed =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("redirected initial manifest commits");

    assert_eq!(committed.fetched.final_manifest_url, target_url);
    assert_eq!(committed.fetched.resolved_request_url, request_url);
    let binding_url = {
        let session = session.read().await;
        session
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .expect("redirected commit stores binding")
            .request_url()
            .to_string()
    };
    assert_eq!(binding_url, request_url);
    assert_eq!(entry.requests.lock().await.as_slice(), ["/live/user/pass/12345.m3u8?token=fixed"]);
    assert_eq!(target.requests.lock().await.as_slice(), ["/redirected/final.m3u8"]);
}
