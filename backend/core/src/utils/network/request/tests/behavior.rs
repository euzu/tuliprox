use super::{
    atomic_download_temp_files, download_text_content_with_headers_and_options, get_input_epg_content_as_file,
    identity_fetch_options, make_epg_test_client, make_provider_with_dns, make_test_app_config,
    next_provider_url_index, preview_request_diagnostics_for_logging, preview_request_target_for_logging,
    request_header_value, resolve_attempt_target, response_with_body, same_origin,
    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result, send_with_retry_and_provider,
    should_try_next_ip_on_connect_error, start_plain_http_server_with_body, start_plain_http_server_with_response,
    start_recording_http_server, test_input_source, test_retry_config, text_response_error_log_label,
    InputEpgFileRequest, PublicIpResolver, RequestFetchOptions, ResourceDestination, TextContentBodyOptions,
    TextContentFetchOptions,
};
use crate::{
    model::{Config, ConfigInput, ConfigProvider, ResourceRetryConfig, ReverseProxyConfig},
    utils::content_coding::{ContentCoding, ContentCodingError, ContentDecodingIoError},
};
use shared::{
    model::{ConfigProviderDto, OnConnectErrorPolicy, ProviderUrlSelectionPolicy},
    utils::{get_base_url_from_str, replace_url_extension, sanitize_sensitive_info},
};
use std::{
    collections::HashSet,
    io::{Error, ErrorKind},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

#[test]
fn ipv6_addresses_embedding_private_ipv4_are_not_public() {
    for address in ["::10.0.0.1", "64:ff9b::a00:1", "2002:0a00:0001::", "::ffff:10.0.0.1"] {
        assert!(address.parse::<IpAddr>().is_ok_and(|ip| !super::super::is_public_ip(ip)), "{address}");
    }
}

#[test]
fn proxy_hosts_are_exempt_from_the_connect_time_local_address_guard() {
    use crate::{model::Config, utils::request::proxy_hosts};

    // `localhost` is what a proxy on this host is usually configured as, and reqwest asks the
    // resolver for the proxy host as well: guarding it would break every fetch through such a proxy.
    let app_config = make_test_app_config(Config {
        proxy: Some(crate::model::ProxyConfig {
            url: "http://localhost:8118".to_string(),
            username: None,
            password: None,
        }),
        ..Config::default()
    });

    assert_eq!(proxy_hosts(&app_config), vec![Arc::from("localhost")]);
}

#[test]
fn resolved_address_sets_are_aggregated_for_exposure() {
    use super::super::{
        verdict_for_addresses, RESOURCE_DESTINATION_PRIVATE_TTL, RESOURCE_DESTINATION_PUBLIC_TTL,
        RESOURCE_DESTINATION_UNANSWERED_TTL,
    };

    let public: IpAddr = "8.8.8.8".parse().expect("valid public address");
    let private: IpAddr = "10.0.0.1".parse().expect("valid private address");
    let blocked: IpAddr = "127.0.0.1".parse().expect("valid loopback address");

    assert_eq!(verdict_for_addresses(&[public]), (ResourceDestination::Public, RESOURCE_DESTINATION_PUBLIC_TTL));
    assert_eq!(
        verdict_for_addresses(&[public, public]),
        (ResourceDestination::Public, RESOURCE_DESTINATION_PUBLIC_TTL)
    );
    assert_eq!(verdict_for_addresses(&[private]), (ResourceDestination::Private, RESOURCE_DESTINATION_PRIVATE_TTL));
    assert_eq!(
        verdict_for_addresses(&[public, private]),
        (ResourceDestination::Private, RESOURCE_DESTINATION_PRIVATE_TTL)
    );
    assert_eq!(
        verdict_for_addresses(&[public, blocked]),
        (ResourceDestination::Blocked, RESOURCE_DESTINATION_PRIVATE_TTL)
    );
    assert_eq!(
        verdict_for_addresses(&[private, blocked]),
        (ResourceDestination::Blocked, RESOURCE_DESTINATION_PRIVATE_TTL)
    );
    // An unanswered name stays non-public, but only briefly: a resolver hiccup must not pin the
    // destination to the proxy for minutes.
    assert_eq!(verdict_for_addresses(&[]), (ResourceDestination::Private, RESOURCE_DESTINATION_UNANSWERED_TTL));
    assert!(RESOURCE_DESTINATION_UNANSWERED_TTL < RESOURCE_DESTINATION_PRIVATE_TTL);
}

#[tokio::test]
async fn atomic_download_temp_files_are_unique_and_use_target_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("cache.ics");
    let (first_temp, first_output) = super::super::create_atomic_download_file(&target).expect("first temp file");
    let (second_temp, second_output) = super::super::create_atomic_download_file(&target).expect("second temp file");

    assert_ne!(first_temp.path(), second_temp.path());
    assert_eq!(first_temp.path().parent(), Some(dir.path()));
    assert_eq!(second_temp.path().parent(), Some(dir.path()));

    drop(first_output);
    drop(second_output);
    drop(first_temp);
    drop(second_temp);
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[test]
fn test_url_mask() {
    // Replace with "***"
    let query = "https://bubblegum.tv/live/username/password/2344";
    let masked = sanitize_sensitive_info(query);
    println!("{masked}");
}

#[test]
fn test_replace_ext() {
    let tests = [
        ("http://hello.world.com", "http://hello.world.com"),
        ("http://hello.world.com/123", "http://hello.world.com/123.mp4"),
        ("http://hello.world.com/123.ts?hello=world", "http://hello.world.com/123.mp4?hello=world"),
        ("http://hello.world.com/123?hello=world", "http://hello.world.com/123.mp4?hello=world"),
        ("http://hello.world.com/123#hello=world", "http://hello.world.com/123.mp4#hello=world"),
    ];

    for (test, expect) in &tests {
        assert_eq!(replace_url_extension(test, ".mp4"), *expect);
    }
}

#[test]
fn tes_base_url() {
    let url = "http://my.provider.com:8080/xmltv?username=hello";
    let expected = "http://my.provider.com:8080";
    assert_eq!(get_base_url_from_str(url).unwrap(), expected);
}

#[test]
fn test_same_origin_checks_scheme_host_and_port() {
    let a = Url::parse("https://example.com/path").expect("url parse should work");
    let b = Url::parse("https://example.com/other").expect("url parse should work");
    let c = Url::parse("http://example.com/other").expect("url parse should work");
    let d = Url::parse("https://example.com:8443/other").expect("url parse should work");

    assert!(same_origin(&a, &b));
    assert!(!same_origin(&a, &c));
    assert!(!same_origin(&a, &d));
}

#[tokio::test]
async fn local_epg_file_respects_max_download_bytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("large.ics");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&source, b"0123456789").await.expect("write source");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    let err = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source.to_string_lossy().as_ref(),
            persist_path: &persist,
            max_bytes: Some(4),
        },
    )
    .await
    .expect_err("size limit should fail");

    assert!(err.to_string().contains("exceeds configured limit"));
    assert_eq!(tokio::fs::read(&persist).await.expect("read existing cache"), b"existing cache");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[tokio::test]
async fn file_url_epg_source_respects_streamed_size_limit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("large.ics");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&source, b"0123456789").await.expect("write source");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let source_url = Url::from_file_path(&source).expect("file url");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    let err = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source_url.as_str(),
            persist_path: &persist,
            max_bytes: Some(4),
        },
    )
    .await
    .expect_err("size limit should fail");

    assert!(err.to_string().contains("exceeds configured limit"));
    assert_eq!(tokio::fs::read(&persist).await.expect("read existing cache"), b"existing cache");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[tokio::test]
async fn local_epg_replace_error_cleans_temp_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("source.ics");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&source, b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").await.expect("write source");
    tokio::fs::create_dir(&persist).await.expect("create conflicting destination");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source.to_string_lossy().as_ref(),
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect_err("replacing a directory should fail");

    assert!(persist.is_dir());
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[tokio::test]
async fn local_epg_read_error_preserves_existing_cache_and_cleans_temp_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("source-directory");
    let persist = dir.path().join("cache.ics");
    tokio::fs::create_dir(&source).await.expect("create source directory");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source.to_string_lossy().as_ref(),
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect_err("reading a directory as an EPG file should fail");

    assert_eq!(tokio::fs::read(&persist).await.expect("read existing cache"), b"existing cache");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[tokio::test]
async fn unsupported_epg_url_scheme_is_rejected_without_network_access() {
    let dir = tempfile::tempdir().expect("temp dir");
    let persist = dir.path().join("cache.ics");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    let err = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: "ftp://example.com/calendar.ics",
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect_err("unsupported scheme should fail");

    assert!(err.to_string().contains("Unsupported EPG URL scheme 'ftp'"));
    assert!(!persist.exists());
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
}

#[cfg(windows)]
#[tokio::test]
async fn absolute_windows_epg_path_is_dispatched_as_local_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = dir.path().join("source.ics");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&source, b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").await.expect("write source");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();
    let source_path = source.to_str().expect("Windows temp path should be UTF-8");
    assert!(source_path.contains(':'));

    get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: source_path,
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect("absolute Windows path should be copied as a local file");

    assert_eq!(tokio::fs::read(&persist).await.expect("read cache"), b"BEGIN:VCALENDAR\nEND:VCALENDAR\n");
}

#[tokio::test]
async fn remote_epg_size_limit_preserves_existing_cache_and_cleans_temp_file() {
    let (addr, _accepted, server_handle) = match start_plain_http_server_with_body(b"0123456789").await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping remote_epg_size_limit_preserves_existing_cache_and_cleans_temp_file: {err}");
            return;
        }
        Err(err) => panic!("failed to start test server: {err}"),
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let url = format!("http://{addr}/calendar.ics");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    let err = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: &url,
            persist_path: &persist,
            max_bytes: Some(4),
        },
    )
    .await
    .expect_err("remote size limit should fail");

    assert!(err.to_string().contains("exceeds configured limit"));
    assert_eq!(tokio::fs::read(&persist).await.expect("read existing cache"), b"existing cache");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
    server_handle.abort();
}

#[tokio::test]
async fn remote_epg_stream_error_preserves_existing_cache_and_cleans_temp_file() {
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\ntruncated".to_string();
    let (addr, _accepted, server_handle) = match start_plain_http_server_with_response(response).await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping remote_epg_stream_error_preserves_existing_cache_and_cleans_temp_file: {err}");
            return;
        }
        Err(err) => panic!("failed to start test server: {err}"),
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let url = format!("http://{addr}/calendar.ics");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();

    get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: dir.path().to_string_lossy().as_ref(),
            url: &url,
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    )
    .await
    .expect_err("truncated response body should fail");

    assert_eq!(tokio::fs::read(&persist).await.expect("read existing cache"), b"existing cache");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
    server_handle.abort();
}

#[tokio::test]
async fn parallel_epg_downloads_to_same_target_are_serialized_and_replace_atomically() {
    let (addr, accepted, max_active, server_handle) =
        match start_delayed_http_server_with_body(b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").await {
            Ok(server) => server,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping parallel_epg_downloads_to_same_target_are_serialized_and_replace_atomically: {err}"
                );
                return;
            }
            Err(err) => panic!("failed to start test server: {err}"),
        };
    let dir = tempfile::tempdir().expect("temp dir");
    let persist = dir.path().join("cache.ics");
    tokio::fs::write(&persist, b"existing cache").await.expect("write existing cache");
    let url = format!("http://{addr}/calendar.ics");
    let app_config = make_test_app_config(Config::default());
    let client = make_epg_test_client();
    let input = ConfigInput::default();
    let storage_dir = dir.path().to_string_lossy();

    let first = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: storage_dir.as_ref(),
            url: &url,
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    );
    let second = get_input_epg_content_as_file(
        &app_config,
        &client,
        &input,
        InputEpgFileRequest {
            headers: None,
            storage_dir: storage_dir.as_ref(),
            url: &url,
            persist_path: &persist,
            max_bytes: Some(1024),
        },
    );
    let (first_result, second_result) = tokio::join!(first, second);

    assert_eq!(first_result.expect("first download"), persist);
    assert_eq!(second_result.expect("second download"), persist);
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
    assert_eq!(tokio::fs::read(&persist).await.expect("read refreshed cache"), b"BEGIN:VCALENDAR\nEND:VCALENDAR\n");
    assert!(atomic_download_temp_files(dir.path()).await.is_empty());
    server_handle.abort();
}

#[test]
fn test_preview_request_diagnostics_for_logging_includes_effective_target_and_host_details() {
    let provider = make_provider_with_dns(true, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1"]);
    let url = Url::parse("http://example.com:8080/stream").expect("url parse should work");

    let diagnostics = preview_request_diagnostics_for_logging(&url, Some(&provider));

    assert_eq!(
        diagnostics,
        "request_url=http://***/stream, effective_url=http://***/stream, host_header=example.com:8080, connect_ip=0.***"
    );
}

#[test]
fn test_preview_request_diagnostics_for_logging_sanitizes_each_stream_url() -> Result<(), url::ParseError> {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1"]);
    let url = Url::parse("http://example.com/live/abcd/efgh/1092671.ts")?;

    let diagnostics = preview_request_diagnostics_for_logging(&url, Some(&provider));

    assert!(!diagnostics.contains("example.com"));
    assert!(!diagnostics.contains("abcd"));
    assert!(!diagnostics.contains("efgh"));
    assert_eq!(
        diagnostics,
        "request_url=http://***/live/***/1092671.ts, effective_url=http://***/live/***/1092671.ts, host_header=0.***, connect_ip=0.***"
    );
    Ok(())
}

#[test]
fn test_http_attempt_uses_bracketed_ipv6_target_for_logging_and_request_url() {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["2a06:98c1:3121::3"]);
    let url = Url::parse("http://example.com/live/stream.ts").expect("url parse should work");

    let preview = preview_request_target_for_logging(&url, Some(&provider));
    let target = resolve_attempt_target(&url, Some(&provider));

    assert_eq!(preview, "http://[2a06:98c1:3121::3]/live/stream.ts");
    assert_eq!(target.effective_url.as_str(), "http://[2a06:98c1:3121::3]/live/stream.ts");
    assert_eq!(target.host_header.as_deref(), Some("[2a06:98c1:3121::3]"));
}

#[test]
fn test_https_attempt_keeps_hostname_and_sets_sni() {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1"]);
    let url = Url::parse("https://example.com/live").expect("url parse should work");

    let target = resolve_attempt_target(&url, Some(&provider));
    assert_eq!(target.effective_url.host_str(), Some("example.com"));
    assert_eq!(target.sni_host.as_deref(), Some("example.com"));
    assert_eq!(target.connect_ip.map(|ip| ip.to_string()), Some("192.168.0.1".to_string()));
    assert_eq!(target.host_header.as_deref(), Some("192.168.0.1"));
}

#[test]
fn test_try_next_ip_policy_uses_next_ip_until_exhausted() {
    let provider = make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "192.168.0.2"]);
    let url = Url::parse("http://example.com/live").expect("url parse should work");
    let mut tried = HashSet::new();

    let first = resolve_attempt_target(&url, Some(&provider));
    let second = resolve_attempt_target(&url, Some(&provider));

    assert!(should_try_next_ip_on_connect_error(Some(&provider), &first, &mut tried));
    assert!(!should_try_next_ip_on_connect_error(Some(&provider), &second, &mut tried));
}

#[test]
fn test_preview_request_target_for_logging_uses_preferred_provider_index() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));
    provider.set_current_index(1);

    let url = Url::parse("provider://provider-a/live").expect("provider url should parse");
    let preview = preview_request_target_for_logging(&url, Some(&provider));

    assert_eq!(preview, "http://provider-b.example/live");
}

pub(in crate::utils::network::request::tests) async fn start_delayed_http_server_with_body(
    body: &'static [u8],
) -> std::io::Result<(SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let accepted_clone = Arc::clone(&accepted);
    let active_clone = Arc::clone(&active);
    let max_active_clone = Arc::clone(&max_active);
    let content_length = body.len();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            accepted_clone.fetch_add(1, Ordering::SeqCst);
            let active = Arc::clone(&active_clone);
            let max_active = Arc::clone(&max_active_clone);
            tokio::spawn(async move {
                let active_count = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(active_count, Ordering::SeqCst);
                let mut buf = vec![0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                let response_head =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(response_head.as_bytes()).await;
                let _ = socket.write_all(body).await;
                let _ = socket.shutdown().await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    Ok((addr, accepted, max_active, handle))
}

pub(in crate::utils::network::request::tests) async fn start_plain_http_server(
) -> std::io::Result<(SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    start_plain_http_server_with_body(b"ok").await
}

#[tokio::test]
async fn public_resolver_rejects_loopback_at_connection_time() {
    let (address, accepted, server) = match start_plain_http_server().await {
        Ok(server) => server,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => return,
        Err(err) => panic!("failed to start test server: {err}"),
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(PublicIpResolver)
        .build()
        .expect("public-only client should build");

    let result = client.get(format!("http://localhost:{}/", address.port())).send().await;

    assert!(result.is_err());
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    server.abort();
}

#[test]
fn content_type_from_ext_maps_fragmented_mp4_extensions_case_insensitively() {
    for (ext, expected) in [
        ("mp4", "video/mp4"),
        ("FMP4", "video/mp4"),
        ("m4s", "video/mp4"),
        ("m4v", "video/mp4"),
        ("cmfv", "video/mp4"),
        ("m4a", "audio/mp4"),
        ("CMFA", "audio/mp4"),
        ("ts", "video/mp2t"),
        ("mkv", "video/x-matroska"),
        ("bin", "application/octet-stream"),
        ("", "application/octet-stream"),
    ] {
        assert_eq!(super::super::content_type_from_ext(ext), expected, "{ext}");
    }
}

#[tokio::test]
async fn http_error_response_option_recovers_transient_statuses_in_both_request_paths(
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    for manual_redirects in [false, true] {
        for status in ["503 Service Unavailable", "429 Too Many Requests", "408 Request Timeout"] {
            let (addr, requests, server) = start_recording_http_server(vec![
                response_with_body(status, ""),
                response_with_body("200 OK", "media"),
            ])
            .await?;
            let url = Url::parse(&format!("http://{addr}/init.hls.fmp4"))?;
            let app_config = test_retry_config(2);
            let options = RequestFetchOptions::default().with_http_error_responses(true);
            let response = if manual_redirects {
                send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
                    &app_config,
                    &client,
                    &test_input_source(url.to_string(), None),
                    None,
                    &url,
                    10,
                    options,
                )
                .await?
                .response
            } else {
                super::super::send_with_retry_and_provider_policy_with_options(
                    &app_config,
                    &url,
                    None,
                    false,
                    true,
                    options,
                    |resolved| client.get(resolved.clone()),
                )
                .await?
            };
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.bytes().await?.as_ref(), b"media");
            assert_eq!(requests.lock().await.len(), 2);
            server.abort();
        }
    }
    Ok(())
}

#[test]
fn text_response_error_log_labels_never_expose_origin_controlled_details() {
    let unsupported = Error::other(ContentCodingError::Unsupported("signed-token-secret".to_string()));
    assert_eq!(text_response_error_log_label(&unsupported), "content_coding class=unsupported");
    assert!(!text_response_error_log_label(&unsupported).contains("signed-token-secret"));

    let decoding = Error::new(ErrorKind::InvalidData, ContentDecodingIoError { coding: ContentCoding::Zstd });
    assert_eq!(text_response_error_log_label(&decoding), "content_decoding coding=zstd");
}

#[tokio::test]
async fn text_content_manifest_uses_one_budget_for_status_decoder_and_success() {
    let corrupt_gzip =
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
    let responses = vec![
        response_with_body("503 Service Unavailable", ""),
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

    let (manifest, _, _) = download_text_content_with_headers_and_options(
        &test_retry_config(3),
        &make_epg_test_client(),
        &input,
        None,
        false,
        options,
    )
    .await
    .expect("third logical attempt should succeed");

    assert_eq!(manifest, "#EXTM3U\nsegment.ts\n");
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 3);
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}

#[tokio::test]
async fn test_provider_request_chain_starts_from_last_successful_url() {
    let (addr_b, accepted_b, handle_b) = match start_plain_http_server_with_body(b"b").await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping test_provider_request_chain_starts_from_last_successful_url: {err}");
            return;
        }
        Err(err) => panic!("failed to start test http server: {err}"),
    };

    let mut cfg = Config { connect_timeout_secs: 1, ..Config::default() };
    cfg.accept_insecure_ssl_certificates = true;
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
        .connect_timeout(Duration::from_millis(400))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client should build");
    let dead_addr = SocketAddr::from(([127, 0, 0, 1], 1));
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec![
            format!("http://127.0.0.1:{}", dead_addr.port()).into(),
            format!("http://127.0.0.1:{}", addr_b.port()).into(),
        ],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));

    let url = Url::parse("provider://provider-a/live").expect("provider url should parse");
    let first_response = send_with_retry_and_provider(&app_config, &url, Some(&provider), false, |resolved_url| {
        client.get(resolved_url.clone())
    })
    .await
    .expect("request should fail over to the second provider url");
    let first_body = first_response.text().await.expect("response body should be readable");

    assert_eq!(first_body, "b");
    assert_eq!(provider.get_current_index(), 1);

    let second_response = send_with_retry_and_provider(&app_config, &url, Some(&provider), false, |resolved_url| {
        client.get(resolved_url.clone())
    })
    .await
    .expect("next request should start from the last successful provider url");
    let second_body = second_response.text().await.expect("response body should be readable");

    assert_eq!(second_body, "b");
    assert_eq!(accepted_b.load(Ordering::SeqCst), 2);

    handle_b.abort();
}

#[tokio::test]
async fn test_provider_request_chain_restarts_from_first_url_when_policy_requests_it() {
    let (addr_b, accepted_b, handle_b) = match start_plain_http_server_with_body(b"b").await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping test_provider_request_chain_restarts_from_first_url_when_policy_requests_it: {err}");
            return;
        }
        Err(err) => panic!("failed to start test http server: {err}"),
    };

    let mut cfg = Config { connect_timeout_secs: 1, ..Config::default() };
    cfg.accept_insecure_ssl_certificates = true;
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
        .connect_timeout(Duration::from_millis(400))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client should build");
    let dead_addr = SocketAddr::from(([127, 0, 0, 1], 1));
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec![
            format!("http://127.0.0.1:{}", dead_addr.port()).into(),
            format!("http://127.0.0.1:{}", addr_b.port()).into(),
        ],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));

    let url = Url::parse("provider://provider-a/live").expect("provider url should parse");
    let first_response = send_with_retry_and_provider(&app_config, &url, Some(&provider), false, |resolved_url| {
        client.get(resolved_url.clone())
    })
    .await
    .expect("request should fail over to the second provider url");
    let first_body = first_response.text().await.expect("response body should be readable");

    assert_eq!(first_body, "b");
    assert_eq!(provider.get_current_index(), 1);

    let second_response = send_with_retry_and_provider(&app_config, &url, Some(&provider), false, |resolved_url| {
        client.get(resolved_url.clone())
    })
    .await
    .expect("next request should restart from the first provider url and fail over again");
    let second_body = second_response.text().await.expect("response body should be readable");

    assert_eq!(second_body, "b");
    assert_eq!(provider.get_current_index(), 1);
    assert_eq!(accepted_b.load(Ordering::SeqCst), 2);

    handle_b.abort();
}

#[test]
fn test_next_provider_url_index_wraps_once_then_stops() {
    assert_eq!(next_provider_url_index(2, 4, 2), Some(3));
    assert_eq!(next_provider_url_index(3, 4, 2), Some(0));
    assert_eq!(next_provider_url_index(0, 4, 2), Some(1));
    assert_eq!(next_provider_url_index(1, 4, 2), None);
    assert_eq!(next_provider_url_index(0, 1, 0), None);
}

#[tokio::test]
async fn test_on_connect_error_try_next_ip_before_provider_rotation() {
    let (addr, accepted, server_handle) = match start_plain_http_server().await {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping test_on_connect_error_try_next_ip_before_provider_rotation: {err}");
            return;
        }
        Err(err) => panic!("failed to start test http server: {err}"),
    };

    let mut cfg = Config { connect_timeout_secs: 1, ..Config::default() };
    cfg.accept_insecure_ssl_certificates = true;
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
        .connect_timeout(Duration::from_millis(400))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client should build");
    let url = Url::parse(format!("http://example.com:{}/ok", addr.port()).as_str()).expect("url parse should work");

    let provider_rotate =
        make_provider_with_dns(false, OnConnectErrorPolicy::RotateProviderUrl, vec!["192.168.0.1", "127.0.0.1"]);
    let result_rotate =
        send_with_retry_and_provider(&app_config, &url, Some(&provider_rotate), false, |resolved_url| {
            client.get(resolved_url.clone())
        })
        .await;
    assert!(result_rotate.is_err(), "without try_next_ip policy the request should fail");

    let provider_try_next =
        make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "127.0.0.1"]);
    let result_try_next =
        send_with_retry_and_provider(&app_config, &url, Some(&provider_try_next), false, |resolved_url| {
            client.get(resolved_url.clone())
        })
        .await;
    assert!(result_try_next.is_ok(), "try_next_ip should succeed by trying the second IP");
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "server should be reached exactly once");

    server_handle.abort();
}
