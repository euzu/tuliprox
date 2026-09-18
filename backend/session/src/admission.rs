//! Admission: whether a request gets a connection, and what happens when it
//! cannot.
//!
//! Classifies a playback request, resolves it against the user's connection
//! limits, walks the configured admission strategies when the limit is reached,
//! and reconstructs the remaining strategies when a grace period fails.
//!
//! This ran in `api_utils` because that is where the HTTP handlers called it
//! from, but every type it decides over - `ConnectionAdmission`,
//! `ConnectionKind`, `AdmissionStrategy`, `GraceResolutionContext`,
//! `UserSession` - already lives here, and the only state it reads is the user
//! and connection managers next door.

use crate::{
    active_user_manager::{ActiveUserManager, UserSession},
    admission_strategy::GraceResolutionContext,
    connection_manager::ConnectionManager,
};
use log::debug;
use shared::model::{AdmissionStrategy, ConnectionDenied, EventMessage, UserConnectionPermission, VirtualId};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, Fingerprint, ProxyUserCredentials};

/// The handles admission reads from the running server.
#[derive(Clone)]
pub struct AdmissionCtx {
    /// Resolved configuration; re-read on each use because it is hot-swapped.
    pub app_config: Arc<AppConfig>,
    /// Per-user session accounting: who holds how many connections.
    pub active_users: Arc<ActiveUserManager>,
    /// Connection admission and teardown.
    pub connection_manager: Arc<ConnectionManager>,
}

/// Default duration an eviction is remembered if not configured.
pub const DEFAULT_RECENT_EVICTION_REENTRY_TTL_MS: u64 = shared::defaults::DEFAULT_RECENT_EVICTION_REENTRY_TTL_MS;

/// Reentry cooldown from a resolved stream config, falling back to the default when
/// no `reverse_proxy.stream` block is configured.
fn reentry_ttl_for_stream(stream: Option<&tuliprox_core::model::StreamConfig>) -> std::time::Duration {
    stream.map_or(std::time::Duration::from_millis(DEFAULT_RECENT_EVICTION_REENTRY_TTL_MS), |stream| {
        stream.recent_eviction_reentry_ttl
    })
}

pub(crate) fn get_reentry_ttl(adm: &AdmissionCtx) -> std::time::Duration {
    let config = adm.app_config.config.load();
    reentry_ttl_for_stream(config.reverse_proxy.as_ref().and_then(|rp| rp.stream.as_ref()))
}

#[derive(Clone, Copy)]
pub enum EvictionReentryGuard<'a> {
    Session(&'a str),
    SocketPlayback { virtual_id: VirtualId },
}

/// The request-scoped inputs every admission path threads through unchanged.
///
/// These used to be ten positional parameters repeated across four functions,
/// three of them bare `bool`s in a row - a shape where transposing two arguments
/// still compiles. Naming them at the call site is the point.
#[derive(Clone, Copy)]
pub struct AdmissionRequest<'a> {
    pub username: &'a str,
    pub max_connections: u32,
    pub soft_connections: u16,
    pub client_ip: &'a str,
    pub request_addr: &'a std::net::SocketAddr,
    /// Whether an existing logical playback session may reopen while the user is
    /// already at limit. Intentionally independent from whether the session is
    /// socket-bound.
    pub use_session_admission: bool,
    pub session_token: Option<&'a str>,
    pub activate_unbound_session: bool,
    pub eviction_reentry_guard: EvictionReentryGuard<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackRequestClass {
    Prepare,
    Activate,
    FollowUp,
    Terminate,
}

#[derive(Clone, Copy)]
pub struct PlaybackRequestFacts<'a> {
    pub existing_session: Option<&'a UserSession>,
    pub prepare_only: bool,
    pub terminate: bool,
}

pub fn classify_playback_request(facts: PlaybackRequestFacts<'_>) -> PlaybackRequestClass {
    if facts.terminate {
        return PlaybackRequestClass::Terminate;
    }
    if facts.prepare_only {
        return PlaybackRequestClass::Prepare;
    }
    if let Some(session) = facts.existing_session {
        // FollowUp only for sessions that are actively counted.
        // PendingProvider has no counted lease yet - activation is still pending.
        // Prepared/Preserved/Expired sessions are not FollowUp.
        if session.lifecycle.is_counted() {
            return PlaybackRequestClass::FollowUp;
        }
    }
    PlaybackRequestClass::Activate
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_playback_request_admission(
    adm: &AdmissionCtx,
    user: &ProxyUserCredentials,
    fingerprint: &Fingerprint,
    user_session: Option<&UserSession>,
    session_token: &str,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
    prepare_only: bool,
    terminate: bool,
) -> (crate::ConnectionAdmission, Option<crate::GraceMode>, PlaybackRequestClass) {
    let request_class =
        classify_playback_request(PlaybackRequestFacts { existing_session: user_session, prepare_only, terminate });
    let limits_enabled =
        (user.max_connections > 0 || user.soft_connections > 0) && adm.app_config.config.load().user_access_control;

    // Handle explicit Terminate: run termination and return exhausted permission.
    // No admission strategies are evaluated — termination immediately expires the playback.
    if request_class == PlaybackRequestClass::Terminate {
        if let Some(session) = user_session {
            adm.active_users.terminate_session(&user.username, session.token.as_str()).await;
        }
        return (
            crate::ConnectionAdmission::exhausted(
                crate::AdmissionRejectionReason::UserConnectionsExhausted,
                user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            ),
            None,
            request_class,
        );
    }

    // Handle Prepare: no admission cost, just prepare state. Return Allowed without
    // running strategies or modifying counted state. Caller handles the actual activation.
    if request_class == PlaybackRequestClass::Prepare {
        return (
            crate::ConnectionAdmission::allowed(
                user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            ),
            None,
            request_class,
        );
    }

    if request_class == PlaybackRequestClass::FollowUp || !limits_enabled {
        return (
            crate::ConnectionAdmission::from_permission(
                user_session.map_or(UserConnectionPermission::Allowed, |session| session.permission),
                user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            ),
            None,
            request_class,
        );
    }

    let result = resolve_admission_with_strategies(
        adm,
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some(session_token),
            activate_unbound_session,
            eviction_reentry_guard,
        },
    )
    .await;

    // The ladder models this outcome fully - it is what is left after every
    // eviction strategy declines - and then returned it to the caller and
    // nobody else. `ActiveUser` covers connects and disconnects; a refusal is
    // neither, so the one outcome a user actually complains about was the one
    // nothing published.
    //
    // Only the strategy path emits. An explicit `Terminate` also resolves to
    // `Exhausted`, but that is a requested teardown, not a denial.
    //
    // Background retry of a recently evicted stream is quietly suppressed to avoid
    // playback ping-pong. Count it as a diagnostic, never as a connection denial.
    if result.admission.is_reentry_suppressed() {
        adm.active_users.record_reentry_suppressed();
    } else if result.admission.permission() == UserConnectionPermission::Exhausted {
        adm.active_users.events().send_event(EventMessage::ConnectionDenied(ConnectionDenied::new(
            Arc::from(user.username.as_str()),
            Arc::from(fingerprint.client_ip.as_str()),
            user.max_connections,
            user.soft_connections,
        )));
    }

    (result.admission, result.grace_mode, request_class)
}

async fn should_suppress_eviction_for_recent_request(
    adm: &AdmissionCtx,
    request: &AdmissionRequest<'_>,
    target: &crate::EvictionTarget,
) -> bool {
    match request.eviction_reentry_guard {
        EvictionReentryGuard::Session(session_token) => adm
            .active_users
            .recently_evicted_session_protected_addr(session_token)
            .await
            .is_some_and(|protected_addr| protected_addr == target.addr),
        EvictionReentryGuard::SocketPlayback { virtual_id } => {
            target.virtual_id != virtual_id
                && adm
                    .active_users
                    .recent_socket_reentry_protected_addr(request.username, request.client_ip, virtual_id)
                    .await
                    .is_some_and(|protected_addr| protected_addr == target.addr)
        }
    }
}

async fn get_admission_for_request(adm: &AdmissionCtx, request: &AdmissionRequest<'_>) -> crate::ConnectionAdmission {
    let AdmissionRequest { username, max_connections, soft_connections, .. } = *request;
    if request.use_session_admission {
        let session_token = request.session_token.unwrap_or_default();
        if request.activate_unbound_session {
            adm.active_users
                .connection_admission_for_session_activation(username, max_connections, soft_connections, session_token)
                .await
        } else {
            adm.active_users
                .connection_admission_for_session(username, max_connections, soft_connections, session_token)
                .await
        }
    } else {
        adm.active_users.connection_admission(username, max_connections, soft_connections).await
    }
}

pub fn connection_priority_for_kind(user: &ProxyUserCredentials, kind: crate::ConnectionKind) -> i8 {
    match kind {
        crate::ConnectionKind::Normal => user.priority,
        crate::ConnectionKind::Soft => user.soft_priority,
    }
}

/// Structured result of evaluating admission strategies.
#[derive(Debug)]
pub struct AdmissionStrategyResolution {
    pub admission: crate::ConnectionAdmission,
    pub grace_mode: Option<crate::GraceMode>,
    /// Present only when the request was admitted via a user-grace strategy.
    pub grace_context: Option<GraceResolutionContext>,
}

pub fn get_effective_admission_strategies(adm: &AdmissionCtx) -> Arc<[AdmissionStrategy]> {
    let config = adm.app_config.config.load();
    let Some(stream_config) = config.reverse_proxy.as_ref().and_then(|rp| rp.stream.as_ref()) else {
        return Arc::from([]);
    };

    // `Some(vec![])` is deliberately not the same as `None`. An explicitly empty
    // list means the operator turned admission strategies off, and must not fall
    // back to the legacy `grace_period_millis` translation below; only an absent
    // list does.
    match stream_config.admission_strategies.as_ref() {
        Some(strategies) => Arc::from(strategies.as_slice()),
        None if stream_config.grace_period_millis > 0 => Arc::from([if stream_config.grace_period_hold_stream {
            AdmissionStrategy::GraceHoldStream
        } else {
            AdmissionStrategy::GraceInstantStream
        }]),
        None => Arc::from([]),
    }
}

enum StrategyLoopResult {
    Admitted(AdmissionStrategyResolution),
    Rejected(crate::AdmissionRejectionReason),
}

/// Shared strategy-evaluation loop used by both the initial admission path
/// (`resolve_admission_with_strategies`) and the remaining-strategies path
/// (`evaluate_remaining_strategies_after_grace`).
async fn evaluate_admission_strategy_loop<F>(
    adm: &AdmissionCtx,
    request: &AdmissionRequest<'_>,
    strategies: &[shared::model::AdmissionStrategy],
    base_idx: usize,
    admission: crate::ConnectionAdmission,
    build_grace_ctx: F,
) -> StrategyLoopResult
where
    F: Fn(usize) -> GraceResolutionContext,
{
    use crate::{evaluate_strategy, AdmissionDecision, StrategyContext};
    use shared::model::UserConnectionPermission;
    let AdmissionRequest { username, client_ip, request_addr, .. } = *request;
    let mut candidates = adm.active_users.get_eviction_candidates(username, client_ip).await;
    let ctx = StrategyContext { username, client_ip };
    // Set once an eviction has been carried out without reducing the user's
    // counted connections. Evicting is destructive and cannot be undone, so a
    // kick that frees nothing is taken as evidence that the next one would not
    // help either, and later eviction strategies are skipped.
    let mut evictions_ineffective = false;
    // Set when a candidate was skipped because the reentry guard protects it.
    let mut suppressed_candidate = false;
    // Set only after a *non-protected* candidate was actually evicted. A genuine
    // eviction means the final rejection is real resource exhaustion, not a
    // reentry suppression, even if an earlier candidate was protected.
    let mut performed_legitimate_eviction = false;

    // `enumerate` rather than a manual counter: the suppressed-eviction arm below
    // uses `continue`, which used to skip a trailing `idx += 1` and hand every
    // later strategy an index one too low. A grace admitted after a suppressed
    // eviction then recorded a `strategy_index` pointing at an earlier strategy,
    // so `evaluate_remaining_strategies_after_grace` replayed the grace itself
    // instead of resuming past it.
    for (idx, strategy) in strategies.iter().enumerate() {
        match evaluate_strategy(*strategy, &ctx, &candidates) {
            AdmissionDecision::NoMatch => {}
            AdmissionDecision::Grace(mode) => {
                if adm.active_users.grant_grace(username).await {
                    // Return a FRESH admission with GracePeriod permission (not the admission
                    // parameter, which may have Exhausted permission). The kind is preserved from
                    // the original admission.
                    return StrategyLoopResult::Admitted(AdmissionStrategyResolution {
                        admission: crate::ConnectionAdmission::grace_period(admission.kind()),
                        grace_mode: Some(mode),
                        grace_context: Some(build_grace_ctx(base_idx + idx)),
                    });
                }
                debug!("Grace grant rejected for user {username}, continuing with later strategies");
            }
            AdmissionDecision::Evict(target) => {
                if evictions_ineffective {
                    debug!(
                        "Skipping eviction strategy {strategy:?} for user {username}: an earlier eviction freed no slot"
                    );
                    continue;
                }
                if should_suppress_eviction_for_recent_request(adm, request, &target).await {
                    debug!(
                        "Skipping eviction strategy {strategy:?} for recently evicted request of user {username} targeting {}",
                        target.addr
                    );
                    suppressed_candidate = true;
                    // Exclude this specific stream candidate so subsequent strategies can evaluate other candidates
                    candidates.retain(|c| c.uid != target.uid);
                    continue;
                }
                debug!("Evicting connection {} for user {username}", target.addr);
                let connections_before = adm.active_users.user_connections(username).await;
                let ttl = get_reentry_ttl(adm);
                adm.active_users.mark_recent_eviction_guard_for_addr(&target.addr, *request_addr, ttl).await;
                adm.connection_manager.release_connection_as_kicked(&target.addr).await;
                performed_legitimate_eviction = true;
                let retry_admission = get_admission_for_request(adm, request).await;
                if retry_admission.permission() == UserConnectionPermission::Allowed {
                    return StrategyLoopResult::Admitted(AdmissionStrategyResolution {
                        admission: retry_admission,
                        grace_mode: None,
                        grace_context: None,
                    });
                }
                if adm.active_users.user_connections(username).await >= connections_before {
                    evictions_ineffective = true;
                    debug!(
                        "Eviction of {} freed no counted connection for user {username}, skipping later eviction strategies",
                        target.addr
                    );
                } else {
                    debug!(
                        "Admission still denied after eviction for user {username}, continuing with later strategies"
                    );
                }
                candidates = adm.active_users.get_eviction_candidates(username, client_ip).await;
            }
        }
    }

    StrategyLoopResult::Rejected(rejection_after_strategy_loop(suppressed_candidate, performed_legitimate_eviction))
}

/// Classifies the rejection after the strategy loop.
///
/// `RecentEvictionReentry` is only correct when the request failed *solely* because
/// every candidate was reentry-protected and no legitimate eviction was performed.
/// Once a non-protected candidate was evicted and admission still fails, the request
/// hit genuine resource exhaustion and must be reported as such.
fn rejection_after_strategy_loop(
    suppressed_candidate: bool,
    performed_legitimate_eviction: bool,
) -> crate::AdmissionRejectionReason {
    if suppressed_candidate && !performed_legitimate_eviction {
        crate::AdmissionRejectionReason::RecentEvictionReentry
    } else {
        crate::AdmissionRejectionReason::UserConnectionsExhausted
    }
}

pub async fn resolve_admission_with_strategies(
    adm: &AdmissionCtx,
    request: AdmissionRequest<'_>,
) -> AdmissionStrategyResolution {
    use shared::model::UserConnectionPermission;

    let username = request.username;
    let admission = get_admission_for_request(adm, &request).await;

    if admission.permission() != UserConnectionPermission::Exhausted {
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let strategies = get_effective_admission_strategies(adm);
    if strategies.is_empty() {
        debug!("No admission strategies configured, denying request for user {username}");
        return AdmissionStrategyResolution {
            admission: crate::ConnectionAdmission::exhausted(
                crate::AdmissionRejectionReason::UserConnectionsExhausted,
                admission.kind(),
            ),
            grace_mode: None,
            grace_context: None,
        };
    }

    let _admission_guard = adm.active_users.acquire_user_admission(username).await;

    // Re-read admission now that the gate is held. The first read above happened
    // before we queued on the gate, so a request ahead of us may have released
    // the very slot we are about to evict somebody for. Walking the strategies on
    // the stale snapshot kicks a live connection to free a slot that is already
    // free.
    let admission = get_admission_for_request(adm, &request).await;

    if admission.permission() != UserConnectionPermission::Exhausted {
        debug!("Admission became available while waiting on the admission gate for user {username}");
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: admission.kind(),
    };

    match evaluate_admission_strategy_loop(adm, &request, &strategies, 0, admission, build_grace_ctx).await {
        StrategyLoopResult::Admitted(resolution) => resolution,
        StrategyLoopResult::Rejected(reason) => {
            debug!("No admission strategy could admit user {username}: {reason:?}");
            AdmissionStrategyResolution {
                admission: crate::ConnectionAdmission::exhausted(reason, admission.kind()),
                grace_mode: None,
                grace_context: None,
            }
        }
    }
}

/// Evaluates only the strategies that come AFTER the already-used grace strategy.
/// This is called when a user-grace has failed and the system needs to determine
/// whether a remaining eviction strategy can free a slot.
///
/// Rules:
/// - Only `grace_context.strategies[(strategy_index + 1)..]` are evaluated
/// - `NoMatch` -> continue to next strategy
/// - `Evict` -> kick target, retry admission
/// - `Grace` -> granted again if the user is eligible; the resolution then carries a
///   `GraceResolutionContext` whose `strategy_index` points at this later strategy, so a
///   second failure resumes past it rather than replaying it
/// - Every strategy exhausted, or an empty remaining slice -> final exhausted
pub async fn evaluate_remaining_strategies_after_grace(
    adm: &AdmissionCtx,
    request: AdmissionRequest<'_>,
    grace_context: &GraceResolutionContext,
    original_kind: Option<crate::ConnectionKind>,
) -> AdmissionStrategyResolution {
    let username = request.username;
    let remaining = grace_context.strategy_index + 1;
    let strategies = &grace_context.strategies;
    if remaining >= strategies.len() {
        debug!("No remaining strategies after grace for user {username}");
        return AdmissionStrategyResolution {
            admission: crate::ConnectionAdmission::exhausted(
                crate::AdmissionRejectionReason::UserConnectionsExhausted,
                original_kind,
            ),
            grace_mode: None,
            grace_context: None,
        };
    }

    // `admission` only carries `kind` into the loop: the Grace arm copies it onto the
    // returned `ConnectionAdmission`, and `build_grace_ctx` copies it onto the
    // `GraceResolutionContext`. Seeding it with `original_kind` keeps every exit from this
    // function reporting the kind the original admission decided.
    let admission =
        crate::ConnectionAdmission::exhausted(crate::AdmissionRejectionReason::UserConnectionsExhausted, original_kind);
    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: original_kind,
    };

    let _admission_guard = adm.active_users.acquire_user_admission(username).await;

    match evaluate_admission_strategy_loop(
        adm,
        &request,
        &strategies[remaining..],
        remaining,
        admission,
        build_grace_ctx,
    )
    .await
    {
        StrategyLoopResult::Admitted(resolution) => resolution,
        StrategyLoopResult::Rejected(reason) => {
            debug!("No remaining strategy could admit user {username}: {reason:?}");
            AdmissionStrategyResolution {
                admission: crate::ConnectionAdmission::exhausted(reason, original_kind),
                grace_mode: None,
                grace_context: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdmissionRejectionReason, ConnectionAdmission};
    use shared::model::UserConnectionPermission;

    #[test]
    fn reentry_ttl_uses_configured_value_and_falls_back_to_default() {
        assert_eq!(
            reentry_ttl_for_stream(None),
            std::time::Duration::from_millis(DEFAULT_RECENT_EVICTION_REENTRY_TTL_MS)
        );
        let stream = tuliprox_core::model::StreamConfig {
            recent_eviction_reentry_ttl: std::time::Duration::from_millis(2_500),
            ..tuliprox_core::model::StreamConfig::default()
        };
        assert_eq!(reentry_ttl_for_stream(Some(&stream)), std::time::Duration::from_millis(2_500));
    }

    #[test]
    fn rejection_classification_distinguishes_reentry_from_exhaustion() {
        // Pure reentry: a protected candidate was skipped and nothing was evicted.
        assert_eq!(rejection_after_strategy_loop(true, false), AdmissionRejectionReason::RecentEvictionReentry);
        // Mixed case: a protected candidate was skipped, but a legitimate eviction was
        // performed and admission still failed. That residual failure is genuine
        // exhaustion, not a quiet suppression.
        assert_eq!(rejection_after_strategy_loop(true, true), AdmissionRejectionReason::UserConnectionsExhausted);
        assert_eq!(rejection_after_strategy_loop(false, true), AdmissionRejectionReason::UserConnectionsExhausted);
        assert_eq!(rejection_after_strategy_loop(false, false), AdmissionRejectionReason::UserConnectionsExhausted);
    }

    #[test]
    fn exhausted_admission_always_carries_a_reason() {
        let exhausted = ConnectionAdmission::from_permission(UserConnectionPermission::Exhausted, None);
        assert_eq!(exhausted.permission(), UserConnectionPermission::Exhausted);
        assert_eq!(exhausted.rejection_reason(), Some(AdmissionRejectionReason::UserConnectionsExhausted));
        assert!(!exhausted.is_reentry_suppressed());

        assert_eq!(
            ConnectionAdmission::from_permission(UserConnectionPermission::Allowed, None).rejection_reason(),
            None
        );
        assert_eq!(
            ConnectionAdmission::from_permission(UserConnectionPermission::GracePeriod, None).permission(),
            UserConnectionPermission::GracePeriod
        );
    }
}
