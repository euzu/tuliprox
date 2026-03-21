use crate::api::model::AppState;
use shared::model::SystemInfo;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const SYSTEM_USAGE_INTERVAL: Duration = Duration::from_secs(5);

pub fn exec_system_usage(app_state: &Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let state = Arc::clone(app_state);

    tokio::spawn(async move {
        let mut sampler = SystemUsageSampler::new();
        let mut interval = tokio::time::interval(SYSTEM_USAGE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if !state.event_manager.has_event_receivers() {
                continue;
            }

            if let Some(info) = sampler.sample() {
                state.event_manager.send_system_info(info);
            }
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

enum SystemUsageSampler {
    Platform(platform::Sampler),
    Fallback(Box<FallbackSampler>),
}

impl SystemUsageSampler {
    fn new() -> Self {
        platform::Sampler::new().map_or_else(
            || Self::Fallback(Box::new(FallbackSampler::new())),
            Self::Platform,
        )
    }

    fn sample(&mut self) -> Option<SystemInfo> {
        match self {
            Self::Platform(sampler) => sampler.sample(),
            Self::Fallback(sampler) => sampler.sample(),
        }
    }
}

struct FallbackSampler {
    inner: sysinfo::System,
    pid: sysinfo::Pid,
}

impl FallbackSampler {
    fn new() -> Self {
        let refresh_kind = sysinfo::RefreshKind::nothing()
            .with_memory(sysinfo::MemoryRefreshKind::nothing().with_ram())
            .with_processes(sysinfo::ProcessRefreshKind::nothing().with_cpu().with_memory());

        Self {
            inner: sysinfo::System::new_with_specifics(refresh_kind),
            pid: sysinfo::Pid::from_u32(std::process::id()),
        }
    }

    fn sample(&mut self) -> Option<SystemInfo> {
        self.inner
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);
        self.inner.refresh_memory();
        self.inner.processes().get(&self.pid).map(|proc| SystemInfo {
            cpu_usage: proc.cpu_usage(),
            memory_usage: proc.memory(),
            memory_total: self.inner.total_memory(),
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{CpuTracker, SystemInfo};
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom},
    };

    pub(super) struct Sampler {
        proc_stat_file: File,
        resident_pages_file: File,
        proc_stat_buf: Vec<u8>,
        resident_pages_buf: Vec<u8>,
        page_size: u64,
        clock_ticks_per_sec: u64,
        memory_total: u64,
        cpu_tracker: CpuTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let mut proc_stat_file = File::open("/proc/self/stat").ok()?;
            let mut resident_pages_file = File::open("/proc/self/statm").ok()?;
            let memory_total = read_linux_mem_total_bytes()?;
            let page_size = read_positive_sysconf(libc::_SC_PAGESIZE)?;
            let clock_ticks_per_sec = read_positive_sysconf(libc::_SC_CLK_TCK)?;
            let mut proc_stat_buf = Vec::with_capacity(256);
            let mut resident_pages_buf = Vec::with_capacity(64);
            read_file(&mut proc_stat_file, &mut proc_stat_buf).ok()?;
            read_file(&mut resident_pages_file, &mut resident_pages_buf).ok()?;
            let cpu_time_secs =
                parse_linux_proc_stat(&proc_stat_buf).map(|(utime, stime)| ticks_to_cpu_secs(utime, stime, clock_ticks_per_sec))?;

            Some(Self {
                proc_stat_file,
                resident_pages_file,
                proc_stat_buf,
                resident_pages_buf,
                page_size,
                clock_ticks_per_sec,
                memory_total,
                cpu_tracker: CpuTracker::new(cpu_time_secs),
            })
        }

        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            read_file(&mut self.proc_stat_file, &mut self.proc_stat_buf).ok()?;
            read_file(&mut self.resident_pages_file, &mut self.resident_pages_buf).ok()?;

            let (utime, stime) = parse_linux_proc_stat(&self.proc_stat_buf)?;
            let resident_pages = parse_linux_proc_statm(&self.resident_pages_buf)?;
            let cpu_time_secs = ticks_to_cpu_secs(utime, stime, self.clock_ticks_per_sec);

            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage: resident_pages.saturating_mul(self.page_size),
                memory_total: self.memory_total,
            })
        }
    }

    fn read_linux_mem_total_bytes() -> Option<u64> {
        let meminfo = std::fs::read("/proc/meminfo").ok()?;
        parse_linux_mem_total_kib(&meminfo).map(|kib| kib.saturating_mul(1024))
    }

    fn read_positive_sysconf(name: libc::c_int) -> Option<u64> {
        // SAFETY: `sysconf` is thread-safe and requires no additional invariants for these constants.
        let value = unsafe { libc::sysconf(name) };
        u64::try_from(value).ok().filter(|v| *v > 0)
    }

    fn read_file(file: &mut File, buf: &mut Vec<u8>) -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        buf.clear();
        file.read_to_end(buf)?;
        Ok(())
    }

    pub(super) fn parse_linux_proc_stat(bytes: &[u8]) -> Option<(u64, u64)> {
        let close_idx = bytes.iter().rposition(|byte| *byte == b')')?;
        let fields = bytes.get(close_idx + 1..)?;
        let mut parts = split_ascii_whitespace(fields);
        let _state = parts.next()?;
        let utime = parse_u64_token(parts.nth(10)?)?;
        let stime = parse_u64_token(parts.next()?)?;
        Some((utime, stime))
    }

    pub(super) fn parse_linux_proc_statm(bytes: &[u8]) -> Option<u64> {
        let mut parts = split_ascii_whitespace(bytes);
        let _size = parts.next()?;
        parse_u64_token(parts.next()?)
    }

    pub(super) fn parse_linux_mem_total_kib(bytes: &[u8]) -> Option<u64> {
        bytes
            .split(|byte| *byte == b'\n')
            .find(|line| line.starts_with(b"MemTotal:"))
            .and_then(|line| split_ascii_whitespace(line).nth(1))
            .and_then(parse_u64_token)
    }

    fn parse_u64_token(token: &[u8]) -> Option<u64> { std::str::from_utf8(token).ok()?.parse::<u64>().ok() }

    fn split_ascii_whitespace(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
        bytes
            .split(u8::is_ascii_whitespace)
            .filter(|token| !token.is_empty())
    }

    #[allow(clippy::cast_precision_loss)]
    fn ticks_to_cpu_secs(utime: u64, stime: u64, clock_ticks_per_sec: u64) -> f64 {
        (utime.saturating_add(stime)) as f64 / clock_ticks_per_sec as f64
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{CpuTracker, SystemInfo};
    use std::{mem::size_of, ptr::null_mut};
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::{
        processthreadsapi::{GetCurrentProcess, GetProcessTimes},
        psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
    };

    pub(super) struct Sampler {
        process: winapi::um::winnt::HANDLE,
        memory_total: u64,
        cpu_tracker: CpuTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let process = unsafe { GetCurrentProcess() };
            let memory_total = query_memory_total()?;
            let cpu_time_secs = query_process_cpu_time_secs(process)?;
            Some(Self {
                process,
                memory_total,
                cpu_tracker: CpuTracker::new(cpu_time_secs),
            })
        }

        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            let cpu_time_secs = query_process_cpu_time_secs(self.process)?;
            let memory_usage = query_process_memory_usage(self.process)?;
            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage,
                memory_total: self.memory_total,
            })
        }
    }

    fn query_memory_total() -> Option<u64> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
            ..unsafe { std::mem::zeroed() }
        };
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0).then_some(status.ullTotalPhys)
    }

    fn query_process_memory_usage(process: winapi::um::winnt::HANDLE) -> Option<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?,
            ..unsafe { std::mem::zeroed() }
        };
        let ok = unsafe {
            GetProcessMemoryInfo(
                process,
                &mut counters,
                u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?,
            )
        };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }

    fn query_process_cpu_time_secs(process: winapi::um::winnt::HANDLE) -> Option<f64> {
        let mut created = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut exited = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut kernel = unsafe { std::mem::zeroed::<FILETIME>() };
        let mut user = unsafe { std::mem::zeroed::<FILETIME>() };
        let ok = unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }

        let kernel_100ns = filetime_to_u64(kernel);
        let user_100ns = filetime_to_u64(user);
        Some((kernel_100ns.saturating_add(user_100ns)) as f64 / 10_000_000.0)
    }

    fn filetime_to_u64(ft: FILETIME) -> u64 {
        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{CpuTracker, SystemInfo};
    use libc::{c_char, c_int, c_void, getrusage, gettimeofday, rusage, sysctlbyname, timeval, RUSAGE_SELF};
    use std::{ffi::CString, mem::{size_of, zeroed}};

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
        cpu_tracker: CpuTracker,
    }

    impl Sampler {
        pub(super) fn new() -> Option<Self> {
            let memory_total = query_memory_total()?;
            let cpu_time_secs = query_process_cpu_time_secs()?;
            Some(Self {
                memory_total,
                cpu_tracker: CpuTracker::new(cpu_time_secs),
            })
        }

        pub(super) fn sample(&mut self) -> Option<SystemInfo> {
            let cpu_time_secs = query_process_cpu_time_secs()?;
            let memory_usage = query_process_memory_usage()?;
            Some(SystemInfo {
                cpu_usage: self.cpu_tracker.sample(cpu_time_secs),
                memory_usage,
                memory_total: self.memory_total,
            })
        }
    }

    fn query_memory_total() -> Option<u64> {
        let name = CString::new("hw.memsize").ok()?;
        let mut value = 0u64;
        let mut size = size_of::<u64>();
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr() as *const c_char,
                (&mut value as *mut u64).cast::<c_void>(),
                &mut size,
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
            task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                (&mut info as *mut MachTaskBasicInfo).cast::<libc::c_int>(),
                &mut count,
            )
        };
        (rc == KERN_SUCCESS).then_some(info.resident_size)
    }

    fn query_process_cpu_time_secs() -> Option<f64> {
        let mut usage = unsafe { zeroed::<rusage>() };
        let rc = unsafe { getrusage(RUSAGE_SELF, &mut usage) };
        if rc != 0 {
            return None;
        }

        let user_secs = timeval_to_secs(usage.ru_utime);
        let system_secs = timeval_to_secs(usage.ru_stime);
        Some(user_secs + system_secs)
    }

    fn timeval_to_secs(tv: timeval) -> f64 { tv.tv_sec as f64 + (tv.tv_usec as f64 / 1_000_000.0) }
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
mod tests {
    use std::time::Duration;

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
    fn test_cpu_tracker_reports_expected_percentage() {
        let mut tracker = super::CpuTracker::new(1.0);
        tracker.last_sample_at -= Duration::from_secs(2);
        let cpu_usage = tracker.sample(2.0);
        assert!((49.0..=51.0).contains(&cpu_usage));
    }
}
