use super::{
    fetch_hls_origin_manifest_request, manifest_body, no_delay_policy, request_header_value, spawn_test_origin,
    test_app_config, test_recovery_timing_policy, HlsManifestOriginBinding, HlsOriginManifestFetchContext,
    HlsOriginManifestFetchRequest, LiveHlsOriginEntry,
};
use crate::{HlsSession, HlsSessionKey};
use axum::http::{header, HeaderMap, HeaderValue};
use std::sync::Arc;
use tokio::sync::RwLock;
use tuliprox_core::model::HlsManifestRecoveryBurstConfig;
use url::Url;

#[tokio::test]
async fn archive_master_resolution_binds_recovery_to_entry_and_scrubs_redirected_variant_credentials() {
    let archive = spawn_test_origin(Arc::new(|path| {
        if path.starts_with("/channel/tracks-v1/") {
            (200, vec![("Set-Cookie", "route=b; Path=/".to_string())], manifest_body())
        } else {
            let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\ntracks-v1/archive-1-2.ts.m3u8\n";
            (200, vec![("Set-Cookie", "session=a; Path=/".to_string())], master.to_string())
        }
    }))
    .await;
    let master_url = format!("{}/channel/archive-1-2.m3u8", archive.base_url);
    let variant_url = format!("{}/channel/tracks-v1/archive-1-2.ts.m3u8", archive.base_url);
    let entry =
        spawn_test_origin(Arc::new(move |_path| (302, vec![("Location", master_url.clone())], String::new()))).await;
    let entry_url = format!("{}/live/user/pass/archive-1-2.m3u8", entry.base_url);
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer entry-secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=entry-secret"));
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: Arc::new(RwLock::new(HlsSession::new(
            HlsSessionKey::new(1, "12345").with_archive_reference(1_784_898_000),
            b"secret",
            0,
        ))),
        origin_entry: LiveHlsOriginEntry::parse(&entry_url).expect("entry url"),
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
        .expect("archive master resolves to its variant");
    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.final_manifest_url, variant_url);
    assert_eq!(fetched.resolved_request_url, entry_url);
    assert_eq!(fetched.provider_session_headers[header::COOKIE], "session=a; route=b");
    {
        let raw_requests = archive.raw_requests.lock().await;
        let variant_request = raw_requests
            .iter()
            .find(|request| request.starts_with("GET /channel/tracks-v1/"))
            .expect("variant request");
        assert_eq!(request_header_value(variant_request, "authorization"), None);
        assert_eq!(request_header_value(variant_request, "cookie"), Some("session=a"));
    }

    let binding = HlsManifestOriginBinding::new(
        Url::parse(&fetched.resolved_request_url).expect("binding url"),
        fetched.provider_url_index,
    )
    .expect("binding");
    let recovered = crate::manifest_fetch::fetch_hls_origin_manifest_recovery_once(&context, &binding)
        .await
        .expect("recovery re-resolves the archive master");
    assert_eq!(recovered.body, manifest_body());
    assert_eq!(recovered.final_manifest_url, variant_url);
    assert_eq!(recovered.resolved_request_url, entry_url);
    assert_eq!(entry.requests.lock().await.len(), 2);
    let raw_requests = archive.raw_requests.lock().await;
    assert_eq!(raw_requests.len(), 4);
    assert!(raw_requests.iter().all(|request| request_header_value(request, "authorization").is_none()));
    assert!(raw_requests.iter().all(|request| request_header_value(request, "cookie") != Some("sid=entry-secret")));
}
