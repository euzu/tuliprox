use super::{
    error::{failed_stream_account, should_reject_success_response_content_type, ProviderStreamPreparationError},
    options::{should_wrap_provider_stream_in_buffer, ProviderRequestCredentialState},
    *,
};
use crate::{
    api::model::{BoxedProviderStream, ProviderStreamFactoryResponse, StreamError},
    model::{AppConfig, Config, MediaToolCapabilities, SourcesConfig},
    utils::{content_coding::ContentCodingError, FileLockManager},
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::TryStreamExt;
use reqwest::StatusCode;
use shared::{
    model::{ConfigPaths, PlaylistItemType, StreamChannel, XtreamCluster},
    utils::Internable,
};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tuliprox_session::{
    response_headers::ProviderResponseHeaderError,
    stream_options::{StreamOptions, StreamResponseMode},
};

struct RedirectRoundTrip {
    entry_url: Url,
    first_origin_task: tokio::task::JoinHandle<Vec<String>>,
    second_origin_task: tokio::task::JoinHandle<String>,
}

fn test_app_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
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

async fn read_http_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0_u8; 1024];
        let read = socket.read(&mut chunk).await.expect("test origin reads request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&request).into_owned()
}

async fn spawn_redirect_round_trip(encoded_body: Vec<u8>) -> RedirectRoundTrip {
    let first_listener = TcpListener::bind("127.0.0.1:0").await.expect("first test origin binds");
    let first_addr = first_listener.local_addr().expect("first test origin address");
    let second_listener = TcpListener::bind("127.0.0.1:0").await.expect("second test origin binds");
    let second_addr = second_listener.local_addr().expect("second test origin address");

    let first_origin_task = tokio::spawn(async move {
        let (mut first_socket, _) = first_listener.accept().await.expect("first origin accepts entry request");
        let entry_request = read_http_request(&mut first_socket).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            second_addr.port()
        );
        first_socket.write_all(redirect.as_bytes()).await.expect("first origin redirects to second origin");

        let (mut final_socket, _) = first_listener.accept().await.expect("first origin accepts final request");
        let final_request = read_http_request(&mut final_socket).await;
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                encoded_body.len()
            );
        final_socket.write_all(response.as_bytes()).await.expect("first origin writes final response head");
        final_socket.write_all(&encoded_body).await.expect("first origin writes final response body");
        vec![entry_request, final_request]
    });

    let second_origin_task = tokio::spawn(async move {
        let (mut socket, _) = second_listener.accept().await.expect("second origin accepts redirect request");
        let request = read_http_request(&mut socket).await;
        let redirect = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{first_addr}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        socket.write_all(redirect.as_bytes()).await.expect("second origin redirects to first origin");
        request
    });

    RedirectRoundTrip {
        entry_url: Url::parse(&format!("http://{first_addr}/entry")).expect("entry URL"),
        first_origin_task,
        second_origin_task,
    }
}

fn redirect_test_options(stream_url: &Url) -> ProviderStreamFactoryOptions {
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::ACCEPT_ENCODING, "br".parse().expect("Accept-Encoding"));
    req_headers.insert(reqwest::header::AUTHORIZATION, "Bearer client-secret".parse().expect("Authorization"));
    req_headers.insert(reqwest::header::COOKIE, "session=client-secret".parse().expect("Cookie"));
    test_options(ProviderContentRepresentationMode::Identity, stream_url, &req_headers, None, None)
}

fn assert_redirect_request_headers(request: &str, credentials_expected: bool) {
    let request = request.to_ascii_lowercase();
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
    assert_eq!(request.contains("\r\nauthorization: bearer client-secret\r\n"), credentials_expected);
    assert_eq!(request.contains("\r\ncookie: session=client-secret\r\n"), credentials_expected);
}

async fn assert_identity_redirect_result(
    response: reqwest::Response,
    first_origin_task: tokio::task::JoinHandle<Vec<String>>,
    second_origin_task: tokio::task::JoinHandle<String>,
) {
    const IDENTITY_BODY: &[u8] = b"decoded redirect body";

    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .expect("redirect response is prepared");
    assert_eq!(collect_stream(stream).await.expect("redirect body decodes"), IDENTITY_BODY);
    let (headers, status, _, _) = info.expect("redirect response metadata");
    assert_eq!(status, StatusCode::OK);
    assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-encoding")));

    let first_requests = tokio::time::timeout(Duration::from_secs(2), first_origin_task)
        .await
        .expect("first origin finishes")
        .expect("first origin task succeeds");
    let second_request = tokio::time::timeout(Duration::from_secs(2), second_origin_task)
        .await
        .expect("second origin finishes")
        .expect("second origin task succeeds");
    assert_eq!(first_requests.len(), 2);
    assert_redirect_request_headers(&first_requests[0], true);
    assert_redirect_request_headers(&second_request, false);
    assert_redirect_request_headers(&first_requests[1], false);
}

fn test_options(
    mode: ProviderContentRepresentationMode,
    stream_url: &Url,
    req_headers: &HeaderMap,
    input_headers: Option<&HashMap<String, String>>,
    session_headers: Option<&HashMap<String, String>>,
) -> ProviderStreamFactoryOptions {
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };
    ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().unwrap(),
        item_type: PlaylistItemType::Catchup,
        share_stream: false,
        stream_options: &stream_options,
        stream_url,
        req_headers,
        input_headers,
        session_headers,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: mode,
    })
}

async fn local_response(
    status: StatusCode,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (reqwest::Response, Arc<AtomicUsize>) {
    use std::fmt::Write as _;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let task_requests = Arc::clone(&requests);
    let response_headers = headers.iter().fold(String::new(), |mut headers, (name, value)| {
        let _ = write!(headers, "{name}: {value}\r\n");
        headers
    });
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        task_requests.fetch_add(1, Ordering::SeqCst);
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let reason = status.canonical_reason().unwrap_or("Status");
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n{response_headers}Connection: close\r\n\r\n",
            status.as_u16(),
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
    });

    let response = reqwest::Client::new().get(format!("http://{addr}/resource")).send().await.unwrap();
    (response, requests)
}

async fn local_stalled_deflate_response() -> (reqwest::Response, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: deflate\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let response = reqwest::Client::new().get(format!("http://{addr}/resource")).send().await.unwrap();
    (response, task)
}

#[derive(Clone, Copy)]
enum TestEncoding {
    Gzip,
    Zlib,
    RawDeflate,
    Brotli,
    Zstd,
}

async fn encode(body: &[u8], encoding: TestEncoding) -> Vec<u8> {
    match encoding {
        TestEncoding::Gzip => {
            let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
            encoder.write_all(body).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        TestEncoding::Zlib => {
            let mut encoder = async_compression::tokio::write::ZlibEncoder::new(Vec::new());
            encoder.write_all(body).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        TestEncoding::RawDeflate => {
            let mut encoder = async_compression::tokio::write::DeflateEncoder::new(Vec::new());
            encoder.write_all(body).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        TestEncoding::Brotli => {
            let mut encoder = async_compression::tokio::write::BrotliEncoder::new(Vec::new());
            encoder.write_all(body).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
        TestEncoding::Zstd => {
            let mut encoder = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
            encoder.write_all(body).await.unwrap();
            encoder.shutdown().await.unwrap();
            encoder.into_inner()
        }
    }
}

async fn collect_stream(stream: BoxedProviderStream) -> Result<Vec<u8>, StreamError> {
    stream
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
}

mod http;
mod lifecycle;
mod policy;
mod recovery;
mod storage;
mod streaming;
mod transport;
