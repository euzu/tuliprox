use crate::{
    model::LogLevelConfig,
    utils::{
        config_file_reader,
        syslog::{in_syslog_worker, SyslogSender, SyslogSink, SyslogTransport},
    },
};
use arc_swap::ArcSwapOption;
use chrono::{Local, Offset, SecondsFormat};
use env_logger::{Builder, Logger, Target};
use log::{info, LevelFilter, Log, Metadata, Record, SetLoggerError};
use parking_lot::RwLock;
use shared::model::{LogConfigDto, LogEntry};
use std::{
    collections::VecDeque,
    fs::File,
    io::Write,
    sync::{Arc, OnceLock},
};

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

pub fn get_log_history() -> Vec<LogEntry> { get_log_buffer().read().iter().cloned().collect() }

pub fn subscribe_logs() -> tokio::sync::broadcast::Receiver<LogEntry> { get_log_broadcaster().subscribe() }

pub(crate) const LOG_ERROR_LEVEL_MOD: &[&str] =
    &["reqwest", "hyper_util", "tungstenite", "rustls_platform_verifier", "tokio_tungstenite", "notify", "mio"];

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
    syslog: ArcSwapOption<SyslogSink>,
    /// Readers hold it while they offer to a sink, `swap_sink` holds it for writing. Once a swap returns, no thread
    /// can still offer to the retired sink, so its shutdown sentinel is queued after every datagram it accepted.
    syslog_gate: RwLock<()>,
}

impl ReloadableLogger {
    fn new(logger: Logger) -> Self {
        Self { inner: RwLock::new(logger), syslog: ArcSwapOption::empty(), syslog_gate: RwLock::new(()) }
    }

    fn replace(&self, logger: Logger) { *self.inner.write() = logger; }

    fn swap_sink(&self, sink: Option<Arc<SyslogSink>>) -> Option<Arc<SyslogSink>> {
        let _gate = self.syslog_gate.write();
        self.syslog.swap(sink)
    }
}

impl Log for ReloadableLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if in_syslog_worker() {
            return self.inner.read().enabled(metadata);
        }
        self.inner.read().enabled(metadata) || self.syslog.load().as_ref().is_some_and(|s| s.enabled(metadata))
    }

    fn log(&self, record: &Record<'_>) {
        let root_enabled = self.inner.read().enabled(record.metadata());
        let sink = if in_syslog_worker() { None } else { self.syslog.load_full() };
        let sink = sink.filter(|s| s.matches(record, root_enabled));
        if !root_enabled && sink.is_none() {
            return;
        }

        let now = Local::now();
        let message = record.args().to_string();
        if let Some(sink) = sink {
            let _gate = self.syslog_gate.read();
            // A swap between the load above and the gate retired `sink`; the current sink takes the record instead.
            let current = self.syslog.load();
            if let Some(current) =
                current.as_ref().filter(|current| Arc::ptr_eq(current, &sink) || current.matches(record, root_enabled))
            {
                current.offer(now, record.level(), record.target(), &message);
            }
        }
        if root_enabled {
            let mut builder = Record::builder();
            builder
                .level(record.level())
                .target(record.target())
                .module_path(record.module_path())
                .file(record.file())
                .line(record.line());
            let msg_args = format_args!("{message}");
            builder.args(msg_args);
            let stdout_record = builder.build();
            self.inner.read().log(&stdout_record);
            let timestamp = now.to_rfc3339_opts(SecondsFormat::Secs, now.offset().fix().local_minus_utc() == 0);
            let entry =
                LogEntry { timestamp, level: record.level().into(), target: record.target().to_string(), message };
            push_log_entry(entry);
        }
    }

    fn flush(&self) { self.inner.read().flush(); }
}

static LOGGER_CONTEXT: OnceLock<LoggerContext> = OnceLock::new();
static LOGGER_HANDLE: OnceLock<&'static ReloadableLogger> = OnceLock::new();

fn configure_format(log_builder: &mut Builder, target: Target) {
    log_builder.target(target);
    log_builder.format(move |buf, record| {
        let now = Local::now();
        let timestamp = now.to_rfc3339_opts(SecondsFormat::Secs, now.offset().fix().local_minus_utc() == 0);
        writeln!(buf, "[{timestamp} {} {}] {}", record.level(), record.target(), record.args())
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

pub(crate) fn build_logger_with_target(log_level: &str, target: Target) -> (Logger, LevelFilter, Vec<String>) {
    let mut log_builder = Builder::new();
    configure_format(&mut log_builder, target);

    let (effective_max_level, log_levels) = apply_level_spec(&mut log_builder, log_level);
    for module in LOG_ERROR_LEVEL_MOD {
        log_builder.filter_module(module, LevelFilter::Error);
    }

    (log_builder.build(), effective_max_level, log_levels)
}

fn build_logger(log_level: &str) -> (Logger, LevelFilter, Vec<String>) {
    build_logger_with_target(log_level, Target::Stdout)
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

pub(crate) fn read_log_config(config_file: &str) -> Option<LogConfigDto> {
    File::open(config_file)
        .ok()
        .and_then(|file| {
            serde_saphyr::from_reader::<_, LogLevelConfig>(config_file_reader(file, true))
                .map_err(|e| eprintln!("Failed to parse log config file: {e}"))
                .ok()
        })
        .and_then(|cfg| cfg.log)
        // The early parse must agree with the full parse, which runs `clean()` inside `ConfigDto::prepare`.
        // Without it a `hostname` of blanks reaches the formatter as `___` instead of falling back to the system
        // hostname, and a `host` with surrounding blanks would not resolve.
        .map(|mut log| {
            log.clean();
            log
        })
}

pub(crate) fn compute_max_level(root_max: LevelFilter, syslog_max: LevelFilter) -> LevelFilter {
    root_max.max(syslog_max)
}

/// The one place that turns log configuration into a running sink. `existing_sender` carries the sender of the
/// currently installed sink so an unchanged transport reuses it instead of rebuilding socket, thread and queue.
pub(crate) fn build_syslog_sink(
    log: Option<&LogConfigDto>,
    existing_sender: Option<&Arc<SyslogSender>>,
) -> Result<Option<Arc<SyslogSink>>, String> {
    #[cfg(test)]
    {
        build_syslog_sink_internal(log, existing_sender, None)
    }
    #[cfg(not(test))]
    {
        build_syslog_sink_internal(log, existing_sender)
    }
}

#[cfg(test)]
pub(crate) fn build_syslog_sink_with_seams(
    log: Option<&LogConfigDto>,
    existing_sender: Option<&Arc<SyslogSender>>,
    seams: crate::utils::syslog::Seams,
) -> Result<Option<Arc<SyslogSink>>, String> {
    build_syslog_sink_internal(log, existing_sender, Some(seams))
}

/// Maps the configured remote level onto a filter level. `trace` has no remote level: request-header dumps would
/// leave the host unencrypted, so validation rejects it and this mapping reports it as absent rather than panicking.
fn syslog_level_filter(level: shared::model::LogLevel) -> Option<LevelFilter> {
    match level {
        shared::model::LogLevel::Debug => Some(LevelFilter::Debug),
        shared::model::LogLevel::Info => Some(LevelFilter::Info),
        shared::model::LogLevel::Warn => Some(LevelFilter::Warn),
        shared::model::LogLevel::Error => Some(LevelFilter::Error),
        shared::model::LogLevel::Trace => None,
    }
}

pub(crate) fn build_syslog_sink_internal(
    log: Option<&LogConfigDto>,
    existing_sender: Option<&Arc<SyslogSender>>,
    #[cfg(test)] seams: Option<crate::utils::syslog::Seams>,
) -> Result<Option<Arc<SyslogSink>>, String> {
    let Some(log) = log else {
        return Ok(None);
    };
    if !log.syslog.enabled {
        return Ok(None);
    }
    // `init_logger` reads the log block before `ConfigDto::prepare` validates it, so the sink validates on its own:
    // the formatter relies on the `max_message_len` minimum, and `trace` must never reach the collector.
    log.syslog.validate().map_err(|err| err.to_string())?;

    let config = crate::model::SyslogConfig::from(&log.syslog);
    let transport = SyslogTransport::from_config(&config);

    let sender = match existing_sender {
        Some(existing) if existing.same_transport(&transport) => Arc::clone(existing),
        #[cfg(test)]
        _ => match seams {
            Some(seams) => SyslogSender::start_with_seams(&config, seams)?,
            None => SyslogSender::start(&config)?,
        },
        #[cfg(not(test))]
        _ => SyslogSender::start(&config)?,
    };

    let filter = log.syslog.log_level.and_then(syslog_level_filter).map(SyslogSink::build_filter);

    Ok(Some(Arc::new(SyslogSink::new(sender, filter))))
}

fn apply_logger_with_context(context: &LoggerContext, log: Option<&LogConfigDto>) {
    let running_sink = LOGGER_HANDLE.get().and_then(|h| h.syslog.load_full());
    let running_sender = running_sink.as_ref().map(|s| &s.sender);

    let new_sink = match build_syslog_sink(log, running_sender) {
        Ok(sink) => sink,
        Err(err) => {
            eprintln!("Failed to configure syslog: {err}");
            None
        }
    };

    let old_to_shutdown = if let Some(old_sink) = running_sink {
        let reused = new_sink.as_ref().is_some_and(|new_s| Arc::ptr_eq(&old_sink.sender, &new_s.sender));
        if reused {
            None
        } else {
            Some(old_sink.sender.clone())
        }
    } else {
        None
    };

    // Sink first: a reload must not leave the stdout logger pointing at the previous sink. On the very first call
    // there is no handle yet, which is why the same store repeats after `install_logger` below.
    if let Some(handle) = LOGGER_HANDLE.get() {
        handle.swap_sink(new_sink.clone());
    }

    // Reload runs on a Tokio worker, so it must not wait for the old worker: `stop` queues the shutdown behind the
    // pending datagrams and the old worker drains them on its own thread.
    if let Some(old_sender) = old_to_shutdown {
        old_sender.stop();
    }

    let config_log_level = log.and_then(|c| c.log_level.as_deref());
    let resolved_log_level = context.resolve_log_level(config_log_level);
    let (logger, root_max, log_levels) = build_logger(&resolved_log_level);
    if let Err(err) = install_logger(logger) {
        eprintln!("Failed to install logger: {err}");
        return;
    }
    if let Some(handle) = LOGGER_HANDLE.get() {
        handle.syslog.store(new_sink.clone());
    }

    let syslog_max = new_sink.as_ref().map_or(LevelFilter::Off, |sink| sink.max_level());
    log::set_max_level(compute_max_level(root_max, syslog_max));
    info!("Log timezone system localtime (TZ)");
    info!("Log Level {}", log_levels.join(", "));
}

pub fn init_logger(user_log_level: Option<&str>, config_file: &str) {
    let context = LoggerContext {
        cli_log_level: user_log_level.map(std::string::ToString::to_string),
        env_log_level: std::env::var("TULIPROX_LOG").ok(),
    };
    let _ = LOGGER_CONTEXT.set(context.clone());
    let log_cfg = read_log_config(config_file);
    apply_logger_with_context(&context, log_cfg.as_ref());
}

pub fn reload_logger(log: Option<&LogConfigDto>) {
    let default_ctx = LoggerContext { cli_log_level: None, env_log_level: None };
    let context = LOGGER_CONTEXT.get().unwrap_or(&default_ctx);
    apply_logger_with_context(context, log);
}

pub fn flush_syslog() {
    if let Some(handle) = LOGGER_HANDLE.get() {
        if let Some(sink) = handle.swap_sink(None) {
            let sender = sink.sender.clone();
            drop(sink);
            sender.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::items_after_statements)]
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
        // The log buffer is process-global, so entries pushed by other tests
        // running in parallel share it. Tag our own entries and assert only on
        // the subsequence we pushed; asserting on absolute buffer positions
        // makes this test order-dependent (see `test_log_broadcast_subscription`).
        const TARGET: &str = "log_buffer_eviction_and_history";

        for i in 0..LOG_BUFFER_CAPACITY + 10 {
            push_log_entry(LogEntry::new("ts", shared::model::LogLevel::Info, TARGET, format!("msg {i}")));
        }
        let history = get_log_history();
        assert_eq!(history.len(), LOG_BUFFER_CAPACITY);

        let ours: Vec<&LogEntry> = history.iter().filter(|entry| entry.target == TARGET).collect();
        assert_eq!(ours.last().unwrap().message, format!("msg {}", LOG_BUFFER_CAPACITY + 9));

        // We pushed 10 more entries than the buffer holds, so at least the
        // first 10 of ours must have been evicted.
        let oldest_retained: usize =
            ours.first().unwrap().message.strip_prefix("msg ").expect("tagged entry").parse().expect("numeric suffix");
        assert!(
            oldest_retained >= 10,
            "expected at least 10 evicted entries, oldest retained is msg {oldest_retained}"
        );
    }

    #[tokio::test]
    async fn test_log_broadcast_subscription() {
        // The broadcast channel is process-global; parallel tests that
        // push entries (e.g. `test_log_buffer_eviction_and_history`
        // flooding 3 600 messages) can land ahead of ours on the
        // shared `Receiver`. Mark our entry uniquely and drain until
        // we see it.
        let mut rx = subscribe_logs();
        let test_entry =
            LogEntry::new("ts", shared::model::LogLevel::Warn, "test_target", "broadcast test unique marker");
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

    #[test]
    fn logging_test_4_ring_buffer_gated_on_root_filter() {
        let (logger, _, _) = build_logger("info");
        let reloadable = ReloadableLogger::new(logger);

        const UNIQUE_TARGET: &str = "logging_test_4_unique_target";
        const REQWEST_MSG: &str = "logging_test_4_reqwest_msg";
        const UNIQUE_MSG: &str = "logging_test_4_unique_msg";

        let reqwest_record = Record::builder()
            .level(Level::Info)
            .target("reqwest")
            .args(format_args!("logging_test_4_reqwest_msg"))
            .build();
        let unique_record = Record::builder()
            .level(Level::Info)
            .target(UNIQUE_TARGET)
            .args(format_args!("logging_test_4_unique_msg"))
            .build();

        reloadable.log(&reqwest_record);
        reloadable.log(&unique_record);

        let history = get_log_history();
        assert!(
            !history.iter().any(|entry| entry.message == REQWEST_MSG),
            "reqwest info record must not be in log history under root info"
        );
        assert!(
            history.iter().any(|entry| entry.target == UNIQUE_TARGET && entry.message == UNIQUE_MSG),
            "unique target info record must be in log history"
        );
    }

    #[test]
    fn logging_test_6_no_sink_ring_buffer_gate() {
        let (logger, _, _) = build_logger("info");
        let reloadable = ReloadableLogger::new(logger);
        assert!(reloadable.syslog.load().is_none());

        const UNIQUE_TARGET: &str = "logging_test_6_tuliprox_unique_target";
        const REQWEST_MSG: &str = "logging_test_6_reqwest_msg";
        const TULIPROX_MSG: &str = "logging_test_6_tuliprox_msg";

        let reqwest_record = Record::builder()
            .level(Level::Info)
            .target("reqwest")
            .args(format_args!("logging_test_6_reqwest_msg"))
            .build();
        let tuliprox_record = Record::builder()
            .level(Level::Info)
            .target(UNIQUE_TARGET)
            .args(format_args!("logging_test_6_tuliprox_msg"))
            .build();

        reloadable.log(&reqwest_record);
        reloadable.log(&tuliprox_record);

        let history = get_log_history();
        assert!(
            !history.iter().any(|entry| entry.message == REQWEST_MSG),
            "with no sink, reqwest info record must not be in log history"
        );
        assert!(
            history.iter().any(|entry| entry.target == UNIQUE_TARGET && entry.message == TULIPROX_MSG),
            "with no sink, tuliprox info record must be in log history"
        );
    }

    #[test]
    fn logging_test_8_early_parse_reads_minimal_yaml_with_syslog() {
        let mut file = tempfile::NamedTempFile::new().expect("create tempfile");
        let yaml = r#"
log:
  log_level: info
  syslog:
    enabled: true
    host: "10.0.0.1"
    port: 514
    format: rfc5424
"#;
        file.write_all(yaml.as_bytes()).expect("write tempfile");
        let file_path = file.path().to_str().expect("valid path");

        let log_dto = read_log_config(file_path).expect("early parse should yield LogConfigDto");
        assert_eq!(log_dto.log_level.as_deref(), Some("info"));
        assert!(log_dto.syslog.enabled);
        assert_eq!(log_dto.syslog.host, "10.0.0.1");
        assert_eq!(log_dto.syslog.port, 514);
        assert_eq!(log_dto.syslog.format, shared::model::SyslogFormat::Rfc5424);
    }

    /// The early parse feeds `init_logger` before any `ConfigDto::prepare` runs, so it has to produce the same
    /// normalised DTO: a blank `hostname` must become `None` (system hostname), not `___` in the header, and a
    /// `host` with surrounding blanks must be trimmed before it is resolved.
    #[test]
    fn logging_test_8b_early_parse_normalises_like_the_full_parse() {
        let mut file = tempfile::NamedTempFile::new().expect("create tempfile");
        let yaml = r#"
log:
  log_level: "  warn  "
  syslog:
    enabled: true
    host: "  10.0.0.1  "
    hostname: "   "
    app_name: "   "
"#;
        file.write_all(yaml.as_bytes()).expect("write tempfile");
        let file_path = file.path().to_str().expect("valid path");

        let log_dto = read_log_config(file_path).expect("early parse");
        assert_eq!(log_dto.syslog.host, "10.0.0.1");
        assert_eq!(log_dto.syslog.hostname, None);
        assert_eq!(log_dto.syslog.app_name, "tuliprox");

        // Same block through the full DTO path must end in the same state.
        let mut parsed: LogConfigDto =
            serde_saphyr::from_str::<LogLevelConfig>(yaml).expect("full parse").log.expect("log block");
        parsed.clean();
        assert_eq!(parsed.syslog, log_dto.syslog);
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn to_string_lossy(&self) -> String { String::from_utf8_lossy(&self.0.lock()).into_owned() }
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn logging_test_1_worker_thread_guard_on_enabled() {
        let (logger, _, _) = build_logger("info");
        let reloadable = ReloadableLogger::new(logger);

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 514,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        let filter = SyslogSink::build_filter(LevelFilter::Debug);
        let sink = Arc::new(SyslogSink::new(sender, Some(filter)));
        reloadable.syslog.store(Some(sink));

        let debug_meta = Metadata::builder().level(Level::Debug).target("test_target").build();
        let info_meta = Metadata::builder().level(Level::Info).target("test_target").build();

        // On normal thread, sink filter (Debug) ORs with root (Info)
        assert!(reloadable.enabled(&debug_meta));
        assert!(reloadable.enabled(&info_meta));

        // In worker thread, sink is bypassed; returns root result only
        {
            let _guard = crate::utils::syslog::SyslogWorkerGuard::enter();
            assert!(!reloadable.enabled(&debug_meta));
            assert!(reloadable.enabled(&info_meta));
        }
    }

    #[test]
    fn logging_test_2_fan_out_levels() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(500))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = SharedWriter(captured);
        let (logger, _, _) = build_logger_with_target("info", Target::Pipe(Box::new(writer.clone())));
        let reloadable = ReloadableLogger::new(logger);

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        let filter = SyslogSink::build_filter(LevelFilter::Debug);
        let sink = Arc::new(SyslogSink::new(sender, Some(filter)));
        reloadable.syslog.store(Some(sink.clone()));

        const DEBUG_TARGET: &str = "logging_test_2_debug_target";
        const DEBUG_MSG: &str = "logging_test_2_debug_msg";
        const INFO_TARGET: &str = "logging_test_2_info_target";
        const INFO_MSG: &str = "logging_test_2_info_msg";

        let debug_args = format_args!("{DEBUG_MSG}");
        let debug_record = Record::builder().level(Level::Debug).target(DEBUG_TARGET).args(debug_args).build();
        let info_args = format_args!("{INFO_MSG}");
        let info_record = Record::builder().level(Level::Info).target(INFO_TARGET).args(info_args).build();

        reloadable.log(&debug_record);

        // Debug record reaches UDP receiver
        let mut buf = [0u8; 1024];
        let (n, _) = rx_sock.recv_from(&mut buf).expect("recv debug datagram");
        let dgram = String::from_utf8_lossy(&buf[..n]);
        assert!(dgram.contains(DEBUG_MSG));

        // But debug record does NOT reach captured stdout nor history
        assert!(!writer.to_string_lossy().contains(DEBUG_MSG));
        let history = get_log_history();
        assert!(!history.iter().any(|e| e.message == DEBUG_MSG));

        // Info record reaches all three
        reloadable.log(&info_record);
        let (n, _) = rx_sock.recv_from(&mut buf).expect("recv info datagram");
        let dgram = String::from_utf8_lossy(&buf[..n]);
        assert!(dgram.contains(INFO_MSG));

        assert!(writer.to_string_lossy().contains(INFO_MSG));
        let history = get_log_history();
        assert!(history.iter().any(|e| e.message == INFO_MSG));

        sink.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_3_noise_list_suppressed_remotely() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let (logger, _, _) = build_logger("info");
        let reloadable = ReloadableLogger::new(logger);

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        let filter = SyslogSink::build_filter(LevelFilter::Debug);
        let sink = Arc::new(SyslogSink::new(sender, Some(filter)));
        reloadable.syslog.store(Some(sink.clone()));

        const REQWEST_MSG: &str = "logging_test_3_reqwest_noise";
        let reqwest_args = format_args!("{REQWEST_MSG}");
        let reqwest_record = Record::builder().level(Level::Debug).target("reqwest").args(reqwest_args).build();

        reloadable.log(&reqwest_record);

        let mut buf = [0u8; 1024];
        let res = rx_sock.recv_from(&mut buf);
        assert!(res.is_err(), "reqwest debug record must not be shipped to UDP receiver");

        // Allowed debug record is shipped
        const ALLOWED_MSG: &str = "logging_test_3_allowed_msg";
        let allowed_args = format_args!("{ALLOWED_MSG}");
        let allowed_record = Record::builder().level(Level::Debug).target("tuliprox").args(allowed_args).build();
        reloadable.log(&allowed_record);
        let (n, _) = rx_sock.recv_from(&mut buf).expect("recv allowed datagram");
        assert!(String::from_utf8_lossy(&buf[..n]).contains(ALLOWED_MSG));

        sink.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_5_sink_without_own_filter_follows_root() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = SharedWriter(captured);
        let (logger, _, _) = build_logger_with_target("info", Target::Pipe(Box::new(writer.clone())));
        let reloadable = ReloadableLogger::new(logger);

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        // filter: None means follow root filter
        let sink = Arc::new(SyslogSink::new(sender, None));
        reloadable.syslog.store(Some(sink.clone()));

        const DEBUG_MSG: &str = "logging_test_5_debug_msg";
        const INFO_MSG: &str = "logging_test_5_info_msg";

        let debug_args = format_args!("{DEBUG_MSG}");
        let debug_record = Record::builder().level(Level::Debug).target("test").args(debug_args).build();
        let info_args = format_args!("{INFO_MSG}");
        let info_record = Record::builder().level(Level::Info).target("test").args(info_args).build();

        reloadable.log(&debug_record);
        let mut buf = [0u8; 1024];
        assert!(rx_sock.recv_from(&mut buf).is_err(), "debug record should not be received");
        assert!(!writer.to_string_lossy().contains(DEBUG_MSG));
        assert!(!get_log_history().iter().any(|e| e.message == DEBUG_MSG));

        reloadable.log(&info_record);
        let (n, _) = rx_sock.recv_from(&mut buf).expect("info record received");
        assert!(String::from_utf8_lossy(&buf[..n]).contains(INFO_MSG));
        assert!(writer.to_string_lossy().contains(INFO_MSG));
        assert!(get_log_history().iter().any(|e| e.message == INFO_MSG));

        sink.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_7_reload_rebuilds_filter_and_reuses_sender() {
        let mut dto1 = LogConfigDto::default();
        dto1.syslog.enabled = true;
        dto1.syslog.host = "127.0.0.1".to_string();
        dto1.syslog.port = 514;
        dto1.syslog.log_level = Some(shared::model::LogLevel::Info);

        let sink1 = build_syslog_sink_internal(Some(&dto1), None, None).expect("build sink1").expect("some sink1");

        let mut dto2 = dto1.clone();
        dto2.syslog.log_level = Some(shared::model::LogLevel::Debug);

        let sink2 = build_syslog_sink_internal(Some(&dto2), Some(&sink1.sender), None)
            .expect("build sink2")
            .expect("some sink2");

        assert!(Arc::ptr_eq(&sink1.sender, &sink2.sender), "same transport must reuse sender");
        assert_eq!(compute_max_level(LevelFilter::Info, sink1.max_level()), LevelFilter::Info);
        assert_eq!(compute_max_level(LevelFilter::Info, sink2.max_level()), LevelFilter::Debug);

        sink1.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_9_disabled_syslog_returns_no_sink() {
        let dto_default = LogConfigDto::default();
        assert!(
            build_syslog_sink(Some(&dto_default), None).expect("build").is_none(),
            "default LogConfigDto must yield None"
        );

        let mut dto_prestaged = LogConfigDto::default();
        dto_prestaged.syslog.host = "192.168.1.100".to_string();
        dto_prestaged.syslog.port = 514;
        dto_prestaged.syslog.enabled = false;
        assert!(
            build_syslog_sink(Some(&dto_prestaged), None).expect("build").is_none(),
            "disabled with prestaged host must yield None"
        );

        assert!(build_syslog_sink(None, None).expect("build").is_none());

        let mut dto_trace = LogConfigDto::default();
        dto_trace.syslog.enabled = true;
        dto_trace.syslog.host = "127.0.0.1".to_string();
        dto_trace.syslog.log_level = Some(shared::model::LogLevel::Trace);
        assert!(
            build_syslog_sink(Some(&dto_trace), None).is_err(),
            "trace must be rejected instead of silently ignored"
        );

        // The early parse skips `ConfigDto::prepare`, so the sink itself must reject values the formatter cannot meet.
        let mut dto_short_json = LogConfigDto::default();
        dto_short_json.syslog.enabled = true;
        dto_short_json.syslog.host = "127.0.0.1".to_string();
        dto_short_json.syslog.format = shared::model::SyslogFormat::Json;
        dto_short_json.syslog.max_message_len = 200;
        assert!(build_syslog_sink(Some(&dto_short_json), None).is_err(), "too small max_message_len must be rejected");
    }

    #[test]
    fn logging_test_9b_inherited_level_never_ships_trace() {
        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            ..crate::model::SyslogConfig::default()
        };
        let sink = SyslogSink::new(SyslogSender::start(&config).expect("sender start"), None);

        let trace_args = format_args!("trace");
        let trace_record = Record::builder().level(Level::Trace).target("test").args(trace_args).build();
        let debug_args = format_args!("debug");
        let debug_record = Record::builder().level(Level::Debug).target("test").args(debug_args).build();

        assert!(!sink.matches(&trace_record, true), "root trace must not reach the collector");
        assert!(sink.matches(&debug_record, true));
        assert!(!sink.matches(&debug_record, false));

        sink.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_10_test_a_module_rules_all_three_levels() {
        let mut builder = Builder::new();
        let (root_max, _) = apply_level_spec(&mut builder, "info,tuliprox=debug");
        assert_eq!(root_max, LevelFilter::Debug);
        assert_eq!(compute_max_level(root_max, LevelFilter::Info), LevelFilter::Debug);

        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = SharedWriter(captured);
        let (logger, _, _) = build_logger_with_target("info,tuliprox=debug", Target::Pipe(Box::new(writer.clone())));
        let reloadable = ReloadableLogger::new(logger);

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");

        // 1. Sink at Info
        let filter_info = SyslogSink::build_filter(LevelFilter::Info);
        let sink_info = Arc::new(SyslogSink::new(sender.clone(), Some(filter_info)));
        reloadable.syslog.store(Some(sink_info));

        const TULIPROX_MSG_1: &str = "logging_test_10_tuliprox_debug_1";
        let tuliprox_args_1 = format_args!("{TULIPROX_MSG_1}");
        let tuliprox_record_1 = Record::builder().level(Level::Debug).target("tuliprox").args(tuliprox_args_1).build();
        reloadable.log(&tuliprox_record_1);

        assert!(writer.to_string_lossy().contains(TULIPROX_MSG_1));
        assert!(get_log_history().iter().any(|e| e.message == TULIPROX_MSG_1));
        let mut buf = [0u8; 1024];
        assert!(rx_sock.recv_from(&mut buf).is_err(), "UDP receiver must get nothing for sink info");

        // reqwest debug record rejected by level 2 and level 3
        const REQWEST_MSG_1: &str = "logging_test_10_reqwest_debug_1";
        let reqwest_args_1 = format_args!("{REQWEST_MSG_1}");
        let reqwest_record_1 = Record::builder().level(Level::Debug).target("reqwest").args(reqwest_args_1).build();
        reloadable.log(&reqwest_record_1);
        assert!(!writer.to_string_lossy().contains(REQWEST_MSG_1));
        assert!(!get_log_history().iter().any(|e| e.message == REQWEST_MSG_1));
        assert!(rx_sock.recv_from(&mut buf).is_err());

        // 2. Sink at Debug
        let filter_debug = SyslogSink::build_filter(LevelFilter::Debug);
        let sink_debug = Arc::new(SyslogSink::new(sender.clone(), Some(filter_debug)));
        reloadable.syslog.store(Some(sink_debug));

        const TULIPROX_MSG_2: &str = "logging_test_10_tuliprox_debug_2";
        let tuliprox_args_2 = format_args!("{TULIPROX_MSG_2}");
        let tuliprox_record_2 = Record::builder().level(Level::Debug).target("tuliprox").args(tuliprox_args_2).build();
        reloadable.log(&tuliprox_record_2);

        let (n, _) = rx_sock.recv_from(&mut buf).expect("recv datagram");
        assert!(String::from_utf8_lossy(&buf[..n]).contains(TULIPROX_MSG_2));
        assert!(writer.to_string_lossy().contains(TULIPROX_MSG_2));
        assert!(get_log_history().iter().any(|e| e.message == TULIPROX_MSG_2));

        // reqwest debug still rejected by noise list
        const REQWEST_MSG_2: &str = "logging_test_10_reqwest_debug_2";
        let reqwest_args_2 = format_args!("{REQWEST_MSG_2}");
        let reqwest_record_2 = Record::builder().level(Level::Debug).target("reqwest").args(reqwest_args_2).build();
        reloadable.log(&reqwest_record_2);
        assert!(!writer.to_string_lossy().contains(REQWEST_MSG_2));
        assert!(!get_log_history().iter().any(|e| e.message == REQWEST_MSG_2));
        assert!(rx_sock.recv_from(&mut buf).is_err());

        // CLI/env override wins over log.log_level
        let ctx_cli =
            LoggerContext { cli_log_level: Some("error".to_string()), env_log_level: Some("warn".to_string()) };
        assert_eq!(ctx_cli.resolve_log_level(Some("info")), "error");

        let ctx_env = LoggerContext { cli_log_level: None, env_log_level: Some("warn".to_string()) };
        assert_eq!(ctx_env.resolve_log_level(Some("info")), "warn");

        sender.shutdown();
    }

    #[test]
    fn logging_test_11_test_b_filter_transitions() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let (logger, _, _) = build_logger("info");
        let reloadable = ReloadableLogger::new(logger);

        let mut dto = LogConfigDto::default();
        dto.syslog.enabled = true;
        dto.syslog.host = "127.0.0.1".to_string();
        dto.syslog.port = port;
        dto.syslog.log_level = Some(shared::model::LogLevel::Debug);

        let sink_debug = build_syslog_sink_internal(Some(&dto), None, None).expect("build").expect("some");
        reloadable.syslog.store(Some(sink_debug.clone()));

        const DEBUG_MSG_1: &str = "logging_test_11_debug_before";
        let args_1 = format_args!("{DEBUG_MSG_1}");
        let record_1 = Record::builder().level(Level::Debug).target("tuliprox").args(args_1).build();
        reloadable.log(&record_1);

        let mut buf = [0u8; 1024];
        let (n, _) = rx_sock.recv_from(&mut buf).expect("recv datagram");
        assert!(String::from_utf8_lossy(&buf[..n]).contains(DEBUG_MSG_1));

        // Reload to Info
        dto.syslog.log_level = Some(shared::model::LogLevel::Info);
        let sink_info =
            build_syslog_sink_internal(Some(&dto), Some(&sink_debug.sender), None).expect("build").expect("some");
        assert!(Arc::ptr_eq(&sink_debug.sender, &sink_info.sender));
        assert_eq!(compute_max_level(LevelFilter::Info, sink_info.max_level()), LevelFilter::Info);

        reloadable.syslog.store(Some(sink_info.clone()));

        const DEBUG_MSG_2: &str = "logging_test_11_debug_after";
        let args_2 = format_args!("{DEBUG_MSG_2}");
        let record_2 = Record::builder().level(Level::Debug).target("tuliprox").args(args_2).build();
        reloadable.log(&record_2);
        assert!(rx_sock.recv_from(&mut buf).is_err(), "no debug record shipped after reload to info");

        sink_info.sender.clone().shutdown();
    }

    #[test]
    fn logging_test_12_test_c_load_and_lifecycle() {
        use std::time::Instant;

        // 1. Full queue under sustained calls
        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 514,
            queue_capacity: 1,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        let sink = SyslogSink::new(sender.clone(), None);

        let now = Local::now();
        // Warm up / fill queue
        for _ in 0..100 {
            sink.offer(now, Level::Info, "target", "warmup");
        }

        let start = Instant::now();
        const CALLS: u32 = 10_000;
        for _ in 0..CALLS {
            sink.offer(now, Level::Info, "target", "sustained");
        }
        let elapsed = start.elapsed();
        let per_call = elapsed / CALLS;
        assert!(per_call < std::time::Duration::from_micros(100), "expected < 100 µs per call, got {per_call:?}");

        // 2. Reload during active shipping does not block longer than SYSLOG_SHUTDOWN_TIMEOUT
        let mut dto = LogConfigDto::default();
        dto.syslog.enabled = true;
        dto.syslog.host = "127.0.0.1".to_string();
        dto.syslog.port = 514;
        dto.syslog.queue_capacity = 1;
        let reload_start = Instant::now();
        for _ in 0..50 {
            sink.offer(now, Level::Info, "target", "reload load");
        }
        let reloaded = build_syslog_sink_internal(Some(&dto), None, None).expect("rebuild during load");
        let reload_elapsed = reload_start.elapsed();
        assert!(
            reload_elapsed
                <= crate::utils::syslog::sender::SYSLOG_SHUTDOWN_TIMEOUT + std::time::Duration::from_millis(500),
            "rebuild under load took {reload_elapsed:?}"
        );
        if let Some(new_sink) = reloaded {
            new_sink.sender.clone().shutdown();
        }

        let shutdown_start = Instant::now();
        sender.shutdown();
        let shutdown_elapsed = shutdown_start.elapsed();
        assert!(
            shutdown_elapsed
                <= crate::utils::syslog::sender::SYSLOG_SHUTDOWN_TIMEOUT + std::time::Duration::from_millis(500),
            "shutdown took {shutdown_elapsed:?}"
        );
    }

    #[test]
    fn logging_test_13_test_d_wire_format_edges() {
        use crate::utils::syslog::{formatter::format, SyslogHeader, SyslogMeta};
        use shared::model::{SyslogFacility, SyslogFormat};

        let formats = [(SyslogFormat::Rfc5424, 480), (SyslogFormat::Rfc3164, 480), (SyslogFormat::Json, 1024)];

        let long_message = "🚀 Über-Nachricht \r\n mit Steuerzeichen \t \0 und viel Text: ".repeat(50);

        for (fmt, max_len) in formats {
            let header = SyslogHeader {
                format: fmt,
                facility: SyslogFacility::Local0,
                app_name: "tuliprox".to_string(),
                hostname: "node-1.example.com".to_string(),
                pid: 12345,
                max_len,
            };
            let meta = SyslogMeta {
                timestamp: Local::now(),
                level: Level::Info,
                target: "tuliprox::api::main_api",
                message: &long_message,
            };

            let (datagram, truncated) = format(&meta, &header);
            assert!(datagram.len() <= max_len, "format {fmt:?} produced len {} > {max_len}", datagram.len());
            assert!(truncated, "expected message to be truncated");

            match fmt {
                SyslogFormat::Rfc5424 => {
                    assert!(datagram.starts_with("<134>1 "));
                    assert!(datagram.contains("node-1.example.com tuliprox 12345 main_api - "));
                }
                SyslogFormat::Rfc3164 => {
                    assert!(datagram.starts_with("<134>"));
                    assert!(datagram.contains("node-1.example.com tuliprox[12345]: "));
                }
                SyslogFormat::Json => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(&datagram).expect("json must remain valid after truncation");
                    assert_eq!(parsed["truncated"], true);
                    assert_eq!(parsed["facility"], "local0");
                    assert_eq!(parsed["level"], "info");
                }
            }
        }
    }

    #[test]
    fn logging_test_14_hot_path_formatting_matrix() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingMsg<'a>(&'a AtomicUsize, &'a str);
        impl std::fmt::Display for CountingMsg<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fetch_add(1, Ordering::SeqCst);
                write!(f, "{}", self.1)
            }
        }

        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 514,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");

        // Case 1: Level 2 rejects + Level 3 rejects => not formatted, offer not called
        {
            let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let writer = SharedWriter(captured);
            let (logger, _, _) = build_logger_with_target("warn", Target::Pipe(Box::new(writer)));
            let reloadable = ReloadableLogger::new(logger);
            let sink = Arc::new(SyslogSink::new(sender.clone(), Some(SyslogSink::build_filter(LevelFilter::Warn))));
            reloadable.syslog.store(Some(sink.clone()));

            let count = AtomicUsize::new(0);
            let msg = CountingMsg(&count, "case_1_msg");
            let args = format_args!("{msg}");
            let record = Record::builder().level(Level::Debug).target("case_1_target").args(args).build();
            reloadable.log(&record);

            assert_eq!(count.load(Ordering::SeqCst), 0, "case 1: must not format");
            assert_eq!(sink.offer_count(), 0, "case 1: offer not called");
        }

        // Case 2: Level 2 accepts + Level 3 rejects => formatted exactly once, in stdout and history, offer not called
        {
            let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let writer = SharedWriter(captured);
            let (logger, _, _) = build_logger_with_target("debug", Target::Pipe(Box::new(writer.clone())));
            let reloadable = ReloadableLogger::new(logger);
            let sink = Arc::new(SyslogSink::new(sender.clone(), Some(SyslogSink::build_filter(LevelFilter::Warn))));
            reloadable.syslog.store(Some(sink.clone()));

            let count = AtomicUsize::new(0);
            const C2_MSG: &str = "case_2_unique_msg";
            let msg = CountingMsg(&count, C2_MSG);
            let args = format_args!("{msg}");
            let record = Record::builder().level(Level::Info).target("case_2_target").args(args).build();
            reloadable.log(&record);

            assert_eq!(count.load(Ordering::SeqCst), 1, "case 2: formatted exactly once");
            assert_eq!(sink.offer_count(), 0, "case 2: offer not called");
            assert!(writer.to_string_lossy().contains(C2_MSG));
            assert!(get_log_history().iter().any(|e| e.message == C2_MSG));
        }

        // Case 3: Level 2 rejects + Level 3 accepts => formatted exactly once, offer called, history empty
        {
            let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let writer = SharedWriter(captured);
            let (logger, _, _) = build_logger_with_target("warn", Target::Pipe(Box::new(writer.clone())));
            let reloadable = ReloadableLogger::new(logger);
            let sink = Arc::new(SyslogSink::new(sender.clone(), Some(SyslogSink::build_filter(LevelFilter::Debug))));
            reloadable.syslog.store(Some(sink.clone()));

            let count = AtomicUsize::new(0);
            const C3_MSG: &str = "case_3_unique_msg";
            let msg = CountingMsg(&count, C3_MSG);
            let args = format_args!("{msg}");
            let record = Record::builder().level(Level::Info).target("case_3_target").args(args).build();
            reloadable.log(&record);

            assert_eq!(count.load(Ordering::SeqCst), 1, "case 3: formatted exactly once");
            assert_eq!(sink.offer_count(), 1, "case 3: offer called");
            assert!(!writer.to_string_lossy().contains(C3_MSG));
            assert!(!get_log_history().iter().any(|e| e.message == C3_MSG));
        }

        // Case 4: Both accept => formatted exactly once total, same string for datagram and ring entry
        {
            let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let writer = SharedWriter(captured);
            let (logger, _, _) = build_logger_with_target("debug", Target::Pipe(Box::new(writer.clone())));
            let reloadable = ReloadableLogger::new(logger);
            let sink = Arc::new(SyslogSink::new(sender.clone(), Some(SyslogSink::build_filter(LevelFilter::Debug))));
            reloadable.syslog.store(Some(sink.clone()));

            let count = AtomicUsize::new(0);
            const C4_MSG: &str = "case_4_unique_msg";
            let msg = CountingMsg(&count, C4_MSG);
            let args = format_args!("{msg}");
            let record = Record::builder().level(Level::Info).target("case_4_target").args(args).build();
            reloadable.log(&record);

            assert_eq!(count.load(Ordering::SeqCst), 1, "case 4: formatted exactly once");
            assert_eq!(sink.offer_count(), 1, "case 4: offer called");
            assert!(writer.to_string_lossy().contains(C4_MSG));
            assert!(get_log_history().iter().any(|e| e.message == C4_MSG));
        }

        sender.shutdown();
    }

    #[test]
    fn logging_test_15_recursion_guard() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(150))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = SharedWriter(captured);
        let (logger, _, _) = build_logger_with_target("debug", Target::Pipe(Box::new(writer.clone())));
        let reloadable = ReloadableLogger::new(logger);

        let dto = LogConfigDto {
            syslog: shared::model::SyslogConfigDto {
                enabled: true,
                host: "127.0.0.1".to_string(),
                port,
                log_level: Some(shared::model::LogLevel::Debug),
                ..Default::default()
            },
            ..Default::default()
        };

        let seams =
            crate::utils::syslog::Seams { report_interval: std::time::Duration::from_millis(50), ..Default::default() };

        let sink = build_syslog_sink_with_seams(Some(&dto), None, seams).expect("build").expect("some");
        reloadable.syslog.store(Some(sink.clone()));

        // In worker thread context (IN_SYSLOG_WORKER = true), log() must skip the sink
        const REPORT_MSG: &str = "logging_test_15_worker_report_msg";
        let report_args = format_args!("{REPORT_MSG}");
        let record = Record::builder()
            .level(Level::Warn)
            .target("tuliprox_core::utils::syslog::sender")
            .args(report_args)
            .build();

        {
            let _guard = crate::utils::syslog::SyslogWorkerGuard::enter();
            reloadable.log(&record);
        }

        // The summary appears in captured stdout and get_log_history()
        assert!(writer.to_string_lossy().contains(REPORT_MSG));
        assert!(get_log_history().iter().any(|e| e.message == REPORT_MSG));

        // Receiver gets NO report datagram
        let mut buf = [0u8; 1024];
        assert!(rx_sock.recv_from(&mut buf).is_err(), "receiver must get no report datagram");

        sink.sender.clone().shutdown();
    }

    /// The guard is only useful if the real worker sets it, so this drives the actual worker loop: it produces a
    /// counter change, waits for the scaled report deadline, and asserts the guard was active on that thread.
    #[test]
    fn logging_test_15b_worker_thread_sets_the_guard() {
        let rx_sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        rx_sock.set_read_timeout(Some(std::time::Duration::from_millis(150))).expect("timeout");
        let port = rx_sock.local_addr().expect("local addr").port();

        let dto = LogConfigDto {
            syslog: shared::model::SyslogConfigDto {
                enabled: true,
                host: "127.0.0.1".to_string(),
                port,
                log_level: Some(shared::model::LogLevel::Debug),
                ..Default::default()
            },
            ..Default::default()
        };
        crate::utils::syslog::sender::probe::reset_worker_guard();
        let seams = crate::utils::syslog::Seams {
            report_interval: std::time::Duration::from_millis(30),
            send: crate::utils::syslog::sender::probe::send,
            ..Default::default()
        };
        let sink = build_syslog_sink_with_seams(Some(&dto), None, seams).expect("build").expect("some");

        // A record long enough to be truncated changes a counter, which is what makes the worker report.
        let long = "x".repeat(4096);
        let now = Local::now();
        for _ in 0..5 {
            sink.offer(now, Level::Info, "guard_probe_target", &long);
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while sink.sender.stats.reports.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "worker never reported");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            crate::utils::syslog::sender::probe::saw_worker_guard(),
            "the worker thread must enter IN_SYSLOG_WORKER before it reports"
        );

        sink.sender.clone().shutdown();
    }

    // Test 16 is the single global wiring test across the test suite that touches the global logger state.
    #[test]
    fn logging_test_16_single_global_wiring_test() {
        let mut file = tempfile::NamedTempFile::new().expect("create tempfile");
        let yaml = r#"
log:
  log_level: info
  syslog:
    enabled: true
    host: "127.0.0.1"
    port: 514
    log_level: debug
"#;
        std::io::Write::write_all(&mut file, yaml.as_bytes()).expect("write yaml");
        let file_path = file.path().to_str().expect("valid path");

        init_logger(None, file_path);

        // log::max_level() equals compute_max_level(resolved_root, sink_level)
        assert_eq!(log::max_level(), compute_max_level(LevelFilter::Info, LevelFilter::Debug));

        let handle = LOGGER_HANDLE.get().expect("logger handle must be set");
        let sink1 = handle.syslog.load_full().expect("sink must be present");
        let sender1 = Arc::clone(&sink1.sender);

        // Reload with same transport keeps Arc::ptr_eq
        let dto_same = LogConfigDto {
            log_level: Some("warn".to_string()),
            syslog: shared::model::SyslogConfigDto {
                enabled: true,
                host: "127.0.0.1".to_string(),
                port: 514,
                log_level: Some(shared::model::LogLevel::Debug),
                ..shared::model::SyslogConfigDto::default()
            },
            ..LogConfigDto::default()
        };

        reload_logger(Some(&dto_same));

        let sink2 = handle.syslog.load_full().expect("sink must be present");
        assert!(Arc::ptr_eq(&sender1, &sink2.sender), "same transport must reuse sender");
        assert_eq!(log::max_level(), compute_max_level(LevelFilter::Warn, LevelFilter::Debug));

        // Transport change replaces the sender
        let mut dto_diff = dto_same.clone();
        dto_diff.syslog.port = 515;
        reload_logger(Some(&dto_diff));

        let sink3 = handle.syslog.load_full().expect("sink must be present");
        assert!(!Arc::ptr_eq(&sender1, &sink3.sender), "transport change must replace sender");

        flush_syslog();
        assert!(handle.syslog.load().is_none(), "flush_syslog must remove sink");
    }

    #[test]
    #[ignore = "benchmark run manually with --nocapture"]
    #[allow(clippy::cast_precision_loss)]
    fn syslog_hot_path_cost() {
        use std::time::Instant;

        let (logger, _, _) = build_logger_with_target("info", Target::Pipe(Box::new(std::io::sink())));
        let reloadable = ReloadableLogger::new(logger);

        let record = Record::builder()
            .level(Level::Info)
            .target("hot_path_target")
            .args(format_args!("hot path benchmark message"))
            .build();

        const ITERATIONS: u64 = 1_000_000;

        // Warm-up
        for _ in 0..10_000 {
            reloadable.log(&record);
        }

        // (a) No sink, root info
        let start_a = Instant::now();
        for _ in 0..ITERATIONS {
            reloadable.log(&record);
        }
        let elapsed_a = start_a.elapsed();
        let ns_per_rec_a = elapsed_a.as_nanos() as f64 / ITERATIONS as f64;

        // (b) Sink present, filter info
        let config = crate::model::SyslogConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 514,
            queue_capacity: 65536,
            ..crate::model::SyslogConfig::default()
        };
        let sender = SyslogSender::start(&config).expect("sender start");
        let filter = SyslogSink::build_filter(LevelFilter::Info);
        let sink = Arc::new(SyslogSink::new(sender, Some(filter)));
        reloadable.syslog.store(Some(sink.clone()));

        // Warm-up
        for _ in 0..10_000 {
            reloadable.log(&record);
        }

        let start_b = Instant::now();
        for _ in 0..ITERATIONS {
            reloadable.log(&record);
        }
        let elapsed_b = start_b.elapsed();
        let ns_per_rec_b = elapsed_b.as_nanos() as f64 / ITERATIONS as f64;

        sink.sender.clone().shutdown();

        println!("\n=== Hot-path measurement ===");
        println!("(a) No sink, root info:        {ns_per_rec_a:.2} ns/record ({elapsed_a:?} total)");
        println!("(b) Sink present, filter info: {ns_per_rec_b:.2} ns/record ({elapsed_b:?} total)");
    }
}
