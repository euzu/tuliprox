use super::{
    classify_resource_destination, download_text_content, download_text_content_with_headers_and_options,
    get_remote_content_as_stream, identity_fetch_options, is_safe_cross_origin_redirect_header, make_epg_test_client,
    make_provider_with_dns, make_test_app_config, preview_request_target_for_logging, request_header_value,
    resolve_attempt_target, resolve_resource_socket_addrs, response_with_body,
    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
    send_input_with_retry_and_provider_policy_with_options_result, start_plain_http_server_with_body,
    start_plain_http_server_with_response, start_recording_http_byte_server, start_recording_http_server,
    strip_sensitive_headers_for_cross_origin_redirect, test_input_source, test_retry_config, RequestFetchOptions,
    ResourceDestination, ResourceDestinationResolver, TextContentBodyOptions, TextContentFetchOptions,
};
use crate::model::{
    Config, ConfigProvider, InputSource, ResourceRetryConfig, ReverseProxyConfig, ReverseProxyDisabledHeaderConfig,
};
use flate2::{
    write::{GzEncoder, ZlibEncoder},
    Compression,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING, COOKIE};
use shared::{
    defaults::DEFAULT_USER_AGENT,
    model::{ConfigProviderDto, InputFetchMethod, OnConnectErrorPolicy, ProviderUrlSelectionPolicy},
};
use std::{
    collections::HashMap,
    io::{ErrorKind, Write},
    net::IpAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use url::Url;

#[test]
fn resource_destinations_are_classified_by_reachability() {
    for (address, expected) in [
        ("1.1.1.1", ResourceDestination::Public),
        ("8.8.8.8", ResourceDestination::Public),
        ("64:ff9b::808:808", ResourceDestination::Public),
        ("2002:808:808::", ResourceDestination::Public),
        ("::808:808", ResourceDestination::Public),
        ("10.0.0.1", ResourceDestination::Private),
        ("172.16.5.5", ResourceDestination::Private),
        ("192.168.1.20", ResourceDestination::Private),
        ("100.64.0.1", ResourceDestination::Private),
        ("fc00::1", ResourceDestination::Private),
        ("64:ff9b::a00:1", ResourceDestination::Private),
        ("2002:0a00:0001::", ResourceDestination::Private),
        ("::a00:1", ResourceDestination::Private),
        ("::ffff:10.0.0.1", ResourceDestination::Private),
        ("0.0.0.0", ResourceDestination::Blocked),
        ("127.0.0.1", ResourceDestination::Blocked),
        ("169.254.1.1", ResourceDestination::Blocked),
        ("169.254.169.254", ResourceDestination::Blocked),
        ("255.255.255.255", ResourceDestination::Blocked),
        ("224.0.0.1", ResourceDestination::Blocked),
        ("::", ResourceDestination::Blocked),
        ("::1", ResourceDestination::Blocked),
        ("fe80::1", ResourceDestination::Blocked),
        ("ff02::1", ResourceDestination::Blocked),
        ("::ffff:127.0.0.1", ResourceDestination::Blocked),
        ("64:ff9b::7f00:1", ResourceDestination::Blocked),
        ("2002:7f00:1::", ResourceDestination::Blocked),
    ] {
        let address = address.parse::<IpAddr>().expect("valid IP address");
        assert_eq!(super::super::classify_ip(address), expected, "{address}");
    }
}

#[tokio::test]
async fn resource_destination_names_are_classified_without_resolving_ip_literals() {
    assert_eq!(classify_resource_destination("192.168.1.20").await, ResourceDestination::Private);
    assert_eq!(classify_resource_destination("[::1]").await, ResourceDestination::Blocked);
    assert_eq!(classify_resource_destination("[64:ff9b::7f00:1]").await, ResourceDestination::Blocked);
    assert_eq!(classify_resource_destination("8.8.8.8").await, ResourceDestination::Public);
}

#[tokio::test]
async fn resource_destination_resolution_refuses_addresses_local_to_this_host() {
    let private = resolve_resource_socket_addrs("10.0.0.1", 80).await.expect("private destination is allowed");
    assert_eq!(private.len(), 1);

    for local_only in ["127.0.0.1", "169.254.169.254", "[::1]", "fe80::1", "0.0.0.0"] {
        let error =
            resolve_resource_socket_addrs(local_only, 80).await.expect_err("local-only destination must be refused");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{local_only}");
    }
}

#[tokio::test]
async fn resource_destination_resolver_refuses_local_only_names() {
    use reqwest::dns::Resolve;
    use std::str::FromStr;

    let resolver = ResourceDestinationResolver::default();
    let name = reqwest::dns::Name::from_str("127.0.0.1").expect("valid destination name");
    let result = resolver.resolve(name).await;

    match result {
        Ok(_) => panic!("local-only destination must be refused"),
        Err(error) => assert!(error.to_string().contains("local to this host"), "{error}"),
    }

    // Only the proxy hosts are exempt, so a proxy on the loopback interface stays usable while a
    // destination that resolves to the loopback interface is still refused.
    let resolver = ResourceDestinationResolver { allowed_hosts: vec![Arc::from("localhost")].into() };
    let name = reqwest::dns::Name::from_str("localhost").expect("valid proxy name");
    assert!(resolver.resolve(name).await.is_ok(), "the configured proxy host must resolve");
    let name = reqwest::dns::Name::from_str("127.0.0.1").expect("valid destination name");
    assert!(resolver.resolve(name).await.is_err(), "a destination may not use the exemption");
}

#[test]
fn test_get_request_headers_prioritization() {
    use super::super::{append_user_agent_stream_index, get_request_headers, overlay_upstream_user_agent};
    use axum::http::header::USER_AGENT;

    // Case 1: No headers provided -> Default UA
    let headers = get_request_headers::<std::collections::hash_map::RandomState>(None, None, None, None);
    assert_eq!(headers.get(USER_AGENT).unwrap(), DEFAULT_USER_AGENT);

    // Case 2: No headers provided but config default UA set -> Config default UA
    let headers =
        get_request_headers::<std::collections::hash_map::RandomState>(None, None, None, Some("Config-Default-UA"));
    assert_eq!(headers.get(USER_AGENT).unwrap(), "Config-Default-UA");

    // Case 3: Only client header -> Client UA (overrides config default UA)
    let mut client_headers = HashMap::new();
    client_headers.insert("User-Agent".to_string(), b"Client-UA".to_vec());
    let headers = get_request_headers(None, Some(&client_headers), None, Some("Config-Default-UA"));
    assert_eq!(headers.get(USER_AGENT).unwrap(), "Client-UA");

    // Case 4: Both config and client -> Config UA overrides
    let mut config_headers = HashMap::new();
    config_headers.insert("User-Agent".to_string(), "Config-UA".to_string());
    let headers = get_request_headers(Some(&config_headers), Some(&client_headers), None, Some("Config-Default-UA"));
    assert_eq!(headers.get(USER_AGENT).unwrap(), "Config-UA");

    // Case 5: Other headers also prioritized
    config_headers.insert("X-Test".to_string(), "From-Config".to_string());
    let mut client_headers = HashMap::new();
    client_headers.insert("X-Test".to_string(), b"From-Client".to_vec());
    let headers = get_request_headers(Some(&config_headers), Some(&client_headers), None, Some("Config-Default-UA"));
    assert_eq!(headers.get("X-Test").unwrap(), "From-Config");

    let mut headers =
        get_request_headers(Some(&config_headers), Some(&client_headers), None, Some("Config-Default-UA"));
    overlay_upstream_user_agent(&mut headers, Some("Channel-UA"), None);
    assert_eq!(headers.get(USER_AGENT).unwrap(), "Channel-UA");

    let disabled = ReverseProxyDisabledHeaderConfig {
        referer_header: false,
        x_header: false,
        cloudflare_header: false,
        custom_header: vec!["User-Agent".to_string()],
    };
    let mut headers =
        get_request_headers(Some(&config_headers), Some(&client_headers), Some(&disabled), Some("Config-Default-UA"));
    overlay_upstream_user_agent(&mut headers, Some("Blocked-Channel-UA"), Some(&disabled));
    assert!(!headers.contains_key(USER_AGENT));

    let mut headers = get_request_headers::<std::collections::hash_map::RandomState>(None, None, None, Some("VLC/3.0"));
    append_user_agent_stream_index(&mut headers, 42);
    assert_eq!(headers.get(USER_AGENT).and_then(|value| value.to_str().ok()), Some("VLC/3.0 42"));
    append_user_agent_stream_index(&mut headers, 42);
    assert_eq!(headers.get(USER_AGENT).and_then(|value| value.to_str().ok()), Some("VLC/3.0 42"));

    let mut headers = HeaderMap::new();
    let extended_user_agent = HeaderValue::from_bytes(b"Receiver/1.0 \xE4");
    assert!(extended_user_agent.is_ok());
    if let Ok(value) = extended_user_agent {
        headers.insert(USER_AGENT, value);
    }
    append_user_agent_stream_index(&mut headers, 7);
    assert_eq!(headers.get(USER_AGENT).map(HeaderValue::as_bytes), Some(b"Receiver/1.0 \xE4 7".as_slice()));
}

#[test]
fn test_cross_origin_redirect_strips_sensitive_headers() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer test".to_string());
    headers.insert("Cookie".to_string(), "sid=123".to_string());
    headers.insert("Proxy-Authorization".to_string(), "Basic abc".to_string());
    headers.insert("Host".to_string(), "old.host".to_string());
    headers.insert("X-API-Key".to_string(), "secret".to_string());
    headers.insert("Accept".to_string(), "application/x-mpegurl".to_string());
    headers.insert("User-Agent".to_string(), "mpv".to_string());

    strip_sensitive_headers_for_cross_origin_redirect(&mut headers);

    assert!(!headers.contains_key("Authorization"));
    assert!(!headers.contains_key("Cookie"));
    assert!(!headers.contains_key("Proxy-Authorization"));
    assert!(!headers.contains_key("Host"));
    assert!(!headers.contains_key("X-API-Key"));
    assert_eq!(headers.get("Accept").map(String::as_str), Some("application/x-mpegurl"));
    assert_eq!(headers.get("User-Agent").map(String::as_str), Some("mpv"));
}

#[test]
fn test_cross_origin_redirect_header_allowlist_is_minimal() {
    assert!(is_safe_cross_origin_redirect_header("accept"));
    assert!(is_safe_cross_origin_redirect_header("user-agent"));
    assert!(is_safe_cross_origin_redirect_header("icy-metadata"));
    assert!(!is_safe_cross_origin_redirect_header("authorization"));
    assert!(!is_safe_cross_origin_redirect_header("cookie"));
    assert!(!is_safe_cross_origin_redirect_header("x-api-key"));
    assert!(!is_safe_cross_origin_redirect_header("x-auth-token"));
}

#[test]
fn test_keep_vhost_false_uses_ip_host_header_for_http() {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1"]);
    let url = Url::parse("http://example.com:8080/stream").expect("url parse should work");

    let target = resolve_attempt_target(&url, Some(&provider));
    assert_eq!(target.effective_url.host_str(), Some("192.168.0.1"));
    assert_eq!(target.host_header.as_deref(), Some("192.168.0.1:8080"));
}

#[test]
fn test_keep_vhost_true_uses_hostname_host_header_for_http() {
    let provider = make_provider_with_dns(true, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1"]);
    let url = Url::parse("http://example.com:8080/stream").expect("url parse should work");

    let target = resolve_attempt_target(&url, Some(&provider));
    assert_eq!(target.effective_url.host_str(), Some("192.168.0.1"));
    assert_eq!(target.host_header.as_deref(), Some("example.com:8080"));
}

#[test]
fn test_preview_request_target_for_logging_does_not_advance_dns_rotation() {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "192.168.0.2"]);
    let url = Url::parse("http://example.com/live").expect("url parse should work");

    let preview = preview_request_target_for_logging(&url, Some(&provider));
    let first = resolve_attempt_target(&url, Some(&provider));
    let second = resolve_attempt_target(&url, Some(&provider));

    assert_eq!(preview, "http://192.168.0.1/live");
    assert_eq!(first.connect_ip.map(|ip| ip.to_string()), Some("192.168.0.1".to_string()));
    assert_eq!(second.connect_ip.map(|ip| ip.to_string()), Some("192.168.0.2".to_string()));

    let provider_https =
        make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "192.168.0.2"]);
    let https_url = Url::parse("https://example.com/live").expect("url parse should work");

    let https_preview = preview_request_target_for_logging(&https_url, Some(&provider_https));
    let https_first = resolve_attempt_target(&https_url, Some(&provider_https));
    let https_second = resolve_attempt_target(&https_url, Some(&provider_https));

    assert_eq!(https_preview, "https://example.com/live (connect_ip=192.168.0.1)");
    assert_eq!(https_first.connect_ip.map(|ip| ip.to_string()), Some("192.168.0.1".to_string()));
    assert_eq!(https_second.connect_ip.map(|ip| ip.to_string()), Some("192.168.0.2".to_string()));
}

pub(in crate::utils::network::request::tests) fn response_with_byte_body(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response =
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    response.extend_from_slice(body);
    response
}

pub(in crate::utils::network::request::tests) fn gzip_encoded(body: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).expect("gzip input");
    encoder.finish().expect("gzip output")
}

pub(in crate::utils::network::request::tests) fn zlib_encoded(body: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).expect("zlib input");
    encoder.finish().expect("zlib output")
}

#[tokio::test]
async fn http_error_response_option_preserves_final_status_headers_and_configured_retries(
) -> Result<(), Box<dyn std::error::Error>> {
    let app_config = test_retry_config(3);
    let client = reqwest::Client::builder().no_proxy().build()?;
    for failover_only in [false, true] {
        for status in [reqwest::StatusCode::FORBIDDEN, reqwest::StatusCode::SERVICE_UNAVAILABLE] {
            let response = format!(
                "HTTP/1.1 {}\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                super::super::format_http_status(status)
            );
            let (addr, accepted, server) = start_plain_http_server_with_response(response).await?;
            let url = Url::parse(&format!("http://{addr}/init.hls.fmp4"))?;
            let mut options = RequestFetchOptions::default().with_http_error_responses(true);
            if failover_only {
                options = options.without_resource_retries();
            }
            let response = super::super::send_with_retry_and_provider_policy_with_options(
                &app_config,
                &url,
                None,
                false,
                true,
                options,
                |resolved| client.get(resolved.clone()),
            )
            .await?;
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[reqwest::header::RETRY_AFTER], "0");
            assert_eq!(accepted.load(Ordering::SeqCst), if !failover_only && status.is_server_error() { 3 } else { 1 });
            server.abort();
        }
    }
    Ok(())
}

#[test]
fn physical_request_attempt_applies_target_and_final_content_coding_options() {
    let client = reqwest::Client::builder().no_proxy().build().expect("test client");
    let request_url = Url::parse("http://origin.example/manifest.m3u8").expect("request URL");
    let effective_url = Url::parse("http://127.0.0.1/manifest.m3u8").expect("effective URL");
    let target = super::super::AttemptTarget {
        request_url: request_url.clone(),
        effective_url: effective_url.clone(),
        host_header: Some("origin.example".to_string()),
        sni_host: None,
        connect_ip: Some("127.0.0.1".parse().expect("connect IP")),
        dns_host: Some("origin.example".to_string()),
    };
    let builder =
        client.get(request_url).header(ACCEPT_ENCODING, "gzip").header(reqwest::header::HOST, "wrong.example");

    let (_, request) = super::super::prepare_physical_request_attempt(builder, &target, identity_fetch_options())
        .expect("physical request should build");

    assert_eq!(request.url(), &effective_url);
    assert_eq!(request.headers()[reqwest::header::HOST], "origin.example");
    assert_eq!(request.headers()[ACCEPT_ENCODING], "identity");
    assert_eq!(request.timeout(), Some(&Duration::from_secs(1)));
}

#[tokio::test]
async fn text_content_download_decodes_headerless_gzip_and_zlib() {
    const TEXT: &[u8] = b"headerless legacy text\n";

    for (label, encoded) in [("gzip", gzip_encoded(TEXT)), ("zlib", zlib_encoded(TEXT))] {
        let (addr, requests, handle) =
            start_recording_http_byte_server(vec![response_with_byte_body("200 OK", &encoded)])
                .await
                .expect("recording origin should start");
        let url = Url::parse(&format!("http://{addr}/guide.xml")).expect("origin URL");
        let input = test_input_source(url.to_string(), None);

        let (content, _) =
            download_text_content(&test_retry_config(1), &make_epg_test_client(), &input, None, None, false)
                .await
                .unwrap_or_else(|error| panic!("headerless {label} text should decode: {error}"));

        assert_eq!(content.as_bytes(), TEXT, "failed for {label}");
        assert_eq!(requests.lock().await.len(), 1);
        handle.abort();
    }
}

#[tokio::test]
async fn text_content_stream_decodes_headerless_gzip_and_zlib() {
    const TEXT: &[u8] = b"streamed legacy text\n";

    for (label, encoded) in [("gzip", gzip_encoded(TEXT)), ("zlib", zlib_encoded(TEXT))] {
        let (addr, requests, handle) =
            start_recording_http_byte_server(vec![response_with_byte_body("200 OK", &encoded)])
                .await
                .expect("recording origin should start");
        let url = Url::parse(&format!("http://{addr}/guide.xml")).expect("origin URL");
        let input = test_input_source(url.to_string(), None);

        let (mut reader, _) =
            get_remote_content_as_stream(&test_retry_config(1), &make_epg_test_client(), &input, None, &url)
                .await
                .unwrap_or_else(|error| panic!("headerless {label} stream should decode: {error}"));
        let mut content = Vec::new();
        reader.read_to_end(&mut content).await.expect("read decoded stream");

        assert_eq!(content, TEXT, "failed for {label}");
        assert_eq!(requests.lock().await.len(), 1);
        handle.abort();
    }
}

#[tokio::test]
async fn content_coding_identity_wins_after_input_and_client_header_merge() {
    let (addr, requests, handle) = start_recording_http_server(vec![response_with_body("200 OK", "ok")])
        .await
        .expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let mut input = test_input_source(url.to_string(), None);
    input.headers.insert("Accept-Encoding".to_string(), "gzip".to_string());
    let mut client_headers = HeaderMap::new();
    client_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("br"));

    let response = send_input_with_retry_and_provider_policy_with_options_result(
        &test_retry_config(1),
        &make_epg_test_client(),
        &input,
        Some(&client_headers),
        &url,
        identity_fetch_options(),
    )
    .await
    .expect("origin request should succeed");
    drop(response);

    let captured = requests.lock().await;
    assert_eq!(captured.len(), 1);
    assert_eq!(request_header_value(&captured[0], "accept-encoding"), Some("identity"));
    handle.abort();
}

#[tokio::test]
async fn content_coding_identity_manifest_retries_temporary_body_transport_error() {
    let truncated = "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n#EXTM3U\n";
    let responses = vec![truncated.to_string(), response_with_body("200 OK", "#EXTM3U\nsegment.ts\n")];
    let (addr, requests, handle) = start_recording_http_server(responses).await.expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);

    let (manifest, _, _) = download_text_content_with_headers_and_options(
        &test_retry_config(2),
        &make_epg_test_client(),
        &input,
        None,
        false,
        TextContentFetchOptions::new(
            identity_fetch_options(),
            TextContentBodyOptions::hls_manifest(1024, Duration::from_secs(1)),
        ),
    )
    .await
    .expect("temporary body transport failure should retry");

    assert_eq!(manifest, "#EXTM3U\nsegment.ts\n");
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}

#[tokio::test]
async fn content_coding_identity_is_reapplied_after_provider_url_switch() {
    let (first_addr, first_requests, first_handle) =
        start_recording_http_server(vec![response_with_body("502 Bad Gateway", "")])
            .await
            .expect("first provider origin should start");
    let (second_addr, second_requests, second_handle) =
        start_recording_http_server(vec![response_with_body("200 OK", "ok")])
            .await
            .expect("second provider origin should start");
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec![format!("http://{first_addr}").into(), format!("http://{second_addr}").into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let url = Url::parse("provider://provider-a/manifest.m3u8").expect("provider URL");
    let input = test_input_source(url.to_string(), Some(provider));

    send_input_with_retry_and_provider_policy_with_options_result(
        &test_retry_config(1),
        &make_epg_test_client(),
        &input,
        None,
        &url,
        identity_fetch_options(),
    )
    .await
    .expect("provider failover should reach successful response");

    let first = first_requests.lock().await;
    let second = second_requests.lock().await;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(request_header_value(&first[0], "accept-encoding"), Some("identity"));
    assert_eq!(request_header_value(&second[0], "accept-encoding"), Some("identity"));
    first_handle.abort();
    second_handle.abort();
}

#[tokio::test]
async fn content_coding_identity_is_reapplied_for_same_origin_manual_redirect() {
    let redirect = "HTTP/1.1 302 Found\r\nLocation: /final.m3u8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (addr, requests, handle) =
        start_recording_http_server(vec![redirect.to_string(), response_with_body("200 OK", "ok")])
            .await
            .expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/entry.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);

    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
        &test_retry_config(1),
        &make_epg_test_client(),
        &input,
        None,
        &url,
        2,
        identity_fetch_options(),
    )
    .await
    .expect("same-origin redirect should succeed");

    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}

#[tokio::test]
async fn content_coding_identity_survives_cross_origin_redirect_credential_scrubbing() {
    let (target_addr, target_requests, target_handle) =
        start_recording_http_server(vec![response_with_body("200 OK", "ok")])
            .await
            .expect("redirect target should start");
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/final.m3u8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let (entry_addr, entry_requests, entry_handle) =
        start_recording_http_server(vec![redirect]).await.expect("redirect entry should start");
    let url = Url::parse(&format!("http://{entry_addr}/entry.m3u8")).expect("entry URL");
    let mut input = test_input_source(url.to_string(), None);
    input.headers.insert("Authorization".to_string(), "Bearer input-secret".to_string());
    let mut client_headers = HeaderMap::new();
    client_headers.insert(COOKIE, HeaderValue::from_static("sid=client-secret"));

    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
        &test_retry_config(1),
        &make_epg_test_client(),
        &input,
        Some(&client_headers),
        &url,
        2,
        identity_fetch_options(),
    )
    .await
    .expect("cross-origin redirect should succeed");

    let entry = entry_requests.lock().await;
    assert_eq!(entry.len(), 1);
    assert_eq!(request_header_value(&entry[0], "authorization"), Some("Bearer input-secret"));
    assert_eq!(request_header_value(&entry[0], "cookie"), Some("sid=client-secret"));
    drop(entry);
    let target = target_requests.lock().await;
    assert_eq!(target.len(), 1);
    assert_eq!(request_header_value(&target[0], "accept-encoding"), Some("identity"));
    assert!(request_header_value(&target[0], "authorization").is_none());
    assert!(request_header_value(&target[0], "cookie").is_none());
    entry_handle.abort();
    target_handle.abort();
}

#[tokio::test]
async fn manual_redirect_provider_failover_restarts_from_provider_entry() {
    let (redirect_addr, redirect_hits, redirect_handle) = match start_plain_http_server_with_response(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    )
    .await
    {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping manual_redirect_provider_failover_restarts_from_provider_entry: {err}");
            return;
        }
        Err(err) => panic!("failed to start redirect target server: {err}"),
    };
    let redirect_url = format!("http://127.0.0.1:{}/redirected", redirect_addr.port());
    let provider_a_response =
        format!("HTTP/1.1 302 Found\r\nLocation: {redirect_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let provider_entrypoint =
        start_plain_http_server_with_response(provider_a_response).await.expect("provider a test server should start");
    let successful_mirror =
        start_plain_http_server_with_body(b"provider-b").await.expect("provider b test server should start");

    let mut cfg = Config { connect_timeout_secs: 1, ..Config::default() };
    cfg.reverse_proxy = Some(ReverseProxyConfig {
        resource_rewrite_disabled: false,
        rewrite_secret: [0; 16],
        resource_retry: ResourceRetryConfig { max_attempts: 1, ..ResourceRetryConfig::default() },
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
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(400))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client should build");
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec![
            format!("http://127.0.0.1:{}", provider_entrypoint.0.port()).into(),
            format!("http://127.0.0.1:{}", successful_mirror.0.port()).into(),
        ],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let input = InputSource {
        name: Arc::<str>::from("test"),
        url: "provider://provider-a/live/u/p/1.m3u8".to_string(),
        provider: Some(provider),
        username: None,
        password: None,
        method: InputFetchMethod::GET,
        headers: HashMap::default(),
    };
    let entry_url = Url::parse(input.url.as_str()).expect("provider URL should parse");

    let response = send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
        &app_config,
        &client,
        &input,
        None,
        &entry_url,
        5,
        RequestFetchOptions::with_attempt_idle_timeout(Duration::from_secs(1)),
    )
    .await
    .expect("request should fail over from redirected target to next provider entry");
    let provider_url_index = response.provider_url_index;
    let body = response.response.text().await.expect("body should be readable");

    assert_eq!(body, "provider-b");
    assert_eq!(provider_url_index, Some(1));
    assert_eq!(provider_entrypoint.1.load(Ordering::SeqCst), 1);
    assert_eq!(redirect_hits.load(Ordering::SeqCst), 1);
    assert_eq!(successful_mirror.1.load(Ordering::SeqCst), 1);

    provider_entrypoint.2.abort();
    successful_mirror.2.abort();
    redirect_handle.abort();
}
