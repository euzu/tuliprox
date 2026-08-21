use crate::model::LogLevelConfig;
use crate::utils::config_file_reader;
use chrono::{Local, Offset, SecondsFormat};
use env_logger::{Builder, Logger, Target};
use log::{info, LevelFilter, Log, Metadata, Record, SetLoggerError};
use parking_lot::RwLock;
use shared::model::LogEntry;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::OnceLock;

pub const LOG_BUFFER_CAPACITY: usize = 1000;
const BROADCAST_CAPACITY: usize = 2048;

static LOG_BUFFER: OnceLock<RwLock<VecDeque<LogEntry>>> = OnceLock::new();
static LOG_BROADCASTER: OnceLock<tokio::sync::broadcast::Sender<LogEntry>> = OnceLock::new();

fn get_log_buffer() -> &'static RwLock<VecDeque<LogEntry>> {
    LOG_BUFFER.get_or_init(|| RwLock::new(VecDeque::with_capacity(LOG_BUFFER_CAPACITY)))
}

fn get_log_broadcaster() -> &'static tokio::sync::broadcast::Sender<LogEntry> {
    LOG_BROADCASTER.get_or_init(|| {
        let (tx, _rx) = tokio::sync::broadcast::channel(BROADCAST_CAPACITY);
        tx
    })
}

pub fn push_log_entry(entry: LogEntry) {
    {
        let mut buffer = get_log_buffer().write();
        if buffer.len() >= LOG_BUFFER_CAPACITY {
            buffer.pop_front();
        }
        buffer.push_back(entry.clone());
    }
    let _ = get_log_broadcaster().send(entry);
}

pub fn get_log_history() -> Vec<LogEntry> {
    get_log_buffer().read().iter().cloned().collect()
}

pub fn subscribe_logs() -> tokio::sync::broadcast::Receiver<LogEntry> {
    get_log_broadcaster().subscribe()
}

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

#[derive(Debug, Clone)]
struct LoggerContext {
    cli_log_level: Option<String>,
    env_log_level: Option<String>,
}

impl LoggerContext {
    fn resolve_log_level(&self, config_log_level: Option<&str>) -> String {
        self.cli_log_level
            .clone()
            .or_else(|| self.env_log_level.clone())
            .or_else(|| config_log_level.map(std::string::ToString::to_string))
            .unwrap_or_else(|| "info".to_string())
    }
}

struct ReloadableLogger {
    inner: RwLock<Logger>,
}

impl ReloadableLogger {
    fn new(logger: Logger) -> Self {
        Self { inner: RwLock::new(logger) }
    }

    fn replace(&self, logger: Logger) {
        *self.inner.write() = logger;
    }
}

impl Log for ReloadableLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.inner.read().enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        self.inner.read().log(record);

        let now = Local::now();
        let timestamp = now.to_rfc3339_opts(SecondsFormat::Secs, now.offset().fix().local_minus_utc() == 0);
        let entry = LogEntry {
            timestamp,
            level: record.level().into(),
            target: record.target().to_string(),
            message: record.args().to_string(),
        };
        push_log_entry(entry);
    }

    fn flush(&self) {
        self.inner.read().flush();
    }
}

static LOGGER_CONTEXT: OnceLock<LoggerContext> = OnceLock::new();
static LOGGER_HANDLE: OnceLock<&'static ReloadableLogger> = OnceLock::new();

fn configure_format(log_builder: &mut Builder) {
    log_builder.target(Target::Stdout);
    log_builder.format(move |buf, record| {
        let now = Local::now();
        let timestamp = now.to_rfc3339_opts(SecondsFormat::Secs, now.offset().fix().local_minus_utc() == 0);
        writeln!(
            buf,
            "[{timestamp} {} {}] {}",
            record.level(),
            record.target(),
            record.args()
        )
    });
}

fn apply_level_spec(log_builder: &mut Builder, log_level: &str) -> (LevelFilter, Vec<String>) {
    let mut log_levels = Vec::new();
    let mut effective_max_level = LevelFilter::Off;

    if log_level.contains('=') {
        for pair in log_level.split(',').map(str::trim) {
            if pair.is_empty() {
                continue;
            }
            if let Some((module, level)) = pair.split_once('=') {
                let module = module.trim();
                let level = level.trim();
                if module.is_empty() || level.is_empty() {
                    continue;
                }
                let module_level = get_log_level(level);
                log_levels.push(format!("{module}={module_level}"));
                log_builder.filter_module(module, module_level);
                effective_max_level = effective_max_level.max(module_level);
            } else {
                let level = get_log_level(pair);
                log_levels.push(level.to_string());
                log_builder.filter_level(level);
                effective_max_level = effective_max_level.max(level);
            }
        }
    } else {
        let token = log_level.trim();
        effective_max_level = get_log_level(token);
        log_builder.filter_level(effective_max_level);
        log_levels.push(token.to_string());
    }

    if effective_max_level == LevelFilter::Off {
        effective_max_level = LevelFilter::Info;
    }

    (effective_max_level, log_levels)
}

fn build_logger(log_level: &str) -> (Logger, LevelFilter, Vec<String>) {
    let mut log_builder = Builder::new();
    configure_format(&mut log_builder);

    let (effective_max_level, log_levels) = apply_level_spec(&mut log_builder, log_level);
    for module in LOG_ERROR_LEVEL_MOD {
        log_builder.filter_module(module, LevelFilter::Error);
    }

    (log_builder.build(), effective_max_level, log_levels)
}

fn install_logger(logger: Logger) -> Result<(), SetLoggerError> {
    if let Some(handle) = LOGGER_HANDLE.get() {
        handle.replace(logger);
        return Ok(());
    }

    let handle = Box::leak(Box::new(ReloadableLogger::new(logger)));
    log::set_logger(handle)?;
    let _ = LOGGER_HANDLE.set(handle);
    Ok(())
}

fn read_config_log_level(config_file: &str) -> Option<String> {
    File::open(config_file)
        .ok()
        .and_then(|file| {
            serde_saphyr::from_reader::<_, LogLevelConfig>(config_file_reader(file, true))
                .map_err(|e| eprintln!("Failed to parse log config file: {e}"))
                .ok()
        })
        .and_then(|cfg| cfg.log.and_then(|l| l.log_level))
}

fn apply_logger_with_context(context: &LoggerContext, config_log_level: Option<&str>) {
    let resolved_log_level = context.resolve_log_level(config_log_level);
    let (logger, effective_max_level, log_levels) = build_logger(&resolved_log_level);
    if let Err(err) = install_logger(logger) {
        eprintln!("Failed to install logger: {err}");
        return;
    }
    log::set_max_level(effective_max_level);
    info!("Log timezone system localtime (TZ)");
    info!("Log Level {}", log_levels.join(", "));
}

pub fn init_logger(user_log_level: Option<&str>, config_file: &str) {

    // tracing_subscriber::registry()
    //     .with(console_subscriber::spawn()) // Console layer
    //     .with(EnvFilter::from_default_env())
    //     .with(fmt::layer()) // stdout logging
    //     .init();

    let context = LoggerContext {
        cli_log_level: user_log_level.map(std::string::ToString::to_string),
        env_log_level: std::env::var("TULIPROX_LOG").ok(),
    };
    let _ = LOGGER_CONTEXT.set(context.clone());
    let config_log_level = read_config_log_level(config_file);
    apply_logger_with_context(&context, config_log_level.as_deref());
}

pub fn reload_logger(config_log_level: Option<&str>) {
    if let Some(context) = LOGGER_CONTEXT.get() {
        apply_logger_with_context(context, config_log_level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    #[test]
    fn apply_level_spec_tracks_highest_level_from_module_rules() {
        let mut builder = Builder::new();
        let (max_level, log_levels) = apply_level_spec(&mut builder, "backend=debug,shared=trace");

        assert_eq!(max_level, LevelFilter::Trace);
        assert_eq!(log_levels, vec!["backend=DEBUG".to_string(), "shared=TRACE".to_string()]);
    }

    #[test]
    fn apply_level_spec_ignores_empty_directives_after_trailing_comma() {
        let mut builder = Builder::new();
        let (max_level, log_levels) = apply_level_spec(&mut builder, "backend=debug,");

        assert_eq!(max_level, LevelFilter::Debug);
        assert_eq!(log_levels, vec!["backend=DEBUG".to_string()]);
    }

    #[test]
    fn reloadable_logger_uses_replaced_logger_configuration() {
        let (error_logger, _, _) = build_logger("error");
        let reloadable = ReloadableLogger::new(error_logger);
        let metadata = Metadata::builder().level(Level::Info).target("test_target").build();

        assert!(!reloadable.enabled(&metadata));

        let (info_logger, _, _) = build_logger("info");
        reloadable.replace(info_logger);

        assert!(reloadable.enabled(&metadata));
    }

    #[test]
    fn test_log_buffer_eviction_and_history() {
        for i in 0..LOG_BUFFER_CAPACITY + 10 {
            push_log_entry(LogEntry::new("ts", shared::model::LogLevel::Info, "target", format!("msg {i}")));
        }
        let history = get_log_history();
        assert_eq!(history.len(), LOG_BUFFER_CAPACITY);
        assert_eq!(history.last().unwrap().message, format!("msg {}", LOG_BUFFER_CAPACITY + 9));
        assert_eq!(history.first().unwrap().message, "msg 10");
    }

    #[tokio::test]
    async fn test_log_broadcast_subscription() {
        // The broadcast channel is process-global; parallel tests that
        // push entries (e.g. `test_log_buffer_eviction_and_history`
        // flooding 3 600 messages) can land ahead of ours on the
        // shared `Receiver`. Mark our entry uniquely and drain until
        // we see it.
        let mut rx = subscribe_logs();
        let test_entry = LogEntry::new(
            "ts",
            shared::model::LogLevel::Warn,
            "test_target",
            "broadcast test unique marker",
        );
        push_log_entry(test_entry.clone());

        let mut attempts = 0;
        let received = loop {
            let entry = rx.recv().await.expect("broadcast channel closed");
            if entry.message == test_entry.message {
                break entry;
            }
            attempts += 1;
            assert!(attempts < 10_000, "test entry never arrived on the broadcast channel");
        };
        assert_eq!(received, test_entry);
    }
}
