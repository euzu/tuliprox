//! Dashboard metrics: throttled
//! `collect-metrics` polling, a snapshot cache, `<plugin-id>.<key>` keys.
//!
//! [`PluginSystem::metrics_snapshot`] is what the status payload and
//! `GET /api/v1/plugins/metrics` ask for. A snapshot younger than
//! [`REFRESH_INTERVAL`] is served as is; otherwise exactly one caller
//! refreshes (single flight) while concurrent callers get the previous
//! snapshot. Each plugin runs in a fresh instance under the normal hook
//! deadline; one that traps, errs or is too slow contributes nothing that
//! cycle and is counted on its circuit breaker.

use crate::{
    host_fns,
    registry::LoadedPlugin,
    runtime::{call_with_deadline, CallError},
    PluginSystem,
};
use shared::model::{PluginHook, PluginMetricDto, PluginMetricSeverityDto};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::Duration,
};
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::metrics_hooks::Metric, tuliprox::plugin::status::Severity,
};

/// A snapshot at most this old is served without calling any plugin.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Metrics kept per plugin per cycle; the rest are dropped. A card grid has no
/// room for more, and the answer is plugin-controlled.
const MAX_METRICS_PER_PLUGIN: usize = 64;

type Metrics = Arc<[PluginMetricDto]>;

struct Snapshot {
    /// Host clock, milliseconds, when the refresh finished. `None` before the first.
    taken_ms: Option<u64>,
    metrics: Metrics,
    /// The registry reload generation the snapshot belongs to.
    generation: u64,
}

/// The last snapshot and when it was taken. Built once with the
/// [`crate::PluginSystem`].
pub(crate) struct MetricsCache {
    snapshot: Mutex<Snapshot>,
    /// Set while one caller refreshes.
    refreshing: AtomicBool,
}

/// What a caller of [`MetricsCache::plan`] does next.
enum Plan<'a> {
    /// A snapshot younger than [`REFRESH_INTERVAL`].
    Fresh(Metrics),
    /// Someone else is refreshing: the previous snapshot.
    Busy(Metrics),
    /// This caller refreshes. Dropping the guard (also on cancellation) lets
    /// the next caller try.
    Refresh(RefreshGuard<'a>),
}

struct RefreshGuard<'a>(&'a AtomicBool);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

impl MetricsCache {
    pub(crate) fn new(_config: &PluginsConfig) -> Self {
        Self {
            snapshot: Mutex::new(Snapshot { taken_ms: None, metrics: Arc::from([]), generation: 0 }),
            refreshing: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current(&self) -> Metrics { Arc::clone(&self.lock().metrics) }

    /// Forget the snapshot when the registry reloaded since it was taken, so a
    /// plugin installed, updated or removed shows (or stops showing) on the
    /// next read instead of after up to [`REFRESH_INTERVAL`].
    fn follow_generation(&self, generation: u64) {
        let mut snapshot = self.lock();
        if snapshot.generation != generation {
            snapshot.generation = generation;
            snapshot.taken_ms = None;
        }
    }

    fn fresh(&self, now_ms: u64) -> Option<Metrics> {
        let snapshot = self.lock();
        let taken = snapshot.taken_ms?;
        let interval = u64::try_from(REFRESH_INTERVAL.as_millis()).unwrap_or(u64::MAX);
        (now_ms.saturating_sub(taken) < interval).then(|| Arc::clone(&snapshot.metrics))
    }

    fn plan(&self, now_ms: u64) -> Plan<'_> {
        if let Some(fresh) = self.fresh(now_ms) {
            return Plan::Fresh(fresh);
        }
        if self.refreshing.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Plan::Busy(self.current());
        }
        let guard = RefreshGuard(&self.refreshing);
        // A refresh may have finished between the check above and taking the flag.
        if let Some(fresh) = self.fresh(now_ms) {
            return Plan::Fresh(fresh);
        }
        Plan::Refresh(guard)
    }

    #[cfg(test)]
    fn store(&self, metrics: Metrics, now_ms: u64) {
        let mut snapshot = self.lock();
        snapshot.taken_ms = Some(now_ms);
        snapshot.metrics = metrics;
    }

    /// [`MetricsCache::store`] for a refresh begun at registry `generation`:
    /// if the registry reloaded meanwhile the result is kept but not fresh, so
    /// the next read collects again.
    fn store_for(&self, metrics: Metrics, now_ms: u64, generation: u64) {
        let mut snapshot = self.lock();
        snapshot.taken_ms = (snapshot.generation == generation).then_some(now_ms);
        snapshot.metrics = metrics;
    }
}

/// `<plugin-id>.<key>` and the card the plugin asked for.
fn to_dto(plugin: &str, metric: Metric) -> PluginMetricDto {
    PluginMetricDto {
        key: format!("{plugin}.{}", metric.key),
        plugin: plugin.to_string(),
        label: metric.label,
        value: metric.value,
        unit: metric.unit,
        severity: metric.severity.map(|severity| match severity {
            Severity::Info => PluginMetricSeverityDto::Info,
            Severity::Warn => PluginMetricSeverityDto::Warn,
            Severity::Error => PluginMetricSeverityDto::Error,
        }),
    }
}

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` function")]
fn bind(err: wasmtime::Error) -> CallError { CallError::Trap(format!("{err:#}")) }

/// One plugin's `collect-metrics` in a fresh instance.
async fn collect_one(
    dispatcher: &crate::dispatch::HookDispatcher<'_>,
    loaded: &LoadedPlugin,
) -> Result<Vec<Metric>, CallError> {
    let indices = loaded
        .metrics_hooks
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export metrics-hooks".to_string()))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_collect_metrics(live.armed())).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))
}

impl PluginSystem {
    /// The metrics of every plugin that exports `collect-metrics`, in run order
    /// (priority, id) and each plugin's own order. At most one `collect-metrics`
    /// round per 10 s; plugins that failed this round are absent.
    pub async fn metrics_snapshot(&self) -> Vec<PluginMetricDto> {
        let cache = self.metrics();
        let generation = *self.changes().borrow();
        cache.follow_generation(generation);
        let clock = &self.shared().clock;
        match cache.plan(clock.wall_ms().get()) {
            Plan::Fresh(metrics) | Plan::Busy(metrics) => metrics.to_vec(),
            Plan::Refresh(_guard) => {
                let metrics: Metrics = self.collect_metrics().await.into();
                cache.store_for(Arc::clone(&metrics), clock.wall_ms().get(), generation);
                metrics.to_vec()
            }
        }
    }

    async fn collect_metrics(&self) -> Vec<PluginMetricDto> {
        let dispatcher = self.dispatcher();
        let mut collected = Vec::new();
        for (plugin, loaded) in dispatcher.runnable(PluginHook::CollectMetrics) {
            match collect_one(&dispatcher, loaded).await {
                Ok(metrics) => {
                    plugin.report(Ok(()));
                    if metrics.len() > MAX_METRICS_PER_PLUGIN {
                        log::warn!(
                            "[plugin:{}] collect-metrics returned {} metrics, keeping the first {MAX_METRICS_PER_PLUGIN}",
                            plugin.id,
                            metrics.len()
                        );
                    }
                    collected.extend(
                        metrics.into_iter().take(MAX_METRICS_PER_PLUGIN).map(|metric| to_dto(&plugin.id, metric)),
                    );
                }
                Err(err) => {
                    plugin.report(Err(&err));
                    log::warn!("[plugin:{}] collect-metrics failed, no metrics this cycle: {err}", plugin.id);
                }
            }
        }
        collected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_core::utils::{Clock, ManualClock};

    fn cache() -> MetricsCache { MetricsCache::new(&PluginsConfig::default()) }

    fn one(key: &str) -> Metrics {
        vec![PluginMetricDto {
            key: key.to_string(),
            plugin: "edge".to_string(),
            label: "Answer".to_string(),
            value: "42".to_string(),
            unit: None,
            severity: None,
        }]
        .into()
    }

    fn metric(key: &str, severity: Option<Severity>) -> Metric {
        Metric {
            key: key.to_string(),
            label: "Answer".to_string(),
            value: "42".to_string(),
            unit: Some("x".to_string()),
            severity,
        }
    }

    #[test]
    fn keys_are_namespaced_by_the_plugin_id() {
        let dto = to_dto("edge", metric("answer", Some(Severity::Warn)));
        assert_eq!(dto.key, "edge.answer");
        assert_eq!(dto.plugin, "edge");
        assert_eq!((dto.label.as_str(), dto.value.as_str(), dto.unit.as_deref()), ("Answer", "42", Some("x")));
        assert_eq!(dto.severity, Some(PluginMetricSeverityDto::Warn));
        assert_eq!(to_dto("edge", metric("answer", None)).severity, None);
        // The same key of two plugins never collides.
        assert_ne!(to_dto("a", metric("k", None)).key, to_dto("b", metric("k", None)).key);
    }

    #[test]
    fn the_first_caller_refreshes_and_a_young_snapshot_is_served_for_ten_seconds() {
        let clock = ManualClock::new(1_000);
        let cache = cache();
        let Plan::Refresh(guard) = cache.plan(clock.now_ms().get()) else { panic!("the first call must refresh") };
        cache.store(one("edge.calls"), clock.now_ms().get());
        drop(guard);

        clock.advance(9_999);
        assert!(matches!(cache.plan(clock.now_ms().get()), Plan::Fresh(metrics) if metrics[0].key == "edge.calls"));
        clock.advance(1);
        assert!(matches!(cache.plan(clock.now_ms().get()), Plan::Refresh(_)), "a 10 s old snapshot is stale");
    }

    #[test]
    fn concurrent_callers_get_the_previous_snapshot_while_one_refreshes() {
        let clock = ManualClock::new(0);
        let cache = cache();
        let Plan::Refresh(first) = cache.plan(clock.now_ms().get()) else { panic!("the first call must refresh") };
        cache.store(one("edge.old"), clock.now_ms().get());
        drop(first);

        clock.advance(10_000);
        let Plan::Refresh(refresher) = cache.plan(clock.now_ms().get()) else { panic!("stale, so it must refresh") };
        assert!(matches!(cache.plan(clock.now_ms().get()), Plan::Busy(metrics) if metrics[0].key == "edge.old"));

        cache.store(one("edge.new"), clock.now_ms().get());
        drop(refresher);
        assert!(matches!(cache.plan(clock.now_ms().get()), Plan::Fresh(metrics) if metrics[0].key == "edge.new"));
    }

    #[test]
    fn a_cancelled_refresh_releases_the_flag_and_keeps_the_old_snapshot() {
        let cache = cache();
        let Plan::Refresh(guard) = cache.plan(0) else { panic!("the first call must refresh") };
        drop(guard);
        // Nothing was stored, so the next caller refreshes again, still at t = 0.
        assert!(matches!(cache.plan(0), Plan::Refresh(_)));
    }

    #[test]
    fn an_empty_snapshot_is_still_a_snapshot() {
        let cache = cache();
        let Plan::Refresh(guard) = cache.plan(5) else { panic!("the first call must refresh") };
        cache.store(Arc::from([]), 5);
        drop(guard);
        assert!(
            matches!(cache.plan(6), Plan::Fresh(metrics) if metrics.is_empty()),
            "no plugin answered; do not poll again"
        );
    }

    #[test]
    fn the_throttle_refreshes_under_wall_time_while_plugins_see_a_frozen_clock() {
        use crate::state::HostClock;
        let frozen = ManualClock::new(1_793_448_000_000);
        let clock = HostClock::Frozen(frozen);
        let cache = cache();

        // First call stores the snapshot, stamped with the host wall clock like `metrics_snapshot`.
        let taken = clock.wall_ms().get();
        let Plan::Refresh(guard) = cache.plan(taken) else { panic!("the first call must refresh") };
        cache.store(one("edge.calls"), taken);
        drop(guard);

        // Wall time moved past the interval: the snapshot is stale and refreshes again.
        let later = taken + 10_000;
        assert!(matches!(cache.plan(later - 1), Plan::Fresh(_)));
        assert!(matches!(cache.plan(later), Plan::Refresh(_)), "wall time advancing refreshes the snapshot");
    }

    #[test]
    fn a_registry_reload_drops_the_snapshot() {
        let cache = cache();
        let Plan::Refresh(guard) = cache.plan(1_000) else { panic!("the first call must refresh") };
        cache.store(one("edge.calls"), 1_000);
        drop(guard);
        cache.follow_generation(0);
        assert!(matches!(cache.plan(1_001), Plan::Fresh(_)), "same generation: the snapshot stays");
        cache.follow_generation(1);
        let Plan::Refresh(guard) = cache.plan(1_002) else { panic!("a reload since: refresh at once") };
        // a refresh begun before a further reload is not fresh for the new generation
        cache.follow_generation(2);
        cache.store_for(one("edge.old"), 1_003, 1);
        drop(guard);
        assert!(matches!(cache.plan(1_004), Plan::Refresh(_)));
    }

    #[test]
    fn a_throttle_fed_the_frozen_plugin_time_would_never_refresh() {
        use crate::state::HostClock;
        let clock = HostClock::Frozen(ManualClock::new(1_793_448_000_000));
        let cache = cache();
        let Plan::Refresh(guard) = cache.plan(clock.now_ms().get()) else { panic!("the first call must refresh") };
        cache.store(one("edge.calls"), clock.now_ms().get());
        drop(guard);
        // Plugin time never moves, so the snapshot would stay fresh forever. This is why the throttle reads
        // `wall_ms`, and why `wall_ms` of a frozen clock must differ from `now_ms`.
        assert!(matches!(cache.plan(clock.now_ms().get()), Plan::Fresh(_)));
        assert!(
            matches!(cache.plan(clock.wall_ms().get()), Plan::Refresh(_)),
            "the wall clock is far past the frozen instant, so the snapshot is stale"
        );
    }
}
