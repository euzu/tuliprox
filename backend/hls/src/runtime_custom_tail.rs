use super::{
    availability::{
        commit_prepared_runtime_custom_tail, commit_terminal_tail_if_lease_reserve_requires_cutover,
        HlsTerminalResolution,
    },
    hls_ctx::HlsCtx,
    lease::{new_hls_access_lease_id, HlsRuntimePolicyRevocation, HlsRuntimePolicyRevocationOutcome},
    prepared_terminal_bundle::{
        HlsPreparedTerminalBundle, HlsPreparedTerminalBundleCompletion, HlsPreparedTerminalBundleKey,
        HlsPreparedTerminalBundleObservation, HlsPreparedTerminalBundleState,
    },
    session_store::HlsSessionHandle,
    terminal_tail::{
        snapshot_terminal_media_asset, terminal_media_asset_identity, HlsLeasePlaybackMode, HlsTerminalAssetIdentity,
        HlsTerminalMediaAsset, HlsTerminalTailCompatibility, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    HlsAccessLeaseId, ProxySessionId,
};
use shared::model::CustomVideoStreamType;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Write as _,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tuliprox_core::{
    model::{is_custom_video_stream_enabled, CustomStreamResponse},
    utils::{format_hls_duration_ms, hls_target_duration_secs},
};

const HLS_STANDALONE_CUSTOM_ACCESS_CAPACITY: usize = 1_024;
const HLS_STANDALONE_CUSTOM_ACCESS_MIN_TTL_MS: u64 = 60_000;
const HLS_STANDALONE_CUSTOM_ACCESS_MAX_TTL_MS: u64 = 15 * 60_000;
const HLS_STANDALONE_CUSTOM_ACCESS_COMPLETION_GRACE_MS: u64 = 60_000;

/// Semantic cause carried by one immutable finite custom-media tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HlsRuntimeCustomTailReason {
    ChannelUnavailable,
    LowPriorityPreempted,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    UserAccountExpired,
    SessionOrLeaseExpired,
}

impl HlsRuntimeCustomTailReason {
    pub const fn video_type(self) -> CustomVideoStreamType {
        match self {
            Self::ChannelUnavailable => CustomVideoStreamType::ChannelUnavailable,
            Self::LowPriorityPreempted => CustomVideoStreamType::LowPriorityPreempted,
            Self::UserConnectionsExhausted => CustomVideoStreamType::UserConnectionsExhausted,
            Self::ProviderConnectionsExhausted => CustomVideoStreamType::ProviderConnectionsExhausted,
            Self::UserAccountExpired => CustomVideoStreamType::UserAccountExpired,
            Self::SessionOrLeaseExpired => CustomVideoStreamType::HlsSessionOrLeaseExpired,
        }
    }

    pub const fn trigger_class(self) -> HlsRuntimeCustomTailTriggerClass {
        match self {
            Self::ChannelUnavailable => HlsRuntimeCustomTailTriggerClass::ReserveAwareAvailability,
            Self::LowPriorityPreempted
            | Self::UserConnectionsExhausted
            | Self::ProviderConnectionsExhausted
            | Self::UserAccountExpired
            | Self::SessionOrLeaseExpired => HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
        }
    }

    pub const fn base_policy(self) -> HlsRuntimeCustomTailBasePolicy {
        match self {
            Self::ChannelUnavailable | Self::LowPriorityPreempted | Self::ProviderConnectionsExhausted => {
                HlsRuntimeCustomTailBasePolicy::PreservePublishedSafeSuffix
            }
            Self::UserConnectionsExhausted | Self::UserAccountExpired | Self::SessionOrLeaseExpired => {
                HlsRuntimeCustomTailBasePolicy::PreserveCompletedOrInFlightPrefix
            }
        }
    }

    pub const fn permits_unpublished_lease_standalone_tail(self) -> bool {
        !matches!(self, Self::SessionOrLeaseExpired)
    }

    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ChannelUnavailable => "channel_unavailable",
            Self::LowPriorityPreempted => "low_priority_preempted",
            Self::UserConnectionsExhausted => "user_connections_exhausted",
            Self::ProviderConnectionsExhausted => "provider_connections_exhausted",
            Self::UserAccountExpired => "user_account_expired",
            Self::SessionOrLeaseExpired => "hls_session_or_lease_expired",
        }
    }

    pub fn from_video_type(video_type: CustomVideoStreamType) -> Option<Self> {
        match video_type {
            CustomVideoStreamType::ChannelUnavailable => Some(Self::ChannelUnavailable),
            CustomVideoStreamType::LowPriorityPreempted => Some(Self::LowPriorityPreempted),
            CustomVideoStreamType::UserConnectionsExhausted => Some(Self::UserConnectionsExhausted),
            CustomVideoStreamType::ProviderConnectionsExhausted => Some(Self::ProviderConnectionsExhausted),
            CustomVideoStreamType::UserAccountExpired => Some(Self::UserAccountExpired),
            CustomVideoStreamType::HlsSessionOrLeaseExpired => Some(Self::SessionOrLeaseExpired),
            CustomVideoStreamType::Provisioning => None,
        }
    }
}

/// Limits which already published live media may precede a finite custom tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsRuntimeCustomTailBasePolicy {
    PreservePublishedSafeSuffix,
    PreserveCompletedOrInFlightPrefix,
}

impl std::fmt::Display for HlsRuntimeCustomTailReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_label())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsRuntimeCustomTailTriggerClass {
    /// Continue live playback while real lease reserve remains safe.
    ReserveAwareAvailability,
    /// Entitlement has ended; publish at the earliest generation-safe boundary.
    ImmediatePolicyCutover,
}

impl HlsRuntimeCustomTailTriggerClass {
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ReserveAwareAvailability => "reserve_aware_availability",
            Self::ImmediatePolicyCutover => "immediate_policy_cutover",
        }
    }
}

/// Preparation cause frozen into the lease-generation CAS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsFiniteTailTrigger {
    AvailabilityReserve,
    RuntimePolicy(HlsRuntimeCustomTailReason),
}

impl HlsFiniteTailTrigger {
    pub const fn reason(self) -> HlsRuntimeCustomTailReason {
        match self {
            Self::AvailabilityReserve => HlsRuntimeCustomTailReason::ChannelUnavailable,
            Self::RuntimePolicy(reason) => reason,
        }
    }

    pub const fn is_runtime_policy(self) -> bool {
        matches!(self, Self::RuntimePolicy(_))
    }
}

#[derive(Clone)]
pub struct HlsRuntimeCustomTailAsset {
    pub reason: HlsRuntimeCustomTailReason,
    pub asset: Arc<HlsTerminalMediaAsset>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct HlsRuntimeCustomTailAssetIdentity {
    pub reason: HlsRuntimeCustomTailReason,
    pub media: HlsTerminalAssetIdentity,
}

impl HlsRuntimeCustomTailAssetIdentity {
    pub const fn new(reason: HlsRuntimeCustomTailReason, media: HlsTerminalAssetIdentity) -> Self {
        Self { reason, media }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn channel_unavailable(media: HlsTerminalAssetIdentity) -> Self {
        Self::new(HlsRuntimeCustomTailReason::ChannelUnavailable, media)
    }

    pub fn from_asset(asset: &HlsRuntimeCustomTailAsset) -> Self {
        Self::new(asset.reason, HlsTerminalAssetIdentity::from_asset(&asset.asset))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsRuntimeCustomTailRevision {
    pub reason: HlsRuntimeCustomTailReason,
    pub asset: Option<HlsTerminalAssetIdentity>,
}

impl HlsRuntimeCustomTailRevision {
    pub const fn from_identity(identity: HlsRuntimeCustomTailAssetIdentity) -> Self {
        Self { reason: identity.reason, asset: Some(identity.media) }
    }

    pub const fn missing(reason: HlsRuntimeCustomTailReason) -> Self {
        Self { reason, asset: None }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsRuntimeCustomTailCompatibility {
    MissingAsset,
    Incompatible(HlsTerminalTailCompatibility),
}

impl HlsRuntimeCustomTailCompatibility {
    pub const fn terminal_compatibility(self) -> HlsTerminalTailCompatibility {
        match self {
            Self::MissingAsset => HlsTerminalTailCompatibility::MissingAsset,
            Self::Incompatible(reason) => reason,
        }
    }
}

fn custom_tail_buffer(
    response: &CustomStreamResponse,
    reason: HlsRuntimeCustomTailReason,
) -> Option<&tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer> {
    match reason {
        HlsRuntimeCustomTailReason::ChannelUnavailable => response.channel_unavailable.as_ref(),
        HlsRuntimeCustomTailReason::LowPriorityPreempted => response.low_priority_preempted.as_ref(),
        HlsRuntimeCustomTailReason::UserConnectionsExhausted => response.user_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => response.provider_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::UserAccountExpired => response.user_account_expired.as_ref(),
        HlsRuntimeCustomTailReason::SessionOrLeaseExpired => response.hls_session_or_lease_expired.as_ref(),
    }
}

pub fn snapshot_hls_runtime_custom_tail_asset(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
) -> Result<HlsRuntimeCustomTailAsset, HlsRuntimeCustomTailCompatibility> {
    if !is_custom_video_stream_enabled(&ctx.app_config) {
        return Err(HlsRuntimeCustomTailCompatibility::MissingAsset);
    }
    let responses = ctx.app_config.custom_stream_response.load_full();
    let buffer = responses
        .as_ref()
        .and_then(|response| custom_tail_buffer(response, reason))
        .ok_or(HlsRuntimeCustomTailCompatibility::MissingAsset)?;
    snapshot_terminal_media_asset(buffer)
        .map(|asset| HlsRuntimeCustomTailAsset { reason, asset })
        .map_err(HlsRuntimeCustomTailCompatibility::Incompatible)
}

pub fn current_hls_runtime_custom_tail_identity(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
) -> Option<HlsRuntimeCustomTailAssetIdentity> {
    if !is_custom_video_stream_enabled(&ctx.app_config) {
        return None;
    }
    ctx.app_config
        .custom_stream_response
        .load_full()
        .as_ref()
        .and_then(|response| custom_tail_buffer(response, reason))
        .and_then(terminal_media_asset_identity)
        .map(|media| HlsRuntimeCustomTailAssetIdentity { reason, media })
}

#[derive(Clone)]
pub struct HlsRuntimeCustomTailRequest {
    pub session: HlsSessionHandle,
    pub proxy_session_id: ProxySessionId,
    pub lease_id: HlsAccessLeaseId,
    pub reason: HlsRuntimeCustomTailReason,
    pub now_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsRuntimeCustomTailOutcome {
    Committed,
    AlreadyCommitted,
    PendingOwnerRegistered,
    Superseded,
    NoLiveLeaseContext,
    NoPublishedManifest,
    MissingAsset,
    Incompatible(HlsTerminalTailCompatibility),
    FailedClosed,
}

impl HlsRuntimeCustomTailOutcome {
    const fn retains_runtime_policy_revocation(self) -> bool {
        matches!(self, Self::Committed | Self::AlreadyCommitted | Self::PendingOwnerRegistered)
    }
}

async fn begin_runtime_custom_tail_revocation(
    ctx: &HlsCtx,
    request: &HlsRuntimeCustomTailRequest,
) -> Result<Option<HlsRuntimePolicyRevocation>, HlsRuntimeCustomTailOutcome> {
    match request.reason.trigger_class() {
        HlsRuntimeCustomTailTriggerClass::ReserveAwareAvailability => Ok(None),
        HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover => {
            match ctx
                .hls_proxy
                .begin_runtime_policy_revocation(
                    &request.lease_id,
                    &request.proxy_session_id,
                    request.reason,
                    request.now_ms,
                )
                .await
            {
                HlsRuntimePolicyRevocationOutcome::Started { token }
                | HlsRuntimePolicyRevocationOutcome::AlreadyPending { token } => Ok(Some(token)),
                HlsRuntimePolicyRevocationOutcome::AlreadyCommitted { .. } => {
                    Err(HlsRuntimeCustomTailOutcome::AlreadyCommitted)
                }
                HlsRuntimePolicyRevocationOutcome::NoPublishedManifest => {
                    Err(HlsRuntimeCustomTailOutcome::NoPublishedManifest)
                }
                HlsRuntimePolicyRevocationOutcome::NoLongerEligible => Err(HlsRuntimeCustomTailOutcome::Superseded),
            }
        }
    }
}

async fn fail_runtime_custom_tail_revocation(
    ctx: &HlsCtx,
    request: &HlsRuntimeCustomTailRequest,
    revocation: Option<&HlsRuntimePolicyRevocation>,
) {
    if let Some(token) = revocation {
        let _ = ctx.hls_proxy.fail_runtime_policy_revocation(&request.lease_id, &request.proxy_session_id, token).await;
    }
}

pub async fn commit_hls_runtime_custom_tail(
    ctx: HlsCtx,
    request: HlsRuntimeCustomTailRequest,
) -> HlsRuntimeCustomTailOutcome {
    log::debug!(
        "HLS runtime custom tail requested: proxy_session={} lease={} reason={} trigger={}",
        super::safe_proxy_session_id(&request.proxy_session_id),
        super::safe_hls_access_lease_id(&request.lease_id),
        request.reason.as_label(),
        request.reason.trigger_class().as_label()
    );
    let revocation = match begin_runtime_custom_tail_revocation(&ctx, &request).await {
        Ok(revocation) => revocation,
        Err(outcome) => return outcome,
    };
    let outcome = resolve_hls_runtime_custom_tail_after_revocation(&ctx, &request).await;
    if !outcome.retains_runtime_policy_revocation() {
        fail_runtime_custom_tail_revocation(&ctx, &request, revocation.as_ref()).await;
    }
    outcome
}

async fn resolve_hls_runtime_custom_tail_after_revocation(
    ctx: &HlsCtx,
    request: &HlsRuntimeCustomTailRequest,
) -> HlsRuntimeCustomTailOutcome {
    let Some(lease) = ctx
        .hls_proxy
        .access_lease_response_snapshot(&request.lease_id, &request.proxy_session_id, request.now_ms)
        .await
    else {
        return HlsRuntimeCustomTailOutcome::NoLiveLeaseContext;
    };
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) => return HlsRuntimeCustomTailOutcome::AlreadyCommitted,
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
            return HlsRuntimeCustomTailOutcome::Superseded;
        }
        HlsLeasePlaybackMode::Live => {}
    }
    if lease.last_manifest_snapshot.is_none() {
        return HlsRuntimeCustomTailOutcome::NoPublishedManifest;
    }
    let asset = match snapshot_hls_runtime_custom_tail_asset(ctx, request.reason) {
        Ok(asset) => asset,
        Err(HlsRuntimeCustomTailCompatibility::MissingAsset) => return HlsRuntimeCustomTailOutcome::MissingAsset,
        Err(HlsRuntimeCustomTailCompatibility::Incompatible(reason)) => {
            return HlsRuntimeCustomTailOutcome::Incompatible(reason);
        }
    };
    let resolution = match request.reason.trigger_class() {
        HlsRuntimeCustomTailTriggerClass::ReserveAwareAvailability => {
            commit_terminal_tail_if_lease_reserve_requires_cutover(
                ctx,
                &request.session,
                &request.proxy_session_id,
                &lease,
                request.now_ms,
            )
            .await
        }
        HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover => {
            let Some(preparation) = ctx
                .hls_proxy
                .prepare_access_lease_runtime_custom_tail(
                    &request.session,
                    &request.lease_id,
                    &request.proxy_session_id,
                    request.reason,
                    request.now_ms,
                )
                .await
            else {
                return HlsRuntimeCustomTailOutcome::Superseded;
            };
            commit_prepared_runtime_custom_tail(
                ctx,
                &request.session,
                &request.proxy_session_id,
                &request.lease_id,
                &preparation,
                request.now_ms,
                asset,
            )
            .await
        }
    };
    runtime_custom_tail_outcome(resolution)
}

fn runtime_custom_tail_outcome(resolution: HlsTerminalResolution) -> HlsRuntimeCustomTailOutcome {
    match resolution {
        HlsTerminalResolution::Committed => HlsRuntimeCustomTailOutcome::Committed,
        HlsTerminalResolution::Pending { .. } => HlsRuntimeCustomTailOutcome::PendingOwnerRegistered,
        HlsTerminalResolution::Reevaluate | HlsTerminalResolution::LiveAllowed => {
            HlsRuntimeCustomTailOutcome::Superseded
        }
        HlsTerminalResolution::FailedClosed { .. } => HlsRuntimeCustomTailOutcome::FailedClosed,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct HlsStandaloneCustomPlanKey {
    pub reason: HlsRuntimeCustomTailReason,
    pub asset: HlsTerminalAssetIdentity,
    pub segment_count: u16,
}

#[derive(Clone)]
pub struct HlsStandaloneCustomPlan {
    pub key: HlsStandaloneCustomPlanKey,
    pub manifest_body: Arc<str>,
    pub prepared_bundle: Arc<HlsPreparedTerminalBundle>,
}

impl HlsStandaloneCustomPlan {
    pub fn segment_bytes(&self, index: u16) -> Option<bytes::Bytes> {
        let segment = self.prepared_bundle.segments.get(usize::from(index))?;
        (segment.index == index).then(|| segment.bytes.clone())
    }
}

/// Server-side authorization bound to one finite standalone custom response.
///
/// The opaque lease ID is the only authorization capability exposed in the
/// segment URI. User identity and an optional Shared-HLS lease incarnation
/// remain server-side.
#[derive(Clone)]
pub struct HlsStandaloneCustomAccess {
    lease_id: HlsAccessLeaseId,
    username: Arc<str>,
    shared_lease: Option<HlsStandaloneSharedLeaseBinding>,
    shared_valid_until_ms: Option<u64>,
}

impl HlsStandaloneCustomAccess {
    pub fn for_user(username: impl Into<Arc<str>>) -> Self {
        Self {
            lease_id: new_hls_access_lease_id(),
            username: username.into(),
            shared_lease: None,
            shared_valid_until_ms: None,
        }
    }

    pub fn for_shared_lease(
        lease_id: HlsAccessLeaseId,
        proxy_session_id: ProxySessionId,
        username: impl Into<Arc<str>>,
        lease_issued_at_ms: u64,
        valid_until_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            username: username.into(),
            shared_lease: Some(HlsStandaloneSharedLeaseBinding { proxy_session_id, lease_issued_at_ms }),
            shared_valid_until_ms: Some(valid_until_ms),
        }
    }

    pub fn lease_id(&self) -> &HlsAccessLeaseId {
        &self.lease_id
    }

    fn valid_until_ms(&self, now_ms: u64, media_duration_ms: u64, segment_count: u16) -> u64 {
        self.shared_valid_until_ms.unwrap_or_else(|| {
            let playback_ms = media_duration_ms.saturating_mul(u64::from(segment_count));
            let ttl_ms = playback_ms
                .saturating_add(HLS_STANDALONE_CUSTOM_ACCESS_COMPLETION_GRACE_MS)
                .clamp(HLS_STANDALONE_CUSTOM_ACCESS_MIN_TTL_MS, HLS_STANDALONE_CUSTOM_ACCESS_MAX_TTL_MS);
            now_ms.saturating_add(ttl_ms)
        })
    }
}

#[derive(Clone)]
pub struct HlsStandaloneSharedLeaseBinding {
    pub proxy_session_id: ProxySessionId,
    pub lease_issued_at_ms: u64,
}

#[derive(Clone)]
pub struct HlsStandaloneCustomAccessEntry {
    access: HlsStandaloneCustomAccess,
    asset_fingerprint: Arc<str>,
    valid_until_ms: u64,
    plan: HlsStandaloneCustomPlan,
}

#[derive(Default)]
struct HlsStandaloneCustomAccessStoreInner {
    entries: HashMap<HlsAccessLeaseId, HlsStandaloneCustomAccessEntry>,
    insertion_order: VecDeque<HlsAccessLeaseId>,
}

/// Bounded registry for immutable standalone custom-response plans.
#[derive(Default)]
pub struct HlsStandaloneCustomAccessStore {
    inner: Mutex<HlsStandaloneCustomAccessStoreInner>,
}

impl HlsStandaloneCustomAccessStore {
    fn lock(&self) -> MutexGuard<'_, HlsStandaloneCustomAccessStoreInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn remove_expired(inner: &mut HlsStandaloneCustomAccessStoreInner, now_ms: u64) {
        let expired = inner
            .entries
            .iter()
            .filter(|&(_lease_id, entry)| entry.valid_until_ms <= now_ms)
            .map(|(lease_id, _entry)| lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in &expired {
            inner.entries.remove(lease_id);
        }
        if !expired.is_empty() {
            inner.insertion_order.retain(|lease_id| !expired.contains(lease_id));
        }
    }

    pub fn register(&self, entry: HlsStandaloneCustomAccessEntry, now_ms: u64) {
        let mut inner = self.lock();
        Self::remove_expired(&mut inner, now_ms);
        let lease_id = entry.access.lease_id.clone();
        inner.insertion_order.retain(|current| current != &lease_id);
        inner.insertion_order.push_back(lease_id.clone());
        inner.entries.insert(lease_id, entry);
        while inner.entries.len() > HLS_STANDALONE_CUSTOM_ACCESS_CAPACITY {
            let Some(oldest) = inner.insertion_order.pop_front() else {
                break;
            };
            inner.entries.remove(&oldest);
        }
    }

    pub fn resolve(
        &self,
        lease_id: &HlsAccessLeaseId,
        asset_fingerprint: &str,
        index: u16,
        now_ms: u64,
    ) -> Result<HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError> {
        let mut inner = self.lock();
        Self::remove_expired(&mut inner, now_ms);
        let entry = inner.entries.get(lease_id).ok_or(HlsStandaloneCustomSegmentError::UnknownAccessLease)?;
        if entry.asset_fingerprint.as_ref() != asset_fingerprint {
            return Err(HlsStandaloneCustomSegmentError::StaleAssetFingerprint);
        }
        let bytes = entry.plan.segment_bytes(index).ok_or(HlsStandaloneCustomSegmentError::InvalidIndex)?;
        Ok(HlsStandaloneCustomSegmentAccess {
            username: Arc::clone(&entry.access.username),
            shared_lease: entry.access.shared_lease.clone(),
            bytes,
        })
    }

    pub fn remove(&self, lease_id: &HlsAccessLeaseId) {
        let mut inner = self.lock();
        inner.entries.remove(lease_id);
        inner.insertion_order.retain(|current| current != lease_id);
    }

    pub fn clear(&self) -> usize {
        let mut inner = self.lock();
        let removed = inner.entries.len();
        inner.entries.clear();
        inner.insertion_order.clear();
        removed
    }

    #[cfg(any(test, feature = "test-support"))]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

pub struct HlsStandaloneCustomSegmentAccess {
    pub username: Arc<str>,
    pub shared_lease: Option<HlsStandaloneSharedLeaseBinding>,
    pub bytes: bytes::Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsStandaloneCustomSegmentError {
    InvalidIndex,
    UnknownAccessLease,
    StaleAssetFingerprint,
}

pub fn standalone_custom_asset_fingerprint(identity: HlsTerminalAssetIdentity) -> String {
    format!("{:016x}", identity.revision)
}

/// Resolves one immutable prepared standalone segment without scanning or
/// rewriting MPEG-TS at the HTTP request boundary.
pub fn resolve_hls_standalone_custom_segment(
    ctx: &HlsCtx,
    lease_id: &HlsAccessLeaseId,
    asset_fingerprint: &str,
    index: u16,
    now_ms: u64,
) -> Result<HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError> {
    ctx.hls_proxy.resolve_standalone_custom_segment(lease_id, asset_fingerprint, index, now_ms)
}

async fn await_prepared_standalone_bundle(
    ctx: &HlsCtx,
    asset: Arc<HlsTerminalMediaAsset>,
    key: HlsPreparedTerminalBundleKey,
) -> Option<Arc<HlsPreparedTerminalBundle>> {
    let state = ctx.hls_proxy.prepared_terminal_bundle_state(key).unwrap_or_else(|| {
        ctx.hls_proxy.start_prepared_terminal_bundle(asset, key.target_duration_ms, key.segment_count)
    });
    match state {
        HlsPreparedTerminalBundleState::Ready { bundle } if bundle.matches_key_and_shape(key) => Some(bundle),
        HlsPreparedTerminalBundleState::Preparing { .. } => {
            let HlsPreparedTerminalBundleObservation::Flight(ticket) =
                ctx.hls_proxy.observe_prepared_terminal_bundle(key)
            else {
                return None;
            };
            let timeout_ms = ctx.hls_proxy.origin_manifest_timeout_ms().max(1);
            match tokio::time::timeout(Duration::from_millis(timeout_ms), ticket.wait()).await {
                Ok(HlsPreparedTerminalBundleCompletion::Ready { bundle }) if bundle.matches_key_and_shape(key) => {
                    Some(bundle)
                }
                Ok(
                    HlsPreparedTerminalBundleCompletion::Ready { .. }
                    | HlsPreparedTerminalBundleCompletion::Failed { .. }
                    | HlsPreparedTerminalBundleCompletion::Incompatible { .. }
                    | HlsPreparedTerminalBundleCompletion::FlightReplaced { .. },
                )
                | Err(_) => None,
            }
        }
        HlsPreparedTerminalBundleState::Ready { .. }
        | HlsPreparedTerminalBundleState::Failed { .. }
        | HlsPreparedTerminalBundleState::Incompatible { .. } => None,
    }
}

pub async fn build_hls_standalone_custom_plan(
    ctx: &HlsCtx,
    base_url: &str,
    access: HlsStandaloneCustomAccess,
    reason: HlsRuntimeCustomTailReason,
    now_ms: u64,
) -> Result<HlsStandaloneCustomPlan, HlsRuntimeCustomTailCompatibility> {
    let snapshot = snapshot_hls_runtime_custom_tail_asset(ctx, reason)?;
    let media = HlsTerminalAssetIdentity::from_asset(&snapshot.asset);
    let target_duration_ms = hls_target_duration_secs(snapshot.asset.duration_ms()).saturating_mul(1_000);
    let prepared_key = HlsPreparedTerminalBundleKey {
        asset: media,
        target_duration_ms,
        segment_count: HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    };
    let Some(prepared_bundle) = await_prepared_standalone_bundle(ctx, Arc::clone(&snapshot.asset), prepared_key).await
    else {
        return Err(HlsRuntimeCustomTailCompatibility::Incompatible(
            HlsTerminalTailCompatibility::TerminalMediaNotReady,
        ));
    };
    let key = HlsStandaloneCustomPlanKey { reason, asset: media, segment_count: HLS_TERMINAL_TAIL_SEGMENT_COUNT };
    let asset_fingerprint = standalone_custom_asset_fingerprint(media);
    let extinf = format_hls_duration_ms(snapshot.asset.duration_ms());
    let mut manifest = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n",
        hls_target_duration_secs(snapshot.asset.duration_ms())
    );
    for index in 0..key.segment_count {
        let _ = writeln!(
            manifest,
            "#EXTINF:{extinf},\n{}/cvs/hls/{}/{asset_fingerprint}/{index}.ts",
            base_url.trim_end_matches('/'),
            access.lease_id().0.as_str()
        );
    }
    manifest.push_str("#EXT-X-ENDLIST\n");
    let plan = HlsStandaloneCustomPlan { key, manifest_body: Arc::from(manifest), prepared_bundle };
    let valid_until_ms = access.valid_until_ms(now_ms, snapshot.asset.duration_ms(), HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    ctx.hls_proxy.register_standalone_custom_access(
        HlsStandaloneCustomAccessEntry {
            access,
            asset_fingerprint: Arc::from(asset_fingerprint),
            valid_until_ms,
            plan: plan.clone(),
        },
        now_ms,
    );
    log::debug!(
        "HLS standalone custom response: reason={} asset_revision={:016x} segments={}",
        reason.as_label(),
        plan.key.asset.revision,
        plan.prepared_bundle.segments.len()
    );
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::{super::prepared_terminal_bundle::HlsPreparedTerminalSegment, *};

    fn standalone_access_entry(
        lease_id: &str,
        asset_revision: u64,
        valid_until_ms: u64,
    ) -> HlsStandaloneCustomAccessEntry {
        let asset = HlsTerminalAssetIdentity { revision: asset_revision, fingerprint: [1; 32] };
        let prepared_bundle = Arc::new(HlsPreparedTerminalBundle {
            key: HlsPreparedTerminalBundleKey { asset, target_duration_ms: 1_000, segment_count: 1 },
            source_asset_duration_ms: 1_000,
            source_asset_duration_ticks_90khz: 90_000,
            segments: Arc::from([HlsPreparedTerminalSegment {
                index: 0,
                timestamp_offset_ticks_90khz: 0,
                bytes: bytes::Bytes::from_static(b"segment"),
            }]),
        });
        HlsStandaloneCustomAccessEntry {
            access: HlsStandaloneCustomAccess {
                lease_id: HlsAccessLeaseId(lease_id.to_string()),
                username: Arc::from("viewer"),
                shared_lease: None,
                shared_valid_until_ms: None,
            },
            asset_fingerprint: Arc::from(format!("{asset_revision:016x}")),
            valid_until_ms,
            plan: HlsStandaloneCustomPlan {
                key: HlsStandaloneCustomPlanKey {
                    reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
                    asset,
                    segment_count: 1,
                },
                manifest_body: Arc::from("#EXTM3U\n#EXT-X-ENDLIST\n"),
                prepared_bundle,
            },
        }
    }

    #[test]
    fn runtime_custom_tail_reason_mapping_is_exhaustive_and_provisioning_is_excluded() {
        let expected = [
            (
                HlsRuntimeCustomTailReason::ChannelUnavailable,
                CustomVideoStreamType::ChannelUnavailable,
                HlsRuntimeCustomTailTriggerClass::ReserveAwareAvailability,
            ),
            (
                HlsRuntimeCustomTailReason::LowPriorityPreempted,
                CustomVideoStreamType::LowPriorityPreempted,
                HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
            ),
            (
                HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                CustomVideoStreamType::UserConnectionsExhausted,
                HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
            ),
            (
                HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
                CustomVideoStreamType::ProviderConnectionsExhausted,
                HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
            ),
            (
                HlsRuntimeCustomTailReason::UserAccountExpired,
                CustomVideoStreamType::UserAccountExpired,
                HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
            ),
            (
                HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
                CustomVideoStreamType::HlsSessionOrLeaseExpired,
                HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover,
            ),
        ];
        for (reason, video_type, trigger) in expected {
            assert_eq!(reason.video_type(), video_type);
            assert_eq!(reason.trigger_class(), trigger);
            assert_eq!(HlsRuntimeCustomTailReason::from_video_type(video_type), Some(reason));
            assert_eq!(
                reason.permits_unpublished_lease_standalone_tail(),
                reason != HlsRuntimeCustomTailReason::SessionOrLeaseExpired
            );
        }
        assert_eq!(HlsRuntimeCustomTailReason::from_video_type(CustomVideoStreamType::Provisioning), None);
    }

    #[test]
    fn runtime_custom_tail_revocation_is_retained_only_by_owned_completion() {
        for outcome in [
            HlsRuntimeCustomTailOutcome::Committed,
            HlsRuntimeCustomTailOutcome::AlreadyCommitted,
            HlsRuntimeCustomTailOutcome::PendingOwnerRegistered,
        ] {
            assert!(outcome.retains_runtime_policy_revocation());
        }
        for outcome in [
            HlsRuntimeCustomTailOutcome::Superseded,
            HlsRuntimeCustomTailOutcome::NoLiveLeaseContext,
            HlsRuntimeCustomTailOutcome::NoPublishedManifest,
            HlsRuntimeCustomTailOutcome::MissingAsset,
            HlsRuntimeCustomTailOutcome::Incompatible(HlsTerminalTailCompatibility::InvalidAsset),
            HlsRuntimeCustomTailOutcome::FailedClosed,
        ] {
            assert!(!outcome.retains_runtime_policy_revocation());
        }
    }

    #[test]
    fn standalone_asset_fingerprint_is_short_revision_identifier() {
        let first = HlsTerminalAssetIdentity { revision: 7, fingerprint: [1; 32] };
        let revised = HlsTerminalAssetIdentity { revision: 8, fingerprint: [1; 32] };
        let replaced = HlsTerminalAssetIdentity { revision: 7, fingerprint: [2; 32] };

        let token = standalone_custom_asset_fingerprint(first);
        assert_eq!(token.len(), 16);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(token, standalone_custom_asset_fingerprint(revised));
        assert_eq!(token, standalone_custom_asset_fingerprint(replaced));
    }

    #[test]
    fn standalone_access_store_expires_entries_at_deadline() {
        let store = HlsStandaloneCustomAccessStore::default();
        let lease_id = HlsAccessLeaseId("lease".to_string());
        store.register(standalone_access_entry("lease", 7, 100), 0);

        assert!(store.resolve(&lease_id, "0000000000000007", 0, 99).is_ok());
        assert_eq!(
            store.resolve(&lease_id, "0000000000000007", 0, 100).map(|_| ()),
            Err(HlsStandaloneCustomSegmentError::UnknownAccessLease)
        );
    }

    #[test]
    fn standalone_access_store_evicts_oldest_entry_at_hard_capacity() {
        let store = HlsStandaloneCustomAccessStore::default();
        for index in 0..=HLS_STANDALONE_CUSTOM_ACCESS_CAPACITY {
            store.register(standalone_access_entry(&format!("lease-{index}"), index as u64, 10_000), 0);
        }

        assert_eq!(store.len(), HLS_STANDALONE_CUSTOM_ACCESS_CAPACITY);
        assert_eq!(
            store.resolve(&HlsAccessLeaseId("lease-0".to_string()), "0000000000000000", 0, 1,).map(|_| ()),
            Err(HlsStandaloneCustomSegmentError::UnknownAccessLease)
        );
        assert!(store.resolve(&HlsAccessLeaseId("lease-1".to_string()), "0000000000000001", 0, 1,).is_ok());
    }
}
