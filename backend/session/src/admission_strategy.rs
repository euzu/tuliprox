use log::debug;
use shared::model::AdmissionStrategy;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    NoMatch,
    Evict(EvictionTarget),
    Grace(GraceMode),
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
}

/// Metadata capturing which grace strategy was chosen and the original connection kind,
/// used to reconstruct the remaining-strategies slice on user-grace failure.
#[derive(Debug, Clone)]
pub struct GraceResolutionContext {
    /// Index of the grace strategy that was actually used.
    pub strategy_index: usize,
    /// Full effective strategy list for stable reconstruction of the remaining slice.
    ///
    /// `Arc` rather than `Vec`: this context is stored on `StreamInfo` and travels with
    /// every clone of it, and the list is immutable once the effective strategies have
    /// been resolved.
    pub strategies: Arc<[AdmissionStrategy]>,
    /// The original `ConnectionKind` from the admission decision that led to this grace.
    /// Preserved so that the remaining-strategy fallback can return the correct kind
    /// (e.g., `Soft`) even when the grace itself hardcoded `Normal`.
    // Stored so the original admission kind remains available when follow-up
    // grace fallback reconstruction starts using it again.
    #[allow(dead_code)]
    pub kind: Option<super::ConnectionKind>,
}

pub fn evaluate_strategy(
    strategy: AdmissionStrategy,
    ctx: &StrategyContext<'_>,
    candidates: &[EvictionCandidate],
) -> AdmissionDecision {
    match strategy {
        AdmissionStrategy::EvictUserSameIpOldest => evaluate_evict_same_ip(ctx, candidates, EvictionOrder::Oldest),
        AdmissionStrategy::EvictUserSameIpLatest => evaluate_evict_same_ip(ctx, candidates, EvictionOrder::Latest),
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
    let selected = select_candidate(candidates.iter().filter(|candidate| candidate.client_ip == ctx.client_ip), order);

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
        Some(candidate) => AdmissionDecision::Evict(EvictionTarget { addr: candidate.addr }),
        None => AdmissionDecision::NoMatch,
    }
}

fn evaluate_evict_user(candidates: &[EvictionCandidate], order: EvictionOrder) -> AdmissionDecision {
    match select_candidate(candidates.iter(), order) {
        Some(candidate) => AdmissionDecision::Evict(EvictionTarget { addr: candidate.addr }),
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

    fn addr(port: u16) -> SocketAddr { SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port) }

    fn candidate(port: u16, ip: &str, ts: u64) -> EvictionCandidate {
        EvictionCandidate { addr: addr(port), client_ip: ip.to_string(), ts }
    }

    #[test]
    fn grace_strategy_returns_grace() {
        let ctx = StrategyContext { username: "user1", client_ip: "1.1.1.1" };
        let candidates = vec![candidate(1000, "1.1.1.1", 100)];
        let result = evaluate_strategy(AdmissionStrategy::GraceHoldStream, &ctx, &candidates);
        assert_eq!(result, AdmissionDecision::Grace(GraceMode::Hold));
    }

    fn assert_evict(strategy: AdmissionStrategy, candidates: &[EvictionCandidate], expected_port: u16) {
        let ctx = StrategyContext { username: "user1", client_ip: "1.1.1.1" };
        let result = evaluate_strategy(strategy, &ctx, candidates);
        match result {
            AdmissionDecision::Evict(target) => assert_eq!(target.addr, addr(expected_port)),
            other => panic!("Expected Evict, got {other:?}"),
        }
    }

    #[test]
    fn evict_oldest_same_ip() {
        let candidates =
            vec![candidate(1000, "1.1.1.1", 100), candidate(1001, "1.1.1.1", 200), candidate(1002, "2.2.2.2", 50)];
        assert_evict(AdmissionStrategy::EvictUserSameIpOldest, &candidates, 1000);
    }

    #[test]
    fn evict_latest_same_ip() {
        let candidates =
            vec![candidate(1000, "1.1.1.1", 100), candidate(1001, "1.1.1.1", 200), candidate(1002, "2.2.2.2", 50)];
        assert_evict(AdmissionStrategy::EvictUserSameIpLatest, &candidates, 1001);
    }

    #[test]
    fn evict_oldest_user_regardless_of_ip() {
        let candidates =
            vec![candidate(1000, "2.2.2.2", 300), candidate(1001, "1.1.1.1", 200), candidate(1002, "3.3.3.3", 100)];
        assert_evict(AdmissionStrategy::EvictUserOldest, &candidates, 1002);
    }

    #[test]
    fn evict_latest_user_regardless_of_ip() {
        let candidates =
            vec![candidate(1000, "2.2.2.2", 300), candidate(1001, "1.1.1.1", 200), candidate(1002, "3.3.3.3", 100)];
        assert_evict(AdmissionStrategy::EvictUserLatest, &candidates, 1000);
    }

    #[test]
    fn no_same_ip_candidate_returns_no_match() {
        let ctx = StrategyContext { username: "user1", client_ip: "1.1.1.1" };
        let candidates = vec![candidate(1000, "2.2.2.2", 100)];
        let result = evaluate_strategy(AdmissionStrategy::EvictUserSameIpOldest, &ctx, &candidates);
        assert_eq!(result, AdmissionDecision::NoMatch);
    }
}
