#[must_use]
pub(crate) fn system_hostname() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(name) = sysinfo::System::host_name() {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    std::env::var("HOSTNAME").ok().and_then(|h| {
        let trimmed = h.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}
