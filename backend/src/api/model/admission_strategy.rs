use log::debug;
use std::net::SocketAddr;
use shared::model::AdmissionStrategy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    NoMatch,
    Evict(EvictionTarget),
    Grace(GraceMode),
    #[cfg_attr(not(test), allow(dead_code))]
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceMode {
    Instant,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionTarget {
    pub addr: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct StrategyContext<'a> {
    pub username: &'a str,
    pub client_ip: &'a str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub strategies: &'a [AdmissionStrategy],
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn evaluate_admission_strategies(
    ctx: &StrategyContext<'_>,
    candidates: &[EvictionCandidate],
) -> AdmissionDecision {
    if ctx.strategies.is_empty() {
        debug!("No admission strategies configured, denying request for user {}", ctx.username);
        return AdmissionDecision::Deny;
    }

    debug!(
        "Evaluating {} admission strategies for user {}",
        ctx.strategies.len(),
        ctx.username
    );

    for strategy in ctx.strategies {
        let decision = evaluate_strategy(*strategy, ctx, candidates);
        match decision {
            AdmissionDecision::NoMatch => {}
            AdmissionDecision::Evict(target) => {
                debug!(
                    "Strategy {:?} selected eviction target {} for user {}",
                    strategy, target.addr, ctx.username
                );
                return AdmissionDecision::Evict(target);
            }
            AdmissionDecision::Grace(mode) => {
                debug!(
                    "Strategy {:?} selected grace mode {:?} for user {} (blocking later strategies)",
                    strategy, mode, ctx.username
                );
                return AdmissionDecision::Grace(mode);
            }
            AdmissionDecision::Deny => {
                debug!("Strategy {:?} denied request for user {}", strategy, ctx.username);
                return AdmissionDecision::Deny;
            }
        }
    }

    debug!(
        "No admission strategy matched for user {}, denying",
        ctx.username
    );
    AdmissionDecision::Deny
}

pub(in crate::api) fn evaluate_strategy(
    strategy: AdmissionStrategy,
    ctx: &StrategyContext<'_>,
    candidates: &[EvictionCandidate],
) -> AdmissionDecision {
    match strategy {
        AdmissionStrategy::EvictUserSameIpOldest => {
            evaluate_evict_same_ip(ctx, candidates, EvictionOrder::Oldest)
        }
        AdmissionStrategy::EvictUserSameIpLatest => {
            evaluate_evict_same_ip(ctx, candidates, EvictionOrder::Latest)
        }
        AdmissionStrategy::EvictUserOldest => evaluate_evict_user(candidates, EvictionOrder::Oldest),
        AdmissionStrategy::EvictUserLatest => evaluate_evict_user(candidates, EvictionOrder::Latest),
        AdmissionStrategy::GraceInstantStream => AdmissionDecision::Grace(GraceMode::Instant),
        AdmissionStrategy::GraceHoldStream => AdmissionDecision::Grace(GraceMode::Hold),
    }
}

#[derive(Clone, Copy)]
enum EvictionOrder {
    Oldest,
    Latest,
}

fn evaluate_evict_same_ip(
    ctx: &StrategyContext<'_>,
    candidates: &[EvictionCandidate],
    order: EvictionOrder,
) -> AdmissionDecision {
    let selected = select_candidate(
        candidates.iter().filter(|candidate| candidate.client_ip == ctx.client_ip),
        order,
    );

    if selected.is_none() {
        debug!(
            "Strategy EvictUserSameIp{:?}: no same-IP candidates for user {}",
            match order {
                EvictionOrder::Oldest => "Oldest",
                EvictionOrder::Latest => "Latest",
            },
            ctx.username
        );
        return AdmissionDecision::NoMatch;
    }

    match selected {
        Some(candidate) => AdmissionDecision::Evict(EvictionTarget {
            addr: candidate.addr,
        }),
        None => AdmissionDecision::NoMatch,
    }
}

fn evaluate_evict_user(candidates: &[EvictionCandidate], order: EvictionOrder) -> AdmissionDecision {
    match select_candidate(candidates.iter(), order) {
        Some(candidate) => AdmissionDecision::Evict(EvictionTarget {
            addr: candidate.addr,
        }),
        None => AdmissionDecision::NoMatch,
    }
}

fn select_candidate<'a>(
    candidates: impl Iterator<Item = &'a EvictionCandidate>,
    order: EvictionOrder,
) -> Option<&'a EvictionCandidate> {
    let mut selected: Option<&EvictionCandidate> = None;
    for candidate in candidates {
        let should_replace = match selected {
            None => true,
            Some(current) => match order {
                EvictionOrder::Oldest => candidate.ts < current.ts,
                EvictionOrder::Latest => candidate.ts > current.ts,
            },
        };
        if should_replace {
            selected = Some(candidate);
        }
    }
    selected
}

#[derive(Debug, Clone)]
pub struct EvictionCandidate {
    pub addr: SocketAddr,
    pub client_ip: String,
    pub ts: u64,
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    fn candidate(port: u16, ip: &str, ts: u64) -> EvictionCandidate {
        EvictionCandidate {
            addr: addr(port),
            client_ip: ip.to_string(),
            ts,
        }
    }

    #[test]
    fn empty_strategy_list_returns_deny() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[],
        };
        assert_eq!(evaluate_admission_strategies(&ctx, &[]), AdmissionDecision::Deny);
    }

    #[test]
    fn evaluator_stops_at_first_matching_strategy() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[
                AdmissionStrategy::GraceInstantStream,
                AdmissionStrategy::EvictUserSameIpOldest,
            ],
        };
        let candidates = vec![candidate(1000, "1.1.1.1", 100)];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        assert_eq!(result, AdmissionDecision::Grace(GraceMode::Instant));
    }

    #[test]
    fn grace_is_blocking() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserSameIpOldest,
            ],
        };
        let candidates = vec![candidate(1000, "1.1.1.1", 100)];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        assert_eq!(result, AdmissionDecision::Grace(GraceMode::Hold));
    }

    #[test]
    fn evict_oldest_same_ip() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[AdmissionStrategy::EvictUserSameIpOldest],
        };
        let candidates = vec![
            candidate(1000, "1.1.1.1", 100),
            candidate(1001, "1.1.1.1", 200),
            candidate(1002, "2.2.2.2", 50),
        ];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        match result {
            AdmissionDecision::Evict(target) => assert_eq!(target.addr, addr(1000)),
            other => panic!("Expected Evict, got {other:?}"),
        }
    }

    #[test]
    fn evict_latest_same_ip() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[AdmissionStrategy::EvictUserSameIpLatest],
        };
        let candidates = vec![
            candidate(1000, "1.1.1.1", 100),
            candidate(1001, "1.1.1.1", 200),
            candidate(1002, "2.2.2.2", 50),
        ];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        match result {
            AdmissionDecision::Evict(target) => assert_eq!(target.addr, addr(1001)),
            other => panic!("Expected Evict, got {other:?}"),
        }
    }

    #[test]
    fn evict_oldest_user_regardless_of_ip() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[AdmissionStrategy::EvictUserOldest],
        };
        let candidates = vec![
            candidate(1000, "2.2.2.2", 300),
            candidate(1001, "1.1.1.1", 200),
            candidate(1002, "3.3.3.3", 100),
        ];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        match result {
            AdmissionDecision::Evict(target) => assert_eq!(target.addr, addr(1002)),
            other => panic!("Expected Evict, got {other:?}"),
        }
    }

    #[test]
    fn evict_latest_user_regardless_of_ip() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[AdmissionStrategy::EvictUserLatest],
        };
        let candidates = vec![
            candidate(1000, "2.2.2.2", 300),
            candidate(1001, "1.1.1.1", 200),
            candidate(1002, "3.3.3.3", 100),
        ];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        match result {
            AdmissionDecision::Evict(target) => assert_eq!(target.addr, addr(1000)),
            other => panic!("Expected Evict, got {other:?}"),
        }
    }

    #[test]
    fn no_same_ip_candidate_returns_no_match_then_deny() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[AdmissionStrategy::EvictUserSameIpOldest],
        };
        let candidates = vec![candidate(1000, "2.2.2.2", 100)];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        assert_eq!(result, AdmissionDecision::Deny);
    }

    #[test]
    fn eviction_falls_through_to_grace() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::GraceHoldStream,
            ],
        };
        let candidates = vec![candidate(1000, "2.2.2.2", 100)];
        let result = evaluate_admission_strategies(&ctx, &candidates);
        assert_eq!(result, AdmissionDecision::Grace(GraceMode::Hold));
    }

    #[test]
    fn empty_candidates_with_eviction_then_grace() {
        let ctx = StrategyContext {
            username: "user1",
            client_ip: "1.1.1.1",
            strategies: &[
                AdmissionStrategy::EvictUserSameIpLatest,
                AdmissionStrategy::GraceInstantStream,
            ],
        };
        let result = evaluate_admission_strategies(&ctx, &[]);
        assert_eq!(result, AdmissionDecision::Grace(GraceMode::Instant));
    }
}
