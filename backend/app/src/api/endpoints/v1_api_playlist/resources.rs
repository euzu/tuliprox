use super::playlist_webplayer;
use crate::{
    api::{
        api_utils::resource_proxy_response, endpoints::api_playlist_utils::STALKER_RESOURCE_SCHEME, model::AppState,
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
        resource_proxy_response(&app_state, &resource_url, &req_headers, None).await.into_response()
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
    match re_resolve_stalker_url(&app_state.app_config, &client, &input, provider_id, kind, false).await {
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
