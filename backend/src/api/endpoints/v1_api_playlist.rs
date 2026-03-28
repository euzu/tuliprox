use crate::{api::{
    api_utils::{create_api_proxy_user, json_or_bin_response},
    endpoints::{
        api_playlist_utils::{get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target},
        extract_accept_header::ExtractAcceptHeader,
        xmltv_api::{rewrite_epg_channel_resource_url, serve_epg_web_ui},
        xtream_api::xtream_get_stream_info_response,
    },
    model::AppState,
}, auth::create_access_token, auth::permission_layer, model::{parse_xmltv_for_web_ui_from_url, AppConfig, ConfigInput, ConfigInputFlags, ConfigInputOptions}, repository::xtream_get_item_for_stream_id};
use axum::{response::IntoResponse, Router};
use log::{debug, error};
use serde_json::json;
use shared::{
    model::{
        permission::Permission,
        InputType, PlaylistEpgRequest, PlaylistRequest, PlaylistUrlResolveRequest, ProxyType, TargetType, UiPlaylistItem,
        XtreamCluster,
    },
    utils::{concat_path_leading_slash, sanitize_sensitive_info, Internable},
};
use std::sync::Arc;
use url::Url;
use shared::utils::deobfuscate_text;
use crate::api::api_utils::resource_response;

fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u_req".intern(),
        input_type: InputType::M3u,
        url: String::from(url),
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix | ConfigInputFlags::ResolveBackground,
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
        }),
        ..Default::default()
    }
}

fn create_config_input_for_xtream(username: &str, password: &str, host: &str) -> ConfigInput {
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
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
        }),
        ..Default::default()
    }
}

fn resolve_provider_url_with_input(input: &ConfigInput, url: &str) -> String {
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

fn resolve_provider_url_for_request(app_config: &AppConfig, playlist_request: &PlaylistRequest, url: &str) -> String {
    if !url.starts_with(shared::utils::PROVIDER_SCHEME_PREFIX) {
        return url.to_string();
    }

    match playlist_request {
        PlaylistRequest::Input(input_id) => app_config
            .get_input_by_id(*input_id)
            .map_or_else(|| url.to_string(), |input| resolve_provider_url_with_input(input.as_ref(), url)),
        PlaylistRequest::Target(target_id) => app_config
            .get_target_by_id(*target_id)
            .and_then(|target| app_config.get_inputs_for_target(&target.name))
            .and_then(|inputs| {
                let mut matches = inputs
                    .into_iter()
                    .filter(|input| input.get_resolve_provider(url).is_some());
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

fn build_playlist_webplayer_url(
    base_url: &str,
    access_token: &str,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> String {
    format!(
        "{base_url}/token/{access_token}/{}/{}/{}",
        target_id,
        cluster.as_stream_type(),
        virtual_id
    )
}

async fn playlist_update(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(targets): axum::extract::Json<Vec<String>>,
) -> impl axum::response::IntoResponse + Send {
    let user_targets = if targets.is_empty() { None } else { Some(targets) };
    let process_targets = app_state.app_config.sources.load().validate_targets(user_targets.as_ref());
    match process_targets {
        Ok(valid_targets) => {
            let valid_targets = Arc::new(valid_targets);
            // Deduplicate rapid clicks: the channel has capacity 1, so at most one
            // update is queued at any time.  Additional requests while the channel
            // is full are silently dropped — the pending run already covers them.
            match app_state.manual_update_sender.try_send(valid_targets) {
                Ok(()) => axum::http::StatusCode::ACCEPTED.into_response(),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    debug!("Manual playlist update deduplicated: an update is already pending or running");
                    axum::http::StatusCode::ACCEPTED.into_response()
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    debug!("Manual playlist update rejected: worker channel closed (server shutting down)");
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }
        Err(err) => {
            error!("Failed playlist update {}", sanitize_sensitive_info(err.to_string().as_str()));
            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response()
        }
    }
}

async fn playlist_content(
    accept: Option<String>,
    app_state: &Arc<AppState>,
    playlist_req: &PlaylistRequest,
    cluster: XtreamCluster,
) -> impl IntoResponse + Send {
    let client = app_state.http_client.load();
    match playlist_req {
        PlaylistRequest::Target(target_id) => get_playlist_for_target(
            app_state.app_config.get_target_by_id(*target_id).as_deref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::Input(input_id) => get_playlist_for_input(
            app_state.app_config.get_input_by_id(*input_id).as_ref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::CustomXtream(xtream) => match Url::parse(&xtream.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                let input = Arc::new(create_config_input_for_xtream(&xtream.username, &xtream.password, &xtream.url));
                get_playlist_for_custom_provider(
                    client.as_ref(),
                    Some(&input),
                    app_state,
                    cluster,
                    accept.as_deref(),
                )
                .await
                .into_response()
            }
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"})),
            )
                .into_response(),
        },
        PlaylistRequest::CustomM3u(m3u) => match Url::parse(&m3u.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                let input = Arc::new(create_config_input_for_m3u(&m3u.url));
                get_playlist_for_custom_provider(
                    client.as_ref(),
                    Some(&input),
                    app_state,
                    cluster,
                    accept.as_deref(),
                )
                .await
                .into_response()
            }
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"})),
            )
                .into_response(),
        },
    }
}

macro_rules! create_player_api_for_cluster {
    ($fn_name:ident, $cluster:expr) => {
        async fn $fn_name(
            ExtractAcceptHeader(accept): ExtractAcceptHeader,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
        ) -> impl IntoResponse + Send {
            playlist_content(accept.clone(), &app_state, &playlist_req, $cluster).await.into_response()
        }
    };
}

create_player_api_for_cluster!(playlist_content_live, XtreamCluster::Live);
create_player_api_for_cluster!(playlist_content_vod, XtreamCluster::Video);
create_player_api_for_cluster!(playlist_content_series, XtreamCluster::Series);

async fn playlist_series_info(
    axum::extract::Path((virtual_id, _provider_id)): axum::extract::Path<(String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    match playlist_req {
        PlaylistRequest::Target(target_id) => {
            if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
                if target.has_output(TargetType::Xtream) {
                    let mut user = create_api_proxy_user(&app_state);
                    user.proxy = ProxyType::Redirect;
                    return xtream_get_stream_info_response(
                        &app_state,
                        &user,
                        &target,
                        &virtual_id,
                        XtreamCluster::Series,
                    )
                    .await
                    .into_response();
                }
            }
        }

        PlaylistRequest::Input(input_id) => {
            if let Some(input) = app_state.app_config.get_input_by_id(input_id) {
                if matches!(input.input_type, InputType::Xtream | InputType::XtreamBatch) {
                    // TODO: Implement series info retrieval for input-based requests
                    debug!("TODO: Implement series info retrieval for input-based requests");
                }
            }
        }
        PlaylistRequest::CustomXtream(_xtream) => {
            // TODO: Implement series info retrieval for custom Xtream requests
            debug!("TODO: Implement series info retrieval for custom Xtream requests");
        }
        PlaylistRequest::CustomM3u(_) => {}
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

fn playlist_webplayer(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> impl axum::response::IntoResponse + Send {
    let access_token = create_access_token(&app_state.app_config.access_token_secret, 30);
    let config = app_state.app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_state.app_config.get_server_info(server_name);
    let base_url = server_info.get_base_url();
    build_playlist_webplayer_url(&base_url, &access_token, target_id, virtual_id, cluster).into_response()
}

async fn playlist_epg(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_epg_req): axum::extract::Json<PlaylistEpgRequest>,
) -> impl IntoResponse + Send {
    match playlist_epg_req {
        PlaylistEpgRequest::Target(target_id) => {
            if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
                let config = &app_state.app_config.config.load();
                if let Some(epg_path) = crate::api::endpoints::xmltv_api::get_epg_path_for_target(config, &target) {
                    return serve_epg_web_ui(&app_state, accept.as_deref(), &epg_path, &target).await;
                }
            }
        }
        PlaylistEpgRequest::Input(_input_id) => {
            // TODO: This is currently not supported, because we could have multiple epg sources for one input
            //     if let Some(target) = app_state.app_config.get_input_by_id(input_id) {
            //         let config = &app_state.app_config.config.load();
            //         if let Some(epg_path) = crate::api::endpoints::xmltv_api::get_epg_path_for_input(config, &target)  {
            //             if let Ok(epg) = parse_xmltv_for_web_ui(&epg_path) {
            //                 return json_or_bin_response(accept.as_ref(), &epg).into_response();
            //             }
            //         }
            //     }
        }
        PlaylistEpgRequest::Custom(url) => {
            if let Ok(epg) = parse_xmltv_for_web_ui_from_url(&app_state, &url).await {
                let config = app_state.app_config.config.load();
                let web_ui_path = config.web_ui.as_ref().and_then(|w| w.path.as_ref()).map_or("", String::as_str);
                let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
                let encrypt_secret = app_state.get_encrypt_secret();
                let epg = epg
                    .into_iter()
                    .map(|channel| rewrite_epg_channel_resource_url(&encrypt_secret, &resource_url, channel))
                    .collect::<Vec<_>>();
                return json_or_bin_response(accept.as_deref(), &epg).into_response();
            }
        }
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn playlist_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Path(resource): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let encrypt_secret = app_state.get_encrypt_secret();
    if let Ok(resource_url) = deobfuscate_text(&encrypt_secret, &resource) {
        resource_response(&app_state, &resource_url, &req_headers, None).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

async fn playlist_resolve_url(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<PlaylistUrlResolveRequest>,
) -> impl IntoResponse + Send {
    match request {
        PlaylistUrlResolveRequest::Webplayer { target_id, virtual_id, cluster } => {
            playlist_webplayer(
                axum::extract::State(app_state),
                target_id,
                virtual_id,
                cluster,
            )
            .into_response()
        }
        PlaylistUrlResolveRequest::Provider { playlist_request, url } => {
            resolve_provider_url_for_request(&app_state.app_config, &playlist_request, &url).into_response()
        }
    }
}

pub fn v1_api_playlist_register_protected(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist/epg", axum::routing::post(playlist_epg))
        .route("/playlist/live", axum::routing::post(playlist_content_live))
        .route("/playlist/vod", axum::routing::post(playlist_content_vod))
        .route("/playlist/series", axum::routing::post(playlist_content_series))
        .route("/playlist/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/playlist/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
}

pub fn v1_api_playlist_register_public(
    router: Router<Arc<AppState>>,
) -> axum::Router<Arc<AppState>> {
    router.route("/playlist/resource/{resource}", axum::routing::get(playlist_resource))
}

pub fn v1_api_playlist_register_with_permissions(
    router: Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
) -> axum::Router<Arc<AppState>> {

    let read_routes = Router::new()
        .route("/live", axum::routing::post(playlist_content_live))
        .route("/vod", axum::routing::post(playlist_content_vod))
        .route("/series", axum::routing::post(playlist_content_series))
        .route("/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
        .layer(permission_layer!(app_state, Permission::PlaylistRead));

    let write_routes = Router::new()
        .route("/update", axum::routing::post(playlist_update))
        .layer(permission_layer!(app_state, Permission::PlaylistWrite));

    let epg_routes = Router::new()
        .route("/epg", axum::routing::post(playlist_epg))
        .layer(permission_layer!(app_state, Permission::EpgRead));

    router.nest("/playlist",
                read_routes
                    .merge(write_routes)
                    .merge(epg_routes)
    )
}

#[cfg(test)]
mod tests {
    use super::resolve_provider_url_for_request;
    use crate::model::{AppConfig, Config, ConfigInput, ConfigProvider, ConfigSource, ConfigTarget, SourcesConfig};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::foundation::Filter;
    use shared::{
        model::{ConfigPaths, ConfigProviderDto, PlaylistRequest, XtreamCluster},
        utils::Internable,
    };
    use std::sync::Arc;

    fn test_app_config(input: Arc<ConfigInput>, source: ConfigSource) -> AppConfig {
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            inputs: vec![input],
            sources: vec![source],
            templates: None,
        };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
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
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        }
    }

    #[test]
    fn resolve_provider_url_for_input_request_rewrites_provider_scheme() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input, source);
        let resolved = resolve_provider_url_for_request(
            &app_config,
            &PlaylistRequest::Input(7),
            "provider://demo/live/user/pass/1359.ts",
        );

        assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
    }

    #[test]
    fn resolve_provider_url_for_target_request_rewrites_provider_scheme() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: Default::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
        let app_config = test_app_config(input, source);
        let resolved = resolve_provider_url_for_request(
            &app_config,
            &PlaylistRequest::Target(11),
            "provider://demo/live/user/pass/1359.ts",
        );

        assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
    }

    #[test]
    fn resolve_provider_url_passthrough_for_unresolved_provider_input_request() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input, source);
        let original = "provider://unknown/live/user/pass/1359.ts";
        let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Input(7), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn resolve_provider_url_passthrough_for_unresolved_provider_target_request() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: Default::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
        let app_config = test_app_config(input, source);
        let original = "provider://unknown/live/user/pass/1359.ts";
        let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn resolve_provider_url_passthrough_for_ambiguous_target_request() {
        let provider_a = ConfigProvider::from(&ConfigProviderDto {
            name: "shared".intern(),
            urls: vec!["http://provider-a.example".intern()],
            dns: None,
        });
        let provider_b = ConfigProvider::from(&ConfigProviderDto {
            name: "shared".intern(),
            urls: vec!["http://provider-b.example".intern()],
            dns: None,
        });
        let input_a = Arc::new(ConfigInput {
            id: 7,
            name: "input-a".intern(),
            provider_configs: Some(vec![Arc::new(provider_a)]),
            ..Default::default()
        });
        let input_b = Arc::new(ConfigInput {
            id: 8,
            name: "input-b".intern(),
            provider_configs: Some(vec![Arc::new(provider_b)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: Default::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source = ConfigSource {
            inputs: vec![Arc::clone(&input_a.name), Arc::clone(&input_b.name)],
            targets: vec![target],
        };
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            inputs: vec![input_a, input_b],
            sources: vec![source],
            templates: None,
        };

        let app_config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
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
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        };

        let original = "provider://shared/live/user/pass/1359.ts";
        let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn build_playlist_webplayer_url_uses_cluster_stream_type() {
        let live = super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Live);
        let movie = super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Video);
        let series =
            super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Series);

        assert_eq!(live, "http://player.example/token/token123/1/live/42");
        assert_eq!(movie, "http://player.example/token/token123/1/movie/42");
        assert_eq!(series, "http://player.example/token/token123/1/series/42");
    }
}

async fn playlist_episode_item(
    axum::extract::Path(virtual_id): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    if let PlaylistRequest::Target(target_id) = playlist_req {
        if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
            if target.has_output(TargetType::Xtream) {
                if let Ok(vid) = virtual_id.parse::<u32>() {
                    if let Ok(pli) =
                        xtream_get_item_for_stream_id(vid, &app_state, &target, Some(XtreamCluster::Series)).await
                    {
                        return axum::Json(json!(UiPlaylistItem::from(pli))).into_response();
                    }
                }
            }
        }
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}
