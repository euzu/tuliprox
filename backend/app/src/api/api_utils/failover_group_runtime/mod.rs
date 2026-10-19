//! Runtime failover of failover group playback: the provider body behind an open client response
//! is replaced by another member when it fails, stalls or delivers slower than real time.

mod budget;
#[cfg(test)]
mod tests;

use super::{
    connection_priority_for_kind,
    failover_group::{
        acquire_group_member, open_group_member, record_member_failure, GroupOpenRequest, GroupSelectionFailure,
        GroupSelectionRequest, OpenedGroupMember, ProbedPayload,
    },
    get_stream_options, StreamResponseMode,
};
use crate::{
    api::model::{AppState, BoxedProviderStream, StreamError},
    auth::Fingerprint,
    model::{ConfigTarget, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::http::HeaderMap;
pub(crate) use budget::{BudgetDecision, FailoverBudget};
use bytes::Bytes;
use futures::Future;
use shared::model::{EventMessage, FailoverGroupPlayback, StreamChannel, SwitchReason, UserConnectionPermission};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::AllocationId;
use tuliprox_mpegts::{
    packet::ts_null_packet_bytes,
    splice::{RealtimeMeter, SpliceOutput, TsSpliceAligner},
};
use tuliprox_repository::failover_group_repository::FailoverGroupRecord;
use tuliprox_session::{
    active_provider_manager::{ConnectionKind, ManagedProviderHandle},
    failover_group::{AudioSignature, FailureClass, SelectionMode},
    MemberBinding,
};

/// While a switch runs, a null packet this often keeps players and proxies from timing out.
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
/// Longest stretch of a new member held back while waiting for its splice point (PAT/PMT and a
/// keyframe); beyond it the member starts at its first PAT.
const SPLICE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
const SPLICE_BUDGET: Duration = Duration::from_secs(4);
/// Re-check interval while every candidate is busy during a switch.
const EXHAUSTED_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Pause before a new round over members that all failed within one switch; it doubles per
/// round up to the maximum.
const NO_CANDIDATE_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const NO_CANDIDATE_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(4);
/// Upper bound for the first bytes of a member opened at runtime.
const RUNTIME_OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Everything a running group stream needs to open another member for its client.
pub(crate) struct GroupFailoverContext {
    pub app_state: Arc<AppState>,
    pub target: Arc<ConfigTarget>,
    /// `target.name` as the health and selection key, allocated once per stream.
    pub target_name: Arc<str>,
    pub group: Arc<FailoverGroupRecord>,
    pub playback: FailoverGroupPlayback,
    pub user: ProxyUserCredentials,
    pub fingerprint: Fingerprint,
    pub req_headers: HeaderMap,
    pub session_token: Arc<str>,
    pub connection_kind: ConnectionKind,
    pub connection_permission: UserConnectionPermission,
    pub share_stream: bool,
    pub base_channel: StreamChannel,
    /// Key of the shared stream this group stream feeds; its provider allocation moves with the
    /// member.
    pub shared_key: Option<Arc<str>>,
}

/// The member a switch opened: its provider body and the slot it holds.
pub(crate) struct RuntimeMember {
    pub index: usize,
    pub stream: BoxedProviderStream,
    pub handle: Option<ManagedProviderHandle>,
    /// Cancelled when the provider manager preempts the member's slot.
    pub cancel: Option<CancellationToken>,
    /// The member's provider allocation; a shared stream owns it now.
    pub allocation_id: Option<AllocationId>,
}

pub(crate) type SwitchFuture = Pin<Box<dyn Future<Output = Option<RuntimeMember>> + Send>>;

/// Decides about and prepares member switches of one running group stream. The owner of the
/// provider handle (direct client stream or shared producer) executes them.
pub(crate) struct GroupFailoverDriver {
    ctx: Arc<GroupFailoverContext>,
    current: usize,
    budget: FailoverBudget,
    aligner: TsSpliceAligner,
    meter: RealtimeMeter,
    stall_timeout: Option<Duration>,
    stall: Pin<Box<Sleep>>,
    stall_armed: bool,
    keepalive: Pin<Box<Sleep>>,
    pending_since: Option<Instant>,
    underrun: bool,
    /// Second piece of a split chunk, sent before the provider body is polled again.
    pending_output: Option<Bytes>,
    /// Provider allocation of the current member while a shared stream owns it.
    shared_allocation: Option<AllocationId>,
}

impl GroupFailoverDriver {
    pub(crate) fn new(ctx: Arc<GroupFailoverContext>, current: usize) -> Self {
        let failover = ctx.playback.failover;
        Self {
            budget: FailoverBudget::new(&failover),
            aligner: TsSpliceAligner::new(SPLICE_BUDGET_BYTES, SPLICE_BUDGET),
            meter: RealtimeMeter::new(failover.slow_source_pct, Duration::from_secs(failover.slow_source_secs)),
            stall_timeout: (failover.no_data_timeout_secs > 0)
                .then(|| Duration::from_secs(failover.no_data_timeout_secs)),
            stall: Box::pin(tokio::time::sleep(Duration::ZERO)),
            stall_armed: false,
            keepalive: Box::pin(tokio::time::sleep(Duration::ZERO)),
            pending_since: None,
            underrun: false,
            pending_output: None,
            shared_allocation: None,
            ctx,
            current,
        }
    }

    pub(crate) fn group_vid(&self) -> u32 { self.ctx.group.group_vid.get() }

    /// Hot path: aligns the chunk and does the per-chunk bookkeeping. `None` while the aligner
    /// holds packets back.
    pub(crate) fn process_chunk(&mut self, chunk: Bytes) -> Option<Bytes> {
        let now = Instant::now();
        let waited = self.pending_since.take().map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let (first, second) = match self.aligner.push(chunk, now) {
            SpliceOutput::Hold => return None,
            SpliceOutput::Emit(packets) => (packets, None),
            SpliceOutput::EmitTwo(first, second) => (first, Some(second)),
        };
        // Only packets that reach the client count as playback and hold off the stall check.
        self.stall_armed = false;
        self.budget.on_data(now);
        let mut slow = self.meter.observe(&first, waited, now);
        if let Some(second) = &second {
            slow |= self.meter.observe(second, Duration::ZERO, now);
        }
        if slow {
            self.underrun = true;
            self.meter.reset(now);
        }
        self.pending_output = second;
        Some(first)
    }

    /// The second piece of the last split chunk, to send before polling the provider again.
    pub(crate) fn take_pending_output(&mut self) -> Option<Bytes> { self.pending_output.take() }

    /// Called while the provider body is pending; true once it stayed silent for the stall timeout.
    pub(crate) fn poll_stalled(&mut self, cx: &mut Context<'_>) -> bool {
        let now = Instant::now();
        // The wait also feeds the underrun meter, with or without a stall timeout.
        self.pending_since.get_or_insert(now);
        let Some(timeout) = self.stall_timeout else {
            return false;
        };
        if !self.stall_armed {
            self.stall.as_mut().reset(tokio::time::Instant::from_std(now + timeout));
            self.stall_armed = true;
        }
        self.stall.as_mut().poll(cx).is_ready()
    }

    /// True once the realtime meter detected an underrun; consumes the signal.
    pub(crate) fn take_underrun(&mut self) -> bool { std::mem::take(&mut self.underrun) }

    /// Records the failure of the current member and returns the future that opens the next one,
    /// or `None` when the budget is spent. The caller releases the current provider slot before
    /// it polls the future, so a retry on the same account finds its slot free.
    pub(crate) fn begin_switch(&mut self, reason: SwitchReason, status: Option<u16>) -> Option<SwitchFuture> {
        let now = Instant::now();
        // A slow member still plays: when no switch is allowed it keeps playing, and the check
        // leaves the incident untouched.
        if reason == SwitchReason::Underrun && self.budget.would_give_up(now) {
            return None;
        }
        let group = &self.ctx.group;
        // The member failed whether or not another one may take over.
        record_member_failure(
            &self.ctx.app_state.member_health,
            &group.members[self.current],
            None,
            failure_class_for(reason, status),
        );
        let decision = self.budget.on_trigger(self.current, group.members.len(), now);
        let (exclude, prefer) = match decision {
            BudgetDecision::GiveUp => return None,
            BudgetDecision::RetrySame => (Vec::new(), Some(self.current)),
            BudgetDecision::Next { exclude } => (exclude, None),
        };
        let deadline = self.budget.deadline().unwrap_or(now);
        self.stall_armed = false;
        self.pending_since = None;
        self.keepalive.as_mut().reset(tokio::time::Instant::now() + KEEPALIVE_INTERVAL);
        Some(Box::pin(switch_member(
            Arc::clone(&self.ctx),
            SwitchRequest {
                shared_allocation: self.shared_allocation,
                previous: self.current,
                exclude,
                prefer,
                deadline: tokio::time::Instant::from_std(deadline),
                reason,
            },
        )))
    }

    /// Continues on the member a switch opened.
    pub(crate) fn on_switched(&mut self, index: usize) {
        let now = Instant::now();
        self.budget.on_switched(index, self.current, now);
        self.current = index;
        self.aligner.begin_member(now);
        self.meter.begin_member(now);
        self.stall_armed = false;
        self.pending_since = None;
        self.underrun = false;
    }

    /// While a switch runs: a null packet every `KEEPALIVE_INTERVAL`.
    pub(crate) fn poll_keepalive(&mut self, cx: &mut Context<'_>) -> Poll<Bytes> {
        if self.keepalive.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.keepalive.as_mut().reset(tokio::time::Instant::now() + KEEPALIVE_INTERVAL);
        Poll::Ready(ts_null_packet_bytes())
    }
}

/// How a member failure counts in the health index: an upstream error by its HTTP status, a
/// transport error without one; an ended, stalled or slow body as a stream failure.
pub(crate) fn failure_class_for(reason: SwitchReason, status: Option<u16>) -> FailureClass {
    match (reason, status.and_then(|status| axum::http::StatusCode::from_u16(status).ok())) {
        (SwitchReason::UpstreamError, Some(status)) => FailureClass::from_status(status),
        (SwitchReason::UpstreamError, None) => FailureClass::Transport,
        (SwitchReason::UpstreamEof | SwitchReason::Stall | SwitchReason::Underrun, _) => FailureClass::Stream,
    }
}

/// The token the provider manager cancels when it preempts the slot of `handle`.
pub(crate) fn member_cancel_token(handle: Option<&ManagedProviderHandle>) -> Option<CancellationToken> {
    handle.and_then(ManagedProviderHandle::handle).and_then(|handle| handle.cancel_token.clone())
}

struct SwitchRequest {
    /// Provider allocation of the replaced member, when a shared stream owns it.
    shared_allocation: Option<AllocationId>,
    previous: usize,
    exclude: Vec<usize>,
    prefer: Option<usize>,
    deadline: tokio::time::Instant,
    reason: SwitchReason,
}

/// Selection mode of a runtime switch: prefer members like the one that played.
fn runtime_mode(ctx: &GroupFailoverContext, previous: usize) -> SelectionMode {
    let probe = ctx
        .app_state
        .member_health
        .health(&ctx.target_name, &ctx.group.members[previous], std::time::Instant::now())
        .probe;
    SelectionMode::Runtime {
        current_codec: probe.as_ref().map(|p| p.codec.clone()).unwrap_or_default(),
        current_resolution: probe.as_ref().map(|p| p.resolution.clone()).unwrap_or_default(),
        current_audio: probe.as_ref().map(|p| AudioSignature {
            codec: p.audio_codec.clone(),
            channels: p.audio_channels.clone(),
            language: p.audio_language.clone(),
        }),
        audio_language: ctx.playback.failover.audio_language,
    }
}

/// Takes over the opened member: a shared stream receives its provider slot, the session is bound
/// to it and the switch is announced.
async fn adopt_member(
    ctx: &GroupFailoverContext,
    detached: Option<AllocationId>,
    previous: usize,
    reason: SwitchReason,
    opened: OpenedGroupMember,
    stream: BoxedProviderStream,
) -> Option<RuntimeMember> {
    let app_state = &ctx.app_state;
    let mut handle = opened.details.provider_handle;
    let cancel = member_cancel_token(handle.as_ref());
    let slot = handle.as_ref().and_then(ManagedProviderHandle::handle);
    let allocation_id = slot.map(|handle| handle.allocation_id);
    let provider = slot
        .and_then(|handle| handle.allocation.get_provider_name())
        .unwrap_or_else(|| Arc::clone(&opened.member.input_name));
    if let (Some(key), Some(detached)) = (&ctx.shared_key, detached) {
        let attached = match handle.take() {
            Some(handle) => app_state.shared_stream_manager.attach_provider(key, detached, handle).await,
            None => true,
        };
        if !attached {
            // The shared stream ended meanwhile; the dropped handle released its slot.
            return None;
        }
    }
    let member_vid = opened.member.member_vid.get();
    app_state
        .active_users
        .bind_member(
            &ctx.user.username,
            &ctx.session_token,
            MemberBinding {
                group_vid: ctx.group.group_vid.get(),
                member_vid,
                member_index: opened.index,
                input_name: Arc::clone(&opened.member.input_name),
            },
        )
        .await;
    if ctx.shared_key.is_some() {
        // Every viewer of the shared stream plays the new member, not only the one that started it.
        app_state
            .active_users
            .update_shared_group_provider(ctx.target.id, ctx.group.group_vid.get(), Arc::clone(&provider))
            .await;
    }
    app_state
        .active_users
        .update_session_provider_binding(&ctx.user.username, &ctx.session_token, provider, Arc::clone(&opened.url))
        .await;
    app_state.event_manager.send_event(EventMessage::FailoverGroupMemberSwitched {
        target_id: ctx.target.id,
        group_vid: ctx.group.group_vid.get(),
        from_member_vid: ctx.group.members[previous].member_vid.get(),
        to_member_vid: member_vid,
        reason,
    });
    Some(RuntimeMember { index: opened.index, stream, handle, cancel, allocation_id })
}

async fn switch_member(ctx: Arc<GroupFailoverContext>, request: SwitchRequest) -> Option<RuntimeMember> {
    let SwitchRequest { previous, mut exclude, mut prefer, deadline, reason, shared_allocation } = request;
    let app_state = &ctx.app_state;
    let priority = connection_priority_for_kind(&ctx.user, ctx.connection_kind);
    let stream_options = get_stream_options(&app_state.app_config, StreamResponseMode::Stream);
    let mode = runtime_mode(&ctx, previous);
    let mut demoted_inputs: Vec<Arc<str>> = Vec::new();
    // Members a new round must not try again within this switch: their provider refused the
    // account (auth, capacity) or delivers nothing that can be spliced.
    let mut hard_exclude: Vec<usize> = Vec::new();
    let mut round_pause = NO_CANDIDATE_RETRY_INTERVAL;
    if let Some(key) = &ctx.shared_key {
        // Subscribers stay while the failed member's slot is given back. Without the expected
        // allocation the shared stream is no longer this one's and nothing may be switched.
        let detached = match shared_allocation {
            Some(allocation_id) => app_state.shared_stream_manager.detach_provider(key, allocation_id).await,
            None => false,
        };
        if !detached {
            return None;
        }
    }
    loop {
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        let wanted = prefer.take();
        let req = GroupSelectionRequest {
            mode: mode.clone(),
            allow_provider_grace: false,
            exclude: &exclude,
            demoted_inputs: &demoted_inputs,
            prefer: wanted,
            bypass_maintenance: ctx.user.is_recording_proxy_user(),
            allow_preempt: false,
            ..GroupSelectionRequest::new(
                &ctx.target_name,
                &ctx.group,
                ctx.playback.start,
                &ctx.fingerprint.addr,
                &ctx.user.username,
                priority,
                ctx.connection_kind,
            )
        };
        let acquired = match tokio::time::timeout_at(deadline, acquire_group_member(app_state, req)).await {
            Ok(Ok(acquired)) => acquired,
            Ok(Err(GroupSelectionFailure::Exhausted)) => {
                // A released slot frees once its provider body has ended, which happens
                // asynchronously; the slot this stream just gave back is worth waiting for.
                prefer = wanted;
                tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + EXHAUSTED_RETRY_INTERVAL)).await;
                continue;
            }
            Ok(Err(GroupSelectionFailure::NoCandidate)) => {
                // Every other member failed in this switch: start a new round over them after a
                // pause, still bounded by the budget. The replaced member stays out.
                exclude.retain(|index| *index == previous || hard_exclude.contains(index));
                tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + round_pause)).await;
                round_pause = (round_pause * 2).min(NO_CANDIDATE_RETRY_MAX_INTERVAL);
                continue;
            }
            Ok(Err(_)) | Err(_) => return None,
        };
        let open_req = GroupOpenRequest {
            app_state,
            user: &ctx.user,
            fingerprint: &ctx.fingerprint,
            req_headers: &ctx.req_headers,
            group: &ctx.group,
            base_channel: &ctx.base_channel,
            stream_options: &stream_options,
            share_stream: ctx.share_stream,
            connection_permission: ctx.connection_permission,
            connection_kind: ctx.connection_kind,
            priority,
            session_token: &ctx.session_token,
            is_reopen: true,
            deadline: deadline.min(tokio::time::Instant::now() + RUNTIME_OPEN_TIMEOUT),
        };
        match open_group_member(&open_req, acquired).await {
            Ok(mut opened) => {
                let stream = opened.details.stream.take();
                let (Some(stream), ProbedPayload::Ts) = (stream, opened.payload) else {
                    // Only MPEG-TS can be spliced into the running response.
                    app_state.connection_manager.release_managed_provider_handle(opened.details.provider_handle);
                    exclude.push(opened.index);
                    hard_exclude.push(opened.index);
                    continue;
                };
                return adopt_member(&ctx, shared_allocation, previous, reason, opened, stream).await;
            }
            Err(failure) => {
                if matches!(failure.class, FailureClass::Auth | FailureClass::Capacity) {
                    if !demoted_inputs.contains(&failure.input_name) {
                        demoted_inputs.push(failure.input_name);
                    }
                    hard_exclude.push(failure.index);
                }
                exclude.push(failure.index);
            }
        }
    }
}

/// How a group playback's member body ended for good: no member could take over.
pub(crate) enum GroupEnd {
    Eof,
    Error(StreamError),
    Stalled,
    /// A switch was started for `reason` but no member could be opened within the budget.
    SwitchFailed {
        reason: SwitchReason,
    },
}

pub(crate) enum GroupPoll {
    Data(Bytes),
    /// A null packet sent while a switch runs: keeps the connection alive, carries no media.
    Keepalive(Bytes),
    Pending,
    Ended(GroupEnd),
}

/// Owner of a group playback's provider slot and body: the direct client stream or the producer
/// of a shared stream.
pub(crate) trait MemberHost {
    fn member_body(&mut self) -> Option<&mut BoxedProviderStream>;
    /// True when the provider manager preempted the current member's slot: no failover then.
    fn member_preempted(&self) -> bool;
    /// Gives up the failed member's body and slot before the next member is acquired, so the
    /// same account can be reused.
    fn release_member(&mut self);
    fn install_member(&mut self, member: RuntimeMember);
}

/// The switch state machine of a group playback, shared by its direct and shared hosts.
pub(crate) struct GroupSwitcher {
    driver: GroupFailoverDriver,
    /// The switch in flight and the failure that started it.
    switch: Option<(SwitchFuture, SwitchReason)>,
}

impl GroupSwitcher {
    pub(crate) fn new(driver: GroupFailoverDriver) -> Self { Self { driver, switch: None } }

    /// True while another member is being opened.
    pub(crate) fn is_switching(&self) -> bool { self.switch.is_some() }

    /// Polls the member body of `host`, switching members underneath it when it fails.
    pub(crate) fn poll_member(&mut self, host: &mut impl MemberHost, cx: &mut Context<'_>) -> GroupPoll {
        loop {
            if let Some(packets) = self.driver.take_pending_output() {
                return GroupPoll::Data(packets);
            }
            if let Some((switch, reason)) = self.switch.as_mut() {
                let reason = *reason;
                match switch.as_mut().poll(cx) {
                    Poll::Ready(Some(member)) => {
                        self.switch = None;
                        self.driver.on_switched(member.index);
                        self.driver.shared_allocation = member.allocation_id;
                        host.install_member(member);
                        continue;
                    }
                    Poll::Ready(None) => {
                        self.switch = None;
                        return GroupPoll::Ended(GroupEnd::SwitchFailed { reason });
                    }
                    Poll::Pending => {
                        return match self.driver.poll_keepalive(cx) {
                            Poll::Ready(keepalive) => GroupPoll::Keepalive(keepalive),
                            Poll::Pending => GroupPoll::Pending,
                        };
                    }
                }
            }
            let Some(body) = host.member_body() else {
                return GroupPoll::Ended(GroupEnd::Eof);
            };
            match body.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let packets = self.driver.process_chunk(chunk);
                    if self.driver.take_underrun() {
                        // The packets already read still go out; the next poll switches.
                        self.start_switch(host, SwitchReason::Underrun, None);
                    }
                    if let Some(packets) = packets {
                        return GroupPoll::Data(packets);
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    if !self.start_switch(host, SwitchReason::UpstreamError, err.provider_http_status()) {
                        return GroupPoll::Ended(GroupEnd::Error(err));
                    }
                }
                Poll::Ready(None) => {
                    if !self.start_switch(host, SwitchReason::UpstreamEof, None) {
                        return GroupPoll::Ended(GroupEnd::Eof);
                    }
                }
                Poll::Pending => {
                    if !self.driver.poll_stalled(cx) {
                        return GroupPoll::Pending;
                    }
                    if !self.start_switch(host, SwitchReason::Stall, None) {
                        return GroupPoll::Ended(GroupEnd::Stalled);
                    }
                }
            }
        }
    }

    fn start_switch(&mut self, host: &mut impl MemberHost, reason: SwitchReason, status: Option<u16>) -> bool {
        if host.member_preempted() {
            return false;
        }
        let Some(switch) = self.driver.begin_switch(reason, status) else {
            return false;
        };
        debug_if_enabled!("Group stream {} lost its member ({reason:?}); switching", self.driver.group_vid());
        host.release_member();
        self.switch = Some((switch, reason));
        true
    }
}

/// The producer of a shared group stream: forwards the member body to the shared stream and
/// replaces the member underneath it when it fails. The shared stream owns the provider slot.
pub(crate) struct GroupFailoverStream {
    switcher: GroupSwitcher,
    host: SharedMemberHost,
}

struct SharedMemberHost {
    body: Option<BoxedProviderStream>,
    /// Preemption of the current member's slot.
    member_cancel: Option<CancellationToken>,
}

impl MemberHost for SharedMemberHost {
    fn member_body(&mut self) -> Option<&mut BoxedProviderStream> { self.body.as_mut() }

    fn member_preempted(&self) -> bool { self.member_cancel.as_ref().is_some_and(CancellationToken::is_cancelled) }

    fn release_member(&mut self) { self.body = None; }

    fn install_member(&mut self, member: RuntimeMember) {
        self.member_cancel = member.cancel;
        self.body = Some(member.stream);
    }
}

impl GroupFailoverStream {
    /// `slot` is the provider handle of the first member, which the shared stream takes over.
    pub(crate) fn new(
        mut driver: GroupFailoverDriver,
        inner: BoxedProviderStream,
        slot: Option<&ManagedProviderHandle>,
    ) -> Self {
        driver.shared_allocation = slot.and_then(ManagedProviderHandle::handle).map(|handle| handle.allocation_id);
        Self {
            switcher: GroupSwitcher::new(driver),
            host: SharedMemberHost { body: Some(inner), member_cancel: member_cancel_token(slot) },
        }
    }
}

impl futures::Stream for GroupFailoverStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.switcher.poll_member(&mut this.host, cx) {
            GroupPoll::Data(packets) | GroupPoll::Keepalive(packets) => Poll::Ready(Some(Ok(packets))),
            GroupPoll::Pending => Poll::Pending,
            GroupPoll::Ended(GroupEnd::Error(err)) => Poll::Ready(Some(Err(err))),
            GroupPoll::Ended(GroupEnd::Eof | GroupEnd::Stalled | GroupEnd::SwitchFailed { .. }) => Poll::Ready(None),
        }
    }
}
