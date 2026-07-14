use crate::{
    api::{
        api_utils::{get_headers_from_request, StreamOptions},
        model::{
            create_channel_unavailable_stream, get_header_filter_for_item_type, get_response_headers,
            log_hls_origin_content_coding,
            model_utils::{provider_response_headers, ProviderResponseHeaderError},
            streams::{buffered_stream::BufferedStream, client_stream::ClientStream},
            AppState, CustomVideoStreamType, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
            ProviderContentRepresentationMode, ProviderStreamFactoryResponse, StreamError, STREAM_IDLE_TIMEOUT,
        },
    },
    model::{AppConfig, ConfigProvider, ReverseProxyDisabledHeaderConfig},
    utils::{
        content_coding::{
            apply_outbound_content_coding_policy, content_decoding_error_from_io, decode_response_to_identity,
            ContentCodingDetection, ContentCodingError, OutboundContentCodingPolicy,
        },
        debug_if_enabled,
        request::{
            get_request_headers, is_safe_cross_origin_redirect_header, preview_request_diagnostics_for_logging,
            preview_request_target_for_logging, send_with_retry_and_provider_policy,
        },
    },
};
use futures::{StreamExt, TryStreamExt};
use log::{debug, log_enabled, warn};
use reqwest::{
    header::{HeaderMap, CONTENT_RANGE, RANGE},
    StatusCode,
};
use shared::{
    create_bitset,
    defaults::DEFAULT_USER_AGENT,
    model::{ConnectFailureReason, FailureStage, PlaylistItemType, StreamChannel, StreamInfo},
    utils::{filter_request_header, is_sanitize_sensitive_info_enabled, sanitize_sensitive_info, Internable},
};
use std::{
    collections::HashMap,
    error::Error as StdError,
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use url::Url;

const RETRY_SECONDS: u64 = 5;
const ERR_MAX_RETRY_COUNT: u32 = 5;

/// Describes whether the provider response head is available before Tuliprox commits the client response head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderResponseHeadAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
struct ProviderStreamPreparationContext {
    representation: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    range_requested: bool,
    hls_content_coding_object_kind: Option<HlsOriginContentCodingObjectKind>,
}

create_bitset!(
    u8,
    ProviderStreamFactoryFlags,
    RetryEnabled,
    InitialRetryLoopEnabled,
    BufferEnabled,
    ShareStream,
    PipeStream,
    RangeRequested
);

#[derive(Debug, Clone)]
pub struct ProviderStreamFactoryOptions {
    addr: SocketAddr,
    item_type: PlaylistItemType,
    flags: ProviderStreamFactoryFlagsSet,
    buffer_size: usize,
    url: Url,
    headers: HeaderMap,
    default_user_agent: Option<axum::http::header::HeaderValue>,
    range_start_bytes: Option<usize>,
    reconnect_flag: CancellationToken,
    provider: Option<Arc<ConfigProvider>>,
    username: Option<String>,
    client_ip: Option<String>,
    user_agent: Option<String>,
    stream_channel: Option<StreamChannel>,
    connect_failure_stage: Option<FailureStage>,
    content_representation: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    hls_content_coding_object_kind: Option<HlsOriginContentCodingObjectKind>,
}

pub(crate) struct ProviderStreamFactoryParams<'a> {
    pub addr: SocketAddr,
    pub item_type: PlaylistItemType,
    pub share_stream: bool,
    pub stream_options: &'a StreamOptions,
    pub stream_url: &'a Url,
    pub req_headers: &'a HeaderMap,
    pub input_headers: Option<&'a HashMap<String, String>>,
    pub session_headers: Option<&'a HashMap<String, String>>,
    pub disabled_headers: Option<&'a ReverseProxyDisabledHeaderConfig>,
    pub default_user_agent: Option<&'a str>,
    pub username: Option<&'a str>,
    pub client_ip: Option<&'a str>,
    pub stream_channel: Option<&'a StreamChannel>,
    pub connect_failure_stage: Option<FailureStage>,
    pub content_representation: ProviderContentRepresentationMode,
}

impl ProviderStreamFactoryOptions {
    pub(crate) fn new(request: &ProviderStreamFactoryParams<'_>) -> Self {
        let ProviderStreamFactoryParams {
            addr,
            item_type,
            share_stream,
            stream_options,
            stream_url,
            req_headers,
            input_headers,
            session_headers,
            disabled_headers,
            default_user_agent,
            username,
            client_ip,
            stream_channel,
            connect_failure_stage,
            content_representation,
        } = request;
        let buffer_size = if stream_options.buffer_enabled { stream_options.buffer_size } else { 0 };
        let user_agent = req_headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let filter_header = get_header_filter_for_item_type(*item_type);
        let mut req_headers = get_headers_from_request(req_headers, &filter_header);
        let requested_range = get_request_range_start_bytes(&req_headers);
        req_headers.remove("range");

        let merged_input_headers = merge_provider_request_headers(*input_headers, *session_headers);

        // We merge configured input headers with the headers from the request.
        let headers = get_request_headers(
            merged_input_headers.as_ref(),
            Some(&req_headers),
            *disabled_headers,
            *default_user_agent,
        );

        let default_user_agent = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| axum::http::header::HeaderValue::from_str(ua).ok());
        let url = (*stream_url).clone();
        let range_start_bytes = if matches!(item_type, PlaylistItemType::Live | PlaylistItemType::LiveUnknown) {
            requested_range
        } else {
            Some(requested_range.unwrap_or(0))
        };
        let mut flags = ProviderStreamFactoryFlagsSet::new();
        if stream_options.stream_retry {
            flags.set(ProviderStreamFactoryFlags::RetryEnabled);
            if !item_type.is_live_adaptive() {
                flags.set(ProviderStreamFactoryFlags::InitialRetryLoopEnabled);
            }
        }
        if stream_options.pipe_provider_stream {
            flags.set(ProviderStreamFactoryFlags::PipeStream);
        }
        if stream_options.buffer_enabled {
            flags.set(ProviderStreamFactoryFlags::BufferEnabled);
        }
        if *share_stream {
            flags.set(ProviderStreamFactoryFlags::ShareStream);
        }
        if requested_range.is_some() {
            flags.set(ProviderStreamFactoryFlags::RangeRequested);
        }

        Self {
            item_type: *item_type,
            addr: *addr,
            flags,
            buffer_size,
            reconnect_flag: CancellationToken::new(),
            url,
            headers,
            default_user_agent,
            range_start_bytes,
            provider: None,
            username: username.map(ToString::to_string),
            client_ip: client_ip.map(ToString::to_string),
            user_agent,
            stream_channel: stream_channel.cloned(),
            connect_failure_stage: *connect_failure_stage,
            content_representation: *content_representation,
            response_head_availability: ProviderResponseHeadAvailability::Available,
            hls_content_coding_object_kind: match *content_representation {
                ProviderContentRepresentationMode::Identity => Some(HlsOriginContentCodingObjectKind::Other),
                ProviderContentRepresentationMode::PreserveOrigin => None,
            },
        }
    }

    pub fn set_provider(&mut self, provider: Option<Arc<ConfigProvider>>) { self.provider = provider; }

    pub fn get_provider(&self) -> Option<&Arc<ConfigProvider>> { self.provider.as_ref() }

    #[inline]
    fn is_piped(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::PipeStream) }

    #[inline]
    fn is_buffer_enabled(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::BufferEnabled) }

    #[inline]
    pub(crate) fn get_buffer_size(&self) -> usize { self.buffer_size }

    #[inline]
    pub fn get_reconnect_flag_clone(&self) -> CancellationToken { self.reconnect_flag.clone() }

    #[inline]
    pub fn cancel_reconnect(&self) { self.reconnect_flag.cancel(); }

    #[inline]
    pub fn get_url(&self) -> &Url { &self.url }

    #[inline]
    pub fn get_url_as_str(&self) -> &str { self.url.as_str() }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.item_type }

    #[inline]
    pub fn should_retry_provider_request(&self) -> bool {
        self.flags.contains(ProviderStreamFactoryFlags::RetryEnabled)
    }

    #[inline]
    pub fn should_retry_initial_open_loop(&self) -> bool {
        self.flags.contains(ProviderStreamFactoryFlags::InitialRetryLoopEnabled)
    }

    #[inline]
    pub fn get_headers(&self) -> &HeaderMap { &self.headers }

    #[inline]
    pub fn get_range_start_bytes(&self) -> Option<usize> { self.range_start_bytes }

    #[inline]
    pub fn should_continue(&self) -> bool { !self.reconnect_flag.is_cancelled() }

    #[inline]
    pub fn was_range_requested(&self) -> bool { self.flags.contains(ProviderStreamFactoryFlags::RangeRequested) }

    fn get_log_url(&self) -> std::borrow::Cow<'_, str> {
        if is_sanitize_sensitive_info_enabled() {
            return std::borrow::Cow::Borrowed(self.url.as_str());
        }

        std::borrow::Cow::Owned(preview_request_target_for_logging(&self.url, self.provider.as_ref()))
    }

    fn build_connect_failed_stream_info(&self, provider_name: Arc<str>) -> Option<StreamInfo> {
        let username = self.username.as_deref()?;
        let client_ip = self.client_ip.as_deref()?;
        let stream_channel = self.stream_channel.clone()?;
        Some(StreamInfo::new(shared::model::StreamInfoParams {
            uid: 0,
            meter_uid: 0,
            username,
            addr: &self.addr,
            client_ip,
            provider: provider_name,
            stream_channel,
            user_agent: self.user_agent.clone().unwrap_or_default(),
            country_code: None,
            session_token: None,
        }))
    }

    fn get_connect_failure_stage(&self) -> Option<FailureStage> { self.connect_failure_stage }

    #[inline]
    pub(crate) fn content_representation(&self) -> ProviderContentRepresentationMode { self.content_representation }

    /// Converts an open delayed until body polling to the only representation safe without an origin response head.
    pub(crate) fn for_deferred_open(mut self) -> Self {
        self.content_representation = ProviderContentRepresentationMode::Identity;
        self.response_head_availability = ProviderResponseHeadAvailability::Unavailable;
        self
    }

    #[cfg(test)]
    pub(crate) fn response_head_is_available(&self) -> bool {
        matches!(self.response_head_availability, ProviderResponseHeadAvailability::Available)
    }

    #[cfg(test)]
    pub(crate) fn hls_content_coding_object_kind(&self) -> Option<HlsOriginContentCodingObjectKind> {
        self.hls_content_coding_object_kind
    }
}

fn merge_provider_request_headers(
    input_headers: Option<&HashMap<String, String>>,
    session_headers: Option<&HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    match (input_headers, session_headers) {
        (None, None) => None,
        (Some(headers), None) | (None, Some(headers)) => Some(headers.clone()),
        (Some(input), Some(session)) => {
            let mut merged = input.clone();
            for (key, value) in session {
                merged.insert(key.clone(), value.clone());
            }
            Some(merged)
        }
    }
}

fn record_provider_open_failure(
    app_state: &Arc<AppState>,
    stream_options: &ProviderStreamFactoryOptions,
    reason: ConnectFailureReason,
    provider_http_status: Option<StatusCode>,
    provider_error_class: Option<&str>,
) {
    let Some(failure_stage) = stream_options.get_connect_failure_stage() else { return };
    let provider_name =
        stream_options.get_provider().map_or_else(|| "unknown".intern(), |provider| provider.name.clone());
    let Some(info) = stream_options.build_connect_failed_stream_info(provider_name) else { return };
    // Resolve target_name from target_id using the stable target config name.
    let target_name =
        app_state.app_config.get_target_by_id(info.channel.target_id).as_deref().map(|t| (&t.name).intern());
    app_state.connection_manager.record_connect_failed_with_provider_failure(
        &info,
        reason,
        failure_stage,
        provider_http_status.map(|status| status.as_u16()),
        provider_error_class,
        target_name,
    );
}

fn classify_provider_status_error(status: StatusCode) -> &'static str {
    if status.is_client_error() {
        "http_4xx"
    } else if status.is_server_error() {
        "http_5xx"
    } else if status.is_redirection() {
        "http_3xx"
    } else {
        "http_other"
    }
}

fn provider_content_type_looks_like_html(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("text/html"))
}

fn should_reject_success_response_content_type(item_type: PlaylistItemType, headers: &HeaderMap) -> bool {
    !item_type.is_live_adaptive() && provider_content_type_looks_like_html(headers)
}

#[derive(Clone, Copy, Debug)]
enum ProviderStreamRequestFailure {
    Status { status: StatusCode, provider_error_class: &'static str, serve_channel_unavailable: bool },
}

#[derive(Debug, thiserror::Error)]
enum ProviderStreamPreparationError {
    #[error(transparent)]
    ContentCoding(#[from] ContentCodingError),

    #[error(transparent)]
    ResponseHeader(#[from] ProviderResponseHeaderError),

    #[error("provider response head is unavailable for status {status} or Content-Range={has_content_range}")]
    DeferredResponseHead { status: StatusCode, has_content_range: bool },
}

impl ProviderStreamPreparationError {
    fn provider_error_class(&self) -> &'static str {
        match self {
            Self::ContentCoding(ContentCodingError::InvalidHeader | ContentCodingError::Unsupported(_))
            | Self::ResponseHeader(ProviderResponseHeaderError::InvalidContentEncoding) => "content_encoding",
            Self::ContentCoding(ContentCodingError::EncodedPartialContent) => "encoded_partial_content",
            Self::ContentCoding(ContentCodingError::PrefixRead(_)) => "body",
            Self::ResponseHeader(
                ProviderResponseHeaderError::InvalidContentLength | ProviderResponseHeaderError::InvalidContentRange,
            )
            | Self::DeferredResponseHead { .. } => "response_headers",
        }
    }
}

impl ProviderStreamRequestFailure {
    fn status(self) -> StatusCode {
        match self {
            Self::Status { status, .. } => status,
        }
    }

    fn provider_error_class(self) -> &'static str {
        match self {
            Self::Status { provider_error_class, .. } => provider_error_class,
        }
    }

    fn should_serve_channel_unavailable(self) -> bool {
        match self {
            Self::Status { serve_channel_unavailable, .. } => serve_channel_unavailable,
        }
    }
}

fn classify_provider_io_error(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;

    match err.kind() {
        ErrorKind::TimedOut => "timeout",
        ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected => "connect",
        ErrorKind::AddrNotAvailable => "dns",
        _ => {
            let lowered = err.to_string().to_ascii_lowercase();
            if lowered.contains("dns")
                || lowered.contains("failed to lookup address information")
                || lowered.contains("name or service not known")
                || lowered.contains("no such host")
                || lowered.contains("temporary failure in name resolution")
            {
                "dns"
            } else {
                "io"
            }
        }
    }
}

fn should_wrap_provider_stream_in_buffer(stream_options: &ProviderStreamFactoryOptions) -> bool {
    !stream_options.is_piped()
        && !stream_options.flags.contains(ProviderStreamFactoryFlags::ShareStream)
        && stream_options.is_buffer_enabled()
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderRequestCredentialState {
    OriginalOrigin,
    Scrubbed,
}

impl ProviderRequestCredentialState {
    fn observe_target(&mut self, original_url: &Url, target_url: &Url) {
        if !same_origin(original_url, target_url) {
            *self = Self::Scrubbed;
        }
    }
}

fn prepare_client(
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
    url_override: Option<&Url>,
    credential_state: ProviderRequestCredentialState,
) -> (reqwest::RequestBuilder, bool) {
    let original_url = stream_options.get_url();
    let url = url_override.unwrap_or(original_url);
    let range_start = stream_options.get_range_start_bytes();
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

    if matches!(credential_state, ProviderRequestCredentialState::Scrubbed)
        || url_override.is_some_and(|url| !same_origin(original_url, url))
    {
        remove_sensitive_headers(&mut headers);
    }
    prepare_default_headers(&mut headers, stream_options);
    let partial = prepare_partial_request_headers(&mut headers, stream_options, range_start);
    let content_coding_policy = match stream_options.content_representation() {
        ProviderContentRepresentationMode::PreserveOrigin => OutboundContentCodingPolicy::Inherit,
        ProviderContentRepresentationMode::Identity => OutboundContentCodingPolicy::Identity,
    };
    apply_outbound_content_coding_policy(&mut headers, content_coding_policy);

    if log_enabled!(log::Level::Debug) {
        let message = format!(
            "Stream requested with headers: {:?}",
            headers.iter().map(|header| (header.0, String::from_utf8_lossy(header.1.as_ref()))).collect::<Vec<_>>()
        );
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let request_builder = request_client.get(url.clone()).headers(headers);

    (request_builder, partial)
}

fn remove_sensitive_headers(headers: &mut axum::http::HeaderMap) {
    let names_to_remove = headers
        .keys()
        .filter(|name| !is_safe_cross_origin_redirect_header(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for name in names_to_remove {
        headers.remove(name);
    }
}

fn provider_headers_require_manual_redirects(headers: &HeaderMap) -> bool {
    headers.keys().any(|name| !is_safe_cross_origin_redirect_header(name.as_str()))
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn prepare_default_headers(headers: &mut axum::http::HeaderMap, stream_options: &ProviderStreamFactoryOptions) {
    // Force Connection: close so the provider releases its slot immediately when the stream ends.
    // This prevents 509 errors from providers counting idle pooled connections against limits.
    headers.insert(axum::http::header::CONNECTION, axum::http::header::HeaderValue::from_static("close"));

    if !headers.contains_key(axum::http::header::USER_AGENT) {
        headers.insert(
            axum::http::header::USER_AGENT,
            stream_options
                .default_user_agent
                .clone()
                .unwrap_or_else(|| axum::http::header::HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }
}

fn prepare_partial_request_headers(
    headers: &mut HeaderMap,
    stream_options: &ProviderStreamFactoryOptions,
    range_start: Option<usize>,
) -> bool {
    if let Some(range) = range_start {
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
    }
}

fn collect_debug_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const HEADER_NAMES: [&str; 8] =
        ["proxy-authenticate", "via", "server", "location", "x-cache", "x-cache-status", "x-served-by", "x-proxy-id"];

    HEADER_NAMES
        .iter()
        .filter_map(|name| {
            headers.get_all(*name).iter().next().map(|value| {
                let value = value.to_str().unwrap_or("<binary>").to_string();
                ((*name).to_string(), value)
            })
        })
        .collect()
}

async fn send_with_manual_redirects(
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
    app_config: &Arc<AppConfig>,
) -> Result<reqwest::Response, std::io::Error> {
    let mut current_url = stream_options.get_url().clone();
    let mut remaining_redirects = 10u8;
    let provider = stream_options.get_provider().cloned();
    let mut credential_state = ProviderRequestCredentialState::OriginalOrigin;

    loop {
        let result = send_with_retry_and_provider_policy(
            app_config,
            &current_url,
            provider.as_ref(),
            true,
            stream_options.should_retry_provider_request(),
            |resolved_url| {
                credential_state.observe_target(stream_options.get_url(), resolved_url);
                prepare_client(request_client, stream_options, Some(resolved_url), credential_state).0
            },
        )
        .await;

        let response = match result {
            Ok(resp) => resp,
            Err(e) => {
                // send_with_retry_and_provider already applies provider failover policy.
                // Do not rotate again here, otherwise non-failover errors (e.g. auth) may
                // incorrectly switch provider URLs.
                debug!("Manual redirect failed: {}", sanitize_sensitive_info(&e.to_string()));
                return Err(e);
            }
        };

        let status = response.status();

        if status.is_redirection() {
            if remaining_redirects == 0 {
                return Ok(response);
            }
            let location = response.headers().get(reqwest::header::LOCATION);
            let Some(location) = location else {
                return Ok(response);
            };
            let Ok(location_str) = location.to_str() else {
                return Ok(response);
            };
            let response_url = response.url().clone();
            let next_url = response_url.join(location_str).or_else(|_| Url::parse(location_str));
            let Ok(next_url) = next_url else {
                return Ok(response);
            };
            current_url = next_url;
            remaining_redirects = remaining_redirects.saturating_sub(1);
            continue;
        }
        return Ok(response);
    }
}

#[cfg(test)]
async fn prepare_provider_stream_response(
    response: reqwest::Response,
    mode: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: mode,
            response_head_availability,
            range_requested: false,
            hls_content_coding_object_kind: matches!(mode, ProviderContentRepresentationMode::Identity)
                .then_some(HlsOriginContentCodingObjectKind::Other),
        },
        Duration::from_secs(STREAM_IDLE_TIMEOUT),
    )
    .await
}

#[cfg(test)]
async fn prepare_provider_stream_response_with_idle_timeout(
    response: reqwest::Response,
    mode: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    idle_timeout: Duration,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: mode,
            response_head_availability,
            range_requested: false,
            hls_content_coding_object_kind: matches!(mode, ProviderContentRepresentationMode::Identity)
                .then_some(HlsOriginContentCodingObjectKind::Other),
        },
        idle_timeout,
    )
    .await
}

async fn prepare_provider_stream_response_for_request(
    response: reqwest::Response,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: stream_options.content_representation(),
            response_head_availability: stream_options.response_head_availability,
            range_requested: stream_options.was_range_requested(),
            hls_content_coding_object_kind: stream_options.hls_content_coding_object_kind,
        },
        Duration::from_secs(STREAM_IDLE_TIMEOUT),
    )
    .await
}

async fn prepare_provider_stream_response_with_context(
    response: reqwest::Response,
    context: ProviderStreamPreparationContext,
    idle_timeout: Duration,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    match context.representation {
        ProviderContentRepresentationMode::PreserveOrigin => {
            if matches!(context.response_head_availability, ProviderResponseHeadAvailability::Unavailable) {
                return Err(ProviderStreamPreparationError::DeferredResponseHead {
                    status: response.status(),
                    has_content_range: response.headers().contains_key(CONTENT_RANGE),
                });
            }
            let headers = provider_response_headers(response.headers(), context.representation)?;
            let response_info = Some((headers, response.status(), Some(response.url().clone()), None));
            let stream = response.bytes_stream().map_err(|error| StreamError::reqwest(&error)).boxed();
            Ok((stream, response_info))
        }
        ProviderContentRepresentationMode::Identity => {
            let origin_status = response.status();
            let origin_has_content_range = response.headers().contains_key(CONTENT_RANGE);
            // `deflate` decoder selection reads a prefix before the returned stream can enter the
            // normal provider body wrappers, so decoder setup must own the same idle bound too.
            let decoded = tokio::time::timeout(
                idle_timeout,
                decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly),
            )
            .await
            .map_err(|_| {
                ContentCodingError::PrefixRead(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "provider body idle timeout during content-decoder setup",
                ))
            })??;
            if matches!(context.response_head_availability, ProviderResponseHeadAvailability::Unavailable)
                && (origin_status != StatusCode::OK || origin_has_content_range)
            {
                return Err(ProviderStreamPreparationError::DeferredResponseHead {
                    status: origin_status,
                    has_content_range: origin_has_content_range,
                });
            }
            if let (Some(observation), Some(object_kind)) =
                (decoded.content_coding_observation(), context.hls_content_coding_object_kind)
            {
                log_hls_origin_content_coding(
                    observation,
                    object_kind,
                    context.range_requested,
                    HlsOriginContentCodingSource::Legacy,
                );
            }
            let headers = provider_response_headers(&decoded.headers, context.representation)?;
            let response_info = Some((headers, decoded.status, Some(decoded.final_url), None));
            let stream = ReaderStream::new(decoded.body).map_err(|error| provider_decoded_body_error(&error)).boxed();
            Ok((stream, response_info))
        }
    }
}

fn provider_decoded_body_error(error: &io::Error) -> StreamError {
    if let Some(error) = content_decoding_error_from_io(error) {
        return StreamError::ContentDecoding(format!("coding={}", error.coding.as_http_token()));
    }

    let mut source = StdError::source(error);
    while let Some(current) = source {
        if let Some(error) = current.downcast_ref::<reqwest::Error>() {
            return StreamError::reqwest(error);
        }
        source = current.source();
    }
    StreamError::StdIo(error.to_string())
}

#[allow(clippy::too_many_lines)]
async fn provider_stream_request(
    app_state: &Arc<AppState>,
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, ProviderStreamRequestFailure> {
    let use_manual_redirects = app_state.should_use_manual_redirects()
        || provider_headers_require_manual_redirects(stream_options.get_headers());
    if log_enabled!(log::Level::Debug) {
        let diagnostics =
            preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
        debug!(
            "Provider request diagnostics: manual_redirects={}, {}",
            use_manual_redirects,
            sanitize_sensitive_info(&diagnostics)
        );
    }
    let response_result = if use_manual_redirects {
        let client_no_redirect = app_state.http_client_no_redirect.load();
        send_with_manual_redirects(&client_no_redirect, stream_options, &app_state.app_config).await
    } else {
        // Use send_with_retry_and_provider for automatic failover support
        let url = stream_options.get_url();
        let provider = stream_options.get_provider().cloned();

        send_with_retry_and_provider_policy(
            &app_state.app_config,
            url,
            provider.as_ref(),
            false,
            stream_options.should_retry_provider_request(),
            |resolved_url| {
                let (client, _partial_content) = prepare_client(
                    request_client,
                    stream_options,
                    Some(resolved_url),
                    ProviderRequestCredentialState::OriginalOrigin,
                );
                client
            },
        )
        .await
    };
    match response_result {
        Ok(response) => {
            let status = response.status();
            let response_url = response.url().clone();
            if log_enabled!(log::Level::Debug) && !status.is_success() {
                let debug_headers = collect_debug_headers(response.headers());
                let diagnostics =
                    preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
                let message =
                    format!(
                        "Provider response error: status={status}, url={response_url}, headers={debug_headers:?}, {diagnostics}"
                    );
                debug!("{}", sanitize_sensitive_info(&message));
            }
            if status.is_success() {
                if should_reject_success_response_content_type(stream_options.get_item_type(), response.headers()) {
                    debug!(
                        "Provider returned HTML content for non-adaptive stream {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    return Err(ProviderStreamRequestFailure::Status {
                        status: StatusCode::BAD_GATEWAY,
                        provider_error_class: "unexpected_content_type",
                        serve_channel_unavailable: true,
                    });
                }
                let response = prepare_provider_stream_response_for_request(response, stream_options).await;
                let (provider_stream, response_info) = match response {
                    Ok(response) => response,
                    Err(error) => {
                        let provider_error_class = error.provider_error_class();
                        return Err(ProviderStreamRequestFailure::Status {
                            status: StatusCode::BAD_GATEWAY,
                            provider_error_class,
                            serve_channel_unavailable: true,
                        });
                    }
                };
                if log_enabled!(log::Level::Debug) {
                    // Unfortunately, the HEAD request does not work, so we need this workaround.
                    // We need some header information from the provider, we extract the necessary headers and forward them to the client
                    let message = format!("Provider response info: {response_info:?}");
                    debug!("{}", sanitize_sensitive_info(&message));
                }
                return Ok(Some((provider_stream, response_info)));
            }

            if status.is_client_error() {
                debug!("Client error status response : {status}");
                return match status {
                    StatusCode::NOT_FOUND
                    | StatusCode::FORBIDDEN
                    | StatusCode::UNAUTHORIZED
                    | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::BAD_REQUEST => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: true,
                    }),
                    _ => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: false,
                    }),
                };
            }
            if status.is_server_error() {
                debug!("Server error status response : {status}");
                return match status {
                    StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: true,
                    }),
                    _ => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: false,
                    }),
                };
            }
            Err(ProviderStreamRequestFailure::Status {
                status,
                provider_error_class: classify_provider_status_error(status),
                serve_channel_unavailable: false,
            })
        }
        Err(err) => {
            let diagnostics =
                preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
            debug!(
                "Provider request failed: {}, {}",
                sanitize_sensitive_info(&err.to_string()),
                sanitize_sensitive_info(&diagnostics)
            );
            Err(ProviderStreamRequestFailure::Status {
                status: StatusCode::SERVICE_UNAVAILABLE,
                provider_error_class: classify_provider_io_error(&err),
                serve_channel_unavailable: true,
            })
        }
    }
}

async fn get_provider_stream(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, ProviderStreamRequestFailure> {
    let log_url = stream_options.get_log_url();
    debug_if_enabled!("stream provider {}", sanitize_sensitive_info(log_url.as_ref()));
    let start = Instant::now();
    let mut connect_err: u32 = 1;

    while stream_options.should_continue() {
        match provider_stream_request(app_state, client, stream_options).await {
            Ok(Some(stream_response)) => {
                return Ok(Some(stream_response));
            }
            Ok(None) => {
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!(
                        "The stream could be unavailable. {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
            }
            Err(failure) => {
                if failure.should_serve_channel_unavailable() {
                    return Err(failure);
                }
                let status = failure.status();
                debug!("Provider stream response error status response : {status}");
                if matches!(
                    status,
                    StatusCode::FORBIDDEN
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::UNAUTHORIZED
                        | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                        | StatusCode::RANGE_NOT_SATISFIABLE
                ) {
                    warn!(
                        "The stream could be unavailable. ({status}) {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!(
                        "The stream could be unavailable. ({status}) {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
            }
        }
        if !stream_options.should_continue() || connect_err > ERR_MAX_RETRY_COUNT {
            break;
        }
        if !stream_options.should_retry_initial_open_loop() {
            break;
        }
        if start.elapsed().as_secs() > RETRY_SECONDS {
            warn!(
                "The stream could be unavailable. Giving up after {RETRY_SECONDS} seconds. {}",
                sanitize_sensitive_info(stream_options.get_log_url().as_ref())
            );
            break;
        }
        connect_err += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
        debug_if_enabled!("Reconnecting stream {}", sanitize_sensitive_info(stream_options.get_log_url().as_ref()));
    }
    debug_if_enabled!("Stopped reconnecting stream {}", sanitize_sensitive_info(stream_options.get_log_url().as_ref()));
    stream_options.cancel_reconnect();
    app_state.connection_manager.release_provider_connection(&stream_options.addr).await;
    Err(ProviderStreamRequestFailure::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        provider_error_class: "service_unavailable",
        serve_channel_unavailable: true,
    })
}

#[allow(clippy::too_many_lines)]
pub async fn create_provider_stream(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    stream_options: ProviderStreamFactoryOptions,
) -> Option<ProviderStreamFactoryResponse> {
    match get_provider_stream(app_state, client, &stream_options).await {
        Ok(Some((init_stream, info))) => {
            if let Some((_headers, _status, _response_url, Some(custom_video_type))) = &info {
                let reason = match custom_video_type {
                    CustomVideoStreamType::ChannelUnavailable => Some(ConnectFailureReason::ChannelUnavailable),
                    CustomVideoStreamType::Provisioning => Some(ConnectFailureReason::Provisioning),
                    CustomVideoStreamType::ProviderConnectionsExhausted => {
                        Some(ConnectFailureReason::ProviderConnectionsExhausted)
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    record_provider_open_failure(app_state, &stream_options, reason, None, None);
                }
            }
            let continue_signal = stream_options.get_reconnect_flag_clone();
            let stream = init_stream.boxed();
            let stream = if should_wrap_provider_stream_in_buffer(&stream_options) {
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
            Some((
                ClientStream::new(stream, continue_signal.clone(), None, stream_options.get_url_as_str()).boxed(),
                info,
            ))
        }
        Ok(None) => None,
        Err(failure) => {
            let status = failure.status();
            app_state.connection_manager.release_provider_connection(&stream_options.addr).await;
            record_provider_open_failure(
                app_state,
                &stream_options,
                ConnectFailureReason::ChannelUnavailable,
                Some(status),
                Some(failure.provider_error_class()),
            );
            if let (Some(boxed_provider_stream), response_info) = create_channel_unavailable_stream(
                &app_state.app_config,
                &get_response_headers(stream_options.get_headers()),
                StatusCode::OK,
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
    use crate::{
        api::model::BoxedProviderStream,
        model::{Config, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::HeaderMap;
    use bytes::Bytes;
    use futures::TryStreamExt;
    use shared::{
        model::{ConfigPaths, PlaylistItemType, StreamChannel, XtreamCluster},
        utils::Internable,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
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

        let (stream, info) = prepare_provider_stream_response(
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
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: false, buffer_size: 0, pipe_provider_stream: false };
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let task_requests = Arc::clone(&requests);
        let response_headers = headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect::<String>();
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

    #[test]
    fn test_provider_stream_factory_options_range_logic() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/stream").unwrap();
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: true, buffer_size: 1024, pipe_provider_stream: false };
        let disabled_headers = None;

        // Case 1: VOD, no initial range requested
        let mut req_headers = HeaderMap::new();
        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Video,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });
        assert!(!options.was_range_requested());
        assert_eq!(options.get_range_start_bytes(), Some(0)); // Should track range start even if not requested

        // Case 2: VOD, range requested
        req_headers.insert("Range", "bytes=100-".parse().unwrap());
        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Video,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });
        assert!(options.was_range_requested());
        assert_eq!(options.get_range_start_bytes(), Some(100));

        // Case 3: Live, no initial range requested
        let req_headers = HeaderMap::new();
        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Live,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });
        assert!(!options.was_range_requested());
        assert_eq!(options.get_range_start_bytes(), None); // Should NOT track range start

        // Case 4: Live, range requested (should be stripped)
        let mut req_headers = HeaderMap::new();
        req_headers.insert("Range", "bytes=100-".parse().unwrap());
        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Live,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });
        assert!(!options.was_range_requested()); // Stripped by filter
        assert_eq!(options.get_range_start_bytes(), None);
    }

    #[test]
    fn test_provider_stream_factory_options_keeps_initial_retry_for_live_adaptive_streams() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/segment.ts").unwrap();
        let req_headers = HeaderMap::new();
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: true, buffer_size: 1024, pipe_provider_stream: false };

        let hls_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::LiveHls,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });

        let dash_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::LiveDash,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });

        assert!(hls_options.should_retry_provider_request());
        assert!(dash_options.should_retry_provider_request());
        assert!(!hls_options.should_retry_initial_open_loop());
        assert!(!dash_options.should_retry_initial_open_loop());
    }

    #[test]
    fn test_shared_streams_do_not_use_provider_buffer_wrapper() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/shared.ts").unwrap();
        let req_headers = HeaderMap::new();
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: true, buffer_size: 1024, pipe_provider_stream: false };

        let shared_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Live,
            share_stream: true,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });

        assert!(
            !should_wrap_provider_stream_in_buffer(&shared_options),
            "shared streams must bypass provider-side BufferedStream"
        );
    }

    #[test]
    fn test_provider_stream_factory_options_builds_connect_failed_stream_info_from_history_context() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/stream").unwrap();
        let req_headers = HeaderMap::new();
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: true, buffer_size: 1024, pipe_provider_stream: false };

        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::Live,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: Some("alice"),
            client_ip: Some("203.0.113.9"),
            stream_channel: Some(&StreamChannel {
                target_id: 1,
                virtual_id: 77,
                provider_id: 3,
                input_name: "input-a".intern(),
                item_type: PlaylistItemType::Live,
                cluster: XtreamCluster::Live,
                group: "News".intern(),
                title: "Example".intern(),
                url: "http://provider.example/live/77".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: None,
                epg_channel_id: None,
                epg_reference_ts: None,
            }),
            connect_failure_stage: Some(FailureStage::ProviderOpen),
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });

        let info = options.build_connect_failed_stream_info("provider-a".intern()).expect("history context");

        assert_eq!(info.username, "alice");
        assert_eq!(info.client_ip, "203.0.113.9");
        assert_eq!(info.provider.as_ref(), "provider-a");
        assert_eq!(info.channel.input_name.as_ref(), "input-a");
        assert_eq!(info.channel.virtual_id, 77);
    }

    #[test]
    fn html_content_type_is_rejected_for_catchup_streams() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::CONTENT_TYPE, "text/html; charset=UTF-8".parse().unwrap());

        assert!(should_reject_success_response_content_type(PlaylistItemType::Catchup, &headers));
        assert!(should_reject_success_response_content_type(PlaylistItemType::Video, &headers));
        assert!(should_reject_success_response_content_type(PlaylistItemType::Live, &headers));
    }

    #[test]
    fn html_content_type_is_allowed_for_live_adaptive_streams() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::CONTENT_TYPE, "text/html; charset=UTF-8".parse().unwrap());

        assert!(!should_reject_success_response_content_type(PlaylistItemType::LiveHls, &headers));
        assert!(!should_reject_success_response_content_type(PlaylistItemType::LiveDash, &headers));
    }

    #[test]
    fn session_headers_are_forwarded_to_provider_requests() {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let stream_url = Url::parse("http://example.com/live/segment.ts").unwrap();
        let req_headers = HeaderMap::new();
        let mut session_headers = HashMap::new();
        session_headers.insert(String::from("cookie"), String::from("sid=abc; pref=1"));
        let stream_options =
            StreamOptions { stream_retry: true, buffer_enabled: true, buffer_size: 1024, pipe_provider_stream: false };

        let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
            addr,
            item_type: PlaylistItemType::LiveHls,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &stream_url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: Some(&session_headers),
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        });

        assert_eq!(
            options.get_headers().get(axum::http::header::COOKIE).and_then(|value| value.to_str().ok()),
            Some("sid=abc; pref=1")
        );
    }

    #[test]
    fn cross_origin_provider_requests_keep_only_redirect_safe_headers() {
        let stream_url = Url::parse("http://provider-a.example/live/segment.ts").unwrap();
        let mut req_headers = HeaderMap::new();
        req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
        req_headers.insert(reqwest::header::RANGE, "bytes=17-".parse().unwrap());
        req_headers.insert(reqwest::header::USER_AGENT, "test-agent".parse().unwrap());
        req_headers.insert(reqwest::header::AUTHORIZATION, "Bearer client".parse().unwrap());
        req_headers.insert(reqwest::header::COOKIE, "client=1".parse().unwrap());
        req_headers.insert(reqwest::header::PROXY_AUTHORIZATION, "Basic proxy-secret".parse().unwrap());
        let input_headers = HashMap::from([
            ("X-API-Key".to_string(), "api-secret".to_string()),
            ("X-Provider-Token".to_string(), "provider-secret".to_string()),
        ]);
        let options = test_options(
            ProviderContentRepresentationMode::PreserveOrigin,
            &stream_url,
            &req_headers,
            Some(&input_headers),
            None,
        );
        assert!(provider_headers_require_manual_redirects(options.get_headers()));

        let mut safe_headers = HeaderMap::new();
        safe_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
        safe_headers.insert(reqwest::header::RANGE, "bytes=17-".parse().unwrap());
        let safe_options =
            test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &safe_headers, None, None);
        assert!(!provider_headers_require_manual_redirects(safe_options.get_headers()));

        let client = reqwest::Client::new();
        let sensitive_headers = ["authorization", "cookie", "proxy-authorization", "x-api-key", "x-provider-token"];

        let same_origin = Url::parse("http://provider-a.example/live/failover.ts").unwrap();
        let same_origin_request = prepare_client(
            &client,
            &options,
            Some(&same_origin),
            ProviderRequestCredentialState::OriginalOrigin,
        )
        .0
        .build()
        .unwrap();
        for name in sensitive_headers {
            assert!(same_origin_request.headers().contains_key(name), "same-origin request lost {name}");
        }

        let cross_origin = Url::parse("http://provider-b.example/live/segment.ts").unwrap();
        for (target, credential_state) in [
            (Some(&cross_origin), ProviderRequestCredentialState::OriginalOrigin),
            (None, ProviderRequestCredentialState::Scrubbed),
        ] {
            let request = prepare_client(&client, &options, target, credential_state).0.build().unwrap();
            for name in sensitive_headers {
                assert!(!request.headers().contains_key(name), "cross-origin request retained {name}");
            }
            assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "gzip");
            assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=17-");
            assert_eq!(request.headers()[reqwest::header::USER_AGENT], "test-agent");
        }
    }

    #[test]
    fn identity_is_enforced_after_header_merges_for_every_request_target() {
        let stream_url = Url::parse("http://provider-a.example/live/segment.ts").unwrap();
        let mut req_headers = HeaderMap::new();
        req_headers.insert(reqwest::header::ACCEPT_ENCODING, "br".parse().unwrap());
        req_headers.insert(reqwest::header::RANGE, "bytes=17-".parse().unwrap());
        req_headers.insert(reqwest::header::AUTHORIZATION, "Bearer client".parse().unwrap());
        req_headers.insert(reqwest::header::COOKIE, "client=1".parse().unwrap());
        let input_headers = HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]);
        let session_headers = HashMap::from([("Accept-Encoding".to_string(), "zstd".to_string())]);
        let options = test_options(
            ProviderContentRepresentationMode::Identity,
            &stream_url,
            &req_headers,
            Some(&input_headers),
            Some(&session_headers),
        );
        let client = reqwest::Client::new();

        let same_origin = Url::parse("http://provider-a.example/live/failover.ts").unwrap();
        let cross_origin = Url::parse("http://provider-b.example/live/segment.ts").unwrap();
        for target in [None, Some(&same_origin), Some(&cross_origin)] {
            let request = prepare_client(&client, &options, target, ProviderRequestCredentialState::OriginalOrigin)
                .0
                .build()
                .unwrap();
            assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "identity");
            assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=17-");
            if target == Some(&cross_origin) {
                assert!(!request.headers().contains_key(reqwest::header::AUTHORIZATION));
                assert!(!request.headers().contains_key(reqwest::header::COOKIE));
            }
        }

        let reconnect_options = options.clone();
        assert_eq!(reconnect_options.content_representation(), ProviderContentRepresentationMode::Identity);
        assert_eq!(reconnect_options.hls_content_coding_object_kind(), Some(HlsOriginContentCodingObjectKind::Other));
        assert_eq!(
            options.clone().for_deferred_open().hls_content_coding_object_kind(),
            Some(HlsOriginContentCodingObjectKind::Other)
        );
    }

    #[test]
    fn preserve_origin_does_not_override_accept_encoding() {
        let stream_url = Url::parse("http://provider.example/movie").unwrap();
        let mut req_headers = HeaderMap::new();
        req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip, br".parse().unwrap());
        let options =
            test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &req_headers, None, None);

        let request =
            prepare_client(&reqwest::Client::new(), &options, None, ProviderRequestCredentialState::OriginalOrigin)
                .0
                .build()
                .unwrap();

        assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "gzip, br");
    }

    #[tokio::test]
    async fn automatic_redirect_round_trip_keeps_identity_and_never_restores_credentials() {
        let encoded = encode(b"decoded redirect body", TestEncoding::Gzip).await;
        let RedirectRoundTrip { entry_url, first_origin_task, second_origin_task } =
            spawn_redirect_round_trip(encoded).await;
        let options = redirect_test_options(&entry_url);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("automatic redirect client");

        let response = prepare_client(&client, &options, None, ProviderRequestCredentialState::OriginalOrigin)
            .0
            .send()
            .await
            .expect("automatic redirect request succeeds");

        assert_identity_redirect_result(response, first_origin_task, second_origin_task).await;
    }

    #[tokio::test]
    async fn manual_redirect_round_trip_keeps_identity_and_never_restores_credentials() {
        let encoded = encode(b"decoded redirect body", TestEncoding::Gzip).await;
        let RedirectRoundTrip { entry_url, first_origin_task, second_origin_task } =
            spawn_redirect_round_trip(encoded).await;
        let options = redirect_test_options(&entry_url);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("manual redirect client");

        let response = send_with_manual_redirects(&client, &options, &test_app_config())
            .await
            .expect("manual redirect request succeeds");

        assert_identity_redirect_result(response, first_origin_task, second_origin_task).await;
    }

    #[tokio::test]
    async fn deferred_open_without_origin_head_accepts_only_plain_origin_200() {
        const IDENTITY_BODY: &[u8] = b"deferred identity body";

        let stream_url = Url::parse("http://provider.example/live/deferred.ts").unwrap();
        let mut req_headers = HeaderMap::new();
        req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
        let deferred_options =
            test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &req_headers, None, None)
                .for_deferred_open();
        let deferred_request = prepare_client(
            &reqwest::Client::new(),
            &deferred_options,
            None,
            ProviderRequestCredentialState::OriginalOrigin,
        )
        .0
        .build()
        .expect("deferred provider request");
        assert_eq!(deferred_options.content_representation(), ProviderContentRepresentationMode::Identity);
        assert!(!deferred_options.response_head_is_available());
        assert_eq!(deferred_options.hls_content_coding_object_kind(), None);
        assert_eq!(deferred_request.headers()[reqwest::header::ACCEPT_ENCODING], "identity");

        let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
        let (response, _) = local_response(StatusCode::OK, &[("Content-Encoding", "gzip")], encoded).await;
        let (stream, info) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Unavailable,
        )
        .await
        .expect("plain 200 is safe for deferred identity streaming");
        assert_eq!(collect_stream(stream).await.expect("deferred body decodes"), IDENTITY_BODY);
        let (headers, status, _, _) = info.expect("normalized deferred response metadata");
        assert_eq!(status, StatusCode::OK);
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-encoding")));
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));

        let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
        let (response, _) = local_response(
            StatusCode::OK,
            &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-21/22")],
            encoded,
        )
        .await;
        assert!(matches!(
            prepare_provider_stream_response(
                response,
                ProviderContentRepresentationMode::Identity,
                ProviderResponseHeadAvailability::Unavailable,
            )
            .await,
            Err(ProviderStreamPreparationError::DeferredResponseHead {
                status: StatusCode::OK,
                has_content_range: true,
            })
        ));

        let (response, _) = local_response(StatusCode::PARTIAL_CONTENT, &[], IDENTITY_BODY.to_vec()).await;
        assert!(matches!(
            prepare_provider_stream_response(
                response,
                ProviderContentRepresentationMode::Identity,
                ProviderResponseHeadAvailability::Unavailable,
            )
            .await,
            Err(ProviderStreamPreparationError::DeferredResponseHead {
                status: StatusCode::PARTIAL_CONTENT,
                has_content_range: false,
            })
        ));
    }

    #[tokio::test]
    async fn deferred_open_rejects_preserve_origin_without_origin_head() {
        let (response, _) = local_response(StatusCode::OK, &[], b"opaque bytes".to_vec()).await;

        assert!(matches!(
            prepare_provider_stream_response(
                response,
                ProviderContentRepresentationMode::PreserveOrigin,
                ProviderResponseHeadAvailability::Unavailable,
            )
            .await,
            Err(ProviderStreamPreparationError::DeferredResponseHead {
                status: StatusCode::OK,
                has_content_range: false,
            })
        ));
    }

    #[tokio::test]
    async fn preserve_origin_rejects_invalid_representation_header_before_stream_release() {
        let (response, requests) =
            local_response(StatusCode::OK, &[("Content-Encoding", "gzip,,br")], b"body must not be released".to_vec())
                .await;

        assert!(matches!(
            prepare_provider_stream_response(
                response,
                ProviderContentRepresentationMode::PreserveOrigin,
                ProviderResponseHeadAvailability::Available,
            )
            .await,
            Err(ProviderStreamPreparationError::ResponseHeader(ProviderResponseHeaderError::InvalidContentEncoding))
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn legacy_hls_segment_key_map_part_and_other_stream_as_identity() {
        const IDENTITY: &[u8] = b"identity hls resource bytes";
        let cases = [
            ("segment", "gzip", TestEncoding::Gzip),
            ("segment-zlib", "deflate", TestEncoding::Zlib),
            ("segment-raw-deflate", "deflate", TestEncoding::RawDeflate),
            ("key", "br", TestEncoding::Brotli),
            ("map", "zstd", TestEncoding::Zstd),
            ("part", "gzip", TestEncoding::Gzip),
            ("other", "br", TestEncoding::Brotli),
        ];

        for (resource_kind, content_encoding, encoding) in cases {
            let encoded = encode(IDENTITY, encoding).await;
            let (response, _) =
                local_response(StatusCode::OK, &[("Content-Encoding", content_encoding)], encoded).await;

            let (stream, info) = prepare_provider_stream_response(
                response,
                ProviderContentRepresentationMode::Identity,
                ProviderResponseHeadAvailability::Available,
            )
            .await
            .unwrap();

            assert_eq!(collect_stream(stream).await.unwrap(), IDENTITY, "failed for {resource_kind}");
            let (headers, status, _, _) = info.unwrap();
            assert_eq!(status, StatusCode::OK);
            assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-encoding")));
            assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
        }
    }

    #[tokio::test]
    async fn legacy_hls_declared_only_preserves_headerless_gzip_magic() {
        let body = Bytes::from_static(b"\x1f\x8bnot-an-encoded-key");
        let (response, _) = local_response(StatusCode::OK, &[], body.to_vec()).await;

        let (stream, _) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .unwrap();

        assert_eq!(collect_stream(stream).await.unwrap(), body);
    }

    #[tokio::test]
    async fn legacy_hls_identity_keeps_unencoded_partial_content_headers() {
        const PARTIAL_BODY: &[u8] = b"abc";
        let (response, _) =
            local_response(StatusCode::PARTIAL_CONTENT, &[("Content-Range", "bytes 0-2/10")], PARTIAL_BODY.to_vec())
                .await;

        let (stream, info) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .expect("unencoded partial identity response is safe");
        let (headers, status, _, _) = info.expect("partial response metadata");

        assert_eq!(collect_stream(stream).await.expect("partial body streams"), PARTIAL_BODY);
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert!(headers.iter().any(|(name, value)| name == "content-length" && value == "3"));
        assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 0-2/10"));
        assert!(headers.iter().all(|(name, _)| name != "content-encoding"));
    }

    #[tokio::test]
    async fn legacy_hls_identity_decoding_removes_stale_representation_headers() {
        const IDENTITY_BODY: &[u8] = b"decoded identity response";
        let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
        let (response, _) =
            local_response(StatusCode::OK, &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-9/10")], encoded)
                .await;

        let (stream, info) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .expect("encoded full response decodes");
        let (headers, status, _, _) = info.expect("decoded response metadata");

        assert_eq!(collect_stream(stream).await.expect("decoded body streams"), IDENTITY_BODY);
        assert_eq!(status, StatusCode::OK);
        for name in ["content-encoding", "content-length", "content-range"] {
            assert!(headers.iter().all(|(key, _)| key != name), "stale header {name}");
        }
    }

    #[tokio::test]
    async fn legacy_hls_rejects_encoded_partial_content_before_client_streaming() {
        let encoded = encode(b"partial identity bytes", TestEncoding::Gzip).await;
        let (response, _) = local_response(
            StatusCode::PARTIAL_CONTENT,
            &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-9/10")],
            encoded,
        )
        .await;

        let result = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await;

        assert!(matches!(
            result,
            Err(ProviderStreamPreparationError::ContentCoding(ContentCodingError::EncodedPartialContent))
        ));
    }

    #[tokio::test]
    async fn legacy_hls_decoder_setup_obeys_provider_body_idle_timeout() {
        let (response, origin_task) = local_stalled_deflate_response().await;

        let result = prepare_provider_stream_response_with_idle_timeout(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
            Duration::from_millis(25),
        )
        .await;
        origin_task.abort();

        assert!(matches!(
            result,
            Err(ProviderStreamPreparationError::ContentCoding(ContentCodingError::PrefixRead(error)))
                if error.kind() == io::ErrorKind::TimedOut
        ));
    }

    #[tokio::test]
    async fn preserve_origin_keeps_encoded_body_and_representation_headers() {
        let encoded = encode(b"preserved representation", TestEncoding::Gzip).await;
        let expected = encoded.clone();
        let expected_len = encoded.len().to_string();
        let (response, _) = local_response(
            StatusCode::PARTIAL_CONTENT,
            &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 10-20/100")],
            encoded,
        )
        .await;

        let (stream, info) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::PreserveOrigin,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .unwrap();
        let (headers, status, _, _) = info.unwrap();

        assert_eq!(collect_stream(stream).await.unwrap(), expected);
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert!(headers.iter().any(|(name, value)| name == "content-encoding" && value == "gzip"));
        assert!(headers.iter().any(|(name, value)| name == "content-length" && value == &expected_len));
        assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 10-20/100"));
        assert!(headers.iter().all(|(name, _)| name != "transfer-encoding"));
    }

    #[tokio::test]
    async fn preserve_origin_keeps_unknown_content_coding_and_body_unchanged() {
        let body = b"opaque provider representation".to_vec();
        let expected = body.clone();
        let expected_len = body.len().to_string();
        let (response, _) = local_response(
            StatusCode::PARTIAL_CONTENT,
            &[("Content-Encoding", "x-provider-coding"), ("Content-Range", "bytes 0-29/100")],
            body,
        )
        .await;

        let (stream, info) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::PreserveOrigin,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .expect("valid unknown coding is preserved");
        let (headers, status, _, _) = info.expect("preserved response metadata");

        assert_eq!(collect_stream(stream).await.expect("opaque body streams"), expected);
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert!(headers.iter().any(|(name, value)| name == "content-encoding" && value == "x-provider-coding"));
        assert!(headers.iter().any(|(name, value)| name == "content-length" && value == &expected_len));
        assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 0-29/100"));
    }

    #[tokio::test]
    async fn preserve_origin_never_magic_sniffs_headerless_body() {
        let body = Bytes::from_static(b"\x1f\x8bopaque non-hls bytes");
        let (response, _) = local_response(StatusCode::OK, &[], body.to_vec()).await;

        let (stream, _) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::PreserveOrigin,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .unwrap();

        assert_eq!(collect_stream(stream).await.unwrap(), body);
    }

    #[tokio::test]
    async fn legacy_hls_stream_decoder_failure_aborts_without_retry() {
        let mut truncated = encode(b"decoder failure after response headers", TestEncoding::Gzip).await;
        truncated.truncate(truncated.len().saturating_sub(5));
        let (response, requests) = local_response(StatusCode::OK, &[("Content-Encoding", "gzip")], truncated).await;

        let (stream, _) = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .unwrap();
        let error = collect_stream(stream).await.expect_err("truncated gzip must terminate the body stream");

        assert!(matches!(error, StreamError::ContentDecoding(_)));
        assert_eq!(requests.load(Ordering::SeqCst), 1, "a body-stream failure cannot trigger a new request");
    }
}
