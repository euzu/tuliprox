use log::{log_enabled, trace, Level};
use shared::utils::human_readable_byte_size;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(target_os = "linux"))]
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

#[macro_export]
macro_rules! exit {
    ($($arg:tt)*) => {{
        log::error!($($arg)*);
        std::process::exit(1);
    }};
}

pub use exit;

static PROCESS_PEAK_RSS_BYTES: AtomicU64 = AtomicU64::new(0);
static PROCESS_PEAK_VMEM_BYTES: AtomicU64 = AtomicU64::new(0);

// fn trim_allocator_after_update() {
//     #[cfg(all(target_os = "linux", target_env = "gnu"))]
//     {
//         log_memory_snapshot("exec_processing before_malloc_trim");
//         // SAFETY: libc::malloc_trim is thread-safe for the process allocator
//         // and does not require any additional invariants on the Rust side.
//         let result = unsafe { libc::malloc_trim(0) };
//         debug!("allocator[malloc_trim] result={result}");
//         log_memory_snapshot("exec_processing after_malloc_trim");
//     }
// }

pub fn log_memory_snapshot(stage: &str) {
    if !log_enabled!(Level::Trace) {
        return;
    }

    #[cfg(target_os = "linux")]
    let snapshot = read_linux_memory_snapshot();
    #[cfg(not(target_os = "linux"))]
    let snapshot = read_fallback_memory_snapshot();

    if let Some((rss_bytes, vmem_bytes)) = snapshot {
        let peak_rss = PROCESS_PEAK_RSS_BYTES.fetch_max(rss_bytes, Ordering::Relaxed).max(rss_bytes);
        let peak_vmem = PROCESS_PEAK_VMEM_BYTES.fetch_max(vmem_bytes, Ordering::Relaxed).max(vmem_bytes);

        trace!(
            "memory[{stage}] rss={} ({}) vmem={} ({}) peak_rss={} ({}) peak_vmem={} ({})",
            rss_bytes,
            human_readable_byte_size(rss_bytes),
            vmem_bytes,
            human_readable_byte_size(vmem_bytes),
            peak_rss,
            human_readable_byte_size(peak_rss),
            peak_vmem,
            human_readable_byte_size(peak_vmem),
        );
    } else {
        trace!("memory[{stage}] process info unavailable");
    }
}

#[cfg(target_os = "linux")]
fn read_linux_memory_snapshot() -> Option<(u64, u64)> {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom},
    };

    const PROC_STATUS_BUF_LEN: usize = 4096;

    let mut file = File::open("/proc/self/status").ok()?;
    let mut buf = [0_u8; PROC_STATUS_BUF_LEN];
    file.seek(SeekFrom::Start(0)).ok()?;
    let len = file.read(&mut buf).ok()?;
    parse_linux_proc_status_memory_bytes(&buf[..len])
}

#[cfg(target_os = "linux")]
fn parse_linux_proc_status_memory_bytes(bytes: &[u8]) -> Option<(u64, u64)> {
    let mut rss_kib = None;
    let mut vmem_kib = None;

    for line in bytes.split(|byte| *byte == b'\n') {
        if line.starts_with(b"VmRSS:") {
            rss_kib = parse_linux_status_kib_value(line);
        } else if line.starts_with(b"VmSize:") {
            vmem_kib = parse_linux_status_kib_value(line);
        }

        if let (Some(rss_kib), Some(vmem_kib)) = (rss_kib, vmem_kib) {
            return Some((rss_kib.saturating_mul(1024), vmem_kib.saturating_mul(1024)));
        }
    }

    None
}

#[cfg(target_os = "linux")]
fn parse_linux_status_kib_value(line: &[u8]) -> Option<u64> {
    line.split(u8::is_ascii_whitespace)
        .filter(|token| !token.is_empty())
        .nth(1)
        .and_then(parse_linux_u64)
}

#[cfg(target_os = "linux")]
fn parse_linux_u64(token: &[u8]) -> Option<u64> {
    if token.is_empty() {
        return None;
    }

    let mut value = 0_u64;
    for byte in token {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }

    Some(value)
}

#[cfg(not(target_os = "linux"))]
fn read_fallback_memory_snapshot() -> Option<(u64, u64)> {
    let pid = Pid::from_u32(std::process::id());
    let refresh_kind = RefreshKind::nothing()
        .with_memory(MemoryRefreshKind::nothing().with_ram())
        .with_processes(ProcessRefreshKind::nothing().with_memory());
    let mut sys = System::new_with_specifics(refresh_kind);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.refresh_memory();

    sys.processes()
        .get(&pid)
        .map(|process| (process.memory(), process.virtual_memory()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn test_parse_linux_proc_status_memory_bytes_extracts_rss_and_vmem() {
        let status = b"Name:\ttuliprox\nVmSize:\t  2048 kB\nVmRSS:\t  1024 kB\nThreads:\t3\n";
        let parsed = super::parse_linux_proc_status_memory_bytes(status);
        assert_eq!(parsed, Some((1_048_576, 2_097_152)));
    }
}
