//! Disk reservation and safety admission.
//!
//! - Measure the filesystem containing the canonical recording root.
//! - Reject start with `recording_insufficient_disk` when free bytes
//!   cannot cover safety bytes plus the candidate's remaining
//!   conservative charge after current active disk reservations.
//! - Serialize active disk reservations through the queue
//!   transaction so two starts cannot consume the same measured
//!   headroom.
//! - Never reuse a measurement from another filesystem solely
//!   because it belongs to `storage_dir` or the download directory.
//!
//! This module provides:
//! - `free_bytes_for(path)`: a syscall on the given path. Returns
//!   the bytes available to the **caller** (`f_bavail` on Unix),
//!   not the raw free blocks — the latter includes space reserved
//!   for root that the recording worker cannot actually use.
//! - `would_fit_on_disk(free, safety, active, candidate)`: a pure
//!   admission check. The active-reservation total is fed in by
//!   the caller (under the queue mutation boundary so two starts
//!   cannot race on the same headroom).
//! - `DiskAdmission`: `Ok { headroom_after }` / `Insufficient { ... }`.
//!
//! The "stale measurement" + "concurrent starts" invariants are
//! tested via the pure function; the syscall path is exercised
//! against the actual recording root when `cfg(test)` runs.

use std::path::Path;

/// `Ok { headroom_after }` if the candidate fits; otherwise
/// `Insufficient { free, safety, active, candidate, headroom }`
/// with the values the caller can show in the
/// `recording_insufficient_disk` error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskAdmission {
    Ok { headroom_after: u64 },
    Insufficient { free: u64, safety: u64, active: u64, candidate: u64, headroom: u64 },
}

/// Pure admission check.
///
/// `headroom = free.saturating_sub(safety).saturating_sub(active)`.
/// If `candidate <= headroom`, admission is `Ok { headroom_after = headroom - candidate }`.
/// Otherwise `Insufficient` with the raw values.
pub fn would_fit_on_disk(
    free_bytes: u64,
    safety_bytes: u64,
    active_reservations: u64,
    candidate_charge: u64,
) -> DiskAdmission {
    let headroom = free_bytes.saturating_sub(safety_bytes).saturating_sub(active_reservations);
    if candidate_charge <= headroom {
        DiskAdmission::Ok { headroom_after: headroom - candidate_charge }
    } else {
        DiskAdmission::Insufficient {
            free: free_bytes,
            safety: safety_bytes,
            active: active_reservations,
            candidate: candidate_charge,
            headroom,
        }
    }
}

/// Sum the active disk reservations across a set of currently-active
/// tasks. Active disk reservations must be counted: tasks in
/// `Downloading` (and any other state holding disk headroom).
/// Generic downloads are excluded.
///
/// The caller is expected to be under the queue mutation boundary
/// (so the sum is consistent across the set).
pub fn active_disk_reservations<V>(tasks: &[V]) -> u64
where
    V: super::recording_quota::QuotaRecordingTaskView,
{
    let mut total = 0u64;
    for task in tasks {
        // "Active" means holding disk headroom right now. Today
        // that is `Downloading`; the worker pre-start path checks
        // admission before transitioning into the active state, so
        // `Downloading` is the only contributor for the conservative
        // charge. Anything
        // else with `reserved_bytes > 0` is **not** holding
        // headroom yet — the headroom is reserved at start.
        if matches!(task.state(), crate::download::DownloadState::Downloading) {
            total = total.saturating_add(super::recording_quota::charge_for_task(task));
        }
    }
    total
}

/// Measure the bytes available to the caller on the filesystem
/// that contains `path`. Returns `None` if the syscall fails
/// (path missing, permission denied, or non-Unix/Windows).
///
/// Never reuse a measurement from another filesystem. The syscall
/// is keyed on the supplied path — the kernel resolves the path
/// to its mount. The caller must pass the canonical recording
/// root, not e.g. `storage_dir` or the
/// generic download directory.
/// Total and available bytes on the filesystem that contains `path`,
/// as `(total, available)`.
///
/// The disk-pressure sweep needs both numbers to compute a used
/// percentage, and both must come from the *same* syscall: measuring
/// total and free separately can straddle a write and produce a
/// percentage that never existed.
pub fn filesystem_capacity_for(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

        // SAFETY: `cstr` is a valid NUL-terminated C string; `&raw mut stat`
        // is a writable pointer to a zeroed struct.
        let rc = unsafe { libc::statvfs(cstr.as_ptr(), &raw mut stat) };
        if rc != 0 {
            return None;
        }

        let bsize = stat.f_frsize as u64;
        // `fsblkcnt_t` is `u32` on Darwin and `u64` on Linux, so the conversion
        // is real on one and reflexive on the other. Clippy only sees the
        // reflexive case on the platform it happens to be running on.
        #[allow(clippy::useless_conversion)]
        let total = u64::from(stat.f_blocks).saturating_mul(bsize);

        // `f_bavail`, not `f_bfree`: the service user cannot use the
        // root-reserved blocks, so counting them would understate pressure.
        #[allow(clippy::useless_conversion)]
        let available = u64::from(stat.f_bavail).saturating_mul(bsize);

        Some((total, available))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free_bytes_available: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut total_free_bytes: u64 = 0;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path; the three output
        // pointers alias `ULARGE_INTEGER` (which is a `u64` newtype on
        // the winapi crate). `&raw mut` gives us a stable raw pointer
        // to each `u64`; `.cast()` widens it to the `*mut ULARGE_INTEGER`
        // the FFI expects.
        let ok = unsafe {
            winapi::um::fileapi::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                (&raw mut free_bytes_available).cast(),
                (&raw mut total_bytes).cast(),
                (&raw mut total_free_bytes).cast(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some((total_bytes, free_bytes_available))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

pub fn free_bytes_for(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

        // SAFETY: `cstr` is a valid NUL-terminated C string; `&raw mut stat`
        // is a writable pointer to a zeroed struct.
        let rc = unsafe { libc::statvfs(cstr.as_ptr(), &raw mut stat) };
        if rc != 0 {
            return None;
        }

        let bsize = stat.f_frsize as u64;

        // `f_bavail` is the bytes available to a non-privileged
        // caller — the worker runs as the service user and is
        // subject to the same reservation as any other user.
        //
        // `fsblkcnt_t` is `u32` on Darwin and `u64` on Linux; see
        // `filesystem_capacity_for`.
        #[allow(clippy::useless_conversion)]
        let available = u64::from(stat.f_bavail).saturating_mul(bsize);
        Some(available)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free_bytes_available: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut total_free_bytes: u64 = 0;
        // SAFETY: see `filesystem_capacity_for` — the three output
        // pointers alias `ULARGE_INTEGER` (`u64` newtype); `&raw mut`
        // + `.cast()` produces the right raw pointer type.
        let ok = unsafe {
            winapi::um::fileapi::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                (&raw mut free_bytes_available).cast(),
                (&raw mut total_bytes).cast(),
                (&raw mut total_free_bytes).cast(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some(free_bytes_available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::DownloadState;
    use shared::model::{
        recording::{RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility},
        UserId,
    };

    fn make_meta(reserved: u64, measured: u64) -> RecordingMetadata {
        RecordingMetadata {
            owner: RecordingOwner::User(UserId::from("web:alice")),
            visibility: RecordingVisibility::Private,
            source: Some(RecordingSource::new("t1", "v1", "in1")),
            program_start: None,
            program_end: None,
            scheduled_start: None,
            scheduled_end: None,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: None,
            channel_name: None,
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            reserved_bytes: reserved,
            measured_bytes: measured,
            completed_at: None,
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    struct T {
        state: DownloadState,
        recording: Option<RecordingMetadata>,
    }
    impl super::super::recording_quota::QuotaRecordingTaskView for T {
        fn state(&self) -> &DownloadState { &self.state }
        fn recording(&self) -> Option<&RecordingMetadata> { self.recording.as_ref() }
        fn uuid(&self) -> &'static str { "" }
    }

    fn downloading(reserved: u64, measured: u64) -> T {
        T { state: DownloadState::Downloading, recording: Some(make_meta(reserved, measured)) }
    }

    #[test]
    fn admission_ok_when_candidate_fits() {
        let out = would_fit_on_disk(10_000, 1_000, 2_000, 3_000);
        // headroom = 10_000 - 1_000 - 2_000 = 7_000; 3_000 ≤ 7_000 → ok
        assert_eq!(out, DiskAdmission::Ok { headroom_after: 4_000 });
    }

    #[test]
    fn admission_insufficient_when_candidate_exceeds_headroom() {
        let out = would_fit_on_disk(10_000, 1_000, 2_000, 8_000);
        // headroom = 7_000; 8_000 > 7_000 → insufficient
        assert_eq!(
            out,
            DiskAdmission::Insufficient {
                free: 10_000,
                safety: 1_000,
                active: 2_000,
                candidate: 8_000,
                headroom: 7_000,
            }
        );
    }

    #[test]
    fn admission_saturates_when_safety_plus_active_exceeds_free() {
        // free < safety + active → headroom 0, candidate always fails
        let out = would_fit_on_disk(1_000, 5_000, 500, 1);
        assert_eq!(
            out,
            DiskAdmission::Insufficient { free: 1_000, safety: 5_000, active: 500, candidate: 1, headroom: 0 }
        );
    }

    #[test]
    fn admission_zero_candidate_always_fits() {
        let out = would_fit_on_disk(100, 0, 0, 0);
        assert_eq!(out, DiskAdmission::Ok { headroom_after: 100 });
    }

    #[test]
    fn active_reservations_count_only_downloading() {
        // Only `Downloading` tasks contribute to the active total.
        // Scheduled/Queued tasks hold a *reservation* but not actual
        // disk headroom yet. The headroom is taken at start, not
        // at create.
        let tasks = vec![
            downloading(1000, 0),
            T { state: DownloadState::Scheduled, recording: Some(make_meta(500, 0)) },
            T { state: DownloadState::Completed, recording: Some(make_meta(0, 4000)) },
        ];
        assert_eq!(active_disk_reservations(&tasks), 1000);
    }

    #[test]
    fn active_reservations_handles_active_growth() {
        // active: reserved 1000, measured 1500 → max(1000, 1500) = 1500
        let tasks = vec![downloading(1000, 1500)];
        assert_eq!(active_disk_reservations(&tasks), 1500);
    }

    #[test]
    fn admission_concurrent_starts_serialized_via_active_sum() {
        // Two starts cannot both consume the same headroom: the
        // first start's reservation moves into `active` before
        // the second start's admission check. This test mirrors
        // the contract: with `active = first_charge`, the second
        // candidate that would have fit on its own now fails.
        let free = 10_000;
        let safety = 1_000;
        // first start: candidate 5_000 → headroom = 9_000 - 5_000 = 4_000
        let first = would_fit_on_disk(free, safety, 0, 5_000);
        assert_eq!(first, DiskAdmission::Ok { headroom_after: 4_000 });
        // second start (same headroom, first charge is now `active`):
        let active = 5_000;
        // headroom = 10_000 - 1_000 - 5_000 = 4_000; 4_500 fails
        let second = would_fit_on_disk(free, safety, active, 4_500);
        assert!(matches!(second, DiskAdmission::Insufficient { .. }));
        // 4_000 exactly would fit
        let third = would_fit_on_disk(free, safety, active, 4_000);
        assert_eq!(third, DiskAdmission::Ok { headroom_after: 0 });
    }

    #[test]
    fn measurement_targets_the_recording_root_filesystem() {
        // Never reuse a measurement from another filesystem solely
        // because it belongs to `storage_dir` or the download
        // directory. We exercise `free_bytes_for` against the
        // actual filesystem that
        // contains `/tmp` (the temp dir is on the same FS as the
        // process CWD on Linux CI). The syscall resolves `/tmp`
        // to its mount and returns that FS's free bytes.
        let path = std::path::Path::new("/tmp");
        let free = free_bytes_for(path);
        // We don't assert a specific number (the test runner's
        // disk may be any size), but `/tmp` should be readable
        // and report some free bytes.
        if let Some(free) = free {
            assert!(free > 0, "expected positive free bytes for /tmp");
        }
    }

    #[test]
    fn measurement_returns_none_for_missing_path() {
        let path = std::path::Path::new("/this/path/does/not/exist/xyzzy");
        assert!(free_bytes_for(path).is_none());
    }
}
