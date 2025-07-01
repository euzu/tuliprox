#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::missing_errors_doc)]

#[macro_use]
mod modules;

include_modules!();

use crate::auth::generate_password;
use crate::model::{AppConfig, Config, Healthcheck, HealthcheckConfig, ProcessTargets, SourcesConfig};
use crate::processing::processor::playlist;
use crate::utils::{config_file_reader, resolve_env_var};
use crate::utils::request::{create_client};
use chrono::{DateTime, Utc};
use clap::{Parser};
use log::{error, info};
use std::fs::File;
use std::sync::Arc;
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use shared::model::ConfigPaths;
use crate::utils::init_logger;

#[derive(Parser)]
#[command(name = "tuliprox")]
#[command(author = "euzu <euzu@proton.me>")]
#[command(version)]
#[command(about = "Extended M3U playlist filter", long_about = None)]
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

fn main() {
    let args = Args::parse();

    if args.genpwd {
        match generate_password() {
            Ok(pwd) => println!("{pwd}"),
            Err(err) => eprintln!("{err}"),
        }
        return;
    }

    let mut config_paths = get_file_paths(&args);

    if args.healthcheck {
        healthcheck(config_paths.config_file_path.as_str());
        return;
    }

    init_logger(args.log_level.as_ref(), config_paths.config_file_path.as_str());

    info!("Version: {VERSION}");
    if let Some(bts) = BUILD_TIMESTAMP.to_string().parse::<DateTime<Utc>>().ok().map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string()) {
        info!("Build time: {bts}");
    }

    let app_config = utils::read_initial_app_config(&mut config_paths, true, true, args.server).unwrap_or_else(|err| exit!("{}", err));
    print_info(&app_config);

    let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&app_config.sources);
    let targets = sources.validate_targets(args.target.as_ref()).unwrap_or_else(|err| exit!("{}", err));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let () = rt.block_on(async {
        if args.server {
            start_in_server_mode(Arc::new(app_config), Arc::new(targets)).await;
        } else {
            start_in_cli_mode(Arc::new(app_config), Arc::new(targets)).await;
        }
    });
}

fn print_info(app_config: &AppConfig) {
    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
    let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_config.paths);
    info!("Current time: {}", chrono::offset::Local::now().format("%Y-%m-%d %H:%M:%S"));
    info!("Temp dir: {}", tempfile::env::temp_dir().display());
    info!("Working dir: {:?}", &config.working_dir);
    info!("Config dir: {:?}", &paths.config_path);
    info!("Config file: {:?}", &paths.config_file_path);
    info!("Source file: {:?}", &paths.sources_file_path);
    info!("Api Proxy File: {:?}", &paths.api_proxy_file_path);
    info!("Mapping file: {:?}", &paths.mapping_file_path.as_ref().map_or_else(|| "not used",  |v| v.as_str()));

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
    let sources_file: String = resolve_env_var(&args.source_file.as_ref().map_or_else(|| utils::get_default_sources_file_path(&config_path),  ToString::to_string));
    let mappings_file = args.mapping_file.as_ref().map(|p| resolve_env_var(p));

    ConfigPaths {
        config_path,
        config_file_path: config_file,
        sources_file_path: sources_file,
        mapping_file_path: mappings_file, // need to be set after config read
        api_proxy_file_path: api_proxy_file,
        custom_stream_response_path: None,
    }
}

async fn start_in_cli_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    let client = create_client(&cfg).build().unwrap_or_else(|err| {
        error!("Failed to build client {err}");
        reqwest::Client::new()
    });
    playlist::exec_processing(Arc::new(client), cfg, targets).await;
}

async fn start_in_server_mode(cfg: Arc<AppConfig>, targets: Arc<ProcessTargets>) {
    if let Err(err) = api::main_api::start_server(cfg, targets).await {
        exit!("Can't start server: {err}");
    }
}

fn healthcheck(config_file: &str) {
    let path = std::path::PathBuf::from(config_file);
    let file = File::open(path).expect("Failed to open config file");
    let config: HealthcheckConfig = serde_yaml::from_reader(config_file_reader(file, true)).expect("Failed to parse config file");

    if let Ok(response) = reqwest::blocking::get(format!("http://localhost:{}/healthcheck", config.api.port)) {
        if let Ok(check) = response.json::<Healthcheck>() {
            if check.status == "ok" {
                std::process::exit(0);
            }
        }
    }
    std::process::exit(1);
}
