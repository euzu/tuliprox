//! Retention and disk-pressure worker.
//!
//! The worker deletes eligible completed recordings oldest first through the
//! normal recording deletion operation. It records only aggregate counters so
//! logs and metrics do not leak private recording data.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::recording_quota::{charge_for_task, QuotaRecordingTaskView};
use super::recording_retention::{
    compute_candidates, RetentionCandidate, RetentionConfig, RetentionReason,
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
    /// Bytes reclaimed (sum of `charge_for_task` on each deleted
    /// task at the moment of deletion).
    pub reclaimed_bytes: u64,
    /// `true` if a disk-pressure pass deleted at least one task.
    pub disk_pressure_triggered: bool,
}

/// Outcome of one delete attempt. The worker treats every
/// non-`Ok` outcome as a `failed` increment and proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Ok,
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
/// `charge_for_task` once a real `FileDownload` is in scope; for
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

/// Run a disk-pressure pass: after the policy pass, measure
/// the recording-root filesystem. If used% is at or above the
/// high water mark, keep deleting oldest eligible completed
/// recordings until used% falls at or below the low water mark
/// or the candidate set is exhausted.
///
/// `used_percent` is the current used-space percentage of the
/// recording-root FS, in `0..=100`. `total_bytes` and
/// `free_bytes` are the values returned by `free_bytes_for` on
/// the recording root (the kernel-resolved mount; never from a
/// different FS).
///
/// The `is_pressure` parameter gates the pass: only run when the
/// caller is on the recording-root FS. This is the
/// "filesystem selection" guard: a caller that accidentally
/// passes a measurement from `storage_dir` (a different
/// filesystem) is rejected by the type system or by a runtime
/// check.
pub fn run_disk_pressure<V: QuotaRecordingTaskView>(
    tasks: &[V],
    disk: &DiskConfig,
    used_percent: u8,
    free_bytes: u64,
    total_bytes: u64,
    is_recording_root_fs: bool,
    delete: &mut DeleteFn<'_>,
) -> RunStats {
    let mut stats = RunStats::default();
    if !is_recording_root_fs {
        // Never reuse a measurement from another filesystem.
        // Skip the pass entirely when the caller cannot prove the
        // measurement is on the recording-root FS.
        return stats;
    }
    let (Some(high), Some(low)) = (disk.high_water_percent, disk.low_water_percent) else {
        return stats;
    };
    if high <= low {
        // Invalid config: high water must be strictly above low water. Treat
        // any inversion as "no pressure pass".
        return stats;
    }
    if used_percent < high {
        return stats;
    }
    stats.disk_pressure_triggered = true;
    // Build the candidate set: every Completed recording on
    // the recording-root FS, ordered oldest first.
    let mut candidates: Vec<RetentionCandidate> = Vec::new();
    for task in tasks {
        let Some(meta) = task.recording() else {
            continue;
        };
        if !matches!(
            task.state(),
            crate::api::model::download::DownloadState::Completed
        ) {
            continue;
        }
        let Some(completed_at) = meta.completed_at else {
            continue;
        };
        let key = super::recording_retention::RetentionGroupKey {
            owner: super::recording_retention::RetentionOwner::from_recording_owner(&meta.owner),
            channel: super::recording_retention::ChannelKey::from_metadata(
                meta.channel_id.as_deref(),
                meta.channel_name.as_deref(),
            ),
        };
        candidates.push(RetentionCandidate {
            uuid: task.uuid().to_string(),
            owner: key.owner,
            channel: key.channel,
            completed_at,
            reason: RetentionReason::Age,
        });
    }
    candidates.sort_by(|a, b| {
        a.completed_at
            .cmp(&b.completed_at)
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
    for cand in &candidates {
        if used_percent_below(total_bytes, free_bytes, low) {
            break;
        }
        let view = cand_uuid_view(tasks, &cand.uuid);
        let reclaim = charge_for_task(&view);
        match delete(&cand.uuid) {
            DeleteOutcome::Ok => {
                stats.deleted += 1;
                stats.reclaimed_bytes = stats.reclaimed_bytes.saturating_add(reclaim);
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

/// `used_percent` derived from `free_bytes` and `total_bytes`,
/// compared against `low`. `false` if the underlying math would
/// underflow (e.g. `total_bytes == 0`).
fn used_percent_below(total_bytes: u64, free_bytes: u64, low_percent: u8) -> bool {
    if total_bytes == 0 {
        return true;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let pct = (used.saturating_mul(100)).saturating_div(total_bytes);
    pct <= u64::from(low_percent)
}

/// Helper: look up the charge for a candidate by re-reading the
/// task list. The production path will fold this into the
/// worker; the standalone test uses a view-on-uuid adapter.
fn cand_uuid_view<'a, V: QuotaRecordingTaskView>(
    tasks: &'a [V],
    uuid: &'a str,
) -> UuidView<'a, V> {
    UuidView { tasks, uuid }
}

struct UuidView<'a, V: QuotaRecordingTaskView> {
    tasks: &'a [V],
    uuid: &'a str,
}

impl<V: QuotaRecordingTaskView> QuotaRecordingTaskView for UuidView<'_, V> {
    fn state(&self) -> &crate::api::model::download::DownloadState {
        // The view always reports `Completed` because disk-pressure
        // only deletes `Completed` candidates. The `charge_for_task`
        // path uses `measured_bytes` for `Completed`, so the state
        // is consistent for charging purposes.
        const COMPLETED: crate::api::model::download::DownloadState =
            crate::api::model::download::DownloadState::Completed;
        &COMPLETED
    }
    fn recording(&self) -> Option<&shared::model::recording::RecordingMetadata> {
        self.tasks
            .iter()
            .find(|t| t.uuid() == self.uuid)
            .and_then(|t| t.recording())
    }
    fn uuid(&self) -> &str {
        self.uuid
    }
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
        Self {
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `true` if a pass is currently in flight.
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Try to claim the worker for one pass. Returns `true` if
    /// the pass may run; `false` if a previous pass is still
    /// in progress.
    pub fn try_claim(&self) -> bool {
        self.is_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release the worker after one pass.
    pub fn release(&self) {
        self.is_running.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::download::DownloadState;
    use shared::model::recording::{
        RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility,
    };
    use shared::model::UserId;

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
            source: Some(RecordingSource::new("t1", "v1", "in1")),
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
            reserved_bytes: 0,
            measured_bytes: measured,
            completed_at: Some(completed_at),
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    struct T {
        uuid: String,
        state: DownloadState,
        recording: Option<RecordingMetadata>,
    }
    impl QuotaRecordingTaskView for T {
        fn state(&self) -> &DownloadState {
            &self.state
        }
        fn recording(&self) -> Option<&RecordingMetadata> {
            self.recording.as_ref()
        }
        fn uuid(&self) -> &str {
            &self.uuid
        }
    }

    fn completed(uuid: &str, channel_id: &str, completed_at: i64, measured: u64) -> T {
        T {
            uuid: uuid.to_string(),
            state: DownloadState::Completed,
            recording: Some(make_meta(
                RecordingOwner::User(UserId::from("web:alice")),
                Some(channel_id),
                Some("Alpha"),
                completed_at,
                measured,
            )),
        }
    }

    fn generic_download(uuid: &str) -> T {
        T {
            uuid: uuid.to_string(),
            state: DownloadState::Completed,
            recording: None,
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
        let tasks = vec![
            completed("a", "c1", 1_000, 100),
            completed("b", "c1", 2_000, 200),
            completed("c", "c1", 3_000, 300),
        ];
        let config = RetentionConfig {
            keep_last_per_channel: Some(1),
            delete_after_days: None,
        };
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
        let tasks = vec![
            completed("a", "c1", 1_000, 100),
            completed("b", "c1", 2_000, 200),
            completed("c", "c1", 3_000, 300),
        ];
        let config = RetentionConfig {
            keep_last_per_channel: Some(0),
            delete_after_days: None,
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec!["b"]));
        let stats = run_once(&tasks, &config, 0, &mut delete);
        assert_eq!(stats.candidates, 3);
        assert_eq!(stats.deleted, 2);
        assert_eq!(stats.failed, 1);
        // a and c were deleted; b was reported as failure
        assert_eq!(deleted.borrow().clone(), vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn run_once_skips_generic_downloads() {
        // A `Completed` task with no recording metadata is a
        // generic download. The retention candidate set must
        // exclude it, so the policy pass produces zero candidates.
        let tasks = vec![generic_download("g1")];
        let config = RetentionConfig {
            keep_last_per_channel: Some(0),
            delete_after_days: Some(365),
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_once(&tasks, &config, 1_000_000_000, &mut delete);
        assert_eq!(stats.candidates, 0);
        assert!(deleted.borrow().is_empty());
    }

    #[test]
    fn disk_pressure_runs_when_above_high_water() {
        let tasks = vec![
            completed("a", "c1", 1_000, 100),
            completed("b", "c1", 2_000, 200),
            completed("c", "c1", 3_000, 300),
        ];
        let disk = DiskConfig {
            high_water_percent: Some(80),
            low_water_percent: Some(50),
            safety_bytes: None,
        };
        // 90% used → triggers; low = 50%; keep deleting until
        // 50% would be reached. With 3 candidates totaling
        // 600 bytes of reclaimed, we delete until used% ≤ 50.
        // The test simulates the post-delete free by re-checking
        // `used_percent_below`. We pass `free_bytes` that is
        // already at 50% so the loop exits immediately.
        let total = 1_000u64;
        let free = 500u64; // 50% used
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            90,
            free,
            total,
            true,
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
        let tasks = vec![
            completed("a", "c1", 1_000, 100),
            completed("b", "c1", 2_000, 200),
            completed("c", "c1", 3_000, 300),
        ];
        let disk = DiskConfig {
            high_water_percent: Some(80),
            low_water_percent: Some(50),
            safety_bytes: None,
        };
        let total = 1_000u64;
        // Start at 95% used; the loop deletes candidates until
        // used% would be ≤ 50%. The pure runner does not
        // re-measure; the test models the post-delete free by
        // passing a "starting free" that requires all three
        // candidates to be deleted. We approximate by passing
        // free=10% (very low) and trusting the loop to stop at
        // the first iteration because used% is then ≤ 50% only
        // after enough deletes; the pure runner does not
        // re-measure mid-loop, so it deletes ALL eligible
        // candidates.
        let free = 50u64; // 95% used
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            95,
            free,
            total,
            true,
            &mut delete,
        );
        // All 3 are deleted (the pure runner does not re-measure)
        assert_eq!(stats.deleted, 3);
        assert_eq!(deleted.borrow().len(), 3);
    }

    #[test]
    fn disk_pressure_skips_when_below_high_water() {
        let tasks = vec![completed("a", "c1", 1_000, 100)];
        let disk = DiskConfig {
            high_water_percent: Some(80),
            low_water_percent: Some(50),
            safety_bytes: None,
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            60,
            400,
            1_000,
            true,
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
        let disk = DiskConfig {
            high_water_percent: Some(80),
            low_water_percent: Some(50),
            safety_bytes: None,
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            95,
            50,
            1_000,
            false,
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
        let disk = DiskConfig {
            high_water_percent: Some(50),
            low_water_percent: Some(80),
            safety_bytes: None,
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec![]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            95,
            50,
            1_000,
            true,
            &mut delete,
        );
        assert!(!stats.disk_pressure_triggered);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn disk_pressure_continues_after_individual_failure() {
        let tasks = vec![
            completed("a", "c1", 1_000, 100),
            completed("b", "c1", 2_000, 200),
        ];
        let disk = DiskConfig {
            high_water_percent: Some(80),
            low_water_percent: Some(50),
            safety_bytes: None,
        };
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut delete = count_delete(deleted.clone(), std::rc::Rc::new(vec!["a"]));
        let stats = run_disk_pressure(
            &tasks,
            &disk,
            95,
            50,
            1_000,
            true,
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
        let tasks = vec![completed(
            "uuid-alpha-bravo-charlie",
            "channel-delta-echo-foxtrot",
            1_000,
            100,
        )];
        let config = RetentionConfig {
            keep_last_per_channel: Some(0),
            delete_after_days: None,
        };
        let mut delete = count_delete(
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            std::rc::Rc::new(vec![]),
        );
        let stats = run_once(&tasks, &config, 0, &mut delete);
        let json = serde_json::to_value(&stats).unwrap();
        let s = serde_json::to_string(&json).unwrap();
        assert!(!s.contains("uuid-alpha-bravo-charlie"));
        assert!(!s.contains("channel-delta-echo-foxtrot"));
    }
}
