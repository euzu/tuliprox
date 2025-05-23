#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::missing_errors_doc)]

#[macro_use]
mod modules;

include_modules!();

use crate::auth::password::generate_password;
use crate::model::{validate_targets, Config, HealthcheckConfig, Healthcheck, ProcessTargets};
use crate::processing::processor::playlist;
use crate::utils::{config_file_reader, resolve_env_var};
use crate::utils::request::{create_client, set_sanitize_sensitive_info};
use chrono::{DateTime, Utc};
use clap::Parser;
use log::{error, info};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::utils::logging::init_logger;

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

    let config_path: String = utils::resolve_directory_path(&resolve_env_var(&args.config_path.unwrap_or_else(utils::get_default_config_path)));
    let config_file: String = resolve_env_var(&args.config_file.unwrap_or_else(|| utils::get_default_config_file_path(&config_path)));
    let api_proxy_file = resolve_env_var(&args.api_proxy.unwrap_or_else(|| utils::get_default_api_proxy_config_path(config_path.as_str())));
    let mappings_file = args.mapping_file.as_ref();

    init_logger(args.log_level.as_ref(), config_file.as_str());

    info!("Version: {VERSION}");
    if let Some(bts) = BUILD_TIMESTAMP.to_string().parse::<DateTime<Utc>>().ok().map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string()) {
        info!("Build time: {bts}");
    }

    if args.healthcheck {
        healthcheck(config_file.as_str());
    }

    let sources_file: String = args.source_file.unwrap_or_else(|| utils::get_default_sources_file_path(&config_path));
    let cfg = utils::read_config(config_path.as_str(), config_file.as_str(),
                                             sources_file.as_str(), api_proxy_file.as_str(),
                                             mappings_file.cloned(), true).unwrap_or_else(|err| exit!("{}", err));

    set_sanitize_sensitive_info(cfg.log.as_ref().is_none_or(|l| l.sanitize_sensitive_info));

    let temp_path = PathBuf::from(&cfg.working_dir).join("tmp");
    create_directories(&cfg, &temp_path);
    let _ = tempfile::env::override_temp_dir(&temp_path);

    let targets = validate_targets(args.target.as_ref(), &cfg.sources).unwrap_or_else(|err| exit!("{}", err));

    info!("Current time: {}", chrono::offset::Local::now().format("%Y-%m-%d %H:%M:%S"));
    info!("Temp dir: {}", temp_path.display());
    info!("Working dir: {:?}", &cfg.working_dir);
    info!("Config dir: {:?}", &cfg.t_config_path);
    info!("Config file: {:?}", &cfg.t_config_file_path);
    info!("Source file: {:?}", &cfg.t_sources_file_path);
    info!("Api Proxy File: {:?}", cfg.t_api_proxy_file_path);
    match utils::read_mappings(&cfg.t_mapping_file_path, true) {
        Ok(Some(mappings)) => {
            info!("Mapping file: {:?}", cfg.t_mapping_file_path);
            cfg.set_mappings(&mappings);
        }
        Ok(None) => {
            info!("Mapping file: not used");
        },
        Err(err) => exit!("{err}"),
    }
    if let Some(cache) = cfg.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()) {
        if cache.enabled {
            info!("Cache dir: {:?}", cache.dir.as_ref().unwrap_or(&String::new()));
        }
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    let () = rt.block_on(async {
        if args.server {
            match utils::read_api_proxy_config(&cfg) {
                Ok(()) => {}
                Err(err) => exit!("{err}"),
            }
            start_in_server_mode(Arc::new(cfg), Arc::new(targets)).await;
        } else {
            start_in_cli_mode(Arc::new(cfg), Arc::new(targets)).await;
        }
    });
}

fn create_directories(cfg: &Config, temp_path: &Path) {
    // Collect the paths into a vector.
    let paths_strings = [
        Some(cfg.working_dir.clone()),
        cfg.backup_dir.clone(),
        cfg.user_config_dir.clone(),
        cfg.video.as_ref().and_then(|v| v.download.as_ref()).and_then(|d| d.directory.clone()),
        cfg.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref().and_then(|c| if c.enabled { c.dir.clone() } else { None }))
    ];

    let mut paths: Vec<PathBuf> = paths_strings.iter()
        .filter_map(|opt| opt.as_ref()) // Get rid of the `Option`
        .map(PathBuf::from).collect();
    paths.push(temp_path.to_path_buf());

    // Iterate over the paths, filter out `None` values, and process the `Some(path)` values.
    for path in &paths {
        if !path.exists() {
            // Create the directory tree if it doesn't exist
            let path_value = path.to_str().unwrap_or("?");
            if let Err(e) = std::fs::create_dir_all(path) {
                error!("Failed to create directory {path_value}: {e}");
            } else {
                info!("Created directory: {path_value}");
            }
        }
    }
}

async fn start_in_cli_mode(cfg: Arc<Config>, targets: Arc<ProcessTargets>) {
    let client = create_client(&cfg).build().unwrap_or_else(|err| {
        error!("Failed to build client {err}");
        reqwest::Client::new()
    });
    playlist::exec_processing(Arc::new(client), cfg, targets).await;
}

async fn start_in_server_mode(cfg: Arc<Config>, targets: Arc<ProcessTargets>) {
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
