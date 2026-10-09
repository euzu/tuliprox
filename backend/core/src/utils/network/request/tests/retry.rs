use super::{
    download_text_content_with_headers_and_options, identity_fetch_options, make_epg_test_client, make_test_app_config,
    request_header_value, response_with_body, send_input_with_retry_and_provider_policy_with_options_result,
    send_with_retry_and_provider_policy, should_retry_text_body_error, start_plain_http_server_with_body,
    start_recording_http_server, test_input_source, test_retry_config, TextContentBodyOptions, TextContentFetchOptions,
};
use crate::{
    model::{Config, ConfigProvider, ResourceRetryConfig, ReverseProxyConfig},
    utils::content_coding::ContentCodingError,
};
use shared::model::{ConfigProviderDto, ProviderUrlSelectionPolicy};
use std::{
    io::{Error, ErrorKind},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use url::Url;

#[test]
fn content_coding_prefix_read_error_is_retryable_but_unsupported_coding_is_not() {
    let prefix_read =
        Error::other(ContentCodingError::PrefixRead(Error::new(ErrorKind::UnexpectedEof, "prefix truncated")));
    let unsupported = Error::other(ContentCodingError::Unsupported("compress".to_string()));

    assert!(should_retry_text_body_error(&prefix_read));
    assert!(!should_retry_text_body_error(&unsupported));
}

#[tokio::test]
async fn content_coding_identity_is_reapplied_for_retry() {
    let responses = vec![response_with_body("500 Internal Server Error", ""), response_with_body("200 OK", "ok")];
    let (addr, requests, handle) = start_recording_http_server(responses).await.expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);

    send_input_with_retry_and_provider_policy_with_options_result(
        &test_retry_config(2),
        &make_epg_test_client(),
        &input,
        None,
        &url,
        identity_fetch_options(),
    )
    .await
    .expect("retry should reach successful response");

    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}

#[tokio::test]
async fn text_content_manifest_retry_budget_is_not_multiplied_by_inner_status_retries() {
    let corrupt_gzip =
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
    let responses = vec![
        response_with_body("503 Service Unavailable", ""),
        response_with_body("502 Bad Gateway", ""),
        corrupt_gzip.to_string(),
        response_with_body("200 OK", "#EXTM3U\nsegment.ts\n"),
    ];
    let (addr, requests, handle) = start_recording_http_server(responses).await.expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);
    let options = TextContentFetchOptions::new(
        identity_fetch_options(),
        TextContentBodyOptions::hls_manifest(1024, Duration::from_secs(1)),
    );

    download_text_content_with_headers_and_options(
        &test_retry_config(3),
        &make_epg_test_client(),
        &input,
        None,
        false,
        options,
    )
    .await
    .expect_err("three logical failures must exhaust the configured budget");

    let captured = requests.lock().await;
    assert_eq!(captured.len(), 3, "the fourth success response must not be requested");
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}

#[tokio::test]
async fn send_with_retry_policy_false_does_not_fail_over_provider_urls() {
    let (addr_b, accepted_b, handle_b) = match start_plain_http_server_with_body(b"b").await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping send_with_retry_policy_false_does_not_fail_over_provider_urls: {err}");
            return;
        }
        Err(err) => panic!("failed to start test http server: {err}"),
    };

    let mut cfg = Config { connect_timeout_secs: 1, ..Config::default() };
    cfg.accept_insecure_ssl_certificates = true;
    cfg.reverse_proxy = Some(ReverseProxyConfig {
        resource_rewrite_disabled: false,
        rewrite_secret: [0; 16],
        resource_retry: ResourceRetryConfig { max_attempts: 3, backoff_millis: 1, ..ResourceRetryConfig::default() },
        disabled_header: None,
        stream: None,
        cache: None,
        rate_limit: None,
        geoip: None,
        stream_history: None,
        qos_aggregation: None,
        hls_cache: None,
    });
    let app_config = make_test_app_config(cfg);
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client should build");
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec!["http://127.0.0.1:1".into(), format!("http://127.0.0.1:{}", addr_b.port()).into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));

    let url = Url::parse("provider://provider-a/live").expect("provider url should parse");
    let result =
        send_with_retry_and_provider_policy(&app_config, &url, Some(&provider), false, false, |resolved_url| {
            client.get(resolved_url.clone())
        })
        .await;

    assert!(result.is_err(), "retry disabled must not fail over to the second provider URL");
    assert_eq!(accepted_b.load(Ordering::SeqCst), 0, "fallback provider URL must not be contacted");
    assert_eq!(provider.get_current_index(), 0, "retry disabled must not advance provider URL selection");

    handle_b.abort();
}
