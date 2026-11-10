use shared::model::FailoverGroupRuntimeConfig;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Playback a member needs before its failure opens a fresh switch budget.
const FRESH_BUDGET_AFTER: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BudgetDecision {
    /// Reopen the current member, it played long enough to deserve another try.
    RetrySame,
    /// Select another member, excluding the ones already tried in this incident.
    Next { exclude: Vec<usize> },
    /// Stop switching: the incident budget or the flap guard is exhausted.
    GiveUp,
}

/// Decides how a running group stream reacts to a failing member. An incident starts at the
/// first trigger and ends after `stable_after_secs` of playback without a new one. Each stretch
/// without playback is bounded by `max_outage_secs`, and `max_switches` bounds the switches within
/// `max_switches_window_secs` across incidents.
pub(crate) struct FailoverBudget {
    budget: Duration,
    stable_reset: Duration,
    flap_window: Duration,
    flap_max_switches: u8,
    same_member_retries: u8,
    incident_started: Option<Instant>,
    last_trigger: Option<Instant>,
    tried: Vec<usize>,
    switches: VecDeque<Instant>,
    same_retries_left: u8,
    member_first_data: Option<Instant>,
}

impl FailoverBudget {
    pub(crate) fn new(failover: &FailoverGroupRuntimeConfig) -> Self {
        Self {
            budget: Duration::from_secs(failover.max_outage_secs),
            stable_reset: Duration::from_secs(failover.stable_after_secs),
            flap_window: Duration::from_secs(failover.max_switches_window_secs),
            flap_max_switches: failover.max_switches,
            same_member_retries: failover.same_source_retries,
            incident_started: None,
            last_trigger: None,
            tried: Vec::new(),
            switches: VecDeque::new(),
            same_retries_left: failover.same_source_retries,
            member_first_data: None,
        }
    }

    pub(crate) fn on_trigger(&mut self, current: usize, member_count: usize, now: Instant) -> BudgetDecision {
        self.last_trigger = Some(now);
        let started = *self.incident_started.get_or_insert(now);
        if now.saturating_duration_since(started) >= self.budget {
            return BudgetDecision::GiveUp;
        }
        if self.flap_max_switches > 0 {
            while self.switches.front().is_some_and(|at| now.saturating_duration_since(*at) > self.flap_window) {
                self.switches.pop_front();
            }
            if self.switches.len() >= usize::from(self.flap_max_switches) {
                return BudgetDecision::GiveUp;
            }
        }
        let stable =
            self.member_first_data.is_some_and(|first| now.saturating_duration_since(first) >= self.stable_reset);
        if stable && self.same_retries_left > 0 {
            self.same_retries_left -= 1;
            return BudgetDecision::RetrySame;
        }
        if !self.tried.contains(&current) {
            self.tried.push(current);
        }
        if self.tried.len() >= member_count {
            self.tried.retain(|index| *index == current);
        }
        BudgetDecision::Next { exclude: self.tried.clone() }
    }

    /// Whether a trigger now would give up, without starting or changing an incident.
    pub(crate) fn would_give_up(&self, now: Instant) -> bool {
        let budget_spent =
            self.incident_started.is_some_and(|started| now.saturating_duration_since(started) >= self.budget);
        let flapping = self.flap_max_switches > 0
            && self.switches.iter().filter(|at| now.saturating_duration_since(**at) <= self.flap_window).count()
                >= usize::from(self.flap_max_switches);
        budget_spent || flapping
    }

    pub(crate) fn on_switched(&mut self, index: usize, previous: usize, now: Instant) {
        if self.flap_max_switches > 0 {
            self.switches.push_back(now);
        }
        if index != previous {
            self.same_retries_left = self.same_member_retries;
        }
        self.member_first_data = None;
    }

    /// Called for packets sent to the client.
    pub(crate) fn on_data(&mut self, now: Instant) {
        let first_data = *self.member_first_data.get_or_insert(now);
        // The budget bounds the time without playback: a member that really plays restarts it,
        // while the members tried in this incident stay excluded until it ends. A member that
        // sends a few packets and fails (an error clip) does not.
        if self.incident_started.is_some()
            && now.saturating_duration_since(first_data) >= FRESH_BUDGET_AFTER.min(self.budget / 2)
        {
            self.incident_started = None;
        }
        if self.last_trigger.is_some_and(|last| now.saturating_duration_since(last) >= self.stable_reset) {
            self.incident_started = None;
            self.last_trigger = None;
            self.tried.clear();
            self.same_retries_left = self.same_member_retries;
        }
    }

    /// End of the current incident's switch budget.
    pub(crate) fn deadline(&self) -> Option<Instant> { self.incident_started.map(|started| started + self.budget) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::FailoverGroupRuntimeConfig;
    use std::time::{Duration, Instant};

    fn config(max_switches: u8) -> FailoverGroupRuntimeConfig {
        FailoverGroupRuntimeConfig {
            max_outage_secs: 20,
            stable_after_secs: 30,
            max_switches_window_secs: 600,
            max_switches,
            same_source_retries: 1,
            ..FailoverGroupRuntimeConfig::default()
        }
    }

    fn secs(start: Instant, secs: u64) -> Instant { start + Duration::from_secs(secs) }

    #[test]
    fn budget_exceeded_gives_up() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        assert_eq!(budget.on_trigger(0, 3, t0), BudgetDecision::Next { exclude: vec![0] });
        budget.on_switched(1, 0, secs(t0, 1));
        assert_eq!(budget.on_trigger(1, 3, secs(t0, 21)), BudgetDecision::GiveUp);
    }

    #[test]
    fn budget_resets_after_stable_playback() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        budget.on_trigger(0, 3, t0);
        budget.on_switched(1, 0, secs(t0, 1));
        budget.on_data(secs(t0, 2));
        budget.on_data(secs(t0, 31));
        assert_eq!(budget.deadline(), None);
        assert_eq!(budget.on_trigger(1, 3, secs(t0, 32)), BudgetDecision::RetrySame);
        assert_eq!(budget.deadline(), Some(secs(t0, 52)));
    }

    #[test]
    fn budget_is_per_incident_not_per_stream() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(0));
        for incident in 0..5u64 {
            let start = secs(t0, incident * 100);
            let current = usize::try_from(incident % 2).expect("index");
            let next = 1 - current;
            assert_ne!(budget.on_trigger(current, 2, start), BudgetDecision::GiveUp);
            budget.on_switched(next, current, start);
            budget.on_data(start);
            budget.on_data(start + Duration::from_secs(31));
        }
    }

    #[test]
    fn walk_restarts_after_full_round_within_budget() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        assert_eq!(budget.on_trigger(0, 3, t0), BudgetDecision::Next { exclude: vec![0] });
        budget.on_switched(1, 0, t0);
        assert_eq!(budget.on_trigger(1, 3, secs(t0, 1)), BudgetDecision::Next { exclude: vec![0, 1] });
        budget.on_switched(2, 1, secs(t0, 1));
        assert_eq!(budget.on_trigger(2, 3, secs(t0, 2)), BudgetDecision::Next { exclude: vec![2] });
    }

    #[test]
    fn flap_guard_gives_up() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(2));
        budget.on_trigger(0, 3, t0);
        budget.on_switched(1, 0, t0);
        budget.on_trigger(1, 3, secs(t0, 1));
        budget.on_switched(2, 1, secs(t0, 1));
        assert_eq!(budget.on_trigger(2, 3, secs(t0, 2)), BudgetDecision::GiveUp);
    }

    #[test]
    fn flap_guard_disabled_with_zero() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(0));
        for step in 0..10u64 {
            let current = usize::try_from(step % 3).expect("index");
            assert!(matches!(budget.on_trigger(current, 3, secs(t0, step)), BudgetDecision::Next { .. }));
            budget.on_switched((current + 1) % 3, current, secs(t0, step));
        }
    }

    #[test]
    fn fresh_member_failure_skips_same_member_retry() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        budget.on_data(t0);
        assert_eq!(budget.on_trigger(0, 2, secs(t0, 5)), BudgetDecision::Next { exclude: vec![0] });
    }

    #[test]
    fn stable_member_failure_retries_same_member_first() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        budget.on_data(t0);
        assert_eq!(budget.on_trigger(0, 2, secs(t0, 31)), BudgetDecision::RetrySame);
        budget.on_switched(0, 0, secs(t0, 31));
        budget.on_data(secs(t0, 32));
        assert_eq!(budget.on_trigger(0, 2, secs(t0, 33)), BudgetDecision::Next { exclude: vec![0] });
    }

    #[test]
    fn failure_after_good_playback_gets_a_fresh_budget() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        assert_eq!(budget.on_trigger(0, 3, t0), BudgetDecision::Next { exclude: vec![0] });
        budget.on_switched(1, 0, secs(t0, 1));
        budget.on_data(secs(t0, 1));
        budget.on_data(secs(t0, 25));
        assert_eq!(budget.on_trigger(1, 3, secs(t0, 26)), BudgetDecision::Next { exclude: vec![0, 1] });
    }

    #[test]
    fn flap_history_is_not_kept_without_guard() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(0));
        for step in 0..100u64 {
            budget.on_switched(1, 0, secs(t0, step));
        }
        assert!(budget.switches.is_empty());
    }

    #[test]
    fn brief_data_does_not_restart_budget() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(0));
        assert_eq!(budget.on_trigger(0, 2, t0), BudgetDecision::Next { exclude: vec![0] });
        budget.on_switched(1, 0, secs(t0, 1));
        budget.on_data(secs(t0, 1));
        budget.on_data(secs(t0, 3));
        assert_eq!(budget.on_trigger(1, 2, secs(t0, 21)), BudgetDecision::GiveUp);
    }

    #[test]
    fn would_give_up_leaves_the_incident_untouched() {
        let t0 = Instant::now();
        let mut budget = FailoverBudget::new(&config(6));
        assert!(!budget.would_give_up(t0));
        assert_eq!(budget.deadline(), None, "asking starts no incident");
        budget.on_trigger(0, 2, t0);
        assert!(budget.would_give_up(secs(t0, 21)));
        assert!(!budget.would_give_up(secs(t0, 5)));
    }
}
