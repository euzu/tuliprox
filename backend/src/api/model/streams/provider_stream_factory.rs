use crate::api::api_utils::{get_headers_from_request, StreamOptions};
use crate::api::model::model_utils::get_response_headers;
use crate::api::model::stream::{BoxedProviderStream, ProviderStreamFactoryResponse};
use crate::api::model::stream_error::StreamError;
use crate::api::model::streams::buffered_stream::BufferedStream;
use crate::api::model::streams::client_stream::ClientStream;
use crate::api::model::streams::provider_stream::{create_channel_unavailable_stream, get_header_filter_for_item_type};
use crate::api::model::streams::timed_client_stream::TimedClientStream;
use shared::model::{PlaylistItemType, DEFAULT_USER_AGENT};
use crate::model::{AppConfig};
use crate::tools::atomic_once_flag::AtomicOnceFlag;
use crate::utils::request::{classify_content_type, get_request_headers, MimeCategory};
use crate::utils::{debug_if_enabled};
use shared::utils::{filter_request_header, sanitize_sensitive_info};
use futures::stream::{self};
use futures::{StreamExt, TryStreamExt};
use log::{debug, log_enabled, warn};
use reqwest::header::{HeaderMap, RANGE};
use reqwest::StatusCode;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

// TODO make this configurable
pub const STREAM_QUEUE_SIZE: usize = 4096; // mpsc channel holding messages. with possible 8192byte chunks
const RETRY_SECONDS: u64 = 5;
const ERR_MAX_RETRY_COUNT: u32 = 5;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ProviderStreamFactoryOptions {
    // item_type: PlaylistItemType,
    reconnect_enabled: bool,
    force_reconnect_secs: u32,
    buffer_enabled: bool,
    buffer_size: usize,
    share_stream: bool,
    pipe_stream: bool,
    url: Url,
    headers: HeaderMap,
    range_bytes: Arc<Option<AtomicUsize>>,
    reconnect_flag: Arc<AtomicOnceFlag>,
}

impl ProviderStreamFactoryOptions {
    pub(crate) fn new(
        item_type: PlaylistItemType,
        share_stream: bool,
        stream_options: &StreamOptions,
        stream_url: &Url,
        req_headers: &HeaderMap,
        input_headers: Option<&HashMap<String, String>>,
    ) -> Self {
        let buffer_size = if stream_options.buffer_enabled { stream_options.buffer_size } else { STREAM_QUEUE_SIZE };
        let filter_header = get_header_filter_for_item_type(item_type);
        let mut req_headers = get_headers_from_request(req_headers, &filter_header);
        // we need the range bytes from client request for seek ing to the right position
        let range_start_bytes = get_request_range_start_bytes(&req_headers);
        req_headers.remove("range");

        // We merge configured input headers with the headers from the request.
        let headers = get_request_headers(input_headers, Some(&req_headers));

        let url = stream_url.clone();
        let range_bytes = Arc::new(range_start_bytes.map(AtomicUsize::new));

        Self {
            // item_type,
            reconnect_enabled: stream_options.stream_retry,
            force_reconnect_secs: stream_options.stream_force_retry_secs,
            pipe_stream: stream_options.pipe_provider_stream,
            buffer_enabled: stream_options.buffer_enabled,
            buffer_size,
            share_stream,
            reconnect_flag: Arc::new(AtomicOnceFlag::new()),
            url,
            headers,
            range_bytes,
        }
    }

    #[inline]
    fn is_piped(&self) -> bool {
        self.pipe_stream
    }

    #[inline]
    fn is_buffer_enabled(&self) -> bool {
        self.buffer_enabled
    }

    #[inline]
    fn is_shared_stream(&self) -> bool {
        self.share_stream
    }

    #[inline]
    pub(crate) fn get_buffer_size(&self) -> usize {
        self.buffer_size
    }

    #[inline]
    pub fn get_reconnect_flag_clone(&self) -> Arc<AtomicOnceFlag> {
        Arc::clone(&self.reconnect_flag)
    }

    #[inline]
    pub fn cancel_reconnect(&self) {
        self.reconnect_flag.notify();
    }

    #[inline]
    pub fn get_url(&self) -> &Url {
        &self.url
    }

    #[inline]
    pub fn get_url_as_str(&self) -> &str {
        self.url.as_str()
    }

    #[inline]
    pub fn should_reconnect(&self) -> bool {
        self.reconnect_enabled
    }

    #[inline]
    pub fn get_headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    pub fn get_total_bytes_send(&self) -> Option<usize> {
        self.range_bytes.as_ref().as_ref().map(|atomic| atomic.load(Ordering::SeqCst))
    }

    // pub fn get_range_bytes(&self) -> &Arc<Option<AtomicUsize>> {
    //     &self.range_bytes
    // }

    #[inline]
    pub fn get_range_bytes_clone(&self) -> Arc<Option<AtomicUsize>> {
        Arc::clone(&self.range_bytes)
    }

    #[inline]
    pub fn should_continue(&self) -> bool {
        self.reconnect_flag.is_active()
    }

    #[inline]
    pub fn get_reconnect_force_secs(&self) -> u32 {
        self.force_reconnect_secs
    }
}

fn get_request_range_start_bytes(req_headers: &HashMap<String, Vec<u8>>) -> Option<usize> {
    // range header looks like  bytes=1234-5566/2345345 or bytes=0-
    if let Some(req_range) = req_headers.get(axum::http::header::RANGE.as_str()) {
        if let Some(bytes_range) = req_range.strip_prefix(b"bytes=") {
            if let Some(index) = bytes_range.iter().position(|&x| x == b'-') {
                let start_bytes = &bytes_range[..index];
                if let Ok(start_str) = std::str::from_utf8(start_bytes) {
                    if let Ok(bytes_requested) = start_str.parse::<usize>() {
                        return Some(bytes_requested);
                    }
                }
            }
        }
    }
    None
}

// fn get_host_and_optional_port(url: &Url) -> Option<String> {
//     let host = url.host_str()?;
//     match url.port() {
//         Some(port) => Some(format!("{host}:{port}")),
//         None => Some(host.to_string()),
//     }
// }


fn prepare_client(request_client: &Arc<reqwest::Client>, stream_options: &ProviderStreamFactoryOptions) -> (reqwest::RequestBuilder, bool) {
    let url = stream_options.get_url();
    let range_start = stream_options.get_total_bytes_send();
    let original_headers = stream_options.get_headers();

    if log_enabled!(log::Level::Debug) {
        let message = format!("original_headers {original_headers:?}");
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let mut headers = HeaderMap::default();

    for (key, value) in original_headers {
        if filter_request_header(key.as_str()) {
            headers.insert(key.clone(), value.clone());
        }
    }

    // if !headers.contains_key(axum::http::header::HOST) {
    //     if let Some(host_header) = get_host_and_optional_port(url) {
    //         if let Ok(header_value) = axum::http::header::HeaderValue::from_str(&host_header) {
    //             headers.insert(axum::http::header::HOST, header_value);
    //         }
    //     }
    // }

    if !headers.contains_key(axum::http::header::CONNECTION) {
        headers.insert(axum::http::header::CONNECTION, axum::http::header::HeaderValue::from_static("keep-alive"));
    }

    if !headers.contains_key(axum::http::header::USER_AGENT) {
        headers.insert(axum::http::header::USER_AGENT, axum::http::header::HeaderValue::from_static(DEFAULT_USER_AGENT));
    }

    let partial = if let Some(range) = range_start {
        let range_header = format!("bytes={range}-");
        if let Ok(header_value) = axum::http::header::HeaderValue::from_str(&range_header) {
            headers.insert(RANGE, header_value);
        }
        true
    } else {
        false
    };

    if log_enabled!(log::Level::Debug) {
        let message = format!("Stream requested with headers: {:?}", headers.iter().map(|header| (header.0, String::from_utf8_lossy(header.1.as_ref()))).collect::<Vec<_>>());
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let request_builder = request_client.get(url.clone()).headers(headers);

    (request_builder, partial)
}

async fn provider_stream_request(cfg: &AppConfig, request_client: Arc<reqwest::Client>, stream_options: &ProviderStreamFactoryOptions) -> Result<Option<ProviderStreamFactoryResponse>, StatusCode> {
    let (client, _partial_content) = prepare_client(&request_client, stream_options);
    match client.send().await {
        Ok(mut response) => {
            let status = response.status();
            if status.is_success() {
                let response_info = {
                    // Unfortunately, the HEAD request does not work, so we need this workaround.
                    // We need some header information from the provider, we extract the necessary headers and forward them to the client
                    if log_enabled!(log::Level::Debug) {
                        let message = format!("Provider response  status: '{}' headers: {:?}", response.status(), response.headers_mut());
                        debug!("{}", sanitize_sensitive_info(&message));
                    }

                    let response_headers: Vec<(String, String)> = get_response_headers(response.headers());
                    //let url = stream_options.get_url();
                    // debug!("First  headers {headers:?} {} {}", sanitize_sensitive_info(url.as_str()));
                    Some((response_headers, response.status(), Some(response.url().clone())))
                };

                let provider_stream = response.bytes_stream().map_err(|err| {
                    // error!("Stream error {err}");
                    StreamError::reqwest(&err)
                }).boxed();
                let boxed_provider_stream = if stream_options.get_reconnect_force_secs() > 0 {
                    TimedClientStream::new(provider_stream, stream_options.get_reconnect_force_secs()).boxed()
                } else {
                    provider_stream
                };
                return Ok(Some((boxed_provider_stream, response_info)));
            }

            if status.is_client_error() {
                debug!("Client error status response : {status}");
                return match status {
                    StatusCode::NOT_FOUND
                    | StatusCode::FORBIDDEN
                    | StatusCode::UNAUTHORIZED
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::BAD_REQUEST => {
                        if let (Some(boxed_provider_stream), response_info) =
                            create_channel_unavailable_stream(cfg, &get_response_headers(stream_options.get_headers()), StatusCode::BAD_GATEWAY)
                        {
                            Ok(Some((boxed_provider_stream, response_info)))
                        } else {
                            Err(StatusCode::SERVICE_UNAVAILABLE)
                        }
                    }
                    _ => Err(status)
                };
            }
            if status.is_server_error() {
                debug!("Server error status response : {status}");
                return match status {
                    StatusCode::INTERNAL_SERVER_ERROR |
                    StatusCode::BAD_GATEWAY |
                    StatusCode::SERVICE_UNAVAILABLE |
                    StatusCode::GATEWAY_TIMEOUT => {
                        if let (Some(boxed_provider_stream), response_info) =
                            create_channel_unavailable_stream(cfg, &get_response_headers(stream_options.get_headers()), StatusCode::BAD_GATEWAY)
                        {
                            Ok(Some((boxed_provider_stream, response_info)))
                        } else {
                            Err(StatusCode::SERVICE_UNAVAILABLE)
                        }
                    }
                    _ => Err(status)
                }
            }
            Err(status)
        }
        Err(_err) => {
            if let (Some(boxed_provider_stream), response_info) =
                create_channel_unavailable_stream(cfg, &get_response_headers(stream_options.get_headers()), StatusCode::BAD_GATEWAY)
            {
                Ok(Some((boxed_provider_stream, response_info)))
            } else {
                Err(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
    }
}

async fn get_provider_stream(cfg: &AppConfig, client: Arc<reqwest::Client>, stream_options: &ProviderStreamFactoryOptions) -> Result<Option<ProviderStreamFactoryResponse>, StatusCode> {
    let url = stream_options.get_url();
    debug_if_enabled!("stream provider {}", sanitize_sensitive_info(url.as_str()));
    let start = Instant::now();
    let mut connect_err: u32 = 1;

    while stream_options.should_continue() {
        match provider_stream_request(cfg, Arc::clone(&client), stream_options).await {
            Ok(Some(stream_response)) => {
                return Ok(Some(stream_response));
            }
            Ok(None) => {
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!("The stream could be unavailable. {}", sanitize_sensitive_info(stream_options.get_url().as_str()));
                }
            }
            Err(status) => {
                debug!("Provider stream response error status response : {status}");
                if status == StatusCode::FORBIDDEN || status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::UNAUTHORIZED {
                    warn!("The stream could be unavailable. ({status}) {}", sanitize_sensitive_info(stream_options.get_url().as_str()));
                    stream_options.cancel_reconnect();
                    return Err(status);
                }
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!("The stream could be unavailable. ({status}) {}", sanitize_sensitive_info(stream_options.get_url().as_str()));
                }
            }
        }
        if !stream_options.should_continue() {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        if connect_err > ERR_MAX_RETRY_COUNT {
            break;
        }
        if start.elapsed().as_secs() > RETRY_SECONDS {
            warn!("The stream could be unavailable. Giving up after {RETRY_SECONDS} seconds. {}", sanitize_sensitive_info(stream_options.get_url().as_str()));
            break;
        }
        connect_err += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
        debug_if_enabled!("Reconnecting stream {}", sanitize_sensitive_info(url.as_str()));
    }
    debug_if_enabled!("Stopped reconnecting stream {}", sanitize_sensitive_info(url.as_str()));
    stream_options.cancel_reconnect();
    Err(StatusCode::SERVICE_UNAVAILABLE)
}


pub async fn create_provider_stream(cfg: Arc<AppConfig>,
                                    client: Arc<reqwest::Client>,
                                    stream_options: ProviderStreamFactoryOptions) -> Option<ProviderStreamFactoryResponse> {
    let client_stream_factory = |stream, reconnect_flag, range_cnt| {
        let stream = if !stream_options.is_piped() && stream_options.is_buffer_enabled() && !stream_options.is_shared_stream() {
            BufferedStream::new(stream, stream_options.get_buffer_size(), stream_options.get_reconnect_flag_clone(), stream_options.get_url_as_str()).boxed()
        } else {
            stream
        };
        ClientStream::new(stream, reconnect_flag, range_cnt, stream_options.get_url_as_str()).boxed()
    };

    match get_provider_stream(&cfg, Arc::clone(&client), &stream_options).await {
        Ok(Some((init_stream, info))) => {
            let is_media_stream_or_not_piped = if let Some((headers, _, _)) = &info {
                // if it is piped or no video stream, then we don't reconnect
                !stream_options.pipe_stream && classify_content_type(headers) == MimeCategory::Video
            } else {
                !stream_options.pipe_stream // don't know what it is but lets assume it is something
            };

            let continue_signal = stream_options.get_reconnect_flag_clone();
            if is_media_stream_or_not_piped && stream_options.should_reconnect() {
                let continue_client_signal = Arc::clone(&continue_signal);
                let continue_streaming_signal = continue_client_signal.clone();
                let stream_options_provider = stream_options.clone();
                let config = Arc::clone(&cfg);
                let unfold: BoxedProviderStream = stream::unfold((), move |()| {
                    let client = Arc::clone(&client);
                    let stream_opts = stream_options_provider.clone();
                    let continue_streaming = continue_streaming_signal.clone();
                    let config_clone = Arc::clone(&config);
                    async move {
                        if continue_streaming.is_active() {
                            match get_provider_stream(&config_clone, client, &stream_opts).await {
                                Ok(Some((stream, _info))) => Some((stream, ())),
                                Ok(None) => None,
                                Err(status) => {
                                    if let (Some(boxed_provider_stream), _response_info) =
                                        create_channel_unavailable_stream(&config_clone, &get_response_headers(stream_opts.get_headers()), status)
                                    {
                                        return Some((boxed_provider_stream, ()));
                                    }
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                }).flatten().boxed();
                Some((client_stream_factory(init_stream.chain(unfold).boxed(), Arc::clone(&continue_client_signal), stream_options.get_range_bytes_clone()).boxed(), info))
            } else {
                Some((client_stream_factory(init_stream.boxed(), Arc::clone(&continue_signal), stream_options.get_range_bytes_clone()).boxed(), info))
            }
        }
        Ok(None) => {
            None
        }
        Err(status) => {
            if let (Some(boxed_provider_stream), response_info) =
                create_channel_unavailable_stream(&cfg, &get_response_headers(stream_options.get_headers()), status)
            {
                return Some((boxed_provider_stream, response_info));
            }
            None
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::api::model::streams::provider_stream_factory::PlaylistItemType;
//     use crate::api::model::streams::provider_stream_factory::{create_provider_stream, BufferStreamOptions};
//     use actix_web::test;
//     use actix_web::test::TestRequest;
//     use actix_web::web;
//     use actix_web::App;
//     use actix_web::{HttpRequest, HttpResponse};
//     use futures::StreamExt;
//     use std::sync::Arc;
//     use crate::model::Config;
//
//     #[tokio::test]
//     async fn test_stream() {
//         let app = App::new().route("/test", web::get().to(test_stream_handler));
//         let server = test::init_service(app).await;
//         let req = TestRequest::get().uri("/test").to_request();
//         let _response = test::call_service(&server, req).await;
//     }
//     async fn test_stream_handler(req: axum::http::Request<axum::body::Body>) ->  impl axum::response::IntoResponse + Send {
//         let cfg = Config::default();
//         let mut counter = 5;
//         let client = Arc::new(reqwest::Client::new());
//         let url = url::Url::parse("https://info.cern.ch/hypertext/WWW/TheProject.html").unwrap();
//         let input = None;
//
//         let options = BufferStreamOptions::new(PlaylistItemType::Live, true, true, 0, false);
//         let value = create_provider_stream(&cfg, Arc::clone(&client), &url, &req, input, options);
//         let mut values = value.await;
//         'outer: while let Some((ref mut stream, info)) = values.as_mut() {
//             if info.is_some() {
//                 println!("{:?}", info.as_ref().unwrap());
//             }
//             while let Some(result) = stream.next().await {
//                 match result {
//                     Ok(bytes) => {
//                         println!("Received {} bytes  {bytes:?}", bytes.len());
//                         counter -= 1;
//                         if counter < 0 {
//                             break 'outer;
//                         }
//                     }
//                     Err(err) => {
//                         eprintln!("Error occurred: {}", err);
//                         break 'outer;
//                     }
//                 }
//             }
//         }
//         HttpResponse::Ok().finish()
//     }
// }
