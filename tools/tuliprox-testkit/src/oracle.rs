use crate::TestkitError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStrategy {
    EvictUserSameIpOldest,
    EvictUserSameIpLatest,
    EvictUserOldest,
    EvictUserLatest,
    GraceInstantStream,
    GraceHoldStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraceMode {
    Instant,
    HoldStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub playback_id: String,
    pub client_ip: String,
    pub started_order: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Admit,
    AdmitSoft,
    Deny,
    Evict { playback_id: String },
    Grace { mode: GraceMode },
}

#[derive(Debug)]
pub struct AdmissionOracle {
    max_connections: usize,
    soft_connections: usize,
    strategies: Vec<AdmissionStrategy>,
    grace: Option<GraceMode>,
    active: HashMap<String, Request>,
}

impl AdmissionOracle {
    #[must_use]
    pub fn new(max_connections: usize, strategies: Vec<AdmissionStrategy>) -> Self {
        Self { max_connections, soft_connections: 0, strategies, grace: None, active: HashMap::new() }
    }

    #[must_use]
    pub fn with_policy(
        max_connections: usize,
        soft_connections: usize,
        strategies: Vec<AdmissionStrategy>,
        grace: Option<GraceMode>,
    ) -> Self {
        Self { max_connections, soft_connections, strategies, grace, active: HashMap::new() }
    }

    pub fn admit_with_evicted(&mut self, request: Request) -> Result<(Decision, Option<Request>), TestkitError> {
        if self.active.contains_key(&request.playback_id) {
            return Err(TestkitError::Configuration("duplicate oracle playback".to_owned()));
        }
        if self.active.len() < self.max_connections {
            self.active.insert(request.playback_id.clone(), request);
            return Ok((Decision::Admit, None));
        }
        if self.active.len() < self.max_connections.saturating_add(self.soft_connections) {
            self.active.insert(request.playback_id.clone(), request);
            return Ok((Decision::AdmitSoft, None));
        }
        for strategy in &self.strategies {
            if let Some(mode) = grace_mode(*strategy) {
                return Ok((Decision::Grace { mode }, None));
            }
            let candidate = self.select(&request, *strategy)?;
            if let Some(candidate) = candidate {
                let victim = candidate.playback_id.clone();
                let evicted_req = self.active.remove(&victim);
                self.active.insert(request.playback_id.clone(), request);
                return Ok((Decision::Evict { playback_id: victim }, evicted_req));
            }
        }
        Ok((
            match self.grace {
                Some(mode) => Decision::Grace { mode },
                None => Decision::Deny,
            },
            None,
        ))
    }

    pub fn admit(&mut self, request: Request) -> Result<Decision, TestkitError> {
        self.admit_with_evicted(request).map(|(decision, _)| decision)
    }

    pub fn rollback(&mut self, playback_id: &str, evicted: Option<Request>) {
        self.active.remove(playback_id);
        if let Some(restored) = evicted {
            self.active.insert(restored.playback_id.clone(), restored);
        }
    }

    pub fn release(&mut self, playback_id: &str) -> Result<(), TestkitError> {
        self.active
            .remove(playback_id)
            .map(|_| ())
            .ok_or_else(|| TestkitError::Configuration(format!("cannot release inactive playback {playback_id}")))
    }

    fn select(&self, incoming: &Request, strategy: AdmissionStrategy) -> Result<Option<&Request>, TestkitError> {
        if grace_mode(strategy).is_some() {
            return Ok(None);
        }
        let same_ip =
            matches!(strategy, AdmissionStrategy::EvictUserSameIpOldest | AdmissionStrategy::EvictUserSameIpLatest);
        let latest = matches!(strategy, AdmissionStrategy::EvictUserSameIpLatest | AdmissionStrategy::EvictUserLatest);
        let candidates = self.active.values().filter(|candidate| !same_ip || candidate.client_ip == incoming.client_ip);
        let selected = if latest {
            candidates.max_by_key(|candidate| candidate.started_order)
        } else {
            candidates.min_by_key(|candidate| candidate.started_order)
        };
        let Some(selected) = selected else { return Ok(None) };
        let tied = self.active.values().any(|candidate| {
            candidate.playback_id != selected.playback_id
                && (!same_ip || candidate.client_ip == incoming.client_ip)
                && candidate.started_order == selected.started_order
        });
        if tied {
            return Err(TestkitError::Protocol("candidate ages are ambiguous at SUT timestamp resolution".to_owned()));
        }
        Ok(Some(selected))
    }
}

const fn grace_mode(strategy: AdmissionStrategy) -> Option<GraceMode> {
    match strategy {
        AdmissionStrategy::GraceInstantStream => Some(GraceMode::Instant),
        AdmissionStrategy::GraceHoldStream => Some(GraceMode::HoldStream),
        AdmissionStrategy::EvictUserSameIpOldest
        | AdmissionStrategy::EvictUserSameIpLatest
        | AdmissionStrategy::EvictUserOldest
        | AdmissionStrategy::EvictUserLatest => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_replaces_latest_candidate() {
        let mut oracle = AdmissionOracle::new(2, vec![AdmissionStrategy::EvictUserSameIpLatest]);
        for (id, order) in [("a", 1), ("b", 2)] {
            assert!(matches!(
                oracle.admit(Request {
                    playback_id: id.to_owned(),
                    client_ip: "10.0.0.1".to_owned(),
                    started_order: order
                }),
                Ok(Decision::Admit)
            ));
        }
        assert!(
            matches!(oracle.admit(Request { playback_id: "c".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 3 }), Ok(Decision::Evict { playback_id }) if playback_id == "b")
        );
    }

    #[test]
    fn soft_slot_precedes_eviction() {
        let mut oracle =
            AdmissionOracle::with_policy(1, 1, vec![AdmissionStrategy::EvictUserLatest], Some(GraceMode::HoldStream));
        assert!(matches!(
            oracle.admit(Request { playback_id: "a".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 1 }),
            Ok(Decision::Admit)
        ));
        assert!(matches!(
            oracle.admit(Request { playback_id: "b".to_owned(), client_ip: "10.0.0.2".to_owned(), started_order: 2 }),
            Ok(Decision::AdmitSoft)
        ));
        assert!(
            matches!(oracle.admit(Request { playback_id: "c".to_owned(), client_ip: "10.0.0.3".to_owned(), started_order: 3 }), Ok(Decision::Evict { playback_id }) if playback_id == "b")
        );
        oracle.release("a").unwrap();
        oracle.release("c").unwrap();
        assert!(matches!(
            oracle.admit(Request { playback_id: "d".to_owned(), client_ip: "10.0.0.4".to_owned(), started_order: 4 }),
            Ok(Decision::Admit)
        ));
        assert!(matches!(
            oracle.admit(Request { playback_id: "e".to_owned(), client_ip: "10.0.0.5".to_owned(), started_order: 5 }),
            Ok(Decision::AdmitSoft)
        ));
    }

    #[test]
    fn soft_slot_exhausts_before_deny() {
        let mut oracle = AdmissionOracle::with_policy(1, 1, Vec::new(), None);
        assert!(matches!(
            oracle.admit(Request { playback_id: "a".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 1 }),
            Ok(Decision::Admit)
        ));
        assert!(matches!(
            oracle.admit(Request { playback_id: "b".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 2 }),
            Ok(Decision::AdmitSoft)
        ));
        assert!(matches!(
            oracle.admit(Request { playback_id: "c".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 3 }),
            Ok(Decision::Deny)
        ));
        // Releasing the soft slot frees the extra capacity again.
        oracle.release("b").unwrap();
        assert!(matches!(
            oracle.admit(Request { playback_id: "d".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 4 }),
            Ok(Decision::AdmitSoft)
        ));
    }

    #[test]
    fn grace_applies_when_capacity_has_no_eviction_strategy() {
        let mut oracle = AdmissionOracle::with_policy(1, 0, Vec::new(), Some(GraceMode::HoldStream));
        assert!(oracle
            .admit(Request { playback_id: "a".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 1 })
            .is_ok());
        assert!(matches!(
            oracle.admit(Request { playback_id: "b".to_owned(), client_ip: "10.0.0.2".to_owned(), started_order: 2 }),
            Ok(Decision::Grace { mode: GraceMode::HoldStream })
        ));
    }

    #[test]
    fn grace_strategy_is_evaluated_in_its_configured_order() {
        let mut oracle = AdmissionOracle::new(1, vec![AdmissionStrategy::GraceInstantStream]);
        assert!(oracle
            .admit(Request { playback_id: "a".to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 1 })
            .is_ok());
        assert!(matches!(
            oracle.admit(Request { playback_id: "b".to_owned(), client_ip: "10.0.0.2".to_owned(), started_order: 2 }),
            Ok(Decision::Grace { mode: GraceMode::Instant })
        ));
    }

    #[test]
    fn equal_candidate_ages_are_ambiguous() {
        let mut oracle = AdmissionOracle::new(2, vec![AdmissionStrategy::EvictUserOldest]);
        for id in ["a", "b"] {
            assert!(oracle
                .admit(Request { playback_id: id.to_owned(), client_ip: "10.0.0.1".to_owned(), started_order: 1 })
                .is_ok());
        }
        assert!(oracle
            .admit(Request { playback_id: "c".to_owned(), client_ip: "10.0.0.2".to_owned(), started_order: 2 })
            .is_err());
    }

    #[test]
    fn sequential_same_ip_eviction_a_then_b_then_c() {
        let mut oracle = AdmissionOracle::with_policy(
            1,
            0,
            vec![
                AdmissionStrategy::EvictUserSameIpLatest,
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::EvictUserLatest,
                AdmissionStrategy::EvictUserOldest,
            ],
            Some(GraceMode::HoldStream),
        );
        // A admitted on first slot
        assert!(matches!(
            oracle.admit(Request { playback_id: "a".to_owned(), client_ip: "10.20.0.10".to_owned(), started_order: 1 }),
            Ok(Decision::Admit)
        ));
        // B evicts A (same IP, latest = A is latest, so A is evicted)
        assert!(matches!(
            oracle.admit(Request { playback_id: "b".to_owned(), client_ip: "10.20.0.10".to_owned(), started_order: 2 }),
            Ok(Decision::Evict { ref playback_id }) if playback_id == "a"
        ));
        // C evicts B (same IP, latest = B is latest)
        assert!(matches!(
            oracle.admit(Request { playback_id: "c".to_owned(), client_ip: "10.20.0.10".to_owned(), started_order: 3 }),
            Ok(Decision::Evict { ref playback_id }) if playback_id == "b"
        ));
        // Only C remains
        assert_eq!(oracle.active.len(), 1);
        assert!(oracle.active.contains_key("c"));
    }
}
