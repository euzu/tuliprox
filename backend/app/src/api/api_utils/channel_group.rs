use super::{create_stream_response_details, resolve_initial_stalker_playback_url};
use crate::{
    api::model::{AppState, BoxedProviderStream, ProviderStreamCustomReason, StreamDetails, StreamError},
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
};
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::StreamExt;
use shared::model::{
    ChannelGroupPlayback, ChannelGroupPolicy, ChannelGroupStartConfig, PlaylistItemType, StreamChannel,
    UserConnectionPermission, VirtualId,
};
use std::{collections::VecDeque, future::Future, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tuliprox_repository::channel_group_repository::{ChannelGroupRecord, GroupMember};
pub(crate) use tuliprox_session::channel_group::{member_key, CircuitAdmission, CircuitView, SelectionMode};
use tuliprox_session::{
    active_provider_manager::{CapacityTier, ConnectionKind, ManagedProviderHandle, PlaybackLeaseRef},
    channel_group::{rank_members, FailureClass, HalfOpenGuard, MemberHealthIndex, RankedCandidate},
    stream_options::StreamOptions,
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
}

pub(crate) fn group_shared_key(target_id: u16, group_vid: u32) -> String { format!("group:{target_id}:{group_vid}") }

pub(crate) struct GroupSelectionRequest<'a> {
    pub target_name: &'a Arc<str>,
    pub group: &'a ChannelGroupRecord,
    pub policy: ChannelGroupPolicy,
    pub mode: SelectionMode,
    pub start: ChannelGroupStartConfig,
    pub addr: &'a SocketAddr,
    pub username: &'a str,
    pub priority: i8,
    pub kind: ConnectionKind,
    pub lease: Option<PlaybackLeaseRef<'a>>,
    pub allow_provider_grace: bool,
    pub exclude: &'a [usize],
    pub demoted_inputs: &'a [Arc<str>],
    pub prefer: Option<usize>,
    /// Recording proxy users play inputs in maintenance, as for single-source playback.
    pub bypass_maintenance: bool,
    /// Runtime switches must not evict other playbacks; only a start may preempt.
    pub allow_preempt: bool,
}

impl<'a> GroupSelectionRequest<'a> {
    /// A start selection: live mode, grace and preemption allowed, nothing excluded.
    pub(crate) fn new(
        target_name: &'a Arc<str>,
        group: &'a ChannelGroupRecord,
        start: ChannelGroupStartConfig,
        addr: &'a SocketAddr,
        username: &'a str,
        priority: i8,
        kind: ConnectionKind,
    ) -> Self {
        Self {
            target_name,
            group,
            policy: group.policy,
            mode: SelectionMode::Live,
            start,
            addr,
            username,
            priority,
            kind,
            lease: None,
            allow_provider_grace: true,
            exclude: &[],
            demoted_inputs: &[],
            prefer: None,
            bypass_maintenance: false,
            allow_preempt: true,
        }
    }
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
    /// Every remaining candidate's input is in maintenance.
    Maintenance,
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
    let budget_secs = req.start.timeout_secs;
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
    if !req.bypass_maintenance {
        ranked.retain(|c| !app_state.maintenance.is_in_maintenance(&req.group.members[c.index].input_name));
        if ranked.is_empty() {
            return Err(GroupSelectionFailure::Maintenance);
        }
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
    // The own slot being released is reused through grace, unless grace is not allowed at all.
    let own_release_grace = req.allow_provider_grace;
    let tier1_match = |tier| match tier {
        CapacityTier::OwnRelease => Some(own_release_grace),
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
    if !req.allow_preempt {
        return Err(GroupSelectionFailure::Exhausted);
    }
    if let Some(acquired) = try_acquire_preempt(app_state, &req, &remaining, all_circuits_open).await {
        return Ok(acquired);
    }

    Err(GroupSelectionFailure::Exhausted)
}

pub(crate) async fn resolve_hls_group_entry(
    app_state: &Arc<AppState>,
    group: Arc<ChannelGroupRecord>,
    req: GroupSelectionRequest<'_>,
) -> Result<ResolvedGroupPlayback, GroupSelectionFailure> {
    let max_attempts = u32::from(req.start.max_attempts.max(1));
    let mut attempts = 0;
    let start_time = std::time::Instant::now();
    let budget = Duration::from_secs(req.start.timeout_secs);
    let mut exclude: Vec<usize> = req.exclude.to_vec();
    let demoted_inputs: Vec<Arc<str>> = req.demoted_inputs.to_vec();

    while attempts < max_attempts && (req.start.timeout_secs == 0 || start_time.elapsed() < budget) {
        attempts += 1;
        let attempt_req = GroupSelectionRequest {
            mode: req.mode.clone(),
            exclude: &exclude,
            demoted_inputs: &demoted_inputs,
            prefer: if attempts == 1 { req.prefer } else { None },
            ..req
        };

        let acquired = acquire_group_member(app_state, attempt_req).await?;
        let member = group.members[acquired.index].clone();
        let Some(member_input) = app_state.app_config.get_input_by_name(&member.input_name) else {
            record_member_failure(&app_state.member_health, &member, acquired.half_open, FailureClass::Stream);
            drop(acquired.handle);
            exclude.push(acquired.index);
            continue;
        };

        let stream_url = Arc::clone(&member.url);
        return Ok(ResolvedGroupPlayback { acquired, member, input: member_input, stream_url });
    }

    Err(GroupSelectionFailure::Exhausted)
}

pub(crate) fn record_member_failure(
    health: &MemberHealthIndex,
    member: &GroupMember,
    guard: Option<HalfOpenGuard>,
    class: FailureClass,
) {
    let now = std::time::Instant::now();
    match guard {
        Some(guard) => health.record_classified_failure_with(guard, class, now),
        None => health.record_classified_failure(&member_key(member), class, now),
    }
}

pub(crate) fn record_member_success(health: &MemberHealthIndex, member: &GroupMember, guard: Option<HalfOpenGuard>) {
    match guard {
        Some(guard) => health.record_success_with(guard),
        None => health.record_success(&member_key(member)),
    }
}

/// Selection settings of `target`, defaults when the target has no `channel_groups` block.
/// The `config.yml` channel group settings with the overrides of `target`.
pub(crate) fn group_playback_settings(app_state: &AppState, target: &ConfigTarget) -> ChannelGroupPlayback {
    let target_cfg = target.options.as_ref().and_then(|options| options.channel_groups.as_ref());
    app_state.app_config.config.load().channel_groups.playback(target_cfg)
}

/// First member whose input is configured, with its URL; it stands in for the group until a
/// member is selected.
pub(crate) fn group_representative(
    app_state: &AppState,
    group: &ChannelGroupRecord,
) -> Option<(Arc<ConfigInput>, Arc<str>)> {
    group.members.iter().find_map(|member| {
        app_state.app_config.get_input_by_name(&member.input_name).map(|input| (input, Arc::clone(&member.url)))
    })
}

/// The client channel of a group playback carrying the identity of `member`.
pub(crate) fn member_stream_channel(
    base: &StreamChannel,
    group: &ChannelGroupRecord,
    member: &GroupMember,
) -> StreamChannel {
    let mut channel = base.clone();
    channel.virtual_id = member.member_vid.get();
    channel.group_virtual_id = Some(group.group_vid.get());
    channel.input_name = Arc::clone(&member.input_name);
    channel.item_type = member.item_type;
    channel.cluster = member.cluster;
    channel.title = Arc::clone(&member.title);
    channel.url = Arc::clone(&member.url);
    channel.provider_id = member.provider_id;
    channel
}

pub(crate) const PROBE_MIN_BYTES: usize = 3 * tuliprox_mpegts::packet::TS_PACKET_SIZE;
const PROBE_MAX_EMPTY_CHUNKS: u32 = 32;
/// Upper bound for the provider response of one member; the start budget can only shorten it.
pub(crate) const PROVIDER_OPEN_TIMEOUT: Duration = Duration::from_secs(4);
/// Upper bound for the first bytes of one member; the start budget can only shorten it.
pub(crate) const PROBE_FIRST_BYTES_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbedPayload {
    Ts,
    Playlist,
}

fn starts_with_playlist_tag(head: &[u8]) -> bool {
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = head.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(head.len());
    head[start..].starts_with(b"#EXTM3U")
}

/// Reads the head of a member body until it shows MPEG-TS sync or an HLS/DASH playlist, bounded by
/// `deadline`. Returns the body with the read chunks put back in front.
pub(crate) async fn probe_member_payload(
    mut stream: BoxedProviderStream,
    adaptive_hint: bool,
    deadline: tokio::time::Instant,
) -> Result<(BoxedProviderStream, ProbedPayload), ()> {
    if adaptive_hint {
        return Ok((stream, ProbedPayload::Playlist));
    }
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut head: Vec<u8> = Vec::with_capacity(PROBE_MIN_BYTES);
    let mut empty_chunks = 0u32;
    while head.len() < PROBE_MIN_BYTES {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) if chunk.is_empty() => {
                empty_chunks += 1;
                if empty_chunks > PROBE_MAX_EMPTY_CHUNKS {
                    return Err(());
                }
            }
            Ok(Some(Ok(chunk))) => {
                let take = chunk.len().min(PROBE_MIN_BYTES - head.len());
                head.extend_from_slice(&chunk[..take]);
                chunks.push(chunk);
            }
            Ok(None) => break,
            Ok(Some(Err(_))) | Err(_) => return Err(()),
        }
    }
    let payload = if starts_with_playlist_tag(&head) {
        ProbedPayload::Playlist
    } else if tuliprox_mpegts::packet::ts_sync_offset(&head)
        .is_some_and(|offset| head.len() > offset + tuliprox_mpegts::packet::TS_PACKET_SIZE)
    {
        // Two sync bytes one packet apart: a single 0x47 in a short text body proves nothing.
        ProbedPayload::Ts
    } else {
        return Err(());
    };
    let stream = if chunks.is_empty() { stream } else { PrefixedStream::new(chunks, stream).boxed() };
    Ok((stream, payload))
}

/// A provider body with the chunks a probe already read put back in front.
pub(crate) struct PrefixedStream {
    head: VecDeque<Bytes>,
    inner: BoxedProviderStream,
}

impl PrefixedStream {
    pub(crate) fn new(head: Vec<Bytes>, inner: BoxedProviderStream) -> Self { Self { head: head.into(), inner } }
}

impl futures::Stream for PrefixedStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.head.pop_front() {
            Some(chunk) => std::task::Poll::Ready(Some(Ok(chunk))),
            None => this.inner.as_mut().poll_next(cx),
        }
    }
}

/// Waits until `has_capacity` holds or `deadline` passes, re-checking on every `notify` wake-up
/// instead of polling.
pub(crate) async fn wait_for_capacity<F, Fut>(
    notify: &Notify,
    deadline: tokio::time::Instant,
    mut has_capacity: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if has_capacity().await {
            return true;
        }
        tokio::select! {
            () = notified => {}
            () = tokio::time::sleep_until(deadline) => return has_capacity().await,
        }
    }
}

/// Waits until `user` is below its connection limit or `deadline` passes.
pub(crate) async fn wait_for_user_capacity(
    app_state: &AppState,
    user: &ProxyUserCredentials,
    deadline: tokio::time::Instant,
) -> bool {
    let notify = app_state.connection_manager.capacity_notified();
    wait_for_capacity(&notify, deadline, || async {
        app_state.active_users.user_connections(&user.username).await < user.max_connections
    })
    .await
}

pub(crate) struct GroupOpenRequest<'a> {
    pub app_state: &'a Arc<AppState>,
    pub user: &'a ProxyUserCredentials,
    pub fingerprint: &'a Fingerprint,
    pub req_headers: &'a HeaderMap,
    pub group: &'a ChannelGroupRecord,
    pub base_channel: &'a StreamChannel,
    pub stream_options: &'a StreamOptions,
    pub share_stream: bool,
    pub connection_permission: UserConnectionPermission,
    pub connection_kind: ConnectionKind,
    pub priority: i8,
    pub session_token: &'a str,
    pub is_reopen: bool,
    pub deadline: tokio::time::Instant,
}

pub(crate) struct OpenedGroupMember {
    pub index: usize,
    pub tier: CapacityTier,
    pub member: GroupMember,
    pub input: Arc<ConfigInput>,
    pub url: Arc<str>,
    pub channel: StreamChannel,
    /// `details.stream` carries the probed head in front of the provider body.
    pub details: StreamDetails,
    pub payload: ProbedPayload,
}

pub(crate) struct MemberOpenFailure {
    pub index: usize,
    pub input_name: Arc<str>,
    pub class: FailureClass,
}

fn is_adaptive_content_type(details: &StreamDetails) -> bool {
    details.stream_info.as_ref().is_some_and(|(headers, _, _, _)| {
        headers.iter().any(|(name, value)| {
            name.as_str().eq_ignore_ascii_case("content-type") && {
                let value = value.to_ascii_lowercase();
                value.contains("mpegurl") || value.contains("dash+xml")
            }
        })
    })
}

/// The URL to request for `member`; a Stalker member needs its playback link resolved first.
async fn member_playback_url(
    req: &GroupOpenRequest<'_>,
    input: &Arc<ConfigInput>,
    member: &GroupMember,
) -> Option<Arc<str>> {
    if !input.input_type.is_stalker() {
        return Some(Arc::clone(&member.url));
    }
    resolve_initial_stalker_playback_url(
        req.app_state,
        input,
        req.user,
        member.provider_id,
        member.cluster,
        member.item_type,
        &member.url,
    )
    .await
    .ok()
}

/// Why an open produced no usable provider body, `None` when it did. A custom video in place of
/// the body counts as a failure.
fn open_failure_class(details: &StreamDetails) -> Option<FailureClass> {
    let exhausted_class = || {
        if details.custom_reason == Some(ProviderStreamCustomReason::ProviderExhausted) {
            FailureClass::Capacity
        } else {
            FailureClass::Stream
        }
    };
    match &details.stream_info {
        Some((_, _, _, Some(_))) => Some(exhausted_class()),
        _ if details.stream.is_some() => None,
        Some((_, status, _, None)) => Some(FailureClass::from_status(*status)),
        None => Some(exhausted_class()),
    }
}

/// Opens the acquired member: input lookup, Stalker link, provider open, custom-video and status
/// classification, payload probe. Records the outcome in the health index; on failure the provider
/// slot is released.
pub(crate) async fn open_group_member(
    req: &GroupOpenRequest<'_>,
    acquired: AcquiredMember,
) -> Result<OpenedGroupMember, MemberOpenFailure> {
    let app_state = req.app_state;
    let AcquiredMember { index, handle, tier, half_open } = acquired;
    let member = req.group.members[index].clone();
    let fail = |class: FailureClass, guard: Option<HalfOpenGuard>| {
        record_member_failure(&app_state.member_health, &member, guard, class);
        MemberOpenFailure { index, input_name: Arc::clone(&member.input_name), class }
    };

    let Some(input) = app_state.app_config.get_input_by_name(&member.input_name) else {
        drop(handle);
        return Err(fail(FailureClass::Stream, half_open));
    };
    let channel = member_stream_channel(req.base_channel, req.group, &member);

    // Neither the Stalker link nor the provider response may outlive the budget: a provider that
    // accepts the connection and never answers must not hold the start or the switch.
    let open_deadline = req.deadline.min(tokio::time::Instant::now() + PROVIDER_OPEN_TIMEOUT);
    let Ok(Some(url)) = tokio::time::timeout_at(open_deadline, member_playback_url(req, &input, &member)).await else {
        drop(handle);
        return Err(fail(FailureClass::Transport, half_open));
    };

    let content_representation = if member.item_type == PlaylistItemType::Catchup {
        crate::api::model::ProviderContentRepresentationMode::Identity
    } else {
        crate::api::model::ProviderContentRepresentationMode::PreserveOrigin
    };
    let opened = tokio::time::timeout_at(
        open_deadline,
        create_stream_response_details(
            app_state,
            req.stream_options,
            url.as_ref(),
            &req.user.username,
            req.fingerprint,
            req.req_headers,
            &input,
            &channel,
            member.item_type,
            content_representation,
            req.share_stream,
            req.connection_permission,
            None,
            true,
            true,
            VirtualId::new(member.member_vid.get()),
            req.priority,
            req.connection_kind,
            req.is_reopen,
            Some(req.session_token),
            None,
            true,
            None,
            None,
            None,
            Some(handle),
        ),
    )
    .await;
    let mut details = match opened {
        Ok(Ok(details)) => details,
        Ok(Err(_)) => return Err(fail(FailureClass::Stream, half_open)),
        // The dropped open released the provider slot it held.
        Err(_) => return Err(fail(FailureClass::Transport, half_open)),
    };

    let release =
        |details: StreamDetails| app_state.connection_manager.release_managed_provider_handle(details.provider_handle);
    if let Some(class) = open_failure_class(&details) {
        release(details);
        return Err(fail(class, half_open));
    }
    let Some(provider_stream) = details.stream.take() else {
        release(details);
        return Err(fail(FailureClass::Stream, half_open));
    };

    let adaptive_hint = member.item_type.is_live_adaptive() || is_adaptive_content_type(&details);
    let probe_deadline = req.deadline.min(tokio::time::Instant::now() + PROBE_FIRST_BYTES_TIMEOUT);
    let Ok((provider_stream, payload)) = probe_member_payload(provider_stream, adaptive_hint, probe_deadline).await
    else {
        release(details);
        return Err(fail(FailureClass::Stream, half_open));
    };
    details.stream = Some(provider_stream);
    record_member_success(&app_state.member_health, &member, half_open);
    Ok(OpenedGroupMember { index, tier, member, input, url, channel, details, payload })
}
