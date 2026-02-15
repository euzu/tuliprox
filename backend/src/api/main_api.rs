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
use crate::api::model::{create_cache, create_http_client, create_http_client_no_redirect, ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadQueue, EventManager, EventMessage, HdHomerunAppState, MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, UpdateGuard};
use crate::api::panel_api::sync_panel_api_exp_dates_on_boot;
use crate::api::scheduler::{exec_interner_prune, exec_scheduler};
use crate::api::serve::serve;
use crate::api::sys_usage::exec_system_usage;
use crate::model::{AppConfig, Config, HdHomeRunFlags, Healthcheck, ProcessTargets, RateLimitConfig};
use crate::processing::processor::exec_processing;
use crate::repository::get_geoip_path;
use crate::repository::load_playlists_into_memory_cache;
use crate::utils::{exec_file_lock_prune, GeoIp};
use crate::VERSION;
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::extract::connect_info::ConnectInfo;
use axum::Router;
use axum::{extract::Request, middleware::Next};
use log::{debug, error, info, warn};
use shared::error::TuliproxError;
use shared::utils::{concat_path_leading_slash, sanitize_sensitive_info};
use shared::{info_err, info_err_res};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicI8;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_http::services::ServeDir;

fn get_web_dir_path(web_ui_enabled: bool, web_root: &str) -> Result<PathBuf, TuliproxError> {
    let web_dir = web_root.to_string();
    let web_dir_path = PathBuf::from(&web_dir);
    if web_ui_enabled && (!&web_dir_path.exists() || !&web_dir_path.is_dir()) {
        return info_err_res!("web_root does not exist or is not a directory: {}", web_dir_path.display());
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
) -> Result<AppState, TuliproxError> {
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
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager = Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager));

    let client = create_http_client(app_config)?;
    let client_no_redirect = create_http_client_no_redirect(app_config)?;

    let cancel_tokens = Arc::new(ArcSwap::from_pointee(CancelTokens::default()));
    let metadata_manager = Arc::new(MetadataUpdateManager::new(cancel_tokens.load().scheduler.clone()));

    Ok(AppState {
        forced_targets: Arc::new(ArcSwap::new(Arc::clone(forced_targets))),
        app_config: Arc::clone(app_config),
        http_client: Arc::new(ArcSwap::from_pointee(client)),
        http_client_no_redirect: Arc::new(ArcSwap::from_pointee(client_no_redirect)),
        downloads: Arc::new(DownloadQueue::new()),
        cache: Arc::new(ArcSwapOption::from(cache)),
        shared_stream_manager,
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens,
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        update_guard: UpdateGuard::new(),
        metadata_manager,
    })
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
        let disabled_headers = app_state.get_disabled_headers();
        let provider_manager = Arc::clone(&app_state.active_provider);
        let metadata_manager = Arc::clone(&app_state.metadata_manager);
        let event_manager = Some(Arc::clone(&app_state.event_manager));

        tokio::spawn(async move {
            exec_processing(&client, app_config_clone, targets_clone, event_manager, Some(playlist_state), update_guard, disabled_headers, Some(provider_manager), Some(metadata_manager), None, None).await;
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
        if hdhomerun.flags.contains(HdHomeRunFlags::Enabled) {
            if hdhomerun.flags.contains(HdHomeRunFlags::SsdpDiscovery) {
                info!("HDHomeRun SSDP discovery is enabled.");
                spawn_ssdp_discover_task(
                    Arc::clone(app_config),
                    host.clone(),
                    cancel_token.clone(),
                );
            } else {
                info!("HDHomeRun SSDP discovery is disabled.");
            }

            if hdhomerun
                .flags
                .contains(HdHomeRunFlags::ProprietaryDiscovery)
            {
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
                    let basic_auth = hdhomerun.flags.contains(HdHomeRunFlags::Auth);
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
) -> Result<(), TuliproxError> {
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
    let app_shared_data = create_shared_data(&app_config, &targets).await?;
    let app_state = Arc::new(app_shared_data);

    // Initialize metadata manager with weak ref to app_state
    // IMPORTANT: clone app_state here to keep it alive for the weak ref, but avoid moving it
    app_state.metadata_manager.set_app_state(Arc::downgrade(&app_state)).await;

    // Start event listener for input metadata updates
    // Clone app_state first to ensure it lives in the closure
    // Explicitly creating a new Arc reference for this closure.
    let app_state_for_listener = Arc::clone(&app_state);
    exec_input_update_listener(&app_state_for_listener, &targets);

    // Keep using the original `app_state` below, which is valid because `Arc::clone` borrows.
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

    sync_panel_api_exp_dates_on_boot(&app_state).await;

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
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await.map_err(|err| info_err!("Failed to bind to {host}:{port}, {err}"))?;
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


fn exec_input_update_listener(app_state: &Arc<AppState>, targets: &Arc<ProcessTargets>) {
    let app_state = Arc::clone(app_state);
    let targets = Arc::clone(targets);

    tokio::spawn(async move {
        let mut rx = app_state.event_manager.get_event_channel();
        // Map<TargetName, Set<InputName>>: Tracks which inputs are currently updating for a given target
        let mut active_target_inputs: HashMap<String, HashSet<String>> = HashMap::new();
        // Set<TargetName>: Tracks which targets are pending an update once their inputs are done
        let mut pending_targets: HashSet<String> = HashSet::new();

        loop {
            match rx.recv().await {
                Ok(EventMessage::InputMetadataUpdatesStarted(input_name)) => {
                    let sources = app_state.app_config.sources.load();
                    for source in &sources.sources {
                        if source.inputs.iter().any(|i| i.as_ref() == input_name.as_ref()) {
                            for target in &source.targets {
                                // Check if this target is allowed by the global process targets
                                if targets.enabled && !targets.target_names.contains(&target.name) {
                                    continue;
                                }
                                // Add this input to the active set for this target
                                active_target_inputs
                                    .entry(target.name.clone())
                                    .or_default()
                                    .insert(input_name.to_string());
                                // Mark target as potentially needing an update
                                pending_targets.insert(target.name.clone());
                            }
                        }
                    }
                }
                Ok(EventMessage::InputMetadataUpdatesCompleted(input_name)) => {
                    // 1. Remove this input from all active sets
                    let target_names: Vec<String> = active_target_inputs.keys().cloned().collect();
                    let mut targets_to_trigger = Vec::new();

                    for target_name in target_names {
                        if let std::collections::hash_map::Entry::Occupied(mut entry) = active_target_inputs.entry(target_name.clone()) {
                            let inputs = entry.get_mut();
                            inputs.remove(input_name.as_ref());
                            if inputs.is_empty() {
                                entry.remove();
                                // If this target was pending, it's now ready to trigger
                                if pending_targets.remove(&target_name) {
                                    targets_to_trigger.push(target_name);
                                }
                            }
                        }
                    }

                    if !targets_to_trigger.is_empty() {
                        info!("Triggering playlist update for targets due to metadata change completion: {targets_to_trigger:?}");

                        let client = app_state.http_client.load().as_ref().clone();
                        let app_config = Arc::clone(&app_state.app_config);
                        let event_manager = Arc::clone(&app_state.event_manager);
                        let playlist_state = Arc::clone(&app_state.playlists);
                        let disabled_headers = app_state.get_disabled_headers();
                        let sources = app_config.sources.load();

                        // For each target, we need to gather ALL its inputs to pass as pre_processed_inputs
                        let targets_set: HashSet<String> = targets_to_trigger.iter().map(Clone::clone).collect();

                        if let Ok(process_targets) = sources.validate_targets(Some(&targets_to_trigger)) {
                            let proc_targets = Arc::new(process_targets);
                            // Collect all inputs for these targets
                            let mut pre_processed_inputs: HashSet<Arc<str>> = HashSet::new();
                            for source in &sources.sources {
                                for target in &source.targets {
                                    if targets_set.contains(&target.name) {
                                        for input in &source.inputs {
                                            pre_processed_inputs.insert(input.clone());
                                        }
                                    }
                                }
                            }

                            let update_guard = app_state.update_guard.clone();
                            let app_state_clone = app_state.clone();

                            // SPAWN the trigger logic so we don't block the event loop with sleep or heavy processing setup
                            tokio::spawn(async move {
                                // Small delay to ensure any lingering updates or file locks from the background thread are fully released
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                                // Wait for any current update to finish before starting a new one
                                let lock_opt = update_guard.acquire_playlist_lock().await;

                                // Only proceed if we successfully acquired the lock (semaphore not closed)
                                if let Some(lock) = lock_opt {
                                    exec_processing(
                                        &client, app_config, proc_targets, Some(event_manager),
                                        Some(playlist_state), Some(update_guard),
                                        disabled_headers,
                                        Some(app_state_clone.active_provider.clone()), Some(app_state_clone.metadata_manager.clone()),
                                        Some(pre_processed_inputs),
                                        Some(lock), // Pass the acquired permit to stay active during processing
                                    ).await;
                                } else {
                                    warn!("Skipping triggered update because shutdown signal received (lock closed)");
                                }
                            });
                        } else {
                            warn!("Failed to validate targets for triggered update: {targets_to_trigger:?}");
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("Input update listener lagged by {skipped} messages. Resetting tracking state (active_targets={}, pending={}) to avoid inconsistencies.",
                        active_target_inputs.len(), pending_targets.len());
                    active_target_inputs.clear();
                    pending_targets.clear();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
