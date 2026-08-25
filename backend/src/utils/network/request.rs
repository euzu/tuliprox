pub use super::DynReader;

/// Seconds a stream may go without producing bytes before it is treated as idle.
///
/// Owned here because this is the client that enforces it; the streaming layer
/// re-exports it so both sides cannot drift apart.
pub const STREAM_IDLE_TIMEOUT: u64 = 60;
use crate::{
    api::model::{
        log_hls_origin_content_coding, persist_pipe_stream::tee_dyn_reader, AppState, HlsOriginContentCodingObjectKind,
        HlsOriginContentCodingSource,
    },
    model::{
        resolve_provider_scheme_url_with_provider_index, AppConfig, Config, ConfigInput, ConfigProvider, InputSource,
        ResourceRetryConfig, ReverseProxyDisabledHeaderConfig,
    },
    utils::{
        async_file_reader, async_file_writer,
        compression::compression_utils::is_gzip,
        content_coding::{
            apply_outbound_content_coding_policy, content_decoding_error_from_io, decode_response_to_identity,
            is_http_body_transport_error, read_utf8_limited, ContentBodyReadError, ContentCodingDetection,
            ContentCodingError, OutboundContentCodingPolicy,
        },
        debug_if_enabled, get_file_path, persist_file,
    },
};
use futures::StreamExt;
use log::{debug, error, log_enabled, trace, warn, Level};
use regex::Regex;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, HOST},
    redirect::Policy,
    StatusCode,
};
use shared::{
    defaults::DEFAULT_USER_AGENT,
    error::{string_to_io_error, TuliproxError},
    model::{format_elapsed_time, InputFetchMethod, OnConnectErrorPolicy, ProviderUrlSelectionPolicy},
    utils::{filter_request_header, human_readable_byte_size, sanitize_sensitive_info, CONTENT_TYPE_JSON},
};
use std::{
    collections::{HashMap, HashSet},
    io::{Error, ErrorKind},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Once},
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    time::sleep,
};
use url::Url;

static PROXY_DIAGNOSTICS_ONCE: Once = Once::new();

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PublicIpResolver;

impl reqwest::dns::Resolve for PublicIpResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = resolve_public_socket_addrs(&host, 0)
                .await
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(crate) async fn resolve_public_socket_addrs(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        tokio::net::lookup_host((host, port)).await?.collect()
    };
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "destination resolves to a non-public address",
        ));
    }
    Ok(addresses)
}

pub(crate) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || address.is_unspecified()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && matches!(b, 18 | 19))
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || segments[0] & 0xfe00 == 0xfc00
        || segments[0] & 0xffc0 == 0xfe80
        || segments[0] & 0xffc0 == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        && address.to_ipv4_mapped().is_none_or(is_public_ipv4)
}

/// Options applied at the final boundary of every physical request attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestFetchOptions {
    pub attempt_idle_timeout: Option<Duration>,
    content_coding: OutboundContentCodingPolicy,
    resource_retry: ResourceRetryExecution,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ResourceRetryExecution {
    #[default]
    Configured,
    ProviderFailoverOnly,
}

impl RequestFetchOptions {
    pub fn with_attempt_idle_timeout(timeout: Duration) -> Self {
        Self { attempt_idle_timeout: Some(timeout.max(Duration::from_millis(1))), ..Self::default() }
    }

    pub(crate) const fn with_content_coding(mut self, content_coding: OutboundContentCodingPolicy) -> Self {
        self.content_coding = content_coding;
        self
    }

    /// Leaves bounded provider failover intact while assigning retry rounds to the caller.
    pub(crate) const fn without_resource_retries(mut self) -> Self {
        self.resource_retry = ResourceRetryExecution::ProviderFailoverOnly;
        self
    }

    fn attempt_idle_timeout_or_default(self) -> Duration {
        self.attempt_idle_timeout.unwrap_or_else(|| Duration::from_secs(STREAM_IDLE_TIMEOUT))
    }

    const fn uses_provider_failover_only(self) -> bool {
        matches!(self.resource_retry, ResourceRetryExecution::ProviderFailoverOnly)
    }
}

fn apply_request_fetch_options(request: &mut reqwest::Request, options: RequestFetchOptions) {
    if let Some(timeout) = options.attempt_idle_timeout {
        *request.timeout_mut() = Some(timeout);
    }
    apply_outbound_content_coding_policy(request.headers_mut(), options.content_coding);
}

fn prepare_physical_request_attempt(
    request_builder: reqwest::RequestBuilder,
    target: &AttemptTarget,
    options: RequestFetchOptions,
) -> Result<(reqwest::Client, reqwest::Request), std::io::Error> {
    let (base_client, request_result) = request_builder.build_split();
    let mut request = request_result.map_err(|error| {
        string_to_io_error(format!("Failed to build request: {}", sanitize_sensitive_info(error.to_string().as_str())))
    })?;
    apply_attempt_to_request(&mut request, target)?;
    apply_request_fetch_options(&mut request, options);
    Ok((base_client, request))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileDownloadOptions {
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

fn log_proxy_diagnostics(config: &Config) {
    PROXY_DIAGNOSTICS_ONCE.call_once(|| {
        if let Some(proxy_cfg) = config.proxy.as_ref() {
            let sanitized_url = sanitize_sensitive_info(proxy_cfg.url.as_str());
            let has_inline_credentials = proxy_cfg
                .url
                .contains('@')
                || proxy_cfg.url.contains("://")
                && proxy_cfg
                .url
                .split("://")
                .nth(1)
                .is_some_and(|part| part.contains('@'));
            let has_explicit_credentials =
                proxy_cfg.username.as_ref().is_some() || proxy_cfg.password.as_ref().is_some();
            debug!(
                "Proxy config enabled: url={sanitized_url}, credentials_inline={has_inline_credentials}, credentials_fields={has_explicit_credentials}"
            );
        } else {
            debug!("Proxy config disabled (config.yml)");
        }

        let env_keys = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ];
        let mut env_values = Vec::new();
        for key in env_keys {
            if let Ok(value) = std::env::var(key) {
                if !value.trim().is_empty() {
                    env_values.push((key, sanitize_sensitive_info(value.as_str()).to_string()));
                }
            }
        }
        if env_values.is_empty() {
            debug!("Proxy env vars not set");
        } else {
            debug!("Proxy env vars present: {env_values:?}");
        }
    });
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

pub fn classify_content_type(headers: &[(String, String)]) -> MimeCategory {
    headers.iter().find_map(|(k, v)| (k == axum::http::header::CONTENT_TYPE.as_str()).then_some(v)).map_or(
        MimeCategory::Unknown,
        |v| match v.to_lowercase().as_str() {
            v if v.starts_with("video/") || v == "application/octet-stream" => MimeCategory::Video,
            v if v.contains("mpegurl") => MimeCategory::M3U8,
            v if v.starts_with("image/") => MimeCategory::Image,
            v if v.starts_with(CONTENT_TYPE_JSON) || v.ends_with("+json") => MimeCategory::Json,
            v if v.starts_with("application/xml") || v.ends_with("+xml") || v == "text/xml" => MimeCategory::Xml,
            v if v.starts_with("text/") => MimeCategory::Text,
            _ => MimeCategory::Unclassified,
        },
    )
}

pub fn format_http_status(status: StatusCode) -> String {
    let code = status.as_u16();
    match status.canonical_reason() {
        Some(reason) => format!("{code} {reason}"),
        None => code.to_string(),
    }
}

pub fn content_type_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}

fn resolve_provider_url_for_attempt(
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    provider_url_index: usize,
) -> Url {
    let Some(provider) = provider else {
        return url.clone();
    };

    match resolve_provider_scheme_url_with_provider_index(url.as_str(), Some(provider.clone()), provider_url_index) {
        Ok((_provider, resolved)) => {
            if resolved.as_ref() == url.as_str() {
                return url.clone();
            }
            Url::parse(resolved.as_ref()).unwrap_or_else(|_| url.clone())
        }
        Err(err) => {
            debug!("Failed to resolve provider URL: {err}");
            url.clone()
        }
    }
}

#[derive(Debug, Clone)]
struct AttemptTarget {
    request_url: Url,
    effective_url: Url,
    host_header: Option<String>,
    sni_host: Option<String>,
    connect_ip: Option<IpAddr>,
    dns_host: Option<String>,
}

impl AttemptTarget {
    fn new(url: Url) -> Self {
        Self {
            request_url: url.clone(),
            effective_url: url,
            host_header: None,
            sni_host: None,
            connect_ip: None,
            dns_host: None,
        }
    }
}

fn is_ip_literal(host: &str) -> bool { host.parse::<IpAddr>().is_ok() }

fn format_host_header_with_port(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn format_ip_host_header_with_port(ip: IpAddr, port: Option<u16>) -> String {
    match (ip, port) {
        (IpAddr::V4(addr), Some(port)) => format!("{addr}:{port}"),
        (IpAddr::V4(addr), None) => addr.to_string(),
        (IpAddr::V6(addr), Some(port)) => format!("[{addr}]:{port}"),
        (IpAddr::V6(addr), None) => format!("[{addr}]"),
    }
}

fn resolve_attempt_target_with_dns_mode(
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    preview_dns_selection: bool,
    provider_url_index: usize,
) -> AttemptTarget {
    let resolved_url = resolve_provider_url_for_attempt(url, provider, provider_url_index);
    let Some(provider) = provider else {
        return AttemptTarget::new(resolved_url);
    };

    let mut target = AttemptTarget::new(resolved_url.clone());
    let scheme = resolved_url.scheme();
    if !provider.dns_enabled_for_scheme(scheme) {
        return target;
    }

    let Some(host) = resolved_url.host_str() else {
        return target;
    };
    if is_ip_literal(host) {
        return target;
    }

    let connect_ip =
        if preview_dns_selection { provider.preview_ip_for_host(host) } else { provider.select_ip_for_host(host) };
    let Some(connect_ip) = connect_ip else {
        return target;
    };
    let keep_vhost = provider.get_dns_config().is_some_and(|dns| dns.keep_vhost);
    let host_header = if keep_vhost {
        format_host_header_with_port(host, resolved_url.port())
    } else {
        format_ip_host_header_with_port(connect_ip, resolved_url.port())
    };

    target.host_header = Some(host_header);
    target.connect_ip = Some(connect_ip);
    target.dns_host = Some(host.to_ascii_lowercase());

    if scheme.eq_ignore_ascii_case("https") {
        target.sni_host = Some(host.to_string());
        return target;
    }

    if scheme.eq_ignore_ascii_case("http") {
        let mut effective = resolved_url.clone();
        if effective.set_ip_host(connect_ip).is_ok() {
            target.effective_url = effective;
        }
    }

    target
}

#[cfg(test)]
fn resolve_attempt_target(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, false, 0)
}

fn resolve_attempt_target_at_provider_index(
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    provider_url_index: usize,
) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, false, provider_url_index)
}

fn preview_attempt_target(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, true, provider_start_index(provider))
}

fn provider_start_index(provider: Option<&Arc<ConfigProvider>>) -> usize {
    provider.map_or(0, |provider| match provider.provider_url_selection_policy() {
        ProviderUrlSelectionPolicy::ResumeLastWorking => provider.get_current_index(),
        ProviderUrlSelectionPolicy::RestartFromFirst => 0,
    })
}

fn next_provider_url_index(current_index: usize, provider_url_count: usize, start_index: usize) -> Option<usize> {
    if provider_url_count <= 1 {
        return None;
    }

    let next_index = (current_index + 1) % provider_url_count;
    (next_index != start_index).then_some(next_index)
}

fn provider_cycle_exhausted(provider: &ConfigProvider, current_index: usize, start_index: usize) -> bool {
    next_provider_url_index(current_index, provider.urls.len(), start_index).is_none()
}

fn log_provider_cycle_exhausted(
    provider: &ConfigProvider,
    start_index: usize,
    current_index: usize,
    last_failure: &str,
) {
    error!(
        "Provider '{}' exhausted all {} URL(s) after one full cycle starting at preferred index {} and ending at index {}: {}",
        provider.name,
        provider.urls.len(),
        start_index,
        current_index,
        sanitize_sensitive_info(last_failure)
    );
}

fn rotate_to_next_provider_url(
    provider: &ConfigProvider,
    provider_url_index: &mut usize,
    start_provider_index: usize,
    reason: &str,
) -> bool {
    let Some(next_index) = next_provider_url_index(*provider_url_index, provider.urls.len(), start_provider_index)
    else {
        return false;
    };

    warn!(
        "Provider '{}' failover: {} -> switching from URL index {} to {}",
        provider.name,
        sanitize_sensitive_info(reason),
        *provider_url_index,
        next_index
    );
    *provider_url_index = next_index;
    true
}

fn format_request_target_for_logging(target: &AttemptTarget) -> String {
    if target.effective_url.scheme().eq_ignore_ascii_case("https") {
        if let Some(connect_ip) = target.connect_ip {
            format!("{} (connect_ip={connect_ip})", target.request_url)
        } else {
            target.request_url.to_string()
        }
    } else {
        target.effective_url.to_string()
    }
}

pub fn preview_request_target_for_logging(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> String {
    let target = preview_attempt_target(url, provider);
    format_request_target_for_logging(&target)
}

pub fn preview_request_diagnostics_for_logging(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> String {
    let target = preview_attempt_target(url, provider);
    let mut parts = vec![
        format!("request_url={}", sanitize_sensitive_info(target.request_url.as_str())),
        format!("effective_url={}", sanitize_sensitive_info(target.effective_url.as_str())),
    ];

    if let Some(host_header) = target.host_header.as_ref() {
        parts.push(format!("host_header={}", sanitize_sensitive_info(host_header)));
    }
    if let Some(connect_ip) = target.connect_ip {
        parts.push(format!("connect_ip={}", sanitize_sensitive_info(&connect_ip.to_string())));
    }
    if let Some(sni_host) = target.sni_host.as_ref() {
        parts.push(format!("sni_host={}", sanitize_sensitive_info(sni_host)));
    }

    parts.join(", ")
}

fn should_try_next_ip_on_connect_error(
    provider: Option<&Arc<ConfigProvider>>,
    target: &AttemptTarget,
    attempted_ips: &mut HashSet<IpAddr>,
) -> bool {
    let Some(provider) = provider else {
        return false;
    };
    let Some(connect_ip) = target.connect_ip else {
        return false;
    };
    let Some(dns_host) = target.dns_host.as_ref() else {
        return false;
    };
    let Some(dns_cfg) = provider.get_dns_config() else {
        return false;
    };
    if dns_cfg.on_connect_error != OnConnectErrorPolicy::TryNextIp {
        return false;
    }

    let inserted = attempted_ips.insert(connect_ip);
    if !inserted {
        return false;
    }

    let total_ips = provider.ip_count_for_host(dns_host);
    total_ips > attempted_ips.len()
}

fn apply_attempt_to_request(request: &mut reqwest::Request, target: &AttemptTarget) -> Result<(), std::io::Error> {
    if request.url().as_str() != target.effective_url.as_str() {
        *request.url_mut() = target.effective_url.clone();
    }
    if let Some(host_header) = target.host_header.as_ref() {
        let host = HeaderValue::from_str(host_header)
            .map_err(|err| string_to_io_error(format!("Invalid host header '{host_header}': {err}")))?;
        request.headers_mut().insert(HOST, host);
    }
    Ok(())
}

fn build_https_attempt_client(
    app_config: &Arc<AppConfig>,
    sni_host: &str,
    connect_ip: IpAddr,
    connect_port: u16,
) -> Result<reqwest::Client, reqwest::Error> {
    let config = app_config.config.load();
    let mut builder = create_client(app_config).http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    drop(config);
    builder = builder.resolve_to_addrs(sni_host, &[SocketAddr::new(connect_ip, connect_port)]);
    builder.build()
}

async fn execute_attempt_request(
    app_config: &Arc<AppConfig>,
    base_client: reqwest::Client,
    request: reqwest::Request,
    target: &AttemptTarget,
) -> Result<reqwest::Response, reqwest::Error> {
    if target.effective_url.scheme().eq_ignore_ascii_case("https") {
        if let (Some(sni_host), Some(connect_ip)) = (target.sni_host.as_ref(), target.connect_ip) {
            let connect_port = target.effective_url.port_or_known_default().unwrap_or(443);
            let https_client = build_https_attempt_client(app_config, sni_host.as_str(), connect_ip, connect_port)?;
            return https_client.execute(request).await;
        }
    }
    base_client.execute(request).await
}

/// Response returned after applying provider URL failover without applying the generic resource retry policy.
pub(crate) struct ProviderFailoverResponse {
    pub(crate) response: reqwest::Response,
    pub(crate) provider_url_index: Option<usize>,
}

#[allow(clippy::too_many_lines)]
async fn send_with_provider_failover_only_with_options(
    app_config: &Arc<AppConfig>,
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    options: RequestFetchOptions,
    mut send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<ProviderFailoverResponse, std::io::Error> {
    let failover_patterns = app_config.config.load().reverse_proxy.as_ref().map_or_else(
        || ResourceRetryConfig::default().failover_redirect_patterns,
        |rp| rp.resource_retry.failover_redirect_patterns.clone(),
    );

    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    'provider_loop: loop {
        let mut attempted_dns_ips = HashSet::new();

        'ip_loop: loop {
            let attempt_target = resolve_attempt_target_at_provider_index(url, provider, provider_url_index);
            if log_enabled!(Level::Debug) {
                if let Some(current_provider) = provider {
                    let attempt_target_log = format_request_target_for_logging(&attempt_target);
                    debug!(
                        "Provider '{}' acquiring URL index {} of {}: {}",
                        current_provider.name,
                        provider_url_index,
                        current_provider.urls.len(),
                        sanitize_sensitive_info(attempt_target_log.as_str())
                    );
                }
            }

            idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
            let (base_client, request) =
                prepare_physical_request_attempt(send(&attempt_target.request_url), &attempt_target, options)?;

            tokio::select! {
                () = &mut idle => {
                    if should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips) {
                        continue 'ip_loop;
                    }

                    let last_provider_failure = format!(
                        "idle timeout while trying {}",
                        sanitize_sensitive_info(attempt_target.request_url.as_str())
                    );
                    if let Some(current_provider) = provider {
                        if rotate_to_next_provider_url(
                            current_provider.as_ref(),
                            &mut provider_url_index,
                            start_provider_index,
                            "idle timeout",
                        ) {
                            continue 'provider_loop;
                        }
                        log_provider_cycle_exhausted(
                            current_provider.as_ref(),
                            start_provider_index,
                            provider_url_index,
                            &last_provider_failure,
                        );
                    }

                    return Err(Error::new(
                        ErrorKind::TimedOut,
                        format!("Request timed out: {}", sanitize_sensitive_info(url.as_str())),
                    ));
                }
                result = execute_attempt_request(app_config, base_client, request, &attempt_target) => match result {
                Ok(response) => {
                    let status = response.status();
                    if allow_redirects && status.is_redirection() {
                        if let Some(current_provider) = provider {
                            current_provider.set_current_index(provider_url_index);
                        }
                        return Ok(ProviderFailoverResponse {
                            response,
                            provider_url_index: provider.map(|_| provider_url_index),
                        });
                    }

                    let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                    if !is_failover && !should_trigger_failover(status) {
                        if status.is_success() {
                            if let Some(current_provider) = provider {
                                current_provider.set_current_index(provider_url_index);
                            }
                        }
                        return Ok(ProviderFailoverResponse {
                            response,
                            provider_url_index: provider.map(|_| provider_url_index),
                        });
                    }

                    let last_provider_failure = format!(
                        "status {} while trying {}",
                        format_http_status(status),
                        sanitize_sensitive_info(attempt_target.request_url.as_str())
                    );

                    if let Some(current_provider) = provider {
                        let reason = format!("status {}", format_http_status(status));
                        if rotate_to_next_provider_url(
                            current_provider.as_ref(),
                            &mut provider_url_index,
                            start_provider_index,
                            reason.as_str(),
                        ) {
                            continue 'provider_loop;
                        }
                        log_provider_cycle_exhausted(
                            current_provider.as_ref(),
                            start_provider_index,
                            provider_url_index,
                            &last_provider_failure,
                        );
                    }

                    return Ok(ProviderFailoverResponse {
                        response,
                        provider_url_index: provider.map(|_| provider_url_index),
                    });
                },
                Err(err) => {
                    if (err.is_timeout() || err.is_connect())
                        && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                    {
                        continue 'ip_loop;
                    }

                    let last_provider_failure = format!(
                        "connection error while trying {}: {}",
                        sanitize_sensitive_info(attempt_target.request_url.as_str()),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );

                    if err.is_timeout() || err.is_connect() {
                        if let Some(current_provider) = provider {
                            if rotate_to_next_provider_url(
                                current_provider.as_ref(),
                                &mut provider_url_index,
                                start_provider_index,
                                "connection error",
                            ) {
                                continue 'provider_loop;
                            }
                            log_provider_cycle_exhausted(
                                current_provider.as_ref(),
                                start_provider_index,
                                provider_url_index,
                                &last_provider_failure,
                            );
                        }
                    }

                    let message = format!("Request error: {}", sanitize_sensitive_info(err.to_string().as_str()));
                    return Err(if err.is_timeout() {
                        Error::new(ErrorKind::TimedOut, message)
                    } else if err.is_connect() {
                        Error::new(ErrorKind::ConnectionRefused, message)
                    } else {
                        string_to_io_error(message)
                    });
                },
                }
            }
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub fn calculate_retry_backoff(base_delay_ms: u64, multiplier: f64, attempt: u32) -> u64 {
    let base = base_delay_ms.max(1);
    if multiplier <= 1.0 {
        return base;
    }
    let delay = (base as f64) * multiplier.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
    if !delay.is_finite() || delay < 1.0 {
        base
    } else if delay >= u64::MAX as f64 {
        u64::MAX
    } else {
        delay as u64
    }
}

/// Sends a request with retry logic and optional provider failover support.
pub async fn send_with_retry_and_provider(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options(
        app_config,
        url,
        provider,
        allow_redirects,
        true,
        RequestFetchOptions::default(),
        send,
    )
    .await
}

/// Canonical retry and provider-failover entry point for outbound resource requests.
///
/// `send_with_retry_and_provider` is a thin wrapper that enables the standard retry policy. Retry attempt counts,
/// backoff values, and failover redirect patterns are sourced from `AppConfig` (`reverse_proxy.resource_retry`). The
/// `url` argument is used as the stable logging/context URL; callers should pass the original request target rather
/// than an already-rotated provider URL.
///
/// When `retry_enabled` is `false`, this function forces `max_attempts` to 1, disables provider URL rotation for idle
/// timeouts, retryable HTTP statuses, and connection/timeout errors, and skips the final fallback provider rotation
/// after attempts are exhausted.
#[allow(clippy::too_many_lines)]
pub async fn send_with_retry_and_provider_policy(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options(
        app_config,
        url,
        provider,
        allow_redirects,
        retry_enabled,
        RequestFetchOptions::default(),
        send,
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn send_with_retry_and_provider_policy_with_options(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    options: RequestFetchOptions,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options_result(
        app_config,
        url,
        provider,
        allow_redirects,
        retry_enabled,
        options,
        send,
    )
    .await
    .map(|result| result.response)
}

#[allow(clippy::too_many_lines)]
async fn send_with_retry_and_provider_policy_with_options_result(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    options: RequestFetchOptions,
    mut send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<ProviderFailoverResponse, std::io::Error> {
    if options.uses_provider_failover_only() {
        return send_with_provider_failover_only_with_options(
            app_config,
            url,
            provider,
            allow_redirects,
            options,
            send,
        )
        .await;
    }

    let config = app_config.config.load();
    let (max_attempts, backoff_ms, backoff_multiplier, failover_patterns) = config.reverse_proxy.as_ref().map_or_else(
        || {
            let (a, b, c) = ResourceRetryConfig::get_default_retry_values();
            (a, b, c, ResourceRetryConfig::default().failover_redirect_patterns)
        },
        |rp| {
            let (a, b, c) = rp.resource_retry.get_retry_values();
            (a, b, c, rp.resource_retry.failover_redirect_patterns.clone())
        },
    );
    let max_attempts = if retry_enabled { max_attempts } else { 1 };
    drop(config);

    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    let max_provider_attempts = provider.as_ref().map_or(0, |p| p.urls.len());
    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let mut last_provider_failure: Option<String> = None;

    'provider_loop: loop {
        // 2. Retry loop for the current URL
        'attempt_loop: for attempt in 0..max_attempts {
            let mut attempted_dns_ips = HashSet::new();

            'ip_loop: loop {
                let attempt_target = resolve_attempt_target_at_provider_index(url, provider, provider_url_index);
                if log_enabled!(Level::Debug) {
                    if let Some(current_provider) = provider {
                        let attempt_target_log = format_request_target_for_logging(&attempt_target);
                        debug!(
                            "Provider '{}' attempting URL index {} of {}: {}",
                            current_provider.name,
                            provider_url_index,
                            max_provider_attempts,
                            sanitize_sensitive_info(attempt_target_log.as_str())
                        );
                    }
                }
                // Reset the idle timer for a new attempt
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let (base_client, request) =
                    prepare_physical_request_attempt(send(&attempt_target.request_url), &attempt_target, options)?;

                tokio::select! {
                    () = &mut idle => {
                        warn!("Request idle for too long: {}", sanitize_sensitive_info(url.as_str()));
                        last_provider_failure = Some(format!(
                            "idle timeout while trying {}",
                            sanitize_sensitive_info(attempt_target.request_url.as_str())
                        ));
                        // 1. Try Provider Failover first
                        let mut provider_failover_exhausted = false;
                        if retry_enabled {
                            if let Some(current_provider) = provider {
                                if rotate_to_next_provider_url(
                                    current_provider.as_ref(),
                                    &mut provider_url_index,
                                    start_provider_index,
                                    "idle timeout",
                                ) {
                                    continue 'provider_loop;
                                }
                            provider_failover_exhausted =
                                max_provider_attempts > 0 && provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index);
                            }
                        }

                        // 2. If no provider or rotation failed, check if we can retry the same URL
                        if attempt < max_attempts - 1 {
                            let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                            warn!("Idle timeout, retrying same URL in {}ms (attempt {})", delay, attempt + 1);
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            continue 'attempt_loop;
                        }

                        if provider_failover_exhausted {
                            if let Some(current_provider) = provider {
                                log_provider_cycle_exhausted(
                                    current_provider.as_ref(),
                                    start_provider_index,
                                    provider_url_index,
                                    last_provider_failure.as_deref().unwrap_or("idle timeout"),
                                );
                            }
                        }

                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            format!(
                                "Request timed out and no retries left: {}",
                                sanitize_sensitive_info(url.as_str())
                            ),
                        ));
                    }

                    result = execute_attempt_request(app_config, base_client, request, &attempt_target) => {
                        match result {
                            Ok(response) => {
                                let status = response.status();
                                if allow_redirects && status.is_redirection() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }
                                let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                                if !is_failover && status.is_success() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                last_provider_failure = Some(format!(
                                    "status {} while trying {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(attempt_target.request_url.as_str())
                                ));

                                // Failover check: Should we switch to the next provider URL?
                                let provider_failover_exhausted = retry_enabled
                                    && (is_failover || should_trigger_failover(status))
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if retry_enabled && (is_failover || should_trigger_failover(status)) {
                                    if let Some(current_provider) = provider {
                                        let reason = format!("status {}", format_http_status(status));
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            reason.as_str(),
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                // Standard retry check for the same URL
                                let is_retryable = status.is_server_error()
                                    || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT);

                                if attempt < max_attempts - 1 && is_retryable {
                                    perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request failed"),
                                        );
                                    }
                                }

                                return Err(string_to_io_error(format!("Request failed ({}): {}",
                                    format_http_status(status), sanitize_sensitive_info(url.as_str()))));
                            }

                            Err(err) => {
                                // For DNS IP-connect policy, attempt next IP before provider URL rotation.
                                if retry_enabled
                                    && (err.is_timeout() || err.is_connect())
                                    && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                                {
                                    continue 'ip_loop;
                                }

                                last_provider_failure = Some(format!(
                                    "connection error while trying {}: {}",
                                    sanitize_sensitive_info(attempt_target.request_url.as_str()),
                                    sanitize_sensitive_info(&err.to_string())
                                ));

                                // Connection errors (Timeout/Connect) trigger failover if provider exists
                                let provider_failover_exhausted = retry_enabled
                                    && (err.is_timeout() || err.is_connect())
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if retry_enabled && (err.is_timeout() || err.is_connect()) {
                                    if let Some(current_provider) = provider {
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            "connection error",
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                // If not a provider or rotation failed, try standard retry
                                if (err.is_timeout() || err.is_connect()) && attempt < max_attempts - 1 {
                                    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                                    tokio::time::sleep(Duration::from_millis(delay)).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request error"),
                                        );
                                    }
                                }

                                let error_message = format!(
                                    "Request error: {}",
                                    sanitize_sensitive_info(&err.to_string())
                                );
                                return Err(if err.is_timeout() {
                                    Error::new(ErrorKind::TimedOut, error_message)
                                } else {
                                    string_to_io_error(error_message)
                                });
                            }
                        }
                    }
                }
            }
        }

        // 2. If per-URL retries are exhausted, try next provider URL as a last resort
        if retry_enabled {
            if let Some(current_provider) = provider {
                if rotate_to_next_provider_url(
                    current_provider.as_ref(),
                    &mut provider_url_index,
                    start_provider_index,
                    "retries exhausted for current URL",
                ) {
                    continue 'provider_loop;
                }

                if max_provider_attempts > 0 {
                    let last_failure =
                        last_provider_failure.as_deref().unwrap_or("all attempts and providers exhausted");
                    log_provider_cycle_exhausted(
                        current_provider.as_ref(),
                        start_provider_index,
                        provider_url_index,
                        last_failure,
                    );
                }
            }
        }

        break;
    }

    Err(string_to_io_error("All attempts and providers exhausted"))
}

fn prepare_input_request_headers(
    app_config: &Arc<AppConfig>,
    input: &InputSource,
    headers: Option<&HeaderMap>,
) -> (HashMap<String, String>, Option<String>) {
    let custom_headers = headers
        .map(|h| h.iter().map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec())).collect::<HashMap<_, _>>());

    let config = app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    let disabled_headers = config.get_disabled_headers();
    drop(config);

    let merged = get_request_headers(
        Some(&input.headers),
        custom_headers.as_ref(),
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );

    let request_headers: HashMap<String, String> = merged
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();

    (request_headers, default_user_agent)
}

#[allow(clippy::implicit_hasher)]
pub(crate) async fn send_input_with_retry_and_provider_policy_with_options_result(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<ProviderFailoverResponse, Error> {
    let (request_headers, default_user_agent) = prepare_input_request_headers(app_config, input, headers);
    send_with_retry_and_provider_policy_with_options_result(
        app_config,
        url,
        input.get_provider(),
        false,
        true,
        options,
        |resolved_url| {
            get_client_request(
                client,
                input.method,
                Some(&request_headers),
                resolved_url,
                None,
                None,
                default_user_agent.as_deref(),
            )
        },
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::implicit_hasher)]
pub(crate) async fn send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<ProviderFailoverResponse, Error> {
    let config = app_config.config.load();
    let (configured_max_attempts, backoff_ms, backoff_multiplier, failover_patterns) =
        config.reverse_proxy.as_ref().map_or_else(
            || {
                let (a, b, c) = ResourceRetryConfig::get_default_retry_values();
                (a, b, c, ResourceRetryConfig::default().failover_redirect_patterns)
            },
            |rp| {
                let (a, b, c) = rp.resource_retry.get_retry_values();
                (a, b, c, rp.resource_retry.failover_redirect_patterns.clone())
            },
        );
    drop(config);
    let provider_failover_only = options.uses_provider_failover_only();
    let max_attempts = if provider_failover_only { 1 } else { configured_max_attempts };

    let (base_headers, default_user_agent) = prepare_input_request_headers(app_config, input, headers);
    let provider = input.get_provider();
    let max_provider_attempts = provider.as_ref().map_or(0, |p| p.urls.len());
    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let mut last_provider_failure: Option<String> = None;
    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    'provider_loop: loop {
        'attempt_loop: for attempt in 0..max_attempts {
            let mut current_url = url.clone();
            let mut current_headers = base_headers.clone();
            let mut remaining_redirects = max_redirects;
            let mut attempted_dns_ips = HashSet::new();

            'redirect_loop: loop {
                let attempt_target =
                    resolve_attempt_target_at_provider_index(&current_url, provider, provider_url_index);
                if log_enabled!(Level::Debug) {
                    if let Some(current_provider) = provider {
                        let attempt_target_log = format_request_target_for_logging(&attempt_target);
                        debug!(
                            "Provider '{}' attempting URL index {} of {}: {}",
                            current_provider.name,
                            provider_url_index,
                            max_provider_attempts,
                            sanitize_sensitive_info(attempt_target_log.as_str())
                        );
                    }
                }
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let request_builder = get_client_request(
                    client,
                    input.method,
                    Some(&current_headers),
                    &attempt_target.request_url,
                    None,
                    None,
                    default_user_agent.as_deref(),
                );
                let (base_client, request) =
                    prepare_physical_request_attempt(request_builder, &attempt_target, options)?;

                tokio::select! {
                    () = &mut idle => {
                        warn!("Request idle for too long: {}", sanitize_sensitive_info(url.as_str()));
                        last_provider_failure = Some(format!(
                            "idle timeout while trying {}",
                            sanitize_sensitive_info(attempt_target.request_url.as_str())
                        ));
                        if let Some(current_provider) = provider {
                            if rotate_to_next_provider_url(
                                current_provider.as_ref(),
                                &mut provider_url_index,
                                start_provider_index,
                                "idle timeout",
                            ) {
                                continue 'provider_loop;
                            }
                            if max_provider_attempts > 0 {
                                log_provider_cycle_exhausted(
                                    current_provider.as_ref(),
                                    start_provider_index,
                                    provider_url_index,
                                    last_provider_failure.as_deref().unwrap_or("idle timeout"),
                                );
                            }
                        }

                        if attempt < max_attempts - 1 {
                            let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                            warn!("Idle timeout, retrying same URL in {}ms (attempt {})", delay, attempt + 1);
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            continue 'attempt_loop;
                        }

                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            format!(
                                "Request timed out and no retries left: {}",
                                sanitize_sensitive_info(url.as_str())
                            ),
                        ));
                    }

                    result = execute_attempt_request(app_config, base_client, request, &attempt_target) => {
                        match result {
                            Ok(response) => {
                                if response.status().is_redirection() {
                                    if remaining_redirects == 0 {
                                        return Err(string_to_io_error(format!(
                                            "Too many redirects while requesting {}",
                                            sanitize_sensitive_info(url.as_str())
                                        )));
                                    }

                                    let response_base_url = response.url().clone();
                                    let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                                        return Err(string_to_io_error(format!(
                                            "Redirect response missing location header for {}",
                                            sanitize_sensitive_info(current_url.as_str())
                                        )));
                                    };
                                    let Ok(location_str) = location.to_str() else {
                                        return Err(string_to_io_error(format!(
                                            "Redirect response contains invalid location header for {}",
                                            sanitize_sensitive_info(current_url.as_str())
                                        )));
                                    };
                                    let next_url = response_base_url
                                        .join(location_str)
                                        .or_else(|_| Url::parse(location_str))
                                        .map_err(|_| {
                                            string_to_io_error(format!(
                                                "Redirect response contains invalid location URL for {}",
                                                sanitize_sensitive_info(current_url.as_str())
                                            ))
                                        })?;

                                    if !same_origin(&response_base_url, &next_url) {
                                        strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
                                    }
                                    current_url = next_url;
                                    remaining_redirects = remaining_redirects.saturating_sub(1);
                                    continue 'redirect_loop;
                                }

                                let status = response.status();
                                let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                                if !is_failover && status.is_success() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                last_provider_failure = Some(format!(
                                    "status {} while trying {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(attempt_target.request_url.as_str())
                                ));

                                let provider_failover_exhausted = (is_failover || should_trigger_failover(status))
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if is_failover || should_trigger_failover(status) {
                                    if let Some(current_provider) = provider {
                                        let reason = format!("status {}", format_http_status(status));
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            reason.as_str(),
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                let is_retryable = status.is_server_error()
                                    || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT);
                                if attempt < max_attempts - 1 && is_retryable {
                                    perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request failed"),
                                        );
                                    }
                                }

                                if provider_failover_only {
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                return Err(string_to_io_error(format!(
                                    "Request failed ({}): {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(url.as_str())
                                )));
                            }
                            Err(err) => {
                                if (err.is_timeout() || err.is_connect())
                                    && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                                {
                                    continue 'redirect_loop;
                                }

                                last_provider_failure = Some(format!(
                                    "connection error while trying {}: {}",
                                    sanitize_sensitive_info(attempt_target.request_url.as_str()),
                                    sanitize_sensitive_info(err.to_string().as_str())
                                ));

                                let provider_failover_exhausted = (err.is_timeout() || err.is_connect())
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if err.is_timeout() || err.is_connect() {
                                    if let Some(current_provider) = provider {
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            "connection error",
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                if (err.is_timeout() || err.is_connect()) && attempt < max_attempts - 1 {
                                    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                                    tokio::time::sleep(Duration::from_millis(delay)).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request error"),
                                        );
                                    }
                                }

                                let error_message = format!(
                                    "Request error: {}",
                                    sanitize_sensitive_info(err.to_string().as_str())
                                );
                                return Err(if err.is_timeout() {
                                    Error::new(ErrorKind::TimedOut, error_message)
                                } else if err.is_connect() {
                                    Error::new(ErrorKind::ConnectionRefused, error_message)
                                } else {
                                    string_to_io_error(error_message)
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(current_provider) = provider {
            if rotate_to_next_provider_url(
                current_provider.as_ref(),
                &mut provider_url_index,
                start_provider_index,
                "retries exhausted for current URL",
            ) {
                continue 'provider_loop;
            }

            if max_provider_attempts > 0 {
                let last_failure = last_provider_failure.as_deref().unwrap_or("all attempts and providers exhausted");
                log_provider_cycle_exhausted(
                    current_provider.as_ref(),
                    start_provider_index,
                    provider_url_index,
                    last_failure,
                );
            }
        }

        break;
    }

    Err(string_to_io_error("All attempts and providers exhausted"))
}

fn is_failover_redirect(url: &Url, patterns: &[Arc<Regex>]) -> bool {
    let redirect_url = url.as_str();
    patterns.iter().any(|pattern| pattern.is_match(redirect_url))
}

/// Helper to handle sleep duration for retries, respecting Retry-After headers
async fn perform_backoff(attempt: u32, ms: u64, mult: f64, response: &reqwest::Response) {
    let wait_dur = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map_or_else(|| Duration::from_millis(calculate_retry_backoff(ms, mult, attempt)), Duration::from_secs);

    tokio::time::sleep(wait_dur).await;
}

pub async fn get_input_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    request: InputEpgFileRequest<'_>,
) -> Result<PathBuf, TuliproxError> {
    let InputEpgFileRequest { headers, storage_dir, url: url_str, persist_path, max_bytes } = request;
    debug_if_enabled!(
        "getting input epg content storage_dir: {}, url: {}",
        storage_dir,
        sanitize_sensitive_info(url_str)
    );

    // This is the single write-lock boundary for EPG cache population. Callers must
    // not hold a lock for `persist_path` while invoking this function.
    let _persist_lock = app_config.file_locks.write_lock(persist_path).await;

    // On Windows, drive-letter paths also parse as URLs (with `c` as the scheme).
    // Interpret an absolute platform path before attempting URL parsing.
    if !Path::new(url_str).is_absolute() && url_str.parse::<url::Url>().is_ok() {
        match download_epg_content_as_file(app_config, client, input, headers, url_str, persist_path, max_bytes).await {
            Ok(content) => Ok(content),
            Err(e) => {
                error!(
                    "can't download input {} epg url: {}  => {}",
                    input.name,
                    sanitize_sensitive_info(url_str),
                    sanitize_sensitive_info(&e.to_string())
                );
                Err(TuliproxError::RepositoryNetwork(format!(
                    "can't download input {} epg url: {}  => {}",
                    input.name,
                    sanitize_sensitive_info(url_str),
                    sanitize_sensitive_info(&e.to_string())
                )))
            }
        }
    } else {
        let Some(file_path) = get_file_path(storage_dir, Some(PathBuf::from(url_str))) else {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            return Err(TuliproxError::RepositoryNetwork(msg));
        };
        if !file_path.exists() {
            let msg = format!("can't read input url: {}", sanitize_sensitive_info(url_str));
            error!("{msg}");
            return Err(TuliproxError::RepositoryNetwork(msg));
        }

        copy_local_epg_file_to_persist(&file_path, persist_path, max_bytes).await.map_err(|err| {
            error!("can't persist to: {}  => {}", persist_path.display(), err);
            TuliproxError::RepositoryNetwork(format!("Failed to persist: {}  => {err}", persist_path.display()))
        })
    }
}

pub async fn get_input_text_content(
    app_state: &Arc<AppState>,
    client: &reqwest::Client,
    input: &InputSource,
    storage_dir: &str,
    persist_filepath: Option<PathBuf>,
) -> Result<String, TuliproxError> {
    debug_if_enabled!(
        "getting input text content storage_dir: {}, url: {}",
        storage_dir,
        sanitize_sensitive_info(&input.url)
    );

    if input.url.parse::<url::Url>().is_ok() {
        match download_text_content(&app_state.app_config, client, input, None, persist_filepath, false).await {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!("Failed to download input '{}': {}", input.name, sanitize_sensitive_info(&e.to_string()));
                Err(TuliproxError::RepositoryNetwork(format!(
                    "Failed to download input '{}': {}",
                    input.name,
                    sanitize_sensitive_info(&e.to_string())
                )))
            }
        }
    } else {
        let result = match get_file_path(storage_dir, Some(PathBuf::from(&input.url))) {
            Some(filepath) => {
                if filepath.exists() {
                    if let Some(persist_file_value) = persist_filepath {
                        let to_file = &persist_file_value;
                        if let Err(e) = tokio::fs::copy(&filepath, to_file).await {
                            error!("can't persist to: {}  => {}", to_file.to_str().unwrap_or("?"), e);
                            return Err(TuliproxError::RepositoryNetwork(format!(
                                "Failed to persist: {}  => {}",
                                to_file.to_str().unwrap_or("?"),
                                e
                            )));
                        }
                    }

                    match get_local_file_content(&filepath).await {
                        Ok(content) => Some(content),
                        Err(err) => {
                            return Err(TuliproxError::RepositoryNetwork(format!("Failed : {err}")));
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        result.map_or_else(
            || {
                let msg = format!("can't read input url: {}", sanitize_sensitive_info(&input.url));
                error!("{msg}");
                Err(TuliproxError::RepositoryNetwork(msg))
            },
            Ok,
        )
    }
}

pub async fn get_input_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    storage_dir: &str,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, TuliproxError> {
    debug_if_enabled!(
        "getting input text content storage_dir: {}, url: {}",
        storage_dir,
        sanitize_sensitive_info(&input.url)
    );

    if input.url.parse::<url::Url>().is_ok() {
        match download_text_content_as_stream(app_config, client, input, persist_filepath).await {
            Ok((content, _response_url)) => Ok(content),
            Err(e) => {
                error!("Failed to download input '{}': {}", input.name, sanitize_sensitive_info(&e.to_string()));
                Err(TuliproxError::RepositoryNetwork(format!(
                    "Failed to download input '{}': {}",
                    input.name,
                    sanitize_sensitive_info(&e.to_string())
                )))
            }
        }
    } else {
        let result = match get_file_path(storage_dir, Some(PathBuf::from(&input.url))) {
            Some(filepath) => {
                if filepath.exists() {
                    match get_local_file_content_as_stream(&filepath).await {
                        Ok(content) => {
                            if let Some(path) = persist_filepath {
                                let tee = tee_dyn_reader(
                                    content,
                                    &path,
                                    Some(Arc::new(|size| {
                                        debug_if_enabled!("Persisted {} bytes", human_readable_byte_size(size as u64));
                                    })),
                                )
                                .await;
                                Some(tee)
                            } else {
                                Some(content)
                            }
                        }
                        Err(err) => {
                            return Err(TuliproxError::RepositoryNetwork(format!("Failed : {err}")));
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        result.map_or_else(
            || {
                let msg = format!("can't read input url: {}", sanitize_sensitive_info(&input.url));
                error!("{msg}");
                Err(TuliproxError::RepositoryNetwork(msg))
            },
            Ok,
        )
    }
}

pub fn get_client_request<S: ::std::hash::BuildHasher + Default>(
    client: &reqwest::Client,
    method: InputFetchMethod,
    headers: Option<&HashMap<String, String, S>>,
    url: &Url,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = match method {
        InputFetchMethod::GET => client.get(url.clone()),
        InputFetchMethod::POST => {
            // let base_url = url[..url::Position::BeforePath].to_string() + url.path();
            let mut params: HashMap<String, String, S> = HashMap::default();
            for (key, value) in url.query_pairs() {
                params.insert(key.to_string(), value.to_string());
            }
            // we could cut the params but we leave them as query and add them as form.
            client.post(url.clone()).form(&params)
        }
    };
    let headers = get_request_headers(headers, custom_headers, disabled_headers, default_user_agent);
    request.headers(headers)
}

pub fn get_request_headers<S: ::std::hash::BuildHasher + Default>(
    request_headers: Option<&HashMap<String, String, S>>,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::default();
    let mut has_user_agent = false;

    // 1. First, we process the configured request headers (from input config).
    // These should have the highest priority.
    if let Some(req_headers) = request_headers {
        for (key, value) in req_headers {
            if let (Ok(key), Ok(value)) =
                (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value.as_bytes()))
            {
                if filter_request_header(key.as_str()) {
                    if disabled_headers.as_ref().is_some_and(|d| d.should_remove(key.as_str())) {
                        continue;
                    }
                    if key == axum::http::header::USER_AGENT {
                        has_user_agent = true;
                    }
                    headers.insert(key, value);
                }
            }
        }
    }

    // 2. Next, we process custom headers (from the client request).
    // These are only added if they don't already exist in the headers map (i.e., not overridden by config).
    if let Some(custom) = custom_headers {
        for (key, value) in custom {
            let key_lc = key.to_lowercase();
            if filter_request_header(key_lc.as_str()) {
                if disabled_headers.as_ref().is_some_and(|d| d.should_remove(key_lc.as_str())) {
                    continue;
                }
                if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value)) {
                    // Only insert if not already present (config takes precedence)
                    if !headers.contains_key(&name) {
                        if name == axum::http::header::USER_AGENT {
                            has_user_agent = true;
                        }
                        headers.insert(name, val);
                    }
                }
            }
        }
    }

    if log_enabled!(Level::Trace) {
        let he: HashMap<String, String> =
            headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
        if !he.is_empty() {
            trace!("Request headers {he:?}");
        }
    }

    // 3. Finally, if no User-Agent was provided by config OR client, use the default.
    if !has_user_agent
        && !disabled_headers.is_some_and(|disabled| disabled.should_remove(axum::http::header::USER_AGENT.as_str()))
    {
        let config_ua = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| HeaderValue::from_str(ua).ok());

        headers.insert(
            axum::http::header::USER_AGENT,
            config_ua.unwrap_or_else(|| HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }

    headers
}

pub fn overlay_upstream_user_agent(
    headers: &mut HeaderMap,
    upstream_user_agent: Option<&str>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
) {
    if disabled_headers.is_some_and(|disabled| disabled.should_remove(axum::http::header::USER_AGENT.as_str())) {
        return;
    }
    if let Some(value) = upstream_user_agent
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        headers.insert(axum::http::header::USER_AGENT, value);
    }
}

// read local file content and return it as a string.
// Gzipped file content is supported.
pub async fn get_local_file_content(file_path: &Path) -> Result<String, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(ErrorKind::NotFound, format!("Failed to open file: {}, {err:?}", file_path.display()))
    })?;

    let mut buf_reader = async_file_reader(file);

    // Peek first 2 bytes to detect gzip encoding
    let buffer = buf_reader.fill_buf().await?;
    let is_gzipped = buffer.len() >= 2 && is_gzip(&buffer[0..2]);

    let mut decoded = String::new();

    if is_gzipped {
        // Use async gzip decoder
        let mut gzip_decoder = async_compression::tokio::bufread::GzipDecoder::new(buf_reader);
        gzip_decoder
            .read_to_string(&mut decoded)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to decode gzip content: {e}")))?;
    } else {
        // read plaintext
        buf_reader
            .read_to_string(&mut decoded)
            .await
            .map_err(|e| std::io::Error::other(format!("Failed to read file: {e}")))?;
    }

    Ok(decoded)
}

pub async fn get_local_file_content_as_stream(file_path: &Path) -> Result<DynReader, std::io::Error> {
    // open file
    let file = File::open(file_path).await.map_err(|err| {
        std::io::Error::new(ErrorKind::NotFound, format!("Failed to open file: {}, {err:?}", file_path.display()))
    })?;

    let mut buf_reader = async_file_reader(file);

    // Peek first 2 Bytes, for gzip detection
    let buffer = buf_reader.fill_buf().await?;
    let is_gzipped = buffer.len() >= 2 && is_gzip(&buffer[0..2]);

    if is_gzipped {
        // use Async Gzip Decoder
        Ok(Box::pin(async_compression::tokio::bufread::GzipDecoder::new(buf_reader)))
    } else {
        Ok(Box::pin(buf_reader))
    }
}

pub async fn get_remote_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url: &Url,
    file_path: &Path,
) -> Result<PathBuf, std::io::Error> {
    get_remote_content_as_file_with_options(
        app_config,
        client,
        input,
        headers,
        url,
        file_path,
        FileDownloadOptions::default(),
    )
    .await
}

pub async fn get_remote_content_as_file_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url: &Url,
    file_path: &Path,
    options: FileDownloadOptions,
) -> Result<PathBuf, std::io::Error> {
    let input_source = InputSource {
        name: input.name.clone(),
        url: url.to_string(),
        provider: input.get_resolve_provider(url.as_str()),
        username: input.username.clone(),
        password: input.password.clone(),
        method: input.method,
        headers: input.headers.clone(),
    };

    let response = send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
        app_config,
        client,
        &input_source,
        headers,
        url,
        10,
        RequestFetchOptions::default(),
    )
    .await?
    .response;

    let start_time = tokio::time::Instant::now();
    let (temp_file, output_file) = if options.atomic_write {
        let (temp_file, output_file) = create_atomic_download_file(file_path)?;
        (Some(temp_file), output_file)
    } else {
        (None, File::create(file_path).await?)
    };
    let mut writer = async_file_writer(output_file);

    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;

    let idle_timeout = tokio::time::Duration::from_secs(STREAM_IDLE_TIMEOUT);
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    loop {
        tokio::select! {
        () = &mut idle => {
            warn!("Stream idle for request, closing {}", sanitize_sensitive_info(url.as_ref()));
            return Err(string_to_io_error(format!(
                "Download timed out for {}",
                sanitize_sensitive_info(url.as_ref())
            )));
        }

        chunk = stream.next() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                match chunk {
                    Some(Ok(bytes)) => {
                        downloaded = downloaded.checked_add(bytes.len() as u64).ok_or_else(|| {
                            string_to_io_error(format!(
                                "Download size overflow for {}",
                                sanitize_sensitive_info(url.as_ref())
                            ))
                        })?;
                        if options.max_bytes.is_some_and(|max| downloaded > max) {
                            return Err(string_to_io_error(format!(
                                "Download exceeds configured limit for {}",
                                sanitize_sensitive_info(url.as_ref())
                            )));
                        }
                        writer.write_all(&bytes).await?;
                    }
                    Some(Err(e)) => {
                        return Err(string_to_io_error(format!("Failed to read chunk: {e}")));
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }

    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);

    if let Some(temp_file) = temp_file {
        persist_atomic_download_file(temp_file, file_path)?;
    }

    debug!(
        "File downloaded successfully to {}, took {}",
        file_path.display(),
        format_elapsed_time(start_time.elapsed().as_secs())
    );

    Ok(file_path.to_path_buf())
}

/// Controls decoding and bounded consumption of a fully buffered text response.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TextContentBodyOptions {
    detection: ContentCodingDetection,
    max_decoded_bytes: Option<usize>,
    deadline: Option<Duration>,
    retry_owner: TextContentRetryOwner,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum TextContentRetryOwner {
    #[default]
    RequestStack,
    DecodedBodyConsumer,
}

impl Default for TextContentBodyOptions {
    fn default() -> Self {
        Self {
            detection: ContentCodingDetection::DeclaredOrLegacyTextMagic,
            max_decoded_bytes: None,
            deadline: None,
            retry_owner: TextContentRetryOwner::RequestStack,
        }
    }
}

impl TextContentBodyOptions {
    /// Selects the narrowly scoped HLS-manifest fallback detection and decoded-size limit.
    pub(crate) fn hls_manifest(max_decoded_bytes: usize, deadline: Duration) -> Self {
        Self {
            detection: ContentCodingDetection::DeclaredOrKnownHlsManifestMagic,
            max_decoded_bytes: Some(max_decoded_bytes),
            deadline: Some(deadline.max(Duration::from_millis(1))),
            retry_owner: TextContentRetryOwner::DecodedBodyConsumer,
        }
    }

    fn legacy_text_with_deadline(deadline: Option<Duration>) -> Self {
        Self { deadline: deadline.map(|value| value.max(Duration::from_millis(1))), ..Self::default() }
    }
}

/// Groups request-boundary and decoded-text-consumer options for one text fetch.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TextContentFetchOptions {
    request: RequestFetchOptions,
    body: TextContentBodyOptions,
}

impl TextContentFetchOptions {
    pub(crate) const fn new(request: RequestFetchOptions, body: TextContentBodyOptions) -> Self {
        Self { request, body }
    }

    fn with_request_options(request: RequestFetchOptions) -> Self {
        Self { body: TextContentBodyOptions::legacy_text_with_deadline(request.attempt_idle_timeout), request }
    }
}

async fn build_decoded_stream_reader(response: reqwest::Response) -> Result<DynReader, std::io::Error> {
    decode_response_to_identity(response, ContentCodingDetection::DeclaredOrLegacyTextMagic)
        .await
        .map(|decoded| decoded.body)
        .map_err(content_coding_error_to_io)
}

fn content_coding_error_to_io(error: ContentCodingError) -> Error {
    let kind = match &error {
        ContentCodingError::PrefixRead(source) => source.kind(),
        ContentCodingError::InvalidHeader
        | ContentCodingError::Unsupported(_)
        | ContentCodingError::EncodedPartialContent => ErrorKind::Other,
    };
    Error::new(kind, error)
}

fn content_body_read_error_to_io(error: ContentBodyReadError) -> Error {
    match error {
        ContentBodyReadError::Io(error) => error,
        error @ ContentBodyReadError::LimitExceeded { .. } => Error::other(error),
        error @ ContentBodyReadError::InvalidUtf8 { .. } => Error::new(ErrorKind::InvalidData, error),
    }
}

/// Builds a fixed/numeric text-response diagnostic without origin-controlled content.
pub(crate) fn text_response_error_log_label(error: &Error) -> String {
    if let Some(error) = content_decoding_error_from_io(error) {
        return format!("content_decoding coding={}", error.coding.as_http_token());
    }
    if let Some(error) = error.get_ref().and_then(|source| source.downcast_ref::<ContentBodyReadError>()) {
        return match error {
            ContentBodyReadError::LimitExceeded { limit } => format!("decoded_body_limit limit={limit}"),
            ContentBodyReadError::InvalidUtf8 { valid_up_to, error_len } => {
                format!("invalid_utf8 valid_up_to={valid_up_to} error_len={error_len:?}")
            }
            ContentBodyReadError::Io(error) => format!("io kind={:?}", error.kind()),
        };
    }
    if let Some(error) = error.get_ref().and_then(|source| source.downcast_ref::<ContentCodingError>()) {
        return match error {
            ContentCodingError::InvalidHeader => "content_coding class=invalid_header".to_string(),
            ContentCodingError::Unsupported(_) => "content_coding class=unsupported".to_string(),
            ContentCodingError::EncodedPartialContent => "content_coding class=encoded_partial_content".to_string(),
            ContentCodingError::PrefixRead(_) => "content_coding class=prefix_read".to_string(),
        };
    }
    if error.kind() == ErrorKind::TimedOut {
        return "timeout".to_string();
    }
    if is_http_body_transport_error(error) {
        return "transport".to_string();
    }
    format!("io kind={:?}", error.kind())
}

async fn read_text_response_with_body_options(
    response: reqwest::Response,
    body_options: TextContentBodyOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let request_url = response.url().to_string();
    let read = async move {
        let mut decoded =
            decode_response_to_identity(response, body_options.detection).await.map_err(content_coding_error_to_io)?;
        if let (ContentCodingDetection::DeclaredOrKnownHlsManifestMagic, Some(observation)) =
            (body_options.detection, decoded.content_coding_observation())
        {
            log_hls_origin_content_coding(
                observation,
                HlsOriginContentCodingObjectKind::Manifest,
                false,
                HlsOriginContentCodingSource::Legacy,
            );
        }
        let content = if let Some(max_decoded_bytes) = body_options.max_decoded_bytes {
            read_utf8_limited(&mut decoded.body, max_decoded_bytes).await.map_err(content_body_read_error_to_io)?
        } else {
            let mut content = String::new();
            decoded.body.read_to_string(&mut content).await.map_err(|error| Error::new(error.kind(), error))?;
            content
        };
        Ok((content, decoded.final_url.to_string(), decoded.headers))
    };

    if let Some(deadline) = body_options.deadline {
        tokio::time::timeout(deadline, read).await.map_err(|_| {
            Error::new(
                ErrorKind::TimedOut,
                format!("Timed out reading content body: {}", sanitize_sensitive_info(&request_url)),
            )
        })?
    } else {
        read.await
    }
}

#[allow(clippy::implicit_hasher)]
pub async fn get_remote_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
) -> Result<(DynReader, String), Error> {
    get_remote_content_as_stream_with_options(app_config, client, input, headers, url, RequestFetchOptions::default())
        .await
}

#[allow(clippy::implicit_hasher)]
async fn get_remote_content_as_stream_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<(DynReader, String), Error> {
    let response =
        send_input_with_retry_and_provider_policy_with_options_result(app_config, client, input, headers, url, options)
            .await?
            .response;
    let response_url = response.url().to_string();

    let reader = build_decoded_stream_reader(response).await?;
    Ok((reader, response_url))
}

fn text_body_retry_values(app_config: &Arc<AppConfig>, body_options: TextContentBodyOptions) -> (u32, u64, f64) {
    if body_options.retry_owner != TextContentRetryOwner::DecodedBodyConsumer {
        return (1, 0, 1.0);
    }
    let config = app_config.config.load();
    let values = config
        .reverse_proxy
        .as_ref()
        .map_or_else(ResourceRetryConfig::get_default_retry_values, |rp| rp.resource_retry.get_retry_values());
    drop(config);
    values
}

fn should_retry_text_body_error(error: &Error) -> bool {
    matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::ConnectionRefused)
        || content_decoding_error_from_io(error).is_some()
        || is_http_body_transport_error(error)
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<ContentCodingError>())
            .is_some_and(|error| matches!(error, ContentCodingError::PrefixRead(_)))
}

async fn sleep_before_text_body_retry(attempt: u32, backoff_ms: u64, backoff_multiplier: f64, err: &Error) {
    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
    warn!(
        "Text response body failed retryably, retrying in {}ms (attempt {}): {}",
        delay,
        attempt + 1,
        text_response_error_log_label(err)
    );
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

fn is_retryable_text_response_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn text_response_status_error(status: StatusCode, url: &Url) -> Error {
    string_to_io_error(format!(
        "Request failed ({}): {}",
        format_http_status(status),
        sanitize_sensitive_info(url.as_str())
    ))
}

async fn get_remote_content_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    get_remote_content_with_headers_and_options(
        app_config,
        client,
        input,
        headers,
        url,
        TextContentFetchOptions::with_request_options(options),
    )
    .await
    .map(|(content, response_url, _)| (content, response_url))
}

async fn get_remote_content_with_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let (max_attempts, backoff_ms, backoff_multiplier) = text_body_retry_values(app_config, options.body);
    let attempt_options = if max_attempts > 1 { options.request.without_resource_retries() } else { options.request };

    // This consumer owns the logical attempt budget whenever decoded-body retries are enabled. Provider failover
    // and redirect hops remain bounded subrequests, but the configured retry count is not applied again below it.
    for attempt in 0..max_attempts {
        let response = match send_input_with_retry_and_provider_policy_with_options_result(
            app_config,
            client,
            input,
            headers,
            url,
            attempt_options,
        )
        .await
        {
            Ok(result) => result.response,
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        if !status.is_success() {
            if is_retryable_text_response_status(status) && attempt + 1 < max_attempts {
                perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                continue;
            }
            return Err(text_response_status_error(status, url));
        }
        match read_text_response_with_body_options(response, options.body).await {
            Ok(result) => return Ok(result),
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(string_to_io_error("Text response body retry attempts exhausted"))
}

#[allow(clippy::too_many_lines)]
async fn get_remote_content_with_manual_redirects_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    get_remote_content_with_manual_redirects_and_headers_and_options(
        app_config,
        client,
        input,
        headers,
        url,
        max_redirects,
        TextContentFetchOptions::with_request_options(options),
    )
    .await
    .map(|(content, response_url, _)| (content, response_url))
}

async fn get_remote_content_with_manual_redirects_and_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let (max_attempts, backoff_ms, backoff_multiplier) = text_body_retry_values(app_config, options.body);
    let attempt_options = if max_attempts > 1 { options.request.without_resource_retries() } else { options.request };

    // Manual redirects retain their own credential-scrubbing loop inside each caller-owned logical attempt.
    for attempt in 0..max_attempts {
        let response = match send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
            app_config,
            client,
            input,
            headers,
            url,
            max_redirects,
            attempt_options,
        )
        .await
        {
            Ok(result) => result.response,
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        if !status.is_success() {
            if is_retryable_text_response_status(status) && attempt + 1 < max_attempts {
                perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                continue;
            }
            return Err(text_response_status_error(status, url));
        }
        match read_text_response_with_body_options(response, options.body).await {
            Ok(result) => return Ok(result),
            Err(error) if should_retry_text_body_error(&error) && attempt + 1 < max_attempts => {
                sleep_before_text_body_retry(attempt, backoff_ms, backoff_multiplier, &error).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(string_to_io_error("Text response body retry attempts exhausted"))
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

/// Reports whether a request header may be retained when the target origin changes.
pub(crate) fn is_safe_cross_origin_redirect_header(key: &str) -> bool {
    key.eq_ignore_ascii_case("accept")
        || key.eq_ignore_ascii_case("accept-encoding")
        || key.eq_ignore_ascii_case("accept-language")
        || key.eq_ignore_ascii_case("user-agent")
        || key.eq_ignore_ascii_case("range")
        || key.eq_ignore_ascii_case("if-range")
        || key.eq_ignore_ascii_case("icy-metadata")
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HashMap<String, String>) {
    headers.retain(|key, _| is_safe_cross_origin_redirect_header(key));
}

fn create_atomic_download_file(file_path: &Path) -> Result<(tempfile::NamedTempFile, File), Error> {
    let parent = file_path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let temp_file = tempfile::Builder::new().prefix(".tuliprox-download-").suffix(".tmp").tempfile_in(parent)?;
    let output_file = File::from_std(temp_file.reopen()?);
    Ok((temp_file, output_file))
}

fn persist_atomic_download_file(temp_file: tempfile::NamedTempFile, file_path: &Path) -> Result<(), Error> {
    match temp_file.persist(file_path) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.error),
    }
}

async fn copy_local_epg_file_to_persist(
    file_path: &Path,
    persist_filepath: &Path,
    max_bytes: Option<u64>,
) -> Result<PathBuf, Error> {
    let mut reader = File::open(file_path).await?;
    let (temp_file, output_file) = create_atomic_download_file(persist_filepath)?;
    let mut writer = async_file_writer(output_file);
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| string_to_io_error(format!("Local EPG file size overflow for {}", file_path.display())))?;
        if max_bytes.is_some_and(|max| copied > max) {
            return Err(string_to_io_error(format!("Local EPG file {} exceeds configured limit", file_path.display())));
        }
        writer.write_all(&buffer[..read]).await?;
    }

    writer.flush().await?;
    writer.shutdown().await?;
    drop(writer);
    drop(reader);
    persist_atomic_download_file(temp_file, persist_filepath)?;
    Ok(persist_filepath.to_path_buf())
}

async fn download_epg_content_as_file(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    headers: Option<&HeaderMap>,
    url_str: &str,
    persist_filepath: &Path,
    max_bytes: Option<u64>,
) -> Result<PathBuf, Error> {
    if let Ok(url) = url_str.parse::<url::Url>() {
        match url.scheme() {
            "file" => {
                let file_path = url.to_file_path().map_err(|()| {
                    Error::new(ErrorKind::Unsupported, format!("Unknown file {}", sanitize_sensitive_info(url_str)))
                })?;
                if file_path.exists() {
                    copy_local_epg_file_to_persist(&file_path, persist_filepath, max_bytes).await
                } else {
                    Err(Error::new(ErrorKind::NotFound, format!("Unknown file {}", file_path.display())))
                }
            }
            "http" | "https" | "provider" => {
                get_remote_content_as_file_with_options(
                    app_config,
                    client,
                    input,
                    headers,
                    &url,
                    persist_filepath,
                    FileDownloadOptions { max_bytes, atomic_write: true },
                )
                .await
            }
            scheme => Err(Error::new(
                ErrorKind::Unsupported,
                format!("Unsupported EPG URL scheme '{scheme}' for {}", sanitize_sensitive_info(url_str)),
            )),
        }
    } else {
        Err(Error::new(ErrorKind::Unsupported, format!("Malformed URL {}", sanitize_sensitive_info(url_str))))
    }
}

pub async fn download_text_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<(String, String), Error> {
    Box::pin(download_text_content_with_options(
        app_config,
        client,
        input,
        headers,
        persist_filepath,
        trace_log,
        RequestFetchOptions::default(),
    ))
    .await
}

pub async fn download_text_content_with_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content(&file_path).await.map(|content| (content, url.to_string())),
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_options(app_config, client, input, headers, &url, options).await
        };
        match result {
            Ok((content, response_url)) => {
                if persist_filepath.is_some() {
                    persist_file(persist_filepath, &content).await;
                }
                Ok((content, response_url))
            }
            Err(err) => Err(err),
        }
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_content, response_url)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

pub async fn download_text_content_with_headers(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
) -> Result<(String, String, HeaderMap), Error> {
    Box::pin(download_text_content_with_headers_and_options(
        app_config,
        client,
        input,
        headers,
        trace_log,
        TextContentFetchOptions::default(),
    ))
    .await
}

pub(crate) async fn download_text_content_with_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => {
                    get_local_file_content(&file_path).await.map(|content| (content, url.to_string(), HeaderMap::new()))
                }
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_headers_and_options(app_config, client, input, headers, &url, options).await
        };
        result
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_, response_url, _)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

pub async fn download_text_content_with_manual_redirects(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
    max_redirects: usize,
) -> Result<(String, String), Error> {
    Box::pin(download_text_content_with_manual_redirects_and_options(
        app_config,
        client,
        input,
        headers,
        persist_filepath,
        trace_log,
        max_redirects,
        RequestFetchOptions::default(),
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn download_text_content_with_manual_redirects_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<(String, String), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content(&file_path).await.map(|content| (content, url.to_string())),
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_manual_redirects_and_options(
                app_config,
                client,
                input,
                headers,
                &url,
                max_redirects,
                options,
            )
            .await
        };
        match result {
            Ok((content, response_url)) => {
                if persist_filepath.is_some() {
                    persist_file(persist_filepath, &content).await;
                }
                Ok((content, response_url))
            }
            Err(err) => Err(err),
        }
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_content, response_url)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

pub async fn download_text_content_with_manual_redirects_and_headers(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
    max_redirects: usize,
) -> Result<(String, String, HeaderMap), Error> {
    Box::pin(download_text_content_with_manual_redirects_and_headers_and_options(
        app_config,
        client,
        input,
        headers,
        trace_log,
        max_redirects,
        TextContentFetchOptions::default(),
    ))
    .await
}

pub(crate) async fn download_text_content_with_manual_redirects_and_headers_and_options(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    trace_log: bool,
    max_redirects: usize,
    options: TextContentFetchOptions,
) -> Result<(String, String, HeaderMap), Error> {
    let start_time = tokio::time::Instant::now();
    let result = if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => {
                    get_local_file_content(&file_path).await.map(|content| (content, url.to_string(), HeaderMap::new()))
                }
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_with_manual_redirects_and_headers_and_options(
                app_config,
                client,
                input,
                headers,
                &url,
                max_redirects,
                options,
            )
            .await
        };
        result
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    };

    let level = if trace_log { log::Level::Trace } else { log::Level::Debug };
    if log_enabled!(level) {
        if let Ok((_, response_url, _)) = result.as_ref() {
            log::log!(
                level,
                "Request took: {} {}",
                format_elapsed_time(start_time.elapsed().as_secs()),
                sanitize_sensitive_info(response_url.as_str())
            );
        }
    }

    result
}

pub async fn download_text_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<(DynReader, String), Error> {
    if let Ok(url) = input.url.parse::<url::Url>() {
        let result = if url.scheme() == "file" {
            match url.to_file_path() {
                Ok(file_path) => get_local_file_content_as_stream(&file_path).await.map(|c| (c, url.to_string())),
                Err(()) => Err(string_to_io_error(format!("Unknown file {}", sanitize_sensitive_info(&input.url)))),
            }
        } else {
            get_remote_content_as_stream(app_config, client, input, None, &url).await
        };
        match result {
            Ok((content, response_url)) => {
                if let Some(path) = persist_filepath {
                    let tee_reader: DynReader = tee_dyn_reader(
                        content,
                        &path,
                        Some(Arc::new(|size| {
                            debug!("Persisted {size} bytes");
                        })),
                    )
                    .await;
                    Ok((tee_reader, response_url))
                } else {
                    Ok((content, response_url))
                }
            }
            Err(err) => Err(err),
        }
    } else {
        Err(string_to_io_error(format!("Malformed URL {}", sanitize_sensitive_info(&input.url))))
    }
}

async fn download_json_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<serde_json::Value, Error> {
    debug_if_enabled!("Downloading json content from {}", sanitize_sensitive_info(&input.url));
    match download_text_content(app_config, client, input, None, persist_filepath, trace_log).await {
        Ok((content, _response_url)) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(value) => Ok(value),
            Err(err) => Err(string_to_io_error(format!("Failed to parse json {err}"))),
        },
        Err(err) => Err(err),
    }
}

pub async fn get_input_json_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
    trace_log: bool,
) -> Result<serde_json::Value, TuliproxError> {
    match download_json_content(app_config, client, input, persist_filepath, trace_log).await {
        Ok(content) => Ok(content),
        Err(e) => Err(TuliproxError::RepositoryNetwork(format!(
            "can't download input {input} => {sanitized}",
            input = input.name,
            sanitized = sanitize_sensitive_info(&e.to_string())
        ))),
    }
}

async fn download_json_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, Error> {
    debug_if_enabled!("Downloading json content as stream from {}", sanitize_sensitive_info(&input.url));
    match download_text_content_as_stream(app_config, client, input, persist_filepath).await {
        Ok((reader, _response_url)) => Ok(reader),
        Err(err) => Err(err),
    }
}

pub async fn get_input_json_content_as_stream(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    persist_filepath: Option<PathBuf>,
) -> Result<DynReader, TuliproxError> {
    match download_json_content_as_stream(app_config, client, input, persist_filepath).await {
        Ok(stream) => Ok(stream),
        Err(e) => Err(TuliproxError::RepositoryNetwork(format!(
            "can't download input {input} => {sanitized}",
            input = input.name,
            sanitized = sanitize_sensitive_info(&e.to_string())
        ))),
    }
}

pub fn create_client_with_redirect(cfg: &AppConfig, redirect_policy: Policy) -> reqwest::ClientBuilder {
    let config = cfg.config.load();
    log_proxy_diagnostics(&config);
    let mut client = reqwest::Client::builder()
        .redirect(redirect_policy)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .danger_accept_invalid_certs(config.accept_insecure_ssl_certificates);

    if let Some(proxy_cfg) = config.proxy.as_ref() {
        match Url::parse(&proxy_cfg.url) {
            Ok(mut url) => {
                let scheme = url.scheme().to_ascii_lowercase();

                match scheme.as_str() {
                    "socks5" | "socks5h" => {
                        if let Some(user) = &proxy_cfg.username {
                            let _ = url.set_username(user);
                        }
                        if let Some(pass) = &proxy_cfg.password {
                            let _ = url.set_password(Some(pass));
                        }
                        match reqwest::Proxy::all(url.as_str()) {
                            Ok(p) => {
                                client = client.proxy(p);
                            }
                            Err(err) => error!("Failed to create SOCKS proxy {url}: {err}"),
                        }
                    }
                    "http" | "https" => match reqwest::Proxy::all(url.as_str()) {
                        Ok(p) => {
                            if let (Some(username), Some(password)) = (&proxy_cfg.username, &proxy_cfg.password) {
                                client = client.proxy(p.basic_auth(username, password));
                            } else {
                                client = client.proxy(p);
                            }
                        }
                        Err(err) => error!("Failed to create HTTP proxy {url}: {err}"),
                    },
                    _ => {
                        error!("Unsupported proxy scheme '{scheme}' in URL: {url}");
                    }
                }
            }
            Err(e) => {
                error!("Invalid proxy URL '{}': {e}", proxy_cfg.url);
            }
        }
    }

    if let Some(rp_config) = config.reverse_proxy.as_ref() {
        if rp_config.disabled_header.as_ref().is_some_and(|d| d.referer_header) {
            client = client.referer(false);
        }
    }

    client
}

pub fn create_client(cfg: &AppConfig) -> reqwest::ClientBuilder {
    create_client_with_redirect(cfg, Policy::limited(10))
}

pub fn parse_range(range: &str) -> Option<(u64, Option<u64>)> {
    // expect: "bytes=START-END"
    if !range.starts_with("bytes=") {
        return None;
    }

    let range = &range[6..];
    let mut parts = range.split('-');

    let start = parts.next()?.parse().ok()?;
    let end = parts.next().and_then(|s| s.parse().ok());

    Some((start, end))
}

pub fn is_file_url(url: &str) -> bool { Url::parse(url).is_ok_and(|u| u.scheme().eq_ignore_ascii_case("file")) }

pub fn is_uri(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme().eq_ignore_ascii_case("file")
            || u.scheme().eq_ignore_ascii_case("http")
            || u.scheme().eq_ignore_ascii_case("https")
    })
}

/// Checks if a status code or error indicates a need for failover
///
/// Returns true for server-side errors that might be resolved by trying another URL.
/// Returns false for client-side errors (401, 403, etc.) where the problem is with
/// credentials or permissions, not the server availability.
pub fn should_trigger_failover(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND
            | StatusCode::GONE
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    )
    // Explicitly NOT triggering failover for:
    // - 401 Unauthorized (wrong credentials)
    // - 403 Forbidden (permission issue)
    // - 402 Payment Required (subscription issue)
    // - 451 Unavailable For Legal Reasons (geo-blocking)
    //
    // Note: DO triggering failover for:
    // - 429 Too Many Requests
    // - 408 Request Timeout
    // - 407 Proxy Authentication Required
}

#[cfg(test)]
mod tests {
    use super::{
        download_text_content, download_text_content_with_headers_and_options, get_input_epg_content_as_file,
        get_remote_content_as_stream, is_safe_cross_origin_redirect_header, next_provider_url_index,
        preview_request_diagnostics_for_logging, preview_request_target_for_logging, resolve_attempt_target,
        same_origin, send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
        send_input_with_retry_and_provider_policy_with_options_result, send_with_retry_and_provider,
        send_with_retry_and_provider_policy, should_retry_text_body_error, should_try_next_ip_on_connect_error,
        strip_sensitive_headers_for_cross_origin_redirect, text_response_error_log_label, InputEpgFileRequest,
        PublicIpResolver, RequestFetchOptions, TextContentBodyOptions, TextContentFetchOptions,
        STREAM_IDLE_TIMEOUT,
    };
    use crate::{
        model::{
            AppConfig, Config, ConfigInput, ConfigProvider, InputSource, MediaToolCapabilities, ResourceRetryConfig,
            ReverseProxyConfig, ReverseProxyDisabledHeaderConfig, SourcesConfig,
        },
        utils::{
            content_coding::{ContentCoding, ContentCodingError, ContentDecodingIoError, OutboundContentCodingPolicy},
            FileLockManager,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use flate2::{
        write::{GzEncoder, ZlibEncoder},
        Compression,
    };
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING, COOKIE};
    use shared::{
        defaults::DEFAULT_USER_AGENT,
        model::{
            ConfigPaths, ConfigProviderDto, DnsScheme, InputFetchMethod, OnConnectErrorPolicy, ProviderDnsDto,
            ProviderUrlSelectionPolicy,
        },
        utils::{get_base_url_from_str, replace_url_extension, sanitize_sensitive_info},
    };
    use std::{
        collections::{HashMap, HashSet},
        io::{Error, ErrorKind, Write},
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, Mutex},
    };
    use url::Url;

    fn make_test_app_config(config: Config) -> Arc<AppConfig> {
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

    fn make_epg_test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client")
    }

    async fn atomic_download_temp_files(directory: &Path) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut entries = tokio::fs::read_dir(directory).await.expect("read temp directory");
        while let Some(entry) = entries.next_entry().await.expect("read temp directory entry") {
            if entry.file_name().to_string_lossy().starts_with(".tuliprox-download-") {
                result.push(entry.path());
            }
        }
        result
    }

    #[tokio::test]
    async fn atomic_download_temp_files_are_unique_and_use_target_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("cache.ics");
        let (first_temp, first_output) = super::create_atomic_download_file(&target).expect("first temp file");
        let (second_temp, second_output) = super::create_atomic_download_file(&target).expect("second temp file");

        assert_ne!(first_temp.path(), second_temp.path());
        assert_eq!(first_temp.path().parent(), Some(dir.path()));
        assert_eq!(second_temp.path().parent(), Some(dir.path()));

        drop(first_output);
        drop(second_output);
        drop(first_temp);
        drop(second_temp);
        assert!(atomic_download_temp_files(dir.path()).await.is_empty());
    }

    fn make_provider_with_dns(
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
    fn test_get_request_headers_prioritization() {
        use super::{get_request_headers, overlay_upstream_user_agent};
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
        let headers =
            get_request_headers(Some(&config_headers), Some(&client_headers), None, Some("Config-Default-UA"));
        assert_eq!(headers.get(USER_AGENT).unwrap(), "Config-UA");

        // Case 5: Other headers also prioritized
        config_headers.insert("X-Test".to_string(), "From-Config".to_string());
        let mut client_headers = HashMap::new();
        client_headers.insert("X-Test".to_string(), b"From-Client".to_vec());
        let headers =
            get_request_headers(Some(&config_headers), Some(&client_headers), None, Some("Config-Default-UA"));
        assert_eq!(headers.get("X-Test").unwrap(), "From-Config");

        let mut headers = get_request_headers(
            Some(&config_headers),
            Some(&client_headers),
            None,
            Some("Config-Default-UA"),
        );
        overlay_upstream_user_agent(&mut headers, Some("Channel-UA"), None);
        assert_eq!(headers.get(USER_AGENT).unwrap(), "Channel-UA");

        let disabled = ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: false,
            cloudflare_header: false,
            custom_header: vec!["User-Agent".to_string()],
        };
        let mut headers = get_request_headers(
            Some(&config_headers),
            Some(&client_headers),
            Some(&disabled),
            Some("Config-Default-UA"),
        );
        overlay_upstream_user_agent(&mut headers, Some("Blocked-Channel-UA"), Some(&disabled));
        assert!(!headers.contains_key(USER_AGENT));
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
    async fn file_url_epg_source_is_copied_to_persist_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.ics");
        let persist = dir.path().join("cache.ics");
        tokio::fs::write(&source, b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").await.expect("write source");
        tokio::fs::write(&persist, b"old cache").await.expect("write old cache");
        let source_url = Url::from_file_path(&source).expect("file url");
        let app_config = make_test_app_config(Config::default());
        let client = make_epg_test_client();
        let input = ConfigInput::default();

        let result = get_input_epg_content_as_file(
            &app_config,
            &client,
            &input,
            InputEpgFileRequest {
                headers: None,
                storage_dir: dir.path().to_string_lossy().as_ref(),
                url: source_url.as_str(),
                persist_path: &persist,
                max_bytes: Some(1024),
            },
        )
        .await
        .expect("download");

        assert_eq!(result, persist);
        assert_eq!(
            tokio::fs::read_to_string(&persist).await.expect("persisted content"),
            "BEGIN:VCALENDAR\nEND:VCALENDAR\n"
        );
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
        let provider =
            make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "192.168.0.2"]);
        let url = Url::parse("http://example.com/live").expect("url parse should work");
        let mut tried = HashSet::new();

        let first = resolve_attempt_target(&url, Some(&provider));
        let second = resolve_attempt_target(&url, Some(&provider));

        assert!(should_try_next_ip_on_connect_error(Some(&provider), &first, &mut tried));
        assert!(!should_try_next_ip_on_connect_error(Some(&provider), &second, &mut tried));
    }

    #[test]
    fn test_preview_request_target_for_logging_does_not_advance_dns_rotation() {
        let provider =
            make_provider_with_dns(false, OnConnectErrorPolicy::TryNextIp, vec!["192.168.0.1", "192.168.0.2"]);
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

    async fn start_plain_http_server_with_body(
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

    async fn start_delayed_http_server_with_body(
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

    async fn start_plain_http_server_with_response(
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

    async fn start_plain_http_server() -> std::io::Result<(SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
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

    async fn start_recording_http_server(
        responses: Vec<String>,
    ) -> std::io::Result<(SocketAddr, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>)> {
        start_recording_http_byte_server(responses.into_iter().map(String::into_bytes).collect()).await
    }

    async fn start_recording_http_byte_server(
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

    async fn start_hanging_http_server(
    ) -> std::io::Result<(SocketAddr, oneshot::Receiver<()>, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 2048];
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let _ = request_seen_tx.send(());
            std::future::pending::<()>().await;
            drop(socket);
        });

        Ok((addr, request_seen_rx, handle))
    }

    fn response_with_body(status: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    }

    fn response_with_byte_body(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn gzip_encoded(body: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).expect("gzip input");
        encoder.finish().expect("gzip output")
    }

    fn zlib_encoded(body: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).expect("zlib input");
        encoder.finish().expect("zlib output")
    }

    fn request_header_value<'a>(request: &'a str, expected_name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(expected_name).then_some(value.trim())
        })
    }

    fn test_retry_config(max_attempts: u32) -> Arc<AppConfig> {
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

    fn identity_fetch_options() -> RequestFetchOptions {
        RequestFetchOptions::with_attempt_idle_timeout(Duration::from_secs(1))
            .with_content_coding(OutboundContentCodingPolicy::Identity)
    }

    #[test]
    fn physical_request_attempt_applies_target_and_final_content_coding_options() {
        let client = reqwest::Client::builder().no_proxy().build().expect("test client");
        let request_url = Url::parse("http://origin.example/manifest.m3u8").expect("request URL");
        let effective_url = Url::parse("http://127.0.0.1/manifest.m3u8").expect("effective URL");
        let target = super::AttemptTarget {
            request_url: request_url.clone(),
            effective_url: effective_url.clone(),
            host_header: Some("origin.example".to_string()),
            sni_host: None,
            connect_ip: Some("127.0.0.1".parse().expect("connect IP")),
            dns_host: Some("origin.example".to_string()),
        };
        let builder =
            client.get(request_url).header(ACCEPT_ENCODING, "gzip").header(reqwest::header::HOST, "wrong.example");

        let (_, request) = super::prepare_physical_request_attempt(builder, &target, identity_fetch_options())
            .expect("physical request should build");

        assert_eq!(request.url(), &effective_url);
        assert_eq!(request.headers()[reqwest::header::HOST], "origin.example");
        assert_eq!(request.headers()[ACCEPT_ENCODING], "identity");
        assert_eq!(request.timeout(), Some(&Duration::from_secs(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn provider_failover_only_default_options_keep_default_idle_timeout() {
        let (addr, request_seen, server) = start_hanging_http_server().await.expect("hanging origin should start");
        let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
        let input = test_input_source(url.to_string(), None);
        let app_config = test_retry_config(5);
        let client = reqwest::Client::builder().no_proxy().build().expect("test client");
        let request_url = url.clone();
        let request = tokio::spawn(async move {
            send_input_with_retry_and_provider_policy_with_options_result(
                &app_config,
                &client,
                &input,
                None,
                &request_url,
                RequestFetchOptions::default().without_resource_retries(),
            )
            .await
        });

        // A ready task prevents Tokio's paused clock from auto-advancing through the default timeout while the local
        // TCP handshake is still in progress. After the request arrives, this guard stops and the test advances time
        // explicitly to the production deadline.
        let hold_virtual_time = Arc::new(AtomicBool::new(true));
        let hold_virtual_time_for_task = Arc::clone(&hold_virtual_time);
        let virtual_time_guard = tokio::spawn(async move {
            while hold_virtual_time_for_task.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        });
        let request_seen = request_seen.await;
        hold_virtual_time.store(false, Ordering::SeqCst);
        virtual_time_guard.await.expect("virtual time guard should stop");
        request_seen.expect("origin should receive the request");
        tokio::time::advance(Duration::from_secs(STREAM_IDLE_TIMEOUT + 1)).await;
        tokio::task::yield_now().await;
        if !request.is_finished() {
            request.abort();
            panic!("provider-only request lost the default idle-timeout guard");
        }

        let Err(error) = request.await.expect("request task should join") else {
            panic!("hanging request must time out");
        };
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        server.abort();
    }

    #[test]
    fn content_coding_prefix_read_error_is_retryable_but_unsupported_coding_is_not() {
        let prefix_read =
            Error::other(ContentCodingError::PrefixRead(Error::new(ErrorKind::UnexpectedEof, "prefix truncated")));
        let unsupported = Error::other(ContentCodingError::Unsupported("compress".to_string()));

        assert!(should_retry_text_body_error(&prefix_read));
        assert!(!should_retry_text_body_error(&unsupported));
    }

    #[test]
    fn text_response_error_log_labels_never_expose_origin_controlled_details() {
        let unsupported = Error::other(ContentCodingError::Unsupported("signed-token-secret".to_string()));
        assert_eq!(text_response_error_log_label(&unsupported), "content_coding class=unsupported");
        assert!(!text_response_error_log_label(&unsupported).contains("signed-token-secret"));

        let decoding = Error::new(ErrorKind::InvalidData, ContentDecodingIoError { coding: ContentCoding::Zstd });
        assert_eq!(text_response_error_log_label(&decoding), "content_decoding coding=zstd");
    }

    fn test_input_source(url: String, provider: Option<Arc<ConfigProvider>>) -> InputSource {
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
    async fn content_coding_identity_is_reapplied_for_retry() {
        let responses = vec![response_with_body("500 Internal Server Error", ""), response_with_body("200 OK", "ok")];
        let (addr, requests, handle) =
            start_recording_http_server(responses).await.expect("recording origin should start");
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
    async fn content_coding_identity_manifest_retries_temporary_body_transport_error() {
        let truncated = "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n#EXTM3U\n";
        let responses = vec![truncated.to_string(), response_with_body("200 OK", "#EXTM3U\nsegment.ts\n")];
        let (addr, requests, handle) =
            start_recording_http_server(responses).await.expect("recording origin should start");
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
    async fn text_content_manifest_uses_one_budget_for_status_decoder_and_success() {
        let corrupt_gzip =
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
        let responses = vec![
            response_with_body("503 Service Unavailable", ""),
            corrupt_gzip.to_string(),
            response_with_body("200 OK", "#EXTM3U\nsegment.ts\n"),
        ];
        let (addr, requests, handle) =
            start_recording_http_server(responses).await.expect("recording origin should start");
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
    async fn text_content_manifest_retry_budget_is_not_multiplied_by_inner_status_retries() {
        let corrupt_gzip =
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
        let responses = vec![
            response_with_body("503 Service Unavailable", ""),
            response_with_body("502 Bad Gateway", ""),
            corrupt_gzip.to_string(),
            response_with_body("200 OK", "#EXTM3U\nsegment.ts\n"),
        ];
        let (addr, requests, handle) =
            start_recording_http_server(responses).await.expect("recording origin should start");
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
    async fn text_content_manifest_repeated_decoder_failures_stop_at_configured_budget() {
        let corrupt_gzip =
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
        let (addr, requests, handle) =
            start_recording_http_server(vec![corrupt_gzip.to_string()]).await.expect("recording origin should start");
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
        .expect_err("decoder failures must exhaust the configured budget");

        let captured = requests.lock().await;
        assert_eq!(captured.len(), 3);
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
        let provider_entrypoint = start_plain_http_server_with_response(provider_a_response)
            .await
            .expect("provider a test server should start");
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
            resource_retry: ResourceRetryConfig {
                max_attempts: 3,
                backoff_millis: 1,
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

    #[tokio::test]
    async fn test_provider_request_chain_restarts_from_first_url_when_policy_requests_it() {
        let (addr_b, accepted_b, handle_b) = match start_plain_http_server_with_body(b"b").await {
            Ok(server) => server,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping test_provider_request_chain_restarts_from_first_url_when_policy_requests_it: {err}"
                );
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
}
