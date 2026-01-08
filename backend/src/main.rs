#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::missing_errors_doc)]

#[macro_use]
mod modules;

include_modules!();

use crate::auth::generate_password;
use crate::library::LibraryProcessor;
use crate::model::{
    AppConfig, Config, Healthcheck, HealthcheckConfig, ProcessTargets, SourcesConfig,
};
use crate::processing::processor::playlist;
use crate::utils::request::create_client;
use crate::utils::{config_file_reader, resolve_env_var};
use crate::utils::{db_viewer, init_logger};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use clap::Parser;
use log::{error, info, warn};
use shared::model::ConfigPaths;
use std::fs::File;
use std::sync::Arc;

#[allow(clippy::struct_excessive_bools)]
#[derive(Parser)]
#[command(name = "tuliprox")]
#[command(author = "euzu <euzu@proton.me>")]
#[command(version)]
#[command(about = "Extended playlist proxy", long_about = None)]
struct Args {
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

    /// The target to process
    #[arg(short = 't', long)]
    target: Option<Vec<String>>,

    /// The user file
    #[arg(short = 'a', long = "api-proxy")]
    api_proxy: Option<String>,

    /// Run in server mode
    #[arg(
        short = 's',
        long,
        default_value_t = false,
        default_missing_value = "true"
    )]
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
    let args = Args::parse();

    db_viewer(args.db_xtream_file_name.as_deref(), args.db_m3u_file_name.as_deref());

    if args.genpwd {
        match generate_password() {
            Ok(pwd) => println!("{pwd}"),
            Err(err) => eprintln!("{err}"),
        }
        return;
    }

    let mut config_paths = get_file_paths(&args);

    init_logger(
        args.log_level.as_deref(),
        config_paths.config_file_path.as_str(),
    );

    if args.healthcheck {
        let healthy = healthcheck(config_paths.config_file_path.as_str()).await;
        std::process::exit(i32::from(!healthy));
    }

    // Handle Library scan before starting main application
    if args.scan_library || args.force_library_rescan {
        info!("Library scan mode requested");
        let app_config = utils::read_initial_app_config(&mut config_paths, true, true, false).await.unwrap_or_else(|err| exit!("{}", err));
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
    let app_config = utils::read_initial_app_config(&mut config_paths, true, true, args.server)
        .await
        .unwrap_or_else(|err| exit!("{err}"));
    print_info(&app_config);

    let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&app_config.sources);
    let targets = sources
        .validate_targets(args.target.as_ref())
        .unwrap_or_else(|err| exit!("{err}"));

    if args.server {
        start_in_server_mode(Arc::new(app_config), Arc::new(targets)).await;
    } else {
        start_in_cli_mode(Arc::new(app_config), Arc::new(targets)).await;
    }
}

fn print_info(app_config: &AppConfig) {
    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_config.paths);
    info!(
        "Current time: {}",
        chrono::offset::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    info!("Temp dir: {}", tempfile::env::temp_dir().display());
    info!("Working dir: {:?}", &config.working_dir);
    info!("Config dir: {:?}", &paths.config_path);
    info!("Config file: {:?}", &paths.config_file_path);
    info!("Source file: {:?}", &paths.sources_file_path);
    info!("Api Proxy File: {:?}", &paths.api_proxy_file_path);
    info!("Mapping path: {:?}", &paths.mapping_file_path.as_ref().map_or_else(|| "not used", |v| v.as_str()));

    if let Some(mapping_paths) = paths.mapping_files_used.as_ref() {
        for mapping_path in mapping_paths {
            info!("Mapping file loaded: {mapping_path}");
        }
    }

    if let Some(cache) = config.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()) {
        if cache.enabled {
            info!("Cache dir: {}", cache.dir);
        }
    }
    if let Some(resource_path) = paths.custom_stream_response_path.as_ref() {
        info!("Resource path: {resource_path}");
    }
}

fn get_file_paths(args: &Args) -> ConfigPaths {
    let config_path: String = utils::resolve_directory_path(&resolve_env_var(&args.config_path.as_ref().map_or_else(utils::get_default_config_path, ToString::to_string)));
    let config_file: String = resolve_env_var(&args.config_file.as_ref().map_or_else(|| utils::get_default_config_file_path(&config_path), ToString::to_string));
    let api_proxy_file = resolve_env_var(&args.api_proxy.as_ref().map_or_else(|| utils::get_default_api_proxy_config_path(config_path.as_str()), ToString::to_string));
    let sources_file: String = resolve_env_var(&args.source_file.as_ref().map_or_else(|| utils::get_default_sources_file_path(&config_path), ToString::to_string));
    let mappings_file = args.mapping_file.as_ref().map(|p| resolve_env_var(p));

    ConfigPaths {
        config_path,
        config_file_path: config_file,
        sources_file_path: sources_file,
        mapping_file_path: mappings_file, // need to be set after config read
        mapping_files_used: None,
        api_proxy_file_path: api_proxy_file,
        custom_stream_response_path: None,
    }
}

async fn start_in_cli_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    let client = create_client(&cfg).build().unwrap_or_else(|err| {
        error!("Failed to build client {err}");
        reqwest::Client::new()
    });
    playlist::exec_processing(&client, cfg, targets, None, None, None).await;
}

async fn start_in_server_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    if let Err(err) = api::main_api::start_server(cfg, targets).await {
        exit!("Can't start server: {err}");
    }
}

async fn scan_library_cli(app_config: &AppConfig, force_rescan: bool) {
    info!("Starting Library scan from CLI (force_rescan: {force_rescan})");

    let Some(processor) = LibraryProcessor::from_app_config(app_config) else {
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
        Ok(file) => {
            match serde_saphyr::from_reader::<_, HealthcheckConfig>(config_file_reader(file, true)) {
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
            }
        }
        Err(err) => {
            error!("Failed to open config file for healthcheck {err:?}");
            false
        }
    }
}
