use axum::http::StatusCode;
use dashmap::DashMap;
use shared::model::FailoverGroupCircuitConfig;
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

pub type MemberKey = (Arc<str>, Arc<str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Stream,
    Transport,
    Auth,
    Capacity,
}

impl FailureClass {
    pub fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            401 | 403 => Self::Auth,
            429 | 509 => Self::Capacity,
            _ => Self::Stream,
        }
    }

    pub fn opens_member_circuit(self) -> bool { !matches!(self, Self::Capacity) }

    pub fn counts_for_input(self) -> bool { matches!(self, Self::Transport | Self::Auth) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitView {
    Closed,
    Counting(u8),
    Open { remaining: Duration },
    HalfOpenInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitPolicy {
    pub failures: u8,
    pub cooldown: Duration,
    pub cooldown_max: Duration,
    pub input_failures: u8,
    pub input_window: Duration,
}

impl CircuitPolicy {
    pub fn new(failures: u8, cooldown: Duration, cooldown_max: Duration) -> Self {
        Self { failures, cooldown, cooldown_max, input_failures: 3, input_window: Duration::from_secs(120) }
    }

    pub fn with_input(mut self, input_failures: u8, input_window: Duration) -> Self {
        self.input_failures = input_failures;
        self.input_window = input_window;
        self
    }
}

impl Default for CircuitPolicy {
    fn default() -> Self { Self::from(&FailoverGroupCircuitConfig::default()) }
}

impl From<&FailoverGroupCircuitConfig> for CircuitPolicy {
    fn from(cfg: &FailoverGroupCircuitConfig) -> Self {
        Self {
            failures: cfg.source_failures,
            cooldown: Duration::from_secs(cfg.cooldown_secs),
            cooldown_max: Duration::from_secs(cfg.cooldown_max_secs),
            input_failures: cfg.input_failures,
            input_window: Duration::from_secs(cfg.input_window_secs),
        }
    }
}

pub use shared::model::CircuitState;

#[derive(Debug, Clone)]
pub struct CircuitEntry {
    pub failures: u8,
    pub open_level: u8,
    pub state: CircuitState,
}

impl Default for CircuitEntry {
    fn default() -> Self { Self { failures: 0, open_level: 0, state: CircuitState::Closed } }
}

#[derive(Debug, Clone, Default)]
pub struct InputCircuitEntry {
    pub circuit: CircuitEntry,
    pub recent: VecDeque<(MemberKey, Instant)>,
}

#[derive(Default)]
pub struct CircuitSharedState {
    pub circuits: DashMap<MemberKey, CircuitEntry>,
    pub input_circuits: DashMap<Arc<str>, InputCircuitEntry>,
}

pub struct HalfOpenGuard {
    pub(crate) member_key: MemberKey,
    pub(crate) input_name: Option<Arc<str>>,
    pub(crate) admit_time: Instant,
    pub(crate) shared: Arc<CircuitSharedState>,
    pub(crate) consumed: bool,
}

impl Drop for HalfOpenGuard {
    fn drop(&mut self) {
        if !self.consumed {
            if let Some(mut entry) = self.shared.circuits.get_mut(&self.member_key) {
                if entry.state == CircuitState::HalfOpenInFlight {
                    entry.state = CircuitState::OpenUntil(self.admit_time);
                }
            }
            if let Some(input) = &self.input_name {
                if let Some(mut entry) = self.shared.input_circuits.get_mut(input) {
                    if entry.circuit.state == CircuitState::HalfOpenInFlight {
                        entry.circuit.state = CircuitState::OpenUntil(self.admit_time);
                    }
                }
            }
        }
    }
}

pub enum CircuitAdmission {
    Closed,
    HalfOpen(HalfOpenGuard),
    Open,
}

pub(crate) fn compute_cooldown(policy: &CircuitPolicy, open_level: u8) -> Duration {
    let multiplier = 1u32.checked_shl(u32::from(open_level.min(16))).unwrap_or(u32::MAX);
    let scaled = policy.cooldown.saturating_mul(multiplier);
    scaled.min(policy.cooldown_max)
}

pub(crate) fn record_input_failure_in_shared(
    shared: &CircuitSharedState,
    key: &MemberKey,
    now: Instant,
    policy: &CircuitPolicy,
) {
    let mut entry = shared.input_circuits.entry(Arc::clone(&key.0)).or_default();
    entry.recent.push_back((key.clone(), now));
    let window = policy.input_window;
    entry.recent.retain(|(_, ts)| if *ts <= now { now.duration_since(*ts) <= window } else { true });
    while entry.recent.len() > usize::from(policy.input_failures) * 4 {
        entry.recent.pop_front();
    }
    let mut distinct = HashSet::new();
    for (k, _) in &entry.recent {
        distinct.insert(k);
        if distinct.len() >= 2 {
            break;
        }
    }
    if entry.recent.len() >= usize::from(policy.input_failures)
        && distinct.len() >= 2
        && entry.circuit.state == CircuitState::Closed
    {
        let cooldown = compute_cooldown(policy, entry.circuit.open_level);
        entry.circuit.state = CircuitState::OpenUntil(now + cooldown);
        entry.circuit.open_level = entry.circuit.open_level.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover_group::health::MemberHealthIndex;

    fn key(input: &str, stream_id: &str) -> MemberKey { (input.into(), stream_id.into()) }

    #[test]
    fn opens_after_threshold_and_backs_off_exponentially() {
        let index = MemberHealthIndex::new(CircuitPolicy::new(2, Duration::from_secs(60), Duration::from_mins(15)));
        let k = key("solis", "1");
        let t0 = Instant::now();
        index.record_failure(&k, t0);
        assert!(matches!(index.admit(&k, t0), CircuitAdmission::Closed));
        index.record_failure(&k, t0);
        assert!(matches!(index.admit(&k, t0), CircuitAdmission::Open));
        let t1 = t0 + Duration::from_secs(61);
        let CircuitAdmission::HalfOpen(guard) = index.admit(&k, t1) else { panic!("expected half-open") };
        index.record_failure_with(guard, t1);
        assert!(matches!(index.admit(&k, t1 + Duration::from_secs(119)), CircuitAdmission::Open));
        assert!(matches!(index.admit(&k, t1 + Duration::from_secs(121)), CircuitAdmission::HalfOpen(_)));
    }

    #[test]
    fn half_open_is_single_flight_under_concurrency() {
        let index = MemberHealthIndex::new(CircuitPolicy::new(1, Duration::from_secs(1), Duration::from_secs(8)));
        let k = key("korath", "7");
        let t0 = Instant::now();
        index.record_failure(&k, t0);
        let later = t0 + Duration::from_secs(2);
        let granted = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    if let CircuitAdmission::HalfOpen(guard) = index.admit(&k, later) {
                        granted.lock().expect("lock").push(guard);
                    }
                });
            }
        });
        assert_eq!(granted.lock().expect("lock").len(), 1);
    }

    #[test]
    fn dropped_half_open_guard_allows_next_probe() {
        let index = MemberHealthIndex::new(CircuitPolicy::new(1, Duration::from_secs(1), Duration::from_secs(8)));
        let k = key("veyra", "3");
        let t0 = Instant::now();
        index.record_failure(&k, t0);
        let later = t0 + Duration::from_secs(2);
        drop(index.admit(&k, later));
        assert!(matches!(index.admit(&k, later), CircuitAdmission::HalfOpen(_)));
    }

    #[test]
    fn success_closes_and_resets_backoff() {
        let index = MemberHealthIndex::new(CircuitPolicy::new(1, Duration::from_secs(1), Duration::from_secs(8)));
        let k = key("orun", "4");
        let t0 = Instant::now();
        index.record_failure(&k, t0);
        let CircuitAdmission::HalfOpen(guard) = index.admit(&k, t0 + Duration::from_secs(2)) else { panic!() };
        index.record_success_with(guard);
        assert!(matches!(index.admit(&k, t0 + Duration::from_secs(2)), CircuitAdmission::Closed));
        assert!(matches!(index.circuit_view(&k, t0 + Duration::from_secs(2)), CircuitView::Closed));
    }

    #[test]
    fn m3u_members_without_provider_id_do_not_share_a_circuit() {
        let index = MemberHealthIndex::new(CircuitPolicy::new(1, Duration::from_secs(60), Duration::from_secs(60)));
        let t0 = Instant::now();
        index.record_failure(&key("m3u-in", "chan-a"), t0);
        assert!(matches!(index.admit(&key("m3u-in", "chan-a"), t0), CircuitAdmission::Open));
        assert!(matches!(index.admit(&key("m3u-in", "chan-b"), t0), CircuitAdmission::Closed));
    }

    #[test]
    fn input_circuit_needs_two_distinct_members() {
        let index = MemberHealthIndex::new(
            CircuitPolicy::new(5, Duration::from_secs(60), Duration::from_secs(60))
                .with_input(3, Duration::from_secs(120)),
        );
        let t0 = Instant::now();
        for _ in 0..3 {
            index.record_classified_failure(&key("solis", "1"), FailureClass::Transport, t0);
        }
        assert!(matches!(index.input_circuit_view("solis", t0), CircuitView::Closed | CircuitView::Counting(_)));
        index.record_classified_failure(&key("solis", "2"), FailureClass::Auth, t0);
        assert!(matches!(index.input_circuit_view("solis", t0), CircuitView::Open { .. }));
        assert!(matches!(index.admit(&key("solis", "77"), t0), CircuitAdmission::Open));
        assert!(matches!(index.admit(&key("korath", "1"), t0), CircuitAdmission::Closed));
    }

    #[test]
    fn input_failures_outside_window_do_not_count() {
        let index = MemberHealthIndex::new(
            CircuitPolicy::new(5, Duration::from_secs(60), Duration::from_secs(60))
                .with_input(2, Duration::from_secs(120)),
        );
        let t0 = Instant::now();
        index.record_classified_failure(&key("solis", "1"), FailureClass::Transport, t0);
        index.record_classified_failure(&key("solis", "2"), FailureClass::Transport, t0 + Duration::from_secs(121));
        assert!(!matches!(index.input_circuit_view("solis", t0 + Duration::from_secs(121)), CircuitView::Open { .. }));
    }

    #[test]
    fn capacity_failure_never_opens_a_circuit() {
        let index = MemberHealthIndex::new(
            CircuitPolicy::new(1, Duration::from_secs(60), Duration::from_secs(60))
                .with_input(2, Duration::from_secs(120)),
        );
        let t0 = Instant::now();
        index.record_classified_failure(&key("solis", "1"), FailureClass::Capacity, t0);
        index.record_classified_failure(&key("solis", "2"), FailureClass::Capacity, t0);
        assert!(matches!(index.admit(&key("solis", "1"), t0), CircuitAdmission::Closed));
        assert!(matches!(index.input_circuit_view("solis", t0), CircuitView::Closed));
    }

    #[test]
    fn failure_class_from_status() {
        assert_eq!(FailureClass::from_status(StatusCode::UNAUTHORIZED), FailureClass::Auth);
        assert_eq!(FailureClass::from_status(StatusCode::FORBIDDEN), FailureClass::Auth);
        assert_eq!(FailureClass::from_status(StatusCode::TOO_MANY_REQUESTS), FailureClass::Capacity);
        assert_eq!(FailureClass::from_status(StatusCode::from_u16(509).unwrap()), FailureClass::Capacity);
        assert_eq!(FailureClass::from_status(StatusCode::NOT_FOUND), FailureClass::Stream);
        assert_eq!(FailureClass::from_status(StatusCode::SERVICE_UNAVAILABLE), FailureClass::Stream);
    }
}
