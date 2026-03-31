use crate::{
    api::{
        api_utils::{get_build_time, get_server_time},
        config_watch::exec_config_watch,
        endpoints::{
            custom_video_stream_api::cvs_api_register,
            hdhomerun_api::hdhr_api_register,
            hls_api::hls_api_register,
            m3u_api::m3u_api_register,
            v1_api::v1_api_register,
            web_index::{index_register_with_path, index_register_without_path},
            websocket_api::ws_api_register,
            xmltv_api::xmltv_api_register,
            xtream_api::xtream_api_register,
        },
        hdhomerun_proprietary::spawn_proprietary_tasks,
        hdhomerun_ssdp::spawn_ssdp_discover_task,
        model::{
            create_cache, create_http_client, create_http_client_no_redirect, exec_provider_dns, ActiveProviderManager,
            ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadQueue, EventManager, EventMessage,
            HdHomerunAppState, MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, UpdateGuard,
        },
        panel_api::sync_panel_api_exp_dates_on_boot,
        scheduler::{exec_interner_prune, exec_scheduler},
        serve::serve,
        sys_usage::exec_system_usage,
    },
    model::{AppConfig, Config, HdHomeRunFlags, Healthcheck, ProcessTargets, RateLimitConfig},
    processing::processor::exec_processing,
    repository::{get_geoip_path, load_playlists_into_memory_cache},
    utils::{exec_file_lock_prune, get_default_web_root_path, GeoIp},
    VERSION,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    extract::{connect_info::ConnectInfo, Request},
    middleware::Next,
    Router,
};
use dashmap::DashSet;
use log::{debug, error, info, warn};
use shared::{
    error::TuliproxError,
    info_err, info_err_res,
    utils::{concat_path_leading_slash, sanitize_sensitive_info},
};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{atomic::AtomicI8, Arc},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_http::compression::predicate::{DefaultPredicate, Predicate};
use tower_http::services::ServeDir;

const METADATA_TRIGGER_WAIT_CYCLE_LIMIT: u32 = 900;

fn collect_rescheduled_targets(
    targets_set: &HashSet<String>,
    running_trigger_targets: &Arc<DashSet<String>>,
    pending_trigger_targets: &Arc<DashSet<String>>,
) -> Vec<String> {
    let mut targets_to_respawn = Vec::new();
    for target in targets_set {
        running_trigger_targets.remove(target);
        if pending_trigger_targets.remove(target).is_some() {
            running_trigger_targets.insert(target.clone());
            targets_to_respawn.push(target.clone());
        }
    }
    targets_to_respawn
}

fn spawn_metadata_trigger_update(
    app_state: &Arc<AppState>,
    targets_to_spawn: &[String],
    running_trigger_targets: &Arc<DashSet<String>>,
    pending_trigger_targets: &Arc<DashSet<String>>,
) {
    if targets_to_spawn.is_empty() {
        return;
    }

    let client = app_state.http_client.load().as_ref().clone();
    let app_config = Arc::clone(&app_state.app_config);
    let event_manager = Arc::clone(&app_state.event_manager);
    let playlist_state = Arc::clone(&app_state.playlists);
    let disabled_headers = app_state.get_disabled_headers();
    let update_guard = app_state.update_guard.clone();
    let app_state_clone = Arc::clone(app_state);
    let running_trigger_targets_clone = Arc::clone(running_trigger_targets);
    let pending_trigger_targets_clone = Arc::clone(pending_trigger_targets);
    let mut current_targets = targets_to_spawn.to_vec();

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        loop {
            if current_targets.is_empty() {
                break;
            }

            info!("Triggering playlist update for targets due to metadata change completion: {current_targets:?}");
            let targets_set: HashSet<String> = current_targets.iter().cloned().collect();

            let (proc_targets, pre_processed_inputs) = {
                let sources = app_config.sources.load();
                match sources.validate_targets(Some(&current_targets)) {
                    Ok(process_targets) => {
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
                        (Some(Arc::new(process_targets)), Some(pre_processed_inputs))
                    }
                    Err(_) => (None, None),
                }
            };

            if let Some(proc_targets) = proc_targets {
                let mut wait_cycles: u32 = 0;
                loop {
                    if let Some(lock) = update_guard.try_playlist() {
                        exec_processing(
                            &client,
                            Arc::clone(&app_config),
                            proc_targets,
                            Some(Arc::clone(&event_manager)),
                            Some(app_state_clone.clone()),
                            Some(Arc::clone(&playlist_state)),
                            Some(update_guard.clone()),
                            disabled_headers.clone(),
                            Some(app_state_clone.active_provider.clone()),
                            Some(app_state_clone.metadata_manager.clone()),
                            pre_processed_inputs.clone(),
                            Some(lock),
                        )
                        .await;
                        break;
                    }

                    wait_cycles = wait_cycles.saturating_add(1);
                    if wait_cycles >= METADATA_TRIGGER_WAIT_CYCLE_LIMIT {
                        warn!(
                            "Aborting metadata-triggered update after waiting ~{}s for playlist lock ({} cycles)",
                            wait_cycles * 2,
                            wait_cycles
                        );
                        break;
                    }
                    if wait_cycles.is_multiple_of(30) {
                        debug!(
                            "Metadata-triggered update is still waiting for active playlist update to finish (waited ~{}s)",
                            wait_cycles * 2
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            } else {
                warn!("Failed to validate targets for triggered update: {current_targets:?}");
            }

            current_targets = collect_rescheduled_targets(
                &targets_set,
                &running_trigger_targets_clone,
                &pending_trigger_targets_clone,
            );
        }
    });
}

fn get_web_dir_path(web_ui_enabled: bool, web_root: &str) -> Result<PathBuf, TuliproxError> {
    let web_dir_path = if web_root.is_empty() { get_default_web_root_path() } else { PathBuf::from(web_root) };
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

async fn healthcheck() -> impl axum::response::IntoResponse { axum::Json(create_healthcheck()) }

async fn create_shared_data(
    app_config: &Arc<AppConfig>,
    forced_targets: &Arc<ProcessTargets>,
) -> Result<(AppState, mpsc::Receiver<Arc<ProcessTargets>>), TuliproxError> {
    let config = app_config.config.load();
    let downloads_state_file = std::path::PathBuf::from(&config.storage_dir).join("downloads_state.json");

    let use_geoip = config.is_geoip_enabled();
    let geoip = if use_geoip {
        let path = get_geoip_path(&config.storage_dir);
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
    active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    active_users.start_adaptive_expiry_worker();
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager));

    let client = create_http_client(app_config)?;
    let client_no_redirect = create_http_client_no_redirect(app_config)?;

    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
    let cancel_tokens = Arc::new(ArcSwap::from_pointee(tokens));

    let (manual_update_sender, manual_update_rx) = mpsc::channel::<Arc<ProcessTargets>>(1);

    let app_state = AppState {
            forced_targets: Arc::new(ArcSwap::new(Arc::clone(forced_targets))),
            app_config: Arc::clone(app_config),
            http_client: Arc::new(ArcSwap::from_pointee(client)),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(client_no_redirect)),
            downloads: Arc::new(DownloadQueue::new_with_state_file(Some(downloads_state_file))),
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
            manual_update_sender,
        };

    app_state.downloads.load_from_disk().await.map_err(|err| {
        TuliproxError::new(shared::error::TuliproxErrorKind::Info, format!("Failed to load persisted downloads: {err}"))
    })?;

    if let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()) {
        crate::api::endpoints::download_api::start_download_scheduler(
            Arc::clone(app_config),
            Arc::clone(&app_state.downloads),
            Arc::clone(&app_state.event_manager),
            Arc::clone(&app_state.active_provider),
            Arc::clone(&app_state.connection_manager),
        );

        if !app_state.downloads.queue.lock().await.is_empty() || app_state.downloads.active.read().await.is_some() {
            crate::api::endpoints::download_api::ensure_download_worker_running(
                app_config,
                download_cfg,
                &app_state.downloads,
                &app_state.event_manager,
                &app_state.active_provider,
                &app_state.connection_manager,
            )
                .await
                .map_err(|err| {
                    TuliproxError::new(
                        shared::error::TuliproxErrorKind::Info,
                        format!("Failed to resume persisted downloads: {err}"),
                    )
                })?;
        }
    }

    Ok((app_state, manual_update_rx))
}

async fn run_manual_update_worker(
    client: reqwest::Client,
    app_state: Arc<AppState>,
    mut rx: mpsc::Receiver<Arc<ProcessTargets>>,
) {
    while let Some(targets) = rx.recv().await {
        exec_processing(
            &client,
            Arc::clone(&app_state.app_config),
            targets,
            Some(Arc::clone(&app_state.event_manager)),
            Some(Arc::clone(&app_state)),
            Some(Arc::clone(&app_state.playlists)),
            Some(app_state.update_guard.clone()),
            app_state.get_disabled_headers(),
            Some(Arc::clone(&app_state.active_provider)),
            Some(Arc::clone(&app_state.metadata_manager)),
            None,
            None,
        )
        .await;
    }
}

fn cancel_all_service_tokens(app_state: &Arc<AppState>) {
    let cancel_tokens = app_state.cancel_tokens.load();
    cancel_tokens.scheduler.cancel();
    cancel_tokens.hdhomerun.cancel();
    cancel_tokens.file_watch.cancel();
    cancel_tokens.provider_dns.cancel();
    app_state.active_users.shutdown();
    // Use the manager's shutdown() rather than cancelling the token directly so
    // the is_shutdown flag is set and workers do not attempt to restart after cancellation.
    app_state.metadata_manager.shutdown();
}

fn exec_update_on_boot(client: &reqwest::Client, app_state: &Arc<AppState>, targets: &Arc<ProcessTargets>) -> bool {
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
        let app_state_clone = Arc::clone(app_state);

        tokio::spawn(async move {
            exec_processing(
                &client,
                app_config_clone,
                targets_clone,
                event_manager,
                Some(app_state_clone),
                Some(playlist_state),
                update_guard,
                disabled_headers,
                Some(provider_manager),
                Some(metadata_manager),
                None,
                None,
            )
            .await;
        });
        return true;
    }
    false
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
fn allow_response_compression(
    _status: axum::http::StatusCode,
    _version: axum::http::Version,
    headers: &axum::http::HeaderMap,
    extensions: &axum::http::Extensions,
) -> bool {
    if let Some(content_type) = headers.get(axum::http::header::CONTENT_TYPE) {
        if let Ok(ct) = content_type.to_str() {
        // Disable compression for wasm , WebKit browser dont like it.
            if ct.starts_with("application/wasm") {
                return false;
            }
        }
    }
    crate::api::api_utils::should_compress_response_extensions(extensions)
}

fn create_compression_layer() -> tower_http::compression::CompressionLayer<impl Predicate> {
    let predicate = DefaultPredicate::new().and(allow_response_compression);
    tower_http::compression::CompressionLayer::new()
        .br(true)
        .deflate(true)
        .gzip(true)
        .zstd(true)
        .compress_when(predicate)
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
                spawn_ssdp_discover_task(Arc::clone(app_config), host.clone(), cancel_token.clone());
            } else {
                info!("HDHomeRun SSDP discovery is disabled.");
            }

            if hdhomerun.flags.contains(HdHomeRunFlags::ProprietaryDiscovery) {
                info!("HDHomeRun proprietary discovery is enabled.");
                spawn_proprietary_tasks(Arc::clone(app_state), host.clone(), cancel_token.clone());
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
                    infos.push(format!("HdHomeRun Server '{}' running: http://{host}:{port}", device.name));
                    let c_token = cancel_token.clone();
                    let connection_manager = Arc::clone(&app_data.connection_manager);
                    tokio::spawn(async move {
                        let router = axum::Router::<Arc<HdHomerunAppState>>::new()
                            .layer(create_cors_layer())
                            .layer(create_compression_layer())
                            .merge(hdhr_api_register(basic_auth));

                        let router: axum::Router<()> = router.with_state(Arc::new(HdHomerunAppState {
                            app_state: Arc::clone(&app_data),
                            device: Arc::clone(&device_clone),
                            hd_scan_state: Arc::new(AtomicI8::new(-1)),
                        }));

                        match tokio::net::TcpListener::bind(format!("{}:{}", app_host.clone(), port)).await {
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
pub async fn start_server(app_config: Arc<AppConfig>, targets: Arc<ProcessTargets>) -> Result<(), TuliproxError> {
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
    let (app_shared_data, manual_update_rx) = create_shared_data(&app_config, &targets).await?;
    let app_state = Arc::new(app_shared_data);

    // Initialize metadata manager with weak ref to app_state
    // IMPORTANT: clone app_state here to keep it alive for the weak ref, but avoid moving it
    app_state.metadata_manager.set_app_state(Arc::downgrade(&app_state)).await;

    // Worker that processes manual playlist update requests one at a time.
    // The bounded channel ensures at most one pending request is queued.
    {
        let worker_client = app_state.http_client.load().as_ref().clone();
        let worker_state = Arc::clone(&app_state);
        tokio::spawn(run_manual_update_worker(worker_client, worker_state, manual_update_rx));
    }

    // Start event listener for input metadata updates
    // Clone app_state first to ensure it lives in the closure
    // Explicitly creating a new Arc reference for this closure.
    let app_state_for_listener = Arc::clone(&app_state);
    exec_input_update_listener(&app_state_for_listener, &targets);

    // Keep using the original `app_state` below, which is valid because `Arc::clone` borrows.
    let shared_data = Arc::clone(&app_state);

    let (cancel_token_scheduler, cancel_token_hdhomerun, cancel_token_file_watch, cancel_token_provider_dns) = {
        let cancel_tokens = app_state.cancel_tokens.load();
        (
            cancel_tokens.scheduler.clone(),
            cancel_tokens.hdhomerun.clone(),
            cancel_tokens.file_watch.clone(),
            cancel_tokens.provider_dns.clone(),
        )
    };

    if let Err(err) = load_playlists_into_memory_cache(&app_state).await {
        error!("Failed to load playlists into memory cache: {err}");
    }

    exec_system_usage(&app_state);

    let client = shared_data.http_client.load();

    if !exec_update_on_boot(client.as_ref(), &app_state, &targets) {
        sync_panel_api_exp_dates_on_boot(&app_state).await;
    }

    exec_scheduler(client.as_ref(), &app_state, &cancel_token_scheduler);
    exec_file_lock_prune(&app_state);
    exec_interner_prune(&app_state);
    exec_config_watch(&app_state, &cancel_token_file_watch);
    exec_provider_dns(&app_state, &cancel_token_provider_dns);

    let web_auth_enabled = is_web_auth_enabled(&cfg, web_ui_enabled);

    if app_config.api_proxy.load().is_some() {
        start_hdhomerun(&app_config, &app_state, &mut infos, &cancel_token_hdhomerun);
    }

    let web_ui_path = cfg.web_ui.as_ref().and_then(|c| c.path.as_ref()).cloned().unwrap_or_default();
    infos.push(format!("Server running: http://{}:{}", &cfg.api.host, &cfg.api.port));
    for info in &infos {
        info!("{info}");
    }

    // Web Server
    let mut router = axum::Router::new()
        .route("/healthcheck", axum::routing::get(healthcheck))
        .nest_service("/.well-known", ServeDir::new(web_dir_path.join("static/.well-known")))
        .merge(ws_api_register(web_auth_enabled, web_ui_path.as_str()));
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
            .merge(v1_api_register(web_auth_enabled, &shared_data, web_ui_path.as_str()));
        if !web_ui_path.is_empty() {
            router = router.merge(index_register_with_path(&web_dir_path, web_ui_path.as_str()));
        }
    }

    let mut api_router = axum::Router::new()
        .merge(xtream_api_register())
        .merge(m3u_api_register())
        .merge(xmltv_api_register())
        .merge(hls_api_register())
        .merge(cvs_api_register());
    if let Some(rate_limiter) = cfg.reverse_proxy.as_ref().and_then(|r| r.rate_limit.clone()) {
        api_router = add_rate_limiter(api_router, &rate_limiter);
    }

    router = router.merge(api_router);

    if web_ui_enabled && web_ui_path.is_empty() {
        router = router.merge(index_register_without_path(&web_dir_path));
    }

    router =
        router.layer(axum::middleware::from_fn(log_req)).layer(create_cors_layer()).layer(create_compression_layer());

    let router: axum::Router<()> = router.with_state(shared_data.clone());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .map_err(|err| info_err!("Failed to bind to {host}:{port}, {err}"))?;

    let server_cancel_token = CancellationToken::new();
    let server_cancel_token_signal = server_cancel_token.clone();
    let app_state_signal = Arc::clone(&app_state);
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received shutdown signal (Ctrl+C), cancelling all background services");
                cancel_all_service_tokens(&app_state_signal);
                server_cancel_token_signal.cancel();
            }
            Err(err) => {
                error!("Failed to listen for Ctrl+C: {err}");
            }
        }
    });

    serve(listener, router, Some(server_cancel_token), &shared_data.connection_manager).await;

    // Final shutdown safeguard for all background services (idempotent).
    cancel_all_service_tokens(&shared_data);
    Ok(())
}

fn add_rate_limiter(router: Router<Arc<AppState>>, rate_limit_cfg: &RateLimitConfig) -> Router<Arc<AppState>> {
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
        .or_else(|| req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.to_string()));

    let safe_ip = client_ip.as_deref().map_or_else(|| sanitize_sensitive_info("<unknown>"), sanitize_sensitive_info);
    let uri_string = uri.to_string();
    let safe_uri = sanitize_sensitive_info(&uri_string);

    debug!("Client request [{method}] -> {safe_uri} from {safe_ip}");
    next.run(req).await
}

#[allow(clippy::too_many_lines)]
fn exec_input_update_listener(app_state: &Arc<AppState>, targets: &Arc<ProcessTargets>) {
    let app_state = Arc::clone(app_state);
    let targets = Arc::clone(targets);

    tokio::spawn(async move {
        let mut rx = app_state.event_manager.get_event_channel();
        // Map<TargetName, Set<InputName>>: Tracks which inputs are currently updating for a given target
        let mut active_target_inputs: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        // Set<TargetName>: Tracks which targets are pending an update once their inputs are done
        let mut pending_targets: HashSet<String> = HashSet::new();
        // Set<TargetName>: Tracks which targets have an active spawned playlist-update task (dedup guard)
        let running_trigger_targets: Arc<DashSet<String>> = Arc::new(DashSet::new());
        // Set<TargetName>: Tracks follow-up triggers that arrive while a target is already running.
        let pending_trigger_targets: Arc<DashSet<String>> = Arc::new(DashSet::new());

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
                                    .insert(input_name.clone());
                                // Mark target as potentially needing an update
                                pending_targets.insert(target.name.clone());
                            }
                        }
                    }
                }
                Ok(EventMessage::InputMetadataUpdatesCompleted(input_name)) => {
                    let mut targets_to_trigger = Vec::new();
                    // Remove this input from all active sets in-place and collect targets
                    // that became ready without cloning all keys first.
                    active_target_inputs.retain(|target_name, inputs| {
                        inputs.remove(&input_name);
                        if inputs.is_empty() {
                            if pending_targets.remove(target_name) {
                                targets_to_trigger.push(target_name.clone());
                            }
                            false
                        } else {
                            true
                        }
                    });

                    // Deduplicate: only spawn for targets that don't already have a running trigger task
                    let targets_to_spawn: Vec<String> = {
                        let mut to_spawn = Vec::new();
                        for target in targets_to_trigger {
                            if running_trigger_targets.insert(target.clone()) {
                                to_spawn.push(target);
                            } else {
                                pending_trigger_targets.insert(target);
                            }
                        }
                        to_spawn
                    };

                    if !targets_to_spawn.is_empty() {
                        spawn_metadata_trigger_update(
                            &app_state,
                            &targets_to_spawn,
                            &running_trigger_targets,
                            &pending_trigger_targets,
                        );
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("Input update listener lagged by {skipped} messages. Resetting input-tracking state (active_targets={}, pending={}) while preserving trigger dedup state.",
                        active_target_inputs.len(), pending_targets.len());
                    active_target_inputs.clear();
                    pending_targets.clear();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
