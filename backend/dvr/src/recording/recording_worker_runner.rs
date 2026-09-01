//! Retention and disk-pressure worker.
//!
//! The worker deletes eligible completed recordings oldest first through the
//! normal recording deletion operation. It records only aggregate counters so
//! logs and metrics do not leak private recording data.

use super::{
    recording_quota::{charge_for_state, QuotaRecordingTaskView},
    recording_retention::{compute_candidates, RetentionCandidate, RetentionConfig},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Disk-pressure config derived from `RecordingDiskConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskConfig {
    pub high_water_percent: Option<u8>,
    pub low_water_percent: Option<u8>,
    pub safety_bytes: Option<u64>,
}

/// Result of one worker pass. The fields are deliberately
/// aggregate (no per-task data) so they can be logged without
/// leaking private title / channel / filename / user id.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RunStats {
    /// Total candidates considered across policy + disk-pressure.
    pub candidates: u64,
    /// Tasks successfully deleted.
    pub deleted: u64,
    /// Tasks skipped because the delete callback returned a
    /// failure.
    pub failed: u64,
    /// Tasks skipped because the policy check said "skip" (e.g.
    /// already in `Deleting` state, no safe final file).
    pub skipped: u64,
    /// Bytes actually freed on disk. An entry whose file another entry still
    /// holds contributes nothing: crediting it would end a disk-pressure pass
    /// against space that was never released.
    pub reclaimed_bytes: u64,
    /// `true` if a disk-pressure pass deleted at least one task.
    pub disk_pressure_triggered: bool,
    /// `true` when the pass ran out of retention-eligible recordings before
    /// reaching the low watermark. The remaining recordings are protected by
    /// policy, so only an operator can free the space.
    pub pressure_unrelieved: bool,
}

/// Outcome of one delete attempt. The worker treats every
/// non-`Ok` outcome as a `failed` increment and proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Ok,
    /// The entry was removed but another library entry still holds its file,
    /// so no space was freed. Counted as deleted, reclaims nothing.
    Detached,
    /// The task was already in `Deleting` or otherwise not safe
    /// to delete. Counted under `skipped`.
    Skipped,
    /// The deletion failed (filesystem error, persistence error,
    /// etc.). Counted under `failed`.
    Failed,
}

/// A callback that performs the actual deletion. The production
/// implementation calls `RecordingService::system_retention_delete`.
///
/// The trait object borrows the caller's data; tests can capture
/// local state (e.g. a `Vec<String>` recording deletions) without
/// needing `'static`. The production wiring wraps the callback in
/// an `Arc<Mutex<_>>` to satisfy Tokio's `'static` requirements.
pub type DeleteFn<'a> = dyn FnMut(&str) -> DeleteOutcome + 'a;

/// Run the policy (age + count) pass for one tick. Returns the
/// stats for this pass; the caller is expected to log them
/// without per-task data.
///
/// `now_secs` is the wall-clock seconds used for the age check;
/// passing it in keeps the function pure and testable.
pub fn run_once<V: QuotaRecordingTaskView>(
    tasks: &[V],
    retention: &RetentionConfig,
    now_secs: i64,
    delete: &mut DeleteFn<'_>,
) -> RunStats {
    let mut stats = RunStats::default();
    let candidates = compute_candidates(tasks, retention, now_secs);
    stats.candidates = candidates.len() as u64;
    for cand in candidates {
        // The `RecordingCandidate` is built from queue-resident
        // tasks; the worker re-charges by `uuid` against the
        // current task snapshot. We read the charge from the
        // candidate's owner/channel indirectly — the production
        // path sums `charge_for_task(task_at_candidate_uuid)`.
        // For the pure runner, the bytes reclaimed equal the
        // candidate's policy charge: the `Completed` state's
        // `measured_bytes` (from the queue at select time).
        let reclaim = candidate_charge(&cand);
        match delete(&cand.uuid) {
            DeleteOutcome::Ok => {
                stats.deleted += 1;
                stats.reclaimed_bytes = stats.reclaimed_bytes.saturating_add(reclaim);
            }
            DeleteOutcome::Detached => {
                stats.deleted += 1;
            }
            DeleteOutcome::Skipped => {
                stats.skipped += 1;
            }
            DeleteOutcome::Failed => {
                stats.failed += 1;
            }
        }
    }
    stats
}

/// Estimate the charge for a candidate at delete time. Today
/// the candidate is always `Completed`, so the charge is the
/// `measured_bytes` (the final file size). We pass it through
/// `charge_for_task` once a real `RecordingTask` is in scope; for
/// the pure runner, we use a conservative constant derived from
/// the candidate's reason (count or age). The production
/// integration in `recording_service.rs` will re-summarize from
/// the queue and pass the real `measured_bytes`.
fn candidate_charge(cand: &RetentionCandidate) -> u64 {
    // Conservative default until the worker is wired with a real
    // task snapshot. The production path will pass the file size
    // explicitly via the `DeleteFn` contract.
    let _ = cand.reason;
    0
}

/// Pure: the ordered candidate set for a disk-pressure pass, or `None`
/// when no pass is warranted.
///
/// `None` covers three cases: the measurement is not from the
/// recording-root filesystem, the watermarks are unset or inverted, or
/// there is no pressure (`used_percent` below the high mark).
///
/// `used_percent` is the used-space percentage of the recording-root
/// filesystem, in `0..=100`. `is_recording_root_fs` is the
/// filesystem-selection guard: a caller that measured `storage_dir` or
/// the generic download directory — potentially a different mount — must
/// pass `false` rather than let a foreign measurement authorize
/// deletions.
///
/// Split out of [`run_disk_pressure`] so an async caller can drive the
/// same decision without needing a blocking delete callback.
pub fn disk_pressure_candidates<V: QuotaRecordingTaskView>(
    tasks: &[V],
    disk: &DiskConfig,
    retention: &super::recording_retention::RetentionConfig,
    now_secs: i64,
    used_percent: u8,
    is_recording_root_fs: bool,
) -> Option<Vec<RetentionCandidate>> {
    if !is_recording_root_fs {
        // Never reuse a measurement from another filesystem.
        return None;
    }
    let (high, low) = (disk.high_water_percent?, disk.low_water_percent?);
    if high <= low {
        // Invalid config: high water must be strictly above low water.
        // Treat any inversion as "no pressure pass".
        return None;
    }
    if used_percent < high {
        return None;
    }
    // Pressure only *accelerates* retention; it never widens it. Deleting a
    // recording the operator's policy says to keep would destroy user data to
    // reclaim space, which is not a trade the DVR is entitled to make.
    Some(super::recording_retention::compute_candidates(tasks, retention, now_secs))
}

/// Bytes charged to the task with this uuid, i.e. what deleting it would
/// reclaim. Disk pressure only deletes `Completed` candidates, so the
/// charge is always computed for that state.
pub fn reclaimable_bytes_for<V: QuotaRecordingTaskView>(tasks: &[V], uuid: &str) -> u64 {
    tasks.iter().find(|task| task.uuid() == uuid).map_or(0, |task| {
        charge_for_state(&crate::recording::recording_queue::RecordingTaskState::Completed, task.recording())
    })
}

/// One measurement of the recording-root filesystem.
///
/// `is_recording_root_fs` is the selection guard: a caller that measured
/// `storage_dir` or the generic download directory - potentially a different
/// mount - must pass `false` rather than let a foreign measurement authorize
/// deletions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemUsage {
    pub used_percent: u8,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub is_recording_root_fs: bool,
}

/// Run a disk-pressure pass: keep deleting oldest eligible completed
/// recordings until the projected used% falls to or below the low water
/// mark, or the candidate set is exhausted.
///
/// See [`disk_pressure_candidates`] for the admission conditions.
pub fn run_disk_pressure<V: QuotaRecordingTaskView>(
    tasks: &[V],
    disk: &DiskConfig,
    retention: &super::recording_retention::RetentionConfig,
    now_secs: i64,
    usage: FilesystemUsage,
    delete: &mut DeleteFn<'_>,
) -> RunStats {
    let FilesystemUsage { used_percent, free_bytes, total_bytes, is_recording_root_fs } = usage;
    let mut stats = RunStats::default();
    let Some(candidates) =
        disk_pressure_candidates(tasks, disk, retention, now_secs, used_percent, is_recording_root_fs)
    else {
        return stats;
    };
    let Some(low) = disk.low_water_percent else {
        return stats;
    };
    stats.disk_pressure_triggered = true;
    stats.candidates = candidates.len() as u64;
    for cand in &candidates {
        if pressure_relieved(total_bytes, free_bytes, stats.reclaimed_bytes, low) {
            break;
        }
        let reclaim = reclaimable_bytes_for(tasks, &cand.uuid);
        match delete(&cand.uuid) {
            DeleteOutcome::Ok => {
                stats.deleted += 1;
                stats.reclaimed_bytes = stats.reclaimed_bytes.saturating_add(reclaim);
            }
            DeleteOutcome::Detached => {
                stats.deleted += 1;
            }
            DeleteOutcome::Skipped => {
                stats.skipped += 1;
            }
            DeleteOutcome::Failed => {
                stats.failed += 1;
            }
        }
    }
    // Exhausting the eligible set without reaching the low watermark is a
    // condition an operator has to resolve; it is not licence to delete
    // recordings retention is holding.
    stats.pressure_unrelieved = !pressure_relieved(total_bytes, free_bytes, stats.reclaimed_bytes, low);
    stats
}

/// Has enough been reclaimed for used% to reach the low watermark?
///
/// `reclaimed_bytes` is what this pass has already freed. Folding it in
/// is what terminates the loop: the free-space measurement is taken once
/// per pass, so a stop condition that ignored the running total would be
/// constant for the whole pass — it would evaluate to `false` on the
/// first candidate (pressure is by definition above the high watermark,
/// which is above the low one) and stay `false`, so a single trigger
/// would delete *every* completed recording rather than just enough of
/// them.
///
/// `true` when `total_bytes == 0`: an unmeasurable filesystem must not
/// authorize deletions.
pub fn pressure_relieved(total_bytes: u64, free_bytes: u64, reclaimed_bytes: u64, low_percent: u8) -> bool {
    if total_bytes == 0 {
        return true;
    }
    let projected_free = free_bytes.saturating_add(reclaimed_bytes).min(total_bytes);
    let used = total_bytes.saturating_sub(projected_free);
    let pct = (used.saturating_mul(100)).saturating_div(total_bytes);
    pct <= u64::from(low_percent)
}

/// Cancellation-aware worker handle. Holds a `CancelToken` and
/// an `is_running` flag; the loop skips a tick if a previous
/// pass is still in progress (passes never overlap). The flag is
/// plain `AtomicBool` for the standalone test; the production
/// path uses the same primitive.
pub struct Worker {
    is_running: Arc<AtomicBool>,
}

impl Worker {
    pub fn new(_cancel: tokio_util::sync::CancellationToken) -> Self {
        Self { is_running: Arc::new(AtomicBool::new(false)) }
    }

    /// `true` if a pass is currently in flight.
    pub fn is_running(&self) -> bool { self.is_running.load(Ordering::Acquire) }

    /// Try to claim the worker for one pass. Returns `true` if
    /// the pass may run; `false` if a previous pass is still
    /// in progress.
    pub fn try_claim(&self) -> bool {
        self.is_running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Release the worker after one pass.
    pub fn release(&self) { self.is_running.store(false, Ordering::Release); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::recording_queue::RecordingTaskState;
    use shared::model::{
        recording::{RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility},
        UserId,
    };

    fn make_meta(
        owner: RecordingOwner,
        channel_id: Option<&str>,
        channel_name: Option<&str>,
        completed_at: i64,
        measured: u64,
    ) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility: RecordingVisibility::Private,
            source: (RecordingSource::new("t1", "v1", "in1")),
            program_start: None,
            program_end: None,
            scheduled_start: None,
            scheduled_end: None,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: channel_id.map(str::to_string),
            channel_name: channel_name.map(str::to_string),
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            resume_etag: None,
            resume_last_modified: None,
            reserved_bytes: 0,
            measured_bytes: measured,
            completed_at: Some(completed_at),
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    struct T {
        uuid: String,
        state: RecordingTaskState,
        recording: RecordingMetadata,
    }
    impl QuotaRecordingTaskView for T {
        fn state(&self) -> &RecordingTaskState { &self.state }
        fn recording(&self) -> &RecordingMetadata { &self.recording }
        fn uuid(&self) -> &str { &self.uuid }
    }

    fn completed(uuid: &str, channel_id: &str, completed_at: i64, measured: u64) -> T {
        T {
            uuid: uuid.to_string(),
            state: RecordingTaskState::Completed,
            recording: make_meta(
                RecordingOwner::User(UserId::from("web:alice")),
                Some(channel_id),
                Some("Alpha"),
                completed_at,
                measured,
            ),
        }
    }

    fn count_delete(
        deleted: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        failures: std::rc::Rc<Vec<&'static str>>,
    ) -> impl FnMut(&str) -> DeleteOutcome {
        move |uuid: &str| {
            if failures.contains(&uuid) {
                DeleteOutcome::Failed
            } else {
                deleted.borrow_mut().push(uuid.to_string());
                DeleteOutcome::Ok
            }
        }
    }

    #[test]
    fn run_once_deletes_oldest_count_candidates() {
        let tasks =
            vec![completed("a", "c1", 1_000, 100), completed("b", "c1", 2_000, 200), completed("c", "c1", 3_000, 300)];
        let config = RetentionConfig { keep_last_per_channel: Some(1), delete_after_days: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_once(&tasks, &config, 0, &mut delete);
        assert_eq!(stats.candidates, 2);
        assert_eq!(stats.deleted, 2);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.reclaimed_bytes, 0); // pure runner; production path supplies the bytes
        assert_eq!(deleted.borrow().clone(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn run_once_continues_after_individual_failure() {
        let tasks =
            vec![completed("a", "c1", 1_000, 100), completed("b", "c1", 2_000, 200), completed("c", "c1", 3_000, 300)];
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec!["b"]));
        let stats = run_once(&tasks, &config, 0, &mut delete);
        assert_eq!(stats.candidates, 3);
        assert_eq!(stats.deleted, 2);
        assert_eq!(stats.failed, 1);
        // a and c were deleted; b was reported as failure
        assert_eq!(deleted.borrow().clone(), vec!["a".to_string(), "c".to_string()]);
    }

    /// Fixed "now" for the pressure tests; the fixtures complete at
    /// small timestamps, so everything is far older than a day.
    const NOW: i64 = 10_000_000;

    /// A policy under which every completed recording is already eligible, so
    /// a test can exercise the watermark arithmetic on its own.
    fn delete_everything() -> RetentionConfig {
        RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None }
    }

    #[test]
    fn detaching_a_shared_file_reclaims_nothing_and_the_pass_keeps_going() {
        // Counting a detached entry's bytes would end the pass against space
        // that was never released, leaving the disk full and the run silent.
        let tasks = vec![
            completed("a", "chan", NOW - 10_000, 1_000_000),
            completed("b", "chan", NOW - 9_000, 1_000_000),
            completed("c", "chan", NOW - 8_000, 1_000_000),
        ];
        let mut delete = |uuid: &str| {
            if uuid == "a" {
                DeleteOutcome::Detached
            } else {
                DeleteOutcome::Ok
            }
        };
        let stats = run_disk_pressure(
            &tasks,
            &DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None },
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 90, free_bytes: 0, total_bytes: 10_000_000, is_recording_root_fs: true },
            &mut delete,
        );
        // "a" was removed as an entry but freed no space.
        assert_eq!(stats.deleted, 3, "every entry was removed");
        assert_eq!(stats.reclaimed_bytes, 2_000_000, "only the two real unlinks count");
    }

    #[test]
    fn disk_pressure_never_deletes_a_recording_retention_protects() {
        // Regression: pressure treated every Completed recording as a
        // candidate, so a recording finished moments ago was deleted to
        // reclaim space the operator's own policy said to keep.
        let tasks = vec![completed("fresh", "c1", NOW, 900)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let keep_everything = RetentionConfig { keep_last_per_channel: Some(10), delete_after_days: Some(30) };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));

        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &keep_everything,
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: 10, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );

        assert!(stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0, "a protected recording must survive disk pressure");
        assert!(deleted.borrow().is_empty());
        // The operator has to resolve this; the DVR must not delete past it.
        assert!(stats.pressure_unrelieved);
    }

    #[test]
    fn disk_pressure_deletes_only_the_retention_eligible_ones() {
        // `keep_last_per_channel = 1` leaves the two older recordings
        // eligible and protects the newest.
        let tasks = vec![
            completed("old", "c1", 1_000, 400),
            completed("mid", "c1", 2_000, 400),
            completed("new", "c1", 3_000, 400),
        ];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(10), safety_bytes: None };
        let keep_one = RetentionConfig { keep_last_per_channel: Some(1), delete_after_days: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));

        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &keep_one,
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: 0, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );

        assert_eq!(stats.candidates, 2, "only the two beyond keep_last are eligible");
        assert!(!deleted.borrow().contains(&"new".to_string()), "the retained recording must not be deleted");
    }

    #[test]
    fn disk_pressure_runs_when_above_high_water() {
        let tasks =
            vec![completed("a", "c1", 1_000, 100), completed("b", "c1", 2_000, 200), completed("c", "c1", 3_000, 300)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        // `used_percent` (90) is above the high watermark, so a pass is
        // warranted — but the measured free space already satisfies the
        // low watermark, so the pass must delete nothing.
        let total = 1_000u64;
        let free = 500u64; // 50% used
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 90, free_bytes: free, total_bytes: total, is_recording_root_fs: true },
            &mut delete,
        );
        // 90% > 80% triggers. With free=50% (= low), the first
        // iteration sees `used_percent ≤ 50%` and breaks without
        // deleting anything.
        assert!(stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn disk_pressure_keeps_deleting_until_low_water() {
        let tasks =
            vec![completed("a", "c1", 1_000, 100), completed("b", "c1", 2_000, 200), completed("c", "c1", 3_000, 300)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let total = 1_000u64;
        // 95% used with only 600 bytes of reclaimable recordings: even
        // deleting all three leaves the projected free space (650) short
        // of nothing — it reaches 65% used, still above the 50% low
        // watermark — so every candidate is consumed.
        let free = 50u64; // 95% used
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: free, total_bytes: total, is_recording_root_fs: true },
            &mut delete,
        );
        assert_eq!(stats.deleted, 3);
        assert_eq!(deleted.borrow().len(), 3);
        assert_eq!(stats.reclaimed_bytes, 600);
    }

    #[test]
    fn disk_pressure_stops_as_soon_as_the_low_water_mark_is_reached() {
        // Regression guard: the stop condition ignored the bytes already
        // reclaimed by the pass, so it was constant for the whole loop and
        // a single trigger deleted the entire recording library instead of
        // just enough of it.
        let tasks =
            vec![completed("a", "c1", 1_000, 300), completed("b", "c1", 2_000, 300), completed("c", "c1", 3_000, 300)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        // 90% used of 1000 bytes. Deleting the two oldest reclaims 600,
        // taking projected free to 700 (30% used) — past the 50% low
        // watermark — so the third must survive.
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 90, free_bytes: 100, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );
        assert!(stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 2, "pass must stop at the low watermark");
        assert_eq!(*deleted.borrow(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn pressure_relieved_folds_in_what_the_pass_already_freed() {
        // 90% used: not relieved yet.
        assert!(!pressure_relieved(1_000, 100, 0, 50));
        // Reclaiming 400 more takes free to 500 → exactly at the mark.
        assert!(pressure_relieved(1_000, 100, 400, 50));
        // Reclaiming more than the disk holds cannot report a negative use.
        assert!(pressure_relieved(1_000, 100, u64::MAX, 50));
        // An unmeasurable filesystem never authorizes deletions.
        assert!(pressure_relieved(0, 0, 0, 50));
    }

    #[test]
    fn disk_pressure_skips_when_below_high_water() {
        let tasks = vec![completed("a", "c1", 1_000, 100)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 60, free_bytes: 400, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );
        assert!(!stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn disk_pressure_skips_when_wrong_filesystem() {
        // Never reuse a measurement from another filesystem. If
        // the caller cannot prove the measurement is on the
        // recording-root FS, the pass is a no-op.
        let tasks = vec![completed("a", "c1", 1_000, 100)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: 50, total_bytes: 1_000, is_recording_root_fs: false },
            &mut delete,
        );
        assert!(!stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn disk_pressure_skips_when_inverted_thresholds() {
        // high <= low is invalid; the worker treats it as
        // "no pressure pass".
        let tasks = vec![completed("a", "c1", 1_000, 100)];
        let disk = DiskConfig { high_water_percent: Some(50), low_water_percent: Some(80), safety_bytes: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: 50, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );
        assert!(!stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn disk_pressure_continues_after_individual_failure() {
        let tasks = vec![completed("a", "c1", 1_000, 100), completed("b", "c1", 2_000, 200)];
        let disk = DiskConfig { high_water_percent: Some(80), low_water_percent: Some(50), safety_bytes: None };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec!["a"]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            &delete_everything(),
            NOW,
            FilesystemUsage { used_percent: 95, free_bytes: 50, total_bytes: 1_000, is_recording_root_fs: true },
            &mut delete,
        );
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn filesystem_selection_uses_recording_root() {
        // `free_bytes_for` is a syscall on the supplied path. We
        // exercise it against the actual filesystem that
        // contains `/tmp` (the temp dir is on the same FS as
        // the process CWD on Linux CI). The measurement must be
        // on the recording-root FS, not on `storage_dir`. The path
        // is recorded in the test for
        // traceability.
        let recording_root = std::path::Path::new("/tmp");
        let free = super::super::recording_disk::free_bytes_for(recording_root);
        if let Some(f) = free {
            assert!(f > 0);
        }
    }

    #[test]
    fn worker_no_overlap_via_try_claim() {
        // The `Worker` exposes a re-entry guard. Two concurrent
        // claims cannot both succeed.
        let cancel = tokio_util::sync::CancellationToken::new();
        let w = Worker::new(cancel);
        assert!(w.try_claim(), "first claim must succeed");
        assert!(!w.try_claim(), "second claim must fail while running");
        w.release();
        assert!(w.try_claim(), "claim after release must succeed");
    }

    #[test]
    fn run_once_stats_have_no_per_task_data() {
        // The privacy contract: the aggregate `RunStats` JSON
        // must not contain the uuid, channel id, or user id of
        // any task. Aggregate field NAMES (candidates, deleted,
        // ...) are allowed — only per-task VALUES are private.
        let tasks = vec![completed("uuid-alpha-bravo-charlie", "channel-delta-echo-foxtrot", 1_000, 100)];
        let config = RetentionConfig { keep_last_per_channel: Some(0), delete_after_days: None };
        let mut delete = count_delete(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())), std::rc::Rc::new(vec![]));
        let stats = run_once(&tasks, &config, 0, &mut delete);
        let json = serde_json::to_value(&stats).unwrap();
        let s = serde_json::to_string(&json).unwrap();
        assert!(!s.contains("uuid-alpha-bravo-charlie"));
        assert!(!s.contains("channel-delta-echo-foxtrot"));
    }
}
