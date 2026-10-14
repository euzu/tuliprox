#![allow(dead_code)]

use crate::{api::model::AppState, model::ConfigInput};
use shared::model::{ChannelGroupPolicy, ChannelGroupSelectionConfig, StreamChannel};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_repository::channel_group_repository::{ChannelGroupRecord, GroupMember};
pub(crate) use tuliprox_session::channel_group::{member_key, CircuitAdmission, CircuitView, SelectionMode};
use tuliprox_session::{
    active_provider_manager::{CapacityTier, ConnectionKind, ManagedProviderHandle, PlaybackLeaseRef},
    channel_group::{rank_members, HalfOpenGuard, RankedCandidate},
};

pub(crate) enum PlaybackSource<'a> {
    Single { input: &'a Arc<ConfigInput>, stream_url: &'a str },
    Group { group: Arc<ChannelGroupRecord>, mode: SelectionMode, prefer: Option<usize> },
}

pub(crate) struct ResolvedGroupPlayback {
    pub acquired: AcquiredMember,
    pub member: GroupMember,
    pub input: Arc<ConfigInput>,
    pub stream_url: Arc<str>,
    pub stream_channel: StreamChannel,
}

pub(crate) fn group_shared_key(target_id: u16, group_vid: u32) -> Arc<str> {
    Arc::from(format!("group:{target_id}:{group_vid}"))
}

pub(crate) struct GroupSelectionRequest<'a> {
    pub target_name: &'a Arc<str>,
    pub group: &'a ChannelGroupRecord,
    pub policy: ChannelGroupPolicy,
    pub mode: SelectionMode,
    pub selection: ChannelGroupSelectionConfig,
    pub addr: &'a SocketAddr,
    pub username: &'a str,
    pub priority: i8,
    pub kind: ConnectionKind,
    pub lease: Option<PlaybackLeaseRef<'a>>,
    pub allow_provider_grace: bool,
    pub exclude: &'a [usize],
    pub demoted_inputs: &'a [Arc<str>],
    pub prefer: Option<usize>,
}

pub(crate) struct AcquiredMember {
    pub index: usize,
    pub handle: ManagedProviderHandle,
    pub tier: CapacityTier,
    pub half_open: Option<HalfOpenGuard>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GroupSelectionFailure {
    Exhausted,
    NoCandidate,
}

pub(crate) async fn acquire_group_member(
    app_state: &Arc<AppState>,
    req: GroupSelectionRequest<'_>,
) -> Result<AcquiredMember, GroupSelectionFailure> {
    acquire_group_member_inner(app_state, req, None::<fn(&Arc<AppState>)>).await
}

#[cfg(test)]
pub(crate) async fn acquire_group_member_with_race<F: FnOnce(&Arc<AppState>)>(
    app_state: &Arc<AppState>,
    req: GroupSelectionRequest<'_>,
    after_probe: F,
) -> Result<AcquiredMember, GroupSelectionFailure> {
    acquire_group_member_inner(app_state, req, Some(after_probe)).await
}

async fn acquire_group_member_inner<F: FnOnce(&Arc<AppState>)>(
    app_state: &Arc<AppState>,
    req: GroupSelectionRequest<'_>,
    after_probe: Option<F>,
) -> Result<AcquiredMember, GroupSelectionFailure> {
    let budget_secs = req.selection.startup_budget_secs;
    if budget_secs > 0 {
        match tokio::time::timeout(Duration::from_secs(budget_secs), select_member_loop(app_state, req, after_probe))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(GroupSelectionFailure::Exhausted),
        }
    } else {
        select_member_loop(app_state, req, after_probe).await
    }
}

fn check_member_circuit(
    app_state: &Arc<AppState>,
    req: &GroupSelectionRequest<'_>,
    cand_idx: usize,
    first_remaining_idx: usize,
    all_circuits_open: bool,
) -> (bool, Option<HalfOpenGuard>) {
    let key = member_key(&req.group.members[cand_idx]);
    let admission = app_state.member_health.admit(&key, std::time::Instant::now());
    match admission {
        CircuitAdmission::Closed => (true, None),
        CircuitAdmission::HalfOpen(guard) => (true, Some(guard)),
        CircuitAdmission::Open => (all_circuits_open && cand_idx == first_remaining_idx, None),
    }
}

fn select_tier_candidates<F>(
    app_state: &Arc<AppState>,
    req: &GroupSelectionRequest<'_>,
    remaining: &mut Vec<RankedCandidate>,
    probed_tiers: &mut Vec<(usize, CapacityTier)>,
    all_circuits_open: bool,
    filter_tier: F,
    probe_tier: &dyn Fn(usize) -> CapacityTier,
) -> Option<AcquiredMember>
where
    F: Fn(CapacityTier) -> Option<bool>,
{
    loop {
        let mut tried_candidate = false;
        let candidate_indices: Vec<(usize, bool)> = probed_tiers
            .iter()
            .filter_map(|(idx, tier)| filter_tier(*tier).map(|allow_grace| (*idx, allow_grace)))
            .collect();

        for (cand_idx, allow_grace) in candidate_indices {
            let Some((_, tier)) = probed_tiers.iter().find(|(idx, _)| *idx == cand_idx).copied() else {
                continue;
            };

            let (can_try, half_open_guard) =
                check_member_circuit(app_state, req, cand_idx, remaining[0].index, all_circuits_open);
            if !can_try {
                continue;
            }

            tried_candidate = true;
            let handle_opt = app_state.active_provider.try_acquire_connection_no_preempt(
                &req.group.members[cand_idx].input_name,
                allow_grace,
                req.addr,
                req.priority,
                req.kind,
                req.lease,
            );

            if let Some(handle) = handle_opt {
                return Some(AcquiredMember {
                    index: cand_idx,
                    handle: ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), handle),
                    tier,
                    half_open: half_open_guard,
                });
            }

            remaining.retain(|c| c.index != cand_idx);
            *probed_tiers = remaining.iter().map(|c| (c.index, probe_tier(c.index))).collect();
            break;
        }

        if !tried_candidate {
            break;
        }
    }
    None
}

async fn try_acquire_preempt(
    app_state: &Arc<AppState>,
    req: &GroupSelectionRequest<'_>,
    remaining: &[RankedCandidate],
    all_circuits_open: bool,
) -> Option<AcquiredMember> {
    for c in remaining {
        let (can_try, half_open_guard) =
            check_member_circuit(app_state, req, c.index, remaining[0].index, all_circuits_open);
        if !can_try {
            continue;
        }

        let handle_opt = app_state
            .active_provider
            .acquire_connection_with_lease_for_session_await(
                &req.group.members[c.index].input_name,
                req.addr,
                req.allow_provider_grace,
                req.priority,
                req.kind,
                req.lease,
            )
            .await;

        if let Some(handle) = handle_opt {
            return Some(AcquiredMember {
                index: c.index,
                handle: ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), handle),
                tier: CapacityTier::Preempt,
                half_open: half_open_guard,
            });
        }

        break;
    }
    None
}

async fn select_member_loop<F: FnOnce(&Arc<AppState>)>(
    app_state: &Arc<AppState>,
    req: GroupSelectionRequest<'_>,
    after_probe: Option<F>,
) -> Result<AcquiredMember, GroupSelectionFailure> {
    let now = std::time::Instant::now();
    let mut ranked =
        rank_members(req.target_name, &req.group.members, req.policy, &app_state.member_health, &req.mode, now);

    ranked.retain(|c| !req.exclude.contains(&c.index));
    if ranked.is_empty() {
        return Err(GroupSelectionFailure::NoCandidate);
    }

    ranked.sort_by_key(|c| req.demoted_inputs.contains(&req.group.members[c.index].input_name));

    if let Some(prefer_idx) = req.prefer {
        if let Some(pos) = ranked.iter().position(|c| c.index == prefer_idx) {
            let cand = ranked.remove(pos);
            ranked.insert(0, cand);
        }
    }

    let all_circuits_open = ranked.iter().all(|c| {
        let key = member_key(&req.group.members[c.index]);
        matches!(app_state.member_health.circuit_view(&key, now), CircuitView::Open { remaining } if remaining > Duration::ZERO)
    });

    let own_releasing = app_state.active_users.pending_release_allocations(req.username).await;

    let probe_tier = |index: usize| {
        app_state.active_provider.capacity_probe(
            &req.group.members[index].input_name,
            req.priority,
            req.kind,
            req.lease.map(|l| l.provider_owner()),
            &own_releasing,
        )
    };

    let mut remaining = ranked;
    let mut probed_tiers: Vec<(usize, CapacityTier)> =
        remaining.iter().map(|c| (c.index, probe_tier(c.index))).collect();

    if let Some(hook) = after_probe {
        hook(app_state);
    }

    // Tier 1: Free ∪ OwnRelease
    let tier1_match = |tier| match tier {
        CapacityTier::OwnRelease => Some(true),
        CapacityTier::Free => Some(false),
        _ => None,
    };
    if let Some(acquired) = select_tier_candidates(
        app_state,
        &req,
        &mut remaining,
        &mut probed_tiers,
        all_circuits_open,
        tier1_match,
        &probe_tier,
    ) {
        return Ok(acquired);
    }

    // Tier 2: Grace (only if allow_provider_grace)
    if req.allow_provider_grace {
        let tier2_match = |tier| match tier {
            CapacityTier::Grace => Some(true),
            CapacityTier::Free => Some(false),
            _ => None,
        };
        if let Some(acquired) = select_tier_candidates(
            app_state,
            &req,
            &mut remaining,
            &mut probed_tiers,
            all_circuits_open,
            tier2_match,
            &probe_tier,
        ) {
            return Ok(acquired);
        }
    }

    // Tier 3: Preempt on the best-ranked candidate
    if let Some(acquired) = try_acquire_preempt(app_state, &req, &remaining, all_circuits_open).await {
        return Ok(acquired);
    }

    Err(GroupSelectionFailure::Exhausted)
}

pub(crate) async fn resolve_hls_group_entry(
    app_state: &Arc<AppState>,
    target: &tuliprox_core::model::ConfigTarget,
    group: Arc<ChannelGroupRecord>,
    req: GroupSelectionRequest<'_>,
) -> Result<ResolvedGroupPlayback, GroupSelectionFailure> {
    let max_attempts = u32::from(req.selection.max_start_attempts.max(1));
    let mut attempts = 0;
    let start_time = std::time::Instant::now();
    let budget = Duration::from_secs(req.selection.startup_budget_secs);
    let mut exclude: Vec<usize> = req.exclude.to_vec();
    let demoted_inputs: Vec<Arc<str>> = req.demoted_inputs.to_vec();

    while attempts < max_attempts && (req.selection.startup_budget_secs == 0 || start_time.elapsed() < budget) {
        attempts += 1;
        let attempt_req = GroupSelectionRequest {
            target_name: req.target_name,
            group: req.group,
            policy: req.policy,
            mode: req.mode.clone(),
            selection: req.selection,
            addr: req.addr,
            username: req.username,
            priority: req.priority,
            kind: req.kind,
            lease: req.lease,
            allow_provider_grace: req.allow_provider_grace,
            exclude: &exclude,
            demoted_inputs: &demoted_inputs,
            prefer: if attempts == 1 { req.prefer } else { None },
        };

        let acquired = acquire_group_member(app_state, attempt_req).await?;
        let member = group.members[acquired.index].clone();
        let Some(member_input) = app_state.app_config.get_input_by_name(&member.input_name) else {
            let key = member_key(&member);
            if let Some(guard) = acquired.half_open {
                app_state.member_health.record_classified_failure_with(
                    guard,
                    tuliprox_session::channel_group::FailureClass::Stream,
                    std::time::Instant::now(),
                );
            } else {
                app_state.member_health.record_classified_failure(
                    &key,
                    tuliprox_session::channel_group::FailureClass::Stream,
                    std::time::Instant::now(),
                );
            }
            drop(acquired.handle);
            exclude.push(acquired.index);
            continue;
        };

        let mut stream_channel = if target.has_output(shared::model::TargetType::Xtream) {
            tuliprox_repository::target_stream_item::member_to_xtream_item(&group, acquired.index, target)
                .to_stream_channel(target.id)
        } else {
            tuliprox_repository::target_stream_item::member_to_m3u_item(&group, acquired.index, target)
                .to_stream_channel(target.id)
        };
        stream_channel.group_virtual_id = Some(group.group_vid.get());

        let stream_url = Arc::clone(&member.url);
        return Ok(ResolvedGroupPlayback { acquired, member, input: member_input, stream_url, stream_channel });
    }

    Err(GroupSelectionFailure::Exhausted)
}
