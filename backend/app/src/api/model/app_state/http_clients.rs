use crate::{
    model::{AppConfig, Config},
    utils::request::{create_client, create_client_with_redirect, PublicIpResolver, ResourceDestinationResolver},
};
use log::error;
use reqwest::Client;
use shared::error::TuliproxError;
use std::{sync::Arc, time::Duration};

/// Creates the default HTTP client.
///
/// Fails if proxy configuration is present but the client cannot be built.
pub fn create_http_client(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let builder = create_client(app_config).http1_only();
    let config = app_config.config.load();
    build_http_client_with_fallback(
        builder,
        &config,
        "Failed to create HTTP client with proxy configuration; refusing to fall back to unconfigured client",
        "HTTP client creation failed with proxy configured",
        "Failed to create HTTP client, using unconfigured http client",
        Client::new,
    )
}

/// Creates a no-redirect HTTP client.
///
/// Fails if proxy configuration is present but the client cannot be built.
///
/// Handling Streaming and Proxy with http/2 is hard, so we strictly use only http/1.1
pub fn create_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    create_no_redirect_client(app_config, None)
}

/// Creates the no-redirect client used for a resource hop that can be reached only from the local
/// network.
///
/// It connects directly, because a configured proxy has no route to such a destination, and its DNS
/// layer refuses addresses local to this host, so a destination cannot resolve to a local address
/// while the connection is built. Private network destinations stay allowed, because self-hosted
/// media servers are the reason this route exists.
pub fn create_resource_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let config = app_config.config.load();
    let mut builder = create_client_with_redirect(app_config, reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(ResourceDestinationResolver::default())
        .http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder.build().map_err(|err| TuliproxError::Config(format!("Failed to create resource HTTP client: {err}")))
}

/// Creates the no-redirect client used for a resource hop that can leave the local network.
///
/// It honours the configured proxy, because a resource fetch must not disclose this host's address to
/// a destination that a proxy was configured to hide it from. Its DNS layer additionally refuses
/// addresses local to this host, so a name that was classified as public cannot resolve to a local
/// address while the connection is built.
pub fn create_resource_public_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    create_no_redirect_client(app_config, Some(Arc::new(ResourceDestinationResolver::allowing_proxy_hosts(app_config))))
}

fn create_no_redirect_client(
    app_config: &AppConfig,
    resolver: Option<Arc<dyn reqwest::dns::Resolve>>,
) -> Result<Client, TuliproxError> {
    let mut builder = create_client_with_redirect(app_config, reqwest::redirect::Policy::none()).http1_only();
    if let Some(resolver) = resolver.clone() {
        builder = builder.dns_resolver(resolver);
    }
    let config = app_config.config.load();
    build_http_client_with_fallback(
        builder,
        &config,
        "Failed to create HTTP client (no redirect) with proxy configuration; refusing to fall back to unconfigured client",
        "HTTP client (no redirect) creation failed with proxy configured",
        "Failed to create HTTP client (no redirect), using unconfigured http client",
        move || {
            let mut fallback = Client::builder().redirect(reqwest::redirect::Policy::none());
            if let Some(resolver) = resolver {
                fallback = fallback.dns_resolver(resolver);
            }
            fallback.build().unwrap_or_else(|err| {
                error!("Failed to create fallback HTTP client (no redirect): {err}");
                Client::new()
            })
        },
    )
}

/// Creates a direct no-redirect client whose connection-time resolver rejects
/// non-public destinations. It is intentionally separate from the general client so
/// configured internal providers keep working.
pub fn create_public_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let config = app_config.config.load();
    let mut builder = create_client_with_redirect(app_config, reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(PublicIpResolver)
        .http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder.build().map_err(|err| TuliproxError::Config(format!("Failed to create public-only HTTP client: {err}")))
}

fn build_http_client_with_fallback(
    mut builder: reqwest::ClientBuilder,
    config: &Arc<Config>,
    proxy_error_log: &str,
    proxy_error_msg: &str,
    fallback_log: &str,
    fallback_client: impl FnOnce() -> Client,
) -> Result<Client, TuliproxError> {
    let proxy_configured = config.proxy.is_some();

    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }

    if let Ok(client) = builder.build() {
        return Ok(client);
    }

    if proxy_configured {
        error!("{proxy_error_log}");
        return Err(TuliproxError::Config(proxy_error_msg.to_string()));
    }

    error!("{fallback_log}");
    Ok(fallback_client())
}
