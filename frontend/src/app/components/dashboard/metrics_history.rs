use crate::app::components::{use_plugin_ui, StatusContext};
use gloo_timers::future::TimeoutFuture;
use shared::model::{PluginMetricDto, StatusCheck, SystemInfo};
use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};
use yew::prelude::*;

const HISTORY_LEN: usize = 40;
/// Points kept per plugin metric.
const PLUGIN_HISTORY_LEN: usize = 60;
/// How often the plugin metrics are read while the stats are shown.
const PLUGIN_METRICS_POLL_MS: u32 = 10_000;
/// How often when none came back: no plugin reports any, or the route is not there.
const PLUGIN_METRICS_IDLE_POLL_MS: u32 = 30_000;

fn rc_identity_key<T>(value: &Option<Rc<T>>) -> Option<usize> { value.as_ref().map(|rc| Rc::as_ptr(rc) as usize) }

fn push_capped_to(buffer: &mut VecDeque<f64>, value: f64, cap: usize) {
    while buffer.len() >= cap {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

fn push_capped(buffer: &mut VecDeque<f64>, value: f64) { push_capped_to(buffer, value, HISTORY_LEN); }

/// The number a plugin metric `value` shows, when it shows one: `12`, `3.5`, `42%`.
pub fn numeric_metric_value(value: &str) -> Option<f64> {
    let text = value.trim();
    text.strip_suffix('%').unwrap_or(text).trim().parse::<f64>().ok().filter(|number| number.is_finite())
}

/// A rolling series per numeric plugin metric, keyed by the metric key.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct PluginMetricsHistory {
    pub series: BTreeMap<String, VecDeque<f64>>,
}

impl PluginMetricsHistory {
    /// Appends the numeric values of one sample. A metric that is not in the
    /// sample any more loses its series.
    pub fn record(&mut self, metrics: &[PluginMetricDto]) {
        self.series.retain(|key, _| metrics.iter().any(|metric| &metric.key == key));
        for metric in metrics {
            if let Some(number) = numeric_metric_value(&metric.value) {
                push_capped_to(self.series.entry(metric.key.clone()).or_default(), number, PLUGIN_HISTORY_LEN);
            }
        }
    }
}

/// The latest plugin metrics with their history.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct PluginMetricsState {
    pub latest: Vec<PluginMetricDto>,
    pub history: PluginMetricsHistory,
}

impl Reducible for PluginMetricsState {
    type Action = Vec<PluginMetricDto>;

    fn reduce(self: Rc<Self>, sample: Vec<PluginMetricDto>) -> Rc<Self> {
        let mut history = self.history.clone();
        history.record(&sample);
        Rc::new(Self { latest: sample, history })
    }
}

/// Polls `GET /api/v1/plugins/metrics` while the component is mounted. The
/// route belongs to another wave: when it is missing, or fails, the list is
/// empty and nothing is reported.
#[hook]
pub fn use_plugin_metrics() -> Rc<PluginMetricsState> {
    let ui = use_plugin_ui();
    let state = use_reducer(PluginMetricsState::default);
    {
        let dispatcher = state.dispatcher();
        let service = ui.map(|ui| ui.service);
        use_effect_with((), move |()| {
            let cancelled = Rc::new(Cell::new(false));
            if let Some(service) = service {
                let cancelled = Rc::clone(&cancelled);
                yew::platform::spawn_local(async move {
                    while !cancelled.get() {
                        let sample = service.metrics().await;
                        if cancelled.get() {
                            break;
                        }
                        let wait = if sample.is_empty() { PLUGIN_METRICS_IDLE_POLL_MS } else { PLUGIN_METRICS_POLL_MS };
                        dispatcher.dispatch(sample);
                        TimeoutFuture::new(wait).await;
                    }
                });
            }
            move || cancelled.set(true)
        });
    }
    Rc::new((*state).clone())
}

/// Rolling time-series of the key server metrics, fed by the streaming status updates.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct MetricsHistory {
    pub cpu: VecDeque<f64>,
    pub memory: VecDeque<f64>,
    pub net_rx: VecDeque<f64>,
    pub net_tx: VecDeque<f64>,
    pub users: VecDeque<f64>,
    pub connections: VecDeque<f64>,
}

impl MetricsHistory {
    fn record_system(&mut self, info: &SystemInfo) {
        push_capped(&mut self.cpu, f64::from(info.cpu_usage));
        let mem_pct =
            if info.memory_total > 0 { (info.memory_usage as f64 / info.memory_total as f64) * 100.0 } else { 0.0 };
        push_capped(&mut self.memory, mem_pct);
        push_capped(&mut self.net_rx, info.net_rx_bytes_per_sec);
        push_capped(&mut self.net_tx, info.net_tx_bytes_per_sec);
    }

    fn record_status(&mut self, status: &StatusCheck) {
        push_capped(&mut self.users, status.active_users as f64);
        push_capped(&mut self.connections, status.active_user_connections as f64);
    }

    pub fn as_vec(buffer: &VecDeque<f64>) -> Rc<[f64]> { buffer.iter().copied().collect::<Vec<_>>().into() }
}

#[derive(Default)]
struct MetricsHistoryCursor {
    system_info: Option<Rc<SystemInfo>>,
    status: Option<Rc<StatusCheck>>,
}

#[hook]
pub fn use_metrics_history() -> Rc<MetricsHistory> {
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");
    let history = use_state(|| Rc::new(MetricsHistory::default()));
    let cursor = use_mut_ref(MetricsHistoryCursor::default);

    {
        let history = history.clone();
        let cursor = cursor.clone();
        let system_info = status_ctx.system_info.clone();
        let status = status_ctx.status.clone();
        let deps = (rc_identity_key(&system_info), rc_identity_key(&status));
        use_effect_with(deps, move |_| {
            let mut next = (**history).clone();
            let mut changed = false;
            let mut cursor = cursor.borrow_mut();

            if let Some(info) = system_info.as_ref() {
                let system_changed = cursor.system_info.as_ref().is_none_or(|prev| !Rc::ptr_eq(prev, info));
                if system_changed {
                    next.record_system(info);
                    cursor.system_info = Some(Rc::clone(info));
                    changed = true;
                }
            }

            if let Some(status) = status.as_ref() {
                let status_changed = cursor.status.as_ref().is_none_or(|prev| !Rc::ptr_eq(prev, status));
                if status_changed {
                    next.record_status(status);
                    cursor.status = Some(Rc::clone(status));
                    changed = true;
                }
            }

            if changed {
                history.set(Rc::new(next));
            }
            || ()
        });
    }

    (*history).clone()
}

#[cfg(test)]
mod tests {
    use super::{numeric_metric_value, MetricsHistory, PluginMetricsState, PLUGIN_HISTORY_LEN};
    use shared::model::{PluginMetricDto, StatusCheck, SystemInfo};
    use std::rc::Rc;
    use yew::Reducible;

    fn metric(key: &str, value: &str) -> PluginMetricDto {
        PluginMetricDto {
            key: key.to_string(),
            plugin: "demo".to_string(),
            label: key.to_string(),
            value: value.to_string(),
            unit: None,
            severity: None,
        }
    }

    #[test]
    fn numeric_values_are_read_with_or_without_a_percent_sign() {
        assert_eq!(numeric_metric_value("12"), Some(12.0));
        assert_eq!(numeric_metric_value(" 3.5 "), Some(3.5));
        assert_eq!(numeric_metric_value("42%"), Some(42.0));
        assert_eq!(numeric_metric_value("-1"), Some(-1.0));
        assert_eq!(numeric_metric_value("12 ms"), None);
        assert_eq!(numeric_metric_value("ok"), None);
        assert_eq!(numeric_metric_value(""), None);
        assert_eq!(numeric_metric_value("NaN"), None);
        assert_eq!(numeric_metric_value("inf"), None);
    }

    #[test]
    fn plugin_series_are_capped_at_sixty_points() {
        let mut state = Rc::new(PluginMetricsState::default());
        for n in 0..(PLUGIN_HISTORY_LEN + 25) {
            state = state.reduce(vec![metric("demo.queue", &n.to_string())]);
        }
        let series = &state.history.series["demo.queue"];
        assert_eq!(series.len(), 60);
        assert_eq!(series.front().copied(), Some(25.0));
        assert_eq!(series.back().copied(), Some(84.0));
    }

    #[test]
    fn only_numeric_values_get_a_series_and_vanished_metrics_lose_theirs() {
        let state =
            Rc::new(PluginMetricsState::default()).reduce(vec![metric("demo.n", "5"), metric("demo.text", "healthy")]);
        assert!(state.history.series.contains_key("demo.n"));
        assert!(!state.history.series.contains_key("demo.text"));
        assert_eq!(state.latest.len(), 2);
        let state = state.reduce(vec![metric("demo.text", "healthy")]);
        assert!(state.history.series.is_empty());
        let state = state.reduce(Vec::new());
        assert_eq!(state.latest, Vec::new());
    }

    fn sample_system(
        cpu_usage: f32,
        memory_usage: u64,
        net_rx_bytes_per_sec: f64,
        net_tx_bytes_per_sec: f64,
    ) -> SystemInfo {
        SystemInfo {
            cpu_usage,
            memory_usage,
            memory_total: 1_000,
            net_rx_bytes_per_sec,
            net_tx_bytes_per_sec,
            net_rx_bytes_total: 0,
            net_tx_bytes_total: 0,
            disk_total_bytes: 0,
            disk_free_bytes: 0,
        }
    }

    #[test]
    fn record_system_preserves_equal_samples() {
        let mut history = MetricsHistory::default();
        let info = sample_system(10.0, 500, 100.0, 200.0);

        history.record_system(&info);
        history.record_system(&info);

        assert_eq!(history.cpu.len(), 2);
        assert_eq!(history.memory.len(), 2);
        assert_eq!(history.net_rx.len(), 2);
        assert_eq!(history.net_tx.len(), 2);
        assert_eq!(history.cpu[0], history.cpu[1]);
    }

    #[test]
    fn record_system_records_disk_percentage_from_used_and_total() {
        let mut history = MetricsHistory::default();
        let info = SystemInfo {
            cpu_usage: 0.0,
            memory_usage: 0,
            memory_total: 0,
            net_rx_bytes_per_sec: 0.0,
            net_tx_bytes_per_sec: 0.0,
            net_rx_bytes_total: 0,
            net_tx_bytes_total: 0,
            disk_total_bytes: 1_000,
            disk_free_bytes: 250,
        };

        history.record_system(&info);
    }

    #[test]
    fn record_status_updates_users_without_system_sample() {
        let mut history = MetricsHistory::default();
        let status = StatusCheck { active_users: 7, active_user_connections: 11, ..StatusCheck::default() };

        history.record_status(&status);

        assert_eq!(history.users.len(), 1);
        assert_eq!(history.connections.len(), 1);
        assert_eq!(history.users[0], 7.0);
        assert_eq!(history.connections[0], 11.0);
        assert!(history.cpu.is_empty());
    }
}
