use super::playlist_webplayer;
use crate::{
    api::{
        api_utils::{is_same_origin, resource_proxy_response},
        endpoints::api_playlist_utils::STALKER_RESOURCE_SCHEME,
        model::AppState,
    },
    iptv::stalker::client::validate_public_playable_url,
    model::{AppConfig, ConfigInput, ConfigInputFlags, ConfigInputOptions, ConfigInputUpdateQuality},
    processing::processor::re_resolve_stalker_url,
};
use axum::response::IntoResponse;
use log::error;
use shared::{
    model::{stalker::StalkerStreamKind, InputType, PlaylistRequest, PlaylistUrlResolveRequest, XtreamCluster},
    utils::{open_web_ui_resource_url, sanitize_sensitive_info, Internable},
};
use std::sync::Arc;
use url::Url;

pub(super) fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u_req".intern(),
        input_type: InputType::M3u,
        url: String::from(url),
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix | ConfigInputFlags::ResolveBackground,
            update_quality: ConfigInputUpdateQuality::default(),
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
            flussonic_hls_catchup: shared::model::FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: shared::model::default_flussonic_hls_catchup_max_duration_secs(),
            default_container_extension: None,
        }),
        ..Default::default()
    }
}

pub(super) fn create_config_input_for_xtream(username: &str, password: &str, host: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "xc_req".intern(),
        input_type: InputType::Xtream,
        url: String::from(host),
        username: Some(String::from(username)),
        password: Some(String::from(password)),
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix | ConfigInputFlags::ResolveBackground,
            update_quality: ConfigInputUpdateQuality::default(),
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
            flussonic_hls_catchup: shared::model::FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: shared::model::default_flussonic_hls_catchup_max_duration_secs(),
            default_container_extension: None,
        }),
        ..Default::default()
    }
}

pub(super) fn resolve_provider_url_with_input(input: &ConfigInput, url: &str) -> String {
    match input.resolve_url(url) {
        Ok(resolved) => resolved.into_owned(),
        Err(err) => {
            let sanitized_url = sanitize_sensitive_info(url);
            let err_text = err.to_string();
            let sanitized_err = sanitize_sensitive_info(&err_text);
            error!("resolve_provider_url_with_input failed for url '{sanitized_url}': {sanitized_err}");
            url.to_string()
        }
    }
}

pub(super) fn resolve_provider_url_for_request(
    app_config: &AppConfig,
    playlist_request: &PlaylistRequest,
    url: &str,
) -> String {
    if !url.starts_with(shared::utils::PROVIDER_SCHEME_PREFIX) {
        return url.to_string();
    }

    match playlist_request {
        PlaylistRequest::Input(input_name) => app_config
            .get_input_by_name(&input_name.intern())
            .map_or_else(|| url.to_string(), |input| resolve_provider_url_with_input(input.as_ref(), url)),
        PlaylistRequest::Target(target_id) => app_config
            .get_target_by_id(*target_id)
            .and_then(|target| app_config.get_inputs_for_target(&target.name))
            .and_then(|inputs| {
                let mut matches = inputs.into_iter().filter(|input| input.get_resolve_provider(url).is_some());
                let first = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                Some(resolve_provider_url_with_input(first.as_ref(), url))
            })
            .unwrap_or_else(|| url.to_string()),
        PlaylistRequest::CustomXtream(_) | PlaylistRequest::CustomM3u(_) => url.to_string(),
    }
}

/// Headers of a Web UI image request that may reach the provider. The browser's own User-Agent is
/// dropped: providers that only serve players answer it with 403, which left most posters empty.
/// The input's configured headers or the configured default User-Agent apply instead.
fn web_ui_resource_headers(req_headers: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    let mut headers = req_headers.clone();
    headers.remove(axum::http::header::USER_AGENT);
    headers
}

/// Credential headers. A Web UI resource token carries only the URL, not the input it came from,
/// so an origin match may come from any playlist URL; credentials are never sent on that basis.
fn is_credential_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie")
        || ["auth", "token", "secret", "api-key", "apikey"].iter().any(|part| name.contains(part))
}

/// The origin URL of `input` that serves `resource`: its own URL, or for a `provider://` URL
/// the matching URL of its provider.
fn url_origin_for(input: &ConfigInput, url: &str, resource: &Url) -> Option<String> {
    match input.get_resolve_provider(url) {
        Some(provider) => {
            provider.urls.iter().find(|provider_url| is_same_origin(provider_url, resource)).map(ToString::to_string)
        }
        None => is_same_origin(url, resource).then(|| url.to_string()),
    }
}

/// The origin URL of `input` or one of its enabled aliases that serves `resource`.
fn input_origin_for(input: &ConfigInput, resource: &Url) -> Option<String> {
    url_origin_for(input, &input.url, resource).or_else(|| {
        input
            .aliases
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|alias| alias.enabled)
            .find_map(|alias| url_origin_for(input, &alias.url, resource))
    })
}

/// The input whose server hosts `resource_url`, so its configured headers (User-Agent, ...) are sent.
/// The copy is fetched with GET, as image requests always are, and without credential headers.
/// Its URL is the matched origin, so the proxy's own origin check accepts it.
fn web_ui_resource_input(app_state: &Arc<AppState>, resource_url: &str) -> Option<Arc<ConfigInput>> {
    let url = shared::model::persisted_resource_url(resource_url)?;
    let resource = Url::parse(url.as_ref()).ok()?;
    let sources = app_state.app_config.sources.load();
    sources.inputs.iter().find_map(|input| {
        let origin = input_origin_for(input, &resource)?;
        // Aliases share the parent's headers and method (`ConfigInput::as_input`).
        let mut prepared = input.as_ref().clone();
        prepared.url = origin;
        prepared.method = shared::model::InputFetchMethod::GET;
        prepared.headers.retain(|name, _| !is_credential_header(name));
        Some(Arc::new(prepared))
    })
}

pub(super) async fn playlist_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Path(resource): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let encrypt_secret = app_state.get_encrypt_secret();
    if let Ok(resource_url) = open_web_ui_resource_url(&encrypt_secret, &resource) {
        if let Some((input_id, cluster, provider_id)) = parse_stalker_resource(&resource_url) {
            return stalker_resource_response(&app_state, input_id, cluster, provider_id).await;
        }
        // This route serves every icon the Web UI shows, so it is classified like the player routes:
        // a public destination goes out through the proxy-aware fetch, which honours a configured
        // proxy, while one that is not provably public is fetched directly so the request cannot leave
        // through a proxy that has no route to it.
        let headers = web_ui_resource_headers(&req_headers);
        let input = web_ui_resource_input(&app_state, &resource_url);
        resource_proxy_response(&app_state, &resource_url, &headers, input.as_deref()).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

fn parse_stalker_resource(resource: &str) -> Option<(u16, XtreamCluster, u32)> {
    let mut parts = resource.strip_prefix(STALKER_RESOURCE_SCHEME)?.split('/');
    let input_id = parts.next()?.parse().ok()?;
    let cluster = parts.next()?.parse().ok()?;
    let provider_id = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((input_id, cluster, provider_id))
}

/// Resolves a Stalker preview link for the input's own account and redirects to it after the public URL check.
async fn stalker_resource_response(
    app_state: &Arc<AppState>,
    input_id: u16,
    cluster: XtreamCluster,
    provider_id: u32,
) -> axum::response::Response {
    let Some(input) = app_state.app_config.get_input_by_id(input_id).filter(|input| input.input_type.is_stalker())
    else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let kind = match cluster {
        XtreamCluster::Live => StalkerStreamKind::Live,
        XtreamCluster::Video => StalkerStreamKind::Movie,
        XtreamCluster::Series => StalkerStreamKind::Episode,
    };
    let client = app_state.http_clients.default.load().as_ref().clone();
    match re_resolve_stalker_url(&app_state.app_config, &client, &input, &input.name, provider_id, kind, false).await {
        Ok(Some(resolved_url)) => {
            let Ok(url) = Url::parse(&resolved_url) else {
                return axum::http::StatusCode::BAD_GATEWAY.into_response();
            };
            if validate_public_playable_url(&url).await.is_err() {
                return axum::http::StatusCode::BAD_GATEWAY.into_response();
            }
            axum::response::Redirect::temporary(url.as_str()).into_response()
        }
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            error!("Failed to resolve Stalker preview stream: {}", sanitize_sensitive_info(&err.to_string()));
            axum::http::StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

pub(super) async fn playlist_resolve_url(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<PlaylistUrlResolveRequest>,
) -> impl IntoResponse + Send {
    match request {
        PlaylistUrlResolveRequest::Webplayer { target_id, virtual_id, cluster } => {
            playlist_webplayer(axum::extract::State(app_state), target_id, virtual_id, cluster).into_response()
        }
        PlaylistUrlResolveRequest::Provider { playlist_request, url } => {
            resolve_provider_url_for_request(&app_state.app_config, &playlist_request, &url).into_response()
        }
    }
}

#[cfg(test)]
mod web_ui_resource_tests {
    use super::{web_ui_resource_headers, web_ui_resource_input};
    use crate::{
        api::api_utils::resource_input_for_url,
        model::{ConfigInput, ConfigProvider, ProviderDnsCache, SourcesConfig},
    };
    use axum::http::{header, HeaderMap, HeaderValue};
    use shared::{
        model::{InputFetchMethod, InputType, ProviderUrlSelectionPolicy},
        utils::Internable,
    };
    use std::{collections::HashMap, sync::Arc};

    #[test]
    fn browser_user_agent_is_not_forwarded_to_the_provider() {
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0 Chrome/140"));
        headers.insert(header::ACCEPT, HeaderValue::from_static("image/avif,image/webp"));
        let forwarded = web_ui_resource_headers(&headers);
        assert!(forwarded.get(header::USER_AGENT).is_none());
        assert_eq!(forwarded.get(header::ACCEPT).and_then(|v| v.to_str().ok()), Some("image/avif,image/webp"));
    }

    #[tokio::test]
    async fn provider_scheme_input_resolves_origins_and_sanitizes_for_web_ui() {
        let provider = Arc::new(ConfigProvider {
            name: "demo".into(),
            urls: vec!["http://10.0.0.1:8080".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
            current_url_index: std::sync::atomic::AtomicUsize::new(0),
            dns: None,
            dns_cache: Arc::new(ProviderDnsCache::default()),
        });

        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), "DemoPlayer/1.0".to_string());
        headers.insert("authorization".to_string(), "Bearer secret-token".to_string());
        headers.insert("cookie".to_string(), "session=abc".to_string());
        headers.insert("x-api-key".to_string(), "key-123".to_string());

        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider-input".intern(),
            input_type: InputType::Xtream,
            url: "provider://demo".to_string(),
            provider_configs: Some(vec![provider]),
            headers,
            method: InputFetchMethod::POST,
            enabled: true,
            ..Default::default()
        });

        let app_state = crate::api::model::create_test_app_state(crate::model::Config::default());
        let sources = SourcesConfig { inputs: vec![Arc::clone(&input)], ..Default::default() };
        app_state.app_config.sources.store(Arc::new(sources));

        let logo_url = "http://10.0.0.1:8080/logo.png";
        let matched = web_ui_resource_input(&app_state, logo_url).expect("provider input should resolve and match");

        // Finding 2: Origin is resolved so resource_input_for_url also matches
        assert!(resource_input_for_url(Some(&matched), logo_url).is_some());
        assert_eq!(matched.headers.get("user-agent").map(String::as_str), Some("DemoPlayer/1.0"));

        // Finding 3: Auth headers stripped
        assert!(!matched.headers.contains_key("authorization"));
        assert!(!matched.headers.contains_key("cookie"));
        assert!(!matched.headers.contains_key("x-api-key"));

        // Finding 4: Method is GET even though input was POST
        assert_eq!(matched.method, InputFetchMethod::GET);
    }

    #[tokio::test]
    async fn http_scheme_input_sanitizes_auth_headers_and_forces_get_method() {
        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), "CustomUA/2.0".to_string());
        headers.insert("authorization".to_string(), "Basic dXNlcjpwYXNz".to_string());
        headers.insert("cookie".to_string(), "token=xyz".to_string());

        let input = Arc::new(ConfigInput {
            id: 2,
            name: "http-input".intern(),
            input_type: InputType::M3u,
            url: "http://example.com:8080/playlist.m3u8".to_string(),
            headers,
            method: InputFetchMethod::POST,
            enabled: true,
            ..Default::default()
        });

        let app_state = crate::api::model::create_test_app_state(crate::model::Config::default());
        let sources = SourcesConfig { inputs: vec![Arc::clone(&input)], ..Default::default() };
        app_state.app_config.sources.store(Arc::new(sources));

        let logo_url = "http://example.com:8080/logo.png";
        let matched = web_ui_resource_input(&app_state, logo_url).expect("http input should match by origin");

        assert!(resource_input_for_url(Some(&matched), logo_url).is_some());
        assert_eq!(matched.headers.get("user-agent").map(String::as_str), Some("CustomUA/2.0"));
        assert!(!matched.headers.contains_key("authorization"));
        assert!(!matched.headers.contains_key("cookie"));
        assert_eq!(matched.method, InputFetchMethod::GET);
    }

    fn alias(id: u16, url: &str, enabled: bool) -> crate::model::ConfigInputAlias {
        crate::model::ConfigInputAlias {
            id,
            name: format!("alias-{id}").intern(),
            url: url.to_string(),
            username: None,
            password: None,
            priority: 0,
            max_connections: 0,
            exp_date: None,
            enabled,
            stalker: None,
        }
    }

    #[tokio::test]
    async fn enabled_alias_origin_keeps_the_input_user_agent() {
        let provider = Arc::new(ConfigProvider {
            name: "mirror".into(),
            urls: vec!["http://10.0.0.9:8080".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
            current_url_index: std::sync::atomic::AtomicUsize::new(0),
            dns: None,
            dns_cache: Arc::new(ProviderDnsCache::default()),
        });
        let input = Arc::new(ConfigInput {
            id: 3,
            name: "aliased-input".intern(),
            input_type: InputType::Xtream,
            url: "http://primary.example:8080".to_string(),
            provider_configs: Some(vec![provider]),
            headers: HashMap::from([("user-agent".to_string(), "AliasUA/1.0".to_string())]),
            aliases: Some(vec![
                alias(4, "http://backup.example:8080", true),
                alias(5, "provider://mirror", true),
                alias(6, "http://disabled.example:8080", false),
            ]),
            enabled: true,
            ..Default::default()
        });
        let app_state = crate::api::model::create_test_app_state(crate::model::Config::default());
        let sources = SourcesConfig { inputs: vec![Arc::clone(&input)], ..Default::default() };
        app_state.app_config.sources.store(Arc::new(sources));

        for logo_url in ["http://backup.example:8080/logo.png", "http://10.0.0.9:8080/logo.png"] {
            let matched = web_ui_resource_input(&app_state, logo_url).expect(logo_url);
            assert!(resource_input_for_url(Some(&matched), logo_url).is_some(), "{logo_url}");
            assert_eq!(matched.headers.get("user-agent").map(String::as_str), Some("AliasUA/1.0"), "{logo_url}");
        }
        assert!(web_ui_resource_input(&app_state, "http://disabled.example:8080/logo.png").is_none());
    }
}
