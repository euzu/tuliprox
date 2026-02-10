use std::sync::atomic::{AtomicU64, Ordering};
use log::{log_enabled, trace, Level};
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use shared::utils::human_readable_byte_size;

#[macro_export]
macro_rules! exit {
    ($($arg:tt)*) => {{
        error!($($arg)*);
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

    let pid = Pid::from_u32(std::process::id());
    let refresh_kind = RefreshKind::nothing()
        .with_memory(MemoryRefreshKind::nothing().with_ram())
        .with_processes(ProcessRefreshKind::nothing().with_memory());
    let mut sys = System::new_with_specifics(refresh_kind);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.refresh_memory();

    if let Some(process) = sys.processes().get(&pid) {
        let rss_bytes = process.memory();
        let vmem_bytes = process.virtual_memory();
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