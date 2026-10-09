use super::RequestFetchOptions;
use crate::{
    model::{
        AppConfig, Config, ConfigProvider, InputSource, MediaToolCapabilities, ResourceRetryConfig, ReverseProxyConfig,
        SourcesConfig,
    },
    utils::{content_coding::OutboundContentCodingPolicy, FileLockManager},
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{
    ConfigPaths, ConfigProviderDto, DnsScheme, InputFetchMethod, OnConnectErrorPolicy, ProviderDnsDto,
    ProviderUrlSelectionPolicy,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

pub(in crate::utils::network::request::tests) fn make_test_app_config(config: Config) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    })
}

pub(in crate::utils::network::request::tests) fn make_epg_test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .expect("test client")
}

pub(in crate::utils::network::request::tests) async fn atomic_download_temp_files(directory: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut entries = tokio::fs::read_dir(directory).await.expect("read temp directory");
    while let Some(entry) = entries.next_entry().await.expect("read temp directory entry") {
        if entry.file_name().to_string_lossy().starts_with(".tuliprox-download-") {
            result.push(entry.path());
        }
    }
    result
}

pub(in crate::utils::network::request::tests) fn make_provider_with_dns(
    keep_vhost: bool,
    on_connect_error: OnConnectErrorPolicy,
    ips: Vec<&str>,
) -> Arc<ConfigProvider> {
    let parsed_ips = ips.into_iter().map(|raw| raw.parse().expect("ip must parse")).collect::<Vec<_>>();
    let dto = ConfigProviderDto {
        name: "provider-a".into(),
        urls: vec!["http://example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: Some(ProviderDnsDto {
            enabled: true,
            schemes: Some(vec![DnsScheme::Http, DnsScheme::Https]),
            keep_vhost,
            overrides: Some(HashMap::from([("example.com".to_string(), parsed_ips)])),
            on_connect_error,
            ..ProviderDnsDto::default()
        }),
    };
    Arc::new(ConfigProvider::from(&dto))
}

pub(in crate::utils::network::request::tests) async fn start_plain_http_server_with_body(
    body: &'static [u8],
) -> std::io::Result<(SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_clone = Arc::clone(&accepted);
    let content_length = body.len();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            accepted_clone.fetch_add(1, Ordering::SeqCst);
            let body = body;
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response_head =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(response_head.as_bytes()).await;
                let _ = socket.write_all(body).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    Ok((addr, accepted, handle))
}

pub(in crate::utils::network::request::tests) async fn start_plain_http_server_with_response(
    response: String,
) -> std::io::Result<(SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_clone = Arc::clone(&accepted);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            accepted_clone.fetch_add(1, Ordering::SeqCst);
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    Ok((addr, accepted, handle))
}

pub(in crate::utils::network::request::tests) async fn start_recording_http_server(
    responses: Vec<String>,
) -> std::io::Result<(SocketAddr, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>)> {
    start_recording_http_byte_server(responses.into_iter().map(String::into_bytes).collect()).await
}

pub(in crate::utils::network::request::tests) async fn start_recording_http_byte_server(
    responses: Vec<Vec<u8>>,
) -> std::io::Result<(SocketAddr, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let responses = Arc::new(responses);
    let response_index = Arc::new(AtomicUsize::new(0));

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let responses = Arc::clone(&responses);
            let response_index = Arc::clone(&response_index);
            tokio::spawn(async move {
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 2048];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") || request.len() >= 16 * 1024 {
                        break;
                    }
                }
                requests.lock().await.push(String::from_utf8_lossy(&request).into_owned());
                let index = response_index.fetch_add(1, Ordering::SeqCst);
                let Some(response) = responses.get(index).or_else(|| responses.last()) else {
                    return;
                };
                let _ = socket.write_all(response).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    Ok((addr, requests, handle))
}

pub(in crate::utils::network::request::tests) fn response_with_body(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

pub(in crate::utils::network::request::tests) fn request_header_value<'a>(
    request: &'a str,
    expected_name: &str,
) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected_name).then_some(value.trim())
    })
}

pub(in crate::utils::network::request::tests) fn test_retry_config(max_attempts: u32) -> Arc<AppConfig> {
    let mut config = Config { connect_timeout_secs: 1, ..Config::default() };
    config.reverse_proxy = Some(ReverseProxyConfig {
        resource_rewrite_disabled: false,
        rewrite_secret: [0; 16],
        resource_retry: ResourceRetryConfig {
            max_attempts,
            backoff_millis: 1,
            backoff_multiplier: 1.0,
            ..ResourceRetryConfig::default()
        },
        disabled_header: None,
        stream: None,
        cache: None,
        rate_limit: None,
        geoip: None,
        stream_history: None,
        qos_aggregation: None,
        hls_cache: None,
    });
    make_test_app_config(config)
}

pub(in crate::utils::network::request::tests) fn identity_fetch_options() -> RequestFetchOptions {
    RequestFetchOptions::with_attempt_idle_timeout(Duration::from_secs(1))
        .with_content_coding(OutboundContentCodingPolicy::Identity)
}

pub(in crate::utils::network::request::tests) fn test_input_source(
    url: String,
    provider: Option<Arc<ConfigProvider>>,
) -> InputSource {
    InputSource {
        name: Arc::from("test"),
        url,
        provider,
        username: None,
        password: None,
        method: InputFetchMethod::GET,
        headers: HashMap::new(),
    }
}
