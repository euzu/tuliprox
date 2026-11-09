//! The harness of the isolation proof (`tests/isolation.rs`): runs an attack the
//! way a hostile plugin would, `max_instances` calls at once on a small
//! multi-thread runtime, and checks what the plugin system promises.
//!
//! Per attack ([`prove`]):
//! a. every call ends within a fixed bound and the plugin's `last_error` names
//!    the classification (deadline, out of memory, trap, budget);
//! b. while the attack runs, a `sleep(10 ms)` probe and a call into a healthy
//!    second plugin both complete within a bound that is far below the
//!    attack's own deadline (a blocked worker could not meet it);
//! c. the call changes nothing (`check` asserts the data) and five failures in
//!    a row open the plugin's breaker (rpc never feeds the breaker, by design);
//! d. afterwards no instance slot and no memory permit is held, and the healthy
//!    plugin still works.
#![allow(dead_code)] // each test uses a different subset

use crate::common::probe_entry;
use serde_json::json;
use shared::model::{HookCtx, PluginBreakerStateDto, PluginHost, PluginInputRequest, PluginStatsDto};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{sync::MutexGuard, task::JoinSet};
use tuliprox_core::model::PluginEntry;
use tuliprox_plugins::PluginSystem;

/// `max_instances` of every attacking manifest: the calls an attack runs at once.
pub const PARALLEL: usize = 4;
/// The attacking manifests' deadline is one second. A guest that held a worker
/// for its deadline could not keep a `sleep(10 ms)` under this.
pub const SLEEP_BOUND: Duration = Duration::from_millis(400);
/// Likewise for a normal call into the healthy plugin, which takes a few ms.
pub const HEALTHY_BOUND: Duration = Duration::from_millis(700);
/// Consecutive failures that open a plugin's breaker.
const BREAKER_THRESHOLD: u64 = 5;
/// The healthy second plugin of every attack.
pub const HEALTHY: &str = "provider-probe";

/// Attacks burn CPU on purpose; running them one at a time keeps the timing
/// bounds about the plugin system and not about a busy test machine.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn serial() -> MutexGuard<'static, ()> { SERIAL.lock().await }

/// The config entry of the healthy plugin.
pub fn healthy_entry() -> PluginEntry { probe_entry(HEALTHY, json!({})) }

pub fn stats(system: &PluginSystem, plugin: &str) -> PluginStatsDto {
    system.api_stats(plugin).unwrap_or_else(|| panic!("plugin {plugin} is not known"))
}

/// One normal call into the healthy plugin; how long it took.
pub async fn healthy_call(system: &PluginSystem) -> Duration {
    let started = Instant::now();
    let ctx = HookCtx::new("").with_input("feed");
    let request = PluginInputRequest {
        input_name: "feed",
        url: "http://host/feed",
        username: None,
        password: None,
        options: &[],
    };
    let items = system.fetch_playlist(HEALTHY, &ctx, &request).await.expect("the healthy plugin answers");
    assert_eq!(items.len(), 5, "the healthy plugin returns its normal answer");
    started.elapsed()
}

/// Measures how late a `sleep(10 ms)` wakes up while it runs.
pub struct Sleeper {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Duration>,
}

impl Sleeper {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let task = tokio::spawn(async move {
            let mut worst = Duration::ZERO;
            while !flag.load(Ordering::Acquire) {
                let started = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                worst = worst.max(started.elapsed());
            }
            worst
        });
        Self { stop, task }
    }

    /// The longest a 10 ms sleep took.
    pub async fn finish(self) -> Duration {
        self.stop.store(true, Ordering::Release);
        self.task.await.expect("the sleep probe")
    }
}

/// What one round of an attack came to.
pub struct Round<T> {
    /// Each attack call's result and how long it took.
    pub outcomes: Vec<(T, Duration)>,
    pub worst_sleep: Duration,
    pub healthy: Duration,
}

/// `parallel` calls of `attack` on the runtime's workers at once. Once they are
/// under way, a normal call into the healthy plugin runs next to them, and a
/// 10 ms sleep probe runs throughout.
pub async fn attack_round<T, F>(parallel: usize, attack: impl Fn(usize) -> F, system: &PluginSystem) -> Round<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let sleeper = Sleeper::start();
    let mut attacks = JoinSet::new();
    for index in 0..parallel {
        let call = attack(index);
        attacks.spawn(async move {
            let started = Instant::now();
            let outcome = call.await;
            (outcome, started.elapsed())
        });
    }
    // Let the attacks reach their guests before the healthy call is made.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let healthy = healthy_call(system).await;
    let mut outcomes = Vec::with_capacity(parallel);
    while let Some(done) = attacks.join_next().await {
        outcomes.push(done.expect("an attack task does not panic"));
    }
    Round { outcomes, worst_sleep: sleeper.finish().await, healthy }
}

/// What an attack is expected to come to.
pub struct Expect<'a> {
    /// The attacking plugin.
    pub plugin: &'a str,
    /// Text of the classification in the plugin's `last_error` (`deadline
    /// exceeded`, `out of memory`, `trapped`); `None` when the host caps the
    /// attack without failing the call.
    pub error: Option<&'a str>,
    /// Whether five failures in a row open the plugin's breaker. The rpc route
    /// never feeds the breaker, so it passes `false`.
    pub breaker_opens: bool,
    /// The longest one attack call may take.
    pub max_call: Duration,
}

/// Runs the attack as one round of [`PARALLEL`] calls and one of a single call
/// (five in all, the breaker threshold) and asserts a to d of the module doc.
/// `check` asserts the data of every call's result.
pub async fn prove<T, F>(system: &PluginSystem, expect: &Expect<'_>, attack: impl Fn(usize) -> F, check: impl Fn(&T))
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    for parallel in [PARALLEL, 1] {
        let round = attack_round(parallel, &attack, system).await;
        eprintln!(
            "DBG {} calls: {:?} sleep {:?} healthy {:?}",
            parallel,
            round.outcomes.iter().map(|(_, took)| *took).collect::<Vec<_>>(),
            round.worst_sleep,
            round.healthy
        );
        for (outcome, took) in &round.outcomes {
            assert!(*took <= expect.max_call, "an attack call took {took:?}, bound {:?}", expect.max_call);
            check(outcome);
        }
        assert!(round.worst_sleep <= SLEEP_BOUND, "a 10 ms sleep took {:?} during the attack", round.worst_sleep);
        assert!(round.healthy <= HEALTHY_BOUND, "the healthy plugin took {:?} during the attack", round.healthy);
    }

    let attacked = stats(system, expect.plugin);
    eprintln!(
        "DBG last_error {:?} failures {} breaker {:?}",
        attacked.last_error, attacked.failures, attacked.breaker.state
    );
    match expect.error {
        Some(text) => {
            assert!(attacked.failures >= BREAKER_THRESHOLD, "failures: {}", attacked.failures);
            let last = attacked.last_error.as_deref().unwrap_or_default();
            assert!(last.contains(text), "last_error `{last}` does not name `{text}`");
        }
        None => assert_eq!(attacked.failures, 0, "last error: {:?}", attacked.last_error),
    }
    let expected = if expect.breaker_opens { PluginBreakerStateDto::Open } else { PluginBreakerStateDto::Closed };
    assert_eq!(attacked.breaker.state, expected, "breaker after {BREAKER_THRESHOLD} calls");

    assert_recovered(system, expect.plugin).await;
    healthy_call(system).await;
    let healthy = stats(system, HEALTHY);
    assert_eq!(healthy.failures, 0, "the healthy plugin never failed: {:?}", healthy.last_error);
    assert_eq!(healthy.breaker.state, PluginBreakerStateDto::Closed);
}

/// No instance slot of `plugin` and no memory permit of the system is held.
/// Waits a moment for work that finishes on its own (the event consumer).
pub async fn assert_recovered(system: &PluginSystem, plugin: &str) {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        let now = stats(system, plugin);
        let live = now.instances.map_or(0, |instances| instances.live);
        if live == 0 && now.budget.in_use_mib == 0 {
            return;
        }
        assert!(
            Instant::now() < give_up,
            "{plugin} still holds {live} instance(s) and {} MiB of the budget",
            now.budget.in_use_mib
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The resident set size of this process in KiB. Linux only.
pub fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// A coarse bound: the process grew by less than `max_mib` since `before`.
/// Skipped where `/proc` is not there.
pub fn assert_rss_growth_below(before: Option<u64>, max_mib: u64) {
    let (Some(before), Some(after)) = (before, rss_kib()) else { return };
    let grown_mib = after.saturating_sub(before) / 1024;
    assert!(grown_mib < max_mib, "host memory grew by {grown_mib} MiB (bound {max_mib} MiB)");
}
