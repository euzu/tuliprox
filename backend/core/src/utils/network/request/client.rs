use super::{get_request_headers, log_proxy_diagnostics};
use crate::model::{AppConfig, Config, ReverseProxyDisabledHeaderConfig};
use log::error;
use reqwest::redirect::Policy;
use shared::{error::TuliproxError, model::InputFetchMethod};
use std::{collections::HashMap, time::Duration};
use url::Url;

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

pub fn create_client_with_redirect(cfg: &AppConfig, redirect_policy: Policy) -> reqwest::ClientBuilder {
    configured_client(&cfg.config.load(), redirect_policy)
}

fn configured_client(config: &Config, redirect_policy: Policy) -> reqwest::ClientBuilder {
    log_proxy_diagnostics(config);
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

/// Dedicated discovery profile. Callers must propagate construction failure, never
/// fall back to an unconfigured client. Builder access permits local CA/DNS fixtures.
pub fn create_tmdb_client(cfg: &AppConfig) -> Result<reqwest::ClientBuilder, TuliproxError> {
    let config = cfg.config.load();
    let invalid_proxy = || TuliproxError::Config("TMDB HTTP profile has invalid proxy configuration".to_string());
    // The general factory historically logs malformed proxies and continues. TMDB
    // must fail closed, using the same snapshot and proxy transformations instead.
    if let Some(proxy) = &config.proxy {
        let mut url = Url::parse(&proxy.url).map_err(|_| invalid_proxy())?;
        match url.scheme() {
            "socks5" | "socks5h" => {
                if let Some(user) = &proxy.username {
                    url.set_username(user).map_err(|()| invalid_proxy())?;
                }
                if let Some(pass) = &proxy.password {
                    url.set_password(Some(pass)).map_err(|()| invalid_proxy())?;
                }
            }
            "http" | "https" => {}
            _ => return Err(invalid_proxy()),
        }
        reqwest::Proxy::all(url.as_str()).map_err(|_| invalid_proxy())?;
    }
    // Discovery carries its own Bearer credential: never inherit the generic TLS bypass.
    // Keep configured proxy/auth and trusted roots, including hostname verification.
    let mut builder = configured_client(&config, Policy::none())
        .danger_accept_invalid_certs(false)
        .retry(reqwest::retry::never())
        .http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    Ok(builder)
}
