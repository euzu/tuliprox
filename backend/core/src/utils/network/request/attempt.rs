use super::{apply_request_fetch_options, create_client, provider_start_index, RequestFetchOptions};
use crate::model::{resolve_provider_scheme_url_with_provider_index, AppConfig, ConfigProvider};
use log::debug;
use reqwest::header::{HeaderValue, HOST};
use shared::{error::string_to_io_error, utils::sanitize_sensitive_info};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use url::Url;

pub(super) fn prepare_physical_request_attempt(
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
pub(super) struct AttemptTarget {
    pub(super) request_url: Url,
    pub(super) effective_url: Url,
    pub(super) host_header: Option<String>,
    pub(super) sni_host: Option<String>,
    pub(super) connect_ip: Option<IpAddr>,
    pub(super) dns_host: Option<String>,
}

impl AttemptTarget {
    pub(super) fn new(url: Url) -> Self {
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
pub(super) fn resolve_attempt_target(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, false, 0)
}

pub(super) fn resolve_attempt_target_at_provider_index(
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    provider_url_index: usize,
) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, false, provider_url_index)
}

pub(super) fn preview_attempt_target(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> AttemptTarget {
    resolve_attempt_target_with_dns_mode(url, provider, true, provider_start_index(provider))
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

pub(super) async fn execute_attempt_request(
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
