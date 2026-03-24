use crate::model::LogLevelConfig;
use crate::utils::config_file_reader;
use chrono::{Local, Offset, SecondsFormat};
use env_logger::{Builder, Target};
use log::{error, info, LevelFilter};
use std::fs::File;
use std::io::Write;

const LOG_ERROR_LEVEL_MOD: &[&str] = &[
    "reqwest",
    "hyper_util",
    "tungstenite",
    "rustls_platform_verifier",
    "tokio_tungstenite",
    "notify",
    "mio",
];

fn get_log_level(log_level: &str) -> LevelFilter {
    match log_level.to_lowercase().as_str() {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        // "info" => LevelFilter::Info,
        _ => LevelFilter::Info,
    }
}

fn apply_log_format(builder: &mut Builder) {
    builder.format(|buf, record| {
        let now = Local::now();
        let timestamp = now.to_rfc3339_opts(SecondsFormat::Secs, now.offset().fix().local_minus_utc() == 0);
        writeln!(buf, "[{timestamp} {} {}] {}", record.level(), record.target(), record.args())
    });
}

fn apply_level_spec(log_builder: &mut Builder, log_level: &str) -> (LevelFilter, Vec<String>) {
    let mut log_levels = vec![];
    let effective_max_level;

    if log_level.contains('=') {
        let mut max_level = LevelFilter::Off;
        for pair in log_level.split(',') {
            if pair.contains('=') {
                let mut kv_iter = pair.split('=').map(str::trim);
                if let (Some(module), Some(level)) = (kv_iter.next(), kv_iter.next()) {
                    let module_level = get_log_level(level);
                    log_levels.push(format!("{module}={module_level}"));
                    log_builder.filter_module(module, module_level);
                    if module_level > max_level {
                        max_level = module_level;
                    }
                }
            } else {
                let level = get_log_level(pair);
                log_levels.push(level.to_string());
                log_builder.filter_level(level);
                if level > max_level {
                    max_level = level;
                }
            }
        }
        effective_max_level = max_level;
    } else {
        effective_max_level = get_log_level(log_level);
        log_builder.filter_level(effective_max_level);
        log_levels.push(log_level.to_string());
    }

    (effective_max_level, log_levels)
}

/// Initialize the real logger using CLI argument and `TULIPROX_LOG` env var only.
/// Call this BEFORE config paths are resolved so that all `log::error!` / `exit!` calls
/// during path resolution are visible.
/// Config-file log level is applied later via `apply_config_log_level`.
pub fn init_logger(user_log_level: Option<&str>) {
    let env_log_level = std::env::var("TULIPROX_LOG").ok();
    let log_level = user_log_level
        .map(std::string::ToString::to_string)
        .or(env_log_level)
        .unwrap_or_else(|| "info".to_string());

    let mut log_builder = Builder::from_default_env();
    log_builder.target(Target::Stdout);
    apply_log_format(&mut log_builder);

    let (effective_max_level, log_levels) = apply_level_spec(&mut log_builder, &log_level);

    for module in LOG_ERROR_LEVEL_MOD {
        log_builder.filter_module(module, LevelFilter::Error);
    }

    log_builder.init(); // panics if called twice — correct, we only call this once
    log::set_max_level(effective_max_level);
    info!("Log timezone system localtime (TZ)");
    info!("Log Level {}", &log_levels.join(", "));
}

/// Apply the log level from the config file if no CLI argument or env var
/// overrides it.  Uses `log::set_max_level` so it works after `init_logger` has
/// already registered the `env_logger` instance.
/// Call this AFTER config paths are resolved.
pub fn apply_config_log_level(user_log_level: Option<&str>, config_file: &str) {
    let env_log_level = std::env::var("TULIPROX_LOG").ok();

    // Only read from config if neither CLI nor env overrides the level.
    if user_log_level.is_some() || env_log_level.is_some() {
        return;
    }

    let Some(log_level) = File::open(config_file)
        .ok()
        .and_then(|file| {
            serde_saphyr::from_reader::<_, LogLevelConfig>(config_file_reader(file, true))
                .map_err(|e| error!("Failed to parse log config file: {e}"))
                .ok()
        })
        .and_then(|cfg| cfg.log.and_then(|l| l.log_level))
    else {
        return;
    };

    // Build a temporary builder just to parse the level spec (no try_init).
    let mut dummy = Builder::from_default_env();
    let (effective_max_level, log_levels) = apply_level_spec(&mut dummy, &log_level);
    log::set_max_level(effective_max_level);
    info!("Log Level updated from config: {}", &log_levels.join(", "));
}
