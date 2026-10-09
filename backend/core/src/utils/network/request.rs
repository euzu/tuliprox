pub use super::DynReader;

/// Seconds a stream may go without producing bytes before it is treated as idle.
///
/// Owned here because this is the client that enforces it; the streaming layer
/// re-exports it so both sides cannot drift apart.
pub const STREAM_IDLE_TIMEOUT: u64 = 60;

use crate::utils::content_coding::{ContentCodingDetection, OutboundContentCodingPolicy};
use reqwest::header::HeaderMap;
use std::{
    path::Path,
    sync::{Arc, Once},
    time::Duration,
};

static PROXY_DIAGNOSTICS_ONCE: Once = Once::new();

#[derive(Debug, Clone, Copy, Default)]
pub struct PublicIpResolver;

/// DNS layer of the proxied resource client.
///
/// Classifying a destination leaves a window: a name can resolve to a public address while it is
/// classified and to a local one while the connection is built. This resolver closes that window,
/// because the addresses it validates are the addresses reqwest connects to. It deliberately does not
/// consult [`DestinationCache`]: a remembered verdict would reopen exactly that window.
///
/// The proxy hosts are exempt: they are resolved without the guard, so a proxy on the loopback
/// interface stays usable. Nothing else is, because everything else is a destination a client picked.
#[derive(Debug, Clone, Default)]
pub struct ResourceDestinationResolver {
    allowed_hosts: Arc<[Arc<str>]>,
}

/// Reachability class of a resource destination.
///
/// Resource URLs are supplied by providers and third-party EPG data, so they are untrusted input.
/// The class answers two separate questions: may a client be handed the URL as-is (any non-public
/// destination would expose an internal address), and may the proxy fetch it (a destination local
/// to the proxy host would turn the proxy into a reader for the host itself).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ResourceDestination {
    /// Publicly routable: safe to expose to clients and to fetch.
    Public,
    /// Non-public but routable, which is normal for self-hosted services on the local network
    /// (RFC1918, unique local addresses, carrier NAT, reserved ranges). Never exposed to clients.
    Private,
    /// Local to the proxy host or its segment: loopback, link-local (including the cloud metadata
    /// endpoints), unspecified, multicast or broadcast. Never exposed and never fetched.
    Blocked,
}

/// How long a verdict that allows a URL to be handed to a client is remembered.
const RESOURCE_DESTINATION_PUBLIC_TTL: Duration = Duration::from_secs(60);

/// How long a verdict that keeps a destination away from clients is remembered. Both classes fail
/// closed, so they can be cached longer than the one that decides exposure.
const RESOURCE_DESTINATION_PRIVATE_TTL: Duration = Duration::from_secs(300);

/// How long a name that produced no answer within the resolution budget is remembered.
///
/// The cause - a resolver that is slow, overloaded, or briefly unreachable - is transient, so such a
/// name must not be pinned to a non-public verdict for minutes. It stays non-public while it is
/// remembered, because nothing was proven.
const RESOURCE_DESTINATION_UNANSWERED_TTL: Duration = Duration::from_secs(10);

/// Upper bound for remembered verdicts, evicted least-recently-used.
const RESOURCE_DESTINATION_CACHE_CAPACITY: usize = 4096;

/// Resolution budget when classifying a destination given as a name.
const RESOURCE_DESTINATION_LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);

/// Options applied at the final boundary of every physical request attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestFetchOptions {
    pub attempt_idle_timeout: Option<Duration>,
    content_coding: OutboundContentCodingPolicy,
    resource_retry: ResourceRetryExecution,
    return_http_errors: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RecordingTaskOptions {
    pub max_bytes: Option<u64>,
    pub atomic_write: bool,
}

pub struct InputEpgFileRequest<'a> {
    pub headers: Option<&'a HeaderMap>,
    pub storage_dir: &'a str,
    pub url: &'a str,
    pub persist_path: &'a Path,
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MimeCategory {
    Unknown,
    Video,
    M3U8,
    Image,
    Json,
    Xml,
    Text,
    Unclassified,
}

/// Response returned after applying provider URL failover without applying the generic resource retry policy.
pub struct ProviderFailoverResponse {
    pub response: reqwest::Response,
    pub provider_url_index: Option<usize>,
}

/// Controls decoding and bounded consumption of a fully buffered text response.
#[derive(Debug, Clone, Copy)]
pub struct TextContentBodyOptions {
    detection: ContentCodingDetection,
    max_decoded_bytes: Option<usize>,
    deadline: Option<Duration>,
    retry_owner: TextContentRetryOwner,
}

/// Groups request-boundary and decoded-text-consumer options for one text fetch.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextContentFetchOptions {
    request: RequestFetchOptions,
    body: TextContentBodyOptions,
}

#[cfg(test)]
mod tests;

mod attempt;
mod body;
mod client;
mod destination;
mod download;
mod headers;
mod options;
mod protocol;
mod redirect;
mod retry;

#[cfg(test)]
use self::attempt::resolve_attempt_target;
pub(crate) use self::destination::classify_host;
#[cfg(test)]
use self::destination::{proxy_hosts, verdict_for_addresses};
#[cfg(test)]
use self::download::create_atomic_download_file;
#[cfg(test)]
use self::retry::{next_provider_url_index, should_try_next_ip_on_connect_error};
use self::{
    attempt::{
        execute_attempt_request, prepare_physical_request_attempt, preview_attempt_target,
        resolve_attempt_target_at_provider_index, AttemptTarget,
    },
    body::{
        get_remote_content_with_headers_and_options, get_remote_content_with_options,
        read_text_response_with_body_options, text_response_status_error,
    },
    headers::{format_request_target_for_logging, log_proxy_diagnostics, prepare_input_request_headers},
    options::{apply_request_fetch_options, ResourceRetryExecution, TextContentRetryOwner},
    redirect::{is_failover_redirect, same_origin, strip_sensitive_headers_for_cross_origin_redirect},
    retry::{
        is_retryable_text_response_status, perform_backoff, provider_start_index, should_retry_text_body_error,
        sleep_before_text_body_retry, text_body_retry_values,
    },
};
pub use self::{
    body::{get_remote_content_as_stream, text_response_error_log_label},
    client::{create_client, create_client_with_redirect, create_tmdb_client, get_client_request},
    destination::{classify_ip, is_public_ip, resolve_public_socket_addrs, resolve_resource_socket_addrs},
    download::{
        classify_resource_destination, download_text_content, download_text_content_as_stream,
        download_text_content_with_headers, download_text_content_with_headers_and_options,
        download_text_content_with_options, get_input_epg_content_as_file, get_input_json_content,
        get_input_json_content_as_stream, get_input_text_content_as_stream, get_local_file_content,
        get_local_file_content_as_stream, get_remote_content_as_file, get_remote_content_as_file_with_options,
    },
    headers::{
        append_user_agent_stream_index, get_request_headers, overlay_upstream_user_agent,
        preview_request_diagnostics_for_logging, preview_request_target_for_logging,
    },
    protocol::{classify_content_type, content_type_from_ext, format_http_status, is_file_url, is_uri, parse_range},
    redirect::{
        download_text_content_with_manual_redirects, download_text_content_with_manual_redirects_and_headers,
        download_text_content_with_manual_redirects_and_headers_and_options,
        download_text_content_with_manual_redirects_and_options, is_safe_cross_origin_redirect_header,
    },
    retry::{
        calculate_retry_backoff, send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
        send_input_with_retry_and_provider_policy_with_options_result, send_with_retry_and_provider,
        send_with_retry_and_provider_policy, send_with_retry_and_provider_policy_with_options, should_trigger_failover,
    },
};
