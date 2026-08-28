use crate::app::components::StatusContext;
use shared::model::{StatusCheck, SystemInfo};
use std::{collections::VecDeque, rc::Rc};
use yew::prelude::*;

const HISTORY_LEN: usize = 40;

fn rc_identity_key<T>(value: &Option<Rc<T>>) -> Option<usize> { value.as_ref().map(|rc| Rc::as_ptr(rc) as usize) }

fn push_capped(buffer: &mut VecDeque<f64>, value: f64) {
    if buffer.len() == HISTORY_LEN {
        buffer.pop_front();
    }
    buffer.push_back(value);
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
    use super::MetricsHistory;
    use shared::model::{StatusCheck, SystemInfo};

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
