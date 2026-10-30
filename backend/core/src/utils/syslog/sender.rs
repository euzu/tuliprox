use super::{
    formatter::{format, sanitize_header_field, SyslogHeader, SyslogMeta},
    hostname::system_hostname,
};
use crate::model::SyslogConfig;
use chrono::{DateTime, Local};
use log::{warn, Level};
use shared::{
    defaults::default_syslog_app_name,
    model::{SyslogFacility, SyslogFormat},
};
use std::{
    io,
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const SYSLOG_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const REPORT_INTERVAL: Duration = Duration::from_secs(300);
const DNS_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct SyslogStats {
    pub dropped_full: AtomicU64,
    pub dropped_unresolved: AtomicU64,
    pub dropped_send_error: AtomicU64,
    pub truncated: AtomicU64,
    pub sent: AtomicU64,
    pub reports: AtomicU64,
}

#[derive(Debug)]
pub(crate) enum WorkerMsg {
    Datagram(String),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyslogTransport {
    pub host: String,
    pub port: u16,
    pub format: SyslogFormat,
    pub facility: SyslogFacility,
    pub app_name: String,
    pub hostname: Option<String>,
    pub queue_capacity: u32,
    pub max_message_len: u16,
}

impl SyslogTransport {
    #[must_use]
    pub(crate) fn from_config(config: &SyslogConfig) -> Self {
        let app_name = sanitize_header_field(&config.app_name, 48);
        let app_name = if app_name == "-" { default_syslog_app_name() } else { app_name };
        let hostname =
            config.hostname.as_deref().map(|h| sanitize_header_field(h, 255)).filter(|h| !h.is_empty() && h != "-");
        Self {
            host: config.host.clone(),
            port: config.port,
            format: config.format,
            facility: config.facility,
            app_name,
            hostname,
            queue_capacity: config.queue_capacity,
            max_message_len: config.max_message_len,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Seams {
    pub resolve: fn(&str, u16) -> io::Result<Vec<SocketAddr>>,
    pub send: fn(&UdpSocket, &[u8], SocketAddr) -> io::Result<usize>,
    pub report_interval: Duration,
    pub dns_retry_interval: Duration,
}

fn default_resolve(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    (host, port).to_socket_addrs().map(Iterator::collect)
}

fn default_send(socket: &UdpSocket, buf: &[u8], target: SocketAddr) -> io::Result<usize> { socket.send_to(buf, target) }

impl Default for Seams {
    fn default() -> Self {
        Self {
            resolve: default_resolve,
            send: default_send,
            report_interval: REPORT_INTERVAL,
            dns_retry_interval: DNS_RETRY_INTERVAL,
        }
    }
}

pub(crate) struct SyslogSender {
    tx: SyncSender<WorkerMsg>,
    header: SyslogHeader,
    transport: SyslogTransport,
    pub(crate) stats: Arc<SyslogStats>,
    done_rx: parking_lot::Mutex<Option<Receiver<()>>>,
}

#[cfg(test)]
static TEST_SKIP_FIRST_BIND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn try_bind_candidate(addr: SocketAddr) -> Option<(UdpSocket, SocketAddr)> {
    #[cfg(test)]
    if TEST_SKIP_FIRST_BIND.swap(false, Ordering::SeqCst) {
        return None;
    }
    let bind_addr = match addr {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    UdpSocket::bind(bind_addr).ok().map(|sock| (sock, addr))
}

impl SyslogSender {
    pub(crate) fn start(config: &SyslogConfig) -> Result<Arc<Self>, String> {
        Self::start_internal(config, Seams::default())
    }

    #[cfg(test)]
    pub(crate) fn start_with_seams(config: &SyslogConfig, seams: Seams) -> Result<Arc<Self>, String> {
        Self::start_internal(config, seams)
    }

    fn start_internal(config: &SyslogConfig, seams: Seams) -> Result<Arc<Self>, String> {
        let transport = SyslogTransport::from_config(config);
        let header_hostname = match transport.hostname.as_deref() {
            Some(h) if !h.is_empty() && h != "-" => h.to_string(),
            _ => system_hostname()
                .map(|h| sanitize_header_field(&h, 255))
                .filter(|h| !h.is_empty() && h != "-")
                .unwrap_or_else(|| "-".to_string()),
        };
        let header = SyslogHeader {
            format: transport.format,
            facility: transport.facility,
            app_name: transport.app_name.clone(),
            hostname: header_hostname,
            pid: std::process::id(),
            max_len: transport.max_message_len as usize,
        };

        let capacity = (transport.queue_capacity as usize).max(1);
        let (tx, rx) = sync_channel(capacity);
        let (done_tx, done_rx) = sync_channel(1);
        let stats = Arc::new(SyslogStats::default());

        let host = transport.host.clone();
        let port = transport.port;
        let worker_stats = Arc::clone(&stats);

        thread::Builder::new()
            .name("tuliprox-syslog".to_string())
            .spawn(move || {
                let worker = WorkerContext { rx, done_tx, host, port, stats: worker_stats, seams };
                worker.run();
            })
            .map_err(|err| format!("Failed to spawn syslog worker thread: {err}"))?;

        Ok(Arc::new(Self { tx, header, transport, stats, done_rx: parking_lot::Mutex::new(Some(done_rx)) }))
    }

    pub(crate) fn offer(&self, now: DateTime<Local>, level: Level, target: &str, message: &str) {
        let meta = SyslogMeta { timestamp: now, level, target, message };
        let (datagram, truncated) = format(&meta, &self.header);
        if truncated {
            self.stats.truncated.fetch_add(1, Ordering::Relaxed);
        }
        match self.tx.try_send(WorkerMsg::Datagram(datagram)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped_send_error.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[must_use]
    pub(crate) fn same_transport(&self, transport: &SyslogTransport) -> bool { self.transport == *transport }

    /// Asks the worker to stop after the datagrams already queued, without waiting for it.
    pub(crate) fn stop(&self) { let _ = self.tx.try_send(WorkerMsg::Shutdown); }

    /// Stops the worker and waits up to `SYSLOG_SHUTDOWN_TIMEOUT` for it to drain the queue.
    pub(crate) fn shutdown(self: Arc<Self>) {
        let done_rx = self.done_rx.lock().take();
        self.stop();
        drop(self);
        if let Some(rx) = done_rx {
            let _ = rx.recv_timeout(SYSLOG_SHUTDOWN_TIMEOUT);
        }
    }
}

struct WorkerReportState {
    last_dropped_full: u64,
    last_dropped_unresolved: u64,
    last_dropped_send_error: u64,
    last_truncated: u64,
    next_report: Instant,
}

/// The address the worker sends to, plus the resolved addresses after it for failover.
struct Target {
    socket: UdpSocket,
    addr: SocketAddr,
    remaining: std::vec::IntoIter<SocketAddr>,
    /// Set once a datagram was sent since the last resolve.
    delivered: bool,
}

impl Target {
    /// Moves to the next resolved address that binds. Returns `false` when none is left.
    fn fail_over(&mut self) -> bool {
        match self.remaining.find_map(try_bind_candidate) {
            Some((socket, addr)) => {
                self.socket = socket;
                self.addr = addr;
                true
            }
            None => false,
        }
    }
}

struct WorkerContext {
    rx: Receiver<WorkerMsg>,
    done_tx: SyncSender<()>,
    host: String,
    port: u16,
    stats: Arc<SyslogStats>,
    seams: Seams,
}

impl WorkerContext {
    fn resolve_and_bind(&self) -> Option<Target> {
        let mut remaining = (self.seams.resolve)(&self.host, self.port).ok()?.into_iter();
        let (socket, addr) = remaining.find_map(try_bind_candidate)?;
        Some(Target { socket, addr, remaining, delivered: false })
    }

    fn maybe_report(&self, state: &mut WorkerReportState) {
        let curr_full = self.stats.dropped_full.load(Ordering::Relaxed);
        let curr_unres = self.stats.dropped_unresolved.load(Ordering::Relaxed);
        let curr_send_err = self.stats.dropped_send_error.load(Ordering::Relaxed);
        let curr_trunc = self.stats.truncated.load(Ordering::Relaxed);
        let curr_sent = self.stats.sent.load(Ordering::Relaxed);

        let changed = curr_full != state.last_dropped_full
            || curr_unres != state.last_dropped_unresolved
            || curr_send_err != state.last_dropped_send_error
            || curr_trunc != state.last_truncated;

        if changed {
            let host = &self.host;
            let port = self.port;
            warn!(
                "syslog worker report for {host}:{port}: dropped_full={curr_full}, dropped_unresolved={curr_unres}, dropped_send_error={curr_send_err}, truncated={curr_trunc}, sent={curr_sent}"
            );
            self.stats.reports.fetch_add(1, Ordering::Relaxed);
            state.last_dropped_full = curr_full;
            state.last_dropped_unresolved = curr_unres;
            state.last_dropped_send_error = curr_send_err;
            state.last_truncated = curr_trunc;
        }
        state.next_report = Instant::now() + self.seams.report_interval;
    }

    fn drain_unresolved(&self) -> bool {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Datagram(_) => {
                    self.stats.dropped_unresolved.fetch_add(1, Ordering::Relaxed);
                }
                WorkerMsg::Shutdown => return true,
            }
        }
        false
    }

    fn run(self) {
        let _guard = crate::utils::syslog::SyslogWorkerGuard::enter();

        let mut report_state = WorkerReportState {
            last_dropped_full: 0,
            last_dropped_unresolved: 0,
            last_dropped_send_error: 0,
            last_truncated: 0,
            next_report: Instant::now() + self.seams.report_interval,
        };

        let mut current_target: Option<Target> = None;
        let mut next_retry = Instant::now();

        'worker: loop {
            while current_target.is_none() {
                let now = Instant::now();
                if now >= next_retry {
                    current_target = self.resolve_and_bind();
                    if current_target.is_some() {
                        break;
                    }
                    next_retry = Instant::now() + self.seams.dns_retry_interval;
                }

                let now = Instant::now();
                let wait_dur = report_state.next_report.min(next_retry).saturating_duration_since(now);
                match self.rx.recv_timeout(wait_dur) {
                    Ok(WorkerMsg::Datagram(_)) => {
                        self.stats.dropped_unresolved.fetch_add(1, Ordering::Relaxed);
                        if self.drain_unresolved() {
                            break 'worker;
                        }
                    }
                    Ok(WorkerMsg::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break 'worker;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }

                if Instant::now() >= report_state.next_report {
                    self.maybe_report(&mut report_state);
                }
            }

            let Some(target) = current_target.as_mut() else {
                continue;
            };

            let now = Instant::now();
            let wait_dur = report_state.next_report.saturating_duration_since(now);
            match self.rx.recv_timeout(wait_dur) {
                Ok(WorkerMsg::Datagram(d)) => {
                    match (self.seams.send)(&target.socket, d.as_bytes(), target.addr) {
                        Ok(_) => {
                            self.stats.sent.fetch_add(1, Ordering::Relaxed);
                            target.delivered = true;
                        }
                        Err(e) => {
                            self.stats.dropped_send_error.fetch_add(1, Ordering::Relaxed);
                            let address_error = matches!(
                                e.kind(),
                                io::ErrorKind::NetworkUnreachable
                                    | io::ErrorKind::HostUnreachable
                                    | io::ErrorKind::InvalidInput
                            );
                            if address_error && !target.fail_over() {
                                // A collector that worked before may have moved, so re-resolve at once. When no
                                // resolved address ever worked, wait for the retry interval instead of running a
                                // DNS lookup per record.
                                next_retry = if target.delivered {
                                    Instant::now()
                                } else {
                                    Instant::now() + self.seams.dns_retry_interval
                                };
                                current_target = None;
                            }
                        }
                    }
                    if Instant::now() >= report_state.next_report {
                        self.maybe_report(&mut report_state);
                    }
                }
                Ok(WorkerMsg::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break 'worker;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.maybe_report(&mut report_state);
                }
            }
        }

        let _ = self.done_tx.send(());
    }
}

/// Test-only observation points that must run on the worker thread. `Seams` holds plain function pointers, so a
/// probe cannot capture state; it records into a module static instead.
#[cfg(test)]
pub(crate) mod probe {
    use super::{io, Ordering, SocketAddr, UdpSocket};
    use std::sync::atomic::AtomicBool;

    static SAW_WORKER_GUARD: AtomicBool = AtomicBool::new(false);

    pub(crate) fn send(socket: &UdpSocket, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        if crate::utils::syslog::in_syslog_worker() {
            SAW_WORKER_GUARD.store(true, Ordering::SeqCst);
        }
        socket.send_to(buf, target)
    }

    pub(crate) fn saw_worker_guard() -> bool { SAW_WORKER_GUARD.load(Ordering::SeqCst) }

    pub(crate) fn reset_worker_guard() { SAW_WORKER_GUARD.store(false, Ordering::SeqCst); }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::all)]
    use super::*;
    use parking_lot::Mutex;
    use shared::model::LogLevel;
    use std::{
        net::UdpSocket,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static BLOCK_SEND: AtomicBool = AtomicBool::new(false);
    static TEST_RESOLVER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TEST_RESOLVER_ANSWERS: Mutex<Vec<Vec<SocketAddr>>> = Mutex::new(Vec::new());
    static TEST_SEND_HOST_UNREACHABLE_ONCE: AtomicBool = AtomicBool::new(false);
    static TEST_UNREACHABLE_ADDR: Mutex<Option<SocketAddr>> = Mutex::new(None);
    static TEST_SEND_CALLS: Mutex<Vec<SocketAddr>> = Mutex::new(Vec::new());
    static TEST_SEND_SAW_WORKER_GUARD: AtomicBool = AtomicBool::new(false);

    fn test_resolve(_host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        TEST_RESOLVER_CALLS.fetch_add(1, Ordering::SeqCst);
        let mut answers = TEST_RESOLVER_ANSWERS.lock();
        if answers.is_empty() {
            Err(io::Error::new(io::ErrorKind::NotFound, "no answer configured"))
        } else {
            Ok(answers.remove(0))
        }
    }

    fn test_send(socket: &UdpSocket, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        // The send seam is the only place test code runs on the worker thread, so it is where the recursion guard
        // has to be observed: without it a `log::*` call from the worker could re-enter the sink.
        if crate::utils::syslog::in_syslog_worker() {
            TEST_SEND_SAW_WORKER_GUARD.store(true, Ordering::SeqCst);
        }
        while BLOCK_SEND.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
        }
        if TEST_SEND_HOST_UNREACHABLE_ONCE.swap(false, Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::HostUnreachable, "host unreachable"));
        }
        if *TEST_UNREACHABLE_ADDR.lock() == Some(target) {
            return Err(io::Error::new(io::ErrorKind::NetworkUnreachable, "network unreachable"));
        }
        TEST_SEND_CALLS.lock().push(target);
        socket.send_to(buf, target)
    }

    #[test]
    fn test_1_receiver_gets_datagram() {
        let _guard = TEST_LOCK.lock();
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = receiver.local_addr().unwrap().port();

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = port;

        let sender = SyslogSender::start(&config).expect("start sender");
        sender.offer(Local::now(), Level::Info, "test_target", "hello world");

        let mut buf = [0u8; 1024];
        let (len, _) = receiver.recv_from(&mut buf).expect("must receive datagram");
        let msg = String::from_utf8_lossy(&buf[..len]);
        assert!(msg.contains("hello world"));
        sender.shutdown();
    }

    #[test]
    fn test_2_capacity_1_blocked_worker_dropped_full() {
        let _guard = TEST_LOCK.lock();
        BLOCK_SEND.store(true, Ordering::SeqCst);

        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let port = receiver.local_addr().unwrap().port();

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = port;
        config.queue_capacity = 1;

        let mut seams = Seams::default();
        seams.send = test_send;

        let sender = SyslogSender::start_with_seams(&config, seams).expect("start sender");

        sender.offer(Local::now(), Level::Info, "t", "msg 1");
        thread::sleep(Duration::from_millis(50));

        sender.offer(Local::now(), Level::Info, "t", "msg 2");

        let start = Instant::now();
        sender.offer(Local::now(), Level::Info, "t", "msg 3");
        assert!(start.elapsed() < Duration::from_millis(50));
        assert!(sender.stats.dropped_full.load(Ordering::Relaxed) >= 1);

        BLOCK_SEND.store(false, Ordering::SeqCst);
        sender.shutdown();
        thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn test_3_unresolvable_host_and_multi_address_selection() {
        let _guard = TEST_LOCK.lock();
        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "unresolvable.example.invalid".to_string();
        config.port = 514;

        let mut seams = Seams::default();
        seams.resolve = |_h, _p| Err(io::Error::new(io::ErrorKind::NotFound, "unresolvable"));
        seams.dns_retry_interval = Duration::from_millis(100);

        let start = Instant::now();
        let sender = SyslogSender::start_with_seams(&config, seams).expect("start must return Ok");
        assert!(start.elapsed() < Duration::from_millis(200));

        // Failed retries without a record are not losses: the counter must stay at zero while idle.
        thread::sleep(Duration::from_millis(250));
        assert_eq!(sender.stats.dropped_unresolved.load(Ordering::Relaxed), 0);

        sender.offer(Local::now(), Level::Info, "t", "lost while unresolved");
        let deadline = Instant::now() + Duration::from_secs(1);
        while sender.stats.dropped_unresolved.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "dropped_unresolved timed out");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sender.stats.dropped_unresolved.load(Ordering::Relaxed), 1);
        sender.shutdown();

        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let valid_port = receiver.local_addr().unwrap().port();
        let valid_addr: SocketAddr = format!("127.0.0.1:{valid_port}").parse().unwrap();
        let dummy_addr: SocketAddr = "127.0.0.1:5140".parse().unwrap();

        TEST_SKIP_FIRST_BIND.store(true, Ordering::SeqCst);
        *TEST_RESOLVER_ANSWERS.lock() = vec![vec![dummy_addr, valid_addr]];

        let mut seams_multi = Seams::default();
        seams_multi.resolve = test_resolve;
        let sender_multi = SyslogSender::start_with_seams(&config, seams_multi).expect("start sender multi");
        sender_multi.offer(Local::now(), Level::Info, "t", "msg multi");

        let mut buf = [0u8; 1024];
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (len, _) = receiver.recv_from(&mut buf).expect("must receive on fallback address");
        assert!(String::from_utf8_lossy(&buf[..len]).contains("msg multi"));
        sender_multi.shutdown();
    }

    #[test]
    fn test_4_shutdown_normal_case_delivery_and_deadline() {
        let _guard = TEST_LOCK.lock();
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = receiver.local_addr().unwrap().port();

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = port;
        config.queue_capacity = 32;

        let sender = SyslogSender::start(&config).expect("start sender");
        for i in 0..5 {
            sender.offer(Local::now(), Level::Info, "t", &format!("msg {i}"));
        }

        let start = Instant::now();
        sender.shutdown();
        assert!(start.elapsed() < SYSLOG_SHUTDOWN_TIMEOUT);

        let mut received_datagrams = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok((len, _)) = receiver.recv_from(&mut buf) {
            received_datagrams.push(String::from_utf8_lossy(&buf[..len]).to_string());
            if received_datagrams.len() == 5 {
                break;
            }
        }
        assert_eq!(received_datagrams.len(), 5);
        for i in 0..5 {
            assert!(received_datagrams.iter().any(|m| m.contains(&format!("msg {i}"))));
        }
    }

    #[test]
    fn test_5_shutdown_blocked_collector_returns_within_deadline() {
        let _guard = TEST_LOCK.lock();
        BLOCK_SEND.store(true, Ordering::SeqCst);

        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let port = receiver.local_addr().unwrap().port();

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = port;
        config.queue_capacity = 1;

        let mut seams = Seams::default();
        seams.send = test_send;

        let sender = SyslogSender::start_with_seams(&config, seams).expect("start sender");

        sender.offer(Local::now(), Level::Info, "t", "msg 1");
        thread::sleep(Duration::from_millis(50));

        // The worker holds msg 1 in a blocked send and the capacity-1 queue holds msg 2, so this one is dropped.
        sender.offer(Local::now(), Level::Info, "t", "msg 2");
        sender.offer(Local::now(), Level::Info, "t", "msg 3");

        let stats = Arc::clone(&sender.stats);
        let start = Instant::now();
        sender.shutdown();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= SYSLOG_SHUTDOWN_TIMEOUT && elapsed < SYSLOG_SHUTDOWN_TIMEOUT + Duration::from_secs(2),
            "shutdown must honour the deadline, took {elapsed:?}"
        );
        // The saturated queue is the documented best-effort loss path, so the loss has to be observable.
        assert!(stats.dropped_full.load(Ordering::Relaxed) >= 1);

        BLOCK_SEND.store(false, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn test_6_no_periodic_re_resolution_and_stale_re_resolve() {
        let _guard = TEST_LOCK.lock();
        TEST_RESOLVER_CALLS.store(0, Ordering::SeqCst);
        TEST_SEND_CALLS.lock().clear();
        TEST_SEND_HOST_UNREACHABLE_ONCE.store(false, Ordering::SeqCst);

        let addr1: SocketAddr = "127.0.0.1:5141".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.2:5142".parse().unwrap();
        *TEST_RESOLVER_ANSWERS.lock() = vec![vec![addr1], vec![addr2]];

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "collector.example".to_string();
        config.port = 514;
        config.queue_capacity = 32;

        let mut seams = Seams::default();
        seams.resolve = test_resolve;
        seams.send = test_send;
        seams.dns_retry_interval = Duration::from_millis(50);

        let sender = SyslogSender::start_with_seams(&config, seams).expect("start sender");

        thread::sleep(Duration::from_millis(50));
        assert_eq!(TEST_RESOLVER_CALLS.load(Ordering::SeqCst), 1);

        for i in 0..5 {
            sender.offer(Local::now(), Level::Info, "t", &format!("send {i}"));
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(TEST_RESOLVER_CALLS.load(Ordering::SeqCst), 1);
        {
            let calls = TEST_SEND_CALLS.lock();
            assert_eq!(calls.len(), 5);
            for target in calls.iter() {
                assert_eq!(*target, addr1);
            }
        }

        TEST_SEND_HOST_UNREACHABLE_ONCE.store(true, Ordering::SeqCst);
        sender.offer(Local::now(), Level::Info, "t", "trigger error");

        let deadline = Instant::now() + Duration::from_secs(1);
        while TEST_RESOLVER_CALLS.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "re-resolution timed out");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(TEST_RESOLVER_CALLS.load(Ordering::SeqCst), 2);

        sender.offer(Local::now(), Level::Info, "t", "send to new addr");
        thread::sleep(Duration::from_millis(50));
        {
            let calls = TEST_SEND_CALLS.lock();
            assert!(calls.contains(&addr2));
        }

        sender.shutdown();
    }

    #[test]
    fn test_6b_unreachable_address_fails_over_to_next_resolved_address() {
        let _guard = TEST_LOCK.lock();
        TEST_RESOLVER_CALLS.store(0, Ordering::SeqCst);
        TEST_SEND_CALLS.lock().clear();

        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let good_addr = receiver.local_addr().unwrap();
        let bad_addr: SocketAddr = "127.0.0.1:5143".parse().unwrap();
        *TEST_UNREACHABLE_ADDR.lock() = Some(bad_addr);
        *TEST_RESOLVER_ANSWERS.lock() = vec![vec![bad_addr, good_addr]];

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "collector.example".to_string();
        config.port = 514;

        let mut seams = Seams::default();
        seams.resolve = test_resolve;
        seams.send = test_send;

        let sender = SyslogSender::start_with_seams(&config, seams).expect("start sender");
        sender.offer(Local::now(), Level::Info, "t", "lost on first address");
        sender.offer(Local::now(), Level::Info, "t", "delivered on second address");

        let mut buf = [0u8; 1024];
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (len, _) = receiver.recv_from(&mut buf).expect("must receive on the next resolved address");
        assert!(String::from_utf8_lossy(&buf[..len]).contains("delivered on second address"));
        assert_eq!(TEST_RESOLVER_CALLS.load(Ordering::SeqCst), 1, "failover must not re-resolve");
        assert_eq!(*TEST_SEND_CALLS.lock(), vec![good_addr]);

        sender.shutdown();
        *TEST_UNREACHABLE_ADDR.lock() = None;
    }

    #[test]
    fn test_7_same_transport() {
        let _guard = TEST_LOCK.lock();
        let mut cfg1 = SyslogConfig::default();
        cfg1.enabled = true;
        cfg1.host = "collector.example".to_string();
        cfg1.port = 514;
        cfg1.log_level = Some(LogLevel::Info);

        let mut cfg2 = cfg1.clone();
        cfg2.log_level = Some(LogLevel::Debug);

        let t1 = SyslogTransport::from_config(&cfg1);
        let t2 = SyslogTransport::from_config(&cfg2);
        assert_eq!(t1, t2);

        let sender = SyslogSender::start(&cfg1).expect("start sender");
        assert!(sender.same_transport(&t2));

        let mut cfg3 = cfg1.clone();
        cfg3.port = 515;
        let t3 = SyslogTransport::from_config(&cfg3);
        assert_ne!(t1, t3);
        assert!(!sender.same_transport(&t3));

        sender.shutdown();
    }

    #[test]
    fn test_8_system_hostname() {
        let name_opt = system_hostname();
        if let Some(name) = name_opt {
            assert_ne!(name, "");
        }
        let meta = SyslogMeta { timestamp: Local::now(), level: Level::Info, target: "t", message: "msg" };
        let header = SyslogHeader {
            format: SyslogFormat::Rfc5424,
            facility: SyslogFacility::Local0,
            app_name: "tuliprox".to_string(),
            hostname: String::new(),
            pid: 1,
            max_len: 1024,
        };
        let (out, _) = format(&meta, &header);
        assert!(out.contains(" - tuliprox 1 t - msg"));
    }

    #[test]
    fn test_9_report_under_continuous_load() {
        let _guard = TEST_LOCK.lock();
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let port = receiver.local_addr().unwrap().port();

        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = port;
        config.queue_capacity = 2048;
        config.max_message_len = 480;

        TEST_SEND_SAW_WORKER_GUARD.store(false, Ordering::SeqCst);
        let mut seams = Seams::default();
        seams.send = test_send;
        seams.report_interval = Duration::from_millis(50);

        let sender = SyslogSender::start_with_seams(&config, seams).expect("start sender");

        let start = Instant::now();
        let mut count = 0;
        let long_msg = "x".repeat(600);
        while start.elapsed() < Duration::from_millis(150) {
            sender.offer(Local::now(), Level::Info, "load_target", &long_msg);
            count += 1;
            thread::sleep(Duration::from_millis(2));
        }
        assert!(count > 10);
        assert!(sender.stats.truncated.load(Ordering::Relaxed) > 0);

        let deadline = Instant::now() + Duration::from_secs(1);
        while sender.stats.reports.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "report timed out");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(sender.stats.reports.load(Ordering::Relaxed) >= 1);
        assert!(
            TEST_SEND_SAW_WORKER_GUARD.load(Ordering::SeqCst),
            "the worker thread must set IN_SYSLOG_WORKER before it sends or reports"
        );

        sender.shutdown();
    }

    #[test]
    fn test_10_idle_shutdown_immediate() {
        let _guard = TEST_LOCK.lock();
        let mut config = SyslogConfig::default();
        config.enabled = true;
        config.host = "127.0.0.1".to_string();
        config.port = 514;

        let sender = SyslogSender::start(&config).expect("start sender");
        let extra_arc = Arc::clone(&sender);

        let start = Instant::now();
        sender.shutdown();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(500), "Shutdown took {elapsed:?} >= 500ms");

        drop(extra_arc);
    }
}
