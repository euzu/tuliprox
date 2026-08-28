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
use shared::model::{AdmissionStrategy, PlaylistItemType, UserConnectionPermission, VirtualId};
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

/// How long an eviction is remembered so the evicted client's immediate retry
/// does not evict its own replacement.
pub const RECENT_EVICTION_REENTRY_TTL_SECS: u64 = 3;

#[derive(Clone, Copy)]
pub enum EvictionReentryGuard<'a> {
    Session(&'a str),
    SocketPlayback { virtual_id: VirtualId },
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
    pub item_type: PlaylistItemType,
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
    let _ = facts.item_type;
    PlaybackRequestClass::Activate
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_playback_request_admission(
    adm: &AdmissionCtx,
    user: &ProxyUserCredentials,
    fingerprint: &Fingerprint,
    item_type: PlaylistItemType,
    user_session: Option<&UserSession>,
    session_token: &str,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
    prepare_only: bool,
    terminate: bool,
) -> (crate::ConnectionAdmission, Option<crate::GraceMode>, PlaybackRequestClass) {
    let request_class = classify_playback_request(PlaybackRequestFacts {
        item_type,
        existing_session: user_session,
        prepare_only,
        terminate,
    });
    let limits_enabled =
        (user.max_connections > 0 || user.soft_connections > 0) && adm.app_config.config.load().user_access_control;

    // Handle explicit Terminate: run termination and return exhausted permission.
    // No admission strategies are evaluated — termination immediately expires the playback.
    if request_class == PlaybackRequestClass::Terminate {
        if let Some(session) = user_session {
            adm.active_users.terminate_session(&user.username, session.token.as_str()).await;
        }
        return (
            crate::ConnectionAdmission {
                permission: UserConnectionPermission::Exhausted,
                kind: user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    // Handle Prepare: no admission cost, just prepare state. Return Allowed without
    // running strategies or modifying counted state. Caller handles the actual activation.
    if request_class == PlaybackRequestClass::Prepare {
        return (
            crate::ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    if request_class == PlaybackRequestClass::FollowUp || !limits_enabled {
        return (
            crate::ConnectionAdmission {
                permission: user_session.map_or(UserConnectionPermission::Allowed, |session| session.permission),
                kind: user_session.and_then(|session| session.connection_kind).or(Some(crate::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    let result = resolve_admission_with_strategies(
        adm,
        &user.username,
        user.max_connections,
        user.soft_connections,
        &fingerprint.client_ip,
        &fingerprint.addr,
        true,
        Some(session_token),
        activate_unbound_session,
        eviction_reentry_guard,
    )
    .await;

    (result.admission, result.grace_mode, request_class)
}

async fn should_suppress_eviction_for_recent_request(
    adm: &AdmissionCtx,
    username: &str,
    client_ip: &str,
    guard: EvictionReentryGuard<'_>,
    target_addr: &std::net::SocketAddr,
) -> bool {
    match guard {
        EvictionReentryGuard::Session(session_token) => adm
            .active_users
            .recently_evicted_session_protected_addr(session_token)
            .await
            .is_some_and(|protected_addr| protected_addr == *target_addr),
        EvictionReentryGuard::SocketPlayback { virtual_id } => adm
            .active_users
            .recent_socket_reentry_protected_addr(username, client_ip, virtual_id)
            .await
            .is_some_and(|protected_addr| protected_addr == *target_addr),
    }
}

async fn get_admission_for_request(
    adm: &AdmissionCtx,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    is_session_request: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
) -> crate::ConnectionAdmission {
    if is_session_request {
        if activate_unbound_session {
            adm.active_users
                .connection_admission_for_session_activation(
                    username,
                    max_connections,
                    soft_connections,
                    session_token.unwrap_or_default(),
                )
                .await
        } else {
            adm.active_users
                .connection_admission_for_session(
                    username,
                    max_connections,
                    soft_connections,
                    session_token.unwrap_or_default(),
                )
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

pub fn get_effective_admission_strategies(adm: &AdmissionCtx) -> Vec<AdmissionStrategy> {
    let config = adm.app_config.config.load();
    let stream_config = config.reverse_proxy.as_ref().and_then(|rp| rp.stream.as_ref());
    match stream_config {
        Some(sc) if sc.admission_strategies.is_some() => sc.admission_strategies.clone().unwrap_or_default(),
        Some(sc) if sc.grace_period_millis > 0 => {
            vec![if sc.grace_period_hold_stream {
                AdmissionStrategy::GraceHoldStream
            } else {
                AdmissionStrategy::GraceInstantStream
            }]
        }
        _ => Vec::new(),
    }
}

/// Shared strategy-evaluation loop used by both the initial admission path
/// (`resolve_admission_with_strategies`) and the remaining-strategies path
/// (`evaluate_remaining_strategies_after_grace`).
///
/// Returns `Some(resolution)` when a Grace or a successful Eviction+Retry is found.
/// Returns `None` when every strategy in `strategies` returns `NoMatch` — the caller
/// is then responsible for constructing the final exhausted result with the correct
/// `kind` (preserved from the original admission).
#[allow(clippy::too_many_arguments)]
async fn evaluate_admission_strategy_loop<'a, F>(
    adm: &AdmissionCtx,
    username: &'a str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &'a str,
    request_addr: &'a std::net::SocketAddr,
    use_session_admission: bool,
    session_token: Option<&'a str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'a>,
    strategies: &'a [shared::model::AdmissionStrategy],
    base_idx: usize,
    admission: crate::ConnectionAdmission,
    build_grace_ctx: F,
) -> Option<AdmissionStrategyResolution>
where
    F: Fn(usize) -> GraceResolutionContext,
{
    use crate::{evaluate_strategy, AdmissionDecision, StrategyContext};
    use shared::model::UserConnectionPermission;
    let mut candidates = adm.active_users.get_eviction_candidates(username, client_ip).await;
    let ctx = StrategyContext { username, client_ip };
    // Set once an eviction has been carried out without reducing the user's
    // counted connections. Evicting is destructive and cannot be undone, so a
    // kick that frees nothing is taken as evidence that the next one would not
    // help either, and later eviction strategies are skipped.
    let mut evictions_ineffective = false;

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
                    return Some(AdmissionStrategyResolution {
                        admission: crate::ConnectionAdmission {
                            permission: UserConnectionPermission::GracePeriod,
                            kind: admission.kind,
                        },
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
                if should_suppress_eviction_for_recent_request(
                    adm,
                    username,
                    client_ip,
                    eviction_reentry_guard,
                    &target.addr,
                )
                .await
                {
                    debug!(
                        "Skipping eviction strategy {strategy:?} for recently evicted request of user {username} targeting {}",
                        target.addr
                    );
                    continue;
                }
                debug!("Evicting connection {} for user {username}", target.addr);
                let connections_before = adm.active_users.user_connections(username).await;
                adm.active_users
                    .mark_recent_eviction_guard_for_addr(&target.addr, *request_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
                    .await;
                adm.connection_manager.release_connection_as_kicked(&target.addr).await;
                let retry_admission = get_admission_for_request(
                    adm,
                    username,
                    max_connections,
                    soft_connections,
                    use_session_admission,
                    session_token,
                    activate_unbound_session,
                )
                .await;
                if retry_admission.permission == UserConnectionPermission::Allowed {
                    return Some(AdmissionStrategyResolution {
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

    // All strategies returned NoMatch — caller constructs the final exhausted result.
    None
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_admission_with_strategies(
    adm: &AdmissionCtx,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &str,
    request_addr: &std::net::SocketAddr,
    // This controls whether an existing logical playback session may reopen while the user is already at limit.
    // It is intentionally independent from whether the session is socket-bound.
    use_session_admission: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
) -> AdmissionStrategyResolution {
    use shared::model::UserConnectionPermission;

    let admission = get_admission_for_request(
        adm,
        username,
        max_connections,
        soft_connections,
        use_session_admission,
        session_token,
        activate_unbound_session,
    )
    .await;

    if admission.permission != UserConnectionPermission::Exhausted {
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let strategies = get_effective_admission_strategies(adm);
    if strategies.is_empty() {
        debug!("No admission strategies configured, denying request for user {username}");
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let _admission_guard = adm.active_users.acquire_user_admission(username).await;

    // Re-read admission now that the gate is held. The first read above happened
    // before we queued on the gate, so a request ahead of us may have released
    // the very slot we are about to evict somebody for. Walking the strategies on
    // the stale snapshot kicks a live connection to free a slot that is already
    // free.
    let admission = get_admission_for_request(
        adm,
        username,
        max_connections,
        soft_connections,
        use_session_admission,
        session_token,
        activate_unbound_session,
    )
    .await;

    if admission.permission != UserConnectionPermission::Exhausted {
        debug!("Admission became available while waiting on the admission gate for user {username}");
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: admission.kind,
    };

    if let Some(resolution) = evaluate_admission_strategy_loop(
        adm,
        username,
        max_connections,
        soft_connections,
        client_ip,
        request_addr,
        use_session_admission,
        session_token,
        activate_unbound_session,
        eviction_reentry_guard,
        &strategies,
        0,
        admission,
        build_grace_ctx,
    )
    .await
    {
        return resolution;
    }

    debug!("No admission strategy could admit user {username}");
    AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None }
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn evaluate_remaining_strategies_after_grace(
    adm: &AdmissionCtx,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &str,
    request_addr: &std::net::SocketAddr,
    use_session_admission: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
    grace_context: &GraceResolutionContext,
    original_kind: Option<crate::ConnectionKind>,
) -> AdmissionStrategyResolution {
    use shared::model::UserConnectionPermission;

    let remaining = grace_context.strategy_index + 1;
    let strategies = &grace_context.strategies;
    if remaining >= strategies.len() {
        debug!("No remaining strategies after grace for user {username}");
        return AdmissionStrategyResolution {
            admission: crate::ConnectionAdmission {
                permission: UserConnectionPermission::Exhausted,
                kind: original_kind,
            },
            grace_mode: None,
            grace_context: None,
        };
    }

    // `admission` only carries `kind` into the loop: the Grace arm copies it onto the
    // returned `ConnectionAdmission`, and `build_grace_ctx` copies it onto the
    // `GraceResolutionContext`. Seeding it with `original_kind` keeps every exit from this
    // function reporting the kind the original admission decided.
    let admission = crate::ConnectionAdmission { permission: UserConnectionPermission::Exhausted, kind: original_kind };
    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: original_kind,
    };

    let _admission_guard = adm.active_users.acquire_user_admission(username).await;

    if let Some(resolution) = evaluate_admission_strategy_loop(
        adm,
        username,
        max_connections,
        soft_connections,
        client_ip,
        request_addr,
        use_session_admission,
        session_token,
        activate_unbound_session,
        eviction_reentry_guard,
        &strategies[remaining..],
        remaining,
        admission,
        build_grace_ctx,
    )
    .await
    {
        return resolution;
    }

    debug!("No remaining strategy could admit user {username}");
    AdmissionStrategyResolution {
        admission: crate::ConnectionAdmission { permission: UserConnectionPermission::Exhausted, kind: original_kind },
        grace_mode: None,
        grace_context: None,
    }
}
