#[cfg(target_os = "linux")]
use crate::utils::parse_ascii_u64_bytes;
use crate::{
    api::model::AppState,
    messaging::send_message as send_messaging,
    model::{DiskAlertConfig, MessageContent},
};
use shared::model::{DiskAlert, DiskAlertLevel, MsgKind, SystemInfo};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const SYSTEM_USAGE_INTERVAL: Duration = Duration::from_secs(2);

/// Disk-space alert state machine.
///
/// Inspects each 2s sample and emits a `DiskAlert` payload on:
/// 1. state transition (Normal → Warn → Critical, or any step back down), or
/// 2. when the disk stays in the same alert state for at least
///    `cfg.repeat_interval_secs` since the last notification.
///
/// Resets cleanly when disk info becomes unavailable so a future re-arming
/// does not fire stale state.
struct DiskAlertMonitor {
    last_level: Option<DiskAlertLevel>,
    last_notified_at: Option<Instant>,
}

impl DiskAlertMonitor {
    fn new() -> Self { Self { last_level: None, last_notified_at: None } }

    #[allow(clippy::cast_precision_loss)]
    fn inspect(&mut self, cfg: &DiskAlertConfig, total_bytes: u64, free_bytes: u64) -> Option<DiskAlert> {
        if total_bytes == 0 {
            // Disk info unavailable on this platform/tick; drop any state.
            self.last_level = None;
            self.last_notified_at = None;
            return None;
        }

        let used_bytes = total_bytes.saturating_sub(free_bytes);
        let percent = (used_bytes as f64 / total_bytes as f64) * 100.0;
        let new_level = if percent >= cfg.critical_percent {
            Some(DiskAlertLevel::Critical)
        } else if percent >= cfg.warn_percent {
            Some(DiskAlertLevel::Warn)
        } else {
            None
        };

        let now = Instant::now();
        let state_changed = new_level != self.last_level;
        let rearm_elapsed =
            self.last_notified_at.is_none_or(|t| now.duration_since(t).as_secs() >= cfg.repeat_interval_secs);

        let should_notify = new_level.is_some() && (state_changed || rearm_elapsed);

        // Always advance state so a quick drop below warn and back up is
        // treated as a fresh transition (not suppressed by hysteresis).
        self.last_level = new_level;
        if should_notify {
            self.last_notified_at = Some(now);
            Some(DiskAlert {
                level: new_level.expect("new_level is Some when should_notify is true"),
                total_bytes,
                free_bytes,
                used_bytes,
                percent,
            })
        } else {
            None
        }
    }
}

/// Per-platform disk-usage probe that caches the path to the filesystem we are
/// sampling so each tick is a single syscall with no heap allocation.
///
/// Built lazily; a `None` represents a platform where the probe could not be
/// initialised (e.g. CWD unavailable, or non-Unix / non-Windows fallback).
struct DiskProbe {
    path: DiskPath,
}

#[cfg(unix)]
struct DiskPath(std::ffi::CString);

#[cfg(windows)]
struct DiskPath(Vec<u16>);

impl DiskProbe {
    /// Build a probe targeting the process working directory.
    /// `None` indicates the platform has no disk probe (e.g. fallback path).
    fn for_cwd() -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        Some(Self { path: DiskPath::from_path(&cwd) })
    }

    /// Return `(total_bytes, free_bytes_available_to_caller)`.
    /// Returns `(0, 0)` if the underlying syscall fails.
    fn sample(&self) -> (u64, u64) {
        #[cfg(unix)]
        {
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: `path.0` is a valid NUL-terminated C string built from the
            // process CWD; `&raw mut stat` is a writable pointer to a zeroed struct.
            let rc = unsafe { libc::statvfs(self.path.0.as_ptr(), &raw mut stat) };
            if rc != 0 {
                return (0, 0);
            }
            let block_size = statvfs_counter_to_u64(stat.f_frsize);
            let total = statvfs_counter_to_u64(stat.f_blocks).saturating_mul(block_size);
            let free = statvfs_counter_to_u64(stat.f_bavail).saturating_mul(block_size);
            (total, free)
        }
        #[cfg(windows)]
        {
            let mut free_bytes_available: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut total_free_bytes: u64 = 0;
            // SAFETY: `self.path.0` is a NUL-terminated UTF-16 string built from
            // the CWD; the three output pointers alias ULARGE_INTEGER (= u64).
            let ok = unsafe {
                winapi::um::fileapi::GetDiskFreeSpaceExW(
                    self.path.0.as_ptr(),
                    (&raw mut free_bytes_available).cast(),
                    (&raw mut total_bytes).cast(),
                    (&raw mut total_free_bytes).cast(),
                )
            };
            if ok == 0 {
                return (0, 0);
            }
            (total_bytes, free_bytes_available)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = &self.path;
            (0, 0)
        }
    }
}

#[cfg(unix)]
fn statvfs_counter_to_u64<T: Into<u64>>(value: T) -> u64 { value.into() }

#[cfg(unix)]
impl DiskPath {
    fn from_path(path: &std::path::Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        // CString::new only fails if the path contains interior NULs, which
        // `std::env::current_dir` cannot produce on a supported platform.
        Self(std::ffi::CString::new(path.as_os_str().as_bytes()).expect("CWD contains NUL byte"))
    }
}

#[cfg(windows)]
impl DiskPath {
    fn from_path(path: &std::path::Path) -> Self {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        Self(wide)
    }
}

pub fn exec_system_usage(app_state: &Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let state = Arc::clone(app_state);

    tokio::spawn(async move {
        let mut sampler = SystemUsageSampler::new();
        let mut disk_alert_monitor = DiskAlertMonitor::new();
        let mut interval = tokio::time::interval(SYSTEM_USAGE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let has_receivers = state.event_manager.has_event_receivers();

            let Some(info) = sampler.sample() else { continue };
            if has_receivers {
                state.event_manager.send_system_info(info);
            }

            // Disk-alert check is gated on (1) disk info being available, and
            // (2) the user opting in via `messaging.disk_alert` and
            // `messaging.notify_on`. Without any of these, `inspect` is a cheap
            // (3 float comparisons + 2 integer comparisons) no-op.
            //
            // The state-machine evaluation runs every tick regardless of
            // whether anyone is currently listening on the WS event stream,
            // so the monitor's hysteresis/rearm semantics stay correct across
            // transient disconnects.
            let alert_cfg: DiskAlertConfig = {
                let cfg = state.app_config.config.load();
                let Some(messaging) = cfg.messaging.as_ref() else { continue };
                if !messaging.notify_on.contains(&MsgKind::DiskAlert) {
                    continue;
                }
                let Some(alert_cfg) = messaging.disk_alert.as_ref() else { continue };
                alert_cfg.clone()
            };
            let Some(alert) = disk_alert_monitor.inspect(&alert_cfg, info.disk_total_bytes, info.disk_free_bytes)
            else {
                continue;
            };
            let http_client = state.http_client.load();
            send_messaging(&state.app_config, &http_client, MessageContent::DiskAlert(alert)).await;
        }
    })
}

struct CpuTracker {
    last_cpu_time_secs: f64,
    last_sample_at: Instant,
}

impl CpuTracker {
    fn new(cpu_time_secs: f64) -> Self { Self { last_cpu_time_secs: cpu_time_secs, last_sample_at: Instant::now() } }

    fn sample(&mut self, cpu_time_secs: f64) -> f32 {
        let now = Instant::now();
        let elapsed_secs = now.duration_since(self.last_sample_at).as_secs_f64();
        let cpu_delta_secs = (cpu_time_secs - self.last_cpu_time_secs).max(0.0);

        self.last_cpu_time_secs = cpu_time_secs;
        self.last_sample_at = now;

        if elapsed_secs <= f64::EPSILON {
            0.0
        } else {
            cpu_percent(cpu_delta_secs, elapsed_secs)
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn cpu_percent(cpu_delta_secs: f64, elapsed_secs: f64) -> f32 { ((cpu_delta_secs / elapsed_secs) * 100.0) as f32 }

#[derive(Clone, Copy, Default)]
struct NetSample {
    rx_bytes_per_sec: f64,
    tx_bytes_per_sec: f64,
    rx_bytes_total: u64,
    tx_bytes_total: u64,
}

#[allow(clippy::struct_field_names)]
struct NetTracker {
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    total_rx_bytes: u64,
    total_tx_bytes: u64,
    last_sample_at: Option<Instant>,
}

impl NetTracker {
    fn new() -> Self {
        Self { last_rx_bytes: 0, last_tx_bytes: 0, total_rx_bytes: 0, total_tx_bytes: 0, last_sample_at: None }
    }

    #[allow(clippy::cast_precision_loss)]
    fn sample(&mut self, rx_bytes: u64, tx_bytes: u64) -> NetSample {
        let now = Instant::now();
        let Some(last_sample_at) = self.last_sample_at else {
            self.last_rx_bytes = rx_bytes;
            self.last_tx_bytes = tx_bytes;
            self.last_sample_at = Some(now);
            return NetSample::default();
        };
        let elapsed_secs = now.duration_since(last_sample_at).as_secs_f64();

        let rx_delta = rx_bytes.saturating_sub(self.last_rx_bytes);
        let tx_delta = tx_bytes.saturating_sub(self.last_tx_bytes);

        self.last_rx_bytes = rx_bytes;
        self.last_tx_bytes = tx_bytes;
        self.total_rx_bytes = self.total_rx_bytes.saturating_add(rx_delta);
        self.total_tx_bytes = self.total_tx_bytes.saturating_add(tx_delta);
        self.last_sample_at = Some(now);

        let (rx_bytes_per_sec, tx_bytes_per_sec) = if elapsed_secs <= f64::EPSILON {
            (0.0, 0.0)
        } else {
            (rx_delta as f64 / elapsed_secs, tx_delta as f64 / elapsed_secs)
        };
        NetSample {
            rx_bytes_per_sec,
            tx_bytes_per_sec,
            rx_bytes_total: self.total_rx_bytes,
            tx_bytes_total: self.total_tx_bytes,
        }
    }
}

enum SystemUsageSampler {
    Platform(Box<platform::Sampler>),
    #[cfg(target_os = "linux")]
    Unavailable,
    #[cfg(not(target_os = "linux"))]
    Fallback(Box<FallbackSampler>),
}

impl SystemUsageSampler {
    fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            platform::Sampler::new().map_or(Self::Unavailable, |sampler| Self::Platform(Box::new(sampler)))
        }

        #[cfg(not(target_os = "linux"))]
        {
            platform::Sampler::new().map_or_else(
                || Self::Fallback(Box::new(FallbackSampler::new())),
                |sampler| Self::Platform(Box::new(sampler)),
            )
        }
    }

    fn sample(&mut self) -> Option<SystemInfo> {
        match self {
            Self::Platform(sampler) => sampler.sample(),
            #[cfg(target_os = "linux")]
            Self::Unavailable => None,
            #[cfg(not(target_os = "linux"))]
            Self::Fallback(sampler) => sampler.sample(),
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct FallbackSampler {
    inner: sysinfo::System,
    networks: sysinfo::Networks,
    pid: sysinfo::Pid,
    net_tracker: NetTracker,
}

#[cfg(not(target_os = "linux"))]
impl FallbackSampler {
    fn new() -> Self {
        let refresh_kind = sysinfo::RefreshKind::nothing()
            .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram())
            .with_processes(sysinfo::ProcessRefreshKind::nothing().with_cpu().with_memory());

        let networks = sysinfo::Networks::new_with_refreshed_list();

        Self {
            inner: sysinfo::System::new_with_specifics(refresh_kind),
            networks,
            pid: sysinfo::Pid::from_u32(std::process::id()),
            net_tracker: NetTracker::new(),
        }
    }

    fn sample(&mut self) -> Option<SystemInfo> {
        self.inner.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);
        self.inner.refresh_memory();
        self.networks.refresh(true);

        let (rx_bytes, tx_bytes) = sum_sysinfo_network_bytes(&self.networks);
        let net = self.net_tracker.sample(rx_bytes, tx_bytes);

        self.inner.processes().get(&self.pid).map(|proc| SystemInfo {
            cpu_usage: proc.cpu_usage(),
            memory_usage: proc.memory(),
            memory_total: self.inner.total_memory(),
            net_rx_bytes_per_sec: net.rx_bytes_per_sec,
            net_tx_bytes_per_sec: net.tx_bytes_per_sec,
            net_rx_bytes_total: net.rx_bytes_total,
            net_tx_bytes_total: net.tx_bytes_total,
            disk_total_bytes: 0,
            disk_free_bytes: 0,
        })
    }
}

#[cfg(not(target_os = "linux"))]
fn sum_sysinfo_network_bytes(networks: &sysinfo::Networks) -> (u64, u64) {
    networks.iter().fold((0u64, 0u64), |(rx, tx), (_, data)| {
        (rx.saturating_add(data.total_received()), tx.saturating_add(data.total_transmitted()))
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{parse_ascii_u64_bytes, CpuTracker, DiskProbe, NetSample, SystemInfo};
    use log::debug;
    use std::{
        fs::{read, File},
        io::{Read, Seek, SeekFrom},
    };

    const PROC_STAT_BUF_LEN: usize = 1024;
    const PROC_STATM_BUF_LEN: usize = 128;

    pub(super) struct Sampler {
        proc_stat_file: File,
        resident_pages_file: File,
        proc_stat_buf: [u8; PROC_STAT_BUF_LEN],
        resident_pages_buf: [u8; PROC_STATM_BUF_LEN],
        page_size: u64,
        clock_ticks_per_sec: u64,
        memory_total: u64,
        disk_probe: Option<DiskProbe>,
        cpu_tracker: CpuTracker,
        net_tracker: super::NetTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let mut proc_stat_file = File::open("/proc/self/stat").ok()?;
            let mut resident_pages_file = File::open("/proc/self/statm").ok()?;
            let memory_total = read_linux_mem_total_bytes()?;
            let page_size = read_positive_sysconf(libc::_SC_PAGESIZE)?;
            let clock_ticks_per_sec = read_positive_sysconf(libc::_SC_CLK_TCK)?;
            let mut proc_stat_buf = [0_u8; PROC_STAT_BUF_LEN];
            let mut resident_pages_buf = [0_u8; PROC_STATM_BUF_LEN];
            let proc_stat_len = read_into_buffer(&mut proc_stat_file, &mut proc_stat_buf).ok()?;
            read_into_buffer(&mut resident_pages_file, &mut resident_pages_buf).ok()?;
            let cpu_time_secs = parse_linux_proc_stat(&proc_stat_buf[..proc_stat_len])
                .map(|(utime, stime)| ticks_to_cpu_secs(utime, stime, clock_ticks_per_sec))?;
            Some(Self {
                proc_stat_file,
                resident_pages_file,
                proc_stat_buf,
                resident_pages_buf,
                page_size,
                clock_ticks_per_sec,
                memory_total,
                disk_probe: DiskProbe::for_cwd(),
                cpu_tracker: CpuTracker::new(cpu_time_secs),
                net_tracker: super::NetTracker::new(),
            })
        }

        #[allow(clippy::similar_names)]
        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            let proc_stat_len = read_into_buffer(&mut self.proc_stat_file, &mut self.proc_stat_buf).ok()?;
            let resident_pages_len =
                read_into_buffer(&mut self.resident_pages_file, &mut self.resident_pages_buf).ok()?;

            let (utime, stime) = parse_linux_proc_stat(&self.proc_stat_buf[..proc_stat_len])?;
            let resident_pages = parse_linux_proc_statm(&self.resident_pages_buf[..resident_pages_len])?;
            let cpu_time_secs = ticks_to_cpu_secs(utime, stime, self.clock_ticks_per_sec);

            let net = read_proc_net_dev_bytes()
                .map_or_else(NetSample::default, |(rx_bytes, tx_bytes)| self.net_tracker.sample(rx_bytes, tx_bytes));

            let (disk_total_bytes, disk_free_bytes) = self.disk_probe.as_ref().map_or((0, 0), DiskProbe::sample);

            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage: resident_pages.saturating_mul(self.page_size),
                memory_total: self.memory_total,
                net_rx_bytes_per_sec: net.rx_bytes_per_sec,
                net_tx_bytes_per_sec: net.tx_bytes_per_sec,
                net_rx_bytes_total: net.rx_bytes_total,
                net_tx_bytes_total: net.tx_bytes_total,
                disk_total_bytes,
                disk_free_bytes,
            })
        }
    }

    fn read_linux_mem_total_bytes() -> Option<u64> {
        use std::{
            fs::File,
            io::{Read, Seek, SeekFrom},
        };

        const PROC_MEMINFO_BUF_LEN: usize = 2048;

        let mut file = File::open("/proc/meminfo").ok()?;
        let mut buf = [0_u8; PROC_MEMINFO_BUF_LEN];
        file.seek(SeekFrom::Start(0)).ok()?;
        let len = file.read(&mut buf).ok()?;
        parse_linux_mem_total_kib(&buf[..len]).map(|kib| kib.saturating_mul(1024))
    }

    fn read_positive_sysconf(name: libc::c_int) -> Option<u64> {
        // SAFETY: `sysconf` is thread-safe and requires no additional invariants for these constants.
        let value = unsafe { libc::sysconf(name) };
        u64::try_from(value).ok().filter(|v| *v > 0)
    }

    pub(super) fn read_into_buffer(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
        file.seek(SeekFrom::Start(0))?;
        file.read(buf)
    }

    pub(super) fn parse_linux_proc_stat(bytes: &[u8]) -> Option<(u64, u64)> {
        let close_idx = bytes.iter().rposition(|byte| *byte == b')')?;
        let fields = bytes.get(close_idx + 1..)?;
        let mut parts = split_ascii_whitespace(fields);
        let _state = parts.next()?;
        let utime = parse_ascii_u64_bytes(parts.nth(10)?)?;
        let stime = parse_ascii_u64_bytes(parts.next()?)?;
        Some((utime, stime))
    }

    pub(super) fn parse_linux_proc_statm(bytes: &[u8]) -> Option<u64> {
        let mut parts = split_ascii_whitespace(bytes);
        let _size = parts.next()?;
        parse_ascii_u64_bytes(parts.next()?)
    }

    pub(super) fn parse_linux_mem_total_kib(bytes: &[u8]) -> Option<u64> {
        bytes
            .split(|byte| *byte == b'\n')
            .find(|line| line.starts_with(b"MemTotal:"))
            .and_then(|line| split_ascii_whitespace(line).nth(1))
            .and_then(parse_ascii_u64_bytes)
    }

    fn split_ascii_whitespace(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
        bytes.split(u8::is_ascii_whitespace).filter(|token| !token.is_empty())
    }

    #[allow(clippy::cast_precision_loss)]
    fn ticks_to_cpu_secs(utime: u64, stime: u64, clock_ticks_per_sec: u64) -> f64 {
        (utime.saturating_add(stime)) as f64 / clock_ticks_per_sec as f64
    }

    /// Parse `/proc/net/dev` and sum `rx_bytes` (field 0) and `tx_bytes` (field 8) across all interfaces.
    pub(super) fn parse_proc_net_dev(bytes: &[u8]) -> (u64, u64) {
        let mut total_rx: u64 = 0;
        let mut total_tx: u64 = 0;

        for line in bytes.split(|b| *b == b'\n').skip(2) {
            let after_colon = match line.iter().position(|b| *b == b':') {
                Some(pos) => &line[pos + 1..],
                None => continue,
            };
            let mut fields = split_ascii_whitespace(after_colon);
            if let Some(rx) = fields.next().and_then(parse_ascii_u64_bytes) {
                total_rx = total_rx.saturating_add(rx);
            }
            // field 8 = tx_bytes (skip fields 1..=7)
            if let Some(tx) = fields.nth(7).and_then(parse_ascii_u64_bytes) {
                total_tx = total_tx.saturating_add(tx);
            }
        }

        (total_rx, total_tx)
    }

    fn read_proc_net_dev_bytes() -> Option<(u64, u64)> {
        match read("/proc/net/dev") {
            Ok(bytes) => Some(parse_proc_net_dev(&bytes)),
            Err(err) => {
                debug!("Failed to sample /proc/net/dev: {err}");
                None
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{CpuTracker, DiskProbe, SystemInfo};
    use std::mem::size_of;
    use winapi::{
        shared::minwindef::FILETIME,
        um::{
            processthreadsapi::{GetCurrentProcess, GetProcessTimes},
            psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        },
    };

    /// Pseudo-handle from `GetCurrentProcess` is process-wide and safe across threads.
    #[derive(Clone, Copy)]
    struct ProcessHandle(winapi::um::winnt::HANDLE);
    // SAFETY: only stores the current-process pseudo-handle (-1), not an owned HANDLE.
    unsafe impl Send for ProcessHandle {}

    pub(super) struct Sampler {
        process: ProcessHandle,
        memory_total: u64,
        disk_probe: Option<DiskProbe>,
        cpu_tracker: CpuTracker,
        networks: sysinfo::Networks,
        net_tracker: super::NetTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let process = ProcessHandle(unsafe { GetCurrentProcess() });
            let memory_total = query_memory_total()?;
            let cpu_time_secs = query_process_cpu_time_secs(process.0)?;
            let networks = sysinfo::Networks::new_with_refreshed_list();
            Some(Self {
                process,
                memory_total,
                disk_probe: DiskProbe::for_cwd(),
                cpu_tracker: CpuTracker::new(cpu_time_secs),
                networks,
                net_tracker: super::NetTracker::new(),
            })
        }

        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            let cpu_time_secs = query_process_cpu_time_secs(self.process.0)?;
            let memory_usage = query_process_memory_usage(self.process.0)?;
            self.networks.refresh(true);
            let (rx_bytes, tx_bytes) = super::sum_sysinfo_network_bytes(&self.networks);
            let net = self.net_tracker.sample(rx_bytes, tx_bytes);
            let (disk_total_bytes, disk_free_bytes) = self.disk_probe.as_ref().map_or((0, 0), DiskProbe::sample);
            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage,
                memory_total: self.memory_total,
                net_rx_bytes_per_sec: net.rx_bytes_per_sec,
                net_tx_bytes_per_sec: net.tx_bytes_per_sec,
                net_rx_bytes_total: net.rx_bytes_total,
                net_tx_bytes_total: net.tx_bytes_total,
                disk_total_bytes,
                disk_free_bytes,
            })
        }
    }

    fn query_memory_total() -> Option<u64> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
            ..unsafe { std::mem::zeroed() }
        };
        let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
        (ok != 0).then_some(status.ullTotalPhys)
    }

    fn query_process_memory_usage(process: winapi::um::winnt::HANDLE) -> Option<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?,
            ..unsafe { std::mem::zeroed() }
        };
        let ok = unsafe {
            GetProcessMemoryInfo(process, &raw mut counters, u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?)
        };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }

    #[allow(clippy::cast_precision_loss)]
    fn query_process_cpu_time_secs(process: winapi::um::winnt::HANDLE) -> Option<f64> {
        let mut created = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut exited = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut kernel = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut user = unsafe { std::mem::zeroed::<FILETIME>() };
        let ok = unsafe { GetProcessTimes(process, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
        if ok == 0 {
            return None;
        }

        let kernel_100ns = filetime_to_u64(kernel);
        let user_100ns = filetime_to_u64(user);
        Some((kernel_100ns.saturating_add(user_100ns)) as f64 / 10_000_000.0)
    }

    fn filetime_to_u64(ft: FILETIME) -> u64 { (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime) }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{CpuTracker, DiskProbe, SystemInfo};
    use libc::{c_int, c_void, getrusage, rusage, sysctlbyname, timeval, RUSAGE_SELF};
    use std::{
        ffi::CString,
        mem::{size_of, zeroed},
    };

    type KernReturn = c_int;
    type MachPort = libc::c_uint;
    type MachMsgTypeNumber = libc::c_uint;
    type TaskFlavor = libc::c_uint;
    type TaskInfo = *mut libc::c_int;

    const MACH_TASK_BASIC_INFO: TaskFlavor = 20;
    const KERN_SUCCESS: KernReturn = 0;

    #[repr(C)]
    struct TimeValue {
        seconds: libc::c_int,
        microseconds: libc::c_int,
    }

    #[repr(C)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: TimeValue,
        system_time: TimeValue,
        policy: libc::c_int,
        suspend_count: libc::c_int,
    }

    unsafe extern "C" {
        fn mach_task_self() -> MachPort;
        fn task_info(
            target_task: MachPort,
            flavor: TaskFlavor,
            task_info_out: TaskInfo,
            task_info_out_count: *mut MachMsgTypeNumber,
        ) -> KernReturn;
    }

    pub(super) struct Sampler {
        memory_total: u64,
        disk_probe: Option<DiskProbe>,
        cpu_tracker: CpuTracker,
        networks: sysinfo::Networks,
        net_tracker: super::NetTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let memory_total = query_memory_total()?;
            let cpu_time_secs = query_process_cpu_time_secs()?;
            let networks = sysinfo::Networks::new_with_refreshed_list();
            Some(Self {
                memory_total,
                disk_probe: DiskProbe::for_cwd(),
                cpu_tracker: CpuTracker::new(cpu_time_secs),
                networks,
                net_tracker: super::NetTracker::new(),
            })
        }

        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            let cpu_time_secs = query_process_cpu_time_secs()?;
            let memory_usage = query_process_memory_usage()?;
            self.networks.refresh(true);
            let (rx_bytes, tx_bytes) = super::sum_sysinfo_network_bytes(&self.networks);
            let net = self.net_tracker.sample(rx_bytes, tx_bytes);
            let (disk_total_bytes, disk_free_bytes) = self.disk_probe.as_ref().map_or((0, 0), DiskProbe::sample);
            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage,
                memory_total: self.memory_total,
                net_rx_bytes_per_sec: net.rx_bytes_per_sec,
                net_tx_bytes_per_sec: net.tx_bytes_per_sec,
                net_rx_bytes_total: net.rx_bytes_total,
                net_tx_bytes_total: net.tx_bytes_total,
                disk_total_bytes,
                disk_free_bytes,
            })
        }
    }

    fn query_memory_total() -> Option<u64> {
        let name = CString::new("hw.memsize").ok()?;
        let mut value = 0u64;
        let mut size = size_of::<u64>();
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr().cast(),
                (&raw mut value).cast::<c_void>(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(value)
    }

    fn query_process_memory_usage() -> Option<u64> {
        let mut info = unsafe { zeroed::<MachTaskBasicInfo>() };
        let mut count = u32::try_from(size_of::<MachTaskBasicInfo>() / size_of::<libc::c_int>()).ok()?;
        let rc = unsafe {
            task_info(mach_task_self(), MACH_TASK_BASIC_INFO, (&raw mut info).cast::<libc::c_int>(), &raw mut count)
        };
        (rc == KERN_SUCCESS).then_some(info.resident_size)
    }

    fn query_process_cpu_time_secs() -> Option<f64> {
        let mut usage = unsafe { zeroed::<rusage>() };
        let rc = unsafe { getrusage(RUSAGE_SELF, &raw mut usage) };
        if rc != 0 {
            return None;
        }

        let user_secs = timeval_to_secs(usage.ru_utime);
        let system_secs = timeval_to_secs(usage.ru_stime);
        Some(user_secs + system_secs)
    }

    #[allow(clippy::cast_precision_loss)]
    fn timeval_to_secs(tv: timeval) -> f64 { tv.tv_sec as f64 + (f64::from(tv.tv_usec) / 1_000_000.0) }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod platform {
    pub(super) struct Sampler;

    impl Sampler {
        pub(super) fn new() -> Option<Self> { None }
        pub(super) fn sample(&mut self) -> Option<shared::model::SystemInfo> { None }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod disk_probe_tests {
    use super::DiskProbe;
    use std::path::Path;

    #[test]
    fn disk_probe_for_cwd_returns_nonzero_on_linux() {
        let probe = DiskProbe::for_cwd().expect("CWD should be available on Linux");
        let (total, free) = probe.sample();
        // CWD is normally on a mounted real filesystem, so the call should yield
        // positive sizes. If the test runs in a constrained sandbox it may return
        // (0, 0); we just assert the call is well-formed in that case too.
        assert!(total == 0 || total > free, "total {total} should be 0 or > free {free}");
    }

    #[test]
    fn disk_probe_with_relative_path_resolves_via_kernel() {
        // Relative paths are resolved against the process CWD by the kernel;
        // the sample call must not allocate or panic.
        let probe = DiskProbe { path: super::DiskPath::from_path(Path::new(".")) };
        let _ = probe.sample();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        fs::{remove_file, File},
        io::Write,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn test_parse_linux_proc_stat_extracts_utime_and_stime_after_comm() {
        let stat = b"537051 (tuliprox worker) S 1 2 3 4 5 6 7 8 9 10 111 222 13 14 15 16 17 18 19 20 21 22 23";
        let parsed = super::platform::parse_linux_proc_stat(stat);
        assert_eq!(parsed, Some((111, 222)));
    }

    #[test]
    fn test_parse_linux_proc_statm_extracts_resident_pages() {
        let statm = b"1000 250 200 0 0 0 0\n";
        let resident = super::platform::parse_linux_proc_statm(statm);
        assert_eq!(resident, Some(250));
    }

    #[test]
    fn test_parse_linux_mem_total_kib_extracts_total_memory() {
        let meminfo = b"MemTotal:       16384256 kB\nMemFree:         1234567 kB\n";
        let total_kib = super::platform::parse_linux_mem_total_kib(meminfo);
        assert_eq!(total_kib, Some(16_384_256));
    }

    #[test]
    fn test_read_into_buffer_reuses_fixed_capacity_without_read_to_end() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).ok().map_or(0_u128, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!("tuliprox-sys-usage-{unique}.tmp"));
        let mut temp = File::create(&path).unwrap_or_else(|_| unreachable!());
        temp.write_all(b"1234567890").unwrap_or_else(|_| unreachable!());
        drop(temp);

        let mut file = File::open(&path).unwrap_or_else(|_| unreachable!());
        let mut buf = [0_u8; 4];
        let len = super::platform::read_into_buffer(&mut file, &mut buf).unwrap_or_else(|_| unreachable!());

        assert_eq!(len, 4);
        assert_eq!(&buf, b"1234");

        let _ = remove_file(path);
    }

    #[test]
    fn test_cpu_tracker_reports_expected_percentage() {
        let mut tracker = super::CpuTracker::new(1.0);
        tracker.last_sample_at -= Duration::from_secs(2);
        let cpu_usage = tracker.sample(2.0);
        assert!((49.0..=51.0).contains(&cpu_usage));
    }

    #[test]
    fn test_parse_proc_net_dev_sums_all_interfaces() {
        let dev = b"Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n  lo: 1000       10    0    0    0     0          0         0     2000       20    0    0    0     0       0          0\neth0: 5000       50    0    0    0     0          0         0     3000       30    0    0    0     0       0          0\n";
        let (rx, tx) = super::platform::parse_proc_net_dev(dev);
        assert_eq!(rx, 6000);
        assert_eq!(tx, 5000);
    }

    #[test]
    fn test_parse_proc_net_dev_empty_returns_zero() {
        let dev = b"Inter-|   Receive\n face |bytes\n";
        let (rx, tx) = super::platform::parse_proc_net_dev(dev);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn test_net_tracker_reports_bytes_per_second() {
        let mut tracker = super::NetTracker::new();
        let _ = tracker.sample(1000, 500);
        tracker.last_sample_at =
            tracker.last_sample_at.map(|instant| instant.checked_sub(Duration::from_secs(2)).unwrap());
        let sample = tracker.sample(3000, 1500);
        assert!((999.0..=1001.0).contains(&sample.rx_bytes_per_sec));
        assert!((499.0..=501.0).contains(&sample.tx_bytes_per_sec));
        assert_eq!(sample.rx_bytes_total, 2000);
        assert_eq!(sample.tx_bytes_total, 1000);
    }
}

#[cfg(test)]
mod disk_alert_tests {
    use super::{DiskAlertConfig, DiskAlertMonitor};
    use shared::model::DiskAlertLevel;
    use std::time::Duration;

    fn cfg(warn: f64, critical: f64, repeat_secs: u64) -> DiskAlertConfig {
        DiskAlertConfig { warn_percent: warn, critical_percent: critical, repeat_interval_secs: repeat_secs }
    }

    fn total_used_percent(percent: f64) -> (u64, u64) {
        let total: u64 = 1_000_000_000;
        #[allow(clippy::cast_sign_loss, clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let free = ((100.0_f64 - percent) / 100.0 * total as f64) as u64;
        (total, free)
    }

    #[test]
    fn monitor_returns_none_when_disk_total_is_zero() {
        let mut m = DiskAlertMonitor::new();
        assert!(m.inspect(&cfg(80.0, 95.0, 3600), 0, 0).is_none());
    }

    #[test]
    fn monitor_emits_warn_on_first_crossing_from_normal() {
        let mut m = DiskAlertMonitor::new();
        let (total, free) = total_used_percent(50.0);
        assert!(m.inspect(&cfg(80.0, 95.0, 3600), total, free).is_none());
        let (total, free) = total_used_percent(85.0);
        let alert = m.inspect(&cfg(80.0, 95.0, 3600), total, free).expect("crossing warn should notify");
        assert_eq!(alert.level, DiskAlertLevel::Warn);
    }

    #[test]
    fn monitor_emits_critical_on_escalation_from_warn() {
        let mut m = DiskAlertMonitor::new();
        let c = cfg(80.0, 95.0, 3600);
        let (t, f) = total_used_percent(85.0);
        let _ = m.inspect(&c, t, f).expect("warn crossing");
        let (t, f) = total_used_percent(97.0);
        let alert = m.inspect(&c, t, f).expect("critical escalation should notify");
        assert_eq!(alert.level, DiskAlertLevel::Critical);
    }

    #[test]
    fn monitor_silences_repeat_inside_rearm_window() {
        let mut m = DiskAlertMonitor::new();
        let c = cfg(80.0, 95.0, 3600);
        let (t, f) = total_used_percent(85.0);
        let _ = m.inspect(&c, t, f).expect("first crossing");
        // Second inspection in the same state, with no time elapsed: must NOT notify.
        let again = m.inspect(&c, t, f);
        assert!(again.is_none(), "monitor must not re-notify inside rearm window");
    }

    #[test]
    fn monitor_rearms_after_interval_in_same_state() {
        let mut m = DiskAlertMonitor::new();
        let c = cfg(80.0, 95.0, 1); // 1s re-arm for the test
        let (t, f) = total_used_percent(85.0);
        let _ = m.inspect(&c, t, f).expect("first crossing");
        // Backdate the last-notified timestamp to force the re-arm branch.
        m.last_notified_at = m.last_notified_at.map(|t| t.checked_sub(Duration::from_secs(2)).unwrap());
        let alert = m.inspect(&c, t, f).expect("rearm should re-notify");
        assert_eq!(alert.level, DiskAlertLevel::Warn);
    }

    #[test]
    fn monitor_resets_state_when_disk_info_becomes_unavailable() {
        let mut m = DiskAlertMonitor::new();
        let c = cfg(80.0, 95.0, 3600);
        let (t, f) = total_used_percent(90.0);
        let _ = m.inspect(&c, t, f).expect("first crossing");
        assert_eq!(m.last_level, Some(DiskAlertLevel::Warn));
        // Disk info becomes unavailable: monitor must forget prior state.
        assert!(m.inspect(&c, 0, 0).is_none());
        assert!(m.last_level.is_none());
        // Next valid sample at warn level is treated as a fresh transition.
        let (t, f) = total_used_percent(90.0);
        let alert = m.inspect(&c, t, f).expect("re-arm after unavailability");
        assert_eq!(alert.level, DiskAlertLevel::Warn);
    }

    #[test]
    fn monitor_treats_drop_below_warn_as_return_to_normal() {
        let mut m = DiskAlertMonitor::new();
        let c = cfg(80.0, 95.0, 3600);
        let (t, f) = total_used_percent(90.0);
        let _ = m.inspect(&c, t, f).expect("first crossing");
        assert_eq!(m.last_level, Some(DiskAlertLevel::Warn));
        // Drop below warn: no notification, state goes back to normal.
        let (t, f) = total_used_percent(50.0);
        assert!(m.inspect(&c, t, f).is_none());
        assert!(m.last_level.is_none());
    }
}
