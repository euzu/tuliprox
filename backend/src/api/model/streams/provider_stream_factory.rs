use crate::api::api_utils::{get_headers_from_request, StreamOptions};
use crate::api::model::{get_response_headers, AppState, CustomVideoStreamType};
use crate::api::model::StreamError;
use crate::api::model::{create_channel_unavailable_stream, get_header_filter_for_item_type};
use crate::api::model::{BoxedProviderStream, ProviderStreamFactoryResponse};
use crate::model::{ReverseProxyDisabledHeaderConfig};
use crate::tools::atomic_once_flag::AtomicOnceFlag;
use crate::utils::debug_if_enabled;
use crate::utils::request::{classify_content_type, get_request_headers, MimeCategory};
use futures::stream::{self};
use futures::{StreamExt, TryStreamExt};
use log::{debug, log_enabled, warn};
use reqwest::header::{HeaderMap, RANGE};
use reqwest::StatusCode;
use shared::model::{PlaylistItemType, DEFAULT_USER_AGENT};
use shared::utils::{filter_request_header, sanitize_sensitive_info};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;
use crate::api::model::streams::buffered_stream::BufferedStream;
use crate::api::model::streams::client_stream::ClientStream;

const RETRY_SECONDS: u64 = 5;
const ERR_MAX_RETRY_COUNT: u32 = 5;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ProviderStreamFactoryOptions {
    addr: SocketAddr,
    // item_type: PlaylistItemType,
    reconnect_enabled: bool,
    buffer_enabled: bool,
    buffer_size: usize,
    share_stream: bool,
    pipe_stream: bool,
    url: Url,
    headers: HeaderMap,
    default_user_agent: Option<axum::http::header::HeaderValue>,
    range_bytes: Arc<Option<AtomicUsize>>,
    range_requested: bool,
    reconnect_flag: Arc<AtomicOnceFlag>,
}

impl ProviderStreamFactoryOptions {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        addr: SocketAddr,
        item_type: PlaylistItemType,
        share_stream: bool,
        stream_options: &StreamOptions,
        stream_url: &Url,
        req_headers: &HeaderMap,
        input_headers: Option<&HashMap<String, String>>,
        disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
        default_user_agent: Option<&str>,
    ) -> Self {
        let buffer_size = if stream_options.buffer_enabled {
            stream_options.buffer_size
        } else {
            0
        };
        let filter_header = get_header_filter_for_item_type(item_type);
        let mut req_headers = get_headers_from_request(req_headers, &filter_header);
        let requested_range = get_request_range_start_bytes(&req_headers);
        req_headers.remove("range");

        // We merge configured input headers with the headers from the request.
        let headers = get_request_headers(
            input_headers,
            Some(&req_headers),
            disabled_headers,
            default_user_agent,
        );

        let default_user_agent = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| axum::http::header::HeaderValue::from_str(ua).ok());

        let url = stream_url.clone();
        let range_bytes = if matches!(item_type, PlaylistItemType::Live | PlaylistItemType::LiveUnknown) {
            Arc::new(requested_range.map(AtomicUsize::new))
        } else {
            Arc::new(Some(AtomicUsize::new(requested_range.unwrap_or(0))))
        };

        Self {
            // item_type,
            addr,
            reconnect_enabled: stream_options.stream_retry,
            pipe_stream: stream_options.pipe_provider_stream,
            buffer_enabled: stream_options.buffer_enabled,
            buffer_size,
            share_stream,
            reconnect_flag: Arc::new(AtomicOnceFlag::new()),
            url,
            headers,
            default_user_agent,
            range_bytes,
            range_requested: requested_range.is_some(),
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
        self.range_bytes
            .as_ref()
            .as_ref()
            .map(|atomic| atomic.load(Ordering::Acquire))
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
    pub fn was_range_requested(&self) -> bool {
        self.range_requested
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

fn prepare_client(
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> (reqwest::RequestBuilder, bool) {
    let url = stream_options.get_url();
    let range_start = stream_options.get_total_bytes_send();
    let original_headers = stream_options.get_headers();

    if log_enabled!(log::Level::Debug) {
        let message = format!("original headers {original_headers:?}");
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let mut headers = HeaderMap::default();

    for (key, value) in original_headers {
        if filter_request_header(key.as_str()) {
            headers.insert(key.clone(), value.clone());
        }
    }

    // Force Connection: close so the provider releases its slot immediately when the stream ends.
    // This prevents 509 errors from providers counting idle pooled connections against limits.
    headers.insert(
        axum::http::header::CONNECTION,
        axum::http::header::HeaderValue::from_static("close"),
    );

    if !headers.contains_key(axum::http::header::USER_AGENT) {
        headers.insert(
            axum::http::header::USER_AGENT,
            stream_options
                .default_user_agent
                .clone()
                .unwrap_or_else(|| axum::http::header::HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }

    let partial = if let Some(range) = range_start {
        if range > 0 || stream_options.was_range_requested() {
            let range_header = format!("bytes={range}-");
            if let Ok(header_value) = axum::http::header::HeaderValue::from_str(&range_header) {
                headers.insert(RANGE, header_value);
            }
            true
        } else {
            false
        }
    } else {
        false
    };

    if log_enabled!(log::Level::Debug) {
        let message = format!(
            "Stream requested with headers: {:?}",
            headers
                .iter()
                .map(|header| (header.0, String::from_utf8_lossy(header.1.as_ref())))
                .collect::<Vec<_>>()
        );
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let request_builder = request_client.get(url.clone()).headers(headers);

    (request_builder, partial)
}

async fn provider_stream_request(
    app_state: &Arc<AppState>,
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, StatusCode> {
    let (client, _partial_content) = prepare_client(request_client, stream_options);
    match client.send().await {
        Ok(mut response) => {
            let status = response.status();
            if status.is_success() {
                let response_info = {
                    // Unfortunately, the HEAD request does not work, so we need this workaround.
                    // We need some header information from the provider, we extract the necessary headers and forward them to the client
                    if log_enabled!(log::Level::Debug) {
                        let message = format!(
                            "Provider response  status: '{}' headers: {:?}",
                            response.status(),
                            response.headers_mut()
                        );
                        debug!("{}", sanitize_sensitive_info(&message));
                    }

                    let response_headers: Vec<(String, String)> =
                        get_response_headers(response.headers());
                    //let url = stream_options.get_url();
                    // debug!("First  headers {headers:?} {} {}", sanitize_sensitive_info(url.as_str()));
                    Some((
                        response_headers,
                        response.status(),
                        Some(response.url().clone()),
                        None,
                    ))
                };

                let provider_stream = response
                    .bytes_stream()
                    .map_err(|err| {
                        // error!("Stream error {err}");
                        StreamError::reqwest(&err)
                    })
                    .boxed();
                return Ok(Some((provider_stream, response_info)));
            }

            if status.is_client_error() {
                debug!("Client error status response : {status}");
                return match status {
                    StatusCode::NOT_FOUND
                    | StatusCode::FORBIDDEN
                    | StatusCode::UNAUTHORIZED
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::BAD_REQUEST => {
                        handle_channel_unavailable_stream(app_state, stream_options).await
                    }
                    _ => Err(status),
                };
            }
            if status.is_server_error() {
                debug!("Server error status response : {status}");
                return match status {
                    StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT => {
                        handle_channel_unavailable_stream(app_state, stream_options).await
                    }
                    _ => Err(status),
                };
            }
            Err(status)
        }
        Err(_err) => {
            handle_channel_unavailable_stream(app_state, stream_options).await
        }
    }
}

async fn handle_channel_unavailable_stream(app_state: &Arc<AppState>,
                                               stream_options: &ProviderStreamFactoryOptions
) -> Result<Option<ProviderStreamFactoryResponse>, StatusCode> {
    app_state.connection_manager.update_stream_detail(&stream_options.addr, CustomVideoStreamType::ChannelUnavailable).await;
    app_state.connection_manager.release_provider_connection(&stream_options.addr).await;

    if let (Some(boxed_provider_stream), response_info) =
        create_channel_unavailable_stream(&app_state.app_config,&get_response_headers(stream_options.get_headers()),
                                          StatusCode::SERVICE_UNAVAILABLE)
    {
        Ok(Some((boxed_provider_stream, response_info)))
    } else {
        Err(StatusCode::SERVICE_UNAVAILABLE)
    }
}

async fn get_provider_stream(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, StatusCode> {
    let url = stream_options.get_url();
    debug_if_enabled!("stream provider {}", sanitize_sensitive_info(url.as_str()));
    let start = Instant::now();
    let mut connect_err: u32 = 1;

    while stream_options.should_continue() {
        match provider_stream_request(app_state, client, stream_options).await {
            Ok(Some(stream_response)) => {
                return Ok(Some(stream_response));
            }
            Ok(None) => {
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!("The stream could be unavailable. {}", sanitize_sensitive_info(stream_options.get_url().as_str()));
                    break;
                }
            }
            Err(status) => {
                debug!("Provider stream response error status response : {status}");
                if matches!(status, StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE | StatusCode::UNAUTHORIZED | StatusCode::RANGE_NOT_SATISFIABLE) {
                    warn!("The stream could be unavailable. ({status}) {}",sanitize_sensitive_info(stream_options.get_url().as_str()));
                    break;
                }
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!("The stream could be unavailable. ({status}) {}",sanitize_sensitive_info(stream_options.get_url().as_str()));
                    break;
                }
            }
        }
        if !stream_options.should_continue() || connect_err > ERR_MAX_RETRY_COUNT {
            break;
        }
        if start.elapsed().as_secs() > RETRY_SECONDS {
            warn!(
                "The stream could be unavailable. Giving up after {RETRY_SECONDS} seconds. {}",
                sanitize_sensitive_info(stream_options.get_url().as_str())
            );
            break;
        }
        connect_err += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
        debug_if_enabled!("Reconnecting stream {}", sanitize_sensitive_info(url.as_str()));
    }
    debug_if_enabled!("Stopped reconnecting stream {}", sanitize_sensitive_info(url.as_str()));
    stream_options.cancel_reconnect();
    app_state.connection_manager.release_provider_connection(&stream_options.addr).await;
    Err(StatusCode::SERVICE_UNAVAILABLE)
}

#[allow(clippy::too_many_lines)]
pub async fn create_provider_stream(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    stream_options: ProviderStreamFactoryOptions,
) -> Option<ProviderStreamFactoryResponse> {
    let client_stream_factory = |stream, reconnect_flag, range_cnt| {
        let stream = if !stream_options.is_piped()
            && stream_options.is_buffer_enabled()
            && !stream_options.is_shared_stream()
        {
            BufferedStream::new(
                stream,
                stream_options.get_buffer_size(),
                stream_options.get_reconnect_flag_clone(),
                stream_options.get_url_as_str(),
            )
            .boxed()
        } else {
            stream
        };
        ClientStream::new(
            stream,
            reconnect_flag,
            range_cnt,
            stream_options.get_url_as_str(),
        )
        .boxed()
    };

    match get_provider_stream(app_state, client, &stream_options).await {
        Ok(Some((init_stream, info))) => {
            let is_media_stream_or_not_piped = if let Some((headers, _, _, _custom_video_type)) = &info {
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
                let app_state_clone = Arc::clone(app_state);
                let client = client.clone();
                let unfold: BoxedProviderStream = stream::unfold((), move |()| {
                    let client = client.clone();
                    let stream_opts = stream_options_provider.clone();
                    let continue_streaming = continue_streaming_signal.clone();
                    let app_state_clone = Arc::clone(&app_state_clone);
                    async move {
                        if continue_streaming.is_active() {
                            match get_provider_stream(&app_state_clone, &client, &stream_opts).await {
                                Ok(Some((stream, _info))) => Some((stream, ())),
                                Ok(None) => {
                                    app_state_clone.connection_manager.release_provider_connection(&stream_opts.addr).await;
                                    continue_streaming.notify();
                                    if let (Some(boxed_provider_stream), _response_info) =
                                        create_channel_unavailable_stream(
                                            &app_state_clone.app_config,
                                            &get_response_headers(stream_opts.get_headers()),
                                            StatusCode::SERVICE_UNAVAILABLE,
                                        )
                                    {
                                        return Some((boxed_provider_stream, ()));
                                    }
                                    None
                                }
                                Err(status) => {
                                    app_state_clone.connection_manager.release_provider_connection(&stream_opts.addr).await;
                                    continue_streaming.notify();
                                    if let (Some(boxed_provider_stream), _response_info) =
                                        create_channel_unavailable_stream(
                                            &app_state_clone.app_config,
                                            &get_response_headers(stream_opts.get_headers()),
                                            status,
                                        )
                                    {
                                        return Some((boxed_provider_stream, ()));
                                    }
                                    None
                                }
                            }
                        } else {
                            app_state_clone.connection_manager.release_provider_connection(&stream_opts.addr).await;
                            None
                        }
                    }
                })
                .flatten()
                .boxed();
                Some((
                    client_stream_factory(
                        init_stream.chain(unfold).boxed(),
                        Arc::clone(&continue_client_signal),
                        stream_options.get_range_bytes_clone(),
                    )
                    .boxed(),
                    info,
                ))
            } else {
                Some((
                    client_stream_factory(
                        init_stream.boxed(),
                        Arc::clone(&continue_signal),
                        stream_options.get_range_bytes_clone(),
                    )
                    .boxed(),
                    info,
                ))
            }
        }
        Ok(None) => None,
        Err(status) => {
            app_state.connection_manager.release_provider_connection(&stream_options.addr).await;
            if let (Some(boxed_provider_stream), response_info) = create_channel_unavailable_stream(
                &app_state.app_config,
                &get_response_headers(stream_options.get_headers()),
                status,
            ) {
                return Some((boxed_provider_stream, response_info));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistItemType;
    use axum::http::HeaderMap;

    #[test]
    fn test_provider_stream_factory_options_range_logic() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/stream").unwrap();
        let stream_options = StreamOptions {
            stream_retry: true,
            buffer_enabled: true,
            buffer_size: 1024,
            pipe_provider_stream: false,
        };
        let disabled_headers = None;

        // Case 1: VOD, no initial range requested
        let mut req_headers = HeaderMap::new();
        let options = ProviderStreamFactoryOptions::new(
            addr,
            PlaylistItemType::Video,
            false,
            &stream_options,
            &stream_url,
            &req_headers,
            None,
            disabled_headers,
            None,
        );
        assert!(!options.was_range_requested());
        assert_eq!(options.get_total_bytes_send(), Some(0)); // Should track even if not requested

        // Case 2: VOD, range requested
        req_headers.insert("Range", "bytes=100-".parse().unwrap());
        let options = ProviderStreamFactoryOptions::new(
            addr,
            PlaylistItemType::Video,
            false,
            &stream_options,
            &stream_url,
            &req_headers,
            None,
            disabled_headers,
            None,
        );
        assert!(options.was_range_requested());
        assert_eq!(options.get_total_bytes_send(), Some(100));

        // Case 3: Live, no initial range requested
        let req_headers = HeaderMap::new();
        let options = ProviderStreamFactoryOptions::new(
            addr,
            PlaylistItemType::Live,
            false,
            &stream_options,
            &stream_url,
            &req_headers,
            None,
            disabled_headers,
            None,
        );
        assert!(!options.was_range_requested());
        assert_eq!(options.get_total_bytes_send(), None); // Should NOT track

        // Case 4: Live, range requested (should be stripped)
        let mut req_headers = HeaderMap::new();
        req_headers.insert("Range", "bytes=100-".parse().unwrap());
        let options = ProviderStreamFactoryOptions::new(
            addr,
            PlaylistItemType::Live,
            false,
            &stream_options,
            &stream_url,
            &req_headers,
            None,
            disabled_headers,
            None,
        );
        assert!(!options.was_range_requested()); // Stripped by filter
        assert_eq!(options.get_total_bytes_send(), None); 
    }
}
