// Clippy policy lives in [workspace.lints.clippy] in the root Cargo.toml and is
// opted into by backend/Cargo.toml's [lints] workspace = true.

// #[cfg(target_os = "linux")]
// #[global_allocator]
// static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
// #[cfg(not(target_env = "msvc"))]
// #[global_allocator]
// static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// `debug_if_enabled!`, `trace_if_enabled!`, `exit!` and the config `from_impl!`
// family are exported by tuliprox-core; make them available unqualified as they
// were when they lived in this crate.
#[macro_use]
extern crate tuliprox_core;

#[macro_use]
mod modules;

include_modules!();

// The media-server anti-corruption layer is its own package; aliased under its
// historical module name so `crate::media_server::X` paths keep resolving.
use crate::{
    api::model::create_http_client,
    auth::generate_password,
    library::{LibraryProcessor, MediaToolProbes},
    model::{AppConfig, Config, Healthcheck, HealthcheckConfig, ProcessTargets, SourcesConfig},
    processing::processor::exec_processing,
    repository::{db_viewer, run_startup_migrations},
    utils::{config_file_reader, init_logger, request::create_client, resolve_env_var},
};
use arc_swap::{access::Access, ArcSwap};
use chrono::{DateTime, Utc};
use clap::Parser;
use log::{error, info, warn};
use shared::{
    defaults::{CONFIG_FILE, CONFIG_PATH, DEFAULT_STORAGE_DIR, SOURCE_FILE},
    model::ConfigPaths,
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_auth as auth;
use tuliprox_config_loader as config_loader;
// The configuration model and shared utilities are their own package. Aliased
// under their historical module names so `crate::model::X` and
// `crate::utils::X` keep resolving across the backend.
use tuliprox_core::{model, utils};
use tuliprox_iptv as iptv;
use tuliprox_library::{library, media_enrichment};
use tuliprox_media_server as media_server;
use tuliprox_messaging as messaging;
use tuliprox_mpegts as mpegts;
use tuliprox_parser as parser;
use tuliprox_repository as repository;

#[allow(clippy::struct_excessive_bools)]
#[derive(Parser)]
#[command(name = "tuliprox")]
#[command(author = "euzu <euzu@proton.me>")]
#[command(version)]
#[command(about = "Extended playlist proxy", long_about = None)]
struct Args {
    /// The home directory (base for config, storage, backup, downloads)
    #[arg(short = 'H', long = "home")]
    home: Option<String>,

    /// The config directory
    #[arg(short = 'p', long = "config-path")]
    config_path: Option<String>,

    /// The config file
    #[arg(short = 'c', long = "config")]
    config_file: Option<String>,

    /// The source config file
    #[arg(short = 'i', long = "source")]
    source_file: Option<String>,

    /// The mapping file
    #[arg(short = 'm', long = "mapping")]
    mapping_file: Option<String>,

    /// The template file
    #[arg(short = 'T', long = "template")]
    template_file: Option<String>,

    /// The target to process
    #[arg(short = 't', long)]
    target: Option<Vec<String>>,

    /// The user file
    #[arg(short = 'a', long = "api-proxy")]
    api_proxy: Option<String>,

    /// Run in server mode
    #[arg(short = 's', long, default_value_t = false, default_missing_value = "true")]
    server: bool,

    /// log level
    #[arg(short = 'l', long = "log-level", default_missing_value = "info")]
    log_level: Option<String>,

    #[arg(short = None, long = "genpwd", default_value_t = false, default_missing_value = "true")]
    genpwd: bool,

    #[arg(short = None, long = "healthcheck", default_value_t = false, default_missing_value = "true"
    )]
    healthcheck: bool,

    /// Scan local Library directory
    #[arg(long = "scan-library", default_value_t = false, default_missing_value = "true")]
    scan_library: bool,

    /// Force rescan of all local Library files
    #[arg(long = "force-library-rescan", default_value_t = false, default_missing_value = "true")]
    force_library_rescan: bool,

    #[arg(long = "dbx")]
    db_xtream_file_name: Option<String>,

    #[arg(long = "dbm")]
    db_m3u_file_name: Option<String>,

    #[arg(long = "dbe")]
    db_epg_file_name: Option<String>,

    #[arg(long = "dbv")] // Target Id Mapping
    db_tim_file_name: Option<String>,

    #[arg(long = "dbms")] // Metadata Retry Status
    db_mrs_file_name: Option<String>,

    #[arg(long = "dbq")] // QoS Snapshot DB
    db_qos_snapshot_file_name: Option<String>,
    /// Query stream history (inline JSON or @file.json)
    #[arg(long = "sh")]
    stream_history: Option<String>,
}

impl Args {
    fn db_viewer_args(&self) -> repository::DbViewerArgs<'_> {
        repository::DbViewerArgs::new(
            self.db_xtream_file_name.as_deref(),
            self.db_m3u_file_name.as_deref(),
            self.db_epg_file_name.as_deref(),
            self.db_tim_file_name.as_deref(),
            self.db_mrs_file_name.as_deref(),
            self.db_qos_snapshot_file_name.as_deref(),
        )
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TIMESTAMP: &str = env!("VERGEN_BUILD_TIMESTAMP");

// #[cfg(not(target_env = "msvc"))]
// #[global_allocator]
// static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
//
// #[allow(non_upper_case_globals)]
// #[export_name = "malloc_conf"]
// pub static malloc_conf: &[u8] = b"lg_prof_interval:25,prof:true,prof_leak:true,prof_active:true,prof_prefix:/tmp/jeprof\0";

#[tokio::main]
async fn main() {
    api::api_utils::init_uptime_clock();
    let args = Args::parse();

    db_viewer(&args.db_viewer_args());

    if let Some(ref sh_input) = args.stream_history {
        std::process::exit(repository::stream_history_viewer(sh_input).await);
    }

    if args.genpwd {
        match generate_password() {
            Ok(pwd) => println!("{pwd}"),
            Err(err) => eprintln!("{err}"),
        }
        return;
    }

    let mut config_paths = get_file_paths(&args);

    init_logger(args.log_level.as_deref(), config_paths.config_file_path.as_str());

    if args.healthcheck {
        let healthy = healthcheck(config_paths.config_file_path.as_str()).await;
        std::process::exit(i32::from(!healthy));
    }

    run_startup_migrations(&config_paths);

    // Handle Library scan before starting main application
    if args.scan_library || args.force_library_rescan {
        info!("Library scan mode requested");
        let app_config = Arc::new(
            crate::config_loader::read_initial_app_config(&mut config_paths, true, true, false)
                .await
                .unwrap_or_else(|err| exit!("{}", err)),
        );
        scan_library_cli(&app_config, args.force_library_rescan).await;
        return;
    }

    info!("Version: {VERSION}");
    if let Some(bts) = BUILD_TIMESTAMP
        .to_string()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
    {
        info!("Build time: {bts}");
    }

    if args.server {
        let missing_files = missing_required_setup_files(&config_paths);
        if !missing_files.is_empty() {
            warn!(
                "Missing required config files for normal startup ({}). Entering setup mode.",
                missing_files.join(", ")
            );
            if let Err(err) = api::setup_api::start_setup_server(&config_paths, &missing_files).await {
                exit!("Can't start setup server: {err}");
            }
            return;
        }
    }

    let app_config = crate::config_loader::read_initial_app_config(&mut config_paths, true, true, args.server)
        .await
        .unwrap_or_else(|err| exit!("{err}"));
    print_info(&app_config).await;

    let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&app_config.sources);
    let targets = sources.validate_targets(args.target.as_ref()).unwrap_or_else(|err| exit!("{err}"));

    if args.server {
        start_in_server_mode(Arc::new(app_config), Arc::new(targets)).await;
    } else {
        start_in_cli_mode(Arc::new(app_config), Arc::new(targets)).await;
    }
}

async fn print_info(app_config: &AppConfig) {
    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_config.paths);
    info!("Current time: {}", chrono::offset::Local::now().format("%Y-%m-%d %H:%M:%S"));
    info!("Temp dir: {}", tempfile::env::temp_dir().display());
    info!("Storage dir: {}", config.storage_dir);
    info!("Config dir: {}", paths.config_path);
    info!("Config file: {}", paths.config_file_path);
    info!("Source file: {}", paths.sources_file_path);
    info!("Api Proxy File: {}", paths.api_proxy_file_path);
    info!("Mapping path: {}", paths.mapping_file_path.as_ref().map_or_else(|| "not used", |v| v.as_str()));
    info!("Template path: {}", paths.template_file_path.as_ref().map_or_else(|| "not used", |v| v.as_str()));
    info!("Resources path: {}", paths.custom_stream_response_path.as_ref().map_or_else(|| "not used", |v| v.as_str()));

    if let Some(mapping_paths) = paths.mapping_files_used.as_ref() {
        for mapping_path in mapping_paths {
            info!("Mapping file loaded: {mapping_path}");
        }
    }
    if let Some(template_paths) = paths.template_files_used.as_ref() {
        for template_path in template_paths {
            info!("Template file loaded: {template_path}");
        }
    }

    if let Some(cache) = config.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()) {
        if cache.enabled {
            info!("Cache dir: {}", cache.directory);
        }
    }
    if let Some(metadata_update) = config.metadata_update.as_ref() {
        info!("Metadata path: {}", metadata_update.cache_path);
    }
    config_loader::runtime_config_report::log_runtime_config_report(app_config).await;
}

fn get_file_paths(args: &Args) -> ConfigPaths {
    let resolve_storage_path = |home_path: &str, storage_dir: Option<&str>| {
        let path = storage_dir.map(str::trim).filter(|value| !value.is_empty()).map(PathBuf::from).map_or_else(
            || utils::get_default_path_for_home(Path::new(home_path), DEFAULT_STORAGE_DIR),
            |configured_path| {
                if configured_path.is_relative() {
                    PathBuf::from(home_path).join(configured_path)
                } else {
                    configured_path
                }
            },
        );
        utils::resolve_directory_path(path.to_string_lossy().as_ref())
    };

    let home_path = args
        .home
        .as_ref()
        .filter(|p| !p.trim().is_empty())
        .map_or_else(utils::get_home_path, |p| PathBuf::from(resolve_env_var(p)));
    let home_path = utils::resolve_directory_path(home_path.to_string_lossy().as_ref());

    let raw_path = args.config_path.as_ref().filter(|p| !p.trim().is_empty()).map_or_else(
        || utils::get_default_path_for_home(Path::new(&home_path), CONFIG_PATH).to_string_lossy().to_string(),
        |p| resolve_env_var(p),
    );
    let config_path: String = utils::resolve_directory_path(&raw_path);

    let config_file: String = resolve_env_var(
        &args
            .config_file
            .as_ref()
            .map_or_else(|| utils::get_default_config_file_path(&config_path), ToString::to_string),
    );
    let api_proxy_file = resolve_env_var(
        &args
            .api_proxy
            .as_ref()
            .map_or_else(|| utils::get_default_api_proxy_config_path(config_path.as_str()), ToString::to_string),
    );
    let sources_file: String = resolve_env_var(
        &args
            .source_file
            .as_ref()
            .map_or_else(|| utils::get_default_sources_file_path(&config_path), ToString::to_string),
    );
    let mappings_file = args.mapping_file.as_ref().map(|p| {
        let path = resolve_env_var(p);
        utils::resolve_mapping_file_path(config_path.as_str(), Some(path.as_str()))
    });
    let template_file = args.template_file.as_ref().map(|p| {
        let path = resolve_env_var(p);
        utils::resolve_template_file_path(config_path.as_str(), Some(path.as_str()))
    });
    let storage_path = if Path::new(&config_file).exists() {
        match crate::config_loader::read_config_file_with_options(
            &config_file,
            crate::config_loader::ReadConfigOptions { resolve_env: true, include_computed: false },
        ) {
            Ok(cfg) => resolve_storage_path(&home_path, cfg.storage_dir.as_deref()),
            Err(err) => {
                eprintln!("Can't read config file {config_file} while resolving storage path: {err}");
                std::process::exit(1);
            }
        }
    } else {
        resolve_storage_path(&home_path, None)
    };

    ConfigPaths {
        home_path,
        config_path,
        storage_path,
        config_file_path: config_file,
        sources_file_path: sources_file,
        mapping_file_path: mappings_file, // need to be set after config read
        mapping_files_used: None,
        template_file_path: template_file, // need to be set after config read
        template_files_used: None,
        api_proxy_file_path: api_proxy_file,
        custom_stream_response_path: None,
    }
}

fn missing_required_setup_files(paths: &ConfigPaths) -> Vec<String> {
    [(CONFIG_FILE, paths.config_file_path.as_str()), (SOURCE_FILE, paths.sources_file_path.as_str())]
        .iter()
        .filter_map(|(name, file_path)| {
            let file_path = std::path::Path::new(file_path);
            if !file_path.exists() {
                Some(format!("{name} missing ({})", file_path.display()))
            } else if !file_path.is_file() {
                Some(format!("{name} exists but is not a regular file ({})", file_path.display()))
            } else {
                None
            }
        })
        .collect()
}

async fn start_in_cli_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    let client = create_client(&cfg).build().unwrap_or_else(|err| {
        error!("Failed to build client {err}");
        reqwest::Client::new()
    });
    // In CLI mode, we don't start background managers for events or providers
    exec_processing(&client, cfg, targets, None, None, None, None, None, None, None, None, None).await;
}

async fn start_in_server_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    if let Err(err) = api::main_api::start_server(cfg, targets).await {
        exit!("Can't start server: {err}");
    }
}

/// Builds a library processor from the application configuration.
///
/// This was `LibraryProcessor::from_app_config`. It moved here because it was
/// the only thing in `library` that knew what an `AppConfig` is, and because
/// building the HTTP client is the composition root's job, not the library's.
fn library_processor_from_app_config(app_config: &Arc<AppConfig>) -> Option<LibraryProcessor> {
    let Ok(client) = create_http_client(app_config) else {
        error!("Failed to create HTTP client for LibraryProcessor, skipping library scan. Please check your configuration.");
        return None;
    };
    // `Access` is in scope here, so `.load()` alone is ambiguous.
    let config = ArcSwap::load(&app_config.config);
    let library_config = config.library.as_ref()?;
    let probes = {
        let for_ffprobe = Arc::clone(app_config);
        let for_ffmpeg = Arc::clone(app_config);
        MediaToolProbes::new(
            Arc::new(move || {
                let app_config = Arc::clone(&for_ffprobe);
                Box::pin(async move { app_config.is_ffprobe_enabled().await })
            }),
            Arc::new(move || {
                let app_config = Arc::clone(&for_ffmpeg);
                Box::pin(async move { app_config.is_ffmpeg_available().await })
            }),
        )
    };
    Some(
        LibraryProcessor::new(library_config.clone(), config.metadata_update.as_ref(), client, &config.storage_dir)
            .with_tool_probes(probes),
    )
}

async fn scan_library_cli(app_config: &Arc<AppConfig>, force_rescan: bool) {
    info!("Starting Library scan from CLI (force_rescan: {force_rescan})");

    let Some(processor) = library_processor_from_app_config(app_config) else {
        error!("Library is not enabled in configuration");
        std::process::exit(1);
    };

    match processor.scan(force_rescan).await {
        Ok(result) => {
            info!("Library scan completed successfully!");
            info!("  Files scanned: {}", result.files_scanned);
            info!("  Files added: {}", result.files_added);
            info!("  Files updated: {}", result.files_updated);
            info!("  Files removed: {}", result.files_removed);
            if result.errors > 0 {
                warn!("  Errors: {}", result.errors);
            }
            std::process::exit(0);
        }
        Err(err) => {
            error!("Library scan failed: {err}");
            std::process::exit(1);
        }
    }
}

async fn healthcheck(config_file: &str) -> bool {
    let path = std::path::PathBuf::from(config_file);
    match File::open(path) {
        Ok(file) => match serde_saphyr::from_reader::<_, HealthcheckConfig>(config_file_reader(file, true)) {
            Ok(config) => {
                match reqwest::Client::new()
                    .get(format!("http://localhost:{}/healthcheck", config.api.port))
                    .send()
                    .await
                {
                    Ok(response) => matches!(response.json::<Healthcheck>().await, Ok(check) if check.status == "ok"),
                    Err(_) => false,
                }
            }
            Err(err) => {
                error!("Failed to parse config file for healthcheck {err:?}");
                false
            }
        },
        Err(err) => {
            error!("Failed to open config file for healthcheck {err:?}");
            false
        }
    }
}
