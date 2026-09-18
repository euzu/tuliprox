//! Scheduler heartbeat and an independent watchdog thread.
//!
//! A wedged Tokio runtime parks every worker on a futex and leaves the I/O
//! driver in `epoll_wait`; at the OS level that is indistinguishable from an
//! idle runtime. No task inside the runtime can report that state, so the
//! detection is split in two:
//!
//! * a heartbeat task records monotonic progress only while the scheduler
//!   still runs, and
//! * a plain `std::thread`, outside the runtime, watches the heartbeat age and
//!   logs a diagnostic snapshot when progress stops.
//!
//! The watchdog never restarts or interrupts anything. It only reports.

use crate::model::RuntimeHealth;
use log::{debug, info, warn};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 1_000;
pub const DEFAULT_STALL_THRESHOLD_MS: u64 = 10_000;
pub const DEFAULT_STALL_RELOG_MS: u64 = 30_000;
pub const DEFAULT_WATCHDOG_POLL_MS: u64 = 1_000;

/// The watchdog only sleeps and reads atomics outside a stall, so it does not
/// need the default thread stack.
const WATCHDOG_STACK_SIZE: usize = 256 * 1024;

fn monotonic_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// Monotonic milliseconds since the process first read the liveness clock.
///
/// Wall-clock reads can jump backwards on NTP adjustments, which would make a
/// heartbeat age meaningless; the watchdog therefore never uses `SystemTime`.
#[must_use]
pub fn monotonic_now_ms() -> u64 { u64::try_from(monotonic_origin().elapsed().as_millis()).unwrap_or(u64::MAX) }

fn read_env_ms(name: &str, default_ms: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_ms)
}

fn watchdog_flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes" | "enabled")
    })
}

/// The watchdog is opt-in: without `TULIPROX_WATCHDOG=1` (or `true`/`on`/`yes`/
/// `enabled`) no heartbeat task and no watchdog thread are started.
fn watchdog_enabled() -> bool { watchdog_flag_enabled(std::env::var("TULIPROX_WATCHDOG").ok().as_deref()) }

/// Progress shared between the in-runtime heartbeat and the watchdog thread.
///
/// Every accessor takes the current monotonic instant instead of reading a
/// clock, so the stall decision is testable without threads or time.
pub struct RuntimeLiveness {
    started_ms: u64,
    heartbeat_interval_ms: u64,
    stall_threshold_ms: u64,
    last_tick_ms: AtomicU64,
    ticks: AtomicU64,
    stall_episodes: AtomicU64,
    max_schedule_delay_ms: AtomicU64,
}

impl RuntimeLiveness {
    #[must_use]
    pub fn new(started_ms: u64, heartbeat_interval_ms: u64, stall_threshold_ms: u64) -> Self {
        Self {
            started_ms,
            heartbeat_interval_ms: heartbeat_interval_ms.max(1),
            stall_threshold_ms: stall_threshold_ms.max(1),
            last_tick_ms: AtomicU64::new(started_ms),
            ticks: AtomicU64::new(0),
            stall_episodes: AtomicU64::new(0),
            max_schedule_delay_ms: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub const fn heartbeat_interval_ms(&self) -> u64 { self.heartbeat_interval_ms }

    #[must_use]
    pub const fn stall_threshold_ms(&self) -> u64 { self.stall_threshold_ms }

    /// Records progress. `schedule_delay_ms` is how late the heartbeat was
    /// scheduled relative to its deadline and surfaces scheduler pressure even
    /// when the threshold is never crossed.
    pub fn tick(&self, now_ms: u64, schedule_delay_ms: u64) {
        self.last_tick_ms.store(now_ms, Ordering::Release);
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.max_schedule_delay_ms.fetch_max(schedule_delay_ms, Ordering::Relaxed);
    }

    /// Saturation covers a heartbeat timestamp that is somehow ahead of `now`.
    #[must_use]
    pub fn age_ms(&self, now_ms: u64) -> u64 { now_ms.saturating_sub(self.last_tick_ms.load(Ordering::Acquire)) }

    #[must_use]
    pub fn is_stalled(&self, now_ms: u64) -> bool { self.age_ms(now_ms) > self.stall_threshold_ms }

    pub fn note_stall(&self) { self.stall_episodes.fetch_add(1, Ordering::Relaxed); }

    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> RuntimeHealth {
        let age = self.age_ms(now_ms);
        let stalled = age > self.stall_threshold_ms;
        RuntimeHealth {
            status: if stalled { "stalled".to_string() } else { "alive".to_string() },
            heartbeat_age_ms: age,
            heartbeat_interval_ms: self.heartbeat_interval_ms,
            stall_threshold_ms: self.stall_threshold_ms,
            stalled_for_ms: stalled.then(|| age.saturating_sub(self.stall_threshold_ms)),
            ticks: self.ticks.load(Ordering::Relaxed),
            stall_episodes: self.stall_episodes.load(Ordering::Relaxed),
            max_schedule_delay_ms: self.max_schedule_delay_ms.load(Ordering::Relaxed),
            uptime_ms: now_ms.saturating_sub(self.started_ms),
        }
    }
}

static INSTANCE: OnceLock<Arc<RuntimeLiveness>> = OnceLock::new();

/// Installs the process-wide liveness state once. Later calls are ignored.
pub fn install(liveness: Arc<RuntimeLiveness>) -> bool { INSTANCE.set(liveness).is_ok() }

#[must_use]
pub fn installed() -> Option<&'static Arc<RuntimeLiveness>> { INSTANCE.get() }

/// Current liveness snapshot, or `None` when the watchdog is not installed.
#[must_use]
pub fn health_snapshot() -> Option<RuntimeHealth> {
    INSTANCE.get().map(|liveness| liveness.snapshot(monotonic_now_ms()))
}

/// Starts the heartbeat task and the watchdog thread, unless already started.
///
/// Returns the shared state on success. The watchdog is opt-in; without
/// `TULIPROX_WATCHDOG=1` nothing is started. The interval and threshold can be
/// overridden with `TULIPROX_WATCHDOG_HEARTBEAT_MS` and
/// `TULIPROX_WATCHDOG_STALL_MS`; the relog interval with
/// `TULIPROX_WATCHDOG_RELOG_MS`.
pub fn start(runtime: &tokio::runtime::Handle) -> Option<Arc<RuntimeLiveness>> {
    if !watchdog_enabled() {
        debug!("Runtime liveness watchdog disabled (set TULIPROX_WATCHDOG=1 to enable)");
        return None;
    }

    let heartbeat_interval_ms = read_env_ms("TULIPROX_WATCHDOG_HEARTBEAT_MS", DEFAULT_HEARTBEAT_INTERVAL_MS);
    let stall_threshold_ms = read_env_ms("TULIPROX_WATCHDOG_STALL_MS", DEFAULT_STALL_THRESHOLD_MS);
    let relog_ms = read_env_ms("TULIPROX_WATCHDOG_RELOG_MS", DEFAULT_STALL_RELOG_MS);

    let liveness = Arc::new(RuntimeLiveness::new(monotonic_now_ms(), heartbeat_interval_ms, stall_threshold_ms));
    if !install(Arc::clone(&liveness)) {
        return None;
    }

    spawn_heartbeat(Arc::clone(&liveness));
    match spawn_watchdog(Arc::clone(&liveness), runtime.clone(), relog_ms) {
        Ok(_) => info!(
            "Runtime liveness watchdog started: heartbeat {heartbeat_interval_ms} ms, stall threshold {stall_threshold_ms} ms"
        ),
        Err(err) => warn!("Failed to start runtime liveness watchdog thread: {err}"),
    }
    Some(liveness)
}

/// Spawns the in-runtime heartbeat task.
fn spawn_heartbeat(liveness: Arc<RuntimeLiveness>) {
    let interval = Duration::from_millis(liveness.heartbeat_interval_ms());
    tokio::spawn(async move {
        let mut expected = Instant::now() + interval;
        loop {
            tokio::time::sleep_until(tokio::time::Instant::from_std(expected)).await;
            let now = Instant::now();
            let delay_ms = u64::try_from(now.saturating_duration_since(expected).as_millis()).unwrap_or(u64::MAX);
            liveness.tick(monotonic_now_ms(), delay_ms);
            expected += interval;
            // After a long stall, sleep_until would return immediately until it
            // catches up; resync so recovery does not become a tight loop.
            if expected < now {
                expected = now + interval;
            }
        }
    });
}

/// Spawns the watchdog thread that lives outside the Tokio runtime.
fn spawn_watchdog(
    liveness: Arc<RuntimeLiveness>,
    runtime: tokio::runtime::Handle,
    relog_ms: u64,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("tuliprox-watchdog".to_string()).stack_size(WATCHDOG_STACK_SIZE).spawn(move || {
        run_watchdog(&liveness, &runtime, relog_ms);
    })
}

/// The decision the watchdog makes on each poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogVerdict {
    /// Within the threshold and not in a stall, or still inside an episode but
    /// before the relog interval is due.
    Quiet,
    /// Above the threshold and it is time to (re)log the stall.
    LogStall,
    /// The heartbeat is fresh again after an episode.
    Recovered,
}

fn watchdog_verdict(
    age_ms: u64,
    stall_threshold_ms: u64,
    in_stall: bool,
    since_last_log_ms: u64,
    relog_ms: u64,
) -> WatchdogVerdict {
    if age_ms > stall_threshold_ms {
        if in_stall && since_last_log_ms < relog_ms {
            WatchdogVerdict::Quiet
        } else {
            WatchdogVerdict::LogStall
        }
    } else if in_stall {
        WatchdogVerdict::Recovered
    } else {
        WatchdogVerdict::Quiet
    }
}

fn run_watchdog(liveness: &RuntimeLiveness, runtime: &tokio::runtime::Handle, relog_ms: u64) {
    let poll = Duration::from_millis(DEFAULT_WATCHDOG_POLL_MS);
    let threshold_ms = liveness.stall_threshold_ms();
    let mut in_stall = false;
    let mut last_log_ms = 0_u64;

    loop {
        std::thread::sleep(poll);
        let now = monotonic_now_ms();
        let age = liveness.age_ms(now);
        let since_last_log = now.saturating_sub(last_log_ms);
        match watchdog_verdict(age, threshold_ms, in_stall, since_last_log, relog_ms) {
            WatchdogVerdict::Quiet => {}
            WatchdogVerdict::LogStall => {
                if !in_stall {
                    liveness.note_stall();
                }
                let snapshot = liveness.snapshot(now);
                warn!(
                    "Runtime liveness stall: heartbeat silent for {age} ms (threshold {threshold_ms} ms, episode {})",
                    snapshot.stall_episodes
                );
                log_runtime_diagnostics(&snapshot, runtime, age);
                last_log_ms = now;
                in_stall = true;
            }
            WatchdogVerdict::Recovered => {
                info!("Runtime liveness recovered: heartbeat resumed after {age} ms of silence");
                in_stall = false;
            }
        }
    }
}

fn log_runtime_diagnostics(snapshot: &RuntimeHealth, runtime: &tokio::runtime::Handle, age_ms: u64) {
    let metrics = runtime.metrics();
    warn!(
        "Runtime diagnostics: heartbeat_age_ms={age_ms}, heartbeat_interval_ms={}, stall_threshold_ms={}, ticks={}, max_schedule_delay_ms={}, uptime_ms={}, workers={}, alive_tasks={}, global_queue_depth={}",
        snapshot.heartbeat_interval_ms,
        snapshot.stall_threshold_ms,
        snapshot.ticks,
        snapshot.max_schedule_delay_ms,
        snapshot.uptime_ms,
        metrics.num_workers(),
        metrics.num_alive_tasks(),
        metrics.global_queue_depth(),
    );

    #[cfg(target_has_atomic = "64")]
    log_worker_diagnostics(&metrics);

    #[cfg(target_os = "linux")]
    log_thread_inventory();
}

// Only metrics that are stable without `tokio_unstable` may be used here: the
// same source must compile in the default build. `worker_poll_count`,
// `worker_local_queue_depth` and the blocking-pool counters are behind
// `tokio_unstable` and are deliberately omitted.
#[cfg(target_has_atomic = "64")]
fn log_worker_diagnostics(metrics: &tokio::runtime::RuntimeMetrics) {
    for worker in 0..metrics.num_workers() {
        warn!(
            "Runtime worker {worker}: park_count={}, total_busy={:?}",
            metrics.worker_park_count(worker),
            metrics.worker_total_busy_duration(worker),
        );
    }
}

/// Logs what every thread in this process is doing, mirroring the manual
/// `/proc/<pid>/task/*/stack` inspection used to diagnose a hang.
#[cfg(target_os = "linux")]
fn log_thread_inventory() {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else { return };

    let mut threads: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let Some(tid) = entry.file_name().into_string().ok() else { continue };
        let base = entry.path();
        let comm =
            std::fs::read_to_string(base.join("comm")).map_or_else(|_| "?".to_string(), |name| name.trim().to_string());
        let state = read_proc_state(&base.join("stat"));
        let wchan = std::fs::read_to_string(base.join("wchan"))
            .map_or_else(|_| "?".to_string(), |value| value.trim().to_string());
        threads.push(format!("tid={tid} name={comm} state={state} wchan={wchan}"));
    }
    threads.sort();
    warn!("Runtime thread inventory ({} threads):\n{}", threads.len(), threads.join("\n"));
}

/// Reads the state field from `/proc/<tid>/stat`.
///
/// The command name in the second field is wrapped in parentheses and may
/// itself contain spaces and parentheses, so the state is the token following
/// the *last* `)`.
#[cfg(target_os = "linux")]
fn read_proc_state(stat_path: &std::path::Path) -> String {
    let Ok(stat) = std::fs::read_to_string(stat_path) else { return "?".to_string() };
    stat.rfind(')')
        .and_then(|closing| stat[closing + 1..].split_whitespace().next())
        .map_or_else(|| "?".to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        monotonic_now_ms, spawn_heartbeat, watchdog_flag_enabled, watchdog_verdict, RuntimeLiveness, WatchdogVerdict,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    #[test]
    fn watchdog_flag_is_off_by_default_and_accepts_conventional_on_values() {
        assert!(!watchdog_flag_enabled(None));
        for value in ["0", "false", "FALSE", " off ", "no", "disabled", ""] {
            assert!(!watchdog_flag_enabled(Some(value)), "expected {value:?} to keep the watchdog off");
        }
        for value in ["1", "true", "TRUE", " on ", "yes", "enabled"] {
            assert!(watchdog_flag_enabled(Some(value)), "expected {value:?} to enable the watchdog");
        }
    }

    #[tokio::test]
    async fn heartbeat_task_records_progress() {
        let liveness = Arc::new(RuntimeLiveness::new(monotonic_now_ms(), 5, 5_000));
        spawn_heartbeat(Arc::clone(&liveness));
        let deadline = Instant::now() + Duration::from_secs(5);
        while liveness.snapshot(monotonic_now_ms()).ticks == 0 {
            assert!(Instant::now() < deadline, "heartbeat task never ticked");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!liveness.is_stalled(monotonic_now_ms()));
    }

    #[test]
    fn fresh_heartbeat_reports_alive() {
        let liveness = RuntimeLiveness::new(1_000, 1_000, 5_000);
        liveness.tick(2_000, 3);
        let snapshot = liveness.snapshot(2_500);
        assert_eq!(snapshot.status, "alive");
        assert_eq!(snapshot.heartbeat_age_ms, 500);
        assert_eq!(snapshot.stalled_for_ms, None);
        assert_eq!(snapshot.ticks, 1);
        assert_eq!(snapshot.max_schedule_delay_ms, 3);
        assert_eq!(snapshot.uptime_ms, 1_500);
    }

    #[test]
    fn silent_heartbeat_reports_stalled_with_overflow_age() {
        let liveness = RuntimeLiveness::new(0, 1_000, 5_000);
        liveness.tick(1_000, 0);
        let snapshot = liveness.snapshot(9_000);
        assert_eq!(snapshot.status, "stalled");
        assert_eq!(snapshot.heartbeat_age_ms, 8_000);
        assert_eq!(snapshot.stalled_for_ms, Some(3_000));
        assert!(liveness.is_stalled(9_000));
        assert!(!liveness.is_stalled(5_500));
    }

    #[test]
    fn heartbeat_age_saturates_when_clock_moves_backwards() {
        let liveness = RuntimeLiveness::new(10_000, 1_000, 5_000);
        assert_eq!(liveness.age_ms(1_000), 0);
    }

    #[test]
    fn max_schedule_delay_keeps_the_worst_observation() {
        let liveness = RuntimeLiveness::new(0, 1_000, 5_000);
        liveness.tick(1_000, 10);
        liveness.tick(2_000, 4);
        liveness.tick(3_000, 25);
        assert_eq!(liveness.snapshot(3_000).max_schedule_delay_ms, 25);
    }

    #[test]
    fn stall_episodes_are_counted_once_per_episode() {
        let liveness = RuntimeLiveness::new(0, 1_000, 5_000);
        assert_eq!(liveness.snapshot(0).stall_episodes, 0);
        liveness.note_stall();
        liveness.note_stall();
        assert_eq!(liveness.snapshot(0).stall_episodes, 2);
    }

    #[test]
    fn verdict_logs_on_entry_then_respects_relog_interval() {
        assert_eq!(watchdog_verdict(1_000, 5_000, false, 0, 30_000), WatchdogVerdict::Quiet);
        assert_eq!(watchdog_verdict(6_000, 5_000, false, 0, 30_000), WatchdogVerdict::LogStall);
        assert_eq!(watchdog_verdict(6_000, 5_000, true, 1_000, 30_000), WatchdogVerdict::Quiet);
        assert_eq!(watchdog_verdict(6_000, 5_000, true, 30_000, 30_000), WatchdogVerdict::LogStall);
    }

    #[test]
    fn verdict_reports_recovery_only_after_an_episode() {
        assert_eq!(watchdog_verdict(100, 5_000, false, 0, 30_000), WatchdogVerdict::Quiet);
        assert_eq!(watchdog_verdict(100, 5_000, true, 0, 30_000), WatchdogVerdict::Recovered);
    }
}
