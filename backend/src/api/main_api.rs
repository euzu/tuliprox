use crate::api::api_utils::{get_build_time, get_server_time};
use crate::api::config_watch::exec_config_watch;
use crate::api::endpoints::custom_video_stream_api::cvs_api_register;
use crate::api::endpoints::hdhomerun_api::hdhr_api_register;
use crate::api::endpoints::hls_api::hls_api_register;
use crate::api::endpoints::m3u_api::m3u_api_register;
use crate::api::endpoints::v1_api::v1_api_register;
use crate::api::endpoints::web_index::{index_register_with_path, index_register_without_path};
use crate::api::endpoints::websocket_api::ws_api_register;
use crate::api::endpoints::xmltv_api::xmltv_api_register;
use crate::api::endpoints::xtream_api::xtream_api_register;
use crate::api::hdhomerun_proprietary::spawn_proprietary_tasks;
use crate::api::hdhomerun_ssdp::spawn_ssdp_discover_task;
use crate::api::model::{create_cache, create_http_client, ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadQueue, EventManager, HdHomerunAppState, PlaylistStorageState, SharedStreamManager, UpdateGuard};
use crate::api::scheduler::{exec_interner_prune, exec_scheduler};
use crate::api::serve::serve;
use crate::model::{AppConfig, Config, Healthcheck, ProcessTargets, RateLimitConfig};
use crate::processing::processor::playlist;
use crate::repository::playlist_repository::load_playlists_into_memory_cache;
use crate::VERSION;
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::Router;
use axum::{middleware::Next, extract::Request};
use axum::extract::connect_info::ConnectInfo;
use log::{debug, error, info};
use shared::utils::{concat_path_leading_slash, sanitize_sensitive_info};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicI8;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_http::services::ServeDir;
use crate::api::panel_api::sync_panel_api_exp_dates_on_boot;
use crate::api::sys_usage::exec_system_usage;
use crate::repository::storage::get_geoip_path;
use crate::utils::{exec_file_lock_prune, GeoIp};

fn get_web_dir_path(web_ui_enabled: bool, web_root: &str) -> Result<PathBuf, std::io::Error> {
    let web_dir = web_root.to_string();
    let web_dir_path = PathBuf::from(&web_dir);
    if web_ui_enabled && (!&web_dir_path.exists() || !&web_dir_path.is_dir()) {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            format!(
                "web_root does not exists or is not an directory: {}",
                web_dir_path.display()
            ),
        ));
    }
    Ok(web_dir_path)
}

fn create_healthcheck() -> Healthcheck {
    Healthcheck {
        status: "ok".to_string(),
        version: VERSION.to_string(),
        build_time: get_build_time(),
        server_time: get_server_time(),
    }
}

async fn healthcheck() -> impl axum::response::IntoResponse {
    axum::Json(create_healthcheck())
}

async fn create_shared_data(
    app_config: &Arc<AppConfig>,
    forced_targets: &Arc<ProcessTargets>,
) -> AppState {
    let config = app_config.config.load();

    let use_geoip = config.is_geoip_enabled();
    let geoip = if use_geoip {
        let path = get_geoip_path(&config.working_dir);
        let _file_lock = app_config.file_locks.read_lock(&path).await;
        match GeoIp::load(&path) {
            Ok(db) => {
                info!("GeoIp db loaded");
                Arc::new(ArcSwapOption::from(Some(Arc::new(db))))
            }
            Err(err) => {
                info!("No GeoIp db found: {err}");
                Arc::new(ArcSwapOption::from(None))
            }
        }
    } else {
        Arc::new(ArcSwapOption::from(None))
    };

    let cache = create_cache(&config);
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(app_config, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    let active_users = Arc::new(ActiveUserManager::new(&config,&geoip, &event_manager));
    let connection_manager = Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager));

    let client = create_http_client(app_config);

    AppState {
        forced_targets: Arc::new(ArcSwap::new(Arc::clone(forced_targets))),
        app_config: Arc::clone(app_config),
        http_client: Arc::new(ArcSwap::from_pointee(client)),
        downloads: Arc::new(DownloadQueue::new()),
        cache: Arc::new(ArcSwapOption::from(cache)),
        shared_stream_manager,
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: Arc::new(ArcSwap::from_pointee(CancelTokens::default())),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        update_guard: UpdateGuard::new(),
    }
}

fn exec_update_on_boot(
    client: &reqwest::Client,
    app_state: &Arc<AppState>,
    targets: &Arc<ProcessTargets>,
) {
    let cfg = &app_state.app_config;
    let update_on_boot = {
        let config = cfg.config.load();
        config.update_on_boot
    };
    if update_on_boot {
        let app_config_clone = Arc::clone(&app_state.app_config);
        let targets_clone = Arc::clone(targets);
        let playlist_state = Arc::clone(&app_state.playlists);
        let client = client.clone();
        let update_guard = Some(app_state.update_guard.clone());
        tokio::spawn(async move {
            playlist::exec_processing(&client, app_config_clone, targets_clone, None, Some(playlist_state), update_guard).await;
        });
    }
}

fn is_web_auth_enabled(cfg: &Arc<Config>, web_ui_enabled: bool) -> bool {
    if web_ui_enabled {
        if let Some(web_auth) = &cfg.web_ui.as_ref().and_then(|c| c.auth.as_ref()) {
            return web_auth.enabled;
        }
    }
    false
}

fn create_cors_layer() -> tower_http::cors::CorsLayer {
    tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
            axum::http::Method::HEAD,
        ])
        .allow_headers(tower_http::cors::Any)
        .max_age(std::time::Duration::from_secs(3600))
}
fn create_compression_layer() -> tower_http::compression::CompressionLayer {
    tower_http::compression::CompressionLayer::new()
        .br(true)
        .deflate(true)
        .gzip(true)
        .zstd(true)
}

pub(in crate::api) fn start_hdhomerun(
    app_config: &Arc<AppConfig>,
    app_state: &Arc<AppState>,
    infos: &mut Vec<String>,
    cancel_token: &CancellationToken,
) {
    let config = app_config.config.load();
    let host = config.api.host.clone();
    let guard = app_config.hdhomerun.load();
    if let Some(hdhomerun) = &*guard {
        if hdhomerun.enabled {
            if hdhomerun.ssdp_discovery {
                info!("HDHomeRun SSDP discovery is enabled.");
                spawn_ssdp_discover_task(
                    Arc::clone(app_config),
                    host.clone(),
                    cancel_token.clone(),
                );
            } else {
                info!("HDHomeRun SSDP discovery is disabled.");
            }

            if hdhomerun.proprietary_discovery {
                info!("HDHomeRun proprietary discovery is enabled.");
                spawn_proprietary_tasks(
                    Arc::clone(app_state),
                    host.clone(),
                    cancel_token.clone(),
                );
            } else {
                info!("HDHomeRun proprietary discovery is disabled.");
            }

            for device in &hdhomerun.devices {
                if device.t_enabled {
                    let app_data = Arc::clone(app_state);
                    let app_host = host.clone();
                    let port = device.port;
                    let device_clone = Arc::new(device.clone());
                    let basic_auth = hdhomerun.auth;
                    infos.push(format!(
                        "HdHomeRun Server '{}' running: http://{host}:{port}",
                        device.name
                    ));
                    let c_token = cancel_token.clone();
                    let connection_manager = Arc::clone(&app_data.connection_manager);
                    tokio::spawn(async move {
                        let router = axum::Router::<Arc<HdHomerunAppState>>::new()
                            .layer(create_cors_layer())
                            .layer(create_compression_layer())
                            .merge(hdhr_api_register(basic_auth));

                        let router: axum::Router<()> =
                            router.with_state(Arc::new(HdHomerunAppState {
                                app_state: Arc::clone(&app_data),
                                device: Arc::clone(&device_clone),
                                hd_scan_state: Arc::new(AtomicI8::new(-1)),
                            }));

                        match tokio::net::TcpListener::bind(format!("{}:{}", app_host.clone(), port))
                            .await
                        {
                            Ok(listener) => {
                                serve(listener, router, Some(c_token), &connection_manager).await;
                            }
                            Err(err) => error!("{err}"),
                        }
                    });
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn start_server(
    app_config: Arc<AppConfig>,
    targets: Arc<ProcessTargets>,
) -> futures::io::Result<()> {
    let mut infos = Vec::new();
    let cfg = app_config.config.load();
    let host = cfg.api.host.clone();
    let port = cfg.api.port;
    let web_ui_enabled = cfg.web_ui.as_ref().is_some_and(|c| c.enabled);
    let web_dir_path = match get_web_dir_path(web_ui_enabled, cfg.api.web_root.as_str()) {
        Ok(result) => result,
        Err(err) => return Err(err),
    };
    if web_ui_enabled {
        infos.push(format!("Web root: {}", web_dir_path.display()));
    }
    let app_shared_data = create_shared_data(&app_config, &targets).await;
    let app_state = Arc::new(app_shared_data);
    let shared_data = Arc::clone(&app_state);

    let (cancel_token_scheduler, cancel_token_hdhomerun, cancel_token_file_watch) = {
        let cancel_tokens = app_state.cancel_tokens.load();
        (
            cancel_tokens.scheduler.clone(),
            cancel_tokens.hdhomerun.clone(),
            cancel_tokens.file_watch.clone(),
        )
    };

    if let Err(err) = load_playlists_into_memory_cache(&app_state).await {
        error!("Failed to load playlists into memory cache: {err}");
    }

    exec_system_usage(&app_state);

    let client = shared_data.http_client.load();

    exec_scheduler(
        client.as_ref(),
        &app_state,
        &targets,
        &cancel_token_scheduler,
    );

    exec_update_on_boot(
        client.as_ref(),
        &app_state,
        &targets,
    );

    sync_panel_api_exp_dates_on_boot(&app_state).await;

    exec_file_lock_prune(&app_state);

    exec_interner_prune(&app_state);

    exec_config_watch(&app_state, &cancel_token_file_watch);

    let web_auth_enabled = is_web_auth_enabled(&cfg, web_ui_enabled);

    if app_config.api_proxy.load().is_some() {
        start_hdhomerun(&app_config, &app_state, &mut infos, &cancel_token_hdhomerun);
    }

    let web_ui_path = cfg
        .web_ui
        .as_ref()
        .and_then(|c| c.path.as_ref())
        .cloned()
        .unwrap_or_default();
    infos.push(format!("Server running: http://{}:{}", &cfg.api.host, &cfg.api.port));
    for info in &infos {
        info!("{info}");
    }

    // Web Server
    let mut router = axum::Router::new()
        .route("/healthcheck", axum::routing::get(healthcheck))
        .nest_service("/.well-known", ServeDir::new(web_dir_path.join("static/.well-known")))
        .merge(ws_api_register(
            web_auth_enabled,
            web_ui_path.as_str(),
        ));
    if web_ui_enabled {
        router = router
            .nest_service(
                &concat_path_leading_slash(&web_ui_path, "static"),
                tower_http::services::ServeDir::new(web_dir_path.join("static")),
            )
            .nest_service(
                &concat_path_leading_slash(&web_ui_path, "assets"),
                tower_http::services::ServeDir::new(web_dir_path.join("assets")),
            )
            .merge(v1_api_register(
                web_auth_enabled,
                Arc::clone(&shared_data),
                web_ui_path.as_str(),
            ));
        if !web_ui_path.is_empty() {
            router = router.merge(index_register_with_path(
                &web_dir_path,
                web_ui_path.as_str(),
            ));
        }
    }

    let mut api_router = axum::Router::new()
        .merge(xtream_api_register())
        .merge(m3u_api_register())
        .merge(xmltv_api_register())
        .merge(hls_api_register())
        .merge(cvs_api_register());
    if let Some(rate_limiter) = cfg
        .reverse_proxy
        .as_ref()
        .and_then(|r| r.rate_limit.clone())
    {
        api_router = add_rate_limiter(api_router, &rate_limiter);
    }

    router = router.merge(api_router);

    if web_ui_enabled && web_ui_path.is_empty() {
        router = router.merge(index_register_without_path(&web_dir_path));
    }

    router = router
        .layer(axum::middleware::from_fn(log_req))
        .layer(create_cors_layer())
        .layer(create_compression_layer());

    let router: axum::Router<()> = router.with_state(shared_data.clone());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    serve(listener, router, None, &shared_data.connection_manager).await;
    Ok(())
}

fn add_rate_limiter(
    router: Router<Arc<AppState>>,
    rate_limit_cfg: &RateLimitConfig,
) -> Router<Arc<AppState>> {
    if rate_limit_cfg.enabled {
        let governor_conf = tower_governor::governor::GovernorConfigBuilder::default()
            .key_extractor(SmartIpKeyExtractor)
            .per_millisecond(rate_limit_cfg.period_millis)
            .burst_size(rate_limit_cfg.burst_size)
            .finish();
        if let Some(config) = governor_conf {
            router.layer(tower_governor::GovernorLayer::new(Arc::new(config)))
        } else {
            error!("Failed to initialize rate limiter");
            router
        }
    } else {
        router
    }
}

async fn log_req(req: Request, next: Next) -> impl axum::response::IntoResponse {
    if !log::log_enabled!(log::Level::Debug) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let uri = req.uri().clone();

    let headers = req.headers();
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.split(',').next().map(str::trim))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            req.extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.to_string())
        });

    let safe_ip = client_ip
        .as_deref().map_or_else(|| sanitize_sensitive_info("<unknown>"), sanitize_sensitive_info);
    let uri_string = uri.to_string();
    let safe_uri = sanitize_sensitive_info(&uri_string);

    debug!("Client request [{method}] -> {safe_uri} from {safe_ip}");
    next.run(req).await
}
